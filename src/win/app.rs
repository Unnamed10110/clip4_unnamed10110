//! Application root: lives on the UI thread. Owns the history store, settings, the
//! workers and the overlay, and dispatches every message to the hidden main window.
//!
//! Borrow discipline: `RefCell`s are borrowed briefly and NEVER across a Win32 call that can
//! pump messages (SetWindowText, MoveWindow, MessageBox, dialogs, ...).

use super::blob::BlobStore;
use super::clipboard;
use super::msg::*;
use super::overlay::{Overlay, Scope};
use super::paste::Paster;
use super::settings::{self, Action, Settings, ACTIONS};
use super::util::*;
use super::worker::{self, IoTask, Workers};
use super::{crash, hook, sound, tray};
use crate::model::*;
use crate::snippets_fmt::Snippet;
use crate::store::{AddOutcome, Store};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::System::DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener};

pub const MAIN_CLASS: PCWSTR = w!("clip4_main");

const TIMER_SAVE: usize = 1;
const TIMER_CAPTURE_RETRY: usize = 2;
const TIMER_LISTENER_RETRY: usize = 3;
const TIMER_HOTKEY_HEALTH: usize = 4;

const IDM_SHOW: u32 = 100;
const IDM_COPY_FOCUSED: u32 = 101;
const IDM_MANAGE_SNIPPETS: u32 = 102;
const IDM_STARTUP: u32 = 103;
const IDM_EXPAND: u32 = 104;
const IDM_SETTINGS: u32 = 105;
const IDM_RESTART: u32 = 106;
const IDM_EXIT: u32 = 107;
const IDM_SNIPPET_BASE: u32 = 1000;

#[derive(Default)]
pub struct CapState {
    pub last_consumed: u32,
    retry_seq: u32,
    attempts: u32,
    pending: Option<u32>,
}

pub struct App {
    pub hwnd: Cell<HWND>,
    pub hinst: HINSTANCE,
    pub settings: RefCell<Settings>,
    pub store: RefCell<Store>,
    pub snippets: RefCell<Vec<Snippet>>,
    pub blobs: BlobStore,
    pub workers: Workers,
    pub paster: Paster,
    pub overlay: Overlay,
    pub cap: RefCell<CapState>,
    /// Plain text of the last paste + when; ignore an identical capture within 3 s (spec 6.5).
    pub echo: RefCell<Option<(String, Instant)>>,
    pub hk_failed: RefCell<Vec<Action>>,
    hk_last: RefCell<[Option<Instant>; 5]>,
    loaded: Cell<bool>,
    pending_adds: RefCell<Vec<Item>>,
    taskbar_created: Cell<u32>,
    exiting: Cell<bool>,
    listener_ok: Cell<bool>,
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

pub fn app() -> Option<Rc<App>> {
    APP.with(|a| a.try_borrow().ok().and_then(|g| g.clone()))
}

static MUTEX: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

fn release_mutex() {
    let h = MUTEX.swap(0, std::sync::atomic::Ordering::SeqCst);
    if h != 0 {
        // SAFETY: handle created by CreateMutexW in `run`.
        unsafe {
            let _ = CloseHandle(HANDLE(h as *mut _));
        }
    }
}

/// Process entry. Returns the exit code.
pub fn run() -> i32 {
    let t_start = Instant::now();
    let settings = Settings::load_with_import();
    super::log::init(settings.debug_log);
    crash::install();
    crate::log_info!("clip4 {} starting", env!("CARGO_PKG_VERSION"));

    // A successor started by a crash/restart waits for its predecessor to exit.
    if let Some(pid) = std::env::args().find_map(|a| a.strip_prefix("--after=").and_then(|v| v.parse::<u32>().ok())) {
        // SAFETY: waits at most 8 s for the old process.
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                WaitForSingleObject(h, 8000);
                let _ = CloseHandle(h);
            }
        }
    }

    // Single instance (spec 17).
    // SAFETY: standard named-mutex pattern.
    unsafe {
        let mname = wide(&format!("Local\\clip4-single-instance{}", profile().map(|p| format!("-{p}")).unwrap_or_default()));
        let m = CreateMutexW(None, true, pcw(&mname));
        let already = windows::Win32::Foundation::GetLastError() == ERROR_ALREADY_EXISTS;
        match m {
            Ok(h) if !already => MUTEX.store(h.0 as isize, std::sync::atomic::Ordering::SeqCst),
            _ => {
                if let Ok(h) = FindWindowW(MAIN_CLASS, PCWSTR::null()) {
                    let _ = PostMessageW(Some(h), WM_SHOW_OVERLAY, WPARAM(0), LPARAM(0));
                }
                crate::log_info!("another instance is running; asked it to show the overlay");
                return 0;
            }
        }
    }

    // SAFETY: module handle of this exe.
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }.map(Into::into).unwrap_or_default();
    let blobs = BlobStore::new(data_dir().join("blobs"));

    // Windows are created after the App exists so early messages find it.
    let app = match App::new(settings, hinst, blobs) {
        Some(a) => Rc::new(a),
        None => {
            crate::log_err!("could not start workers");
            release_mutex();
            return 1;
        }
    };
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    if !app.create_main_window() {
        release_mutex();
        return 1;
    }
    app.startup();
    crate::log_info!("startup complete in {} ms", t_start.elapsed().as_millis());

    let mut msg = MSG::default();
    // SAFETY: standard blocking message loop (the UI thread never polls or sleeps).
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if app.overlay.pre_translate(&msg) {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    app.shutdown();
    APP.with(|a| *a.borrow_mut() = None);
    release_mutex();
    crate::log_info!("clip4 exiting");
    0
}

