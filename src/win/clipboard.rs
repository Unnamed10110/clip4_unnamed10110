//! Clipboard access: RAII lock, snapshot capture, atomic write, backup/restore.
//!
//! The one rule (spec 6.2 / lesson 18.1): the clipboard is held open ONLY for raw byte
//! copies. Everything else — hashing, parsing, indexing, disk — happens after the
//! [`ClipboardGuard`] is dropped, on another thread.

use super::util::{from_wide, pcw, sleep_pump, wide};
use crate::model::*;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;
use windows::Win32::Foundation::{CloseHandle, GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_GDI, TYMED_HGLOBAL};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::{OleGetClipboard, OleInitialize, OleUninitialize, ReleaseStgMedium};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

// ---- limits (spec 6.4) ----
pub const MAX_FORMATS: usize = 12;
pub const MAX_FORMAT_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_PRIMARY_IMAGE_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_ITEM_BYTES: usize = 10 * 1024 * 1024;

/// Writes (paste, restore, backup, snippet `{{clipboard}}`) keep trying ~1.5 s: another clipboard
/// watcher (Flow Launcher, ShareX, ...) may be reading right after every change.
const WRITE_TRIES: u32 = 100;
const WRITE_GAP_MS: u64 = 15;

/// Open clipboard; closing is `Drop`, so no return path can leave it locked.
pub struct ClipboardGuard(());

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: only constructed after a successful OpenClipboard on this thread.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Opens the clipboard, retrying `attempts` times `delay_ms` apart. No wait after the final
/// attempt (lesson 18.12). Waits keep pumping this thread's messages (see `util::sleep_pump`).
/// Never called on the UI thread.
pub fn open(owner: Option<HWND>, attempts: u32, delay_ms: u64) -> Option<ClipboardGuard> {
    for i in 0..attempts {
        // SAFETY: OpenClipboard has no memory-safety preconditions.
        if unsafe { OpenClipboard(owner) }.is_ok() {
            return Some(ClipboardGuard(()));
        }
        if i + 1 < attempts {
            sleep_pump(delay_ms);
        }
    }
    None
}

/// The clipboard holds at least one format. No lock needed.
pub fn has_any_format() -> bool {
    // SAFETY: plain query.
    unsafe { CountClipboardFormats() > 0 }
}

/// The content carries one of the privacy formats (spec 20.1), whatever its value. No lock
/// needed: `IsClipboardFormatAvailable` works without opening the clipboard.
pub fn looks_private() -> bool {
    ["ExcludeClipboardContentFromMonitorProcessing", "Clipboard Viewer Ignore", "CanIncludeInClipboardHistory"]
        .iter()
        // SAFETY: plain query on a registered format id.
        .any(|n| unsafe { IsClipboardFormatAvailable(register(n)) }.is_ok())
}

/// Process currently holding the clipboard open, if anyone does (diagnostics only).
pub fn holder_name() -> Option<String> {
    // SAFETY: plain query.
    let h = unsafe { GetOpenClipboardWindow() }.ok()?;
    Some(process_name_of_window(h))
}

/// Process that owns the current clipboard content (diagnostics only).
fn content_owner_name() -> String {
    // SAFETY: plain query; works without opening the clipboard.
    match unsafe { GetClipboardOwner() } {
        Ok(h) if !h.0.is_null() => process_name_of_window(h),
        _ => "<none>".into(),
    }
}

/// Name of the process currently holding the clipboard open (diagnostics only).
pub fn open_owner_name() -> String {
    // SAFETY: plain queries.
    unsafe {
        let Ok(hwnd) = GetOpenClipboardWindow() else { return "<unknown>".into() };
        process_name_of_window(hwnd)
    }
}

pub fn process_name_of_window(hwnd: HWND) -> String {
    // SAFETY: handles are closed on every path.
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return format!("pid {pid}") };
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if ok {
            let full = from_wide(&buf[..len as usize]);
            full.rsplit('\\').next().unwrap_or(&full).to_string()
        } else {
            format!("pid {pid}")
        }
    }
}

// ---- format names ----

static NAMES: Mutex<Option<HashMap<u32, String>>> = Mutex::new(None);

/// Registered-format id -> name (cached; ids are stable within a Windows session).
pub fn format_name(id: u32) -> Option<String> {
    if let Ok(g) = NAMES.lock() {
        if let Some(n) = g.as_ref().and_then(|m| m.get(&id)) {
            return Some(n.clone());
        }
    }
    let mut buf = [0u16; 256];
    // SAFETY: buffer is valid for its length.
    let n = unsafe { GetClipboardFormatNameW(id, &mut buf) };
    if n <= 0 {
        return None;
    }
    let name = from_wide(&buf[..n as usize]);
    if let Ok(mut g) = NAMES.lock() {
        g.get_or_insert_with(HashMap::new).insert(id, name.clone());
    }
    Some(name)
}

