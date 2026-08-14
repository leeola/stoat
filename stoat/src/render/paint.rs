//! Color math and cell-level painters that more than one view draws with.
//!
//! These grew inside the review renderer, where the two-column diff needed them
//! first, and every other view then imported its color helpers from the review
//! module. Nothing here carries review meaning, so it lives on its own.
//!
//! A painter that carries a view's meaning stays with that view. The
//! review-shaped ones another module legitimately borrows stay `pub(crate)` in
//! [`super::review`].

use crate::display_map::display_width;
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
    paint_style_runs(buf, start_x, y, text, max_cols, |byte_idx| {
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

        if in_moved {
            moved_style
        } else if in_span {
            highlight_style
        } else {
            base_style
        }
    });
}

/// Paint `text` at `start_x` on row `y`, giving each character the style
/// `style_at` answers for its byte offset.
///
/// `max_cols` budgets terminal columns, not characters. A double-width glyph
/// spends two, a combining mark none, and a glyph too wide for what remains of
/// the budget or of the buffer is dropped rather than cut in half. A painter
/// that walks characters as columns instead puts every glyph after the first
/// wide one in the wrong place, and its highlight spans with them.
///
/// `style_at` receives byte offsets in increasing order, so a caller resolving
/// spans through a monotonic cursor keeps that cursor across the whole row.
///
/// Characters sharing a style are painted in one write, which keeps a grapheme
/// cluster whole. The write segments by cluster, and a zero-width character
/// never opens a run, so a span boundary between a base character and its mark
/// never separates them.
pub(crate) fn paint_style_runs(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text: &str,
    max_cols: usize,
    mut style_at: impl FnMut(usize) -> Style,
) {
    let end_x = start_x
        .saturating_add(u16::try_from(max_cols).unwrap_or(u16::MAX))
        .min(buf.area.x + buf.area.width);

    let mut x = start_x;
    let mut run_start = 0;
    let mut run_style: Option<Style> = None;

    for (byte_idx, ch) in text.char_indices() {
        if display_width(ch) == 0 {
            continue;
        }
        let style = style_at(byte_idx);
        match run_style {
            Some(prev) if prev != style => {
                x = paint_run(buf, x, y, &text[run_start..byte_idx], end_x, prev);
                if x >= end_x {
                    return;
                }
                run_start = byte_idx;
                run_style = Some(style);
            },
            None => {
                run_start = byte_idx;
                run_style = Some(style);
            },
            _ => {},
        }
    }

    if let Some(style) = run_style {
        paint_run(buf, x, y, &text[run_start..], end_x, style);
    }
}

/// Paint one same-style run, answering the column after it.
fn paint_run(buf: &mut Buffer, x: u16, y: u16, text: &str, end_x: u16, style: Style) -> u16 {
    if x >= end_x {
        return x;
    }
    let (next_x, _) = buf.set_stringn(x, y, text, (end_x - x) as usize, style);
    // The write resets the cells a double-width glyph continues into, which
    // leaves half of it outside the run's style. Restating the style over what
    // the write covered keeps a span wash on whole glyphs.
    for col in x..next_x {
        buf[(col, y)].set_style(style);
    }
    next_x
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use std::slice;

    /// A wide glyph occupies the two columns its width claims, and a span over
    /// it washes both.
    ///
    /// A painter stepping one column per character puts every glyph after the
    /// first wide one a column early, so the row's text and its highlight
    /// spans disagree with the buffer about where anything sits.
    #[test]
    fn wide_glyphs_take_two_columns_and_a_span_washes_both() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let hl = Style::default().bg(Color::Rgb(10, 20, 30));
        // Bytes 3..6 are the second glyph of the three.
        let span = 3..6;
        render_side_text(
            &mut buf,
            0,
            0,
            "日本語x",
            10,
            Style::default(),
            slice::from_ref(&span),
            hl,
            &[],
            Style::default(),
        );

        assert_eq!(buf[(0, 0)].symbol(), "日");
        assert_eq!(buf[(2, 0)].symbol(), "本");
        assert_eq!(buf[(4, 0)].symbol(), "語");
        assert_eq!(buf[(6, 0)].symbol(), "x");

        assert_eq!(buf[(2, 0)].bg, Color::Rgb(10, 20, 30), "the span's glyph");
        assert_eq!(
            buf[(3, 0)].bg,
            Color::Rgb(10, 20, 30),
            "and the column it continues into",
        );
        assert_eq!(buf[(0, 0)].bg, Color::Reset, "the glyph before it");
        assert_eq!(buf[(4, 0)].bg, Color::Reset, "the glyph after it");
    }

    /// The budget drops a glyph too wide for what remains rather than painting
    /// half of it.
    #[test]
    fn a_wide_glyph_straddling_the_budget_is_dropped() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        render_side_text(
            &mut buf,
            0,
            0,
            "日本",
            3,
            Style::default(),
            &[],
            Style::default(),
            &[],
            Style::default(),
        );

        assert_eq!(buf[(0, 0)].symbol(), "日");
        assert_eq!(
            buf[(1, 0)].symbol(),
            " ",
            "the first glyph continues into the second column",
        );
        assert_eq!(
            buf[(2, 0)].symbol(),
            " ",
            "and the second glyph needs two of the one column left",
        );
    }

    /// A combining mark is drawn inside its base character's cell, taking no
    /// column of its own even when a change span starts between the two.
    #[test]
    fn a_combining_mark_stays_in_its_base_cell() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let hl = Style::default().bg(Color::Rgb(10, 20, 30));
        // "e" then U+0301, so the span opens on the mark alone.
        let span = 1..3;
        render_side_text(
            &mut buf,
            0,
            0,
            "e\u{301}x",
            10,
            Style::default(),
            slice::from_ref(&span),
            hl,
            &[],
            Style::default(),
        );

        assert_eq!(buf[(0, 0)].symbol(), "e\u{301}");
        assert_eq!(buf[(1, 0)].symbol(), "x");
        assert_eq!(
            buf[(0, 0)].bg,
            Color::Reset,
            "the cluster takes the style its base resolved",
        );
    }

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
