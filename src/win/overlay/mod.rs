//! The two-window overlay (spec 10): a pinned pane on the left and the main list on the
//! right, moved together. State lives in `State`; painting is in `paint`, input in `input`.
//!
//! Rule for this module: borrow `st` briefly, copy what you need, DROP the borrow, then call
//! Win32 (SetWindowText, MoveWindow, ... can synchronously re-enter our window procedures).

mod input;
mod paint;
mod preview;

use super::app::App;
use super::gfx::Gfx;
use super::util::{pcw, wide};
use crate::layout::{self, LayoutInput};
use crate::model::*;
use crate::search;
use crate::snippets_fmt::Snippet;
use crate::store::Store;
use crate::theme::{self, Metrics, Overrides, Palette};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use preview::{Preview, PREVIEW_FLAG, T_PREVIEW};
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const PANE_CLASS: PCWSTR = w!("clip4_pane");

pub(crate) const T_ACQUIRE: usize = 1;
pub(crate) const T_AGES: usize = 2;
pub(crate) const T_FOCUSLOSS: usize = 3;
/// While the overlay is up, check every 250 ms that it still owns the foreground.
pub(crate) const T_FOCUSPOLL: usize = 4;

pub const GAP: f32 = 12.0;
/// Thickness of the draggable resize edges (logical px).
pub const GRIP: f32 = 6.0;

/// Logical (96-dpi) size of the two panes. The user resizes them by dragging the edges; the
/// values are persisted in settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dims {
    pub pinned_w: f32,
    pub main_w: f32,
    pub h: f32,
}

impl Dims {
    pub fn from_settings(s: &super::settings::Settings) -> Dims {
        Dims { pinned_w: s.pinned_w as f32, main_w: s.main_w as f32, h: s.pane_h as f32 }
    }
}

impl Default for Dims {
    fn default() -> Dims {
        use super::settings::*;
        Dims { pinned_w: PINNED_W_DEFAULT as f32, main_w: MAIN_W_DEFAULT as f32, h: PANE_H_DEFAULT as f32 }
    }
}

/// Which edge of a pane is being dragged. The top edge is not resizable (the position is).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Zone {
    E,
    S,
    SE,
    W,
    SW,
}

/// The resize grip under client point `(x, y)` of a `w` x `h` px pane, if any. The main pane
/// grows to the right, the pinned pane to the left; both share the bottom edge.
pub fn zone_at(pid: PaneId, w: f32, h: f32, x: f32, y: f32, grip: f32) -> Option<Zone> {
    let bottom = y >= h - grip;
    let right = pid == PaneId::Main && x >= w - grip;
    let left = pid == PaneId::Pinned && x < grip;
    match (left, right, bottom) {
        (_, true, true) => Some(Zone::SE),
        (true, _, true) => Some(Zone::SW),
        (_, true, false) => Some(Zone::E),
        (true, _, false) => Some(Zone::W),
        (_, _, true) => Some(Zone::S),
        _ => None,
    }
}

/// Largest logical sizes that still fit the monitor's work area from the pair's anchor.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_pinned_w: f32,
    pub max_main_w: f32,
    pub max_h: f32,
}

/// New dimensions after dragging `zone` by `(dx, dy)` logical px from `start`.
pub fn resized(zone: Zone, start: Dims, dx: f32, dy: f32, lim: &Limits) -> Dims {
    use super::settings::*;
    // The upper bound is the monitor limit, but never less than the size the drag started from:
    // a pair already hanging over an edge must not snap smaller the moment the mouse moves.
    let clamp = |v: f32, range: (u32, u32), max: f32, cur: f32| {
        let hi = max.max(cur).max(range.0 as f32).min(range.1 as f32);
        v.clamp(range.0 as f32, hi)
    };
    let mut d = start;
    if matches!(zone, Zone::E | Zone::SE) {
        d.main_w = clamp(start.main_w + dx, MAIN_W_RANGE, lim.max_main_w, start.main_w);
    }
    if matches!(zone, Zone::W | Zone::SW) {
        d.pinned_w = clamp(start.pinned_w - dx, PINNED_W_RANGE, lim.max_pinned_w, start.pinned_w);
    }
    if matches!(zone, Zone::S | Zone::SE | Zone::SW) {
        d.h = clamp(start.h + dy, PANE_H_RANGE, lim.max_h, start.h);
    }
    d
}

