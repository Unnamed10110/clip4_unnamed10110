//! Rotating diagnostics log (spec 19.3). 1 MB x 3 files. NEVER log clipboard content.
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use windows::Win32::System::SystemInformation::GetLocalTime;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

const MAX_BYTES: u64 = 1024 * 1024;
const KEEP: usize = 3;

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

struct Sink {
    path: PathBuf,
    file: File,
    written: u64,
}

fn rotated(path: &std::path::Path, n: usize) -> PathBuf {
    if n == 0 {
        path.to_path_buf()
    } else {
        path.with_extension(format!("log.{n}"))
    }
}

impl Sink {
    fn open(path: PathBuf) -> Option<Sink> {
        let _ = fs::create_dir_all(path.parent()?);
        let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Some(Sink { path, file, written })
    }

    fn rotate(&mut self) {
        for n in (0..KEEP - 1).rev() {
            let _ = fs::rename(rotated(&self.path, n), rotated(&self.path, n + 1));
        }
        if let Ok(f) = OpenOptions::new().create(true).write(true).truncate(true).open(&self.path) {
            self.file = f;
            self.written = 0;
        }
    }
}

pub fn init(debug: bool) {
    set_debug(debug);
    if let Ok(mut g) = SINK.lock() {
        *g = Sink::open(super::util::local_dir().join("clip4.log"));
    }
}

pub fn set_debug(on: bool) {
    LEVEL.store(if on { Level::Debug as u8 } else { Level::Info as u8 }, Ordering::Relaxed);
}

pub fn enabled(l: Level) -> bool {
    (l as u8) <= LEVEL.load(Ordering::Relaxed)
}

pub fn write(l: Level, msg: std::fmt::Arguments) {
    if !enabled(l) {
        return;
    }
    let tag = match l {
        Level::Error => "ERROR",
        Level::Warn => "WARN ",
        Level::Info => "INFO ",
        Level::Debug => "DEBUG",
    };
    // SAFETY: GetLocalTime has no preconditions.
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} [{}] [{:?}] {}\n",
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond,
        t.wMilliseconds,
        tag,
        std::thread::current().id(),
        msg
    );
    // try_lock: logging must never block a latency-sensitive thread behind a slow disk.
    if let Ok(mut g) = SINK.try_lock() {
        if let Some(s) = g.as_mut() {
            if s.written + line.len() as u64 > MAX_BYTES {
                s.rotate();
            }
            if s.file.write_all(line.as_bytes()).is_ok() {
                s.written += line.len() as u64;
            }
        }
    }
}

#[macro_export]
macro_rules! log_err { ($($a:tt)*) => { $crate::win::log::write($crate::win::log::Level::Error, format_args!($($a)*)) } }
#[macro_export]
macro_rules! log_warn { ($($a:tt)*) => { $crate::win::log::write($crate::win::log::Level::Warn, format_args!($($a)*)) } }
#[macro_export]
macro_rules! log_info { ($($a:tt)*) => { $crate::win::log::write($crate::win::log::Level::Info, format_args!($($a)*)) } }
#[macro_export]
macro_rules! log_dbg { ($($a:tt)*) => { $crate::win::log::write($crate::win::log::Level::Debug, format_args!($($a)*)) } }
