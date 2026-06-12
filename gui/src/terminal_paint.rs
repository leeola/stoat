//! Monospace cell-grid painter for the [`crate::terminal_view::Terminal`].
//!
//! Paints a terminal screen as a true cell grid rather than flow-laid-out text:
//! full-cell background quads first, then each row's glyphs shaped once and
//! painted at the row origin (cell width is the font's em-advance, so glyphs
//! land on the column grid), then a theme-colored cursor. This replaces the
//! per-row `StyledText` divs, whose natural glyph advances drift off the grid
//! and whose backgrounds leave seams.
//!
//! [`PaintScreen`] is a per-frame snapshot the view builds with colors already
//! resolved; the paint runs in a `canvas` closure with the measured cell size.

use crate::run_pane::render::{cursor_dimensions, CURSOR_COLOR};
use gpui::{
    fill, point, px, size, App, BorderStyle, Bounds, Font, FontStyle, FontWeight, Hsla, PaintQuad,
    Pixels, SharedString, Size, TextRun, UnderlineStyle, Window,
};
use stoat::run::CursorShape;

/// One resolved screen cell ready to paint. `bg == None` is the terminal's
/// default (transparent) background; a wide glyph sets `wide` and the column
/// after it sets `wide_spacer` (skipped, the glyph spans both columns).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PaintCell {
    pub ch: char,
    pub fg: Hsla,
    pub bg: Option<Hsla>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub wide: bool,
    pub wide_spacer: bool,
}

/// The cursor's grid position, shape, and whether the pane is focused (a filled
/// block when focused, a hollow outline when not).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PaintCursor {
    pub line: usize,
    pub column: usize,
    pub shape: CursorShape,
    pub focused: bool,
}

/// A per-frame screen snapshot: every row's cells, plus the cursor if visible.
pub(crate) struct PaintScreen {
    pub rows: Vec<Vec<PaintCell>>,
    pub cursor: Option<PaintCursor>,
}

