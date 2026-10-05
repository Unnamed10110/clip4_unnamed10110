//! Plumbing shared by the modal dialogs: one window class, DPI-aware metrics, a control
//! factory, centred placement on the cursor's monitor, foreground acquisition and the nested
//! modal loop (`run_modal`) that keeps the rest of the app's message processing alive.

use crate::win::app::{app, App};
use crate::win::util::{from_wide, pcw, wide};
use std::cell::Cell;
use std::rc::Rc;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

pub use crate::win::app::message_box;

pub const ID_OK: i32 = 1;
pub const ID_CANCEL: i32 = 2;
/// Control style bits that the windows crate types differently per family.
pub const TAB: u32 = WS_TABSTOP.0;
pub const SS_NOPREFIX: u32 = 0x80;
pub const SS_RIGHT: u32 = 0x2;
pub const BST_CHECKED: usize = 1;
pub const EM_SETSEL: u32 = 0xB1;
pub const EM_SETLIMITTEXT: u32 = 0xC5;
pub const EM_EMPTYUNDOBUFFER: u32 = 0xCD;
const DC_HASDEFID: isize = 0x534B;

/// A dialog's message handler. `None` = default handling. Takes `&self`: a handler may pump
/// messages (MessageBox, ChooseColor) and be re-entered, so state lives in `Cell`/`RefCell`.
pub trait Dlg {
    fn msg(&self, app: &App, h: HWND, m: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT>;
}

struct Shell {
    dlg: Rc<dyn Dlg>,
    /// Minimum outer size (0 = fixed-size dialog).
    min: (i32, i32),
    /// Focused child while the dialog is inactive (restored on re-activation).
    focus: Cell<HWND>,
}

const CLASS: PCWSTR = w!("clip4_dialog");

thread_local! {
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "system" fn dlg_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_NCCREATE {
        // SAFETY: lParam is this call's CREATESTRUCTW; lpCreateParams is the Box<Shell> from `create`.
        let cs = &*(l.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(h, GWLP_USERDATA, cs.lpCreateParams as isize);
        return DefWindowProcW(h, m, w, l);
    }
    let p = GetWindowLongPtrW(h, GWLP_USERDATA) as *const Shell;
    if p.is_null() {
        return DefWindowProcW(h, m, w, l);
    }
    if m == WM_NCDESTROY {
        SetWindowLongPtrW(h, GWLP_USERDATA, 0);
        // SAFETY: allocated in `create`, freed exactly once here.
        drop(Box::from_raw(p as *mut Shell));
        return DefWindowProcW(h, m, w, l);
    }
    // SAFETY: valid until WM_NCDESTROY; the Rc clone keeps the handler alive across re-entrancy.
    let (dlg, min, focus) = {
        let s = &*p;
        (s.dlg.clone(), s.min, s.focus.get())
    };
    match m {
        WM_GETMINMAXINFO if min.0 > 0 => {
            // SAFETY: lParam points at a MINMAXINFO for this message.
            let mmi = &mut *(l.0 as *mut MINMAXINFO);
            mmi.ptMinTrackSize = POINT { x: min.0, y: min.1 };
            return LRESULT(0);
        }
        DM_GETDEFID => return LRESULT((DC_HASDEFID << 16) | ID_OK as isize),
        WM_ACTIVATE => {
            if (w.0 & 0xFFFF) == 0 {
                let f = GetFocus();
                if IsChild(h, f).as_bool() {
                    (*p).focus.set(f);
                }
            } else if !focus.0.is_null() {
                let _ = SetFocus(Some(focus));
            }
            return LRESULT(0);
        }
        _ => {}
    }
    let Some(app) = app() else { return DefWindowProcW(h, m, w, l) };
    if m == WM_CLOSE {
        dlg.msg(&app, h, WM_COMMAND, WPARAM(ID_CANCEL as usize), LPARAM(0));
        return LRESULT(0);
    }
    match dlg.msg(&app, h, m, w, l) {
        Some(r) => r,
        // A plain window's DefWindowProc paints static/group boxes white: use the dialog face colour.
        None if m == WM_CTLCOLORSTATIC || m == WM_CTLCOLORBTN => {
            let hdc = HDC(w.0 as *mut _);
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(GetSysColor(COLOR_WINDOWTEXT)));
            LRESULT(GetSysColorBrush(COLOR_3DFACE).0 as isize)
        }
        None => DefWindowProcW(h, m, w, l),
    }
}

/// Work area and DPI scale of the monitor under the cursor.
pub struct Screen {
    pub work: RECT,
    pub dpi: u32,
    pub scale: f32,
}

pub fn screen_at_cursor() -> Screen {
    // SAFETY: plain monitor queries into local out-params.
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let work = if GetMonitorInfoW(mon, &mut mi).as_bool() { mi.rcWork } else { RECT { left: 0, top: 0, right: 1024, bottom: 768 } };
        let (mut dx, mut dy) = (96u32, 96u32);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_err() || dx == 0 {
            dx = 96;
        }
        Screen { work, dpi: dx, scale: dx as f32 / 96.0 }
    }
}

