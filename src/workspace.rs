//! Window contents: tabs, view switcher, find/goto bar and status bar.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use gpui::{
    AnyElement, App, Context, Entity, ExternalPaths, FocusHandle, Focusable, FontWeight, Image,
    ImageFormat, IntoElement, MouseButton, PathPromptOptions, PromptLevel, Render, SharedString,
    Subscription, Task, Window, actions, deferred, div, img, linear_color_stop, linear_gradient,
    prelude::*, px, rgb,
};

#[path = "menu.rs"]
mod menu;

use crate::doc::Buffer;
use crate::doc::buffer::DiskChange;
use crate::session;
use crate::update;
use crate::doc::detect::{self, Kind};
use crate::doc::encoding::{self, Encoding};
use crate::doc::rows;
use crate::theme::{self, Settings};
use crate::views::hex::HexView;
use crate::views::json::JsonView;
use crate::views::markdown::MarkdownView;
use crate::views::text::{TextEvent, TextView};
use crate::views::{JumpTo, ViewMode};

actions!(
    workspace,
    [
        Open,
        Save,
        SaveAs,
        NewFile,
        CloseTab,
        NextTab,
        PrevTab,
        ShowText,
        ShowHex,
        ShowTree,
        ShowMarkdown,
        CycleView,
        Find,
        FindNext,
        FindPrev,
        Replace,
        GotoLine,
        ShowChangelog,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        Quit,
        SaveAll,
        OpenSettings,
        CloseOverlay,
    ]
);

struct Tab {
    /// Stable id, names the session backup file.
    id: u64,
    /// Buffer version last written to the session backup.
    backed_version: Option<u64>,
    /// Unhandled change of the file on disk.
    external: Option<DiskChange>,
    /// tail -f: reload automatically when the file grows and stay at the end.
    follow: bool,
    buffer: Entity<Buffer>,
    kind: Kind,
    mode: ViewMode,
    text: Entity<TextView>,
    hex: Option<Entity<HexView>>,
    tree: Option<Entity<JsonView>>,
    md: Option<Entity<MarkdownView>>,
    _subs: Vec<Subscription>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BarKind {
    Find,
    Replace,
    Goto,
}

struct Bar {
    kind: BarKind,
    input: Entity<TextView>,
    /// Second input of the replace bar.
    replace: Option<(Entity<TextView>, Subscription)>,
    _sub: Subscription,
}

/// A long-running background job (search or save) with progress.
struct Job {
    label: &'static str,
    total: u64,
    progress: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    _task: Task<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialog {
    About,
    Updates,
}

pub struct Workspace {
    menu: Option<usize>,
    menu_by_hover: bool,
    submenu: Option<SharedString>,
    /// The encoding drop-up of the status bar is open.
    encoding_menu: bool,
    dialog: Option<Dialog>,
    save_queue: Vec<(Entity<Buffer>, PathBuf)>,
    tabs: Vec<Tab>,
    next_tab_id: u64,
    session_hash: u64,
    /// Window moved or resized since the placement was last written.
    bounds_dirty: bool,
    last_disk_check: std::time::Instant,
    active: usize,
    focus: FocusHandle,
    bar: Option<Bar>,
    message: Option<SharedString>,
    job: Option<Job>,
    _ticker: Option<Task<()>>,
}

impl Workspace {
    pub fn new(files: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut ws = Workspace {
            menu: None,
            menu_by_hover: false,
            submenu: None,
            encoding_menu: false,
            dialog: None,
            save_queue: Vec::new(),
            tabs: Vec::new(),
            next_tab_id: 1,
            session_hash: 0,
            bounds_dirty: false,
            last_disk_check: std::time::Instant::now(),
            active: 0,
            focus: cx.focus_handle(),
            bar: None,
            message: None,
            job: None,
            _ticker: None,
        };
        if ws.session_enabled(cx) {
            ws.restore_session(window, cx);
            ws.session_hash = ws.session_fingerprint(cx);
        }
        for f in &files {
            ws.open_path(f, window, cx);
        }
        if ws.tabs.is_empty() {
            ws.new_tab(Buffer::empty(), window, cx);
        }
        ws.start_ticker(window, cx);
        cx.observe_window_bounds(window, |ws, _, _| ws.bounds_dirty = true)
        .detach();
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            this.update(cx, |ws, cx| ws.confirm_quit(window, cx)).unwrap_or(true)
        });
        ws
    }

