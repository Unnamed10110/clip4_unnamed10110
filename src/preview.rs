//! Preview text (spec 7.2), search text (spec 9) and the parsers behind them.
//!
//! Every clipboard payload is untrusted (spec 19.2): all offsets and lengths are
//! bounds-checked, no input can panic these functions, and every loop makes progress.

use crate::model::*;
use windows::Win32::Globalization::{MultiByteToWideChar, CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};

/// Preview length in chars (spec 7.2).
const PREVIEW_CHARS: usize = 300;
/// Text indexed for search per item (spec 9).
const SEARCH_MAX_BYTES: usize = 500 * 1024;
/// Largest DIB we accept (spec 6.4).
const DIB_MAX_BYTES: u64 = 256 * 1024 * 1024;

// ------------------------------------------------------------------ public API

/// What a row shows: the first 300 chars of the best textual content of ALL formats,
/// else an image label, else the primary format's name. Never a placeholder for text.
pub fn build_preview(formats: &[(FormatKey, Payload)], primary: &FormatKey) -> String {
    let some = |s: String| (!s.is_empty()).then_some(s);
    std_bytes(formats, CF_UNICODETEXT)
        .and_then(|b| some(unicode_head(b, PREVIEW_CHARS)))
        .or_else(|| {
            let b = before_nul(std_bytes(formats, CF_TEXT)?);
            some(decode_ansi(b.get(..b.len().min(PREVIEW_CHARS)).unwrap_or(b)))
        })
        .or_else(|| some(take_chars(hdrop_paths(std_bytes(formats, CF_HDROP)?).join(", "), PREVIEW_CHARS)))
        .or_else(|| {
            let frag = fragment_slice(named_bytes(formats, FMT_HTML)?)?;
            some(html_to_text_cap(&String::from_utf8_lossy(frag), PREVIEW_CHARS))
        })
        .or_else(|| some(rtf_text_cap(named_bytes(formats, FMT_RTF)?, PREVIEW_CHARS)))
        .map(|s| take_chars(s, PREVIEW_CHARS))
        .or_else(|| image_label(formats))
        .unwrap_or_else(|| primary.label())
}

/// Text fed to the search index (spec 9): full text (<= 500 KB), file paths, or a label.
pub fn search_text(formats: &[(FormatKey, Payload)], primary: &FormatKey) -> String {
    let label = || primary.label();
    match kind_of(primary) {
        Kind::Text => best_text(formats).map_or_else(label, |mut s| {
            let mut n = SEARCH_MAX_BYTES.min(s.len());
            while !s.is_char_boundary(n) {
                n -= 1;
            }
            s.truncate(n);
            s
        }),
        Kind::Files => std_bytes(formats, CF_HDROP)
            .map(|b| hdrop_paths(b).join("\n"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(label),
        Kind::Image => image_label(formats).unwrap_or_else(label),
        Kind::Other => label(),
    }
}

/// Full best text: Unicode > ANSI > file paths > HTML > RTF. `None` if none yields text.
pub fn best_text(formats: &[(FormatKey, Payload)]) -> Option<String> {
    let some = |s: String| (!s.is_empty()).then_some(s);
    std_bytes(formats, CF_UNICODETEXT)
        .and_then(|b| some(decode_unicode(b)))
        .or_else(|| some(decode_ansi(std_bytes(formats, CF_TEXT)?)))
        .or_else(|| some(hdrop_paths(std_bytes(formats, CF_HDROP)?).join("\r\n")))
        .or_else(|| {
            let frag = fragment_slice(named_bytes(formats, FMT_HTML)?)?;
            some(html_to_text(&String::from_utf8_lossy(frag)))
        })
        .or_else(|| some(rtf_to_text(named_bytes(formats, FMT_RTF)?)))
}

/// UTF-16LE with terminating NUL (a `CF_UNICODETEXT` payload).
pub fn unicode_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect()
}

/// UTF-16LE up to the first NUL, lossy; an odd trailing byte is ignored.
pub fn decode_unicode(b: &[u8]) -> String {
    unicode_head(b, usize::MAX)
}

/// ANSI (CP_ACP) up to the first NUL.
pub fn decode_ansi(b: &[u8]) -> String {
    ansi_cp(CP_ACP, before_nul(b))
}

/// Fragment of an `HTML Format` payload (see [`fragment_slice`]), UTF-8 lossy.
pub fn html_fragment(html_format: &[u8]) -> Option<String> {
    fragment_slice(html_format).map(|s| String::from_utf8_lossy(s).into_owned())
}

/// Plain text of an HTML fragment: tags dropped, entities decoded, blocks -> newlines.
pub fn html_to_text(html: &str) -> String {
    html_to_text_cap(html, usize::MAX)
}

/// Plain text of an RTF document.
pub fn rtf_to_text(rtf: &[u8]) -> String {
    rtf_text_cap(rtf, usize::MAX)
}

/// Decodes `&amp;`-style and numeric entities in `s` (no tag handling).
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(rel) = s.get(i..).and_then(|r| r.find('&')) {
        out.push_str(s.get(i..i + rel).unwrap_or_default());
        i += rel;
        match entity_at(s, i) {
            Some((c, n)) => {
                out.push(c);
                i += n;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out.push_str(s.get(i..).unwrap_or_default());
    out
}

/// Paths of a `DROPFILES` payload (`CF_HDROP`), ANSI or wide.
pub fn hdrop_paths(b: &[u8]) -> Vec<String> {
    let (Some(off), Some(wide)) = (u32_at(b, 0), u32_at(b, 16)) else {
        return Vec::new();
    };
    // DROPFILES is 20 bytes: pFiles, POINT pt, fNC, fWide.
    let Some(list) = usize::try_from(off).ok().filter(|&o| o >= 20).and_then(|o| b.get(o..)) else {
        return Vec::new();
    };
    if wide != 0 {
        let units: Vec<u16> = list.chunks_exact(2).map(le16).collect();
        units.split(|&u| u == 0).take_while(|p| !p.is_empty()).map(String::from_utf16_lossy).collect()
    } else {
        list.split(|&x| x == 0).take_while(|p| !p.is_empty()).map(decode_ansi).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DibInfo {
    pub width: i32,
    /// Absolute value (top-down DIBs have a negative height on the wire).
    pub height: i32,
    pub bpp: u16,
    pub header_size: u32,
}

/// Parses a BITMAPCOREHEADER / BITMAPINFOHEADER / V4 / V5. A buffer holding only the
/// first 16 bytes of the header is enough.
pub fn dib_info(b: &[u8]) -> Option<DibInfo> {
    let header_size = u32_at(b, 0)?;
    let (width, height, bpp) = match header_size {
        12 => (i32::from(u16_at(b, 4)?), i32::from(u16_at(b, 6)?), u16_at(b, 10)?),
        40 | 52 | 56 | 108 | 124 => (i32_at(b, 4)?, i32_at(b, 8)?, u16_at(b, 14)?),
        _ => return None,
    };
    let height = i32::try_from(height.unsigned_abs()).ok()?;
    (width > 0 && height > 0 && matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32))
        .then_some(DibInfo { width, height, bpp, header_size })
}

/// True for a complete, uncompressed (BI_RGB / BITFIELDS) DIB whose computed size is at
/// most 256 MB and fits in `b`. Header-only buffers and other compressions are rejected.
pub fn dib_size_ok(b: &[u8]) -> bool {
    dib_check(b).unwrap_or(false)
}

/// `"Image 1920×1080"` from a DIB (full or header-only) or a PNG's IHDR.
pub fn image_label(formats: &[(FormatKey, Payload)]) -> Option<String> {
    let dib = [CF_DIBV5, CF_DIB].iter().find_map(|&cf| dib_info(std_bytes(formats, cf)?));
    let (w, h) = match dib {
        Some(d) => (d.width.unsigned_abs(), d.height.unsigned_abs()),
        None => png_size(named_bytes(formats, FMT_PNG)?)?,
    };
    Some(format!("Image {w}\u{D7}{h}"))
}

// ------------------------------------------------------------------ small helpers

fn std_bytes(formats: &[(FormatKey, Payload)], cf: u32) -> Option<&[u8]> {
    formats.iter().filter(|(k, _)| k.is_std(cf)).find_map(|(_, p)| p.bytes())
}

fn named_bytes<'a>(formats: &'a [(FormatKey, Payload)], name: &str) -> Option<&'a [u8]> {
    formats.iter().filter(|(k, _)| k.is_named(name)).find_map(|(_, p)| p.bytes())
}

fn take_chars(mut s: String, n: usize) -> String {
    if let Some((i, _)) = s.char_indices().nth(n) {
        s.truncate(i);
    }
    s
}

fn before_nul(b: &[u8]) -> &[u8] {
    b.split(|&x| x == 0).next().unwrap_or_default()
}

fn le16(c: &[u8]) -> u16 {
    c.try_into().map(u16::from_le_bytes).unwrap_or(0)
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(le16(b.get(off..off.checked_add(2)?)?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off.checked_add(4)?)?.try_into().ok().map(u32::from_le_bytes)
}

fn i32_at(b: &[u8], off: usize) -> Option<i32> {
    u32_at(b, off).map(|v| i32::from_le_bytes(v.to_le_bytes()))
}

fn be32_at(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off.checked_add(4)?)?.try_into().ok().map(u32::from_be_bytes)
}

/// First `max_units` UTF-16 units up to the first NUL; never ends on a dangling high
/// surrogate when it had to cut.
fn unicode_head(b: &[u8], max_units: usize) -> String {
    let mut u: Vec<u16> = b.chunks_exact(2).map(le16).take_while(|&x| x != 0).take(max_units).collect();
    if u.len() == max_units && u.last().is_some_and(|x| (0xD800..0xDC00).contains(x)) {
        u.pop();
    }
    String::from_utf16_lossy(&u)
}

/// Code page -> String via `MultiByteToWideChar`; Latin-1 if the API refuses.
fn ansi_cp(cp: u32, b: &[u8]) -> String {
    // The windows crate unwraps the length conversion, so cap at i32::MAX.
    let b = b.get(..b.len().min(i32::MAX as usize)).unwrap_or_default();
    if b.is_empty() {
        return String::new();
    }
    let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
    // SAFETY: both calls receive valid slices; the output buffer is sized by the first call.
    let units = unsafe {
        let n = usize::try_from(MultiByteToWideChar(cp, flags, b, None)).unwrap_or(0);
        if n == 0 {
            return b.iter().map(|&c| char::from(c)).collect();
        }
        let mut buf = vec![0u16; n];
        let w = usize::try_from(MultiByteToWideChar(cp, flags, b, Some(&mut buf))).unwrap_or(0);
        buf.truncate(w);
        buf
    };
    String::from_utf16_lossy(&units)
}

fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.get(..8)? != b"\x89PNG\r\n\x1a\n" || b.get(12..16)? != b"IHDR" {
        return None;
    }
    let (w, h) = (be32_at(b, 16)?, be32_at(b, 20)?);
    (w > 0 && h > 0).then_some((w, h))
}

// ------------------------------------------------------------------ DIB size check

fn dib_check(b: &[u8]) -> Option<bool> {
    let d = dib_info(b)?;
    let (w, h, bpp) = (u64::try_from(d.width).ok()?, u64::try_from(d.height).ok()?, u64::from(d.bpp));
    let core = d.header_size == 12;
    let (compression, clr_used) = if core { (0, 0) } else { (u32_at(b, 16)?, u32_at(b, 32)?) };
    // BI_RGB, BI_BITFIELDS, BI_ALPHABITFIELDS only; the rest cannot be size-checked.
    if !matches!(compression, 0 | 3 | 6) {
        return Some(false);
    }
    let entries = match d.bpp {
        1..=8 if clr_used > (1 << d.bpp) => return Some(false),
        1..=8 if clr_used == 0 => 1u64 << d.bpp,
        _ => u64::from(clr_used),
    };
    let palette = entries * if core { 3 } else { 4 };
    let masks = match (d.header_size, compression) {
        (40, 3) => 12,
        (40, 6) => 16,
        _ => 0,
    };
    let stride = w.checked_mul(bpp)?.checked_add(31)? / 32 * 4;
    let total = stride.checked_mul(h)?.checked_add(palette + masks + u64::from(d.header_size))?;
    Some(total <= DIB_MAX_BYTES && total <= b.len() as u64)
}

// ------------------------------------------------------------------ HTML Format

/// Fragment bytes of an `HTML Format` payload: StartFragment..EndFragment, else
/// StartHTML..EndHTML, else everything after the header. Offsets that are out of range,
/// reversed or point into the header are ignored. `None` if nothing remains.
fn fragment_slice(b: &[u8]) -> Option<&[u8]> {
    let end = b.iter().rposition(|&c| c != 0).map_or(0, |p| p + 1);
    let b = b.get(..end)?;
    let (mut sh, mut eh, mut sf, mut ef) = (None, None, None, None);
    let mut body = 0; // first byte after the "Key:value" header lines
    if b.starts_with(b"Version:") {
        for _ in 0..32 {
            let rest = b.get(body..)?;
            let n = rest.iter().position(|&c| c == b'\n').map_or(rest.len(), |p| p + 1);
            let Some((key, val)) = header_line(rest.get(..n)?) else { break };
            let val = std::str::from_utf8(val).ok().and_then(|v| v.trim().parse::<usize>().ok());
            match key {
                b"StartHTML" => sh = val,
                b"EndHTML" => eh = val,
                b"StartFragment" => sf = val,
                b"EndFragment" => ef = val,
                _ => {}
            }
            body += n;
        }
    }
    // A sane fragment pair decides, even if it is empty.
    if let Some((s, e)) = sf.zip(ef).filter(|&(s, e)| s >= body && s <= e && e <= b.len()) {
        return b.get(s..e).filter(|x| !x.is_empty());
    }
    let span = |s: usize, e: usize| b.get(s..e).filter(|x| s >= body && !x.is_empty());
    span(sh.unwrap_or(body), eh.unwrap_or(b.len())).or_else(|| span(body, b.len()))
}

/// `Key:value` header line (key = 1..=20 ASCII letters).
fn header_line(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|&c| c == b':')?;
    let key = line.get(..colon)?;
    let ok = (1..=20).contains(&key.len()) && key.iter().all(u8::is_ascii_alphabetic);
    ok.then(|| (key, line.get(colon + 1..).unwrap_or_default()))
}

