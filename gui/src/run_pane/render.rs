//! Run pane vterm grid render.
//!
//! Turns a [`VtermGrid`] (`stoat/src/run/vterm.rs`) into a column
//! of gpui rows where each character maps to one styled cell.
//! Mirrors the TUI's `stoat/src/render/run_pane.rs:107-137` row
//! pass: blank-and-unstyled cells drop out, the remaining cells
//! carry fg/bg/modifier styling, and selection swaps fg/bg to
//! reproduce the TUI's `Modifier::REVERSED` highlight.

use gpui::{
    div, px, AnyElement, Div, FontStyle, FontWeight, HighlightStyle, Hsla, IntoElement,
    ParentElement, SharedString, StrikethroughStyle, Styled, StyledText, UnderlineStyle,
};
use std::ops::Range;
use stoat::run::{GridSelection, OutputBlock, StyledCell, TermColor, TermModifier, VtermGrid};

/// Named-color hex table mirroring
/// `gui/src/editor/render.rs::NAMED_COLOR_HEX`. Indexed-color and
/// 24-bit RGB resolve through the same fallback ramps the editor's
/// `ratatui_color_to_hsla` uses; the duplication is intentional --
/// the editor table is private and pulling vterm rendering into
/// the editor module would be the wrong direction.
const NAMED_COLOR_HEX: [u32; 16] = [
    0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5, 0x7f7f7f,
    0xff0000, 0x00ff00, 0xffff00, 0x5c5cff, 0xff00ff, 0x00ffff, 0xffffff,
];

pub(crate) fn term_color_to_hsla(color: TermColor) -> Option<Hsla> {
    let hex = match color {
        TermColor::Reset => return None,
        TermColor::Black => NAMED_COLOR_HEX[0],
        TermColor::Red => NAMED_COLOR_HEX[1],
        TermColor::Green => NAMED_COLOR_HEX[2],
        TermColor::Yellow => NAMED_COLOR_HEX[3],
        TermColor::Blue => NAMED_COLOR_HEX[4],
        TermColor::Magenta => NAMED_COLOR_HEX[5],
        TermColor::Cyan => NAMED_COLOR_HEX[6],
        TermColor::Gray => NAMED_COLOR_HEX[7],
        TermColor::DarkGray => NAMED_COLOR_HEX[8],
        TermColor::LightRed => NAMED_COLOR_HEX[9],
        TermColor::LightGreen => NAMED_COLOR_HEX[10],
        TermColor::LightYellow => NAMED_COLOR_HEX[11],
        TermColor::LightBlue => NAMED_COLOR_HEX[12],
        TermColor::LightMagenta => NAMED_COLOR_HEX[13],
        TermColor::LightCyan => NAMED_COLOR_HEX[14],
        TermColor::White => NAMED_COLOR_HEX[15],
        TermColor::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
        TermColor::Indexed(n) => indexed_color_hex(n),
    };
    Some(gpui::rgb(hex).into())
}

fn indexed_color_hex(n: u8) -> u32 {
    if (n as usize) < NAMED_COLOR_HEX.len() {
        return NAMED_COLOR_HEX[n as usize];
    }
    if n >= 232 {
        let level = 8u32 + 10u32 * u32::from(n - 232);
        return (level << 16) | (level << 8) | level;
    }
    let offset = u32::from(n - 16);
    let r = offset / 36;
    let g = (offset / 6) % 6;
    let b = offset % 6;
    let channel = |v: u32| if v == 0 { 0 } else { 55 + 40 * v };
    (channel(r) << 16) | (channel(g) << 8) | channel(b)
}

pub(crate) fn cell_style(cell: &StyledCell, selected: bool) -> HighlightStyle {
    let mut fg = cell.fg.and_then(term_color_to_hsla);
    let mut bg = cell.bg.and_then(term_color_to_hsla);
    let reversed = selected || cell.modifiers.contains(TermModifier::REVERSED);
    if reversed {
        std::mem::swap(&mut fg, &mut bg);
    }
    HighlightStyle {
        color: fg,
        background_color: bg,
        font_weight: cell
            .modifiers
            .contains(TermModifier::BOLD)
            .then_some(FontWeight::BOLD),
        font_style: cell
            .modifiers
            .contains(TermModifier::ITALIC)
            .then_some(FontStyle::Italic),
        underline: cell
            .modifiers
            .contains(TermModifier::UNDERLINED)
            .then_some(UnderlineStyle {
                thickness: px(1.0),
                color: None,
                wavy: false,
            }),
        strikethrough: cell
            .modifiers
            .contains(TermModifier::CROSSED_OUT)
            .then_some(StrikethroughStyle {
                thickness: px(1.0),
                color: None,
            }),
        fade_out: None,
    }
}

fn cell_is_blank_unstyled(cell: &StyledCell) -> bool {
    cell.ch == ' ' && cell.fg.is_none() && cell.bg.is_none() && cell.modifiers.is_empty()
}

