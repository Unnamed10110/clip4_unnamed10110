//! User commands: every paste mode, merge, transform, snippet and hotkey action.
//! The UI thread only prepares data and enqueues a `Job`; all sequencing is on the paste thread.

use super::app::{message_box, normalize, App};
use super::msg::UiMsg;
use super::overlay::Scope;
use super::paste::Job;
use super::snippets_store;
use super::util::*;
use super::{dialogs, uia};
use crate::model::*;
use crate::preview;
use crate::snippets_fmt::{self as sf, Now, Snippet};
use crate::transform::{self, CaseOp};
use std::time::Instant;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::Globalization::*;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    Paste,
    PastePlain,
    CleanUrl,
    Markdown,
    HtmlText,
    EditPaste,
    EditSave,
    Merge,
    Excel,
    Delete,
    TogglePin,
    CopyPlain,
    OpenUrl,
    SaveImage,
    Keystroke,
    Transform(CaseOp),
    ClearList,
}

type Formats = Vec<(FormatKey, Payload)>;

fn text_formats(text: &str) -> Formats {
    vec![(FormatKey::Standard(CF_UNICODETEXT), Payload::inline(preview::unicode_bytes(text)))]
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace("\r\n", "<br>").replace('\n', "<br>")
}

/// Text of an item for plain operations.
fn text_of(it: &Item) -> Option<String> {
    it.text().filter(|t| !t.is_empty())
}

/// Merged payloads for 2+ text items (spec 12.3): Unicode joined with CRLF, RTF paragraphs, HTML <br>.
fn merged_formats(items: &[Item]) -> Formats {
    let texts: Vec<String> = items.iter().map(|i| text_of(i).unwrap_or_default()).collect();
    let mut out = text_formats(&transform::merge_unicode(&texts));
    let inline = |it: &Item, name: &str| it.payload_named(name).and_then(|p| p.bytes().map(|b| b.to_vec()));
    if items.iter().any(|i| inline(i, FMT_RTF).is_some()) {
        let parts: Vec<Vec<u8>> = items.iter().zip(&texts).map(|(i, t)| inline(i, FMT_RTF).unwrap_or_else(|| transform::rtf_from_text(t))).collect();
        out.push((FormatKey::reg(FMT_RTF), Payload::inline(transform::merge_rtf(&parts))));
    }
    if items.iter().any(|i| inline(i, FMT_HTML).is_some()) {
        let parts: Vec<Vec<u8>> = items.iter().zip(&texts).map(|(i, t)| inline(i, FMT_HTML).unwrap_or_else(|| transform::make_html_format(&html_escape(t)))).collect();
        out.push((FormatKey::reg(FMT_HTML), Payload::inline(transform::merge_html(&parts))));
    }
    out
}

fn is_text_only(it: &Item) -> bool {
    it.kind == Kind::Text && it.payload_std(CF_HDROP).is_none()
}

/// Text whose formatting survives being merged into one payload: plain text, or an HTML fragment
/// that carries its own inline styles (what browsers write). RTF, Office-style HTML (its styles sit
/// in a `<style>` block that a merge would drop), images, files and any other format are not merged.
fn merges_cleanly(it: &Item) -> bool {
    is_text_only(it)
        && text_of(it).is_some()
        && it.formats.iter().all(|(k, p)| {
            k.is_std(CF_UNICODETEXT)
                || k.is_std(CF_TEXT)
                || k.is_std(CF_OEMTEXT)
                || k.is_std(CF_LOCALE)
                || (k.is_named(FMT_HTML) && p.bytes().is_some_and(|b| !has_style_block(b)))
        })
}

fn has_style_block(html: &[u8]) -> bool {
    html.windows(6).any(|w| w.eq_ignore_ascii_case(b"<style"))
}

