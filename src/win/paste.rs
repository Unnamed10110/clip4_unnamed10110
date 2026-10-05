//! Paste engine (spec 12). All sequencing and sleeping happens on the paste thread:
//! never on the UI thread, never on the hook thread (lesson 18.2).

use super::blob::BlobStore;
use super::clipboard::{self, Backup};
use super::msg::{post_ui, UiMsg, WM_PASTE_WAKE};
use super::util::{guarded, now_unix_ms, pcw, wide, SendHwnd};
use crate::model::*;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// Tunable delays (spec 12.1 / 12.5; measured in clip2).
const AFTER_FOCUS_MS: u64 = 40;
const AFTER_MODS_MS: u64 = 20;
const SETTLE_AFTER_PASTE_MS: u64 = 420;
const XL_AFTER_SET: u64 = 30;
const XL_AFTER_F2: u64 = 180;
const XL_AFTER_PASTE: u64 = 220;
const XL_AFTER_ENTER: u64 = 150;
const WATCHDOG_MS: i64 = 60_000;

pub enum Job {
    /// Standard paste. `restore` = clipboard swap: back up first, restore afterwards.
    Clipboard { formats: Vec<(FormatKey, Payload)>, target: SendHwnd, restore: bool },
    /// Mixed multi-paste: one clipboard swap per step, always restoring the user's clipboard.
    Sequence { steps: Vec<Vec<(FormatKey, Payload)>>, target: SendHwnd },
    /// Type text as Unicode keystrokes without touching the clipboard.
    Keystrokes { text: String, target: Option<SendHwnd> },
    /// F2 / paste text / Enter for each text.
    Excel { texts: Vec<String>, target: SendHwnd },
    /// Put these formats on the clipboard and stop (no Ctrl+V, no restore).
    SetOnly { formats: Vec<(FormatKey, Payload)> },
    /// Ctrl+C in the focused window; falls back to Ctrl+A, Ctrl+C (spec 15).
    SyntheticCopy,
    /// Expand a snippet (reading `{{clipboard}}` here, off the UI thread) and paste it.
    Snippet { snippet: crate::snippets_fmt::Snippet, now: crate::snippets_fmt::Now, target: SendHwnd },
}

struct Busy {
    flag: AtomicBool,
    since: AtomicI64,
}

/// Clears the busy flag on every exit path (lesson 18.3).
struct BusyGuard<'a>(&'a Busy);
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.flag.store(false, Ordering::SeqCst);
    }
}

pub struct Paster {
    slot: Arc<Mutex<Option<Job>>>,
    busy: Arc<Busy>,
    wake: SendHwnd,
}

impl Paster {
    /// Spawns the paste thread and waits for its message-only window.
    pub fn start(blobs: BlobStore) -> Option<Paster> {
        let slot: Arc<Mutex<Option<Job>>> = Arc::new(Mutex::new(None));
        let busy = Arc::new(Busy { flag: AtomicBool::new(false), since: AtomicI64::new(0) });
        let (tx, rx) = std::sync::mpsc::channel::<isize>();
        let (s2, b2) = (slot.clone(), busy.clone());
        std::thread::Builder::new()
            .name("paste".into())
            .spawn(move || {
                guarded("paste", || thread_main(blobs, s2, b2, tx));
            })
            .ok()?;
        let wake = rx.recv_timeout(Duration::from_secs(5)).ok()?;
        clipboard::set_own_owner(SendHwnd(wake).get());
        Some(Paster { slot, busy, wake: SendHwnd(wake) })
    }

    /// Queue of depth 1: a request while busy is dropped with a trace.
    pub fn submit(&self, job: Job) -> bool {
        let now = now_unix_ms();
        if self.busy.flag.swap(true, Ordering::SeqCst) {
            if now - self.busy.since.load(Ordering::SeqCst) > WATCHDOG_MS {
                crate::log_err!("paste watchdog: previous paste still marked busy after 60 s; accepting new request");
            } else {
                crate::log_info!("paste request dropped: another paste is in progress");
                return false;
            }
        }
        self.busy.since.store(now, Ordering::SeqCst);
        if let Ok(mut g) = self.slot.lock() {
            *g = Some(job);
        }
        // SAFETY: posting to the paste thread's window.
        unsafe {
            let _ = PostMessageW(Some(self.wake.get()), WM_PASTE_WAKE, WPARAM(0), LPARAM(0));
        }
        true
    }

