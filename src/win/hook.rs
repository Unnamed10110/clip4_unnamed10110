//! Low-level keyboard hook on its own thread (spec 5.2, 11.1, 11.2).
//!
//! The callback does one thing: classify a key and `PostMessageW` to the UI thread. It
//! never blocks, never allocates, never touches application state beyond a `try_read`
//! of the binding table.

use super::msg::{WM_HOOK_HOTKEY, WM_HOOK_REARM};
use super::settings::{Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};
use super::util::{guarded, SendHwnd};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::RwLock;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, Debug)]
pub struct Binding {
    pub mods: u32,
    pub vk: u32,
    /// Sent back as WPARAM.
    pub id: usize,
}

static BINDINGS: RwLock<Vec<Binding>> = RwLock::new(Vec::new());
/// Bitset of virtual-key codes any binding uses: uninteresting keys cost one test.
static VK_MASK: [AtomicU64; 4] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static TARGET: AtomicIsize = AtomicIsize::new(0);
static THREAD_ID: AtomicU32 = AtomicU32::new(0);
static HELD_VK: AtomicU32 = AtomicU32::new(0);
static STOP: AtomicBool = AtomicBool::new(false);

const VK_DECIMAL_U: u32 = 0x6E;
const VK_DELETE_U: u32 = 0x2E;

fn mask_set(vk: u32) {
    VK_MASK[(vk as usize >> 6) & 3].fetch_or(1u64 << (vk & 63), Ordering::Relaxed);
}

fn mask_has(vk: u32) -> bool {
    VK_MASK[(vk as usize >> 6) & 3].load(Ordering::Relaxed) & (1u64 << (vk & 63)) != 0
}

/// Replaces the table of hotkeys the hook watches.
pub fn set_bindings(list: &[(usize, Hotkey)]) {
    let v: Vec<Binding> = list.iter().filter(|(_, h)| h.is_bound()).map(|(id, h)| Binding { mods: h.mods, vk: h.vk, id: *id }).collect();
    for m in &VK_MASK {
        m.store(0, Ordering::Relaxed);
    }
    for b in &v {
        mask_set(b.vk);
        if b.vk == VK_DECIMAL_U {
            mask_set(VK_DELETE_U); // NumPad-dot arrives as Delete when NumLock is off
        }
    }
    if let Ok(mut g) = BINDINGS.write() {
        *g = v;
    }
}

fn current_mods() -> u32 {
    let down = |vk: VIRTUAL_KEY| (unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000) != 0;
    let mut m = 0;
    if down(VK_CONTROL) {
        m |= MOD_CONTROL;
    }
    if down(VK_SHIFT) {
        m |= MOD_SHIFT;
    }
    if down(VK_MENU) {
        m |= MOD_ALT;
    }
    if down(VK_LWIN) || down(VK_RWIN) {
        m |= MOD_WIN;
    }
    m
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        let _ = std::panic::catch_unwind(|| {
            // SAFETY: for HC_ACTION lparam points to a KBDLLHOOKSTRUCT valid for this call.
            let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let msg = wparam.0 as u32;
            let injected = kb.flags.0 & (LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0) != 0;
            if msg == WM_KEYUP || msg == WM_SYSKEYUP {
                if HELD_VK.load(Ordering::Relaxed) == kb.vkCode {
                    HELD_VK.store(0, Ordering::Relaxed);
                }
                return;
            }
            if injected || !(msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN) {
                return;
            }
            // Prefilter on the virtual-key code before anything else.
            if !mask_has(kb.vkCode) {
                return;
            }
            let mut vk = kb.vkCode;
            if vk == VK_DELETE_U && kb.scanCode == 0x53 && kb.flags.0 & LLKHF_EXTENDED.0 == 0 {
                vk = VK_DECIMAL_U;
            }
            if HELD_VK.swap(kb.vkCode, Ordering::Relaxed) == kb.vkCode {
                return; // auto-repeat (MOD_NOREPEAT semantics)
            }
            let mods = current_mods();
            if let Ok(g) = BINDINGS.try_read() {
                if let Some(b) = g.iter().find(|b| b.vk == vk && b.mods == mods) {
                    let t = TARGET.load(Ordering::Relaxed);
                    if t != 0 {
                        // SAFETY: PostMessageW is safe to call from a hook.
                        let _ = unsafe { PostMessageW(Some(SendHwnd(t).get()), WM_HOOK_HOTKEY, WPARAM(b.id), LPARAM(0)) };
                    }
                }
            }
        });
    }
    // SAFETY: always pass the event on; the hook never swallows keys.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn install() -> Option<HHOOK> {
    // SAFETY: standard WH_KEYBOARD_LL install on this thread.
    unsafe {
        let hmod = GetModuleHandleW(None).ok()?;
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(HINSTANCE(hmod.0)), 0).ok()
    }
}

fn thread_main() {
    // SAFETY: force creation of this thread's message queue before publishing the id.
    unsafe {
        let mut m = MSG::default();
        let _ = PeekMessageW(&mut m, None, WM_USER, WM_USER, PM_NOREMOVE);
        THREAD_ID.store(GetCurrentThreadId(), Ordering::SeqCst);
    }
    let mut hook = install();
    if hook.is_none() {
        crate::log_err!("WH_KEYBOARD_LL install failed");
    }
    // SAFETY: thread timer (no window) as a 60 s backstop re-arm. With a NULL window the system
    // assigns the timer id, so the returned value (not a constant) identifies WM_TIMER below.
    let rearm_timer = unsafe { SetTimer(None, 0, 60_000, None) };
    let mut msg = MSG::default();
    // SAFETY: standard message loop.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        if STOP.load(Ordering::Relaxed) {
            break;
        }
        if msg.message == WM_HOOK_REARM || (msg.message == WM_TIMER && msg.wParam.0 == rearm_timer) {
            // Install the NEW hook first, then drop the old handle (lesson 18.11).
            let fresh = install();
            if let Some(old) = hook.take() {
                // SAFETY: handle came from SetWindowsHookExW.
                unsafe {
                    let _ = UnhookWindowsHookEx(old);
                }
            }
            crate::log_dbg!("keyboard hook re-armed (ok={})", fresh.is_some());
            hook = fresh;
        }
    }
    if let Some(h) = hook {
        // SAFETY: as above.
        unsafe {
            let _ = UnhookWindowsHookEx(h);
        }
    }
}

/// Starts the hook thread. A panic restarts the loop with a fresh hook.
pub fn start(ui: SendHwnd) {
    TARGET.store(ui.0, Ordering::Relaxed);
    STOP.store(false, Ordering::Relaxed);
    let spawned = std::thread::Builder::new().name("hook".into()).spawn(|| loop {
        let clean = guarded("hook", thread_main);
        if clean || STOP.load(Ordering::Relaxed) {
            break;
        }
        crate::log_err!("hook thread panicked; re-installing");
        std::thread::sleep(std::time::Duration::from_millis(500));
    });
    if spawned.is_err() {
        crate::log_err!("could not spawn hook thread");
    }
}

/// Ask the hook thread to re-install now (after resume from sleep, session switch, ...).
pub fn rearm() {
    let tid = THREAD_ID.load(Ordering::SeqCst);
    if tid != 0 {
        // SAFETY: posting to a thread id we created.
        unsafe {
            let _ = PostThreadMessageW(tid, WM_HOOK_REARM, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn stop() {
    STOP.store(true, Ordering::Relaxed);
    let tid = THREAD_ID.load(Ordering::SeqCst);
    if tid != 0 {
        // SAFETY: as above.
        unsafe {
            let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}
