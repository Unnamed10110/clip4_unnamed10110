//! SHA-1 through CNG (BCryptHash); no hashing crate (spec 4).
use windows::core::PCWSTR;
use windows::Win32::Security::Cryptography::{
    BCryptCloseAlgorithmProvider, BCryptHash, BCryptOpenAlgorithmProvider, BCRYPT_ALG_HANDLE,
    BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS,
};

pub fn sha1(data: &[u8]) -> Option<[u8; 20]> {
    let mut alg = BCRYPT_ALG_HANDLE::default();
    let name: Vec<u16> = "SHA1\0".encode_utf16().collect();
    // SAFETY: opens a provider for the duration of one hash; closed on every path.
    unsafe {
        if BCryptOpenAlgorithmProvider(&mut alg, PCWSTR(name.as_ptr()), PCWSTR::null(), BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0)).is_err() {
            return None;
        }
        let mut out = [0u8; 20];
        let ok = BCryptHash(alg, None, data, &mut out).is_ok();
        let _ = BCryptCloseAlgorithmProvider(alg, 0);
        ok.then_some(out)
    }
}

#[cfg(test)]
mod tests {
    use crate::win::util::hex;
    #[test]
    fn known_vectors() {
        assert_eq!(hex(&super::sha1(b"abc").expect("sha1")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(&super::sha1(b"").expect("sha1")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }
}