    /// Repaint while the line index or a job is running, so progress shows.
    fn start_ticker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._ticker = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
                let Ok(busy) = this.update_in(cx, |ws, window, cx| {
                    ws.autosave(window, cx);
                    ws.check_disk(cx);
                    let busy = ws.job.is_some()
                        || ws.tabs.get(ws.active).is_some_and(|t| !t.buffer.read(cx).line_index().is_done());
                    if busy {
                        cx.notify();
                        // Line numbers in the gutter appear once indexed.
                        if let Some(t) = ws.tabs.get(ws.active) {
                            t.text.update(cx, |_, cx| cx.notify());
                        }
                    }
                    busy
                }) else {
                    break;
                };
                if !busy {
                    cx.background_executor().timer(std::time::Duration::from_millis(750)).await;
                }
            }
        }));
    }

    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn new_tab(&mut self, buffer: Buffer, window: &mut Window, cx: &mut Context<Self>) {
        self.new_tab_with_id(buffer, None, window, cx);
    }

    fn new_tab_with_id(&mut self, mut buffer: Buffer, id: Option<u64>, window: &mut Window, cx: &mut Context<Self>) {
        if buffer.path.is_none() && buffer.title.is_none() {
            // notepad++ style: "new 1", "new 2", …
            let n = self
                .tabs
                .iter()
                .filter_map(|t| t.buffer.read(cx).title.as_deref()?.strip_prefix("new ")?.parse::<u32>().ok())
                .max()
                .unwrap_or(0);
            buffer.title = Some(format!("new {}", n + 1));
        }
        let id = id.unwrap_or(self.next_tab_id);
        self.next_tab_id = self.next_tab_id.max(id + 1);
        let sample_len = buffer.len().min(8192);
        let kind = detect::detect(buffer.path.as_deref(), &buffer.slice(0..sample_len));
        let buffer = cx.new(|_| buffer);
        let text = cx.new(|cx| TextView::new(buffer.clone(), cx));
        let subs = vec![
            cx.subscribe(&text, |ws, _, e: &TextEvent, cx| match e {
                TextEvent::Message(m) => ws.flash(m.clone(), cx),
                TextEvent::Cancelled => ws.close_overlay(cx),
                _ => cx.notify(),
            }),
            cx.observe(&buffer, |_, _, cx| cx.notify()),
        ];
        let mode = match kind {
            Kind::Binary => ViewMode::Hex,
            _ => ViewMode::Text,
        };
        self.tabs.push(Tab { id, backed_version: None, external: None, follow: false, buffer, kind, mode: ViewMode::Text, text, hex: None, tree: None, md: None, _subs: subs });
        self.active = self.tabs.len() - 1;
        self.set_mode(mode, window, cx);
        self.update_title(window, cx);
    }

    pub fn open_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Some(i) = self.tabs.iter().position(|t| t.buffer.read(cx).path.as_deref() == Some(&path)) {
            self.active = i;
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        if path.is_dir() {
            self.flash(tr!("{path} is a directory", path = path.display()).into(), cx);
            return;
        }
        let buffer = if path.exists() {
            match Buffer::open(&path) {
                Ok(b) => b,
                Err(e) => {
                    self.flash(tr!("Cannot open {path}: {error}", path = path.display(), error = e).into(), cx);
                    return;
                }
            }
        } else {
            // Like notepad++: a missing file opens as a new document with that name.
            let mut b = Buffer::empty();
            b.path = Some(path.clone());
            b
        };
        // Replace an untouched empty tab.
        let replace = self.tabs.len() == 1 && {
            let b = self.tabs[0].buffer.read(cx);
            b.path.is_none() && b.is_empty() && !b.is_dirty()
        };
        if replace {
            self.tabs.clear();
        }
        if path.exists() {
            self.remember(&path, cx);
        }
        self.new_tab(buffer, window, cx);
    }

    fn remember(&mut self, path: &Path, cx: &mut Context<Self>) {
        let s = cx.global_mut::<Settings>();
        s.add_recent(path);
        s.save();
    }

    fn clear_recent(&mut self, cx: &mut Context<Self>) {
        let s = cx.global_mut::<Settings>();
        s.recent.clear();
        s.save();
        cx.notify();
    }

    fn set_scheme(&mut self, i: usize, cx: &mut Context<Self>) {
        theme::set_scheme(i);
        Settings::get(cx).save();
        cx.refresh_windows();
    }

    fn toggle_word_wrap(&mut self, cx: &mut Context<Self>) {
        let s = cx.global_mut::<Settings>();
        s.word_wrap = !s.word_wrap;
        s.save();
        cx.refresh_windows();
    }

    fn toggle_status_bar(&mut self, cx: &mut Context<Self>) {
        let s = cx.global_mut::<Settings>();
        s.status_bar = !s.status_bar;
        s.save();
        cx.refresh_windows();
    }

    fn show_dialog(&mut self, d: Dialog, cx: &mut Context<Self>) {
        self.dialog = Some(d);
        cx.notify();
    }

    fn close_overlay(&mut self, cx: &mut Context<Self>) {
        self.close_menu(cx);
        if self.dialog.take().is_some() {
            cx.notify();
        }
    }

    fn update_title(&self, window: &mut Window, cx: &App) {
        if let Some(t) = self.tabs.get(self.active) {
            let b = t.buffer.read(cx);
            let dirty = if b.is_dirty() { "● " } else { "" };
            window.set_window_title(&format!("{dirty}{} - rustpad++", b.file_name()));
        }
    }

    fn flash(&mut self, msg: SharedString, cx: &mut Context<Self>) {
        self.message = Some(msg.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(5)).await;
            this.update(cx, |ws, cx| {
                if ws.message.as_ref() == Some(&msg) {
                    ws.message = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn focus_active(&self, window: &mut Window, cx: &mut App) {
        let t = self.tab();
        let handle = match t.mode {
            ViewMode::Text => t.text.focus_handle(cx),
            ViewMode::Hex => t.hex.as_ref().map(|v| v.focus_handle(cx)).unwrap_or(t.text.focus_handle(cx)),
            ViewMode::Tree => t.tree.as_ref().map(|v| v.focus_handle(cx)).unwrap_or(t.text.focus_handle(cx)),
            ViewMode::Markdown => t.md.as_ref().map(|v| v.focus_handle(cx)).unwrap_or(t.text.focus_handle(cx)),
        };
        window.focus(&handle);
    }

    fn jump_to_text(&mut self, offset: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(ViewMode::Text, window, cx);
        self.tab().text.update(cx, |v, cx| v.go_to(offset..offset, cx));
    }

    fn set_mode(&mut self, mode: ViewMode, window: &mut Window, cx: &mut Context<Self>) {
        let i = self.active;
        let buffer = self.tabs[i].buffer.clone();
        // Carry the position over between text and hex.
        let cursor = match self.tabs[i].mode {
            ViewMode::Hex => self.tabs[i].hex.as_ref().map(|h| h.read(cx).cursor()),
            ViewMode::Text => Some(self.tabs[i].text.read(cx).cursor()),
            _ => None,
        };
        match mode {
            ViewMode::Text => {
                if let (Some(c), ViewMode::Hex) = (cursor, self.tabs[i].mode) {
                    self.tabs[i].text.update(cx, |v, cx| v.go_to(c..c, cx));
                }
            }
            ViewMode::Hex => {
                if self.tabs[i].hex.is_none() {
                    let v = cx.new(|cx| HexView::new(buffer, window, cx));
                    let sub = cx.subscribe_in(&v, window, |ws, _, e: &JumpTo, window, cx| ws.jump_to_text(e.0, window, cx));
                    self.tabs[i]._subs.push(sub);
                    self.tabs[i].hex = Some(v);
                }
                if let Some(c) = cursor {
                    self.tabs[i].hex.as_ref().unwrap().update(cx, |v, cx| v.go_to(c, cx));
                }
            }
            ViewMode::Tree => {
                if self.tabs[i].tree.is_none() {
                    let v = cx.new(|cx| JsonView::new(buffer, window, cx));
                    let sub = cx.subscribe_in(&v, window, |ws, _, e: &JumpTo, window, cx| ws.jump_to_text(e.0, window, cx));
                    self.tabs[i]._subs.push(sub);
                    self.tabs[i].tree = Some(v);
                }
            }
            ViewMode::Markdown => {
                if self.tabs[i].md.is_none() {
                    let v = cx.new(|cx| MarkdownView::new(buffer, window, cx));
                    let sub = cx.subscribe_in(&v, window, |ws, _, e: &JumpTo, window, cx| ws.jump_to_text(e.0, window, cx));
                    self.tabs[i]._subs.push(sub);
                    self.tabs[i].md = Some(v);
                }
            }
        }
        self.tabs[i].mode = mode;
        if self.bar.is_none() {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    fn available_modes(kind: Kind) -> &'static [ViewMode] {
        match kind {
            Kind::Json => &[ViewMode::Text, ViewMode::Tree, ViewMode::Hex],
            Kind::Markdown => &[ViewMode::Text, ViewMode::Markdown, ViewMode::Hex],
            Kind::Text | Kind::Binary => &[ViewMode::Text, ViewMode::Hex, ViewMode::Tree, ViewMode::Markdown],
        }
    }

    // ------------------------------------------------------------ actions

    fn open(&mut self, _: &Open, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update_in(cx, |ws, window, cx| {
                    for p in paths {
                        ws.open_path(&p, window, cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn new_file(&mut self, _: &NewFile, window: &mut Window, cx: &mut Context<Self>) {
        self.new_tab(Buffer::empty(), window, cx);
    }

    fn save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        match self.tab().buffer.read(cx).path.clone() {
            Some(p) => {
                let b = self.tab().buffer.clone();
                self.save_to(b, p, window, cx)
            }
            None => self.save_as(&SaveAs, window, cx),
        }
    }

    fn save_as(&mut self, _: &SaveAs, window: &mut Window, cx: &mut Context<Self>) {
        let b = self.tab().buffer.read(cx);
        let dir = b
            .path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let name = b.file_name();
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update_in(cx, |ws, window, cx| {
                    let b = ws.tab().buffer.clone();
                    ws.save_to(b, path, window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    fn save_all(&mut self, _: &SaveAll, window: &mut Window, cx: &mut Context<Self>) {
        let mut unnamed = 0;
        for t in &self.tabs {
            let b = t.buffer.read(cx);
            if !b.is_dirty() {
                continue;
            }
            match &b.path {
                Some(p) => self.save_queue.push((t.buffer.clone(), p.clone())),
                None => unnamed += 1,
            }
        }
        if unnamed > 0 {
            self.flash(trn!("{n} untitled file: please save it with “Save As”", "{n} untitled files: please save them one by one with “Save As”", unnamed).into(), cx);
        }
        self.next_save(window, cx);
    }

    fn next_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.job.is_none()
            && let Some((b, p)) = self.save_queue.pop()
        {
            self.save_to(b, p, window, cx);
        }
    }

    fn save_to(&mut self, buffer: Entity<Buffer>, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.job.is_some() {
            self.save_queue.push((buffer, path));
            return;
        }
        let snapshot = buffer.read(cx).snapshot();
        let version = buffer.read(cx).version();
        let total = snapshot.len();
        let progress = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (p, c) = (progress.clone(), cancel.clone());
        let target = path.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    snapshot.write_to(&target, |n| {
                        p.store(n, Ordering::Relaxed);
                        !c.load(Ordering::Relaxed)
                    })
                })
                .await;
            this.update_in(cx, |ws, window, cx| {
                ws.job = None;
                match result {
                    Ok(()) => {
                        buffer.update(cx, |b, cx| {
                            b.finish_save(&path, version);
                            cx.notify();
                        });
                        ws.flash(tr!("Saved {path}", path = path.display()).into(), cx);
                        ws.remember(&path, cx);
                    }
                    Err(e) => ws.flash(tr!("Save failed: {error}", error = e).into(), cx),
                }
                ws.update_title(window, cx);
                ws.next_save(window, cx);
            })
            .ok();
        });
        self.job = Some(Job { label: n_!("Saving"), total, progress, cancel, _task: task });
        cx.notify();
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab_at(self.active, window, cx);
    }

    fn close_tab_at(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs[i].buffer.read(cx).is_dirty() {
            let name = self.tabs[i].buffer.read(cx).file_name();
            let rx = window.prompt(
                PromptLevel::Warning,
                &tr!("Save changes to {name}?", name = name),
                None,
                &[tr!("Save"), tr!("Discard"), tr!("Cancel")],
                cx,
            );
            let buffer = self.tabs[i].buffer.clone();
            cx.spawn_in(window, async move |this, cx| {
                let Ok(answer) = rx.await else { return };
                this.update_in(cx, |ws, window, cx| {
                    let Some(i) = ws.tabs.iter().position(|t| t.buffer == buffer) else { return };
                    match answer {
                        0 => {
                            ws.active = i;
                            ws.save(&Save, window, cx);
                        }
                        1 => ws.remove_tab(i, window, cx),
                        _ => {}
                    }
                })
                .ok();
            })
            .detach();
            return;
        }
        self.remove_tab(i, window, cx);
    }

    fn remove_tab(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs.remove(i);
        if self.tabs.is_empty() {
            self.new_tab(Buffer::empty(), window, cx);
        }
        self.active = self.active.min(self.tabs.len() - 1);
        self.focus_active(window, cx);
        self.update_title(window, cx);
        cx.notify();
    }

    /// The changelog that comes with this version, as a Markdown tab (like
    /// Notepad++ shows its change.log). Opened again, it is just switched to.
    pub(crate) fn show_changelog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        const CHANGELOG: &str = include_str!("../CHANGELOG.md");
        const TITLE: &str = "CHANGELOG.md";
        self.dialog = None;
        let open = self.tabs.iter().position(|t| {
            let b = t.buffer.read(cx);
            b.path.is_none() && b.title.as_deref() == Some(TITLE)
        });
        if let Some(i) = open {
            self.switch_tab(i, window, cx);
            return;
        }
        let mut buffer = Buffer::from_text(CHANGELOG);
        buffer.title = Some(TITLE.into());
        self.new_tab(buffer, window, cx);
        self.tabs[self.active].kind = Kind::Markdown;
        self.set_mode(ViewMode::Markdown, window, cx);
    }

    fn switch_tab(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active = i;
        self.focus_active(window, cx);
        self.update_title(window, cx);
        cx.notify();
    }

    fn confirm_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // Remember size and position, whatever happens next.
        let s = cx.global_mut::<Settings>();
        s.window = Some(current_placement(window));
        s.save();
        self.bounds_dirty = false;
        if self.session_enabled(cx) {
            // Unsaved changes are kept in the session, no need to ask.
            session::write_logged(&self.session_snapshot(true, cx));
            return true;
        }
        let dirty = self.tabs.iter().filter(|t| t.buffer.read(cx).is_dirty()).count();
        if dirty == 0 {
            return true;
        }
        let rx = window.prompt(
            PromptLevel::Warning,
            &trn!("{n} document with unsaved changes.", "{n} documents with unsaved changes.", dirty),
            Some(tr!("Quit anyway? The changes will be lost.")),
            &[tr!("Quit"), tr!("Cancel")],
            cx,
        );
        cx.spawn(async move |_, cx| {
            if let Ok(0) = rx.await {
                cx.update(|cx| cx.quit()).ok();
            }
        })
        .detach();
        false
    }

    pub(crate) fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_quit(window, cx) {
            cx.quit();
        }
    }

    fn zoom(&mut self, delta: f32, cx: &mut Context<Self>) {
        let s = cx.global_mut::<Settings>();
        s.font_size = if delta == 0.0 { px(theme::DEFAULT_FONT_SIZE) } else { (s.font_size + px(delta)).clamp(px(7.), px(48.)) };
        s.save();
        cx.refresh_windows();
    }

    // ------------------------------------------------------------ find / goto

    fn open_bar(&mut self, kind: BarKind, window: &mut Window, cx: &mut Context<Self>) {
        let search = |k: BarKind| matches!(k, BarKind::Find | BarKind::Replace);
        let initial = match &self.bar {
            // Reopening, or switching between find and replace, keeps the search text.
            Some(b) if b.kind == kind || (search(b.kind) && search(kind)) => b.input.read(cx).content(cx),
            _ if search(kind) => {
                let t = self.tab().text.read(cx);
                let sel = t.selection();
                if !sel.is_empty() && sel.end - sel.start < 256 {
                    self.tab().buffer.read(cx).text(sel).replace(['\n', '\r'], "")
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        };
        let replace_initial = self
            .bar
            .as_ref()
            .and_then(|b| b.replace.as_ref())
            .map(|(r, _)| r.read(cx).content(cx))
            .unwrap_or_default();
        let input = cx.new(|cx| TextView::single_line(&initial, cx));
        let sub = cx.subscribe_in(&input, window, |ws, _, e: &TextEvent, window, cx| match e {
            TextEvent::Submitted => ws.submit_bar(window, cx),
            TextEvent::Cancelled => ws.close_bar(window, cx),
            TextEvent::TabbedOut => ws.cycle_bar_focus(window, cx),
            _ => {}
        });
        let replace = (kind == BarKind::Replace).then(|| {
            let r = cx.new(|cx| TextView::single_line(&replace_initial, cx));
            let sub = cx.subscribe_in(&r, window, |ws, _, e: &TextEvent, window, cx| match e {
                TextEvent::Submitted => ws.replace_one(window, cx),
                TextEvent::Cancelled => ws.close_bar(window, cx),
                TextEvent::TabbedOut => ws.cycle_bar_focus(window, cx),
                _ => {}
            });
            (r, sub)
        });
        window.focus(&input.focus_handle(cx));
        input.update(cx, |v, cx| v.select_all_text(cx));
        self.bar = Some(Bar { kind, input, replace, _sub: sub });
        cx.notify();
    }

    /// Tab between the find and replace fields.
    fn cycle_bar_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = &self.bar else { return };
        let Some((replace, _)) = &bar.replace else { return };
        let next = if bar.input.focus_handle(cx).is_focused(window) { replace.clone() } else { bar.input.clone() };
        window.focus(&next.focus_handle(cx));
        next.update(cx, |v, cx| v.select_all_text(cx));
    }

    fn close_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.bar = None;
        if let Some(j) = self.job.take_if(|j| j.label == "Searching" || j.label == "Replacing") {
            j.cancel.store(true, Ordering::Relaxed);
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    fn search_terms(&self, cx: &App) -> Option<(String, String)> {
        let bar = self.bar.as_ref().filter(|b| b.kind == BarKind::Replace)?;
        let needle = bar.input.read(cx).content(cx);
        let with = bar.replace.as_ref().map(|(r, _)| r.read(cx).content(cx)).unwrap_or_default();
        (!needle.is_empty()).then_some((needle, with))
    }

    /// Replace the current match (if the selection is one) and find the next.
    fn replace_one(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((needle, with)) = self.search_terms(cx) else { return };
        if self.tab().mode == ViewMode::Text {
            let text = self.tab().text.clone();
            let sel = text.read(cx).selection();
            if sel.end - sel.start == needle.len() as u64 && self.tab().buffer.read(cx).slice(sel).as_ref() == needle.as_bytes() {
                text.update(cx, |v, cx| {
                    v.highlight = None;
                    v.insert(&with, cx);
                });
            }
        }
        self.find(true, window, cx);
    }

    /// Replace every match in the document as one undo step.
    fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((needle, with)) = self.search_terms(cx) else { return };
        if let Some(j) = self.job.take() {
            j.cancel.store(true, Ordering::Relaxed);
        }
        if self.tab().mode != ViewMode::Text {
            self.set_mode(ViewMode::Text, window, cx);
        }
        let text = self.tab().text.clone();
        let buffer = self.tab().buffer.read(cx);
        let snapshot = buffer.snapshot();
        let version = buffer.version();
        let total = buffer.len();
        let progress = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (p, c) = (progress.clone(), cancel.clone());
        let task = cx.spawn_in(window, async move |this, cx| {
            let n = needle.into_bytes();
            let nlen = n.len() as u64;
            let hits = cx
                .background_executor()
                .spawn(async move {
                    let mut hits = Vec::new();
                    let mut from = 0;
                    let tick = |pos: u64| {
                        p.store(pos, Ordering::Relaxed);
                        !c.load(Ordering::Relaxed)
                    };
                    while let Some(h) = snapshot.find(&n, from, tick) {
                        hits.push(h);
                        from = h + nlen;
                    }
                    (!c.load(Ordering::Relaxed)).then_some(hits)
                })
                .await;
            this.update_in(cx, |ws, _window, cx| {
                ws.job = None;
                let Some(hits) = hits else { return };
                let msg = if hits.is_empty() {
                    tr!("Not found").to_string()
                } else if text.read(cx).buffer_version(cx) != version {
                    tr!("Document changed while searching - nothing replaced").to_string()
                } else {
                    text.update(cx, |v, cx| v.replace_all(&hits, nlen, &with, cx));
                    trn!("Replaced {n} occurrence", "Replaced {n} occurrences", hits.len())
                };
                ws.flash(msg.into(), cx);
                cx.notify();
            })
            .ok();
        });
        self.job = Some(Job { label: n_!("Replacing"), total, progress, cancel, _task: task });
        cx.notify();
    }

    fn submit_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = &self.bar else { return };
        match bar.kind {
            BarKind::Find | BarKind::Replace => self.find(true, window, cx),
            BarKind::Goto => {
                let q = bar.input.read(cx).content(cx).trim().to_string();
                self.goto(&q, window, cx);
            }
        }
    }

    fn goto(&mut self, q: &str, window: &mut Window, cx: &mut Context<Self>) {
        let t = self.tab();
        let b = t.buffer.read(cx);
        let len = b.len();
        let target = if let Some(hex) = q.strip_prefix("0x").or_else(|| q.strip_prefix("0X")) {
            u64::from_str_radix(hex, 16).ok().map(|o| o.min(len))
        } else if let Some(p) = q.strip_suffix('%') {
            p.trim().parse::<f64>().ok().map(|p| ((len as f64) * p.clamp(0.0, 100.0) / 100.0) as u64)
        } else if t.mode == ViewMode::Hex {
            q.parse::<u64>().ok().map(|o| o.min(len))
        } else {
            match q.parse::<u64>() {
                Ok(line) => match b.offset_of_line(line.saturating_sub(1)) {
                    Some(o) => Some(o),
                    None if b.line_index().is_done() => Some(len),
                    None => {
                        let pct = (b.line_index().progress() * 100.0) as u32;
                        self.flash(tr!("Line {line} not indexed yet ({pct}% scanned) - try 0x<offset> or <n>%", line = line, pct = pct).into(), cx);
                        return;
                    }
                },
                Err(_) => None,
            }
        };
        let Some(target) = target else {
            self.flash(tr!("Enter a line number, 0x<hex offset> or <percent>%").into(), cx);
            return;
        };
        self.bar = None;
        let t = self.tab();
        match t.mode {
            ViewMode::Hex => {
                if let Some(h) = &t.hex {
                    h.update(cx, |v, cx| v.go_to(target, cx));
                }
                self.focus_active(window, cx);
            }
            _ => {
                let wrap = t.text.read(cx).wrap();
                let target = if q.ends_with('%') { rows::row_containing(b, target, wrap).start } else { target };
                self.jump_to_text(target, window, cx);
                self.focus_active(window, cx);
            }
        }
        cx.notify();
    }

    fn find(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let needle = match &self.bar {
            Some(b) if matches!(b.kind, BarKind::Find | BarKind::Replace) => b.input.read(cx).content(cx),
            _ => {
                self.open_bar(BarKind::Find, window, cx);
                return;
            }
        };
        if needle.is_empty() {
            return;
        }
        if let Some(j) = self.job.take() {
            j.cancel.store(true, Ordering::Relaxed);
        }
        if self.tab().mode != ViewMode::Text {
            self.set_mode(ViewMode::Text, window, cx);
        }
        let text = self.tab().text.clone();
        let buffer = self.tab().buffer.read(cx);
        let snapshot = buffer.snapshot();
        let total = buffer.len();
        let sel = text.read(cx).selection();
        let from = if forward { if sel.is_empty() { sel.start } else { sel.start + 1 } } else { sel.start };
        let progress = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (p, c) = (progress.clone(), cancel.clone());
        let task = cx.spawn_in(window, async move |this, cx| {
            let n = needle.into_bytes();
            let nlen = n.len() as u64;
            let (hit, wrapped) = cx
                .background_executor()
                .spawn(async move {
                    let tick = |pos: u64| {
                        p.store(pos, Ordering::Relaxed);
                        !c.load(Ordering::Relaxed)
                    };
                    let first = if forward { snapshot.find(&n, from, tick) } else { snapshot.rfind(&n, from, tick) };
                    if first.is_some() || c.load(Ordering::Relaxed) {
                        return (first, false);
                    }
                    // Wrap around.
                    let again = if forward {
                        snapshot.find(&n, 0, tick).filter(|&h| h < from)
                    } else {
                        snapshot.rfind(&n, snapshot.len(), tick).filter(|&h| h >= from)
                    };
                    (again, true)
                })
                .await;
            this.update_in(cx, |ws, _window, cx| {
                ws.job = None;
                match hit {
                    Some(h) => {
                        text.update(cx, |v, cx| {
                            v.highlight = Some(h..h + nlen);
                            v.go_to(h..h + nlen, cx);
                        });
                        if wrapped {
                            ws.flash(tr!("Search wrapped around").into(), cx);
                        }
                    }
                    None => ws.flash(tr!("Not found").into(), cx),
                }
                cx.notify();
            })
            .ok();
        });
        self.job = Some(Job { label: n_!("Searching"), total, progress, cancel, _task: task });
        cx.notify();
    }

    // ------------------------------------------------------------ rendering

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Double-click on the free area of the tab bar opens a new document.
        let mut strip = div()
            .id("tab-strip")
            .flex()
            .flex_row()
            .flex_1()
            .h_full()
            .min_w_0()
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|ws, e: &gpui::MouseDownEvent, window, cx| {
                    if e.click_count == 2 {
                        ws.new_tab(Buffer::empty(), window, cx);
                    }
                }),
            );
        for (i, t) in self.tabs.iter().enumerate() {
            let b = t.buffer.read(cx);
            let active = i == self.active;
            let name = b.file_name();
            let dirty = b.is_dirty();
            strip = strip.child(
                div()
                    .id(("tab", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h_full()
                    .border_r_1()
                    .border_color(theme::border())
                    .when(active, |d| d.bg(theme::bg()).border_t_2().border_color(theme::accent()))
                    .when(!active, |d| d.text_color(theme::text_dim()).hover(|s| s.bg(theme::bg_hover())))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |ws, _, window, cx| {
                        // Keep clicks on a tab away from the strip's double-click handler.
                        cx.stop_propagation();
                        ws.close_menu(cx);
                        ws.switch_tab(i, window, cx)
                    }))
                    .on_mouse_down(MouseButton::Middle, cx.listener(move |ws, _, window, cx| ws.close_tab_at(i, window, cx)))
                    .child(name)
                    .child(
                        div()
                            .id(("close", i))
                            .text_color(if dirty { theme::warn() } else { theme::text_faint() })
                            .hover(|s| s.text_color(theme::text()))
                            .on_mouse_down(MouseButton::Left, cx.listener(move |ws, _, window, cx| {
                                cx.stop_propagation();
                                ws.close_tab_at(i, window, cx)
                            }))
                            .child(if dirty { "●" } else { "×" }),
                    ),
            );
        }
        strip
    }

    fn render_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bar = self.bar.as_ref()?;
        let (label, hint) = match bar.kind {
            BarKind::Find | BarKind::Replace => (tr!("Find"), tr!("Enter / F3: next   Shift+F3: previous   Esc: close")),
            BarKind::Goto => (tr!("Go to"), tr!("line number, 0x<offset> or <percent>%")),
        };
        let field = |input: &Entity<TextView>| {
            div()
                .w(px(360.))
                .px_1()
                .bg(theme::bg())
                .border_1()
                .border_color(theme::accent())
                .rounded_sm()
                .child(input.clone())
        };
        let row = |label: &'static str| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().text_color(theme::text_dim()).w(px(72.)).flex_none().whitespace_nowrap().child(label))
        };
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_3()
                .rounded_sm()
                .border_1()
                .border_color(theme::border())
                .bg(theme::bg_alt())
                .hover(|s| s.bg(theme::bg_hover()))
                .child(label)
        };
        let first = row(label)
            .child(field(&bar.input))
            .child(div().text_size(px(12.)).text_color(theme::text_faint()).child(hint))
            .child(div().flex_1())
            .child(
                div()
                    .id("close-bar")
                    .px_2()
                    .text_color(theme::text_dim())
                    .hover(|s| s.text_color(theme::text()))
                    .on_click(cx.listener(|ws, _, window, cx| ws.close_bar(window, cx)))
                    .child("×"),
            );
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_1()
                .bg(theme::bg_panel())
                .border_b_1()
                .border_color(theme::border())
                .child(first)
                .children(bar.replace.as_ref().map(|(input, _)| {
                    row(tr!("Replace"))
                        .child(field(input))
                        .child(
                            button("replace-one", tr!("Replace"))
                                .on_click(cx.listener(|ws, _, window, cx| ws.replace_one(window, cx))),
                        )
                        .child(
                            button("replace-all", tr!("Replace all"))
                                .on_click(cx.listener(|ws, _, window, cx| ws.replace_all(window, cx))),
                        )
                        .child(div().text_size(px(12.)).text_color(theme::text_faint()).child(tr!("Enter: replace and find next")))
                }))
                .into_any_element(),
        )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.tab();
        let b = t.buffer.read(cx);
        let len = b.len();
        let mut left: Vec<String> = Vec::new();
        match t.mode {
            ViewMode::Hex => {
                if let Some(h) = &t.hex {
                    let h = h.read(cx);
                    let c = h.cursor();
                    let byte = b.byte_at(c).map(|v| tr!("  value {value} ({hex})", value = v, hex = format!("0x{v:02X}"))).unwrap_or_default();
                    left.push(tr!("Offset {hex} ({offset}){byte}", hex = format!("0x{c:X}"), offset = c, byte = byte));
                }
            }
            _ => {
                let tv = t.text.read(cx);
                let c = tv.cursor();
                let ls = rows::line_start(b, c);
                let col = String::from_utf8_lossy(&b.slice(ls..c)).chars().count() + 1;
                let ln = b.line_of_offset(c).map(|l| (l + 1).to_string()).unwrap_or_else(|| "?".into());
                let sel = tv.selection();
                left.push(tr!("Ln {line}, Col {column}", line = ln, column = col));
                if !sel.is_empty() {
                    left.push(trn!("Sel {n} byte", "Sel {n} bytes", sel.end - sel.start));
                }
                left.push(tr!("Pos {offset}", offset = c));
            }
        }
        let mut right: Vec<String> = Vec::new();
        if let Some(j) = &self.job {
            let done = j.progress.load(Ordering::Relaxed);
            let pct = if j.total == 0 { 100.0 } else { done as f64 * 100.0 / j.total as f64 };
            right.push(format!("{}… {pct:.0}%", crate::i18n::tr(j.label)));
        }
        let idx = b.line_index();
        match b.line_count() {
            Some(n) => right.push(trn!("{n} line", "{n} lines", n)),
            None if !idx.is_done() => right.push(tr!("indexing lines {pct}%", pct = format!("{:.0}", idx.progress() * 100.0))),
            None => {}
        }
        if t.follow {
            right.push(tr!("following end of file").into());
        }
        right.push(human_size(len));
        right.push(b.line_ending.label().into());
        match t.kind {
            Kind::Binary => right.push(tr!("Binary").into()),
            Kind::Json => right.push("JSON".into()),
            Kind::Markdown => right.push("Markdown".into()),
            Kind::Text => {}
        }
        let bom = b.bom.then(|| {
            div()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(theme::border())
                .text_size(px(10.))
                .text_color(theme::text_faint())
                .child("BOM")
        });
        let encoding = self.render_encoding_button(b.encoding, cx);

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_4()
            .px_3()
            .h(px(24.))
            .bg(theme::bg_alt())
            .border_t_1()
            .border_color(theme::border())
            .text_size(px(12.))
            .text_color(theme::text_dim())
            .children(left)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(theme::warn())
                    .children(self.message.clone()),
            )
            .children(right)
            .children(bom)
            .child(encoding)
    }

    fn render_encoding_button(&self, current: Encoding, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.encoding_menu;
        let mut el = div()
            .id("encoding")
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_1()
            .rounded_sm()
            .cursor_pointer()
            .when(open, |d| d.bg(theme::bg_active()))
            .when(!open, |d| d.hover(|s| s.bg(theme::bg_hover())))
            .on_mouse_down(MouseButton::Left, cx.listener(|ws, _, _, cx| {
                // Keep the window's close-on-click from closing it right away.
                cx.stop_propagation();
                let was_open = ws.encoding_menu;
                ws.close_menu(cx);
                ws.encoding_menu = !was_open;
                cx.notify();
            }))
            .child(current.label())
            .child(div().text_color(theme::text_faint()).child("▴"));
        if open {
            el = el.child(deferred(div().absolute().bottom(px(22.)).right_0().child(self.render_encoding_menu(cx))).with_priority(1));
        }
        el
    }

    fn render_encoding_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let b = self.tab().buffer.read(cx);
        let (current, bom, has_file) = (b.encoding, b.bom, b.path.is_some());
        let heading = |text: &'static str| {
            div()
                .flex_none()
                .flex()
                .items_end()
                .h(px(22.))
                .px_3()
                .pb(px(2.))
                .text_size(px(11.))
                .text_color(theme::text_faint())
                .child(text.to_uppercase())
        };
        let item = |id: (&'static str, usize), label: String, checked: bool, enabled: bool| {
            div()
                .id(id)
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .h(px(24.))
                .mx_1()
                .pl_1()
                .pr_3()
                .rounded_sm()
                .when(enabled, |d| d.hover(|s| s.bg(theme::bg_hover())))
                .when(!enabled, |d| d.text_color(theme::text_faint()))
                .child(div().w(px(20.)).flex().justify_center().text_color(theme::accent()).child(if checked { "✓" } else { "" }))
                .child(label)
        };
        let mut list = div()
            .id("encoding-menu")
            .occlude()
            .flex()
            .flex_col()
            .min_w(px(230.))
            .py_1()
            .bg(theme::bg_panel())
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .shadow_lg()
            .font_family("DejaVu Sans")
            .text_size(px(13.))
            .text_color(theme::text())
            .child(heading(tr!("Reopen with encoding")));
        for (i, enc) in Encoding::ALL.into_iter().enumerate() {
            let row = item(("reopen-as", i), enc.label().to_string(), enc == current, has_file);
            list = list.child(if has_file {
                row.on_click(cx.listener(move |ws, _, window, cx| {
                    ws.close_menu(cx);
                    ws.reopen_with_encoding(enc, window, cx);
                }))
            } else {
                row
            });
        }
        list = list.child(div().my_1().h(px(1.)).bg(theme::border())).child(heading(tr!("Convert to")));
        let targets = [
            (Encoding::Utf8, false),
            (Encoding::Utf8, true),
            (Encoding::Utf16Le, true),
            (Encoding::Utf16Be, true),
            (Encoding::Windows1252, false),
            (Encoding::Iso8859_1, false),
            (Encoding::Iso8859_15, false),
        ];
        for (i, (enc, with_bom)) in targets.into_iter().enumerate() {
            let checked = enc == current && with_bom == bom;
            list = list.child(item(("convert-to", i), encoding_label(enc, with_bom), checked, true).on_click(cx.listener(
                move |ws, _, window, cx| {
                    ws.close_menu(cx);
                    ws.convert_encoding(enc, with_bom, window, cx);
                },
            )));
        }
        list
    }

    /// Read the file again in another encoding (asks first if that loses changes).
    fn reopen_with_encoding(&mut self, enc: Encoding, window: &mut Window, cx: &mut Context<Self>) {
        let buffer = self.tab().buffer.clone();
        let b = buffer.read(cx);
        if b.path.is_none() {
            return;
        }
        if !b.is_dirty() {
            self.reopen_tab_as(self.active, enc, window, cx);
            return;
        }
        let rx = window.prompt(
            PromptLevel::Warning,
            &tr!("Reopen {name} as {encoding}?", name = b.file_name(), encoding = enc.label()),
            Some(tr!("Your unsaved changes will be lost.")),
            &[tr!("Reopen"), tr!("Cancel")],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = rx.await {
                this.update_in(cx, |ws, window, cx| {
                    if let Some(i) = ws.tabs.iter().position(|t| t.buffer == buffer) {
                        ws.reopen_tab_as(i, enc, window, cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn reopen_tab_as(&mut self, i: usize, enc: Encoding, window: &mut Window, cx: &mut Context<Self>) {
        let result = self.tabs[i].buffer.update(cx, |b, cx| {
            let old = b.encoding;
            b.encoding = enc;
            let r = b.reload();
            if r.is_err() {
                b.encoding = old;
            }
            cx.notify();
            r
        });
        if let Err(e) = result {
            self.flash(tr!("Reload failed: {error}", error = e).into(), cx);
            return;
        }
        // Text that looked binary in the wrong encoding may be readable now.
        let b = self.tabs[i].buffer.read(cx);
        let kind = detect::detect(b.path.as_deref(), &b.slice(0..b.len().min(8192)));
        self.tabs[i].kind = kind;
        if i == self.active && kind != Kind::Binary && self.tabs[i].mode == ViewMode::Hex {
            self.set_mode(ViewMode::Text, window, cx);
        }
        self.update_title(window, cx);
        cx.notify();
    }

    /// Save in another encoding from now on.
    fn convert_encoding(&mut self, enc: Encoding, bom: bool, window: &mut Window, cx: &mut Context<Self>) {
        let buffer = self.tab().buffer.clone();
        let b = buffer.read(cx);
        if (b.encoding, b.bom) == (enc, bom) {
            return;
        }
        // Check up front while that is cheap; saving checks again anyway.
        if !enc.is_unicode()
            && b.len() <= 64 << 20
            && let Some(c) = encoding::first_unmappable(&b.text(0..b.len()), enc)
        {
            self.flash(tr!("Cannot convert to {encoding}: “{char}” does not exist there", encoding = enc.label(), char = c).into(), cx);
            return;
        }
        buffer.update(cx, |b, cx| {
            b.convert(enc, bom);
            cx.notify();
        });
        self.update_title(window, cx);
        self.flash(tr!("Will be saved as {encoding}", encoding = encoding_label(enc, bom)).into(), cx);
    }
}

/// A small button for bars and dialogs; `primary` in the accent color.
fn bar_button(id: &'static str, label: &'static str, primary: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .px_3()
        .py(px(2.))
        .rounded_sm()
        .border_1()
        .border_color(theme::border())
        .cursor_pointer()
        .when(primary, |d| d.bg(theme::accent()).text_color(theme::on_accent()))
        .when(!primary, |d| d.bg(theme::bg_alt()).hover(|s| s.bg(theme::bg_hover())))
        .child(label)
}

/// "UTF-8 with BOM" etc. for menus and messages.
fn encoding_label(enc: Encoding, bom: bool) -> String {
    if bom { tr!("{encoding} with BOM", encoding = enc.label()) } else { enc.label().to_string() }
}


pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_title(window, cx);
        let t = self.tab();
        let content: AnyElement = match t.mode {
            ViewMode::Text => t.text.clone().into_any_element(),
            ViewMode::Hex => t.hex.clone().map(|v| v.into_any_element()).unwrap_or_else(|| div().into_any_element()),
            ViewMode::Tree => t.tree.clone().map(|v| v.into_any_element()).unwrap_or_else(|| div().into_any_element()),
            ViewMode::Markdown => t.md.clone().map(|v| v.into_any_element()).unwrap_or_else(|| div().into_any_element()),
        };
        let s = Settings::get(cx);
        let status_bar = s.status_bar;
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .font_family(s.mono_family.clone())
            .on_mouse_down(MouseButton::Left, cx.listener(|ws, _, _, cx| ws.close_menu(cx)))
            .on_action(cx.listener(|ws, _: &SaveAll, w, cx| ws.save_all(&SaveAll, w, cx)))
            .on_action(cx.listener(|_, _: &OpenSettings, _, cx| crate::settings_window::open(cx)))
            .on_action(cx.listener(|ws, _: &CloseOverlay, _, cx| ws.close_overlay(cx)))
            .text_size(px(13.))
            .on_action(cx.listener(Self::open))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::save_as))
            .on_action(cx.listener(Self::new_file))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::quit))
            .on_action(cx.listener(|ws, _: &NextTab, window, cx| {
                let i = (ws.active + 1) % ws.tabs.len();
                ws.switch_tab(i, window, cx)
            }))
            .on_action(cx.listener(|ws, _: &PrevTab, window, cx| {
                let i = (ws.active + ws.tabs.len() - 1) % ws.tabs.len();
                ws.switch_tab(i, window, cx)
            }))
            .on_action(cx.listener(|ws, _: &ShowText, w, cx| ws.set_mode(ViewMode::Text, w, cx)))
            .on_action(cx.listener(|ws, _: &ShowHex, w, cx| ws.set_mode(ViewMode::Hex, w, cx)))
            .on_action(cx.listener(|ws, _: &ShowTree, w, cx| ws.set_mode(ViewMode::Tree, w, cx)))
            .on_action(cx.listener(|ws, _: &ShowMarkdown, w, cx| ws.set_mode(ViewMode::Markdown, w, cx)))
            .on_action(cx.listener(|ws, _: &CycleView, w, cx| {
                let modes = Self::available_modes(ws.tab().kind);
                let i = modes.iter().position(|m| *m == ws.tab().mode).unwrap_or(0);
                ws.set_mode(modes[(i + 1) % modes.len()], w, cx)
            }))
            .on_action(cx.listener(|ws, _: &Find, w, cx| ws.open_bar(BarKind::Find, w, cx)))
            .on_action(cx.listener(|ws, _: &FindNext, w, cx| ws.find(true, w, cx)))
            .on_action(cx.listener(|ws, _: &FindPrev, w, cx| ws.find(false, w, cx)))
            .on_action(cx.listener(|ws, _: &Replace, w, cx| ws.open_bar(BarKind::Replace, w, cx)))
            .on_action(cx.listener(|ws, _: &ShowChangelog, w, cx| ws.show_changelog(w, cx)))
            .on_action(cx.listener(|ws, _: &GotoLine, w, cx| ws.open_bar(BarKind::Goto, w, cx)))
            .on_action(cx.listener(|ws, _: &ZoomIn, _, cx| ws.zoom(1.0, cx)))
            .on_action(cx.listener(|ws, _: &ZoomOut, _, cx| ws.zoom(-1.0, cx)))
            .on_action(cx.listener(|ws, _: &ZoomReset, _, cx| ws.zoom(0.0, cx)))
            .on_drop(cx.listener(|ws, paths: &ExternalPaths, window, cx| {
                for p in paths.paths() {
                    ws.open_path(p, window, cx);
                }
            }))
            .child(div().font_family("DejaVu Sans").child(self.render_menubar(cx)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(30.))
                    .border_t_1()
                    .bg(theme::bg_alt())
                    .border_b_1()
                    .border_color(theme::border())
                    .child(self.render_tabs(cx)),
            )
            .children(self.render_bar(cx))
            .children(self.render_external(cx))
            .children(Self::render_update_bar(cx))
            .child(div().flex_1().min_h_0().overflow_hidden().child(content))
            .when(status_bar, |d| d.child(self.render_status(cx)))
            .children(self.dialog.map(|d| self.render_dialog(d, cx)))
    }
}

