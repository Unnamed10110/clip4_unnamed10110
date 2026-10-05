//! Colour tokens, the 15 theme presets, per-element overrides and DPI-scaled metrics
//! (spec 10.2, 16.2, 16.4, 18.19-18.21). Pure logic: no Win32, no allocation.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const BLACK: Rgb = Rgb::new(0, 0, 0);
    pub const WHITE: Rgb = Rgb::new(255, 255, 255);

    pub const fn new(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }
    /// `0xRRGGBB`.
    pub const fn from_rgb_hex(rrggbb: u32) -> Rgb {
        Rgb::new((rrggbb >> 16) as u8, (rrggbb >> 8) as u8, rrggbb as u8)
    }
    /// Win32 COLORREF `0x00BBGGRR` (bits above 24 are ignored).
    pub const fn from_colorref(c: u32) -> Rgb {
        Rgb::new(c as u8, (c >> 8) as u8, (c >> 16) as u8)
    }
    pub const fn colorref(self) -> u32 {
        self.r as u32 | (self.g as u32) << 8 | (self.b as u32) << 16
    }
}

/// Linear blend of `a` toward `b` by `n/255` (`n = 0` -> `a`, `n = 255` -> `b`), rounded to nearest.
pub fn mix(a: Rgb, b: Rgb, n: u8) -> Rgb {
    let n = n as u32;
    let ch = |x: u8, y: u8| ((x as u32 * (255 - n) + y as u32 * n + 127) / 255) as u8;
    Rgb::new(ch(a.r, b.r), ch(a.g, b.g), ch(a.b, b.b))
}

// ---------------------------------------------------------------- presets (spec 16.2)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub text: Rgb,
    pub accent: Rgb,
    pub border: Rgb,
    pub dim: Rgb,
}

const fn preset_def(name: &'static str, text: u32, accent: u32, border: u32, dim: u32) -> Preset {
    Preset {
        name,
        text: Rgb::from_rgb_hex(text),
        accent: Rgb::from_rgb_hex(accent),
        border: Rgb::from_rgb_hex(border),
        dim: Rgb::from_rgb_hex(dim),
    }
}

/// The persisted theme index is an index into this table: APPEND new presets only (lesson 18.21).
pub const PRESETS: [Preset; 15] = [
    preset_def(
        "Neon Green (AS/400)",
        0x00FF66,
        0x00FF66,
        0x00C850,
        0x008028,
    ),
    preset_def("Neon Red", 0xFF2040, 0xFF2040, 0xDC1432, 0x780818),
    preset_def("Neon Blue", 0x2878FF, 0x2878FF, 0x1E64DC, 0x10388C),
    preset_def("Neon Cyan", 0x00F0FF, 0x00F0FF, 0x00C8E6, 0x006478),
    preset_def("Neon Purple", 0xC83CFF, 0xC83CFF, 0xAA28DC, 0x5A1482),
    preset_def("Neon Yellow", 0xFFE600, 0xFFE600, 0xDCC800, 0x786E00),
    preset_def("Neon Orange", 0xFF8000, 0xFF8000, 0xDC6E00, 0x823C00),
    preset_def("Neon White", 0xF0F0F0, 0xF0F0F0, 0xC8C8C8, 0x6E6E6E),
    preset_def("Slate", 0xB6D5F4, 0x83BDF8, 0x466C93, 0x233A51),
    preset_def("Teal", 0xA7DDDC, 0x57CCCC, 0x277676, 0x104040),
    preset_def("Moss", 0xBBDBBB, 0x8CC98E, 0x4D744E, 0x273F28),
    preset_def("Ember", 0xE9CBAB, 0xE3AB6A, 0x856136, 0x493319),
    preset_def("Clay", 0xF3C4BF, 0xF39D95, 0x8F5753, 0x4F2D2B),
    preset_def("Violet", 0xD3CBF2, 0xBCAAF4, 0x6C6090, 0x3A334F),
    preset_def("Graphite", 0xD1D1D1, 0xB7B7B7, 0x696969, 0x383838),
];
pub const DEFAULT_PRESET: usize = 0;
/// Registry sentinel: "follow the preset" (spec 16.4).
pub const COLOR_FOLLOW_PRESET: u32 = 0xFF00_0000;

