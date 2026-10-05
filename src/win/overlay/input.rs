//! Window procedures, keyboard / mouse handling, focus acquisition and the context menu.

use super::paint::{self, Hit};
use super::*;
use crate::transform::CaseOp;
use crate::win::app::app;
use crate::win::commands::Cmd;
use crate::win::gfx::contains;
use crate::win::util::from_wide;
use windows::Win32::System::Threading::AttachThreadInput;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetFocus};

const EN_CHANGE: u32 = 0x0300;

fn key_down(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: plain query.
    let v = unsafe { GetKeyState(vk.0 as i32) };
    v < 0
}

fn loword(l: LPARAM) -> i32 {
    (l.0 & 0xFFFF) as i16 as i32
}
fn hiword(l: LPARAM) -> i32 {
    ((l.0 >> 16) & 0xFFFF) as i16 as i32
}

/// Brings `hwnd` to the foreground reliably, even over the Start menu (spec 10.11):
/// AttachThreadInput with the foreground thread first, phantom Alt only if that fails.
pub(super) fn take_foreground(hwnd: HWND) {
    // SAFETY: input-queue attach is always detached; no system setting is changed.
    unsafe {
        let fg = GetForegroundWindow();
        let me = windows::Win32::System::Threading::GetCurrentThreadId();
        let ftid = if fg.0.is_null() { 0 } else { GetWindowThreadProcessId(fg, None) };
        let attached = ftid != 0 && ftid != me && AttachThreadInput(me, ftid, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let mut ok = SetForegroundWindow(hwnd).as_bool();
        if attached {
            let _ = AttachThreadInput(me, ftid, false);
        }
        if !ok && GetForegroundWindow() != hwnd {
            // Phantom Alt press grants the foreground right.
            keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
            ok = SetForegroundWindow(hwnd).as_bool();
        }
        let _ = SetFocus(Some(hwnd));
        let _ = ok;
    }
}

use windows::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP};
use windows::Win32::UI::WindowsAndMessaging::{BringWindowToTop, GetWindowThreadProcessId};

pub(super) unsafe extern "system" fn pane_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    crate::win::util::wndproc_guard(h, m, w, l, || {
        // SAFETY: called from the window procedure with its own arguments.
        unsafe { pane_proc_body(h, m, w, l) }
    })
}

