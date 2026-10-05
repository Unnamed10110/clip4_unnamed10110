//! In-memory history store (spec 6.7 de-duplication, 7.3 limits). Owned by the UI thread;
//! items are kept newest first.

use crate::model::*;
use std::collections::HashSet;

const MIN_MAX_ITEMS: usize = 10;
const MAX_MAX_ITEMS: usize = 2000;
/// Images arriving this close to the top item may be "near-identical" re-captures.
const NEAR_IMAGE_MS: u64 = 750;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddOutcome {
    /// Inserted at the top with this fresh id.
    Added(u64),
    /// Same as the current top item; the incoming item was dropped.
    Duplicate,
    /// Same as an older item, which was moved to the top (id of that item).
    MovedToTop(u64),
}

pub struct Store {
    items: Vec<Item>,
    next_id: u64,
    max_items: usize,
}

impl Store {
    pub fn new(max_items: usize) -> Store {
        Store { items: Vec::new(), next_id: 1, max_items: max_items.clamp(MIN_MAX_ITEMS, MAX_MAX_ITEMS) }
    }

    pub fn set_max_items(&mut self, n: usize) {
        self.max_items = n.clamp(MIN_MAX_ITEMS, MAX_MAX_ITEMS);
        self.evict();
    }
    pub fn max_items(&self) -> usize {
        self.max_items
    }
    pub fn items(&self) -> &[Item] {
        &self.items
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Monotonic; ids are never reused, not even after remove/clear.
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = id.saturating_add(1);
        id
    }

    /// Replaces the content (`items` newest first) and enforces the limits.
    pub fn load(&mut self, items: Vec<Item>) {
        let max_id = items.iter().map(|i| i.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(max_id.saturating_add(1));
        self.items = items;
        self.evict();
    }

    /// Cheap clone for the save worker (payloads and indexes are `Arc`s).
    pub fn snapshot(&self) -> Vec<Item> {
        self.items.clone()
    }

    pub fn add(&mut self, mut item: Item) -> AddOutcome {
        if let Some(top) = self.items.first() {
            if same_exact(top, &item) || near_identical_image(top, &item) {
                return AddOutcome::Duplicate;
            }
        }
        if let Some(pos) = self.items.iter().position(|old| same_exact(old, &item)) {
            let mut old = self.items.remove(pos);
            old.unix_ms = item.unix_ms;
            let id = old.id;
            self.items.insert(0, old);
            return AddOutcome::MovedToTop(id);
        }
        item.id = self.alloc_id();
        let id = item.id;
        self.items.insert(0, item);
        self.evict();
        AddOutcome::Added(id)
    }

    pub fn find(&self, id: u64) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }

    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.items.iter().position(|i| i.id == id)
    }

    /// Removes the given ids; returns how many items were removed.
    pub fn remove(&mut self, ids: &[u64]) -> usize {
        let before = self.items.len();
        self.items.retain(|i| !ids.contains(&i.id));
        before - self.items.len()
    }

    /// Returns false when `id` is unknown. Unpinning can push the unpinned count over the
    /// limit, in which case the oldest unpinned item (possibly this one) is evicted at once.
    pub fn set_pinned(&mut self, id: u64, pinned: bool) -> bool {
        let Some(item) = self.items.iter_mut().find(|i| i.id == id) else {
            return false;
        };
        item.pinned = pinned;
        self.evict();
        true
    }

    pub fn clear_unpinned(&mut self) {
        self.items.retain(|i| i.pinned);
    }

    /// Ids stay reserved: `next_id` is not reset.
    pub fn clear_all(&mut self) {
        self.items.clear();
    }

    /// In-place transform: `f` builds the replacement; position, id, pinned state and
    /// timestamp of the original are kept. Returns false when `id` is unknown.
    pub fn replace_item(&mut self, id: u64, f: impl FnOnce(&Item) -> Item) -> bool {
        let Some(slot) = self.items.iter_mut().find(|i| i.id == id) else {
            return false;
        };
        let mut new = f(slot);
        new.id = slot.id;
        new.pinned = slot.pinned;
        new.unix_ms = slot.unix_ms;
        *slot = new;
        true
    }

    pub fn pinned_count(&self) -> usize {
        self.items.iter().filter(|i| i.pinned).count()
    }

    /// Every blob referenced by the history (blobs outside this set may be deleted).
    pub fn blob_refs(&self) -> HashSet<[u8; 20]> {
        self.items.iter().flat_map(|i| i.blob_refs().copied()).collect()
    }

    /// Keeps the newest `max_items` unpinned items; pinned ones never count and never go.
    fn evict(&mut self) {
        let mut room = self.max_items;
        self.items.retain(|i| {
            i.pinned || {
                let keep = room > 0;
                room = room.saturating_sub(1);
                keep
            }
        });
    }
}

