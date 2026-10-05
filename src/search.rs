//! Search (spec 9): per-item Bloom gate + case-insensitive fuzzy scoring. Pure, std only.
//!
//! # Gate design
//! Every item carries a 1,024-byte trigram Bloom and a 32-byte character Bloom, built
//! once off the UI thread ([`SearchIndex::build`]). Fuzzy matching accepts *non-contiguous*
//! matches ("hlo" finds "hello"), but a trigram gate only admits contiguous ones, so the
//! two filters play different roles and the gate never excludes anything the scorer would
//! accept:
//!
//! 1. [`SearchIndex::may_match`] (the gate) is the character Bloom: every query char must
//!    be present. This is a necessary condition for BOTH an exact substring and a
//!    subsequence match, for any query length >= 1.
//! 2. For queries >= 3 chars the trigram Bloom ([`SearchIndex::has_trigrams`]) decides
//!    whether the (full-text) exact-substring scan is worth running: if some query trigram
//!    is absent, no substring match can exist, so the scan is skipped and only the
//!    subsequence matcher runs. Bloom filters have no false negatives, so this never
//!    changes a result: `score(ix, q) == fuzzy_score(ix.text(), q)` always.
//!
//! The subsequence matcher only looks at the first [`FUZZY_WINDOW`] bytes of an item
//! (scattered matches deep inside a long text are noise and would be slow); exact
//! substrings are searched in the full indexed text.

/// Text beyond this many bytes is not indexed (spec 9).
pub const MAX_INDEXED_BYTES: usize = 500 * 1024;
/// Subsequence matching looks at this many leading bytes only.
pub const FUZZY_WINDOW: usize = 2048;
/// Longer queries are exact-substring only.
const MAX_FUZZY_QUERY: usize = 64;
/// Occurrences of a substring considered when picking the best-aligned one.
const MAX_OCCURRENCES: usize = 8;

const TRI_BYTES: usize = 1024;
const CHAR_BYTES: usize = 32;

// Scoring. Subsequence scores stay far below SUBSTR_BASE (<= 64 chars * ~40), so an exact
// substring always outranks any scattered match of the same query.
const SUBSTR_BASE: i32 = 10_000;
const EXACT_BONUS: i32 = 200; // the whole text equals the query
const NEAR_START: usize = 48; // bytes: matches this close to the start get a bonus
const MATCH: i32 = 16;
const CONSECUTIVE: i32 = 4;
const GAP_START: i32 = 3;
const GAP_EXT: i32 = 1;
const GAP_MAX: i32 = 15; // < MATCH: every matched char still nets a positive score

pub struct SearchIndex {
    tri: [u8; TRI_BYTES],
    chars: [u8; CHAR_BYTES],
    text: String,
}

