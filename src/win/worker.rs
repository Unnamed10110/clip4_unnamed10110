//! Background threads (spec 5): the capture thread (snapshot -> item) and the I/O worker
//! (history load/save, clip2 import, thumbnails, image export).
//!
//! Results travel to the UI thread by `PostMessageW` with an owned [`UiMsg`].

use super::blob::{BlobStore, Clip2Blobs};
use super::clipboard::{self, Captured, Snapshot};
use super::msg::{post_ui, UiMsg};
use super::util::{data_dir, guarded, now_unix_ms, SendHwnd};
use super::{dpapi, wic};
use crate::codec;
use crate::model::*;
use std::fs;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::System::Threading::TerminateThread;

pub enum CaptureTask {
    /// Capture the clipboard now. `seq` is the sequence number of the update that triggered it.
    Capture { seq: u32 },
}

pub enum IoTask {
    Load,
    Save(Vec<Item>),
    ImportClip2,
    Thumb { id: u64, key: FormatKey, payload: Payload, max_px: u32 },
    SaveImage { key: FormatKey, payload: Payload, path: String },
    /// Delete every blob file (Clear history).
    ClearBlobs,
}

/// A thread that reads the clipboard (capture, stage 2) and what the guard needs to cut a stuck
/// read short.
struct Reader {
    name: &'static str,
    /// Start (unix ms) of the task in progress, 0 when idle.
    since: Arc<AtomicI64>,
    /// Sequence of the task in progress (diagnostics).
    seq: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct Workers {
    cap: Arc<Mutex<Sender<CaptureTask>>>,
    io: Sender<IoTask>,
    /// Stage 2 requests (see `spawn_completer`).
    rest: Arc<Mutex<Sender<RestReq>>>,
    /// `[capture, stage 2]`
    readers: Arc<Mutex<[Reader; 2]>>,
    blobs: BlobStore,
}

/// A read that has been running this long is stuck on a source whose delayed rendering does not
/// answer. While it runs, the clipboard is held for every app (nobody can copy or paste), and it
/// cannot be cancelled: the only way to make Windows release the clipboard is to end the thread.
/// A legitimate render of a huge Excel range can take longer; holding everyone's clipboard that long
/// is worse than skipping that one copy.
/// Normally 5 s; 3 s in non-blocking mode.
pub fn stall_ms() -> i64 {
    stall_for(super::settings::nonblocking())
}
/// Same, when a paste is already waiting for the clipboard: 750 ms, 300 ms in non-blocking mode.
pub fn paste_waiting_ms() -> i64 {
    paste_wait_for(super::settings::nonblocking())
}

fn stall_for(nonblocking: bool) -> i64 {
    if nonblocking {
        3_000
    } else {
        5_000
    }
}

fn paste_wait_for(nonblocking: bool) -> i64 {
    if nonblocking {
        300
    } else {
        750
    }
}

fn stalled(since: i64, now: i64, limit: i64) -> bool {
    since != 0 && now - since >= limit
}

/// Ends a reader thread.
fn kill(thread: &JoinHandle<()>) {
    use std::os::windows::io::AsRawHandle;
    // SAFETY: the handle belongs to the live `JoinHandle`. The thread sits in a clipboard call and
    // holds none of our locks; the cost is its leaked stack and COM state.
    unsafe {
        let _ = TerminateThread(HANDLE(thread.as_raw_handle()), 1);
    }
}

fn spawn_capture_thread(blobs: BlobStore, since: Arc<AtomicI64>, seq: Arc<AtomicU32>) -> Option<(Sender<CaptureTask>, JoinHandle<()>)> {
    let (tx, rx) = channel::<CaptureTask>();
    let handle = std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            guarded("capture", || {
                while let Ok(t) = rx.recv() {
                    let CaptureTask::Capture { seq: s } = t;
                    seq.store(s, Ordering::SeqCst);
                    since.store(now_unix_ms(), Ordering::SeqCst);
                    post_ui(UiMsg::ReaderStarted);
                    // A panic drops this task only; the loop (and the thread) keeps serving.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| do_capture(&blobs, s)));
                    since.store(0, Ordering::SeqCst);
                    if r.is_err() {
                        crate::log_err!("capture task panicked; dropped");
                    }
                }
            });
        })
        .ok()?;
    Some((tx, handle))
}

