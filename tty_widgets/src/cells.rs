//! Shared cell-fallback drawing helpers for the box-drawing widgets.
//!
//! Widgets with a cell form (a border, a popover box) degrade by writing
//! box-drawing glyphs and fills into the ratatui buffer; these are the common
//! primitives they share.

use ratatui::{buffer::Buffer, layout::Rect, style::Style, symbols::border};

/// Set the cell at (`x`, `y`) to `symbol` in `style`, ignoring an out-of-bounds
/// position so callers need not clip to the buffer themselves.
pub(crate) fn put(buf: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(symbol).set_style(style);
    }
}

/// Draw a box-drawing perimeter around `area` using `set` and `style`.
///
/// A zero-width or zero-height area draws nothing.
pub(crate) fn draw_perimeter(buf: &mut Buffer, area: Rect, set: border::Set<'_>, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let right = area.x + area.width - 1;
    let bottom = area.y + area.height - 1;

    put(buf, area.x, area.y, set.top_left, style);
    put(buf, right, area.y, set.top_right, style);
    put(buf, area.x, bottom, set.bottom_left, style);
    put(buf, right, bottom, set.bottom_right, style);

    for x in (area.x + 1)..right {
        put(buf, x, area.y, set.horizontal_top, style);
        put(buf, x, bottom, set.horizontal_bottom, style);
    }
    for y in (area.y + 1)..bottom {
        put(buf, area.x, y, set.vertical_left, style);
        put(buf, right, y, set.vertical_right, style);
    }
}

/// Fill every cell of `area` with a space in `style`, painting its background.
pub(crate) fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.y..(area.y + area.height) {
        for x in area.x..(area.x + area.width) {
            put(buf, x, y, " ", style);
        }
    }
}