static WORDMARK_LIGHT: LazyLock<Arc<Image>> = LazyLock::new(|| {
    Arc::new(Image::from_bytes(ImageFormat::Png, include_bytes!("../assets/logo/png/rustpad-wordmark.png").to_vec()))
});
/// Logo orange, independent of the color scheme.
const BRAND: u32 = 0xe2531f;
const BRAND_LIGHT: u32 = 0xf77b45;
const BRAND_DARK: u32 = 0xb9380f;

static WORDMARK_DARK: LazyLock<Arc<Image>> = LazyLock::new(|| {
    Arc::new(Image::from_bytes(ImageFormat::Png, include_bytes!("../assets/logo/png/rustpad-wordmark-dark.png").to_vec()))
});

impl Workspace {
    fn render_dialog(&self, d: Dialog, cx: &mut Context<Self>) -> AnyElement {
        let content = match d {
            Dialog::About => Self::render_about(),
            Dialog::Updates => Self::render_updates(cx),
        };
        deferred(
            div()
                .id("dialog-backdrop")
                .occlude()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::shadow())
                .on_mouse_down(MouseButton::Left, cx.listener(|ws, _, _, cx| ws.close_overlay(cx)))
                .child(
                    div()
                        .id("dialog")
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .w(px(if d == Dialog::About { 540. } else { 440. }))
                        // Small windows: the content scrolls, the footer stays.
                        .max_h(gpui::relative(0.94))
                        .flex()
                        .flex_col()
                        .bg(theme::bg_panel())
                        .border_1()
                        .border_color(theme::border())
                        .rounded_lg()
                        .shadow_lg()
                        .font_family("DejaVu Sans")
                        .child(div().id("dialog-content").flex_1().min_h_0().overflow_y_scroll().child(content))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .justify_end()
                                .px_6()
                                .py_3()
                                .border_t_1()
                                .border_color(theme::border())
                                .bg(theme::bg_alt())
                                .rounded_b_lg()
                                .child(
                                    div()
                                        .id("dialog-ok")
                                        .px_5()
                                        .py_1()
                                        .rounded_md()
                                        .bg(theme::accent())
                                        .text_color(theme::on_accent())
                                        .cursor_pointer()
                                        .hover(|s| s.opacity(0.85))
                                        .on_click(cx.listener(|ws, _, _, cx| ws.close_overlay(cx)))
                                        .child("OK"),
                                ),
                        ),
                ),
        )
        .with_priority(3)
        .into_any_element()
    }

    /// "rustpad++ 0.2.0 is available" with install, release notes and skip.
    fn render_update_bar(cx: &mut Context<Self>) -> Option<AnyElement> {
        let release = update::available(cx)?;
        let busy = cx.try_global::<update::Updates>().is_some_and(|u| u.busy);
        let text = update::note(cx).unwrap_or_else(|| tr!("rustpad++ {version} is available.", version = release.version));
        let page = release.page.clone();
        let main = if update::can_update() {
            bar_button("update-install", tr!("Install"), true)
                .when(busy, |d| d.opacity(0.5))
                .on_click(|_, _, cx| update::install(update::Action::Update, cx))
        } else {
            // A build run from its own folder cannot replace itself.
            let page = page.clone();
            bar_button("update-download", tr!("Download"), true).on_click(move |_, _, cx| cx.open_url(&page))
        };
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .bg(theme::bg_panel())
                .border_b_1()
                .border_color(theme::accent())
                .font_family("DejaVu Sans")
                .text_size(px(13.))
                .child(div().text_color(theme::accent()).child("⬆"))
                .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(text))
                .child(main)
                .child(bar_button("update-notes", tr!("What's new?"), false).on_click(move |_, _, cx| cx.open_url(&page)))
                .child(
                    bar_button("update-skip", tr!("Skip this version"), false)
                        .when(busy, |d| d.opacity(0.5))
                        .on_click(|_, _, cx| update::skip(cx)),
                )
                .into_any_element(),
        )
    }

    /// Help → Check for Updates: version, state of the check, and what can be done.
    fn render_updates(cx: &mut Context<Self>) -> AnyElement {
        let (text, dot) = update::status(cx);
        let state = cx.try_global::<update::Updates>();
        let (busy, checking) = state.map_or((false, false), |u| (u.busy, u.checking));
        let newer = update::newer(cx);
        let mut buttons = div().flex().flex_row().flex_wrap().gap_2().child(
            bar_button("updates-check", tr!("Check now"), newer.is_none())
                .when(checking || busy, |d| d.opacity(0.5))
                .on_click(|_, _, cx| update::check(cx)),
        );
        if let Some(release) = newer {
            buttons = buttons.child(if update::can_update() {
                bar_button("updates-install", tr!("Install"), true)
                    .when(busy, |d| d.opacity(0.5))
                    .on_click(|_, _, cx| update::install(update::Action::Update, cx))
            } else {
                bar_button("updates-download", tr!("Download"), true).on_click(move |_, _, cx| cx.open_url(&release.page))
            });
        }
        if update::is_build() {
            buttons = buttons.child(
                bar_button("updates-install-app", tr!("Install for all users"), false)
                    .when(busy, |d| d.opacity(0.5))
                    .on_click(|_, _, cx| update::install(update::Action::InstallApp, cx)),
            );
        }
        div()
            .p_6()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_size(px(18.)).child(tr!("Check for Updates")))
            .child(div().text_color(theme::text_dim()).child(tr!("Installed version: {version}", version = update::VERSION)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().flex_none().size(px(10.)).rounded_full().bg(dot))
                    .child(text),
            )
            .children(update::note(cx).map(|n| div().text_size(px(12.)).text_color(theme::warn()).child(n)))
            .child(buttons)
            .when(update::is_build(), |d| {
                d.child(div().text_size(px(12.)).text_color(theme::text_faint()).child(tr!(
                    "This rustpad++ runs from its build folder. Installed for all users, it gets a menu entry and can update itself."
                )))
            })
            .into_any_element()
    }

    fn render_about() -> AnyElement {
        // Wordmark text color follows the scheme. PNG, not SVG: gpui 0.2.2 skips the
        // RGBA->BGRA swap for SVG images, which turns the orange blue.
        let logo = if theme::scheme().dark { &WORDMARK_DARK } else { &WORDMARK_LIGHT };
        let build = if cfg!(debug_assertions) { tr!("Debug build") } else { tr!("Release build") };
        let repo = env!("CARGO_PKG_REPOSITORY");
        let repo_name = repo.trim_start_matches("https://github.com/");
        let author = env!("CARGO_PKG_AUTHORS");
        let config_dir = crate::config::path()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .map(|p| match std::env::var_os("HOME").map(PathBuf::from) {
                Some(home) if p.starts_with(&home) => {
                    format!("~/{}", p.strip_prefix(&home).unwrap_or(&p).display())
                }
                _ => p.display().to_string(),
            })
            .unwrap_or_else(|| "-".into());

        let icon = |name: &'static str, size: f32| gpui::svg().path(format!("icons/{name}.svg")).flex_none().size(px(size));
        let open = |url: String| move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| cx.open_url(&url);
        let tile = |glyph: &'static str, label: &'static str| {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .gap_1()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(theme::border())
                .bg(theme::bg_alt())
                .child(
                    div()
                        .font_family("DejaVu Sans Mono")
                        .font_weight(FontWeight::BOLD)
                        .text_size(px(16.))
                        .text_color(rgb(BRAND))
                        .child(glyph),
                )
                .child(div().text_size(px(12.)).text_color(theme::text_dim()).child(label))
        };
        // A button that opens a page of the project.
        // A button for a page of the project; the caller says what a click does.
        let link_button = |id: &'static str, glyph: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex_1()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap_2()
                .py(px(7.))
                .rounded_md()
                .border_1()
                .border_color(theme::border())
                .bg(theme::bg_alt())
                .text_size(px(12.5))
                .cursor_pointer()
                .hover(|s| s.border_color(rgb(BRAND)).bg(theme::bg_hover()))
                .child(icon(glyph, 15.).text_color(rgb(BRAND)))
                .child(label)
        };
        // Key/value row; the value opens `url` when given.
        let info = |id: &'static str, glyph: &'static str, key: &'static str, value: String, url: Option<String>| {
            let value = div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .min_w_0()
                .text_color(theme::text_dim())
                .child(value);
            let value = match url {
                Some(url) => value
                    .cursor_pointer()
                    .text_color(theme::accent())
                    .hover(|s| s.underline())
                    .on_click(open(url))
                    .child(icon("external", 11.).text_color(theme::accent())),
                None => value,
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .text_size(px(12.5))
                .child(icon(glyph, 14.).text_color(theme::text_faint()))
                .child(div().w(px(100.)).flex_none().text_color(theme::text_faint()).child(key))
                .child(value)
        };

        div()
            .flex()
            .flex_col()
            // Header: brand stripe, wordmark, version, claim.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .pb_5()
                    .bg(theme::bg())
                    .rounded_t_lg()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div().h(px(4.)).w_full().rounded_t_lg().bg(linear_gradient(
                            90.,
                            linear_color_stop(rgb(BRAND_LIGHT), 0.),
                            linear_color_stop(rgb(BRAND_DARK), 1.),
                        )),
                    )
                    .child(img(Arc::clone(logo)).mt_3().w(px(300.)).h(px(300. * 256. / 848.)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py(px(2.))
                            .rounded_full()
                            .border_1()
                            .border_color(theme::border())
                            .bg(theme::bg_panel())
                            .text_size(px(12.))
                            .text_color(theme::text_dim())
                            .child(tr!("Version {version}", version = env!("CARGO_PKG_VERSION")))
                            .child(div().text_color(theme::text_faint()).child("·"))
                            .child(build),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme::text())
                                    .child(tr!("Less editor. More text.")),
                            )
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(theme::text_dim())
                                    .child(tr!("No clutter, no waiting - just your file.")),
                            ),
                    ),
            )
            // Body: views, project links, facts, credits.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .px_6()
                    .pt_5()
                    .pb_6()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(tile("Aa", tr!("Text")))
                            .child(tile("0x", tr!("Hex")))
                            .child(tile("{ }", tr!("JSON Tree")))
                            .child(tile("#", tr!("Markdown"))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(link_button("about-source", "code", tr!("Source code")).on_click(open(repo.to_string())))
                            .child(link_button("about-issues", "issue", tr!("Report a bug")).on_click(open(format!("{repo}/issues"))))
                            .child(
                                link_button("about-changelog", "changelog", tr!("Changelog"))
                                    .on_click(|_, window, cx| window.dispatch_action(Box::new(ShowChangelog), cx)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(info("about-author", "user", tr!("Author"), author.to_string(), Some(format!("https://github.com/{author}"))))
                            .child(info("about-repo", "code", tr!("Repository"), repo_name.to_string(), Some(repo.to_string())))
                            .child(info("about-built", "package", tr!("Built with"), tr!("Rust and GPUI").into(), None))
                            .child(info("about-config", "folder", tr!("Settings"), config_dir, None)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .px_4()
                            .py_3()
                            .rounded_md()
                            .border_1()
                            .border_color(theme::border())
                            .bg(theme::bg_alt())
                            .child(icon("heart", 18.).text_color(rgb(BRAND)))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .gap(px(2.))
                                    .text_size(px(12.5))
                                    .child(div().text_color(theme::text()).child(tr!("Inspired by Notepad++ by Don Ho.")))
                                    .child(
                                        div()
                                            .text_color(theme::text_dim())
                                            .child(tr!("Thanks for more than 20 years of a great example.")),
                                    ),
                            )
                            .child(
                                div()
                                    .id("about-npp")
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .text_size(px(12.))
                                    .text_color(theme::accent())
                                    .cursor_pointer()
                                    .hover(|s| s.underline())
                                    .on_click(open("https://notepad-plus-plus.org/".to_string()))
                                    .child("notepad-plus-plus.org")
                                    .child(icon("external", 11.).text_color(theme::accent())),
                            ),
                    ),
            )
            .into_any_element()
    }
}

