//! Rendered Markdown view.
//!
//! The document is parsed once (in the background for large files) into a
//! flat list of blocks, which is then rendered through a virtualized
//! `list()` so only the visible blocks are laid out.

use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Font, FontFallbacks,
    FontStyle, FontWeight, Hsla, InteractiveText, IntoElement, ListAlignment, ListOffset, ListState,
    MouseButton, Render, StrikethroughStyle, StyledText, Task, TextRun, UnderlineStyle, WeakEntity,
    Window, div, list, prelude::*, px,
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use super::JumpTo;
use super::text::{DocEnd, DocStart, MoveDown, MoveUp, PageDown, PageUp};
use crate::doc::Buffer;
use crate::theme::{self, Settings};

/// Only this much of a document is rendered.
const MAX_RENDER: u64 = 16 << 20;
/// Above this size parsing happens off the main thread.
const SYNC_LIMIT: usize = 1 << 20;
const BODY_FAMILY: &str = "DejaVu Sans";
const BODY_SIZE: f32 = 15.0;
const CONTENT_W: f32 = 860.0;

// ---------------------------------------------------------------- model

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub link: bool,
}

/// Formatted text: a string plus style runs covering it completely.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inline {
    pub text: String,
    pub runs: Vec<(usize, SpanStyle)>,
    pub links: Vec<(Range<usize>, String)>,
}

impl Inline {
    fn push(&mut self, s: &str, style: SpanStyle) {
        if s.is_empty() {
            return;
        }
        self.text.push_str(s);
        match self.runs.last_mut() {
            Some((len, st)) if *st == style => *len += s.len(),
            _ => self.runs.push((s.len(), style)),
        }
    }

    fn trim_end(&mut self) {
        while self.text.ends_with(['\n', ' ']) {
            self.text.pop();
            if let Some((len, _)) = self.runs.last_mut() {
                *len -= 1;
                if *len == 0 {
                    self.runs.pop();
                }
            }
        }
        let n = self.text.len();
        for (r, _) in &mut self.links {
            r.end = r.end.min(n);
            r.start = r.start.min(r.end);
        }
    }

    fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BlockKind {
    Heading(u8, Inline),
    Paragraph(Inline),
    Code { lang: String, text: String },
    Rule,
    Table { header: Vec<Inline>, rows: Vec<Vec<Inline>> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub kind: BlockKind,
    /// Byte offset of the block in the source.
    pub offset: usize,
    /// Nesting depth of lists.
    pub indent: u8,
    /// Nesting depth of block quotes.
    pub quote: u8,
    /// List marker ("•", "3.", "☑") shown before the first block of an item.
    pub marker: Option<String>,
}

enum Ctx {
    Para,
    Heading(u8),
    Cell,
}

/// A table being read: header cells, finished rows, the row in progress,
/// whether we are in the header, and where the table starts.
type TableState = (Vec<Inline>, Vec<Vec<Inline>>, Vec<Inline>, bool, usize);

struct Converter {
    blocks: Vec<Block>,
    inline: Option<(Inline, Ctx, usize)>,
    style: SpanStyle,
    bold: u32,
    italic: u32,
    strike: u32,
    links: Vec<(usize, String)>,
    lists: Vec<Option<u64>>,
    quote: u8,
    marker: Option<String>,
    item_offset: usize,
    code: Option<(String, String, usize)>,
    table: Option<TableState>,
}

impl Converter {
    fn new() -> Self {
        Converter {
            blocks: Vec::new(),
            inline: None,
            style: SpanStyle::default(),
            bold: 0,
            italic: 0,
            strike: 0,
            links: Vec::new(),
            lists: Vec::new(),
            quote: 0,
            marker: None,
            item_offset: 0,
            code: None,
            table: None,
        }
    }

    fn restyle(&mut self) {
        self.style = SpanStyle {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            code: false,
            link: !self.links.is_empty(),
        };
    }

    fn push_block(&mut self, kind: BlockKind, offset: usize) {
        let marker = self.marker.take();
        let offset = if marker.is_some() { self.item_offset.min(offset) } else { offset };
        self.blocks.push(Block {
            kind,
            offset,
            indent: self.lists.len() as u8,
            quote: self.quote,
            marker,
        });
    }

    /// Inline content outside a paragraph (tight list items) opens an implicit one.
    fn inline_mut(&mut self, offset: usize) -> &mut Inline {
        if self.inline.is_none() {
            self.inline = Some((Inline::default(), Ctx::Para, offset));
        }
        &mut self.inline.as_mut().unwrap().0
    }

    fn text(&mut self, s: &str, style: SpanStyle, offset: usize) {
        if let Some((_, text, _)) = &mut self.code {
            text.push_str(s);
            return;
        }
        self.inline_mut(offset).push(s, style);
    }

    fn flush(&mut self) {
        let Some((mut inline, ctx, offset)) = self.inline.take() else { return };
        inline.trim_end();
        match ctx {
            Ctx::Cell => {
                if let Some(t) = &mut self.table {
                    t.2.push(inline);
                }
            }
            Ctx::Heading(l) => self.push_block(BlockKind::Heading(l, inline), offset),
            Ctx::Para => {
                if !inline.is_empty() || self.marker.is_some() {
                    self.push_block(BlockKind::Paragraph(inline), offset);
                }
            }
        }
    }

    fn event(&mut self, ev: Event, range: Range<usize>) {
        let at = range.start;
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    self.flush();
                    self.inline = Some((Inline::default(), Ctx::Para, at));
                }
                Tag::Heading { level, .. } => {
                    self.flush();
                    self.inline = Some((Inline::default(), Ctx::Heading(level as u8), at));
                }
                Tag::BlockQuote(_) => {
                    self.flush();
                    self.quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    self.flush();
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => l.split_whitespace().next().unwrap_or("").to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    self.code = Some((lang, String::new(), at));
                }
                Tag::HtmlBlock => {
                    self.flush();
                    self.code = Some(("html".into(), String::new(), at));
                }
                Tag::List(start) => {
                    self.flush();
                    self.lists.push(start);
                }
                Tag::Item => {
                    self.flush();
                    let marker = match self.lists.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}.");
                            *n += 1;
                            m
                        }
                        _ => ["•", "◦", "▪"][(self.lists.len().saturating_sub(1)) % 3].to_string(),
                    };
                    self.marker = Some(marker);
                    self.item_offset = at;
                }
                Tag::Table(_) => {
                    self.flush();
                    self.table = Some((Vec::new(), Vec::new(), Vec::new(), false, at));
                }
                Tag::TableHead => {
                    if let Some(t) = &mut self.table {
                        t.3 = true;
                    }
                }
                Tag::TableRow => {}
                Tag::TableCell => self.inline = Some((Inline::default(), Ctx::Cell, at)),
                Tag::Emphasis => {
                    self.italic += 1;
                    self.restyle();
                }
                Tag::Strong => {
                    self.bold += 1;
                    self.restyle();
                }
                Tag::Strikethrough => {
                    self.strike += 1;
                    self.restyle();
                }
                Tag::Link { dest_url, .. } => {
                    let start = self.inline_mut(at).text.len();
                    self.links.push((start, dest_url.to_string()));
                    self.restyle();
                }
                Tag::Image { .. } => {
                    let style = self.style;
                    self.text("🖼 ", style, at);
                }
                Tag::FootnoteDefinition(name) => {
                    self.flush();
                    let style = SpanStyle { code: true, ..Default::default() };
                    self.inline = Some((Inline::default(), Ctx::Para, at));
                    self.text(&format!("[^{name}]: "), style, at);
                }
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Paragraph | TagEnd::Heading(_) => self.flush(),
                TagEnd::BlockQuote(_) => {
                    self.flush();
                    self.quote = self.quote.saturating_sub(1);
                }
                TagEnd::CodeBlock | TagEnd::HtmlBlock => {
                    if let Some((lang, mut text, offset)) = self.code.take() {
                        while text.ends_with('\n') {
                            text.pop();
                        }
                        self.push_block(BlockKind::Code { lang, text }, offset);
                    }
                }
                TagEnd::List(_) => {
                    self.flush();
                    self.lists.pop();
                }
                TagEnd::Item => {
                    self.flush();
                    self.marker = None;
                }
                TagEnd::TableCell => self.flush(),
                TagEnd::TableHead => {
                    if let Some(t) = &mut self.table {
                        t.0 = std::mem::take(&mut t.2);
                        t.3 = false;
                    }
                }
                TagEnd::TableRow => {
                    if let Some(t) = &mut self.table {
                        let row = std::mem::take(&mut t.2);
                        t.1.push(row);
                    }
                }
                TagEnd::Table => {
                    if let Some((header, rows, _, _, offset)) = self.table.take() {
                        self.push_block(BlockKind::Table { header, rows }, offset);
                    }
                }
                TagEnd::Emphasis => {
                    self.italic = self.italic.saturating_sub(1);
                    self.restyle();
                }
                TagEnd::Strong => {
                    self.bold = self.bold.saturating_sub(1);
                    self.restyle();
                }
                TagEnd::Strikethrough => {
                    self.strike = self.strike.saturating_sub(1);
                    self.restyle();
                }
                TagEnd::Link => {
                    if let Some((start, url)) = self.links.pop() {
                        let end = self.inline_mut(at).text.len();
                        self.inline_mut(at).links.push((start..end, url));
                    }
                    self.restyle();
                }
                TagEnd::FootnoteDefinition => self.flush(),
                _ => {}
            },
            Event::Text(t) => {
                let style = self.style;
                self.text(&t, style, at);
            }
            Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => {
                let style = SpanStyle { code: true, ..self.style };
                self.text(&t, style, at);
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                let style = SpanStyle { code: true, ..self.style };
                self.text(&t, style, at);
            }
            Event::FootnoteReference(name) => {
                let style = SpanStyle { code: true, ..self.style };
                self.text(&format!("[^{name}]"), style, at);
            }
            Event::SoftBreak => {
                let style = self.style;
                self.text(" ", style, at);
            }
            Event::HardBreak => {
                let style = self.style;
                self.text("\n", style, at);
            }
            Event::Rule => {
                self.flush();
                self.push_block(BlockKind::Rule, at);
            }
            Event::TaskListMarker(done) => {
                self.marker = Some(if done { "☑" } else { "☐" }.to_string());
            }
        }
    }
}

