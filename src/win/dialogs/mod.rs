//! Modal dialogs, built from plain Win32 windows and controls (no resource templates):
//! `edit_text`, `snippet_editor`, `manage_snippets` and `settings_dialog`.
//! `modal` holds the shared plumbing (window class, DPI metrics, nested modal loop).

mod editor;
mod modal;
mod settings_dlg;
mod snippet_dlg;

pub use editor::edit_text;
pub use settings_dlg::settings_dialog;
pub use snippet_dlg::{manage_snippets, snippet_editor};
