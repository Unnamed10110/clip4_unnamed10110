//! Persistent settings in `HKCU\Software\clip4` (spec 16.4). All reads validate and clamp.
use super::reg::{self, Key};
use super::util::from_wide;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

pub const KEY_PATH: &str = "Software\\clip4";
pub const SNIPPETS_PATH: &str = "Software\\clip4\\Snippets";

/// Registry key honouring the `CLIP4_PROFILE` sandbox.
pub fn key_path() -> String {
    match super::util::profile() {
        Some(p) => format!("Software\\clip4-{p}"),
        None => KEY_PATH.to_string(),
    }
}

pub fn snippets_path() -> String {
    format!("{}\\Snippets", key_path())
}
const CLIP2_PATH: &str = "Software\\clip2";
const RUN_PATH: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

pub const MOD_ALT: u32 = 1;
pub const MOD_CONTROL: u32 = 2;
pub const MOD_SHIFT: u32 = 4;
pub const MOD_WIN: u32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: u32,
    /// 0 = unbound.
    pub vk: u32,
}

impl Hotkey {
    pub const NONE: Hotkey = Hotkey { mods: 0, vk: 0 };
    pub fn is_bound(&self) -> bool {
        self.vk != 0
    }
    pub fn describe(&self) -> String {
        if !self.is_bound() {
            return "(none)".into();
        }
        let mut s = String::new();
        for (m, n) in [(MOD_CONTROL, "Ctrl+"), (MOD_SHIFT, "Shift+"), (MOD_ALT, "Alt+"), (MOD_WIN, "Win+")] {
            if self.mods & m != 0 {
                s.push_str(n);
            }
        }
        s.push_str(&vk_name(self.vk));
        s
    }
}

pub fn vk_name(vk: u32) -> String {
    match vk {
        0x6E => return "NumPad .".into(),
        0x60..=0x69 => return format!("NumPad {}", vk - 0x60),
        0x6A => return "NumPad *".into(),
        0x6B => return "NumPad +".into(),
        0x6D => return "NumPad -".into(),
        0x6F => return "NumPad /".into(),
        0x70..=0x87 => return format!("F{}", vk - 0x6F),
        _ => {}
    }
    // SAFETY: plain key-name lookup into a local buffer.
    unsafe {
        let sc = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
        let ext = matches!(VIRTUAL_KEY(vk as u16), VK_INSERT | VK_DELETE | VK_HOME | VK_END | VK_PRIOR | VK_NEXT | VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN);
        let lparam = ((sc << 16) | if ext { 1 << 24 } else { 0 }) as i32;
        let mut buf = [0u16; 64];
        let n = windows::Win32::UI::Input::KeyboardAndMouse::GetKeyNameTextW(lparam, &mut buf);
        if n > 0 {
            return from_wide(&buf[..n as usize]);
        }
    }
    format!("VK {vk:#04x}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Overlay = 0,
    Snippets = 1,
    CopyFocused = 2,
    KeystrokePaste = 3,
    ClipboardPaste = 4,
}

pub const ACTIONS: [Action; 5] =
    [Action::Overlay, Action::Snippets, Action::CopyFocused, Action::KeystrokePaste, Action::ClipboardPaste];

impl Action {
    pub fn reg_name(self) -> &'static str {
        match self {
            Action::Overlay => "Hotkey",
            Action::Snippets => "Snippets",
            Action::CopyFocused => "CopyFocused",
            Action::KeystrokePaste => "PasteFocused",
            Action::ClipboardPaste => "PasteClipboard",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Action::Overlay => "Toggle clipboard overlay",
            Action::Snippets => "Toggle snippets overlay",
            Action::CopyFocused => "Copy from focused control",
            Action::KeystrokePaste => "Paste as keystrokes",
            Action::ClipboardPaste => "Paste via clipboard + Ctrl+V",
        }
    }
    pub fn from_index(i: usize) -> Option<Action> {
        ACTIONS.get(i).copied()
    }
    pub fn default_hotkey(self) -> Hotkey {
        match self {
            Action::Overlay => Hotkey { mods: MOD_CONTROL, vk: VK_DECIMAL.0 as u32 },
            Action::Snippets => Hotkey::NONE,
            Action::CopyFocused => Hotkey { mods: MOD_CONTROL, vk: VK_F10.0 as u32 },
            Action::KeystrokePaste => Hotkey { mods: MOD_CONTROL, vk: VK_F11.0 as u32 },
            Action::ClipboardPaste => Hotkey { mods: MOD_CONTROL | MOD_SHIFT, vk: VK_F11.0 as u32 },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub hotkeys: [Hotkey; 5],
    pub theme_id: usize,
    pub font_face: String,
    pub content_size: u32,
    pub ui_size: u32,
    /// Registry values: Background, Text, Accent(SELBG), SelectedText(SELFG), Border, Dim.
    pub colors: [u32; 6],
    pub max_items: usize,
    pub expand_selected: bool,
    pub overlay_pos: Option<(i32, i32)>,
    /// Hash of the monitor layout `overlay_pos` was saved under.
    pub overlay_pos_cfg: u32,
    /// Overlay size in LOGICAL (96-dpi) px; the user resizes it by dragging the edges.
    pub pinned_w: u32,
    pub main_w: u32,
    pub pane_h: u32,
    pub debug_log: bool,
    pub sound: bool,
}

pub const FOLLOW: u32 = 0xFF00_0000;

// Overlay size limits (logical px) and defaults (spec 10.1).
pub const PINNED_W_RANGE: (u32, u32) = (200, 1200);
pub const MAIN_W_RANGE: (u32, u32) = (360, 2400);
pub const PANE_H_RANGE: (u32, u32) = (260, 2000);
pub const PINNED_W_DEFAULT: u32 = 320;
pub const MAIN_W_DEFAULT: u32 = 640;
pub const PANE_H_DEFAULT: u32 = 520;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hotkeys: ACTIONS.map(|a| a.default_hotkey()),
            theme_id: 0,
            font_face: "Consolas".into(),
            content_size: 14,
            ui_size: 16,
            colors: [FOLLOW; 6],
            max_items: 300,
            expand_selected: true,
            overlay_pos: None,
            overlay_pos_cfg: 0,
            pinned_w: PINNED_W_DEFAULT,
            main_w: MAIN_W_DEFAULT,
            pane_h: PANE_H_DEFAULT,
            debug_log: false,
            sound: true,
        }
    }
}