/// Preset at `idx`; an out-of-range index yields the default preset.
pub fn preset(idx: usize) -> Preset {
    PRESETS.get(idx).copied().unwrap_or(PRESETS[DEFAULT_PRESET])
}

// ---------------------------------------------------------------- overrides (spec 16.2 / 16.4)

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Overrides {
    pub bg: Option<Rgb>,
    pub text: Option<Rgb>,
    pub accent: Option<Rgb>,
    pub sel_text: Option<Rgb>,
    pub border: Option<Rgb>,
    pub dim: Option<Rgb>,
}

impl Overrides {
    fn slots(&self) -> [Option<Rgb>; 6] {
        [
            self.bg,
            self.text,
            self.accent,
            self.sel_text,
            self.border,
            self.dim,
        ]
    }

    pub fn any(&self) -> bool {
        self.slots().iter().any(Option::is_some)
    }

    /// Registry order: `ColorBG, ColorTXT, ColorSELBG (accent), ColorSELFG (selected text),
    /// ColorBORDER, ColorDIM`. `0xFF000000` or any value with a non-zero high byte
    /// (malformed) follows the preset; otherwise `0x00BBGGRR`.
    pub fn from_registry(v: [u32; 6]) -> Overrides {
        let c = |x: u32| (x >> 24 == 0).then(|| Rgb::from_colorref(x));
        Overrides {
            bg: c(v[0]),
            text: c(v[1]),
            accent: c(v[2]),
            sel_text: c(v[3]),
            border: c(v[4]),
            dim: c(v[5]),
        }
    }

    pub fn to_registry(&self) -> [u32; 6] {
        self.slots()
            .map(|s| s.map_or(COLOR_FOLLOW_PRESET, Rgb::colorref))
    }
}

/// Theme selection (settings dropdown, also when re-selecting the current preset): store the
/// (clamped) index AND clear every override, or the preset has no visible effect (lesson 18.20).
pub fn select_preset(current: &mut usize, o: &mut Overrides, idx: usize) {
    *current = idx.min(PRESETS.len() - 1);
    *o = Overrides::default();
}

// ---------------------------------------------------------------- palette (spec 10.2)

/// All derived colours. Text is neutral: ink tokens blend BG toward white, never toward the
/// preset's own text colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: Rgb,
    pub accent: Rgb,
    pub surface_hover: Rgb,
    pub surface_field: Rgb,
    pub surface_raised: Rgb,
    pub hairline: Rgb,
    pub card_edge: Rgb,
    pub ink_high: Rgb,
    pub ink_mid: Rgb,
    pub ink_low: Rgb,
    pub selected_row: Rgb,
    pub selected_row_inactive: Rgb,
    pub sel_text: Rgb,
    pub badge_fill: Rgb,
    pub badge_text: Rgb,
    pub button_primary: Rgb,
    pub button_primary_text: Rgb,
}

/// Overrides: `bg`/`accent` replace BG/ACCENT; `text` replaces the "white" that `ink_high` and
/// `ink_mid` blend toward (tints them); `border` replaces `hairline`; `dim` replaces `ink_low`;
/// `sel_text` replaces `sel_text` (default `ink_high`). Out-of-range `preset` => `DEFAULT_PRESET`.
pub fn palette(preset_idx: usize, o: &Overrides) -> Palette {
    let bg = o.bg.unwrap_or(Rgb::BLACK);
    let accent = o.accent.unwrap_or(preset(preset_idx).accent);
    let ink = o.text.unwrap_or(Rgb::WHITE);
    let white = |n| mix(bg, Rgb::WHITE, n);
    let ink_high = mix(bg, ink, 214);
    Palette {
        bg,
        accent,
        surface_hover: white(12),
        surface_field: white(18),
        surface_raised: white(30),
        hairline: o.border.unwrap_or_else(|| white(26)),
        card_edge: white(38),
        ink_high,
        ink_mid: mix(bg, ink, 120),
        ink_low: o.dim.unwrap_or_else(|| white(86)),
        selected_row: mix(bg, accent, 40),
        selected_row_inactive: mix(bg, accent, 20),
        sel_text: o.sel_text.unwrap_or(ink_high),
        badge_fill: mix(bg, accent, 46),
        badge_text: accent,
        button_primary: accent,
        button_primary_text: mix(accent, Rgb::BLACK, 200),
    }
}