/// Creates a hidden, centred dialog window with client size `(cw, ch)` (logical px).
/// `parent` owns it (no taskbar button; defaults to the hidden main window).
pub fn create(app: &App, title: &str, (cw, ch): (i32, i32), resizable: bool, scr: &Screen, parent: Option<HWND>, dlg: Rc<dyn Dlg>) -> Option<HWND> {
    let owner = parent.or_else(|| Some(app.hwnd.get()).filter(|h| !h.0.is_null()));
    // SAFETY: class registration + window creation on the UI thread.
    unsafe {
        if !REGISTERED.with(|r| r.replace(true)) {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(dlg_proc),
                hInstance: app.hinst,
                hIcon: LoadIconW(Some(app.hinst), PCWSTR(std::ptr::without_provenance::<u16>(1))).unwrap_or_default(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH((COLOR_3DFACE.0 + 1) as isize as *mut _),
                lpszClassName: CLASS,
                ..Default::default()
            };
            RegisterClassW(&wc);
        }
        // Resizable dialogs clip children (no flicker); fixed ones have group boxes, which only
        // draw their frame and need the parent to paint the background under them.
        let mut style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        if resizable {
            style |= WS_THICKFRAME | WS_MAXIMIZEBOX | WS_CLIPCHILDREN;
        }
        let ex = WS_EX_TOPMOST;
        let px = |v: i32| (v as f32 * scr.scale).round() as i32;
        let mut r = RECT { left: 0, top: 0, right: px(cw), bottom: px(ch) };
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, ex, scr.dpi);
        let (ow, oh) = (r.right - r.left, r.bottom - r.top);
        let x = (scr.work.left + (scr.work.right - scr.work.left - ow) / 2).max(scr.work.left);
        let y = (scr.work.top + (scr.work.bottom - scr.work.top - oh) / 2).max(scr.work.top);
        let min = if resizable { (ow * 2 / 3, oh * 2 / 3) } else { (0, 0) };
        let shell = Box::into_raw(Box::new(Shell { dlg, min, focus: Cell::new(HWND::default()) }));
        let t = wide(title);
        match CreateWindowExW(ex, CLASS, pcw(&t), style, x, y, ow, oh, owner, None, Some(app.hinst), Some(shell as *const _)) {
            Ok(h) => Some(h),
            Err(e) => {
                crate::log_err!("dialog window creation failed: {e}");
                None // ponytail: the Shell box leaks if creation fails before WM_NCCREATE; a rare failure path
            }
        }
    }
}

/// Shows the dialog and gives it the foreground (AttachThreadInput trick, as the overlay does).
pub fn present(h: HWND, focus: HWND) {
    // SAFETY: window calls on a window we own; the input-queue attach is always detached.
    unsafe {
        let _ = ShowWindow(h, SW_SHOW);
        let fg = GetForegroundWindow();
        let me = GetCurrentThreadId();
        let ftid = if fg.0.is_null() { 0 } else { GetWindowThreadProcessId(fg, None) };
        let attached = ftid != 0 && ftid != me && AttachThreadInput(me, ftid, true).as_bool();
        let _ = BringWindowToTop(h);
        let ok = SetForegroundWindow(h).as_bool();
        if attached {
            let _ = AttachThreadInput(me, ftid, false);
        }
        if !ok && GetForegroundWindow() != h {
            // Phantom Alt press grants the foreground right.
            keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
            let _ = SetForegroundWindow(h);
        }
        let target = if focus.0.is_null() { h } else { focus };
        let _ = SetFocus(Some(target));
    }
}

/// Brings an already-open dialog forward instead of opening a second copy.
pub fn raise_if_open(slot: &Cell<HWND>) -> bool {
    let h = slot.get();
    // SAFETY: IsWindow tolerates stale handles.
    if unsafe { IsWindow(Some(h)) }.as_bool() {
        present(h, HWND::default());
        return true;
    }
    false
}

