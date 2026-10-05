//! `settings_dialog`: the "clip4 Settings" window (spec 16.1).
//!
//! Look settings (theme, fonts, sizes, colours, expand, sound) apply live through
//! `App::apply_settings(.., false)`; hotkeys and history size are only applied on Save.

use super::modal::*;
use crate::theme::{self, Overrides, Rgb, PRESETS};
use crate::win::app::App;
use crate::win::gfx::Gfx;
use crate::win::settings::{self, Action, Hotkey, Settings, ACTIONS, FOLLOW, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};
use crate::win::util::{pcw, wide};
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_FOCUS};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------- pure helpers

/// What a key press means in a hotkey field.
#[derive(Debug, PartialEq, Eq)]
pub enum Capture {
    /// A lone modifier or a key that is not allowed: keep waiting.
    Ignore,
    /// Esc / Backspace: unbind.
    Clear,
    Set(Hotkey),
}

/// `vk` pressed while `mods` (MOD_* bits) are held. Keys that type text need a modifier;
/// only F1-F24, Pause, Scroll Lock and Print Screen may be bound bare.
pub fn capture(vk: u32, mods: u32) -> Capture {
    const MODIFIERS: [u32; 10] = [0x10, 0x11, 0x12, 0x5B, 0x5C, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4];
    if vk == 0 || vk > 0xFE || vk == 0xA5 || vk == 0xE5 || MODIFIERS.contains(&vk) {
        return Capture::Ignore;
    }
    if mods == 0 && matches!(vk, 0x1B | 0x08) {
        return Capture::Clear;
    }
    let bare_ok = matches!(vk, 0x70..=0x87 | 0x13 | 0x91 | 0x2C);
    if mods == 0 && !bare_ok {
        return Capture::Ignore;
    }
    Capture::Set(Hotkey { mods, vk })
}

/// Hotkey <-> message parameter (`mods << 8 | vk`).
fn pack(h: Hotkey) -> isize {
    ((h.mods << 8) | h.vk) as isize
}

fn unpack(v: isize) -> Hotkey {
    Hotkey { mods: (v >> 8) as u32 & 0xF, vk: v as u32 & 0xFF }
}

/// History size field: a whole number in 10..=2000.
pub fn parse_history(s: &str) -> Result<usize, String> {
    match s.trim().parse::<usize>() {
        Ok(n) if (10..=2000).contains(&n) => Ok(n),
        _ => Err("History size must be a whole number between 10 and 2000.".into()),
    }
}

/// The first two actions bound to the same combination.
pub fn find_conflict(hk: &[Hotkey; 5]) -> Option<(Action, Action)> {
    for (i, a) in hk.iter().enumerate() {
        for (j, b) in hk.iter().enumerate().skip(i + 1) {
            if a.is_bound() && a == b {
                return Some((Action::from_index(i)?, Action::from_index(j)?));
            }
        }
    }
    None
}

/// The colour each swatch should show: Background, Text, Accent, Selected text, Border, Dim.
pub fn effective_colors(s: &Settings) -> [Rgb; 6] {
    let o = Overrides::from_registry(s.colors);
    let p = theme::palette(s.theme_id, &o);
    [p.bg, o.text.unwrap_or(p.ink_high), p.accent, p.sel_text, p.hairline, p.ink_low]
}

fn current_mods() -> u32 {
    // SAFETY: plain key-state queries.
    let down = |vk: VIRTUAL_KEY| unsafe { GetKeyState(vk.0 as i32) } < 0;
    let bit = |on: bool, m: u32| if on { m } else { 0 };
    bit(down(VK_CONTROL), MOD_CONTROL) | bit(down(VK_SHIFT), MOD_SHIFT) | bit(down(VK_MENU), MOD_ALT) | bit(down(VK_LWIN) || down(VK_RWIN), MOD_WIN)
}

// ---------------------------------------------------------------- hotkey field