// ---------------------------------------------------------------- duplicate rules (6.7)

fn payload_eq(a: &Payload, b: &Payload) -> bool {
    match (a, b) {
        (Payload::Inline(x), Payload::Inline(y)) => x == y,
        (Payload::OnDisk { sha1: s1, len: l1 }, Payload::OnDisk { sha1: s2, len: l2 }) => s1 == s2 && l1 == l2,
        _ => false,
    }
}

/// Rule (a): same keys, same payloads (order-insensitive).
fn same_formats(a: &Item, b: &Item) -> bool {
    a.formats.len() == b.formats.len()
        && a.formats.iter().all(|(k, p)| b.payload(k).is_some_and(|q| payload_eq(p, q)))
}

/// Full text of an item: the model's `text()`, else a minimal CF_UNICODETEXT/CF_TEXT decode.
fn text_of(item: &Item) -> Option<String> {
    item.text().or_else(|| {
        if let Some(b) = item.payload_std(CF_UNICODETEXT).and_then(Payload::bytes) {
            let units: Vec<u16> = b
                .chunks_exact(2)
                .map(|p| u16::from_le_bytes(p.try_into().unwrap_or_default()))
                .take_while(|&u| u != 0)
                .collect();
            return Some(String::from_utf16_lossy(&units));
        }
        let b = item.payload_std(CF_TEXT).and_then(Payload::bytes)?;
        Some(String::from_utf8_lossy(b.split(|&x| x == 0).next().unwrap_or_default()).into_owned())
    })
}

fn normalised_text(item: &Item) -> Option<String> {
    let t = text_of(item)?.replace("\r\n", "\n");
    let t = t.split('\0').next().unwrap_or_default().trim();
    // Empty text is not evidence of sameness (an image may carry an empty CF_TEXT).
    (!t.is_empty()).then(|| t.to_string())
}

/// Rule (b): both have text and the normalised text is identical.
fn same_text(a: &Item, b: &Item) -> bool {
    a.has_text() && b.has_text() && normalised_text(a).is_some_and(|t| Some(t) == normalised_text(b))
}

/// Rule (c), exact part: both images with identical primary-format bytes / sha1.
fn same_image(a: &Item, b: &Item) -> bool {
    a.kind == Kind::Image
        && b.kind == Kind::Image
        && a.primary == b.primary
        && matches!((a.payload(&a.primary), b.payload(&b.primary)), (Some(x), Some(y)) if payload_eq(x, y))
}

/// Rule (c), loose part: same primary key and length, arriving within 750 ms of `top`.
fn near_identical_image(top: &Item, new: &Item) -> bool {
    top.kind == Kind::Image
        && new.kind == Kind::Image
        && top.primary == new.primary
        && top.unix_ms.abs_diff(new.unix_ms) <= NEAR_IMAGE_MS
        && matches!((top.payload(&top.primary), new.payload(&new.primary)), (Some(x), Some(y)) if x.len() == y.len())
}