/// A maximal run of same-styled cells within a row, the unit shaped as one
/// [`TextRun`]. Background is painted per cell separately, so it is not a run
/// key; only the glyph color and text decorations are.
#[derive(Clone, Debug, PartialEq)]
struct RunSpan {
    text: String,
    fg: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl RunSpan {
    fn matches(&self, cell: &PaintCell) -> bool {
        self.fg == cell.fg
            && self.bold == cell.bold
            && self.italic == cell.italic
            && self.underline == cell.underline
    }
}

/// Group a row's cells into runs of identical glyph style, skipping the spacer
/// columns that follow wide glyphs. The concatenated run text is the row's
/// glyph string; the shaped line advances each glyph by the font's em-advance,
/// so a wide glyph's own double advance fills the skipped column.
fn group_runs(cells: &[PaintCell]) -> Vec<RunSpan> {
    let mut runs: Vec<RunSpan> = Vec::new();
    for cell in cells {
        if cell.wide_spacer {
            continue;
        }
        match runs.last_mut() {
            Some(run) if run.matches(cell) => run.text.push(cell.ch),
            _ => runs.push(RunSpan {
                text: cell.ch.to_string(),
                fg: cell.fg,
                bold: cell.bold,
                italic: cell.italic,
                underline: cell.underline,
            }),
        }
    }
    runs
}

fn styled_font(base: &Font, bold: bool, italic: bool) -> Font {
    let mut font = base.clone();
    if bold {
        font.weight = FontWeight::BOLD;
    }
    if italic {
        font.style = FontStyle::Italic;
    }
    font
}

/// Paint `screen` into `bounds` using `cell_size` whole-cell metrics. The cell
/// width must be the font's em-advance so shaped glyphs align to the columns
/// the backgrounds and cursor are drawn at.
pub(crate) fn paint_screen(
    screen: &PaintScreen,
    bounds: Bounds<Pixels>,
    cell_size: Size<Pixels>,
    base_font: &Font,
    font_size: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let cw = cell_size.width;
    let line_height = cell_size.height;

    // Backgrounds first, as full cell rects, so adjacent same-bg cells form a
    // seamless fill rather than the glyph-height runs StyledText produced.
    for (row_idx, row) in screen.rows.iter().enumerate() {
        for (col, cell) in row.iter().enumerate() {
            if cell.wide_spacer {
                continue;
            }
            if let Some(bg) = cell.bg {
                let width = if cell.wide { cw * 2.0 } else { cw };
                let origin = point(
                    bounds.origin.x + cw * col as f32,
                    bounds.origin.y + line_height * row_idx as f32,
                );
                window.paint_quad(fill(
                    Bounds {
                        origin,
                        size: size(width, line_height),
                    },
                    bg,
                ));
            }
        }
    }

    // The cursor sits under the glyphs: painting text last lets the cell's
    // character show over a filled block.
    if let Some(cursor) = &screen.cursor {
        let dims = cursor_dimensions(cursor.shape, cell_size);
        let left = bounds.origin.x + cw * cursor.column as f32;
        let top = bounds.origin.y
            + line_height * cursor.line as f32
            + match cursor.shape {
                CursorShape::Underline => line_height - dims.height,
                _ => px(0.0),
            };
        let cursor_bounds = Bounds {
            origin: point(left, top),
            size: dims,
        };
        if cursor.focused {
            window.paint_quad(fill(cursor_bounds, CURSOR_COLOR));
        } else {
            window.paint_quad(PaintQuad {
                bounds: cursor_bounds,
                corner_radii: px(0.0).into(),
                background: gpui::transparent_black().into(),
                border_color: CURSOR_COLOR,
                border_widths: px(1.0).into(),
                border_style: BorderStyle::default(),
            });
        }
    }

    for (row_idx, row) in screen.rows.iter().enumerate() {
        let runs = group_runs(row);
        if runs.is_empty() {
            continue;
        }
        let mut text = String::new();
        let mut text_runs = Vec::with_capacity(runs.len());
        for run in &runs {
            text.push_str(&run.text);
            text_runs.push(TextRun {
                len: run.text.len(),
                font: styled_font(base_font, run.bold, run.italic),
                color: run.fg,
                background_color: None,
                underline: run.underline.then(|| UnderlineStyle {
                    thickness: px(1.0),
                    color: None,
                    wavy: false,
                }),
                strikethrough: None,
            });
        }
        let shaped =
            window
                .text_system()
                .shape_line(SharedString::from(text), font_size, &text_runs, None);
        let origin = point(
            bounds.origin.x,
            bounds.origin.y + line_height * row_idx as f32,
        );
        let _ = shaped.paint(origin, line_height, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{group_runs, PaintCell};
    use gpui::{rgb, Hsla};

    fn cell(ch: char, fg: Hsla, bold: bool) -> PaintCell {
        PaintCell {
            ch,
            fg,
            bg: None,
            bold,
            italic: false,
            underline: false,
            wide: false,
            wide_spacer: false,
        }
    }

    #[test]
    fn merges_same_style_and_splits_on_change() {
        let red: Hsla = rgb(0xff0000).into();
        let blue: Hsla = rgb(0x0000ff).into();
        let row = [
            cell('a', red, false),
            cell('b', red, false),
            cell('c', blue, false),
            cell('d', blue, true),
        ];
        let runs = group_runs(&row);
        let summary: Vec<(&str, bool)> = runs.iter().map(|r| (r.text.as_str(), r.bold)).collect();
        assert_eq!(
            summary,
            vec![("ab", false), ("c", false), ("d", true)],
            "same fg+style merges; fg and bold changes split"
        );
    }

    #[test]
    fn skips_wide_spacer_columns() {
        let fg: Hsla = rgb(0xffffff).into();
        let mut wide = cell('世', fg, false);
        wide.wide = true;
        let mut spacer = cell(' ', fg, false);
        spacer.wide_spacer = true;
        let row = [wide, spacer, cell('x', fg, false)];
        let runs = group_runs(&row);
        assert_eq!(runs.len(), 1, "spacer does not break the run");
        assert_eq!(runs[0].text, "世x", "spacer column contributes no glyph");
    }
}
