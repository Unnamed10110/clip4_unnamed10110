//! Preview-on-hover: resting the mouse on an image row pops up a larger copy of the image in a
//! small borderless window beside the overlay.
//!
//! The popup never takes focus and is transparent to the mouse, so it cannot disturb the list.
//! Decoding happens on the I/O worker (as for row thumbnails); the finished bitmap comes back
//! through `Overlay::on_thumb` under `id | PREVIEW_FLAG` and lives in the popup's own `Gfx`.

use super::*;
use crate::win::gfx::{rect, Gfx};
use crate::win::worker::IoTask;
use std::collections::VecDeque;
use windows::core::w;
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};

pub const PREVIEW_CLASS: PCWSTR = w!("clip4_preview");
/// Marks a worker result as a preview bitmap (the item id is in the low bits).
pub const PREVIEW_FLAG: u64 = 1 << 63;
pub(crate) const T_PREVIEW: usize = 5;
/// How long the mouse must rest on a row before the preview appears.
const DELAY_MS: u32 = 350;
/// Longest side of a preview, in logical px.
const MAX_SIDE: f32 = 480.0;
/// Previews that are kept decoded (small LRU: a re-hover is instant, memory stays bounded).
const KEEP: usize = 4;

/// Which side of the overlay the preview opens on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Right,
    Left,
}

/// Picks the side with room for `want_w` px; if neither has, the roomier one provided it is at
/// least `min_w`; otherwise there is no sensible place (None).
pub fn choose_side(right_space: f32, left_space: f32, want_w: f32, min_w: f32) -> Option<Side> {
    if right_space >= want_w {
        Some(Side::Right)
    } else if left_space >= want_w {
        Some(Side::Left)
    } else {
        let (best, side) = if right_space >= left_space { (right_space, Side::Right) } else { (left_space, Side::Left) };
        (best >= min_w).then_some(side)
    }
}

/// The size an image of `bw` x `bh` px is shown at: never enlarged, shrunk (keeping the aspect
/// ratio) to fit `max_w` x `max_h`.
pub fn fit_size(bw: u32, bh: u32, max_w: f32, max_h: f32) -> (f32, f32) {
    let (bw, bh) = (bw.max(1) as f32, bh.max(1) as f32);
    let f = (max_w / bw).min(max_h / bh).clamp(0.0, 1.0);
    ((bw * f).max(1.0), (bh * f).max(1.0))
}

/// Vertical position that centres a `h`-tall popup on `y`, kept inside `[top, bottom]`.
pub fn centred_y(y: i32, h: i32, top: i32, bottom: i32) -> i32 {
    (y - h / 2).clamp(top, (bottom - h).max(top))
}

/// Per-overlay hover-preview state.
pub struct Preview {
    pub hwnd: HWND,
    /// The image row the mouse is resting on (pane, item id).
    pub target: Option<(PaneId, u64)>,
    pub shown: bool,
    /// Preview bitmaps currently decoded in the popup's Gfx, oldest first.
    pub cached: VecDeque<u64>,
}

impl Preview {
    pub fn new() -> Preview {
        Preview { hwnd: HWND::default(), target: None, shown: false, cached: VecDeque::new() }
    }
}

impl Default for Preview {
    fn default() -> Self {
        Preview::new()
    }
}

/// The image format worth decoding for an item, if it has one.
fn image_source(it: &Item) -> Option<(FormatKey, Payload)> {
    it.formats.iter().find(|(k, _)| k.is_std(CF_DIBV5) || k.is_std(CF_DIB) || k.is_named(FMT_PNG)).map(|(k, p)| (k.clone(), p.clone()))
}

