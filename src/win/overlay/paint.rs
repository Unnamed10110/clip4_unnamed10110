//! Painting and hit-testing. Both derive every rectangle from the SAME functions
//! (`geo`, `layout::layout`, `card_buttons`) — spec 10.9 / lesson 18.17.

use super::*;
use crate::layout;
use crate::win::gfx::{contains, inset, rect, Align, Font, Gfx, RectF};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};

/// Pane geometry shared by paint, hit-test and the EDIT-control placement.
pub struct Geo {
    pub w: f32,
    pub h: f32,
    pub pill: RectF,
    pub seg_all: Option<RectF>,
    pub seg_snip: Option<RectF>,
    pub seg_box: Option<RectF>,
    pub edit_rect: RECT,
    pub list_top: f32,
    pub list_bottom: f32,
    pub viewport_h: f32,
}

pub fn seg_labels(st: &State) -> (String, String) {
    (format!("All {}", st.counts.0), format!("Snippets {}", st.counts.1))
}

pub fn geo(st: &State, pid: PaneId, gf: &Gfx) -> Geo {
    let m = &st.metrics;
    let (pw, ph) = st.pane_px(pid);
    let (w, h) = (pw as f32, ph as f32);
    let s = m.scale;
    let top = m.search_top;
    let (mut right, mut seg_all, mut seg_snip, mut seg_box) = (w - m.side_inset, None, None, None);
    if pid == PaneId::Main {
        let (la, ls) = seg_labels(st);
        let pad = 12.0 * s;
        let (wa, ws) = (gf.text_width(&la, Font::Ui) + 2.0 * pad, gf.text_width(&ls, Font::Ui) + 2.0 * pad);
        let x1 = w - m.side_inset;
        let x0 = x1 - (wa + ws);
        seg_box = Some(rect(x0, top, x1, top + m.search_h));
        seg_all = Some(rect(x0, top, x0 + wa, top + m.search_h));
        seg_snip = Some(rect(x0 + wa, top, x1, top + m.search_h));
        right = x0 - 8.0 * s;
    }
    // Narrow panes (user-resized) must never squeeze the search pill to nothing.
    let right = right.max(m.side_inset + 80.0 * s);
    let pill = rect(m.side_inset, top, right, top + m.search_h);
    let line = gf.line_height(Font::Ui).max(m.ui);
    let ey = pill.top + (m.search_h - line) / 2.0;
    let edit_rect = RECT { left: (pill.left + 36.0 * s) as i32, top: ey as i32, right: (pill.right - 10.0 * s) as i32, bottom: (ey + line) as i32 };
    Geo {
        w,
        h,
        pill,
        seg_all,
        seg_snip,
        seg_box,
        edit_rect,
        list_top: m.header_h,
        list_bottom: h - m.footer_h,
        viewport_h: (h - m.header_h - m.footer_h).max(1.0),
    }
}

// ---------------- card buttons ----------------

pub fn btn_parts(b: Btn, multi: bool) -> (&'static str, &'static str) {
    match b {
        Btn::Paste => ("↵", "Paste"),
        Btn::CleanUrl => ("U", "Clean URL"),
        Btn::Plain => ("P", "Plain"),
        Btn::Edit => ("E", "Edit"),
        Btn::Merge => ("M", if multi { "Merge" } else { "Link" }),
    }
}

pub fn card_buttons(st: &State, gf: &Gfx, card: RectF, lines: u32, c: &CardCache, multi: bool) -> Vec<(Btn, RectF)> {
    let m = &st.metrics;
    let parts = m.card_parts(lines);
    let mut want = vec![Btn::Paste];
    if c.is_url {
        want.push(Btn::CleanUrl);
    }
    if c.has_text {
        want.extend([Btn::Plain, Btn::Edit, Btn::Merge]);
    }
    let pad_x = 12.0 * m.scale;
    let mut x = card.left + parts.pad;
    let y = card.top + parts.buttons_y;
    let mut out = Vec::new();
    for b in want {
        let (k, l) = btn_parts(b, multi);
        let wdt = gf.text_width(k, Font::UiSemibold) + gf.text_width(l, Font::Ui) + 6.0 * m.scale + 2.0 * pad_x;
        out.push((b, rect(x, y, x + wdt, y + parts.buttons_h)));
        x += wdt + 8.0 * m.scale;
    }
    out
}

