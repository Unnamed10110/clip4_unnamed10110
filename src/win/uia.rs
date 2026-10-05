//! UI Automation text extraction for "Copy from focused control" (spec 15).
//!
//! Order: `TextPattern` (selection, else document range) -> `ValuePattern` ->
//! `LegacyIAccessiblePattern`; first non-empty wins. Password fields yield `None`.
//! Never logs text content (spec 19.3), only failure codes.

use windows::core::BSTR;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomationElement,
    IUIAutomationLegacyIAccessiblePattern, IUIAutomationTextPattern, IUIAutomationValuePattern,
    UIA_LegacyIAccessiblePatternId, UIA_TextPatternId, UIA_ValuePatternId,
};

/// Text cap in UTF-16 units (passed to `GetText`), ~5 MB.
const MAX_CHARS: i32 = 5 * 1024 * 1024;
/// Upper bound on selection ranges we walk (a hostile provider may report millions).
const MAX_RANGES: i32 = 10_000;

/// Runs on a dedicated STA thread created by the caller (NOT the UI thread). TextPattern
/// selection, else document range; ValuePattern; LegacyIAccessible value. `None` when
/// nothing was found or the focused element is a password field.
pub fn read_focused_text() -> Option<String> {
    let _com = ComGuard::new()?;
    // Every COM object lives inside `read_inner`, so all are released before `_com` drops.
    read_inner()
}

fn read_inner() -> Option<String> {
    let auto = create_automation()?;
    // SAFETY: plain COM call on a live interface; the result is an owned smart pointer.
    let el = ok("GetFocusedElement", unsafe { auto.GetFocusedElement() })?;
    // Fail closed: if we cannot tell whether it is a password field, do not read it.
    // SAFETY: as above.
    let is_pw = ok("IsPassword", unsafe { el.CurrentIsPassword() })?;
    if is_pw.as_bool() {
        crate::log_dbg!("uia: focused element is a password field; skipped");
        return None;
    }
    from_text_pattern(&el)
        .or_else(|| from_value_pattern(&el))
        .or_else(|| from_legacy_pattern(&el))
}

fn create_automation() -> Option<IUIAutomation> {
    // SAFETY: standard in-proc CoCreateInstance; COM is initialised on this thread by ComGuard.
    // CUIAutomation8 (Win8+) first, plain CUIAutomation as fallback.
    unsafe {
        CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
            .or_else(|_| CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER))
    }
    .map_err(|e| {
        crate::log_warn!(
            "uia: CoCreateInstance(CUIAutomation) failed: {:#x}",
            e.code().0
        )
    })
    .ok()
}

fn from_text_pattern(el: &IUIAutomationElement) -> Option<String> {
    // SAFETY: COM calls on live interfaces; every result is an owned smart pointer / BSTR.
    let tp: IUIAutomationTextPattern = ok("TextPattern", unsafe {
        el.GetCurrentPatternAs(UIA_TextPatternId)
    })?;

    if let Some(sel) = ok("GetSelection", unsafe { tp.GetSelection() }) {
        let n = ok("Selection.Length", unsafe { sel.Length() })
            .unwrap_or(0)
            .clamp(0, MAX_RANGES);
        let mut parts: Vec<String> = Vec::new();
        let mut used: i32 = 0;
        for i in 0..n {
            let remaining = MAX_CHARS.saturating_sub(used);
            if remaining <= 0 {
                break;
            }
            let text = ok("Selection.GetElement", unsafe { sel.GetElement(i) })
                .and_then(|r| ok("Range.GetText", unsafe { r.GetText(remaining) }));
            if let Some(t) = text.and_then(bstr_clean) {
                // Bytes >= UTF-16 units: conservative, so the total can only undershoot the cap.
                used = used.saturating_add(i32::try_from(t.len()).unwrap_or(i32::MAX));
                parts.push(t);
            }
        }
        if let Some(s) = join_ranges(&parts) {
            return Some(s);
        }
    }

    let doc = ok("DocumentRange", unsafe { tp.DocumentRange() })?;
    ok("DocumentRange.GetText", unsafe { doc.GetText(MAX_CHARS) }).and_then(bstr_clean)
}