static WORKERS: OnceLock<Workers> = OnceLock::new();

/// For the paste thread: a paste is waiting on a clipboard that one of our readers holds.
pub fn unstick(min_ms: i64) -> bool {
    WORKERS.get().is_some_and(|w| w.unstick(min_ms))
}

impl Workers {
    pub fn start(blobs: BlobStore) -> Workers {
        let new_clocks = || (Arc::new(AtomicI64::new(0)), Arc::new(AtomicU32::new(0)));
        let ((cap_since, cap_seq), (rest_since, rest_seq)) = (new_clocks(), new_clocks());
        let (dead_tx, _) = channel::<CaptureTask>();
        let (cap_tx, cap_thread) = match spawn_capture_thread(blobs.clone(), cap_since.clone(), cap_seq.clone()) {
            Some((tx, h)) => (tx, Some(h)),
            None => (dead_tx, None),
        };
        let (io_tx, io_rx) = channel::<IoTask>();
        let b2 = blobs.clone();
        let _ = std::thread::Builder::new().name("io".into()).spawn(move || {
            wic::com_init();
            guarded("io", || {
                while let Ok(t) = io_rx.recv() {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| do_io(&b2, t)));
                    if r.is_err() {
                        crate::log_err!("io task panicked; dropped");
                    }
                }
            });
        });
        let (rest_tx, rest_thread) = spawn_completer(blobs.clone(), rest_since.clone(), rest_seq.clone());
        let readers = [
            Reader { name: "capture", since: cap_since, seq: cap_seq, thread: cap_thread },
            Reader { name: "stage 2 read", since: rest_since, seq: rest_seq, thread: rest_thread },
        ];
        let w = Workers { cap: Arc::new(Mutex::new(cap_tx)), io: io_tx, rest: Arc::new(Mutex::new(rest_tx)), readers: Arc::new(Mutex::new(readers)), blobs };
        let _ = WORKERS.set(w.clone());
        w
    }

    pub fn capture(&self, seq: u32) {
        self.unstick(stall_ms());
        if let Ok(g) = self.cap.lock() {
            let _ = g.send(CaptureTask::Capture { seq });
        }
    }

    pub fn io(&self, t: IoTask) {
        let _ = self.io.send(t);
    }

    /// Asks for the formats stage 1 left out of item `id` (captured at `seq`) once the clipboard
    /// has been quiet for `COMPLETE_QUIET`.
    pub fn complete(&self, seq: u32, id: u64) {
        if let Ok(g) = self.rest.lock() {
            let _ = g.send(RestReq { seq, id });
        }
    }

    /// A clipboard read is in progress.
    pub fn busy(&self) -> bool {
        self.readers.lock().is_ok_and(|r| r.iter().any(|x| x.since.load(Ordering::SeqCst) != 0))
    }

    /// Cuts short every reader whose current task has run for `min_ms` or more: its thread is ended
    /// and replaced, and the copy it was reading is skipped. True when something was cut.
    pub fn unstick(&self, min_ms: i64) -> bool {
        let Ok(mut readers) = self.readers.lock() else { return false };
        let now = now_unix_ms();
        let mut cut = Vec::new();
        for (i, r) in readers.iter_mut().enumerate() {
            let since = r.since.load(Ordering::SeqCst);
            if !stalled(since, now, min_ms) {
                continue;
            }
            let (seq, owner) = (r.seq.load(Ordering::SeqCst), clipboard::content_owner_name());
            crate::log_warn!("{} of sequence {seq} stalled for {} ms (content owner {owner}); cutting it short", r.name, now - since);
            if let Some(t) = r.thread.take() {
                kill(&t);
            }
            r.since.store(0, Ordering::SeqCst);
            // The replacement; the dead thread's queue (at most stale requests) goes with it.
            if i == 0 {
                if let Some((tx, h)) = spawn_capture_thread(self.blobs.clone(), r.since.clone(), r.seq.clone()) {
                    r.thread = Some(h);
                    if let Ok(mut g) = self.cap.lock() {
                        *g = tx;
                    }
                }
            } else {
                let (tx, h) = spawn_completer(self.blobs.clone(), r.since.clone(), r.seq.clone());
                r.thread = h;
                if let Ok(mut g) = self.rest.lock() {
                    *g = tx;
                }
            }
            cut.push(owner);
        }
        drop(readers);
        let Some(owner) = cut.pop() else { return false };
        post_ui(UiMsg::Notice(format!("A copy from {owner} was skipped: it did not hand over its data in time.")));
        // Whether Windows really released the clipboard is the open question of this mechanism.
        let _ = std::thread::Builder::new().name("unstick-check".into()).spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            match clipboard::holder_name() {
                Some(h) => crate::log_warn!("after cutting a read short the clipboard is still held by {h}"),
                None => crate::log_info!("the clipboard was released after cutting a read short"),
            }
        });
        true
    }
}

