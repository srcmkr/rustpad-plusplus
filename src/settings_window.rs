//! Separate settings window with tab navigation. Changes apply live to all
//! windows and are written to the config file immediately.

use gpui::{
    App, AppContext, Bounds, Context, FocusHandle, Focusable, Global, IntoElement, MouseButton,
    Render, SharedString, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions, deferred,
    div, prelude::*, px, rgb, size,
};

use crate::doc::rows;
use crate::theme::{self, DEFAULT_FONT_SIZE, SCHEMES, Settings};
use crate::ui;
use crate::workspace::{CloseOverlay, CloseTab};

fn tabs() -> [&'static str; 4] {
    [tr!("General"), tr!("Editor"), tr!("Appearance"), tr!("File Types")]
}
const FONT_CANDIDATES: [&str; 7] = [
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "Ubuntu Mono",
    "JetBrains Mono",
    "Fira Code",
    "FreeMono",
];

#[derive(Clone, Copy)]
struct OpenSettings(WindowHandle<SettingsWindow>);
impl Global for OpenSettings {}

/// Open the settings window, or raise it if it is already open.
/// Open the settings window as a dialog of the focused main window.
pub fn open(cx: &mut App) {
    if let Some(handle) = cx.try_global::<OpenSettings>().map(|o| o.0)
        && handle.update(cx, |_, window, _| window.activate()).is_ok()
    {
        return;
    }
    let bounds = Bounds::centered(None, size(px(680.), px(500.)), cx);
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions { title: Some(tr!("Settings - rustpad++").into()), ..Default::default() }),
            app_id: Some("rustpad".into()),
            window_min_size: Some(size(px(520.), px(360.))),
            // A dialog of the (focused) main window: GPUI sets WM_TRANSIENT_FOR
            // and _NET_WM_WINDOW_TYPE_DIALOG before mapping, so the window
            // manager places it over the main window right away (muffin,
            // mutter and metacity center such dialogs on their parent).
            kind: gpui::WindowKind::Floating,
            ..Default::default()
        },
        |window, cx| {
            // The title given at creation only lands in the Latin-1 WM_NAME
            // property; setting it again also writes UTF-8 _NET_WM_NAME.
            window.set_window_title(tr!("Settings - rustpad++"));
            cx.new(|cx| {
                let focus = cx.focus_handle();
                window.focus(&focus, cx);
                let installed = cx.text_system().all_font_names();
                let fonts = FONT_CANDIDATES
                    .iter()
                    .filter(|f| installed.iter().any(|i| i == *f))
                    .map(|f| SharedString::from(*f))
                    .collect();
                SettingsWindow { tab: 0, fonts, focus, defaults: None, loading_defaults: false, file_type_error: None, open_dropdown: None }
            })
        },
    );
    if let Ok(handle) = handle {
        cx.set_global(OpenSettings(handle));
        crate::icon::apply();
    }
}

pub struct SettingsWindow {
    tab: usize,
    fonts: Vec<SharedString>,
    focus: FocusHandle,
    /// Current default app per file type: (desktop id, name). `None` until queried.
    defaults: Option<Vec<Option<(String, String)>>>,
    loading_defaults: bool,
    file_type_error: Option<String>,
    /// Id of the open dropdown, if any.
    open_dropdown: Option<&'static str>,
}

impl Focusable for SettingsWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Change a setting, persist it and repaint every window.
fn apply(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    f(cx.global_mut::<Settings>());
    Settings::get(cx).save();
    cx.refresh_windows();
}

fn section(title: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(div().text_size(px(13.)).font_weight(gpui::FontWeight::MEDIUM).text_color(theme::text_dim()).child(title))
}

fn row(label: &'static str, hint: Option<&'static str>, control: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_4()
        .py_1()
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                // Lets long hints wrap instead of pushing the control out.
                .min_w_0()
                .child(label)
                .children(hint.map(|h| div().text_size(px(12.)).text_color(theme::text_faint()).child(h))),
        )
        .child(div().flex_none().child(control))
}