/// Case/whitespace folding shared by [`SearchIndex::build`] and [`prepare`]: any
/// whitespace becomes ' ' (so a query "a b" finds text split over lines), everything else
/// is lowercased using the first char of its lowercase mapping ('İ' becomes 'i').
fn fold(c: char) -> char {
    if c.is_whitespace() {
        ' '
    } else if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

fn char_bit(c: char) -> usize {
    let v = c as u32;
    if v < 256 {
        v as usize
    } else {
        (v.wrapping_mul(0x9E37_79B1) >> 24) as usize
    }
}

/// Two bit positions (0..8192) for a trigram.
fn tri_bits(a: char, b: char, c: char) -> [usize; 2] {
    let packed = ((a as u64) << 42) | ((b as u64) << 21) | c as u64; // chars are < 2^21
    let h = packed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    [(h >> 51) as usize, ((h >> 38) & 0x1FFF) as usize]
}

fn set_bit(bits: &mut [u8], n: usize) {
    if let Some(b) = bits.get_mut(n >> 3) {
        *b |= 1 << (n & 7);
    }
}

fn get_bit(bits: &[u8], n: usize) -> bool {
    bits.get(n >> 3).is_some_and(|b| b & (1 << (n & 7)) != 0)
}

impl SearchIndex {
    /// Single pass over `text` (truncated to [`MAX_INDEXED_BYTES`] of folded text).
    pub fn build(text: &str) -> SearchIndex {
        let mut ix = SearchIndex {
            tri: [0; TRI_BYTES],
            chars: [0; CHAR_BYTES],
            text: String::with_capacity(text.len().min(MAX_INDEXED_BYTES)),
        };
        let (mut a, mut b) = ('\0', '\0');
        for (n, raw) in text.chars().enumerate() {
            let c = fold(raw);
            if ix.text.len() + c.len_utf8() > MAX_INDEXED_BYTES {
                break;
            }
            ix.text.push(c);
            set_bit(&mut ix.chars, char_bit(c));
            if n >= 2 {
                for bit in tri_bits(a, b, c) {
                    set_bit(&mut ix.tri, bit);
                }
            }
            (a, b) = (b, c);
        }
        ix
    }

    /// The folded (lowercased) indexed text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The Bloom gate: every query char is (probably) in the text. Necessary for any
    /// match [`fuzzy_score`] accepts; see the module docs.
    pub fn may_match(&self, q: &Query) -> bool {
        self.chars.iter().zip(&q.mask).all(|(h, m)| h & m == *m)
    }

    /// Every query trigram is (probably) in the text; vacuously true for queries < 3 chars.
    pub fn has_trigrams(&self, q: &Query) -> bool {
        q.tris.iter().flatten().all(|&n| get_bit(&self.tri, n))
    }
}

pub struct Query {
    text: String,
    chars: Vec<char>,
    mask: [u8; CHAR_BYTES],
    tris: Vec<[usize; 2]>,
}

/// Trims and folds the query. An empty query matches everything with score 0.
pub fn prepare(query: &str) -> Query {
    let text: String = query.trim().chars().map(fold).collect();
    let chars: Vec<char> = text.chars().collect();
    let mut mask = [0; CHAR_BYTES];
    for &c in &chars {
        set_bit(&mut mask, char_bit(c));
    }
    let tris = chars
        .windows(3)
        .filter_map(|w| match w {
            [a, b, c] => Some(tri_bits(*a, *b, *c)),
            _ => None,
        })
        .collect();
    Query {
        text,
        chars,
        mask,
        tris,
    }
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }
}

fn prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1; // 0 is always a boundary
    }
    s.get(..end).unwrap_or(s)
}

fn boundary(prev: Option<char>) -> i32 {
    match prev {
        None => 12, // start of text
        Some(p) if p.is_whitespace() => 10,
        Some(p) if !p.is_alphanumeric() => 8, // punctuation, path separators, ...
        _ => 0,
    }
}

fn near_start(off: usize) -> i32 {
    (NEAR_START - off.min(NEAR_START)) as i32
}

/// Best of the first few occurrences of the query as an exact substring.
fn substring_score(hay: &str, q: &Query) -> Option<i32> {
    hay.match_indices(q.text.as_str())
        .take(MAX_OCCURRENCES)
        .map(|(off, m)| {
            let prev = hay.get(..off).and_then(|h| h.chars().next_back());
            let exact = if off == 0 && m.len() == hay.len() {
                EXACT_BONUS
            } else {
                0
            };
            SUBSTR_BASE + near_start(off) + 6 * boundary(prev) + exact
        })
        .max()
}

/// fzf-v1 style: find the earliest end of a subsequence match, walk back to the latest
/// start for that end (tightest window), then score that window.
/// ponytail: a later, better aligned subsequence match is not considered; upgrade to a
/// DP over the window if ranking quality of scattered matches ever matters.
fn subsequence_score(hay: &str, q: &Query) -> Option<i32> {
    if q.chars.len() > MAX_FUZZY_QUERY {
        return None;
    }
    let win = prefix(hay, FUZZY_WINDOW);

    let mut want = q.chars.iter();
    let mut cur = *want.next()?;
    let mut end = None;
    for (i, c) in win.char_indices() {
        if c == cur {
            match want.next() {
                Some(&n) => cur = n,
                None => {
                    end = Some(i + c.len_utf8());
                    break;
                }
            }
        }
    }
    let head = win.get(..end?)?;

    let mut want = q.chars.iter().rev();
    let mut cur = *want.next()?;
    let mut start = 0;
    for (i, c) in head.char_indices().rev() {
        if c == cur {
            match want.next() {
                Some(&n) => cur = n,
                None => {
                    start = i;
                    break;
                }
            }
        }
    }

    // Leftmost greedy match from `start` ends exactly at `end` (`end` is minimal).
    let mut prev = win.get(..start).and_then(|h| h.chars().next_back());
    let mut want = q.chars.iter();
    let mut cur = want.next();
    let mut score = near_start(start) >> 2;
    let mut run = 0; // bonus carried along a contiguous run
    let mut gap: Option<i32> = None; // chars since the previous match
    for c in head.get(start..)?.chars() {
        if cur == Some(&c) {
            let b = boundary(prev);
            let (bonus, penalty) = match gap {
                None => (b, 0),
                Some(0) => (b.max(run).max(CONSECUTIVE), 0),
                Some(g) => (b, (GAP_START + (g - 1) * GAP_EXT).min(GAP_MAX)),
            };
            run = bonus;
            score += MATCH + if gap.is_none() { 2 * bonus } else { bonus } - penalty;
            gap = Some(0);
            cur = want.next();
        } else {
            gap = gap.map(|g| g + 1);
        }
        prev = Some(c);
    }
    cur.is_none().then_some(score)
}

