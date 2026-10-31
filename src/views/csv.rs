//! Sortable table view for CSV and TSV files.
//!
//! The document is parsed once (in the background for large files) into
//! rows of cells; the first row is the header. Clicking a header sorts by
//! that column, ascending, descending, then back to file order. Only the
//! visible rows are laid out. Dragging a header's right edge resizes the
//! column; double-clicking it returns to the automatic width.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ListHorizontalSizingBehavior,
    MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Render, ScrollStrategy, Stateful, Task, TextRun, UniformListScrollHandle,
    Window, div, prelude::*, px, uniform_list,
};

use super::JumpTo;
use super::scrollbar::{self, Axis, Bar, Drag};
use super::text::{DocEnd, DocStart, MoveDown, MoveUp, Newline, PageDown, PageUp};
use crate::doc::Buffer;
use crate::theme::{self, Settings};

/// Only this much of a document is shown.
const MAX_RENDER: u64 = 16 << 20;
/// Above this size parsing happens off the main thread.
const SYNC_LIMIT: usize = 1 << 20;
/// Rows looked at to size the columns.
const WIDTH_SAMPLE: usize = 1000;
/// Column width bounds, in characters.
const MIN_COL: usize = 3;
const MAX_COL: usize = 40;

// ---------------------------------------------------------------- model

pub struct Row {
    /// Byte offset of the record in the document.
    pub offset: u64,
    pub cells: Vec<String>,
}

#[derive(Default)]
pub struct Table {
    pub header: Vec<String>,
    pub rows: Vec<Row>,
    /// Column widths in characters.
    pub widths: Vec<usize>,
}

/// The separator used in the first line: `,`, `;`, tab or `|`, whichever occurs most.
fn delimiter(src: &str) -> char {
    let mut counts = [0usize; 4];
    let mut quoted = false;
    for c in src.chars() {
        match c {
            '"' => quoted = !quoted,
            '\n' if !quoted => break,
            ',' | ';' | '\t' | '|' if !quoted => counts[",;\t|".find(c).unwrap()] += 1,
            _ => {}
        }
    }
    let best = (0..4).max_by_key(|&i| (counts[i], std::cmp::Reverse(i))).unwrap();
    if counts[best] == 0 { ',' } else { ",;\t|".chars().nth(best).unwrap() }
}

/// Split `src` into records of fields. Quoted fields may hold separators,
/// line breaks and doubled quotes. Blank lines are skipped.
fn records(src: &str, delim: char) -> Vec<(usize, Vec<String>)> {
    let mut out = Vec::new();
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut start = 0;
    let mut quoted = false;
    let mut chars = src.char_indices().peekable();
    let mut end_record = |fields: &mut Vec<String>, field: &mut String, start: usize| {
        fields.push(std::mem::take(field));
        if fields.len() > 1 || !fields[0].is_empty() {
            out.push((start, std::mem::take(fields)));
        }
        fields.clear();
    };
    while let Some((_, c)) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek().is_some_and(|&(_, n)| n == '"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' && field.is_empty() {
            quoted = true;
        } else if c == delim {
            fields.push(std::mem::take(&mut field));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek().is_some_and(|&(_, n)| n == '\n') {
                chars.next();
            }
            end_record(&mut fields, &mut field, start);
            start = chars.peek().map_or(src.len(), |&(j, _)| j);
        } else {
            field.push(c);
        }
    }
    if !field.is_empty() || !fields.is_empty() {
        end_record(&mut fields, &mut field, start);
    }
    out
}

pub fn parse(src: &str) -> Table {
    let body = src.trim_start_matches('\u{feff}');
    let skipped = src.len() - body.len();
    let mut recs = records(body, delimiter(body)).into_iter();
    let Some((_, header)) = recs.next() else { return Table::default() };
    let rows: Vec<Row> = recs.map(|(offset, cells)| Row { offset: (skipped + offset) as u64, cells }).collect();
    let cols = rows.iter().map(|r| r.cells.len()).max().unwrap_or(0).max(header.len());
    let mut widths = vec![MIN_COL; cols];
    for cells in std::iter::once(&header).chain(rows.iter().take(WIDTH_SAMPLE).map(|r| &r.cells)) {
        for (w, c) in widths.iter_mut().zip(cells) {
            *w = (*w).max(c.chars().count().min(MAX_COL));
        }
    }
    Table { header, rows, widths }
}