impl App {
    fn new(settings: Settings, hinst: HINSTANCE, blobs: BlobStore) -> Option<App> {
        let mut store = Store::new(settings.max_items);
        store.set_max_items(settings.max_items);
        let workers = Workers::start(blobs.clone());
        let paster = Paster::start(blobs.clone())?;
        Some(App {
            hwnd: Cell::new(HWND::default()),
            hinst,
            snippets: RefCell::new(super::snippets_store::load()),
            settings: RefCell::new(settings),
            store: RefCell::new(store),
            blobs,
            workers,
            paster,
            overlay: Overlay::new(),
            cap: RefCell::new(CapState::default()),
            echo: RefCell::new(None),
            hk_failed: RefCell::new(Vec::new()),
            hk_last: RefCell::new([None; 5]),
            loaded: Cell::new(false),
            pending_adds: RefCell::new(Vec::new()),
            taskbar_created: Cell::new(0),
            exiting: Cell::new(false),
            listener_ok: Cell::new(false),
        })
    }

    fn create_main_window(&self) -> bool {
        // SAFETY: class + window creation on the UI thread.
        unsafe {
            let wc = WNDCLASSW { lpfnWndProc: Some(main_proc), hInstance: self.hinst, lpszClassName: MAIN_CLASS, ..Default::default() };
            RegisterClassW(&wc);
            match CreateWindowExW(WINDOW_EX_STYLE(0), MAIN_CLASS, w!("clip4"), WS_POPUP, 0, 0, 0, 0, None, None, Some(self.hinst), None) {
                Ok(h) => {
                    self.hwnd.set(h);
                    self.taskbar_created.set(RegisterWindowMessageW(w!("TaskbarCreated")));
                    true
                }
                Err(e) => {
                    crate::log_err!("main window creation failed: {e}");
                    false
                }
            }
        }
    }

    fn startup(&self) {
        let hwnd = self.hwnd.get();
        let ui = SendHwnd::new(hwnd);
        set_ui_hwnd(hwnd);
        worker::set_capture_owner(ui);

        tray::add(hwnd, "clip4");
        self.add_listener();
        self.register_hotkeys();
        hook::start(ui);
        self.sync_hook_bindings();
        self.overlay.create(self);
        self.workers.io(IoTask::Load);
        // 1 Hz housekeeping is NOT needed while idle; this slow tick only re-checks hotkey health.
        // SAFETY: timer on our window.
        unsafe {
            SetTimer(Some(hwnd), TIMER_HOTKEY_HEALTH, 10 * 60 * 1000, None);
        }
        // First run: offer the one-time clip2 import (spec 8.6). Asked once, whatever the answer.
        let asked = super::reg::Key::open(&settings::key_path(), false).and_then(|k| k.dword("Clip2ImportAsked")).is_some();
        if profile().is_none() && !asked && !data_dir().join("history.dat").exists() && worker::clip2_history_exists() {
            if let Some(k) = super::reg::Key::create(&settings::key_path()) {
                k.set_dword("Clip2ImportAsked", 1);
            }
            let ans = message_box(hwnd, "clip2 history was found. Import it into clip4?", "clip4", MB_YESNO | MB_ICONQUESTION);
            if ans == IDYES {
                self.workers.io(IoTask::ImportClip2);
            }
        }
    }

