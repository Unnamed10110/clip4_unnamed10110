//! `snippet_editor` (name + rich-text body with Bold/Italic/Underline) and `manage_snippets`.

use super::editor::to_crlf;
use super::modal::*;
use crate::snippets_fmt::{encoded_len, validate, Snippet, MAX_VALUE_CHARS};
use crate::win::app::App;
use crate::win::util::wide;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::LoadLibraryW;
use windows::Win32::UI::Controls::NMHDR;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------- RTF decisions (pure)

/// `\fonttbl` entries as (font number, face name).
fn font_faces(rtf: &str) -> Vec<(i32, String)> {
    let Some(start) = rtf.find("{\\fonttbl") else { return Vec::new() };
    let mut out = Vec::new();
    let (mut depth, mut num, mut name, mut named) = (0usize, None::<i32>, String::new(), false);
    let mut it = rtf.get(start..).unwrap_or("").chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' => {
                depth += 1;
                if depth == 2 {
                    (num, named) = (None, false);
                    name.clear();
                }
            }
            '}' => {
                if depth == 2 {
                    out.extend(num.map(|n| (n, name.trim().to_string())));
                }
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            '\\' => {
                let mut word = String::new();
                while let Some(p) = it.next_if(char::is_ascii_alphabetic) {
                    word.push(p);
                }
                if word.is_empty() {
                    it.next(); // escaped symbol
                    continue;
                }
                let mut digits = String::new();
                digits.extend(it.next_if_eq(&'-'));
                while let Some(p) = it.next_if(char::is_ascii_digit) {
                    digits.push(p);
                }
                it.next_if_eq(&' ');
                if depth == 2 && word == "f" {
                    num = digits.parse().ok();
                }
            }
            ';' if depth == 2 => named = true,
            c if depth == 2 && !named && c != '\r' && c != '\n' => name.push(c),
            _ => {}
        }
    }
    out
}