impl Overlay {
    pub(super) fn create_preview_window(&self, app: &App) {
        // SAFETY: class + window creation on the UI thread.
        let hwnd = unsafe {
            let wc = WNDCLASSW { lpfnWndProc: Some(preview_proc), hInstance: app.hinst, lpszClassName: PREVIEW_CLASS, ..Default::default() };
            RegisterClassW(&wc);
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PREVIEW_CLASS,
                w!("clip4 preview"),
                WS_POPUP,
                0,
                0,
                10,
                10,
                None,
                None,
                Some(app.hinst),
                None,
            )
            .unwrap_or_default()
        };
        self.round_corners(hwnd);
        self.st.borrow_mut().preview.hwnd = hwnd;
    }

    /// The image row at view index `row` of pane `pid`, if that row is an image.
    pub(super) fn image_row_target(&self, app: &App, pid: PaneId, row: usize) -> Option<(PaneId, u64)> {
        let id = {
            let st = self.st.borrow();
            if pid == PaneId::Main && st.scope == Scope::Snippets {
                return None;
            }
            st.panes[pid as usize].rows.get(row)?.id
        };
        (app.store.borrow().find(id)?.kind == Kind::Image).then_some((pid, id))
    }

    /// Called whenever the mouse moves over a row: `target` is the image row under it, if any.
    /// A change of row hides the popup and (re)starts the hover delay.
    pub(super) fn set_hover_preview(&self, target: Option<(PaneId, u64)>) {
        let (main, changed) = {
            let Ok(mut st) = self.st.try_borrow_mut() else { return };
            let changed = st.preview.target != target;
            st.preview.target = target;
            (st.panes[0].hwnd, changed)
        };
        if !changed {
            return;
        }
        self.hide_preview_window();
        // SAFETY: one-shot timer on the main pane window.
        unsafe {
            let _ = KillTimer(Some(main), T_PREVIEW);
            if target.is_some() {
                let _ = SetTimer(Some(main), T_PREVIEW, DELAY_MS, None);
            }
        }
    }

    /// The popup goes away (the hover target is kept, so resting on the row shows it again only
    /// after a fresh hover; use `set_hover_preview(None)` to forget the target too).
    pub(super) fn hide_preview_window(&self) {
        let hwnd = {
            let Ok(mut st) = self.st.try_borrow_mut() else { return };
            if !st.preview.shown {
                return;
            }
            st.preview.shown = false;
            st.preview.hwnd
        };
        // SAFETY: hiding our own window.
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }

    /// Hover target and popup both gone (overlay hidden, wheel, keyboard, ...).
    pub(super) fn clear_preview(&self) {
        self.set_hover_preview(None);
    }

    /// The hover delay elapsed: show the preview, or ask the worker for it first.
    pub(super) fn show_preview(&self, app: &App) {
        let (target, visible, scale, hwnd, main, pinned) = {
            let st = self.st.borrow();
            (st.preview.target, st.visible, st.scale, st.preview.hwnd, st.panes[0].hwnd, st.panes[1].hwnd)
        };
        let Some((pid, id)) = target else { return };
        if !visible || hwnd.0.is_null() {
            return;
        }
        let key = id | PREVIEW_FLAG;
        let size = self.gfx_preview.borrow().as_ref().and_then(|g| g.bitmap_size(key));
        let Some((bw, bh)) = size else {
            // Not decoded yet: ask once; `on_preview_thumb` calls back when it arrives.
            let already = self.st.borrow().panes[pid as usize].thumbs_asked.contains(&key);
            if !already {
                let src = app.store.borrow().find(id).and_then(image_source);
                if let Some((fk, payload)) = src {
                    self.st.borrow_mut().panes[pid as usize].thumbs_asked.insert(key);
                    app.workers.io(IoTask::Thumb { id: key, key: fk, payload, max_px: (MAX_SIDE * scale) as u32 });
                }
            }
            return;
        };

        let (mut mr, mut pr) = (RECT::default(), RECT::default());
        let mut cur = POINT::default();
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        // SAFETY: window / cursor / monitor queries.
        unsafe {
            let _ = GetWindowRect(main, &mut mr);
            let _ = GetWindowRect(pinned, &mut pr);
            let _ = GetCursorPos(&mut cur);
            let _ = GetMonitorInfoW(MonitorFromWindow(main, MONITOR_DEFAULTTONEAREST), &mut mi);
        }
        let wk = mi.rcWork;
        let (gap, pad) = (8.0 * scale, 8.0 * scale);
        let right_space = wk.right as f32 - mr.right as f32 - gap;
        let left_space = pr.left as f32 - wk.left as f32 - gap;
        let want_w = (bw as f32).min(MAX_SIDE * scale) + 2.0 * pad;
        let Some(side) = choose_side(right_space, left_space, want_w, 160.0 * scale) else { return };
        let space = if side == Side::Right { right_space } else { left_space };
        let (iw, ih) = fit_size(bw, bh, (space - 2.0 * pad).min(MAX_SIDE * scale), ((wk.bottom - wk.top) as f32 - 2.0 * pad).min(MAX_SIDE * scale));
        let (w, h) = ((iw + 2.0 * pad).round() as i32, (ih + 2.0 * pad).round() as i32);
        let x = if side == Side::Right { mr.right + gap as i32 } else { pr.left - gap as i32 - w };
        let y = centred_y(cur.y, h, wk.top, wk.bottom);
        // SAFETY: show without activating; topmost like the overlay itself.
        unsafe {
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        self.apply_region(hwnd, scale);
        self.st.borrow_mut().preview.shown = true;
    }

    /// A decoded preview bitmap arrived from the worker.
    pub(super) fn on_preview_thumb(&self, app: &App, id: u64, w: u32, h: u32, bgra: &[u8]) {
        let key = id | PREVIEW_FLAG;
        let evict = {
            let mut slot = self.gfx_preview.borrow_mut();
            if slot.is_none() {
                *slot = Gfx::new();
            }
            if let Some(gf) = slot.as_mut() {
                gf.put_bitmap(key, w, h, bgra);
            }
            let mut st = self.st.borrow_mut();
            for p in st.panes.iter_mut() {
                p.thumbs_asked.remove(&key);
            }
            st.preview.cached.retain(|k| *k != key);
            st.preview.cached.push_back(key);
            if st.preview.cached.len() > KEEP {
                st.preview.cached.pop_front()
            } else {
                None
            }
        };
        if let Some(old) = evict {
            if let Some(gf) = self.gfx_preview.borrow_mut().as_mut() {
                gf.remove_bitmap(old);
            }
        }
        // Still resting on that row? Then show it now.
        if self.st.borrow().preview.target.map(|t| t.1) == Some(id) {
            self.show_preview(app);
        }
    }

    pub(super) fn paint_preview(&self, hwnd: HWND) {
        let mut ps = PAINTSTRUCT::default();
        // SAFETY: standard BeginPaint/EndPaint pair; the client rect gives the surface size.
        unsafe {
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            if let (Ok(st), Ok(mut slot)) = (self.st.try_borrow(), self.gfx_preview.try_borrow_mut()) {
                if let Some(gf) = slot.as_mut() {
                    let (w, h) = (r.right as f32, r.bottom as f32);
                    if gf.begin(hdc, r.right, r.bottom) {
                        let s = st.scale;
                        gf.clear(st.pal.bg);
                        if let Some((_, id)) = st.preview.target {
                            let pad = 8.0 * s;
                            gf.draw_bitmap(id | PREVIEW_FLAG, rect(pad, pad, w - pad, h - pad));
                        }
                        gf.stroke_round(rect(0.0, 0.0, w, h), 8.0 * s, st.pal.hairline, 1.0);
                        gf.end();
                    }
                }
            }
            let _ = EndPaint(hwnd, &ps);
        }
    }
}