    fn shutdown(&self) {
        let hwnd = self.hwnd.get();
        hook::stop();
        // SAFETY: undo registrations; window is destroyed with the process.
        unsafe {
            for a in ACTIONS {
                let _ = UnregisterHotKey(Some(hwnd), a as i32 + 1);
            }
            let _ = RemoveClipboardFormatListener(hwnd);
        }
        tray::remove(hwnd);
        self.flush_save_blocking();
    }

    // ---------------- listener / hotkeys ----------------

    fn add_listener(&self) {
        let hwnd = self.hwnd.get();
        // SAFETY: valid window handle.
        if unsafe { AddClipboardFormatListener(hwnd) }.is_ok() {
            self.listener_ok.set(true);
            return;
        }
        // Check the return value (lesson 18.13): retry once after 150 ms, then warn.
        // SAFETY: one-shot timer.
        unsafe {
            SetTimer(Some(hwnd), TIMER_LISTENER_RETRY, 150, None);
        }
    }

    pub fn register_hotkeys(&self) {
        let hwnd = self.hwnd.get();
        let mut failed = Vec::new();
        for a in ACTIONS {
            let id = a as i32 + 1;
            // SAFETY: (un)register on our window.
            unsafe {
                let _ = UnregisterHotKey(Some(hwnd), id);
            }
            let hk = self.settings.borrow().hotkey(a);
            if !hk.is_bound() {
                continue;
            }
            // SAFETY: as above.
            let r = unsafe { RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(hk.mods) | MOD_NOREPEAT, hk.vk) };
            if r.is_err() {
                crate::log_warn!("hotkey {} ({}) is already taken by another application", a.title(), hk.describe());
                failed.push(a);
            }
        }
        *self.hk_failed.borrow_mut() = failed;
        self.sync_hook_bindings();
    }

    pub fn sync_hook_bindings(&self) {
        let s = self.settings.borrow();
        hook::set_bindings(&[(Action::Overlay as usize, s.hotkey(Action::Overlay)), (Action::Snippets as usize, s.hotkey(Action::Snippets))]);
    }

    fn on_hotkey(&self, a: Action) {
        crate::log_dbg!("hotkey {:?}", a);
        // RegisterHotKey and the LL hook both report one press: de-duplicate within 250 ms.
        {
            let mut last = self.hk_last.borrow_mut();
            let slot = &mut last[a as usize];
            if slot.is_some_and(|t| t.elapsed() < Duration::from_millis(250)) {
                return;
            }
            *slot = Some(Instant::now());
        }
        match a {
            Action::Overlay => self.overlay.toggle(self, Scope::History),
            Action::Snippets => self.overlay.toggle(self, Scope::Snippets),
            Action::CopyFocused => self.copy_from_focused(),
            Action::KeystrokePaste => self.hotkey_keystroke_paste(),
            Action::ClipboardPaste => self.hotkey_swap_paste(),
        }
    }

    // ---------------- capture ----------------

    fn on_clipboard_update(&self) {
        let seq = clipboard::sequence();
        if clipboard::is_own(seq) {
            crate::log_dbg!("clipboard update {seq} is our own echo");
            return;
        }
        {
            let mut c = self.cap.borrow_mut();
            if c.last_consumed == seq {
                return;
            }
            if c.retry_seq != seq {
                c.retry_seq = seq;
                c.attempts = 0;
            }
        }
        self.workers.capture(seq);
    }

    fn on_capture_busy(&self, seq: u32) {
        let delay = {
            let mut c = self.cap.borrow_mut();
            if c.retry_seq != seq {
                c.retry_seq = seq;
                c.attempts = 0;
            }
            c.attempts += 1;
            if c.attempts > 10 {
                crate::log_warn!("capture of sequence {seq} abandoned after 10 attempts");
                return;
            }
            c.pending = Some(seq);
            [50u32, 100, 150, 200][(c.attempts as usize - 1).min(3)]
        };
        // SAFETY: one-shot timer (the UI thread never sleeps).
        unsafe {
            SetTimer(Some(self.hwnd.get()), TIMER_CAPTURE_RETRY, delay, None);
        }
    }

    fn on_captured(&self, seq: u32, item: Item, lock_ms: u32) {
        {
            let mut c = self.cap.borrow_mut();
            if c.last_consumed == seq {
                return;
            }
            c.last_consumed = seq;
        }
        crate::log_dbg!("captured seq {seq}: {} formats, lock {lock_ms} ms", item.formats.len());
        // Time-boxed echo suppression: identical text within 3 s of our own paste (Office writes back).
        {
            let mut e = self.echo.borrow_mut();
            if let Some((t, at)) = e.as_ref() {
                if at.elapsed() < Duration::from_secs(3) {
                    if item.text().map(|x| normalize(&x)).as_deref() == Some(t.as_str()) {
                        crate::log_dbg!("capture ignored as paste echo");
                        return;
                    }
                } else {
                    *e = None;
                }
            }
        }
        if !self.loaded.get() {
            self.pending_adds.borrow_mut().push(item);
            return;
        }
        self.add_item(item, true);
    }

    pub fn add_item(&self, item: Item, with_sound: bool) -> Option<u64> {
        let outcome = self.store.borrow_mut().add(item);
        let id = match outcome {
            AddOutcome::Added(id) | AddOutcome::MovedToTop(id) => Some(id),
            AddOutcome::Duplicate => None,
        };
        if id.is_some() {
            if with_sound && self.settings.borrow().sound {
                sound::click(); // fire-and-forget, after the capture is complete
            }
            self.schedule_save();
            self.overlay.refresh(self);
        }
        id
    }

    // ---------------- persistence ----------------

    pub fn schedule_save(&self) {
        // SAFETY: resetting the timer debounces to 1.5 s after the LAST change.
        unsafe {
            SetTimer(Some(self.hwnd.get()), TIMER_SAVE, 1500, None);
        }
    }

    fn save_now(&self) {
        let snap = self.store.borrow().snapshot();
        self.workers.io(IoTask::Save(snap));
    }

    fn flush_save_blocking(&self) {
        if !self.loaded.get() {
            return; // never overwrite history that has not been read yet
        }
        let snap = self.store.borrow().snapshot();
        worker::save_blocking(&self.blobs, &snap);
    }

    // ---------------- UI messages ----------------

    fn on_ui_msg(&self, m: UiMsg) {
        match m {
            UiMsg::Captured { seq, item, lock_ms } => self.on_captured(seq, item, lock_ms),
            UiMsg::CaptureBusy { seq } => self.on_capture_busy(seq),
            UiMsg::CaptureConsumed { seq } => {
                self.cap.borrow_mut().last_consumed = seq;
            }
            UiMsg::Loaded { items, note } => {
                let imported = note.as_deref().is_some_and(|n| n.starts_with("Imported"));
                if imported {
                    // Imported items go to the BACK of the existing history.
                    let mut cur = self.store.borrow().snapshot();
                    cur.extend(items);
                    self.store.borrow_mut().load(cur);
                    self.schedule_save();
                } else {
                    self.store.borrow_mut().load(items);
                    self.loaded.set(true);
                    let pend: Vec<Item> = std::mem::take(&mut *self.pending_adds.borrow_mut());
                    for it in pend {
                        self.add_item(it, false);
                    }
                }
                self.overlay.refresh(self);
                if let Some(n) = note {
                    self.notice("clip4", &n);
                }
            }
            UiMsg::Saved { ok } => {
                if !ok {
                    crate::log_err!("history save failed");
                }
            }
            UiMsg::Thumb { id, w, h, bgra } => self.overlay.on_thumb(self, id, w, h, &bgra),
            UiMsg::PasteResult { ok, note } => {
                if !ok {
                    self.notice("Paste failed", &note);
                }
            }
            UiMsg::Notice(t) => self.notice("clip4", &t),
            UiMsg::FocusedText { text, via_uia } => self.on_focused_text(text, via_uia),
            UiMsg::Hotkey(a) => self.on_hotkey(a),
        }
    }

    pub fn notice(&self, title: &str, text: &str) {
        tray::notice(self.hwnd.get(), title, text);
    }

    // ---------------- settings ----------------

    /// Applies new settings to the running app. Hotkeys/history size apply on Save only.
    pub fn apply_settings(&self, new: Settings, hotkeys_and_size: bool) {
        let old = self.settings.borrow().clone();
        {
            let mut s = self.settings.borrow_mut();
            s.theme_id = new.theme_id;
            s.font_face = new.font_face.clone();
            s.content_size = new.content_size;
            s.ui_size = new.ui_size;
            s.colors = new.colors;
            s.expand_selected = new.expand_selected;
            s.sound = new.sound;
            if hotkeys_and_size {
                s.hotkeys = new.hotkeys;
                s.max_items = new.max_items;
            }
        }
        let look_changed = {
            let s = self.settings.borrow();
            s.theme_id != old.theme_id || s.font_face != old.font_face || s.content_size != old.content_size || s.ui_size != old.ui_size || s.colors != old.colors
        };
        self.settings.borrow().save_look_only();
        if hotkeys_and_size {
            self.settings.borrow().save();
            self.store.borrow_mut().set_max_items(new.max_items);
            self.register_hotkeys();
            self.schedule_save();
        }
        super::log::set_debug(self.settings.borrow().debug_log);
        self.overlay.apply_look(self);
        if look_changed || old.expand_selected != new.expand_selected {
            self.overlay.refresh(self);
        }
    }

    // ---------------- tray ----------------

    fn on_tray(&self, lparam: LPARAM) {
        match (lparam.0 & 0xFFFF) as u32 {
            WM_LBUTTONUP => self.overlay.toggle(self, Scope::History),
            WM_RBUTTONUP | WM_CONTEXTMENU => self.tray_menu(),
            _ => {}
        }
    }

    fn tray_menu(&self) {
        let hwnd = self.hwnd.get();
        // SAFETY: popup menu built and destroyed here.
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let add = |m: HMENU, id: u32, text: &str, checked: bool| {
                let t = wide(text);
                let _ = AppendMenuW(m, MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED }, id as usize, pcw(&t));
            };
            add(menu, IDM_SHOW, "Show clipboard", false);
            add(menu, IDM_COPY_FOCUSED, "Copy from focused control", false);
            if let Ok(sub) = CreatePopupMenu() {
                for (i, s) in self.snippets.borrow().iter().enumerate().take(200) {
                    add(sub, IDM_SNIPPET_BASE + i as u32, &s.name.replace('&', "&&"), false);
                }
                let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
                add(sub, IDM_MANAGE_SNIPPETS, "Manage…", false);
                let t = wide("Snippets");
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, pcw(&t));
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            add(menu, IDM_STARTUP, "Start with Windows", settings::startup_enabled());
            add(menu, IDM_EXPAND, "Expand selected item", self.settings.borrow().expand_selected);
            add(menu, IDM_SETTINGS, "Settings", false);
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            add(menu, IDM_RESTART, "Restart", false);
            add(menu, IDM_EXIT, "Exit", false);

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, hwnd, None);
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            self.on_menu_command(cmd.0 as u32);
        }
    }

    fn on_menu_command(&self, id: u32) {
        match id {
            IDM_SHOW => self.overlay.toggle(self, Scope::History),
            IDM_COPY_FOCUSED => self.copy_from_focused(),
            IDM_MANAGE_SNIPPETS => super::dialogs::manage_snippets(self),
            IDM_STARTUP => settings::set_startup(!settings::startup_enabled()),
            IDM_EXPAND => {
                let mut s = self.settings.borrow().clone();
                s.expand_selected = !s.expand_selected;
                self.apply_settings(s, false);
                self.settings.borrow().save();
            }
            IDM_SETTINGS => super::dialogs::settings_dialog(self),
            IDM_RESTART => self.restart(),
            IDM_EXIT => self.request_exit(),
            n if n >= IDM_SNIPPET_BASE => self.paste_snippet_by_index((n - IDM_SNIPPET_BASE) as usize, None),
            _ => {}
        }
    }

    pub fn request_exit(&self) {
        self.exiting.set(true);
        // SAFETY: ends the message loop; `shutdown` flushes history.
        unsafe { PostQuitMessage(0) };
    }

    fn restart(&self) {
        self.flush_save_blocking();
        self.loaded.set(false); // shutdown must not write again
        release_mutex();
        crash::spawn_successor();
        self.exiting.set(true);
        // SAFETY: ends the message loop.
        unsafe { PostQuitMessage(0) };
    }

    // ---------------- timers & system events ----------------

    fn on_timer(&self, id: usize) {
        let hwnd = self.hwnd.get();
        // SAFETY: all of these are one-shot except the slow health tick.
        unsafe {
            match id {
                TIMER_SAVE => {
                    let _ = KillTimer(Some(hwnd), TIMER_SAVE);
                    if self.loaded.get() {
                        self.save_now();
                    }
                }
                TIMER_CAPTURE_RETRY => {
                    let _ = KillTimer(Some(hwnd), TIMER_CAPTURE_RETRY);
                    let seq = self.cap.borrow_mut().pending.take();
                    if let Some(s) = seq {
                        if self.cap.borrow().last_consumed != s {
                            self.workers.capture(s);
                        }
                    }
                }
                TIMER_LISTENER_RETRY => {
                    let _ = KillTimer(Some(hwnd), TIMER_LISTENER_RETRY);
                    if AddClipboardFormatListener(hwnd).is_ok() {
                        self.listener_ok.set(true);
                    } else {
                        crate::log_err!("AddClipboardFormatListener failed twice; history will not record");
                        self.notice("clip4", "Clipboard monitoring could not be started, so copies will not be recorded. Try restarting clip4.");
                    }
                }
                TIMER_HOTKEY_HEALTH => {
                    // Defence in depth: re-arm the hook and re-claim hotkeys that earlier failed.
                    hook::rearm();
                    if !self.hk_failed.borrow().is_empty() {
                        self.register_hotkeys();
                    }
                }
                _ => {}
            }
        }
    }

    fn on_power(&self, wparam: WPARAM) {
        // PBT_APMRESUMEAUTOMATIC / PBT_APMRESUMESUSPEND: listener and hook must survive sleep.
        if wparam.0 == 0x12 || wparam.0 == 0x7 {
            crate::log_info!("resume from sleep: re-arming listener, hook and hotkeys");
            let hwnd = self.hwnd.get();
            // SAFETY: re-register the listener (idempotent: remove first).
            unsafe {
                let _ = RemoveClipboardFormatListener(hwnd);
            }
            self.add_listener();
            hook::rearm();
            self.register_hotkeys();
        }
    }
}

