//! Native geometry vocabulary for the shared layout core.
//!
//! [`pane`](crate::pane) lays panes out over a [`Rect`] grid. The GUI drives
//! its own flex layout rather than reading these, so the type stays a small
//! integer rectangle rather than a float one.

/// A rectangle in terminal cells: a top-left corner plus a size.
///
/// All four fields are cell counts, so an empty rectangle has `width` or
/// `height` of zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }
}
