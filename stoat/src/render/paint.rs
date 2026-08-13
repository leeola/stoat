//! Color math and cell-level painters that more than one view draws with.
//!
//! These grew inside the review renderer, where the two-column diff needed them
//! first, and every other view then imported its color helpers from the review
//! module. Nothing here carries review meaning, so it lives on its own.
//!
//! A painter that carries a view's meaning stays with that view. The
//! review-shaped ones another module legitimately borrows stay `pub(crate)` in
//! [`super::review`].

use ratatui::{
    buffer::Buffer,
    style::{Color, Style},
};
use std::fmt::Write;

/// Fill `cols` content cells from `start_x` with a background wash, leaving each
/// symbol untouched so text painted afterward keeps the wash. Ratatui's
/// `set_style` patches only the fields a style sets, and token styles set no
/// background.
pub(crate) fn fill_line_tint(buf: &mut Buffer, start_x: u16, y: u16, cols: usize, tint: [u8; 3]) {
    let color = Color::Rgb(tint[0], tint[1], tint[2]);
    for i in 0..cols {
        let x = start_x + i as u16;
        if x >= buf.area.x + buf.area.width {
            break;
        }
        buf[(x, y)].set_bg(color);
    }
}

pub(crate) fn style_rgb(color: Option<Color>) -> Option<[u8; 3]> {
    match color {
        Some(Color::Rgb(r, g, b)) => Some([r, g, b]),
        _ => None,
    }
}

/// Blend `fg` toward `bg` by `amount`, where `0.0` returns `fg` unchanged and
/// `1.0` returns `bg`.
///
/// Dims an unfocused pane's colors toward the theme background by a configurable
/// fraction. `amount` is clamped to `0.0..=1.0`.
pub(crate) fn dim_rgb(fg: [u8; 3], bg: [u8; 3], amount: f32) -> [u8; 3] {
    let amount = amount.clamp(0.0, 1.0);
    let blend = |f: u8, b: u8| (f as f32 * (1.0 - amount) + b as f32 * amount).round() as u8;
    [
        blend(fg[0], bg[0]),
        blend(fg[1], bg[1]),
        blend(fg[2], bg[2]),
    ]
}

/// Paint `num` right-aligned in the five-cell number field at `x`.
///
/// `scratch` holds the formatted field. Callers paint one number per row of a
/// side, so they pass one buffer for the whole loop rather than letting each row
/// allocate its own.
pub(crate) fn render_side_num(
    buf: &mut Buffer,
    scratch: &mut String,
    x: u16,
    y: u16,
    num: u32,
    style: Style,
) {
    scratch.clear();
    write!(scratch, "{num:>4} ").expect("writing to a String is infallible");
    for (i, ch) in scratch.chars().enumerate() {
        let col = x + i as u16;
        if col >= buf.area.x + buf.area.width {
            break;
        }
        buf[(col, y)].set_char(ch).set_style(style);
    }
}

pub(crate) fn render_empty_num(buf: &mut Buffer, x: u16, y: u16, style: Style) {
    for i in 0..5u16 {
        let col = x + i;
        if col >= buf.area.x + buf.area.width {
            break;
        }
        buf[(col, y)].set_char('.').set_style(style);
    }
}

/// Render text with sub-line change span highlighting. Characters
/// within any `spans` range get `highlight_style`; characters within
/// any `moved_spans` range get the diff theme's move color (cyan)
/// regardless of which side they live on. The rest get `base_style`.
///
/// Move highlighting takes precedence over change highlighting: if a
/// byte falls in both a change span and a moved span, the move color
/// wins so users see at a glance that the token relocated rather than
/// was replaced.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_side_text(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text: &str,
    max_cols: usize,
    base_style: Style,
    spans: &[std::ops::Range<usize>],
    highlight_style: Style,
    moved_spans: &[std::ops::Range<usize>],
    moved_style: Style,
) {
    debug_assert!(
        moved_spans.is_sorted_by_key(|s| s.start),
        "moved_spans must be start-sorted for the monotonic cursor"
    );
    debug_assert!(
        spans.is_sorted_by_key(|s| s.start),
        "spans must be start-sorted for the monotonic cursor"
    );

    let mut moved_cursor = 0;
    let mut span_cursor = 0;
    for (col, (byte_idx, ch)) in text.char_indices().enumerate() {
        if col >= max_cols {
            break;
        }
        let x = start_x + col as u16;
        if x >= buf.area.x + buf.area.width {
            break;
        }

        while moved_spans
            .get(moved_cursor)
            .is_some_and(|s| s.end <= byte_idx)
        {
            moved_cursor += 1;
        }
        let in_moved = matches!(moved_spans.get(moved_cursor), Some(s) if s.start <= byte_idx);

        while spans.get(span_cursor).is_some_and(|s| s.end <= byte_idx) {
            span_cursor += 1;
        }
        let in_span = matches!(spans.get(span_cursor), Some(s) if s.start <= byte_idx);

        let style = if in_moved {
            moved_style
        } else if in_span {
            highlight_style
        } else {
            base_style
        };
        buf[(x, y)].set_char(ch).set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dim_rgb_blends_between_fg_and_bg() {
        let fg = [200, 100, 40];
        assert_eq!(dim_rgb(fg, [0, 0, 0], 0.0), fg, "amount 0 keeps fg");
        assert_eq!(
            dim_rgb(fg, [0, 0, 0], 1.0),
            [0, 0, 0],
            "amount 1 reaches bg"
        );
        assert_eq!(
            dim_rgb(fg, [50, 50, 50], 0.5),
            [125, 75, 45],
            "midpoint blend"
        );
        assert_eq!(
            dim_rgb(fg, [0, 0, 0], 2.0),
            [0, 0, 0],
            "amount clamps above 1"
        );
    }
}
