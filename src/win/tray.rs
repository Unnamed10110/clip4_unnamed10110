//! Notification-area icon (spec 17).
use super::msg::WM_TRAY;
use super::util::wide;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const ID: u32 = 1;

#[allow(clippy::manual_dangling_ptr)] // MAKEINTRESOURCE(1): an integer id, not a real pointer
fn icon() -> HICON {
    // SAFETY: resource icon 1 is embedded by build.rs; fall back to the stock icon if absent.
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        match LoadImageW(Some(hinst.into()), PCWSTR(1 as *const u16), IMAGE_ICON, cx, cy, LR_SHARED) {
            Ok(h) => HICON(h.0),
            Err(_) => LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        }
    }
}

fn base(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: ID, ..Default::default() }
}

fn copy_str(dst: &mut [u16], s: &str) {
    let w = wide(s);
    let n = w.len().min(dst.len());
    dst[..n].copy_from_slice(&w[..n]);
    if let Some(last) = dst.get_mut(n.saturating_sub(1)) {
        *last = 0;
    }
}

/// Adds (or re-adds, after Explorer restarts) the tray icon.
pub fn add(hwnd: HWND, tip: &str) -> bool {
    let mut d = base(hwnd);
    d.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    d.uCallbackMessage = WM_TRAY;
    d.hIcon = icon();
    copy_str(&mut d.szTip, tip);
    // SAFETY: fully initialised NOTIFYICONDATAW.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &base(hwnd));
        Shell_NotifyIconW(NIM_ADD, &d).as_bool()
    }
}

pub fn remove(hwnd: HWND) {
    // SAFETY: as above.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &base(hwnd));
    }
}

/// Non-modal balloon notice.
pub fn notice(hwnd: HWND, title: &str, text: &str) {
    let mut d = base(hwnd);
    d.uFlags = NIF_INFO;
    d.dwInfoFlags = NIIF_INFO;
    copy_str(&mut d.szInfoTitle, title);
    copy_str(&mut d.szInfo, text);
    // SAFETY: as above.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
    }
}