/// Convert Markdown source into a flat block list.
pub fn parse_blocks(src: &str) -> Vec<Block> {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut c = Converter::new();
    for (ev, range) in Parser::new_ext(src, opts).into_offset_iter() {
        c.event(ev, range);
    }
    c.flush();
    c.blocks
}

// ---------------------------------------------------------------- view

pub struct MarkdownView {
    buffer: Entity<Buffer>,
    focus: FocusHandle,
    blocks: Rc<Vec<Block>>,
    list: ListState,
    parsed_version: Option<u64>,
    truncated: bool,
    parsing: bool,
    _reparse: Option<Task<()>>,
}

impl EventEmitter<JumpTo> for MarkdownView {}

impl Focusable for MarkdownView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl MarkdownView {
    pub fn new(buffer: Entity<Buffer>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&buffer, |this, _, cx| this.schedule(Duration::from_millis(150), cx)).detach();
        let mut v = MarkdownView {
            buffer,
            focus: cx.focus_handle(),
            blocks: Rc::new(Vec::new()),
            list: ListState::new(0, ListAlignment::Top, px(600.)),
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
        let bytes = b.slice(0..len.min(MAX_RENDER)).into_owned();
        self.parsed_version = Some(version);
        if bytes.len() <= SYNC_LIMIT {
            let src = String::from_utf8_lossy(&bytes);
            let blocks = parse_blocks(&src);
            self.apply(blocks, cx);
            return;
        }
        self.parsing = true;
        cx.notify();
        self._reparse = Some(cx.spawn(async move |this, cx| {
            let blocks = cx
                .background_executor()
                .spawn(async move { parse_blocks(&String::from_utf8_lossy(&bytes)) })
                .await;
            this.update(cx, |v, cx| {
                if v.parsed_version == Some(version) {
                    v.parsing = false;
                    v.apply(blocks, cx);
                }
            })
            .ok();
        }));
    }