/// `formats` with a line break inside the same payloads (CRLF in the text, `<br>` in the HTML,
/// `\par` in the RTF), so a multi-paste needs no separator paste of its own: that second clipboard
/// swap + Ctrl+V is what some targets dropped, gluing items together. Text that already ends a line
/// is kept as it is. None when a payload cannot be extended (on disk, malformed) or there is no text.
fn with_line_break(formats: &Formats) -> Option<Formats> {
    let mut out = Formats::new();
    let mut has_text = false;
    for (k, p) in formats {
        let payload = if k.is_std(CF_UNICODETEXT) {
            has_text = true;
            let t = preview::decode_unicode(p.bytes()?);
            if t.ends_with('\n') {
                return Some(formats.clone());
            }
            Payload::inline(preview::unicode_bytes(&(t + "\r\n")))
        } else if k.is_std(CF_TEXT) {
            has_text = true;
            let b = p.bytes()?;
            if preview::decode_ansi(b).ends_with('\n') {
                return Some(formats.clone());
            }
            let mut v = b.split(|&c| c == 0).next().unwrap_or_default().to_vec();
            v.extend_from_slice(b"\r\n\0");
            Payload::inline(v)
        } else if k.is_named(FMT_HTML) {
            Payload::inline(transform::html_append_br(p.bytes()?)?)
        } else if k.is_named(FMT_RTF) {
            Payload::inline(transform::rtf_append_par(p.bytes()?)?)
        } else {
            p.clone()
        };
        out.push((k.clone(), payload));
    }
    has_text.then_some(out)
}

/// The clipboard states a multi-paste goes through: every item with ALL its own formats, a line
/// break after each but the last (inside the item's own payloads when possible, else a state of
/// its own).
fn sequence_steps(items: &[Item]) -> Vec<Formats> {
    let mut steps = Vec::new();
    for (i, it) in items.iter().enumerate() {
        if i + 1 == items.len() {
            steps.push(it.formats.clone());
        } else if let Some(f) = is_text_only(it).then(|| with_line_break(&it.formats)).flatten() {
            steps.push(f);
        } else {
            steps.push(it.formats.clone());
            steps.push(text_formats("\r\n"));
        }
    }
    steps
}

impl App {
    /// Where a paste should land: the window the user was in before the overlay opened.
    fn paste_target(&self) -> HWND {
        if self.overlay.is_visible() {
            self.overlay.target()
        } else {
            // SAFETY: plain query.
            unsafe { GetForegroundWindow() }
        }
    }

    fn picked_items(&self) -> Vec<Item> {
        if self.overlay.is_visible() {
            self.overlay.selected_items(self)
        } else {
            self.store.borrow().items().first().cloned().into_iter().collect()
        }
    }

    fn close_overlay(&self) {
        if self.overlay.is_visible() {
            self.overlay.hide(self);
        }
    }

    fn set_echo(&self, text: Option<String>) {
        *self.echo.borrow_mut() = text.map(|t| (normalize(&t), Instant::now()));
    }

    /// Hide the overlay and paste `formats` into the previous window.
    fn submit_paste(&self, formats: Formats, echo_text: Option<String>, restore: bool) {
        let target = self.paste_target();
        self.close_overlay();
        self.set_echo(echo_text);
        if !self.paster.submit(Job::Clipboard { formats, target: SendHwnd::new(target), restore }) {
            self.notice("clip4", "A paste is already in progress.");
        }
    }

    fn submit_plain(&self, text: String) {
        self.submit_paste(text_formats(&text), Some(text), false);
    }

    fn set_clipboard_only(&self, formats: Formats, echo: Option<String>) {
        self.set_echo(echo);
        self.paster.submit(Job::SetOnly { formats });
    }

