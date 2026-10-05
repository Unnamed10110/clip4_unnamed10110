//! Snippets (spec 14): the pure parts. Storage format, validation, placeholder
//! expansion (plain and RTF) and RTF -> plain text. No registry, no I/O.

use std::borrow::Cow;

/// Registry REG_SZ guard: values of 32,767 chars or more are not stored.
pub const MAX_VALUE_CHARS: usize = 32766;
/// Opens the snippet manager in the overlay; cannot be a snippet name.
pub const RESERVED_NAME: &str = "*set";

const SEP_CONTENT: char = '\u{1}';
const SEP_PLAIN: char = '\u{2}';

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Snippet {
    pub name: String,
    /// Plain text, or RTF source (starts with `{\rtf`).
    pub content: String,
    pub content_plain: Option<String>,
}

fn looks_rtf(s: &str) -> bool {
    s.trim_start().starts_with("{\\rtf")
}

impl Snippet {
    pub fn is_rtf(&self) -> bool {
        looks_rtf(&self.content)
    }
}

/// `name U+0001 content [U+0002 content_plain]`. The caller must [`validate`] first:
/// a U+0001/U+0002 inside a field would corrupt the round trip.
pub fn encode(s: &Snippet) -> String {
    let mut v = format!("{}{SEP_CONTENT}{}", s.name, s.content);
    if let Some(p) = &s.content_plain {
        v.push(SEP_PLAIN);
        v.push_str(p);
    }
    v
}

/// Inverse of [`encode`]; `None` when there is no U+0001 separator.
pub fn decode(value: &str) -> Option<Snippet> {
    let (name, rest) = value.split_once(SEP_CONTENT)?;
    let (content, content_plain) = match rest.split_once(SEP_PLAIN) {
        Some((c, p)) => (c, Some(p.to_string())),
        None => (rest, None),
    };
    Some(Snippet {
        name: name.to_string(),
        content: content.to_string(),
        content_plain,
    })
}

/// Length of [`encode`]'s result in UTF-16 code units, NOT counting the terminating NUL
/// the registry adds (so a value is storable iff this is <= [`MAX_VALUE_CHARS`]).
pub fn encoded_len(s: &Snippet) -> usize {
    let units = |t: &str| t.encode_utf16().count();
    units(&s.name) + 1 + units(&s.content) + s.content_plain.as_deref().map_or(0, |p| 1 + units(p))
}

