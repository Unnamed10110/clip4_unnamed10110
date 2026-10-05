//! CLP4 (de)serialisation and clip2 import (spec 8.2, 8.5, 8.6).
//!
//! Pure byte-level logic. DPAPI wrapping and blob I/O live in the Win32 layer; this module
//! works on the plaintext "Inner" and the outer header. Every read is bounds-checked and
//! corrupt input degrades ("skip this item" / "stop here, keep what was read"), never panics.

use crate::model::*;
use std::borrow::Cow;
use std::sync::Arc;

pub const FILE_MAGIC: &[u8; 4] = b"CLP4";
pub const FILE_VERSION: u32 = 1;
/// Per-format cap on load (inline bytes).
pub const MAX_FORMAT_BYTES: usize = 16 * 1024 * 1024;
/// Per-item cap on load (inline bytes).
pub const MAX_ITEM_BYTES: usize = 16 * 1024 * 1024;
/// Unpinned item cap on load; pinned items beyond it are still kept.
pub const MAX_ITEMS: usize = 2000;

/// Smallest possible serialised item (id + ms + flags + formatCount).
const MIN_ITEM_BYTES: usize = 8 + 8 + 1 + 4;
/// First id of registered clipboard formats.
const FIRST_REGISTERED: u32 = 0xC000;
/// clip2 `size` value meaning "payload is a 20-byte SHA-1 into the blob dir".
const CLIP2_BLOB_MARK: u32 = 0xFFFF_FFFF;

// ---------------------------------------------------------------- cursor

/// Bounds-checked reader: every accessor returns `None` instead of reading past the end.
struct Cur<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Cur { b, pos: 0 }
    }
    fn left(&self) -> usize {
        self.b.len().saturating_sub(self.pos)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.b.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }
    fn u16(&mut self) -> Option<u16> {
        self.arr().map(u16::from_le_bytes)
    }
    fn u32(&mut self) -> Option<u32> {
        self.arr().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.arr().map(u64::from_le_bytes)
    }
    fn i64(&mut self) -> Option<i64> {
        self.arr().map(i64::from_le_bytes)
    }
}

// ---------------------------------------------------------------- shared item assembly

/// A payload as read from a file, before the caps decide whether it is kept (and copied).
enum Raw<'a> {
    Bytes(&'a [u8]),
    Blob { sha1: [u8; 20], len: u64 },
}

/// Collects the formats of one item, applying duplicate-key and size caps.
#[derive(Default)]
struct Acc {
    formats: Vec<(FormatKey, Payload)>,
    inline: usize,
    /// The per-item cap was hit: everything after it is dropped.
    full: bool,
}

impl Acc {
    fn push(&mut self, key: FormatKey, raw: Raw<'_>) {
        if self.full || self.formats.iter().any(|(k, _)| *k == key) {
            return;
        }
        let payload = match raw {
            Raw::Bytes(b) => {
                if b.len() > MAX_FORMAT_BYTES {
                    return;
                }
                if self.inline + b.len() > MAX_ITEM_BYTES {
                    self.full = true;
                    return;
                }
                self.inline += b.len();
                Payload::Inline(Arc::from(b))
            }
            Raw::Blob { sha1, len } => Payload::OnDisk { sha1, len },
        };
        self.formats.push((key, payload));
    }
}

/// Bytes that let `Item::new` derive preview/search for a dehydrated format.
fn stand_in(key: &FormatKey, p: &Payload, blobs: &dyn BlobSource) -> Option<Payload> {
    let Payload::OnDisk { sha1, .. } = p else {
        return Some(p.clone());
    };
    let textual = key.is_std(CF_UNICODETEXT)
        || key.is_std(CF_TEXT)
        || key.is_std(CF_HDROP)
        || key.is_named(FMT_HTML)
        || key.is_named(FMT_RTF);
    let bytes = if textual {
        blobs.read_all(sha1)
    } else if key.is_std(CF_DIB) || key.is_std(CF_DIBV5) {
        blobs.read_head(sha1)
    } else {
        None
    };
    bytes.map(Payload::inline)
}

