//! Themed replacement for GPUI's fallback prompt (the one Linux uses for
//! `window.prompt`): it follows the color scheme and wraps long text.
//!
//! The first action is the default (Enter); the last one is taken as cancel
//! (Escape), which is how every prompt in rustpad++ orders its buttons.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent, PromptHandle,
    PromptLevel, PromptResponse, Render, Window, div, prelude::*, px,
};

use crate::theme;

pub fn install(cx: &mut App) {
    cx.set_prompt_builder(|level, message, detail, actions, handle: PromptHandle, window, cx| {
        let view = cx.new(|cx| ThemedPrompt {
            level,
            message: message.to_string(),
            detail: detail.map(str::to_string),
            actions: actions.iter().map(|a| a.label().to_string()).collect(),
            focus: cx.focus_handle(),
        });
        handle.with_view(view, window, cx)
    });
}

struct ThemedPrompt {
    level: PromptLevel,
    message: String,
    detail: Option<String>,
    actions: Vec<String>,
    focus: FocusHandle,
}

impl EventEmitter<PromptResponse> for ThemedPrompt {}

impl Focusable for ThemedPrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ThemedPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let last = self.actions.len().saturating_sub(1);
        let (symbol, color) = match self.level {
            PromptLevel::Info => ("ℹ", theme::accent()),
            PromptLevel::Warning => ("⚠", theme::warn()),
            PromptLevel::Critical => ("⚠", theme::error()),
        };
        // Cancel on the left, the default action on the right.
        let buttons = self.actions.iter().enumerate().rev().map(|(i, label)| {
            let primary = i == 0;
            div()
                .id(("prompt-action", i))
                .px_4()
                .py_1()
                .rounded_md()
                .border_1()
                .cursor_pointer()
                .when(primary, |d| d.bg(theme::accent()).border_color(theme::accent()).text_color(theme::on_accent()))
                .when(!primary, |d| d.bg(theme::bg_alt()).border_color(theme::border()).hover(|s| s.bg(theme::bg_hover())))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(PromptResponse(i))))
                .child(label.clone())
        });
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::shadow())
            .font_family("DejaVu Sans")
            .text_size(px(13.))
            .text_color(theme::text())
            .child(
                div()
                    .id("prompt")
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(move |_, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                        "enter" => cx.emit(PromptResponse(0)),
                        "escape" => cx.emit(PromptResponse(last)),
                        _ => {}
                    }))
                    .w(px(420.))
                    .max_w(gpui::relative(0.9))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_5()
                    .bg(theme::bg_panel())
                    .border_1()
                    .border_color(theme::border())
                    .rounded_lg()
                    .shadow_lg()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_3()
                            .child(div().flex_none().text_size(px(20.)).text_color(color).child(symbol))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(div().text_size(px(15.)).font_weight(gpui::FontWeight::SEMIBOLD).child(self.message.clone()))
                                    .children(self.detail.clone().map(|d| div().text_color(theme::text_dim()).child(d))),
                            ),
                    )
                    .child(div().flex().flex_row().justify_end().gap_2().children(buttons)),
            )
    }
}
