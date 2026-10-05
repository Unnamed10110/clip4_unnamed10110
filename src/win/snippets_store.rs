//! Snippets in `HKCU\Software\clip4\Snippets`: `Count` (DWORD) and `N0`, `N1`, ... (REG_SZ).
use super::reg::Key;
use super::settings::snippets_path;
use crate::snippets_fmt::{decode, encode, validate, Snippet};

pub fn load() -> Vec<Snippet> {
    let Some(k) = Key::open(&snippets_path(), false) else { return Vec::new() };
    let n = k.dword("Count").unwrap_or(0).min(10_000);
    (0..n).filter_map(|i| k.string(&format!("N{i}")).and_then(|s| decode(&s))).collect()
}

/// Writes the whole list. Entries that fail validation (too long, reserved name) are skipped.
pub fn save(list: &[Snippet]) -> bool {
    let Some(k) = Key::create(&snippets_path()) else { return false };
    let old = k.dword("Count").unwrap_or(0);
    let good: Vec<&Snippet> = list.iter().filter(|s| validate(s).is_ok()).collect();
    for (i, s) in good.iter().enumerate() {
        k.set_string(&format!("N{i}"), &encode(s));
    }
    for i in good.len() as u32..old {
        k.delete_value(&format!("N{i}"));
    }
    k.set_dword("Count", good.len() as u32)
}