/// Name -> id (re-resolved at every paste; lesson 18.15).
pub fn register(name: &str) -> u32 {
    let w = wide(name);
    // SAFETY: valid NUL-terminated string.
    unsafe { RegisterClipboardFormatW(pcw(&w)) }
}

pub fn key_for(id: u32) -> Option<FormatKey> {
    if id >= 0xC000 {
        format_name(id).map(FormatKey::Registered)
    } else {
        Some(FormatKey::Standard(id))
    }
}

pub fn id_for(key: &FormatKey) -> u32 {
    match key {
        FormatKey::Standard(id) => *id,
        FormatKey::Registered(n) => register(n),
    }
}

pub fn sequence() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetClipboardSequenceNumber() }
}

// ---- own-sequence ring (spec 6.5) ----

const RING: usize = 32;
static OWN: Mutex<([u32; RING], usize)> = Mutex::new(([0; RING], 0));

/// The paste thread's clipboard-owner window. `EmptyClipboard` makes it the owner, so a
/// capture that finds us as owner knows the content is ours — race-free, unlike sequence
/// numbers alone (the sequence can advance once more as the clipboard is closed).
static OWN_OWNER: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

pub fn set_own_owner(h: HWND) {
    OWN_OWNER.store(h.0 as isize, std::sync::atomic::Ordering::SeqCst);
}

pub fn owner_is_us() -> bool {
    let me = OWN_OWNER.load(std::sync::atomic::Ordering::SeqCst);
    // SAFETY: plain query.
    me != 0 && unsafe { GetClipboardOwner() }.is_ok_and(|h| h.0 as isize == me)
}

/// When clip4 last wrote the clipboard (unix ms).
static LAST_OWN_WRITE_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// True if clip4 wrote the clipboard within the last `ms` (apps such as Office write a pasted
/// item back right after a paste; that change is not a copy by the user).
pub fn own_write_within(ms: i64) -> bool {
    super::util::now_unix_ms() - LAST_OWN_WRITE_MS.load(std::sync::atomic::Ordering::Relaxed) < ms
}

pub fn mark_own(seq: u32) {
    LAST_OWN_WRITE_MS.store(super::util::now_unix_ms(), std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut g) = OWN.lock() {
        let i = g.1 % RING;
        g.0[i] = seq;
        g.1 = g.1.wrapping_add(1);
    }
}

pub fn is_own(seq: u32) -> bool {
    seq != 0 && OWN.lock().map(|g| g.0.contains(&seq)).unwrap_or(false)
}

// ---- reading ----

enum Read {
    Bytes(Vec<u8>),
    TooBig(usize),
    Fail,
}

fn read_hglobal(h: HANDLE, max: usize, trim_nul: Option<usize>) -> Read {
    let hg = HGLOBAL(h.0);
    // SAFETY: lock/size/copy/unlock on a clipboard-owned HGLOBAL while the clipboard is open.
    unsafe {
        let size = GlobalSize(hg);
        if size == 0 {
            return Read::Fail;
        }
        if size > max {
            return Read::TooBig(size);
        }
        let p = GlobalLock(hg) as *const u8;
        if p.is_null() {
            return Read::Fail;
        }
        let src = std::slice::from_raw_parts(p, size);
        // GlobalSize is an upper bound, not the payload length: text stops at the first NUL (spec 19.2).
        let n = match trim_nul {
            Some(2) => src.chunks_exact(2).position(|c| c == [0, 0]).map_or(size & !1, |i| i * 2),
            Some(_) => src.iter().position(|&b| b == 0).unwrap_or(size),
            None => size,
        };
        let extra = trim_nul.unwrap_or(0);
        let mut v = Vec::new();
        let r = if v.try_reserve_exact(n + extra).is_ok() {
            v.extend_from_slice(&src[..n]);
            v.extend(std::iter::repeat_n(0u8, extra));
            Read::Bytes(v)
        } else {
            Read::TooBig(size)
        };
        let _ = GlobalUnlock(hg);
        r
    }
}

