//! DPAPI (CryptProtectData), current-user scope.
use super::util::{pcw, wide};
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

fn take_blob(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if out.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: DPAPI returns a LocalAlloc'd buffer of cbData bytes.
    let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    // SAFETY: buffer was allocated by DPAPI with LocalAlloc.
    unsafe { LocalFree(Some(HLOCAL(out.pbData as *mut _))) };
    v
}

pub fn protect(data: &[u8], description: &str) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB { cbData: u32::try_from(data.len()).ok()?, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB::default();
    let d = wide(description);
    // SAFETY: input/out blobs are valid for the call.
    unsafe { CryptProtectData(&input, pcw(&d), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out) }.ok()?;
    Some(take_blob(out))
}

pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB { cbData: u32::try_from(data.len()).ok()?, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as above.
    unsafe { CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out) }.ok()?;
    Some(take_blob(out))
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let c = super::protect(b"hello clip4", "clip4 history").expect("protect");
        assert_ne!(c, b"hello clip4");
        assert_eq!(super::unprotect(&c).expect("unprotect"), b"hello clip4");
        assert!(super::unprotect(b"garbage").is_none());
    }
}
