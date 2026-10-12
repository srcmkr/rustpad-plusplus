//! The text editor view.
//!
//! Scrolling is anchored at a byte offset (`top`) instead of a line number,
//! so jumping anywhere in a terabyte file is O(1) and nothing outside the
//! visible rows is ever read. Rows are laid out per frame from the buffer.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, ShapedLine,
    SharedString, Style, Styled, TextRun, UTF16Selection, Window, actions, div, fill, point,
    prelude::*, px, relative, size,
};

use crate::doc::Buffer;
use crate::doc::rows::{self, DisplayRow, RowSpan};
use crate::theme::{self, Settings};

actions!(
    editor,
    [
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        Home,
        End,
        SelectHome,
        SelectEnd,
        DocStart,
        DocEnd,
        SelectDocStart,
        SelectDocEnd,
        PageUp,
        PageDown,
        SelectPageUp,
        SelectPageDown,
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        Newline,
        Tab,
        SelectAll,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        DuplicateLine,
        DeleteLine,
        Cancel,
    ]
);

/// Copying more than this into the clipboard is refused.
const MAX_CLIPBOARD: u64 = 64 << 20;
const SCROLLBAR_W: f32 = 12.0;
const MIN_THUMB: f32 = 28.0;

#[derive(Clone, Debug)]
pub enum TextEvent {
    Edited,
    CursorMoved,
    /// Enter in a single-line input.
    Submitted,
    /// Escape.
    Cancelled,
    /// Tab in a single-line input: move to the next field.
    TabbedOut,
    Message(SharedString),
}

pub struct TextView {
    pub buffer: Entity<Buffer>,
    focus: FocusHandle,
    single_line: bool,
    top: u64,
    scroll_x: Pixels,
    /// Word wrap width in columns, 0 when off. Set from the view width on paint.
    wrap: usize,
    cursor: u64,
    /// The cursor sits at a soft row break and belongs to the end of the
    /// upper row (after End, or a click right of a wrapped row) rather than
    /// the start of the lower one.
    at_row_end: bool,
    anchor: u64,
    goal_col: Option<usize>,
    marked: Option<Range<u64>>,
    /// Highlighted search hit.
    pub highlight: Option<Range<u64>>,
    autoscroll: bool,
    selecting: bool,
    thumb_drag: Option<Pixels>,
    wheel_rest: f32,
    layout: Option<Layout>,
}

struct PaintedRow {
    row: DisplayRow,
    line: ShapedLine,
    y: Pixels,
}

struct Layout {
    bounds: Bounds<Pixels>,
    text_x: Pixels,
    line_height: Pixels,
    rows: Vec<PaintedRow>,
    visible_rows: usize,
    track: Option<Bounds<Pixels>>,
    thumb: Option<Bounds<Pixels>>,
}

impl EventEmitter<TextEvent> for TextView {}

impl Focusable for TextView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TextView {
    pub fn new(buffer: Entity<Buffer>, cx: &mut Context<Self>) -> Self {
        cx.observe(&buffer, |this, _, cx| {
            let len = this.buffer.read(cx).len();
            this.cursor = this.cursor.min(len);
            this.anchor = this.anchor.min(len);
            this.top = this.top.min(len);
            cx.notify();
        })
        .detach();
        TextView {
            buffer,
            focus: cx.focus_handle(),
            single_line: false,
            top: 0,
            scroll_x: px(0.),
            wrap: 0,
            cursor: 0,
            at_row_end: false,
            anchor: 0,
            goal_col: None,
            marked: None,
            highlight: None,
            autoscroll: false,
            selecting: false,
            thumb_drag: None,
            wheel_rest: 0.0,
            layout: None,
        }
    }

    pub fn single_line(text: &str, cx: &mut Context<Self>) -> Self {
        let buffer = cx.new(|_| Buffer::from_text(text));
        let mut v = Self::new(buffer, cx);
        v.single_line = true;
        v.cursor = text.len() as u64;
        v.anchor = 0;
        v
    }

    pub fn content(&self, cx: &App) -> String {
        let b = self.buffer.read(cx);
        b.text(0..b.len())
    }

    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Cursor to the end with the last line at the bottom of the view (tail -f).
    pub fn scroll_to_end(&mut self, cx: &mut Context<Self>) {
        let b = self.buffer.read(cx);
        let len = b.len();
        self.top = self.max_top(b);
        self.cursor = len;
        self.anchor = len;
        cx.emit(TextEvent::CursorMoved);
        cx.notify();
    }

    pub fn top(&self) -> u64 {
        self.top
    }

    /// Restore a saved cursor and scroll position (clamped to the document).
    pub fn restore_position(&mut self, cursor: u64, top: u64, cx: &mut Context<Self>) {
        let b = self.buffer.read(cx);
        let len = b.len();
        self.cursor = cursor.min(len);
        self.anchor = self.cursor;
        self.top = rows::row_containing(b, top.min(len), self.wrap).start;
        cx.notify();
    }

