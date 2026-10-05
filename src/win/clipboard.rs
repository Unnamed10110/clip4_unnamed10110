//! Clipboard access: RAII lock, snapshot capture, atomic write, backup/restore.
//!
//! The one rule (spec 6.2 / lesson 18.1): the clipboard is held open ONLY for raw byte
//! copies. Everything else — hashing, parsing, indexing, disk — happens after the
//! [`ClipboardGuard`] is dropped, on another thread.

use super::util::{from_wide, pcw, wide};
use crate::model::*;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

// ---- limits (spec 6.4) ----
pub const MAX_FORMATS: usize = 12;
pub const MAX_FORMAT_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_PRIMARY_IMAGE_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_ITEM_BYTES: usize = 10 * 1024 * 1024;

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

/// Opens the clipboard, retrying `attempts` times `delay_ms` apart. No sleep after the
/// final attempt (lesson 18.12).
pub fn open(owner: Option<HWND>, attempts: u32, delay_ms: u64) -> Option<ClipboardGuard> {
    for i in 0..attempts {
        // SAFETY: OpenClipboard has no memory-safety preconditions.
        if unsafe { OpenClipboard(owner) }.is_ok() {
            return Some(ClipboardGuard(()));
        }
        if i + 1 < attempts {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
    }
    None
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

fn owner_is_us() -> bool {
    let me = OWN_OWNER.load(std::sync::atomic::Ordering::SeqCst);
    // SAFETY: plain query.
    me != 0 && unsafe { GetClipboardOwner() }.is_ok_and(|h| h.0 as isize == me)
}

pub fn mark_own(seq: u32) {
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

/// Takes a raw-bytes snapshot. Holds the clipboard only for the copy.
pub fn capture(owner: Option<HWND>) -> Captured {
    let Some(guard) = open(owner, 8, 5) else {
        crate::log_warn!("OpenClipboard failed after 8 attempts (capture); owner = {}", open_owner_name());
        return Captured::Busy;
    };
    let t0 = Instant::now();
    let seq = sequence();
    if owner_is_us() {
        return Captured::Own(seq);
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

    // Privacy opt-outs first (spec 20.1): no content is read at all.
    let mut named: Vec<(u32, String)> = Vec::new();
    for &id in ids.iter().filter(|&&i| i >= 0xC000) {
        if let Some(n) = format_name(id) {
            if is_exclusion_name(&n) {
                return Captured::Excluded(seq);
            }
            named.push((id, n));
        }
    }
    if let Some((id, _)) = named.iter().find(|(_, n)| n.eq_ignore_ascii_case("CanIncludeInClipboardHistory")) {
        // SAFETY: clipboard is open.
        if let Ok(h) = unsafe { GetClipboardData(*id) } {
            if let Read::Bytes(b) = read_hglobal(h, 16, None) {
                if b.len() >= 4 && u32::from_le_bytes([b[0], b[1], b[2], b[3]]) == 0 {
                    return Captured::Excluded(seq);
                }
            }
        }
    }

    // Plan: (id, key) in capture order, at most MAX_FORMATS.
    let has = |cf: u32| ids.contains(&cf);
    let name_id = |n: &str| named.iter().find(|(_, x)| x.eq_ignore_ascii_case(n)).map(|(i, _)| *i);
    let mut plan: Vec<(u32, FormatKey)> = Vec::new();
    if has(CF_HDROP) {
        plan.push((CF_HDROP, FormatKey::Standard(CF_HDROP)));
    }
    if has(CF_UNICODETEXT) {
        plan.push((CF_UNICODETEXT, FormatKey::Standard(CF_UNICODETEXT)));
    } else if has(CF_TEXT) {
        plan.push((CF_TEXT, FormatKey::Standard(CF_TEXT)));
    }
    // One image format only; Windows synthesises the others when we paste (6.4).
    for cf in [CF_DIBV5, CF_DIB, CF_BITMAP] {
        if has(cf) {
            plan.push((cf, FormatKey::Standard(cf)));
            break;
        }
    }
    for n in [FMT_PNG, FMT_HTML, FMT_RTF, FMT_DROPEFFECT] {
        if let Some(id) = name_id(n) {
            plan.push((id, FormatKey::reg(n)));
        }
    }
    for (id, n) in &named {
        let known = [FMT_PNG, FMT_HTML, FMT_RTF, FMT_DROPEFFECT].iter().any(|k| k.eq_ignore_ascii_case(n));
        let skip = SKIP_NAMES.iter().any(|k| k.eq_ignore_ascii_case(n))
            || n.eq_ignore_ascii_case("CanIncludeInClipboardHistory")
            || n.eq_ignore_ascii_case("CanUploadToCloudClipboard");
        if !known && !skip {
            plan.push((*id, FormatKey::Registered(n.clone())));
        }
    }
    plan.truncate(MAX_FORMATS);

    let mut formats: Vec<(FormatKey, Vec<u8>)> = Vec::new();
    let mut total = 0usize;
    for (i, (id, key)) in plan.iter().enumerate() {
        let is_primary_image = key.is_std(CF_DIBV5) || key.is_std(CF_DIB) || key.is_std(CF_BITMAP);
        let image_is_primary = is_primary_image && !has(CF_UNICODETEXT) && !has(CF_TEXT) && !has(CF_HDROP);
        let cap = if image_is_primary { MAX_PRIMARY_IMAGE_BYTES } else { MAX_FORMAT_BYTES };
        if !image_is_primary && total + 1 > MAX_ITEM_BYTES {
            continue;
        }
        // SAFETY: clipboard is open.
        let Ok(h) = (unsafe { GetClipboardData(*id) }) else { continue };
        let r = if key.is_std(CF_BITMAP) {
            bitmap_to_dib(h, cap)
        } else if key.is_std(CF_UNICODETEXT) {
            read_hglobal(h, cap, Some(2))
        } else if key.is_std(CF_TEXT) {
            read_hglobal(h, cap, Some(1))
        } else {
            read_hglobal(h, cap, None)
        };
        match r {
            Read::Bytes(b) => {
                if !image_is_primary && total + b.len() > MAX_ITEM_BYTES {
                    crate::log_dbg!("format {} skipped: item byte cap", key.label());
                    continue;
                }
                if !image_is_primary {
                    total += b.len();
                }
                // CF_BITMAP is stored as CF_DIB.
                let k = if key.is_std(CF_BITMAP) { FormatKey::Standard(CF_DIB) } else { key.clone() };
                formats.push((k, b));
            }
            Read::TooBig(n) => crate::log_info!("format {} skipped: {} bytes over limit (#{i})", key.label(), n),
            Read::Fail => crate::log_dbg!("format {} unreadable", key.label()),
        }
    }
    drop(guard);
    let lock_ms = t0.elapsed().as_millis() as u32;
    if lock_ms > 20 {
        crate::log_warn!("clipboard lock held {lock_ms} ms during capture");
    }
    if formats.is_empty() {
        return Captured::Empty(seq);
    }
    Captured::Ok(Snapshot { seq, formats, lock_ms })
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
pub fn write_prepared(owner: HWND, prepared: Vec<Prepared>, attempts: u32, delay_ms: u64) -> Result<u32, &'static str> {
    let Some(guard) = open(Some(owner), attempts, delay_ms) else {
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
    let guard = open(Some(owner), 24, 8)?;
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

/// Restores a backup: 30 x 8 ms, then once more after 60 ms. Never empties when the backup is empty.
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
    match write_prepared(owner, prepared, 30, 8) {
        Ok(_) => true,
        Err(_) => {
            std::thread::sleep(Duration::from_millis(60));
            let mut again = Vec::new();
            for (id, bytes) in &b.formats {
                if let Some(h) = alloc_global(bytes, 0) {
                    again.push(Prepared { id: *id, h: Some(h) });
                }
            }
            let r = write_prepared(owner, again, 30, 8).is_ok();
            if !r {
                crate::log_err!("CLIPBOARD RESTORE FAILED");
            }
            r
        }
    }
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
    let guard = open(Some(owner), 8, 5)?;
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