/// Builds the final item; `None` when no format survived. A format whose blob cannot be
/// read stays in the item (as `OnDisk`) but is left out of the preview derivation.
fn build_item(
    id: u64,
    unix_ms: i64,
    pinned: bool,
    formats: Vec<(FormatKey, Payload)>,
    blobs: &dyn BlobSource,
) -> Option<Item> {
    if formats.is_empty() {
        return None;
    }
    let mut derive: Vec<_> = formats
        .iter()
        .filter_map(|(k, p)| Some((k.clone(), stand_in(k, p, blobs)?)))
        .collect();
    if derive.is_empty() {
        // Nothing readable: derive from empty stand-ins so primary/kind/label still come out right.
        derive = formats.iter().map(|(k, _)| (k.clone(), Payload::inline(Vec::new()))).collect();
    }
    let mut item = Item::new(id, unix_ms, pinned, derive);
    item.primary = pick_primary(&formats);
    item.kind = kind_of(&item.primary);
    item.formats = formats;
    Some(item)
}

// ---------------------------------------------------------------- CLP4 encode

fn encodable(k: &FormatKey, p: &Payload) -> bool {
    let key_ok = match k {
        FormatKey::Standard(_) => true,
        FormatKey::Registered(n) => n.encode_utf16().count() <= usize::from(u16::MAX),
    };
    key_ok && !matches!(p, Payload::Inline(b) if u32::try_from(b.len()).is_err())
}

/// `Inner := u32 itemCount Item*` (spec 8.2). Formats that cannot be represented
/// (name > 65535 UTF-16 units, inline payload > 4 GiB) are skipped.
pub fn encode_inner(items: &[Item]) -> Vec<u8> {
    let mut o = Vec::new();
    o.extend((items.len() as u32).to_le_bytes());
    for it in items {
        let fmts: Vec<_> = it.formats.iter().filter(|(k, p)| encodable(k, p)).collect();
        o.extend(it.id.to_le_bytes());
        o.extend(it.unix_ms.to_le_bytes());
        o.push(u8::from(it.pinned));
        o.extend((fmts.len() as u32).to_le_bytes());
        for (k, p) in fmts {
            match k {
                FormatKey::Standard(id) => {
                    o.push(0);
                    o.extend(id.to_le_bytes());
                }
                FormatKey::Registered(name) => {
                    let units: Vec<u16> = name.encode_utf16().collect();
                    o.push(1);
                    o.extend((units.len() as u16).to_le_bytes());
                    o.extend(units.iter().flat_map(|u| u.to_le_bytes()));
                }
            }
            match p {
                Payload::Inline(b) => {
                    o.push(0);
                    o.extend((b.len() as u32).to_le_bytes());
                    o.extend_from_slice(b);
                }
                Payload::OnDisk { sha1, len } => {
                    o.push(1);
                    o.extend(len.to_le_bytes());
                    o.extend_from_slice(sha1);
                }
            }
        }
    }
    o
}

// ---------------------------------------------------------------- CLP4 decode

/// `Some(None)`: well-formed but unusable key (format dropped); `None`: corrupt (stop).
fn read_key(c: &mut Cur) -> Option<Option<FormatKey>> {
    match c.u8()? {
        0 => {
            let id = c.u32()?;
            Some((1..FIRST_REGISTERED).contains(&id).then_some(FormatKey::Standard(id)))
        }
        1 => {
            let n = usize::from(c.u16()?);
            let raw = c.take(n * 2)?;
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|p| u16::from_le_bytes(p.try_into().unwrap_or_default()))
                .collect();
            let name = String::from_utf16_lossy(&units);
            Some((!name.is_empty()).then_some(FormatKey::Registered(name)))
        }
        _ => None,
    }
}

fn read_payload<'a>(c: &mut Cur<'a>) -> Option<Raw<'a>> {
    match c.u8()? {
        0 => {
            let n = c.u32()? as usize;
            Some(Raw::Bytes(c.take(n)?))
        }
        1 => {
            let len = c.u64()?;
            Some(Raw::Blob { sha1: c.arr()?, len })
        }
        _ => None,
    }
}

type Record = (u64, i64, bool, Vec<(FormatKey, Payload)>);

fn read_record(c: &mut Cur) -> Option<Record> {
    let (id, ms, flags, n) = (c.u64()?, c.i64()?, c.u8()?, c.u32()?);
    let mut acc = Acc::default();
    for _ in 0..n {
        let key = read_key(c)?;
        let raw = read_payload(c)?;
        if let Some(k) = key {
            acc.push(k, raw);
        }
    }
    Some((id, ms, flags & 1 != 0, acc.formats))
}

