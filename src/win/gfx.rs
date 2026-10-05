//! Direct2D + DirectWrite drawing surface (spec 10.10).
//!
//! Rendering goes through an `ID2D1DCRenderTarget` bound to the paint DC. That keeps the
//! native EDIT child (the search field) correct: GDI clipping from `WS_CLIPCHILDREN` is
//! honoured, so the control is never painted over and nothing flickers.
//! Rendering is on demand only (WM_PAINT); there is no frame loop.

use crate::theme::Rgb;
use std::collections::HashMap;
use windows::core::{w, Interface, PCWSTR};
use windows_numerics::Vector2;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::HDC;

pub type RectF = D2D_RECT_F;

pub fn rect(l: f32, t: f32, r: f32, b: f32) -> RectF {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}

pub fn contains(r: &RectF, x: f32, y: f32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

pub fn inset(r: &RectF, d: f32) -> RectF {
    rect(r.left + d, r.top + d, r.right - d, r.bottom - d)
}

fn col(c: Rgb, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: c.r as f32 / 255.0, g: c.g as f32 / 255.0, b: c.b as f32 / 255.0, a }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Font {
    /// Clipboard content in the user's face.
    Content,
    /// Card body: contentSize + 3.
    ContentBig,
    /// Chrome (labels, buttons, key caps, ages).
    Ui,
    UiSemibold,
    /// Snippet names: chrome face at content size.
    Name,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

struct Fmt {
    fmt: IDWriteTextFormat,
    ellipsis: Option<IDWriteInlineObject>,
}

pub struct Gfx {
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
    rt: Option<ID2D1DCRenderTarget>,
    brush: Option<ID2D1SolidColorBrush>,
    fonts: HashMap<Font, Fmt>,
    ui_face: String,
    bitmaps: HashMap<u64, ID2D1Bitmap>,
    drawing: bool,
}

impl Gfx {
    pub fn new() -> Option<Gfx> {
        // SAFETY: factory creation.
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?;
            Some(Gfx { d2d, dw, rt: None, brush: None, fonts: HashMap::new(), ui_face: String::new(), bitmaps: HashMap::new(), drawing: false })
        }
    }

    pub fn ui_face(&self) -> &str {
        &self.ui_face
    }

    fn family_exists(&self, name: &str) -> bool {
        let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: font collection query.
        unsafe {
            let mut coll: Option<IDWriteFontCollection> = None;
            if self.dw.GetSystemFontCollection(&mut coll, false).is_err() {
                return false;
            }
            let Some(coll) = coll else { return false };
            let (mut idx, mut exists) = (0u32, false.into());
            coll.FindFamilyName(PCWSTR(w.as_ptr()), &mut idx, &mut exists).is_ok() && exists.as_bool()
        }
    }

    /// Installed font family names, sorted (for the Settings dropdown).
    pub fn installed_families(&self) -> Vec<String> {
        let mut out = Vec::new();
        // SAFETY: font collection enumeration.
        unsafe {
            let mut coll: Option<IDWriteFontCollection> = None;
            if self.dw.GetSystemFontCollection(&mut coll, false).is_err() {
                return out;
            }
            let Some(coll) = coll else { return out };
            for i in 0..coll.GetFontFamilyCount() {
                let Ok(fam) = coll.GetFontFamily(i) else { continue };
                let Ok(names) = fam.GetFamilyNames() else { continue };
                let mut idx = 0u32;
                let mut exists = false.into();
                let _ = names.FindLocaleName(w!("en-us"), &mut idx, &mut exists);
                let idx = if exists.as_bool() { idx } else { 0 };
                let Ok(len) = names.GetStringLength(idx) else { continue };
                let mut buf = vec![0u16; len as usize + 1];
                if names.GetString(idx, &mut buf).is_ok() {
                    out.push(String::from_utf16_lossy(&buf[..len as usize]));
                }
            }
        }
        out.sort_by_key(|s| s.to_lowercase());
        out.dedup();
        out
    }

    /// (Re)builds the text formats. Sizes are in physical pixels.
    pub fn set_fonts(&mut self, content_face: &str, content_px: f32, ui_px: f32) {
        let face = if self.family_exists(content_face) { content_face } else { "Consolas" };
        let ui = ["Segoe UI Variable Text", "Segoe UI", "Tahoma"].into_iter().find(|f| self.family_exists(f)).unwrap_or("Segoe UI");
        self.ui_face = ui.to_string();
        self.fonts.clear();
        let specs = [
            (Font::Content, face, content_px, DWRITE_FONT_WEIGHT_NORMAL),
            (Font::ContentBig, face, content_px + 3.0, DWRITE_FONT_WEIGHT_NORMAL),
            (Font::Ui, ui, ui_px, DWRITE_FONT_WEIGHT_NORMAL),
            (Font::UiSemibold, ui, ui_px, DWRITE_FONT_WEIGHT_SEMI_BOLD),
            (Font::Name, ui, content_px, DWRITE_FONT_WEIGHT_NORMAL),
        ];
        for (role, family, px, weight) in specs {
            if let Some(f) = self.make_format(family, px, weight) {
                self.fonts.insert(role, f);
            }
        }
    }

    fn make_format(&self, family: &str, px: f32, weight: DWRITE_FONT_WEIGHT) -> Option<Fmt> {
        let fam: Vec<u16> = family.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: DirectWrite object creation.
        unsafe {
            let fmt = self.dw.CreateTextFormat(PCWSTR(fam.as_ptr()), None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, px, w!("en-us")).ok()?;
            let _ = fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            let ellipsis = self.dw.CreateEllipsisTrimmingSign(&fmt).ok();
            Some(Fmt { fmt, ellipsis })
        }
    }

    fn layout(&self, s: &str, role: Font, max_w: f32, max_h: f32) -> Option<IDWriteTextLayout> {
        let f = self.fonts.get(&role)?;
        let w: Vec<u16> = s.encode_utf16().collect();
        // SAFETY: layout creation over a live slice.
        unsafe {
            let l = self.dw.CreateTextLayout(&w, &f.fmt, max_w.max(1.0), max_h.max(1.0)).ok()?;
            let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
            let _ = l.SetTrimming(&trim, f.ellipsis.as_ref());
            Some(l)
        }
    }

    /// Un-trimmed width of `s` (used to size buttons, segments and key caps).
    pub fn text_width(&self, s: &str, role: Font) -> f32 {
        let Some(l) = self.layout(s, role, 10_000.0, 1000.0) else { return s.chars().count() as f32 * 8.0 };
        let mut m = DWRITE_TEXT_METRICS::default();
        // SAFETY: metrics out param.
        unsafe {
            let _ = l.GetMetrics(&mut m);
        }
        m.widthIncludingTrailingWhitespace
    }

    pub fn line_height(&self, role: Font) -> f32 {
        let Some(l) = self.layout("Ag", role, 1000.0, 1000.0) else { return 16.0 };
        let mut m = DWRITE_TEXT_METRICS::default();
        // SAFETY: metrics out param.
        unsafe {
            let _ = l.GetMetrics(&mut m);
        }
        m.height
    }

    // ---- frame ----

    fn ensure_rt(&mut self) -> bool {
        if self.rt.is_some() {
            return true;
        }
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE, // no GPU driver load: ~60 MB less resident, plenty fast for a 640x520 list
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        // SAFETY: render target + brush creation.
        unsafe {
            let Ok(rt) = self.d2d.CreateDCRenderTarget(&props) else { return false };
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let Ok(b) = rt.CreateSolidColorBrush(&col(Rgb::WHITE, 1.0), None) else { return false };
            self.rt = Some(rt);
            self.brush = Some(b);
        }
        self.bitmaps.clear();
        true
    }

    /// Binds the paint DC and begins drawing. False if the device could not be created.
    pub fn begin(&mut self, hdc: HDC, w: i32, h: i32) -> bool {
        if !self.ensure_rt() {
            return false;
        }
        let Some(rt) = self.rt.as_ref() else { return false };
        let r = RECT { left: 0, top: 0, right: w, bottom: h };
        // SAFETY: DC valid for the duration of WM_PAINT.
        unsafe {
            if rt.BindDC(hdc, &r).is_err() {
                self.rt = None;
                return false;
            }
            rt.BeginDraw();
        }
        self.drawing = true;
        true
    }

    /// Ends the frame. On `D2DERR_RECREATE_TARGET` device resources are dropped and rebuilt on the next paint.
    pub fn end(&mut self) {
        if !self.drawing {
            return;
        }
        self.drawing = false;
        let Some(rt) = self.rt.as_ref() else { return };
        // SAFETY: matches BeginDraw above.
        let r = unsafe { rt.EndDraw(None, None) };
        if r.is_err() {
            self.rt = None;
            self.brush = None;
            self.bitmaps.clear();
        }
    }

    fn set_color(&self, c: Rgb, a: f32) -> Option<&ID2D1SolidColorBrush> {
        let b = self.brush.as_ref()?;
        // SAFETY: brush owned by this Gfx.
        unsafe { b.SetColor(&col(c, a)) };
        Some(b)
    }

    pub fn clear(&self, c: Rgb) {
        if let Some(rt) = self.rt.as_ref() {
            // SAFETY: inside BeginDraw/EndDraw.
            unsafe { rt.Clear(Some(&col(c, 1.0))) };
        }
    }

    pub fn fill_rect(&self, r: RectF, c: Rgb) {
        if let (Some(rt), Some(b)) = (self.rt.as_ref(), self.set_color(c, 1.0)) {
            // SAFETY: inside BeginDraw/EndDraw.
            unsafe { rt.FillRectangle(&r, b) };
        }
    }

    /// Antialiased rounded rectangle (GDI RoundRect is not).
    pub fn fill_round(&self, r: RectF, radius: f32, c: Rgb) {
        if let (Some(rt), Some(b)) = (self.rt.as_ref(), self.set_color(c, 1.0)) {
            let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
            // SAFETY: inside BeginDraw/EndDraw.
            unsafe { rt.FillRoundedRectangle(&rr, b) };
        }
    }

    pub fn stroke_round(&self, r: RectF, radius: f32, c: Rgb, width: f32) {
        if let (Some(rt), Some(b)) = (self.rt.as_ref(), self.set_color(c, 1.0)) {
            let rr = D2D1_ROUNDED_RECT { rect: inset(&r, width / 2.0), radiusX: radius, radiusY: radius };
            // SAFETY: inside BeginDraw/EndDraw.
            unsafe { rt.DrawRoundedRectangle(&rr, b, width, None) };
        }
    }

    pub fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32, c: Rgb, width: f32) {
        if let (Some(rt), Some(b)) = (self.rt.as_ref(), self.set_color(c, 1.0)) {
            // SAFETY: inside BeginDraw/EndDraw.
            unsafe { rt.DrawLine(Vector2 { X: x1, Y: y1 }, Vector2 { X: x2, Y: y2 }, b, width, None) };
        }
    }

    pub fn circle(&self, cx: f32, cy: f32, r: f32, c: Rgb, stroke: Option<f32>) {
        if let (Some(rt), Some(b)) = (self.rt.as_ref(), self.set_color(c, 1.0)) {
            let e = D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: r, radiusY: r };
            // SAFETY: inside BeginDraw/EndDraw.
            unsafe {
                match stroke {
                    Some(w) => rt.DrawEllipse(&e, b, w, None),
                    None => rt.FillEllipse(&e, b),
                }
            }
        }
    }

    pub fn push_clip(&self, r: RectF) {
        if let Some(rt) = self.rt.as_ref() {
            // SAFETY: balanced with pop_clip by the caller.
            unsafe { rt.PushAxisAlignedClip(&r, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE) };
        }
    }

    pub fn pop_clip(&self) {
        if let Some(rt) = self.rt.as_ref() {
            // SAFETY: matches push_clip.
            unsafe { rt.PopAxisAlignedClip() };
        }
    }

    /// Single-line text, vertically centred in `r`, ellipsised at the real column width.
    pub fn text(&self, s: &str, role: Font, r: RectF, c: Rgb, align: Align) {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 1.0 || h <= 1.0 || s.is_empty() {
            return;
        }
        let Some(l) = self.layout(s, role, w, h) else { return };
        // SAFETY: layout owned; drawing inside BeginDraw/EndDraw.
        unsafe {
            let _ = l.SetTextAlignment(match align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
            });
            let _ = l.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            if let (Some(rt), Some(b)) = (self.rt.as_ref(), self.set_color(c, 1.0)) {
                rt.DrawTextLayout(Vector2 { X: r.left, Y: r.top }, &l, b, D2D1_DRAW_TEXT_OPTIONS_CLIP);
            }
        }
    }

    // ---- bitmaps (thumbnails) ----

    pub fn has_bitmap(&self, id: u64) -> bool {
        self.bitmaps.contains_key(&id)
    }

    /// Pixel size of a cached bitmap.
    pub fn bitmap_size(&self, id: u64) -> Option<(u32, u32)> {
        // SAFETY: plain query on a live bitmap.
        self.bitmaps.get(&id).map(|bm| {
            let s = unsafe { bm.GetPixelSize() };
            (s.width, s.height)
        })
    }

    pub fn remove_bitmap(&mut self, id: u64) {
        self.bitmaps.remove(&id);
    }

    pub fn put_bitmap(&mut self, id: u64, w: u32, h: u32, bgra: &[u8]) {
        if !self.ensure_rt() || bgra.len() < (w * h * 4) as usize {
            return;
        }
        let Some(rt) = self.rt.as_ref() else { return };
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        // SAFETY: bgra is valid for w*h*4 bytes.
        unsafe {
            if let Ok(bm) = rt.CreateBitmap(D2D_SIZE_U { width: w, height: h }, Some(bgra.as_ptr() as *const _), w * 4, &props) {
                if self.bitmaps.len() > 200 {
                    self.bitmaps.clear();
                }
                self.bitmaps.insert(id, bm);
            }
        }
    }

    pub fn draw_bitmap(&self, id: u64, dest: RectF) {
        if let (Some(rt), Some(bm)) = (self.rt.as_ref(), self.bitmaps.get(&id)) {
            // SAFETY: inside BeginDraw/EndDraw.
            unsafe { rt.DrawBitmap(bm, Some(&dest), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None) };
        }
    }

    pub fn drop_bitmaps(&mut self) {
        self.bitmaps.clear();
    }
}

/// Keeps `HWND`-typed imports referenced for the UI modules that re-export helpers.
pub fn _hwnd_marker(_: HWND) {}

#[allow(dead_code)]
fn _iface_marker<T: Interface>() {}
