//! Cross-thread messages delivered to the UI thread's hidden main window.
use super::settings::Action;
use crate::model::{FormatKey, Item, Payload};
use windows::Win32::UI::WindowsAndMessaging::WM_APP;

/// Boxed [`UiMsg`] in LPARAM (see `util::post_box`).
pub const WM_UI: u32 = WM_APP + 1;
/// Tray icon callback.
pub const WM_TRAY: u32 = WM_APP + 2;
/// Posted by the hook thread; WPARAM = `Action as usize`.
pub const WM_HOOK_HOTKEY: u32 = WM_APP + 3;
/// A second instance asks the running one to show the overlay.
pub const WM_SHOW_OVERLAY: u32 = WM_APP + 4;
/// Posted to the paste thread's message-only window to wake its loop.
pub const WM_PASTE_WAKE: u32 = WM_APP + 5;
/// Posted to the hook thread to force a re-install.
pub const WM_HOOK_REARM: u32 = WM_APP + 6;

pub enum UiMsg {
    /// A built item from the capture worker (id not yet assigned). `more`: stage 2 will follow.
    Captured { seq: u32, item: Item, lock_ms: u32, more: bool },
    /// Stage 2 of a capture: formats to add to item `id` (the ones it does not have yet).
    Completed { id: u64, formats: Vec<(FormatKey, Payload)> },
    /// The clipboard could not be opened; retry later.
    /// `unreadable`: the clipboard opened but the source handed nothing over (short retries);
    /// otherwise the clipboard could not be opened (long retries).
    CaptureBusy { seq: u32, unreadable: bool },
    /// The snapshot was safely copied (or deliberately discarded); the sequence is consumed.
    CaptureConsumed { seq: u32 },
    Loaded { items: Vec<Item>, note: Option<String> },
    Saved { ok: bool },
    Thumb { id: u64, w: u32, h: u32, bgra: Vec<u8> },
    /// Image dimensions label for a dehydrated item.
    PasteResult { ok: bool, note: String },
    Notice(String),
    /// Text obtained by "copy from focused control".
    FocusedText { text: Option<String>, via_uia: bool },
    /// A global hotkey fired via the hook thread or RegisterHotKey.
    Hotkey(Action),
}

/// Posts a message to the UI thread's main window.
pub fn post_ui(m: UiMsg) -> bool {
    super::util::post_box(super::util::ui_hwnd(), WM_UI, 0, m)
}