// ------------------------------------------------------------------ HTML -> text

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Gap {
    No,
    Space,
    Tab,
    Newlines(u8),
}

/// Streaming output: collapses whitespace, caps blank lines, trims the start.
struct TextOut {
    s: String,
    chars: usize,
    cap: usize,
    gap: Gap,
}

impl TextOut {
    fn set_gap(&mut self, g: Gap) {
        self.gap = self.gap.max(g);
    }
    /// `<br>`: a newline that adds up (two `<br>` make a blank line).
    fn newline(&mut self) {
        self.gap = match self.gap {
            Gap::Newlines(n) => Gap::Newlines((n + 1).min(2)),
            _ => Gap::Newlines(1),
        };
    }
    fn lit(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        if !self.s.is_empty() {
            match self.gap {
                Gap::No => {}
                Gap::Space => self.s.push(' '),
                Gap::Tab => self.s.push('\t'),
                Gap::Newlines(n) => (0..n).for_each(|_| self.s.push('\n')),
            }
        }
        self.gap = Gap::No;
        self.s.push_str(t);
        self.chars += t.chars().count();
    }
    fn full(&self) -> bool {
        self.chars >= self.cap
    }
}

const HTML_BLOCKS: &[&str] = &[
    "p", "div", "li", "ul", "ol", "tr", "table", "h1", "h2", "h3", "h4", "h5", "h6", "blockquote", "pre", "section",
    "article", "header", "footer", "nav", "aside", "form", "dl", "dt", "dd", "hr", "address", "figure", "figcaption",
    "main",
];

struct HtmlConv<'a> {
    html: &'a str,
    out: TextOut,
    pre: bool,
    /// A `<head>`/`<title>` had no end tag; stops repeated O(n) searches on hostile input.
    no_head_end: bool,
}

fn find_ci(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let tail = hay.get(from..)?;
    tail.windows(needle.len().max(1)).position(|w| w.eq_ignore_ascii_case(needle)).map(|p| p + from)
}

/// Converts until at least `cap` chars are produced (the result is a prefix of the full text).
fn html_to_text_cap(html: &str, cap: usize) -> String {
    let mut c = HtmlConv {
        html,
        out: TextOut { s: String::new(), chars: 0, cap, gap: Gap::No },
        pre: false,
        no_head_end: false,
    };
    c.run();
    let mut s = c.out.s;
    s.truncate(s.trim_end().len());
    s
}