// ---------------- capture ----------------

/// The capture thread reads through the hidden main window as clipboard "owner" handle; it
/// only reads, so no thread affinity is needed.
static CAPTURE_OWNER: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

pub fn set_capture_owner(h: SendHwnd) {
    CAPTURE_OWNER.store(h.0, std::sync::atomic::Ordering::Relaxed);
}

/// Pause before touching the clipboard. Many sources (every OLE/.NET app) finish with an
/// `OleFlushClipboard` that needs the clipboard right after the change notification; if we
/// open it first, that flush fails while we wait on the source's delayed rendering.
const CAPTURE_SETTLE: std::time::Duration = std::time::Duration::from_millis(25);
/// Non-blocking mode waits longer, so the other clipboard watchers (Flow Launcher, ShareX,
/// AutoHotkey, Windows services), which all read right after a change, are done before we start.
const CAPTURE_SETTLE_NONBLOCKING: std::time::Duration = std::time::Duration::from_millis(150);

fn capture_owner() -> Option<HWND> {
    match CAPTURE_OWNER.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        h => Some(SendHwnd(h).get()),
    }
}

fn do_capture(blobs: &BlobStore, trigger_seq: u32) {
    std::thread::sleep(if super::settings::nonblocking() { CAPTURE_SETTLE_NONBLOCKING } else { CAPTURE_SETTLE });
    match clipboard::capture(capture_owner()) {
        Captured::Busy => {
            post_ui(UiMsg::CaptureBusy { seq: trigger_seq, unreadable: false });
        }
        Captured::Unreadable => {
            post_ui(UiMsg::CaptureBusy { seq: trigger_seq, unreadable: true });
        }
        Captured::Excluded(seq) => {
            // Privacy opt-out: nothing recorded, no sound, nothing logged about content.
            crate::log_dbg!("clipboard update seq {seq} excluded by an opt-out format");
            post_ui(UiMsg::CaptureConsumed { seq });
        }
        Captured::Empty(seq) | Captured::Own(seq) => {
            post_ui(UiMsg::CaptureConsumed { seq });
        }
        Captured::Ok(snap) => {
            // The bytes are safely copied: from here the sequence counts as consumed (lesson 18.4).
            let (seq, lock_ms, more) = (snap.seq, snap.lock_ms, snap.more);
            if clipboard::is_own(seq) {
                post_ui(UiMsg::CaptureConsumed { seq });
                return;
            }
            match build_item(blobs, snap) {
                Some(item) => {
                    post_ui(UiMsg::Captured { seq, item, lock_ms, more });
                }
                None => {
                    post_ui(UiMsg::CaptureConsumed { seq });
                }
            }
        }
    }
}

// ---------------- stage 2 ----------------