/// Scores `hay_lower` (already folded) against `q`; no Bloom gate. Exact substring
/// (>= 10,000) > scattered subsequence (small positive). Empty query => `Some(0)`.
pub fn fuzzy_score(hay_lower: &str, q: &Query) -> Option<i32> {
    if q.is_empty() {
        return Some(0);
    }
    substring_score(hay_lower, q).or_else(|| subsequence_score(hay_lower, q))
}

/// Gate, then score. Same result as `fuzzy_score(ix.text(), q)` but skips the full-text
/// substring scan when the trigram Bloom proves it cannot succeed.
pub fn score(ix: &SearchIndex, q: &Query) -> Option<i32> {
    if q.is_empty() {
        return Some(0);
    }
    if !ix.may_match(q) {
        return None;
    }
    let sub = if ix.has_trigrams(q) {
        substring_score(ix.text(), q)
    } else {
        None
    };
    sub.or_else(|| subsequence_score(ix.text(), q))
}

fn sorted(hits: impl Iterator<Item = (usize, i32)>) -> Vec<(usize, i32)> {
    let mut v: Vec<_> = hits.collect();
    v.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// Ranks candidates given in recency order (newest first): `(position, score)` by score
/// descending, ties by position ascending. Empty query => every position, score 0.
pub fn rank<'a>(indexes: impl Iterator<Item = &'a SearchIndex>, q: &Query) -> Vec<(usize, i32)> {
    sorted(
        indexes
            .enumerate()
            .filter_map(|(i, ix)| score(ix, q).map(|s| (i, s))),
    )
}