/// On/off switch; the caller adds the click handler.
fn switch(id: impl Into<gpui::ElementId>, on: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .w(px(40.))
        .h(px(22.))
        .rounded_full()
        .p(px(3.))
        .flex()
        .cursor_pointer()
        .when(on, |d| d.bg(theme::accent()).justify_end())
        .when(!on, |d| d.bg(theme::control_active()).justify_start())
        .child(div().size(px(16.)).rounded_full().bg(rgb(0xffffff)))
}

fn button(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    ui::button(id, label, false)
}

impl SettingsWindow {
    fn toggle(&self, id: &'static str, on: bool, cx: &mut Context<Self>, f: fn(&mut Settings)) -> impl IntoElement {
        switch(id, on).on_click(cx.listener(move |_, _, _, cx| apply(cx, f)))
    }

    fn load_defaults(&mut self, cx: &mut Context<Self>) {
        self.loading_defaults = true;
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async {
                    crate::file_types::query_defaults()
                        .into_iter()
                        .map(|id| {
                            id.map(|id| {
                                let name = crate::file_types::app_name(&id)
                                    .unwrap_or_else(|| id.trim_end_matches(".desktop").to_string());
                                (id, name)
                            })
                        })
                        .collect()
                })
                .await;
            this.update(cx, |v, cx| {
                v.defaults = Some(found);
                v.loading_defaults = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn file_types(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::file_types::{DESKTOP_ID, TYPES};
        if self.defaults.is_none() && !self.loading_defaults {
            self.load_defaults(cx);
        }
        let mut list = section(tr!("Default application")).child(
            div()
                .text_size(px(12.))
                .text_color(theme::text_faint())
                .child(tr!("Files of these types open in rustpad++ when you double-click them in the file manager.")),
        );
        if let Some(e) = &self.file_type_error {
            list = list.child(div().text_color(theme::error()).child(e.clone()));
        }
        for (i, t) in TYPES.iter().enumerate() {
            let current = self.defaults.as_ref().map(|d| d[i].clone());
            let on = matches!(&current, Some(Some((id, _))) if id == DESKTOP_ID);
            let hint = match &current {
                None => tr!("{extensions} · checking…", extensions = t.extensions),
                Some(Some((_, name))) => tr!("{extensions} · currently: {app}", extensions = t.extensions, app = name),
                Some(None) => tr!("{extensions} · no default application", extensions = t.extensions),
            };
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .py_1()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .child(crate::i18n::tr(t.label))
                            .child(div().text_size(px(12.)).text_color(theme::text_faint()).child(hint)),
                    )
                    .child(switch(("file-type", i), on).on_click(cx.listener(move |v, _, _, cx| {
                        v.file_type_error = crate::file_types::set_default(&TYPES[i], !on)
                            .err()
                            .map(|e| tr!("Could not change the default application: {error}", error = e));
                        v.load_defaults(cx);
                        cx.notify();
                    }))),
            );
        }
        div().flex().flex_col().gap_6().child(list)
    }