    pub fn run_cmd(&self, c: Cmd) {
        // Snippets scope: Enter / click pastes the snippet.
        if self.overlay.scope() == Scope::Snippets && self.overlay.is_visible() {
            match c {
                Cmd::Paste | Cmd::PastePlain => {
                    if let Some((i, _)) = self.overlay.selected_snippet(self) {
                        let target = self.paste_target();
                        self.close_overlay();
                        self.paste_snippet_by_index(i, Some(target));
                    }
                }
                Cmd::Delete => self.snippet_delete_selected(),
                _ => {}
            }
            return;
        }
        let items = self.picked_items();
        if items.is_empty() && !matches!(c, Cmd::ClearList) {
            return;
        }
        match c {
            Cmd::Paste => {
                if items.len() == 1 {
                    let it = &items[0];
                    self.submit_paste(it.formats.clone(), it.text(), false);
                } else {
                    self.multi_paste(&items);
                }
            }
            Cmd::PastePlain => {
                if items.len() >= 2 {
                    let texts: Vec<String> = items.iter().map(|i| text_of(i).unwrap_or_default()).collect();
                    let merged = transform::merge_unicode(&texts);
                    let mut it = Item::new(0, now_unix_ms(), false, text_formats(&merged));
                    it.id = 0;
                    self.add_item(it, false);
                    self.submit_plain(merged);
                } else if let Some(t) = text_of(&items[0]) {
                    self.submit_plain(t);
                }
            }
            Cmd::CleanUrl => {
                if let Some(u) = text_of(&items[0]).and_then(|t| transform::clean_url(&t)) {
                    self.submit_plain(u);
                }
            }
            Cmd::Markdown => {
                if let Some(t) = text_of(&items[0]) {
                    let title = items[0]
                        .payload_named(FMT_HTML)
                        .and_then(|p| p.bytes())
                        .and_then(preview::html_fragment)
                        .and_then(|h| transform::html_title(&h));
                    self.submit_plain(transform::markdown_link(t.trim(), title.as_deref()));
                }
            }
            Cmd::HtmlText => {
                if let Some(h) = items[0].payload_named(FMT_HTML).and_then(|p| p.bytes()).and_then(preview::html_fragment) {
                    let t = preview::html_to_text(&h);
                    if !t.is_empty() {
                        self.submit_plain(t);
                    }
                }
            }
            Cmd::EditPaste | Cmd::EditSave => {
                let texts: Vec<String> = items.iter().filter_map(text_of).collect();
                if texts.is_empty() {
                    return;
                }
                let initial = transform::edit_text_join(&texts);
                let target = self.paste_target();
                self.close_overlay();
                let title = if c == Cmd::EditPaste { "Edit, then paste" } else { "Edit, then save as new item" };
                let Some(result) = dialogs::edit_text(self, title, &initial) else { return };
                if c == Cmd::EditPaste {
                    self.set_echo(Some(result.clone()));
                    let _ = self.paster.submit(Job::Clipboard { formats: text_formats(&result), target: SendHwnd::new(target), restore: false });
                } else {
                    let f = text_formats(&result);
                    self.add_item(Item::new(0, now_unix_ms(), false, f.clone()), false);
                    self.set_clipboard_only(f, None);
                }
            }
            Cmd::Merge => {
                if items.len() >= 2 {
                    let f = merged_formats(&items);
                    if let Some(id) = self.add_item(Item::new(0, now_unix_ms(), false, f), false) {
                        self.overlay.select_item(self, id);
                    }
                }
            }
            Cmd::Excel => {
                let texts: Vec<String> = items.iter().filter_map(text_of).collect();
                if texts.is_empty() {
                    return;
                }
                let target = self.paste_target();
                self.close_overlay();
                let _ = self.paster.submit(Job::Excel { texts, target: SendHwnd::new(target) });
            }
            Cmd::Delete => {
                let ids: Vec<u64> = items.iter().map(|i| i.id).collect();
                self.store.borrow_mut().remove(&ids);
                self.schedule_save();
                self.overlay.refresh(self);
            }
            Cmd::TogglePin => {
                let pin = !items[0].pinned;
                {
                    let mut s = self.store.borrow_mut();
                    for it in &items {
                        s.set_pinned(it.id, pin);
                    }
                }
                self.schedule_save();
                self.overlay.refresh(self);
            }
            Cmd::CopyPlain => {
                if let Some(t) = text_of(&items[0]) {
                    self.close_overlay();
                    self.set_clipboard_only(text_formats(&t), Some(t));
                }
            }
            Cmd::OpenUrl => {
                if let Some(t) = text_of(&items[0]).map(|t| t.trim().to_string()) {
                    let lower = t.to_ascii_lowercase();
                    if (lower.starts_with("http://") || lower.starts_with("https://")) && !t.contains(|c: char| c.is_whitespace() || c == '"') {
                        self.close_overlay();
                        // Through explorer.exe, so the browser starts in the user's normal context
                        // instead of inheriting clip4's elevation.
                        let w = wide(&format!("\"{t}\""));
                        // SAFETY: shell open of an http(s) URL only.
                        unsafe {
                            ShellExecuteW(None, windows::core::w!("open"), windows::core::w!("explorer.exe"), pcw(&w), PCWSTR::null(), SW_SHOWNORMAL);
                        }
                    }
                }
            }
            Cmd::SaveImage => self.save_image(&items[0]),
            Cmd::Keystroke => {
                if let Some(t) = text_of(&items[0]) {
                    let target = self.paste_target();
                    self.close_overlay();
                    let _ = self.paster.submit(Job::Keystrokes { text: t, target: Some(SendHwnd::new(target)) });
                }
            }
            Cmd::Transform(op) => {
                let id = items[0].id;
                let Some(t) = text_of(&items[0]) else { return };
                let new_text = transform::apply_case(op, &t);
                self.store.borrow_mut().replace_item(id, |old| {
                    let mut f: Formats = old
                        .formats
                        .iter()
                        .filter(|(k, _)| !(k.is_std(CF_UNICODETEXT) || k.is_std(CF_TEXT) || k.is_named(FMT_HTML) || k.is_named(FMT_RTF)))
                        .cloned()
                        .collect();
                    f.insert(0, text_formats(&new_text).remove(0));
                    Item::new(old.id, old.unix_ms, old.pinned, f)
                });
                self.schedule_save();
                self.overlay.refresh(self);
                self.set_clipboard_only(text_formats(&new_text), Some(new_text));
            }
            Cmd::ClearList => {
                self.close_overlay();
                let r = message_box(self.hwnd.get(), "Delete all unpinned items from the list?", "clip4", MB_YESNO | MB_ICONQUESTION);
                if r == IDYES {
                    self.store.borrow_mut().clear_unpinned();
                    self.schedule_save();
                    self.overlay.refresh(self);
                }
            }
        }
    }