fn same_exact(a: &Item, b: &Item) -> bool {
    same_formats(a, b) || same_text(a, b) || same_image(a, b)
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }
    fn text(s: &str, ms: i64) -> Item {
        Item::new(0, ms, false, vec![(FormatKey::Standard(CF_UNICODETEXT), Payload::inline(utf16(s)))])
    }
    fn image(bytes: Vec<u8>, ms: i64) -> Item {
        Item::new(0, ms, false, vec![(FormatKey::Standard(CF_DIB), Payload::inline(bytes))])
    }
    fn with_id(mut it: Item, id: u64) -> Item {
        it.id = id;
        it
    }
    fn texts(s: &Store) -> Vec<String> {
        s.items().iter().map(|i| text_of(i).unwrap_or_default()).collect()
    }
    fn id_of(o: AddOutcome) -> u64 {
        match o {
            AddOutcome::Added(id) | AddOutcome::MovedToTop(id) => id,
            AddOutcome::Duplicate => 0,
        }
    }

    #[test]
    fn add_assigns_monotonic_ids_newest_first() {
        let mut s = Store::new(300);
        assert!(s.is_empty());
        assert_eq!(s.add(with_id(text("a", 1), 999)), AddOutcome::Added(1));
        assert_eq!(s.add(text("b", 2)), AddOutcome::Added(2));
        assert_eq!(s.add(text("c", 3)), AddOutcome::Added(3));
        assert_eq!(texts(&s), ["c", "b", "a"]);
        assert_eq!(s.len(), 3);
        assert_eq!(s.find(2).map(|i| i.unix_ms), Some(2));
        assert_eq!(s.index_of(1), Some(2));
        assert_eq!(s.index_of(42), None);
        assert!(s.find(42).is_none());
    }

    #[test]
    fn exact_duplicate_of_top_is_dropped_without_touching_timestamp() {
        let mut s = Store::new(300);
        s.add(text("a", 100));
        s.add(text("b", 200));
        assert_eq!(s.add(text("b", 9_999)), AddOutcome::Duplicate);
        assert_eq!(s.len(), 2);
        assert_eq!(s.items()[0].unix_ms, 200);
    }

    #[test]
    fn text_duplicate_is_normalised_but_formats_may_differ() {
        let mut s = Store::new(300);
        s.add(text("line1\nline2", 1));
        assert_eq!(s.add(text("  line1\r\nline2 \r\n", 2)), AddOutcome::Duplicate);
        // same text with an extra format is still a duplicate by rule (b)
        let rich = Item::new(
            0,
            3,
            false,
            vec![
                (FormatKey::Standard(CF_UNICODETEXT), Payload::inline(utf16("line1\nline2"))),
                (FormatKey::reg(FMT_HTML), Payload::inline(b"<b>x</b>".to_vec())),
            ],
        );
        assert_eq!(s.add(rich), AddOutcome::Duplicate);
        assert_eq!(s.add(text("line1\nline3", 4)), AddOutcome::Added(2));
        // interior whitespace is significant
        assert!(matches!(s.add(text("line1  line3", 5)), AddOutcome::Added(_)));
    }

    #[test]
    fn text_in_cf_text_and_nul_termination() {
        let a = Item::new(0, 1, false, vec![(FormatKey::Standard(CF_TEXT), Payload::inline(b"hello\0garbage".to_vec()))]);
        let b = Item::new(0, 2, false, vec![(FormatKey::Standard(CF_TEXT), Payload::inline(b"hello\r\n".to_vec()))]);
        let mut s = Store::new(300);
        s.add(a);
        assert_eq!(s.add(b), AddOutcome::Duplicate);
        let c = Item::new(0, 3, false, vec![(FormatKey::Standard(CF_UNICODETEXT), Payload::inline(utf16("hello")))]);
        assert_eq!(s.add(c), AddOutcome::Duplicate, "CF_TEXT vs CF_UNICODETEXT with the same text");
    }

    #[test]
    fn empty_text_is_not_a_duplicate_signal() {
        let mut s = Store::new(300);
        let a = Item::new(
            0,
            1,
            false,
            vec![(FormatKey::Standard(CF_TEXT), Payload::inline(b"".to_vec())), (FormatKey::Standard(CF_DIB), Payload::inline(vec![1; 50]))],
        );
        let b = Item::new(
            0,
            5_000,
            false,
            vec![(FormatKey::Standard(CF_TEXT), Payload::inline(b"  ".to_vec())), (FormatKey::Standard(CF_DIB), Payload::inline(vec![2; 70]))],
        );
        s.add(a);
        assert!(matches!(s.add(b), AddOutcome::Added(_)));
    }

    #[test]
    fn older_duplicate_moves_to_top_keeping_id_and_pin() {
        let mut s = Store::new(300);
        s.add(text("a", 100));
        s.add(text("b", 200));
        s.add(text("c", 300));
        assert!(s.set_pinned(1, true)); // "a" is pinned and oldest
        let out = s.add(text("a", 5_000));
        assert_eq!(out, AddOutcome::MovedToTop(1));
        assert_eq!(texts(&s), ["a", "c", "b"]);
        assert_eq!(s.len(), 3);
        assert_eq!(s.items()[0].unix_ms, 5_000);
        assert!(s.items()[0].pinned);
        assert_eq!(id_of(s.add(text("d", 6_000))), 4, "no id was consumed by the move");
        // text-equal (not byte-equal) older duplicate also moves
        assert_eq!(s.add(text("  b\r\n", 7_000)), AddOutcome::MovedToTop(2));
        assert_eq!(texts(&s)[0], "b");
    }

    #[test]
    fn image_duplicates() {
        let mut s = Store::new(300);
        let base = vec![7u8; 400];
        s.add(image(base.clone(), 10_000));
        // identical bytes: duplicate no matter how far apart
        assert_eq!(s.add(image(base.clone(), 99_000)), AddOutcome::Duplicate);
        // near-identical: same primary + length, different bytes, within 750 ms
        let mut tweak = base.clone();
        tweak[100] ^= 1;
        assert_eq!(s.add(image(tweak.clone(), 10_700)), AddOutcome::Duplicate);
        assert_eq!(s.add(image(tweak.clone(), 9_300)), AddOutcome::Duplicate);
        assert_eq!(s.len(), 1);
        assert_eq!(s.items()[0].unix_ms, 10_000);
        // 751 ms later: a new image
        assert!(matches!(s.add(image(tweak.clone(), 10_751)), AddOutcome::Added(_)));
        // within 750 ms but different length: new image
        assert!(matches!(s.add(image(vec![7u8; 401], 10_800)), AddOutcome::Added(_)));
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn near_identical_rule_does_not_apply_to_older_items() {
        let mut s = Store::new(300);
        let base = vec![7u8; 400];
        let mut tweak = base.clone();
        tweak[0] = 8;
        s.add(image(base.clone(), 10_000));
        s.add(text("between", 10_100));
        assert!(matches!(s.add(image(tweak, 10_200)), AddOutcome::Added(_)));
        // exact image equality with an older item does move it
        assert_eq!(s.add(image(base, 20_000)), AddOutcome::MovedToTop(1));
        assert_eq!(s.items()[0].unix_ms, 20_000);
    }

    #[test]
    fn image_blob_refs_compare_by_sha1() {
        let blob = |sha: u8, ms: i64| Item::new(0, ms, false, vec![(FormatKey::Standard(CF_DIB), Payload::OnDisk { sha1: [sha; 20], len: 1 << 20 })]);
        let mut s = Store::new(300);
        s.add(blob(1, 0));
        assert_eq!(s.add(blob(1, 10_000)), AddOutcome::Duplicate);
        assert!(matches!(s.add(blob(2, 20_000)), AddOutcome::Added(_)));
        assert_eq!(s.add(blob(1, 30_000)), AddOutcome::MovedToTop(1));
        // different sha1 but same primary + length within 750 ms: near-identical
        assert_eq!(s.add(blob(3, 30_500)), AddOutcome::Duplicate);
    }

    #[test]
    fn different_format_sets_are_not_duplicates() {
        let mut s = Store::new(300);
        let f = |k: FormatKey, b: &[u8]| Item::new(0, 1, false, vec![(k, Payload::inline(b.to_vec()))]);
        s.add(f(FormatKey::reg("Foo"), b"x"));
        assert!(matches!(s.add(f(FormatKey::reg("Foo"), b"y")), AddOutcome::Added(_)));
        assert!(matches!(s.add(f(FormatKey::reg("Bar"), b"y")), AddOutcome::Added(_)));
        assert_eq!(s.add(f(FormatKey::reg("Bar"), b"y")), AddOutcome::Duplicate);
        assert_eq!(s.add(f(FormatKey::reg("Foo"), b"x")), AddOutcome::MovedToTop(1));
    }

    #[test]
    fn eviction_drops_oldest_unpinned_only() {
        let mut s = Store::new(10);
        assert_eq!(s.max_items(), 10);
        for i in 0..10 {
            s.add(text(&format!("t{i}"), i));
        }
        assert_eq!(s.len(), 10);
        s.add(text("t10", 10));
        assert_eq!(s.len(), 10);
        assert!(s.find(1).is_none(), "oldest evicted");
        assert!(s.find(2).is_some());

        // pinned items neither count nor get evicted
        s.set_pinned(2, true);
        s.set_pinned(3, true);
        assert_eq!(s.pinned_count(), 2);
        for i in 11..20 {
            s.add(text(&format!("t{i}"), i));
        }
        assert_eq!(s.len(), 12, "10 unpinned + 2 pinned");
        assert!(s.find(2).is_some() && s.find(3).is_some());
        assert_eq!(s.items().iter().filter(|i| !i.pinned).count(), 10);
        assert_eq!(texts(&s)[0], "t19");
    }

    #[test]
    fn unpinning_over_the_limit_evicts_oldest_unpinned() {
        let mut s = Store::new(10);
        for i in 0..10 {
            s.add(text(&format!("t{i}"), i));
        }
        s.set_pinned(1, true); // oldest, now exempt
        s.add(text("new", 100)); // 10 unpinned + 1 pinned
        assert_eq!(s.len(), 11);
        assert!(s.set_pinned(1, false)); // 11 unpinned -> the oldest (this one) goes
        assert_eq!(s.len(), 10);
        assert!(s.find(1).is_none());
        assert!(!s.set_pinned(1, true), "unknown id");
    }

    #[test]
    fn set_max_items_clamps_and_evicts_immediately() {
        let mut s = Store::new(1);
        assert_eq!(s.max_items(), 10);
        assert_eq!(Store::new(5_000).max_items(), 2000);
        let mut s2 = Store::new(2000);
        for i in 0..30 {
            s2.add(text(&format!("t{i}"), i));
        }
        s2.set_pinned(1, true);
        s2.set_max_items(12);
        assert_eq!(s2.max_items(), 12);
        assert_eq!(s2.len(), 13);
        assert!(s2.find(1).is_some());
        assert!(s2.find(30).is_some() && s2.find(19).is_some() && s2.find(18).is_none());
        s2.set_max_items(0);
        assert_eq!(s2.max_items(), 10);
        assert_eq!(s2.len(), 11);
        s.set_max_items(usize::MAX);
        assert_eq!(s.max_items(), 2000);
    }

    #[test]
    fn ids_are_never_reused() {
        let mut s = Store::new(300);
        assert_eq!(s.alloc_id(), 1);
        assert_eq!(s.alloc_id(), 2);
        assert_eq!(s.add(text("a", 1)), AddOutcome::Added(3));
        assert_eq!(s.remove(&[3]), 1);
        assert_eq!(s.add(text("b", 2)), AddOutcome::Added(4));
        s.clear_all();
        assert_eq!(s.add(text("c", 3)), AddOutcome::Added(5));
        s.clear_unpinned();
        assert_eq!(s.add(text("d", 4)), AddOutcome::Added(6));
        // duplicates and moves do not burn ids
        assert_eq!(s.add(text("d", 5)), AddOutcome::Duplicate);
        assert_eq!(s.alloc_id(), 7);
    }

    #[test]
    fn remove_pin_and_clear() {
        let mut s = Store::new(300);
        for t in ["a", "b", "c", "d", "e"] {
            s.add(text(t, 1));
        }
        assert_eq!(s.remove(&[2, 4, 99]), 2);
        assert_eq!(texts(&s), ["e", "c", "a"]);
        assert_eq!(s.remove(&[]), 0);
        assert!(s.set_pinned(3, true));
        assert_eq!(s.pinned_count(), 1);
        s.clear_unpinned();
        assert_eq!(texts(&s), ["c"]);
        assert!(s.items()[0].pinned);
        s.clear_all();
        assert!(s.is_empty());
        assert_eq!(s.pinned_count(), 0);
    }

    #[test]
    fn replace_item_keeps_position_id_pin_and_timestamp() {
        let mut s = Store::new(300);
        s.add(text("a", 100));
        s.add(text("b", 200));
        s.add(text("c", 300));
        s.set_pinned(2, true);
        let ok = s.replace_item(2, |old| {
            assert_eq!(text_of(old).as_deref(), Some("b"));
            let mut n = text("B!", 123_456);
            n.pinned = false;
            n.id = 77;
            n
        });
        assert!(ok);
        assert_eq!(texts(&s), ["c", "B!", "a"]);
        let it = &s.items()[1];
        assert_eq!((it.id, it.pinned, it.unix_ms), (2, true, 200));
        assert!(!s.replace_item(99, |o| o.clone()));
    }

    #[test]
    fn blob_refs_collects_all_on_disk_payloads() {
        let mut s = Store::new(300);
        assert!(s.blob_refs().is_empty());
        let big = |sha: u8, extra: u8| {
            Item::new(
                0,
                1,
                false,
                vec![
                    (FormatKey::Standard(CF_UNICODETEXT), Payload::OnDisk { sha1: [sha; 20], len: 1 << 20 }),
                    (FormatKey::reg("X"), Payload::OnDisk { sha1: [extra; 20], len: 1 << 20 }),
                    (FormatKey::reg("Y"), Payload::inline(vec![1])),
                ],
            )
        };
        s.add(big(1, 2));
        s.add(big(3, 2));
        s.add(text("inline only", 5));
        let refs = s.blob_refs();
        assert_eq!(refs, HashSet::from([[1u8; 20], [2; 20], [3; 20]]));
    }

    #[test]
    fn load_sets_next_id_clamps_and_snapshot_matches() {
        let mut s = Store::new(10);
        let items: Vec<Item> = (0..25u64).rev().map(|i| with_id(text(&format!("t{i}"), i as i64), i * 2 + 5)).collect();
        // newest first: ids 53, 51, ... ; pin the 2nd-oldest and the oldest
        let mut items = items;
        let n = items.len();
        items[n - 1].pinned = true;
        items[n - 2].pinned = true;
        s.load(items);
        assert_eq!(s.len(), 12, "10 unpinned + 2 pinned");
        assert_eq!(s.pinned_count(), 2);
        assert_eq!(s.items()[0].id, 53);
        assert_eq!(s.alloc_id(), 54, "max id (53, even if evicted) + 1");
        let snap = s.snapshot();
        assert_eq!(snap.len(), s.len());
        assert!(snap.iter().zip(s.items()).all(|(a, b)| a.id == b.id));
        // loading replaces; next_id never goes backwards
        s.load(vec![with_id(text("x", 1), 3)]);
        assert_eq!(s.len(), 1);
        assert_eq!(s.alloc_id(), 55);
        s.load(Vec::new());
        assert!(s.is_empty());
    }
}