/// User-readable reason a snippet cannot be saved.
pub fn validate(s: &Snippet) -> Result<(), String> {
    let name = s.name.trim();
    if name.is_empty() {
        return Err("The snippet needs a name.".into());
    }
    if name.eq_ignore_ascii_case(RESERVED_NAME) {
        return Err(format!(
            "\"{RESERVED_NAME}\" is reserved for the snippet manager; choose another name."
        ));
    }
    let fields = [
        Some(s.name.as_str()),
        Some(s.content.as_str()),
        s.content_plain.as_deref(),
    ];
    if fields
        .iter()
        .flatten()
        .any(|f| f.contains([SEP_CONTENT, SEP_PLAIN]))
    {
        return Err("The snippet contains a reserved control character (U+0001 or U+0002).".into());
    }
    let len = encoded_len(s);
    if len > MAX_VALUE_CHARS {
        return Err(format!(
            "The snippet is too long: {len} characters stored, the limit is {MAX_VALUE_CHARS} (over by {}).",
            len - MAX_VALUE_CHARS
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- placeholders

#[derive(Clone, Debug, Default)]
pub struct Now {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// Locale-formatted by the caller.
    pub date_str: String,
    pub time_str: String,
}

fn value<'a>(name: &str, now: &'a Now, clipboard: &'a str) -> Option<Cow<'a, str>> {
    Some(match name.to_ascii_lowercase().as_str() {
        "date" => Cow::Borrowed(now.date_str.as_str()),
        "time" => Cow::Borrowed(now.time_str.as_str()),
        "datetime" => Cow::Owned(format!("{} {}", now.date_str, now.time_str)),
        "year" => Cow::Owned(format!("{:04}", now.year)),
        "month" => Cow::Owned(format!("{:02}", now.month)),
        "day" => Cow::Owned(format!("{:02}", now.day)),
        "hour" => Cow::Owned(format!("{:02}", now.hour)),
        "minute" => Cow::Owned(format!("{:02}", now.minute)),
        "second" => Cow::Owned(format!("{:02}", now.second)),
        "clipboard" => Cow::Borrowed(clipboard),
        _ => return None,
    })
}

/// Longest `{{name}}` body is "clipboard"/"datetime"; only this much is searched for the
/// closing braces so a stray `{{` cannot make expansion quadratic.
const MAX_BODY: usize = 14;

/// A placeholder at the start of `rest`: its value and the bytes it spans. In RTF mode the
/// editor-escaped form `\{\{name\}\}` is recognised as well as the literal `{{name}}`.
fn placeholder<'a>(
    rest: &str,
    now: &'a Now,
    clip: &'a str,
    rtf: bool,
) -> Option<(Cow<'a, str>, usize)> {
    let (open, close) = if rtf && rest.starts_with("\\{\\{") {
        ("\\{\\{", "\\}\\}")
    } else if rest.starts_with("{{") {
        ("{{", "}}")
    } else {
        return None;
    };
    let body = rest.as_bytes().get(open.len()..)?;
    let body = body.get(..body.len().min(MAX_BODY))?;
    let end = body
        .windows(close.len())
        .position(|w| w == close.as_bytes())?;
    let name = std::str::from_utf8(body.get(..end)?).ok()?;
    Some((value(name, now, clip)?, open.len() + end + close.len()))
}

/// Single pass: substituted text is never re-scanned. Unknown placeholders stay as is.
fn expand_with(template: &str, now: &Now, clip: &str, rtf: bool) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(c) = rest.chars().next() {
        match placeholder(rest, now, clip, rtf) {
            Some((v, n)) => {
                if rtf {
                    rtf_escape_into(&mut out, &v);
                } else {
                    out.push_str(&v);
                }
                rest = rest.get(n..).unwrap_or("");
            }
            None => {
                out.push(c);
                rest = rest.get(c.len_utf8()..).unwrap_or("");
            }
        }
    }
    out
}

/// Plain-text expansion of `{{date}} {{time}} {{datetime}} {{year}} {{month}} {{day}}
/// {{hour}} {{minute}} {{second}} {{clipboard}}` (case-insensitive names).
pub fn expand(template: &str, now: &Now, clipboard: &str) -> String {
    expand_with(template, now, clipboard, false)
}

/// Same placeholders inside RTF source; substituted values are RTF-escaped.
pub fn expand_rtf(template: &str, now: &Now, clipboard: &str) -> String {
    expand_with(template, now, clipboard, true)
}

/// Escapes plain text for embedding in RTF: `\ { }`, line breaks, tabs, non-ASCII as
/// `\uN?` (UTF-16 code units, signed), other control chars as `\'hh`.
fn rtf_escape_into(out: &mut String, text: &str) {
    use std::fmt::Write;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' | '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' | '\n' => out.push_str("\\par "),
            '\t' => out.push_str("\\tab "),
            c if c.is_ascii_control() => {
                let _ = write!(out, "\\'{:02x}", c as u32);
            }
            c if c.is_ascii() => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{}?", *u as i16);
                }
            }
        }
    }
}

// ---------------------------------------------------------------- RTF -> text

/// Windows-1252 0x80..=0x9F (the rest of cp1252 equals Latin-1).
const CP1252_HIGH: [u16; 32] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
    0x0152, 0x008D, 0x017D, 0x008F, 0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178,
];

fn cp1252(b: u8) -> char {
    let cp = match b {
        0x80..=0x9F => CP1252_HIGH
            .get(usize::from(b - 0x80))
            .copied()
            .map_or(u32::from(b), u32::from),
        _ => u32::from(b),
    };
    char::from_u32(cp).unwrap_or('\u{FFFD}')
}

/// Destinations whose content is not document text.
const SKIPPED: [&str; 19] = [
    "fonttbl",
    "colortbl",
    "stylesheet",
    "info",
    "pict",
    "header",
    "footer",
    "footnote",
    "themedata",
    "colorschememapping",
    "latentstyles",
    "datastore",
    "listtable",
    "listoverridetable",
    "rsidtbl",
    "fldinst",
    "xmlnstbl",
    "pntext",
    "listtext", // list bullets: the plain half is just the items
];

