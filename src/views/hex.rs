//! Hex view: 16 bytes per row, addressed by row number (u64), so any offset
//! of any file size is reachable instantly.

use gpui::{
    App, Bounds, ClipboardItem, Context, Element, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render, ScrollDelta,
    ScrollWheelEvent, ShapedLine, SharedString, Style, TextRun, Window, div, fill, point,
    prelude::*, px, relative, size,
};

use super::JumpTo;
use super::text::{
    Copy, DocEnd, DocStart, End, Home, MoveDown, MoveLeft, MoveRight, MoveUp, PageDown, PageUp,
    SelectAll, SelectDown, SelectLeft, SelectRight, SelectUp,
};
use crate::doc::Buffer;
use crate::theme::{self, Settings};

const PER_ROW: u64 = 16;
const SCROLLBAR_W: f32 = 12.0;

pub struct HexView {
    buffer: Entity<Buffer>,
    focus: FocusHandle,
    top_row: u64,
    cursor: u64,
    anchor: u64,
    selecting: bool,
    thumb_drag: Option<Pixels>,
    wheel_rest: f32,
    layout: Option<HexLayout>,
}

struct HexLayout {
    bounds: Bounds<Pixels>,
    char_w: Pixels,
    line_height: Pixels,
    offset_cols: usize,
    visible_rows: u64,
    track: Option<Bounds<Pixels>>,
    thumb: Option<Bounds<Pixels>>,
}

impl EventEmitter<JumpTo> for HexView {}

impl Focusable for HexView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn hex_col(offset_cols: usize, i: usize) -> usize {
    offset_cols + 2 + i * 3 + usize::from(i >= 8)
}

fn ascii_col(offset_cols: usize, i: usize) -> usize {
    hex_col(offset_cols, 16) + 1 + i
}