    fn segmented<T: Clone + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: Vec<(SharedString, T)>,
        current: T,
        cx: &mut Context<Self>,
        set: fn(&mut Window, &mut App, T),
    ) -> impl IntoElement {
        let mut seg = div()
            .flex()
            .flex_row()
            .p(px(2.))
            .gap(px(2.))
            .rounded_full()
            .bg(theme::control())
            .border_1()
            .border_color(theme::glass_border());
        for (i, (label, value)) in options.into_iter().enumerate() {
            let active = value == current;
            let value = value.clone();
            seg = seg.child(
                div()
                    .id((id, i))
                    .h(px(ui::PILL_H - 4.))
                    .flex()
                    .items_center()
                    .px_3()
                    .rounded_full()
                    .cursor_pointer()
                    .when(active, |d| d.bg(theme::control_active()).text_color(theme::text()))
                    .when(!active, |d| d.text_color(theme::text_dim()).hover(|s| s.bg(theme::control_hover())))
                    .on_click(cx.listener(move |_, _, window, cx| set(window, cx, value.clone())))
                    .child(label),
            );
        }
        seg
    }

    /// A button showing the current choice that opens a list of all options.
    fn dropdown<T: Clone + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: Vec<(SharedString, T)>,
        current: T,
        cx: &mut Context<Self>,
        set: fn(&mut Window, &mut App, T),
    ) -> impl IntoElement {
        let open = self.open_dropdown == Some(id);
        let label = options.iter().find(|(_, v)| *v == current).map(|(l, _)| l.clone()).unwrap_or_default();
        let trigger = div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .min_w(px(220.))
            .h(px(ui::PILL_H))
            .px_4()
            .rounded_full()
            .cursor_pointer()
            .border_1()
            .border_color(if open { theme::accent() } else { theme::glass_border() })
            .bg(theme::control())
            .hover(|s| s.bg(theme::control_hover()))
            .on_click(cx.listener(move |v, _, _, cx| {
                v.open_dropdown = if v.open_dropdown == Some(id) { None } else { Some(id) };
                cx.notify();
            }))
            .child(label)
            .child(div().text_color(theme::text_dim()).child("▾"));
        let mut list = ui::popover()
            .id((id, usize::MAX))
            .occlude()
            .absolute()
            .top(px(ui::PILL_H + 4.))
            .left_0()
            .min_w(px(220.))
            .flex()
            .flex_col();
        if open {
            for (i, (label, value)) in options.into_iter().enumerate() {
                let active = value == current;
                list = list.child(
                    ui::row()
                        .id((id, i))
                        .h(px(28.))
                        .px_2()
                        .hover(|s| s.bg(theme::control_hover()))
                        .on_click(cx.listener(move |v, _, window, cx| {
                            v.open_dropdown = None;
                            set(window, cx, value.clone());
                            cx.notify();
                        }))
                        .child(div().w(px(18.)).text_color(theme::accent()).child(if active { "✓" } else { "" }))
                        .child(label),
                );
            }
        }
        // Above the backdrop that the window adds while a dropdown is open.
        div().relative().child(trigger).when(open, |d| d.child(deferred(list).priority(2)))
    }

    fn general(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Settings::get(cx);
        let recent = s.recent.len();
        let (single, restore) = (s.single_instance, s.restore_session);
        let language = s.language.clone();
        let languages = crate::i18n::choices()
            .into_iter()
            .map(|(code, name)| (SharedString::from(name), code.to_string()))
            .collect();
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                section(tr!("Language")).child(row(
                    tr!("User interface"),
                    Some(tr!("Applies immediately to all windows")),
                    self.dropdown("language", languages, language, cx, |window, cx, code| {
                        crate::i18n::set_language(&code);
                        window.set_window_title(tr!("Settings - rustpad++"));
                        apply(cx, |s| s.language = code);
                    }),
                )),
            )
            .child(
                section(tr!("Startup"))
                    .child(row(
                        tr!("Single window"),
                        Some(tr!("Further launches open files as tabs in the running window (fastest)")),
                        self.toggle("single", single, cx, |s| s.single_instance = !s.single_instance),
                    ))
                    .child(row(
                        tr!("Restore session"),
                        Some(tr!("Keep all tabs including unsaved changes on quit, without asking")),
                        self.toggle("session", restore, cx, |s| s.restore_session = !s.restore_session),
                    )),
            )
            .child(
                section(tr!("History")).child(row(
                    tr!("Recent files"),
                    Some(if recent == 0 { tr!("No entries") } else { tr!("Up to 5 files in the File menu") }),
                    button("clear-recent", tr!("Clear ({count})", count = recent)).on_click(cx.listener(|_, _, _, cx| {
                        apply(cx, |s| s.recent.clear());
                    })),
                )),
            )
            .child(self.updates(cx))
    }

    /// Whether to check for new versions, where things stand, and installing.
    fn updates(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::update::{self, Action};
        let check = Settings::get(cx).check_updates;
        let (text, dot) = update::status(cx);
        let busy = cx.try_global::<update::Updates>().is_some_and(|u| u.busy);
        let mut buttons = div().flex().flex_row().flex_wrap().gap_2();
        if let Some(release) = update::newer(cx) {
            buttons = buttons.child(if update::can_update() {
                button("settings-update", tr!("Install"))
                    .when(busy, |d| d.opacity(0.5))
                    .on_click(|_, _, cx| update::install(Action::Update, cx))
            } else {
                button("settings-download", tr!("Download")).on_click(move |_, _, cx| cx.open_url(&release.page))
            });
        }
        if update::is_build() {
            buttons = buttons.child(
                button("settings-install-app", tr!("Install for all users"))
                    .when(busy, |d| d.opacity(0.5))
                    .on_click(|_, _, cx| update::install(Action::InstallApp, cx)),
            );
        }
        section(tr!("Updates"))
            .child(row(
                tr!("Check for updates"),
                Some(tr!(
                    "Asks GitHub for a newer version 30 seconds after the start and every 6 hours. GitHub sees your IP address, nothing else."
                )),
                switch("check-updates", check).on_click(cx.listener(|_, _, _, cx| {
                    apply(cx, |s| s.check_updates = !s.check_updates);
                    if Settings::get(cx).check_updates {
                        update::check(cx);
                    }
                })),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_3()
                    .py_1()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(div().flex_none().size(px(10.)).rounded_full().bg(dot))
                            .child(tr!("Version {version}", version = update::VERSION))
                            .child(div().text_color(theme::text_faint()).child("·"))
                            .child(text),
                    )
                    .child(div().flex_1())
                    .child(buttons),
            )
            .when(update::is_build(), |d| {
                d.child(div().text_size(px(12.)).text_color(theme::text_faint()).child(tr!(
                    "This rustpad++ runs from its build folder. Installed for all users, it gets a menu entry and can update itself."
                )))
            })
            .children(update::note(cx).map(|n| div().text_size(px(12.)).text_color(theme::warn()).child(n)))
    }

    fn editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Settings::get(cx);
        let size = f32::from(s.font_size);
        let family = s.mono_family.clone();
        let (line_numbers, word_wrap) = (s.line_numbers, s.word_wrap);
        let fonts: Vec<(SharedString, SharedString)> = self.fonts.iter().map(|f| (f.clone(), f.clone())).collect();
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                section(tr!("Font"))
                    .child(row(
                        tr!("Font size"),
                        Some(tr!("Also with Ctrl + / Ctrl − / Ctrl 0")),
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(button("smaller", "−").on_click(cx.listener(|_, _, _, cx| {
                                apply(cx, |s| s.font_size = (s.font_size - px(1.)).max(px(7.)))
                            })))
                            .child(div().w(px(48.)).flex().justify_center().child(format!("{size:.0} px")))
                            .child(button("larger", "+").on_click(cx.listener(|_, _, _, cx| {
                                apply(cx, |s| s.font_size = (s.font_size + px(1.)).min(px(48.)))
                            })))
                            .child(button("reset-size", tr!("Default")).on_click(cx.listener(|_, _, _, cx| {
                                apply(cx, |s| s.font_size = px(DEFAULT_FONT_SIZE))
                            }))),
                    ))
                    .child(row(
                        tr!("Font family"),
                        None,
                        self.dropdown("font", fonts, family, cx, |_, cx, f| apply(cx, |s| s.mono_family = f)),
                    )),
            )
            .child(
                section(tr!("Editor display"))
                    .child(row(
                        tr!("Tab width"),
                        None,
                        self.segmented(
                            "tab",
                            vec![("2".into(), 2usize), ("4".into(), 4), ("8".into(), 8)],
                            rows::tab_width(),
                            cx,
                            |_, cx, w| {
                                rows::set_tab_width(w);
                                apply(cx, |_| {});
                            },
                        ),
                    ))
                    .child(row(
                        tr!("Line numbers"),
                        None,
                        self.toggle("lines", line_numbers, cx, |s| s.line_numbers = !s.line_numbers),
                    ))
                    .child(row(
                        tr!("Word wrap"),
                        Some(tr!("Wrap long lines at the window width instead of scrolling sideways")),
                        self.toggle("wrap", word_wrap, cx, |s| s.word_wrap = !s.word_wrap),
                    )),
            )
    }

    fn appearance(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Settings::get(cx);
        let status = s.status_bar;
        let current = theme::scheme_index();
        let mut cards = div().flex().flex_row().flex_wrap().gap_3();
        for (i, sch) in SCHEMES.iter().enumerate() {
            let active = i == current;
            cards = cards.child(
                div()
                    .id(("scheme", i))
                    .w(px(114.))
                    .cursor_pointer()
                    .rounded(px(ui::R_POPOVER))
                    .border_2()
                    .border_color(if active { theme::accent() } else { theme::border() })
                    .overflow_hidden()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        theme::set_scheme(i);
                        apply(cx, |_| {});
                    }))
                    .child(
                        div()
                            .h(px(56.))
                            // Inside the 2 px border.
                            .rounded_t(px(ui::R_POPOVER - 2.))
                            .bg(rgb(sch_bg(sch)))
                            .p_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().h(px(6.)).w(px(70.)).rounded_full().bg(rgb(sch_color(sch, 0))))
                            .child(div().h(px(6.)).w(px(88.)).rounded_full().bg(rgb(sch_color(sch, 1))))
                            .child(div().h(px(6.)).w(px(50.)).rounded_full().bg(rgb(sch_color(sch, 2)))),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_b(px(ui::R_POPOVER - 2.))
                            .bg(theme::bg_alt())
                            .flex()
                            .flex_row()
                            .justify_between()
                            .whitespace_nowrap()
                            .child(crate::i18n::tr(sch.name))
                            .child(if active { "✓" } else { "" }),
                    ),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(section(tr!("Color scheme")).child(cards))
            .child(section(tr!("Window")).child(row(
                tr!("Show status bar"),
                None,
                self.toggle("status", status, cx, |s| s.status_bar = !s.status_bar),
            )))
    }
}