/// CF_BITMAP -> packed DIB bytes (BITMAPINFOHEADER + 32bpp pixels). Handle-based formats
/// are not HGLOBALs and must not be GlobalLock'ed (spec 6.4).
fn bitmap_to_dib(h: HANDLE, max: usize) -> Read {
    // SAFETY: GDI calls on the clipboard-owned bitmap while the clipboard is open.
    unsafe {
        let hbm = HBITMAP(h.0);
        let mut bm = BITMAP::default();
        if GetObjectW(hbm.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0 {
            return Read::Fail;
        }
        let (w, hgt) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || hgt <= 0 {
            return Read::Fail;
        }
        let stride = (w as usize) * 4;
        let Some(img) = stride.checked_mul(hgt as usize) else { return Read::Fail };
        if img + 40 > max {
            return Read::TooBig(img + 40);
        }
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: 40,
                biWidth: w,
                biHeight: hgt,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: img as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut out = Vec::new();
        if out.try_reserve_exact(40 + img).is_err() {
            return Read::TooBig(img + 40);
        }
        out.resize(40 + img, 0u8);
        let hdc = GetDC(None);
        let got = GetDIBits(hdc, hbm, 0, hgt as u32, Some(out[40..].as_mut_ptr() as *mut _), &mut bmi, DIB_RGB_COLORS);
        ReleaseDC(None, hdc);
        if got == 0 {
            return Read::Fail;
        }
        let hdr: [u8; 40] = std::mem::transmute(bmi.bmiHeader);
        out[..40].copy_from_slice(&hdr);
        Read::Bytes(out)
    }
}

// ---- capture ----

pub struct Snapshot {
    pub seq: u32,
    pub formats: Vec<(FormatKey, Vec<u8>)>,
    pub lock_ms: u32,
    /// Formats were left for `capture_rest` (stage 2).
    pub more: bool,
}

pub enum Captured {
    Ok(Snapshot),
    /// A privacy opt-out format was present (spec 20.1): record nothing.
    Excluded(u32),
    /// Nothing capturable (e.g. only delayed handle formats).
    Empty(u32),
    /// clip4 itself wrote this content (spec 6.5): nothing to record.
    Own(u32),
    /// Could not open the clipboard within the retry ladder.
    Busy,
    /// The source announced data but handed none over (each retry costs lock time: retried briefly).
    Unreadable,
}

const SKIP_NAMES: [&str; 14] = [
    "DataObject",
    "Ole Private Data",
    "OwnerLink",
    "ObjectLink",
    "Native",
    "Link Source",
    "Link Source Descriptor",
    "Object Descriptor",
    "Embed Source",
    "Embedded Object",
    "Shell IDList Array",
    "Shell Object Offsets",
    "FileGroupDescriptor",
    "FileGroupDescriptorW",
];

fn is_exclusion_name(n: &str) -> bool {
    n.eq_ignore_ascii_case("ExcludeClipboardContentFromMonitorProcessing") || n.eq_ignore_ascii_case("Clipboard Viewer Ignore")
}

/// Formats that stand in for each other: the first readable member is captured (Windows
/// synthesises the others when we paste, spec 6.4).
type Group = Vec<(u32, FormatKey)>;

/// What `capture` reads, in priority order (spec 6.4): file list, text, one image, the rich
/// formats, then any other registered format; at most `MAX_FORMATS` groups. Pure.
pub(crate) fn plan_formats(ids: &[u32], named: &[(u32, String)]) -> Vec<Group> {
    let has = |cf: u32| ids.contains(&cf);
    let family = |cfs: &[u32]| -> Group { cfs.iter().copied().filter(|&cf| has(cf)).map(|cf| (cf, FormatKey::Standard(cf))).collect() };
    let name_id = |n: &str| named.iter().find(|(_, x)| x.eq_ignore_ascii_case(n)).map(|(i, _)| *i);
    let mut plan: Vec<Group> = Vec::new();
    for g in [family(&[CF_HDROP]), family(&[CF_UNICODETEXT, CF_TEXT]), family(&[CF_DIBV5, CF_DIB, CF_BITMAP])] {
        if !g.is_empty() {
            plan.push(g);
        }
    }
    const KNOWN: [&str; 4] = [FMT_PNG, FMT_HTML, FMT_RTF, FMT_DROPEFFECT];
    for n in KNOWN {
        if let Some(id) = name_id(n) {
            plan.push(vec![(id, FormatKey::reg(n))]);
        }
    }
    for (id, n) in named {
        let skip = KNOWN.iter().chain(SKIP_NAMES.iter()).any(|k| k.eq_ignore_ascii_case(n))
            || n.eq_ignore_ascii_case("CanIncludeInClipboardHistory")
            || n.eq_ignore_ascii_case("CanUploadToCloudClipboard")
            // Chromium/WebView2 bookkeeping ("…source URL", "…source RFH token"): rendered on demand
            // (slow, under the lock) and of no use to any paste target.
            || n.starts_with("Chromium internal source")
            || is_exclusion_name(n);
        if !skip {
            plan.push(vec![(*id, FormatKey::Registered(n.clone()))]);
        }
    }
    plan.truncate(MAX_FORMATS);
    plan
}

/// Splits a plan into what is read at once and what is read later (see `capture_rest`): the file
/// list, the text, and the image only when there is neither (a screenshot). Everything else is
/// the "rest". A source with none of those has nothing to defer: all of it is read at once. Pure.
pub(crate) fn split_plan(plan: Vec<Group>) -> (Vec<Group>, Vec<Group>) {
    let has = |g: &Group, cfs: &[u32]| g.iter().any(|(_, k)| cfs.iter().any(|&cf| k.is_std(cf)));
    let files_or_text = plan.iter().any(|g| has(g, &[CF_HDROP, CF_UNICODETEXT, CF_TEXT]));
    let (first, rest): (Vec<Group>, Vec<Group>) = plan
        .into_iter()
        .partition(|g| has(g, &[CF_HDROP, CF_UNICODETEXT, CF_TEXT]) || (!files_or_text && has(g, &[CF_DIBV5, CF_DIB, CF_BITMAP])));
    if first.is_empty() {
        (rest, Vec::new())
    } else {
        (first, rest)
    }
}

/// Bytes of one retrieved clipboard handle (bitmap / UTF-16 text / ANSI text / raw HGLOBAL).
fn read_handle(h: HANDLE, key: &FormatKey, cap: usize) -> Read {
    if key.is_std(CF_BITMAP) {
        bitmap_to_dib(h, cap)
    } else if key.is_std(CF_UNICODETEXT) {
        read_hglobal(h, cap, Some(2))
    } else if key.is_std(CF_TEXT) {
        read_hglobal(h, cap, Some(1))
    } else {
        read_hglobal(h, cap, None)
    }
}

/// `OleInitialize` for the duration of one OLE read (the capture thread is otherwise a plain
/// thread that sits in `recv`, which an STA must not do).
struct OleScope;

impl OleScope {
    fn init() -> Option<OleScope> {
        // SAFETY: balanced by OleUninitialize in Drop (S_FALSE also needs the balance).
        unsafe { OleInitialize(None) }.ok().map(|_| OleScope)
    }
}

impl Drop for OleScope {
    fn drop(&mut self) {
        // SAFETY: matches the successful OleInitialize in `init`.
        unsafe { OleUninitialize() };
    }
}

/// One format through OLE. Ok(Read) when the source handed something over; Err(HRESULT) when
/// `GetData` itself failed.
fn ole_get(obj: &IDataObject, id: u32, key: &FormatKey, cap: usize) -> Result<Read, i32> {
    let gdi = key.is_std(CF_BITMAP);
    let tymed = (if gdi { TYMED_GDI } else { TYMED_HGLOBAL }).0 as u32;
    // Registered format ids are <= 0xFFFF, so the u16 FORMATETC field holds every id.
    let fe = FORMATETC { cfFormat: id as u16, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed };
    // SAFETY: COM call on a live data object; the returned medium is released exactly once below.
    let mut m = unsafe { obj.GetData(&fe) }.map_err(|e| e.code().0)?;
    let r = if m.tymed == tymed {
        // SAFETY: the union member read matches the tymed the source actually returned.
        let h = unsafe { if gdi { m.u.hBitmap.0 } else { m.u.hGlobal.0 } };
        read_handle(HANDLE(h), key, cap)
    } else {
        Read::Fail
    };
    // SAFETY: releases the handle (and pUnkForRelease) the way the source asked; never touched again.
    unsafe { ReleaseStgMedium(&mut m) };
    Ok(r)
}

enum PlanRead {
    Excluded,
    Formats { formats: Vec<(FormatKey, Vec<u8>)>, unreadable: Vec<String> },
}

/// Reads the plan through `fetch` (raw `GetClipboardData` or OLE `GetData`), first readable
/// member of each group, within the size limits (spec 6.4).
fn read_plan(plan: &[Group], can_include: Option<u32>, has_text_or_files: bool, path: &str, mut fetch: impl FnMut(u32, &FormatKey, usize) -> Result<Read, i32>) -> PlanRead {
    // CanIncludeInClipboardHistory = 0 is an opt-out (spec 20.1): checked before any content.
    if let Some(id) = can_include {
        if let Ok(Read::Bytes(b)) = fetch(id, &FormatKey::reg("CanIncludeInClipboardHistory"), 16) {
            if b.len() >= 4 && u32::from_le_bytes([b[0], b[1], b[2], b[3]]) == 0 {
                return PlanRead::Excluded;
            }
        }
    }
    let mut formats: Vec<(FormatKey, Vec<u8>)> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    let mut total = 0usize;
    for group in plan {
        let image = group.iter().any(|(_, k)| k.is_std(CF_DIBV5) || k.is_std(CF_DIB) || k.is_std(CF_BITMAP));
        let primary_image = image && !has_text_or_files;
        let cap = if primary_image { MAX_PRIMARY_IMAGE_BYTES } else { MAX_FORMAT_BYTES };
        if !primary_image && total + 1 > MAX_ITEM_BYTES {
            continue;
        }
        for (id, key) in group {
            let t = Instant::now();
            let r = fetch(*id, key, cap);
            let ms = t.elapsed().as_millis();
            if ms > 20 {
                crate::log_info!("slow format {} ({path}): {ms} ms", key.label());
            }
            match r {
                Ok(Read::Bytes(b)) => {
                    if !primary_image && total + b.len() > MAX_ITEM_BYTES {
                        crate::log_dbg!("format {} skipped: item byte cap", key.label());
                    } else {
                        if !primary_image {
                            total += b.len();
                        }
                        // CF_BITMAP is stored as CF_DIB.
                        let k = if key.is_std(CF_BITMAP) { FormatKey::Standard(CF_DIB) } else { key.clone() };
                        formats.push((k, b));
                    }
                    break;
                }
                // Its siblings would be just as big.
                Ok(Read::TooBig(n)) => {
                    crate::log_info!("format {} skipped: {n} bytes over limit", key.label());
                    break;
                }
                // Try the next member of the group.
                Ok(Read::Fail) => unreadable.push(key.label()),
                Err(code) => unreadable.push(format!("{} ({code:#x})", key.label())),
            }
        }
    }
    PlanRead::Formats { formats, unreadable }
}

fn finish(seq: u32, r: PlanRead, lock_ms: u32, more: bool) -> Captured {
    match r {
        PlanRead::Excluded => Captured::Excluded(seq),
        PlanRead::Formats { formats, unreadable } => {
            if formats.is_empty() {
                if unreadable.is_empty() {
                    return Captured::Empty(seq);
                }
                // The source offered data but would not hand it over right now: retried (bounded)
                // instead of silently losing the copy.
                crate::log_info!("capture of sequence {seq}: nothing readable [{}]; content owner {}", unreadable.join(", "), content_owner_name());
                return Captured::Unreadable;
            }
            if !unreadable.is_empty() {
                crate::log_dbg!("capture of sequence {seq}: unreadable [{}]", unreadable.join(", "));
            }
            Captured::Ok(Snapshot { seq, formats, lock_ms, more })
        }
    }
}

/// Takes a raw-bytes snapshot.
///
/// The global clipboard lock is held only for cheap work: sequence number, format list, privacy
/// check. For a source that put a live OLE data object on the clipboard ("DataObject": DBeaver,
/// Office, .NET apps, ...) every read is a cross-process render, so the lock is released and the
/// data is read through OLE, which talks to the source directly; other apps can copy meanwhile.
/// Other sources are read under the lock as plain byte copies.
///
/// Stage 1: only the essential groups (`split_plan`), so the item shows up at once and the source
/// is asked for little right after the copy (Excel renders every extra format on its UI thread).
pub fn capture(owner: Option<HWND>) -> Captured {
    capture_stage(owner, Stage::First)
}

/// Stage 2: the formats `capture` left out (`Snapshot::more`), read once the clipboard has been
/// quiet. Yields `Empty` unless the clipboard is still at `seq`.
pub fn capture_rest(owner: Option<HWND>, seq: u32) -> Captured {
    capture_stage(owner, Stage::Rest(seq))
}

#[derive(Clone, Copy)]
enum Stage {
    First,
    Rest(u32),
}

fn capture_stage(owner: Option<HWND>, stage: Stage) -> Captured {
    let (c, used_ole) = capture_impl(owner, true, stage);
    if used_ole && matches!(c, Captured::Unreadable) {
        // Some OLE sources refuse direct reads (DBeaver/SWT answers CLIPBRD_E_BAD_DATA): read
        // them the classic way, under the lock.
        crate::log_dbg!("OLE read returned nothing; reading under the clipboard lock instead");
        return capture_impl(owner, false, stage).0;
    }
    c
}

/// One capture attempt; the bool says whether the OLE path was used.
fn capture_impl(owner: Option<HWND>, allow_ole: bool, stage: Stage) -> (Captured, bool) {
    let Some(guard) = open(owner, 8, 5) else {
        crate::log_warn!("OpenClipboard failed after 8 attempts (capture); owner = {}", open_owner_name());
        return (Captured::Busy, false);
    };
    let t0 = Instant::now();
    let seq = sequence();
    if owner_is_us() {
        return (Captured::Own(seq), false);
    }

    // Enumerate.
    let mut ids: Vec<u32> = Vec::new();
    let mut f = 0u32;
    loop {
        // SAFETY: clipboard is open.
        f = unsafe { EnumClipboardFormats(f) };
        if f == 0 || ids.len() > 200 {
            break;
        }
        ids.push(f);
    }

    // Privacy opt-outs first (spec 20.1): by name, no content is read at all.
    let mut named: Vec<(u32, String)> = Vec::new();
    for &id in ids.iter().filter(|&&i| i >= 0xC000) {
        if let Some(n) = format_name(id) {
            if is_exclusion_name(&n) {
                return (Captured::Excluded(seq), false);
            }
            named.push((id, n));
        }
    }
    let can_include = named.iter().find(|(_, n)| n.eq_ignore_ascii_case("CanIncludeInClipboardHistory")).map(|(i, _)| *i);
    let has_text_or_files = ids.iter().any(|&i| matches!(i, CF_UNICODETEXT | CF_TEXT | CF_HDROP));
    let ole = named.iter().any(|(_, n)| n.eq_ignore_ascii_case("DataObject"));
    let (first, rest) = split_plan(plan_formats(&ids, &named));
    let (plan, more) = match stage {
        Stage::First => {
            let more = !rest.is_empty();
            (first, more)
        }
        // A newer copy replaced it: that one has its own capture.
        Stage::Rest(want) if want != seq => return (Captured::Empty(seq), false),
        Stage::Rest(_) => (rest, false),
    };

    if ole && allow_ole {
        // OLE opens the clipboard itself: ours must be closed first.
        drop(guard);
        let lock_ms = t0.elapsed().as_millis() as u32;
        return (capture_ole(seq, &plan, can_include, has_text_or_files, lock_ms, more), true);
    }

    let r = read_plan(&plan, can_include, has_text_or_files, "raw", |id, key, cap| {
        // SAFETY: clipboard is open (the guard lives until after read_plan).
        match unsafe { GetClipboardData(id) } {
            Ok(h) => Ok(read_handle(h, key, cap)),
            Err(e) => Err(e.code().0),
        }
    });
    drop(guard);
    let lock_ms = t0.elapsed().as_millis() as u32;
    if lock_ms > 20 {
        crate::log_warn!("clipboard lock held {lock_ms} ms during capture");
    }
    (finish(seq, r, lock_ms, more), false)
}

/// The OLE half of `capture`: runs with the clipboard closed.
fn capture_ole(seq: u32, plan: &[Group], can_include: Option<u32>, has_text_or_files: bool, lock_ms: u32, more: bool) -> Captured {
    let Some(_ole) = OleScope::init() else {
        crate::log_warn!("OleInitialize failed on the capture thread");
        return Captured::Busy;
    };
    // SAFETY: OLE is initialised on this thread for as long as `obj` lives (it is dropped first).
    let obj = match unsafe { OleGetClipboard() } {
        Ok(o) => o,
        Err(e) => {
            crate::log_info!("OleGetClipboard failed ({:#x}); clipboard held by {}", e.code().0, open_owner_name());
            return Captured::Busy;
        }
    };
    let t = Instant::now();
    let r = read_plan(plan, can_include, has_text_or_files, "ole", |id, key, cap| ole_get(&obj, id, key, cap));
    drop(obj);
    let ole_ms = t.elapsed().as_millis();
    // The content changed while we were reading: what we have may be a mix. The newer change
    // has its own notification and capture.
    if sequence() != seq {
        crate::log_dbg!("clipboard changed during the OLE read of sequence {seq}; discarded");
        return Captured::Empty(seq);
    }
    crate::log_dbg!("OLE capture of sequence {seq}: lock {lock_ms} ms, read {ole_ms} ms");
    finish(seq, r, lock_ms, more)
}

// ---- writing ----

/// An allocated, filled HGLOBAL that frees itself unless ownership is handed to the clipboard.
pub struct Prepared {
    id: u32,
    h: Option<HGLOBAL>,
}

impl Prepared {
    fn into_raw(mut self) -> (u32, HGLOBAL) {
        (self.id, self.h.take().unwrap_or_default())
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        if let Some(h) = self.h.take() {
            // SAFETY: still owned by us (never passed to SetClipboardData).
            unsafe {
                let _ = GlobalFree(Some(h));
            }
        }
    }
}

fn alloc_global(bytes: &[u8], terminate: usize) -> Option<HGLOBAL> {
    // SAFETY: allocate, lock, copy, unlock.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len() + terminate).ok()?;
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Some(h)
    }
}

