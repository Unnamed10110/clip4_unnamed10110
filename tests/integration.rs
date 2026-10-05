//! Real-clipboard integration tests (spec 22.2).
//!
//! They start a sandboxed `clip4.exe` (`CLIP4_PROFILE`, so no real data or registry is touched)
//! and drive it through the genuine Windows clipboard, so they overwrite YOUR clipboard and
//! are `#[ignore]`d by default:
//!
//!     cargo test --test integration -- --ignored --test-threads=1
//!
//! Do not touch the keyboard/clipboard while they run.
#![cfg(windows)]

use clip4::model::*;
use clip4::win::blob::BlobStore;
use clip4::win::reg::Key;
use clip4::win::util::{wide, pcw};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ------------------------------------------------------------------ sandboxed app

struct Sandbox {
    child: Child,
    profile: String,
    data: PathBuf,
    log: PathBuf,
}

impl Sandbox {
    fn start(name: &str) -> Sandbox {
        let profile = format!("it{}{}", std::process::id(), name);
        let root = std::env::temp_dir().join(format!("clip4-{profile}"));
        let _ = std::fs::remove_dir_all(&root);
        if let Some(k) = Key::create(&format!("Software\\clip4-{profile}")) {
            k.set_dword("DebugLog", 1);
            k.set_dword("Sound", 0);
            // Private hotkeys (Ctrl+Alt+Shift+F13..F16) so a real clip2/clip4 or any other app that
            // owns the defaults can never interfere with, or be triggered by, these tests.
            for (name, vk) in [("Hotkey", 0x7Cu32), ("PasteFocused", 0x7D), ("PasteClipboard", 0x7E), ("CopyFocused", 0x7F)] {
                k.set_dword(&format!("{name}Modifiers"), 7);
                k.set_dword(&format!("{name}VkCode"), vk);
            }
        }
        let child = Command::new(env!("CARGO_BIN_EXE_clip4")).env("CLIP4_PROFILE", &profile).spawn().expect("spawn clip4");
        let sb = Sandbox { child, profile, data: root.join("data"), log: root.join("local").join("clip4.log") };
        assert!(sb.wait_log("starting", 10), "clip4 did not start");
        std::thread::sleep(Duration::from_millis(1200)); // listener + hotkeys + history load
        sb
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_log(&self, needle: &str, secs: u64) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if self.log_text().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Decrypts and decodes the saved history (the same code path clip4 uses at startup).
    fn history(&self) -> Vec<Item> {
        let Ok(file) = std::fs::read(self.data.join("history.dat")) else { return Vec::new() };
        let Some(blob) = clip4::codec::unwrap_file(&file) else { return Vec::new() };
        let Some(inner) = clip4::win::dpapi::unprotect(blob) else { return Vec::new() };
        clip4::codec::decode_inner(&inner, &BlobStore::new(self.data.join("blobs")))
    }

    fn texts(&self) -> Vec<String> {
        self.history().iter().filter_map(|i| i.text()).collect()
    }

    /// Polls the (debounced, 1.5 s) saved history until `pred` holds.
    fn wait_history(&self, secs: u64, pred: impl Fn(&[Item]) -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if pred(&self.history()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        false
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        clip4::win::reg::delete_tree(&format!("Software\\clip4-{}", self.profile));
        let _ = std::fs::remove_dir_all(self.data.parent().unwrap_or(&self.data));
    }
}

// ------------------------------------------------------------------ raw clipboard helpers

fn owner_window() -> HWND {
    // A hidden message-only window: a REAL owner, as lesson 18.22 demands.
    let cls = wide("STATIC");
    // SAFETY: plain window creation.
    unsafe { CreateWindowExW(WINDOW_EX_STYLE(0), pcw(&cls), pcw(&cls), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None).expect("owner window") }
}

fn open_retry(owner: HWND, tries: u32, gap_ms: u64) -> Result<u32, u32> {
    for i in 0..tries {
        // SAFETY: plain call.
        if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
            return Ok(i); // failed attempts before success
        }
        std::thread::sleep(Duration::from_millis(gap_ms));
    }
    Err(tries)
}

fn fmt_id(f: &FormatKey) -> u32 {
    match f {
        FormatKey::Standard(i) => *i,
        FormatKey::Registered(n) => {
            let w = wide(n);
            // SAFETY: valid NUL-terminated name.
            unsafe { RegisterClipboardFormatW(pcw(&w)) }
        }
    }
}

fn global(bytes: &[u8]) -> HGLOBAL {
    // SAFETY: allocate + fill.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len() + 2).expect("alloc");
        let p = GlobalLock(h) as *mut u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        h
    }
}