const WM_HK_SET: u32 = WM_APP + 0x40;

fn hk_subclass(h: HWND, idx: usize) {
    // SAFETY: subclass installed on a control we created; removed at WM_NCDESTROY.
    unsafe {
        let _ = SetWindowSubclass(h, Some(hk_proc), 2, idx);
    }
}

unsafe extern "system" fn hk_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM, _uid: usize, idx: usize) -> LRESULT {
    match m {
        WM_GETDLGCODE => {
            // Plain Tab / Shift+Tab still move focus; every other key is ours.
            let vk = if l.0 == 0 { 0 } else { (*(l.0 as *const MSG)).wParam.0 as u32 };
            let code = if vk == VK_TAB.0 as u32 && current_mods() & !MOD_SHIFT == 0 { DLGC_WANTCHARS } else { DLGC_WANTALLKEYS | DLGC_WANTCHARS };
            return LRESULT(code as isize);
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let hk = match capture(w.0 as u32, current_mods()) {
                Capture::Ignore => None,
                Capture::Clear => Some(Hotkey::NONE),
                Capture::Set(hk) => Some(hk),
            };
            if let (Some(hk), Ok(parent)) = (hk, GetParent(h)) {
                let _ = PostMessageW(Some(parent), WM_HK_SET, WPARAM(idx), LPARAM(pack(hk)));
            }
            return LRESULT(0);
        }
        WM_KEYUP | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR | WM_DEADCHAR | WM_SYSDEADCHAR | WM_CONTEXTMENU => return LRESULT(0),
        WM_SETFOCUS => {
            let r = DefSubclassProc(h, m, w, l);
            send(h, EM_SETSEL, 0, -1); // highlighted = "press the combination now"
            return r;
        }
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(h, Some(hk_proc), 2);
        }
        _ => {}
    }
    DefSubclassProc(h, m, w, l)
}

// ---------------------------------------------------------------- the dialog

const ID_HK: i32 = 200;
const ID_THEME: i32 = 210;
const ID_FONT: i32 = 211;
const ID_CSIZE: i32 = 212;
const ID_USIZE: i32 = 213;
const ID_SW: i32 = 220;
const ID_RESET: i32 = 230;
const ID_HIST: i32 = 240;
const ID_CLEAR: i32 = 241;
const ID_EXPAND: i32 = 250;
const ID_STARTUP: i32 = 251;
const ID_SOUND: i32 = 252;
const ID_DEFAULTS: i32 = 260;
const TIMER_SWATCH: usize = 1;
const SWATCH_NAMES: [&str; 6] = ["Background", "Text", "Accent", "Selected text", "Border", "Dim"];
const IN_USE: &str = "Already in use by another application";

thread_local! {
    static OPEN: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
}

struct SettingsDlg {
    ui: Ui,
    /// Edited (not yet saved) hotkeys.
    hk: Cell<[Hotkey; 5]>,
    hk_edit: [Cell<HWND>; 5],
    hk_status: [Cell<HWND>; 5],
    theme: Cell<HWND>,
    font: Cell<HWND>,
    csize: Cell<HWND>,
    usize_: Cell<HWND>,
    swatch: [Cell<HWND>; 6],
    custom_lbl: Cell<HWND>,
    custom_btn: Cell<HWND>,
    hist: Cell<HWND>,
    expand: Cell<HWND>,
    startup: Cell<HWND>,
    sound: Cell<HWND>,
    /// Swatch whose first click is waiting to see whether a second one follows.
    pending: Cell<Option<usize>>,
    /// Swatch reset by a double-click, and when.
    reset_at: Cell<Option<(usize, Instant)>>,
    custom_colors: Cell<[COLORREF; 16]>,
    done: Cell<bool>,
    saved: Cell<bool>,
}

impl SettingsDlg {
    fn group(&self, h: HWND, text: &str, r: (i32, i32, i32, i32)) {
        self.ui.add(h, w!("BUTTON"), text, BS_GROUPBOX as u32, 0, r, -1);
    }

