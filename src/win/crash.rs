//! Panic hook, unhandled-exception filter, minidumps and the single restart (spec 19.1).

use super::util::{local_dir, now_unix_ms, pcw, wide};
use std::panic;
use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_NONE};
use windows::Win32::System::Diagnostics::Debug::*;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId};

pub fn install() {
    panic::set_hook(Box::new(|info| {
        let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic>".into());
        let bt = std::backtrace::Backtrace::force_capture();
        crate::log_err!("PANIC at {loc}: {msg}\n{bt}");
    }));
    // SAFETY: installs a process-wide filter.
    unsafe {
        SetUnhandledExceptionFilter(Some(filter));
    }
}

unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
    let dir = local_dir().join("crash");
    let _ = std::fs::create_dir_all(&dir);
    let stamp = now_unix_ms();
    let path = dir.join(format!("clip4-{stamp}.dmp"));
    let w = wide(&path.to_string_lossy());
    if let Ok(h) = CreateFileW(pcw(&w), GENERIC_WRITE.0, FILE_SHARE_NONE, None, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, None) {
        let ex = MINIDUMP_EXCEPTION_INFORMATION { ThreadId: GetCurrentThreadId(), ExceptionPointers: info as *mut _, ClientPointers: false.into() };
        let _ = MiniDumpWriteDump(GetCurrentProcess(), GetCurrentProcessId(), h, MiniDumpNormal, Some(&ex), None, None);
        let _ = CloseHandle(h);
    }
    crate::log_err!("UNHANDLED EXCEPTION; minidump written to {}", path.display());
    // Restart once, never in a loop: skip when the previous crash was less than 60 s ago.
    let marker = dir.join("last-crash");
    let recent = std::fs::read_to_string(&marker).ok().and_then(|s| s.trim().parse::<i64>().ok()).is_some_and(|t| stamp - t < 60_000);
    let _ = std::fs::write(&marker, stamp.to_string());
    if !recent {
        spawn_successor();
    }
    1 // EXCEPTION_EXECUTE_HANDLER
}

/// Starts a new instance that waits for this process to exit before taking the single-instance mutex.
pub fn spawn_successor() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).arg(format!("--after={}", std::process::id())).spawn();
    }
}
