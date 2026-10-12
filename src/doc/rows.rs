//! Visual rows: a logical line, split into segments of at most `SEG` bytes.
//!
//! Everything is addressed by byte offset so we never need to know how many
//! lines precede a position. Very long lines (or binary data without
//! newlines) are cut into fixed segments so a row never costs more than a
//! few kilobytes to lay out. With word wrap (`wrap` > 0 columns) a row also
//! ends where it reaches that width, after the last space if there is one.
//! Rows are always found by walking forward from a line start, so the
//! breaks are the same whichever way we get there.

use std::sync::atomic::{AtomicUsize, Ordering};

use unicode_width::UnicodeWidthChar;

use super::buffer::Buffer;

pub const SEG: u64 = 2048;
static TAB: AtomicUsize = AtomicUsize::new(4);

pub fn tab_width() -> usize {
    TAB.load(Ordering::Relaxed)
}

pub fn set_tab_width(w: usize) {
    TAB.store(w.clamp(1, 16), Ordering::Relaxed);
}
const BACK_SCAN: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowSpan {
    pub start: u64,
    /// End of the row content, excluding the line terminator.
    pub end: u64,
    /// Start of the following row, `None` for the last row.
    pub next: Option<u64>,
}

pub fn row_at(buf: &Buffer, start: u64, wrap: usize) -> RowSpan {
    let span = segment_at(buf, start);
    if wrap == 0 {
        return span;
    }
    wrap_break(buf, span, wrap).map_or(span, |end| RowSpan { start, end, next: Some(end) })
}

/// Where a row of more than `wrap` columns breaks: after the last space that
/// still fits (not inside the indentation), else at the width. `None` if the
/// row fits.
fn wrap_break(buf: &Buffer, span: RowSpan, wrap: usize) -> Option<u64> {
    // Every character takes at least one column and at most four bytes, so
    // this much is enough to find the break.
    let limit = span.end.min(span.start + 4 * (wrap as u64 + 1));
    let bytes = buf.slice(span.start..limit);
    let tab = tab_width();
    let (mut col, mut pos, mut after_space, mut text_seen) = (0usize, 0u64, None, false);
    for chunk in bytes.utf8_chunks() {
        let chars = chunk.valid().chars().map(|c| (c, c.len_utf8())).chain(chunk.invalid().iter().map(|_| ('\u{FFFD}', 1)));
        for (c, n) in chars {
            let w = if c == '\t' { tab - col % tab } else { columns(c) };
            // The first character always fits, so every row makes progress.
            if col + w > wrap && pos > 0 {
                return Some(span.start + after_space.unwrap_or(pos));
            }
            col += w;
            pos += n as u64;
            if c == ' ' || c == '\t' {
                if text_seen {
                    after_space = Some(pos);
                }
            } else {
                text_seen = true;
            }
        }
    }
    None
}

fn segment_at(buf: &Buffer, start: u64) -> RowSpan {
    let len = buf.len();
    match buf.find_byte(start, b'\n', SEG) {
        Some(p) => {
            let end = if p > start && buf.byte_at(p - 1) == Some(b'\r') { p - 1 } else { p };
            RowSpan { start, end, next: Some(p + 1) }
        }
        None if start + SEG >= len => RowSpan { start, end: len, next: None },
        None => {
            let mut e = start + SEG;
            // Do not split a UTF-8 sequence.
            for _ in 0..3 {
                match buf.byte_at(e) {
                    Some(b) if b & 0xC0 == 0x80 && e > start + 1 => e -= 1,
                    _ => break,
                }
            }
            RowSpan { start, end: e, next: Some(e) }
        }
    }
}

pub fn is_line_start(buf: &Buffer, offset: u64) -> bool {
    offset == 0 || buf.byte_at(offset - 1) == Some(b'\n')
}

/// Start of the logical line containing `offset` (bounded backward scan).
pub fn line_start(buf: &Buffer, offset: u64) -> u64 {
    match buf.rfind_byte(offset, b'\n', BACK_SCAN) {
        Some(p) => p + 1,
        None => offset.saturating_sub(BACK_SCAN),
    }
}

pub fn row_containing(buf: &Buffer, offset: u64, wrap: usize) -> RowSpan {
    let offset = offset.min(buf.len());
    let mut r = row_at(buf, line_start(buf, offset), wrap);
    while let Some(n) = r.next {
        if n > offset {
            break;
        }
        r = row_at(buf, n, wrap);
    }
    r
}

pub fn prev_row(buf: &Buffer, row_start: u64, wrap: usize) -> Option<RowSpan> {
    (row_start > 0).then(|| row_containing(buf, row_start - 1, wrap))
}

/// Monospace columns a character takes: two for wide (CJK, most emoji),
/// none for combining marks. C0 controls and DEL are shown as one symbol,
/// C1 controls as `<9F>`.
fn columns(c: char) -> usize {
    match c as u32 {
        0..0x20 | 0x7f => 1,
        0x80..=0x9f => 4,
        _ => c.width().unwrap_or(1),
    }
}

/// A row prepared for display: control characters made visible, tabs
/// expanded, invalid UTF-8 replaced, with a map back to byte offsets.
pub struct DisplayRow {
    pub span: RowSpan,
    pub text: String,
    /// `(source offset relative to span.start, display byte index)` for each
    /// character boundary, including the end.
    pub map: Vec<(u32, u32)>,
}