/// Stage 1 reads only the essential formats so the source app (Excel above all, which renders each
/// format on request on its UI thread) is not hammered right after Ctrl+C. The rest is read here,
/// once no newer copy has asked for 600 ms.
const COMPLETE_QUIET: Duration = Duration::from_millis(600);

struct RestReq {
    seq: u32,
    id: u64,
}

/// The newest request, once `quiet` has passed without a newer one; `None` when the channel closes.
/// A burst of copies therefore completes only its last copy.
fn coalesce<T>(rx: &Receiver<T>, quiet: Duration) -> Option<T> {
    let mut latest = rx.recv().ok()?;
    while let Ok(next) = rx.recv_timeout(quiet) {
        latest = next;
    }
    Some(latest)
}

/// Its own thread, so a slow source never holds up the capture of the next copy. A read that
/// never returns only stalls stage 2 (the items keep what stage 1 gave them).
fn spawn_completer(blobs: BlobStore, since: Arc<AtomicI64>, seq: Arc<AtomicU32>) -> (Sender<RestReq>, Option<JoinHandle<()>>) {
    let (tx, rx) = channel::<RestReq>();
    let handle = std::thread::Builder::new()
        .name("complete".into())
        .spawn(move || {
            guarded("complete", || {
                while let Some(r) = coalesce(&rx, COMPLETE_QUIET) {
                    seq.store(r.seq, Ordering::SeqCst);
                    since.store(now_unix_ms(), Ordering::SeqCst);
                    post_ui(UiMsg::ReaderStarted);
                    let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| do_complete(&blobs, r)));
                    since.store(0, Ordering::SeqCst);
                    if done.is_err() {
                        crate::log_err!("capture completion panicked; dropped");
                    }
                }
            });
        })
        .ok();
    (tx, handle)
}

fn do_complete(blobs: &BlobStore, r: RestReq) {
    match clipboard::capture_rest(capture_owner(), r.seq) {
        Captured::Ok(snap) => {
            let n = snap.formats.len();
            if let Some(item) = build_item(blobs, snap) {
                crate::log_dbg!("completed seq {}: {n} more formats", r.seq);
                post_ui(UiMsg::Completed { id: r.id, formats: item.formats });
            }
        }
        _ => crate::log_dbg!("completion of seq {} dropped (superseded, private or unreadable)", r.seq),
    }
}

/// Snapshot -> Item: DIB validation, blob offload, preview, search index.
fn build_item(blobs: &BlobStore, snap: Snapshot) -> Option<Item> {
    let mut inline: Vec<(FormatKey, Payload)> = Vec::new();
    for (k, b) in snap.formats {
        if (k.is_std(CF_DIB) || k.is_std(CF_DIBV5)) && !crate::preview::dib_size_ok(&b) {
            crate::log_warn!("rejected an inconsistent DIB ({} bytes)", b.len());
            continue;
        }
        inline.push((k, Payload::inline(b)));
    }
    if inline.is_empty() {
        return None;
    }
    // Derive preview/search from the in-memory bytes, then dehydrate large payloads.
    let mut item = Item::new(0, now_unix_ms(), false, inline);
    dehydrate(blobs, &mut item);
    Some(item)
}

/// Moves every payload >= 256 KB to a blob file (identical payloads share one file).
pub fn dehydrate(blobs: &BlobStore, item: &mut Item) {
    for (k, p) in item.formats.iter_mut() {
        if let Payload::Inline(b) = p {
            if b.len() >= BLOB_THRESHOLD {
                match blobs.put(k, b) {
                    Some(sha1) => *p = Payload::OnDisk { sha1, len: b.len() as u64 },
                    None => crate::log_warn!("blob write failed for {}; keeping it in memory", k.label()),
                }
            }
        }
    }
}

// ---------------- I/O ----------------

fn history_path() -> std::path::PathBuf {
    data_dir().join("history.dat")
}