pub fn normalize(s: &str) -> String {
    s.trim().replace("\r\n", "\n").replace('\r', "\n")
}

pub fn message_box(owner: HWND, text: &str, title: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let (t, c) = (wide(text), wide(title));
    // SAFETY: modal box on the UI thread; callers hold no RefCell borrows.
    unsafe { MessageBoxW(Some(owner), pcw(&t), pcw(&c), style) }
}

unsafe extern "system" fn main_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    wndproc_guard(h, m, w, l, || main_proc_inner(h, m, w, l))
}

fn main_proc_inner(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: called from the window procedure with its own arguments.
    unsafe { main_proc_body(h, m, w, l) }
}

unsafe fn main_proc_body(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(h, m, w, l) };
    match m {
        WM_CLIPBOARDUPDATE => {
            app.on_clipboard_update();
            LRESULT(0)
        }
        WM_HOTKEY => {
            if let Some(a) = Action::from_index(w.0.wrapping_sub(1)) {
                app.on_hotkey(a);
            }
            LRESULT(0)
        }
        WM_HOOK_HOTKEY => {
            if let Some(a) = Action::from_index(w.0) {
                app.on_hotkey(a);
            }
            LRESULT(0)
        }
        WM_UI => {
            if let Some(b) = take_box::<UiMsg>(l) {
                app.on_ui_msg(*b);
            }
            LRESULT(0)
        }
        WM_TRAY => {
            app.on_tray(l);
            LRESULT(0)
        }
        WM_SHOW_OVERLAY => {
            app.overlay.show(&app, Scope::History);
            LRESULT(0)
        }
        WM_TIMER => {
            app.on_timer(w.0);
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            app.on_power(w);
            LRESULT(1)
        }
        WM_DISPLAYCHANGE => {
            app.overlay.on_display_change(&app);
            LRESULT(0)
        }
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if w.0 != 0 {
                app.flush_save_blocking();
            }
            LRESULT(0)
        }
        // A polite shutdown request (taskkill without /f, installers): exit cleanly and flush history.
        WM_CLOSE => {
            app.request_exit();
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ if m == app.taskbar_created.get() && m != 0 => {
            tray::add(h, "clip4");
            LRESULT(0)
        }
        _ => DefWindowProcW(h, m, w, l),
    }
}

/// Unused when only the app module is compiled in tests.
#[allow(dead_code)]
fn _keep(_: VIRTUAL_KEY, _: &NOTIFYICONDATAW) {}