/// Allocates every HGLOBAL BEFORE the clipboard is opened (lesson 18.8: prepare first, empty second).
pub fn prepare(formats: &[(FormatKey, Vec<u8>)]) -> Option<Vec<Prepared>> {
    let mut out = Vec::with_capacity(formats.len());
    for (k, b) in formats {
        let id = id_for(k);
        if id == 0 {
            continue;
        }
        let (body, term) = if k.is_std(CF_UNICODETEXT) {
            let n = b.chunks_exact(2).position(|c| c == [0, 0]).map_or(b.len() & !1, |i| i * 2);
            (&b[..n], 2)
        } else if k.is_std(CF_TEXT) {
            let n = b.iter().position(|&x| x == 0).unwrap_or(b.len());
            (&b[..n], 1)
        } else {
            (&b[..], 0)
        };
        let h = alloc_global(body, term)?;
        out.push(Prepared { id, h: Some(h) });
    }
    (!out.is_empty()).then_some(out)
}

/// Replaces the clipboard contents with `prepared`. Returns the resulting sequence number
/// (also recorded as an own echo). On any failure before `EmptyClipboard` the user's
/// clipboard is untouched.
pub fn write_prepared(owner: HWND, prepared: Vec<Prepared>) -> Result<u32, &'static str> {
    let Some(guard) = open(Some(owner), WRITE_TRIES, WRITE_GAP_MS) else {
        crate::log_warn!("OpenClipboard failed (write); owner = {}", open_owner_name());
        return Err("clipboard busy");
    };
    // SAFETY: clipboard open with a real owner window (lesson 18.22).
    if unsafe { EmptyClipboard() }.is_err() {
        return Err("EmptyClipboard failed");
    }
    mark_own(sequence());
    let mut ok = 0;
    for p in prepared {
        let (id, h) = p.into_raw();
        // SAFETY: on success the system owns h; on failure we free it.
        match unsafe { SetClipboardData(id, Some(HANDLE(h.0))) } {
            Ok(_) => ok += 1,
            Err(e) => {
                crate::log_warn!("SetClipboardData({id}) failed: {e}");
                // SAFETY: ownership did not transfer.
                unsafe {
                    let _ = GlobalFree(Some(h));
                }
            }
        }
        mark_own(sequence());
    }
    let seq = sequence();
    mark_own(seq);
    drop(guard);
    if ok == 0 {
        return Err("no format accepted");
    }
    Ok(seq)
}