impl HexView {
    pub fn new(buffer: Entity<Buffer>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&buffer, |_, _, cx| cx.notify()).detach();
        HexView {
            buffer,
            focus: cx.focus_handle(),
            top_row: 0,
            cursor: 0,
            anchor: 0,
            selecting: false,
            thumb_drag: None,
            wheel_rest: 0.0,
            layout: None,
        }
    }

    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    pub fn selection(&self) -> std::ops::Range<u64> {
        self.cursor.min(self.anchor)..self.cursor.max(self.anchor) + 1
    }

    fn len(&self, cx: &App) -> u64 {
        self.buffer.read(cx).len()
    }

    fn rows(&self, cx: &App) -> u64 {
        self.len(cx).div_ceil(PER_ROW).max(1)
    }

    fn visible(&self) -> u64 {
        self.layout.as_ref().map(|l| l.visible_rows).unwrap_or(40).max(1)
    }

    fn max_top(&self, cx: &App) -> u64 {
        self.rows(cx).saturating_sub(self.visible())
    }

    pub fn go_to(&mut self, offset: u64, cx: &mut Context<Self>) {
        let last = self.len(cx).saturating_sub(1);
        self.cursor = offset.min(last);
        self.anchor = self.cursor;
        let row = self.cursor / PER_ROW;
        self.top_row = row.saturating_sub(self.visible() / 3).min(self.max_top(cx));
        cx.notify();
    }

    fn set_cursor(&mut self, pos: u64, select: bool, cx: &mut Context<Self>) {
        let last = self.len(cx).saturating_sub(1);
        self.cursor = pos.min(last);
        if !select {
            self.anchor = self.cursor;
        }
        let row = self.cursor / PER_ROW;
        let vis = self.visible();
        if row < self.top_row {
            self.top_row = row;
        } else if row >= self.top_row + vis {
            self.top_row = row + 1 - vis;
        }
        cx.notify();
    }

    fn step(&mut self, delta: i64, select: bool, cx: &mut Context<Self>) {
        let pos = if delta < 0 {
            self.cursor.saturating_sub(delta.unsigned_abs())
        } else {
            self.cursor.saturating_add(delta as u64)
        };
        self.set_cursor(pos, select, cx);
    }

    fn scroll(&mut self, rows: i64, cx: &mut Context<Self>) {
        let max = self.max_top(cx);
        self.top_row = if rows < 0 {
            self.top_row.saturating_sub(rows.unsigned_abs())
        } else {
            self.top_row.saturating_add(rows as u64).min(max)
        };
        cx.notify();
    }

    fn hit(&self, p: Point<Pixels>, cx: &App) -> Option<u64> {
        let l = self.layout.as_ref()?;
        let row = self.top_row + (f32::from(p.y - l.bounds.top()) / f32::from(l.line_height)).max(0.0) as u64;
        let col = (f32::from(p.x - l.bounds.left() - l.char_w * 0.5) / f32::from(l.char_w)).max(0.0) as usize;
        let i = if col >= ascii_col(l.offset_cols, 0) {
            (col - ascii_col(l.offset_cols, 0)).min(15)
        } else {
            (0..16).rev().find(|&i| col >= hex_col(l.offset_cols, i)).unwrap_or(0)
        };
        Some((row * PER_ROW + i as u64).min(self.len(cx).saturating_sub(1)))
    }

    fn drag_thumb(&mut self, y: Pixels, cx: &mut Context<Self>) {
        let (Some(l), Some(grab)) = (&self.layout, self.thumb_drag) else { return };
        let (Some(track), Some(thumb)) = (l.track, l.thumb) else { return };
        let span = (track.size.height - thumb.size.height).max(px(1.));
        let frac = (f32::from(y - grab - track.top()) / f32::from(span)).clamp(0.0, 1.0) as f64;
        self.top_row = ((self.max_top(cx) as f64) * frac) as u64;
        cx.notify();
    }

    fn on_mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        if let Some(l) = &self.layout
            && let Some(track) = l.track
            && track.contains(&e.position)
        {
            match l.thumb {
                Some(t) if t.contains(&e.position) => self.thumb_drag = Some(e.position.y - t.top()),
                _ => {
                    self.thumb_drag = Some(l.thumb.map(|t| t.size.height / 2.).unwrap_or(px(0.)));
                    self.drag_thumb(e.position.y, cx);
                }
            }
            return;
        }
        if let Some(pos) = self.hit(e.position, cx) {
            self.selecting = true;
            self.set_cursor(pos, e.modifiers.shift, cx);
            if e.click_count == 2 {
                cx.emit(JumpTo(pos));
            }
        }
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.thumb_drag.is_some() {
            self.drag_thumb(e.position.y, cx);
        } else if self.selecting && e.pressed_button == Some(MouseButton::Left)
            && let Some(pos) = self.hit(e.position, cx)
        {
            self.set_cursor(pos, true, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
        self.thumb_drag = None;
    }

    fn on_scroll(&mut self, e: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lh = f32::from(Settings::get(cx).line_height());
        let dy = match e.delta {
            ScrollDelta::Lines(p) => p.y * 3.0,
            ScrollDelta::Pixels(p) => f32::from(p.y) / lh,
        };
        self.wheel_rest -= dy;
        let whole = self.wheel_rest.trunc();
        if whole != 0.0 {
            self.wheel_rest -= whole;
            self.scroll(whole as i64, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let sel = self.selection();
        if sel.end - sel.start > 16 << 20 {
            return;
        }
        let bytes = self.buffer.read(cx).slice(sel);
        let s: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
        cx.write_to_clipboard(ClipboardItem::new_string(s.join(" ")));
    }
}

impl Render for HexView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = |v: &HexView| v.visible().saturating_sub(1) as i64 * PER_ROW as i64;
        div()
            .id("hex-view")
            .key_context("HexView")
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|v, _: &MoveLeft, _, cx| v.step(-1, false, cx)))
            .on_action(cx.listener(|v, _: &MoveRight, _, cx| v.step(1, false, cx)))
            .on_action(cx.listener(|v, _: &MoveUp, _, cx| v.step(-16, false, cx)))
            .on_action(cx.listener(|v, _: &MoveDown, _, cx| v.step(16, false, cx)))
            .on_action(cx.listener(|v, _: &SelectLeft, _, cx| v.step(-1, true, cx)))
            .on_action(cx.listener(|v, _: &SelectRight, _, cx| v.step(1, true, cx)))
            .on_action(cx.listener(|v, _: &SelectUp, _, cx| v.step(-16, true, cx)))
            .on_action(cx.listener(|v, _: &SelectDown, _, cx| v.step(16, true, cx)))
            .on_action(cx.listener(move |v, _: &PageUp, _, cx| {
                let p = page(v);
                v.scroll(-p / 16, cx);
                v.step(-p, false, cx)
            }))
            .on_action(cx.listener(move |v, _: &PageDown, _, cx| {
                let p = page(v);
                v.scroll(p / 16, cx);
                v.step(p, false, cx)
            }))
            .on_action(cx.listener(|v, _: &Home, _, cx| {
                let p = v.cursor - v.cursor % PER_ROW;
                v.set_cursor(p, false, cx)
            }))
            .on_action(cx.listener(|v, _: &End, _, cx| {
                let p = v.cursor - v.cursor % PER_ROW + PER_ROW - 1;
                v.set_cursor(p, false, cx)
            }))
            .on_action(cx.listener(|v, _: &DocStart, _, cx| v.set_cursor(0, false, cx)))
            .on_action(cx.listener(|v, _: &DocEnd, _, cx| v.set_cursor(u64::MAX, false, cx)))
            .on_action(cx.listener(|v, _: &SelectAll, _, cx| {
                v.anchor = 0;
                v.cursor = v.len(cx).saturating_sub(1);
                cx.notify()
            }))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(HexElement { view: cx.entity() })
    }
}

struct HexElement {
    view: Entity<HexView>,
}

struct HexPrepaint {
    lines: Vec<(Pixels, ShapedLine)>,
    quads: Vec<PaintQuad>,
    thumb: Option<Bounds<Pixels>>,
}

impl IntoElement for HexElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

