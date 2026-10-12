//! In-window menu bar (GPUI has no native menu bar on Linux).
//!
//! Menus are rebuilt on every render from the current state, so checkmarks
//! and the recent-files list are always up to date. Dropdowns are deferred
//! so they paint above the editor, and occlude it for mouse input.

use std::rc::Rc;

use gpui::{
    Action, AnyElement, Context, IntoElement, MouseButton, SharedString, Window, deferred, div,
    prelude::*, px, relative,
};

use super::{BarKind, Dialog, Workspace};
use crate::theme::{self, SCHEMES, Settings};
use crate::views::ViewMode;
use crate::views::text as editor;

pub const BAR_H: f32 = 26.0;

type Handler = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

pub enum Item {
    Action {
        label: SharedString,
        shortcut: Option<&'static str>,
        checked: Option<bool>,
        enabled: bool,
        run: Handler,
    },
    Sub {
        label: SharedString,
        items: Vec<Item>,
    },
    Sep,
}

pub struct Menu {
    pub label: &'static str,
    pub items: Vec<Item>,
}

fn action(
    label: impl Into<SharedString>,
    shortcut: Option<&'static str>,
    run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> Item {
    Item::Action { label: label.into(), shortcut, checked: None, enabled: true, run: Rc::new(run) }
}

fn check(
    label: impl Into<SharedString>,
    shortcut: Option<&'static str>,
    checked: bool,
    run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> Item {
    Item::Action { label: label.into(), shortcut, checked: Some(checked), enabled: true, run: Rc::new(run) }
}

/// "Ctrl+Shift+L" with the key names of the UI language ("Strg+Umschalt+L").
fn shortcut_label(keys: &str) -> String {
    keys.split('+')
        .map(|k| match k {
            "Ctrl" => tr!("Ctrl"),
            "Shift" => tr!("Shift"),
            "Del" => tr!("Del"),
            k => k,
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Send an editor action to whatever has the keyboard focus (editor or bar input).
fn dispatch(label: &'static str, shortcut: &'static str, enabled: bool, a: impl Action) -> Item {
    let a: Box<dyn Action> = Box::new(a);
    Item::Action {
        label: label.into(),
        shortcut: Some(shortcut),
        checked: None,
        enabled,
        run: Rc::new(move |_, window, cx| window.dispatch_action(a.boxed_clone(), cx)),
    }
}

fn disabled(label: impl Into<SharedString>) -> Item {
    Item::Action { label: label.into(), shortcut: None, checked: None, enabled: false, run: Rc::new(|_, _, _| {}) }
}

impl Workspace {
    fn menus(&self, cx: &Context<Self>) -> Vec<Menu> {
        let settings = Settings::get(cx);

        let mut recent: Vec<Item> = settings
            .recent
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let path = p.clone();
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let dir = p.parent().map(short_dir).unwrap_or_default();
                action(format!("{}  {name}  -  {dir}", i + 1), None, move |ws, w, cx| ws.open_path(&path, w, cx))
            })
            .collect();
        if recent.is_empty() {
            recent.push(disabled(tr!("No Entries")));
        }
        recent.push(Item::Sep);
        recent.push(Item::Action {
            label: tr!("Clear List").into(),
            shortcut: None,
            checked: None,
            enabled: !settings.recent.is_empty(),
            run: Rc::new(|ws, _, cx| ws.clear_recent(cx)),
        });

        let mode = self.tab().mode;
        let view_item = |m: ViewMode, label: &'static str, key: &'static str| {
            check(label, Some(key), mode == m, move |ws, w, cx| ws.set_mode(m, w, cx))
        };

        let text_mode = mode == ViewMode::Text;
        let buffer = self.tab().buffer.read(cx);
        let has_sel = text_mode && !self.tab().text.read(cx).selection().is_empty();
        let (can_undo, can_redo) = (text_mode && buffer.can_undo(), text_mode && buffer.can_redo());

        let current = theme::scheme_index();
        let schemes = SCHEMES
            .iter()
            .enumerate()
            .map(|(i, s)| check(crate::i18n::tr(s.name), None, i == current, move |ws, _, cx| ws.set_scheme(i, cx)))
            .collect();

        vec![
            Menu {
                label: tr!("File"),
                items: vec![
                    action(tr!("New File"), Some("Ctrl+N"), |ws, w, cx| ws.new_file(&super::NewFile, w, cx)),
                    action(tr!("Open File…"), Some("Ctrl+O"), |ws, w, cx| ws.open(&super::Open, w, cx)),
                    Item::Sub { label: tr!("Recent Files").into(), items: recent },
                    Item::Sep,
                    action(tr!("Save"), Some("Ctrl+S"), |ws, w, cx| ws.save(&super::Save, w, cx)),
                    action(tr!("Save As…"), Some("Ctrl+Alt+S"), |ws, w, cx| ws.save_as(&super::SaveAs, w, cx)),
                    action(tr!("Save All"), Some("Ctrl+Shift+S"), |ws, w, cx| ws.save_all(&super::SaveAll, w, cx)),
                    Item::Sep,
                    action(tr!("Quit"), Some("Ctrl+Q"), |ws, w, cx| ws.quit(&super::Quit, w, cx)),
                ],
            },
            Menu {
                label: tr!("Edit"),
                items: vec![
                    dispatch(tr!("Undo"), "Ctrl+Z", can_undo, editor::Undo),
                    dispatch(tr!("Redo"), "Ctrl+Y", can_redo, editor::Redo),
                    Item::Sep,
                    dispatch(tr!("Cut"), "Ctrl+X", has_sel, editor::Cut),
                    dispatch(tr!("Copy"), "Ctrl+C", has_sel || mode == ViewMode::Hex, editor::Copy),
                    dispatch(tr!("Paste"), "Ctrl+V", text_mode, editor::Paste),
                    dispatch(tr!("Delete"), "Del", text_mode, editor::Delete),
                    dispatch(tr!("Select All"), "Ctrl+A", text_mode || mode == ViewMode::Hex, editor::SelectAll),
                    Item::Sep,
                    dispatch(tr!("Duplicate Line"), "Ctrl+D", text_mode, editor::DuplicateLine),
                    dispatch(tr!("Delete Line"), "Ctrl+Shift+L", text_mode, editor::DeleteLine),
                    Item::Sep,
                    action(tr!("Find…"), Some("Ctrl+F"), |ws, w, cx| ws.open_bar(BarKind::Find, w, cx)),
                    action(tr!("Find Next"), Some("F3"), |ws, w, cx| ws.find(true, w, cx)),
                    action(tr!("Find Previous"), Some("Shift+F3"), |ws, w, cx| ws.find(false, w, cx)),
                    action(tr!("Find and Replace…"), Some("Ctrl+H"), |ws, w, cx| ws.open_bar(BarKind::Replace, w, cx)),
                    Item::Sep,
                    action(tr!("Go To…"), Some("Ctrl+G"), |ws, w, cx| ws.open_bar(BarKind::Goto, w, cx)),
                ],
            },
            Menu {
                label: tr!("View"),
                items: vec![
                    view_item(ViewMode::Text, tr!("Text"), "Ctrl+1"),
                    view_item(ViewMode::Hex, tr!("Hex"), "Ctrl+2"),
                    view_item(ViewMode::Tree, tr!("JSON Tree"), "Ctrl+3"),
                    view_item(ViewMode::Markdown, tr!("Markdown"), "Ctrl+4"),
                    Item::Sep,
                    check(tr!("Word Wrap"), None, settings.word_wrap, |ws, _, cx| ws.toggle_word_wrap(cx)),
                    check(tr!("Follow End of File (tail -f)"), None, self.tab().follow, |ws, _, cx| ws.toggle_follow(cx)),
                ],
            },
            Menu {
                label: tr!("Settings"),
                items: vec![
                    Item::Sub { label: tr!("Color Scheme").into(), items: schemes },
                    check(tr!("Status Bar"), None, settings.status_bar, |ws, _, cx| ws.toggle_status_bar(cx)),
                    Item::Sep,
                    action(tr!("All Settings…"), Some("Ctrl+,"), |_, _, cx| crate::settings_window::open(cx)),
                ],
            },
            Menu {
                label: tr!("Help"),
                items: vec![
                    action(tr!("Check for Updates…"), None, |ws, _, cx| {
                        ws.show_dialog(Dialog::Updates, cx);
                        crate::update::check(cx);
                    }),
                    action(tr!("About rustpad++"), None, |ws, _, cx| ws.show_dialog(Dialog::About, cx)),
                ],
            },
        ]
    }

    pub(super) fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.is_some() || self.encoding_menu {
            self.menu = None;
            self.submenu = None;
            self.encoding_menu = false;
            cx.notify();
        }
    }

    pub(super) fn render_menubar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let menus = self.menus(cx);
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(BAR_H))
            .px_1()
            .bg(theme::bg_alt())
            .text_size(px(13.))
            .text_color(theme::text());
        for (i, menu) in menus.into_iter().enumerate() {
            let open = self.menu == Some(i);
            let mut label = div()
                .id(("menu", i))
                .relative()
                .h_full()
                .px_3()
                .flex()
                .items_center()
                .rounded_sm()
                .when(open, |d| d.bg(theme::bg_active()))
                .when(!open, |d| d.hover(|s| s.bg(theme::bg_hover())))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _, _, cx| {
                        cx.stop_propagation();
                        // A click closes the menu only if it was not just opened by hovering.
                        ws.menu = if ws.menu == Some(i) && !ws.menu_by_hover { None } else { Some(i) };
                        ws.menu_by_hover = false;
                        ws.submenu = None;
                        cx.notify();
                    }),
                )
                .on_hover(cx.listener(move |ws, hovered: &bool, _, cx| {
                    if *hovered && ws.menu.is_some() && ws.menu != Some(i) {
                        ws.menu = Some(i);
                        ws.menu_by_hover = true;
                        ws.submenu = None;
                        cx.notify();
                    }
                }))
                .child(menu.label);
            if open {
                label = label.child(
                    deferred(
                        div()
                            .absolute()
                            .top(px(BAR_H))
                            .left_0()
                            .child(self.render_dropdown(menu.items, false, cx)),
                    )
                    .with_priority(1),
                );
            }
            bar = bar.child(label);
        }
        bar
    }

    fn render_dropdown(&self, items: Vec<Item>, nested: bool, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .id("dropdown")
            .occlude()
            .flex()
            .flex_col()
            .min_w(px(260.))
            .py_1()
            .bg(theme::bg_panel())
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .shadow_lg()
            .text_size(px(13.))
            .text_color(theme::text());
        for (idx, item) in items.into_iter().enumerate() {
            list = list.child(match item {
                Item::Sep => div().my_1().h(px(1.)).bg(theme::border()).into_any_element(),
                Item::Action { label, shortcut, checked, enabled, run } => {
                    let row = Self::row(idx, checked, label, shortcut.map(|s| shortcut_label(s).into()), enabled)
                        .on_hover(cx.listener(move |ws, hovered: &bool, _, cx| {
                            // Hovering a plain item of the top-level menu closes an open submenu.
                            if *hovered && !nested && ws.submenu.is_some() {
                                ws.submenu = None;
                                cx.notify();
                            }
                        }));
                    if enabled {
                        row.on_click(cx.listener(move |ws, _, window, cx| {
                            ws.close_menu(cx);
                            run(ws, window, cx);
                        }))
                        .into_any_element()
                    } else {
                        row.into_any_element()
                    }
                }
                Item::Sub { label, items } => {
                    let open = self.submenu.as_ref() == Some(&label);
                    let key = label.clone();
                    let mut row = Self::row(idx, None, label, Some("▸".into()), true)
                        .relative()
                        .when(open, |d| d.bg(theme::bg_hover()))
                        .on_hover(cx.listener(move |ws, hovered: &bool, _, cx| {
                            if *hovered && ws.submenu.as_ref() != Some(&key) {
                                ws.submenu = Some(key.clone());
                                cx.notify();
                            }
                        }));
                    if open {
                        row = row.child(
                            div().absolute().top(px(-5.)).left(relative(1.)).child(self.render_dropdown(items, true, cx)),
                        );
                    }
                    row.into_any_element()
                }
            });
        }
        list.into_any_element()
    }

    fn row(
        idx: usize,
        checked: Option<bool>,
        label: SharedString,
        trailing: Option<SharedString>,
        enabled: bool,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(("item", idx))
            .flex()
            .flex_row()
            .items_center()
            .h(px(26.))
            .mx_1()
            .pl_1()
            .pr_3()
            .rounded_sm()
            .when(enabled, |d| d.hover(|s| s.bg(theme::bg_hover())))
            .when(!enabled, |d| d.text_color(theme::text_faint()))
            .child(
                div()
                    .w(px(20.))
                    .flex()
                    .justify_center()
                    .text_color(theme::accent())
                    .child(if checked == Some(true) { "✓" } else { "" }),
            )
            .child(div().flex_1().pr_6().whitespace_nowrap().child(label))
            .children(trailing.map(|t| div().text_color(theme::text_dim()).text_size(px(12.)).child(t)))
    }
}

/// `~` for the home directory and an ellipsis in the middle of long paths.
fn short_dir(dir: &std::path::Path) -> String {
    let mut s = dir.display().to_string();
    if let Some(home) = std::env::var_os("HOME").map(|h| h.to_string_lossy().into_owned())
        && let Some(rest) = s.strip_prefix(&home)
    {
        s = format!("~{rest}");
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > 48 {
        let head: String = chars[..20].iter().collect();
        let tail: String = chars[chars.len() - 25..].iter().collect();
        s = format!("{head}…{tail}");
    }
    s
}