    fn apply(&mut self, blocks: Vec<Block>, cx: &mut Context<Self>) {
        let top = self.list.logical_scroll_top();
        let n = blocks.len() + 1; // +1: notice / bottom padding row
        self.blocks = Rc::new(blocks);
        self.list.reset(n);
        self.list.scroll_to(ListOffset { item_ix: top.item_ix.min(n - 1), offset_in_item: top.offset_in_item });
        cx.notify();
    }

    fn page(&self, window: &Window) -> gpui::Pixels {
        (window.viewport_size().height - px(120.)).max(px(100.))
    }
}

fn body_font(bold: bool, italic: bool) -> Font {
    let mut f = gpui::font(BODY_FAMILY);
    f.fallbacks = Some(FontFallbacks::from_fonts(vec![
        "Noto Sans".into(),
        "Liberation Sans".into(),
        "Noto Color Emoji".into(),
    ]));
    if bold {
        f.weight = FontWeight::BOLD;
    }
    if italic {
        f.style = FontStyle::Italic;
    }
    f
}

fn styled(inline: &Inline, mono: &Font, base: Hsla, force_bold: bool) -> StyledText {
    let runs = inline
        .runs
        .iter()
        .map(|&(len, s)| {
            let font = if s.code {
                mono.clone()
            } else {
                body_font(s.bold || force_bold, s.italic)
            };
            let color = if s.link {
                theme::accent()
            } else if s.code {
                theme::syn_string()
            } else {
                base
            };
            TextRun {
                len,
                font,
                color,
                background_color: s.code.then(theme::bg_alt),
                underline: s.link.then(|| UnderlineStyle { thickness: px(1.), color: Some(theme::accent()), wavy: false }),
                strikethrough: s.strike.then(|| StrikethroughStyle { thickness: px(1.), color: Some(base) }),
            }
        })
        .collect();
    StyledText::new(inline.text.clone()).with_runs(runs)
}

fn inline_element(id: (&'static str, usize), inline: &Inline, mono: &Font, base: Hsla, bold: bool) -> AnyElement {
    let text = styled(inline, mono, base, bold);
    if inline.links.is_empty() {
        return text.into_any_element();
    }
    let ranges: Vec<Range<usize>> = inline.links.iter().map(|(r, _)| r.clone()).collect();
    let urls: Vec<String> = inline.links.iter().map(|(_, u)| u.clone()).collect();
    InteractiveText::new(id, text)
        .on_click(ranges, move |i, _, cx| {
            let url = &urls[i];
            if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:") {
                cx.open_url(url);
            }
        })
        .into_any_element()
}