#[derive(Clone, Copy)]
struct Group {
    skip: bool,
    uc: u32, // \ucN: fallback chars after \u
}

/// Plain text of RTF source (CRLF paragraph breaks). Handles groups, ignorable `\*`
/// destinations, font/color/style tables, `\par \line \tab`, `\'hh` (cp1252, whatever
/// `\ansicpg` says), `\uN` with fallback skipping, and escaped `\ { }`. One trailing
/// paragraph break is dropped: RichEdit/WordPad end every document with `\par`.
pub fn rtf_plain_text(rtf: &str) -> String {
    let src: Vec<char> = rtf.chars().collect();
    let mut out = String::with_capacity(rtf.len() / 2);
    let mut cur = Group { skip: false, uc: 1 };
    let mut stack: Vec<Group> = Vec::new();
    let mut skip_fallback = 0; // chars still to drop after \uN
    let mut high: Option<u16> = None; // pending UTF-16 high surrogate
    let mut i = 0;
    while let Some(&c) = src.get(i) {
        i += 1;
        match c {
            '{' => stack.push(cur),
            '}' => cur = stack.pop().unwrap_or(cur),
            '\r' | '\n' => {}
            '\\' => {
                let Some(&n) = src.get(i) else { break };
                i += 1;
                match n {
                    '\\' | '{' | '}' => emit(&mut out, &cur, &mut skip_fallback, n),
                    '\'' => {
                        let hex: String = src.iter().skip(i).take(2).collect();
                        i += 2;
                        if let Ok(b) = u8::from_str_radix(&hex, 16) {
                            emit(&mut out, &cur, &mut skip_fallback, cp1252(b));
                        }
                    }
                    '*' => cur.skip = true,
                    '~' => emit(&mut out, &cur, &mut skip_fallback, '\u{a0}'),
                    '_' => emit(&mut out, &cur, &mut skip_fallback, '-'),
                    '\r' | '\n' => emit_str(&mut out, &cur, "\r\n"),
                    n if n.is_ascii_alphabetic() => {
                        let word_end = src
                            .iter()
                            .skip(i)
                            .position(|c| !c.is_ascii_alphabetic())
                            .map_or(src.len(), |p| i + p);
                        let word: String = src
                            .get(i - 1..word_end)
                            .unwrap_or_default()
                            .iter()
                            .collect();
                        i = word_end;
                        let neg = src.get(i) == Some(&'-');
                        let digits_end = src
                            .iter()
                            .skip(i + usize::from(neg))
                            .position(|c| !c.is_ascii_digit());
                        let digits_end = digits_end.map_or(src.len(), |p| i + usize::from(neg) + p);
                        let digits: String = src
                            .get(i + usize::from(neg)..digits_end)
                            .unwrap_or_default()
                            .iter()
                            .collect();
                        let param: Option<i32> =
                            digits.parse::<i32>().ok().map(|v| if neg { -v } else { v });
                        if param.is_some() {
                            i = digits_end;
                        }
                        if src.get(i) == Some(&' ') {
                            i += 1; // the delimiter space belongs to the control word
                        }
                        match word.as_str() {
                            "par" | "line" | "sect" | "page" => emit_str(&mut out, &cur, "\r\n"),
                            "tab" => emit_str(&mut out, &cur, "\t"),
                            "emdash" => emit_str(&mut out, &cur, "\u{2014}"),
                            "endash" => emit_str(&mut out, &cur, "\u{2013}"),
                            "bullet" => emit_str(&mut out, &cur, "\u{2022}"),
                            "lquote" => emit_str(&mut out, &cur, "\u{2018}"),
                            "rquote" => emit_str(&mut out, &cur, "\u{2019}"),
                            "ldblquote" => emit_str(&mut out, &cur, "\u{201C}"),
                            "rdblquote" => emit_str(&mut out, &cur, "\u{201D}"),
                            "emspace" | "enspace" | "qmspace" => emit_str(&mut out, &cur, " "),
                            "uc" => cur.uc = param.map_or(1, |v| v.max(0) as u32),
                            "u" => {
                                let unit = param.map_or(0, |v| v as u16); // \u-3913 == 62
                                match (high.take(), unit) {
                                    (Some(h), 0xDC00..=0xDFFF) => {
                                        let ch = char::decode_utf16([h, unit])
                                            .next()
                                            .and_then(Result::ok);
                                        emit_str(
                                            &mut out,
                                            &cur,
                                            ch.unwrap_or('\u{FFFD}').encode_utf8(&mut [0; 4]),
                                        );
                                    }
                                    (_, 0xD800..=0xDBFF) => high = Some(unit),
                                    (_, u) => {
                                        let ch = char::from_u32(u32::from(u)).unwrap_or('\u{FFFD}');
                                        emit_str(&mut out, &cur, ch.encode_utf8(&mut [0; 4]));
                                    }
                                }
                                skip_fallback = cur.uc;
                            }
                            w if SKIPPED.contains(&w) => cur.skip = true,
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            c => emit(&mut out, &cur, &mut skip_fallback, c),
        }
    }
    if out.ends_with("\r\n") {
        out.truncate(out.len() - 2);
    }
    out
}

/// Literal text char: dropped inside skipped groups and as `\uN` fallback.
fn emit(out: &mut String, g: &Group, skip_fallback: &mut u32, c: char) {
    if *skip_fallback > 0 {
        *skip_fallback -= 1;
    } else if !g.skip {
        out.push(c);
    }
}

fn emit_str(out: &mut String, g: &Group, s: &str) {
    if !g.skip {
        out.push_str(s);
    }
}

/// What to publish as `CF_UNICODETEXT` for `s`: `content_plain`, else the text extracted
/// from RTF content, else the content itself. Never raw RTF source.
pub fn plain_for_paste(s: &Snippet) -> String {
    let src = s.content_plain.as_deref().unwrap_or(&s.content);
    if looks_rtf(src) {
        rtf_plain_text(src)
    } else {
        src.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snip(name: &str, content: &str, plain: Option<&str>) -> Snippet {
        Snippet {
            name: name.into(),
            content: content.into(),
            content_plain: plain.map(String::from),
        }
    }

    fn now() -> Now {
        Now {
            year: 2026,
            month: 3,
            day: 7,
            hour: 9,
            minute: 5,
            second: 1,
            date_str: "07/03/2026".into(),
            time_str: "09:05".into(),
        }
    }

    #[test]
    fn encode_decode_round_trip() {
        for s in [
            snip("greet", "Hello\r\nworld {{date}}", None),
            snip("rtf", "{\\rtf1 Hi\\par}", Some("Hi")),
            snip("empty plain", "{\\rtf1}", Some("")),
            snip("\u{e9}\u{1F600}", "caf\u{e9}", None),
            snip("n", "", None),
        ] {
            assert_eq!(decode(&encode(&s)), Some(s.clone()), "{s:?}");
        }
        assert_eq!(encode(&snip("a", "b", None)), "a\u{1}b");
        assert_eq!(encode(&snip("a", "b", Some("c"))), "a\u{1}b\u{2}c");
        assert_eq!(decode("no separator"), None);
        assert_eq!(decode(""), None);
        assert_eq!(decode("a\u{1}"), Some(snip("a", "", None)));
        // plain-less stays plain-less, empty plain stays Some("")
        assert_eq!(decode("a\u{1}b").and_then(|s| s.content_plain), None);
        assert_eq!(
            decode("a\u{1}b\u{2}").and_then(|s| s.content_plain),
            Some(String::new())
        );
    }

    #[test]
    fn encoded_len_counts_utf16_units() {
        assert_eq!(encoded_len(&snip("ab", "cd", None)), 5);
        assert_eq!(encoded_len(&snip("ab", "cd", Some("e"))), 7);
        assert_eq!(encoded_len(&snip("\u{1F600}", "\u{1F600}", None)), 5); // surrogate pairs
        let s = snip("n", "\u{e9}x", Some("\u{4e2d}"));
        assert_eq!(encoded_len(&s), encode(&s).encode_utf16().count());
    }

    #[test]
    fn validate_cases() {
        assert!(validate(&snip("ok", "text", None)).is_ok());
        assert!(validate(&snip("", "text", None)).is_err());
        assert!(validate(&snip("   ", "text", None)).is_err());
        for n in ["*set", "*SET", "*Set", " *set "] {
            let e = validate(&snip(n, "x", None)).unwrap_err();
            assert!(e.contains("*set"), "{e}");
        }
        assert!(validate(&snip("*settings", "x", None)).is_ok());
        assert!(validate(&snip("a\u{1}b", "x", None)).is_err());
        assert!(validate(&snip("a", "x\u{2}y", None)).is_err());
        assert!(validate(&snip("a", "x", Some("y\u{1}"))).is_err());

        // limit: name(1) + sep(1) + content => content of 32764 chars is exactly 32766
        let at = snip("a", &"x".repeat(MAX_VALUE_CHARS - 2), None);
        assert_eq!(encoded_len(&at), MAX_VALUE_CHARS);
        assert!(validate(&at).is_ok());
        let over = snip("a", &"x".repeat(MAX_VALUE_CHARS - 1), None);
        let e = validate(&over).unwrap_err();
        assert!(e.contains("32767") && e.contains("32766"), "{e}");
        // the optional plain half counts too, in UTF-16 units
        let mut two = snip("a", &"x".repeat(MAX_VALUE_CHARS - 6), Some("12"));
        assert_eq!(encoded_len(&two), MAX_VALUE_CHARS - 1);
        assert!(validate(&two).is_ok());
        two.content_plain = Some("\u{1F600}\u{1F600}".into());
        assert!(validate(&two).is_err());
    }

    #[test]
    fn rtf_detection() {
        assert!(snip("a", "{\\rtf1\\ansi x}", None).is_rtf());
        assert!(snip("a", "  {\\rtf1 x}", None).is_rtf());
        assert!(!snip("a", "plain {\\rtf1}", None).is_rtf());
        assert!(!snip("a", "", None).is_rtf());
    }

    #[test]
    fn every_placeholder() {
        let n = now();
        let cases = [
            ("{{date}}", "07/03/2026"),
            ("{{time}}", "09:05"),
            ("{{datetime}}", "07/03/2026 09:05"),
            ("{{year}}", "2026"),
            ("{{month}}", "03"),
            ("{{day}}", "07"),
            ("{{hour}}", "09"),
            ("{{minute}}", "05"),
            ("{{second}}", "01"),
            ("{{clipboard}}", "CLIP"),
            ("{{DATE}} {{Time}} {{ClipBoard}}", "07/03/2026 09:05 CLIP"),
            ("a {{year}}-{{month}}-{{day}} b", "a 2026-03-07 b"),
            ("no placeholders \u{e9}", "no placeholders \u{e9}"),
        ];
        for (t, want) in cases {
            assert_eq!(expand(t, &n, "CLIP"), want, "{t}");
        }
        let early = Now { year: 5, ..now() };
        assert_eq!(expand("{{year}}", &early, ""), "0005");
    }

    #[test]
    fn unknown_and_malformed_placeholders_are_untouched() {
        let n = now();
        for t in [
            "{{nope}}",
            "{{ date }}",
            "{date}",
            "{{date}",
            "{{date",
            "{{",
            "}}",
            "{{}}",
            "{{{{",
            "{{date} }",
            "{{clipboardx}}",
            "{{da\u{e9}}}",
            "\u{e9}{{",
        ] {
            assert_eq!(expand(t, &n, "C"), t, "{t}");
        }
        assert_eq!(expand("{{{date}}}", &n, ""), "{07/03/2026}");
        assert_eq!(expand("{{x}}{{day}}", &n, ""), "{{x}}07");
        assert_eq!(
            expand(&format!("{}{{{{date}}}}", "{{".repeat(5000)), &n, ""),
            format!("{}07/03/2026", "{{".repeat(5000))
        );
    }

    #[test]
    fn clipboard_text_is_not_re_expanded() {
        let n = now();
        assert_eq!(
            expand("[{{clipboard}}]", &n, "{{date}} {{clipboard}}"),
            "[{{date}} {{clipboard}}]"
        );
        let mut n2 = now();
        n2.date_str = "{{time}}".into();
        assert_eq!(expand("{{date}}", &n2, ""), "{{time}}");
        assert_eq!(
            expand_rtf("{{clipboard}}", &n, "{{date}}"),
            "\\{\\{date\\}\\}"
        );
    }

    #[test]
    fn rtf_expansion_escapes_values() {
        let n = now();
        let t = "{\\rtf1\\ansi Hi {{clipboard}} on {{date}}}";
        assert_eq!(
            expand_rtf(t, &n, "a\\b {c}"),
            "{\\rtf1\\ansi Hi a\\\\b \\{c\\} on 07/03/2026}"
        );
        assert_eq!(
            expand_rtf("{{clipboard}}", &n, "l1\r\nl2\nl3\rl4\tx"),
            "l1\\par l2\\par l3\\par l4\\tab x"
        );
        assert_eq!(expand_rtf("{{clipboard}}", &n, "caf\u{e9}"), "caf\\u233?");
        assert_eq!(expand_rtf("{{clipboard}}", &n, "\u{4e2d}"), "\\u20013?");
        assert_eq!(expand_rtf("{{clipboard}}", &n, "\u{20ac}"), "\\u8364?");
        assert_eq!(expand_rtf("{{clipboard}}", &n, "\u{ffe5}"), "\\u-27?"); // > 0x7FFF: signed
        assert_eq!(
            expand_rtf("{{clipboard}}", &n, "\u{1F600}"),
            "\\u-10179?\\u-8704?"
        );
        assert_eq!(expand_rtf("{{clipboard}}", &n, "\u{1}"), "\\'01");
        // the editor's own escaped form of a typed placeholder works too
        assert_eq!(
            expand_rtf("{\\rtf1 \\{\\{Date\\}\\} \\{\\{nope\\}\\}}", &n, ""),
            "{\\rtf1 07/03/2026 \\{\\{nope\\}\\}}"
        );
        // ... but only in RTF mode
        assert_eq!(expand("\\{\\{date\\}\\}", &n, ""), "\\{\\{date\\}\\}");
    }

    #[test]
    fn rtf_escape_round_trips_through_plain_text() {
        for s in [
            "plain",
            "a\\b {c} d",
            "caf\u{e9} \u{4e2d} \u{1F600} \u{20ac}",
            "x\r\ny\tz",
            "\u{ffe5}\u{7fff}\u{8000}",
        ] {
            let mut esc = String::new();
            rtf_escape_into(&mut esc, s);
            let doc = format!("{{\\rtf1\\ansi {esc}}}");
            assert_eq!(rtf_plain_text(&doc), s, "{doc}");
        }
    }

    #[test]
    fn rtf_plain_text_samples() {
        let t = rtf_plain_text;
        assert_eq!(t("{\\rtf1\\ansi\\deff0 Hello world}"), "Hello world");
        assert_eq!(t("{\\rtf1 one\\par two\\par}"), "one\r\ntwo");
        assert_eq!(t("{\\rtf1 a\\tab b\\line c}"), "a\tb\r\nc");
        assert_eq!(t("{\\rtf1 \\{x\\} \\\\ y}"), "{x} \\ y");
        // tables and other non-text destinations are skipped
        let doc =
            "{\\rtf1\\ansi{\\fonttbl{\\f0\\fswiss Arial;}}{\\colortbl;\\red255\\green0\\blue0;}\
                   {\\stylesheet{\\s0 Normal;}}{\\info{\\title T}}{\\*\\generator Riched20 10.0;}\
                   \\pard\\f0\\fs22 Body {\\b bold} text\\par}";
        assert_eq!(t(doc), "Body bold text");
        // \'hh via cp1252
        assert_eq!(
            t("{\\rtf1\\ansi\\ansicpg1252 caf\\'e9 \\'93q\\'94 \\'80 \\'99}"),
            "caf\u{e9} \u{201c}q\u{201d} \u{20ac} \u{2122}"
        );
        // \uN with fallback skipping (default \uc1)
        assert_eq!(
            t("{\\rtf1 caf\\u233? \\u20013?x \\u-3913?}"),
            "caf\u{e9} \u{4e2d}x \u{f0b7}"
        );
        assert_eq!(
            t("{\\rtf1 \\u233\\'e9x \\uc2\\u233ab c}"),
            "\u{e9}x \u{e9} c"
        );
        // surrogate pair
        assert_eq!(t("{\\rtf1 \\u-10179?\\u-8704?!}"), "\u{1F600}!");
        // ignorable destination, field result kept, instructions dropped
        assert_eq!(t("{\\rtf1 a{\\*\\unknown hidden}b{\\field{\\*\\fldinst HYPERLINK \"x\"}{\\fldrslt link}}c}"), "ablinkc");
        // spacing details
        assert_eq!(t("{\\rtf1\\b bold\\b0  after}"), "bold after");
        assert_eq!(
            t("{\\rtf1 a\\~b\\emdash c\\bullet d}"),
            "a\u{a0}b\u{2014}c\u{2022}d"
        );
        // robustness
        for junk in [
            "",
            "{",
            "}",
            "\\",
            "{\\rtf1 \\",
            "{\\rtf1 \\'",
            "{\\rtf1 \\'z",
            "{\\rtf1 \\u",
            "{\\rtf1 \\u99999999999?}",
            "}}}{{{",
            "\\u-1?\\u-1?\u{1F600}",
            "{\\rtf1 \\uc-5\\u1 }",
        ] {
            let _ = t(junk);
        }
        assert_eq!(t("not rtf at all"), "not rtf at all");
    }

    #[test]
    fn plain_for_paste_never_exposes_rtf() {
        let rtf = "{\\rtf1\\ansi Hello\\par World\\par}";
        let cases = [
            snip("a", rtf, None),
            snip("a", rtf, Some("Explicit")),
            snip("a", rtf, Some(rtf)), // bad plain half
            snip("a", "{\\rtf1 {\\fonttbl{\\f0 Arial;}}\\b X\\b0 y}", None),
            snip("a", "plain {text}", None),
            snip("a", "", None),
        ];
        let want = [
            "Hello\r\nWorld",
            "Explicit",
            "Hello\r\nWorld",
            "Xy",
            "plain {text}",
            "",
        ];
        for (s, w) in cases.iter().zip(want) {
            let p = plain_for_paste(s);
            assert_eq!(p, w, "{s:?}");
            assert!(!p.trim_start().starts_with("{\\rtf"));
        }
        assert_eq!(
            plain_for_paste(&snip("a", "plain", Some("override"))),
            "override"
        );
    }

    #[test]
    fn random_junk_never_panics() {
        const TOKENS: [&str; 24] = [
            "{",
            "}",
            "\\",
            "\\'",
            "\\u",
            "\\uc",
            "\\par",
            "\\*",
            "a",
            " ",
            "-",
            "12",
            "\u{e9}",
            "\u{1F600}",
            "\\u-1?",
            "{{",
            "}}",
            "\\{",
            "\\}",
            "\\fonttbl",
            "{{date}}",
            "\\'4",
            "\u{1}",
            "\u{2}",
        ];
        let mut state = 99u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as usize
        };
        let n = now();
        for _ in 0..2000 {
            let len = next() % 40;
            let s: String = (0..len).map(|_| TOKENS[next() % TOKENS.len()]).collect();
            let _ = (
                rtf_plain_text(&s),
                expand(&s, &n, &s),
                expand_rtf(&s, &n, &s),
                decode(&s),
            );
            let sn = snip(&s, &s, Some(&s));
            let _ = (plain_for_paste(&sn), validate(&sn), encoded_len(&sn));
        }
    }

    #[test]
    fn rtf_template_flow() {
        // template -> expand_rtf -> plain text, as the paste path does
        let n = now();
        let tpl = "{\\rtf1\\ansi Dear \\{\\{clipboard\\}\\},\\par Sent {{datetime}}\\par}";
        let doc = expand_rtf(tpl, &n, "Se\u{f1}or {X}");
        assert_eq!(
            rtf_plain_text(&doc),
            "Dear Se\u{f1}or {X},\r\nSent 07/03/2026 09:05"
        );
    }
}