    pub fn wrap(&self) -> usize {
        self.wrap
    }

    pub fn buffer_version(&self, cx: &App) -> u64 {
        self.buffer.read(cx).version()
    }

    pub fn selection(&self) -> Range<u64> {
        self.cursor.min(self.anchor)..self.cursor.max(self.anchor)
    }

    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.anchor = 0;
        self.cursor = self.buffer.read(cx).len();
        cx.notify();
    }

    /// Place the cursor (and optionally a selection) and scroll it into view.
    pub fn go_to(&mut self, range: Range<u64>, cx: &mut Context<Self>) {
        let len = self.buffer.read(cx).len();
        self.anchor = range.start.min(len);
        self.cursor = range.end.min(len);
        self.at_row_end = false;
        self.goal_col = None;
        self.center_cursor(cx);
        cx.emit(TextEvent::CursorMoved);
        cx.notify();
    }

    fn visible_rows(&self) -> usize {
        self.layout.as_ref().map(|l| l.visible_rows).unwrap_or(40).max(1)
    }

    fn center_cursor(&mut self, cx: &mut Context<Self>) {
        let half = self.visible_rows() / 3;
        let b = self.buffer.read(cx);
        let mut r = rows::row_containing(b, self.cursor, self.wrap);
        for _ in 0..half {
            match rows::prev_row(b, r.start, self.wrap) {
                Some(p) => r = p,
                None => break,
            }
        }
        self.top = r.start;
        self.autoscroll = true;
    }

    // ------------------------------------------------------------ scrolling

    fn scroll_rows(&mut self, n: isize, cx: &mut Context<Self>) {
        let b = self.buffer.read(cx);
        if n > 0 {
            for _ in 0..n {
                match rows::row_at(b, self.top, self.wrap).next {
                    Some(next) => self.top = next,
                    None => break,
                }
            }
            self.top = self.clamp_top(b, self.top);
        } else {
            for _ in 0..(-n) {
                match rows::prev_row(b, self.top, self.wrap) {
                    Some(p) => self.top = p.start,
                    None => break,
                }
            }
        }
        cx.notify();
    }

    /// Keep the view filled near the end of the document. Walking forward is
    /// cheap, so only compute the last page when we are actually close to it.
    fn clamp_top(&self, b: &Buffer, top: u64) -> u64 {
        let mut r = rows::row_at(b, top, self.wrap);
        for _ in 1..self.visible_rows() {
            match r.next {
                Some(n) => r = rows::row_at(b, n, self.wrap),
                None => return top.min(self.max_top(b)),
            }
        }
        top
    }

    /// Topmost row offset that still fills the view.
    fn max_top(&self, b: &Buffer) -> u64 {
        let mut r = rows::row_containing(b, b.len(), self.wrap);
        for _ in 1..self.visible_rows() {
            match rows::prev_row(b, r.start, self.wrap) {
                Some(p) => r = p,
                None => break,
            }
        }
        r.start
    }

    fn ensure_cursor_visible(&mut self, cx: &mut Context<Self>) {
        self.autoscroll = true;
        let b = self.buffer.read(cx);
        // Work with the start of the row the cursor is shown in.
        let cursor_row = self.cursor_span(b);
        let c = cursor_row.start;
        if c < self.top {
            self.top = c;
            return;
        }
        let n = self.visible_rows();
        let mut r = rows::row_at(b, self.top, self.wrap);
        for _ in 1..n {
            if c < r.next.unwrap_or(u64::MAX) {
                return;
            }
            r = rows::row_at(b, r.next.unwrap(), self.wrap);
        }
        if c < r.next.unwrap_or(u64::MAX) {
            return;
        }
        // Cursor is below: put its row at the bottom.
        let mut r = cursor_row;
        for _ in 1..n {
            match rows::prev_row(b, r.start, self.wrap) {
                Some(p) => r = p,
                None => break,
            }
        }
        self.top = r.start;
    }

    // ------------------------------------------------------------ movement

    /// Whether `pos` is the end of `span` and the following row continues the same line.
    fn is_soft_end(span: RowSpan, pos: u64) -> bool {
        pos == span.end && span.end > span.start && span.next == Some(span.end)
    }

    /// The row the cursor is shown in.
    fn cursor_span(&self, b: &Buffer) -> RowSpan {
        if self.at_row_end && self.cursor > 0 {
            let r = rows::row_containing(b, self.cursor - 1, self.wrap);
            if Self::is_soft_end(r, self.cursor) {
                return r;
            }
        }
        rows::row_containing(b, self.cursor, self.wrap)
    }

    fn set_cursor(&mut self, pos: u64, select: bool, cx: &mut Context<Self>) {
        self.place_cursor(pos, select, false, cx);
    }

    /// Like `set_cursor`, with the row the cursor belongs to at a soft break.
    fn place_cursor(&mut self, pos: u64, select: bool, at_row_end: bool, cx: &mut Context<Self>) {
        self.at_row_end = at_row_end;
        self.cursor = pos;
        if !select {
            self.anchor = pos;
        }
        self.marked = None;
        self.ensure_cursor_visible(cx);
        cx.emit(TextEvent::CursorMoved);
        cx.notify();
    }

    fn prev_char(b: &Buffer, pos: u64) -> u64 {
        if pos == 0 {
            return 0;
        }
        let bytes = b.slice(pos.saturating_sub(4)..pos);
        if bytes.ends_with(b"\r\n") {
            return pos - 2;
        }
        let mut n = 1;
        while n < bytes.len() && bytes[bytes.len() - n] & 0xC0 == 0x80 {
            n += 1;
        }
        pos - n as u64
    }

    fn next_char(b: &Buffer, pos: u64) -> u64 {
        if pos >= b.len() {
            return b.len();
        }
        let bytes = b.slice(pos..(pos + 4).min(b.len()));
        if bytes.starts_with(b"\r\n") {
            return pos + 2;
        }
        let n = match bytes[0] {
            x if x >= 0xF0 => 4,
            x if x >= 0xE0 => 3,
            x if x >= 0xC0 => 2,
            _ => 1,
        };
        (pos + n).min(b.len())
    }

    fn class(c: u8) -> u8 {
        if c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80 {
            2
        } else if c.is_ascii_whitespace() {
            0
        } else {
            1
        }
    }

    fn word_right(b: &Buffer, pos: u64) -> u64 {
        let end = (pos + rows::SEG).min(b.len());
        let bytes = b.slice(pos..end);
        let mut i = 0;
        while i < bytes.len() && Self::class(bytes[i]) == 0 {
            if bytes[i] == b'\n' && i > 0 {
                return pos + i as u64;
            }
            i += 1;
        }
        if i < bytes.len() {
            let k = Self::class(bytes[i]);
            while i < bytes.len() && Self::class(bytes[i]) == k {
                i += 1;
            }
        }
        (pos + i.max(1) as u64).min(b.len())
    }

    fn word_left(b: &Buffer, pos: u64) -> u64 {
        let start = pos.saturating_sub(rows::SEG);
        let bytes = b.slice(start..pos);
        let mut i = bytes.len();
        while i > 0 && Self::class(bytes[i - 1]) == 0 {
            if bytes[i - 1] == b'\n' && i < bytes.len() {
                return start + i as u64;
            }
            i -= 1;
        }
        if i > 0 {
            let k = Self::class(bytes[i - 1]);
            while i > 0 && Self::class(bytes[i - 1]) == k {
                i -= 1;
            }
        }
        let r = start + i as u64;
        if r == pos { pos.saturating_sub(1) } else { r }
    }

    fn col_of(row: &DisplayRow, pos: u64) -> usize {
        let d = row.disp_index(pos);
        row.text[..d].chars().count()
    }

    fn pos_for_col(row: &DisplayRow, col: usize) -> u64 {
        let disp = row.text.char_indices().nth(col).map(|(i, _)| i).unwrap_or(row.text.len());
        row.offset_for_disp(disp).min(row.span.end)
    }

    fn vertical(&mut self, rows_delta: isize, select: bool, cx: &mut Context<Self>) {
        let b = self.buffer.read(cx);
        let cur = self.cursor_span(b);
        let cur_row = DisplayRow::build(b, cur);
        let col = self.goal_col.unwrap_or_else(|| Self::col_of(&cur_row, self.cursor));
        let mut target = cur;
        let mut moved = 0;
        if rows_delta > 0 {
            while moved < rows_delta {
                match target.next {
                    Some(n) => target = rows::row_at(b, n, self.wrap),
                    None => break,
                }
                moved += 1;
            }
        } else {
            while moved < -rows_delta {
                match rows::prev_row(b, target.start, self.wrap) {
                    Some(p) => target = p,
                    None => break,
                }
                moved += 1;
            }
        }
        let pos = if moved == 0 {
            if rows_delta > 0 { b.len() } else { 0 }
        } else {
            Self::pos_for_col(&DisplayRow::build(b, target), col)
        };
        let soft_end = moved > 0 && Self::is_soft_end(target, pos);
        self.place_cursor(pos, select, soft_end, cx);
        self.goal_col = Some(col);
    }

    fn horizontal(&mut self, pos: u64, select: bool, cx: &mut Context<Self>) {
        self.goal_col = None;
        self.set_cursor(pos, select, cx);
    }

    fn move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        let sel = self.selection();
        let pos = if sel.is_empty() { Self::prev_char(self.buffer.read(cx), self.cursor) } else { sel.start };
        self.horizontal(pos, false, cx);
    }
    fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        let sel = self.selection();
        let pos = if sel.is_empty() { Self::next_char(self.buffer.read(cx), self.cursor) } else { sel.end };
        self.horizontal(pos, false, cx);
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let pos = Self::prev_char(self.buffer.read(cx), self.cursor);
        self.horizontal(pos, true, cx);
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let pos = Self::next_char(self.buffer.read(cx), self.cursor);
        self.horizontal(pos, true, cx);
    }
    fn word_left_a(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let pos = Self::word_left(self.buffer.read(cx), self.cursor);
        self.horizontal(pos, false, cx);
    }
    fn word_right_a(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let pos = Self::word_right(self.buffer.read(cx), self.cursor);
        self.horizontal(pos, false, cx);
    }
    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let pos = Self::word_left(self.buffer.read(cx), self.cursor);
        self.horizontal(pos, true, cx);
    }
    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let pos = Self::word_right(self.buffer.read(cx), self.cursor);
        self.horizontal(pos, true, cx);
    }
    fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        if !self.single_line {
            self.vertical(-1, false, cx);
        }
    }
    fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        if !self.single_line {
            self.vertical(1, false, cx);
        }
    }
    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, true, cx);
    }
    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, true, cx);
    }

    fn home_pos(&self, cx: &App) -> u64 {
        // Smart home: first non-blank of the row, or the row start.
        let b = self.buffer.read(cx);
        let r = self.cursor_span(b);
        let bytes = b.slice(r.start..r.end);
        let indent = bytes.iter().take_while(|c| **c == b' ' || **c == b'\t').count() as u64;
        if self.cursor == r.start + indent { r.start } else { r.start + indent }
    }
    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let pos = self.home_pos(cx);
        self.horizontal(pos, false, cx);
    }
    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        let pos = self.home_pos(cx);
        self.horizontal(pos, true, cx);
    }
    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.row_end(false, cx);
    }
    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.row_end(true, cx);
    }
    fn row_end(&mut self, select: bool, cx: &mut Context<Self>) {
        let r = self.cursor_span(self.buffer.read(cx));
        self.goal_col = None;
        self.place_cursor(r.end, select, Self::is_soft_end(r, r.end), cx);
    }
    fn doc_start(&mut self, _: &DocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(0, false, cx);
    }
    fn doc_end(&mut self, _: &DocEnd, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.buffer.read(cx).len();
        self.horizontal(len, false, cx);
    }
    fn select_doc_start(&mut self, _: &SelectDocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(0, true, cx);
    }
    fn select_doc_end(&mut self, _: &SelectDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.buffer.read(cx).len();
        self.horizontal(len, true, cx);
    }
    fn page(&mut self, down: bool, select: bool, cx: &mut Context<Self>) {
        let n = self.visible_rows().saturating_sub(1).max(1) as isize;
        self.scroll_rows(if down { n } else { -n }, cx);
        self.vertical(if down { n } else { -n }, select, cx);
    }
    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.page(false, false, cx);
    }
    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.page(true, false, cx);
    }
    fn select_page_up(&mut self, _: &SelectPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.page(false, true, cx);
    }
    fn select_page_down(&mut self, _: &SelectPageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.page(true, true, cx);
    }
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all_text(cx);
    }

    // ------------------------------------------------------------ editing

    pub fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        let range = self.marked.take().unwrap_or_else(|| self.selection());
        let before = self.cursor;
        let pos = self.buffer.update(cx, |b, cx| {
            let p = b.replace(range, text, before);
            cx.notify();
            p
        });
        self.goal_col = None;
        self.set_cursor(pos, false, cx);
        cx.emit(TextEvent::Edited);
    }

    /// Replace all `needle_len`-byte matches at `hits` with `text` (one undo step).
    pub fn replace_all(&mut self, hits: &[u64], needle_len: u64, text: &str, cx: &mut Context<Self>) {
        let before = self.cursor;
        let pos = self.buffer.update(cx, |b, cx| {
            let p = b.replace_all(hits, needle_len, text, before);
            cx.notify();
            p
        });
        self.highlight = None;
        self.goal_col = None;
        self.set_cursor(pos, false, cx);
        cx.emit(TextEvent::Edited);
    }

    fn delete_range(&mut self, range: Range<u64>, cx: &mut Context<Self>) {
        if range.is_empty() {
            return;
        }
        self.anchor = range.start;
        self.cursor = range.end;
        self.insert("", cx);
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let sel = self.selection();
        let range = if sel.is_empty() { Self::prev_char(self.buffer.read(cx), self.cursor)..self.cursor } else { sel };
        self.delete_range(range, cx);
    }
    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        let sel = self.selection();
        let range = if sel.is_empty() { self.cursor..Self::next_char(self.buffer.read(cx), self.cursor) } else { sel };
        self.delete_range(range, cx);
    }
    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let start = Self::word_left(self.buffer.read(cx), self.cursor);
        self.delete_range(start..self.cursor, cx);
    }
    fn delete_word_right(&mut self, _: &DeleteWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let end = Self::word_right(self.buffer.read(cx), self.cursor);
        self.delete_range(self.cursor..end, cx);
    }
    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if self.single_line {
            cx.emit(TextEvent::Submitted);
            return;
        }
        // Keep the indentation of the current line.
        let b = self.buffer.read(cx);
        let ls = rows::line_start(b, self.cursor);
        let head = b.slice(ls..self.cursor.min(ls + 256));
        let indent: String = head
            .iter()
            .take_while(|c| **c == b' ' || **c == b'\t')
            .map(|&c| c as char)
            .collect();
        let text = format!("{}{}", b.line_ending.as_str(), indent);
        self.insert(&text, cx);
    }
    fn tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        if self.single_line {
            cx.emit(TextEvent::TabbedOut);
        } else {
            self.insert("\t", cx);
        }
    }
    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.anchor != self.cursor && !self.single_line {
            self.anchor = self.cursor;
            cx.notify();
        } else {
            cx.emit(TextEvent::Cancelled);
        }
    }

    fn selected_text(&self, cx: &mut Context<Self>) -> Option<String> {
        let sel = self.selection();
        if sel.is_empty() {
            return None;
        }
        if sel.end - sel.start > MAX_CLIPBOARD {
            cx.emit(TextEvent::Message(tr!("Selection too large for the clipboard (max 64 MiB)").into()));
            return None;
        }
        Some(self.buffer.read(cx).text(sel))
    }

    fn line_range(&self, cx: &App) -> Range<u64> {
        let b = self.buffer.read(cx);
        let sel = self.selection();
        let start = rows::line_start(b, sel.start);
        let end = match b.find_byte(sel.end.max(start), b'\n', u64::MAX) {
            Some(p) => p + 1,
            None => b.len(),
        };
        start..end
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.selected_text(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(t));
        }
    }
    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.selected_text(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(t));
            self.insert("", cx);
        }
    }
    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
            let text = if self.single_line { text.replace(['\r', '\n'], " ") } else { text };
            self.insert(&text, cx);
        }
    }
    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        let pos = self.buffer.update(cx, |b, cx| {
            cx.notify();
            b.undo()
        });
        if let Some(p) = pos {
            self.set_cursor(p, false, cx);
            cx.emit(TextEvent::Edited);
        }
    }
    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        let pos = self.buffer.update(cx, |b, cx| {
            cx.notify();
            b.redo()
        });
        if let Some(p) = pos {
            self.set_cursor(p, false, cx);
            cx.emit(TextEvent::Edited);
        }
    }
    fn duplicate_line(&mut self, _: &DuplicateLine, _: &mut Window, cx: &mut Context<Self>) {
        if self.single_line {
            return;
        }
        let range = self.line_range(cx);
        let b = self.buffer.read(cx);
        let mut text = b.text(range.clone());
        if !text.ends_with('\n') {
            text.insert_str(0, b.line_ending.as_str());
        }
        let (c, a) = (self.cursor, self.anchor);
        let at = range.end;
        self.buffer.update(cx, |b, cx| {
            b.replace(at..at, &text, c);
            cx.notify();
        });
        self.cursor = c;
        self.anchor = a;
        cx.emit(TextEvent::Edited);
        cx.notify();
    }
    fn delete_line(&mut self, _: &DeleteLine, _: &mut Window, cx: &mut Context<Self>) {
        let range = self.line_range(cx);
        self.delete_range(range, cx);
    }

    // ------------------------------------------------------------ mouse

    /// Document offset under `p`, and whether it is the soft end of its row.
    fn hit(&self, p: Point<Pixels>) -> Option<(u64, bool)> {
        let l = self.layout.as_ref()?;
        let row = l
            .rows
            .iter()
            .rev()
            .find(|r| p.y >= r.y)
            .or(l.rows.first())?;
        let x = p.x - l.text_x + self.scroll_x;
        let disp = row.line.closest_index_for_x(x);
        let pos = row.row.offset_for_disp(disp).min(row.row.span.end);
        Some((pos, Self::is_soft_end(row.row.span, pos)))
    }

    fn on_mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        if let Some(l) = &self.layout
            && let Some(track) = l.track
                && track.contains(&e.position)
            {
                match l.thumb {
                    Some(thumb) if thumb.contains(&e.position) => {
                        self.thumb_drag = Some(e.position.y - thumb.top());
                    }
                    _ => {
                        let half = l.thumb.map(|t| t.size.height / 2.).unwrap_or(px(0.));
                        self.thumb_drag = Some(half);
                        self.drag_thumb(e.position.y, cx);
                    }
                }
                return;
            }
        let Some((pos, soft_end)) = self.hit(e.position) else { return };
        self.goal_col = None;
        self.at_row_end = false;
        match e.click_count {
            2 => {
                let b = self.buffer.read(cx);
                let start = Self::word_left(b, Self::next_char(b, pos));
                let end = Self::word_right(b, start);
                self.anchor = start;
                self.cursor = end;
                cx.notify();
            }
            3 => {
                self.anchor = pos;
                self.cursor = pos;
                let r = self.line_range(cx);
                self.anchor = r.start;
                self.cursor = r.end;
                cx.notify();
            }
            _ => {
                self.selecting = true;
                self.place_cursor(pos, e.modifiers.shift, soft_end, cx);
            }
        }
        cx.emit(TextEvent::CursorMoved);
    }

    fn drag_thumb(&mut self, y: Pixels, cx: &mut Context<Self>) {
        let (Some(l), Some(grab)) = (&self.layout, self.thumb_drag) else { return };
        let (Some(track), Some(thumb)) = (l.track, l.thumb) else { return };
        let span = (track.size.height - thumb.size.height).max(px(1.));
        let frac = (f32::from(y - grab - track.top()) / f32::from(span)).clamp(0.0, 1.0);
        let b = self.buffer.read(cx);
        let target = (b.len() as f64 * frac as f64) as u64;
        let top = rows::row_containing(b, target, self.wrap).start;
        self.top = self.clamp_top(b, top);
        cx.notify();
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.thumb_drag.is_some() {
            self.drag_thumb(e.position.y, cx);
            return;
        }
        if !self.selecting || e.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if let Some(l) = &self.layout {
            if e.position.y < l.bounds.top() {
                self.scroll_rows(-1, cx);
            } else if e.position.y > l.bounds.bottom() {
                self.scroll_rows(1, cx);
            }
        }
        if let Some((pos, soft_end)) = self.hit(e.position) {
            self.cursor = pos;
            self.at_row_end = soft_end;
            cx.emit(TextEvent::CursorMoved);
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
        self.thumb_drag = None;
    }

    fn on_scroll(&mut self, e: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lh = Settings::get(cx).line_height();
        let (dx, dy) = match e.delta {
            ScrollDelta::Lines(p) => (p.x * 3.0 * f32::from(lh), p.y * 3.0),
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y) / f32::from(lh)),
        };
        let (dx, dy) = if e.modifiers.shift { (dy * f32::from(lh), 0.0) } else { (dx, dy) };
        if dx != 0.0 {
            self.scroll_x = (self.scroll_x - px(dx)).max(px(0.));
            cx.notify();
        }
        self.wheel_rest -= dy;
        let whole = self.wheel_rest.trunc();
        if whole != 0.0 {
            self.wheel_rest -= whole;
            self.scroll_rows(whole as isize, cx);
        }
    }
}