    pub fn is_busy(&self) -> bool {
        self.busy.flag.load(Ordering::SeqCst)
    }
}

unsafe extern "system" fn paste_wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // WM_DESTROYCLIPBOARD, WM_RENDER* etc. need no handling, but the window must exist and
    // its thread must keep pumping so another app's EmptyClipboard never stalls on us.
    DefWindowProcW(h, m, w, l)
}

fn thread_main(blobs: BlobStore, slot: Arc<Mutex<Option<Job>>>, busy: Arc<Busy>, ready: std::sync::mpsc::Sender<isize>) {
    // SAFETY: standard message-only window creation on this thread.
    let hwnd = unsafe {
        let hinst = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
        let cls = wide("clip4_paste");
        let wc = WNDCLASSW { lpfnWndProc: Some(paste_wndproc), hInstance: hinst.into(), lpszClassName: pcw(&cls), ..Default::default() };
        RegisterClassW(&wc);
        match CreateWindowExW(WINDOW_EX_STYLE(0), pcw(&cls), pcw(&cls), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hinst.into()), None) {
            Ok(h) => h,
            Err(e) => {
                crate::log_err!("paste window creation failed: {e}");
                return;
            }
        }
    };
    let _ = ready.send(hwnd.0 as isize);
    let ctx = Ctx { hwnd, blobs };
    let mut msg = MSG::default();
    // SAFETY: standard loop.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        // SAFETY: standard dispatch.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let job = slot.lock().ok().and_then(|mut g| g.take());
        if let Some(job) = job {
            let _guard = BusyGuard(&busy);
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctx.run(job)));
            let (ok, note) = match r {
                Ok(Ok(())) => (true, String::new()),
                Ok(Err(e)) => (false, e),
                Err(_) => (false, "internal error while pasting".to_string()),
            };
            if !ok {
                crate::log_warn!("paste aborted: {note}");
            }
            post_ui(UiMsg::PasteResult { ok, note });
            // Settle: the target may still be reading the clipboard.
            sleep_pump(SETTLE_AFTER_PASTE_MS);
        }
    }
}

/// Sleeps while still pumping this thread's messages (keeps clipboard-owner callbacks flowing).
fn sleep_pump(ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    loop {
        let now = Instant::now();
        if now >= end {
            break;
        }
        let left = (end - now).as_millis().min(u32::MAX as u128) as u32;
        // SAFETY: wait for input or timeout, then drain.
        unsafe {
            MsgWaitForMultipleObjectsEx(None, left, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            let mut m = MSG::default();
            while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
                if m.message == WM_QUIT {
                    return;
                }
                let _ = TranslateMessage(&m);
                DispatchMessageW(&m);
            }
        }
    }
}

struct Ctx {
    hwnd: HWND,
    blobs: BlobStore,
}

type R = Result<(), String>;

impl Ctx {
    fn run(&self, job: Job) -> R {
        match job {
            Job::Clipboard { formats, target, restore } => self.clipboard_paste(formats, target.get(), restore),
            Job::Sequence { steps, target } => self.sequence(steps, target.get()),
            Job::Keystrokes { text, target } => {
                if let Some(t) = target {
                    self.focus(t.get())?;
                    sleep_pump(AFTER_FOCUS_MS);
                }
                release_modifiers();
                sleep_pump(AFTER_MODS_MS);
                crate::log_dbg!("typing {} chars into {}", text.chars().count(), clipboard::process_name_of_window(foreground()));
                type_text(&text)
            }
            Job::Excel { texts, target } => self.excel(texts, target.get()),
            Job::SetOnly { formats } => self.set_clipboard(&formats),
            Job::SyntheticCopy => self.synthetic_copy(),
            Job::Snippet { snippet, now, target } => {
                let clip = if super::commands::snippet_needs_clipboard(&snippet) { clipboard::read_text_now(self.hwnd).unwrap_or_default() } else { String::new() };
                let formats = super::commands::snippet_formats(&snippet, &now, &clip);
                self.clipboard_paste(formats, target.get(), false)
            }
        }
    }

