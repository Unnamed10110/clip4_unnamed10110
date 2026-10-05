//! The one layout function behind paint, hit-testing, paging and "ensure selection visible"
//! (spec 10.9, lessons 18.17-18.19). Pure logic.
//!
//! Every item is a plain row of `metrics.row_h` except, optionally, the selected item, which is
//! an expanded card of `metrics.card_height(lines)`. The card is shown whole or not at all: if it
//! does not fit the viewport the item stays a normal selected row. Because at most one band
//! differs from the rest, every query is O(1) (plus the visible bands), not O(items).

use crate::theme::Metrics;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    /// Viewport-relative y; negative for a partly scrolled-off band.
    pub top: f32,
    pub height: f32,
    /// Index into the pane's visible list.
    pub item: usize,
    pub expanded: bool,
    /// Clamped 1..=4 for an expanded band; 0 for a normal row (not queried).
    pub card_lines: u32,
}

pub struct LayoutInput<'a> {
    pub metrics: &'a Metrics,
    pub count: usize,
    pub selected: Option<usize>,
    /// Card feature on AND this pane supports it.
    pub expand: bool,
    /// Body lines of the item's card (clamped to 1..=4 by the layout); only asked for the selected item.
    pub card_lines: &'a dyn Fn(usize) -> u32,
}

/// Resolved geometry for one (input, viewport height) pair.
struct Geo {
    count: usize,
    row_h: f32,
    card_h: f32,
    /// The expanded item and its clamped line count.
    card: Option<(usize, u32)>,
}

impl Geo {
    fn new(inp: &LayoutInput, viewport_h: f32) -> Geo {
        let m = inp.metrics;
        let card = inp
            .selected
            .filter(|&i| inp.expand && i < inp.count)
            .map(|i| (i, (inp.card_lines)(i).clamp(1, 4)))
            .filter(|&(_, lines)| m.card_height(lines) <= viewport_h);
        Geo {
            count: inp.count,
            row_h: m.row_h,
            card_h: card.map_or(m.row_h, |(_, l)| m.card_height(l)),
            card,
        }
    }

    /// Extra height of the card over a normal row (0 without a card).
    fn extra(&self) -> f32 {
        self.card_h - self.row_h
    }

    fn total(&self) -> f32 {
        self.count as f32 * self.row_h + self.extra()
    }

    /// Content-space top of item `i`.
    fn top(&self, i: usize) -> f32 {
        let below_card = self.card.is_some_and(|(e, _)| e < i);
        i as f32 * self.row_h + if below_card { self.extra() } else { 0.0 }
    }

    fn height(&self, i: usize) -> f32 {
        if self.card.is_some_and(|(e, _)| e == i) {
            self.card_h
        } else {
            self.row_h
        }
    }

    fn max_scroll(&self, viewport_h: f32) -> f32 {
        (self.total() - viewport_h).max(0.0)
    }

    /// First item whose band ends below `scroll`.
    fn first_visible(&self, scroll: f32) -> usize {
        let guess = match self.card {
            Some((e, _)) => {
                let card_top = e as f32 * self.row_h;
                if scroll < card_top {
                    scroll / self.row_h
                } else if scroll < card_top + self.card_h {
                    e as f32
                } else {
                    e as f32 + 1.0 + (scroll - card_top - self.card_h) / self.row_h
                }
            }
            None => scroll / self.row_h,
        };
        // Float guess, then fix up against the exact geometry (saturating cast: NaN/negative -> 0).
        let mut i = (guess as usize).min(self.count);
        while i > 0 && self.top(i) > scroll {
            i -= 1;
        }
        while i < self.count && self.top(i) + self.height(i) <= scroll {
            i += 1;
        }
        i
    }
}

/// Total height of all bands, assuming the selected card (if `expand`) is shown. For the exact
/// value at a given viewport (the card may not fit) use [`content_height_in`].
pub fn content_height(inp: &LayoutInput) -> f32 {
    content_height_in(inp, f32::INFINITY)
}

/// Total height of all bands when laid out for a viewport of `viewport_h`.
pub fn content_height_in(inp: &LayoutInput, viewport_h: f32) -> f32 {
    Geo::new(inp, viewport_h.max(0.0)).total()
}