/// Decodes `Inner`. Truncated/corrupt data ends the load early and keeps what was read.
pub fn decode_inner(buf: &[u8], blobs: &dyn BlobSource) -> Vec<Item> {
    let mut c = Cur::new(buf);
    let Some(count) = c.u32() else {
        return Vec::new();
    };
    let mut items = Vec::with_capacity((count as usize).min(c.left() / MIN_ITEM_BYTES).min(MAX_ITEMS));
    let mut unpinned = 0;
    for _ in 0..count {
        let Some((id, ms, pinned, formats)) = read_record(&mut c) else {
            break;
        };
        if !pinned {
            if unpinned >= MAX_ITEMS {
                continue;
            }
            unpinned += 1;
        }
        items.extend(build_item(id, ms, pinned, formats, blobs));
    }
    items
}

// ---------------------------------------------------------------- outer file

/// `"CLP4" u32 version(=1) u32 blobLen` followed by the DPAPI blob.
pub fn wrap_file(dpapi_blob: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(12 + dpapi_blob.len());
    o.extend_from_slice(FILE_MAGIC);
    o.extend(FILE_VERSION.to_le_bytes());
    o.extend((dpapi_blob.len() as u32).to_le_bytes());
    o.extend_from_slice(dpapi_blob);
    o
}

/// Validates magic, version and length; returns the DPAPI blob.
pub fn unwrap_file(file: &[u8]) -> Option<&[u8]> {
    let mut c = Cur::new(file);
    if c.take(4)? != FILE_MAGIC || c.u32()? != FILE_VERSION {
        return None;
    }
    let len = c.u32()? as usize;
    c.take(len)
}

// ---------------------------------------------------------------- clip2 import (spec 8.6)

pub enum Clip2File<'a> {
    /// `"CLP3" u32 blobLen blob`: the DPAPI-protected Inner.
    Dpapi(&'a [u8]),
    /// Legacy plaintext Inner (starts with `"CLP2"`).
    Plain(&'a [u8]),
    Invalid,
}

pub fn clip2_classify(file: &[u8]) -> Clip2File<'_> {
    if file.starts_with(b"CLP2") {
        return Clip2File::Plain(file);
    }
    let mut c = Cur::new(file);
    if c.take(4) == Some(b"CLP3".as_slice()) {
        if let Some(blob) = c.u32().and_then(|n| c.take(n as usize)) {
            return Clip2File::Dpapi(blob);
        }
    }
    Clip2File::Invalid
}

/// clip2 stored raw ids; registered ones are meaningless, so identify them by content.
fn sniff(b: &[u8]) -> Option<FormatKey> {
    if b.starts_with(b"Version:") && b.windows(10).any(|w| w == b"StartHTML:") {
        Some(FormatKey::reg(FMT_HTML))
    } else if b.starts_with(b"{\\rtf") {
        Some(FormatKey::reg(FMT_RTF))
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(FormatKey::reg(FMT_PNG))
    } else {
        None
    }
}

/// version 1 record: `u32 fmt, u32 size, bytes` (text only, unpinned).
fn clip2_record_v1(c: &mut Cur) -> Option<(bool, Acc)> {
    let fmt = c.u32()?;
    let size = c.u32()? as usize;
    let bytes = c.take(size)?;
    let mut acc = Acc::default();
    if fmt == CF_UNICODETEXT || fmt == CF_TEXT {
        acc.push(FormatKey::Standard(fmt), Raw::Bytes(bytes));
    }
    Some((false, acc))
}

/// version 2 record: `u8 pinned, u32 formatCount, (u32 fmt, u32 size, payload)*`.
fn clip2_record_v2(c: &mut Cur, blobs: &dyn BlobSource) -> Option<(bool, Acc)> {
    let pinned = c.u8()? != 0;
    let n = c.u32()?;
    let mut acc = Acc::default();
    for _ in 0..n {
        let fmt = c.u32()?;
        let size = c.u32()?;
        let bytes: Cow<[u8]> = if size == CLIP2_BLOB_MARK {
            match blobs.read_all(&c.arr()?) {
                Some(v) => Cow::Owned(v),
                None => continue, // missing blob: drop this format only
            }
        } else {
            Cow::Borrowed(c.take(size as usize)?)
        };
        let key = if fmt < FIRST_REGISTERED {
            (fmt != 0).then_some(FormatKey::Standard(fmt))
        } else {
            sniff(&bytes)
        };
        if let Some(k) = key {
            acc.push(k, Raw::Bytes(&bytes));
        }
    }
    Some((pinned, acc))
}