    /// Reads dehydrated payloads from disk — BEFORE the clipboard is opened (lesson 18.1).
    fn hydrate(&self, formats: &[(FormatKey, Payload)]) -> Vec<(FormatKey, Vec<u8>)> {
        let mut out = Vec::new();
        for (k, p) in formats {
            match p {
                Payload::Inline(b) => out.push((k.clone(), b.to_vec())),
                Payload::OnDisk { sha1, .. } => match self.blobs.get(sha1) {
                    Some(b) => out.push((k.clone(), b)),
                    None => crate::log_warn!("blob for {} is missing; format skipped", k.label()),
                },
            }
        }
        out
    }

    fn set_clipboard(&self, formats: &[(FormatKey, Payload)]) -> R {
        let bytes = self.hydrate(formats);
        if bytes.is_empty() {
            return Err("the item's data is no longer available".into());
        }
        let prepared = clipboard::prepare(&bytes).ok_or("could not allocate clipboard memory")?;
        clipboard::write_prepared(self.hwnd, prepared, 24, 8).map(|_| ()).map_err(|e| format!("could not set the clipboard ({e})"))
    }

    fn clipboard_paste(&self, formats: Vec<(FormatKey, Payload)>, target: HWND, restore: bool) -> R {
        crate::log_dbg!(
            "paste: target={:?} ({}) foreground={:?} restore={restore}",
            target.0,
            clipboard::process_name_of_window(target),
            foreground().0
        );
        check_target(target)?;
        let backup = if restore { clipboard::backup(self.hwnd) } else { None };
        let r = (|| {
            self.set_clipboard(&formats)?;
            self.focus(target)?;
            sleep_pump(AFTER_FOCUS_MS);
            release_modifiers();
            sleep_pump(AFTER_MODS_MS);
            send_ctrl_v()
        })();
        if restore {
            sleep_pump(SETTLE_AFTER_PASTE_MS);
            self.restore_backup(backup);
        }
        r
    }

    fn sequence(&self, steps: Vec<Vec<(FormatKey, Payload)>>, target: HWND) -> R {
        check_target(target)?;
        let backup = clipboard::backup(self.hwnd);
        let r = (|| {
            self.focus(target)?;
            sleep_pump(AFTER_FOCUS_MS);
            release_modifiers();
            sleep_pump(AFTER_MODS_MS);
            let n = steps.len();
            for (i, step) in steps.iter().enumerate() {
                if foreground() != target {
                    return Err("the target window lost focus; paste stopped".to_string());
                }
                self.set_clipboard(step)?;
                send_ctrl_v()?;
                sleep_pump(SETTLE_AFTER_PASTE_MS / 2);
                let text_only = step.iter().all(|(k, _)| k.is_std(CF_UNICODETEXT) || k.is_std(CF_TEXT));
                if i + 1 < n && !text_only {
                    let crlf = vec![(FormatKey::Standard(CF_UNICODETEXT), Payload::inline(crate::preview::unicode_bytes("\r\n")))];
                    self.set_clipboard(&crlf)?;
                    send_ctrl_v()?;
                    sleep_pump(SETTLE_AFTER_PASTE_MS / 2);
                }
            }
            Ok(())
        })();
        sleep_pump(SETTLE_AFTER_PASTE_MS);
        self.restore_backup(backup);
        r
    }

    fn excel(&self, texts: Vec<String>, target: HWND) -> R {
        check_target(target)?;
        let backup = clipboard::backup(self.hwnd);
        // Wait for Z / Ctrl / Shift to be physically released, else Excel gets Ctrl+Z (undo).
        for vk in [0x5Au16, VK_CONTROL.0, VK_SHIFT.0] {
            wait_key_up(vk, 1200);
        }
        let r = (|| {
            self.focus(target)?;
            sleep_pump(AFTER_FOCUS_MS);
            release_modifiers();
            for t in &texts {
                if foreground() != target {
                    return Err("focus left the target window; fill stopped".to_string());
                }
                let f = vec![(FormatKey::Standard(CF_UNICODETEXT), Payload::inline(crate::preview::unicode_bytes(t)))];
                self.set_clipboard(&f)?;
                sleep_pump(XL_AFTER_SET);
                tap(VK_F2)?;
                sleep_pump(XL_AFTER_F2);
                send_ctrl_v()?;
                sleep_pump(XL_AFTER_PASTE);
                tap(VK_RETURN)?;
                sleep_pump(XL_AFTER_ENTER);
            }
            Ok(())
        })();
        sleep_pump(SETTLE_AFTER_PASTE_MS);
        self.restore_backup(backup);
        r
    }