// ---- backup / restore (spec 12.6) ----

#[derive(Default)]
pub struct Backup {
    pub formats: Vec<(u32, Vec<u8>)>,
}

/// Copies every HGLOBAL-based format of the current clipboard (CF_BITMAP as DIB).
pub fn backup(owner: HWND) -> Option<Backup> {
    let guard = open(Some(owner), WRITE_TRIES, WRITE_GAP_MS)?;
    let mut b = Backup::default();
    let mut f = 0u32;
    let mut total = 0usize;
    loop {
        // SAFETY: clipboard open.
        f = unsafe { EnumClipboardFormats(f) };
        if f == 0 {
            break;
        }
        let skip_handle = matches!(f, CF_PALETTE | CF_ENHMETAFILE | CF_METAFILEPICT | 0x82 | 0x8E | 0x83) || (0x200..0x300).contains(&f) || (0x300..0x400).contains(&f);
        if skip_handle || matches!(f, CF_OEMTEXT | CF_LOCALE) {
            continue;
        }
        // SAFETY: clipboard open.
        let Ok(h) = (unsafe { GetClipboardData(f) }) else { continue };
        let left = MAX_PRIMARY_IMAGE_BYTES.saturating_sub(total);
        let (id, r) = if f == CF_BITMAP {
            (CF_DIB, bitmap_to_dib(h, left))
        } else {
            (f, read_hglobal(h, left, None))
        };
        if let Read::Bytes(v) = r {
            total += v.len();
            // Skip duplicates synthesised from a format we already hold.
            if !b.formats.iter().any(|(i, _)| *i == id) {
                b.formats.push((id, v));
            }
        }
    }
    drop(guard);
    Some(b)
}