/// Sort key of a cell: numbers (also with a decimal comma) before text.
#[derive(PartialEq, PartialOrd)]
enum Key {
    Num(f64),
    Text(String),
}

fn key(cell: &str) -> Key {
    let t = cell.trim();
    let num = t.parse::<f64>().ok().or_else(|| (!t.contains('.')).then(|| t.replace(',', ".").parse().ok()).flatten());
    match num {
        Some(n) if n.is_finite() => Key::Num(n),
        _ => Key::Text(t.to_lowercase()),
    }
}

/// Row order for sorting by `col`; stable, so equal cells keep file order.
pub fn sorted(rows: &[Row], col: usize, ascending: bool) -> Vec<usize> {
    let keys: Vec<Key> = rows.iter().map(|r| key(r.cells.get(col).map_or("", String::as_str))).collect();
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| {
        let o = keys[a].partial_cmp(&keys[b]).unwrap_or(Ordering::Equal);
        if ascending { o } else { o.reverse() }
    });
    order
}

// ---------------------------------------------------------------- view

pub struct CsvView {
    buffer: Entity<Buffer>,
    focus: FocusHandle,
    table: Rc<Table>,
    /// Display order: indices into `table.rows`.
    order: Vec<usize>,
    /// Column and direction (ascending) of the current sort.
    sort: Option<(usize, bool)>,
    /// Position in `order`.
    selected: usize,
    scroll: UniformListScrollHandle,
    /// Column widths set by dragging, in characters, so they follow zooming.
    custom: HashMap<usize, f32>,
    /// Column being resized: column, mouse x and width at the start.
    resizing: Option<(usize, Pixels, f32)>,
    /// Character width at the last render.
    char_w: Pixels,
    /// Width of a row at the last render.
    content_w: Pixels,
    /// Scrollbars at the last render, where the rows don't fit.
    hbar: Option<Bar>,
    vbar: Option<Bar>,
    /// Scrollbar thumb being dragged.
    drag: Option<Drag>,
    parsed_version: Option<u64>,
    truncated: bool,
    parsing: bool,
    _reparse: Option<Task<()>>,
}

impl EventEmitter<JumpTo> for CsvView {}