/// Replaces the clipboard with `formats` and closes it again.
fn set_formats(owner: HWND, formats: &[(FormatKey, Vec<u8>)]) {
    open_retry(owner, 100, 10).expect("open clipboard");
    // SAFETY: open clipboard with a real owner.
    unsafe {
        EmptyClipboard().expect("empty");
        for (k, b) in formats {
            SetClipboardData(fmt_id(k), Some(HANDLE(global(b).0))).expect("set");
        }
        let _ = CloseClipboard();
    }
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().chain(std::iter::once(0)).flat_map(|u| u.to_le_bytes()).collect()
}

fn text_fmt(s: &str) -> (FormatKey, Vec<u8>) {
    (FormatKey::Standard(CF_UNICODETEXT), utf16(s))
}

/// A `w` x `h` 32bpp bottom-up DIB.
fn dib(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(40 + (w * h * 4) as usize);
    v.extend_from_slice(&40u32.to_le_bytes());
    v.extend_from_slice(&(w as i32).to_le_bytes());
    v.extend_from_slice(&(h as i32).to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&32u16.to_le_bytes());
    v.extend_from_slice(&[0u8; 24]);
    v.resize(40 + (w * h * 4) as usize, 0x5A);
    v
}

/// Every format currently on the clipboard as (id, bytes).
fn read_all(owner: HWND) -> Vec<(u32, Vec<u8>)> {
    open_retry(owner, 100, 10).expect("open clipboard");
    let mut out = Vec::new();
    // SAFETY: open clipboard; HGLOBAL formats only.
    unsafe {
        let mut f = 0;
        loop {
            f = EnumClipboardFormats(f);
            if f == 0 {
                break;
            }
            if matches!(f, CF_BITMAP | CF_PALETTE | CF_ENHMETAFILE | CF_METAFILEPICT) || (0x200..0x400).contains(&f) {
                continue;
            }
            if let Ok(h) = GetClipboardData(f) {
                let hg = HGLOBAL(h.0);
                let n = GlobalSize(hg);
                let p = GlobalLock(hg) as *const u8;
                if !p.is_null() {
                    out.push((f, std::slice::from_raw_parts(p, n).to_vec()));
                    let _ = GlobalUnlock(hg);
                }
            }
        }
        let _ = CloseClipboard();
    }
    out
}

fn lock_times(log: &str) -> Vec<u32> {
    log.lines()
        .filter_map(|l| l.split("lock ").nth(1))
        .filter_map(|r| r.split(" ms").next())
        .filter_map(|n| n.trim().parse().ok())
        .collect()
}

// ------------------------------------------------------------------ tests

#[test]
#[ignore = "uses the real clipboard"]
fn contention_copy_is_recorded_after_release() {
    let app = Sandbox::start("a");
    let owner = owner_window();
    set_formats(owner, &[text_fmt("copied-during-contention")]);
    // Another "process" grabs the clipboard immediately and holds it for 1.5 s.
    open_retry(owner, 100, 1).expect("hold");
    std::thread::sleep(Duration::from_millis(1500));
    // SAFETY: release.
    unsafe {
        let _ = CloseClipboard();
    }
    assert!(
        app.wait_history(8, |h| h.iter().any(|i| i.text().as_deref() == Some("copied-during-contention"))),
        "the copy made during contention was lost\n{}",
        app.log_text()
    );
}

