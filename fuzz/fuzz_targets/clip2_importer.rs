//! `cargo +nightly fuzz run clip2_importer` — the clip2 importer must never panic.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = clip4::codec::clip2_classify(data);
    let _ = clip4::codec::clip2_import_inner(data, &clip4::model::NoBlobs, 0);
});