// ---------------------------------------------------------------- session

/// GPUI's window bounds with the origin corrected from the X server.
fn current_placement(window: &Window) -> gpui::WindowBounds {
    let fix = |mut b: gpui::Bounds<gpui::Pixels>| {
        if let Some((x, y)) = crate::placement::main_window_origin() {
            b.origin = gpui::point(px(x as f32), px(y as f32));
        }
        b
    };
    match window.window_bounds() {
        gpui::WindowBounds::Windowed(b) => gpui::WindowBounds::Windowed(fix(b)),
        other => other,
    }
}

fn mode_name(m: ViewMode) -> &'static str {
    match m {
        ViewMode::Text => "text",
        ViewMode::Hex => "hex",
        ViewMode::Tree => "tree",
        ViewMode::Markdown => "markdown",
    }
}

fn mode_from_name(s: &str) -> Option<ViewMode> {
    Some(match s {
        "text" => ViewMode::Text,
        "hex" => ViewMode::Hex,
        "tree" => ViewMode::Tree,
        "markdown" => ViewMode::Markdown,
        _ => return None,
    })
}

impl Workspace {
    fn session_enabled(&self, cx: &App) -> bool {
        Settings::get(cx).restore_session && session::is_owner()
    }

    fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (tabs, active) = session::load();
        let mut problems = Vec::new();
        for t in tabs {
            let buffer = match (&t.backup, &t.path) {
                (Some(bk), path) => {
                    let restored = std::fs::read(bk).and_then(|d| Buffer::read_backup(path.clone(), &d));
                    match restored {
                        Ok(b) => b,
                        Err(e) => {
                            session::reject_backup(bk);
                            let name = path
                                .as_ref()
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().into_owned())
                                .or(t.title.clone())
                                .unwrap_or_default();
                            let why = if e.kind() == std::io::ErrorKind::InvalidData {
                                tr!("the file was changed externally").to_string()
                            } else {
                                e.to_string()
                            };
                            problems.push(tr!(
                                "Unsaved changes to {name} not restored ({why}) - the backup is in ~/.local/state/rustpad/backup",
                                name = name,
                                why = why,
                            ));
                            match path.as_ref().and_then(|p| Buffer::open_with(p, t.encoding).ok()) {
                                Some(b) => b,
                                None => continue,
                            }
                        }
                    }
                }
                (None, Some(p)) => match Buffer::open_with(p, t.encoding) {
                    Ok(b) => b,
                    Err(_) => continue, // deleted or moved since
                },
                (None, None) => continue,
            };
            let mut buffer = buffer;
            if buffer.path.is_none() {
                buffer.title = t.title.clone();
            }
            // A restored backup is plain UTF-8 content; the session knows how to save it.
            if t.backup.is_some()
                && let (Some(enc), Some(bom)) = (t.encoding, t.bom)
            {
                buffer.encoding = enc;
                buffer.bom = bom && enc.has_bom();
            }
            let dirty = buffer.is_dirty();
            let version = buffer.version();
            self.new_tab_with_id(buffer, Some(t.id), window, cx);
            let tab = self.tabs.last_mut().unwrap();
            if dirty {
                // The backup on disk already holds exactly this state.
                tab.backed_version = Some(version);
            }
            let text = tab.text.clone();
            text.update(cx, |v, cx| v.restore_position(t.cursor, t.top, cx));
            if let Some(m) = mode_from_name(&t.mode) {
                self.set_mode(m, window, cx);
            }
        }
        if !self.tabs.is_empty() {
            self.active = active.min(self.tabs.len() - 1);
            self.focus_active(window, cx);
        }
        if let Some(p) = problems.first() {
            self.flash(p.clone().into(), cx);
        }
    }

    fn session_fingerprint(&self, cx: &App) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.active.hash(&mut h);
        for t in &self.tabs {
            let b = t.buffer.read(cx);
            (t.id, b.version(), b.is_dirty(), &b.path, &b.title, mode_name(t.mode)).hash(&mut h);
        }
        h.finish()
    }

    /// Collect the session; `all` forces cursor positions etc. to be current
    /// and is used on quit.
    fn session_snapshot(&mut self, _all: bool, cx: &mut Context<Self>) -> session::Session {
        let mut tabs = Vec::new();
        for t in &mut self.tabs {
            let b = t.buffer.read(cx);
            // An untouched empty "new" tab is not worth remembering.
            if b.path.is_none() && b.is_empty() {
                continue;
            }
            let dirty = b.is_dirty();
            let write_backup = (dirty && t.backed_version != Some(b.version())).then(|| b.snapshot());
            if dirty {
                t.backed_version = Some(b.version());
            } else {
                t.backed_version = None;
            }
            let tv = t.text.read(cx);
            tabs.push(session::TabState {
                id: t.id,
                path: b.path.clone(),
                title: b.title.clone(),
                mode: mode_name(t.mode),
                cursor: tv.cursor(),
                top: tv.top(),
                write_backup,
                dirty,
                encoding: b.encoding,
                bom: b.bom,
            });
        }
        let active = self.active.min(tabs.len().saturating_sub(1));
        session::Session { tabs, active }
    }

    /// Called from the ticker: write the session in the background whenever
    /// tabs or contents changed, so even a crash loses at most a few seconds.
    fn autosave(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bounds_dirty {
            self.bounds_dirty = false;
            let s = cx.global_mut::<Settings>();
            s.window = Some(current_placement(window));
            s.save();
        }
        if !self.session_enabled(cx) {
            return;
        }
        let hash = self.session_fingerprint(cx);
        if hash == self.session_hash {
            return;
        }
        self.session_hash = hash;
        let snap = self.session_snapshot(false, cx);
        cx.background_executor().spawn(async move { session::write_logged(&snap) }).detach();
    }
}