fn from_value_pattern(el: &IUIAutomationElement) -> Option<String> {
    // SAFETY: COM calls on live interfaces.
    let vp: IUIAutomationValuePattern = ok("ValuePattern", unsafe {
        el.GetCurrentPatternAs(UIA_ValuePatternId)
    })?;
    ok("Value.CurrentValue", unsafe { vp.CurrentValue() }).and_then(bstr_clean)
}

fn from_legacy_pattern(el: &IUIAutomationElement) -> Option<String> {
    // SAFETY: COM calls on live interfaces.
    let lp: IUIAutomationLegacyIAccessiblePattern = ok("LegacyIAccessiblePattern", unsafe {
        el.GetCurrentPatternAs(UIA_LegacyIAccessiblePatternId)
    })?;
    ok("Legacy.CurrentValue", unsafe { lp.CurrentValue() }).and_then(bstr_clean)
}

// ---- helpers ----

/// Logs a failed COM call (HRESULT only, never content) at debug level and turns it into `None`.
/// Unsupported patterns are the common, expected failure, hence debug rather than warn.
fn ok<T>(what: &str, r: windows::core::Result<T>) -> Option<T> {
    r.map_err(|e| crate::log_dbg!("uia: {what} failed: {:#x}", e.code().0))
        .ok()
}

fn bstr_clean(b: BSTR) -> Option<String> {
    clean(b.to_string())
}

/// Providers sometimes pad with trailing NULs; empty means "nothing found".
fn clean(mut s: String) -> Option<String> {
    s.truncate(s.trim_end_matches('\0').len());
    (!s.is_empty()).then_some(s)
}

/// Joins non-empty selection ranges with CRLF; `None` when there is nothing.
fn join_ranges(parts: &[String]) -> Option<String> {
    let v: Vec<&str> = parts
        .iter()
        .map(String::as_str)
        .filter(|p| !p.is_empty())
        .collect();
    (!v.is_empty()).then(|| v.join("\r\n"))
}

/// Balanced `CoInitializeEx(STA)` / `CoUninitialize` (spec 19.2).
struct ComGuard(bool);

impl ComGuard {
    fn new() -> Option<Self> {
        // SAFETY: plain FFI call without pointers.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_ok() {
            return Some(Self(true)); // S_OK and S_FALSE both require a matching CoUninitialize
        }
        if hr == RPC_E_CHANGED_MODE {
            return Some(Self(false)); // thread already MTA: COM is usable but not ours to undo
        }
        crate::log_warn!("uia: CoInitializeEx failed: {:#x}", hr.0);
        None
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx in `new` on this same thread.
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_trailing_nuls_and_empties() {
        assert_eq!(clean("abc\0\0".into()).as_deref(), Some("abc"));
        assert_eq!(clean("a\0b".into()).as_deref(), Some("a\0b"));
        assert_eq!(clean(String::new()), None);
        assert_eq!(clean("\0\0".into()), None);
        assert_eq!(clean(" ".into()).as_deref(), Some(" "));
    }

    #[test]
    fn join_ranges_uses_crlf_and_skips_empty() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            join_ranges(&p(&["a", "", "b", "c"])).as_deref(),
            Some("a\r\nb\r\nc")
        );
        assert_eq!(join_ranges(&p(&["only"])).as_deref(), Some("only"));
        assert_eq!(join_ranges(&p(&["", ""])), None);
        assert_eq!(join_ranges(&[]), None);
    }

    /// Needs an interactive desktop with a focused text control; run with `--ignored`
    /// (it waits 3 s so you can focus Notepad that contains text).
    #[test]
    #[ignore]
    fn reads_focused_text_from_desktop() {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let t = std::thread::spawn(read_focused_text).join().ok().flatten();
        assert!(t.is_some_and(|s| !s.is_empty()));
    }
}
