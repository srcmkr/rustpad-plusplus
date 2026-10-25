//! Lazy JSON tree view.
//!
//! Nothing is parsed up front. A node only knows the byte offset of its
//! value; expanding it scans its direct children (skipping nested values
//! without building them), one page at a time. Scans that would touch more
//! than a few megabytes run in the background on a buffer snapshot.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle, Hsla, IntoElement,
    MouseButton, MouseDownEvent, Render, ScrollStrategy, SharedString, Stateful, StyledText, Task,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use super::JumpTo;
use super::text::{DocEnd, DocStart, MoveDown, MoveLeft, MoveRight, MoveUp, Newline, PageDown, PageUp};
use crate::doc::Buffer;
use crate::theme::{self, Settings};

/// Children loaded per page.
const PAGE: usize = 500;
/// Bytes a synchronous scan may touch before it is moved to the background.
const SYNC_BUDGET: u64 = 4 << 20;
/// Bytes inspected to decide whether the file is JSON Lines.
const NDJSON_PROBE: u64 = 1 << 20;
const PREVIEW_CHARS: usize = 200;

// ====================================================================== scanner

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VKind {
    Object,
    Array,
    String,
    Number,
    Keyword,
    /// Virtual root of a JSON Lines document: whitespace-separated values.
    Seq,
}

impl VKind {
    fn is_container(self) -> bool {
        matches!(self, VKind::Object | VKind::Array | VKind::Seq)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanErr {
    /// Malformed or truncated JSON at this offset.
    Invalid(u64),
    /// The byte budget was exceeded.
    Budget,
    Cancelled,
}

/// Byte reader over the piece table, refilling from `chunk_at`.
struct Rd<'a> {
    b: &'a Buffer,
    chunk: &'a [u8],
    base: u64,
    i: usize,
    limit: u64,
    cancel: Option<&'a AtomicBool>,
}

impl<'a> Rd<'a> {
    fn new(b: &'a Buffer, pos: u64, limit: u64, cancel: Option<&'a AtomicBool>) -> Self {
        let mut rd = Rd { b, chunk: &[], base: pos, i: 0, limit: limit.min(b.len()), cancel };
        rd.refill();
        rd
    }

    #[inline]
    fn pos(&self) -> u64 {
        self.base + self.i as u64
    }

    fn refill(&mut self) {
        let p = self.pos();
        let c = self.b.chunk_at(p);
        let max = self.limit.saturating_sub(p).min(c.len() as u64) as usize;
        self.chunk = &c[..max];
        self.base = p;
        self.i = 0;
    }

    /// Next byte, refilling as needed. `Err` at the budget limit or EOF.
    #[inline]
    fn peek(&mut self) -> Result<Option<u8>, ScanErr> {
        if self.i >= self.chunk.len() {
            self.refill();
            if self.chunk.is_empty() {
                return self.at_end();
            }
        }
        Ok(Some(self.chunk[self.i]))
    }

    fn at_end(&self) -> Result<Option<u8>, ScanErr> {
        if self.cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(ScanErr::Cancelled);
        }
        if self.pos() < self.b.len() { Err(ScanErr::Budget) } else { Ok(None) }
    }

    /// Like `peek`, but end of input is an error.
    fn need(&mut self) -> Result<u8, ScanErr> {
        self.peek()?.ok_or(ScanErr::Invalid(self.pos()))
    }

    #[inline]
    fn bump(&mut self) {
        self.i += 1;
    }