#[test]
#[ignore = "uses the real clipboard"]
fn lock_time_for_10mb_image_is_short() {
    let app = Sandbox::start("b");
    let owner = owner_window();
    set_formats(owner, &[(FormatKey::Standard(CF_DIB), dib(2000, 1250))]); // ~10 MB
    assert!(app.wait_log("captured seq", 10), "image was not captured\n{}", app.log_text());
    let t = lock_times(&app.log_text());
    assert!(!t.is_empty());
    assert!(t.iter().all(|&ms| ms < 30), "clipboard lock held {t:?} ms (budget 30 ms)");
}

#[test]
#[ignore = "uses the real clipboard"]
fn large_image_captures_do_not_starve_other_apps() {
    let app = Sandbox::start("c");
    let owner = owner_window();
    let img = dib(1000, 1000); // 4 MB
    let mut worst = 0;
    for _ in 0..25 {
        // "copy" in a second app, then "paste" it back, each with a 10 ms x 20 retry window.
        open_retry(owner, 20, 10).expect("copy: OpenClipboard failed beyond its retry window");
        // SAFETY: open clipboard, real owner.
        unsafe {
            EmptyClipboard().expect("empty");
            SetClipboardData(CF_DIB, Some(HANDLE(global(&img).0))).expect("set");
            let _ = CloseClipboard();
        }
        let failed = open_retry(owner, 20, 10).expect("paste: OpenClipboard failed beyond its retry window");
        worst = worst.max(failed);
        // SAFETY: close.
        unsafe {
            let _ = CloseClipboard();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(worst <= 12, "another app needed {worst} extra attempts to open the clipboard");
    drop(app);
}

#[test]
#[ignore = "uses the real clipboard"]
fn excluded_content_is_never_recorded() {
    let app = Sandbox::start("d");
    let owner = owner_window();
    let secret = "super-secret-value-12345";
    set_formats(owner, &[text_fmt(secret), (FormatKey::reg("ExcludeClipboardContentFromMonitorProcessing"), vec![1, 0, 0, 0])]);
    std::thread::sleep(Duration::from_millis(600));
    // A normal copy afterwards proves capture still works, and flushes a save.
    set_formats(owner, &[text_fmt("ordinary-copy")]);
    assert!(app.wait_history(8, |h| h.iter().any(|i| i.text().as_deref() == Some("ordinary-copy"))), "{}", app.log_text());
    assert!(!app.texts().iter().any(|t| t.contains(secret)), "excluded content reached the history");
    assert!(!app.log_text().contains(secret), "excluded content reached the log");
    // CanIncludeInClipboardHistory = 0 is honoured too.
    set_formats(owner, &[text_fmt("second-secret-777"), (FormatKey::reg("CanIncludeInClipboardHistory"), vec![0, 0, 0, 0])]);
    std::thread::sleep(Duration::from_millis(600));
    set_formats(owner, &[text_fmt("ordinary-two")]);
    assert!(app.wait_history(8, |h| h.iter().any(|i| i.text().as_deref() == Some("ordinary-two"))));
    assert!(!app.texts().iter().any(|t| t.contains("second-secret")));
}

/// A visible top-level window of ours that can safely receive a swap-paste.
fn foreground_target() -> Option<HWND> {
    let cls = wide("EDIT");
    // SAFETY: plain window creation + foreground request.
    unsafe {
        let h = CreateWindowExW(WINDOW_EX_STYLE(0), pcw(&cls), pcw(&wide("clip4-it-target")), WS_OVERLAPPEDWINDOW | WS_VISIBLE | WINDOW_STYLE(ES_MULTILINE as u32), 100, 100, 400, 200, None, None, None, None).ok()?;
        for _ in 0..10 {
            keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
            let _ = SetForegroundWindow(h);
            let _ = SetFocus(Some(h));
            std::thread::sleep(Duration::from_millis(100));
            if GetForegroundWindow() == h {
                return Some(h);
            }
        }
        let _ = DestroyWindow(h);
        None
    }
}

/// The sandbox hotkeys: Ctrl+Alt+Shift + F13 (overlay), F14 (keystroke paste), F15 (swap paste).
fn hotkey(f_vk: u16) -> [VIRTUAL_KEY; 4] {
    [VK_CONTROL, VK_MENU, VK_SHIFT, VIRTUAL_KEY(f_vk)]
}

fn send_chord(keys: &[VIRTUAL_KEY]) {
    let mk = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, time: 0, dwExtraInfo: 0 } },
    };
    let mut v: Vec<INPUT> = keys.iter().map(|&k| mk(k, false)).collect();
    v.extend(keys.iter().rev().map(|&k| mk(k, true)));
    // SAFETY: fully initialised inputs.
    unsafe {
        SendInput(&v, std::mem::size_of::<INPUT>() as i32);
    }
}