impl EntityInputHandler for TextView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|_| 0..0)
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(&mut self, _: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        let text = if self.single_line { text.replace(['\r', '\n'], "") } else { text.to_string() };
        self.insert(&text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start = self.marked.as_ref().map(|m| m.start).unwrap_or(self.selection().start);
        self.insert(text, cx);
        self.marked = (!text.is_empty()).then(|| start..start + text.len() as u64);
    }

    fn bounds_for_range(&mut self, _: Range<usize>, _: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let l = self.layout.as_ref()?;
        let row = l.rows.iter().find(|r| {
            self.cursor >= r.row.span.start && self.cursor <= r.row.span.end
        })?;
        let x = l.text_x - self.scroll_x + row.line.x_for_index(row.row.disp_index(self.cursor));
        Some(Bounds::new(point(x, row.y), size(px(2.), l.line_height)))
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

impl Render for TextView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("text-view")
            .key_context("Editor")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .size_full()
            .overflow_hidden()
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::word_left_a))
            .on_action(cx.listener(Self::word_right_a))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::select_page_up))
            .on_action(cx.listener(Self::select_page_down))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::tab))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::duplicate_line))
            .on_action(cx.listener(Self::delete_line))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(TextElement { view: cx.entity() })
    }
}

// ---------------------------------------------------------------- element