// ---------------------------------------------------------------- external changes

impl Workspace {
    /// Once a second: did any open file change on disk?
    fn check_disk(&mut self, cx: &mut Context<Self>) {
        if self.last_disk_check.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        self.last_disk_check = std::time::Instant::now();
        for i in 0..self.tabs.len() {
            let buffer = self.tabs[i].buffer.clone();
            let Some(change) = buffer.read(cx).disk_change() else { continue };
            let dirty = buffer.read(cx).is_dirty();
            let name = buffer.read(cx).file_name();
            match change {
                // Reading past the new end of a mapped file would crash: act now.
                DiskChange::Truncated if !dirty => {
                    buffer.update(cx, |b, cx| {
                        b.reload().ok();
                        cx.notify();
                    });
                    if self.tabs[i].follow {
                        self.follow_end(i, cx);
                    }
                    self.flash(tr!("{name} was truncated externally and reloaded", name = name).into(), cx);
                }
                DiskChange::Truncated => {
                    buffer.update(cx, |b, cx| {
                        b.detach_truncated();
                        cx.notify();
                    });
                    self.tabs[i].external = Some(DiskChange::Truncated);
                    cx.notify();
                }
                DiskChange::Grown if self.tabs[i].follow && !dirty => {
                    buffer.update(cx, |b, cx| {
                        b.reload_appended().ok();
                        cx.notify();
                    });
                    self.tabs[i].external = None;
                    self.follow_end(i, cx);
                }
                c if self.tabs[i].external != Some(c) => {
                    self.tabs[i].external = Some(c);
                    cx.notify();
                }
                _ => {}
            }
        }
    }

