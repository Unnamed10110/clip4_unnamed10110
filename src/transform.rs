//! Text transforms and smart paste (spec 12.3, 13, 14). Pure functions over untrusted text.

use crate::preview::{decode_entities, html_fragment, rtf_to_text};
use std::fmt::Write as _;

/// Query parameters removed by "Clean URL" besides `utm_*` (spec 13), compared case-insensitively.
const TRACKING_PARAMS: &[&str] = &[
    "fbclid", "gclid", "gclsrc", "dclid", "msclkid", "mc_eid", "mc_cid", "igshid", "igsh", "si", "ref_src", "ref_url",
    "_ga", "_gl", "yclid", "wbraid", "gbraid", "vero_id", "oly_anon_id", "oly_enc_id", "s_kwcid", "spm", "scid",
    "mkt_tok", "twclid", "ttclid",
];

// ------------------------------------------------------------------ URLs

/// Part of `url` after an `http://` / `https://` scheme (case-insensitive).
fn after_scheme(url: &str) -> Option<&str> {
    ["https://", "http://"]
        .iter()
        .find(|p| url.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p)))
        .and_then(|p| url.get(p.len()..))
}

/// True if the trimmed text is exactly one http(s) URL with a host and no whitespace.
pub fn is_single_url(text: &str) -> bool {
    let t = text.trim();
    after_scheme(t).is_some_and(|rest| !rest.starts_with(['/', '?', '#']) && !rest.is_empty())
        && !t.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn is_tracking(param: &str) -> bool {
    let name = param.split('=').next().unwrap_or_default();
    name.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("utm_")) || TRACKING_PARAMS.iter().any(|t| t.eq_ignore_ascii_case(name))
}

/// The URL without tracking parameters; fragment, order and raw text of the remaining
/// parameters are kept. `None` if `text` is not a single http(s) URL.
pub fn clean_url(text: &str) -> Option<String> {
    if !is_single_url(text) {
        return None;
    }
    let url = text.trim();
    let (head, fragment) = url.split_once('#').map_or((url, None), |(h, f)| (h, Some(f)));
    let Some((base, query)) = head.split_once('?') else {
        return Some(url.to_string());
    };
    if !query.split('&').any(is_tracking) {
        return Some(url.to_string());
    }
    let kept: Vec<&str> = query.split('&').filter(|p| !p.is_empty() && !is_tracking(p)).collect();
    let mut out = base.to_string();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    Some(out)
}

/// `<title>` text of an HTML document, entity-decoded with whitespace collapsed.
pub fn html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase(); // same byte offsets as `html`
    let mut from = 0;
    let start = loop {
        let s = from + lower.get(from..)?.find("<title")? + "<title".len();
        if matches!(lower.as_bytes().get(s), Some(b'>' | b' ' | b'\t' | b'\r' | b'\n')) {
            break s;
        }
        from = s;
    };
    let open_end = start + lower.get(start..)?.find('>')? + 1;
    let close = open_end + lower.get(open_end..)?.find("</title")?;
    let text = decode_entities(html.get(open_end..close)?);
    let title = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then_some(title)
}

/// Host part of an http(s) URL (no userinfo, no port).
fn url_host(url: &str) -> &str {
    let rest = after_scheme(url).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    match host.strip_prefix('[') {
        Some(v6) => host.get(..v6.find(']').map_or(host.len(), |p| p + 2)).unwrap_or(host),
        None => host.split(':').next().unwrap_or_default(),
    }
}

/// `[title](url)` for a URL (title = HTML title, else host); inline code for anything else.
pub fn markdown_link(text: &str, html_title: Option<&str>) -> String {
    if is_single_url(text) {
        let url = text.trim();
        let title = html_title
            .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| url_host(url).to_string());
        let mut escaped = String::with_capacity(title.len());
        for c in title.chars() {
            if matches!(c, '[' | ']' | '\\') {
                escaped.push('\\');
            }
            escaped.push(c);
        }
        return format!("[{escaped}]({url})");
    }
    if text.is_empty() {
        return String::new();
    }
    // The fence is longer than any backtick run inside; padded if the text edges are backticks.
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') { " " } else { "" };
    format!("{fence}{pad}{text}{pad}{fence}")
}

