//! Shared drawing primitives and unit conversions for the widgets.
//!
//! Widgets with a cell form (a border, a popover box) degrade by writing
//! box-drawing glyphs and fills into the ratatui buffer. These are the common
//! primitives they share. Widgets with an off-grid form convert cell
//! coordinates into the sixteenths the wire commands carry, which is the other
//! half of this module.

use ratatui::{buffer::Buffer, layout::Rect, style::Style, symbols::border};

/// Convert a cell coordinate plus a sub-cell `offset` into absolute sixteenths.
///
/// The wire commands anchor in signed sixteenths, so a widget positioned near
/// the far edge of a wide surface lands past i16. The math widens to i32 and
/// saturates, because a clamped anchor draws at the edge while a wrapped one
/// jumps to the opposite side of the screen.
///
/// `offset` takes u16 and i16 alike, matching the widgets that expose an
/// unsigned area-relative offset and those that allow a negative nudge.
pub(crate) fn to_sixteenths(cell: u16, offset: impl Into<i32>) -> i16 {
    let total = i32::from(cell) * 16 + offset.into();
    total.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// Narrow a sixteenths offset computed in u16 to the signed anchor the wire
/// commands carry.
///
/// Saturates rather than wrapping, because a wrapped offset reads as a large
/// negative one and throws the element to the far side of the surface.
pub(crate) fn signed_sixteenths(offset: u16) -> i16 {
    offset.min(i16::MAX as u16) as i16
}

/// Convert a whole-cell width or height into sixteenths.
///
/// Saturates rather than wrapping, so an implausibly tall surface yields a
/// span that covers everything instead of a short one that covers nothing.
pub(crate) fn span_sixteenths(cells: u16) -> u16 {
    (u32::from(cells) * 16).min(u32::from(u16::MAX)) as u16
}

/// Sixteenths a run of `chars` advances at `scale`, the glyph size in 256ths of
/// a cell.
///
/// The product overflows a u16 long before the quotient does, so the widening
/// matters even for advances that comfortably fit the result. Takes the
/// character count directly rather than a narrowed one, since a truncated
/// count corrupts the answer before any arithmetic runs.
pub(crate) fn advance_sixteenths(chars: usize, scale: u16) -> u16 {
    let total = (chars as u64).saturating_mul(u64::from(scale)) / 16;
    total.min(u64::from(u16::MAX)) as u16
}

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

#[cfg(test)]
mod tests {
    use super::{advance_sixteenths, signed_sixteenths, span_sixteenths, to_sixteenths};

    #[test]
    fn an_anchor_past_i16_clamps_to_the_edge() {
        assert_eq!(to_sixteenths(2047, 15u16), 32767, "the last exact anchor");
        assert_eq!(to_sixteenths(2048, 0u16), i16::MAX, "one cell further");
        assert_eq!(to_sixteenths(u16::MAX, u16::MAX), i16::MAX, "and far past");
        assert_eq!(to_sixteenths(0, i16::MIN), i16::MIN, "a nudge off the left");
    }

    #[test]
    fn an_offset_past_i16_clamps_to_the_edge() {
        assert_eq!(signed_sixteenths(32767), i16::MAX, "the last exact offset");
        assert_eq!(signed_sixteenths(32768), i16::MAX, "one sixteenth further");
        assert_eq!(signed_sixteenths(u16::MAX), i16::MAX, "and a saturated one");
    }

    #[test]
    fn a_span_past_u16_clamps_to_the_widest() {
        assert_eq!(span_sixteenths(4095), 65520, "the last exact span");
        assert_eq!(span_sixteenths(4096), u16::MAX, "one cell further");
    }

    /// The product overflows a u16 well before the quotient does, so a legal
    /// advance is the interesting case, not a saturating one.
    #[test]
    fn a_long_run_advances_without_overflowing_the_product() {
        assert_eq!(
            advance_sixteenths(410, 160),
            4100,
            "65600 before the divide"
        );
        assert_eq!(
            advance_sixteenths(usize::MAX, 256),
            u16::MAX,
            "and saturates"
        );
    }
}