unsafe fn pane_proc_body(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(h, m, w, l) };
    let ov = &app.overlay;
    let pid = {
        let Ok(st) = ov.st.try_borrow() else { return DefWindowProcW(h, m, w, l) };
        if st.panes[0].hwnd == h {
            PaneId::Main
        } else if st.panes[1].hwnd == h {
            PaneId::Pinned
        } else {
            return DefWindowProcW(h, m, w, l);
        }
    };
    match m {
        WM_PAINT => {
            ov.paint(&app, pid, h);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        // Alt+F4 / system close must never destroy a pane: the overlay only hides.
        WM_CLOSE => {
            ov.hide(&app);
            LRESULT(0)
        }
        WM_MOUSEACTIVATE if pid == PaneId::Pinned => LRESULT(MA_NOACTIVATE as isize),
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if ov.on_key_down(&app, false, w.0 as u32, None) {
                return LRESULT(0);
            }
            DefWindowProcW(h, m, w, l)
        }
        WM_CHAR => {
            ov.on_char(&app, w.0 as u32);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            ov.on_lbutton_down(&app, pid, loword(l) as f32, hiword(l) as f32);
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            ov.on_dblclick(&app, pid, loword(l) as f32, hiword(l) as f32);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            ov.on_lbutton_up(&app);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            ov.on_rbutton_up(&app, pid, loword(l) as f32, hiword(l) as f32);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            ov.on_mouse_move(&app, pid, h, loword(l) as f32, hiword(l) as f32, w.0 & 1 != 0);
            LRESULT(0)
        }
        // Capture lost mid-drag (Alt+Tab, another window grabbing the mouse): stop dragging.
        WM_CAPTURECHANGED => {
            if let Ok(mut st) = ov.st.try_borrow_mut() {
                st.drag = None;
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            ov.on_mouse_leave(pid);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((w.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
            ov.on_wheel(&app, pid, delta);
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            if let Ok(st) = ov.st.try_borrow() {
                let hdc = HDC(w.0 as *mut _);
                // SAFETY: colouring the edit's DC to match the painted pill (lesson 18.24).
                unsafe {
                    SetTextColor(hdc, COLORREF(st.pal.ink_high.colorref()));
                    SetBkColor(hdc, COLORREF(st.pal.surface_field.colorref()));
                }
                return LRESULT(st.brush_field.0 as isize);
            }
            DefWindowProcW(h, m, w, l)
        }
        WM_COMMAND => {
            if ((w.0 >> 16) & 0xFFFF) as u32 == EN_CHANGE {
                ov.on_edit_change(&app, pid);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            ov.on_timer(&app, w.0);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (w.0 & 0xFFFF) == 0 {
                ov.on_deactivate(&app, h);
            } else {
                // SAFETY: cancel a pending focus-loss check.
                unsafe {
                    let main = ov.st.borrow().panes[0].hwnd;
                    let _ = KillTimer(Some(main), T_FOCUSLOSS);
                }
            }
            DefWindowProcW(h, m, w, l)
        }
        // Resize cursors over the draggable edges (and for the whole of a resize drag).
        WM_SETCURSOR => {
            if (l.0 & 0xFFFF) as u32 == HTCLIENT {
                if let Some(c) = ov.resize_cursor(pid, h) {
                    // SAFETY: stock system cursor.
                    unsafe {
                        if let Ok(cur) = LoadCursorW(None, c) {
                            SetCursor(Some(cur));
                        }
                    }
                    return LRESULT(1);
                }
            }
            DefWindowProcW(h, m, w, l)
        }
        WM_DPICHANGED => {
            let dpi = (w.0 & 0xFFFF) as f32;
            if dpi > 0.0 {
                ov.st.borrow_mut().scale = dpi / 96.0;
                ov.apply_look(&app);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(h, m, w, l),
    }
}

// ---------------- search edit subclass ----------------

pub(super) fn subclass_edit(edit: HWND, id: PaneId) {
    // SAFETY: subclass installed on a control we created.
    unsafe {
        let _ = SetWindowSubclass(edit, Some(edit_proc), 1, id as usize);
    }
}

unsafe extern "system" fn edit_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM, _uid: usize, refdata: usize) -> LRESULT {
    crate::win::util::wndproc_guard(h, m, w, l, || {
        // SAFETY: called from the subclass procedure with its own arguments.
        unsafe { edit_proc_body(h, m, w, l, refdata) }
    })
}

unsafe fn edit_proc_body(h: HWND, m: u32, w: WPARAM, l: LPARAM, refdata: usize) -> LRESULT {
    let pid = if refdata == 0 { PaneId::Main } else { PaneId::Pinned };
    if let Some(app) = app() {
        match m {
            WM_SETFOCUS => app.overlay.set_logical_focus(pid),
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                if app.overlay.on_key_down(&app, true, w.0 as u32, Some(pid)) {
                    return LRESULT(0);
                }
            }
            // Swallow the control characters we act on so the edit never beeps.
            WM_CHAR if matches!(w.0, 0x0D | 0x09 | 0x1B | 0x06) => return LRESULT(0),
            _ => {}
        }
    }
    if m == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(h, Some(edit_proc), 1);
    }
    DefSubclassProc(h, m, w, l)
}

// ---------------- behaviour ----------------

impl Overlay {
    pub(super) fn set_logical_focus(&self, pid: PaneId) {
        let changed = {
            let Ok(mut st) = self.st.try_borrow_mut() else { return };
            let c = st.focus != pid;
            st.focus = pid;
            c
        };
        if changed {
            self.invalidate_all();
        }
    }

    fn real_focus_list(&self) {
        let main = self.st.borrow().panes[0].hwnd;
        // SAFETY: focus the list window (the main pane window owns list focus for both panes).
        unsafe {
            let _ = SetFocus(Some(main));
        }
    }

    fn focused_pane(&self) -> PaneId {
        self.st.borrow().focus
    }

    /// Returns true when the key was consumed.
    pub(super) fn on_key_down(&self, app: &App, from_edit: bool, vk: u32, edit_pane: Option<PaneId>) -> bool {
        let (ctrl, shift) = (key_down(VK_CONTROL), key_down(VK_SHIFT));
        if let Some(p) = edit_pane {
            self.set_logical_focus(p);
        }
        // The shortcut sheet: any (non-modifier) key dismisses it.
        let sheet = self.st.borrow().sheet;
        if sheet {
            if !matches!(VIRTUAL_KEY(vk as u16), VK_SHIFT | VK_CONTROL | VK_MENU | VK_LWIN | VK_RWIN) {
                self.st.borrow_mut().sheet = false;
                self.invalidate_all();
            }
            return true;
        }
        // Any key other than a digit / Backspace ends number-jump mode.
        let is_digit = (0x30..=0x39).contains(&vk) || (0x60..=0x69).contains(&vk);
        if !is_digit && vk != VK_BACK.0 as u32 && vk != VK_RETURN.0 as u32 && !matches!(VIRTUAL_KEY(vk as u16), VK_SHIFT | VK_CONTROL | VK_MENU) {
            self.clear_number();
        }
        self.clear_preview();
        let pid = self.focused_pane();
        match VIRTUAL_KEY(vk as u16) {
            VK_ESCAPE => {
                self.hide(app);
                true
            }
            VK_F1 => {
                self.st.borrow_mut().sheet = true;
                self.invalidate_all();
                true
            }
            VK_TAB => {
                let other = pid.other();
                self.set_logical_focus(other);
                self.real_focus_list();
                self.invalidate_all();
                true
            }
            VK_UP => {
                self.move_by(app, pid, -1, shift);
                true
            }
            VK_DOWN => {
                self.move_by(app, pid, 1, shift);
                true
            }
            VK_PRIOR | VK_NEXT => {
                let step = {
                    let st = self.st.borrow();
                    let lines = st.card_lines_of(pid);
                    let cl = move |_: usize| lines;
                    layout::page_step(&st.layout_input(pid, &cl), st.viewport_h()) as isize
                };
                self.move_by(app, pid, if vk == VK_PRIOR.0 as u32 { -step } else { step }, shift);
                true
            }
            VK_HOME if !from_edit => {
                self.move_to(app, pid, Some(0), shift);
                true
            }
            VK_END if !from_edit => {
                let n = self.st.borrow().panes[pid as usize].rows.len();
                self.move_to(app, pid, n.checked_sub(1), shift);
                true
            }
            VK_RETURN => {
                if self.st.borrow().panes[pid as usize].number_buf.is_empty() && self.try_manage_snippets(app) {
                    return true;
                }
                if ctrl {
                    app.run_cmd(Cmd::PastePlain);
                } else {
                    app.run_cmd(Cmd::Paste);
                }
                true
            }
            VK_LEFT if ctrl => {
                self.set_scope(app, Scope::History);
                true
            }
            VK_RIGHT if ctrl => {
                self.set_scope(app, Scope::Snippets);
                true
            }
            VK_F if ctrl => {
                self.focus_search(pid);
                true
            }
            VK_DELETE if !from_edit => {
                if self.scope() == Scope::Snippets && pid == PaneId::Main {
                    app.snippet_delete_selected();
                } else {
                    app.run_cmd(Cmd::Delete);
                }
                true
            }
            VK_BACK if !from_edit => {
                self.on_backspace(app, pid);
                true
            }
            _ => false,
        }
    }

    /// `*set` + Enter in the Snippets scope opens the manager (spec 14).
    fn try_manage_snippets(&self, app: &App) -> bool {
        let hit = {
            let st = self.st.borrow();
            st.scope == Scope::Snippets && st.focus == PaneId::Main && st.panes[0].query.trim().eq_ignore_ascii_case("*set")
        };
        if hit {
            self.hide(app);
            crate::win::dialogs::manage_snippets(app);
        }
        hit
    }

    pub(super) fn on_char(&self, app: &App, ch: u32) {
        if ch < 0x20 || ch == 0x7F {
            return;
        }
        let Some(c) = char::from_u32(ch) else { return };
        if self.st.borrow().sheet {
            return;
        }
        let pid = self.focused_pane();
        let snippets_scope = self.scope() == Scope::Snippets && pid == PaneId::Main;
        if c.is_ascii_digit() {
            self.number_digit(app, pid, c);
            return;
        }
        if c == '?' {
            self.st.borrow_mut().sheet = true;
            self.invalidate_all();
            return;
        }
        let up = c.to_ascii_uppercase();
        if snippets_scope {
            match up {
                'A' => return app.snippet_add(),
                'E' => return app.snippet_edit_selected(),
                _ => {}
            }
        } else {
            let cmd = match up {
                'U' => Some(Cmd::CleanUrl),
                'M' => Some(if self.selection_count() >= 2 { Cmd::Merge } else { Cmd::Markdown }),
                'P' => Some(Cmd::PastePlain),
                'H' => Some(Cmd::HtmlText),
                'E' => Some(Cmd::EditPaste),
                'X' => Some(Cmd::EditSave),
                'Z' => Some(Cmd::Excel),
                _ => None,
            };
            if let Some(cmd) = cmd {
                app.run_cmd(cmd);
                return;
            }
        }
        // Everything else is typing: hand it to the search field.
        let edit = self.st.borrow().panes[pid as usize].edit;
        // SAFETY: focus the edit and forward the character.
        unsafe {
            let _ = SetFocus(Some(edit));
            SendMessageW(edit, WM_CHAR, Some(WPARAM(ch as usize)), Some(LPARAM(1)));
        }
    }

    pub fn selection_count(&self) -> usize {
        let st = self.st.borrow();
        let p = &st.panes[st.focus as usize];
        if p.multi.len() >= 2 {
            p.multi.len()
        } else {
            usize::from(p.sel.is_some())
        }
    }

    fn focus_search(&self, pid: PaneId) {
        let edit = self.st.borrow().panes[pid as usize].edit;
        // SAFETY: focus + select all.
        unsafe {
            let _ = SetFocus(Some(edit));
            SendMessageW(edit, 0x00B1, Some(WPARAM(0)), Some(LPARAM(-1))); // EM_SETSEL
        }
    }

    fn clear_number(&self) {
        let had = {
            let Ok(mut st) = self.st.try_borrow_mut() else { return };
            let mut had = false;
            for p in st.panes.iter_mut() {
                if !p.number_buf.is_empty() {
                    p.number_buf.clear();
                    p.number_target = None;
                    had = true;
                }
            }
            had
        };
        if had {
            self.layout_edits();
            self.invalidate_all();
        }
    }

    fn number_digit(&self, app: &App, pid: PaneId, d: char) {
        {
            let mut st = self.st.borrow_mut();
            let p = &mut st.panes[pid as usize];
            if p.number_buf.len() >= 6 {
                return;
            }
            p.number_buf.push(d);
        }
        self.apply_number(app, pid);
    }

    fn on_backspace(&self, app: &App, pid: PaneId) {
        let editing_number = !self.st.borrow().panes[pid as usize].number_buf.is_empty();
        if editing_number {
            self.st.borrow_mut().panes[pid as usize].number_buf.pop();
            self.apply_number(app, pid);
        } else {
            // Backspace on the list edits the search text.
            let edit = self.st.borrow().panes[pid as usize].edit;
            // SAFETY: focus the edit and forward a backspace.
            unsafe {
                let _ = SetFocus(Some(edit));
                SendMessageW(edit, WM_CHAR, Some(WPARAM(8)), Some(LPARAM(1)));
            }
        }
    }

    /// Resolves the typed number to an item of the UNFILTERED pane list (spec 10.6).
    fn apply_number(&self, app: &App, pid: PaneId) {
        {
            let store = app.store.borrow();
            let snippets = app.snippets.borrow();
            let mut st = self.st.borrow_mut();
            let scope = st.active_scope(pid);
            let n: usize = st.panes[pid as usize].number_buf.parse().unwrap_or(0);
            let target: Option<u64> = if n == 0 {
                None
            } else if pid == PaneId::Main && scope == Scope::Snippets {
                (n <= snippets.len()).then(|| (n - 1) as u64)
            } else {
                nth_item_id(&store, pid, n)
            };
            let p = &mut st.panes[pid as usize];
            p.number_target = target;
            if let Some(idx) = target.and_then(|id| p.rows.iter().position(|r| r.id == id)) {
                p.sel = Some(idx);
                p.sel_id = Some(p.rows[idx].id);
                p.anchor = Some(idx);
                p.multi.clear();
            }
            st.refresh_card(pid, &store, &snippets);
            st.ensure_visible(pid, &store);
        }
        self.layout_edits();
        self.invalidate_all();
    }

    fn move_by(&self, app: &App, pid: PaneId, delta: isize, extend: bool) {
        let cur = {
            let st = self.st.borrow();
            let p = &st.panes[pid as usize];
            if p.rows.is_empty() {
                return;
            }
            p.sel.map(|i| i as isize)
        };
        let n = self.st.borrow().panes[pid as usize].rows.len() as isize;
        let next = match cur {
            Some(c) => (c + delta).clamp(0, n - 1),
            None => 0,
        };
        self.move_to(app, pid, Some(next as usize), extend);
    }

    fn move_to(&self, app: &App, pid: PaneId, idx: Option<usize>, extend: bool) {
        let Some(idx) = idx else { return };
        {
            let store = app.store.borrow();
            let snippets = app.snippets.borrow();
            let mut st = self.st.borrow_mut();
            let p = &mut st.panes[pid as usize];
            if idx >= p.rows.len() {
                return;
            }
            if !extend {
                p.anchor = Some(idx);
                p.multi.clear();
            }
            p.sel = Some(idx);
            p.sel_id = Some(p.rows[idx].id);
            p.number_buf.clear();
            p.number_target = None;
            if extend {
                let a = p.anchor.unwrap_or(idx);
                let (lo, hi) = (a.min(idx), a.max(idx));
                p.multi = p.rows[lo..=hi].iter().map(|r| r.id).collect();
            }
            st.refresh_card(pid, &store, &snippets);
            st.ensure_visible(pid, &store);
        }
        self.layout_edits();
        self.invalidate(pid);
    }

    pub(super) fn set_scope(&self, app: &App, scope: Scope) {
        if self.st.borrow().scope == scope {
            return;
        }
        {
            let mut st = self.st.borrow_mut();
            st.scope = scope;
            st.panes[0].scroll = 0.0;
            st.panes[0].sel_id = None;
            st.panes[0].multi.clear();
        }
        self.rebuild_pane(app, PaneId::Main);
        self.invalidate_all();
    }

    pub(super) fn on_edit_change(&self, app: &App, pid: PaneId) {
        let edit = self.st.borrow().panes[pid as usize].edit;
        let mut buf = [0u16; 512];
        // SAFETY: bounded read of the control text.
        let n = unsafe { GetWindowTextW(edit, &mut buf) };
        let q = from_wide(&buf[..n.max(0) as usize]);
        {
            let Ok(mut st) = self.st.try_borrow_mut() else { return };
            if st.panes[pid as usize].query == q {
                return;
            }
            st.panes[pid as usize].query = q;
            st.panes[pid as usize].sel_id = None;
            st.panes[pid as usize].scroll = 0.0;
        }
        self.rebuild_pane(app, pid);
        self.invalidate(pid);
    }

    // ---------------- mouse ----------------

    fn hit_at(&self, pid: PaneId, x: f32, y: f32) -> Hit {
        let st = self.st.borrow();
        let g = self.gfx[pid as usize].borrow();
        match g.as_ref() {
            Some(gf) => paint::hit(&st, pid, gf, x, y),
            None => Hit::None,
        }
    }

    pub(super) fn on_lbutton_down(&self, app: &App, pid: PaneId, x: f32, y: f32) {
        if self.st.borrow().sheet {
            self.st.borrow_mut().sheet = false;
            self.invalidate_all();
            return;
        }
        self.set_logical_focus(pid);
        self.real_focus_list();
        // The outer edges resize the overlay (main: right/bottom, pinned: left/bottom).
        if let Some(zone) = self.zone_under(pid, x, y) {
            self.begin_drag(pid, DragKind::Resize(zone));
            return;
        }
        let (ctrl, shift) = (key_down(VK_CONTROL), key_down(VK_SHIFT));
        match self.hit_at(pid, x, y) {
            Hit::Row(i) => {
                {
                    let store = app.store.borrow();
                    let snippets = app.snippets.borrow();
                    let mut st = self.st.borrow_mut();
                    let p = &mut st.panes[pid as usize];
                    let Some(id) = p.rows.get(i).map(|r| r.id) else { return };
                    if ctrl {
                        if p.multi.is_empty() {
                            if let Some(cur) = p.sel_id {
                                p.multi.insert(cur);
                            }
                        }
                        if !p.multi.remove(&id) {
                            p.multi.insert(id);
                        }
                    } else if shift {
                        let a = p.anchor.unwrap_or(i);
                        let (lo, hi) = (a.min(i), a.max(i));
                        p.multi = p.rows[lo..=hi].iter().map(|r| r.id).collect();
                    } else {
                        p.multi.clear();
                        p.anchor = Some(i);
                    }
                    p.sel = Some(i);
                    p.sel_id = Some(id);
                    p.number_buf.clear();
                    p.number_target = None;
                    st.refresh_card(pid, &store, &snippets);
                    st.ensure_visible(pid, &store);
                }
                self.layout_edits();
                self.invalidate(pid);
            }
            Hit::CardBtn(_, b) => {
                let cmd = match b {
                    Btn::Paste => Cmd::Paste,
                    Btn::CleanUrl => Cmd::CleanUrl,
                    Btn::Plain => Cmd::PastePlain,
                    Btn::Edit => Cmd::EditPaste,
                    Btn::Merge => {
                        if self.selection_count() >= 2 {
                            Cmd::Merge
                        } else {
                            Cmd::Markdown
                        }
                    }
                };
                app.run_cmd(cmd);
            }
            Hit::SegAll => self.set_scope(app, Scope::History),
            Hit::SegSnip => self.set_scope(app, Scope::Snippets),
            Hit::Header => {
                // Drag the pair by the header (not the search pill itself).
                let pill_hit = {
                    let st = self.st.borrow();
                    let g = self.gfx[pid as usize].borrow();
                    g.as_ref().is_some_and(|gf| contains(&paint::geo(&st, pid, gf).pill, x, y))
                };
                if !pill_hit {
                    self.begin_drag(pid, DragKind::Move);
                }
            }
            // The footer is just as good a handle as the header.
            Hit::Footer => self.begin_drag(pid, DragKind::Move),
            _ => {}
        }
    }

    pub(super) fn on_dblclick(&self, app: &App, pid: PaneId, x: f32, y: f32) {
        if let Hit::Row(_) = self.hit_at(pid, x, y) {
            self.on_lbutton_down(app, pid, x, y);
            app.run_cmd(Cmd::Paste);
        }
    }

    pub(super) fn on_lbutton_up(&self, app: &App) {
        let drag = self.st.borrow_mut().drag.take();
        if let Some(d) = drag {
            // SAFETY: release capture taken in on_lbutton_down.
            unsafe {
                let _ = ReleaseCapture();
            }
            if d.moved {
                self.remember_position(app);
            }
        }
    }

    pub(super) fn on_mouse_move(&self, app: &App, pid: PaneId, hwnd: HWND, x: f32, y: f32, left_down: bool) {
        // Drag: both windows follow the cursor (move) or the edge being resized.
        let dragging = self.st.borrow().drag.is_some();
        if dragging {
            if left_down {
                self.drag_to(app);
            } else {
                // The button was released somewhere we never saw it: finish the drag now.
                self.on_lbutton_up(app);
            }
            return;
        }
        let hit = self.hit_at(pid, x, y);
        // Resting on an image row (not its buttons) starts the preview timer.
        let img = match hit {
            Hit::Row(i) => self.image_row_target(app, pid, i),
            _ => None,
        };
        self.set_hover_preview(img);
        let (hover, btn) = match hit {
            Hit::Row(i) => (Some(i), None),
            Hit::CardBtn(i, b) => (Some(i), Some(b)),
            _ => (None, None),
        };
        let need_track = {
            let Ok(mut st) = self.st.try_borrow_mut() else { return };
            let p = &mut st.panes[pid as usize];
            let changed = p.hover != hover || p.hover_btn != btn;
            p.hover = hover;
            p.hover_btn = btn;
            let t = !p.tracking;
            p.tracking = true;
            (changed, t)
        };
        if need_track.1 {
            let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
            // SAFETY: request WM_MOUSELEAVE.
            unsafe {
                let _ = TrackMouseEvent(&mut tme);
            }
        }
        if need_track.0 {
            self.invalidate(pid);
        }
    }

    pub(super) fn on_mouse_leave(&self, pid: PaneId) {
        self.clear_preview();
        if let Ok(mut st) = self.st.try_borrow_mut() {
            let p = &mut st.panes[pid as usize];
            p.tracking = false;
            p.hover = None;
            p.hover_btn = None;
        }
        self.invalidate(pid);
    }

    pub(super) fn on_wheel(&self, app: &App, pid: PaneId, notches: f32) {
        self.clear_preview(); // the rows move under a resting cursor
        {
            let store = app.store.borrow();
            let mut st = self.st.borrow_mut();
            let step = st.metrics.row_h * 3.0 * notches;
            let lines = st.card_lines_of(pid);
            let cl = move |_: usize| lines;
            let ms = layout::max_scroll(&st.layout_input(pid, &cl), st.viewport_h());
            let p = &mut st.panes[pid as usize];
            p.scroll = (p.scroll - step).clamp(0.0, ms);
            let _ = &store;
        }
        self.invalidate(pid);
    }

    pub(super) fn on_rbutton_up(&self, app: &App, pid: PaneId, x: f32, y: f32) {
        self.set_logical_focus(pid);
        let hit = self.hit_at(pid, x, y);
        let (Hit::Row(i) | Hit::CardBtn(i, _)) = hit else { return };
        {
            let store = app.store.borrow();
            let snippets = app.snippets.borrow();
            let mut st = self.st.borrow_mut();
            let p = &mut st.panes[pid as usize];
            let Some(id) = p.rows.get(i).map(|r| r.id) else { return };
            if !p.multi.contains(&id) {
                p.multi.clear();
                p.sel = Some(i);
                p.sel_id = Some(id);
                p.anchor = Some(i);
            }
            st.refresh_card(pid, &store, &snippets);
        }
        self.invalidate(pid);
        self.context_menu(app, pid);
    }

    fn context_menu(&self, app: &App, pid: PaneId) {
        let hwnd = self.st.borrow().panes[pid as usize].hwnd;
        let snippets = self.scope() == Scope::Snippets && pid == PaneId::Main;
        let items = self.selected_items(app);
        let multi = items.len() >= 2;
        let first = items.first();
        let is_url = first.and_then(|it| it.text()).is_some_and(|t| crate::transform::is_single_url(&t));
        let is_image = first.is_some_and(|it| it.kind == Kind::Image);
        let has_text = items.iter().any(|it| it.has_text() || it.text().is_some());
        let pinned = first.is_some_and(|it| it.pinned);
        // SAFETY: popup menu built, tracked and destroyed here.
        let cmd = unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let add = |m: HMENU, id: usize, text: &str, on: bool| {
                let t = wide(text);
                let _ = AppendMenuW(m, MF_STRING | if on { MENU_ITEM_FLAGS(0) } else { MF_GRAYED }, id, pcw(&t));
            };
            let sep = |m: HMENU| {
                let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());
            };
            if snippets {
                add(menu, 50, "Paste", true);
                add(menu, 51, "Edit…", true);
                add(menu, 52, "Add…", true);
                sep(menu);
                add(menu, 53, "Delete", true);
            } else {
                add(menu, 1, if pinned { "Unpin" } else { "Pin" }, true);
                add(menu, 2, "Copy as plain text", has_text);
                add(menu, 3, "Open URL", is_url);
                add(menu, 4, "Save image to file…", is_image);
                add(menu, 5, "Keystroke paste", has_text);
                sep(menu);
                if let Ok(t) = CreatePopupMenu() {
                    for (id, n) in [(10, "UPPER"), (11, "lower"), (12, "Title Case"), (13, "Remove line breaks"), (14, "Trim"), (15, "Plain text")] {
                        add(t, id, n, has_text);
                    }
                    let l = wide("Transform text");
                    let _ = AppendMenuW(menu, MF_POPUP, t.0 as usize, pcw(&l));
                }
                if let Ok(t) = CreatePopupMenu() {
                    add(t, 20, "Plain text", has_text);
                    add(t, 21, "Clean URL", is_url);
                    add(t, 22, "Markdown link", has_text);
                    add(t, 23, "HTML as text", has_text);
                    let l = wide("Paste as");
                    let _ = AppendMenuW(menu, MF_POPUP, t.0 as usize, pcw(&l));
                }
                sep(menu);
                add(menu, 30, "Edit && Paste…", has_text);
                add(menu, 31, "Edit && Save as new…", has_text);
                add(menu, 32, "Merge selection into new item", multi);
                add(menu, 33, "Paste && merge as plain text", multi);
                add(menu, 34, "Paste into Excel", multi);
                sep(menu);
                add(menu, 40, "Delete", true);
                add(menu, 41, "Clear list…", true);
            }
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let c = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None);
            let _ = DestroyMenu(menu);
            c.0 as u32
        };
        let run = |c: Cmd| app.run_cmd(c);
        match cmd {
            0 => {}
            1 => run(Cmd::TogglePin),
            2 => run(Cmd::CopyPlain),
            3 => run(Cmd::OpenUrl),
            4 => run(Cmd::SaveImage),
            5 => run(Cmd::Keystroke),
            10 => run(Cmd::Transform(CaseOp::Upper)),
            11 => run(Cmd::Transform(CaseOp::Lower)),
            12 => run(Cmd::Transform(CaseOp::Title)),
            13 => run(Cmd::Transform(CaseOp::RemoveLineBreaks)),
            14 => run(Cmd::Transform(CaseOp::Trim)),
            15 => run(Cmd::Transform(CaseOp::Plain)),
            20 => run(Cmd::PastePlain),
            21 => run(Cmd::CleanUrl),
            22 => run(Cmd::Markdown),
            23 => run(Cmd::HtmlText),
            30 => run(Cmd::EditPaste),
            31 => run(Cmd::EditSave),
            32 => run(Cmd::Merge),
            33 => run(Cmd::PastePlain),
            34 => run(Cmd::Excel),
            40 => run(Cmd::Delete),
            41 => run(Cmd::ClearList),
            50 => run(Cmd::Paste),
            51 => app.snippet_edit_selected(),
            52 => app.snippet_add(),
            53 => app.snippet_delete_selected(),
            _ => {}
        }
    }

    // ---------------- timers / focus ----------------

    pub(super) fn on_timer(&self, app: &App, id: usize) {
        let (m, p) = {
            let st = self.st.borrow();
            (st.panes[0].hwnd, st.panes[1].hwnd)
        };
        match id {
            T_ACQUIRE => {
                // SAFETY: foreground queries; retries for up to 3 s (spec 10.11).
                unsafe {
                    let fg = GetForegroundWindow();
                    if fg == m || fg == p {
                        self.st.borrow_mut().acquired = true;
                        let _ = KillTimer(Some(m), T_ACQUIRE);
                        return;
                    }
                    let expired = self.st.borrow().acquire_deadline.is_none_or(|d| Instant::now() > d);
                    if expired {
                        let _ = KillTimer(Some(m), T_ACQUIRE);
                        // Giving up. An overlay that never owned the keyboard (for example when it was
                        // summoned over an elevated window) would otherwise sit there with nothing able
                        // to dismiss it.
                        crate::log_warn!("overlay could not acquire the foreground within 3 s; closing it");
                        self.hide(app);
                    } else {
                        take_foreground(m);
                    }
                }
            }
            T_AGES => {
                if self.is_visible() {
                    self.invalidate_all();
                }
            }
            T_FOCUSLOSS => {
                // SAFETY: one-shot timer.
                unsafe {
                    let _ = KillTimer(Some(m), T_FOCUSLOSS);
                }
                self.hide_if_foreground_lost(app);
            }
            T_FOCUSPOLL => self.hide_if_foreground_lost(app),
            T_PREVIEW => {
                // SAFETY: one-shot timer.
                unsafe {
                    let _ = KillTimer(Some(m), T_PREVIEW);
                }
                self.show_preview(app);
            }
            _ => {}
        }
    }

    /// The overlay closes as soon as another application owns the foreground.
    fn hide_if_foreground_lost(&self, app: &App) {
        let (m, p, vis, acq) = {
            let st = self.st.borrow();
            (st.panes[0].hwnd, st.panes[1].hwnd, st.visible, st.acquired)
        };
        if !(vis && acq) {
            return;
        }
        // SAFETY: plain query.
        let fg = unsafe { GetForegroundWindow() };
        if !foreground_is_ours(fg, m, p) {
            crate::log_dbg!("overlay lost the foreground; hiding");
            self.hide(app);
        }
    }

    pub(super) fn on_deactivate(&self, _app: &App, _h: HWND) {
        let (vis, acq, m) = {
            let st = self.st.borrow();
            (st.visible, st.acquired, st.panes[0].hwnd)
        };
        if vis && acq {
            // Deferred check: activation hops between our own two windows are not "leaving".
            // SAFETY: one-shot timer.
            unsafe {
                let _ = SetTimer(Some(m), T_FOCUSLOSS, 120, None);
            }
        }
    }
}


/// True while `fg` is one of the overlay windows, a window they own (menus, message boxes), or
/// momentarily nothing (Windows reports a NULL foreground in the middle of a switch).
fn foreground_is_ours(fg: HWND, main: HWND, pinned: HWND) -> bool {
    if fg.0.is_null() || fg == main || fg == pinned {
        return true;
    }
    // SAFETY: plain query.
    unsafe { GetWindow(fg, GW_OWNER) }.is_ok_and(|o| o == main || o == pinned)
}