fn do_io(blobs: &BlobStore, t: IoTask) {
    match t {
        IoTask::Load => {
            let (items, note) = load_history(blobs);
            post_ui(UiMsg::Loaded { items, note });
        }
        IoTask::Save(items) => {
            let ok = save_history(blobs, &items);
            post_ui(UiMsg::Saved { ok });
        }
        IoTask::ImportClip2 => {
            let items = import_clip2(blobs);
            let n = items.len();
            post_ui(UiMsg::Loaded { items, note: Some(format!("Imported {n} items from clip2")) });
        }
        IoTask::Thumb { id, key, payload, max_px } => {
            let bytes = match &payload {
                Payload::Inline(b) => Some(b.to_vec()),
                Payload::OnDisk { sha1, .. } => blobs.get(sha1),
            };
            if let Some((w, h, bgra)) = bytes.and_then(|b| wic::thumbnail(&key, &b, max_px)) {
                post_ui(UiMsg::Thumb { id, w, h, bgra });
            }
        }
        IoTask::SaveImage { key, payload, path } => {
            let bytes = match &payload {
                Payload::Inline(b) => Some(b.to_vec()),
                Payload::OnDisk { sha1, .. } => blobs.get(sha1),
            };
            let ok = bytes.is_some_and(|b| wic::save_png(&key, &b, &path));
            let note = if ok { "Image saved".to_string() } else { "Could not save the image".to_string() };
            post_ui(UiMsg::Notice(note));
        }
        IoTask::ClearBlobs => blobs.clear(),
    }
}

fn read_history_file(path: &std::path::Path, blobs: &BlobStore) -> Option<Vec<Item>> {
    let file = fs::read(path).ok()?;
    let blob = codec::unwrap_file(&file)?;
    let inner = dpapi::unprotect(blob)?;
    Some(codec::decode_inner(&inner, blobs))
}

fn load_history(blobs: &BlobStore) -> (Vec<Item>, Option<String>) {
    let main = history_path();
    let bak = main.with_extension("dat.bak");
    if !main.exists() && !bak.exists() {
        return (Vec::new(), None);
    }
    if let Some(items) = read_history_file(&main, blobs) {
        return (items, None);
    }
    crate::log_err!("history.dat failed to load; trying history.dat.bak");
    if let Some(items) = read_history_file(&bak, blobs) {
        return (items, Some("History was recovered from the backup copy".into()));
    }
    // Keep the broken files for diagnosis, start empty.
    let stamp = now_unix_ms();
    for p in [&main, &bak] {
        if p.exists() {
            let mut dest = p.clone().into_os_string();
            dest.push(format!(".corrupt-{stamp}"));
            let _ = fs::rename(p, dest);
        }
    }
    crate::log_err!("history could not be read; starting empty (old files renamed *.corrupt-{stamp})");
    (Vec::new(), Some("History file was unreadable and has been set aside; starting empty".into()))
}

/// Synchronous save for clean exit / restart / end-of-session.
pub fn save_blocking(blobs: &BlobStore, items: &[Item]) -> bool {
    save_history(blobs, items)
}

fn save_history(blobs: &BlobStore, items: &[Item]) -> bool {
    let mut work: Vec<Item> = items.to_vec();
    for it in work.iter_mut() {
        dehydrate(blobs, it);
    }
    let inner = codec::encode_inner(&work);
    let Some(enc) = dpapi::protect(&inner, "clip4 history") else {
        crate::log_err!("history save: DPAPI protect failed");
        return false;
    };
    let file = codec::wrap_file(&enc);
    let dir = data_dir();
    let _ = fs::create_dir_all(&dir);
    let (main, tmp) = (history_path(), dir.join("history.dat.tmp"));
    let bak = dir.join("history.dat.bak");
    if write_flushed(&tmp, &file).is_err() {
        crate::log_err!("history save: could not write the temp file");
        return false;
    }
    if main.exists() {
        let _ = fs::rename(&main, &bak);
    }
    if move_replace(&tmp, &main).is_err() {
        crate::log_err!("history save: could not move the new file into place");
        // Put the previous generation back so the next start still finds history.
        let _ = fs::rename(&bak, &main);
        return false;
    }
    let keep: std::collections::HashSet<[u8; 20]> = work.iter().flat_map(|i| i.blob_refs().copied()).collect();
    let removed = blobs.gc(&keep);
    crate::log_dbg!("history saved: {} items, {} bytes, {} orphan blobs removed", work.len(), file.len(), removed);
    true
}