    fn build(&self, h: HWND, app: &App) {
        let ui = &self.ui;
        let hotkeys = self.hk.get();
        self.group(h, "Hotkeys", (12, 8, 624, 190));
        for (i, a) in ACTIONS.iter().enumerate() {
            let y = 30 + i as i32 * 28;
            ui.label(h, &format!("{}:", a.title()), (24, y + 4, 190, 18));
            let style = TAB | (ES_READONLY | ES_AUTOHSCROLL) as u32;
            let e = ui.add(h, w!("EDIT"), "", style, WS_EX_CLIENTEDGE.0, (220, y, 170, 24), ID_HK + i as i32);
            hk_subclass(e, i);
            if let (Some(ce), Some(cs)) = (self.hk_edit.get(i), self.hk_status.get(i)) {
                ce.set(e);
                cs.set(ui.label(h, "", (398, y + 4, 236, 18)));
            }
        }
        ui.label(h, "Click a field, then press the key combination. Esc or Backspace clears it. Applied on Save.", (24, 172, 600, 18));

        self.group(h, "Appearance", (12, 204, 624, 196));
        let combo = TAB | CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0;
        ui.label(h, "Overlay theme:", (24, 232, 110, 18));
        self.theme.set(ui.add(h, w!("COMBOBOX"), "", combo, 0, (140, 228, 230, 300), ID_THEME));
        ui.label(h, "Content size:", (392, 232, 90, 18));
        self.csize.set(ui.add(h, w!("COMBOBOX"), "", combo, 0, (486, 228, 64, 240), ID_CSIZE));
        ui.label(h, "Content font:", (24, 262, 110, 18));
        self.font.set(ui.add(h, w!("COMBOBOX"), "", combo, 0, (140, 258, 230, 300), ID_FONT));
        ui.label(h, "UI text size:", (392, 262, 90, 18));
        self.usize_.set(ui.add(h, w!("COMBOBOX"), "", combo, 0, (486, 258, 64, 240), ID_USIZE));
        for p in PRESETS.iter() {
            add_item(self.theme.get(), CB_ADDSTRING, p.name);
        }
        for n in 10..=24 {
            add_item(self.csize.get(), CB_ADDSTRING, &n.to_string());
        }
        for n in 10..=28 {
            add_item(self.usize_.get(), CB_ADDSTRING, &n.to_string());
        }
        for f in Gfx::new().map(|g| g.installed_families()).unwrap_or_default() {
            add_item(self.font.get(), CB_ADDSTRING, &f);
        }
        ui.label(h, "Colours: click a swatch to change it, double-click to follow the theme again.", (24, 290, 600, 18));
        for (i, (cell, name)) in self.swatch.iter().zip(SWATCH_NAMES).enumerate() {
            let (x, y) = (24 + (i as i32 % 3) * 204, 312 + (i as i32 / 3) * 28);
            cell.set(ui.add(h, w!("BUTTON"), "", TAB | BS_OWNERDRAW as u32, 0, (x, y, 30, 22), ID_SW + i as i32));
            ui.label(h, name, (x + 38, y + 3, 150, 18));
        }
        self.custom_lbl.set(ui.label(h, "Custom colours active \u{2014}", (24, 372, 170, 18)));
        self.custom_btn.set(ui.button(h, "Reset", (196, 368, 64, 24), ID_RESET, false));

        self.group(h, "History and behaviour", (12, 406, 624, 88));
        ui.label(h, "History size (10-2000):", (24, 436, 150, 18));
        self.hist.set(ui.add(h, w!("EDIT"), &app.settings.borrow().max_items.to_string(), TAB | (ES_NUMBER | ES_AUTOHSCROLL) as u32, WS_EX_CLIENTEDGE.0, (180, 432, 70, 24), ID_HIST));
        send(self.hist.get(), EM_SETLIMITTEXT, 4, 0);
        ui.button(h, "Clear history\u{2026}", (488, 431, 136, 26), ID_CLEAR, false);
        let check = TAB | BS_AUTOCHECKBOX as u32;
        self.expand.set(ui.add(h, w!("BUTTON"), "Expand selected item", check, 0, (24, 462, 190, 22), ID_EXPAND));
        self.startup.set(ui.add(h, w!("BUTTON"), "Start with Windows", check, 0, (222, 462, 190, 22), ID_STARTUP));
        self.sound.set(ui.add(h, w!("BUTTON"), "Play capture sound", check, 0, (420, 462, 204, 22), ID_SOUND));

        ui.button(h, "Defaults", (12, 502, 100, 28), ID_DEFAULTS, false);
        ui.button(h, "Save", (448, 502, 92, 28), ID_OK, true);
        ui.button(h, "Cancel", (544, 502, 92, 28), ID_CANCEL, false);

        self.sync(app);
        self.refresh_hotkeys(app, hotkeys);
    }