/// Does this RTF carry character formatting beyond the plain default? Looks at the document
/// body only (font/colour/style tables and `\*` destinations are skipped) for bold, italic,
/// underline, strike, super/subscript, a colour or highlight, a font FACE other than the
/// default font's (RichEdit lists the same face twice, so numbers alone would mislead), or a
/// font size that differs from the first one. Off switches (`\b0`, `\ulnone`) do not count.
pub fn rtf_has_formatting(rtf: &str) -> bool {
    const HEADER: [&str; 7] = ["\\fonttbl", "\\colortbl", "\\stylesheet", "\\info", "\\listtable", "\\listoverridetable", "\\rsidtbl"];
    let faces = font_faces(rtf);
    let face = |n: i32| faces.iter().find(|(k, _)| *k == n).map_or_else(|| format!("#{n}"), |(_, f)| f.to_lowercase());
    let b = rtf.as_bytes();
    let (mut i, mut depth, mut skip_at, mut first_fs, mut default_font) = (0usize, 0usize, None::<usize>, None::<i32>, 0);
    while let Some(&c) = b.get(i) {
        i += 1;
        match c {
            b'{' => {
                depth += 1;
                let rest = rtf.get(i..).unwrap_or("");
                if skip_at.is_none() && (rest.starts_with("\\*") || HEADER.iter().any(|h| rest.starts_with(h))) {
                    skip_at = Some(depth);
                }
            }
            b'}' => {
                if skip_at == Some(depth) {
                    skip_at = None;
                }
                depth = depth.saturating_sub(1);
            }
            b'\\' => {
                if !b.get(i).is_some_and(u8::is_ascii_alphabetic) {
                    i += 1; // escaped symbol (\\ \{ \} \' ...): not a control word
                    continue;
                }
                let start = i;
                while b.get(i).is_some_and(u8::is_ascii_alphabetic) {
                    i += 1;
                }
                let word = rtf.get(start..i).unwrap_or("");
                let neg = b.get(i) == Some(&b'-');
                let dstart = i + usize::from(neg);
                let mut dend = dstart;
                while b.get(dend).is_some_and(u8::is_ascii_digit) {
                    dend += 1;
                }
                let param = rtf.get(dstart..dend).and_then(|d| d.parse::<i32>().ok()).map(|v| if neg { -v } else { v });
                if param.is_some() {
                    i = dend;
                }
                if skip_at.is_some() {
                    continue;
                }
                let on = param != Some(0);
                let rich = match word {
                    "deff" => {
                        default_font = param.unwrap_or(0);
                        false
                    }
                    "b" | "i" | "strike" | "striked" | "super" | "sub" => on,
                    "cf" | "highlight" | "cb" => param.is_some_and(|n| n > 0),
                    "f" => param.is_some_and(|n| face(n) != face(default_font)),
                    "ulnone" | "ulc" => false,
                    w if w.starts_with("ul") => on,
                    "fs" => {
                        let first = *first_fs.get_or_insert(param.unwrap_or(0));
                        param.is_some_and(|n| n != first)
                    }
                    _ => false,
                };
                if rich {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// RTF source as 7-bit bytes: any non-ASCII char becomes `\uN?` (UTF-16 units, signed).
pub fn rtf_to_ascii(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c as u8);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.extend_from_slice(format!("\\u{}?", *u as i16).as_bytes());
            }
        }
    }
    out
}

// ---------------------------------------------------------------- RichEdit glue (constants + structs)

const EM_EXLIMITTEXT: u32 = 0x0435;
const EM_GETCHARFORMAT: u32 = 0x043A;
const EM_SETCHARFORMAT: u32 = 0x0444;
const EM_SETEVENTMASK: u32 = 0x0445;
const EM_STREAMIN: u32 = 0x0449;
const EM_STREAMOUT: u32 = 0x044A;
const ENM_CHANGE: isize = 1;
const ENM_SELCHANGE: isize = 0x80000;
const EN_SELCHANGE: u32 = 0x0702;
const EN_CHANGE: u32 = 0x0300;
const SCF_DEFAULT: usize = 0;
const SCF_SELECTION: usize = 1;
const SF_RTF: usize = 2;
const CFM_BOLD: u32 = 1;
const CFM_ITALIC: u32 = 2;
const CFM_UNDERLINE: u32 = 4;
const CFM_FACE: u32 = 0x2000_0000;
const CFM_SIZE: u32 = 0x8000_0000;
const ES_NOHIDESEL: u32 = 0x100;

#[repr(C)]
struct CharFormat {
    size: u32,
    mask: u32,
    effects: u32,
    height: i32,
    offset: i32,
    color: u32,
    charset: u8,
    pitch: u8,
    face: [u16; 32],
}

impl CharFormat {
    fn new() -> CharFormat {
        CharFormat { size: std::mem::size_of::<CharFormat>() as u32, mask: 0, effects: 0, height: 0, offset: 0, color: 0, charset: 1, pitch: 0, face: [0; 32] }
    }
}

type StreamCb = unsafe extern "system" fn(usize, *mut u8, i32, *mut i32) -> u32;

/// richedit.h declares EDITSTREAM under `#pragma pack(4)`: the callback sits at offset 12.
#[repr(C, packed(4))]
struct EditStream {
    cookie: usize,
    error: u32,
    callback: Option<StreamCb>,
}

struct Src<'a> {
    data: &'a [u8],
    pos: usize,
}