fn write_flushed(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = fs::File::create(path)?;
    f.write_all(data)?;
    f.sync_all()
}

fn move_replace(from: &std::path::Path, to: &std::path::Path) -> windows::core::Result<()> {
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let (f, t) = (super::util::wide(&from.to_string_lossy()), super::util::wide(&to.to_string_lossy()));
    // SAFETY: valid NUL-terminated paths.
    unsafe { MoveFileExW(super::util::pcw(&f), super::util::pcw(&t), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) }
}

// ---------------- clip2 import ----------------

pub fn clip2_dir() -> std::path::PathBuf {
    super::util::env_dir("APPDATA").unwrap_or_else(std::env::temp_dir).join("clip2")
}

pub fn clip2_history_exists() -> bool {
    clip2_dir().join("history.dat").exists()
}

fn import_clip2(blobs: &BlobStore) -> Vec<Item> {
    let dir = clip2_dir();
    let Ok(file) = fs::read(dir.join("history.dat")) else { return Vec::new() };
    let inner_owned;
    let inner: &[u8] = match codec::clip2_classify(&file) {
        codec::Clip2File::Dpapi(b) => match dpapi::unprotect(b) {
            Some(v) => {
                inner_owned = v;
                &inner_owned
            }
            None => {
                crate::log_err!("clip2 import: DPAPI decrypt failed");
                return Vec::new();
            }
        },
        codec::Clip2File::Plain(b) => b,
        codec::Clip2File::Invalid => {
            crate::log_err!("clip2 import: unrecognised file");
            return Vec::new();
        }
    };
    let mut items = codec::clip2_import_inner(inner, &Clip2Blobs(dir.join("blobs")), now_unix_ms());
    for it in items.iter_mut() {
        dehydrate(blobs, it);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_yields_the_newest_request_after_the_quiet_period() {
        let (tx, rx) = channel();
        for n in 1..=3 {
            tx.send(n).unwrap();
        }
        assert_eq!(coalesce(&rx, Duration::from_millis(30)), Some(3));
        drop(tx);
        assert_eq!(coalesce(&rx, Duration::from_millis(30)), None, "a closed, empty channel ends the thread");
    }

    #[test]
    fn coalesce_waits_for_a_newer_request_arriving_within_the_quiet_period() {
        let (tx, rx) = channel();
        tx.send(1).unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            tx.send(2).unwrap();
        });
        assert_eq!(coalesce(&rx, Duration::from_millis(200)), Some(2));
        t.join().unwrap();
    }

    #[test]
    fn non_blocking_mode_cuts_stalls_and_paste_waits_shorter() {
        assert_eq!((stall_for(false), stall_for(true)), (5_000, 3_000));
        assert_eq!((paste_wait_for(false), paste_wait_for(true)), (750, 300));
    }

    #[test]
    fn a_task_counts_as_stalled_only_after_the_limit() {
        assert!(!stalled(0, 10_000, 5_000), "idle readers have since == 0");
        assert!(!stalled(1_000, 5_999, 5_000));
        assert!(stalled(1_000, 6_000, 5_000));
    }

    #[test]
    fn a_blocked_thread_can_be_ended() {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::WaitForSingleObject;
        let (tx, rx) = channel::<()>();
        let t = std::thread::spawn(move || {
            let _ = rx.recv(); // blocked for good, like a read stuck in a source app
        });
        kill(&t);
        // SAFETY: the handle is the live thread's.
        let r = unsafe { WaitForSingleObject(HANDLE(t.as_raw_handle()), 2000) };
        assert_eq!(r, WAIT_OBJECT_0, "the thread ended");
        drop(tx);
    }
}