pub(crate) fn render_grid_row(
    grid: &VtermGrid,
    row_idx: usize,
    selection: Option<&GridSelection>,
) -> Div {
    let row = grid.row(row_idx);
    let row_u16 = u16::try_from(row_idx).unwrap_or(u16::MAX);
    let mut text = String::with_capacity(row.len());
    let mut runs: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for (col, cell) in row.iter().enumerate() {
        let col_u16 = u16::try_from(col).unwrap_or(u16::MAX);
        let selected = selection.is_some_and(|sel| sel.contains(col_u16, row_u16));
        let start = text.len();
        text.push(cell.ch);
        let end = text.len();
        if cell_is_blank_unstyled(cell) && !selected {
            continue;
        }
        runs.push((start..end, cell_style(cell, selected)));
    }
    div().child(StyledText::new(SharedString::from(text)).with_highlights(runs))
}

pub(crate) fn render_block(block: &OutputBlock) -> AnyElement {
    let header = div()
        .px_2()
        .py_1()
        .child(SharedString::from(format!("$ {}", block.command)));
    let mut col = div().flex().flex_col().w_full().child(header);
    for row_idx in 0..block.grid.line_count() {
        col = col.child(div().px_2().child(render_grid_row(
            &block.grid,
            row_idx,
            block.selection.as_ref(),
        )));
    }
    if let Some(err) = &block.error {
        col = col.child(div().px_2().child(SharedString::from(err.clone())));
    }
    if block.finished {
        let status = block.exit_status.unwrap_or(-1);
        if status != 0 {
            col = col.child(
                div()
                    .px_2()
                    .child(SharedString::from(format!("[exit {status}]"))),
            );
        }
    }
    col.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_of(color: Hsla) -> u32 {
        let rgba = color.to_rgb();
        let r = (rgba.r * 255.0).round() as u32;
        let g = (rgba.g * 255.0).round() as u32;
        let b = (rgba.b * 255.0).round() as u32;
        (r << 16) | (g << 8) | b
    }

    #[test]
    fn term_color_to_hsla_named_colors() {
        assert_eq!(
            term_color_to_hsla(TermColor::Black).map(hex_of),
            Some(0x000000),
        );
        assert_eq!(
            term_color_to_hsla(TermColor::Red).map(hex_of),
            Some(0xcd0000),
        );
        assert_eq!(
            term_color_to_hsla(TermColor::White).map(hex_of),
            Some(0xffffff),
        );
    }

    #[test]
    fn term_color_to_hsla_rgb_passthrough() {
        assert_eq!(
            term_color_to_hsla(TermColor::Rgb(0x12, 0x34, 0x56)).map(hex_of),
            Some(0x123456),
        );
    }

    #[test]
    fn term_color_to_hsla_reset_returns_none() {
        assert_eq!(term_color_to_hsla(TermColor::Reset), None);
    }

    #[test]
    fn cell_style_applies_fg_bg_and_modifiers() {
        let mut modifiers = TermModifier::empty();
        modifiers |= TermModifier::BOLD;
        modifiers |= TermModifier::ITALIC;
        let cell = StyledCell {
            ch: 'X',
            fg: Some(TermColor::Red),
            bg: Some(TermColor::Blue),
            modifiers,
        };
        let style = cell_style(&cell, false);
        assert_eq!(style.color.map(hex_of), Some(0xcd0000));
        assert_eq!(style.background_color.map(hex_of), Some(0x0000ee));
        assert_eq!(style.font_weight, Some(FontWeight::BOLD));
        assert_eq!(style.font_style, Some(FontStyle::Italic));
    }

    #[test]
    fn cell_style_selection_swaps_fg_and_bg() {
        let cell = StyledCell {
            ch: 'X',
            fg: Some(TermColor::Red),
            bg: Some(TermColor::Blue),
            modifiers: TermModifier::empty(),
        };
        let style = cell_style(&cell, true);
        assert_eq!(style.color.map(hex_of), Some(0x0000ee));
        assert_eq!(style.background_color.map(hex_of), Some(0xcd0000));
    }

    #[test]
    fn cell_style_reverse_modifier_swaps_without_selection() {
        let mut modifiers = TermModifier::empty();
        modifiers |= TermModifier::REVERSED;
        let cell = StyledCell {
            ch: 'X',
            fg: Some(TermColor::Red),
            bg: Some(TermColor::Blue),
            modifiers,
        };
        let style = cell_style(&cell, false);
        assert_eq!(style.color.map(hex_of), Some(0x0000ee));
        assert_eq!(style.background_color.map(hex_of), Some(0xcd0000));
    }

    #[test]
    fn render_grid_row_emits_cell_text() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"hello");
        let _ = render_grid_row(&grid, 0, None);
        let row_chars: String = grid.row(0).iter().map(|c| c.ch).collect();
        assert!(
            row_chars.starts_with("hello"),
            "row text should preserve fed bytes: {row_chars:?}",
        );
    }
}
