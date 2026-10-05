//! clip4 — Windows clipboard manager. See CLIP4_SPEC.md.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

// ---- pure logic (no windows state; unit-testable) ----
pub mod codec;
pub mod layout;
pub mod model;
pub mod preview;
pub mod search;
pub mod snippets_fmt;
pub mod store;
pub mod theme;
pub mod transform;

/// "clip4 <version>": logged at startup, and what the build scripts look for in the binary to be
/// sure it was built from the version in Cargo.toml.
pub const VERSION_STRING: &str = concat!("clip4 ", env!("CARGO_PKG_VERSION"));

// ---- Win32 layer and application ----
pub mod win;
