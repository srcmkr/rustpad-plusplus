//! Shared shapes and surfaces, so menus, dropdowns, dialogs and buttons look
//! the same everywhere: every action is a pill, every container a rounded
//! rectangle, floating lists are frosted glass. Colors come from the scheme.

use std::sync::OnceLock;

use gpui::{
    App, BoxShadow, Div, ElementId, InteractiveElement, SharedString, Stateful, Styled, div, point,
    prelude::*, px, rgba,
};

use crate::theme;

/// Corner radii. Concentric: a row inside a popover is rounded by the
/// popover's radius minus its padding.
pub const R_ROW: f32 = 8.;
pub const R_POPOVER: f32 = 12.;
pub const R_DIALOG: f32 = 16.;
/// Padding inside a popover, around its rows.
pub const POPOVER_PAD: f32 = 4.;
/// Height of a pill button.
pub const PILL_H: f32 = 26.;

/// The UI font: Inter where installed, else DejaVu Sans (which is everywhere).
pub fn font() -> &'static str {
    FONT.get().copied().unwrap_or("DejaVu Sans")
}

static FONT: OnceLock<&'static str> = OnceLock::new();

/// Picks the UI font from the installed ones; call once at startup.
pub fn init(cx: &App) {
    let inter = cx.text_system().all_font_names().iter().any(|n| n == "Inter");
    FONT.set(if inter { "Inter" } else { "DejaVu Sans" }).ok();
}

/// Soft shadow under floating surfaces; it only shows over light content.
fn float_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: rgba(0x0000002e).into(),
        offset: point(px(0.), px(8.)),
        blur_radius: px(24.),
        spread_radius: px(0.),
        inset: false,
    }]
}

/// Menus, dropdowns and other floating lists.
pub fn popover() -> Div {
    div()
        .p(px(POPOVER_PAD))
        .bg(theme::glass())
        .backdrop_blur(px(16.))
        .border_1()
        .border_color(theme::glass_border())
        .rounded(px(R_POPOVER))
        .shadow(float_shadow())
}

/// Dialog card.
pub fn dialog() -> Div {
    div()
        .bg(theme::glass())
        .backdrop_blur(px(24.))
        .border_1()
        .border_color(theme::glass_border())
        .rounded(px(R_DIALOG))
        .shadow(float_shadow())
}

/// A row in a popover; the caller adds hover and content.
pub fn row() -> Div {
    div().flex().flex_row().items_center().rounded(px(R_ROW))
}

/// A thin line between groups in a popover.
pub fn separator() -> Div {
    div().my_1().mx_2().h(px(1.)).bg(theme::glass_border())
}

/// Pill-shaped button. A primary one is filled (one per view); the others
/// are translucent and lighter on hover.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, primary: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .h(px(PILL_H))
        .px_4()
        .rounded_full()
        .border_1()
        .cursor_pointer()
        .whitespace_nowrap()
        .when(primary, |d| {
            d.bg(theme::primary()).border_color(theme::primary()).text_color(theme::on_primary()).hover(|s| s.opacity(0.88))
        })
        .when(!primary, |d| {
            d.bg(theme::control()).border_color(theme::glass_border()).hover(|s| s.bg(theme::control_hover()))
        })
        .child(label.into())
}