fn render_block(ix: usize, block: &Block, mono: &Font, view: WeakEntity<MarkdownView>) -> AnyElement {
    let offset = block.offset as u64;
    let content: AnyElement = match &block.kind {
        BlockKind::Heading(level, inline) => {
            let (size, color) = match level {
                1 => (28., theme::text()),
                2 => (23., theme::text()),
                3 => (19., theme::text()),
                4 => (17., theme::text()),
                _ => (15., theme::text_dim()),
            };
            div()
                .text_size(px(size))
                .line_height(px(size * 1.3))
                .pt(px(if *level <= 2 { 14. } else { 8. }))
                .pb(px(if *level <= 2 { 6. } else { 2. }))
                .when(*level <= 2, |d| d.border_b_1().border_color(theme::border()))
                .child(inline_element(("md-h", ix), inline, mono, color, true))
                .into_any_element()
        }
        BlockKind::Paragraph(inline) => inline_element(("md-p", ix), inline, mono, theme::text(), false),
        BlockKind::Code { lang, text } => div()
            .flex()
            .flex_col()
            .bg(theme::bg_panel())
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .px_3()
            .py_2()
            .font(mono.clone())
            .text_size(px(13.))
            .line_height(px(19.))
            .text_color(theme::text())
            .when(!lang.is_empty(), |d| {
                d.child(div().text_size(px(11.)).text_color(theme::text_faint()).child(lang.clone()))
            })
            .child(text.clone())
            .into_any_element(),
        BlockKind::Rule => div().my_2().h(px(1.)).bg(theme::border()).into_any_element(),
        BlockKind::Table { header, rows } => {
            let cols = header.len().max(rows.iter().map(Vec::len).max().unwrap_or(0)).max(1);
            let row_el = |cells: &Vec<Inline>, head: bool, r: usize| {
                let mut row = div()
                    .flex()
                    .flex_row()
                    .border_b_1()
                    .border_color(theme::border())
                    .when(head, |d| d.bg(theme::bg_alt()))
                    .when(!head && r % 2 == 1, |d| d.bg(theme::bg_panel()));
                for c in 0..cols {
                    let cell = cells.get(c).cloned().unwrap_or_default();
                    row = row.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .when(c > 0, |d| d.border_l_1().border_color(theme::border()))
                            .child(inline_element(("md-cell", ix * 4096 + r * 64 + c), &cell, mono, theme::text(), head)),
                    );
                }
                row
            };
            let mut table = div().flex().flex_col().border_1().border_color(theme::border()).rounded_sm();
            if !header.is_empty() {
                table = table.child(row_el(header, true, 0));
            }
            for (r, cells) in rows.iter().enumerate() {
                table = table.child(row_el(cells, false, r + 1));
            }
            table.into_any_element()
        }
    };

    let mut body = div().flex().flex_row().gap_2().w_full();
    if let Some(m) = &block.marker {
        let checked = m == "☑";
        body = body.child(
            div()
                .flex_none()
                .min_w(px(18.))
                .text_color(if checked { theme::accent() } else { theme::text_dim() })
                .child(m.clone()),
        );
    } else if block.indent > 0 {
        body = body.child(div().flex_none().min_w(px(18.)));
    }
    body = body.child(div().flex_1().min_w_0().child(content));

    let mut inner = div()
        .w_full()
        .max_w(px(CONTENT_W))
        .pl(px(block.indent.saturating_sub(1) as f32 * 22.))
        .child(body);
    for _ in 0..block.quote {
        inner = div()
            .w_full()
            .max_w(px(CONTENT_W))
            .border_l_4()
            .border_color(theme::accent())
            .pl_3()
            .text_color(theme::text_dim())
            .child(inner);
    }

    div()
        .w_full()
        .flex()
        .justify_center()
        .px_6()
        .py(px(5.))
        .on_mouse_down(MouseButton::Left, move |e, _, cx| {
            if e.click_count == 2 {
                view.update(cx, |_, cx| cx.emit(JumpTo(offset))).ok();
            }
        })
        .child(inner)
        .into_any_element()
}