impl HtmlConv<'_> {
    fn run(&mut self) {
        let html = self.html;
        let b = html.as_bytes();
        let mut i = 0;
        while let Some(&c) = b.get(i) {
            if self.out.full() {
                break;
            }
            match c {
                b'<' => i = self.tag(i),
                b'&' => match entity_at(html, i) {
                    Some((ch, n)) => {
                        self.put_char(ch);
                        i += n;
                    }
                    None => {
                        self.out.lit("&");
                        i += 1;
                    }
                },
                b' ' | b'\t' | b'\r' | b'\n' | 0x0C => {
                    self.put_char(char::from(c));
                    i += 1;
                }
                0..=0x1F => i += 1,
                _ => {
                    let tail = b.get(i..).unwrap_or_default();
                    let run = tail.iter().take_while(|&&d| !matches!(d, b'<' | b'&' | 0..=0x20)).count().max(1);
                    self.out.lit(html.get(i..i + run).unwrap_or_default());
                    i += run;
                }
            }
        }
    }

    /// Whitespace or literal char; whitespace collapses except inside `<pre>`.
    fn put_char(&mut self, c: char) {
        if !c.is_whitespace() {
            self.out.lit(c.encode_utf8(&mut [0; 4]));
        } else if !self.pre {
            self.out.set_gap(Gap::Space);
        } else if c == '\n' {
            self.out.newline();
        } else if c != '\r' {
            self.out.lit(if c == '\t' { "\t" } else { " " });
        }
    }

    /// Handles markup at `html[i] == '<'`; returns where to continue.
    fn tag(&mut self, i: usize) -> usize {
        let b = self.html.as_bytes();
        let rest = b.get(i + 1..).unwrap_or_default();
        if rest.starts_with(b"!--") {
            return find_ci(b, i + 4, b"-->").map_or(b.len(), |p| p + 3);
        }
        let closing = rest.first() == Some(&b'/');
        let ns = i + 1 + usize::from(closing);
        let name: String = b
            .get(ns..)
            .unwrap_or_default()
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric())
            .take(12)
            .map(|c| char::from(c.to_ascii_lowercase()))
            .collect();
        if name.is_empty() {
            // "<!DOCTYPE", "<?xml", "</>" are skipped; any other '<' is literal text.
            if matches!(rest.first(), Some(b'/' | b'!' | b'?')) {
                return tag_end(b, ns);
            }
            self.out.lit("<");
            return i + 1;
        }
        let end = tag_end(b, ns);
        match (closing, name.as_str()) {
            (false, "script" | "style") => {
                return find_ci(b, end, format!("</{name}").as_bytes()).map_or(b.len(), |p| tag_end(b, p));
            }
            (false, "head" | "title") if !self.no_head_end => match find_ci(b, end, format!("</{name}").as_bytes()) {
                Some(p) => return tag_end(b, p),
                None => self.no_head_end = true,
            },
            (_, "br") => self.out.newline(),
            (_, "pre") => {
                self.pre = !closing;
                self.out.set_gap(Gap::Newlines(1));
            }
            (true, "td" | "th") => self.out.set_gap(Gap::Tab),
            (_, n) if HTML_BLOCKS.contains(&n) => self.out.set_gap(Gap::Newlines(1)),
            _ => {}
        }
        end
    }
}

/// Index just past the `>` that ends the tag whose name starts at `from` (quotes honoured).
fn tag_end(b: &[u8], from: usize) -> usize {
    let mut quote = None;
    let mut j = from;
    while let Some(&c) = b.get(j) {
        j += 1;
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(c),
            (None, b'>') => return j,
            _ => {}
        }
    }
    b.len()
}

/// HTML 4 Latin-1 entity names for U+00A0..=U+00FF, in code point order.
const LATIN1: [&str; 96] = [
    "nbsp", "iexcl", "cent", "pound", "curren", "yen", "brvbar", "sect", "uml", "copy", "ordf", "laquo", "not", "shy",
    "reg", "macr", "deg", "plusmn", "sup2", "sup3", "acute", "micro", "para", "middot", "cedil", "sup1", "ordm",
    "raquo", "frac14", "frac12", "frac34", "iquest", "Agrave", "Aacute", "Acirc", "Atilde", "Auml", "Aring", "AElig",
    "Ccedil", "Egrave", "Eacute", "Ecirc", "Euml", "Igrave", "Iacute", "Icirc", "Iuml", "ETH", "Ntilde", "Ograve",
    "Oacute", "Ocirc", "Otilde", "Ouml", "times", "Oslash", "Ugrave", "Uacute", "Ucirc", "Uuml", "Yacute", "THORN",
    "szlig", "agrave", "aacute", "acirc", "atilde", "auml", "aring", "aelig", "ccedil", "egrave", "eacute", "ecirc",
    "euml", "igrave", "iacute", "icirc", "iuml", "eth", "ntilde", "ograve", "oacute", "ocirc", "otilde", "ouml",
    "divide", "oslash", "ugrave", "uacute", "ucirc", "uuml", "yacute", "thorn", "yuml",
];

const NAMED: &[(&str, char)] = &[
    ("amp", '&'),
    ("lt", '<'),
    ("gt", '>'),
    ("quot", '"'),
    ("apos", '\''),
    ("ensp", ' '),
    ("emsp", ' '),
    ("thinsp", ' '),
    ("hellip", '\u{2026}'),
    ("ndash", '\u{2013}'),
    ("mdash", '\u{2014}'),
    ("lsquo", '\u{2018}'),
    ("rsquo", '\u{2019}'),
    ("sbquo", '\u{201A}'),
    ("ldquo", '\u{201C}'),
    ("rdquo", '\u{201D}'),
    ("bdquo", '\u{201E}'),
    ("bull", '\u{2022}'),
    ("euro", '\u{20AC}'),
    ("trade", '\u{2122}'),
    ("permil", '\u{2030}'),
    ("dagger", '\u{2020}'),
    ("prime", '\u{2032}'),
    ("lsaquo", '\u{2039}'),
    ("rsaquo", '\u{203A}'),
    ("larr", '\u{2190}'),
    ("uarr", '\u{2191}'),
    ("rarr", '\u{2192}'),
    ("darr", '\u{2193}'),
    ("harr", '\u{2194}'),
    ("minus", '\u{2212}'),
    ("ne", '\u{2260}'),
    ("le", '\u{2264}'),
    ("ge", '\u{2265}'),
    ("infin", '\u{221E}'),
    ("hearts", '\u{2665}'),
];

/// Entity starting at `s[i] == '&'` -> (char, bytes consumed). Non-breaking spaces
/// become plain spaces.
fn entity_at(s: &str, i: usize) -> Option<(char, usize)> {
    let rest = s.get(i + 1..)?;
    let len = rest.bytes().take(32).position(|b| b == b';')?;
    let name = rest.get(..len)?;
    let c = match name.strip_prefix('#') {
        Some(num) => {
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            match code {
                0x80..=0x9F => ansi_cp(1252, &[u8::try_from(code).ok()?]).chars().next()?, // Windows-1252 quirk
                _ => char::from_u32(code).filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))?,
            }
        }
        None => match LATIN1.iter().position(|n| *n == name) {
            Some(p) => char::from_u32(0xA0 + u32::try_from(p).ok()?)?,
            None => NAMED.iter().find(|(n, _)| *n == name)?.1,
        },
    };
    Some((if c == '\u{A0}' { ' ' } else { c }, len + 2))
}

// ------------------------------------------------------------------ RTF -> text

const RTF_MAX_DEPTH: usize = 256;

/// Destinations whose content is not document text.
const RTF_SKIP: &[&str] = &[
    "fonttbl", "colortbl", "stylesheet", "info", "pict", "header", "headerl", "headerr", "headerf", "footer", "footerl",
    "footerr", "footerf", "footnote", "filetbl", "listtable", "listoverridetable", "revtbl", "rsidtbl", "generator",
    "themedata", "colorschememapping", "latentstyles", "datastore", "xmlnstbl", "fldinst", "object", "objdata",
    "bkmkstart", "bkmkend", "private", "mmathpr", "userprops", "pgdsctbl", "falt", "panose", "fname", "ftnsep",
    "ftnsepc", "aftnsep", "aftnsepc",
];

#[derive(Clone, Copy)]
struct RtfState {
    skip: bool,
    /// `\ucN`: fallback chars after each `\uN`.
    uc: u32,
}

struct RtfOut {
    s: String,
    n: usize,
    /// Pending high surrogate from a `\uN` pair.
    hi: Option<u16>,
    /// Pending ANSI bytes (`\'hh`), decoded together so DBCS pairs survive.
    ansi: Vec<u8>,
    cp: u32,
}

impl RtfOut {
    fn put(&mut self, c: char) {
        self.s.push(c);
        self.n += 1;
    }
    fn flush_ansi(&mut self) {
        if !self.ansi.is_empty() {
            let t = ansi_cp(self.cp, &self.ansi);
            self.n += t.chars().count();
            self.s.push_str(&t);
            self.ansi.clear();
        }
    }
    fn flush_hi(&mut self) {
        if self.hi.take().is_some() {
            self.put('\u{FFFD}');
        }
    }
    fn ch(&mut self, c: char) {
        self.flush_ansi();
        self.flush_hi();
        self.put(c);
    }
    fn byte(&mut self, b: u8) {
        self.flush_hi();
        self.ansi.push(b);
    }
    /// One UTF-16 unit from `\uN`.
    fn unit(&mut self, u: u16) {
        self.flush_ansi();
        match (self.hi.take(), u) {
            (Some(h), 0xDC00..=0xDFFF) => {
                let cp = 0x10000 + ((u32::from(h) - 0xD800) << 10) + (u32::from(u) - 0xDC00);
                self.put(char::from_u32(cp).unwrap_or('\u{FFFD}'));
            }
            (prev, _) => {
                if prev.is_some() {
                    self.put('\u{FFFD}');
                }
                if (0xD800..0xDC00).contains(&u) {
                    self.hi = Some(u);
                } else {
                    self.put(char::from_u32(u32::from(u)).unwrap_or('\u{FFFD}'));
                }
            }
        }
    }
}