const COLOR_NAMES: [&str; 6] = ["ColorBG", "ColorTXT", "ColorSELBG", "ColorSELFG", "ColorBORDER", "ColorDIM"];

impl Settings {
    pub fn hotkey(&self, a: Action) -> Hotkey {
        self.hotkeys[a as usize]
    }

    /// Startup load: a first run imports clip2's registry settings and snippets (spec 16.6).
    pub fn load_with_import() -> Settings {
        import_clip2();
        Settings::load()
    }

    pub fn load() -> Settings {
        let mut s = Settings::default();
        let Some(k) = Key::open(&key_path(), false) else { return s };
        for a in ACTIONS {
            let (m, v) = (k.dword(&format!("{}Modifiers", a.reg_name())), k.dword(&format!("{}VkCode", a.reg_name())));
            if let (Some(m), Some(v)) = (m, v) {
                if m <= 0x0F && v <= 0xFE {
                    s.hotkeys[a as usize] = Hotkey { mods: m, vk: v };
                }
            }
        }
        if let Some(v) = k.dword("ThemeId") {
            s.theme_id = if (v as usize) < 15 { v as usize } else { 0 };
        }
        if let Some(f) = k.string("ThemeFontFace") {
            let f = f.trim();
            if !f.is_empty() && f.len() < 64 {
                s.font_face = f.to_string();
            }
        }
        if let Some(v) = k.dword("ThemeFontSize") {
            s.content_size = v.clamp(10, 24);
        }
        if let Some(v) = k.dword("UiFontSize") {
            s.ui_size = v.clamp(10, 28);
        }
        for (i, n) in COLOR_NAMES.iter().enumerate() {
            if let Some(v) = k.dword(n) {
                s.colors[i] = if v >> 24 == 0 { v } else { FOLLOW };
            }
        }
        if let Some(v) = k.dword("MaxItems") {
            s.max_items = (v as usize).clamp(10, 2000);
        }
        if let Some(v) = k.dword("ExpandSelected") {
            s.expand_selected = v != 0;
        }
        if let (Some(x), Some(y)) = (k.dword("OverlayPosX"), k.dword("OverlayPosY")) {
            let (x, y) = (x as i32, y as i32);
            if (-32000..=32000).contains(&x) && (-32000..=32000).contains(&y) {
                s.overlay_pos = Some((x, y));
            }
        }
        s.overlay_pos_cfg = k.dword("OverlayPosCfg").unwrap_or(0);
        if let Some(v) = k.dword("OverlayPinnedWidth") {
            s.pinned_w = v.clamp(PINNED_W_RANGE.0, PINNED_W_RANGE.1);
        }
        if let Some(v) = k.dword("OverlayMainWidth") {
            s.main_w = v.clamp(MAIN_W_RANGE.0, MAIN_W_RANGE.1);
        }
        if let Some(v) = k.dword("OverlayHeight") {
            s.pane_h = v.clamp(PANE_H_RANGE.0, PANE_H_RANGE.1);
        }
        s.debug_log = k.dword("DebugLog").is_some_and(|v| v != 0);
        s.sound = k.dword("Sound").is_none_or(|v| v != 0);
        s
    }