// ---------------- hit testing ----------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Row(usize),
    CardBtn(usize, Btn),
    SegAll,
    SegSnip,
    Header,
    Footer,
    None,
}

pub fn bands_for(st: &State, pid: PaneId) -> Vec<layout::Band> {
    let lines = st.card_lines_of(pid);
    let cl = move |_: usize| lines;
    let inp = st.layout_input(pid, &cl);
    layout::layout(&inp, st.panes[pid as usize].scroll, st.viewport_h())
}

pub fn hit(st: &State, pid: PaneId, gf: &Gfx, x: f32, y: f32) -> Hit {
    let g = geo(st, pid, gf);
    if y < g.list_top {
        if pid == PaneId::Main {
            if g.seg_all.as_ref().is_some_and(|r| contains(r, x, y)) {
                return Hit::SegAll;
            }
            if g.seg_snip.as_ref().is_some_and(|r| contains(r, x, y)) {
                return Hit::SegSnip;
            }
        }
        return Hit::Header;
    }
    if y >= g.list_bottom {
        return Hit::Footer;
    }
    let bands = bands_for(st, pid);
    let ly = y - g.list_top;
    match layout::hit_test(&bands, ly) {
        Some(item) => {
            if let Some(b) = bands.iter().find(|b| b.item == item && b.expanded) {
                let p = &st.panes[pid as usize];
                if let Some(c) = p.card.as_ref() {
                    let card = rect(st.metrics.side_inset, g.list_top + b.top, g.w - st.metrics.side_inset, g.list_top + b.top + b.height);
                    let multi = p.multi.len() >= 2;
                    for (btn, r) in card_buttons(st, gf, card, b.card_lines, c, multi) {
                        if contains(&r, x, y) {
                            return Hit::CardBtn(item, btn);
                        }
                    }
                }
            }
            Hit::Row(item)
        }
        None => Hit::None,
    }
}

// ---------------- painting ----------------

pub struct ThumbReq {
    pub id: u64,
    pub key: FormatKey,
    pub payload: Payload,
    pub max_px: u32,
}

impl Overlay {
    fn paint_into(&self, app: &App, pid: PaneId, hdc: HDC) -> Vec<ThumbReq> {
        let store = app.store.borrow();
        let snippets = app.snippets.borrow();
        if let (Ok(mut st), Ok(mut slot)) = (self.st.try_borrow_mut(), self.gfx[pid as usize].try_borrow_mut()) {
            if let Some(gf) = slot.as_mut() {
                let (w, h) = st.pane_px(pid);
                return draw(&mut st, &store, &snippets, gf, pid, hdc, w, h);
            }
        }
        Vec::new()
    }

    /// Renders both panes once into an off-screen DC so the first real show is not slowed by
    /// Direct2D device / DirectWrite font-cache creation (hotkey -> first frame budget, spec 3).
    pub(super) fn prewarm(&self, app: &App) {
        use windows::Win32::Graphics::Gdi::*;
        for pid in [PaneId::Main, PaneId::Pinned] {
            let (w, h) = self.st.borrow().pane_px(pid);
            // SAFETY: a throw-away memory DC + bitmap, all released below.
            unsafe {
                let screen = GetDC(None);
                let mem = CreateCompatibleDC(Some(screen));
                let bmp = CreateCompatibleBitmap(screen, w, h);
                let old = SelectObject(mem, bmp.into());
                let _ = self.paint_into(app, pid, mem);
                SelectObject(mem, old);
                let _ = DeleteObject(bmp.into());
                let _ = DeleteDC(mem);
                ReleaseDC(None, screen);
            }
            if let Some(gf) = self.gfx[pid as usize].borrow_mut().as_mut() {
                gf.drop_bitmaps();
            }
        }
    }