unsafe extern "system" fn read_cb(cookie: usize, buf: *mut u8, cb: i32, got: *mut i32) -> u32 {
    // SAFETY: cookie is the `&mut Src` passed by `stream_in`, alive for the whole EM_STREAMIN.
    let src = &mut *(cookie as *mut Src<'_>);
    let rest = src.data.get(src.pos..).unwrap_or(&[]);
    let n = rest.len().min(cb.max(0) as usize);
    std::ptr::copy_nonoverlapping(rest.as_ptr(), buf, n);
    src.pos += n;
    *got = n as i32;
    0
}

unsafe extern "system" fn write_cb(cookie: usize, buf: *mut u8, cb: i32, got: *mut i32) -> u32 {
    // SAFETY: cookie is the `&mut Vec<u8>` passed by `stream_out`; buf is valid for cb bytes.
    let out = &mut *(cookie as *mut Vec<u8>);
    out.extend_from_slice(std::slice::from_raw_parts(buf, cb.max(0) as usize));
    *got = cb;
    0
}

fn stream_in(rich: HWND, rtf: &[u8]) {
    let mut src = Src { data: rtf, pos: 0 };
    let es = EditStream { cookie: &mut src as *mut Src<'_> as usize, error: 0, callback: Some(read_cb) };
    send(rich, EM_STREAMIN, SF_RTF, &es as *const EditStream as isize);
}

fn stream_out(rich: HWND) -> String {
    let mut out: Vec<u8> = Vec::new();
    let es = EditStream { cookie: &mut out as *mut Vec<u8> as usize, error: 0, callback: Some(write_cb) };
    send(rich, EM_STREAMOUT, SF_RTF, &es as *const EditStream as isize);
    out.retain(|&b| b != 0); // RichEdit pads the stream with a NUL; it would truncate the registry string
    String::from_utf8_lossy(&out).into_owned()
}

fn char_format(rich: HWND) -> CharFormat {
    let mut cf = CharFormat::new();
    send(rich, EM_GETCHARFORMAT, SCF_SELECTION, &mut cf as *mut CharFormat as isize);
    cf
}

// ---------------------------------------------------------------- snippet editor

const ID_NAME: i32 = 101;
const ID_BOLD: i32 = 102;
const ID_ITALIC: i32 = 103;
const ID_UNDERLINE: i32 = 104;
const ID_BODY: i32 = 105;
const TIMER_COUNTER: usize = 1;

struct SnipDlg {
    ui: Ui,
    fonts: [Font; 3],
    name: Cell<HWND>,
    fmt_btns: [Cell<HWND>; 3],
    body: Cell<HWND>,
    hint: Cell<HWND>,
    counter: Cell<HWND>,
    ok: Cell<HWND>,
    cancel: Cell<HWND>,
    /// 0 = fine, 1 = approaching the limit, 2 = over it.
    level: Cell<u8>,
    /// Lower-cased name of the snippet being edited (it may keep its own name).
    own_name: Option<String>,
    done: Cell<bool>,
    result: RefCell<Option<Snippet>>,
}

impl SnipDlg {
    fn build(&self, h: HWND, initial: Option<&Snippet>) {
        let ui = &self.ui;
        ui.label(h, "Name:", (12, 15, 52, 18));
        let name = ui.add(h, w!("EDIT"), initial.map_or("", |s| s.name.as_str()), TAB | ES_AUTOHSCROLL as u32, WS_EX_CLIENTEDGE.0, (68, 12, 300, 24), ID_NAME);
        send(name, EM_SETLIMITTEXT, 200, 0);
        subclass_keys(name, None);
        self.name.set(name);
        let specs = [("B", ID_BOLD), ("I", ID_ITALIC), ("U", ID_UNDERLINE)];
        for ((cell, font), (text, id)) in self.fmt_btns.iter().zip(&self.fonts).zip(specs) {
            let b = ui.add(h, w!("BUTTON"), text, TAB | (BS_CHECKBOX | BS_PUSHLIKE) as u32, 0, (12, 44, 32, 26), id);
            set_font(b, font.0);
            cell.set(b);
        }
        // SAFETY: loads the RichEdit 4.1+ control class once; failure leaves the class missing.
        unsafe {
            let _ = LoadLibraryW(w!("Msftedit.dll"));
        }
        let style = TAB | WS_VSCROLL.0 | (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32 | ES_NOHIDESEL;
        let body = ui.add(h, w!("RICHEDIT50W"), "", style, WS_EX_CLIENTEDGE.0, (12, 76, 400, 250), ID_BODY);
        if !body.0.is_null() {
            send(body, EM_EXLIMITTEXT, 0, 0x7FF_FFFF);
            send(body, EM_SETEVENTMASK, 0, ENM_CHANGE | ENM_SELCHANGE);
            let mut cf = CharFormat::new();
            cf.mask = CFM_FACE | CFM_SIZE;
            cf.height = 220; // 11 pt, in twips
            for (d, c) in cf.face.iter_mut().zip("Calibri".encode_utf16()) {
                *d = c;
            }
            send(body, EM_SETCHARFORMAT, SCF_DEFAULT, &cf as *const CharFormat as isize);
            subclass_keys(body, Some([ID_BOLD, ID_ITALIC, ID_UNDERLINE]));
            match initial {
                Some(s) if s.is_rtf() => stream_in(body, &rtf_to_ascii(&s.content)),
                Some(s) => set_text(body, &to_crlf(&s.content)),
                None => {}
            }
            send(body, EM_EMPTYUNDOBUFFER, 0, 0);
        }
        self.body.set(body);
        self.hint.set(ui.label(h, "Placeholders: {{date}} {{time}} {{datetime}} {{year}} {{month}} {{day}} {{hour}} {{minute}} {{second}} {{clipboard}}", (12, 330, 400, 34)));
        self.counter.set(ui.add(h, w!("STATIC"), "", SS_NOPREFIX | SS_RIGHT, 0, (420, 330, 120, 18), -1));
        self.ok.set(ui.button(h, "OK", (0, 0, 92, 28), ID_OK, true));
        self.cancel.set(ui.button(h, "Cancel", (0, 0, 92, 28), ID_CANCEL, false));
        self.layout(h);
        self.sync_buttons();
        self.update_counter();
    }

    fn layout(&self, h: HWND) {
        let (w, ch) = client_size(h);
        let ui = &self.ui;
        let (m, bh, bw, gap) = (ui.px(12), ui.px(28), ui.px(92), ui.px(8));
        let by = ch - m - bh;
        let hint_y = by - ui.px(8) - ui.px(34);
        put(self.name.get(), m + ui.px(56), m, w - 2 * m - ui.px(56), ui.px(24));
        let tb_y = m + ui.px(32);
        for (i, b) in self.fmt_btns.iter().enumerate() {
            put(b.get(), m + i as i32 * (ui.px(32) + ui.px(4)), tb_y, ui.px(32), ui.px(26));
        }
        let body_y = tb_y + ui.px(26) + ui.px(6);
        put(self.body.get(), m, body_y, w - 2 * m, hint_y - ui.px(6) - body_y);
        put(self.hint.get(), m, hint_y, w - 2 * m - ui.px(130), ui.px(34));
        put(self.counter.get(), w - m - ui.px(124), hint_y, ui.px(124), ui.px(18));
        put(self.ok.get(), w - m - 2 * bw - gap, by, bw, bh);
        put(self.cancel.get(), w - m - bw, by, bw, bh);
    }

    /// The snippet as currently edited: RTF + plain text when any character formatting is
    /// present, otherwise plain text only.
    fn snippet(&self) -> Snippet {
        let name = get_text(self.name.get()).trim().to_string();
        let plain = to_crlf(&get_text(self.body.get()));
        let rtf = stream_out(self.body.get());
        if rtf_has_formatting(&rtf) {
            Snippet { name, content: rtf, content_plain: Some(plain) }
        } else {
            Snippet { name, content: plain, content_plain: None }
        }
    }

    fn toggle(&self, effect: u32) {
        let body = self.body.get();
        let mut cf = char_format(body);
        let on = cf.mask & effect != 0 && cf.effects & effect != 0;
        cf.mask = effect;
        cf.effects = if on { 0 } else { effect };
        send(body, EM_SETCHARFORMAT, SCF_SELECTION, &cf as *const CharFormat as isize);
        // SAFETY: focus returns to the editor so typing continues with the new format.
        unsafe {
            let _ = SetFocus(Some(body));
        }
        self.sync_buttons();
    }

    fn sync_buttons(&self) {
        let cf = char_format(self.body.get());
        for (b, e) in self.fmt_btns.iter().zip([CFM_BOLD, CFM_ITALIC, CFM_UNDERLINE]) {
            set_check(b.get(), cf.mask & e != 0 && cf.effects & e != 0);
        }
    }

    fn update_counter(&self) {
        let n = encoded_len(&self.snippet());
        let level = if n > MAX_VALUE_CHARS {
            2
        } else if n * 10 >= MAX_VALUE_CHARS * 9 {
            1
        } else {
            0
        };
        self.level.set(level);
        set_text(self.counter.get(), &format!("{n} / {MAX_VALUE_CHARS}"));
    }

    fn accept(&self, app: &App, h: HWND) {
        let s = self.snippet();
        if let Err(e) = validate(&s) {
            message_box(h, &e, "clip4", MB_OK | MB_ICONWARNING);
            return;
        }
        let key = s.name.to_lowercase();
        let taken = self.own_name.as_deref() != Some(key.as_str()) && app.snippets.borrow().iter().any(|x| x.name.trim().to_lowercase() == key);
        if taken {
            message_box(h, "A snippet with this name already exists. Names are not case sensitive.", "clip4", MB_OK | MB_ICONWARNING);
            return;
        }
        *self.result.borrow_mut() = Some(s);
        finish(h, &self.done);
    }
}

impl Dlg for SnipDlg {
    fn msg(&self, app: &App, h: HWND, m: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
        match m {
            WM_SIZE => self.layout(h),
            WM_COMMAND => match (loword(w), hiword(w)) {
                (ID_OK, _) => self.accept(app, h),
                (ID_CANCEL, _) => finish(h, &self.done),
                (ID_BOLD, _) => self.toggle(CFM_BOLD),
                (ID_ITALIC, _) => self.toggle(CFM_ITALIC),
                (ID_UNDERLINE, _) => self.toggle(CFM_UNDERLINE),
                (ID_NAME | ID_BODY, EN_CHANGE) => {
                    // Debounced: streaming the RTF on every keystroke would be wasteful.
                    // SAFETY: one-shot timer on this dialog.
                    unsafe {
                        SetTimer(Some(h), TIMER_COUNTER, 150, None);
                    }
                }
                _ => {}
            },
            WM_NOTIFY => {
                // SAFETY: WM_NOTIFY's lParam is an NMHDR*.
                let nm = unsafe { &*(l.0 as *const NMHDR) };
                if nm.code == EN_SELCHANGE && nm.hwndFrom == self.body.get() {
                    self.sync_buttons();
                }
            }
            WM_TIMER if w.0 == TIMER_COUNTER => {
                // SAFETY: stops the one-shot timer.
                unsafe {
                    let _ = KillTimer(Some(h), TIMER_COUNTER);
                }
                self.update_counter();
            }
            WM_CTLCOLORSTATIC if HWND(l.0 as *mut _) == self.counter.get() && self.level.get() > 0 => {
                let colour = if self.level.get() > 1 { 0x0000_00C0 } else { 0x0000_64D0 }; // 0x00BBGGRR: red / dark orange
                // SAFETY: wParam is the static's DC for this message.
                unsafe {
                    let hdc = HDC(w.0 as *mut _);
                    SetTextColor(hdc, COLORREF(colour));
                    SetBkMode(hdc, TRANSPARENT);
                    return Some(LRESULT(GetSysColorBrush(COLOR_3DFACE).0 as isize));
                }
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

fn snippet_editor_with(app: &App, initial: Option<&Snippet>, owner: Option<HWND>) -> Option<Snippet> {
    let scr = screen_at_cursor();
    let s = scr.scale;
    let px = |v: f32| (v * s).round() as i32;
    let dlg = Rc::new(SnipDlg {
        ui: Ui::new(s),
        fonts: [Font::new("Segoe UI", px(12.0), 700, false, false), Font::new("Segoe UI", px(12.0), 400, true, false), Font::new("Segoe UI", px(12.0), 400, false, true)],
        name: Cell::default(),
        fmt_btns: Default::default(),
        body: Cell::default(),
        hint: Cell::default(),
        counter: Cell::default(),
        ok: Cell::default(),
        cancel: Cell::default(),
        level: Cell::new(0),
        own_name: initial.map(|i| i.name.trim().to_lowercase()),
        done: Cell::new(false),
        result: RefCell::new(None),
    });
    let title = if initial.is_some() { "Edit snippet" } else { "Add snippet" };
    let h = create(app, title, (640, 470), true, &scr, owner, dlg.clone())?;
    dlg.build(h, initial);
    if dlg.body.get().0.is_null() {
        crate::log_err!("RichEdit control (Msftedit.dll) is unavailable");
        close(h);
        return None;
    }
    present(h, if initial.is_some() { dlg.body.get() } else { dlg.name.get() });
    run_modal(h, owner, &dlg.done);
    close(h);
    let r = dlg.result.borrow_mut().take();
    r
}

/// Add (`None`) or edit (`Some`) a snippet. `None` back = cancelled.
pub fn snippet_editor(app: &App, initial: Option<&Snippet>) -> Option<Snippet> {
    snippet_editor_with(app, initial, None)
}

// ---------------------------------------------------------------- manager

const ID_LIST: i32 = 100;
const ID_ADD: i32 = 101;
const ID_DELETE: i32 = 102;
const ID_UP: i32 = 103;
const ID_DOWN: i32 = 104;
const LBS_NOINTEGRALHEIGHT: u32 = 0x100;

thread_local! {
    static OPEN: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
}

struct MgrDlg {
    ui: Ui,
    list: Cell<HWND>,
    /// Add, Edit, Delete, Move up, Move down, Close.
    btns: [Cell<HWND>; 6],
    done: Cell<bool>,
}

impl MgrDlg {
    fn build(&self, h: HWND, app: &App) {
        let ui = &self.ui;
        self.list.set(ui.add(h, w!("LISTBOX"), "", TAB | WS_VSCROLL.0 | LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT, WS_EX_CLIENTEDGE.0, (12, 12, 300, 300), ID_LIST));
        let specs = [("Add\u{2026}", ID_ADD), ("Edit\u{2026}", ID_OK), ("Delete", ID_DELETE), ("Move up", ID_UP), ("Move down", ID_DOWN), ("Close", ID_CANCEL)];
        for (cell, (text, id)) in self.btns.iter().zip(specs) {
            cell.set(ui.button(h, text, (320, 12, 110, 28), id, id == ID_OK));
        }
        self.layout(h);
        self.refill(app, Some(0));
    }

    fn layout(&self, h: HWND) {
        let (w, ch) = client_size(h);
        let ui = &self.ui;
        let (m, bw, bh, step) = (ui.px(12), ui.px(110), ui.px(28), ui.px(34));
        put(self.list.get(), m, m, w - 3 * m - bw, ch - 2 * m);
        let x = w - m - bw;
        for (i, b) in self.btns.iter().enumerate().take(5) {
            let extra = if i >= 3 { ui.px(12) } else { 0 };
            put(b.get(), x, m + i as i32 * step + extra, bw, bh);
        }
        if let Some(close_btn) = self.btns.get(5) {
            put(close_btn.get(), x, ch - m - bh, bw, bh);
        }
    }

    /// Reloads the list box from the app's snippets and selects `sel` (clamped).
    fn refill(&self, app: &App, sel: Option<usize>) {
        let names: Vec<String> = app.snippets.borrow().iter().map(|s| s.name.clone()).collect();
        let list = self.list.get();
        send(list, LB_RESETCONTENT, 0, 0);
        for n in &names {
            let t = wide(n);
            send(list, LB_ADDSTRING, 0, t.as_ptr() as isize);
        }
        if !names.is_empty() {
            send(list, LB_SETCURSEL, sel.unwrap_or(0).min(names.len() - 1), 0);
        }
        self.sync_buttons(names.len());
    }

    fn selected(&self) -> Option<usize> {
        usize::try_from(send(self.list.get(), LB_GETCURSEL, 0, 0)).ok()
    }

    fn sync_buttons(&self, count: usize) {
        let sel = self.selected();
        let on = [true, sel.is_some(), sel.is_some(), sel.is_some_and(|i| i > 0), sel.is_some_and(|i| i + 1 < count)];
        for (b, on) in self.btns.iter().zip(on) {
            enable(b.get(), on);
        }
    }

    fn add(&self, app: &App, h: HWND) {
        let Some(s) = snippet_editor_with(app, None, Some(h)) else { return };
        let last = {
            let mut list = app.snippets.borrow_mut();
            list.push(s);
            list.len() - 1
        };
        app.snippets_changed();
        self.refill(app, Some(last));
    }

    fn edit(&self, app: &App, h: HWND) {
        let Some(i) = self.selected() else { return };
        let Some(cur) = app.snippets.borrow().get(i).cloned() else { return };
        let Some(s) = snippet_editor_with(app, Some(&cur), Some(h)) else { return };
        if let Some(slot) = app.snippets.borrow_mut().get_mut(i) {
            *slot = s;
        }
        app.snippets_changed();
        self.refill(app, Some(i));
    }

    fn delete(&self, app: &App, h: HWND) {
        let Some(i) = self.selected() else { return };
        let Some(name) = app.snippets.borrow().get(i).map(|s| s.name.clone()) else { return };
        if message_box(h, &format!("Delete snippet \"{name}\"?"), "clip4", MB_YESNO | MB_ICONQUESTION) != IDYES {
            return;
        }
        {
            let mut list = app.snippets.borrow_mut();
            if i < list.len() {
                list.remove(i);
            }
        }
        app.snippets_changed();
        self.refill(app, Some(i));
    }

    fn shift(&self, app: &App, up: bool) {
        let Some(i) = self.selected() else { return };
        let j = if up { i.wrapping_sub(1) } else { i + 1 };
        {
            let mut list = app.snippets.borrow_mut();
            if i >= list.len() || j >= list.len() {
                return;
            }
            list.swap(i, j);
        }
        app.snippets_changed();
        self.refill(app, Some(j));
    }
}

impl Dlg for MgrDlg {
    fn msg(&self, app: &App, h: HWND, m: u32, w: WPARAM, _l: LPARAM) -> Option<LRESULT> {
        match m {
            WM_SIZE => self.layout(h),
            WM_COMMAND => match (loword(w), hiword(w)) {
                (ID_LIST, LBN_DBLCLK) => self.edit(app, h),
                (ID_LIST, LBN_SELCHANGE) => self.sync_buttons(app.snippets.borrow().len()),
                (ID_ADD, _) => self.add(app, h),
                (ID_OK, _) => self.edit(app, h),
                (ID_DELETE, _) => self.delete(app, h),
                (ID_UP, _) => self.shift(app, true),
                (ID_DOWN, _) => self.shift(app, false),
                (ID_CANCEL, _) => finish(h, &self.done),
                _ => {}
            },
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

/// The "Manage snippets" window: list, add, edit (rich editor), delete, reorder. Every change
/// is written back to `app.snippets` and persisted/refreshed through `snippets_changed`.
pub fn manage_snippets(app: &App) {
    if OPEN.with(raise_if_open) {
        return;
    }
    let scr = screen_at_cursor();
    let dlg = Rc::new(MgrDlg { ui: Ui::new(scr.scale), list: Cell::default(), btns: Default::default(), done: Cell::new(false) });
    let Some(h) = create(app, "Manage snippets", (460, 340), true, &scr, None, dlg.clone()) else { return };
    OPEN.with(|o| o.set(h));
    dlg.build(h, app);
    present(h, dlg.list.get());
    run_modal(h, None, &dlg.done);
    OPEN.with(|o| o.set(HWND::default()));
    close(h);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What RichEdit streams out for unformatted text (header + body).
    const PLAIN: &str = "{\\rtf1\\ansi\\ansicpg1252\\deff0\\nouicompat\\deflang1033{\\fonttbl{\\f0\\fnil\\fcharset0 Calibri;}}\r\n{\\*\\generator Riched20 10.0.19041}\\viewkind4\\uc1 \r\n\\pard\\sa200\\sl276\\slmult1\\f0\\fs22\\lang9 hello \\\\b world\\par\r\n}\r\n";

    #[test]
    fn unformatted_richedit_output_is_plain() {
        assert!(!rtf_has_formatting(PLAIN));
        assert!(!rtf_has_formatting(""));
        assert!(!rtf_has_formatting("plain text, no rtf at all \\\\b"));
    }

    #[test]
    fn character_formatting_is_detected() {
        let with = |body: &str| PLAIN.replace("hello", body);
        assert!(rtf_has_formatting(&with("\\b bold\\b0 x")));
        assert!(rtf_has_formatting(&with("\\i it\\i0 x")));
        assert!(rtf_has_formatting(&with("\\ul under\\ulnone x")));
        assert!(rtf_has_formatting(&with("\\cf1 red")));
        assert!(rtf_has_formatting(&with("\\highlight2 mark")));
        assert!(rtf_has_formatting(&with("\\f1 other font")));
        assert!(rtf_has_formatting(&with("\\fs40 big")));
        assert!(rtf_has_formatting(&with("\\b1 bold")));
        assert!(rtf_has_formatting(&with("\\uldb x")));
    }

    #[test]
    fn off_switches_and_headers_do_not_count() {
        let with = |body: &str| PLAIN.replace("hello", body);
        assert!(!rtf_has_formatting(&with("\\b0 x\\i0 y\\ulnone z\\cf0 w\\fs22 v")));
        // more fonts / colours in the header tables only
        let hdr = "{\\rtf1{\\fonttbl{\\f0 A;}{\\f1 B;}}{\\colortbl;\\red255\\green0\\blue0;}\\pard\\f0\\fs22 text\\par}";
        assert!(!rtf_has_formatting(hdr));
        assert!(rtf_has_formatting("{\\rtf1{\\fonttbl{\\f0 A;}{\\f1 B;}}\\pard\\f1 text\\par}"));
        // an ignorable destination is skipped, an escaped backslash-b is text
        assert!(!rtf_has_formatting("{\\rtf1{\\*\\foo \\b x}\\pard text \\\\b\\par}"));
    }

    #[test]
    fn same_face_listed_twice_is_not_a_font_change() {
        // what RichEdit really emits: f1 is a second entry for the same face, used by the last paragraph mark
        let rtf = "{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0\\fnil\\fcharset0 Calibri;}{\\f1\\fnil Calibri;}}\\pard\\f0\\fs22 line1\\par\r\nline2\\f1\\par\r\n}\r\n";
        assert!(!rtf_has_formatting(rtf));
        let other = rtf.replace("{\\f1\\fnil Calibri;}", "{\\f1\\fnil Arial;}");
        assert!(rtf_has_formatting(&other));
        assert_eq!(font_faces(rtf), vec![(0, "Calibri".to_string()), (1, "Calibri".to_string())]);
        assert!(font_faces("no table").is_empty());
    }

    #[test]
    fn rtf_ascii_escapes_non_ascii() {
        assert_eq!(rtf_to_ascii("abc"), b"abc");
        assert_eq!(rtf_to_ascii("\u{e9}"), b"\\u233?");
        assert_eq!(rtf_to_ascii("\u{20ac}"), b"\\u8364?");
        assert_eq!(rtf_to_ascii("\u{1F600}"), b"\\u-10179?\\u-8704?");
        assert_eq!(rtf_to_ascii("\u{4e2d}x"), b"\\u20013?x");
    }
}
