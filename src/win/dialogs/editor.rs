//! `edit_text`: the modal multi-line editor used by "Edit & paste" / "Edit & save as new".

use super::modal::*;
use crate::win::app::App;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Every line break (`\r\n`, `\r`, `\n`) as `\r\n`, which is what an EDIT control needs.
pub fn to_crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\r\n")
}

const ID_EDIT: i32 = 100;

struct EditDlg {
    ui: Ui,
    text_font: Font,
    edit: Cell<HWND>,
    hint: Cell<HWND>,
    ok: Cell<HWND>,
    cancel: Cell<HWND>,
    done: Cell<bool>,
    result: RefCell<Option<String>>,
}

impl EditDlg {
    fn build(&self, h: HWND, initial: &str) {
        let ui = &self.ui;
        let style = TAB | (WS_VSCROLL.0) | (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32;
        let edit = ui.add(h, w!("EDIT"), &to_crlf(initial), style, WS_EX_CLIENTEDGE.0, (12, 12, 400, 280), ID_EDIT);
        set_font(edit, self.text_font.0);
        send(edit, EM_SETLIMITTEXT, 0, 0); // 0 = the maximum
        subclass_keys(edit, None);
        self.hint.set(ui.label(h, "Ctrl+Enter: OK    Esc: cancel", (12, 300, 200, 18)));
        self.ok.set(ui.button(h, "OK", (0, 0, 92, 28), ID_OK, true));
        self.cancel.set(ui.button(h, "Cancel", (0, 0, 92, 28), ID_CANCEL, false));
        self.edit.set(edit);
        self.layout(h);
        let n = get_text(edit).encode_utf16().count();
        send(edit, EM_SETSEL, n, n as isize); // caret at the end
    }

    fn layout(&self, h: HWND) {
        let (w, ch) = client_size(h);
        let (m, bh, bw, gap) = (self.ui.px(12), self.ui.px(28), self.ui.px(92), self.ui.px(8));
        let by = ch - m - bh;
        put(self.edit.get(), m, m, w - 2 * m, by - 2 * m);
        put(self.ok.get(), w - m - 2 * bw - gap, by, bw, bh);
        put(self.cancel.get(), w - m - bw, by, bw, bh);
        put(self.hint.get(), m, by + self.ui.px(5), w - 2 * m - 2 * bw - 2 * gap, self.ui.px(18));
    }
}

impl Dlg for EditDlg {
    fn msg(&self, _app: &App, h: HWND, m: u32, w: WPARAM, _l: LPARAM) -> Option<LRESULT> {
        match m {
            WM_SIZE => {
                self.layout(h);
                Some(LRESULT(0))
            }
            WM_COMMAND => {
                match loword(w) {
                    ID_OK => {
                        // An empty result is treated as "nothing to use" (cancelled).
                        let text = get_text(self.edit.get());
                        *self.result.borrow_mut() = (!text.is_empty()).then_some(text);
                        finish(h, &self.done);
                    }
                    ID_CANCEL => finish(h, &self.done),
                    _ => {}
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

/// Modal multi-line editor. Ctrl+Enter / OK confirm, Esc / Cancel / close discard.
/// The text comes back with CRLF line breaks, exactly as typed (not trimmed).
pub fn edit_text(app: &App, title: &str, initial: &str) -> Option<String> {
    let scr = screen_at_cursor();
    let (face, size) = {
        let s = app.settings.borrow();
        (s.font_face.clone(), s.content_size)
    };
    let dlg = Rc::new(EditDlg {
        ui: Ui::new(scr.scale),
        text_font: Font::new(&face, (size as f32 * scr.scale).round() as i32, 400, false, false),
        edit: Cell::default(),
        hint: Cell::default(),
        ok: Cell::default(),
        cancel: Cell::default(),
        done: Cell::new(false),
        result: RefCell::new(None),
    });
    let h = create(app, title, (640, 360), true, &scr, None, dlg.clone())?;
    dlg.build(h, initial);
    present(h, dlg.edit.get());
    run_modal(h, None, &dlg.done);
    close(h);
    let r = dlg.result.borrow_mut().take();
    r
}

#[cfg(test)]
mod tests {
    use super::to_crlf;

    #[test]
    fn crlf_normalisation() {
        assert_eq!(to_crlf("a\nb"), "a\r\nb");
        assert_eq!(to_crlf("a\r\nb"), "a\r\nb");
        assert_eq!(to_crlf("a\rb\n\nc"), "a\r\nb\r\n\r\nc");
        assert_eq!(to_crlf(""), "");
        assert_eq!(to_crlf("\n"), "\r\n");
    }
}