/// `max(0, content height at this viewport - viewport_h)`.
pub fn max_scroll(inp: &LayoutInput, viewport_h: f32) -> f32 {
    let vh = viewport_h.max(0.0);
    Geo::new(inp, vh).max_scroll(vh)
}

/// Bands intersecting `[0, viewport_h)`, contiguous and non-overlapping, top to bottom.
/// `scroll` is clamped to `[0, max_scroll]`.
pub fn layout(inp: &LayoutInput, scroll: f32, viewport_h: f32) -> Vec<Band> {
    let vh = viewport_h.max(0.0);
    let g = Geo::new(inp, vh);
    let s = scroll.max(0.0).min(g.max_scroll(vh));
    let mut i = g.first_visible(s);
    let mut top = g.top(i) - s;
    let mut bands = Vec::with_capacity(((vh / g.row_h) as usize).saturating_add(2).min(g.count));
    while i < g.count && top < vh {
        let h = g.height(i);
        let card_lines = g.card.filter(|&(e, _)| e == i).map_or(0, |(_, l)| l);
        bands.push(Band {
            top,
            height: h,
            item: i,
            expanded: card_lines != 0,
            card_lines,
        });
        top += h;
        i += 1;
    }
    bands
}

/// New scroll offset: minimal movement so the selected band is present and whole (the whole card
/// when expanded; a band taller than the viewport is aligned to the top). Unchanged when there is
/// no valid selection or it is already whole. The result is within `[0, max_scroll]`.
pub fn ensure_selection_visible(inp: &LayoutInput, scroll: f32, viewport_h: f32) -> f32 {
    let vh = viewport_h.max(0.0);
    let g = Geo::new(inp, vh);
    let max = g.max_scroll(vh);
    let s = scroll.max(0.0).min(max);
    let Some(sel) = inp.selected.filter(|&i| i < g.count) else {
        return s;
    };
    let (top, h) = (g.top(sel), g.height(sel));
    let want = if top < s || h >= vh {
        top
    } else if top + h > s + vh {
        top + h - vh
    } else {
        s
    };
    want.min(max)
}

/// Item under viewport-relative `y` (bands are half-open `[top, top + height)`).
pub fn hit_test(bands: &[Band], y: f32) -> Option<usize> {
    bands
        .iter()
        .find(|b| y >= b.top && y < b.top + b.height)
        .map(|b| b.item)
}

/// Items moved by PgUp/PgDn: how many items the viewport holds (a shown card takes the space of
/// `1 + extra/row_h` rows). At least 1.
pub fn page_step(inp: &LayoutInput, viewport_h: f32) -> usize {
    let vh = viewport_h.max(0.0);
    let g = Geo::new(inp, vh);
    (((vh - g.extra()) / g.row_h) as usize).max(1)
}