/// Consumes one pending `\uN` fallback char; true if the char must be swallowed.
fn eat_fallback(fallback: &mut u32) -> bool {
    let eat = *fallback > 0;
    *fallback = fallback.saturating_sub(1);
    eat
}

/// Extracts text until at least `cap` chars are produced.
fn rtf_text_cap(rtf: &[u8], cap: usize) -> String {
    let mut o = RtfOut { s: String::new(), n: 0, hi: None, ansi: Vec::new(), cp: CP_ACP };
    let mut cur = RtfState { skip: false, uc: 1 };
    let mut stack: Vec<RtfState> = Vec::new();
    let (mut depth, mut i, mut group_start, mut fallback) = (0usize, 0usize, false, 0u32);
    while let Some(&c) = rtf.get(i) {
        if o.n >= cap {
            break;
        }
        i += 1;
        let at_group_start = std::mem::take(&mut group_start);
        match c {
            b'{' => {
                // Only the first RTF_MAX_DEPTH levels save their state; deeper ones inherit.
                if stack.len() == depth && depth < RTF_MAX_DEPTH {
                    stack.push(cur);
                }
                depth += 1;
                group_start = true;
                fallback = 0;
            }
            b'}' => {
                fallback = 0;
                if depth == 0 {
                    continue;
                }
                if stack.len() == depth {
                    cur = stack.pop().unwrap_or(cur);
                }
                depth -= 1;
                if depth == 0 {
                    break; // anything after the document's closing brace is padding
                }
            }
            b'\\' => {
                let Some(&n) = rtf.get(i) else { break };
                match n {
                    b'\'' => {
                        i += 1;
                        let hex = rtf.get(i..i + 2).and_then(|h| std::str::from_utf8(h).ok());
                        if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                            i += 2;
                            if !eat_fallback(&mut fallback) && !cur.skip {
                                o.byte(v);
                            }
                        }
                    }
                    b'\\' | b'{' | b'}' | b'~' | b'_' => {
                        i += 1;
                        let ch = match n {
                            b'~' => ' ',
                            b'_' => '-',
                            _ => char::from(n),
                        };
                        if !eat_fallback(&mut fallback) && !cur.skip {
                            o.ch(ch);
                        }
                    }
                    b'*' => {
                        i += 1;
                        cur.skip = true;
                    }
                    b'\r' | b'\n' => {
                        i += 1;
                        if !cur.skip {
                            o.ch('\n');
                        }
                    }
                    b'a'..=b'z' | b'A'..=b'Z' => {
                        fallback = 0;
                        let start = i;
                        while rtf.get(i).is_some_and(u8::is_ascii_alphabetic) {
                            i += 1;
                        }
                        let word = rtf.get(start..i).unwrap_or_default();
                        let neg = rtf.get(i) == Some(&b'-') && rtf.get(i + 1).is_some_and(u8::is_ascii_digit);
                        i += usize::from(neg);
                        let mut param: Option<i64> = None;
                        while let Some(d) = rtf.get(i).filter(|d| d.is_ascii_digit()) {
                            param = Some(param.unwrap_or(0).saturating_mul(10).saturating_add(i64::from(d - b'0')));
                            i += 1;
                        }
                        let param = param.map(|p| if neg { -p } else { p });
                        if rtf.get(i) == Some(&b' ') {
                            i += 1;
                        }
                        let sym = match word {
                            b"par" | b"line" | b"sect" | b"row" | b"page" => Some('\n'),
                            b"tab" | b"cell" => Some('\t'),
                            b"emspace" | b"enspace" | b"qmspace" => Some(' '),
                            b"emdash" => Some('\u{2014}'),
                            b"endash" => Some('\u{2013}'),
                            b"bullet" => Some('\u{2022}'),
                            b"lquote" => Some('\u{2018}'),
                            b"rquote" => Some('\u{2019}'),
                            b"ldblquote" => Some('\u{201C}'),
                            b"rdblquote" => Some('\u{201D}'),
                            _ => None,
                        };
                        if let Some(ch) = sym {
                            if !cur.skip {
                                o.ch(ch);
                            }
                        } else {
                            match (word, param) {
                                (b"u", Some(p)) => {
                                    if !cur.skip {
                                        o.unit((p & 0xFFFF) as u16);
                                    }
                                    fallback = cur.uc;
                                }
                                (b"uc", Some(p)) => cur.uc = u32::try_from(p.clamp(0, 16)).unwrap_or(1),
                                (b"ansicpg", Some(p)) => {
                                    if let Some(cp) = u32::try_from(p).ok().filter(|&cp| cp > 0) {
                                        o.flush_ansi();
                                        o.cp = cp;
                                    }
                                }
                                (b"bin", Some(p)) => i = i.saturating_add(usize::try_from(p).unwrap_or(0)),
                                _ if at_group_start && RTF_SKIP.iter().any(|w| w.as_bytes() == word) => {
                                    cur.skip = true;
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => i += 1, // other control symbols (\- \: ...) carry no text
                }
            }
            0..=0x1F | 0x7F => {} // raw CR/LF/NUL/control bytes are not text in RTF
            _ => {
                if !eat_fallback(&mut fallback) && !cur.skip {
                    if c < 0x80 {
                        o.ch(char::from(c));
                    } else {
                        o.byte(c); // non-conformant raw ANSI byte
                    }
                }
            }
        }
    }
    o.flush_ansi();
    o.flush_hi();
    let keep = o.s.trim_end_matches(['\r', '\n']).len();
    o.s.truncate(keep);
    o.s
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    fn fk(cf: u32) -> FormatKey {
        FormatKey::Standard(cf)
    }
    fn uni(s: &str) -> (FormatKey, Payload) {
        (fk(CF_UNICODETEXT), Payload::inline(unicode_bytes(s)))
    }
    fn raw(k: FormatKey, b: &[u8]) -> (FormatKey, Payload) {
        (k, Payload::inline(b.to_vec()))
    }
    fn prev(f: &[(FormatKey, Payload)]) -> String {
        build_preview(f, &pick_primary(f))
    }
    fn search(f: &[(FormatKey, Payload)]) -> String {
        search_text(f, &pick_primary(f))
    }

    /// Tiny deterministic generator for fuzz-ish loops.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn below(&mut self, n: usize) -> usize {
            self.next() as usize % n.max(1)
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next() as u8).collect()
        }
        /// Random concatenation of `toks`, sprinkled with random bytes.
        fn soup(&mut self, toks: &[&str], n: usize) -> Vec<u8> {
            let mut v = Vec::new();
            for _ in 0..n {
                if self.below(8) == 0 {
                    let k = 1 + self.below(4);
                    v.extend(self.bytes(k));
                } else {
                    v.extend(toks[self.below(toks.len())].as_bytes());
                }
            }
            v
        }
    }

    fn dib(w: i32, h: i32, bpp: u16) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(40u32.to_le_bytes());
        v.extend(w.to_le_bytes());
        v.extend(h.to_le_bytes());
        v.extend(1u16.to_le_bytes());
        v.extend(bpp.to_le_bytes());
        v.extend([0u8; 24]); // compression .. clr_important
        let stride = (w.unsigned_abs() as usize * bpp as usize).div_ceil(32) * 4;
        let palette = if bpp <= 8 { 4 << bpp } else { 0 };
        v.resize(40 + palette + stride * h.unsigned_abs() as usize, 0);
        v
    }

    fn dib_v5(w: i32, h: i32, bpp: u16) -> Vec<u8> {
        let mut v = dib(w, h, bpp);
        v[..4].copy_from_slice(&124u32.to_le_bytes());
        v.splice(40..40, [0u8; 84]); // V5 header is 124 bytes
        v
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        v.extend(w.to_be_bytes());
        v.extend(h.to_be_bytes());
        v.extend([8, 6, 0, 0, 0]);
        v
    }

    fn dropfiles(wide: bool, paths: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(20u32.to_le_bytes());
        v.extend([0u8; 12]);
        v.extend(u32::from(wide).to_le_bytes());
        for p in paths {
            if wide {
                p.encode_utf16().for_each(|u| v.extend(u.to_le_bytes()));
                v.extend([0, 0]);
            } else {
                v.extend(p.as_bytes());
                v.push(0);
            }
        }
        v.extend(if wide { &[0u8, 0][..] } else { &[0u8][..] });
        v
    }

    fn html_format(frag: &str) -> Vec<u8> {
        crate::transform::make_html_format(frag)
    }

    // ---- preview rules

    #[test]
    fn unicode_first_300_units_and_stops_at_nul() {
        let long = "x".repeat(10_000);
        assert_eq!(prev(&[uni(&long)]), "x".repeat(300));
        let mut b = unicode_bytes("abc");
        b.extend(unicode_bytes("hidden"));
        assert_eq!(prev(&[raw(fk(CF_UNICODETEXT), &b)]), "abc");
        assert_eq!(prev(&[uni("short")]), "short");
    }

    #[test]
    fn never_appends_dots_or_placeholder() {
        let p = prev(&[uni(&"y".repeat(20_000))]);
        assert_eq!(p.chars().count(), 300);
        assert!(!p.contains("..."));
        assert!(!prev(&[uni("abc")]).contains("..."));
        // Large non-ASCII text is also cut by chars, not refused.
        assert_eq!(prev(&[uni(&"\u{E9}".repeat(50_000))]).chars().count(), 300);
    }

    #[test]
    fn keeps_control_whitespace() {
        assert_eq!(prev(&[uni("a\r\nb\tc")]), "a\r\nb\tc");
    }

    #[test]
    fn whitespace_only_text_is_returned() {
        assert_eq!(prev(&[uni("   \r\n ")]), "   \r\n ");
        let f = [uni("  "), raw(fk(CF_TEXT), b"fallback\0")];
        assert_eq!(prev(&f), "  ");
    }

    #[test]
    fn empty_unicode_falls_through() {
        let f = [uni(""), raw(fk(CF_TEXT), b"ansi\0")];
        assert_eq!(prev(&f), "ansi");
    }

    #[test]
    fn surrogate_pair_at_boundary() {
        // 299 'a' + U+1F600 (2 units): unit 300 is the high surrogate -> dropped.
        let s = format!("{}\u{1F600}tail", "a".repeat(299));
        assert_eq!(prev(&[uni(&s)]), "a".repeat(299));
        // 298 'a' + pair = exactly 300 units -> kept whole.
        let s = format!("{}\u{1F600}tail", "a".repeat(298));
        assert_eq!(prev(&[uni(&s)]), format!("{}\u{1F600}", "a".repeat(298)));
        assert!(!prev(&[uni(&format!("{}\u{1F600}", "b".repeat(299)))]).contains('\u{FFFD}'));
    }

    #[test]
    fn cf_text_ansi_first_300_bytes() {
        let f = [raw(fk(CF_TEXT), "z".repeat(1000).as_bytes())];
        assert_eq!(prev(&f), "z".repeat(300));
        let f = [raw(fk(CF_TEXT), b"hello\0junk")];
        assert_eq!(prev(&f), "hello");
        // Unicode wins over ANSI.
        let f = [raw(fk(CF_TEXT), b"ansi\0"), uni("wide")];
        assert_eq!(prev(&f), "wide");
    }

    #[test]
    fn ansi_high_bytes_do_not_panic() {
        let s = decode_ansi(&[b'A', 0xE9, b'B', 0]);
        assert!(s.starts_with('A') && !s.is_empty());
        // SAFETY: plain Win32 query.
        if unsafe { windows::Win32::Globalization::GetACP() } == 1252 {
            assert_eq!(decode_ansi(&[b'c', b'a', b'f', 0xE9]), "caf\u{E9}");
        }
    }

    #[test]
    fn hdrop_preview_wide_and_ansi() {
        let f = [raw(fk(CF_HDROP), &dropfiles(true, &["C:\\a\\b.txt", "D:\\c d\\\u{E9}.png"]))];
        assert_eq!(prev(&f), "C:\\a\\b.txt, D:\\c d\\\u{E9}.png");
        let f = [raw(fk(CF_HDROP), &dropfiles(false, &["C:\\x.txt", "C:\\y.txt"]))];
        assert_eq!(prev(&f), "C:\\x.txt, C:\\y.txt");
        let many: Vec<String> = (0..100).map(|i| format!("C:\\folder\\file{i}.txt")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let f = [raw(fk(CF_HDROP), &dropfiles(true, &refs))];
        assert_eq!(prev(&f).chars().count(), 300);
    }

    #[test]
    fn hdrop_parser_edges() {
        assert!(hdrop_paths(&[]).is_empty());
        assert!(hdrop_paths(&[1, 2, 3]).is_empty());
        assert!(hdrop_paths(&dropfiles(true, &[])).is_empty());
        let mut hostile = dropfiles(true, &["a"]);
        hostile[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(hdrop_paths(&hostile).is_empty());
        hostile[..4].copy_from_slice(&4u32.to_le_bytes()); // points into the header
        assert!(hdrop_paths(&hostile).is_empty());
        // A missing terminator is tolerated.
        let mut b = dropfiles(false, &[]);
        b.truncate(20);
        b.extend(b"C:\\no_nul");
        assert_eq!(hdrop_paths(&b), vec!["C:\\no_nul"]);
    }

    #[test]
    fn html_only_preview() {
        let frag = "<p>Hello&nbsp;<b>wor</b>ld &amp; <i>co</i></p><p>second</p>";
        let f = [raw(FormatKey::reg(FMT_HTML), &html_format(frag))];
        assert_eq!(prev(&f), "Hello world & co\nsecond");
        assert_eq!(best_text(&f).as_deref(), Some("Hello world & co\nsecond"));
        let big = format!("<div>{}</div>", "word ".repeat(20_000));
        let p = prev(&[raw(FormatKey::reg(FMT_HTML), &html_format(&big))]);
        assert_eq!(p.chars().count(), 300);
        assert!(p.starts_with("word word"));
    }

    #[test]
    fn rtf_only_preview() {
        let rtf = br"{\rtf1\ansi{\fonttbl{\f0 Arial;}}\pard Hello \b World\b0\par}";
        let f = [raw(FormatKey::reg(FMT_RTF), rtf)];
        assert_eq!(prev(&f), "Hello World");
        assert_eq!(best_text(&f).as_deref(), Some("Hello World"));
        let long = format!(r"{{\rtf1 {}}}", "abcde ".repeat(10_000));
        assert_eq!(prev(&[raw(FormatKey::reg(FMT_RTF), long.as_bytes())]).chars().count(), 300);
    }

    #[test]
    fn preview_uses_all_formats_not_just_primary() {
        // Primary is the DIB (higher priority than HTML), yet the HTML text is shown.
        let f = [raw(fk(CF_DIB), &dib(2, 2, 24)), raw(FormatKey::reg(FMT_HTML), &html_format("<b>caption</b>"))];
        assert_eq!(pick_primary(&f), fk(CF_DIB));
        assert_eq!(prev(&f), "caption");
    }

    #[test]
    fn labels() {
        assert_eq!(prev(&[raw(fk(CF_DIB), &dib(1920, 1080, 24))]), "Image 1920\u{D7}1080");
        assert_eq!(prev(&[raw(fk(CF_DIBV5), &dib_v5(8, -4, 32))]), "Image 8\u{D7}4");
        assert_eq!(prev(&[raw(FormatKey::reg(FMT_PNG), &png(640, 480))]), "Image 640\u{D7}480");
        assert_eq!(prev(&[raw(FormatKey::reg("MyFormat"), b"zz")]), "MyFormat");
        assert_eq!(prev(&[raw(fk(CF_ENHMETAFILE), b"zz")]), "CF_ENHMETAFILE");
        assert_eq!(prev(&[raw(fk(CF_DIB), b"garbage")]), "CF_DIB");
        assert_eq!(prev(&[]), "CF_0");
    }

    #[test]
    fn truncated_dib_header_label() {
        let full = dib(1024, 768, 32);
        assert!(dib_size_ok(&full));
        for n in [16, 20, 40, 124] {
            let f = [raw(fk(CF_DIB), &full[..n])];
            assert_eq!(image_label(&f).as_deref(), Some("Image 1024\u{D7}768"), "n={n}");
            assert_eq!(prev(&f), "Image 1024\u{D7}768");
        }
        assert_eq!(image_label(&[raw(fk(CF_DIB), &full[..15])]), None);
        assert!(!dib_size_ok(&full[..40]));
    }

    #[test]
    fn on_disk_payloads_are_ignored() {
        let f = [
            (fk(CF_UNICODETEXT), Payload::OnDisk { sha1: [0; 20], len: 1_000_000 }),
            raw(fk(CF_TEXT), b"inline\0"),
        ];
        assert_eq!(prev(&f), "inline");
        let f = [(fk(CF_UNICODETEXT), Payload::OnDisk { sha1: [0; 20], len: 5 })];
        assert_eq!(prev(&f), "CF_UNICODETEXT");
        assert_eq!(best_text(&f), None);
    }

    // ---- best_text / search_text

    #[test]
    fn best_text_order() {
        let html = (FormatKey::reg(FMT_HTML), Payload::inline(html_format("<p>html</p>")));
        let rtf = raw(FormatKey::reg(FMT_RTF), br"{\rtf1 rtf}");
        let hd = raw(fk(CF_HDROP), &dropfiles(true, &["C:\\a", "C:\\b"]));
        let ansi = raw(fk(CF_TEXT), b"ansi");
        let all = [rtf.clone(), html.clone(), hd.clone(), ansi.clone(), uni("uni")];
        assert_eq!(best_text(&all).as_deref(), Some("uni"));
        assert_eq!(best_text(&all[..4]).as_deref(), Some("ansi"));
        assert_eq!(best_text(&all[..3]).as_deref(), Some("C:\\a\r\nC:\\b"));
        assert_eq!(best_text(&all[..2]).as_deref(), Some("html"));
        assert_eq!(best_text(&all[..1]).as_deref(), Some("rtf"));
        assert_eq!(best_text(&[raw(FormatKey::reg("X"), b"abc")]), None);
        assert_eq!(best_text(&[uni("")]), None);
        assert_eq!(best_text(&[]), None);
        assert_eq!(best_text(&[uni(&"q".repeat(10_000))]).map(|s| s.len()), Some(10_000)); // not truncated
    }

    #[test]
    fn search_text_by_kind() {
        let big = "\u{E9}".repeat(400_000); // 800,000 bytes
        let s = search(&[uni(&big)]);
        assert_eq!(s.len(), SEARCH_MAX_BYTES);
        assert!(s.chars().all(|c| c == '\u{E9}'));
        // The limit lands inside a 3-byte char (512000 % 3 == 2): cut back to the boundary.
        let s = search(&[uni(&"\u{20AC}".repeat(200_000))]);
        assert_eq!(s.len(), SEARCH_MAX_BYTES - 2);
        assert_eq!(search(&[uni("find me")]), "find me");
        let f = [raw(fk(CF_HDROP), &dropfiles(true, &["C:\\a.txt", "C:\\b.txt"]))];
        assert_eq!(search(&f), "C:\\a.txt\nC:\\b.txt");
        assert_eq!(search(&[raw(fk(CF_DIB), &dib(3, 5, 24))]), "Image 3\u{D7}5");
        assert_eq!(search(&[raw(FormatKey::reg("Foo"), b"x")]), "Foo");
        assert_eq!(search(&[raw(fk(CF_ENHMETAFILE), b"x")]), "CF_ENHMETAFILE");
        let f = [raw(FormatKey::reg(FMT_HTML), &html_format("<i>only html</i>"))];
        assert_eq!(search(&f), "only html");
    }

    // ---- unicode helpers

    #[test]
    fn unicode_roundtrip_and_odd_bytes() {
        let s = "h\u{E9}llo \u{1F600} w\u{F6}rld";
        let b = unicode_bytes(s);
        assert_eq!(b.len(), (s.encode_utf16().count() + 1) * 2);
        assert_eq!(decode_unicode(&b), s);
        let mut odd = unicode_bytes("ab");
        odd.pop(); // drops half of the NUL
        assert_eq!(decode_unicode(&odd), "ab");
        assert_eq!(decode_unicode(&[0x41]), "");
        assert_eq!(decode_unicode(&[]), "");
        assert_eq!(decode_unicode(&[0x00, 0xD8, 0x41, 0x00]), "\u{FFFD}A"); // lone surrogate
    }

    // ---- html_fragment

    #[test]
    fn html_fragment_valid_offsets() {
        assert_eq!(html_fragment(&html_format("<b>x</b> y")).as_deref(), Some("<b>x</b> y"));
        // Hand-built Chrome-like payload.
        let doc = "<html><body><!--StartFragment-->FRAG<!--EndFragment--></body></html>";
        let sf = 105 + doc.find("FRAG").unwrap();
        let hdr = |ef: usize| {
            format!(
                "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{sf:010}\r\nEndFragment:{ef:010}\r\n",
                105,
                105 + doc.len()
            )
        };
        let p = format!("{}{doc}", hdr(sf + 4));
        assert_eq!(&p[..105], &hdr(sf + 4)[..]);
        assert_eq!(html_fragment(p.as_bytes()).as_deref(), Some("FRAG"));
        // Trailing NUL padding is ignored.
        let mut padded = p.into_bytes();
        padded.extend([0, 0, 0]);
        assert_eq!(html_fragment(&padded).as_deref(), Some("FRAG"));
    }

    #[test]
    fn html_fragment_utf8() {
        let b = html_format("caf\u{E9} \u{1F600}");
        assert_eq!(html_fragment(&b).as_deref(), Some("caf\u{E9} \u{1F600}"));
    }

    #[test]
    fn html_fragment_hostile_offsets() {
        let ok = String::from_utf8(html_format("<i>keep</i>")).unwrap();
        let with = |edits: &[(&str, &str)]| -> Vec<u8> {
            let mut r = ok.clone();
            for (key, val) in edits {
                let at = r.find(key).unwrap() + key.len();
                r.replace_range(at..at + val.len(), val);
            }
            r.into_bytes()
        };
        for bad in ["4294967295", "0000000000", "0000000001", "9999999999"] {
            for key in ["StartFragment:", "EndFragment:", "StartHTML:", "EndHTML:"] {
                assert!(html_fragment(&with(&[(key, bad)])).is_some(), "{key}{bad}");
            }
        }
        // Reversed fragment -> falls back to StartHTML..EndHTML.
        let r = html_fragment(&with(&[("StartFragment:", "0000000140"), ("EndFragment:", "0000000130")])).unwrap();
        assert!(r.starts_with("<html>") && r.contains("<i>keep</i>"));
        // Everything out of range -> whole body after the header.
        let r = html_fragment(&with(&[("StartHTML:", "9999999999"), ("EndHTML:", "9999999999"), ("EndFragment:", "9999999999")])).unwrap();
        assert!(r.starts_with("<html>") && !r.contains("Version"));
        // Offsets pointing into the header are not trusted.
        let r = html_fragment(&with(&[("StartFragment:", "0000000000")])).unwrap();
        assert!(!r.contains("Version"));
        let r = html_fragment(&with(&[("StartFragment:", "0000000000"), ("StartHTML:", "0000000000")])).unwrap();
        assert!(!r.contains("Version"));
    }

    #[test]
    fn html_fragment_missing_header() {
        assert_eq!(html_fragment(b"<b>raw</b>").as_deref(), Some("<b>raw</b>"));
        assert_eq!(html_fragment(b"Note: plain, no header").as_deref(), Some("Note: plain, no header"));
        assert_eq!(html_fragment(b"Version:0.9\r\n<p>x</p>").as_deref(), Some("<p>x</p>"));
        assert_eq!(html_fragment(b"Version:0.9\r\nStartHTML:0000000013\r\n<p>x</p>").as_deref(), Some("<p>x</p>"));
        assert_eq!(html_fragment(b""), None);
        assert_eq!(html_fragment(&[0, 0, 0]), None);
        assert_eq!(html_fragment(b"Version:0.9\r\n"), None);
        assert_eq!(
            html_fragment(b"Version:0.9\r\nStartHTML:-1\r\nEndHTML:abc\r\n<p>y</p>").as_deref(),
            Some("<p>y</p>")
        );
    }

    // ---- html_to_text

    #[test]
    fn html_chrome_snippet() {
        let h = "<meta charset='utf-8'><span style=\"color: rgb(0, 0, 0); font-family: Arial;\">Hello&nbsp;<b>world</b></span><br><a href=\"https://x.y/?a=1&amp;b=2\" title='a > b'>link</a><div>last</div>";
        assert_eq!(html_to_text(h), "Hello world\nlink\nlast");
    }

    #[test]
    fn html_word_snippet() {
        let h = "<html xmlns:o=\"urn:schemas-microsoft-com:office:office\"><head><title>T</title><style>p {margin:0}</style></head>\
<body><!--[if gte mso 9]><xml><o:x/></xml><![endif]--><p class=MsoNormal>First<o:p></o:p></p>\r\n\
<p class=MsoNormal><o:p>&nbsp;</o:p></p><p class=MsoNormal>Se&shy;cond  line\r\n continues</p></body></html>";
        assert_eq!(html_to_text(h), "First\nSe\u{AD}cond line continues");
    }

    #[test]
    fn html_blocks_cells_and_breaks() {
        assert_eq!(html_to_text("<table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>"), "a\tb\nc\td");
        assert_eq!(html_to_text("<ul><li>one</li><li>two</li></ul>"), "one\ntwo");
        assert_eq!(html_to_text("a<br>b<br><br>c<br><br><br><br>d"), "a\nb\n\nc\n\nd");
        assert_eq!(html_to_text("<h1>T</h1><h2>S</h2>x"), "T\nS\nx");
        assert_eq!(html_to_text("<br><br>  lead<br>"), "lead");
        assert_eq!(html_to_text("   \n\t  "), "");
        assert_eq!(html_to_text("a   b\n\n\tc"), "a b c");
        assert_eq!(html_to_text("<b>a</b><i>b</i>"), "ab");
        assert_eq!(html_to_text("<pre>x\n  y\n\n\nz</pre>"), "x\n  y\n\nz");
    }

    #[test]
    fn html_skips_script_style_head_comments() {
        assert_eq!(html_to_text("a<script>var x = '<b>';</script>b<style>.a{}</style>c"), "abc");
        assert_eq!(html_to_text("a<SCRIPT type=x>alert(1)</SCRIPT >b"), "ab");
        assert_eq!(html_to_text("<head><title>no</title></head>yes"), "yes");
        assert_eq!(html_to_text("a<!-- hidden <b>x</b> -->b"), "ab");
        assert_eq!(html_to_text("a<!-- unterminated"), "a");
        assert_eq!(html_to_text("<!DOCTYPE html><?xml version='1.0'?>x"), "x");
        assert_eq!(html_to_text("x<script>never closed"), "x");
        assert_eq!(html_to_text("<head>x"), "x"); // an unclosed head is not swallowed
    }

    #[test]
    fn html_entities() {
        assert_eq!(html_to_text("&lt;a&gt; &amp; &quot;q&quot; &apos;s&apos;"), "<a> & \"q\" 's'");
        assert_eq!(
            html_to_text("&copy; &reg; &hellip; &ndash; &mdash; &laquo;x&raquo; &bull; &middot; &euro;"),
            "\u{A9} \u{AE} \u{2026} \u{2013} \u{2014} \u{AB}x\u{BB} \u{2022} \u{B7} \u{20AC}"
        );
        assert_eq!(html_to_text("&aacute;&eacute;&ntilde;&Ntilde;&uuml;&yuml;&szlig;"), "\u{E1}\u{E9}\u{F1}\u{D1}\u{FC}\u{FF}\u{DF}");
        assert_eq!(html_to_text("&#65;&#x42;&#X43;&#128512;"), "ABC\u{1F600}");
        assert_eq!(html_to_text("&nbsp;&nbsp;a&nbsp;b"), "a b");
        assert_eq!(html_to_text("AT&T &unknown; &#xZZ; &#; &amp"), "AT&T &unknown; &#xZZ; &#; &amp");
        assert_eq!(html_to_text("&#0; &#xD800; &#1114112;"), "&#0; &#xD800; &#1114112;");
        assert_eq!(html_to_text("5 < 6 and 7 > 3"), "5 < 6 and 7 > 3");
        assert_eq!(decode_entities("a &amp; b &lt; &#33; &zzz; &"), "a & b < ! &zzz; &");
        assert_eq!(html_to_text("&#146;").chars().count(), 1); // Windows-1252 range
    }

    #[test]
    fn html_cap_is_a_prefix() {
        let h = "<p>alpha beta</p>".repeat(500);
        let full = html_to_text(&h);
        let cut = html_to_text_cap(&h, 40);
        assert!((40..80).contains(&cut.chars().count()));
        assert!(full.starts_with(&cut));
    }

    #[test]
    fn html_pathological_inputs_are_fast() {
        let cases = [
            "<head>".repeat(200_000),
            "<title>".repeat(200_000),
            "&".repeat(500_000),
            "<".repeat(500_000),
            "</".repeat(250_000),
            "<a ".repeat(100_000),
            "<!--".repeat(100_000),
            "<script>".repeat(100_000),
            "<a href='".repeat(100_000),
        ];
        for s in cases {
            let t = std::time::Instant::now();
            let _ = html_to_text(&s);
            assert!(t.elapsed().as_secs() < 5);
        }
    }

    // ---- rtf_to_text

    #[test]
    fn rtf_word_snippet() {
        let rtf = b"{\\rtf1\\ansi\\ansicpg1252\\deff0\\nouicompat\\deflang1033{\\fonttbl{\\f0\\fnil\\fcharset0 Calibri;}}\r\n\
{\\colortbl ;\\red255\\green0\\blue0;}\r\n{\\*\\generator Riched20 10.0.19041}\\viewkind4\\uc1 \r\n\
\\pard\\sa200\\sl276\\slmult1\\f0\\fs22\\lang9 Hello \\b world\\b0  caf\\'e9 \\u8364? \\u-10179?\\u-8704?\\par\r\n\
Second line\\line break\\tab tabbed\\par\r\n}\r\n\0\0";
        assert_eq!(rtf_to_text(rtf), "Hello world caf\u{E9} \u{20AC} \u{1F600}\nSecond line\nbreak\ttabbed");
    }

    #[test]
    fn rtf_field_hyperlink_and_skipped_groups() {
        let rtf = br#"{\rtf1\ansi{\stylesheet{\s1 Heading 1;}}{\info{\title Secret}{\author Me}}
{\header Page header}{\footer\pard Page footer}
{\field{\*\fldinst{HYPERLINK "http://example.com"}}{\fldrslt{\ul click here}}} then {\pict\wmetafile8\picw100 0102{030405}} text{\footnote note}.}"#;
        assert_eq!(rtf_to_text(rtf), "click here then  text.");
    }

    #[test]
    fn rtf_escapes_and_symbols() {
        assert_eq!(rtf_to_text(br"{\rtf1 a\{b\}c\\d\~e\_f\-g}"), "a{b}c\\d e-fg");
        assert_eq!(
            rtf_to_text(br"{\rtf1 \emdash\endash\bullet\lquote\rquote\ldblquote\rdblquote}"),
            "\u{2014}\u{2013}\u{2022}\u{2018}\u{2019}\u{201C}\u{201D}"
        );
        assert_eq!(rtf_to_text(br"{\rtf1 one\par two\par\par three\par}"), "one\ntwo\n\nthree");
        assert_eq!(rtf_to_text(b"{\\rtf1 line\\\ncont}"), "line\ncont");
        assert_eq!(rtf_to_text(br"{\rtf1 a\cell b\row}"), "a\tb");
    }

    #[test]
    fn rtf_unicode_and_uc() {
        assert_eq!(rtf_to_text(br"{\rtf1 \u233?x}"), "\u{E9}x");
        assert_eq!(rtf_to_text(br"{\rtf1 \uc2\u233 ??x}"), "\u{E9}x");
        assert_eq!(rtf_to_text(br"{\rtf1 \uc0\u233x}"), "\u{E9}x");
        assert_eq!(rtf_to_text(br"{\rtf1 \u233\'e9x}"), "\u{E9}x"); // fallback \'hh is skipped
        assert_eq!(rtf_to_text(br"{\rtf1 \u55357?\u56832?!}"), "\u{1F600}!");
        assert_eq!(rtf_to_text(br"{\rtf1 \u55357?x}"), "\u{FFFD}x"); // lone high surrogate
        assert_eq!(rtf_to_text(br"{\rtf1 \u56832?x}"), "\u{FFFD}x"); // lone low surrogate
        assert_eq!(rtf_to_text(br"{\rtf1 \u99999999999999999999?x}"), "\u{FFFF}x");
    }

    #[test]
    fn rtf_ansi_code_pages() {
        assert_eq!(rtf_to_text(br"{\rtf1\ansi\ansicpg1252 caf\'e9 \'80}"), "caf\u{E9} \u{20AC}");
        assert_eq!(rtf_to_text(br"{\rtf1\ansi\ansicpg1251 \'cf\'f0\'e8}"), "\u{41F}\u{440}\u{438}");
        assert_eq!(rtf_to_text(br"{\rtf1\ansi\ansicpg932 \'93\'fa\'96\'7b}"), "\u{65E5}\u{672C}"); // DBCS pairs stay together
        assert_eq!(rtf_to_text(br"{\rtf1\ansicpg99999 abc \'e9}").chars().count(), 5); // unknown cp -> Latin-1
        assert!(rtf_to_text(br"{\rtf1 bad\'zz hex\'4}").starts_with("bad"));
    }

    #[test]
    fn rtf_misc_robustness() {
        assert_eq!(rtf_to_text(b""), "");
        assert_eq!(rtf_to_text(b"plain text no braces"), "plain text no braces");
        assert_eq!(rtf_to_text(b"{\\rtf1 unbalanced {{{ open"), "unbalanced  open");
        assert_eq!(rtf_to_text(b"}}}{\\rtf1 x}"), "x");
        assert_eq!(rtf_to_text(br"{\rtf1 a\bin3 {}}b}"), "ab");
        assert_eq!(rtf_to_text(b"{\\rtf1 trailing\\"), "trailing");
        let deep = format!("{{\\rtf1 {}deep{}tail}}", "{".repeat(100_000), "}".repeat(100_000));
        assert_eq!(rtf_to_text(deep.as_bytes()), "deeptail");
        assert!(rtf_to_text(&b"{\\x".repeat(100_000)).is_empty());
        assert_eq!(rtf_to_text(br"{\rtf1 x}after the end"), "x");
    }

    // ---- DIB / PNG

    #[test]
    fn dib_info_variants() {
        assert_eq!(dib_info(&dib(10, 20, 24)), Some(DibInfo { width: 10, height: 20, bpp: 24, header_size: 40 }));
        assert_eq!(dib_info(&dib(10, -20, 24)).map(|d| d.height), Some(20));
        let mut core = vec![0u8; 12];
        core[..4].copy_from_slice(&12u32.to_le_bytes());
        core[4..6].copy_from_slice(&7u16.to_le_bytes());
        core[6..8].copy_from_slice(&9u16.to_le_bytes());
        core[8..10].copy_from_slice(&1u16.to_le_bytes());
        core[10..12].copy_from_slice(&8u16.to_le_bytes());
        assert_eq!(dib_info(&core), Some(DibInfo { width: 7, height: 9, bpp: 8, header_size: 12 }));
        for hs in [52u32, 56, 108, 124] {
            let mut v = dib(5, 6, 32);
            v[..4].copy_from_slice(&hs.to_le_bytes());
            assert_eq!(dib_info(&v).map(|d| (d.width, d.header_size)), Some((5, hs)));
        }
        for bad in [0u32, 1, 39, 41, 64, 125, u32::MAX] {
            let mut v = dib(5, 6, 32);
            v[..4].copy_from_slice(&bad.to_le_bytes());
            assert_eq!(dib_info(&v), None);
        }
        assert_eq!(dib_info(&dib(0, 5, 24)), None);
        assert_eq!(dib_info(&dib(5, 0, 24)), None);
        assert_eq!(dib_info(&dib(-5, 5, 24)), None);
        let mut min_h = dib(5, 5, 24);
        min_h[8..12].copy_from_slice(&i32::MIN.to_le_bytes());
        assert_eq!(dib_info(&min_h), None);
        assert_eq!(dib_info(&dib(5, 5, 7)), None);
        assert_eq!(dib_info(&[]), None);
    }

    #[test]
    fn dib_size_checks() {
        assert!(dib_size_ok(&dib(100, 100, 24)));
        assert!(dib_size_ok(&dib(3, 3, 32)));
        assert!(dib_size_ok(&dib(33, 2, 1)));
        // Claims more data than present.
        let mut short = dib(100, 100, 24);
        short.pop();
        assert!(!dib_size_ok(&short));
        // Huge claimed dimensions, tiny buffer.
        for (w, h) in [(40_000, 40_000), (i32::MAX, i32::MAX), (i32::MAX, 1), (1, i32::MAX)] {
            let mut v = dib(1, 1, 32);
            v[4..8].copy_from_slice(&w.to_le_bytes());
            v[8..12].copy_from_slice(&h.to_le_bytes());
            assert!(dib_info(&v).is_some());
            assert!(!dib_size_ok(&v), "{w}x{h}");
        }
        // Palette: 8bpp has 256 entries by default; a bogus clr_used is rejected.
        let p8 = dib(4, 4, 8);
        assert!(dib_size_ok(&p8));
        let mut bad = p8.clone();
        bad[32..36].copy_from_slice(&1000u32.to_le_bytes());
        assert!(!dib_size_ok(&bad));
        // Compression other than RGB/BITFIELDS is not trusted.
        let mut rle = dib(4, 4, 8);
        rle[16..20].copy_from_slice(&1u32.to_le_bytes());
        assert!(!dib_size_ok(&rle));
        // BITFIELDS adds 12 mask bytes after a 40-byte header.
        let mut bf = dib(2, 2, 32);
        bf[16..20].copy_from_slice(&3u32.to_le_bytes());
        assert!(!dib_size_ok(&bf));
        bf.extend([0u8; 12]);
        assert!(dib_size_ok(&bf));
        // V5 header.
        assert!(dib_size_ok(&dib_v5(10, 10, 32)));
    }

    #[test]
    fn png_labels() {
        assert_eq!(image_label(&[raw(FormatKey::reg(FMT_PNG), &png(1, 2))]).as_deref(), Some("Image 1\u{D7}2"));
        assert_eq!(image_label(&[raw(FormatKey::reg(FMT_PNG), &png(0, 2))]), None);
        assert_eq!(image_label(&[raw(FormatKey::reg(FMT_PNG), &png(1, 2)[..20])]), None);
        assert_eq!(image_label(&[raw(FormatKey::reg(FMT_PNG), b"not a png at all, no sir")]), None);
        assert_eq!(image_label(&[]), None);
        // DIB takes precedence over PNG; DIBV5 over DIB.
        let f = [raw(FormatKey::reg(FMT_PNG), &png(9, 9)), raw(fk(CF_DIB), &dib(2, 3, 24))];
        assert_eq!(image_label(&f).as_deref(), Some("Image 2\u{D7}3"));
        let f = [raw(fk(CF_DIB), &dib(2, 3, 24)), raw(fk(CF_DIBV5), &dib_v5(4, 5, 24))];
        assert_eq!(image_label(&f).as_deref(), Some("Image 4\u{D7}5"));
    }

    // ---- fuzz-ish: nothing may panic or hang

    #[test]
    fn fuzz_random_bytes_never_panic() {
        let mut r = Lcg(0x1234_5678_9abc_def0);
        for round in 0..1500 {
            let n = r.below(if round % 50 == 0 { 3000 } else { 200 });
            let b = r.bytes(n);
            let _ = (decode_unicode(&b), decode_ansi(&b), html_fragment(&b), hdrop_paths(&b), dib_info(&b), dib_size_ok(&b));
            let _ = (rtf_to_text(&b), html_to_text(&String::from_utf8_lossy(&b)));
            let f = [
                raw(fk(CF_UNICODETEXT), &b),
                raw(fk(CF_TEXT), &b),
                raw(fk(CF_HDROP), &b),
                raw(fk(CF_DIB), &b),
                raw(fk(CF_DIBV5), &b),
                raw(FormatKey::reg(FMT_HTML), &b),
                raw(FormatKey::reg(FMT_RTF), &b),
                raw(FormatKey::reg(FMT_PNG), &b),
            ];
            for one in f.chunks(1) {
                let _ = (prev(one), search(one), best_text(one), image_label(one));
            }
            let _ = (prev(&f), search(&f), best_text(&f));
        }
    }

    #[test]
    fn fuzz_structured_tokens_never_panic() {
        let html = [
            "<", ">", "&", ";", "#", "x", "<!--", "-->", "<script>", "</script>", "<head>", "</head>", "<br>", "</p>",
            "<p>", "&amp;", "&#x1F600;", "&#0;", "\"", "'", "a", " ", "\n", "<td>", "</td>", "\u{E9}", "\u{1F600}",
            "<pre>", "</pre>", "<title>", "</title>", "<!", "<?", "</", "&#", "&#x", "=", "<a href=\"", "<b>",
        ];
        let rtf = [
            "{", "}", "\\", "\\'", "\\u", "\\u-1", "\\u55357?", "\\u56832?", "\\uc0", "\\uc2", "\\par", "\\*", "\\bin5",
            "\\bin99999999999", "\\fonttbl", "abc", " ", "\\'e9", "\\ansicpg1252", "\\ansicpg99999999999999999999", "\\~",
            "\\-", "\\\n", "?", "\\u99999999999999999999", "\\tab", "\\field", "\\pict",
        ];
        let hdr = [
            "Version:0.9\r\n", "StartHTML:", "EndHTML:", "StartFragment:", "EndFragment:", "0000000105", "4294967295",
            "-1", "\r\n", "<html>", "<!--StartFragment-->", "x", ":", "99999999999999999999999",
        ];
        let mut r = Lcg(42);
        for _ in 0..800 {
            let n = r.below(60);
            let h = r.soup(&html, n);
            let _ = html_to_text(&String::from_utf8_lossy(&h));
            let _ = decode_entities(&String::from_utf8_lossy(&h));
            let g = r.soup(&rtf, n);
            let _ = rtf_to_text(&g);
            let k = r.soup(&hdr, n);
            let _ = html_fragment(&k);
            let _ = prev(&[raw(FormatKey::reg(FMT_HTML), &k), raw(FormatKey::reg(FMT_RTF), &g)]);
        }
    }

    #[test]
    fn fuzz_mutated_valid_inputs() {
        let mut r = Lcg(7);
        let seeds: Vec<(FormatKey, Vec<u8>)> = vec![
            (fk(CF_HDROP), dropfiles(true, &["C:\\a", "C:\\b"])),
            (fk(CF_HDROP), dropfiles(false, &["C:\\a", "C:\\b"])),
            (fk(CF_DIB), dib(9, 9, 24)),
            (fk(CF_DIBV5), dib_v5(9, 9, 32)),
            (FormatKey::reg(FMT_HTML), html_format("<p>a &amp; b</p>")),
            (FormatKey::reg(FMT_RTF), br"{\rtf1\ansi{\fonttbl{\f0 A;}}\pard x\u233? y\par}".to_vec()),
            (FormatKey::reg(FMT_PNG), png(5, 5)),
            (fk(CF_UNICODETEXT), unicode_bytes("h\u{E9}llo \u{1F600}")),
        ];
        for _ in 0..3000 {
            let (k, s) = &seeds[r.below(seeds.len())];
            let mut b = s.clone();
            for _ in 0..1 + r.below(4) {
                match r.below(3) {
                    0 if !b.is_empty() => {
                        let at = r.below(b.len());
                        b[at] = r.next() as u8;
                    }
                    1 => b.truncate(r.below(b.len() + 1)),
                    _ => {
                        let at = r.below(b.len() + 1);
                        b.insert(at, r.next() as u8);
                    }
                }
            }
            let f = [raw(k.clone(), &b)];
            let _ = (prev(&f), search(&f), best_text(&f), image_label(&f), dib_size_ok(&b));
        }
    }
}