    fn skip_ws(&mut self) -> Result<(), ScanErr> {
        while let Some(c) = self.peek()? {
            if !matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                break;
            }
            self.bump();
        }
        Ok(())
    }

    fn check_cancel(&self) -> Result<(), ScanErr> {
        if self.cancel.is_some_and(|c| c.load(Ordering::Relaxed)) { Err(ScanErr::Cancelled) } else { Ok(()) }
    }

    /// At an opening quote: skip past the closing quote.
    fn skip_string(&mut self) -> Result<(), ScanErr> {
        let start = self.pos();
        self.bump();
        loop {
            if self.i >= self.chunk.len() {
                self.refill();
                if self.chunk.is_empty() {
                    self.at_end()?;
                    return Err(ScanErr::Invalid(start));
                }
                self.check_cancel()?;
            }
            match memchr::memchr2(b'"', b'\\', &self.chunk[self.i..]) {
                Some(j) => {
                    let at = self.i + j;
                    if self.chunk[at] == b'"' {
                        self.i = at + 1;
                        return Ok(());
                    }
                    // Escape: skip the backslash and the escaped byte. This may
                    // step past the chunk end; `pos()` stays correct.
                    self.i = at + 2;
                }
                None => self.i = self.chunk.len(),
            }
        }
    }

    /// At `{` or `[`: skip to just past the matching close.
    fn skip_container(&mut self) -> Result<(), ScanErr> {
        let start = self.pos();
        let mut depth = 0u64;
        loop {
            if self.i >= self.chunk.len() {
                self.refill();
                if self.chunk.is_empty() {
                    self.at_end()?;
                    return Err(ScanErr::Invalid(start));
                }
                self.check_cancel()?;
            }
            let rest = &self.chunk[self.i..];
            match rest.iter().position(|&c| matches!(c, b'"' | b'{' | b'}' | b'[' | b']')) {
                None => self.i = self.chunk.len(),
                Some(j) => {
                    self.i += j;
                    match self.chunk[self.i] {
                        b'"' => self.skip_string()?,
                        b'{' | b'[' => {
                            depth += 1;
                            self.bump();
                        }
                        _ => {
                            self.bump();
                            depth -= 1;
                            if depth == 0 {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
    }

    fn skip_primitive(&mut self) -> Result<(), ScanErr> {
        let start = self.pos();
        while let Some(c) = self.peek()? {
            if !(c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.')) {
                break;
            }
            self.bump();
        }
        if self.pos() == start { Err(ScanErr::Invalid(start)) } else { Ok(()) }
    }

    /// Skip the value at the current position and return its kind.
    fn skip_value(&mut self) -> Result<VKind, ScanErr> {
        let at = self.pos();
        let kind = kind_of(self.need()?).ok_or(ScanErr::Invalid(at))?;
        match kind {
            VKind::String => self.skip_string()?,
            VKind::Object | VKind::Array => self.skip_container()?,
            _ => self.skip_primitive()?,
        }
        Ok(kind)
    }
}

fn kind_of(c: u8) -> Option<VKind> {
    Some(match c {
        b'{' => VKind::Object,
        b'[' => VKind::Array,
        b'"' => VKind::String,
        b'-' | b'0'..=b'9' => VKind::Number,
        b't' | b'f' | b'n' => VKind::Keyword,
        _ => return None,
    })
}

#[derive(Clone, Debug)]
pub struct Child {
    /// Object key (unescaped), `None` for array/sequence elements.
    pub key: Option<String>,
    pub start: u64,
    pub end: u64,
    pub kind: VKind,
    pub preview: String,
}

#[derive(Clone, Debug)]
pub struct Page {
    pub children: Vec<Child>,
    /// Where to resume (just after the last child's value), if more remain.
    pub next: Option<u64>,
    /// Offset just past the container, once its end was reached.
    pub end: Option<u64>,
    /// Malformed input encountered after the returned children.
    pub error: Option<u64>,
    /// The byte limit stopped the scan before `max` children: not a real
    /// page boundary, the caller should continue from `next`.
    pub budget_hit: bool,
}

/// Scan up to `max` direct children of the container at `start`.
/// `resume` is a previous page's `next`. Positions past `limit` are not read.
pub fn scan_children(
    b: &Buffer,
    kind: VKind,
    start: u64,
    resume: Option<u64>,
    max: usize,
    limit: u64,
    cancel: Option<&AtomicBool>,
) -> Result<Page, ScanErr> {
    let first = resume.is_none();
    let pos = match resume {
        Some(p) => p,
        None if kind == VKind::Seq => start,
        None => start + 1,
    };
    let mut rd = Rd::new(b, pos, limit, cancel);
    let close = if kind == VKind::Object { b'}' } else { b']' };
    let mut page = Page { children: Vec::new(), next: None, end: None, error: None, budget_hit: false };
    let mut first = first;
    loop {
        // Separator / end.
        match rd.skip_ws() {
            Ok(()) => {}
            Err(ScanErr::Budget) if !page.children.is_empty() => {
                page.budget_hit = true;
                break;
            }
            Err(e) => return Err(e),
        }
        let c = match rd.peek() {
            Ok(c) => c,
            Err(ScanErr::Budget) if !page.children.is_empty() => {
                page.budget_hit = true;
                break;
            }
            Err(e) => return Err(e),
        };
        if kind == VKind::Seq {
            if c.is_none() {
                page.end = Some(rd.pos());
                return Ok(page);
            }
        } else {
            match c {
                Some(c) if c == close => {
                    page.end = Some(rd.pos() + 1);
                    return Ok(page);
                }
                Some(b',') if !first => {
                    rd.bump();
                    if let Err(e) = rd.skip_ws() {
                        if e == ScanErr::Budget && !page.children.is_empty() {
                            page.budget_hit = true;
                            break;
                        }
                        return Err(e);
                    }
                }
                _ if first => {}
                _ => {
                    page.error = Some(rd.pos());
                    return Ok(page);
                }
            }
        }
        first = false;

        let child_start = page.children.last().map(|c| c.end);
        match read_child(&mut rd, kind) {
            Ok(child) => page.children.push(child),
            Err(ScanErr::Invalid(at)) => {
                page.error = Some(at);
                return Ok(page);
            }
            Err(ScanErr::Budget) if !page.children.is_empty() => {
                page.budget_hit = true;
                page.next = child_start;
                return Ok(page);
            }
            Err(e) => return Err(e),
        }
        if page.children.len() >= max {
            break;
        }
    }
    page.next = page.children.last().map(|c| c.end);
    Ok(page)
}

fn read_child(rd: &mut Rd, parent: VKind) -> Result<Child, ScanErr> {
    let mut key = None;
    if parent == VKind::Object {
        let ks = rd.pos();
        if rd.need()? != b'"' {
            return Err(ScanErr::Invalid(ks));
        }
        rd.skip_string()?;
        let ke = rd.pos();
        let raw = rd.b.slice(ks + 1..(ke - 1).min(ks + 1 + 1024));
        key = Some(unescape(&raw));
        rd.skip_ws()?;
        let colon = rd.pos();
        if rd.need()? != b':' {
            return Err(ScanErr::Invalid(colon));
        }
        rd.bump();
        rd.skip_ws()?;
    }
    let start = rd.pos();
    let kind = rd.skip_value()?;
    let end = rd.pos();
    let preview = match kind {
        VKind::String => {
            let raw = rd.b.slice(start + 1..(end - 1).min(start + 1 + PREVIEW_CHARS as u64 * 4));
            let mut s = unescape(&raw);
            truncate_chars(&mut s, end - start - 2 > raw.len() as u64);
            s
        }
        VKind::Number | VKind::Keyword => {
            let raw = rd.b.slice(start..end.min(start + PREVIEW_CHARS as u64));
            let s = String::from_utf8_lossy(&raw).into_owned();
            if kind == VKind::Keyword && !matches!(s.as_str(), "true" | "false" | "null") {
                return Err(ScanErr::Invalid(start));
            }
            s
        }
        _ => String::new(),
    };
    Ok(Child { key, start, end, kind, preview })
}

/// Pieces of text in their colors, laid out as one text.
fn colored(parts: Vec<(String, Hsla)>) -> StyledText {
    let mut text = String::new();
    let mut highlights = Vec::with_capacity(parts.len());
    for (t, color) in parts {
        let start = text.len();
        text.push_str(&t);
        highlights.push((start..text.len(), HighlightStyle { color: Some(color), ..Default::default() }));
    }
    StyledText::new(text).with_highlights(highlights)
}

fn truncate_chars(s: &mut String, cut: bool) {
    if let Some((i, _)) = s.char_indices().nth(PREVIEW_CHARS) {
        s.truncate(i);
        s.push('…');
    } else if cut {
        s.push('…');
    }
}

/// Decode JSON string escapes; tolerant of truncated input.
fn unescape(raw: &[u8]) -> String {
    if !raw.contains(&b'\\') {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let c = raw[i];
        if c != b'\\' || i + 1 >= raw.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let e = raw[i + 1];
        i += 2;
        match e {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'u' if i + 4 <= raw.len() => {
                let hex = std::str::from_utf8(&raw[i..i + 4]).ok().and_then(|h| u32::from_str_radix(h, 16).ok());
                i += 4;
                let mut cp = hex.unwrap_or(0xFFFD);
                // Surrogate pair.
                if (0xD800..0xDC00).contains(&cp) && raw.get(i) == Some(&b'\\') && raw.get(i + 1) == Some(&b'u') && i + 6 <= raw.len()
                    && let Some(lo) = std::str::from_utf8(&raw[i + 2..i + 6]).ok().and_then(|h| u32::from_str_radix(h, 16).ok())
                        && (0xDC00..0xE000).contains(&lo)
                    {
                        cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                        i += 6;
                    }
                let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                let mut tmp = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
            }
            other => out.push(other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// How the document's root should be presented.
#[derive(Debug, PartialEq, Eq)]
enum RootShape {
    Empty,
    Invalid(u64),
    Single(u64, VKind),
    /// JSON Lines: several top-level values starting at this offset.
    Lines(u64),
}

fn probe_root(b: &Buffer) -> RootShape {
    let mut rd = Rd::new(b, 0, NDJSON_PROBE, None);
    if rd.skip_ws().is_err() {
        return RootShape::Single(0, VKind::Object);
    }
    let start = rd.pos();
    let first = match rd.peek() {
        Ok(None) => return RootShape::Empty,
        Ok(Some(c)) => c,
        Err(_) => return RootShape::Invalid(start),
    };
    let Some(kind) = kind_of(first) else { return RootShape::Invalid(start) };
    if rd.skip_value().is_ok() && rd.skip_ws().is_ok() && matches!(rd.peek(), Ok(Some(c)) if kind_of(c).is_some()) {
        return RootShape::Lines(start);
    }
    RootShape::Single(start, kind)
}

// ====================================================================== tree

#[derive(Clone, Debug, PartialEq, Eq)]
enum Label {
    Root,
    Key(String),
    Index(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Children {
    Unscanned,
    Partial(u64),
    Scanning,
    Complete,
    Error(u64),
}

struct Node {
    parent: Option<usize>,
    depth: usize,
    label: Label,
    kind: VKind,
    start: u64,
    /// End of the value, when known (children of scanned containers).
    end: Option<u64>,
    preview: String,
    /// DevTools-style one-line preview of a small collapsed container.
    inline: Option<Option<Vec<(String, Hsla)>>>,
    expanded: bool,
    children: Vec<usize>,
    state: Children,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowItem {
    Node(usize),
    /// "load more" row for the node's children.
    More(usize),
    Scanning(usize),
    Error(usize),
}

pub struct JsonView {
    buffer: Entity<Buffer>,
    focus: FocusHandle,
    version: u64,
    nodes: Vec<Node>,
    rows: Vec<RowItem>,
    selected: usize,
    root_error: Option<SharedString>,
    scroll: UniformListScrollHandle,
    cancel: Arc<AtomicBool>,
    tasks: Vec<Task<()>>,
    /// Paths that were expanded before an edit, re-expanded when seen again.
    restore: HashSet<String>,
}

impl EventEmitter<JumpTo> for JsonView {}

impl Focusable for JsonView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl JsonView {
    pub fn new(buffer: Entity<Buffer>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&buffer, |this, buffer, cx| {
            if buffer.read(cx).version() != this.version {
                this.reset(cx);
            }
        })
        .detach();
        let mut v = JsonView {
            version: buffer.read(cx).version(),
            buffer,
            focus: cx.focus_handle(),
            nodes: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            root_error: None,
            scroll: UniformListScrollHandle::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            tasks: Vec::new(),
            restore: HashSet::new(),
        };
        v.build_root(cx);
        v
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        self.restore = (0..self.nodes.len())
            .filter(|&i| self.nodes[i].expanded)
            .map(|i| self.path(i))
            .collect();
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.tasks.clear();
        self.version = self.buffer.read(cx).version();
        self.build_root(cx);
        cx.notify();
    }

    fn build_root(&mut self, cx: &mut Context<Self>) {
        self.nodes.clear();
        self.root_error = None;
        self.selected = 0;
        let shape = probe_root(self.buffer.read(cx));
        let (start, kind) = match shape {
            RootShape::Empty => {
                self.root_error = Some(tr!("Empty document").into());
                self.rows.clear();
                return;
            }
            RootShape::Invalid(at) => {
                self.root_error = Some(tr!("⚠ invalid JSON at offset {offset}", offset = at).into());
                self.rows.clear();
                return;
            }
            RootShape::Single(s, k) => (s, k),
            RootShape::Lines(s) => (s, VKind::Seq),
        };
        let preview = if kind.is_container() {
            String::new()
        } else {
            // A lone primitive: scan it as a one-element sequence to get a preview.
            scan_children(self.buffer.read(cx), VKind::Seq, start, None, 1, SYNC_BUDGET, None)
                .ok()
                .and_then(|p| p.children.into_iter().next())
                .map(|c| c.preview)
                .unwrap_or_default()
        };
        self.nodes.push(Node {
            parent: None,
            depth: 0,
            label: Label::Root,
            kind,
            start,
            end: None,
            preview,
            inline: None,
            expanded: false,
            children: Vec::new(),
            state: Children::Unscanned,
        });
        if kind.is_container() {
            self.expand(0, cx);
        }
        self.rebuild_rows();
    }

    // ---------------------------------------------------------------- scanning

    fn expand(&mut self, i: usize, cx: &mut Context<Self>) {
        let n = &mut self.nodes[i];
        if !n.kind.is_container() {
            return;
        }
        n.expanded = true;
        if n.state == Children::Unscanned {
            self.load_page(i, cx);
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn collapse(&mut self, i: usize, cx: &mut Context<Self>) {
        self.nodes[i].expanded = false;
        self.rebuild_rows();
        cx.notify();
    }

    fn toggle(&mut self, i: usize, cx: &mut Context<Self>) {
        if self.nodes[i].expanded { self.collapse(i, cx) } else { self.expand(i, cx) }
    }

    fn load_page(&mut self, i: usize, cx: &mut Context<Self>) {
        let n = &self.nodes[i];
        let resume = match n.state {
            Children::Unscanned => None,
            Children::Partial(p) => Some(p),
            _ => return,
        };
        let (kind, start) = (n.kind, n.start);
        let from = resume.unwrap_or(start);
        let result = scan_children(self.buffer.read(cx), kind, start, resume, PAGE, from + SYNC_BUDGET, None);
        match result {
            Err(ScanErr::Budget) => self.load_page_background(i, kind, start, resume, PAGE, cx),
            Ok(page) if page.budget_hit && page.next.is_some() => {
                let (got, next) = (page.children.len(), page.next);
                self.apply_page(i, Ok(page), cx);
                self.load_page_background(i, kind, start, next, PAGE - got, cx);
            }
            other => self.apply_page(i, other, cx),
        }
    }

    fn load_page_background(
        &mut self,
        i: usize,
        kind: VKind,
        start: u64,
        resume: Option<u64>,
        max: usize,
        cx: &mut Context<Self>,
    ) {
        self.nodes[i].state = Children::Scanning;
        let snapshot = self.buffer.read(cx).snapshot();
        let cancel = self.cancel.clone();
        let version = self.version;
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { scan_children(&snapshot, kind, start, resume, max, u64::MAX, Some(&cancel)) })
                .await;
            this.update(cx, |v, cx| {
                if v.version == version && i < v.nodes.len() {
                    v.nodes[i].state = match resume {
                        Some(p) => Children::Partial(p),
                        None => Children::Unscanned,
                    };
                    v.apply_page(i, result, cx);
                    v.rebuild_rows();
                    cx.notify();
                }
            })
            .ok();
        });
        self.tasks.push(task);
    }

    fn apply_page(&mut self, i: usize, result: Result<Page, ScanErr>, cx: &mut Context<Self>) {
        let page = match result {
            Ok(p) => p,
            Err(ScanErr::Invalid(at)) => {
                self.nodes[i].state = Children::Error(at);
                return;
            }
            Err(ScanErr::Cancelled) | Err(ScanErr::Budget) => return,
        };
        let depth = self.nodes[i].depth + 1;
        let base = self.nodes[i].children.len() as u64;
        let parent_kind = self.nodes[i].kind;
        let mut to_restore = Vec::new();
        for (k, c) in page.children.into_iter().enumerate() {
            let label = match (parent_kind, c.key) {
                (VKind::Object, Some(key)) => Label::Key(key),
                _ => Label::Index(base + k as u64),
            };
            let idx = self.nodes.len();
            self.nodes.push(Node {
                parent: Some(i),
                depth,
                label,
                kind: c.kind,
                start: c.start,
                end: Some(c.end),
                preview: c.preview,
                inline: None,
                expanded: false,
                children: Vec::new(),
                state: Children::Unscanned,
            });
            self.nodes[i].children.push(idx);
            if c.kind.is_container() && !self.restore.is_empty() && self.restore.contains(&self.path(idx)) {
                to_restore.push(idx);
            }
        }
        self.nodes[i].state = match (page.error, page.end, page.next) {
            (Some(at), _, _) => Children::Error(at),
            (_, Some(_), _) => Children::Complete,
            (_, _, Some(p)) => Children::Partial(p),
            _ => Children::Complete,
        };
        for idx in to_restore {
            self.expand(idx, cx);
        }
    }

    // ---------------------------------------------------------------- rows

    fn rebuild_rows(&mut self) {
        let keep = self.rows.get(self.selected).copied();
        self.rows.clear();
        if self.nodes.is_empty() {
            return;
        }
        self.rows = self.flatten();
        self.selected = keep
            .and_then(|k| self.rows.iter().position(|r| *r == k))
            .unwrap_or(self.selected)
            .min(self.rows.len().saturating_sub(1));
    }

    fn flatten(&self) -> Vec<RowItem> {
        fn walk(v: &JsonView, i: usize, out: &mut Vec<RowItem>) {
            out.push(RowItem::Node(i));
            let n = &v.nodes[i];
            if !n.expanded {
                return;
            }
            for &c in &n.children {
                walk(v, c, out);
            }
            match n.state {
                Children::Partial(_) => out.push(RowItem::More(i)),
                Children::Scanning => out.push(RowItem::Scanning(i)),
                Children::Error(_) => out.push(RowItem::Error(i)),
                _ => {}
            }
        }
        // Recursion depth is bounded by what the user expanded by hand.
        let mut out = Vec::with_capacity(self.rows.capacity());
        if !self.nodes.is_empty() {
            walk(self, 0, &mut out);
        }
        out
    }

    fn path(&self, mut i: usize) -> String {
        let mut parts = Vec::new();
        loop {
            let n = &self.nodes[i];
            match &n.label {
                Label::Root => break,
                Label::Index(k) => parts.push(format!("[{k}]")),
                Label::Key(k) => {
                    if !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '_') && !k.starts_with(|c: char| c.is_ascii_digit()) {
                        parts.push(format!(".{k}"));
                    } else {
                        parts.push(format!("[{k:?}]"));
                    }
                }
            }
            match n.parent {
                Some(p) => i = p,
                None => break,
            }
        }
        parts.reverse();
        format!("${}", parts.concat())
    }

    // ---------------------------------------------------------------- input

    fn select(&mut self, row: usize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = row.min(self.rows.len() - 1);
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.notify();
    }

    fn activate(&mut self, row: usize, cx: &mut Context<Self>) {
        match self.rows.get(row).copied() {
            Some(RowItem::Node(i)) => cx.emit(JumpTo(self.nodes[i].start)),
            Some(RowItem::More(i)) => {
                self.load_page(i, cx);
                self.rebuild_rows();
                cx.notify();
            }
            Some(RowItem::Error(i)) => {
                if let Children::Error(at) = self.nodes[i].state {
                    cx.emit(JumpTo(at));
                }
            }
            _ => {}
        }
    }

    fn on_row_down(&mut self, row: usize, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.selected = row;
        match (self.rows.get(row).copied(), e.click_count) {
            (Some(RowItem::More(_)), _) => self.activate(row, cx),
            (_, 2) => self.activate(row, cx),
            _ => {}
        }
        cx.notify();
    }

    fn right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(RowItem::Node(i)) = self.rows.get(self.selected).copied() {
            if self.nodes[i].kind.is_container() && !self.nodes[i].expanded {
                self.expand(i, cx);
            } else if self.nodes[i].expanded {
                self.select(self.selected + 1, cx);
            }
        }
    }

    fn left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        let node = match self.rows.get(self.selected).copied() {
            Some(RowItem::Node(i)) => i,
            Some(RowItem::More(i) | RowItem::Scanning(i) | RowItem::Error(i)) => {
                if let Some(r) = self.rows.iter().position(|r| *r == RowItem::Node(i)) {
                    self.select(r, cx);
                }
                return;
            }
            None => return,
        };
        if self.nodes[node].expanded {
            self.collapse(node, cx);
        } else if let Some(p) = self.nodes[node].parent
            && let Some(r) = self.rows.iter().position(|r| *r == RowItem::Node(p))
        {
            self.select(r, cx);
        }
    }

    // ---------------------------------------------------------------- render

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<Stateful<gpui::Div>> {
        let s = Settings::get(cx);
        let lh = s.line_height();
        let indent = s.font_size * 1.4;
        let focused_sel = self.selected;
        let mut out = Vec::with_capacity(range.len());
        for ix in range {
            let Some(item) = self.rows.get(ix).copied() else { continue };
            let mut row = div()
                .id(("json-row", ix))
                .h(lh)
                .flex()
                .flex_row()
                .items_center()
                .whitespace_nowrap()
                .overflow_hidden()
                .when(ix == focused_sel, |d| d.bg(theme::selection()))
                .when(ix != focused_sel, |d| d.hover(|s| s.bg(theme::bg_hover())))
                .on_mouse_down(MouseButton::Left, cx.listener(move |v, e: &MouseDownEvent, w, cx| v.on_row_down(ix, e, w, cx)));
            match item {
                RowItem::Node(i) => {
                    let n = &self.nodes[i];
                    row = row.pl(indent * n.depth as f32 + px(4.));
                    let arrow = if n.kind.is_container() {
                        if n.expanded { "▼" } else { "▶" }
                    } else {
                        " "
                    };
                    row = row.child(
                        div()
                            .w(indent)
                            .flex_none()
                            .text_color(theme::text_faint())
                            .text_size(s.font_size * 0.75)
                            .on_mouse_down(MouseButton::Left, cx.listener(move |v, _: &MouseDownEvent, w, cx| {
                                cx.stop_propagation();
                                w.focus(&v.focus, cx);
                                if let Some(r) = v.rows.iter().position(|r| *r == RowItem::Node(i)) {
                                    v.selected = r;
                                }
                                v.toggle(i, cx);
                            }))
                            .child(arrow),
                    );
                    // One text with colored ranges: separate elements would lose the
                    // space after ":", which does not count toward a text's width.
                    let mut parts: Vec<(String, Hsla)> = Vec::new();
                    match &n.label {
                        Label::Root => {
                            let name = if n.kind == VKind::Seq { tr!("document ") } else { "$ " };
                            parts.push((name.to_string(), theme::text_dim()));
                        }
                        Label::Key(k) => {
                            let mut k = k.replace(['\n', '\r'], " ");
                            truncate_chars(&mut k, false);
                            parts.push((format!("\"{k}\""), theme::syn_key()));
                            parts.push((": ".into(), theme::syn_punct()));
                        }
                        Label::Index(k) => {
                            let lbl = if self.nodes[n.parent.unwrap_or(0)].kind == VKind::Seq {
                                tr!("line {line}", line = k + 1)
                            } else {
                                format!("[{k}]")
                            };
                            parts.push((lbl, theme::text_dim()));
                            parts.push((": ".into(), theme::syn_punct()));
                        }
                    }
                    let collapsed_container = n.kind.is_container() && !n.expanded;
                    let (text, color): (String, Hsla) = match n.kind {
                        VKind::String => (format!("\"{}\"", n.preview.replace(['\n', '\r'], "⏎")), theme::syn_string()),
                        VKind::Number => (n.preview.clone(), theme::syn_number()),
                        VKind::Keyword => (n.preview.clone(), theme::syn_keyword()),
                        VKind::Object | VKind::Array | VKind::Seq => (self.summary(i), theme::text_dim()),
                    };
                    match collapsed_container.then(|| self.inline_preview(i, cx)).flatten() {
                        Some(segments) => parts.extend(segments),
                        None => parts.push((text, color)),
                    }
                    row = row.child(colored(parts));
                }
                RowItem::More(i) => {
                    let n = &self.nodes[i];
                    row = row
                        .pl(indent * (n.depth + 1) as f32 + indent + px(4.))
                        .text_color(theme::accent())
                        .child(tr!("… load more ({count} loaded)", count = group(n.children.len() as u64)));
                }
                RowItem::Scanning(i) => {
                    let n = &self.nodes[i];
                    row = row
                        .pl(indent * (n.depth + 1) as f32 + indent + px(4.))
                        .text_color(theme::text_faint())
                        .child(match n.children.len() {
                            0 => tr!("scanning…").to_string(),
                            k => tr!("scanning… ({count} loaded)", count = group(k as u64)),
                        });
                }
                RowItem::Error(i) => {
                    let n = &self.nodes[i];
                    let at = if let Children::Error(at) = n.state { at } else { 0 };
                    row = row
                        .pl(indent * (n.depth + 1) as f32 + indent + px(4.))
                        .text_color(theme::error())
                        .child(tr!("⚠ invalid JSON at offset {offset}", offset = at));
                }
            }
            out.push(row);
        }
        out
    }

    /// One-line preview of a small collapsed container, computed once per
    /// node (only for rows that get rendered) and cached.
    fn inline_preview(&mut self, i: usize, cx: &App) -> Option<Vec<(String, Hsla)>> {
        if let Some(cached) = &self.nodes[i].inline {
            return cached.clone();
        }
        let n = &self.nodes[i];
        let computed = match n.end {
            Some(end) if end - n.start <= INLINE_MAX_BYTES && matches!(n.kind, VKind::Object | VKind::Array) => {
                inline_segments(self.buffer.read(cx), n.kind, n.start, end)
            }
            _ => None,
        };
        self.nodes[i].inline = Some(computed.clone());
        computed
    }

    fn summary(&self, i: usize) -> String {
        let n = &self.nodes[i];
        let count = n.children.len() as u64;
        let (open, close, unit) = match n.kind {
            VKind::Object => ("{", "}", trn!("key", "keys", count)),
            VKind::Seq => ("", "", trn!("value", "values", count)),
            _ => ("[", "]", trn!("item", "items", count)),
        };
        match n.state {
            Children::Complete => format!("{open} {} {unit} {close}", group(count)),
            Children::Partial(_) => format!("{open} {}+ {unit} {close}", group(count)),
            Children::Scanning if count > 0 => format!("{open} {}+ {unit} {close}", group(count)),
            _ => format!("{open}…{close}"),
        }
    }
}

/// Containers up to this many source bytes get an inline preview.
const INLINE_MAX_BYTES: u64 = 2048;
/// Inline previews are cut at about this many characters.
const INLINE_MAX_CHARS: usize = 120;

/// Build `{ id: 1, name: "x", … }` / `[ "a", 3 ]` segments with colors.
fn inline_segments(b: &Buffer, kind: VKind, start: u64, end: u64) -> Option<Vec<(String, Hsla)>> {
    let page = scan_children(b, kind, start, None, 64, end, None).ok()?;
    if page.error.is_some() {
        return None;
    }
    let (open, close) = if kind == VKind::Object { ("{", "}") } else { ("[", "]") };
    if page.children.is_empty() {
        return Some(vec![(format!("{open}{close}"), theme::text_dim())]);
    }
    let mut out = vec![(format!("{open} "), theme::text_dim())];
    let mut used = 2;
    for (k, c) in page.children.iter().enumerate() {
        let mut piece: Vec<(String, Hsla)> = Vec::new();
        if k > 0 {
            piece.push((", ".into(), theme::text_dim()));
        }
        if let Some(key) = &c.key {
            let mut key = key.replace(['\n', '\r'], " ");
            shorten(&mut key, 24);
            piece.push((format!("{key}: "), theme::text_dim()));
        }
        piece.push(match c.kind {
            VKind::String => {
                let mut v = c.preview.replace(['\n', '\r'], "⏎");
                shorten(&mut v, 32);
                (format!("\"{v}\""), theme::syn_string())
            }
            VKind::Number => (c.preview.clone(), theme::syn_number()),
            VKind::Keyword => (c.preview.clone(), theme::syn_keyword()),
            VKind::Object => ("{…}".into(), theme::text_dim()),
            _ => ("[…]".into(), theme::text_dim()),
        });
        let len: usize = piece.iter().map(|(t, _)| t.chars().count()).sum();
        if used + len > INLINE_MAX_CHARS && k > 0 {
            out.push((", …".into(), theme::text_dim()));
            out.push((format!(" {close}"), theme::text_dim()));
            return Some(out);
        }
        used += len;
        out.extend(piece);
    }
    if page.end.is_none() {
        out.push((", …".into(), theme::text_dim()));
    }
    out.push((format!(" {close}"), theme::text_dim()));
    Some(out)
}

fn shorten(s: &mut String, max: usize) {
    if let Some((i, _)) = s.char_indices().nth(max) {
        s.truncate(i);
        s.push('…');
    }
}

/// 1234567 -> "1 234 567" (thin spaces).
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('\u{2009}');
        }
        out.push(c);
    }
    out
}

impl Render for JsonView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Settings::get(cx);
        let font = s.mono();
        let font_size = s.font_size;
        let header = match self.rows.get(self.selected).copied() {
            Some(RowItem::Node(i)) => format!("{}   @ {}", self.path(i), self.nodes[i].start),
            _ => String::new(),
        };
        let page = 20;
        div()
            .id("json-view")
            .key_context("JsonView")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .font(font)
            .text_size(font_size)
            .on_action(cx.listener(|v, _: &MoveUp, _, cx| v.select(v.selected.saturating_sub(1), cx)))
            .on_action(cx.listener(|v, _: &MoveDown, _, cx| v.select(v.selected + 1, cx)))
            .on_action(cx.listener(move |v, _: &PageUp, _, cx| v.select(v.selected.saturating_sub(page), cx)))
            .on_action(cx.listener(move |v, _: &PageDown, _, cx| v.select(v.selected + page, cx)))
            .on_action(cx.listener(|v, _: &DocStart, _, cx| v.select(0, cx)))
            .on_action(cx.listener(|v, _: &DocEnd, _, cx| v.select(usize::MAX, cx)))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(|v, _: &Newline, _, cx| v.activate(v.selected, cx)))
            .child(
                div()
                    .flex_none()
                    .h(px(24.))
                    .px_2()
                    .flex()
                    .items_center()
                    .bg(theme::bg_panel())
                    .border_b_1()
                    .border_color(theme::border())
                    .text_size(px(12.))
                    .text_color(theme::text_dim())
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(header),
            )
            .child(match &self.root_error {
                Some(msg) => div()
                    .flex_1()
                    .p_4()
                    .text_color(theme::error())
                    .child(msg.clone())
                    .into_any_element(),
                None => uniform_list("json-rows", self.rows.len(), cx.processor(|v, range, _, cx| v.render_rows(range, cx)))
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .into_any_element(),
            })
    }
}

// ====================================================================== tests

#[cfg(test)]
mod tests {
    use super::*;

    fn page(text: &str, kind: VKind, start: u64) -> Page {
        let b = Buffer::from_text(text);
        scan_children(&b, kind, start, None, 100, u64::MAX, None).unwrap()
    }

    #[test]
    fn object_children_with_nesting_and_escapes() {
        let t = r#"{"a": 1, "b\"q": "x\\\"}y", "c": {"d": [1, {"e": "]"}]}, "f": [true, null]}"#;
        let p = page(t, VKind::Object, 0);
        let keys: Vec<_> = p.children.iter().map(|c| c.key.clone().unwrap()).collect();
        assert_eq!(keys, ["a", "b\"q", "c", "f"]);
        assert_eq!(p.children[0].preview, "1");
        assert_eq!(p.children[1].kind, VKind::String);
        assert_eq!(p.children[1].preview, "x\\\"}y");
        assert_eq!(p.children[2].kind, VKind::Object);
        assert_eq!(&t[p.children[2].start as usize..p.children[2].end as usize], r#"{"d": [1, {"e": "]"}]}"#);
        assert_eq!(p.end, Some(t.len() as u64));
        assert!(p.error.is_none());
    }

    #[test]
    fn paging_resumes() {
        let t = "[1,2,3,4,5]";
        let b = Buffer::from_text(t);
        let p1 = scan_children(&b, VKind::Array, 0, None, 2, u64::MAX, None).unwrap();
        assert_eq!(p1.children.len(), 2);
        assert!(p1.end.is_none());
        let p2 = scan_children(&b, VKind::Array, 0, p1.next, 10, u64::MAX, None).unwrap();
        let vals: Vec<_> = p2.children.iter().map(|c| c.preview.clone()).collect();
        assert_eq!(vals, ["3", "4", "5"]);
        assert_eq!(p2.end, Some(t.len() as u64));
    }

    #[test]
    fn empty_containers() {
        assert!(page("{ }", VKind::Object, 0).children.is_empty());
        assert_eq!(page("[]", VKind::Array, 0).end, Some(2));
    }

    #[test]
    fn ndjson() {
        let t = "{\"a\":1}\n{\"a\":2}\n[3]\n";
        let b = Buffer::from_text(t);
        assert_eq!(probe_root(&b), RootShape::Lines(0));
        let p = page(t, VKind::Seq, 0);
        assert_eq!(p.children.len(), 3);
        assert_eq!(p.children[2].kind, VKind::Array);
        let single = Buffer::from_text("  {\"a\": [1]}  ");
        assert_eq!(probe_root(&single), RootShape::Single(2, VKind::Object));
    }

    #[test]
    fn invalid_json() {
        let p = page(r#"{"a": 1 "b": 2}"#, VKind::Object, 0);
        assert_eq!(p.children.len(), 1);
        assert_eq!(p.error, Some(8));
        let p = page(r#"{"a": tru}"#, VKind::Object, 0);
        assert_eq!(p.error, Some(6));
        let p = page(r#"[1, "unterminated"#, VKind::Array, 0);
        assert_eq!(p.error, Some(4));
        assert_eq!(probe_root(&Buffer::from_text("hello")), RootShape::Invalid(0));
        assert_eq!(probe_root(&Buffer::from_text("  ")), RootShape::Empty);
    }

    #[test]
    fn budget_is_respected() {
        let big = format!("[{}]", vec!["\"xxxxxxxxxx\""; 1000].join(","));
        let b = Buffer::from_text(&big);
        // Budget too small for even the first child: reported as Budget.
        assert_eq!(scan_children(&b, VKind::Array, 0, None, 10_000, 5, None).unwrap_err(), ScanErr::Budget);
        // Budget allows a few children: returns a partial page.
        let p = scan_children(&b, VKind::Array, 0, None, 10_000, 100, None).unwrap();
        assert!(!p.children.is_empty() && p.next.is_some() && p.end.is_none());
        let p2 = scan_children(&b, VKind::Array, 0, p.next, 10_000, u64::MAX, None).unwrap();
        assert_eq!(p.children.len() + p2.children.len(), 1000);
    }

    #[test]
    fn budget_flag_marks_non_page_boundaries() {
        let big = format!("[{}]", vec!["\"xxxxxxxxxx\""; 100].join(","));
        let b = Buffer::from_text(&big);
        let p = scan_children(&b, VKind::Array, 0, None, 500, 100, None).unwrap();
        assert!(p.budget_hit && p.next.is_some());
        let p = scan_children(&b, VKind::Array, 0, None, 10, u64::MAX, None).unwrap();
        assert!(!p.budget_hit && p.next.is_some());
    }

    #[test]
    fn inline_preview_segments() {
        let t = r#"{"id": 1, "name": "user1", "tags": ["a"], "ok": true}"#;
        let b = Buffer::from_text(t);
        let segs = inline_segments(&b, VKind::Object, 0, t.len() as u64).unwrap();
        let text: String = segs.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(text, r#"{ id: 1, name: "user1", tags: […], ok: true }"#);
        let t = r#"["a", "b", 3]"#;
        let b = Buffer::from_text(t);
        let text: String = inline_segments(&b, VKind::Array, 0, t.len() as u64).unwrap().iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(text, r#"[ "a", "b", 3 ]"#);
        let long = format!("[{}]", (0..200).map(|i| i.to_string()).collect::<Vec<_>>().join(","));
        let b = Buffer::from_text(&long);
        let text: String = inline_segments(&b, VKind::Array, 0, long.len() as u64).unwrap().iter().map(|(s, _)| s.clone()).collect();
        assert!(text.ends_with(", … ]") && text.chars().count() <= INLINE_MAX_CHARS + 6);
    }

    #[test]
    fn unescape_works() {
        assert_eq!(unescape("a\\nb\\u00e9\\ud83d\\ude00".as_bytes()), "a\nb\u{e9}\u{1f600}");
        assert_eq!(group(1234567), "1\u{2009}234\u{2009}567");
    }
}