    fn follow_end(&mut self, i: usize, cx: &mut Context<Self>) {
        self.tabs[i].text.update(cx, |v, cx| v.scroll_to_end(cx));
    }

    fn reload_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        let t = &mut self.tabs[i];
        let appended = t.external == Some(DiskChange::Grown);
        t.external = None;
        let result = t.buffer.update(cx, |b, cx| {
            let r = if appended { b.reload_appended() } else { b.reload() };
            cx.notify();
            r
        });
        if let Err(e) = result {
            self.flash(tr!("Reload failed: {error}", error = e).into(), cx);
        }
        cx.notify();
    }

    fn ignore_external(&mut self, i: usize, cx: &mut Context<Self>) {
        let t = &mut self.tabs[i];
        if let Some(c) = t.external.take() {
            t.buffer.update(cx, |b, _| b.accept_disk_change(c));
        }
        cx.notify();
    }

    fn toggle_follow(&mut self, cx: &mut Context<Self>) {
        let i = self.active;
        let on = !self.tabs[i].follow;
        self.tabs[i].follow = on;
        if on {
            if self.tabs[i].external == Some(DiskChange::Grown) {
                self.reload_tab(i, cx);
            }
            self.follow_end(i, cx);
        }
        cx.notify();
    }

    fn render_external(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let i = self.active;
        let t = self.tabs.get(i)?;
        let change = t.external?;
        let b = t.buffer.read(cx);
        let name = b.file_name();
        let dirty = b.is_dirty();
        let lose = if dirty { tr!(" Reloading discards your unsaved changes.") } else { "" };
        let text = match change {
            DiskChange::Grown => tr!("“{name}” has grown outside rustpad++.{lose}", name = name, lose = lose),
            DiskChange::Replaced => tr!("“{name}” was changed outside rustpad++.{lose}", name = name, lose = lose),
            DiskChange::Modified => tr!("“{name}” was overwritten outside rustpad++. Please reload.{lose}", name = name, lose = lose),
            DiskChange::Missing => tr!("“{name}” was deleted or moved. The content stays open; saving creates the file again.", name = name),
            DiskChange::Truncated => tr!(
                "“{name}” was truncated externally. Your changes are kept; parts beyond the new end of file are missing.",
                name = name,
            ),
        };
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .px_3()
                .py(px(2.))
                .rounded_sm()
                .border_1()
                .border_color(theme::border())
                .when(primary, |d| d.bg(theme::accent()).text_color(theme::on_accent()))
                .when(!primary, |d| d.bg(theme::bg_alt()).hover(|s| s.bg(theme::bg_hover())))
                .child(label)
        };
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .bg(theme::bg_panel())
            .border_b_1()
            .border_color(theme::warn())
            .font_family("DejaVu Sans")
            .text_size(px(13.))
            .child(div().text_color(theme::warn()).child("⚠"))
            .child(div().flex_1().min_w_0().child(text));
        match change {
            DiskChange::Missing | DiskChange::Truncated => {
                row = row.child(
                    button("ext-ok", "OK", true).on_click(cx.listener(move |ws, _, _, cx| ws.ignore_external(i, cx))),
                );
            }
            _ => {
                row = row.child(
                    button("ext-reload", tr!("Reload"), true).on_click(cx.listener(move |ws, _, _, cx| ws.reload_tab(i, cx))),
                );
                if change == DiskChange::Grown && !dirty {
                    row = row.child(
                        button("ext-follow", tr!("Follow end"), false).on_click(cx.listener(|ws, _, _, cx| ws.toggle_follow(cx))),
                    );
                }
                row = row.child(
                    button("ext-ignore", tr!("Ignore"), false).on_click(cx.listener(move |ws, _, _, cx| ws.ignore_external(i, cx))),
                );
            }
        }
        Some(row.into_any_element())
    }
}