impl DisplayRow {
    pub fn build(buf: &Buffer, span: RowSpan) -> DisplayRow {
        let bytes = buf.slice(span.start..span.end);
        let mut text = String::with_capacity(bytes.len());
        let mut map = Vec::with_capacity(bytes.len() + 1);
        let mut src = 0u32;
        let mut col = 0usize;
        let tab = tab_width();
        for chunk in bytes.utf8_chunks() {
            for ch in chunk.valid().chars() {
                map.push((src, text.len() as u32));
                match ch {
                    '\t' => {
                        let n = tab - col % tab;
                        text.extend(std::iter::repeat_n(' ', n));
                        col += n;
                    }
                    c if (c as u32) < 0x20 => {
                        text.push(char::from_u32(0x2400 + c as u32).unwrap_or('?'));
                        col += 1;
                    }
                    '\u{7f}' => {
                        text.push('\u{2421}');
                        col += 1;
                    }
                    // C1 controls have no symbol in Unicode: show the code, like vim.
                    c @ '\u{80}'..='\u{9f}' => {
                        text.push_str(&format!("<{:02X}>", c as u32));
                        col += 4;
                    }
                    c => {
                        text.push(c);
                        col += columns(c);
                    }
                }
                src += ch.len_utf8() as u32;
            }
            for _ in chunk.invalid() {
                map.push((src, text.len() as u32));
                text.push('\u{FFFD}');
                src += 1;
                col += 1;
            }
        }
        map.push((src, text.len() as u32));
        DisplayRow { span, text, map }
    }

    /// Display index for a document offset within (or at the end of) this row.
    pub fn disp_index(&self, offset: u64) -> usize {
        let rel = offset.saturating_sub(self.span.start).min(u32::MAX as u64) as u32;
        let i = self.map.partition_point(|&(s, _)| s <= rel).saturating_sub(1);
        self.map[i].1 as usize
    }

    /// Document offset for a display index (snapped to a character boundary).
    pub fn offset_for_disp(&self, disp: usize) -> u64 {
        let i = self.map.partition_point(|&(_, d)| (d as usize) <= disp).saturating_sub(1);
        self.span.start + self.map[i].0 as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_and_navigation() {
        let b = Buffer::from_text("ab\r\ncd\n\nlast");
        let r0 = row_at(&b, 0, 0);
        assert_eq!((r0.end, r0.next), (2, Some(4)));
        let r1 = row_at(&b, 4, 0);
        assert_eq!((r1.end, r1.next), (6, Some(7)));
        let r3 = row_at(&b, 8, 0);
        assert_eq!((r3.end, r3.next), (12, None));
        assert_eq!(row_containing(&b, 5, 0).start, 4);
        assert_eq!(row_containing(&b, 6, 0).start, 4);
        assert_eq!(row_containing(&b, 7, 0).start, 7);
        assert_eq!(prev_row(&b, 8, 0).unwrap().start, 7);
        assert_eq!(prev_row(&b, 4, 0).unwrap().start, 0);
        assert!(prev_row(&b, 0, 0).is_none());
    }

    #[test]
    fn long_lines_are_segmented() {
        let text = "x".repeat(SEG as usize * 2 + 10);
        let b = Buffer::from_text(&text);
        let r = row_at(&b, 0, 0);
        assert_eq!(r.next, Some(SEG));
        assert_eq!(row_containing(&b, SEG * 2 + 3, 0).start, SEG * 2);
        assert_eq!(prev_row(&b, SEG * 2, 0).unwrap().start, SEG);
    }

    #[test]
    fn word_wrap() {
        let b = Buffer::from_text("hello world foo\nabcdefghij\nx");
        // Breaks after the last space that fits.
        let r = row_at(&b, 0, 12);
        assert_eq!((r.start, r.end, r.next), (0, 12, Some(12)));
        let r = row_at(&b, 12, 12);
        assert_eq!((r.end, r.next), (15, Some(16)));
        // No space: hard break at the width.
        let r = row_at(&b, 16, 4);
        assert_eq!((r.end, r.next), (20, Some(20)));
        assert_eq!(row_containing(&b, 23, 4).start, 20);
        assert_eq!(row_containing(&b, 24, 4).start, 24);
        assert_eq!(prev_row(&b, 24, 4).unwrap().start, 20);
        assert_eq!(prev_row(&b, 12, 12).unwrap().start, 0);
        // Fits: unchanged.
        assert_eq!(row_at(&b, 27, 4), row_at(&b, 27, 0));
        // Multi-byte characters count as one column and are never split.
        let u = Buffer::from_text("ääää");
        assert_eq!(row_at(&u, 0, 3).end, 6);
        // Wide characters take two columns, combining marks none.
        let w = Buffer::from_text("日本語です");
        assert_eq!(row_at(&w, 0, 5).end, 6);
        let e = Buffer::from_text("e\u{301}e\u{301}e\u{301}x");
        assert_eq!(row_at(&e, 0, 3).end, 9);
        // Tabs count to the next tab stop; no break inside the indentation.
        let t = Buffer::from_text("\tabcdef");
        assert_eq!(row_at(&t, 0, 6).end, 3);
        let t = Buffer::from_text("  ab cd");
        assert_eq!(row_at(&t, 0, 6).end, 5);
    }

    #[test]
    fn display_mapping() {
        let b = Buffer::from_text("a\tb\u{0}é");
        let d = DisplayRow::build(&b, row_at(&b, 0, 0));
        assert_eq!(d.text, "a   b\u{2400}é");
        assert_eq!(d.disp_index(2), 4);
        assert_eq!(d.offset_for_disp(2), 1);
        assert_eq!(d.disp_index(b.len()), d.text.len());
        // C1 controls (e.g. 0x96 read as ISO-8859-15) become "<96>".
        let c1 = Buffer::from_text("a\u{96}b");
        let d = DisplayRow::build(&c1, row_at(&c1, 0, 0));
        assert_eq!(d.text, "a<96>b");
        assert_eq!(d.disp_index(3), 5);
        assert_eq!(d.offset_for_disp(3), 1);
        assert_eq!(row_at(&c1, 0, 4).end, 1);
    }
}