/// Runs a nested message loop until `done` is set (the dialog's handler sets it via
/// [`finish`]). Everything else keeps being dispatched, so hotkeys, the tray and capture
/// continue to work. `disable` is an owner window that is disabled while the dialog is up.
/// WM_QUIT is re-posted for the main loop.
pub fn run_modal(dlg: HWND, disable: Option<HWND>, done: &Cell<bool>) {
    // SAFETY: standard message loop on the UI thread; no RefCell borrows are held by callers.
    unsafe {
        if let Some(o) = disable {
            let _ = EnableWindow(o, false);
        }
        let mut msg = MSG::default();
        while !done.get() {
            match GetMessageW(&mut msg, None, 0, 0).0 {
                0 => {
                    PostQuitMessage(msg.wParam.0 as i32);
                    break;
                }
                -1 => break,
                _ => {}
            }
            if IsDialogMessageW(dlg, &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Some(o) = disable {
            let _ = EnableWindow(o, true);
        }
    }
}

/// Marks the dialog finished and wakes the nested loop.
pub fn finish(h: HWND, done: &Cell<bool>) {
    done.set(true);
    // SAFETY: wake-up message so GetMessageW returns and the loop re-checks `done`.
    unsafe {
        let _ = PostMessageW(Some(h), WM_NULL, WPARAM(0), LPARAM(0));
    }
}

/// Destroys the dialog (call after `run_modal`, with the owner already re-enabled).
pub fn close(h: HWND) {
    // SAFETY: destroys a window of this thread.
    unsafe {
        let _ = DestroyWindow(h);
    }
}

// ---------------------------------------------------------------- fonts & controls

/// RAII GDI font.
pub struct Font(pub HFONT);

impl Font {
    pub fn new(face: &str, px: i32, weight: i32, italic: bool, underline: bool) -> Font {
        let f = wide(face);
        // SAFETY: GDI font creation; deleted in Drop.
        Font(unsafe { CreateFontW(-px.max(1), 0, 0, 0, weight, italic as u32, underline as u32, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, pcw(&f)) })
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            // SAFETY: the font is no longer used (windows are destroyed before the state drops).
            unsafe {
                let _ = DeleteObject(self.0.into());
            }
        }
    }
}

/// DPI scale + the dialog UI font (Segoe UI 9 pt scaled) + a control factory.
pub struct Ui {
    pub scale: f32,
    pub font: Font,
}

impl Ui {
    pub fn new(scale: f32) -> Ui {
        Ui { scale, font: Font::new("Segoe UI", (12.0 * scale).round() as i32, 400, false, false) }
    }

    /// Logical -> physical pixels.
    pub fn px(&self, v: i32) -> i32 {
        (v as f32 * self.scale).round() as i32
    }

    /// Child control at a logical rectangle. Creation order is the Tab order.
    #[allow(clippy::too_many_arguments)] // parent, class, text, styles, rect, id: one call site shape
    pub fn add(&self, parent: HWND, class: PCWSTR, text: &str, style: u32, ex: u32, (x, y, cw, ch): (i32, i32, i32, i32), id: i32) -> HWND {
        let t = wide(text);
        // SAFETY: child creation on the UI thread; the control id travels in the HMENU slot.
        unsafe {
            let h = CreateWindowExW(
                WINDOW_EX_STYLE(ex),
                class,
                pcw(&t),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
                self.px(x),
                self.px(y),
                self.px(cw),
                self.px(ch),
                Some(parent),
                Some(HMENU(id as isize as *mut _)),
                None,
                None,
            )
            .unwrap_or_default();
            if !h.0.is_null() {
                set_font(h, self.font.0);
            }
            h
        }
    }

    pub fn label(&self, parent: HWND, text: &str, r: (i32, i32, i32, i32)) -> HWND {
        self.add(parent, w!("STATIC"), text, SS_NOPREFIX, 0, r, -1)
    }

    pub fn button(&self, parent: HWND, text: &str, r: (i32, i32, i32, i32), id: i32, default: bool) -> HWND {
        let kind = if default { BS_DEFPUSHBUTTON } else { BS_PUSHBUTTON };
        self.add(parent, w!("BUTTON"), text, TAB | kind as u32, 0, r, id)
    }
}

pub fn set_font(h: HWND, f: HFONT) {
    send(h, WM_SETFONT, f.0 as usize, 1);
}

// ---------------------------------------------------------------- small Win32 wrappers

pub fn send(h: HWND, m: u32, w: usize, l: isize) -> isize {
    // SAFETY: plain SendMessage to a window of this thread; pointers in `l` are the caller's.
    unsafe { SendMessageW(h, m, Some(WPARAM(w)), Some(LPARAM(l))).0 }
}

pub fn get_text(h: HWND) -> String {
    // SAFETY: buffer sized from GetWindowTextLengthW.
    unsafe {
        let n = GetWindowTextLengthW(h).max(0) as usize;
        let mut buf = vec![0u16; n + 1];
        let got = GetWindowTextW(h, &mut buf).max(0) as usize;
        from_wide(buf.get(..got).unwrap_or(&[]))
    }
}

pub fn set_text(h: HWND, s: &str) {
    let t = wide(s);
    // SAFETY: NUL-terminated buffer.
    unsafe {
        let _ = SetWindowTextW(h, pcw(&t));
    }
}

pub fn put(h: HWND, x: i32, y: i32, cw: i32, ch: i32) {
    // SAFETY: moves a window of this dialog.
    unsafe {
        let _ = MoveWindow(h, x, y, cw.max(1), ch.max(1), true);
    }
}

pub fn client_size(h: HWND) -> (i32, i32) {
    let mut r = RECT::default();
    // SAFETY: out-param is local.
    unsafe {
        let _ = GetClientRect(h, &mut r);
    }
    (r.right - r.left, r.bottom - r.top)
}

pub fn show(h: HWND, visible: bool) {
    // SAFETY: visibility of a control.
    unsafe {
        let _ = ShowWindow(h, if visible { SW_SHOW } else { SW_HIDE });
    }
}

pub fn enable(h: HWND, on: bool) {
    // SAFETY: enable state of a control.
    unsafe {
        let _ = EnableWindow(h, on);
    }
}

pub fn loword(w: WPARAM) -> i32 {
    (w.0 & 0xFFFF) as i32
}

pub fn hiword(w: WPARAM) -> u32 {
    ((w.0 >> 16) & 0xFFFF) as u32
}

pub fn set_check(h: HWND, on: bool) {
    send(h, BM_SETCHECK, on as usize, 0);
}

pub fn is_checked(h: HWND) -> bool {
    send(h, 0xF0 /* BM_GETCHECK */, 0, 0) as usize == BST_CHECKED
}

pub fn invalidate(h: HWND) {
    // SAFETY: repaint request.
    unsafe {
        let _ = InvalidateRect(Some(h), None, true);
    }
}

// ---------------------------------------------------------------- shared key handling

/// Subclass for EDIT / RICHEDIT controls inside a dialog: Ctrl+Enter confirms, Esc cancels,
/// Ctrl+A selects all. With `rich_ids`, Ctrl+B / I / U post those command ids (bold, italic,
/// underline) to the parent instead of relying on the control's own shortcuts.
pub fn subclass_keys(h: HWND, rich_ids: Option<[i32; 3]>) {
    let data = rich_ids.map_or(0, |[b, i, u]| (b as usize) | (i as usize) << 16 | (u as usize) << 32 | 1 << 48);
    // SAFETY: subclass installed on a control we created; removed at WM_NCDESTROY.
    unsafe {
        let _ = SetWindowSubclass(h, Some(keys_proc), 1, data);
    }
}

fn post_cmd(h: HWND, id: i32) {
    // SAFETY: posts WM_COMMAND to the dialog (posted, so the control finishes its own handling first).
    unsafe {
        if let Ok(p) = GetParent(h) {
            let _ = PostMessageW(Some(p), WM_COMMAND, WPARAM(id as usize), LPARAM(0));
        }
    }
}

unsafe extern "system" fn keys_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM, _uid: usize, data: usize) -> LRESULT {
    let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
    let rich = data >> 48 & 1 == 1;
    let vk = w.0 as u32;
    match m {
        WM_KEYDOWN => {
            if vk == VK_RETURN.0 as u32 && ctrl {
                post_cmd(h, ID_OK);
                return LRESULT(0);
            }
            if vk == VK_ESCAPE.0 as u32 {
                post_cmd(h, ID_CANCEL);
                return LRESULT(0);
            }
            if ctrl && vk == b'A' as u32 {
                send(h, EM_SETSEL, 0, -1);
                return LRESULT(0);
            }
            if rich && ctrl {
                let id = match vk as u8 {
                    b'B' => Some(data & 0xFFFF),
                    b'I' => Some(data >> 16 & 0xFFFF),
                    b'U' => Some(data >> 32 & 0xFFFF),
                    _ => None,
                };
                if let Some(id) = id {
                    post_cmd(h, id as i32);
                    return LRESULT(0);
                }
            }
        }
        // Swallow the characters those keys would insert (Ctrl+Enter = LF, Esc, Ctrl+B/I(Tab)/U).
        WM_CHAR if w.0 == 0x0A || w.0 == 0x1B || (rich && ctrl && matches!(w.0, 0x02 | 0x09 | 0x15)) => return LRESULT(0),
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(h, Some(keys_proc), 1);
        }
        _ => {}
    }
    DefSubclassProc(h, m, w, l)
}