fn run(len: usize, font: &gpui::Font, color: Hsla) -> TextRun {
    TextRun { len, font: font.clone(), color, background_color: None, underline: None, strikethrough: None }
}

impl Element for HexElement {
    type RequestLayoutState = ();
    type PrepaintState = HexPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
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
    ) -> HexPrepaint {
        let s = Settings::get(cx);
        let font = s.mono();
        let fs = s.font_size;
        let lh = s.line_height();
        let ts = window.text_system().clone();
        let char_w = ts.shape_line("0".into(), fs, &[run(1, &font, theme::text())], None).width;

        let view = self.view.read(cx);
        let buffer = view.buffer.read(cx);
        let len = buffer.len();
        let offset_cols = if len > u32::MAX as u64 { 16 } else { 8 };
        let visible_rows = ((bounds.size.height / lh).floor() as u64).max(1);
        let total_rows = len.div_ceil(PER_ROW).max(1);
        let top = view.top_row.min(total_rows.saturating_sub(1));
        let left = bounds.left() + char_w * 0.5;

        let sel = if len == 0 { 0..0 } else { view.selection() };
        let focused = view.focus.is_focused(window);
        let mut lines = Vec::new();
        let mut quads = Vec::new();

        for r in 0..=visible_rows {
            let row = top + r;
            let start = row * PER_ROW;
            if start >= len {
                break;
            }
            let bytes = buffer.slice(start..(start + PER_ROW).min(len));
            let y = bounds.top() + lh * r as f32;

            let mut text = String::with_capacity(96);
            let mut runs = Vec::new();
            let off = format!("{start:0width$X}  ", width = offset_cols);
            runs.push(run(off.len(), &font, theme::syn_offset()));
            text.push_str(&off);
            for i in 0..16 {
                let piece = match bytes.get(i) {
                    Some(b) => format!("{b:02X} "),
                    None => "   ".to_string(),
                };
                let piece = if i == 7 { format!("{piece} ") } else { piece };
                let color = match bytes.get(i) {
                    Some(0) => theme::text_faint(),
                    Some(b) if b.is_ascii_graphic() || *b == b' ' => theme::text(),
                    Some(_) => theme::syn_number(),
                    None => theme::text(),
                };
                runs.push(run(piece.len(), &font, color));
                text.push_str(&piece);
            }
            text.push(' ');
            runs.push(run(1, &font, theme::text()));
            for &b in bytes.iter() {
                let (ch, color) = if b.is_ascii_graphic() || b == b' ' {
                    (b as char, theme::syn_string())
                } else {
                    ('.', theme::text_faint())
                };
                text.push(ch);
                runs.push(run(1, &font, color));
            }

            // Selection highlight in both columns.
            for i in 0..bytes.len() {
                let o = start + i as u64;
                if o >= sel.start && o < sel.end {
                    let color = if o == view.cursor && focused { theme::accent() } else { theme::selection() };
                    let hx = left + char_w * hex_col(offset_cols, i) as f32;
                    quads.push(fill(Bounds::new(point(hx, y), size(char_w * 2., lh)), color));
                    let ax = left + char_w * ascii_col(offset_cols, i) as f32;
                    quads.push(fill(Bounds::new(point(ax, y), size(char_w, lh)), color));
                }
            }

            let shaped = ts.shape_line(SharedString::from(text), fs, &runs, None);
            lines.push((y, shaped));
        }

        let track = Bounds::new(point(bounds.right() - px(SCROLLBAR_W), bounds.top()), size(px(SCROLLBAR_W), bounds.size.height));
        let thumb = (total_rows > visible_rows).then(|| {
            let h = (track.size.height * (visible_rows as f32 / total_rows as f32)).max(px(28.));
            let max_top = total_rows.saturating_sub(visible_rows).max(1);
            let frac = (top as f64 / max_top as f64).clamp(0.0, 1.0) as f32;
            Bounds::new(point(track.left() + px(2.), track.top() + (track.size.height - h) * frac), size(px(SCROLLBAR_W - 4.), h))
        });

        self.view.update(cx, |v, _| {
            v.top_row = top;
            v.layout = Some(HexLayout {
                bounds,
                char_w,
                line_height: lh,
                offset_cols,
                visible_rows,
                track: thumb.map(|_| track),
                thumb,
            });
        });
        HexPrepaint { lines, quads, thumb }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        pp: &mut HexPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_quad(fill(bounds, theme::bg()));
        let lh = Settings::get(cx).line_height();
        let char_w = self.view.read(cx).layout.as_ref().map(|l| l.char_w).unwrap_or(px(8.));
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for q in pp.quads.drain(..) {
                window.paint_quad(q);
            }
            for (y, line) in &pp.lines {
                line.paint(point(bounds.left() + char_w * 0.5, *y), lh, window, cx).ok();
            }
            if let Some(t) = pp.thumb {
                window.paint_quad(fill(t, theme::scrollbar_thumb()).corner_radii(px(3.)));
            }
        });
    }
}