struct TextElement {
    view: Entity<TextView>,
}

struct Prepaint {
    gutter_w: Pixels,
    line_numbers: Vec<(Pixels, ShapedLine, bool)>,
    selections: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    current_line: Option<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

fn run(len: usize, font: &gpui::Font, color: Hsla) -> TextRun {
    TextRun { len, font: font.clone(), color, background_color: None, underline: None, strikethrough: None }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let single = self.view.read(cx).single_line;
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = if single { Settings::get(cx).line_height().into() } else { relative(1.).into() };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let settings = Settings::get(cx);
        let font = settings.mono();
        let font_size = settings.font_size;
        let lh = settings.line_height();
        let line_numbers = settings.line_numbers;
        let view = self.view.read(cx);
        let single = view.single_line;
        let focused = view.focus.is_focused(window);
        let buffer = view.buffer.read(cx);
        let len = buffer.len();
        let ts = window.text_system().clone();

        let char_w = ts.shape_line("0".into(), font_size, &[run(1, &font, theme::text())], None).width;
        let visible_rows = ((bounds.size.height / lh).floor() as usize).max(1);

        // Word wrap width from the space right of the gutter (sized like below;
        // while lines are still being counted, assume ten digits).
        let wrap = if single || !settings.word_wrap {
            0
        } else {
            let digits = buffer.line_count().map_or(10, |n| n.to_string().len()).max(4);
            let gutter = if line_numbers { char_w * (digits as f32 + 2.5) } else { char_w * 0.5 };
            let usable = bounds.size.width - gutter - px(SCROLLBAR_W) - char_w;
            ((usable / char_w).floor() as usize).max(10)
        };
        // A new width moves the row breaks: start at the row holding the old top.
        let top = if wrap == view.wrap { view.top.min(len) } else { rows::row_containing(buffer, view.top.min(len), wrap).start };

        // Rows.
        let mut spans: Vec<RowSpan> = Vec::with_capacity(visible_rows + 1);
        let mut next = Some(top);
        while let Some(start) = next {
            if spans.len() > visible_rows {
                break;
            }
            let span = rows::row_at(buffer, start, wrap);
            next = span.next;
            spans.push(span);
        }

        // Line numbers: the first row's line is looked up in the index,
        // the rest are counted while walking.
        let show_numbers = line_numbers;
        let mut line_numbers = Vec::new();
        let gutter_w = if single || !show_numbers {
            px(0.)
        } else {
            let first_line = buffer.line_of_offset(spans[0].start);
            let digits = buffer
                .line_count()
                .map(|n| n.to_string().len())
                .unwrap_or(first_line.map(|l| (l + visible_rows as u64).to_string().len()).unwrap_or(0))
                .max(4);
            let w = char_w * (digits as f32 + 2.0);
            if let Some(mut ln) = first_line {
                let cursor_line_start = rows::line_start(buffer, view.cursor);
                for (i, s) in spans.iter().enumerate() {
                    if i > 0 && rows::is_line_start(buffer, s.start) {
                        ln += 1;
                    }
                    if rows::is_line_start(buffer, s.start) {
                        let label: SharedString = (ln + 1).to_string().into();
                        let active = s.start == cursor_line_start;
                        let color = if active { theme::gutter_active() } else { theme::gutter_text() };
                        let shaped = ts.shape_line(label.clone(), font_size, &[run(label.len(), &font, color)], None);
                        let x = w - char_w - shaped.width;
                        line_numbers.push((x, shaped, active));
                    } else {
                        line_numbers.push((px(-1.), ShapedLine::default(), false));
                    }
                }
            }
            w
        };

        let text_x = bounds.left() + gutter_w + if single { px(2.) } else { char_w * 0.5 };
        let has_scrollbar = !single && len > 0;
        let text_right = bounds.right() - if has_scrollbar { px(SCROLLBAR_W) } else { px(0.) };
        let mut painted = Vec::with_capacity(spans.len());
        for (i, span) in spans.into_iter().enumerate() {
            let row = DisplayRow::build(buffer, span);
            let text: SharedString = row.text.clone().into();
            let line = ts.shape_line(text, font_size, &[run(row.text.len(), &font, theme::text())], None);
            painted.push(PaintedRow { row, line, y: bounds.top() + lh * i as f32 });
        }

        // Horizontal autoscroll to keep the cursor visible.
        let cursor = view.cursor;
        let mut scroll_x = if wrap > 0 { px(0.) } else { view.scroll_x };
        let at_row_end = view.at_row_end;
        let cursor_row = painted
            .iter()
            .find(|r| at_row_end && TextView::is_soft_end(r.row.span, cursor))
            .or_else(|| {
                painted.iter().find(|r| {
                    cursor >= r.row.span.start
                        && (cursor < r.row.span.next.unwrap_or(u64::MAX) || r.row.span.next.is_none())
                })
            });
        if view.autoscroll
            && wrap == 0
            && let Some(r) = cursor_row
        {
            let cx_pos = r.line.x_for_index(r.row.disp_index(cursor));
            let width = text_right - text_x - char_w * 2.;
            if cx_pos < scroll_x {
                scroll_x = (cx_pos - char_w * 4.).max(px(0.));
            } else if cx_pos > scroll_x + width {
                scroll_x = cx_pos - width + char_w * 4.;
            }
        }

        // Selection and search highlight.
        let sel = view.selection();
        let mut selections = Vec::new();
        let add_range = |range: &Range<u64>, color: Hsla, selections: &mut Vec<PaintQuad>| {
            if range.is_empty() {
                return;
            }
            for r in &painted {
                let s = r.row.span;
                let row_end = s.next.unwrap_or(len);
                if range.end <= s.start || range.start >= row_end.max(s.start + 1) {
                    continue;
                }
                let a = r.line.x_for_index(r.row.disp_index(range.start.max(s.start)));
                let mut b = r.line.x_for_index(r.row.disp_index(range.end.min(s.end)));
                if range.end > s.end && s.next.is_some() && s.end < row_end {
                    b += char_w * 0.6; // the line break is selected
                }
                selections.push(fill(
                    Bounds::from_corners(
                        point(text_x - scroll_x + a, r.y),
                        point(text_x - scroll_x + b, r.y + lh),
                    ),
                    color,
                ));
            }
        };
        if let Some(h) = &view.highlight {
            add_range(h, theme::find_match(), &mut selections);
        }
        add_range(&sel, theme::selection(), &mut selections);

        let mut cursor_quad = None;
        let mut current_line = None;
        if let Some(r) = cursor_row {
            let x = text_x - scroll_x + r.line.x_for_index(r.row.disp_index(cursor));
            if focused {
                cursor_quad = Some(fill(Bounds::new(point(x, r.y), size(px(2.), lh)), theme::cursor()));
            }
            if !single && sel.is_empty() {
                current_line = Some(fill(
                    Bounds::new(point(bounds.left(), r.y), size(bounds.size.width, lh)),
                    theme::current_line(),
                ));
            }
        }

        // Scrollbar: position is proportional to the byte offset.
        let (track, thumb) = if has_scrollbar {
            let track = Bounds::new(
                point(bounds.right() - px(SCROLLBAR_W), bounds.top()),
                size(px(SCROLLBAR_W), bounds.size.height),
            );
            let shown = painted.last().map(|r| r.row.span.next.unwrap_or(len)).unwrap_or(len) - top;
            let frac_visible = (shown as f64 / len.max(1) as f64).clamp(0.0, 1.0) as f32;
            if frac_visible < 1.0 || top > 0 {
                let h = (track.size.height * frac_visible).max(px(MIN_THUMB)).min(track.size.height);
                let frac_top = (top as f64 / len.max(1) as f64) as f32;
                let y = track.top() + (track.size.height - h) * frac_top.clamp(0.0, 1.0);
                (Some(track), Some(Bounds::new(point(track.left() + px(2.), y), size(px(SCROLLBAR_W - 4.), h))))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        self.view.update(cx, |v, _| {
            v.wrap = wrap;
            v.top = top;
            v.scroll_x = scroll_x;
            v.autoscroll = false;
            v.layout = Some(Layout {
                bounds,
                text_x,
                line_height: lh,
                rows: painted,
                visible_rows,
                track,
                thumb,
            });
        });

        Prepaint { gutter_w, line_numbers, selections, cursor: cursor_quad, current_line }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        pp: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.view.read(cx).focus.clone();
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.view.clone()), cx);

        let single = self.view.read(cx).single_line;
        if !single {
            window.paint_quad(fill(bounds, theme::bg()));
        }
        if let Some(q) = pp.current_line.take() {
            window.paint_quad(q);
        }

        let view = self.view.read(cx);
        let Some(layout) = view.layout.as_ref() else { return };
        let lh = layout.line_height;
        let scroll_x = view.scroll_x;
        let text_x = layout.text_x;
        let thumb = layout.thumb;
        let dragging = view.thumb_drag.is_some();
        let text_clip = Bounds::from_corners(
            point(bounds.left() + pp.gutter_w, bounds.top()),
            point(layout.track.map(|t| t.left()).unwrap_or(bounds.right()), bounds.bottom()),
        );
        let rows: Vec<(Pixels, ShapedLine)> = layout.rows.iter().map(|r| (r.y, r.line.clone())).collect();

        window.with_content_mask(Some(gpui::ContentMask { bounds: text_clip }), |window| {
            for q in pp.selections.drain(..) {
                window.paint_quad(q);
            }
            for (y, line) in &rows {
                line.paint(point(text_x - scroll_x, *y), lh, window, cx).ok();
            }
            if let Some(c) = pp.cursor.take() {
                window.paint_quad(c);
            }
        });

        if !single {
            for ((x, line, _), (y, _)) in pp.line_numbers.iter().zip(rows.iter()) {
                if *x >= px(0.) {
                    line.paint(point(bounds.left() + *x, *y), lh, window, cx).ok();
                }
            }
            if let Some(t) = thumb {
                let color = if dragging { theme::scrollbar_thumb_active() } else { theme::scrollbar_thumb() };
                window.paint_quad(fill(t, color).corner_radii(px(3.)));
            }
        }
    }
}