    pub fn save(&self) {
        let Some(k) = Key::create(&key_path()) else { return };
        for a in ACTIONS {
            let h = self.hotkey(a);
            k.set_dword(&format!("{}Modifiers", a.reg_name()), h.mods);
            k.set_dword(&format!("{}VkCode", a.reg_name()), h.vk);
        }
        self.save_look(&k);
        k.set_dword("MaxItems", self.max_items as u32);
        k.set_dword("ExpandSelected", self.expand_selected as u32);
        k.set_dword("Sound", self.sound as u32);
        k.set_dword("DebugLog", self.debug_log as u32);
        self.save_pos(&k);
    }

    /// Theme/font/size/colours only (applied live, so persisted immediately).
    pub fn save_look_only(&self) {
        if let Some(k) = Key::create(&key_path()) {
            self.save_look(&k);
        }
    }

    pub fn save_pos_only(&self) {
        if let Some(k) = Key::create(&key_path()) {
            self.save_pos(&k);
        }
    }

    fn save_look(&self, k: &Key) {
        k.set_dword("ThemeId", self.theme_id as u32);
        k.set_string("ThemeFontFace", &self.font_face);
        k.set_dword("ThemeFontSize", self.content_size);
        k.set_dword("UiFontSize", self.ui_size);
        for (i, n) in COLOR_NAMES.iter().enumerate() {
            k.set_dword(n, self.colors[i]);
        }
    }

    fn save_pos(&self, k: &Key) {
        k.set_dword("OverlayPinnedWidth", self.pinned_w);
        k.set_dword("OverlayMainWidth", self.main_w);
        k.set_dword("OverlayHeight", self.pane_h);
        if let Some((x, y)) = self.overlay_pos {
            k.set_dword("OverlayPosX", x as u32);
            k.set_dword("OverlayPosY", y as u32);
            k.set_dword("OverlayPosCfg", self.overlay_pos_cfg);
        }
    }
}

// ---- Start with Windows (spec 16.5) ----

pub fn startup_enabled() -> bool {
    Key::open(RUN_PATH, false).is_some_and(|k| k.string("clip4").is_some())
}

pub fn set_startup(on: bool) {
    let Some(k) = Key::create(RUN_PATH) else { return };
    if on {
        if let Ok(exe) = std::env::current_exe() {
            k.set_string("clip4", &format!("\"{}\"", exe.display()));
        }
    } else {
        k.delete_value("clip4");
    }
}

// ---- clip2 import (spec 16.6) ----

/// First run only: copies clip2's registry values (same names) and its Snippets subkey.
/// Returns true when something was imported.
pub fn import_clip2() -> bool {
    if super::util::profile().is_some() {
        return false;
    }
    if reg::exists(&key_path()) {
        return false;
    }
    let Some(old) = Key::open(CLIP2_PATH, false) else { return false };
    let Some(new) = Key::create(&key_path()) else { return false };
    for n in old.value_names() {
        if let Some(v) = old.dword(&n) {
            new.set_dword(&n, v);
        } else if let Some(s) = old.string(&n) {
            new.set_string(&n, &s);
        }
    }
    if let Some(os) = Key::open(&format!("{CLIP2_PATH}\\Snippets"), false) {
        if let Some(ns) = Key::create(&snippets_path()) {
            for n in os.value_names() {
                if let Some(v) = os.dword(&n) {
                    ns.set_dword(&n, v);
                } else if let Some(s) = os.string(&n) {
                    ns.set_string(&n, &s);
                }
            }
        }
    }
    true
}