    fn synthetic_copy(&self) -> R {
        release_modifiers();
        sleep_pump(AFTER_MODS_MS);
        let ctrl_combo = |vk: VIRTUAL_KEY| {
            let seq = [vk_key(VK_LCONTROL, false), vk_key(vk, false), vk_key(vk, true), vk_key(VK_LCONTROL, true)];
            if send(&seq) == seq.len() {
                Ok(())
            } else {
                Err("SendInput was blocked".to_string())
            }
        };
        let changed = |before: u32| {
            for _ in 0..40 {
                if clipboard::sequence() != before {
                    return true;
                }
                sleep_pump(10);
            }
            false
        };
        let before = clipboard::sequence();
        ctrl_combo(VK_C)?;
        if changed(before) {
            return Ok(());
        }
        ctrl_combo(VK_A)?;
        sleep_pump(60);
        ctrl_combo(VK_C)?;
        if changed(before) {
            Ok(())
        } else {
            Err("the focused window did not provide any text".into())
        }
    }

    fn restore_backup(&self, backup: Option<Backup>) {
        match backup {
            Some(b) => {
                if !clipboard::restore(self.hwnd, &b) {
                    post_ui(UiMsg::Notice("Your previous clipboard could not be restored.".into()));
                }
            }
            None => crate::log_warn!("no clipboard backup was available to restore"),
        }
    }

    /// Verified focus restore (lesson 18.9): returns Err instead of pasting into the wrong window.
    fn focus(&self, target: HWND) -> R {
        // SAFETY: window and thread queries; attach is always detached.
        unsafe {
            if !IsWindow(Some(target)).as_bool() {
                return Err("the target window no longer exists".into());
            }
            if foreground() == target {
                return Ok(());
            }
            if IsIconic(target).as_bool() {
                let _ = ShowWindow(target, SW_RESTORE);
            }
            let me = GetCurrentThreadId();
            let t_tid = GetWindowThreadProcessId(target, None);
            let f_tid = GetWindowThreadProcessId(foreground(), None);
            let mut attached = Vec::new();
            for tid in [f_tid, t_tid] {
                if tid != 0 && tid != me && !attached.contains(&tid) && AttachThreadInput(me, tid, true).as_bool() {
                    attached.push(tid);
                }
            }
            let _ = SetForegroundWindow(target);
            for tid in attached {
                let _ = AttachThreadInput(me, tid, false);
            }
        }
        for _ in 0..30 {
            if foreground() == target {
                return Ok(());
            }
            sleep_pump(10);
        }
        Err("could not return focus to the target window".into())
    }
}

fn foreground() -> HWND {
    // SAFETY: no preconditions.
    unsafe { GetForegroundWindow() }
}

// ---- UIPI (spec 20.3) ----

fn process_elevated(h: windows::Win32::Foundation::HANDLE) -> Option<bool> {
    use windows::Win32::Security::*;
    // SAFETY: token handle closed on all paths.
    unsafe {
        let mut tok = windows::Win32::Foundation::HANDLE::default();
        OpenProcessToken(h, TOKEN_QUERY, &mut tok).ok()?;
        let mut e = TOKEN_ELEVATION::default();
        let mut ret = 0u32;
        let ok = GetTokenInformation(tok, TokenElevation, Some(&mut e as *mut _ as *mut _), std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut ret).is_ok();
        let _ = CloseHandle(tok);
        ok.then_some(e.TokenIsElevated != 0)
    }
}

