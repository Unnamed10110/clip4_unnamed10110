//! `cargo +nightly fuzz run history_parser` — the CLP4 inner parser must never panic.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = clip4::codec::unwrap_file(data);
    let _ = clip4::codec::decode_inner(data, &clip4::model::NoBlobs);
});