    /// Puts every look/behaviour control in line with the live settings.
    fn sync(&self, app: &App) {
        let s = app.settings.borrow().clone();
        send(self.theme.get(), CB_SETCURSEL, s.theme_id, 0);
        let name = wide(&s.font_face);
        let font = self.font.get();
        let mut idx = send(font, CB_FINDSTRINGEXACT, usize::MAX, name.as_ptr() as isize);
        if idx < 0 {
            idx = send(font, CB_INSERTSTRING, 0, name.as_ptr() as isize); // a face that is no longer installed
        }
        send(font, CB_SETCURSEL, idx.max(0) as usize, 0);
        send(self.csize.get(), CB_SETCURSEL, s.content_size.clamp(10, 24) as usize - 10, 0);
        send(self.usize_.get(), CB_SETCURSEL, s.ui_size.clamp(10, 28) as usize - 10, 0);
        set_check(self.expand.get(), s.expand_selected);
        set_check(self.sound.get(), s.sound);
        set_check(self.startup.get(), settings::startup_enabled());
        self.refresh_colors(&s);
    }

    fn refresh_colors(&self, s: &Settings) {
        for sw in &self.swatch {
            invalidate(sw.get());
        }
        let any = Overrides::from_registry(s.colors).any();
        show(self.custom_lbl.get(), any);
        show(self.custom_btn.get(), any);
    }

    /// Hotkey fields + the "already in use" notes (only for the binding that is registered now).
    fn refresh_hotkeys(&self, app: &App, hk: [Hotkey; 5]) {
        let registered = app.settings.borrow().hotkeys;
        let failed: Vec<Action> = app.hk_failed.borrow().clone();
        for (((a, k), reg), (e, st)) in ACTIONS.iter().zip(hk).zip(registered).zip(self.hk_edit.iter().zip(&self.hk_status)) {
            set_text(e.get(), &k.describe());
            set_text(st.get(), if k.is_bound() && k == reg && failed.contains(a) { IN_USE } else { "" });
        }
    }

    /// Applies a look change immediately (persists the look and repaints the overlay).
    fn live(&self, app: &App, f: impl FnOnce(&mut Settings)) {
        let mut n = app.settings.borrow().clone();
        f(&mut n);
        app.apply_settings(n, false);
        let s = app.settings.borrow().clone();
        self.refresh_colors(&s);
    }