    fn multi_paste(&self, items: &[Item]) {
        if items.iter().all(merges_cleanly) {
            // Plain text and self-contained browser HTML: one payload, ONE paste (fast, and the line
            // breaks cannot get lost). The merge is kept in history.
            let f = merged_formats(items);
            let text = f.first().and_then(|(_, p)| p.bytes()).map(preview::decode_unicode);
            self.add_item(Item::new(0, now_unix_ms(), false, f.clone()), false);
            self.submit_paste(f, text, false);
        } else {
            // Office/RTF text, images, files: a merged RTF/HTML payload would lose formatting (the
            // first item's font and colour tables win; Office HTML loses its <style> block), so each
            // item is pasted with ALL its own formats, one clipboard swap per item, and the user's
            // clipboard is restored afterwards.
            if items.iter().all(is_text_only) {
                // The combined text is still kept in history as one item.
                self.add_item(Item::new(0, now_unix_ms(), false, merged_formats(items)), false);
            }
            let target = self.paste_target();
            self.close_overlay();
            let _ = self.paster.submit(Job::Sequence { steps: sequence_steps(items), target: SendHwnd::new(target) });
        }
    }

    fn save_image(&self, it: &Item) {
        use windows::Win32::UI::Controls::Dialogs::*;
        let Some((k, p)) = it.formats.iter().find(|(k, _)| k.is_std(CF_DIBV5) || k.is_std(CF_DIB) || k.is_named(FMT_PNG)) else { return };
        let (k, p) = (k.clone(), p.clone());
        self.close_overlay();
        let mut file = [0u16; 520];
        let name = wide("clip4-image.png");
        file[..name.len()].copy_from_slice(&name);
        let filter: Vec<u16> = "PNG image\0*.png\0\0".encode_utf16().collect();
        let defext = wide("png");
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            hwndOwner: self.hwnd.get(),
            lpstrFilter: pcw(&filter),
            lpstrFile: windows::core::PWSTR(file.as_mut_ptr()),
            nMaxFile: file.len() as u32,
            lpstrDefExt: pcw(&defext),
            Flags: OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST,
            ..Default::default()
        };
        // SAFETY: modal common dialog on the UI thread; no borrows are held.
        if unsafe { GetSaveFileNameW(&mut ofn) }.as_bool() {
            self.workers.io(super::worker::IoTask::SaveImage { key: k, payload: p, path: from_wide(&file) });
        }
    }

    // ---------------- snippets ----------------

    pub fn paste_snippet_by_index(&self, idx: usize, target: Option<HWND>) {
        let Some(s) = self.snippets.borrow().get(idx).cloned() else { return };
        let target = target.unwrap_or_else(|| self.paste_target());
        self.set_echo(None);
        let job = Job::Snippet { snippet: s, now: now_for_snippets(), target: SendHwnd::new(target) };
        if !self.paster.submit(job) {
            self.notice("clip4", "A paste is already in progress.");
        }
    }

    pub fn snippet_add(&self) {
        self.close_overlay();
        if let Some(s) = dialogs::snippet_editor(self, None) {
            self.snippets.borrow_mut().push(s);
            self.snippets_changed();
        }
    }

    pub fn snippet_edit_selected(&self) {
        let Some((i, cur)) = self.overlay.selected_snippet(self) else { return };
        self.close_overlay();
        if let Some(s) = dialogs::snippet_editor(self, Some(&cur)) {
            if let Some(slot) = self.snippets.borrow_mut().get_mut(i) {
                *slot = s;
            }
            self.snippets_changed();
        }
    }

    pub fn snippet_delete_selected(&self) {
        let Some((i, cur)) = self.overlay.selected_snippet(self) else { return };
        let r = message_box(self.overlay.main_hwnd(), &format!("Delete snippet \"{}\"?", cur.name), "clip4", MB_YESNO | MB_ICONQUESTION);
        if r == IDYES {
            self.snippets.borrow_mut().remove(i);
            self.snippets_changed();
        }
    }

    pub fn snippets_changed(&self) {
        let list = self.snippets.borrow().clone();
        snippets_store::save(&list);
        self.overlay.refresh(self);
    }

    // ---------------- global-hotkey actions ----------------

    pub fn hotkey_keystroke_paste(&self) {
        let items = self.picked_items();
        let Some(t) = items.first().and_then(text_of) else { return };
        let visible = self.overlay.is_visible();
        let target = visible.then(|| SendHwnd::new(self.overlay.target()));
        self.close_overlay();
        let _ = self.paster.submit(Job::Keystrokes { text: t, target });
    }

    pub fn hotkey_swap_paste(&self) {
        let items = self.picked_items();
        let Some(it) = items.first() else { return };
        let f = it.formats.clone();
        let echo = it.text();
        let target = self.paste_target();
        self.close_overlay();
        self.set_echo(echo);
        let _ = self.paster.submit(Job::Clipboard { formats: f, target: SendHwnd::new(target), restore: true });
    }

    // ---------------- copy from focused control (spec 15) ----------------

    pub fn copy_from_focused(&self) {
        // UI Automation on a dedicated STA thread with a timeout: a hung provider must not freeze anything.
        let _ = std::thread::Builder::new().name("uia-supervisor".into()).spawn(|| {
            guarded("uia-supervisor", || {
                let (tx, rx) = std::sync::mpsc::channel();
                let _ = std::thread::Builder::new().name("uia".into()).spawn(move || {
                    guarded("uia", || {
                        let _ = tx.send(uia::read_focused_text());
                    });
                });
                let text = rx.recv_timeout(std::time::Duration::from_millis(1500)).ok().flatten();
                super::msg::post_ui(UiMsg::FocusedText { text, via_uia: true });
            });
        });
    }

    pub fn on_focused_text(&self, text: Option<String>, _via_uia: bool) {
        match text.filter(|t| !t.trim().is_empty()) {
            Some(t) => {
                let f = text_formats(&t);
                self.add_item(Item::new(0, now_unix_ms(), false, f.clone()), true);
                self.set_clipboard_only(f, None);
            }
            None => {
                // Synthetic copy fallback; the resulting clipboard change is captured normally.
                let _ = self.paster.submit(Job::SyntheticCopy);
            }
        }
    }
}