unsafe extern "system" fn preview_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    crate::win::util::wndproc_guard(h, m, w, l, || match m {
        WM_PAINT => {
            if let Some(app) = crate::win::app::app() {
                app.overlay.paint_preview(h);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        // Never take the mouse or the focus: pointer events fall through to what is beneath.
        WM_NCHITTEST => LRESULT(-1), // HTTRANSPARENT
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        // SAFETY: default handling of the same message.
        _ => unsafe { DefWindowProcW(h, m, w, l) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_prefers_right_then_left_then_the_roomier_one() {
        assert_eq!(choose_side(500.0, 500.0, 400.0, 160.0), Some(Side::Right));
        assert_eq!(choose_side(100.0, 500.0, 400.0, 160.0), Some(Side::Left));
        assert_eq!(choose_side(300.0, 250.0, 400.0, 160.0), Some(Side::Right), "neither fits: the roomier side");
        assert_eq!(choose_side(200.0, 350.0, 400.0, 160.0), Some(Side::Left));
        assert_eq!(choose_side(100.0, 120.0, 400.0, 160.0), None, "no usable room: no preview");
    }

    #[test]
    fn images_are_shrunk_to_fit_but_never_enlarged() {
        assert_eq!(fit_size(400, 300, 480.0, 480.0), (400.0, 300.0), "small images stay at natural size");
        assert_eq!(fit_size(960, 480, 480.0, 480.0), (480.0, 240.0), "wide image limited by width");
        assert_eq!(fit_size(480, 960, 480.0, 480.0), (240.0, 480.0), "tall image limited by height");
        let (w, h) = fit_size(1000, 1000, 200.0, 300.0);
        assert_eq!((w, h), (200.0, 200.0), "limited by the tighter of the two bounds");
        let (w, h) = fit_size(0, 0, 100.0, 100.0);
        assert!(w >= 1.0 && h >= 1.0, "degenerate sizes never produce an empty window");
        assert_eq!(fit_size(500, 500, 0.0, 0.0), (1.0, 1.0), "no room at all collapses to the minimum");
    }

    #[test]
    fn popup_is_centred_on_the_cursor_and_kept_on_screen() {
        assert_eq!(centred_y(500, 200, 0, 1000), 400);
        assert_eq!(centred_y(50, 200, 0, 1000), 0, "clamped to the top");
        assert_eq!(centred_y(990, 200, 0, 1000), 800, "clamped to the bottom");
        assert_eq!(centred_y(500, 2000, 0, 1000), 0, "taller than the screen: pinned to the top");
    }
}