#[test]
#[ignore = "uses the real clipboard and keyboard"]
fn swap_paste_restores_the_clipboard_byte_identically() {
    let app = Sandbox::start("e");
    let owner = owner_window();
    // 1. Record "item-to-paste" as the newest history item.
    set_formats(owner, &[text_fmt("item-to-paste")]);
    assert!(app.wait_history(8, |h| h.iter().any(|i| i.text().as_deref() == Some("item-to-paste"))));
    // 2. The user's current clipboard: several formats, NOT recorded (excluded).
    let prior = vec![
        text_fmt("the user's prior clipboard"),
        (FormatKey::reg("HTML Format"), b"Version:0.9\r\n<html>prior</html>".to_vec()),
        (FormatKey::reg("clip4 test custom"), vec![1, 2, 3, 4, 5, 6, 7]),
        (FormatKey::reg("ExcludeClipboardContentFromMonitorProcessing"), vec![1, 0, 0, 0]),
    ];
    set_formats(owner, &prior);
    std::thread::sleep(Duration::from_millis(500));
    let before = read_all(owner);
    // 3. Ctrl+Shift+F11 = paste via clipboard swap into the foreground window, then restore.
    let Some(target) = foreground_target() else {
        eprintln!("could not take the foreground safely; test skipped");
        return;
    };
    send_chord(&hotkey(0x7E));
    assert!(app.wait_log("sending Ctrl+V", 6), "swap paste never started\n{}", app.log_text());
    std::thread::sleep(Duration::from_millis(1500)); // 420 ms settle + restore
    let after = read_all(owner);
    // SAFETY: cleanup.
    unsafe {
        let _ = DestroyWindow(target);
    }
    assert!(!app.log_text().contains("CLIPBOARD RESTORE FAILED"));
    let norm = |v: Vec<(u32, Vec<u8>)>| {
        let mut v: Vec<_> = v.into_iter().filter(|(id, _)| *id >= 0xC000 || *id == CF_UNICODETEXT).collect();
        v.sort();
        v
    };
    assert_eq!(norm(before), norm(after), "the user's clipboard was not restored byte-identically");
}

// ------------------------------------------------------------------ hook liveness

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

static PROBE_AT: AtomicU64 = AtomicU64::new(0); // micros (since T0) when the probe key arrived
static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn micros_now() -> u64 {
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64 + 1
}

unsafe extern "system" fn probe_proc(h: HWND, m: u32, w: windows::Win32::Foundation::WPARAM, l: windows::Win32::Foundation::LPARAM) -> windows::Win32::Foundation::LRESULT {
    if m == WM_KEYDOWN && w.0 == 0x58 {
        PROBE_AT.store(micros_now(), Ordering::SeqCst);
    }
    DefWindowProcW(h, m, w, l)
}

