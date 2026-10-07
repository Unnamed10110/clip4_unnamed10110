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
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::Win32::Foundation::HWND;

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

#[derive(Clone)]
pub struct Workers {
    cap: Arc<Mutex<Sender<CaptureTask>>>,
    /// Start time (unix ms) of the capture in progress, 0 when idle — for the 30 s watchdog.
    cap_since: Arc<AtomicI64>,
    io: Sender<IoTask>,
    /// Stage 2 requests (see `spawn_completer`).
    rest: Sender<RestReq>,
    blobs: BlobStore,
}

/// A capture that has been running this long is stuck (a source whose delayed rendering never
/// returns). It cannot be cancelled, so the capture thread is replaced (spec 19.4).
const CAPTURE_WATCHDOG_MS: i64 = 30_000;

fn spawn_capture_thread(blobs: BlobStore, since: Arc<AtomicI64>) -> Option<Sender<CaptureTask>> {
    let (tx, rx) = channel::<CaptureTask>();
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            guarded("capture", || {
                while let Ok(t) = rx.recv() {
                    since.store(now_unix_ms(), Ordering::SeqCst);
                    // A panic drops this task only; the loop (and the thread) keeps serving.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match t {
                        CaptureTask::Capture { seq } => do_capture(&blobs, seq),
                    }));
                    since.store(0, Ordering::SeqCst);
                    if r.is_err() {
                        crate::log_err!("capture task panicked; dropped");
                    }
                }
            });
        })
        .ok()?;
    Some(tx)
}

impl Workers {
    pub fn start(blobs: BlobStore) -> Workers {
        let cap_since = Arc::new(AtomicI64::new(0));
        let (dead_tx, _) = channel::<CaptureTask>();
        let cap_tx = spawn_capture_thread(blobs.clone(), cap_since.clone()).unwrap_or(dead_tx);
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
        let rest = spawn_completer(blobs.clone());
        Workers { cap: Arc::new(Mutex::new(cap_tx)), cap_since, io: io_tx, rest, blobs }
    }

    pub fn capture(&self, seq: u32) {
        let since = self.cap_since.load(Ordering::SeqCst);
        if since != 0 && now_unix_ms() - since > CAPTURE_WATCHDOG_MS {
            crate::log_err!("capture watchdog: a capture has been stuck for {} s; replacing the capture thread", (now_unix_ms() - since) / 1000);
            self.cap_since.store(0, Ordering::SeqCst);
            if let (Some(tx), Ok(mut g)) = (spawn_capture_thread(self.blobs.clone(), self.cap_since.clone()), self.cap.lock()) {
                *g = tx; // the stuck thread's receiver is dropped; it exits if it ever unblocks
            }
        }
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
        let _ = self.rest.send(RestReq { seq, id });
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

fn capture_owner() -> Option<HWND> {
    match CAPTURE_OWNER.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        h => Some(SendHwnd(h).get()),
    }
}

fn do_capture(blobs: &BlobStore, trigger_seq: u32) {
    std::thread::sleep(CAPTURE_SETTLE);
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
fn spawn_completer(blobs: BlobStore) -> Sender<RestReq> {
    let (tx, rx) = channel::<RestReq>();
    let _ = std::thread::Builder::new().name("complete".into()).spawn(move || {
        guarded("complete", || {
            while let Some(r) = coalesce(&rx, COMPLETE_QUIET) {
                let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| do_complete(&blobs, r)));
                if done.is_err() {
                    crate::log_err!("capture completion panicked; dropped");
                }
            }
        });
    });
    tx
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
}