impl Focusable for CsvView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl CsvView {
    pub fn new(buffer: Entity<Buffer>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&buffer, |this, _, cx| this.schedule(Duration::from_millis(150), cx)).detach();
        let mut v = CsvView {
            buffer,
            focus: cx.focus_handle(),
            table: Rc::new(Table::default()),
            order: Vec::new(),
            sort: None,
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            custom: HashMap::new(),
            resizing: None,
            char_w: px(8.),
            content_w: px(0.),
            hbar: None,
            vbar: None,
            drag: None,
            parsed_version: None,
            truncated: false,
            parsing: false,
            _reparse: None,
        };
        v.reparse(cx);
        v
    }

    fn schedule(&mut self, delay: Duration, cx: &mut Context<Self>) {
        if Some(self.buffer.read(cx).version()) == self.parsed_version {
            return;
        }
        self._reparse = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |v, cx| v.reparse(cx)).ok();
        }));
    }

    fn reparse(&mut self, cx: &mut Context<Self>) {
        let b = self.buffer.read(cx);
        let version = b.version();
        let len = b.len();
        self.truncated = len > MAX_RENDER;
        let mut bytes = b.slice(0..len.min(MAX_RENDER)).into_owned();
        if self.truncated {
            // Drop the partial last record.
            let end = bytes.iter().rposition(|&c| c == b'\n').unwrap_or(0);
            bytes.truncate(end);
        }
        self.parsed_version = Some(version);
        if bytes.len() <= SYNC_LIMIT {
            let table = parse(&String::from_utf8_lossy(&bytes));
            self.apply(table, cx);
            return;
        }
        self.parsing = true;
        cx.notify();
        self._reparse = Some(cx.spawn(async move |this, cx| {
            let table = cx.background_executor().spawn(async move { parse(&String::from_utf8_lossy(&bytes)) }).await;
            this.update(cx, |v, cx| {
                if v.parsed_version == Some(version) {
                    v.parsing = false;
                    v.apply(table, cx);
                }
            })
            .ok();
        }));
    }

    fn apply(&mut self, table: Table, cx: &mut Context<Self>) {
        self.table = Rc::new(table);
        // Keep the sort across edits, unless its column is gone.
        self.sort = self.sort.filter(|&(c, _)| c < self.table.widths.len());
        self.resort();
        self.selected = self.selected.min(self.order.len().saturating_sub(1));
        cx.notify();
    }

    fn resort(&mut self) {
        self.order = match self.sort {
            Some((col, asc)) => sorted(&self.table.rows, col, asc),
            None => (0..self.table.rows.len()).collect(),
        };
    }

    /// Header click: ascending, descending, file order.
    fn cycle_sort(&mut self, col: usize, cx: &mut Context<Self>) {
        self.sort = match self.sort {
            Some((c, true)) if c == col => Some((col, false)),
            Some((c, false)) if c == col => None,
            _ => Some((col, true)),
        };
        let row = self.order.get(self.selected).copied();
        self.resort();
        // Keep the selected record selected.
        self.selected = row.and_then(|r| self.order.iter().position(|&o| o == r)).unwrap_or(0);
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.notify();
    }

    fn select(&mut self, row: usize, cx: &mut Context<Self>) {
        if self.order.is_empty() {
            return;
        }
        self.selected = row.min(self.order.len() - 1);
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.notify();
    }

    fn activate(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(&r) = self.order.get(row) {
            cx.emit(JumpTo(self.table.rows[r].offset));
        }
    }

    fn on_row_down(&mut self, row: usize, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.selected = row;
        if e.click_count == 2 {
            self.activate(row, cx);
        }
        cx.notify();
    }

    /// Width of column `c` in characters, padding included.
    fn col_chars(&self, c: usize) -> f32 {
        self.custom.get(&c).copied().unwrap_or(self.table.widths[c] as f32 + 2.)
    }

    fn start_resize(&mut self, c: usize, e: &MouseDownEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if e.click_count == 2 {
            self.custom.remove(&c);
            self.resizing = None;
        } else {
            self.resizing = Some((c, e.position.x, self.col_chars(c)));
        }
        cx.notify();
    }

    /// Both scrollbars as the latest layout gives them.
    fn current_bars(&self) -> (Option<Bar>, Option<Bar>) {
        if self.table.header.is_empty() {
            return (None, None);
        }
        let state = self.scroll.0.borrow();
        let base = &state.base_handle;
        let h = state.last_item_size.and_then(|s| Bar::new(Axis::Horizontal, base.bounds().left(), s.item.width, self.content_w, -base.offset().x));
        drop(state);
        (h, scrollbar::uniform_vbar(&self.scroll))
    }

    /// Scrollbar press: on the thumb starts a drag, beside it scrolls a page that way.
    fn on_bar_down(&mut self, axis: Axis, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.focus(&self.focus, cx);
        let bar = if axis == Axis::Horizontal { self.hbar } else { self.vbar };
        let Some(bar) = bar else { return };
        match bar.press(e.position, scrollbar::uniform_offset(&self.scroll, axis)) {
            Ok(drag) => self.drag = Some(drag),
            Err(to) => scrollbar::set_uniform_offset(&self.scroll, axis, to),
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag {
            let bar = if drag.axis == Axis::Horizontal { self.hbar } else { self.vbar };
            if e.pressed_button != Some(MouseButton::Left) {
                self.drag = None;
            } else if let Some(bar) = bar {
                scrollbar::set_uniform_offset(&self.scroll, drag.axis, bar.dragged(&drag, e.position));
            }
            cx.notify();
            return;
        }
        let Some((c, x0, w0)) = self.resizing else { return };
        if e.pressed_button != Some(MouseButton::Left) {
            self.resizing = None;
        } else {
            self.custom.insert(c, (w0 + (e.position.x - x0) / self.char_w).max(MIN_COL as f32));
        }
        cx.notify();
    }

    fn end_resize(&mut self, cx: &mut Context<Self>) {
        if self.resizing.take().is_some() | self.drag.take().is_some() {
            cx.notify();
        }
    }

    fn render_rows(&mut self, range: Range<usize>, cols: &[Pixels], num_w: Pixels, cx: &mut Context<Self>) -> Vec<Stateful<gpui::Div>> {
        let lh = Settings::get(cx).line_height();
        let table = self.table.clone();
        let mut out = Vec::with_capacity(range.len());
        for ix in range {
            let Some(&r) = self.order.get(ix) else { continue };
            let row = &table.rows[r];
            out.push(
                div()
                    .id(("csv-row", ix))
                    .h(lh)
                    .flex()
                    .flex_row()
                    .items_center()
                    .whitespace_nowrap()
                    .border_b_1()
                    .border_color(theme::border())
                    .when(ix == self.selected, |d| d.bg(theme::selection()))
                    .when(ix != self.selected, |d| d.hover(|s| s.bg(theme::bg_hover())))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |v, e: &MouseDownEvent, w, cx| v.on_row_down(ix, e, w, cx)))
                    .child(div().w(num_w).flex_none().pr_2().text_right().text_color(theme::text_faint()).child((r + 1).to_string()))
                    .children(cols.iter().enumerate().map(|(c, &w)| {
                        let text = row.cells.get(c).map_or(String::new(), |s| s.replace(['\n', '\r'], "⏎"));
                        cell(w).child(text)
                    })),
            );
        }
        out
    }
}