impl Render for MarkdownView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = Settings::get(cx).mono();
        let blocks = self.blocks.clone();
        let view = cx.entity().downgrade();
        let truncated = self.truncated;
        let count = blocks.len();

        let notice = if self.parsing {
            Some(tr!("Rendering Markdown…"))
        } else if truncated {
            Some(tr!("Large document: only the first 16 MiB are rendered."))
        } else {
            None
        };

        div()
            .id("markdown-view")
            .key_context("MarkdownView")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .font(body_font(false, false))
            .text_size(px(BODY_SIZE))
            .line_height(px(BODY_SIZE * 1.55))
            .on_action(cx.listener(|v, _: &PageDown, w, cx| {
                v.list.scroll_by(v.page(w));
                cx.notify()
            }))
            .on_action(cx.listener(|v, _: &PageUp, w, cx| {
                v.list.scroll_by(-v.page(w));
                cx.notify()
            }))
            .on_action(cx.listener(|v, _: &MoveDown, _, cx| {
                v.list.scroll_by(px(40.));
                cx.notify()
            }))
            .on_action(cx.listener(|v, _: &MoveUp, _, cx| {
                v.list.scroll_by(px(-40.));
                cx.notify()
            }))
            .on_action(cx.listener(|v, _: &DocStart, _, cx| {
                v.list.scroll_to(ListOffset { item_ix: 0, offset_in_item: px(0.) });
                cx.notify()
            }))
            .on_action(cx.listener(|v, _: &DocEnd, _, cx| {
                let n = v.list.item_count();
                v.list.scroll_to_reveal_item(n.saturating_sub(1));
                cx.notify()
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|v, _, window, _| window.focus(&v.focus)))
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
                list(self.list.clone(), move |ix, _, _| {
                    if ix >= count {
                        // Bottom padding.
                        return div().h(px(80.)).into_any_element();
                    }
                    render_block(ix, &blocks[ix], &mono, view.clone())
                })
                .flex_1()
                .size_full()
                .pt_4(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para_text(b: &Block) -> &str {
        match &b.kind {
            BlockKind::Paragraph(i) | BlockKind::Heading(_, i) => &i.text,
            _ => panic!("not text: {b:?}"),
        }
    }

    #[test]
    fn headings_and_inline_styles() {
        let src = "# Title\n\nSome **bold** and `code` and [link](https://x.y).\n";
        let blocks = parse_blocks(src);
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[0].kind, BlockKind::Heading(1, _)));
        assert_eq!(para_text(&blocks[0]), "Title");
        assert_eq!(blocks[0].offset, 0);
        let BlockKind::Paragraph(p) = &blocks[1].kind else { panic!() };
        assert_eq!(p.text, "Some bold and code and link.");
        assert!(p.runs.iter().any(|(_, s)| s.bold));
        assert!(p.runs.iter().any(|(_, s)| s.code));
        assert_eq!(p.links, vec![(23..27, "https://x.y".to_string())]);
        assert_eq!(p.runs.iter().map(|r| r.0).sum::<usize>(), p.text.len());
        assert_eq!(blocks[1].offset, src.find("Some").unwrap());
    }

    #[test]
    fn nested_lists_and_tasks() {
        let src = "- a\n  - b\n- [x] done\n1. one\n2. two\n";
        let blocks = parse_blocks(src);
        let summary: Vec<(String, u8, Option<String>)> =
            blocks.iter().map(|b| (para_text(b).to_string(), b.indent, b.marker.clone())).collect();
        assert_eq!(
            summary,
            vec![
                ("a".into(), 1, Some("•".into())),
                ("b".into(), 2, Some("◦".into())),
                ("done".into(), 1, Some("☑".into())),
                ("one".into(), 1, Some("1.".into())),
                ("two".into(), 1, Some("2.".into())),
            ]
        );
        assert_eq!(blocks[1].offset, src.find("- b").unwrap());
    }

    #[test]
    fn code_quote_rule() {
        let src = "```rust\nfn main() {}\n```\n\n> quoted\n\n---\n";
        let blocks = parse_blocks(src);
        assert_eq!(blocks[0].kind, BlockKind::Code { lang: "rust".into(), text: "fn main() {}".into() });
        assert_eq!(blocks[1].quote, 1);
        assert_eq!(para_text(&blocks[1]), "quoted");
        assert_eq!(blocks[2].kind, BlockKind::Rule);
        assert_eq!(blocks[2].offset, src.find("---").unwrap());
    }

    #[test]
    fn tables() {
        let src = "| A | B |\n|---|---|\n| 1 | *2* |\n| 3 | 4 |\n";
        let blocks = parse_blocks(src);
        assert_eq!(blocks.len(), 1);
        let BlockKind::Table { header, rows } = &blocks[0].kind else { panic!() };
        assert_eq!(header.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), ["A", "B"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1].text, "2");
        assert!(rows[0][1].runs[0].1.italic);
        assert_eq!(blocks[0].offset, 0);
    }
}