// ---------------------------------------------------------------- metrics (spec 10.2)

/// Logical-px constants of the expanded card. Everything about the card (its height and the
/// parts inside) derives from these, so the body can never touch the buttons (lesson 18.19).
const CARD_PAD: f32 = 14.0;
const META_GAP: f32 = 8.0;
const BODY_BUTTONS_GAP: f32 = 10.0;
const META_EXTRA: f32 = 8.0; // meta row height = uiSize + 8
const BUTTONS_EXTRA: f32 = 14.0; // buttons height = uiSize + 14
const LINE_EXTRA: f32 = 9.0; // body line height = contentSize + 9
const MAX_CARD_LINES: u32 = 4;

/// All values in PHYSICAL pixels (logical x `scale`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub scale: f32,
    pub content: f32,
    pub ui: f32,
    pub row_h: f32,
    pub header_h: f32,
    pub search_top: f32,
    pub search_h: f32,
    pub footer_h: f32,
    pub side_inset: f32,
    pub icon_gutter: f32,
    pub age_col: f32,
    pub row_gap_min: f32,
    pub r_row: f32,
    pub r_card: f32,
    pub r_field: f32,
    pub r_button: f32,
    pub r_cap: f32,
    pub content_size: u32,
    pub ui_size: u32,
}

/// Geometry inside a card of height `card_height(lines)`, relative to the card's top-left.
/// `pad` is the top AND bottom padding; the buttons end at `card_height - pad`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CardParts {
    pub pad: f32,
    pub meta_y: f32,
    pub meta_h: f32,
    pub body_y: f32,
    pub body_h: f32,
    pub buttons_y: f32,
    pub buttons_h: f32,
    pub gap_body_buttons: f32,
}

impl Metrics {
    /// `content_size` is clamped to 10..=24, `ui_size` to 10..=28; a non-finite or
    /// non-positive `scale` (dpi/96) falls back to 1.0.
    pub fn new(content_size: u32, ui_size: u32, scale: f32) -> Metrics {
        let (content_size, ui_size) = (content_size.clamp(10, 24), ui_size.clamp(10, 28));
        let s = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let (c, u) = (content_size as f32, ui_size as f32);
        let big = c.max(u);
        Metrics {
            scale: s,
            content: c * s,
            ui: u * s,
            row_h: (c + 26.0) * s,
            header_h: (big + 40.0) * s,
            search_top: 12.0 * s,
            search_h: (big + 20.0) * s,
            footer_h: (u + 22.0) * s,
            side_inset: 8.0 * s,
            icon_gutter: (c + 12.0) * s,
            age_col: 3.0 * c * s,
            row_gap_min: 12.0 * s,
            r_row: 9.0 * s,
            r_card: 11.0 * s,
            r_field: 9.0 * s,
            r_button: 7.0 * s,
            r_cap: 5.0 * s,
            content_size,
            ui_size,
        }
    }

    /// Body line height in the expanded card: `(contentSize + 9) * scale`.
    pub fn card_line_height(&self) -> f32 {
        (self.content_size as f32 + LINE_EXTRA) * self.scale
    }

    /// `(2*ui + 68 + lines.clamp(1,4) * (content + 9)) * scale` (spec 10.2), derived from `card_parts`.
    pub fn card_height(&self, lines: u32) -> f32 {
        let p = self.card_parts(lines);
        p.buttons_y + p.buttons_h + p.pad
    }