/// Current local time, with date/time strings formatted for the user's locale.
pub fn now_for_snippets() -> Now {
    // SAFETY: plain time + locale formatting calls into local buffers.
    unsafe {
        let st = GetLocalTime();
        let mut d = [0u16; 64];
        let mut t = [0u16; 64];
        let nd = GetDateFormatEx(PCWSTR::null(), DATE_SHORTDATE, Some(&st), PCWSTR::null(), Some(&mut d), PCWSTR::null());
        let nt = GetTimeFormatEx(PCWSTR::null(), TIME_FORMAT_FLAGS(0), Some(&st), PCWSTR::null(), Some(&mut t));
        Now {
            year: st.wYear as u32,
            month: st.wMonth as u32,
            day: st.wDay as u32,
            hour: st.wHour as u32,
            minute: st.wMinute as u32,
            second: st.wSecond as u32,
            date_str: if nd > 0 { from_wide(&d) } else { format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay) },
            time_str: if nt > 0 { from_wide(&t) } else { format!("{:02}:{:02}", st.wHour, st.wMinute) },
        }
    }
}

pub fn snippet_needs_clipboard(s: &Snippet) -> bool {
    s.content.contains("{{clipboard}}") || s.content_plain.as_deref().is_some_and(|c| c.contains("{{clipboard}}"))
}