fn cell(w: Pixels) -> gpui::Div {
    div().w(w).flex_none().px(px(6.)).overflow_hidden().text_ellipsis().border_l_1().border_color(theme::border())
}

impl Render for CsvView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Settings::get(cx);
        let font = s.mono();
        let font_size = s.font_size;
        let lh = s.line_height();
        let run = TextRun { len: 1, font: font.clone(), color: theme::text(), background_color: None, underline: None, strikethrough: None, letter_spacing: None };
        let char_w = window.text_system().shape_line("0".into(), font_size, &[run]).width;
        self.char_w = char_w;
        let table = self.table.clone();
        let cols: Rc<Vec<Pixels>> = Rc::new((0..table.widths.len()).map(|c| char_w * self.col_chars(c)).collect());
        let digits = table.rows.len().max(1).to_string().len();
        let num_w = char_w * (digits as f32 + 2.);
        // The list clamps its scroll offset only when it lays out, after this render, and a
        // wheel step past the right edge is stored unclamped. Clamp here, from the row width,
        // so the header never runs past the rows below it.
        let content_w = cols.iter().fold(num_w, |a, &w| a + w);
        self.content_w = content_w;
        let view_w = self.scroll.0.borrow().last_item_size.map(|s| s.item.width);
        let max_x = view_w.map_or(px(0.), |w| (content_w - w).max(px(0.)));
        let mut scroll_x = self.scroll.0.borrow().base_handle.offset().x;
        if view_w.is_some() && (scroll_x < -max_x || scroll_x > px(0.)) {
            scroll_x = scroll_x.clamp(-max_x, px(0.));
            scrollbar::set_uniform_offset(&self.scroll, Axis::Horizontal, -scroll_x);
        }
        (self.hbar, self.vbar) = self.current_bars();
        scrollbar::recheck((self.hbar, self.vbar), Self::current_bars, window, cx);
        if self.drag.is_some_and(|d| if d.axis == Axis::Horizontal { self.hbar.is_none() } else { self.vbar.is_none() }) {
            self.drag = None;
        }
        let dragging = |axis| self.drag.is_some_and(|d: Drag| d.axis == axis);
        let hbar = self.hbar.map(|bar| {
            bar.element(dragging(Axis::Horizontal))
                .id("csv-hbar")
                .on_mouse_down(MouseButton::Left, cx.listener(|v, e: &MouseDownEvent, w, cx| v.on_bar_down(Axis::Horizontal, e, w, cx)))
        });
        let vbar = self.vbar.map(|bar| {
            bar.element(dragging(Axis::Vertical))
                .id("csv-vbar")
                .on_mouse_down(MouseButton::Left, cx.listener(|v, e: &MouseDownEvent, w, cx| v.on_bar_down(Axis::Vertical, e, w, cx)))
        });

        let notice = if self.parsing {
            Some(tr!("Reading table…"))
        } else if self.truncated {
            Some(tr!("Large document: only the first 16 MiB are shown."))
        } else {
            None
        };
        let page = 20;

        let header = div()
            .flex_none()
            .h(lh)
            .flex()
            .flex_row()
            .items_center()
            .ml(scroll_x)
            .whitespace_nowrap()
            .child(div().w(num_w).flex_none())
            .children(cols.iter().enumerate().map(|(c, &w)| {
                let name = table.header.get(c).cloned().unwrap_or_default();
                let arrow = match self.sort {
                    Some((col, true)) if col == c => " ▲",
                    Some((col, false)) if col == c => " ▼",
                    _ => "",
                };
                cell(w)
                    .id(("csv-head", c))
                    .relative()
                    .h_full()
                    .flex()
                    .items_center()
                    .font_weight(gpui::FontWeight::BOLD)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::bg_hover()))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |v, _, w, cx| {
                        w.focus(&v.focus, cx);
                        v.cycle_sort(c, cx);
                    }))
                    .child(format!("{name}{arrow}"))
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right_0()
                            .w(px(5.))
                            .cursor_col_resize()
                            .hover(|s| s.bg(theme::accent()))
                            .when(self.resizing.is_some_and(|r| r.0 == c), |d| d.bg(theme::accent()))
                            .on_mouse_down(MouseButton::Left, cx.listener(move |v, e: &MouseDownEvent, _, cx| v.start_resize(c, e, cx))),
                    )
            }));

        div()
            .id("csv-view")
            .key_context("CsvView")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .font(font)
            .text_size(font_size)
            .when(self.resizing.is_some(), |d| d.cursor_col_resize())
            .on_mouse_move(cx.listener(|v, e: &MouseMoveEvent, _, cx| v.on_mouse_move(e, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(|v, _, _, cx| v.end_resize(cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|v, _, _, cx| v.end_resize(cx)))
            .on_action(cx.listener(|v, _: &MoveUp, _, cx| v.select(v.selected.saturating_sub(1), cx)))
            .on_action(cx.listener(|v, _: &MoveDown, _, cx| v.select(v.selected + 1, cx)))
            .on_action(cx.listener(move |v, _: &PageUp, _, cx| v.select(v.selected.saturating_sub(page), cx)))
            .on_action(cx.listener(move |v, _: &PageDown, _, cx| v.select(v.selected + page, cx)))
            .on_action(cx.listener(|v, _: &DocStart, _, cx| v.select(0, cx)))
            .on_action(cx.listener(|v, _: &DocEnd, _, cx| v.select(usize::MAX, cx)))
            .on_action(cx.listener(|v, _: &Newline, _, cx| v.activate(v.selected, cx)))
            .children(notice.map(|n| {
                div()
                    .px_4()
                    .py_1()
                    .bg(theme::bg_panel())
                    .border_b_1()
                    .border_color(theme::border())
                    .text_size(px(12.))
                    .text_color(theme::warn())
                    .child(n)
            }))
            .child(
                div()
                    .flex_none()
                    .overflow_hidden()
                    .bg(theme::bg_panel())
                    .border_b_1()
                    .border_color(theme::border())
                    .child(header),
            )
            .child(if table.header.is_empty() && !self.parsing {
                div().flex_1().p_4().text_color(theme::text_dim()).child(tr!("Empty document")).into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(
                        uniform_list("csv-rows", self.order.len(), cx.processor(move |v, range, _, cx| v.render_rows(range, &cols, num_w, cx)))
                            .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                            .track_scroll(&self.scroll)
                            .flex_1()
                            .min_w_0()
                            .h_full(),
                    )
                    .children(vbar)
                    .into_any_element()
            })
            .children(hbar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_separator() {
        assert_eq!(delimiter("a,b,c\n1;2"), ',');
        assert_eq!(delimiter("a;b;\"x,y,z\"\n"), ';');
        assert_eq!(delimiter("a\tb\n"), '\t');
        assert_eq!(delimiter("single\n"), ',');
    }

    #[test]
    fn quotes_line_breaks_and_offsets() {
        let t = parse("name;note\r\nAnna;\"says \"\"hi\"\";\nbye\"\n\nBert;x\n");
        assert_eq!(t.header, ["name", "note"]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0].cells, ["Anna", "says \"hi\";\nbye"]);
        assert_eq!(t.rows[0].offset, 11);
        assert_eq!(t.rows[1].cells, ["Bert", "x"]);
        assert_eq!(t.rows[1].offset, 36);
        assert_eq!(t.widths, [4, 14]);
        assert_eq!(parse("\u{feff}a\nb").rows[0].offset, 5);
    }

    #[test]
    fn ragged_rows_widen_the_table() {
        let t = parse("a\n1,2,3");
        assert_eq!(t.widths.len(), 3);
    }

    #[test]
    fn sorts_numbers_then_text() {
        let t = parse("v;w\n10;\nb;\n9;\n1,5;\nA;\n");
        let col = |o: Vec<usize>| o.iter().map(|&i| t.rows[i].cells[0].clone()).collect::<Vec<_>>();
        assert_eq!(col(sorted(&t.rows, 0, true)), ["1,5", "9", "10", "A", "b"]);
        assert_eq!(col(sorted(&t.rows, 0, false)), ["b", "A", "10", "9", "1,5"]);
    }
}