// ------------------------------------------------------------------ case transforms

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseOp {
    Upper,
    Lower,
    Title,
    RemoveLineBreaks,
    Trim,
    Plain,
}

pub fn apply_case(op: CaseOp, text: &str) -> String {
    match op {
        CaseOp::Upper => text.to_uppercase(),
        CaseOp::Lower => text.to_lowercase(),
        CaseOp::Title => {
            let mut out = String::with_capacity(text.len());
            let mut word_start = true; // no letter/digit seen yet in the current word
            for c in text.chars() {
                if c.is_whitespace() {
                    word_start = true;
                    out.push(c);
                } else if c.is_alphabetic() && word_start {
                    word_start = false;
                    out.extend(c.to_uppercase());
                } else if c.is_alphanumeric() {
                    word_start = false;
                    out.extend(c.to_lowercase());
                } else {
                    out.push(c);
                }
            }
            out
        }
        CaseOp::RemoveLineBreaks => {
            let mut out = String::with_capacity(text.len());
            let mut in_break = false;
            for c in text.chars() {
                if matches!(c, '\r' | '\n') {
                    if !in_break {
                        out.push(' ');
                    }
                    in_break = true;
                } else {
                    in_break = false;
                    out.push(c);
                }
            }
            out.trim().to_string()
        }
        CaseOp::Trim => text.trim().to_string(),
        CaseOp::Plain => text.to_string(),
    }
}

// ------------------------------------------------------------------ multi-paste merges

/// Unicode texts joined with `\r\n` (spec 12.3).
pub fn merge_unicode(texts: &[String]) -> String {
    texts.join("\r\n")
}

/// Text handed to the editor for a multi-selection.
pub fn edit_text_join(texts: &[String]) -> String {
    texts.join("\r\n")
}

/// RTF-escapes plain text: `\uN?` for non-ASCII, `\par` for line breaks, `\tab` for tabs.
fn rtf_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\\par ");
            }
            '\t' => out.push_str("\\tab "),
            '\\' | '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            ' '..='~' => out.push(c),
            c if c.is_ascii() => {} // other control chars
            c => {
                for u in c.encode_utf16(&mut [0; 2]) {
                    let signed = i16::from_le_bytes(u.to_le_bytes());
                    let _ = write!(out, "\\u{signed}?");
                }
            }
        }
    }
    out
}

/// Minimal valid RTF document for plain text.
pub fn rtf_from_text(text: &str) -> Vec<u8> {
    format!("{{\\rtf1\\ansi\\deff0 {}}}", rtf_escape(text)).into_bytes()
}