// Preview colors for the scheme cards (read straight from the palette).
fn sch_bg(s: &theme::Scheme) -> u32 {
    s.preview()[0]
}
fn sch_color(s: &theme::Scheme, i: usize) -> u32 {
    s.preview()[i + 1]
}

impl Render for SettingsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut nav = div()
            .flex()
            .flex_row()
            .gap_1()
            .px_4()
            .pt_3()
            .border_b_1()
            .border_color(theme::border())
            .bg(theme::bg_alt());
        for (i, name) in tabs().iter().enumerate() {
            let active = i == self.tab;
            nav = nav.child(
                div()
                    .id(("settings-tab", i))
                    .px_4()
                    .py_2()
                    .border_b_2()
                    .when(active, |d| d.border_color(theme::accent()).text_color(theme::text()))
                    .when(!active, |d| d.border_color(gpui::transparent_black()).text_color(theme::text_dim()).hover(|s| s.text_color(theme::text())))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                        this.tab = i;
                        cx.notify();
                    }))
                    .child(*name),
            );
        }
        let body = match self.tab {
            0 => self.general(cx).into_any_element(),
            1 => self.editor(cx).into_any_element(),
            2 => self.appearance(cx).into_any_element(),
            _ => self.file_types(cx).into_any_element(),
        };
        div()
            .id("settings")
            .track_focus(&self.focus)
            .key_context("Settings")
            .on_action(cx.listener(|_, _: &CloseTab, window, _| window.remove_window()))
            .on_action(cx.listener(|v, _: &CloseOverlay, _, cx| {
                v.open_dropdown = None;
                cx.notify();
            }))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .text_size(px(13.))
            .font_family(crate::ui::font())
            .child(nav)
            .child(div().id("settings-body").flex_1().overflow_y_scroll().p_6().child(body))
            // Clicks anywhere outside an open dropdown close it.
            .when(self.open_dropdown.is_some(), |d| {
                d.child(
                    deferred(
                        div()
                            .id("dropdown-backdrop")
                            .occlude()
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .on_mouse_down(MouseButton::Left, cx.listener(|v, _, _, cx| {
                                v.open_dropdown = None;
                                cx.notify();
                            })),
                    )
                    .priority(1),
                )
            })
    }
}