/// Clipboard formats for a snippet paste: RTF + Unicode text for rich snippets (never raw RTF
/// as Unicode), Unicode text otherwise. Placeholders are expanded here.
pub fn snippet_formats(s: &Snippet, now: &Now, clip: &str) -> Vec<(FormatKey, Payload)> {
    if s.is_rtf() {
        let rtf = sf::expand_rtf(&s.content, now, clip);
        let plain = match &s.content_plain {
            Some(p) => sf::expand(p, now, clip),
            None => sf::rtf_plain_text(&rtf),
        };
        let mut f = text_formats(&plain);
        f.push((FormatKey::reg(FMT_RTF), Payload::inline(rtf.into_bytes())));
        f
    } else {
        text_formats(&sf::expand(&s.content, now, clip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(formats: Vec<(FormatKey, Payload)>) -> Item {
        Item::new(1, 0, false, formats)
    }

    fn text(s: &str) -> (FormatKey, Payload) {
        (FormatKey::Standard(CF_UNICODETEXT), Payload::inline(preview::unicode_bytes(s)))
    }

    fn html(fragment: &str) -> (FormatKey, Payload) {
        (FormatKey::reg(FMT_HTML), Payload::inline(transform::make_html_format(fragment)))
    }

    fn rtf(s: &str) -> (FormatKey, Payload) {
        (FormatKey::reg(FMT_RTF), Payload::inline(s.as_bytes().to_vec()))
    }

    fn unicode_of(f: &Formats) -> String {
        preview::decode_unicode(f.iter().find(|(k, _)| k.is_std(CF_UNICODETEXT)).and_then(|(_, p)| p.bytes()).unwrap())
    }

    #[test]
    fn plain_text_and_self_contained_html_merge() {
        assert!(merges_cleanly(&item(vec![text("hello")])));
        assert!(merges_cleanly(&item(vec![text("hello"), html("<span style=\"color: red\">hello</span>")])), "browser HTML keeps its inline styles");
        let office = (FormatKey::reg(FMT_HTML), Payload::inline(b"Version:0.9\r\n<html><head><STYLE>.xl65{color:red}</STYLE></head>".to_vec()));
        assert!(!merges_cleanly(&item(vec![text("hello"), office])), "a merge would drop the <style> block");
        assert!(!merges_cleanly(&item(vec![text("hello"), rtf("{\\rtf1 hello}")])), "a merged RTF keeps only the first item's tables");
        let on_disk = item(vec![(FormatKey::Standard(CF_UNICODETEXT), Payload::OnDisk { sha1: [0; 20], len: 300_000 })]);
        assert!(!merges_cleanly(&on_disk), "big text lives on disk: only the paste thread can read it");
    }

    #[test]
    fn a_line_break_goes_into_every_payload() {
        let f = vec![text("hello"), html("<b>hello</b>"), rtf("{\\rtf1 hello}")];
        let g = with_line_break(&f).unwrap();
        assert_eq!(unicode_of(&g), "hello\r\n");
        let h = g.iter().find(|(k, _)| k.is_named(FMT_HTML)).and_then(|(_, p)| p.bytes()).unwrap();
        assert_eq!(preview::html_fragment(h).unwrap(), "<b>hello</b><br>");
        let r = g.iter().find(|(k, _)| k.is_named(FMT_RTF)).and_then(|(_, p)| p.bytes()).unwrap();
        assert_eq!(r, b"{\\rtf1 hello\\par }");
        // Already ends a line (an Excel cell): unchanged.
        let cell = vec![text("hello\r\n"), html("<b>hello</b>")];
        assert_eq!(unicode_of(&with_line_break(&cell).unwrap()), "hello\r\n");
        // A payload that cannot be extended: the caller pastes a separator of its own.
        let big = vec![text("hello"), (FormatKey::reg(FMT_HTML), Payload::OnDisk { sha1: [0; 20], len: 300_000 })];
        assert!(with_line_break(&big).is_none());
        assert!(with_line_break(&vec![html("x")]).is_none(), "no text, no break");
    }

    #[test]
    fn a_multi_paste_has_a_line_break_after_every_item_but_the_last() {
        let a = item(vec![text("one"), rtf("{\\rtf1 one}")]);
        let b = item(vec![text("two"), rtf("{\\rtf1 two}")]);
        let c = item(vec![text("three"), rtf("{\\rtf1 three}")]);
        let steps = sequence_steps(&[a, b, c]);
        assert_eq!(steps.len(), 3, "the break travels inside the items: no separator states");
        assert_eq!([unicode_of(&steps[0]), unicode_of(&steps[1]), unicode_of(&steps[2])], ["one\r\n", "two\r\n", "three"]);
        // An image has no text to extend: it gets a separator state of its own.
        let img = item(vec![(FormatKey::Standard(CF_DIB), Payload::inline(vec![0u8; 64]))]);
        let steps = sequence_steps(&[img, item(vec![text("after")])]);
        assert_eq!(steps.len(), 3);
        assert_eq!(unicode_of(&steps[1]), "\r\n");
        assert_eq!(unicode_of(&steps[2]), "after");
    }
}