/// Imports a decrypted clip2 `Inner`. Ids are 1..n, timestamps descend by one second per item
/// starting at `base_unix_ms`; payloads (including hydrated blobs) stay inline.
pub fn clip2_import_inner(inner: &[u8], blobs: &dyn BlobSource, base_unix_ms: i64) -> Vec<Item> {
    let mut c = Cur::new(inner);
    if c.take(4) != Some(b"CLP2".as_slice()) {
        return Vec::new();
    }
    let (Some(version), Some(count)) = (c.u32(), c.u32()) else {
        return Vec::new();
    };
    if version != 1 && version != 2 {
        return Vec::new();
    }
    let mut items: Vec<Item> = Vec::new();
    for _ in 0..count {
        if items.len() >= MAX_ITEMS {
            break;
        }
        let rec = if version == 1 { clip2_record_v1(&mut c) } else { clip2_record_v2(&mut c, blobs) };
        let Some((pinned, acc)) = rec else {
            break;
        };
        let n = items.len() as i64;
        let ms = base_unix_ms.saturating_sub(n * 1000);
        items.extend(build_item(n as u64 + 1, ms, pinned, acc.formats, blobs));
    }
    items
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// In-memory blob store that also records which accessor was used.
    #[derive(Default)]
    struct MemBlobs {
        map: HashMap<[u8; 20], Vec<u8>>,
        log: RefCell<Vec<&'static str>>,
    }
    impl MemBlobs {
        fn with(mut self, sha1: [u8; 20], data: Vec<u8>) -> Self {
            self.map.insert(sha1, data);
            self
        }
    }
    impl BlobSource for MemBlobs {
        fn read_all(&self, sha1: &[u8; 20]) -> Option<Vec<u8>> {
            self.log.borrow_mut().push("all");
            self.map.get(sha1).cloned()
        }
        fn read_head(&self, sha1: &[u8; 20]) -> Option<Vec<u8>> {
            self.log.borrow_mut().push("head");
            self.map.get(sha1).map(|v| v[..v.len().min(128)].to_vec())
        }
    }

    /// Deterministic LCG (no rand crate).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }
    fn std(cf: u32) -> FormatKey {
        FormatKey::Standard(cf)
    }
    fn inl(b: &[u8]) -> Payload {
        Payload::inline(b.to_vec())
    }
    fn dib(w: i32, h: i32, pixels: usize) -> Vec<u8> {
        let mut v = vec![0u8; 40 + pixels];
        v[0..4].copy_from_slice(&40u32.to_le_bytes());
        v[4..8].copy_from_slice(&w.to_le_bytes());
        v[8..12].copy_from_slice(&h.to_le_bytes());
        v[12..14].copy_from_slice(&1u16.to_le_bytes());
        v[14..16].copy_from_slice(&32u16.to_le_bytes());
        v
    }
    fn same_payload(a: &Payload, b: &Payload) -> bool {
        match (a, b) {
            (Payload::Inline(x), Payload::Inline(y)) => x == y,
            (Payload::OnDisk { sha1: s1, len: l1 }, Payload::OnDisk { sha1: s2, len: l2 }) => s1 == s2 && l1 == l2,
            _ => false,
        }
    }
    fn assert_same_items(a: &[Item], b: &[Item]) {
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b) {
            assert_eq!((x.id, x.unix_ms, x.pinned), (y.id, y.unix_ms, y.pinned));
            assert_eq!(x.primary, y.primary);
            assert_eq!(x.kind, y.kind);
            assert_eq!(x.formats.len(), y.formats.len());
            for ((k1, p1), (k2, p2)) in x.formats.iter().zip(&y.formats) {
                assert_eq!(k1, k2);
                assert!(same_payload(p1, p2), "payload of {k1:?}");
            }
        }
    }

    const BLOB_SHA: [u8; 20] = [9; 20];
    const MISSING_SHA: [u8; 20] = [4; 20];

    fn sample_items() -> Vec<Item> {
        vec![
            Item::new(
                11,
                1_700_000_000_123,
                false,
                vec![
                    (std(CF_UNICODETEXT), Payload::inline(utf16("héllo wörld ✓ 😀"))),
                    (std(CF_TEXT), inl(b"hello")),
                    (FormatKey::reg(FMT_HTML), inl(b"Version:0.9\r\nStartHTML:0000000105\r\n<b>x</b>")),
                ],
            ),
            Item::new(
                7,
                -5,
                true,
                vec![
                    (std(CF_UNICODETEXT), Payload::OnDisk { sha1: BLOB_SHA, len: 300_000 }),
                    (FormatKey::reg(FMT_RTF), inl(b"{\\rtf1 hi}")),
                ],
            ),
            Item::new(
                3,
                1_700_000_999_999,
                false,
                vec![
                    (std(CF_DIB), Payload::inline(dib(64, 32, 100))),
                    (FormatKey::reg(FMT_PNG), Payload::OnDisk { sha1: MISSING_SHA, len: 1 << 20 }),
                    (FormatKey::reg("Zürich ünicode"), inl(&[])),
                ],
            ),
        ]
    }

    fn sample_blobs() -> MemBlobs {
        MemBlobs::default().with(BLOB_SHA, vec![b'a'; 300_000])
    }

    // ---- round trip

    #[test]
    fn round_trip_preserves_everything() {
        let items = sample_items();
        let blobs = sample_blobs();
        let back = decode_inner(&encode_inner(&items), &blobs);
        assert_same_items(&items, &back);
        assert!(back[1].pinned && !back[0].pinned);
        assert!(matches!(back[1].formats[0].1, Payload::OnDisk { sha1: BLOB_SHA, len: 300_000 }));
        assert!(matches!(back[2].formats[1].1, Payload::OnDisk { sha1: MISSING_SHA, .. }));
        assert_eq!(back[2].primary, std(CF_DIB));
        assert_eq!(back[0].formats[2].0, FormatKey::reg(FMT_HTML));
    }

    #[test]
    fn stand_ins_use_read_all_for_text_and_read_head_for_dib() {
        let blobs = MemBlobs::default()
            .with([1; 20], vec![0; 10])
            .with([2; 20], vec![0; 10])
            .with([3; 20], vec![0; 10]);
        let item = Item::new(
            1,
            1,
            false,
            vec![
                (std(CF_DIBV5), Payload::OnDisk { sha1: [1; 20], len: 99 }),
                (FormatKey::reg("Other"), Payload::OnDisk { sha1: [2; 20], len: 99 }),
                (FormatKey::reg(FMT_HTML), Payload::OnDisk { sha1: [3; 20], len: 99 }),
            ],
        );
        let back = decode_inner(&encode_inner(&[item]), &blobs);
        assert_eq!(back.len(), 1);
        assert_eq!(*blobs.log.borrow(), vec!["head", "all"]); // "Other": no stand-in, no read
        assert!(back[0].formats.iter().all(|(_, p)| matches!(p, Payload::OnDisk { .. })));
    }

    #[test]
    fn unreadable_blobs_keep_the_item_with_correct_primary() {
        let item = Item::new(5, 5, false, vec![(std(CF_UNICODETEXT), Payload::OnDisk { sha1: MISSING_SHA, len: 400_000 })]);
        let back = decode_inner(&encode_inner(&[item]), &NoBlobs);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].primary, std(CF_UNICODETEXT));
        assert_eq!(back[0].kind, Kind::Text);
        assert!(matches!(back[0].formats[0].1, Payload::OnDisk { len: 400_000, .. }));
    }

    #[test]
    fn empty_and_zero_format_inputs() {
        assert!(decode_inner(&[], &NoBlobs).is_empty());
        assert!(decode_inner(&encode_inner(&[]), &NoBlobs).is_empty());
        let mut empty = Item::new(1, 1, false, vec![(std(CF_TEXT), inl(b"x"))]);
        empty.formats.clear();
        assert!(decode_inner(&encode_inner(&[empty]), &NoBlobs).is_empty());
    }

    #[test]
    fn duplicate_keys_keep_first_and_bad_keys_are_dropped() {
        let mut it = Item::new(1, 1, false, vec![(std(CF_TEXT), inl(b"first"))]);
        it.formats.push((std(CF_TEXT), inl(b"second")));
        it.formats.push((std(0xC123), inl(b"registered id as standard")));
        it.formats.push((FormatKey::Registered(String::new()), inl(b"nameless")));
        it.formats.push((std(CF_HDROP), inl(b"keep")));
        let back = decode_inner(&encode_inner(&[it]), &NoBlobs);
        assert_eq!(back[0].formats.len(), 2);
        assert_eq!(back[0].formats[0].1.bytes(), Some(b"first".as_slice()));
        assert_eq!(back[0].formats[1].0, std(CF_HDROP));
    }

    // ---- caps

    #[test]
    fn per_format_and_per_item_caps() {
        let huge = vec![b'x'; MAX_FORMAT_BYTES + 1];
        let nine = vec![b'y'; 9 * 1024 * 1024];
        let it1 = Item::new(1, 1, false, vec![(std(CF_TEXT), Payload::inline(huge)), (std(CF_UNICODETEXT), inl(b"ok"))]);
        let it2 = Item::new(
            2,
            2,
            false,
            vec![
                (std(CF_TEXT), Payload::inline(nine.clone())),
                (FormatKey::reg(FMT_HTML), Payload::inline(nine)), // would exceed the item cap
                (std(CF_HDROP), inl(b"after the cap")),             // dropped too
            ],
        );
        let it3 = Item::new(3, 3, false, vec![(std(CF_TEXT), Payload::inline(vec![b'z'; MAX_FORMAT_BYTES + 1]))]);
        let back = decode_inner(&encode_inner(&[it1, it2, it3]), &NoBlobs);
        assert_eq!(back.len(), 2, "item with only an oversized format is dropped");
        assert_eq!(back[0].formats.len(), 1);
        assert!(back[0].formats[0].0.is_std(CF_UNICODETEXT));
        assert_eq!(back[1].formats.len(), 1);
        assert!(back[1].formats[0].0.is_std(CF_TEXT));
    }

    #[test]
    fn item_count_cap_keeps_all_pinned() {
        let mk = |id: u64, pinned: bool| Item::new(id, id as i64, pinned, vec![(std(CF_TEXT), inl(b"x"))]);
        let mut items: Vec<Item> = (0..MAX_ITEMS as u64 + 5).map(|i| mk(i + 1, false)).collect();
        items.extend((0..3).map(|i| mk(10_000 + i, true)));
        let back = decode_inner(&encode_inner(&items), &NoBlobs);
        assert_eq!(back.iter().filter(|i| !i.pinned).count(), MAX_ITEMS);
        assert_eq!(back.iter().filter(|i| i.pinned).count(), 3);
    }

    #[test]
    fn huge_counts_do_not_allocate_or_loop_forever() {
        for count in [u32::MAX, 1 << 24] {
            let mut b = count.to_le_bytes().to_vec();
            b.extend([0u8; 40]);
            assert!(decode_inner(&b, &NoBlobs).len() <= 1);
        }
        // one item claiming u32::MAX formats, then nothing
        let mut b = 1u32.to_le_bytes().to_vec();
        b.extend(1u64.to_le_bytes());
        b.extend(0i64.to_le_bytes());
        b.push(0);
        b.extend(u32::MAX.to_le_bytes());
        assert!(decode_inner(&b, &NoBlobs).is_empty());
        // registered name claiming 65535 units with no bytes behind it
        let mut b = 1u32.to_le_bytes().to_vec();
        b.extend([0u8; 17]);
        b.extend(1u32.to_le_bytes());
        b.extend([1, 0xFF, 0xFF]);
        assert!(decode_inner(&b, &NoBlobs).is_empty());
    }

    // ---- outer file

    #[test]
    fn wrap_unwrap_file() {
        let blob = b"opaque dpapi bytes".to_vec();
        let f = wrap_file(&blob);
        assert_eq!(&f[..4], b"CLP4");
        assert_eq!(unwrap_file(&f), Some(blob.as_slice()));
        assert_eq!(unwrap_file(&wrap_file(&[])), Some([].as_slice()));
        // every truncation fails; bad magic / version fail; trailing bytes are tolerated
        for n in 0..f.len() {
            assert!(unwrap_file(&f[..n]).is_none(), "truncated at {n}");
        }
        let mut bad = f.clone();
        bad[0] = b'X';
        assert!(unwrap_file(&bad).is_none());
        let mut bad = f.clone();
        bad[4] = 2;
        assert!(unwrap_file(&bad).is_none());
        let mut lying = f.clone();
        lying[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(unwrap_file(&lying).is_none());
        let mut trailing = f.clone();
        trailing.push(0);
        assert_eq!(unwrap_file(&trailing), Some(blob.as_slice()));
    }

    // ---- fuzz

    #[test]
    fn fuzz_truncation_at_every_offset() {
        let buf = encode_inner(&sample_items());
        let blobs = MemBlobs::default().with(BLOB_SHA, vec![b'a'; 64]);
        let full = decode_inner(&buf, &blobs).len();
        for n in 0..=buf.len() {
            assert!(decode_inner(&buf[..n], &blobs).len() <= full);
        }
        assert_eq!(decode_inner(&buf, &blobs).len(), full);
    }

    #[test]
    fn fuzz_random_byte_flips_and_garbage() {
        let items: Vec<Item> = sample_items().into_iter().chain(sample_items()).collect();
        let buf = encode_inner(&items);
        let blobs = MemBlobs::default().with(BLOB_SHA, vec![b'a'; 64]);
        let mut rng = Lcg(0xC11B_0004);
        for _ in 0..5000 {
            let mut b = buf.clone();
            for _ in 0..1 + rng.below(4) {
                let i = rng.below(b.len());
                b[i] = rng.next() as u8;
            }
            if rng.below(4) == 0 {
                b.truncate(rng.below(b.len()));
            }
            let _ = decode_inner(&b, &blobs);
        }
        for _ in 0..2000 {
            let n = rng.below(300);
            let junk: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
            let _ = decode_inner(&junk, &blobs);
            let _ = clip2_import_inner(&junk, &blobs, 0);
            let _ = clip2_classify(&junk);
            let _ = unwrap_file(&junk);
        }
    }

    // ---- clip2 fixtures

    enum Part {
        Bytes(u32, Vec<u8>),
        Blob(u32, [u8; 20]),
    }
    fn v2_record(pinned: bool, parts: &[Part]) -> Vec<u8> {
        let mut o = vec![u8::from(pinned)];
        o.extend((parts.len() as u32).to_le_bytes());
        for p in parts {
            match p {
                Part::Bytes(fmt, b) => {
                    o.extend(fmt.to_le_bytes());
                    o.extend((b.len() as u32).to_le_bytes());
                    o.extend_from_slice(b);
                }
                Part::Blob(fmt, sha) => {
                    o.extend(fmt.to_le_bytes());
                    o.extend(CLIP2_BLOB_MARK.to_le_bytes());
                    o.extend_from_slice(sha);
                }
            }
        }
        o
    }
    fn clip2_inner(version: u32, records: &[Vec<u8>]) -> Vec<u8> {
        let mut o = b"CLP2".to_vec();
        o.extend(version.to_le_bytes());
        o.extend((records.len() as u32).to_le_bytes());
        records.iter().for_each(|r| o.extend_from_slice(r));
        o
    }
    fn v1_record(fmt: u32, bytes: &[u8]) -> Vec<u8> {
        let mut o = fmt.to_le_bytes().to_vec();
        o.extend((bytes.len() as u32).to_le_bytes());
        o.extend_from_slice(bytes);
        o
    }

    const HTML: &[u8] = b"Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000200\r\n<html>hi</html>";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n....IHDR";

    #[test]
    fn clip2_classify_variants() {
        let blob = b"dpapi";
        let mut f3 = b"CLP3".to_vec();
        f3.extend((blob.len() as u32).to_le_bytes());
        f3.extend_from_slice(blob);
        assert!(matches!(clip2_classify(&f3), Clip2File::Dpapi(b) if b == blob));
        assert!(matches!(clip2_classify(&f3[..f3.len() - 1]), Clip2File::Invalid));
        assert!(matches!(clip2_classify(b"CLP3"), Clip2File::Invalid));
        let plain = clip2_inner(1, &[]);
        assert!(matches!(clip2_classify(&plain), Clip2File::Plain(b) if b == plain.as_slice()));
        assert!(matches!(clip2_classify(b""), Clip2File::Invalid));
        assert!(matches!(clip2_classify(b"CLP4xxxx"), Clip2File::Invalid));
    }

    #[test]
    fn clip2_v1_text_only_with_descending_timestamps() {
        let inner = clip2_inner(
            1,
            &[
                v1_record(CF_UNICODETEXT, &utf16("newest")),
                v1_record(CF_TEXT, b"middle"),
                v1_record(0xC0A1, HTML), // v1 is text only: not CF_TEXT/UNICODE -> record dropped
                v1_record(CF_UNICODETEXT, &utf16("oldest")),
            ],
        );
        let items = clip2_import_inner(&inner, &NoBlobs, 1_000_000);
        assert_eq!(items.len(), 3);
        assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(items.iter().map(|i| i.unix_ms).collect::<Vec<_>>(), [1_000_000, 999_000, 998_000]);
        assert!(items.iter().all(|i| !i.pinned && i.formats.len() == 1));
        assert!(items[1].formats[0].0.is_std(CF_TEXT));
        assert_eq!(items[0].formats[0].1.bytes(), Some(utf16("newest").as_slice()));
    }

    #[test]
    fn clip2_v2_blob_markers_sniffing_and_drops() {
        let blobs = MemBlobs::default()
            .with([1; 20], PNG.to_vec())
            .with([2; 20], utf16("from the blob dir"));
        let inner = clip2_inner(
            2,
            &[
                v2_record(
                    true,
                    &[
                        Part::Bytes(CF_UNICODETEXT, utf16("pinned text")),
                        Part::Bytes(0xC0A1, HTML.to_vec()),
                        Part::Blob(0xC0A2, [1; 20]),                // PNG by sniffing, hydrated inline
                        Part::Blob(0xC0A3, [77; 20]),               // missing blob: format dropped
                        Part::Bytes(0xC0A4, b"mystery bytes".to_vec()), // unknown: dropped
                        Part::Bytes(0xC0A5, HTML.to_vec()),         // duplicate "HTML Format": first wins
                    ],
                ),
                v2_record(false, &[Part::Bytes(0xC0FF, b"only unknown".to_vec())]), // no formats left: item dropped
                v2_record(false, &[Part::Blob(CF_UNICODETEXT, [2; 20]), Part::Bytes(0xC001, b"{\\rtf1 x}".to_vec())]),
                v2_record(false, &[Part::Bytes(CF_HDROP, b"\x14\0\0\0".to_vec()), Part::Bytes(0xC002, Vec::new())]),
            ],
        );
        let items = clip2_import_inner(&inner, &blobs, 5_000);
        assert_eq!(items.len(), 3);
        assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(items.iter().map(|i| i.unix_ms).collect::<Vec<_>>(), [5_000, 4_000, 3_000]);

        let a = &items[0];
        assert!(a.pinned);
        let keys: Vec<_> = a.formats.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, [std(CF_UNICODETEXT), FormatKey::reg(FMT_HTML), FormatKey::reg(FMT_PNG)]);
        assert_eq!(a.payload_named(FMT_PNG).and_then(Payload::bytes), Some(PNG));
        assert!(a.formats.iter().all(|(_, p)| p.bytes().is_some()), "all hydrated inline");

        let b = &items[1];
        assert!(!b.pinned);
        assert_eq!(b.formats[0].1.bytes(), Some(utf16("from the blob dir").as_slice()));
        assert!(b.formats[1].0.is_named(FMT_RTF));

        assert_eq!(items[2].formats.len(), 1);
        assert!(items[2].formats[0].0.is_std(CF_HDROP));
    }

    #[test]
    fn clip2_sniff_rules() {
        assert_eq!(sniff(HTML), Some(FormatKey::reg(FMT_HTML)));
        assert_eq!(sniff(b"Version:0.9 but no header fields"), None);
        assert_eq!(sniff(b"x Version:0.9 StartHTML:1"), None);
        assert_eq!(sniff(b"{\\rtf1\\ansi}"), Some(FormatKey::reg(FMT_RTF)));
        assert_eq!(sniff(PNG), Some(FormatKey::reg(FMT_PNG)));
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn clip2_rejects_bad_headers_and_caps_item_count() {
        assert!(clip2_import_inner(b"", &NoBlobs, 0).is_empty());
        assert!(clip2_import_inner(b"CLP4\x01\0\0\0\0\0\0\0", &NoBlobs, 0).is_empty());
        assert!(clip2_import_inner(&clip2_inner(3, &[v1_record(CF_TEXT, b"x")]), &NoBlobs, 0).is_empty());
        let recs: Vec<_> = (0..MAX_ITEMS + 10).map(|_| v1_record(CF_TEXT, b"x")).collect();
        assert_eq!(clip2_import_inner(&clip2_inner(1, &recs), &NoBlobs, 0).len(), MAX_ITEMS);
        // count lies about the number of records: keep what exists
        let mut inner = clip2_inner(1, &[v1_record(CF_TEXT, b"x")]);
        inner[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(clip2_import_inner(&inner, &NoBlobs, 0).len(), 1);
    }

    #[test]
    fn fuzz_clip2_import() {
        let blobs = MemBlobs::default().with([1; 20], PNG.to_vec());
        let v2 = clip2_inner(
            2,
            &[
                v2_record(true, &[Part::Bytes(CF_UNICODETEXT, utf16("abc")), Part::Blob(0xC0A2, [1; 20])]),
                v2_record(false, &[Part::Bytes(0xC0A1, HTML.to_vec()), Part::Bytes(CF_TEXT, b"abc".to_vec())]),
            ],
        );
        let v1 = clip2_inner(1, &[v1_record(CF_TEXT, b"abc"), v1_record(CF_UNICODETEXT, &utf16("def"))]);
        for buf in [&v1, &v2] {
            for n in 0..=buf.len() {
                let _ = clip2_import_inner(&buf[..n], &blobs, 0);
            }
            let mut rng = Lcg(42);
            for _ in 0..3000 {
                let mut b = buf.clone();
                for _ in 0..1 + rng.below(3) {
                    let i = rng.below(b.len());
                    b[i] = rng.next() as u8;
                }
                let _ = clip2_import_inner(&b, &blobs, i64::MIN);
            }
        }
    }
}