    pub fn card_parts(&self, lines: u32) -> CardParts {
        let (s, ui) = (self.scale, self.ui_size as f32);
        let n = lines.clamp(1, MAX_CARD_LINES) as f32;
        let meta_h = ui + META_EXTRA;
        let body_y = CARD_PAD + meta_h + META_GAP;
        let body_h = n * (self.content_size as f32 + LINE_EXTRA);
        CardParts {
            pad: CARD_PAD * s,
            meta_y: CARD_PAD * s,
            meta_h: meta_h * s,
            body_y: body_y * s,
            body_h: body_h * s,
            buttons_y: (body_y + body_h + BODY_BUTTONS_GAP) * s,
            buttons_h: (ui + BUTTONS_EXTRA) * s,
            gap_body_buttons: BODY_BUTTONS_GAP * s,
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const SCALES: [f32; 4] = [1.0, 1.25, 1.5, 2.0];

    fn hex(v: u32) -> Rgb {
        Rgb::from_rgb_hex(v)
    }

    #[test]
    fn presets_match_spec_table() {
        // (name, text, accent, border, dim) exactly as in spec 16.2.
        let t: [(&str, u32, u32, u32, u32); 15] = [
            (
                "Neon Green (AS/400)",
                0x00FF66,
                0x00FF66,
                0x00C850,
                0x008028,
            ),
            ("Neon Red", 0xFF2040, 0xFF2040, 0xDC1432, 0x780818),
            ("Neon Blue", 0x2878FF, 0x2878FF, 0x1E64DC, 0x10388C),
            ("Neon Cyan", 0x00F0FF, 0x00F0FF, 0x00C8E6, 0x006478),
            ("Neon Purple", 0xC83CFF, 0xC83CFF, 0xAA28DC, 0x5A1482),
            ("Neon Yellow", 0xFFE600, 0xFFE600, 0xDCC800, 0x786E00),
            ("Neon Orange", 0xFF8000, 0xFF8000, 0xDC6E00, 0x823C00),
            ("Neon White", 0xF0F0F0, 0xF0F0F0, 0xC8C8C8, 0x6E6E6E),
            ("Slate", 0xB6D5F4, 0x83BDF8, 0x466C93, 0x233A51),
            ("Teal", 0xA7DDDC, 0x57CCCC, 0x277676, 0x104040),
            ("Moss", 0xBBDBBB, 0x8CC98E, 0x4D744E, 0x273F28),
            ("Ember", 0xE9CBAB, 0xE3AB6A, 0x856136, 0x493319),
            ("Clay", 0xF3C4BF, 0xF39D95, 0x8F5753, 0x4F2D2B),
            ("Violet", 0xD3CBF2, 0xBCAAF4, 0x6C6090, 0x3A334F),
            ("Graphite", 0xD1D1D1, 0xB7B7B7, 0x696969, 0x383838),
        ];
        for (p, e) in PRESETS.iter().zip(t) {
            assert_eq!(p.name, e.0);
            assert_eq!(
                (p.text, p.accent, p.border, p.dim),
                (hex(e.1), hex(e.2), hex(e.3), hex(e.4)),
                "{}",
                e.0
            );
        }
        assert_eq!(PRESETS[DEFAULT_PRESET].name, "Neon Green (AS/400)");
        assert_eq!(preset(99), PRESETS[0]);
        assert_eq!(preset(14).name, "Graphite");
    }

    #[test]
    fn mix_endpoints_and_rounding() {
        let (a, b) = (Rgb::new(10, 100, 250), Rgb::new(200, 0, 255));
        assert_eq!(mix(a, b, 0), a);
        assert_eq!(mix(a, b, 255), b);
        assert_eq!(mix(Rgb::BLACK, Rgb::WHITE, 12), Rgb::new(12, 12, 12));
        assert_eq!(mix(Rgb::WHITE, Rgb::BLACK, 200), Rgb::new(55, 55, 55));
        // 102 * 40 / 255 = 16.0 ; 102 * 46 / 255 = 18.4 (down) ; 102 * 200 / 255 = 80.0
        assert_eq!(mix(Rgb::BLACK, Rgb::new(0, 255, 102), 46).b, 18);
        // 1/255 * 128 = 0.50196 -> rounds up to 1 ; 1/255 * 127 = 0.498 -> 0
        assert_eq!(mix(Rgb::BLACK, Rgb::new(1, 1, 1), 128).r, 1);
        assert_eq!(mix(Rgb::BLACK, Rgb::new(1, 1, 1), 127).r, 0);
        // symmetric: mix(a, b, n) is within 1 of mix(b, a, 255 - n)
        for n in 0..=255u8 {
            let (x, y) = (mix(a, b, n), mix(b, a, 255 - n));
            assert!(x.r.abs_diff(y.r) <= 1 && x.g.abs_diff(y.g) <= 1 && x.b.abs_diff(y.b) <= 1);
        }
    }

    #[test]
    fn rgb_hex_and_colorref_round_trip() {
        assert_eq!(hex(0x112233), Rgb::new(0x11, 0x22, 0x33));
        assert_eq!(Rgb::from_colorref(0x00332211), Rgb::new(0x11, 0x22, 0x33));
        assert_eq!(Rgb::new(0x11, 0x22, 0x33).colorref(), 0x0033_2211);
        assert_eq!(Rgb::from_colorref(0xAB33_2211), Rgb::new(0x11, 0x22, 0x33)); // high byte ignored
        let mut x = 12345u32;
        for _ in 0..1000 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            let c = x & 0x00FF_FFFF;
            assert_eq!(Rgb::from_colorref(c).colorref(), c);
            assert_eq!(hex(c).r, (c >> 16) as u8);
        }
        assert_eq!(Rgb::default(), Rgb::BLACK);
    }

    #[test]
    fn registry_round_trip_and_malformed() {
        let o = Overrides {
            bg: Some(Rgb::new(1, 2, 3)),
            text: None,
            accent: Some(Rgb::new(4, 5, 6)),
            sel_text: Some(Rgb::BLACK),
            border: None,
            dim: Some(Rgb::WHITE),
        };
        let r = o.to_registry();
        assert_eq!(
            r,
            [
                0x030201,
                COLOR_FOLLOW_PRESET,
                0x060504,
                0,
                COLOR_FOLLOW_PRESET,
                0xFFFFFF
            ]
        );
        assert_eq!(Overrides::from_registry(r), o);
        assert!(o.any());
        assert!(!Overrides::default().any());
        assert_eq!(Overrides::default().to_registry(), [COLOR_FOLLOW_PRESET; 6]);
        assert_eq!(
            Overrides::from_registry([COLOR_FOLLOW_PRESET; 6]),
            Overrides::default()
        );
        // any non-zero high byte is malformed -> follow preset
        let bad = Overrides::from_registry([
            0x01000000,
            0xFFFFFFFF,
            0x80112233,
            0x00000100 | 0xFF000000,
            0x7F000000,
            0x00ABCDEF,
        ]);
        assert_eq!(
            bad,
            Overrides {
                dim: Some(Rgb::from_colorref(0xABCDEF)),
                ..Overrides::default()
            }
        );
        assert_eq!(bad.to_registry()[5], 0x00ABCDEF);
        assert!(bad.to_registry()[..5]
            .iter()
            .all(|&v| v == COLOR_FOLLOW_PRESET));
    }

    #[test]
    fn preset_zero_tokens_hand_computed() {
        // BG black, ACCENT 00FF66. mix(black, white, n) = (n, n, n).
        let p = palette(0, &Overrides::default());
        let g = |n: u8| Rgb::new(n, n, n);
        assert_eq!(p.bg, Rgb::BLACK);
        assert_eq!(p.accent, Rgb::new(0x00, 0xFF, 0x66));
        assert_eq!(p.surface_hover, g(12));
        assert_eq!(p.surface_field, g(18));
        assert_eq!(p.surface_raised, g(30));
        assert_eq!(p.hairline, g(26));
        assert_eq!(p.card_edge, g(38));
        assert_eq!(p.ink_high, g(214));
        assert_eq!(p.ink_mid, g(120));
        assert_eq!(p.ink_low, g(86));
        assert_eq!(p.selected_row, Rgb::new(0, 40, 16)); // 255*40/255=40, 102*40/255=16.0
        assert_eq!(p.selected_row_inactive, Rgb::new(0, 20, 8)); // 102*20/255=8.0
        assert_eq!(p.sel_text, g(214));
        assert_eq!(p.badge_fill, Rgb::new(0, 46, 18)); // 102*46/255=18.4
        assert_eq!(p.badge_text, p.accent);
        assert_eq!(p.button_primary, p.accent);
        assert_eq!(p.button_primary_text, Rgb::new(0, 55, 22)); // 255*55/255, 102*55/255=22.0
    }

    #[test]
    fn out_of_range_preset_is_default_and_other_presets_use_their_accent() {
        let o = Overrides::default();
        assert_eq!(palette(15, &o), palette(0, &o));
        assert_eq!(palette(usize::MAX, &o), palette(0, &o));
        for (i, pr) in PRESETS.iter().enumerate() {
            let p = palette(i, &o);
            assert_eq!(p.accent, pr.accent);
            assert_eq!(p.surface_hover, palette(0, &o).surface_hover); // neutral surfaces never depend on the preset
            assert_eq!(p.ink_high, palette(0, &o).ink_high);
        }
    }

    #[test]
    fn override_semantics() {
        let base = palette(0, &Overrides::default());
        let c = Rgb::new(200, 100, 50);
        // bg shifts every BG-relative token
        let p = palette(
            0,
            &Overrides {
                bg: Some(Rgb::new(20, 30, 40)),
                ..Default::default()
            },
        );
        assert_eq!(p.bg, Rgb::new(20, 30, 40));
        assert_eq!(p.surface_hover, mix(Rgb::new(20, 30, 40), Rgb::WHITE, 12));
        assert_eq!(p.selected_row, mix(Rgb::new(20, 30, 40), base.accent, 40));
        assert_eq!(p.button_primary_text, base.button_primary_text);
        // accent
        let p = palette(
            3,
            &Overrides {
                accent: Some(c),
                ..Default::default()
            },
        );
        assert_eq!((p.accent, p.badge_text, p.button_primary), (c, c, c));
        assert_eq!(p.selected_row, mix(Rgb::BLACK, c, 40));
        assert_eq!(p.selected_row_inactive, mix(Rgb::BLACK, c, 20));
        assert_eq!(p.badge_fill, mix(Rgb::BLACK, c, 46));
        assert_eq!(p.button_primary_text, mix(c, Rgb::BLACK, 200));
        // text tints ink_high / ink_mid and (by default) sel_text, nothing else
        let p = palette(
            0,
            &Overrides {
                text: Some(c),
                ..Default::default()
            },
        );
        assert_eq!(p.ink_high, mix(Rgb::BLACK, c, 214));
        assert_eq!(p.ink_mid, mix(Rgb::BLACK, c, 120));
        assert_eq!(p.sel_text, p.ink_high);
        assert_eq!(
            (p.ink_low, p.hairline, p.surface_field),
            (base.ink_low, base.hairline, base.surface_field)
        );
        // sel_text overrides sel_text only
        let p = palette(
            0,
            &Overrides {
                text: Some(c),
                sel_text: Some(Rgb::WHITE),
                ..Default::default()
            },
        );
        assert_eq!(p.sel_text, Rgb::WHITE);
        assert_eq!(p.ink_high, mix(Rgb::BLACK, c, 214));
        // border -> hairline, dim -> ink_low, verbatim
        let p = palette(
            0,
            &Overrides {
                border: Some(c),
                dim: Some(Rgb::new(9, 8, 7)),
                ..Default::default()
            },
        );
        assert_eq!((p.hairline, p.ink_low), (c, Rgb::new(9, 8, 7)));
        assert_eq!((p.card_edge, p.ink_mid), (base.card_edge, base.ink_mid));
    }

    #[test]
    fn select_preset_clears_all_overrides() {
        let all = Some(Rgb::new(9, 9, 9));
        let mut o = Overrides {
            bg: all,
            text: all,
            accent: all,
            sel_text: all,
            border: all,
            dim: all,
        };
        let mut cur = 0usize;
        assert!(palette(cur, &o) != palette(5, &Overrides::default()));
        select_preset(&mut cur, &mut o, 5);
        assert_eq!(cur, 5);
        assert_eq!(o, Overrides::default());
        assert!(!o.any());
        assert_eq!(palette(cur, &o), palette(5, &Overrides::default()));
        // re-selecting the current preset with overrides active also clears them
        o.accent = all;
        select_preset(&mut cur, &mut o, 5);
        assert_eq!((cur, o.any()), (5, false));
        assert_eq!(palette(cur, &o).accent, PRESETS[5].accent);
        // clamped
        select_preset(&mut cur, &mut o, 1000);
        assert_eq!(cur, 14);
    }

    // ---- metrics

    #[test]
    fn spec_metrics_numbers() {
        for content in 10..=24u32 {
            for ui in 10..=28u32 {
                for s in SCALES {
                    let m = Metrics::new(content, ui, s);
                    let (c, u, big) = (content as f32, ui as f32, content.max(ui) as f32);
                    let near = |a: f32, b: f32| assert!((a - b).abs() < 1e-3, "{a} vs {b}");
                    near(m.row_h, (c + 26.0) * s);
                    near(m.header_h, (big + 40.0) * s);
                    near(m.search_h, (big + 20.0) * s);
                    near(m.search_top, 12.0 * s);
                    near(m.footer_h, (u + 22.0) * s);
                    near(m.icon_gutter, (c + 12.0) * s);
                    near(m.age_col, 3.0 * c * s);
                    near(m.side_inset, 8.0 * s);
                    near(m.row_gap_min, 12.0 * s);
                    near(m.content, c * s);
                    near(m.ui, u * s);
                    near(m.r_row, 9.0 * s);
                    near(m.r_card, 11.0 * s);
                    near(m.r_field, 9.0 * s);
                    near(m.r_button, 7.0 * s);
                    near(m.r_cap, 5.0 * s);
                    near(m.card_line_height(), (c + 9.0) * s);
                    // search field + 12 top + 8 bottom fills the header exactly
                    near(m.search_top + m.search_h + 8.0 * s, m.header_h);
                }
            }
        }
        let m = Metrics::new(14, 16, 1.0);
        assert_eq!(
            (
                m.row_h,
                m.header_h,
                m.search_h,
                m.footer_h,
                m.icon_gutter,
                m.age_col
            ),
            (40.0, 56.0, 36.0, 38.0, 26.0, 42.0)
        );
        assert_eq!(m.card_height(1), 2.0 * 16.0 + 68.0 + 23.0);
    }

    #[test]
    fn sizes_and_scale_are_clamped() {
        let m = Metrics::new(0, 100, 1.0);
        assert_eq!((m.content_size, m.ui_size), (10, 28));
        let m = Metrics::new(99, 0, 1.0);
        assert_eq!((m.content_size, m.ui_size), (24, 10));
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(Metrics::new(14, 16, bad).scale, 1.0);
        }
    }

