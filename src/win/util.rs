//! Small shared helpers: wide strings, time, thread entry points, cross-thread posting.
use std::panic::{catch_unwind, AssertUnwindSafe};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// NUL-terminated UTF-16.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn pcw(v: &[u16]) -> PCWSTR {
    PCWSTR(v.as_ptr())
}

/// UTF-16 up to the first NUL.
pub fn from_wide(v: &[u16]) -> String {
    let end = v.iter().position(|&c| c == 0).unwrap_or(v.len());
    String::from_utf16_lossy(&v[..end])
}

pub fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

static UI_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// The hidden main window every worker posts results to.
pub fn set_ui_hwnd(h: HWND) {
    UI_HWND.store(h.0 as isize, std::sync::atomic::Ordering::Relaxed);
}

pub fn ui_hwnd() -> SendHwnd {
    SendHwnd(UI_HWND.load(std::sync::atomic::Ordering::Relaxed))
}

/// `HWND` is `!Send`; threads exchange it as a plain integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendHwnd(pub isize);
impl SendHwnd {
    pub fn new(h: HWND) -> Self {
        SendHwnd(h.0 as isize)
    }
    pub fn get(self) -> HWND {
        HWND(self.0 as *mut core::ffi::c_void)
    }
}

/// Post an owned value to a window; the receiver MUST reconstruct it with [`take_box`].
/// If the post fails the value is dropped here (no leak).
pub fn post_box<T: Send + 'static>(hwnd: SendHwnd, msg: u32, wparam: usize, val: T) -> bool {
    let raw = Box::into_raw(Box::new(val));
    // SAFETY: PostMessageW with a heap pointer in LPARAM; reclaimed on failure below or by take_box.
    let ok = unsafe { PostMessageW(Some(hwnd.get()), msg, WPARAM(wparam), LPARAM(raw as isize)) }.is_ok();
    if !ok {
        // SAFETY: the pointer was just created by Box::into_raw and never delivered.
        drop(unsafe { Box::from_raw(raw) });
    }
    ok
}

/// # Safety
/// `lparam` must come from a message posted with [`post_box::<T>`], and be taken exactly once.
pub unsafe fn take_box<T>(lparam: LPARAM) -> Option<Box<T>> {
    let p = lparam.0 as *mut T;
    if p.is_null() {
        None
    } else {
        Some(Box::from_raw(p))
    }
}

/// Thread entry wrapper (spec 19.1): a panic is logged, never propagated. Returns whether
/// the body completed normally.
pub fn guarded<F: FnOnce()>(name: &str, f: F) -> bool {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(()) => true,
        Err(_) => {
            crate::log_err!("thread '{name}' panicked (see panic hook output above)");
            false
        }
    }
}

pub fn env_dir(var: &str) -> Option<std::path::PathBuf> {
    std::env::var_os(var).map(std::path::PathBuf::from)
}

/// Developer/test isolation: `CLIP4_PROFILE=name` moves every file, registry key and the
/// single-instance mutex into a private sandbox so a test run never touches real data.
pub fn profile() -> Option<String> {
    std::env::var("CLIP4_PROFILE").ok().filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

/// `%APPDATA%\clip4`
pub fn data_dir() -> std::path::PathBuf {
    match profile() {
        Some(p) => std::env::temp_dir().join(format!("clip4-{p}")).join("data"),
        None => env_dir("APPDATA").unwrap_or_else(std::env::temp_dir).join("clip4"),
    }
}

/// `%LOCALAPPDATA%\clip4`
pub fn local_dir() -> std::path::PathBuf {
    match profile() {
        Some(p) => std::env::temp_dir().join(format!("clip4-{p}")).join("local"),
        None => env_dir("LOCALAPPDATA").unwrap_or_else(std::env::temp_dir).join("clip4"),
    }
}

pub fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

pub fn unhex20(s: &str) -> Option<[u8; 20]> {
    if s.len() != 40 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Runs a window-procedure body so a panic can never unwind across the FFI boundary
/// (which would abort the whole process): it is logged and the message falls through to
/// `DefWindowProcW` instead (spec 19.1).
pub fn wndproc_guard(h: HWND, m: u32, w: WPARAM, l: LPARAM, f: impl FnOnce() -> windows::Win32::Foundation::LRESULT) -> windows::Win32::Foundation::LRESULT {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            crate::log_err!("window procedure panicked on message {m:#x}; continuing");
            // SAFETY: forwarding the original message.
            unsafe { windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(h, m, w, l) }
        }
    }
}