/// [`rank`] for plain strings (snippet names, ...): folds a scratch copy per string.
pub fn rank_strs<'a>(strs: impl Iterator<Item = &'a str>, q: &Query) -> Vec<(usize, i32)> {
    let mut buf = String::new();
    sorted(strs.enumerate().filter_map(|(i, s)| {
        buf.clear();
        buf.extend(s.chars().map(fold));
        fuzzy_score(&buf, q).map(|sc| (i, sc))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    const WORDS: [&str; 24] = [
        "the",
        "quick",
        "brown",
        "fox",
        "config",
        "hello",
        "world",
        "fn",
        "main",
        "let",
        "mut",
        "error",
        "warning",
        "path",
        "C:\\Users\\sbritos",
        "https://example.com/a?b=1",
        "Stra\u{df}e",
        "\u{c9}COLE",
        "\u{f1}and\u{fa}",
        "\u{130}stanbul",
        "getUserName",
        "TODO",
        "{\"k\": [1, 2]}",
        "ok",
    ];
    const SEPS: [&str; 6] = [" ", " ", "\n", "_", ".", "\\"];

    fn text_of(rng: &mut Lcg, approx_bytes: usize) -> String {
        let mut s = String::new();
        while s.len() < approx_bytes {
            s.push_str(WORDS[rng.below(WORDS.len())]);
            s.push_str(SEPS[rng.below(SEPS.len())]);
        }
        s
    }

    fn idx(texts: &[&str]) -> Vec<SearchIndex> {
        texts.iter().map(|t| SearchIndex::build(t)).collect()
    }

    fn order(texts: &[&str], query: &str) -> Vec<usize> {
        let ixs = idx(texts);
        rank(ixs.iter(), &prepare(query))
            .into_iter()
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn index_is_send_sync() {
        fn ok<T: Send + Sync>() {}
        ok::<SearchIndex>();
        ok::<Query>();
    }

    #[test]
    fn ranking_on_fixed_corpus() {
        let corpus = [
            "f_o_x scattered",     // 0 scattered
            "the brown fox jumps", // 1 substring at a word boundary
            "foxtrot",             // 2 substring at the start
            "xfoxx",               // 3 substring mid-word
            "nothing here",        // 4 no match
            "fire ox",             // 5 scattered, tighter
        ];
        assert_eq!(order(&corpus, "fox"), vec![2, 1, 3, 5, 0]);
        assert_eq!(order(&corpus, "FOX"), vec![2, 1, 3, 5, 0]);
    }

    #[test]
    fn exact_substring_beats_scattered() {
        let corpus = ["hello world", "xhlox"];
        assert_eq!(order(&corpus, "hlo"), vec![1, 0]);
        let ixs = idx(&corpus);
        let q = prepare("hlo");
        assert!(score(&ixs[0], &q).is_some_and(|s| s < SUBSTR_BASE));
        assert!(score(&ixs[1], &q).is_some_and(|s| s >= SUBSTR_BASE));
    }

    #[test]
    fn word_boundary_beats_mid_word() {
        assert_eq!(
            order(&["abcdef", "ab def", "x_def", "xxdef"], "def"),
            vec![1, 2, 3, 0]
        );
        // scattered: boundary starts and tight runs win
        assert_eq!(
            order(&["xxfxxbxxr", "foo bar", "fxobxar"], "fbr"),
            vec![1, 2, 0]
        );
    }

    #[test]
    fn exact_equal_and_prefix_order() {
        assert_eq!(order(&["abc def", "abc", "xx abc"], "abc"), vec![1, 0, 2]);
    }

    #[test]
    fn recency_breaks_ties() {
        let corpus = ["foo bar", "foo bar", "foo bar"];
        let ixs = idx(&corpus);
        let r = rank(ixs.iter(), &prepare("foo"));
        assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert!(r.iter().all(|x| x.1 == r[0].1));
    }

    #[test]
    fn empty_query_returns_everything_in_order() {
        let ixs = idx(&["b", "a", ""]);
        for q in ["", "   ", "\t\n"] {
            let p = prepare(q);
            assert!(p.is_empty());
            assert_eq!(rank(ixs.iter(), &p), vec![(0, 0), (1, 0), (2, 0)]);
            assert_eq!(rank_strs(["b", "a"].into_iter(), &p), vec![(0, 0), (1, 0)]);
            assert_eq!(fuzzy_score("anything", &p), Some(0));
        }
    }

    #[test]
    fn non_contiguous_matches_pass_the_gate() {
        let ix = SearchIndex::build("Hello World");
        let q = prepare("hlo wrd");
        assert!(ix.may_match(&q));
        assert!(!ix.has_trigrams(&q)); // trigram bloom alone would have rejected it
        assert!(score(&ix, &q).is_some());
        assert_eq!(score(&ix, &q), fuzzy_score(ix.text(), &q));
        assert!(score(&ix, &prepare("hlz")).is_none());
        assert!(score(&ix, &prepare("worldx")).is_none());
        assert!(score(&ix, &prepare("dlrow")).is_none()); // order matters
    }

    #[test]
    fn one_and_two_char_queries() {
        let corpus = ["alpha", "beta", "xyz", "b"];
        assert_eq!(order(&corpus, "a"), vec![0, 1]);
        assert_eq!(order(&corpus, "b"), vec![3, 1]); // equal text first
        assert_eq!(order(&corpus, "be"), vec![1]);
        assert_eq!(order(&corpus, "ea"), vec![1]); // scattered, in order
        assert_eq!(order(&corpus, "ae"), Vec::<usize>::new()); // wrong order
        assert_eq!(order(&corpus, "zz"), Vec::<usize>::new());
        assert_eq!(order(&corpus, "q"), Vec::<usize>::new());
    }

    #[test]
    fn unicode_and_case_folding() {
        let ixs = idx(&[
            "\u{c9}COLE Normale",
            "\u{f1}and\u{fa} guaran\u{ed}",
            "\u{130}stanbul",
            "STRASSE",
        ]);
        let r = |q: &str| {
            rank(ixs.iter(), &prepare(q))
                .into_iter()
                .map(|x| x.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(r("\u{e9}cole"), vec![0]);
        assert_eq!(r("\u{c9}COLE"), vec![0]);
        assert_eq!(r("\u{d1}AND\u{da}"), vec![1]);
        assert_eq!(r("guaran\u{cd}"), vec![1]);
        assert_eq!(r("istanbul"), vec![2]);
        assert_eq!(r("\u{130}STANBUL"), vec![2]);
        assert_eq!(r("strasse"), vec![3]);
        // must not panic
        let _ = r("\u{130}");
        let _ = r("\u{1F600}\u{0301}\0");
        assert_eq!(
            rank_strs(["Caf\u{c9}"].into_iter(), &prepare("caf\u{e9}")).len(),
            1
        );
    }

    #[test]
    fn whitespace_is_folded() {
        let ix = SearchIndex::build("line one\r\nline\ttwo");
        assert!(score(&ix, &prepare("one line")).is_some());
        assert!(score(&ix, &prepare("line two")).is_some_and(|s| s >= SUBSTR_BASE));
        assert_eq!(prepare("  foo  ").text, "foo");
    }

    #[test]
    fn long_text_is_truncated_at_char_boundary() {
        let ascii = format!("{}needle", "a".repeat(MAX_INDEXED_BYTES + 100));
        let ix = SearchIndex::build(&ascii);
        assert_eq!(ix.text().len(), MAX_INDEXED_BYTES);
        assert!(score(&ix, &prepare("needle")).is_none());
        assert!(score(&ix, &prepare("aaaa")).is_some());

        let wide = "\u{e9}".repeat(MAX_INDEXED_BYTES); // 2 bytes each
        let ix = SearchIndex::build(&wide);
        assert!(ix.text().len() <= MAX_INDEXED_BYTES && ix.text().len() >= MAX_INDEXED_BYTES - 1);

        // substring deep in a long text (past the fuzzy window) is still found
        let deep = format!("{}needle", "a ".repeat(100_000));
        let ix = SearchIndex::build(&deep);
        assert!(score(&ix, &prepare("needle")).is_some_and(|s| s >= SUBSTR_BASE));
        // ... but scattered matches only look at the head
        let far = format!("{}xyz", "a ".repeat(100_000));
        assert!(score(&SearchIndex::build(&far), &prepare("xzy")).is_none());
    }

    #[test]
    fn degenerate_inputs_never_panic() {
        let weird = [
            "",
            " ",
            "\0",
            "\u{130}\u{130}\u{130}",
            "a",
            "ab",
            "\u{10FFFF}",
            "\r\n\r\n",
            "{{",
            "\u{1F600}x\u{1F600}",
        ];
        for t in weird {
            let ix = SearchIndex::build(t);
            for q in weird
                .iter()
                .chain(["abc", "abcdefghijklmnopqrstuvwxyz".repeat(5).as_str()].iter())
            {
                let p = prepare(q);
                let _ = score(&ix, &p);
                let _ = fuzzy_score(ix.text(), &p);
            }
        }
        // query longer than the text, and longer than the fuzzy cap
        let ix = SearchIndex::build("abc");
        assert!(score(&ix, &prepare("abcd")).is_none());
        let long = "ab".repeat(100);
        assert!(
            score(&SearchIndex::build(&long), &prepare(&long)).is_some_and(|s| s >= SUBSTR_BASE)
        );
        assert!(score(&SearchIndex::build(&long), &prepare(&format!("{long}x"))).is_none());
    }

    #[test]
    fn gate_never_excludes_a_match_randomised() {
        let mut rng = Lcg(42);
        let pool: Vec<char> =
            "abcdeABCDE xyz_./\\\n\t\u{e9}\u{c9}\u{f1}\u{130}\u{df}\u{3a3}\u{4e2d}\u{1F600}\0"
                .chars()
                .collect();
        let mut checked = 0;
        for _ in 0..400 {
            let len = rng.below(300);
            let text: String = (0..len).map(|_| pool[rng.below(pool.len())]).collect();
            let ix = SearchIndex::build(&text);
            let folded: Vec<char> = ix.text().chars().collect();
            let mut queries: Vec<String> = Vec::new();
            for _ in 0..12 {
                // random garbage, an exact substring, and a subsequence of the text
                queries.push(
                    (0..1 + rng.below(6))
                        .map(|_| pool[rng.below(pool.len())])
                        .collect(),
                );
                if !folded.is_empty() {
                    let a = rng.below(folded.len());
                    let b = (a + 1 + rng.below(8)).min(folded.len());
                    queries.push(folded[a..b].iter().collect());
                    let m = 1 + rng.below(8);
                    queries.push(
                        folded
                            .iter()
                            .filter(|_| rng.below(4) == 0)
                            .take(m)
                            .collect(),
                    );
                }
            }
            for qs in &queries {
                let q = prepare(qs);
                let direct = fuzzy_score(ix.text(), &q);
                if direct.is_some() {
                    assert!(
                        ix.may_match(&q),
                        "gate excluded a match: {qs:?} in {text:?}"
                    );
                    checked += 1;
                }
                assert_eq!(
                    score(&ix, &q),
                    direct,
                    "score != fuzzy_score for {qs:?} in {text:?}"
                );
            }
        }
        assert!(checked > 1000, "test is not exercising matches ({checked})");
    }

    #[test]
    fn derived_substrings_and_subsequences_always_match() {
        let mut rng = Lcg(7);
        for _ in 0..200 {
            let size = 200 + rng.below(1500);
            let text = text_of(&mut rng, size);
            let ix = SearchIndex::build(&text);
            let chars: Vec<char> = ix.text().chars().collect();
            let a = rng.below(chars.len() - 10);
            let sub: String = chars[a..a + 3 + rng.below(7)].iter().collect();
            if !sub.trim().is_empty() {
                assert!(
                    score(&ix, &prepare(&sub)).is_some_and(|s| s >= SUBSTR_BASE),
                    "{sub:?}"
                );
            }
            let head: Vec<char> = chars.iter().take(1000).copied().collect();
            let subseq: String = head.iter().filter(|_| rng.below(6) == 0).take(10).collect();
            if !subseq.trim().is_empty() {
                assert!(score(&ix, &prepare(&subseq)).is_some(), "{subseq:?}");
            }
        }
    }

    #[test]
    fn subsequence_scores_stay_below_substring_band() {
        let q = prepare(&"a".repeat(64));
        let best = fuzzy_score(&"a".repeat(100), &q); // also an exact substring here
        assert!(best.is_some_and(|s| s >= SUBSTR_BASE));
        let text = "a b".repeat(40) + &"a".repeat(64);
        let s = fuzzy_score(&text[..50], &prepare("aaaaaaaa"));
        assert!(s.is_some_and(|s| (1..SUBSTR_BASE).contains(&s)));
    }

    #[test]
    fn rank_strs_matches_names() {
        let names = ["Greeting", "Signature", "Gr", "Address"];
        let r = rank_strs(names.iter().copied(), &prepare("gr"));
        assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), vec![2, 0, 1]); // "Si[g]natu[r]e" last
        assert_eq!(rank_strs(names.iter().copied(), &prepare("sgn")).len(), 1);
    }

    /// Sanity guard for the 10 ms p95 budget (spec 3): 2,000 items x ~1 KB.
    #[test]
    fn search_speed_2000_items() {
        let mut rng = Lcg(2024);
        let t0 = Instant::now();
        let ixs: Vec<SearchIndex> = (0..2000)
            .map(|_| SearchIndex::build(&text_of(&mut rng, 1024)))
            .collect();
        println!("build 2000 x ~1KB: {:?}", t0.elapsed());
        let mut worst = std::time::Duration::ZERO;
        for qs in [
            "c",
            "ex",
            "config",
            "hlo wrld",
            "fn main",
            "xqz",
            "getUser",
            "quikbrwn",
            "https://example.com",
            "zzzz",
            "stra\u{df}e ok",
            "dlrow olleh",
            "enoc",
        ] {
            let q = prepare(qs);
            let t = Instant::now();
            let r = rank(ixs.iter(), &q);
            let el = t.elapsed();
            worst = worst.max(el);
            println!("query {qs:?}: {:?} ({} hits)", el, r.len());
        }
        let limit = if cfg!(debug_assertions) { 100 } else { 10 }; // spec 3: 10 ms p95 (release)
        assert!(worst.as_millis() < limit, "search too slow: {worst:?}");
    }

    #[test]
    fn build_speed_500kb() {
        let text = "lorem ipsum dolor sit amet ".repeat(30_000); // ~800 KB, truncated
        let t = Instant::now();
        let ix = SearchIndex::build(&text);
        println!("build 800KB (truncated to 500KB): {:?}", t.elapsed());
        assert_eq!(ix.text().len(), MAX_INDEXED_BYTES);
        assert!(t.elapsed().as_millis() < 500);
    }
}