    fn combo_changed(&self, app: &App, id: i32, code: u32) {
        let cb = match id {
            ID_THEME => self.theme.get(),
            ID_FONT => self.font.get(),
            ID_CSIZE => self.csize.get(),
            _ => self.usize_.get(),
        };
        // While the list is open wait for CBN_SELENDOK; re-picking the current item sends only that.
        if code == CBN_SELCHANGE && send(cb, CB_GETDROPPEDSTATE, 0, 0) != 0 {
            return;
        }
        let Ok(sel) = usize::try_from(send(cb, CB_GETCURSEL, 0, 0)) else { return };
        match id {
            ID_THEME => self.live(app, |n| {
                // Selecting a preset clears every colour override first (lesson 18.20).
                let mut o = Overrides::from_registry(n.colors);
                theme::select_preset(&mut n.theme_id, &mut o, sel);
                n.colors = o.to_registry();
            }),
            ID_FONT => {
                let mut buf = vec![0u16; send(cb, CB_GETLBTEXTLEN, sel, 0).max(0) as usize + 1];
                send(cb, CB_GETLBTEXT, sel, buf.as_mut_ptr() as isize);
                let face = crate::win::util::from_wide(&buf);
                self.live(app, |n| n.font_face = face);
            }
            ID_CSIZE => self.live(app, |n| n.content_size = 10 + sel as u32),
            _ => self.live(app, |n| n.ui_size = 10 + sel as u32),
        }
    }

    /// BN_CLICKED / BN_DOUBLECLICKED of swatch `i`. The first click waits one double-click time
    /// before opening the picker; a second one in that window resets the colour instead.
    fn swatch_clicked(&self, h: HWND, i: usize, app: &App) {
        // SAFETY: timer on this dialog.
        unsafe {
            let window = Duration::from_millis(GetDoubleClickTime() as u64);
            // A double-click may be reported as BN_DOUBLECLICKED and then BN_CLICKED: act once.
            if self.reset_at.get().is_some_and(|(j, t)| j == i && t.elapsed() < window) {
                return;
            }
            if self.pending.get() == Some(i) {
                // Second click inside the double-click time: back to the theme's colour.
                let _ = KillTimer(Some(h), TIMER_SWATCH);
                self.pending.set(None);
                self.reset_at.set(Some((i, Instant::now())));
                self.live(app, |n| {
                    if let Some(c) = n.colors.get_mut(i) {
                        *c = FOLLOW;
                    }
                });
            } else {
                self.pending.set(Some(i));
                SetTimer(Some(h), TIMER_SWATCH, GetDoubleClickTime(), None);
            }
        }
    }

    fn pick_color(&self, h: HWND, i: usize, app: &App) {
        let s = app.settings.borrow().clone();
        let init = effective_colors(&s).get(i).copied().unwrap_or(Rgb::BLACK);
        let mut custom = self.custom_colors.get();
        let mut cc = CHOOSECOLORW {
            lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
            hwndOwner: h,
            rgbResult: COLORREF(init.colorref()),
            lpCustColors: custom.as_mut_ptr(),
            Flags: CC_FULLOPEN | CC_RGBINIT,
            ..Default::default()
        };
        // SAFETY: modal common dialog; `custom` outlives the call and no RefCell borrow is held.
        let ok = unsafe { ChooseColorW(&mut cc) }.as_bool();
        self.custom_colors.set(custom);
        if ok {
            let c = cc.rgbResult.0 & 0x00FF_FFFF;
            self.live(app, |n| {
                if let Some(slot) = n.colors.get_mut(i) {
                    *slot = c;
                }
            });
        }
    }

    fn draw_swatch(&self, app: &App, di: &DRAWITEMSTRUCT) {
        let Some(i) = (di.CtlID as i32).checked_sub(ID_SW).and_then(|i| usize::try_from(i).ok()) else { return };
        let s = app.settings.borrow().clone();
        let col = effective_colors(&s).get(i).copied().unwrap_or(Rgb::BLACK);
        let custom = s.colors.get(i).is_some_and(|c| c >> 24 == 0);
        // SAFETY: GDI drawing into the DC supplied with WM_DRAWITEM; brushes are deleted here.
        unsafe {
            let (fill, frame) = (CreateSolidBrush(COLORREF(col.colorref())), CreateSolidBrush(COLORREF(0x0040_4040)));
            let mut r = di.rcItem;
            FillRect(di.hDC, &r, fill);
            FrameRect(di.hDC, &r, frame);
            if custom {
                r = RECT { left: r.left + 1, top: r.top + 1, right: r.right - 1, bottom: r.bottom - 1 };
                FrameRect(di.hDC, &r, frame);
            }
            if di.itemState.0 & ODS_FOCUS.0 != 0 {
                let f = RECT { left: r.left + 2, top: r.top + 2, right: r.right - 2, bottom: r.bottom - 2 };
                let _ = DrawFocusRect(di.hDC, &f);
            }
            let _ = DeleteObject(fill.into());
            let _ = DeleteObject(frame.into());
        }
    }

