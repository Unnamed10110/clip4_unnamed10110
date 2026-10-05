//! Validates the clip2 importer against the clip2 history on THIS machine, if any.
//! Prints counts and format names only — never content. Run with `-- --ignored --nocapture`.
#![cfg(windows)]

use clip4::codec::{clip2_classify, clip2_import_inner, Clip2File};
use clip4::win::blob::Clip2Blobs;

#[test]
#[ignore = "reads the real clip2 history of the current user"]
fn real_clip2_history_imports() {
    let dir = std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("clip2");
    let Ok(file) = std::fs::read(dir.join("history.dat")) else {
        eprintln!("no clip2 history here; nothing to check");
        return;
    };
    let owned;
    let inner: &[u8] = match clip2_classify(&file) {
        Clip2File::Dpapi(b) => {
            owned = clip4::win::dpapi::unprotect(b).expect("clip2 DPAPI blob should decrypt for the same user");
            &owned
        }
        Clip2File::Plain(b) => b,
        Clip2File::Invalid => panic!("unrecognised clip2 file"),
    };
    let items = clip2_import_inner(inner, &Clip2Blobs(dir.join("blobs")), 1_700_000_000_000);
    let mut kinds = std::collections::BTreeMap::<String, usize>::new();
    for it in &items {
        *kinds.entry(format!("{:?}", it.kind)).or_default() += 1;
    }
    let with_preview = items.iter().filter(|i| !i.preview.is_empty()).count();
    let formats: usize = items.iter().map(|i| i.formats.len()).sum();
    eprintln!("clip2 import: {} items, {} formats, {} with preview, kinds {:?}", items.len(), formats, with_preview, kinds);
    assert!(!items.is_empty());
    assert!(items.iter().all(|i| !i.formats.is_empty()));
}