    #[test]
    fn card_parts_never_overlap_and_gap_ge_8_everywhere() {
        let near = |a: f32, b: f32| assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        for content in 10..=24u32 {
            for ui in 10..=28u32 {
                for s in SCALES {
                    let m = Metrics::new(content, ui, s);
                    for lines in 0..=6u32 {
                        let n = lines.clamp(1, 4) as f32;
                        let (p, h) = (m.card_parts(lines), m.card_height(lines));
                        // spec 10.2: 2*ui + 68 + lines * (content + 9)
                        near(h, (2.0 * ui as f32 + 68.0 + n * (content as f32 + 9.0)) * s);
                        near(p.body_h, n * m.card_line_height());
                        // the real gap (not just the nominal field) is >= 8 logical px
                        let gap = p.buttons_y - (p.body_y + p.body_h);
                        assert!(
                            gap >= 8.0 * s - 1e-3,
                            "gap {gap} c{content} u{ui} s{s} l{lines}"
                        );
                        assert!(p.gap_body_buttons >= 8.0 * s - 1e-3);
                        near(gap, p.gap_body_buttons);
                        // ordering: pad < meta < body < buttons < bottom pad
                        near(p.meta_y, p.pad);
                        assert!(p.meta_y + p.meta_h <= p.body_y + 1e-3);
                        assert!(p.body_y + p.body_h <= p.buttons_y + 1e-3);
                        // buttons end exactly at the bottom padding line; nothing leaks out
                        near(p.buttons_y + p.buttons_h + p.pad, h);
                        assert!(p.buttons_y + p.buttons_h <= h - p.pad + 1e-3);
                        // meta/buttons heights fit the chrome text
                        assert!(p.meta_h >= m.ui && p.buttons_h >= m.ui);
                    }
                    assert!(m.card_height(1) > m.row_h, "a card is taller than a row");
                    assert!(m.card_height(4) > m.card_height(3));
                    assert_eq!(m.card_height(0), m.card_height(1));
                    assert_eq!(m.card_height(9), m.card_height(4));
                }
            }
        }
    }
}