/// Index of the `}` closing the group opened by the `{` at `start`.
fn group_end(b: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = start;
    while let Some(&c) = b.get(i) {
        match c {
            b'\\' => i += 1, // skip the escaped byte
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Index after the control word / symbol at `b[i] == '\\'` (and its parameter and delimiter).
fn word_end(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    if b.get(j).is_some_and(u8::is_ascii_alphabetic) {
        while b.get(j).is_some_and(u8::is_ascii_alphabetic) {
            j += 1;
        }
        if b.get(j) == Some(&b'-') && b.get(j + 1).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        if b.get(j) == Some(&b' ') {
            j += 1;
        }
        j
    } else {
        j + 1
    }
}

/// Document-level tables that precede the text in an RTF header.
const RTF_HEADER_GROUPS: &[&str] = &["fonttbl", "colortbl", "stylesheet", "info", "listtable", "listoverridetable", "rsidtbl"];

fn is_header_group(group_content: &[u8]) -> bool {
    let Some(w) = group_content.strip_prefix(b"\\") else {
        return false;
    };
    let n = w.iter().take_while(|c| c.is_ascii_alphabetic()).count();
    w.first() == Some(&b'*') || w.get(..n).is_some_and(|name| RTF_HEADER_GROUPS.iter().any(|h| h.as_bytes() == name))
}

/// Splits a balanced RTF document into (prolog, body): the prolog is `\rtf1`, the
/// control words before the header tables and the tables themselves; the body is the text.
fn rtf_parts(doc: &[u8]) -> Option<(&[u8], &[u8])> {
    let first = doc.iter().position(|&c| !c.is_ascii_whitespace() && c != 0)?;
    let doc = doc.get(first..)?;
    if !doc.starts_with(b"{\\rtf") {
        return None;
    }
    let inner = doc.get(1..group_end(doc, 0)?)?;
    let mut i = word_end(inner, 0); // past "\rtf1"
    let mut split = i;
    loop {
        match inner.get(i) {
            Some(b'{') => {
                let end = group_end(inner, i)?;
                if !is_header_group(inner.get(i + 1..end)?) {
                    break;
                }
                i = end + 1;
                split = i;
            }
            Some(b'\\') => i = word_end(inner, i),
            Some(b' ' | b'\r' | b'\n') => i += 1,
            _ => break,
        }
    }
    inner.split_at_checked(split)
}

/// Combines RTF documents into one, bodies separated by `\par`. The first RTF part supplies
/// the font/colour tables; parts that are not (balanced) RTF are escaped as plain text.
// ponytail: later parts keep their own \fN / \cfN indices, so they may pick up the first
// document's fonts and colours. Remap the tables if mixed-format multi-paste matters.
pub fn merge_rtf(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut prolog: Option<&[u8]> = None;
    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(parts.len());
    for p in parts {
        match rtf_parts(p) {
            Some((pro, body)) => {
                prolog.get_or_insert(pro);
                bodies.push(body.to_vec());
            }
            None => {
                let trimmed = p.trim_ascii_start();
                let text = if trimmed.starts_with(b"{\\rtf") { rtf_to_text(p) } else { String::from_utf8_lossy(p).into_owned() };
                bodies.push(rtf_escape(&text).into_bytes());
            }
        }
    }
    let mut out = b"{".to_vec();
    out.extend_from_slice(prolog.unwrap_or(b"\\rtf1\\ansi\\deff0 "));
    if out.last().is_some_and(u8::is_ascii_alphanumeric) {
        out.push(b' '); // keep a digit-leading body from extending \rtf1
    }
    for (n, body) in bodies.iter().enumerate() {
        if n > 0 && !out.trim_ascii_end().ends_with(b"\\par") {
            out.extend_from_slice(b"\\par ");
        }
        out.extend_from_slice(body);
    }
    out.push(b'}');
    out
}

/// `"HTML Format"` payload for `fragment` (spec: offsets are zero-padded 10-digit byte counts).
pub fn make_html_format(fragment: &str) -> Vec<u8> {
    const PRE: &str = "<html><body>\r\n<!--StartFragment-->";
    const POST: &str = "<!--EndFragment-->\r\n</body>\r\n</html>";
    let header = |sh: usize, eh: usize, sf: usize, ef: usize| {
        format!("Version:0.9\r\nStartHTML:{sh:010}\r\nEndHTML:{eh:010}\r\nStartFragment:{sf:010}\r\nEndFragment:{ef:010}\r\n")
    };
    let start_html = header(0, 0, 0, 0).len();
    let start_fragment = start_html + PRE.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + POST.len();
    let mut out = header(start_html, end_html, start_fragment, end_fragment).into_bytes();
    out.extend_from_slice(PRE.as_bytes());
    out.extend_from_slice(fragment.as_bytes());
    out.extend_from_slice(POST.as_bytes());
    out
}

/// Merges `"HTML Format"` payloads: fragments joined with `<br>`.
pub fn merge_html(parts: &[Vec<u8>]) -> Vec<u8> {
    let fragments: Vec<String> = parts
        .iter()
        .filter_map(|p| html_fragment(p))
        // Some producers put their offsets outside the markers; keep exactly one pair.
        .map(|f| f.replace("<!--StartFragment-->", "").replace("<!--EndFragment-->", ""))
        .collect();
    make_html_format(&fragments.join("<br>"))
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn below(&mut self, n: usize) -> usize {
            self.next() as usize % n.max(1)
        }
        fn soup(&mut self, toks: &[&str], n: usize) -> Vec<u8> {
            let mut v = Vec::new();
            for _ in 0..n {
                if self.below(8) == 0 {
                    v.push(self.next() as u8);
                } else {
                    v.extend(toks[self.below(toks.len())].as_bytes());
                }
            }
            v
        }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ---- is_single_url / clean_url

    #[test]
    fn single_url_detection() {
        for ok in ["http://a.b", "https://example.com/path?x=1#f", "  https://x.y  \r\n", "HTTP://EXAMPLE.COM", "https://u:p@h.io:8080/"] {
            assert!(is_single_url(ok), "{ok:?}");
        }
        for bad in [
            "", "   ", "http://", "https:// x", "see https://x.y", "ftp://x.y", "https://a.b c", "www.x.y", "https://a.b\nhttps://c.d",
            "https:///path", "https://?x", "https://#f", "<https://x.y>", "https://x.y\u{0}", "htt\u{E9}://x.y",
        ] {
            assert!(!is_single_url(bad), "{bad:?}");
        }
    }

    #[test]
    fn clean_url_removes_every_listed_name() {
        for name in TRACKING_PARAMS {
            for n in [name.to_string(), name.to_uppercase()] {
                let want = "https://example.com/p?keep=1&z=2#frag";
                assert_eq!(clean_url(&format!("https://example.com/p?keep=1&{n}=xyz&z=2#frag")).as_deref(), Some(want), "{n} middle");
                assert_eq!(clean_url(&format!("https://example.com/p?{n}=xyz&keep=1&z=2#frag")).as_deref(), Some(want), "{n} first");
                assert_eq!(clean_url(&format!("https://example.com/p?keep=1&z=2&{n}=xyz#frag")).as_deref(), Some(want), "{n} last");
                assert_eq!(clean_url(&format!("https://example.com/p?keep=1&{n}&z=2#frag")).as_deref(), Some(want), "{n} valueless");
                assert_eq!(clean_url(&format!("https://example.com/p?{n}=xyz")).as_deref(), Some("https://example.com/p"), "{n} only");
            }
        }
        assert_eq!(TRACKING_PARAMS.len(), 26);
    }

    #[test]
    fn clean_url_utm_prefix() {
        let u = "https://x.com/?a=1&utm_source=n&UTM_Medium=m&utm_campaign=c&utm_content=&utm_term=t&utm_=z&utm_id=7&b=2";
        assert_eq!(clean_url(u).as_deref(), Some("https://x.com/?a=1&b=2"));
        // Similar-looking names are kept.
        let keep = "https://x.com/?utm=1&xutm_source=2&sid=3&spm_x=4&ref=5&gclid2=6&_gax=7&si_x=8&utm%5Fsource=9";
        assert_eq!(clean_url(keep).as_deref(), Some(keep));
    }

    #[test]
    fn clean_url_preserves_order_raw_text_and_fragment() {
        let u = "https://x.com/a/b?b=%20c&utm_source=t&a=1&q=a+b%2Fc&c&d=x=y&fbclid=1#sec?utm_x=1&y";
        assert_eq!(clean_url(u).as_deref(), Some("https://x.com/a/b?b=%20c&a=1&q=a+b%2Fc&c&d=x=y#sec?utm_x=1&y"));
        assert_eq!(clean_url("https://x.com/?utm_source=a&fbclid=b#top").as_deref(), Some("https://x.com/#top"));
        assert_eq!(clean_url("https://x.com/?utm_source=a&fbclid=b").as_deref(), Some("https://x.com/"));
        assert_eq!(clean_url("https://x.com?utm_source=a").as_deref(), Some("https://x.com"));
        assert_eq!(clean_url("HTTPS://X.COM/?GCLID=1&Keep=Me").as_deref(), Some("HTTPS://X.COM/?Keep=Me"));
    }

    #[test]
    fn clean_url_untouched_when_nothing_to_remove() {
        for u in [
            "https://x.com/", "https://x.com/?", "https://x.com/?a=1&&b=2", "https://x.com/?a=1&", "https://x.com/#/r?utm_x=1",
            "https://x.com/p?a=1#f", "http://x.com/?=v&&",
        ] {
            assert_eq!(clean_url(u).as_deref(), Some(u), "{u}");
        }
        // Empty segments are dropped only when something was actually removed.
        assert_eq!(clean_url("https://x.com/?utm_a=1&").as_deref(), Some("https://x.com/"));
        assert_eq!(clean_url("https://x.com/?a=1&&gclid=2&").as_deref(), Some("https://x.com/?a=1"));
    }

    #[test]
    fn clean_url_trims_and_rejects_non_urls() {
        assert_eq!(clean_url(" https://x.com/?gclid=1 \r\n").as_deref(), Some("https://x.com/"));
        for bad in ["hello", "", "ftp://x.com/?gclid=1", "https://a b/?gclid=1", "gclid=1", "text https://x.com/?gclid=1"] {
            assert_eq!(clean_url(bad), None, "{bad:?}");
        }
    }

    // ---- html_title / markdown_link

    #[test]
    fn html_title_extraction() {
        assert_eq!(html_title("<html><head><title>Hello</title></head>").as_deref(), Some("Hello"));
        assert_eq!(html_title("<TITLE lang=en>  A &amp; B\r\n  &lt;C&gt;&nbsp;D </TITLE>").as_deref(), Some("A & B <C> D"));
        assert_eq!(html_title("<titles>no</titles><title>yes</title>").as_deref(), Some("yes"));
        assert_eq!(html_title("<title></title>"), None);
        assert_eq!(html_title("<title>   </title>"), None);
        assert_eq!(html_title("<title>unterminated"), None);
        assert_eq!(html_title("<title"), None);
        assert_eq!(html_title("no title here"), None);
        assert_eq!(html_title("<title>caf\u{E9} \u{1F600}</title>").as_deref(), Some("caf\u{E9} \u{1F600}"));
        // Non-ASCII before the tag keeps offsets aligned.
        assert_eq!(html_title("\u{E9}\u{E9}<TiTle>X</TITLE>").as_deref(), Some("X"));
        // The whole "HTML Format" payload works too.
        let payload = String::from_utf8(make_html_format("<p>x</p>")).unwrap();
        assert_eq!(html_title(&payload), None);
    }

    #[test]
    fn markdown_links() {
        assert_eq!(markdown_link("https://www.rust-lang.org/learn?x=1", Some("Learn Rust")), "[Learn Rust](https://www.rust-lang.org/learn?x=1)");
        assert_eq!(markdown_link("  https://www.rust-lang.org/learn \n", None), "[www.rust-lang.org](https://www.rust-lang.org/learn)");
        assert_eq!(markdown_link("https://u:p@host.io:8080/x", None), "[host.io](https://u:p@host.io:8080/x)");
        assert_eq!(markdown_link("http://[::1]:80/x", None), "[\\[::1\\]](http://[::1]:80/x)");
        assert_eq!(markdown_link("https://a.b", Some("")), "[a.b](https://a.b)");
        assert_eq!(markdown_link("https://a.b", Some("  \n ")), "[a.b](https://a.b)");
        assert_eq!(markdown_link("https://a.b", Some("A [b]\n c\\d")), "[A \\[b\\] c\\\\d](https://a.b)");
    }

    #[test]
    fn markdown_code_spans() {
        assert_eq!(markdown_link("hello world", None), "`hello world`");
        assert_eq!(markdown_link("hello world", Some("ignored title")), "`hello world`");
        assert_eq!(markdown_link("a `b` c", None), "``a `b` c``");
        assert_eq!(markdown_link("a ``b`` c ` d", None), "```a ``b`` c ` d```");
        assert_eq!(markdown_link("`x`", None), "`` `x` ``");
        assert_eq!(markdown_link("x`", None), "`` x` ``");
        assert_eq!(markdown_link("see https://x.y", None), "`see https://x.y`");
        assert_eq!(markdown_link("", None), "");
    }

    // ---- apply_case

    #[test]
    fn case_ops() {
        assert_eq!(apply_case(CaseOp::Upper, "Hello \u{E9}\u{DF}"), "HELLO \u{C9}SS");
        assert_eq!(apply_case(CaseOp::Lower, "HeLLo \u{C9}"), "hello \u{E9}");
        assert_eq!(apply_case(CaseOp::Title, "hELLO wORLD  foo\tbar\nbaz"), "Hello World  Foo\tBar\nBaz");
        assert_eq!(apply_case(CaseOp::Title, "(quoted) 3rd o'neil \u{E9}lan \u{1F600}x \"hi\" a-b"), "(Quoted) 3rd O'neil \u{C9}lan \u{1F600}X \"Hi\" A-b");
        assert_eq!(apply_case(CaseOp::Title, ""), "");
        assert_eq!(apply_case(CaseOp::RemoveLineBreaks, "a\r\nb\n\nc\rd  \r\n"), "a b c d");
        assert_eq!(apply_case(CaseOp::RemoveLineBreaks, "\n\n  a\r\n b \n"), "a  b");
        assert_eq!(apply_case(CaseOp::RemoveLineBreaks, "\r\n"), "");
        assert_eq!(apply_case(CaseOp::Trim, " \t a b\r\n "), "a b");
        assert_eq!(apply_case(CaseOp::Trim, "a \n b"), "a \n b");
        assert_eq!(apply_case(CaseOp::Plain, " \t keep\r\n "), " \t keep\r\n ");
    }

    // ---- merges

    #[test]
    fn unicode_merge_and_edit_join() {
        assert_eq!(merge_unicode(&strs(&["a", "b", "c"])), "a\r\nb\r\nc");
        assert_eq!(edit_text_join(&strs(&["a", "b"])), "a\r\nb");
        assert_eq!(merge_unicode(&strs(&["only"])), "only");
        assert_eq!(merge_unicode(&[]), "");
        assert_eq!(edit_text_join(&strs(&["", ""])), "\r\n");
    }

    #[test]
    fn rtf_from_text_round_trips_through_rtf_to_text() {
        for t in ["plain", "a\r\nb", "a\nb", "a\rb", "tab\there", "{braces} \\ back", "caf\u{E9} \u{20AC} \u{1F600}", " leading", "", "l1\n\nl3", "a\u{7f}b\u{1}c", "\u{10FFFF}\u{FFFF}", "50% \\u123? {\\par}"] {
            let rtf = rtf_from_text(t);
            assert!(rtf.is_ascii());
            assert!(rtf.starts_with(b"{\\rtf1\\ansi\\deff0 ") && rtf.ends_with(b"}"));
            assert_eq!(group_end(&rtf, 0), Some(rtf.len() - 1));
            let want = t.replace("\r\n", "\n").replace('\r', "\n").replace(['\u{7f}', '\u{1}'], "");
            assert_eq!(rtf_to_text(&rtf), want.trim_end_matches('\n'), "{t:?}");
        }
    }

    #[test]
    fn rtf_from_text_exact_output() {
        assert_eq!(String::from_utf8(rtf_from_text("a\u{E9}\u{20AC}\r\nb\t{")).unwrap(), "{\\rtf1\\ansi\\deff0 a\\u233?\\u8364?\\par b\\tab \\{}");
        assert_eq!(String::from_utf8(rtf_from_text("\u{1F600}")).unwrap(), "{\\rtf1\\ansi\\deff0 \\u-10179?\\u-8704?}");
    }

    const WORD1: &[u8] = b"{\\rtf1\\ansi\\ansicpg1252\\deff0{\\fonttbl{\\f0 Arial;}}{\\colortbl ;\\red255\\green0\\blue0;}\\viewkind4\\uc1\\pard\\f0 One\\par\r\n}";
    const WORD2: &[u8] = b"{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Times;}}{\\*\\generator X}\\pard Two\\par\r\n}\0\0";

    fn balanced(rtf: &[u8]) -> bool {
        group_end(rtf, 0) == Some(rtf.len().wrapping_sub(1))
    }

    #[test]
    fn rtf_parts_split() {
        let (pro, body) = rtf_parts(WORD1).unwrap();
        assert!(pro.starts_with(b"\\rtf1\\ansi") && pro.ends_with(b"\\blue0;}"));
        assert!(body.starts_with(b"\\viewkind4"));
        let (pro, body) = rtf_parts(b"{\\rtf1\\ansi\\deff0 Hello}").unwrap();
        assert_eq!((pro, body), (&b"\\rtf1"[..], &b"\\ansi\\deff0 Hello"[..]));
        assert!(rtf_parts(b"not rtf").is_none());
        assert!(rtf_parts(b"{\\rtf1 unbalanced").is_none());
        assert!(rtf_parts(b"").is_none());
        assert!(rtf_parts(b"  \r\n{\\rtf1 x}").is_some());
    }

    #[test]
    fn merge_rtf_documents() {
        let m = merge_rtf(&[WORD1.to_vec(), WORD2.to_vec()]);
        assert!(m.starts_with(b"{\\rtf1\\ansi\\ansicpg1252") && balanced(&m));
        assert_eq!(rtf_to_text(&m), "One\nTwo");
        // Only the first document's tables are kept.
        let s = String::from_utf8(m).unwrap();
        assert_eq!(s.matches("\\fonttbl").count(), 1);
        assert!(s.contains("Arial") && !s.contains("Times") && !s.contains("generator"));
        // Bodies that do not end with \par get a separator.
        let m = merge_rtf(&[b"{\\rtf1 one}".to_vec(), b"{\\rtf1 two}".to_vec(), b"{\\rtf1\\ansi three}".to_vec()]);
        assert_eq!(rtf_to_text(&m), "one\ntwo\nthree");
        assert!(balanced(&m));
    }

    #[test]
    fn merge_rtf_mixed_and_garbage_parts() {
        let m = merge_rtf(&[b"123 plain {x}".to_vec(), WORD2.to_vec(), "caf\u{E9} \u{1F600}".as_bytes().to_vec(), b"{\\rtf1 unbalanced {".to_vec(), vec![0xFF, 0xFE, b'\\', b'{']]);
        assert!(balanced(&m) && m.starts_with(b"{\\rtf1"));
        assert_eq!(rtf_to_text(&m), "123 plain {x}\nTwo\ncaf\u{E9} \u{1F600}\nunbalanced \n\u{FFFD}\u{FFFD}\\{");
        let m = merge_rtf(&[b"42".to_vec(), b"{\\rtf1 x}".to_vec()]);
        assert_eq!(rtf_to_text(&m), "42\nx");
        assert_eq!(rtf_to_text(&merge_rtf(&[])), "");
        assert!(balanced(&merge_rtf(&[])));
        assert_eq!(rtf_to_text(&merge_rtf(&[WORD1.to_vec()])), "One");
        assert_eq!(rtf_to_text(&merge_rtf(&[Vec::new(), b"a".to_vec()])), "\na");
    }

    #[test]
    fn make_html_format_offsets() {
        for frag in ["<b>x</b>", "plain", "caf\u{E9} \u{1F600} <i>y</i>", "multi\r\nline <p>a</p>"] {
            let b = make_html_format(frag);
            let text = std::str::from_utf8(&b).unwrap();
            assert!(text.starts_with("Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:"));
            let num = |key: &str| -> usize {
                let at = text.find(key).unwrap() + key.len();
                let digits = &text[at..at + 10];
                assert!(digits.bytes().all(|c| c.is_ascii_digit()));
                digits.parse().unwrap()
            };
            let (sh, eh, sf, ef) = (num("StartHTML:"), num("EndHTML:"), num("StartFragment:"), num("EndFragment:"));
            assert_eq!(sh, 105);
            assert_eq!(text[..sh].matches("\r\n").count(), 5);
            assert_eq!(eh, b.len());
            assert_eq!(&b[sf..ef], frag.as_bytes());
            assert!(text[sh..].starts_with("<html>") && text.ends_with("</html>"));
            assert!(text[..sf].ends_with("<!--StartFragment-->") && text[ef..].starts_with("<!--EndFragment-->"));
            assert_eq!(html_fragment(&b).as_deref(), Some(frag));
        }
        // Empty fragment: a valid, empty fragment (no panic, nothing to paste).
        assert_eq!(html_fragment(&make_html_format("")), None);
    }

    #[test]
    fn merge_html_joins_fragments() {
        let parts = vec![make_html_format("<b>a</b>"), make_html_format("b &amp; c"), b"<i>raw, no header</i>".to_vec(), make_html_format(""), Vec::new()];
        let m = merge_html(&parts);
        assert_eq!(html_fragment(&m).as_deref(), Some("<b>a</b><br>b &amp; c<br><i>raw, no header</i>"));
        assert_eq!(crate::preview::html_to_text(&html_fragment(&m).unwrap()), "a\nb & c\nraw, no header");
        assert_eq!(html_fragment(&merge_html(&[make_html_format("solo")])).as_deref(), Some("solo"));
        // Markers inside a fragment (offsets outside them) are not duplicated.
        let m = String::from_utf8(merge_html(&[b"<!--StartFragment-->x<!--EndFragment-->".to_vec(), make_html_format("y")])).unwrap();
        assert_eq!(m.matches("<!--StartFragment-->").count(), 1);
        assert_eq!(m.matches("<!--EndFragment-->").count(), 1);
        assert_eq!(html_fragment(m.as_bytes()).as_deref(), Some("x<br>y"));
        assert_eq!(html_fragment(&merge_html(&[])), None);
    }

    // ---- never panic

    #[test]
    fn fuzz_text_transforms_never_panic() {
        let toks = ["http://", "https://", "?", "&", "=", "#", "utm_", "gclid", "a", " ", "\n", "\u{E9}", "\u{1F600}", "[", "]", "`", "@", ":", "<title>", "</title>", "<TITLE", "&amp;", "/"];
        let mut r = Lcg(99);
        for _ in 0..3000 {
            let n = r.below(30);
            let s = String::from_utf8_lossy(&r.soup(&toks, n)).into_owned();
            let _ = (is_single_url(&s), clean_url(&s), html_title(&s), markdown_link(&s, Some(&s)), markdown_link(&s, None));
            for op in [CaseOp::Upper, CaseOp::Lower, CaseOp::Title, CaseOp::RemoveLineBreaks, CaseOp::Trim, CaseOp::Plain] {
                let _ = apply_case(op, &s);
            }
            if let Some(u) = clean_url(&s) {
                assert!(u.len() <= s.trim().len());
                assert!(is_single_url(&u));
            }
            let _ = rtf_from_text(&s);
        }
    }

    #[test]
    fn fuzz_merges_never_panic_and_stay_balanced() {
        let rtf = ["{", "}", "\\", "\\'", "\\u", "\\u-1?", "\\par", "\\*", "\\bin5", "\\fonttbl", "\\colortbl", "abc", " ", "{\\rtf1", "{\\rtf1\\ansi", "{\\fonttbl{\\f0 A;}}", "{\\*\\x y}", "\\{", "\\}", "\\\\", "1"];
        let html = ["Version:0.9\r\n", "StartHTML:", "StartFragment:", "EndFragment:", "EndHTML:", "0000000105", "99999999999", "<html>", "<!--StartFragment-->", "x", "\r\n", ":"];
        let mut r = Lcg(5);
        for _ in 0..1500 {
            let parts: Vec<Vec<u8>> = (0..1 + r.below(4))
                .map(|_| {
                    let n = r.below(40);
                    if r.below(2) == 0 {
                        let mut v = b"{\\rtf1".to_vec();
                        v.extend(r.soup(&rtf, n));
                        v
                    } else {
                        r.soup(&rtf, n)
                    }
                })
                .collect();
            let m = merge_rtf(&parts);
            assert!(m.starts_with(b"{\\rtf1") && balanced(&m), "{:?}", String::from_utf8_lossy(&m));
            let _ = rtf_to_text(&m);
            let hparts: Vec<Vec<u8>> = (0..1 + r.below(4)).map(|_| r.soup(&html, 12)).collect();
            let h = merge_html(&hparts);
            assert!(h.starts_with(b"Version:0.9\r\nStartHTML:0000000105\r\n"));
            let _ = html_fragment(&h);
        }
    }
}