/// Shrinks `d` so the pair (both panes plus the gap) fits `avail_w` x `avail_h` logical px,
/// cutting the main pane first and never going below the minimum sizes.
pub fn fit(d: Dims, avail_w: f32, avail_h: f32) -> Dims {
    use super::settings::*;
    let mut d = d;
    let over = d.pinned_w + d.main_w - avail_w;
    if over > 0.0 {
        let cut_main = over.min(d.main_w - MAIN_W_RANGE.0 as f32).max(0.0);
        d.main_w -= cut_main;
        let rest = over - cut_main;
        if rest > 0.0 {
            d.pinned_w = (d.pinned_w - rest).max(PINNED_W_RANGE.0 as f32);
        }
    }
    d.h = d.h.min(avail_h).max(PANE_H_RANGE.0 as f32);
    d
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    History,
    Snippets,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaneId {
    Main = 0,
    Pinned = 1,
}

impl PaneId {
    pub fn other(self) -> PaneId {
        match self {
            PaneId::Main => PaneId::Pinned,
            PaneId::Pinned => PaneId::Main,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Btn {
    Paste,
    CleanUrl,
    Plain,
    Edit,
    Merge,
}

#[derive(Clone)]
pub struct Row {
    /// Index into the store's items (or the snippet list).
    pub src: usize,
    /// Item id (snippet index for snippets).
    pub id: u64,
    /// Original 1-based number in the unfiltered pane list.
    pub number: usize,
}

/// What the expanded card shows for the selected item (computed once per selection).
pub struct CardCache {
    pub id: u64,
    pub lines: Vec<String>,
    pub total_lines: usize,
    pub n_files: usize,
    pub is_url: bool,
    pub has_text: bool,
}

pub struct Pane {
    pub id: PaneId,
    pub hwnd: HWND,
    pub edit: HWND,
    pub query: String,
    pub rows: Vec<Row>,
    pub sel: Option<usize>,
    pub sel_id: Option<u64>,
    pub anchor: Option<usize>,
    pub multi: BTreeSet<u64>,
    pub scroll: f32,
    pub hover: Option<usize>,
    pub hover_btn: Option<Btn>,
    pub number_buf: String,
    pub number_target: Option<u64>,
    pub card: Option<CardCache>,
    pub thumbs_asked: HashSet<u64>,
    pub tracking: bool,
}

impl Pane {
    fn new(id: PaneId) -> Pane {
        Pane {
            id,
            hwnd: HWND::default(),
            edit: HWND::default(),
            query: String::new(),
            rows: Vec::new(),
            sel: None,
            sel_id: None,
            anchor: None,
            multi: BTreeSet::new(),
            scroll: 0.0,
            hover: None,
            hover_btn: None,
            number_buf: String::new(),
            number_target: None,
            card: None,
            thumbs_asked: HashSet::new(),
            tracking: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DragKind {
    Move,
    Resize(Zone),
}

pub struct Drag {
    pub kind: DragKind,
    pub start_cursor: POINT,
    pub start_main: POINT,
    pub start_dims: Dims,
    pub limits: Limits,
    pub moved: bool,
}

pub struct State {
    pub panes: [Pane; 2],
    pub scope: Scope,
    pub focus: PaneId,
    pub visible: bool,
    pub prev_fg: HWND,
    pub acquired: bool,
    pub acquire_deadline: Option<Instant>,
    pub scale: f32,
    pub dims: Dims,
    pub metrics: Metrics,
    pub pal: Palette,
    pub expand: bool,
    pub sheet: bool,
    pub drag: Option<Drag>,
    pub font_edit: HFONT,
    pub brush_field: HBRUSH,
    pub dirty: bool,
    pub rounded_via_region: bool,
    /// (history items, snippets) for the scope control labels.
    pub counts: (usize, usize),
    /// When the overlay was last shown, to log hotkey -> first frame latency.
    pub shown_at: Option<Instant>,
    /// Hover-preview popup state.
    pub preview: Preview,
}

pub struct Overlay {
    pub st: RefCell<State>,
    pub gfx: [RefCell<Option<Gfx>>; 2],
    /// Render surface (and bitmap cache) of the hover-preview popup.
    pub gfx_preview: RefCell<Option<Gfx>>,
}

fn pane_size(scale: f32, dims: &Dims, id: PaneId) -> (i32, i32) {
    let w = if id == PaneId::Main { dims.main_w } else { dims.pinned_w };
    ((w * scale).round() as i32, (dims.h * scale).round() as i32)
}

pub fn monitor_scale(hmon: HMONITOR) -> f32 {
    // Test hook (sandbox profiles only): lets layout be checked at 150% / 200% on a 100% display.
    if super::util::profile().is_some() {
        if let Some(f) = std::env::var("CLIP4_FORCE_SCALE").ok().and_then(|v| v.parse::<f32>().ok()) {
            return f.clamp(0.5, 4.0);
        }
    }
    let (mut x, mut y) = (96u32, 96u32);
    // SAFETY: out params are valid.
    if unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut x, &mut y) }.is_err() || x == 0 {
        return 1.0;
    }
    x as f32 / 96.0
}

/// A hash of the monitor layout; the dragged position is only trusted under the same layout.
fn monitor_cfg_hash() -> u32 {
    unsafe extern "system" fn cb(_: HMONITOR, _: HDC, r: *mut RECT, lp: LPARAM) -> windows::core::BOOL {
        // SAFETY: lp is &mut u32 passed below; r is valid for the callback.
        let acc = unsafe { &mut *(lp.0 as *mut u32) };
        let r = unsafe { &*r };
        let mut h = 17u32;
        for v in [r.left, r.top, r.right, r.bottom] {
            h = h.wrapping_mul(31).wrapping_add(v as u32);
        }
        // Summed, so the result does not depend on the order Windows enumerates monitors in.
        *acc = acc.wrapping_add(h);
        true.into()
    }
    let mut h = 17u32;
    // SAFETY: callback only touches `h`.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut h as *mut u32 as isize));
    }
    h
}

impl Overlay {
    pub fn new() -> Overlay {
        let metrics = Metrics::new(14, 16, 1.0);
        let pal = theme::palette(0, &Overrides::default());
        Overlay {
            st: RefCell::new(State {
                panes: [Pane::new(PaneId::Main), Pane::new(PaneId::Pinned)],
                scope: Scope::History,
                focus: PaneId::Main,
                visible: false,
                prev_fg: HWND::default(),
                acquired: false,
                acquire_deadline: None,
                scale: 1.0,
                dims: Dims::default(),
                metrics,
                pal,
                expand: true,
                sheet: false,
                drag: None,
                font_edit: HFONT::default(),
                brush_field: HBRUSH::default(),
                dirty: true,
                rounded_via_region: false,
                counts: (0, 0),
                shown_at: None,
                preview: Preview::new(),
            }),
            gfx: [RefCell::new(None), RefCell::new(None)],
            gfx_preview: RefCell::new(None),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.st.try_borrow().map(|s| s.visible).unwrap_or(false)
    }

    pub fn target(&self) -> HWND {
        self.st.borrow().prev_fg
    }

    pub fn pre_translate(&self, _msg: &MSG) -> bool {
        false
    }

    // ---------------- creation ----------------

    pub fn create(&self, app: &App) {
        // SAFETY: class and windows are created on the UI thread.
        unsafe {
            let wc = WNDCLASSW {
                style: CS_DBLCLKS | CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(input::pane_proc),
                hInstance: app.hinst,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                lpszClassName: PANE_CLASS,
                ..Default::default()
            };
            RegisterClassW(&wc);
            for id in [PaneId::Pinned, PaneId::Main] {
                let ex = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | if id == PaneId::Pinned { WS_EX_NOACTIVATE } else { WINDOW_EX_STYLE(0) };
                let (pw, ph) = pane_size(1.0, &Dims::default(), id);
                let hwnd = CreateWindowExW(ex, PANE_CLASS, w!("clip4"), WS_POPUP | WS_CLIPCHILDREN, 0, 0, pw, ph, None, None, Some(app.hinst), None).unwrap_or_default();
                let edit = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("EDIT"),
                    PCWSTR::null(),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                    0,
                    0,
                    10,
                    10,
                    Some(hwnd),
                    None,
                    Some(app.hinst),
                    None,
                )
                .unwrap_or_default();
                let cue = wide(if id == PaneId::Main { "Search" } else { "Search pinned" });
                SendMessageW(edit, 0x1501, Some(WPARAM(1)), Some(LPARAM(cue.as_ptr() as isize))); // EM_SETCUEBANNER
                input::subclass_edit(edit, id);
                self.round_corners(hwnd);
                let mut st = self.st.borrow_mut();
                st.panes[id as usize].hwnd = hwnd;
                st.panes[id as usize].edit = edit;
            }
        }
        self.create_preview_window(app);
        self.apply_look(app);
        self.prewarm(app);
    }

    fn round_corners(&self, hwnd: HWND) {
        // Windows 11: DWMWCP_ROUND. Windows 10 rejects the attribute; a rounded region is the fallback.
        let pref: i32 = 2;
        // SAFETY: attribute value is a valid i32.
        let r = unsafe { DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &pref as *const _ as *const _, 4) };
        if r.is_err() {
            if let Ok(mut st) = self.st.try_borrow_mut() {
                st.rounded_via_region = true;
            }
        }
    }

    fn apply_region(&self, hwnd: HWND, scale: f32) {
        if !self.st.borrow().rounded_via_region {
            return;
        }
        let mut r = RECT::default();
        // SAFETY: region ownership passes to the window.
        unsafe {
            let _ = GetClientRect(hwnd, &mut r);
            let d = (22.0 * scale) as i32;
            let rgn = CreateRoundRectRgn(0, 0, r.right + 1, r.bottom + 1, d, d);
            SetWindowRgn(hwnd, Some(rgn), true);
        }
    }

    /// Re-derives metrics, palette, fonts and child-control geometry from settings + scale.
    pub fn apply_look(&self, app: &App) {
        let s = app.settings.borrow().clone();
        let scale = self.st.borrow().scale;
        let (metrics, pal) = {
            let m = Metrics::new(s.content_size, s.ui_size, scale);
            (m, theme::palette(s.theme_id, &Overrides::from_registry(s.colors)))
        };
        let (content_px, ui_px) = (metrics.content, metrics.ui);
        // Fonts for both render targets.
        let mut ui_face = String::from("Segoe UI");
        for g in &self.gfx {
            let mut slot = g.borrow_mut();
            if slot.is_none() {
                *slot = Gfx::new();
            }
            if let Some(gf) = slot.as_mut() {
                gf.set_fonts(&s.font_face, content_px, ui_px);
                gf.drop_bitmaps();
                ui_face = gf.ui_face().to_string();
            }
        }
        // GDI font + background brush for the EDIT controls.
        let face = wide(&ui_face);
        // SAFETY: GDI object creation; the previous objects are deleted after the swap.
        let (new_font, new_brush) = unsafe {
            let f = CreateFontW(-(ui_px.round() as i32), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, pcw(&face));
            let b = CreateSolidBrush(COLORREF(pal.surface_field.colorref()));
            (f, b)
        };
        let (old_font, old_brush, edits) = {
            let mut st = self.st.borrow_mut();
            st.metrics = metrics;
            st.pal = pal;
            st.dims = Dims::from_settings(&s);
            st.expand = s.expand_selected;
            let o = (st.font_edit, st.brush_field, [st.panes[0].edit, st.panes[1].edit]);
            st.font_edit = new_font;
            st.brush_field = new_brush;
            o
        };
        // SAFETY: set font on controls, then free the old GDI objects.
        unsafe {
            for e in edits {
                if !e.0.is_null() {
                    SendMessageW(e, WM_SETFONT, Some(WPARAM(new_font.0 as usize)), Some(LPARAM(1)));
                }
            }
            if !old_font.0.is_null() {
                let _ = DeleteObject(old_font.into());
            }
            if !old_brush.0.is_null() {
                let _ = DeleteObject(old_brush.into());
            }
        }
        self.resize_windows(app);
        self.layout_edits();
        self.invalidate_all();
    }

    fn resize_windows(&self, _app: &App) {
        let (scale, dims, hw): (f32, Dims, [HWND; 2]) = {
            let st = self.st.borrow();
            (st.scale, st.dims, [st.panes[0].hwnd, st.panes[1].hwnd])
        };
        for (i, id) in [PaneId::Main, PaneId::Pinned].into_iter().enumerate() {
            if hw[i].0.is_null() {
                continue;
            }
            let (pw, ph) = pane_size(scale, &dims, id);
            // SAFETY: plain resize without moving/activating.
            unsafe {
                let _ = SetWindowPos(hw[i], None, 0, 0, pw, ph, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
            self.apply_region(hw[i], scale);
        }
    }

    /// Positions the EDIT controls inside the painted search pills.
    pub(crate) fn layout_edits(&self) {
        for pid in [PaneId::Main, PaneId::Pinned] {
            let (edit, rect, hide) = {
                let st = self.st.borrow();
                let p = &st.panes[pid as usize];
                if p.edit.0.is_null() {
                    continue;
                }
                let gfx = self.gfx[pid as usize].borrow();
                let Some(gf) = gfx.as_ref() else { continue };
                let g = paint::geo(&st, pid, gf);
                (p.edit, g.edit_rect, !p.number_buf.is_empty())
            };
            // SAFETY: moving a child control; SetWindowPos may re-enter, no borrows held.
            unsafe {
                let _ = SetWindowPos(edit, None, rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, SWP_NOZORDER | SWP_NOACTIVATE);
                let _ = ShowWindow(edit, if hide { SW_HIDE } else { SW_SHOWNA });
            }
        }
    }

    pub fn invalidate_all(&self) {
        let hw = {
            let st = self.st.borrow();
            [st.panes[0].hwnd, st.panes[1].hwnd]
        };
        for h in hw {
            if !h.0.is_null() {
                // SAFETY: invalidate the whole client area.
                unsafe {
                    let _ = InvalidateRect(Some(h), None, false);
                }
            }
        }
    }

    pub fn invalidate(&self, pid: PaneId) {
        let h = self.st.borrow().panes[pid as usize].hwnd;
        if !h.0.is_null() {
            // SAFETY: as above.
            unsafe {
                let _ = InvalidateRect(Some(h), None, false);
            }
        }
    }

    // ---------------- show / hide ----------------

    pub fn toggle(&self, app: &App, scope: Scope) {
        let (vis, cur) = {
            let st = self.st.borrow();
            (st.visible, st.scope)
        };
        if vis && cur == scope {
            self.hide(app);
        } else {
            self.show(app, scope);
        }
    }

    pub fn show(&self, app: &App, scope: Scope) {
        crate::log_dbg!("overlay show ({scope:?})");
        // Remember where the user was (unless that is us).
        // SAFETY: plain queries.
        let fg = unsafe { GetForegroundWindow() };
        let (m, p) = {
            let st = self.st.borrow();
            (st.panes[0].hwnd, st.panes[1].hwnd)
        };
        let was_visible = self.st.borrow().visible;
        if fg != m && fg != p && !fg.0.is_null() {
            self.st.borrow_mut().prev_fg = fg;
        }
        crate::log_dbg!("overlay show: previous window {:?} ({})", self.st.borrow().prev_fg.0, super::clipboard::process_name_of_window(self.st.borrow().prev_fg));
        // Where to open: the remembered position (while the monitor layout is unchanged) wins; it
        // lives on ITS OWN monitor, whichever window the user happens to be working in. Otherwise
        // centre on the monitor of that window.
        let prev = self.st.borrow().prev_fg;
        let saved = Self::saved_position(app);
        // SAFETY: monitor queries.
        let hmon = unsafe {
            if let Some((sx, sy)) = saved {
                MonitorFromPoint(POINT { x: sx + 1, y: sy + 1 }, MONITOR_DEFAULTTONEAREST)
            } else if prev.0.is_null() {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY)
            } else {
                MonitorFromWindow(prev, MONITOR_DEFAULTTONEAREST)
            }
        };
        let scale = monitor_scale(hmon);
        if (scale - self.st.borrow().scale).abs() > 0.001 {
            self.st.borrow_mut().scale = scale;
            self.apply_look(app);
        }
        {
            let mut st = self.st.borrow_mut();
            st.scope = scope;
            st.focus = PaneId::Main;
            st.sheet = false;
            st.acquired = false;
            st.acquire_deadline = Some(Instant::now() + Duration::from_secs(3));
            for p in st.panes.iter_mut() {
                p.query.clear();
                p.number_buf.clear();
                p.number_target = None;
                p.multi.clear();
                p.scroll = 0.0;
                p.sel = None;
                p.sel_id = None;
                p.hover = None;
            }
        }
        self.clear_edits();
        self.fit_to_monitor(hmon);
        self.rebuild_all(app);
        self.place_pair(app, hmon);
        self.layout_edits();
        {
            let mut st = self.st.borrow_mut();
            st.visible = true;
            st.shown_at = Some(Instant::now());
        }
        // SAFETY: show windows; the pinned pane is shown WITHOUT activation so focus stays on the list.
        unsafe {
            let _ = ShowWindow(p, SW_SHOWNOACTIVATE);
            let _ = ShowWindow(m, SW_SHOW);
            let _ = SetTimer(Some(m), T_ACQUIRE, 50, None);
            let _ = SetTimer(Some(m), T_AGES, 1000, None);
            let _ = SetTimer(Some(m), T_FOCUSPOLL, 250, None);
        }
        input::take_foreground(m);
        // SAFETY: plain query. Usually we already own the foreground here; no need to wait for a tick.
        if unsafe { GetForegroundWindow() } == m {
            self.st.borrow_mut().acquired = true;
        }
        self.invalidate_all();
        let _ = was_visible;
    }

    pub fn hide(&self, _app: &App) {
        crate::log_dbg!("overlay hide (visible={})", self.is_visible());
        let (m, p, vis) = {
            let st = self.st.borrow();
            (st.panes[0].hwnd, st.panes[1].hwnd, st.visible)
        };
        if !vis {
            return;
        }
        {
            let mut st = self.st.borrow_mut();
            st.visible = false;
            st.sheet = false;
            st.drag = None;
        }
        self.clear_preview();
        // SAFETY: hide + stop timers (nothing runs while idle).
        unsafe {
            let _ = KillTimer(Some(m), T_ACQUIRE);
            let _ = KillTimer(Some(m), T_AGES);
            let _ = KillTimer(Some(m), T_FOCUSLOSS);
            let _ = KillTimer(Some(m), T_FOCUSPOLL);
            let _ = ShowWindow(p, SW_HIDE);
            let _ = ShowWindow(m, SW_HIDE);
        }
    }

    fn clear_edits(&self) {
        let edits = {
            let st = self.st.borrow();
            [st.panes[0].edit, st.panes[1].edit]
        };
        for e in edits {
            // SAFETY: EN_CHANGE fires synchronously; no borrow is held and query is already empty.
            unsafe {
                let _ = SetWindowTextW(e, w!(""));
            }
        }
    }

    /// A remembered size from a bigger screen must not overflow this monitor.
    fn fit_to_monitor(&self, hmon: HMONITOR) {
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        // SAFETY: monitor info query.
        unsafe {
            let _ = GetMonitorInfoW(hmon, &mut mi);
        }
        let mut st = self.st.borrow_mut();
        let (w, h) = ((mi.rcWork.right - mi.rcWork.left) as f32 / st.scale, (mi.rcWork.bottom - mi.rcWork.top) as f32 / st.scale);
        // Leave room for the gap and the 50 px offset from the top of the work area.
        st.dims = fit(st.dims, w - GAP, h - 50.0);
    }

    /// Centres the pair on `hmon`'s work area (or restores the dragged position), clamped.
    fn place_pair(&self, app: &App, hmon: HMONITOR) {
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        // SAFETY: monitor info query.
        unsafe {
            let _ = GetMonitorInfoW(hmon, &mut mi);
        }
        let work = mi.rcWork;
        let (scale, dims, hw) = {
            let st = self.st.borrow();
            (st.scale, st.dims, [st.panes[0].hwnd, st.panes[1].hwnd])
        };
        let (mw, mh) = pane_size(scale, &dims, PaneId::Main);
        let (pw, ph) = pane_size(scale, &dims, PaneId::Pinned);
        let gap = (GAP * scale).round() as i32;
        let total_w = pw + gap + mw;
        let mut x = work.left + ((work.right - work.left) - total_w) / 2;
        let mut y = work.top + (50.0 * scale).round() as i32;
        if let Some((sx, sy)) = Self::saved_position(app) {
            // The stored position is that of the MAIN pane; the pinned pane sits to its left.
            x = sx - pw - gap;
            y = sy;
        }
        x = x.clamp(work.left, (work.right - total_w).max(work.left));
        y = y.clamp(work.top, (work.bottom - mh.max(ph)).max(work.top));
        // SAFETY: position both windows (topmost, no activation).
        unsafe {
            let _ = SetWindowPos(hw[1], Some(HWND_TOPMOST), x, y, pw, ph, SWP_NOACTIVATE);
            let _ = SetWindowPos(hw[0], Some(HWND_TOPMOST), x + pw + gap, y, mw, mh, SWP_NOACTIVATE);
        }
        self.apply_region(hw[0], scale);
        self.apply_region(hw[1], scale);
    }

    /// Moves the pair so the main pane's top-left is at `(mx, my)`.
    pub(crate) fn move_pair(&self, mx: i32, my: i32) {
        let (scale, dims, hw) = {
            let st = self.st.borrow();
            (st.scale, st.dims, [st.panes[0].hwnd, st.panes[1].hwnd])
        };
        let (pw, _) = pane_size(scale, &dims, PaneId::Pinned);
        let gap = (GAP * scale).round() as i32;
        // SAFETY: move without resizing/activating.
        unsafe {
            let _ = SetWindowPos(hw[1], None, mx - pw - gap, my, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            let _ = SetWindowPos(hw[0], None, mx, my, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    pub fn on_display_change(&self, app: &App) {
        if self.is_visible() {
            self.hide(app);
        }
    }

    /// The remembered top-left of the MAIN pane, if it is still usable (see below).
    fn saved_position(app: &App) -> Option<(i32, i32)> {
        let s = app.settings.borrow();
        let (x, y) = s.overlay_pos?;
        // Under the same monitor layout the spot is trusted outright. After a layout change (dock,
        // undock) it is still honoured as long as it lies on a monitor that is connected now.
        // SAFETY: plain query.
        let on_screen = unsafe { !MonitorFromPoint(POINT { x: x + 1, y: y + 1 }, MONITOR_DEFAULTTONULL).0.is_null() };
        (s.overlay_pos_cfg == monitor_cfg_hash() || on_screen).then_some((x, y))
    }

    /// Persists where the overlay is and how big it is (after a drag or a resize).
    pub(crate) fn remember_position(&self, app: &App) {
        let (m, dims) = {
            let st = self.st.borrow();
            (st.panes[0].hwnd, st.dims)
        };
        let mut r = RECT::default();
        // SAFETY: window rect query.
        if unsafe { GetWindowRect(m, &mut r) }.is_ok() {
            let mut s = app.settings.borrow_mut();
            s.overlay_pos = Some((r.left, r.top));
            s.overlay_pos_cfg = monitor_cfg_hash();
            s.pinned_w = dims.pinned_w.round() as u32;
            s.main_w = dims.main_w.round() as u32;
            s.pane_h = dims.h.round() as u32;
            s.save_pos_only();
        }
    }

    // ---------------- drag: move and resize ----------------

    /// The resize edge under a client point of pane `pid`, if any.
    pub(crate) fn zone_under(&self, pid: PaneId, x: f32, y: f32) -> Option<Zone> {
        let st = self.st.borrow();
        let (w, h) = st.pane_px(pid);
        zone_at(pid, w as f32, h as f32, x, y, GRIP * st.scale)
    }

    /// Starts a mouse-capture drag (move, or resize of one edge) from the pane window `pid`.
    pub(crate) fn begin_drag(&self, pid: PaneId, kind: DragKind) {
        let (main, src, dims, scale) = {
            let st = self.st.borrow();
            (st.panes[0].hwnd, st.panes[pid as usize].hwnd, st.dims, st.scale)
        };
        let (mut cur, mut r) = (POINT::default(), RECT::default());
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        // SAFETY: cursor / window / monitor queries, then mouse capture for the drag.
        unsafe {
            let _ = GetCursorPos(&mut cur);
            let _ = GetWindowRect(main, &mut r);
            let _ = GetMonitorInfoW(MonitorFromWindow(main, MONITOR_DEFAULTTONEAREST), &mut mi);
            SetCapture(src);
        }
        // Sizes may grow until the pair reaches the edge of its monitor's work area.
        let gap = GAP * scale;
        let work = mi.rcWork;
        let limits = Limits {
            max_main_w: (work.right - r.left) as f32 / scale,
            max_pinned_w: ((r.left - work.left) as f32 - gap) / scale,
            max_h: (work.bottom - r.top) as f32 / scale,
        };
        self.st.borrow_mut().drag = Some(Drag { kind, start_cursor: cur, start_main: POINT { x: r.left, y: r.top }, start_dims: dims, limits, moved: false });
    }

    /// Applies the current cursor position to the active drag.
    pub(crate) fn drag_to(&self, app: &App) {
        let mut cur = POINT::default();
        // SAFETY: cursor query.
        unsafe {
            let _ = GetCursorPos(&mut cur);
        }
        let (kind, start_cursor, start_main, start_dims, limits, scale) = {
            let mut st = self.st.borrow_mut();
            let scale = st.scale;
            let Some(d) = st.drag.as_mut() else { return };
            d.moved = true;
            (d.kind, d.start_cursor, d.start_main, d.start_dims, d.limits, scale)
        };
        let (dx, dy) = (cur.x - start_cursor.x, cur.y - start_cursor.y);
        match kind {
            DragKind::Move => self.move_pair(start_main.x + dx, start_main.y + dy),
            DragKind::Resize(zone) => {
                let new = resized(zone, start_dims, dx as f32 / scale, dy as f32 / scale, &limits);
                if new != self.st.borrow().dims {
                    self.st.borrow_mut().dims = new;
                    {
                        // Keep settings in step so an interleaved apply_look cannot undo the drag.
                        let mut s = app.settings.borrow_mut();
                        s.pinned_w = new.pinned_w.round() as u32;
                        s.main_w = new.main_w.round() as u32;
                        s.pane_h = new.h.round() as u32;
                    }
                    self.apply_dims(app);
                }
            }
        }
    }

    /// Re-lays out both windows for the current `dims`; the main pane's top-left stays put.
    fn apply_dims(&self, app: &App) {
        let (scale, dims, hw) = {
            let st = self.st.borrow();
            (st.scale, st.dims, [st.panes[0].hwnd, st.panes[1].hwnd])
        };
        let mut r = RECT::default();
        // SAFETY: window rect query.
        if unsafe { GetWindowRect(hw[0], &mut r) }.is_err() {
            return;
        }
        let (mw, mh) = pane_size(scale, &dims, PaneId::Main);
        let (pw, ph) = pane_size(scale, &dims, PaneId::Pinned);
        let gap = (GAP * scale).round() as i32;
        // SAFETY: resize + reposition both windows without activating them.
        unsafe {
            let _ = SetWindowPos(hw[0], None, r.left, r.top, mw, mh, SWP_NOZORDER | SWP_NOACTIVATE);
            let _ = SetWindowPos(hw[1], None, r.left - gap - pw, r.top, pw, ph, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        self.apply_region(hw[0], scale);
        self.apply_region(hw[1], scale);
        {
            // A different height changes how many rows fit: keep the selection on screen.
            let store = app.store.borrow();
            let mut st = self.st.borrow_mut();
            st.ensure_visible(PaneId::Main, &store);
            st.ensure_visible(PaneId::Pinned, &store);
        }
        self.layout_edits();
        self.invalidate_all();
    }

    /// Cursor to show for the current mouse position / drag (None = leave the default arrow).
    pub(crate) fn resize_cursor(&self, pid: PaneId, hwnd: HWND) -> Option<PCWSTR> {
        let dragging = self.st.borrow().drag.as_ref().map(|d| d.kind);
        let zone = match dragging {
            Some(DragKind::Resize(z)) => Some(z),
            Some(DragKind::Move) => None,
            None => {
                let mut pt = POINT::default();
                // SAFETY: cursor query + conversion to the pane's client coordinates.
                unsafe {
                    let _ = GetCursorPos(&mut pt);
                    let _ = ScreenToClient(hwnd, &mut pt);
                }
                self.zone_under(pid, pt.x as f32, pt.y as f32)
            }
        };
        zone.map(|z| match z {
            Zone::E | Zone::W => IDC_SIZEWE,
            Zone::S => IDC_SIZENS,
            Zone::SE => IDC_SIZENWSE,
            Zone::SW => IDC_SIZENESW,
        })
    }

    // ---------------- data / views ----------------

    pub fn refresh(&self, app: &App) {
        if !self.is_visible() {
            self.st.borrow_mut().dirty = true;
            return;
        }
        self.rebuild_all(app);
        self.invalidate_all();
    }

    pub fn rebuild_all(&self, app: &App) {
        let store = app.store.borrow();
        let snippets = app.snippets.borrow();
        let mut st = self.st.borrow_mut();
        st.dirty = false;
        st.counts = (store.len() - store.pinned_count(), snippets.len());
        st.rebuild_view(PaneId::Main, &store, &snippets);
        st.rebuild_view(PaneId::Pinned, &store, &snippets);
        st.ensure_visible(PaneId::Main, &store);
        st.ensure_visible(PaneId::Pinned, &store);
    }

    pub(crate) fn rebuild_pane(&self, app: &App, pid: PaneId) {
        let store = app.store.borrow();
        let snippets = app.snippets.borrow();
        let mut st = self.st.borrow_mut();
        st.rebuild_view(pid, &store, &snippets);
        st.ensure_visible(pid, &store);
    }

    /// Items currently selected in the focused pane, in display order.
    pub fn selected_items(&self, app: &App) -> Vec<Item> {
        let st = self.st.borrow();
        let store = app.store.borrow();
        let p = &st.panes[st.focus as usize];
        if p.id == PaneId::Main && st.scope == Scope::Snippets {
            return Vec::new();
        }
        let ids = p.selection_ids();
        ids.into_iter().filter_map(|id| store.find(id).cloned()).collect()
    }

    /// The snippet selected in the main pane (Snippets scope).
    pub fn selected_snippet(&self, app: &App) -> Option<(usize, Snippet)> {
        let st = self.st.borrow();
        if st.scope != Scope::Snippets {
            return None;
        }
        let p = &st.panes[0];
        let i = p.sel.and_then(|i| p.rows.get(i)).map(|r| r.src)?;
        app.snippets.borrow().get(i).cloned().map(|s| (i, s))
    }

    pub fn main_hwnd(&self) -> HWND {
        self.st.borrow().panes[0].hwnd
    }

    /// Selects one item by id in the main pane (clears multi-selection).
    pub fn select_item(&self, app: &App, id: u64) {
        {
            let store = app.store.borrow();
            let snippets = app.snippets.borrow();
            let mut st = self.st.borrow_mut();
            let p = &mut st.panes[0];
            if let Some(i) = p.rows.iter().position(|r| r.id == id) {
                p.sel = Some(i);
                p.sel_id = Some(id);
                p.anchor = Some(i);
                p.multi.clear();
            }
            st.refresh_card(PaneId::Main, &store, &snippets);
            st.ensure_visible(PaneId::Main, &store);
        }
        self.invalidate_all();
    }

    pub fn scope(&self) -> Scope {
        self.st.borrow().scope
    }

    pub fn on_thumb(&self, app: &App, id: u64, w: u32, h: u32, bgra: &[u8]) {
        if id & PREVIEW_FLAG != 0 {
            self.on_preview_thumb(app, id & !PREVIEW_FLAG, w, h, bgra);
            return;
        }
        for g in &self.gfx {
            if let Some(gf) = g.borrow_mut().as_mut() {
                gf.put_bitmap(id, w, h, bgra);
            }
        }
        // Arrived: forget the request so a later device reset can ask again.
        for p in self.st.borrow_mut().panes.iter_mut() {
            p.thumbs_asked.remove(&id);
        }
        self.invalidate_all();
    }
}

impl Default for Overlay {
    fn default() -> Self {
        Overlay::new()
    }
}

impl Pane {
    /// Selected item ids in view (top-to-bottom) order.
    pub fn selection_ids(&self) -> Vec<u64> {
        if self.multi.len() >= 2 {
            return self.rows.iter().filter(|r| self.multi.contains(&r.id)).map(|r| r.id).collect();
        }
        if let Some(id) = self.sel.and_then(|i| self.rows.get(i)).map(|r| r.id) {
            return vec![id];
        }
        self.number_target.into_iter().collect()
    }
}

impl State {
    /// Physical size of a pane's window.
    pub fn pane_px(&self, id: PaneId) -> (i32, i32) {
        pane_size(self.scale, &self.dims, id)
    }

    pub fn active_scope(&self, pid: PaneId) -> Scope {
        if pid == PaneId::Main {
            self.scope
        } else {
            Scope::History
        }
    }

    /// Rebuilds one pane's visible list from the store / snippets and the search query.
    pub fn rebuild_view(&mut self, pid: PaneId, store: &Store, snippets: &[Snippet]) {
        let scope = self.active_scope(pid);
        let p = &mut self.panes[pid as usize];
        let mut rows: Vec<Row> = match (pid, scope) {
            (PaneId::Main, Scope::Snippets) => (0..snippets.len()).map(|i| Row { src: i, id: i as u64, number: i + 1 }).collect(),
            (PaneId::Pinned, _) => {
                let mut n = 0;
                store
                    .items()
                    .iter()
                    .enumerate()
                    .filter(|(_, it)| it.pinned)
                    .map(|(i, it)| {
                        n += 1;
                        Row { src: i, id: it.id, number: n }
                    })
                    .collect()
            }
            // Pinned items live in the pinned pane only; the main list holds the rest.
            _ => {
                let mut n = 0;
                store
                    .items()
                    .iter()
                    .enumerate()
                    .filter(|(_, it)| !it.pinned)
                    .map(|(i, it)| {
                        n += 1;
                        Row { src: i, id: it.id, number: n }
                    })
                    .collect()
            }
        };
        let q = search::prepare(&p.query);
        if !q.is_empty() {
            let ranked = if scope == Scope::Snippets && pid == PaneId::Main {
                search::rank_strs(rows.iter().map(|r| snippets.get(r.src).map(|s| s.name.as_str()).unwrap_or("")), &q)
            } else {
                let items = store.items();
                let empty = search::SearchIndex::build("");
                search::rank(rows.iter().map(|r| items.get(r.src).map(|it| &*it.index).unwrap_or(&empty)), &q)
            };
            rows = ranked.into_iter().filter_map(|(pos, _)| rows.get(pos).cloned()).collect();
        }
        // Keep the selection on the same item when possible. If it vanished (deleted, pinned,
        // evicted) stay at the same position instead of jumping to the top; with no remembered
        // selection (fresh show, new query) start at the top.
        let keep = p.sel_id.and_then(|id| rows.iter().position(|r| r.id == id));
        let fallback = match (p.sel_id, p.sel) {
            (Some(_), Some(i)) => Some(i.min(rows.len().saturating_sub(1))),
            _ => Some(0),
        };
        let empty = rows.is_empty();
        p.rows = rows;
        p.sel = keep.or(fallback).filter(|_| !empty);
        p.sel_id = p.sel.and_then(|i| p.rows.get(i)).map(|r| r.id);
        p.anchor = p.sel;
        let alive: HashSet<u64> = p.rows.iter().map(|r| r.id).collect();
        p.multi.retain(|id| alive.contains(id));
        p.hover = None;
        p.card = None;
        self.refresh_card(pid, store, snippets);
    }

    /// Recomputes the expanded-card content for the current selection.
    pub fn refresh_card(&mut self, pid: PaneId, store: &Store, snippets: &[Snippet]) {
        let scope = self.active_scope(pid);
        let p = &mut self.panes[pid as usize];
        let Some(row) = p.sel.and_then(|i| p.rows.get(i)) else {
            p.card = None;
            return;
        };
        if p.card.as_ref().is_some_and(|c| c.id == row.id) {
            return;
        }
        p.card = if pid == PaneId::Main && scope == Scope::Snippets {
            snippets.get(row.src).map(|s| {
                let text = crate::snippets_fmt::plain_for_paste(s);
                make_card(row.id, &text, false)
            })
        } else {
            store.items().get(row.src).map(|it| match it.kind {
                // Four body lines give the thumbnail room to be a real preview.
                Kind::Image => CardCache { id: row.id, lines: vec![it.preview.clone(), String::new(), String::new(), String::new()], total_lines: 1, n_files: 0, is_url: false, has_text: false },
                Kind::Files => {
                    let text = it.text().unwrap_or_else(|| it.preview.clone());
                    let mut c = make_card(row.id, &text, false);
                    c.n_files = c.total_lines;
                    c
                }
                _ => match card_head_unicode(it) {
                    // Big text: decode only what the card shows; count lines on the raw bytes.
                    Some((head, total)) => {
                        let mut c = make_card(row.id, &head, true);
                        c.total_lines = total;
                        c
                    }
                    None => {
                        let text = it.text().unwrap_or_else(|| it.preview.clone());
                        make_card(row.id, &text, it.has_text() || it.kind == Kind::Text)
                    }
                },
            })
        };
    }

    pub fn layout_input<'a>(&'a self, pid: PaneId, card_lines: &'a dyn Fn(usize) -> u32) -> LayoutInput<'a> {
        let p = &self.panes[pid as usize];
        LayoutInput {
            metrics: &self.metrics,
            count: p.rows.len(),
            selected: p.sel,
            expand: self.expand && pid == PaneId::Main,
            card_lines,
        }
    }

    /// Viewport height of a pane's list area (physical px).
    pub fn viewport_h(&self) -> f32 {
        (self.dims.h * self.scale - self.metrics.header_h - self.metrics.footer_h).max(1.0)
    }

    pub fn card_lines_of(&self, pid: PaneId) -> u32 {
        self.panes[pid as usize].card.as_ref().map_or(1, |c| c.lines.len().clamp(1, 4) as u32)
    }

    /// Scrolls just enough that the selected band is present and whole (the card included).
    pub fn ensure_visible(&mut self, pid: PaneId, store: &Store) {
        let _ = store;
        let lines = self.card_lines_of(pid);
        let vh = self.viewport_h();
        let cl = move |_: usize| lines;
        let inp = self.layout_input(pid, &cl);
        let s = layout::ensure_selection_visible(&inp, self.panes[pid as usize].scroll, vh);
        self.panes[pid as usize].scroll = s;
    }
}

/// For a large inline `CF_UNICODETEXT`: (first 8 KB decoded, total line count) without
/// decoding or allocating the whole payload on the UI thread.
fn card_head_unicode(it: &Item) -> Option<(String, usize)> {
    const BIG: usize = 64 * 1024;
    let b = it.payload_std(CF_UNICODETEXT)?.bytes()?;
    if b.len() < BIG {
        return None;
    }
    let head = crate::preview::decode_unicode(&b[..8 * 1024]);
    let lines = 1 + b.chunks_exact(2).take_while(|c| *c != [0, 0]).filter(|c| *c == [10u8, 0]).count();
    Some((head, lines))
}

fn make_card(id: u64, text: &str, has_text: bool) -> CardCache {
    let mut lines: Vec<String> = Vec::new();
    let mut total = 0usize;
    for l in text.lines() {
        total += 1;
        if lines.len() < 4 {
            lines.push(l.replace('\t', "    "));
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
        total = 1;
    }
    CardCache { id, lines, total_lines: total, n_files: 0, is_url: crate::transform::is_single_url(text), has_text }
}

/// The item a typed number refers to in pane `pid`: numbers count the rows of that pane's
/// own unfiltered list (main = unpinned items, pinned = pinned items), newest first, from 1.
pub fn nth_item_id(store: &Store, pid: PaneId, n: usize) -> Option<u64> {
    let want_pinned = pid == PaneId::Pinned;
    store.items().iter().filter(|i| i.pinned == want_pinned).nth(n.checked_sub(1)?).map(|i| i.id)
}

pub fn age_text(unix_ms: i64) -> String {
    let secs = ((super::util::now_unix_ms() - unix_ms) / 1000).max(0);
    match secs {
        0..=4 => "now".into(),
        5..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        86400..=604799 => format!("{}d", secs / 86400),
        _ => format!("{}w", secs / 604800),
    }
}

/// Single-line version of a preview for a list row.
pub fn flatten(s: &str) -> String {
    s.chars().map(|c| if matches!(c, '\r' | '\n' | '\t') { ' ' } else { c }).collect()
}


#[cfg(test)]
mod tests {
    use super::*;

    fn text_item(id: u64, text: &str, pinned: bool) -> Item {
        let bytes: Vec<u8> = text.encode_utf16().chain(std::iter::once(0)).flat_map(|u| u.to_le_bytes()).collect();
        Item::new(id, 1_000 + id as i64, pinned, vec![(FormatKey::Standard(CF_UNICODETEXT), Payload::inline(bytes))])
    }

    fn store_of(texts: &[(&str, bool)]) -> Store {
        let mut s = Store::new(300);
        // `load` takes newest first.
        s.load(texts.iter().enumerate().map(|(i, (t, p))| text_item(100 - i as u64, t, *p)).collect());
        s
    }

    #[test]
    fn pinned_items_are_only_in_the_pinned_pane() {
        let store = store_of(&[("alpha", false), ("beta", true), ("gamma", false), ("delta", true)]);
        let ov = Overlay::new();
        let mut st = ov.st.borrow_mut();
        st.rebuild_view(PaneId::Main, &store, &[]);
        st.rebuild_view(PaneId::Pinned, &store, &[]);
        let main: Vec<(usize, usize)> = st.panes[0].rows.iter().map(|r| (r.src, r.number)).collect();
        assert_eq!(main, [(0, 1), (2, 2)], "main list: unpinned only, numbered 1..");
        let pinned: Vec<(usize, usize)> = st.panes[1].rows.iter().map(|r| (r.src, r.number)).collect();
        assert_eq!(pinned, [(1, 1), (3, 2)], "pinned pane: pinned only, numbered 1..");
    }

    #[test]
    fn typed_numbers_count_the_rows_of_their_own_pane() {
        let store = store_of(&[("alpha", false), ("beta", true), ("gamma", false), ("delta", true)]);
        let id = |i: usize| store.items()[i].id;
        assert_eq!(nth_item_id(&store, PaneId::Main, 1), Some(id(0)));
        assert_eq!(nth_item_id(&store, PaneId::Main, 2), Some(id(2)), "gamma is #2 of the main list");
        assert_eq!(nth_item_id(&store, PaneId::Main, 3), None);
        assert_eq!(nth_item_id(&store, PaneId::Pinned, 1), Some(id(1)));
        assert_eq!(nth_item_id(&store, PaneId::Pinned, 2), Some(id(3)));
        assert_eq!(nth_item_id(&store, PaneId::Main, 0), None);
    }

    #[test]
    fn pinning_moves_an_item_between_panes_and_keeps_the_selection_position() {
        let mut store = store_of(&[("one", false), ("two", false), ("three", false), ("four", false)]);
        let ov = Overlay::new();
        let mut st = ov.st.borrow_mut();
        st.rebuild_view(PaneId::Main, &store, &[]);
        st.panes[0].sel = Some(1);
        st.panes[0].sel_id = Some(st.panes[0].rows[1].id);
        let id = st.panes[0].rows[1].id;
        store.set_pinned(id, true);
        st.rebuild_view(PaneId::Main, &store, &[]);
        st.rebuild_view(PaneId::Pinned, &store, &[]);
        assert_eq!(st.panes[0].rows.len(), 3, "gone from the main list");
        assert_eq!(st.panes[1].rows.iter().map(|r| r.id).collect::<Vec<_>>(), [id], "now in the pinned pane");
        assert_eq!(st.panes[0].sel, Some(1), "selection stays at the same position, not the top");
    }

    #[test]
    fn selection_falls_back_to_the_last_row_and_a_fresh_view_starts_at_the_top() {
        let mut store = store_of(&[("one", false), ("two", false), ("three", false)]);
        let ov = Overlay::new();
        let mut st = ov.st.borrow_mut();
        st.rebuild_view(PaneId::Main, &store, &[]);
        st.panes[0].sel = Some(2);
        st.panes[0].sel_id = Some(st.panes[0].rows[2].id);
        let id = st.panes[0].rows[2].id;
        store.remove(&[id]);
        st.rebuild_view(PaneId::Main, &store, &[]);
        assert_eq!(st.panes[0].sel, Some(1), "deleted the last row: select the new last row");
        st.panes[0].sel_id = None; // what show() and a changed query do
        st.rebuild_view(PaneId::Main, &store, &[]);
        assert_eq!(st.panes[0].sel, Some(0));
    }

    #[test]
    fn search_filters_keeps_original_numbers_and_selects_first_hit() {
        let store = store_of(&[("apple pie", false), ("banana split", false), ("cherry tart", false)]);
        let ov = Overlay::new();
        let mut st = ov.st.borrow_mut();
        st.panes[0].query = "split".into();
        st.rebuild_view(PaneId::Main, &store, &[]);
        let rows = &st.panes[0].rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, 2, "number jump keys on the unfiltered position");
        assert_eq!(st.panes[0].sel, Some(0));
        st.panes[0].query = "zzzzzz".into();
        st.rebuild_view(PaneId::Main, &store, &[]);
        assert!(st.panes[0].rows.is_empty());
        assert_eq!(st.panes[0].sel, None);
    }

    #[test]
    fn selection_survives_a_rebuild_when_the_item_still_exists() {
        let mut store = store_of(&[("one", false), ("two", false), ("three", false)]);
        let ov = Overlay::new();
        let mut st = ov.st.borrow_mut();
        st.rebuild_view(PaneId::Main, &store, &[]);
        st.panes[0].sel = Some(2);
        st.panes[0].sel_id = Some(st.panes[0].rows[2].id);
        // A new item arrives on top: the selected item moves down but stays selected.
        store.add(text_item(0, "newest", false));
        st.rebuild_view(PaneId::Main, &store, &[]);
        assert_eq!(st.panes[0].sel, Some(3));
        // Deleting it keeps the selection at the same position (clamped), not back at the top.
        let id = st.panes[0].sel_id.unwrap_or(0);
        store.remove(&[id]);
        st.rebuild_view(PaneId::Main, &store, &[]);
        assert_eq!(st.panes[0].sel, Some(2));
    }

    #[test]
    fn selection_ids_follow_display_order_and_fall_back_to_the_typed_number() {
        let store = store_of(&[("one", false), ("two", false), ("three", false)]);
        let ov = Overlay::new();
        let mut st = ov.st.borrow_mut();
        st.rebuild_view(PaneId::Main, &store, &[]);
        let ids: Vec<u64> = st.panes[0].rows.iter().map(|r| r.id).collect();
        st.panes[0].multi = [ids[2], ids[0]].into_iter().collect();
        assert_eq!(st.panes[0].selection_ids(), vec![ids[0], ids[2]], "top-to-bottom, not insertion order");
        st.panes[0].multi.clear();
        st.panes[0].sel = None;
        st.panes[0].number_target = Some(ids[1]);
        assert_eq!(st.panes[0].selection_ids(), vec![ids[1]]);
    }

    #[test]
    fn grips_are_on_the_outer_edges_only() {
        let (w, h, g) = (640.0, 520.0, 6.0);
        // Main grows right/down.
        assert_eq!(zone_at(PaneId::Main, w, h, 639.0, 100.0, g), Some(Zone::E));
        assert_eq!(zone_at(PaneId::Main, w, h, 100.0, 519.0, g), Some(Zone::S));
        assert_eq!(zone_at(PaneId::Main, w, h, 639.0, 519.0, g), Some(Zone::SE));
        assert_eq!(zone_at(PaneId::Main, w, h, 1.0, 100.0, g), None, "main's left edge is not a grip");
        assert_eq!(zone_at(PaneId::Main, w, h, 300.0, 1.0, g), None, "the top edge never resizes");
        assert_eq!(zone_at(PaneId::Main, w, h, 300.0, 300.0, g), None);
        // Pinned grows left/down.
        assert_eq!(zone_at(PaneId::Pinned, 320.0, h, 2.0, 100.0, g), Some(Zone::W));
        assert_eq!(zone_at(PaneId::Pinned, 320.0, h, 2.0, 519.0, g), Some(Zone::SW));
        assert_eq!(zone_at(PaneId::Pinned, 320.0, h, 319.0, 100.0, g), None, "pinned's right edge is not a grip");
    }

    #[test]
    fn resizing_respects_minimums_maximums_and_direction() {
        let start = Dims { pinned_w: 320.0, main_w: 640.0, h: 520.0 };
        let lim = Limits { max_pinned_w: 500.0, max_main_w: 900.0, max_h: 700.0 };
        let d = resized(Zone::E, start, 100.0, 50.0, &lim);
        assert_eq!((d.main_w, d.pinned_w, d.h), (740.0, 320.0, 520.0), "E changes only the main width");
        let d = resized(Zone::E, start, 5000.0, 0.0, &lim);
        assert_eq!(d.main_w, 900.0, "capped by the monitor");
        let d = resized(Zone::E, start, -5000.0, 0.0, &lim);
        assert_eq!(d.main_w, 360.0, "capped by the minimum");
        let d = resized(Zone::W, start, 50.0, 0.0, &lim);
        assert_eq!(d.pinned_w, 270.0, "dragging the pinned pane's left edge right makes it narrower");
        let d = resized(Zone::SW, start, -100.0, 100.0, &lim);
        assert_eq!((d.pinned_w, d.h), (420.0, 620.0));
        let d = resized(Zone::SE, start, 10.0, -5000.0, &lim);
        assert_eq!((d.main_w, d.h), (650.0, 260.0));
    }

    #[test]
    fn a_pair_already_past_the_limit_does_not_snap_smaller() {
        let start = Dims { pinned_w: 320.0, main_w: 1000.0, h: 520.0 };
        let lim = Limits { max_pinned_w: 500.0, max_main_w: 900.0, max_h: 700.0 };
        assert_eq!(resized(Zone::E, start, 0.0, 0.0, &lim).main_w, 1000.0);
        assert_eq!(resized(Zone::E, start, -30.0, 0.0, &lim).main_w, 970.0);
        assert_eq!(resized(Zone::E, start, 30.0, 0.0, &lim).main_w, 1000.0, "cannot grow further though");
    }

    #[test]
    fn fit_shrinks_main_first_then_pinned_and_respects_minimums() {
        let big = Dims { pinned_w: 400.0, main_w: 1200.0, h: 900.0 };
        let d = fit(big, 1000.0, 600.0);
        assert_eq!((d.pinned_w, d.main_w, d.h), (400.0, 600.0, 600.0));
        let d = fit(big, 500.0, 100.0);
        assert_eq!((d.main_w, d.pinned_w, d.h), (360.0, 200.0, 260.0), "never below the minimums");
        let small = Dims::default();
        assert_eq!(fit(small, 5000.0, 5000.0), small, "fitting leaves a smaller pair alone");
    }

    #[test]
    fn age_labels() {
        let now = crate::win::util::now_unix_ms();
        assert_eq!(age_text(now), "now");
        assert_eq!(age_text(now - 12_000), "12s");
        assert_eq!(age_text(now - 5 * 60_000), "5m");
        assert_eq!(age_text(now - 3 * 3_600_000), "3h");
        assert_eq!(age_text(now - 2 * 86_400_000), "2d");
        assert_eq!(age_text(now - 8 * 86_400_000), "1w");
    }

    #[test]
    fn flatten_replaces_line_breaks_and_tabs() {
        assert_eq!(flatten("a\r\nb\tc\nd"), "a  b c d");
    }
}