/// Restores a backup (with the ~1.5 s write budget). Never empties when the backup is empty.
pub fn restore(owner: HWND, b: &Backup) -> bool {
    if b.formats.is_empty() {
        return true;
    }
    // A backup that carries both DIB-ish and its synthesised twin would be re-synthesised anyway.
    let mut prepared = Vec::new();
    for (id, bytes) in &b.formats {
        let (body, term) = match *id {
            CF_UNICODETEXT => (&bytes[..], 2usize.saturating_sub(trailing_nul(bytes, 2))),
            CF_TEXT => (&bytes[..], 1usize.saturating_sub(trailing_nul(bytes, 1))),
            _ => (&bytes[..], 0),
        };
        if let Some(h) = alloc_global(body, term) {
            prepared.push(Prepared { id: *id, h: Some(h) });
        }
    }
    if prepared.is_empty() {
        return false;
    }
    let ok = write_prepared(owner, prepared).is_ok();
    if !ok {
        crate::log_err!("CLIPBOARD RESTORE FAILED");
    }
    ok
}

fn trailing_nul(b: &[u8], unit: usize) -> usize {
    if b.len() >= unit && b[b.len() - unit..].iter().all(|&x| x == 0) {
        unit
    } else {
        0
    }
}

/// Current clipboard text via a short open/copy/close (for `{{clipboard}}`).
pub fn read_text_now(owner: HWND) -> Option<String> {
    let guard = open(Some(owner), WRITE_TRIES, WRITE_GAP_MS)?;
    // SAFETY: clipboard open.
    let h = unsafe { GetClipboardData(CF_UNICODETEXT) }.ok()?;
    let r = match read_hglobal(h, MAX_FORMAT_BYTES, Some(2)) {
        Read::Bytes(b) => Some(crate::preview::decode_unicode(&b)),
        _ => None,
    };
    drop(guard);
    r
}