    fn defaults(&self, app: &App) {
        let d = Settings::default();
        self.hk.set(d.hotkeys);
        self.refresh_hotkeys(app, d.hotkeys);
        self.live(app, |n| {
            n.theme_id = d.theme_id;
            n.colors = d.colors;
            n.font_face = d.font_face.clone();
            n.content_size = d.content_size;
            n.ui_size = d.ui_size;
            n.expand_selected = d.expand_selected;
            n.sound = d.sound;
        });
        app.settings.borrow().save();
        set_text(self.hist.get(), &d.max_items.to_string());
        settings::set_startup(false);
        self.sync(app);
    }

    fn clear_history(&self, app: &App, h: HWND) {
        let ask = |text: &str, style| message_box(h, text, "clip4", style) == IDYES;
        if !ask("Delete all unpinned items from the clipboard history?", MB_YESNO | MB_ICONQUESTION) {
            return;
        }
        app.store.borrow_mut().clear_unpinned();
        if ask("Also delete the pinned items? This cannot be undone.", MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) {
            app.store.borrow_mut().clear_all();
            app.blobs.clear();
        }
        // Blobs of removed unpinned items are left for the next save's garbage collection.
        app.schedule_save();
        app.overlay.refresh(app);
    }

    fn save(&self, app: &App, h: HWND) {
        let warn = |text: &str| {
            message_box(h, text, "clip4", MB_OK | MB_ICONWARNING);
        };
        let max_items = match parse_history(&get_text(self.hist.get())) {
            Ok(n) => n,
            Err(e) => {
                warn(&e);
                // SAFETY: focus a control of this dialog.
                unsafe {
                    let _ = SetFocus(Some(self.hist.get()));
                }
                return;
            }
        };
        let hotkeys = self.hk.get();
        if let Some((a, b)) = find_conflict(&hotkeys) {
            warn(&format!("\"{}\" and \"{}\" use the same key combination ({}). Give each action its own.", a.title(), b.title(), hotkeys.get(a as usize).map_or_else(String::new, Hotkey::describe)));
            return;
        }
        let mut n = app.settings.borrow().clone();
        n.hotkeys = hotkeys;
        n.max_items = max_items;
        app.apply_settings(n, true);
        self.saved.set(true);
        finish(h, &self.done);
    }
}

fn add_item(cb: HWND, msg: u32, text: &str) {
    let t = wide(text);
    send(cb, msg, 0, pcw(&t).0 as isize);
}