/// True when synthetic input into `target` would be dropped because it runs at a higher
/// integrity level than clip4.
pub fn target_blocked(target: HWND) -> bool {
    // SAFETY: handle closed below.
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(target, Some(&mut pid));
        if pid == 0 || pid == GetCurrentProcessId() {
            return false;
        }
        if process_elevated(GetCurrentProcess()).unwrap_or(false) {
            return false;
        }
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => {
                // Access denied while opening an elevated process's token also means "higher".
                let r = process_elevated(h).unwrap_or(true);
                let _ = CloseHandle(h);
                r
            }
            Err(_) => true,
        }
    }
}

fn check_target(target: HWND) -> R {
    if target_blocked(target) {
        return Err("the target window runs as administrator; Windows blocks pasting into it from a normal-privilege clip4".into());
    }
    Ok(())
}

// ---- input synthesis ----

fn kbd(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn vk_key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    // SAFETY: plain scan-code lookup.
    let sc = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
    kbd(vk, sc, if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) })
}

fn send(inputs: &[INPUT]) -> usize {
    // SAFETY: slice of fully initialised INPUTs.
    unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) as usize }
}

fn tap(vk: VIRTUAL_KEY) -> R {
    let seq = [vk_key(vk, false), vk_key(vk, true)];
    if send(&seq) != seq.len() {
        return Err("SendInput was blocked".into());
    }
    Ok(())
}

/// Ctrl+V as ONE SendInput batch; the return count is checked.
fn send_ctrl_v() -> R {
    crate::log_dbg!("sending Ctrl+V; foreground is {}", clipboard::process_name_of_window(foreground()));
    let seq = [vk_key(VK_LCONTROL, false), vk_key(VK_V, false), vk_key(VK_V, true), vk_key(VK_LCONTROL, true)];
    let n = send(&seq);
    if n != seq.len() {
        return Err(format!("SendInput inserted {n} of 4 events (blocked by another application or UIPI)"));
    }
    Ok(())
}

/// Key-up for each specific L/R modifier the hardware still reports as held (plus the generic
/// Ctrl/Shift/Alt codes, which some injectors and remote sessions set instead of L/R).
fn release_modifiers() {
    let down = |vk: VIRTUAL_KEY| {
        // SAFETY: plain query.
        let st = unsafe { GetAsyncKeyState(vk.0 as i32) };
        st as u16 & 0x8000 != 0
    };
    let mut ups = Vec::new();
    for vk in [VK_LCONTROL, VK_RCONTROL, VK_LSHIFT, VK_RSHIFT, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN] {
        if down(vk) {
            ups.push(vk_key(vk, true));
        }
    }
    if ups.is_empty() {
        for vk in [VK_CONTROL, VK_SHIFT, VK_MENU] {
            if down(vk) {
                ups.push(vk_key(vk, true));
            }
        }
    }
    if !ups.is_empty() {
        send(&ups);
    }
}

fn wait_key_up(vk: u16, max_ms: u64) {
    let end = Instant::now() + Duration::from_millis(max_ms);
    // SAFETY: plain query.
    while unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0 && Instant::now() < end {
        sleep_pump(10);
    }
}

/// `KEYEVENTF_UNICODE` typing (spec 12.4).
fn type_text(text: &str) -> R {
    let mut events: Vec<INPUT> = Vec::new();
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                events.push(vk_key(VK_RETURN, false));
                events.push(vk_key(VK_RETURN, true));
            }
            '\n' => {
                events.push(vk_key(VK_RETURN, false));
                events.push(vk_key(VK_RETURN, true));
            }
            '\t' => {
                events.push(vk_key(VK_TAB, false));
                events.push(vk_key(VK_TAB, true));
            }
            c => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    events.push(kbd(VIRTUAL_KEY(0), *u, KEYEVENTF_UNICODE));
                    events.push(kbd(VIRTUAL_KEY(0), *u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                }
            }
        }
    }
    for batch in events.chunks(24) {
        let mut rest = batch;
        let mut tries = 0;
        while !rest.is_empty() {
            let n = send(rest);
            if n == rest.len() {
                break;
            }
            tries += 1;
            if tries > 8 {
                return Err("typing was interrupted (input blocked)".into());
            }
            rest = &rest[n..];
            sleep_pump(15);
        }
    }
    Ok(())
}