pub fn has_format(cf: u32) -> bool {
    // SAFETY: no preconditions.
    unsafe { IsClipboardFormatAvailable(cf).is_ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(list: &[(u32, &str)]) -> Vec<(u32, String)> {
        list.iter().map(|(i, n)| (*i, n.to_string())).collect()
    }

    fn ids_of(plan: &[Group]) -> Vec<Vec<u32>> {
        plan.iter().map(|g| g.iter().map(|(i, _)| *i).collect()).collect()
    }

    #[test]
    fn families_fall_back_in_priority_order() {
        let ids = [CF_TEXT, CF_UNICODETEXT, CF_BITMAP, CF_DIB, CF_DIBV5, CF_HDROP];
        let plan = plan_formats(&ids, &[]);
        assert_eq!(ids_of(&plan), vec![vec![CF_HDROP], vec![CF_UNICODETEXT, CF_TEXT], vec![CF_DIBV5, CF_DIB, CF_BITMAP]]);
    }

    #[test]
    fn rich_formats_follow_and_markers_are_never_data() {
        let n = named(&[
            (0xC010, "DataObject"),
            (0xC011, "Ole Private Data"),
            (0xC012, "HTML Format"),
            (0xC013, "Rich Text Format"),
            (0xC014, "Chromium Web Custom MIME Data Format"),
            (0xC015, "CanIncludeInClipboardHistory"),
            (0xC016, "Shell IDList Array"),
            (0xC017, "Chromium internal source URL"),
            (0xC018, "Chromium internal source RFH token"),
        ]);
        let ids: Vec<u32> = std::iter::once(CF_UNICODETEXT).chain(n.iter().map(|(i, _)| *i)).collect();
        let plan = plan_formats(&ids, &n);
        assert_eq!(ids_of(&plan), vec![vec![CF_UNICODETEXT], vec![0xC012], vec![0xC013], vec![0xC014]], "Chromium-internal bookkeeping is skipped");
    }

    #[test]
    fn plan_is_capped() {
        let n: Vec<(u32, String)> = (0..40u32).map(|i| (0xC100 + i, format!("Custom {i}"))).collect();
        let ids: Vec<u32> = n.iter().map(|(i, _)| *i).collect();
        assert_eq!(plan_formats(&ids, &n).len(), MAX_FORMATS);
    }

    fn split(ids: &[u32], n: &[(u32, &str)]) -> (Vec<Vec<u32>>, Vec<Vec<u32>>) {
        let n = named(n);
        let (a, b) = split_plan(plan_formats(ids, &n));
        (ids_of(&a), ids_of(&b))
    }

    #[test]
    fn text_is_read_at_once_and_the_rest_later() {
        let n = [(0xC012, "HTML Format"), (0xC013, "Rich Text Format"), (0xC014, "Biff12")];
        let (first, rest) = split(&[CF_UNICODETEXT, CF_TEXT, CF_DIBV5, 0xC012, 0xC013, 0xC014], &n);
        assert_eq!(first, vec![vec![CF_UNICODETEXT, CF_TEXT]]);
        assert_eq!(rest, vec![vec![CF_DIBV5], vec![0xC012], vec![0xC013], vec![0xC014]], "the image next to text is secondary");
    }

    #[test]
    fn a_screenshot_is_read_at_once() {
        let (first, rest) = split(&[CF_DIBV5, CF_DIB], &[]);
        assert_eq!(first, vec![vec![CF_DIBV5, CF_DIB]]);
        assert!(rest.is_empty());
    }

    #[test]
    fn files_and_text_are_both_essential() {
        let (first, rest) = split(&[CF_HDROP, CF_UNICODETEXT, CF_DIB, 0xC012], &[(0xC012, "HTML Format")]);
        assert_eq!(first, vec![vec![CF_HDROP], vec![CF_UNICODETEXT]]);
        assert_eq!(rest, vec![vec![CF_DIB], vec![0xC012]]);
    }

    #[test]
    fn a_source_without_text_files_or_image_defers_nothing() {
        let n = [(0xC012, "HTML Format"), (0xC013, "Rich Text Format")];
        let (first, rest) = split(&[0xC012, 0xC013], &n);
        assert_eq!(first, vec![vec![0xC012], vec![0xC013]]);
        assert!(rest.is_empty());
    }

    #[test]
    fn empty_clipboard_plans_nothing() {
        assert!(plan_formats(&[], &[]).is_empty());
    }
}