impl Dlg for SettingsDlg {
    fn msg(&self, app: &App, h: HWND, m: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
        match m {
            WM_COMMAND => {
                let (id, code) = (loword(w), hiword(w));
                match id {
                    ID_OK => self.save(app, h),
                    ID_CANCEL => finish(h, &self.done),
                    ID_THEME | ID_FONT | ID_CSIZE | ID_USIZE if code == CBN_SELENDOK || code == CBN_SELCHANGE => self.combo_changed(app, id, code),
                    ID_SW..=225 if code == BN_CLICKED || code == BN_DOUBLECLICKED => self.swatch_clicked(h, (id - ID_SW) as usize, app),
                    ID_RESET => self.live(app, |n| n.colors = [FOLLOW; 6]),
                    ID_EXPAND | ID_SOUND => {
                        let (ex, snd) = (is_checked(self.expand.get()), is_checked(self.sound.get()));
                        self.live(app, |n| {
                            n.expand_selected = ex;
                            n.sound = snd;
                        });
                        app.settings.borrow().save();
                    }
                    ID_STARTUP => settings::set_startup(is_checked(self.startup.get())),
                    ID_DEFAULTS => self.defaults(app),
                    ID_CLEAR => self.clear_history(app, h),
                    _ => {}
                }
            }
            WM_HK_SET => {
                let mut hk = self.hk.get();
                if let Some(slot) = hk.get_mut(w.0) {
                    *slot = unpack(l.0);
                }
                self.hk.set(hk);
                self.refresh_hotkeys(app, hk);
            }
            WM_DRAWITEM => {
                // SAFETY: lParam is a DRAWITEMSTRUCT* for this message.
                self.draw_swatch(app, unsafe { &*(l.0 as *const DRAWITEMSTRUCT) });
                return Some(LRESULT(1));
            }
            WM_TIMER if w.0 == TIMER_SWATCH => {
                // SAFETY: one-shot timer.
                unsafe {
                    let _ = KillTimer(Some(h), TIMER_SWATCH);
                }
                if let Some(i) = self.pending.take() {
                    self.pick_color(h, i, app);
                }
            }
            WM_CTLCOLORSTATIC => {
                let ctl = HWND(l.0 as *mut _);
                let is_status = self.hk_status.iter().any(|s| s.get() == ctl);
                let is_field = self.hk_edit.iter().any(|e| e.get() == ctl);
                if is_status || is_field {
                    // SAFETY: wParam is the control's DC for this message.
                    unsafe {
                        let hdc = HDC(w.0 as *mut _);
                        SetBkMode(hdc, TRANSPARENT);
                        if is_status {
                            SetTextColor(hdc, COLORREF(0x0000_00C0)); // red
                        }
                        return Some(LRESULT(GetSysColorBrush(if is_field { COLOR_WINDOW } else { COLOR_3DFACE }).0 as isize));
                    }
                }
                return None;
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

/// The "clip4 Settings" dialog (spec 16.1).
pub fn settings_dialog(app: &App) {
    if OPEN.with(raise_if_open) {
        return;
    }
    let scr = screen_at_cursor();
    let dlg = Rc::new(SettingsDlg {
        ui: Ui::new(scr.scale),
        hk: Cell::new(app.settings.borrow().hotkeys),
        hk_edit: Default::default(),
        hk_status: Default::default(),
        theme: Cell::default(),
        font: Cell::default(),
        csize: Cell::default(),
        usize_: Cell::default(),
        swatch: Default::default(),
        custom_lbl: Cell::default(),
        custom_btn: Cell::default(),
        hist: Cell::default(),
        expand: Cell::default(),
        startup: Cell::default(),
        sound: Cell::default(),
        pending: Cell::new(None),
        reset_at: Cell::new(None),
        custom_colors: Cell::new([COLORREF(0x00FF_FFFF); 16]),
        done: Cell::new(false),
        saved: Cell::new(false),
    });
    let Some(h) = create(app, "clip4 Settings", (648, 542), false, &scr, None, dlg.clone()) else { return };
    OPEN.with(|o| o.set(h));
    dlg.build(h, app);
    present(h, dlg.hk_edit.first().map_or(h, Cell::get));
    run_modal(h, None, &dlg.done);
    OPEN.with(|o| o.set(HWND::default()));
    close(h);
    if dlg.saved.get() {
        let failed: Vec<&str> = app.hk_failed.borrow().iter().map(|a| a.title()).collect();
        if !failed.is_empty() {
            let text = format!("These hotkeys are already in use by another application and could not be registered:\n\n{}", failed.join("\n"));
            message_box(app.hwnd.get(), &text, "clip4", MB_OK | MB_ICONWARNING | MB_SETFOREGROUND);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: u32 = MOD_CONTROL;

    #[test]
    fn capture_rules() {
        assert_eq!(capture(0x11, CTRL), Capture::Ignore); // lone Ctrl
        assert_eq!(capture(0x10, MOD_SHIFT), Capture::Ignore);
        assert_eq!(capture(0x5B, MOD_WIN), Capture::Ignore);
        assert_eq!(capture(0x1B, 0), Capture::Clear);
        assert_eq!(capture(0x08, 0), Capture::Clear);
        assert_eq!(capture(0x41, 0), Capture::Ignore); // bare letter would break typing
        assert_eq!(capture(0x0D, 0), Capture::Ignore);
        assert_eq!(capture(0x7A, 0), Capture::Set(Hotkey { mods: 0, vk: 0x7A })); // bare F11
        assert_eq!(capture(0x6E, CTRL), Capture::Set(Hotkey { mods: CTRL, vk: 0x6E }));
        assert_eq!(capture(0x7A, CTRL | MOD_SHIFT), Capture::Set(Hotkey { mods: CTRL | MOD_SHIFT, vk: 0x7A }));
        assert_eq!(capture(0x08, CTRL), Capture::Set(Hotkey { mods: CTRL, vk: 0x08 }));
        assert_eq!(capture(0, CTRL), Capture::Ignore);
        assert_eq!(capture(0xFF, CTRL), Capture::Ignore);
    }

    #[test]
    fn pack_round_trip() {
        for a in ACTIONS {
            let h = a.default_hotkey();
            assert_eq!(unpack(pack(h)), h);
        }
        assert_eq!(unpack(pack(Hotkey::NONE)), Hotkey::NONE);
        let all = Hotkey { mods: MOD_CONTROL | MOD_SHIFT | MOD_ALT | MOD_WIN, vk: 0xFE };
        assert_eq!(unpack(pack(all)), all);
    }

    #[test]
    fn hotkey_text() {
        assert_eq!(Hotkey { mods: CTRL | MOD_SHIFT, vk: 0x7A }.describe(), "Ctrl+Shift+F11");
        assert_eq!(Hotkey::NONE.describe(), "(none)");
    }

    #[test]
    fn history_size_validation() {
        assert_eq!(parse_history("300"), Ok(300));
        assert_eq!(parse_history(" 10 "), Ok(10));
        assert_eq!(parse_history("2000"), Ok(2000));
        for bad in ["9", "2001", "", "abc", "-5", "1e3", "12.5"] {
            assert!(parse_history(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn duplicate_hotkeys_are_found_but_unbound_ones_are_not() {
        let mut hk = Settings::default().hotkeys;
        assert_eq!(find_conflict(&hk), None);
        hk[Action::KeystrokePaste as usize] = hk[Action::CopyFocused as usize];
        assert_eq!(find_conflict(&hk), Some((Action::CopyFocused, Action::KeystrokePaste)));
        let mut none = [Hotkey::NONE; 5];
        assert_eq!(find_conflict(&none), None);
        none[1] = Hotkey { mods: CTRL, vk: 0x41 };
        assert_eq!(find_conflict(&none), None);
    }

    #[test]
    fn swatches_show_theme_colours_until_overridden() {
        let mut s = Settings::default();
        let base = effective_colors(&s);
        assert_eq!(base[0], Rgb::BLACK);
        assert_eq!(base[2], PRESETS[0].accent);
        s.theme_id = 3;
        assert_eq!(effective_colors(&s)[2], PRESETS[3].accent);
        s.colors[2] = Rgb::new(1, 2, 3).colorref();
        s.colors[1] = Rgb::new(9, 8, 7).colorref();
        let e = effective_colors(&s);
        assert_eq!((e[2], e[1]), (Rgb::new(1, 2, 3), Rgb::new(9, 8, 7)));
        s.colors = [FOLLOW; 6];
        assert_eq!(effective_colors(&s)[2], PRESETS[3].accent);
    }
}