    pub(super) fn paint(&self, app: &App, pid: PaneId, hwnd: HWND) {
        let mut ps = PAINTSTRUCT::default();
        // SAFETY: standard BeginPaint/EndPaint pair.
        let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
        let reqs = self.paint_into(app, pid, hdc);
        // SAFETY: matches BeginPaint.
        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
        if let Ok(mut st) = self.st.try_borrow_mut() {
            if pid == PaneId::Main {
                if let Some(t) = st.shown_at.take() {
                    crate::log_dbg!("overlay first frame {} ms after show()", t.elapsed().as_millis());
                }
            }
        }
        for r in reqs {
            app.workers.io(crate::win::worker::IoTask::Thumb { id: r.id, key: r.key, payload: r.payload, max_px: r.max_px });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(st: &mut State, store: &Store, snippets: &[Snippet], gf: &mut Gfx, pid: PaneId, hdc: HDC, w: i32, h: i32) -> Vec<ThumbReq> {
    let mut reqs = Vec::new();
    if !gf.begin(hdc, w, h) {
        return reqs;
    }
    let pal = st.pal;
    gf.clear(pal.bg);
    st.refresh_card(pid, store, snippets);
    // Clamp scroll through the one layout function.
    {
        let lines = st.card_lines_of(pid);
        let cl = move |_: usize| lines;
        let inp = st.layout_input(pid, &cl);
        let ms = layout::max_scroll(&inp, st.viewport_h());
        let p = &mut st.panes[pid as usize];
        p.scroll = p.scroll.clamp(0.0, ms);
    }
    let g = geo(st, pid, gf);
    draw_list(st, store, snippets, gf, pid, &g, &mut reqs);
    for r in &reqs {
        st.panes[pid as usize].thumbs_asked.insert(r.id);
    }
    draw_header(st, gf, pid, &g);
    draw_footer(st, gf, pid, &g);
    if st.sheet && pid == PaneId::Main {
        draw_sheet(st, gf, &g);
    }
    let r = 8.0 * st.scale;
    gf.stroke_round(rect(0.0, 0.0, g.w, g.h), r, pal.hairline, 1.0);
    // Resize grip hint in the corner the pair grows from (main: bottom-right, pinned: bottom-left).
    let s = st.scale;
    for i in 0..3 {
        let o = (7.0 + 4.0 * i as f32) * s;
        if pid == PaneId::Main {
            gf.line(g.w - o, g.h - 6.0 * s, g.w - 6.0 * s, g.h - o, pal.ink_low, 1.0 * s);
        } else {
            gf.line(o, g.h - 6.0 * s, 6.0 * s, g.h - o, pal.ink_low, 1.0 * s);
        }
    }
    gf.end();
    reqs
}

fn draw_list(st: &mut State, store: &Store, snippets: &[Snippet], gf: &mut Gfx, pid: PaneId, g: &Geo, reqs: &mut Vec<ThumbReq>) {
    let m = st.metrics;
    let pal = st.pal;
    let s = m.scale;
    let active = st.focus == pid;
    let scope = st.active_scope(pid);
    let clip = rect(0.0, g.list_top, g.w, g.list_bottom);
    let rows_empty = st.panes[pid as usize].rows.is_empty();
    if rows_empty {
        let msg = if !st.panes[pid as usize].query.is_empty() {
            "No matches"
        } else if scope == Scope::Snippets && pid == PaneId::Main {
            "No snippets yet — press A to add one"
        } else if pid == PaneId::Pinned {
            "Nothing pinned"
        } else {
            "Nothing copied yet"
        };
        gf.text(msg, Font::Ui, rect(0.0, g.list_top, g.w, g.list_bottom), pal.ink_low, Align::Center);
        return;
    }
    gf.push_clip(clip);
    let asked_now = st.panes[pid as usize].thumbs_asked.clone();
    let bands = bands_for(st, pid);
    let showing_numbers = !st.panes[pid as usize].number_buf.is_empty();
    let x0 = m.side_inset;
    let x1 = g.w - m.side_inset;
    let thumb_px = (96.0 * s) as u32;
    for b in &bands {
        let p = &st.panes[pid as usize];
        let Some(row) = p.rows.get(b.item) else { continue };
        let y = g.list_top + b.top;
        let rr = rect(x0, y, x1, y + b.height);
        let selected = p.sel == Some(b.item);
        let multi_sel = p.multi.len() >= 2 && p.multi.contains(&row.id);
        let item = if scope == Scope::Snippets && pid == PaneId::Main { None } else { store.items().get(row.src) };
        let snippet = if scope == Scope::Snippets && pid == PaneId::Main { snippets.get(row.src) } else { None };

        if b.expanded {
            if let Some(c) = p.card.as_ref() {
                draw_card(st, gf, pid, rr, b.card_lines, c, item, snippet, row, p.hover_btn, reqs, &asked_now, thumb_px);
                continue;
            }
        }
        if selected || multi_sel {
            gf.fill_round(rr, m.r_row, if active { pal.selected_row } else { pal.selected_row_inactive });
        } else if p.hover == Some(b.item) {
            gf.fill_round(rr, m.r_row, pal.surface_hover);
        }
        if item.is_some_and(|it| it.pinned) {
            gf.fill_rect(rect(x0, y + 5.0 * s, x0 + 2.0 * s, y + b.height - 5.0 * s), pal.accent);
        }
        let ink = if selected || multi_sel { pal.sel_text } else { pal.ink_high };
        // Gutter: number while typing digits, else the type icon / thumbnail.
        let gx = x0 + 8.0 * s;
        let gutter = rect(gx, y, gx + m.icon_gutter, y + b.height);
        if showing_numbers {
            gf.text(&row.number.to_string(), Font::Ui, gutter, pal.ink_mid, Align::Center);
        } else if let Some(it) = item {
            let isz = m.content * 1.1;
            let ir = rect(gutter.left + (m.icon_gutter - isz) / 2.0, y + (b.height - isz) / 2.0, gutter.left + (m.icon_gutter + isz) / 2.0, y + (b.height + isz) / 2.0);
            if it.kind == Kind::Image {
                request_thumb(st_thumbs(st, pid), gf, it, thumb_px, reqs);
                if gf.has_bitmap(it.id) {
                    gf.draw_bitmap(it.id, ir);
                } else {
                    icon_image(gf, ir, pal.ink_mid, s);
                }
            } else if it.kind == Kind::Files {
                icon_files(gf, ir, pal.ink_mid, s);
            } else {
                icon_text(gf, ir, pal.ink_mid, s);
            }
        } else {
            icon_text(gf, rect(gutter.left + 2.0 * s, y + (b.height - m.content) / 2.0, gutter.right - 2.0 * s, y + (b.height + m.content) / 2.0), pal.ink_mid, s);
        }
        let tl = gutter.right + 4.0 * s;
        let tr = x1 - m.age_col - m.row_gap_min;
        let trect = rect(tl, y, tr.max(tl + 10.0), y + b.height);
        if let Some(it) = item {
            gf.text(&flatten(&it.preview), Font::Content, trect, ink, Align::Left);
            gf.text(&age_text(it.unix_ms), Font::Ui, rect(x1 - m.age_col - 6.0 * s, y, x1 - 10.0 * s, y + b.height), pal.ink_low, Align::Right);
        } else if let Some(sn) = snippet {
            gf.text(&flatten(&sn.name), Font::Name, rect(tl, y, x1 - 10.0 * s, y + b.height), ink, Align::Left);
        }
    }
    gf.pop_clip();
    // Thin scrollbar.
    {
        let lines = st.card_lines_of(pid);
        let cl = move |_: usize| lines;
        let inp = st.layout_input(pid, &cl);
        let total = layout::content_height_in(&inp, st.viewport_h());
        if total > g.viewport_h + 1.0 {
            let sc = st.panes[pid as usize].scroll;
            let th = (g.viewport_h * g.viewport_h / total).max(24.0 * s);
            let ty = g.list_top + (sc / (total - g.viewport_h).max(1.0)) * (g.viewport_h - th);
            gf.fill_round(rect(g.w - 5.0 * s, ty, g.w - 2.0 * s, ty + th), 1.5 * s, pal.surface_raised);
        }
    }
}

fn st_thumbs(st: &mut State, pid: PaneId) -> &mut HashSet<u64> {
    &mut st.panes[pid as usize].thumbs_asked
}

fn request_thumb(asked: &mut HashSet<u64>, gf: &Gfx, it: &Item, max_px: u32, reqs: &mut Vec<ThumbReq>) {
    if gf.has_bitmap(it.id) || asked.contains(&it.id) {
        return;
    }
    if let Some((k, p)) = it.formats.iter().find(|(k, _)| k.is_std(CF_DIBV5) || k.is_std(CF_DIB) || k.is_named(FMT_PNG)) {
        asked.insert(it.id);
        reqs.push(ThumbReq { id: it.id, key: k.clone(), payload: p.clone(), max_px });
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_card(
    st: &State,
    gf: &mut Gfx,
    pid: PaneId,
    rr: RectF,
    lines: u32,
    c: &CardCache,
    item: Option<&Item>,
    snippet: Option<&Snippet>,
    row: &Row,
    hover_btn: Option<Btn>,
    reqs: &mut Vec<ThumbReq>,
    asked: &HashSet<u64>,
    thumb_px: u32,
) {
    let m = st.metrics;
    let pal = st.pal;
    let s = m.scale;
    let parts = m.card_parts(lines);
    gf.fill_round(rr, m.r_card, pal.surface_field);
    gf.stroke_round(rr, m.r_card, pal.card_edge, 1.0);
    if item.is_some_and(|it| it.pinned) {
        gf.fill_rect(rect(rr.left, rr.top + 8.0 * s, rr.left + 2.0 * s, rr.bottom - 8.0 * s), pal.accent);
    }
    let x = rr.left + parts.pad;
    let right = rr.right - parts.pad;
    // Meta row.
    let my = rr.top + parts.meta_y;
    let mrow = |l: f32, r: f32| rect(l, my, r, my + parts.meta_h);
    let num = format!("#{}", row.number);
    let nw = gf.text_width(&num, Font::UiSemibold) + 4.0 * s;
    gf.text(&num, Font::UiSemibold, mrow(x, x + nw), pal.ink_mid, Align::Left);
    let mut cx = x + nw + 8.0 * s;
    let kind_label = item.map(|it| it.kind.label()).unwrap_or("Snippet");
    let bw = gf.text_width(kind_label, Font::Ui) + 14.0 * s;
    let br = mrow(cx, cx + bw);
    gf.fill_round(br, 5.0 * s, pal.badge_fill);
    gf.text(kind_label, Font::Ui, br, pal.badge_text, Align::Center);
    cx += bw + 10.0 * s;
    if c.n_files > 0 {
        let t = if c.n_files == 1 { "1 file".to_string() } else { format!("{} files", c.n_files) };
        let tw = gf.text_width(&t, Font::Ui) + 4.0 * s;
        gf.text(&t, Font::Ui, mrow(cx, cx + tw), pal.ink_mid, Align::Left);
    } else if c.has_text {
        let t = if c.total_lines == 1 { "1 line".to_string() } else { format!("{} lines", c.total_lines) };
        let tw = gf.text_width(&t, Font::Ui) + 4.0 * s;
        gf.text(&t, Font::Ui, mrow(cx, cx + tw), pal.ink_mid, Align::Left);
    }
    if let Some(it) = item {
        let a = age_text(it.unix_ms);
        gf.text(&a, Font::Ui, mrow(right - 80.0 * s, right), pal.ink_low, Align::Right);
    }
    // Body: real content, up to 4 lines.
    let lh = m.card_line_height();
    let by = rr.top + parts.body_y;
    let mut bx = x;
    if let Some(it) = item {
        if it.kind == Kind::Image {
            request_thumb_ro(gf, it, asked, thumb_px, reqs);
            if gf.has_bitmap(it.id) {
                let side = parts.body_h;
                gf.draw_bitmap(it.id, rect(x, by, x + side * 1.33, by + side));
                bx = x + side * 1.33 + 14.0 * s;
            }
        }
    }
    for (i, l) in c.lines.iter().enumerate().take(lines as usize) {
        gf.text(l, Font::ContentBig, rect(bx, by + i as f32 * lh, right, by + (i as f32 + 1.0) * lh), pal.ink_high, Align::Left);
    }
    // Buttons.
    let multi = st.panes[pid as usize].multi.len() >= 2;
    for (b, r) in card_buttons(st, gf, rr, lines, c, multi) {
        let primary = b == Btn::Paste;
        let hover = hover_btn == Some(b);
        let fill = if primary {
            pal.button_primary
        } else if hover {
            crate::theme::mix(pal.surface_raised, crate::theme::Rgb::WHITE, 24)
        } else {
            pal.surface_raised
        };
        gf.fill_round(r, m.r_button, fill);
        let (k, l) = btn_parts(b, multi);
        let kw = gf.text_width(k, Font::UiSemibold);
        let pad = 12.0 * s;
        let (kc, lc) = if primary { (pal.button_primary_text, pal.button_primary_text) } else { (pal.ink_mid, pal.ink_high) };
        gf.text(k, Font::UiSemibold, rect(r.left + pad, r.top, r.left + pad + kw + 2.0, r.bottom), kc, Align::Left);
        gf.text(l, Font::Ui, rect(r.left + pad + kw + 6.0 * s, r.top, r.right - 4.0, r.bottom), lc, Align::Left);
    }
    let _ = snippet;
}

fn request_thumb_ro(gf: &Gfx, it: &Item, asked: &HashSet<u64>, max_px: u32, reqs: &mut Vec<ThumbReq>) {
    if gf.has_bitmap(it.id) || asked.contains(&it.id) || reqs.iter().any(|r| r.id == it.id) {
        return;
    }
    if let Some((k, p)) = it.formats.iter().find(|(k, _)| k.is_std(CF_DIBV5) || k.is_std(CF_DIB) || k.is_named(FMT_PNG)) {
        reqs.push(ThumbReq { id: it.id, key: k.clone(), payload: p.clone(), max_px });
    }
}

// ---------------- header / footer / sheet ----------------

fn draw_header(st: &State, gf: &Gfx, pid: PaneId, g: &Geo) {
    let m = st.metrics;
    let pal = st.pal;
    let s = m.scale;
    gf.fill_round(g.pill, m.r_field, pal.surface_field);
    // Magnifier.
    let (cx, cy) = (g.pill.left + 18.0 * s, (g.pill.top + g.pill.bottom) / 2.0 - 1.0 * s);
    gf.circle(cx, cy, 5.0 * s, pal.ink_low, Some(1.6 * s));
    gf.line(cx + 3.6 * s, cy + 3.6 * s, cx + 7.5 * s, cy + 7.5 * s, pal.ink_low, 1.8 * s);
    let p = &st.panes[pid as usize];
    if !p.number_buf.is_empty() {
        let r = rect(g.pill.left + 36.0 * s, g.pill.top, g.pill.right - 8.0 * s, g.pill.bottom);
        gf.text(&format!("#{}", p.number_buf), Font::UiSemibold, r, pal.accent, Align::Left);
    }
    if let (Some(bx), Some(a), Some(sn)) = (g.seg_box, g.seg_all, g.seg_snip) {
        gf.fill_round(bx, m.r_field, pal.surface_field);
        let (la, ls) = seg_labels(st);
        let (active, idle) = if st.scope == Scope::History { (a, sn) } else { (sn, a) };
        gf.fill_round(inset(&active, 2.0 * s), m.r_field - 2.0 * s, pal.surface_raised);
        let (ta, ts) = if st.scope == Scope::History { (&la, &ls) } else { (&ls, &la) };
        gf.text(ta, Font::Ui, active, pal.ink_high, Align::Center);
        gf.text(ts, Font::Ui, idle, pal.ink_mid, Align::Center);
    }
}

fn caps_for(pid: PaneId) -> &'static [(&'static str, &'static str)] {
    if pid == PaneId::Main {
        &[("↵", "Paste"), ("Tab", "Pinned"), ("?", "Shortcuts")]
    } else {
        &[("Tab", "List"), ("↵", "Paste")]
    }
}

fn draw_footer(st: &State, gf: &Gfx, pid: PaneId, g: &Geo) {
    let m = st.metrics;
    let pal = st.pal;
    let s = m.scale;
    let top = g.list_bottom;
    let cap_h = m.ui + 8.0 * s;
    let cy = top + (m.footer_h - cap_h) / 2.0;
    let mut x = m.side_inset + 6.0 * s;
    for (k, l) in caps_for(pid) {
        let kw = gf.text_width(k, Font::Ui) + 12.0 * s;
        let cap = rect(x, cy, x + kw, cy + cap_h);
        gf.fill_round(cap, m.r_cap, pal.surface_field);
        gf.text(k, Font::Ui, cap, pal.ink_mid, Align::Center);
        x += kw + 6.0 * s;
        let lw = gf.text_width(l, Font::Ui) + 4.0 * s;
        gf.text(l, Font::Ui, rect(x, top, x + lw, top + m.footer_h), pal.ink_low, Align::Left);
        x += lw + 14.0 * s;
    }
    let n = st.panes[pid as usize].multi.len();
    if n >= 2 {
        let t = format!("{n} selected");
        gf.text(&t, Font::Ui, rect(g.w - 140.0 * s, top, g.w - m.side_inset - 6.0 * s, top + m.footer_h), pal.accent, Align::Right);
    }
}

const SHEET: [(&str, &str); 20] = [
    ("↑ ↓  PgUp PgDn  Home End", "Move selection"),
    ("Shift + ↑ ↓", "Extend selection"),
    ("Ctrl + Click", "Toggle in selection"),
    ("Enter · Double-click", "Paste"),
    ("Ctrl + Enter", "Paste as plain text"),
    ("Tab", "Switch main / pinned"),
    ("Ctrl + → / ←", "Snippets / Clipboard"),
    ("Ctrl + F", "Focus search"),
    ("0–9", "Jump to item number"),
    ("Delete", "Delete item(s)"),
    ("U", "Paste, tracking removed"),
    ("M", "Merge (2+) / Markdown link"),
    ("P", "Plain text (2+: add merged)"),
    ("H", "HTML as plain text"),
    ("E", "Edit, then paste"),
    ("X", "Edit, save as new item"),
    ("Z", "Excel fill (2+)"),
    ("A  (Snippets)", "Add snippet"),
    ("? · F1", "This sheet"),
    ("Esc", "Close"),
];

fn draw_sheet(st: &State, gf: &Gfx, g: &Geo) {
    let m = st.metrics;
    let pal = st.pal;
    let s = m.scale;
    let panel = rect(m.side_inset, m.search_top, g.w - m.side_inset, g.h - m.side_inset);
    gf.fill_round(panel, m.r_card, crate::theme::mix(pal.bg, pal.surface_field, 250));
    gf.stroke_round(panel, m.r_card, pal.card_edge, 1.0);
    gf.text("Keyboard shortcuts", Font::UiSemibold, rect(panel.left + 16.0 * s, panel.top + 8.0 * s, panel.right, panel.top + 8.0 * s + m.ui * 2.0), pal.ink_high, Align::Left);
    let row_h = (m.ui + 8.0 * s).max(20.0 * s);
    let mut y = panel.top + 8.0 * s + m.ui * 2.0 + 4.0 * s;
    let kw = (panel.right - panel.left) * 0.42;
    for (k, d) in SHEET {
        if y + row_h > panel.bottom {
            break;
        }
        gf.text(k, Font::UiSemibold, rect(panel.left + 16.0 * s, y, panel.left + 16.0 * s + kw, y + row_h), pal.ink_mid, Align::Left);
        gf.text(d, Font::Ui, rect(panel.left + 24.0 * s + kw, y, panel.right - 12.0 * s, y + row_h), pal.ink_high, Align::Left);
        y += row_h;
    }
}

// ---------------- icons (vector, antialiased) ----------------

fn icon_text(gf: &Gfx, r: RectF, c: crate::theme::Rgb, s: f32) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let b = rect(r.left + w * 0.18, r.top + h * 0.08, r.right - w * 0.18, r.bottom - h * 0.08);
    gf.stroke_round(b, 2.0 * s, c, 1.3 * s);
    for i in 0..3 {
        let y = b.top + (b.bottom - b.top) * (0.3 + 0.22 * i as f32);
        gf.line(b.left + 3.0 * s, y, b.right - 3.0 * s - if i == 2 { 3.0 * s } else { 0.0 }, y, c, 1.1 * s);
    }
}

fn icon_image(gf: &Gfx, r: RectF, c: crate::theme::Rgb, s: f32) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let b = rect(r.left + w * 0.06, r.top + h * 0.16, r.right - w * 0.06, r.bottom - h * 0.16);
    gf.stroke_round(b, 2.5 * s, c, 1.3 * s);
    gf.circle(b.left + (b.right - b.left) * 0.3, b.top + (b.bottom - b.top) * 0.34, 1.8 * s, c, None);
    gf.line(b.left + 2.0 * s, b.bottom - 3.0 * s, b.left + (b.right - b.left) * 0.45, b.top + (b.bottom - b.top) * 0.55, c, 1.2 * s);
    gf.line(b.left + (b.right - b.left) * 0.45, b.top + (b.bottom - b.top) * 0.55, b.right - 2.0 * s, b.bottom - 3.0 * s, c, 1.2 * s);
}

fn icon_files(gf: &Gfx, r: RectF, c: crate::theme::Rgb, s: f32) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let b = rect(r.left + w * 0.05, r.top + h * 0.22, r.right - w * 0.05, r.bottom - h * 0.14);
    gf.stroke_round(b, 2.0 * s, c, 1.3 * s);
    gf.line(b.left + 1.0 * s, b.top, b.left + (b.right - b.left) * 0.38, b.top, c, 2.2 * s);
    gf.line(b.left + 1.0 * s, b.top - 2.0 * s, b.left + (b.right - b.left) * 0.34, b.top - 2.0 * s, c, 1.2 * s);
}
