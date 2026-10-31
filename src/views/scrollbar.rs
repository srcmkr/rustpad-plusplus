//! Scrollbars for the element-based views (table, tree, Markdown).
//!
//! gpui draws no scrollbars of its own. A [`Bar`] is the geometry of one
//! scrollbar computed at render time from the view and content lengths and
//! the scroll distance; the view keeps it for its mouse handlers. Pressing the
//! thumb starts a [`Drag`], pressing beside it scrolls one page that way.
//!
//! The geometry comes from the list's last layout, which happens after the
//! render that reads it. So each view checks on the next frame whether its
//! bars still match and renders again if not, see [`recheck`].

use gpui::{Div, Pixels, Point, UniformListScrollHandle, div, point, prelude::*, px};

use crate::theme;

/// Thickness of a scrollbar, in pixels.
pub const THICKNESS: f32 = 12.0;
const MIN_THUMB: f32 = 24.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

impl Axis {
    fn of(self, p: Point<Pixels>) -> Pixels {
        match self {
            Axis::Horizontal => p.x,
            Axis::Vertical => p.y,
        }
    }
}

/// One scrollbar. Lengths are along its axis; `origin` is the track start in
/// window coordinates, `thumb_at` is relative to it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    pub axis: Axis,
    origin: Pixels,
    track: Pixels,
    thumb_at: Pixels,
    thumb_len: Pixels,
    /// Largest scroll distance.
    pub max: Pixels,
}

/// A thumb drag: mouse position and scroll distance when it started.
#[derive(Clone, Copy, Debug)]
pub struct Drag {
    pub axis: Axis,
    mouse: Pixels,
    offset: Pixels,
}

impl Bar {
    /// The bar for a viewport of length `view` (also the track length) over
    /// `content`, scrolled by `offset` (zero or positive). `None` when
    /// everything fits.
    pub fn new(axis: Axis, origin: Pixels, view: Pixels, content: Pixels, offset: Pixels) -> Option<Bar> {
        let max = content - view;
        if max <= px(0.5) || view <= px(0.) {
            return None;
        }
        let thumb_len = (view * (view / content)).max(px(MIN_THUMB)).min(view);
        let thumb_at = (view - thumb_len) * (offset.clamp(px(0.), max) / max);
        Some(Bar { axis, origin, track: view, thumb_at, thumb_len, max })
    }

    /// A press at `p`: on the thumb it starts a drag, beside it the result is
    /// the scroll distance one page further that way.
    pub fn press(&self, p: Point<Pixels>, offset: Pixels) -> Result<Drag, Pixels> {
        let at = self.axis.of(p) - self.origin;
        if at >= self.thumb_at && at <= self.thumb_at + self.thumb_len {
            Ok(Drag { axis: self.axis, mouse: self.axis.of(p), offset })
        } else {
            let page = if at < self.thumb_at { -self.track } else { self.track };
            Err((offset + page).clamp(px(0.), self.max))
        }
    }

    /// Scroll distance for the mouse at `p` during `drag`.
    pub fn dragged(&self, drag: &Drag, p: Point<Pixels>) -> Pixels {
        let free = (self.track - self.thumb_len).max(px(1.));
        (drag.offset + self.max * ((self.axis.of(p) - drag.mouse) / free)).clamp(px(0.), self.max)
    }

    /// The track with its thumb. The caller adds an id and the mouse-down listener.
    pub fn element(&self, active: bool) -> Div {
        let color = if active { theme::scrollbar_thumb_active() } else { theme::scrollbar_thumb() };
        let thumb = div().absolute().rounded(px(3.)).bg(color);
        let thumb = match self.axis {
            Axis::Horizontal => thumb.top(px(2.)).bottom(px(2.)).left(self.thumb_at).w(self.thumb_len),
            Axis::Vertical => thumb.left(px(2.)).right(px(2.)).top(self.thumb_at).h(self.thumb_len),
        };
        let track = div().flex_none().relative().bg(theme::bg_panel()).border_color(theme::border());
        let track = match self.axis {
            Axis::Horizontal => track.h(px(THICKNESS)).border_t_1(),
            Axis::Vertical => track.w(px(THICKNESS)).h_full().border_l_1(),
        };
        track.child(thumb)
    }
}

/// Renders the view again on the next frame if `current` (the bars as the
/// latest layout gives them) no longer matches `shown` (what this render drew).
pub fn recheck<V: 'static, T: PartialEq + 'static>(
    shown: T,
    current: fn(&V) -> T,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<V>,
) {
    cx.on_next_frame(window, move |v, _, cx| {
        if current(v) != shown {
            cx.notify();
        }
    });
}

/// Scroll distance of a uniform list along `axis`, zero or positive.
pub fn uniform_offset(handle: &UniformListScrollHandle, axis: Axis) -> Pixels {
    -axis.of(handle.0.borrow().base_handle.offset())
}

/// Scrolls a uniform list along `axis` to `distance` (zero or positive).
pub fn set_uniform_offset(handle: &UniformListScrollHandle, axis: Axis, distance: Pixels) {
    let base = handle.0.borrow().base_handle.clone();
    let mut o = base.offset();
    match axis {
        Axis::Horizontal => o.x = -distance,
        Axis::Vertical => o.y = -distance,
    }
    base.set_offset(o);
}

/// The vertical bar of a uniform list, from its last layout.
pub fn uniform_vbar(handle: &UniformListScrollHandle) -> Option<Bar> {
    let state = handle.0.borrow();
    let size = state.last_item_size?;
    let top = state.base_handle.bounds().top();
    let offset = -state.base_handle.offset().y;
    Bar::new(Axis::Vertical, top, size.item.height, size.contents.height, offset)
}

/// Scroll distance of a gpui `list`, zero or positive.
pub fn list_offset(list: &gpui::ListState) -> Pixels {
    -list.scroll_px_offset_for_scrollbar().y
}

/// The vertical bar of a gpui `list`, from its last layout.
pub fn list_vbar(list: &gpui::ListState) -> Option<Bar> {
    let view = list.viewport_bounds();
    let max = list.max_offset_for_scrollbar().y;
    Bar::new(Axis::Vertical, view.top(), view.size.height, view.size.height + max, list_offset(list))
}

/// Scrolls a gpui `list` to `distance`.
pub fn set_list_offset(list: &gpui::ListState, distance: Pixels) {
    list.set_offset_from_scrollbar(point(px(0.), -distance));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_press_and_drag() {
        assert!(Bar::new(Axis::Vertical, px(0.), px(100.), px(100.), px(0.)).is_none());
        // Content four times the view: thumb is a quarter of the track.
        let bar = Bar::new(Axis::Vertical, px(50.), px(100.), px(400.), px(150.)).unwrap();
        assert_eq!(bar.max, px(300.));
        assert_eq!(bar.thumb_len, px(25.));
        assert_eq!(bar.thumb_at, px(37.5));
        // Above the thumb: a page up; below: a page down, clamped.
        assert_eq!(bar.press(point(px(0.), px(55.)), px(150.)).unwrap_err(), px(50.));
        assert_eq!(bar.press(point(px(0.), px(140.)), px(250.)).unwrap_err(), px(300.));
        // Dragging the thumb over the whole free track scrolls the whole range.
        let drag = bar.press(point(px(0.), px(100.)), px(150.)).unwrap();
        assert_eq!(bar.dragged(&drag, point(px(0.), px(100.) + px(75.))), px(300.));
        assert_eq!(bar.dragged(&drag, point(px(0.), px(0.))), px(0.));
    }
}
