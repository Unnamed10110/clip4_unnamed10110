//! Thin registry wrapper (HKCU only). Every read validates; callers clamp.
use super::util::{from_wide, pcw, wide};
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::*;

pub struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: handle came from RegOpenKeyEx/RegCreateKeyEx.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

impl Key {
    pub fn open(path: &str, write: bool) -> Option<Key> {
        let p = wide(path);
        let mut h = HKEY::default();
        let sam = if write { KEY_READ | KEY_WRITE } else { KEY_READ };
        // SAFETY: valid NUL-terminated path, out handle.
        let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, pcw(&p), None, sam, &mut h) };
        (r == ERROR_SUCCESS).then_some(Key(h))
    }

    pub fn create(path: &str) -> Option<Key> {
        let p = wide(path);
        let mut h = HKEY::default();
        // SAFETY: as above.
        let r = unsafe {
            RegCreateKeyExW(HKEY_CURRENT_USER, pcw(&p), None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_READ | KEY_WRITE, None, &mut h, None)
        };
        (r == ERROR_SUCCESS).then_some(Key(h))
    }

    pub fn dword(&self, name: &str) -> Option<u32> {
        let n = wide(name);
        let mut ty = REG_VALUE_TYPE(0);
        let mut data = [0u8; 4];
        let mut len = 4u32;
        // SAFETY: buffers sized as declared.
        let r = unsafe { RegQueryValueExW(self.0, pcw(&n), None, Some(&mut ty), Some(data.as_mut_ptr()), Some(&mut len)) };
        (r == ERROR_SUCCESS && ty == REG_DWORD && len == 4).then(|| u32::from_le_bytes(data))
    }

    pub fn set_dword(&self, name: &str, v: u32) -> bool {
        let n = wide(name);
        // SAFETY: valid buffer.
        unsafe { RegSetValueExW(self.0, pcw(&n), None, REG_DWORD, Some(&v.to_le_bytes())) == ERROR_SUCCESS }
    }

    pub fn string(&self, name: &str) -> Option<String> {
        let n = wide(name);
        let mut ty = REG_VALUE_TYPE(0);
        let mut len = 0u32;
        // SAFETY: size query then read; buffer re-checked against the returned length.
        unsafe {
            if RegQueryValueExW(self.0, pcw(&n), None, Some(&mut ty), None, Some(&mut len)) != ERROR_SUCCESS
                || (ty != REG_SZ && ty != REG_EXPAND_SZ)
                || len > 4 * 1024 * 1024
            {
                return None;
            }
            let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
            let mut len2 = (buf.len() * 2) as u32;
            if RegQueryValueExW(self.0, pcw(&n), None, Some(&mut ty), Some(buf.as_mut_ptr() as *mut u8), Some(&mut len2)) != ERROR_SUCCESS {
                return None;
            }
            Some(from_wide(&buf))
        }
    }

    pub fn set_string(&self, name: &str, v: &str) -> bool {
        let n = wide(name);
        let w = wide(v);
        // SAFETY: w is a valid u16 buffer; reinterpret as bytes for the call.
        let bytes = unsafe { std::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * 2) };
        // SAFETY: valid buffers.
        unsafe { RegSetValueExW(self.0, pcw(&n), None, REG_SZ, Some(bytes)) == ERROR_SUCCESS }
    }

    pub fn delete_value(&self, name: &str) {
        let n = wide(name);
        // SAFETY: valid name.
        unsafe {
            let _ = RegDeleteValueW(self.0, pcw(&n));
        }
    }

    /// Names of all values under this key.
    pub fn value_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0..4096u32 {
            let mut buf = [0u16; 260];
            let mut len = buf.len() as u32;
            // SAFETY: buffer/len are consistent.
            let r = unsafe { RegEnumValueW(self.0, i, Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) };
            if r != ERROR_SUCCESS {
                break;
            }
            out.push(from_wide(&buf[..len as usize]));
        }
        out
    }
}

pub fn delete_tree(path: &str) {
    let p = wide(path);
    // SAFETY: valid path.
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, pcw(&p));
    }
}

pub fn exists(path: &str) -> bool {
    Key::open(path, false).is_some()
}