/// Whether item's card (`card_lines` clamped to 1..=4) fits a viewport of `viewport_h` (geometry only).
pub fn card_fits(inp: &LayoutInput, viewport_h: f32, item: usize) -> bool {
    inp.metrics.card_height((inp.card_lines)(item).clamp(1, 4)) <= viewport_h
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 0.05; // tops reach ~30_000 px: f32 ulp there is 2e-3
    const SCALES: [f32; 4] = [1.0, 1.25, 1.5, 2.0];

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32 // 31 bits
        }
        /// Inclusive range.
        fn range(&mut self, lo: u32, hi: u32) -> u32 {
            lo + self.next() % (hi - lo + 1)
        }
        fn float(&mut self, lo: f32, hi: f32) -> f32 {
            lo + self.next() as f32 / 2_147_483_648.0 * (hi - lo)
        }
    }

    #[track_caller]
    fn near(a: f32, b: f32) {
        assert!((a - b).abs() <= EPS, "{a} vs {b}");
    }

    fn check_case(rng: &mut Lcg) {
        let m = Metrics::new(
            rng.range(10, 24),
            rng.range(10, 28),
            SCALES[rng.range(0, 3) as usize],
        );
        let count = rng.range(0, 300) as usize;
        let lines: Vec<u32> = (0..count).map(|_| rng.range(0, 6)).collect();
        let f = |i: usize| lines.get(i).copied().unwrap_or(1);
        let selected = match rng.range(0, 9) {
            0 => None,
            1 => Some(count + rng.range(0, 2) as usize), // out of range: ignored
            2 => count.checked_sub(1),                   // last item
            3 => Some(0),
            _ => (count > 0).then(|| rng.range(0, count as u32 - 1) as usize),
        };
        let expand = rng.range(0, 3) != 0;
        let vh = match rng.range(0, 3) {
            0 => rng.float(1.0, m.row_h),
            1 => rng.float(m.row_h, m.card_height(4) + 3.0 * m.row_h),
            2 => rng.float(1.0, 1500.0),
            _ => rng.range(1, 30) as f32 * m.row_h,
        };
        let inp = LayoutInput {
            metrics: &m,
            count,
            selected,
            expand,
            card_lines: &f,
        };
        let total_cards = |vh| content_height_in(&inp, vh);

        // which item (if any) must be expanded
        let sel_ok = selected.filter(|&i| i < count);
        let want_card = sel_ok.filter(|&i| expand && m.card_height(f(i).clamp(1, 4)) <= vh);
        if let (Some(i), true) = (sel_ok, expand) {
            assert_eq!(card_fits(&inp, vh, i), want_card.is_some());
        }

        // brute-force oracle for the total
        let oracle_total: f32 = (0..count)
            .map(|i| {
                if want_card == Some(i) {
                    m.card_height(f(i).clamp(1, 4))
                } else {
                    m.row_h
                }
            })
            .sum();
        assert!(
            (total_cards(vh) - oracle_total).abs() <= oracle_total * 1e-5 + EPS,
            "total"
        );
        let assumed = count as f32 * m.row_h
            + if sel_ok.is_some() && expand {
                m.card_height(f(sel_ok.unwrap_or(0)).clamp(1, 4)) - m.row_h
            } else {
                0.0
            };
        assert!(
            (content_height(&inp) - assumed).abs() <= assumed * 1e-5 + EPS,
            "content_height"
        );
        let total = total_cards(vh);
        let maxs = max_scroll(&inp, vh);
        near(maxs, (total - vh).max(0.0));

        let scroll = match rng.range(0, 5) {
            0 => -50.0,
            1 => 1e9,
            _ => rng.float(-50.0, total + 200.0),
        };
        let s = scroll.clamp(0.0, maxs);
        let bands = layout(&inp, scroll, vh);

        // ---- bands: valid, contiguous, correct kinds
        for (k, b) in bands.iter().enumerate() {
            assert!(b.item < count);
            assert!(
                b.height > 0.0 && b.top < vh && b.top + b.height > 0.0,
                "band outside the viewport: {b:?}"
            );
            assert!(b.height <= total + EPS);
            assert!(
                b.top + s + b.height <= total + EPS,
                "band beyond the content"
            );
            if let Some(n) = bands.get(k + 1) {
                assert_eq!(n.item, b.item + 1);
                assert!(
                    (n.top - (b.top + b.height)).abs() <= EPS,
                    "gap/overlap between bands"
                );
            }
            if b.expanded {
                assert_eq!(Some(b.item), want_card);
                assert_eq!(b.card_lines, f(b.item).clamp(1, 4));
                assert_eq!(b.height, m.card_height(b.card_lines), "card squashed");
            } else {
                assert_eq!((b.height, b.card_lines), (m.row_h, 0));
                assert_ne!(Some(b.item), want_card);
            }
        }
        // ---- the visible set equals a brute-force scan
        let mut acc = 0.0f32;
        for i in 0..count {
            let h = if want_card == Some(i) {
                m.card_height(f(i).clamp(1, 4))
            } else {
                m.row_h
            };
            let (top, bottom) = (acc - s, acc - s + h);
            let present = bands.iter().any(|b| b.item == i);
            if bottom > EPS && top < vh - EPS {
                assert!(
                    present,
                    "item {i} should be visible (top {top}, bottom {bottom}, vh {vh})"
                );
            }
            if present {
                assert!(bottom > -EPS && top < vh + EPS);
            }
            acc += h;
        }
        match (bands.first(), bands.last()) {
            (Some(first), Some(last)) => {
                assert!(first.top <= EPS);
                assert!(last.item == count - 1 || last.top + last.height >= vh - EPS);
                if s == 0.0 {
                    assert_eq!((first.item, first.top), (0, 0.0));
                }
            }
            _ => assert!(
                count == 0 || vh < 1e-6,
                "no bands for a non-empty list and a real viewport"
            ),
        }

        // ---- hit test maps band midpoints (and tops) back
        for b in &bands {
            let mid = b.top + b.height / 2.0;
            if mid >= 0.0 && mid < vh {
                assert_eq!(hit_test(&bands, mid), Some(b.item));
            }
            if b.top >= 0.0 {
                assert_eq!(hit_test(&bands, b.top), Some(b.item));
            }
        }
        if let Some(first) = bands.first() {
            assert_eq!(hit_test(&bands, first.top - 0.5), None); // above the first band
        }
        if let Some(last) = bands.last().filter(|l| l.item + 1 == count) {
            assert_eq!(hit_test(&bands, last.top + last.height), None);
        }

        // ---- scroll clamping
        assert_eq!(layout(&inp, 1e9, vh), layout(&inp, maxs, vh));
        assert_eq!(layout(&inp, -10.0, vh), layout(&inp, 0.0, vh));
        if let (Some(last), true) = (layout(&inp, maxs, vh).last(), total >= vh) {
            assert_eq!(last.item, count - 1);
            near(last.top + last.height, vh);
        }

        // ---- ensure_selection_visible
        let ns = ensure_selection_visible(&inp, scroll, vh);
        assert!(ns >= 0.0 && ns <= maxs, "scroll {ns} outside [0, {maxs}]");
        near(ensure_selection_visible(&inp, ns, vh), ns); // idempotent
        let after = layout(&inp, ns, vh);
        match sel_ok {
            None => near(ns, s),
            Some(sel) => {
                let b = after.iter().find(|b| b.item == sel).copied();
                let b = b.unwrap_or_else(|| panic!("selected band missing (sel {sel}, ns {ns})"));
                assert_eq!(b.expanded, want_card == Some(sel));
                assert_eq!(
                    b.height,
                    if b.expanded {
                        m.card_height(f(sel).clamp(1, 4))
                    } else {
                        m.row_h
                    }
                );
                if b.height <= vh + EPS {
                    assert!(
                        b.top >= -EPS && b.top + b.height <= vh + EPS,
                        "selected band not whole: {b:?} vh {vh}"
                    );
                } else {
                    near(b.top, 0.0); // taller than the viewport (tiny window): top aligned
                }
                // minimal movement: unchanged if it was whole already, else flush with an edge
                match bands.iter().find(|b| b.item == sel) {
                    Some(b0) if b0.top >= 0.0 && b0.top + b0.height <= vh => near(ns, s),
                    _ => assert!(
                        b.top.abs() <= EPS || (b.top + b.height - vh).abs() <= EPS,
                        "moved too far: {b:?}"
                    ),
                }
            }
        }
        // page step
        let step = page_step(&inp, vh);
        assert!(step >= 1);
        if want_card.is_none() && vh >= m.row_h {
            assert_eq!(step, (vh / m.row_h) as usize);
        }
    }

    #[test]
    fn property_random_layouts() {
        let mut rng = Lcg(0x5EED_C11B_4000_0001);
        for _ in 0..6000 {
            check_case(&mut rng);
        }
    }

    fn mk<'a>(
        m: &'a Metrics,
        count: usize,
        selected: Option<usize>,
        expand: bool,
        f: &'a dyn Fn(usize) -> u32,
    ) -> LayoutInput<'a> {
        LayoutInput {
            metrics: m,
            count,
            selected,
            expand,
            card_lines: f,
        }
    }

    #[test]
    fn empty_list() {
        let m = Metrics::new(14, 16, 1.0);
        let f = |_| 2;
        for (sel, expand) in [(None, false), (Some(0), true), (Some(7), true)] {
            let inp = mk(&m, 0, sel, expand, &f);
            assert!(layout(&inp, 0.0, 500.0).is_empty());
            assert!(layout(&inp, 99.0, 500.0).is_empty());
            assert_eq!(max_scroll(&inp, 500.0), 0.0);
            assert_eq!(ensure_selection_visible(&inp, 123.0, 500.0), 0.0);
            assert_eq!(hit_test(&[], 10.0), None);
            assert!(page_step(&inp, 500.0) >= 1);
        }
        assert_eq!(content_height(&mk(&m, 0, None, true, &f)), 0.0);
        assert_eq!(hit_test(&[], -1.0), None);
    }

    #[test]
    fn viewport_smaller_than_one_row() {
        let m = Metrics::new(14, 16, 1.0);
        let f = |_| 1;
        let inp = mk(&m, 100, Some(5), true, &f);
        let vh = m.row_h * 0.5;
        assert!(!card_fits(&inp, vh, 5));
        let ns = ensure_selection_visible(&inp, 0.0, vh);
        assert_eq!(ns, 5.0 * m.row_h); // top aligned
        let bands = layout(&inp, ns, vh);
        assert_eq!(
            bands,
            vec![Band {
                top: 0.0,
                height: m.row_h,
                item: 5,
                expanded: false,
                card_lines: 0
            }]
        );
        assert_eq!(hit_test(&bands, 1.0), Some(5));
        assert_eq!(hit_test(&bands, vh + 5.0), Some(5)); // the band is taller than the viewport
        assert!(layout(&inp, 0.0, 0.0).is_empty());
        assert!(page_step(&inp, vh) >= 1);
        // garbage in, no panic
        let _ = layout(&inp, f32::NAN, f32::NAN);
        let _ = layout(&inp, f32::INFINITY, -5.0);
        let _ = ensure_selection_visible(&inp, f32::NAN, f32::NAN);
        assert!(layout(&inp, f32::NAN, 200.0)
            .first()
            .is_some_and(|b| b.item == 0));
    }

    #[test]
    fn last_item_with_expansion_is_whole_and_above_the_bottom() {
        for (c, u, s) in [(14, 16, 1.0), (24, 28, 2.0), (10, 10, 1.25), (18, 22, 1.5)] {
            let m = Metrics::new(c, u, s);
            for lines in 1..=4u32 {
                let f = move |_| lines;
                for extra_rows in [0.0, 0.3, 1.0, 7.5] {
                    let vh = m.card_height(lines) + extra_rows * m.row_h; // card fits
                    let inp = mk(&m, 50, Some(49), true, &f);
                    let ns = ensure_selection_visible(&inp, 0.0, vh);
                    let bands = layout(&inp, ns, vh);
                    let last = bands.last().copied();
                    let last = last.unwrap_or_else(|| panic!("no bands"));
                    assert_eq!(
                        (last.item, last.expanded, last.card_lines),
                        (49, true, lines)
                    );
                    assert_eq!(last.height, m.card_height(lines));
                    assert!(
                        last.top >= -EPS && last.top + last.height <= vh + EPS,
                        "{last:?} vh {vh}"
                    );
                    assert_eq!(ns, max_scroll(&inp, vh)); // nothing below the card
                }
            }
        }
    }

    #[test]
    fn card_that_cannot_fit_is_a_whole_normal_row() {
        let m = Metrics::new(14, 16, 1.0);
        let f = |_| 4;
        let vh = m.card_height(4) - 1.0; // card does not fit, several rows do
        let inp = mk(&m, 40, Some(20), true, &f);
        assert!(!card_fits(&inp, vh, 20));
        let ns = ensure_selection_visible(&inp, 0.0, vh);
        let b = layout(&inp, ns, vh).into_iter().find(|b| b.item == 20);
        let b = b.unwrap_or_else(|| panic!("missing"));
        assert!(!b.expanded && b.height == m.row_h && b.top >= 0.0 && b.top + b.height <= vh);
        assert_eq!(content_height_in(&inp, vh), 40.0 * m.row_h);
        assert!(content_height(&inp) > 40.0 * m.row_h); // assumes the card; the exact value needs the viewport
        assert_eq!(page_step(&inp, vh), (vh / m.row_h) as usize);
    }

    #[test]
    fn selection_switching_between_row_and_card() {
        let m = Metrics::new(14, 16, 1.0);
        let f = |_| 3;
        let vh = 10.0 * m.row_h;
        // selected row 12 at the bottom of the viewport, plain row
        let plain = mk(&m, 100, Some(12), false, &f);
        let s1 = ensure_selection_visible(&plain, 0.0, vh);
        assert_eq!(s1, 13.0 * m.row_h - vh);
        // same selection becomes a card: must scroll further so the whole card shows
        let card = mk(&m, 100, Some(12), true, &f);
        let s2 = ensure_selection_visible(&card, s1, vh);
        assert!(s2 > s1);
        let bands = layout(&card, s2, vh);
        let b = bands
            .iter()
            .find(|b| b.item == 12)
            .copied()
            .unwrap_or_else(|| panic!("missing"));
        assert!(b.expanded && b.top >= 0.0 && b.top + b.height <= vh + EPS);
        near(b.top + b.height, vh);
        // and back: the row shrinks, scroll stays valid and the row stays whole
        let s3 = ensure_selection_visible(&plain, s2, vh);
        let b = layout(&plain, s3, vh)
            .into_iter()
            .find(|b| b.item == 12)
            .unwrap_or_else(|| panic!("missing"));
        assert!(!b.expanded && b.top >= 0.0 && b.top + b.height <= vh + EPS);
        // moving up from far below
        let up = mk(&m, 100, Some(3), true, &f);
        let s4 = ensure_selection_visible(&up, 50.0 * m.row_h, vh);
        assert_eq!(s4, 3.0 * m.row_h);
    }

    #[test]
    fn hit_test_with_a_card_in_the_middle() {
        let m = Metrics::new(14, 16, 1.0); // row 40, card(2 lines) = 100 + 46
        let f = |_| 2;
        let inp = mk(&m, 20, Some(3), true, &f);
        let vh = 700.0;
        let bands = layout(&inp, 0.0, vh);
        let card_h = m.card_height(2);
        assert_eq!(card_h, 146.0);
        assert_eq!(
            bands[3],
            Band {
                top: 120.0,
                height: card_h,
                item: 3,
                expanded: true,
                card_lines: 2
            }
        );
        assert_eq!(bands[4].top, 120.0 + card_h);
        assert_eq!(hit_test(&bands, 119.0), Some(2));
        assert_eq!(hit_test(&bands, 120.0), Some(3));
        assert_eq!(hit_test(&bands, 120.0 + card_h - 1.0), Some(3));
        assert_eq!(hit_test(&bands, 120.0 + card_h), Some(4)); // the row below the card, not the card
        assert_eq!(hit_test(&bands, 120.0 + card_h + 41.0), Some(5));
        // scrolled into the middle of the card: first band is the card with negative top
        let bands = layout(&inp, 150.0, vh);
        assert_eq!((bands[0].item, bands[0].top), (3, -30.0));
        assert_eq!(hit_test(&bands, 0.0), Some(3));
        assert_eq!(hit_test(&bands, card_h - 30.0), Some(4));
    }

    #[test]
    fn page_step_counts_items_and_accounts_for_the_card() {
        let m = Metrics::new(14, 16, 1.0);
        let f1 = |_| 1;
        let f3 = |_| 3;
        assert_eq!(page_step(&mk(&m, 100, Some(5), false, &f1), 520.0), 13);
        assert_eq!(page_step(&mk(&m, 100, None, true, &f1), 520.0), 13);
        assert_eq!(page_step(&mk(&m, 100, Some(5), true, &f1), 520.0), 10); // (520-83)/40
        assert_eq!(page_step(&mk(&m, 100, Some(5), true, &f3), 520.0), 9); // (520-129)/40
        assert_eq!(page_step(&mk(&m, 100, Some(5), true, &f1), 10.0), 1);
        assert_eq!(page_step(&mk(&m, 0, None, false, &f1), 0.0), 1);
    }

    #[test]
    fn card_fits_is_pure_geometry() {
        let m = Metrics::new(14, 16, 1.0);
        let f = |i: usize| if i == 0 { 1 } else { 9 }; // 9 clamps to 4
        let inp = mk(&m, 3, None, false, &f);
        assert!(card_fits(&inp, m.card_height(1), 0));
        assert!(!card_fits(&inp, m.card_height(1) - 0.5, 0));
        assert!(card_fits(&inp, m.card_height(4), 1));
        assert!(!card_fits(&inp, m.card_height(3), 1));
    }
}