fn probe_window() -> Option<HWND> {
    let cls = wide("clip4_it_probe");
    // SAFETY: class + window creation, foreground request.
    unsafe {
        let wc = WNDCLASSW { lpfnWndProc: Some(probe_proc), lpszClassName: pcw(&cls), ..Default::default() };
        RegisterClassW(&wc);
        let h = CreateWindowExW(WINDOW_EX_STYLE(0), pcw(&cls), pcw(&cls), WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 100, 300, 150, None, None, None, None).ok()?;
        for _ in 0..10 {
            keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
            let _ = SetForegroundWindow(h);
            std::thread::sleep(Duration::from_millis(100));
            if GetForegroundWindow() == h {
                return Some(h);
            }
        }
        let _ = DestroyWindow(h);
        None
    }
}

/// Sends `n` probe keystrokes and returns each one's delivery latency in microseconds.
fn probe_latencies(n: usize) -> Vec<u64> {
    let mut out = Vec::new();
    let mut msg = MSG::default();
    for _ in 0..n {
        PROBE_AT.store(0, Ordering::SeqCst);
        let sent = micros_now();
        let key = |up: bool| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0x58), wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, time: 0, dwExtraInfo: 0 } },
        };
        // SAFETY: inputs are fully initialised; this thread owns the window, so it pumps messages.
        unsafe {
            SendInput(&[key(false), key(true)], std::mem::size_of::<INPUT>() as i32);
            let end = Instant::now() + Duration::from_millis(200);
            while PROBE_AT.load(Ordering::SeqCst) == 0 && Instant::now() < end {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(Duration::from_micros(200));
            }
        }
        let at = PROBE_AT.load(Ordering::SeqCst);
        if at != 0 {
            out.push(at.saturating_sub(sent));
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    out
}

#[test]
#[ignore = "uses the real clipboard and keyboard"]
fn keystroke_latency_stays_low_during_a_long_paste_and_hotkeys_survive() {
    let _ = AtomicI64::new(0);
    let app = Sandbox::start("f");
    let owner = owner_window();
    // The newest history item is long, so "paste as keystrokes" types for a good while.
    set_formats(owner, &[text_fmt(&"The quick brown fox jumps over the lazy dog. ".repeat(60))]);
    assert!(app.wait_history(8, |h| !h.is_empty()));
    let Some(win) = probe_window() else {
        eprintln!("could not take the foreground safely; test skipped");
        return;
    };
    // Keystrokes only reach us while we hold the foreground; retake it if something stole it.
    let mut base = probe_latencies(60);
    for _ in 0..4 {
        if base.len() >= 50 {
            break;
        }
        // SAFETY: foreground request on our own window.
        unsafe {
            keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
            let _ = SetForegroundWindow(win);
        }
        std::thread::sleep(Duration::from_millis(300));
        base = probe_latencies(60);
    }
    if base.len() < 50 {
        eprintln!("the probe window could not keep the foreground; test skipped");
        return;
    }
    // The keystroke-paste hotkey types the item into the foreground window on clip4's paste thread (~2700 chars).
    send_chord(&hotkey(0x7D));
    std::thread::sleep(Duration::from_millis(150));
    let busy = probe_latencies(60);
    // The overlay hotkey must still work afterwards.
    std::thread::sleep(Duration::from_millis(2500));
    send_chord(&hotkey(0x7C));
    let shown = app.wait_log("overlay show", 5);
    // SAFETY: cleanup.
    unsafe {
        let _ = DestroyWindow(win);
    }
    let med = |v: &[u64]| {
        let mut s = v.to_vec();
        s.sort_unstable();
        s.get(s.len() / 2).copied().unwrap_or(0)
    };
    let (mb, mx) = (med(&base), med(&busy));
    eprintln!("probe latency median: idle {mb} us, during paste {mx} us (n={}/{})", base.len(), busy.len());
    assert!(app.log_text().contains("hotkey KeystrokePaste"), "the keystroke-paste hotkey never fired
{}", app.log_text());
    assert!(!busy.is_empty(), "no keystrokes were delivered during the paste");
    assert!(mx < mb + 5_000, "typing latency grew by more than 5 ms during a paste: {mb} -> {mx} us");
    assert!(shown, "the overlay hotkey stopped working after the paste\n{}", app.log_text());
}
