//! Monospace cell-grid painter for the [`crate::terminal_view::Terminal`].
//!
//! Paints a terminal screen as a true cell grid rather than flow-laid-out text.
//! Full-cell background quads come first, then each row's glyphs, then a
//! theme-colored cursor.
//!
//! A row is shaped once so ligatures and font fallback still apply, but each
//! resulting glyph is painted at its source column rather than at its shaped
//! advance. An over-wide fallback glyph therefore cannot shove the rest of the
//! row off the grid, while a ligature glyph still spans the columns it covers.
//! This replaces the per-row `StyledText` divs, whose natural glyph advances
//! drift off the grid and whose backgrounds leave seams.
//!
//! [`PaintScreen`] is a per-frame snapshot the view builds with colors already
//! resolved; the paint runs in a `canvas` closure with the measured cell size.

use crate::run_pane::render::{cursor_dimensions, CURSOR_COLOR};
use gpui::{
    fill, point, px, size, BorderStyle, Bounds, Font, FontStyle, FontWeight, Hsla, PaintQuad,
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

/// Maps a shaped glyph back to the cell it came from. `byte_start` is the
/// offset of the cell's character in the row's shaped text, the same
/// concatenation [`group_runs`] builds, so a glyph's `index` resolves to the
/// column and foreground it was emitted at. A ligature glyph carries its first
/// cell's byte index, so it resolves to that cell's column.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CellSpan {
    byte_start: usize,
    column: usize,
    fg: Hsla,
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

/// One [`CellSpan`] per painted cell, in column order, skipping the spacer
/// columns after wide glyphs. The `byte_start` offsets match the row text
/// [`group_runs`] concatenates: both walk the row in order and skip
/// `wide_spacer`s, so a shaped glyph's `index` indexes into the same string.
fn cell_spans(cells: &[PaintCell]) -> Vec<CellSpan> {
    let mut spans = Vec::new();
    let mut byte_start = 0;
    for (column, cell) in cells.iter().enumerate() {
        if cell.wide_spacer {
            continue;
        }
        spans.push(CellSpan {
            byte_start,
            column,
            fg: cell.fg,
        });
        byte_start += cell.ch.len_utf8();
    }
    spans
}

/// The source cell a glyph at byte `index` came from: the last span starting at
/// or before `index`. Spans are column-ordered with ascending `byte_start`, so a
/// cluster glyph (whose `index` is its first character) resolves to the leftmost
/// cell it covers.
fn cell_for_glyph(index: usize, spans: &[CellSpan]) -> Option<CellSpan> {
    spans
        .iter()
        .rev()
        .find(|span| span.byte_start <= index)
        .copied()
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
                underline: None,
                strikethrough: None,
            });
        }
        let spans = cell_spans(row);
        let shaped =
            window
                .text_system()
                .shape_line(SharedString::from(text), font_size, &text_runs, None);

        // Paint each glyph at its source column instead of its shaped advance,
        // so an over-wide fallback glyph cannot drift the rest of the row off
        // the cell grid. A ligature glyph still spans its columns by its own
        // width. The baseline mirrors gpui's own line painter.
        let row_top = bounds.origin.y + line_height * row_idx as f32;
        let baseline = (line_height - shaped.ascent - shaped.descent) / 2.0 + shaped.ascent;
        for run in &shaped.runs {
            for glyph in &run.glyphs {
                let Some(span) = cell_for_glyph(glyph.index, &spans) else {
                    continue;
                };
                let position = point(
                    bounds.origin.x + cw * span.column as f32,
                    row_top + baseline,
                );
                if glyph.is_emoji {
                    let _ = window.paint_emoji(position, run.font_id, glyph.id, font_size);
                } else {
                    let _ = window.paint_glyph(position, run.font_id, glyph.id, font_size, span.fg);
                }
            }
        }

        paint_underlines(
            row,
            bounds.origin.x,
            cw,
            row_top + baseline + shaped.descent * 0.618,
            window,
        );
    }
}

/// Paint a grid-aligned underline under each underlined cell. Decoupled from
/// glyph painting so the line spans full cells (doubled for wide glyphs) rather
/// than the shaped advance.
fn paint_underlines(
    row: &[PaintCell],
    origin_x: Pixels,
    cw: Pixels,
    y: Pixels,
    window: &mut Window,
) {
    for (col, cell) in row.iter().enumerate() {
        if cell.wide_spacer || !cell.underline {
            continue;
        }
        let width = if cell.wide { cw * 2.0 } else { cw };
        window.paint_underline(
            point(origin_x + cw * col as f32, y),
            width,
            &UnderlineStyle {
                thickness: px(1.0),
                color: Some(cell.fg),
                wavy: false,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{cell_for_glyph, cell_spans, group_runs, PaintCell};
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

    fn wide_cell_row(fg: Hsla) -> [PaintCell; 4] {
        let mut wide = cell('世', fg, false);
        wide.wide = true;
        let mut spacer = cell(' ', fg, false);
        spacer.wide_spacer = true;
        [cell('a', fg, false), wide, spacer, cell('x', fg, false)]
    }

    #[test]
    fn cell_spans_track_column_and_byte_offset() {
        let fg: Hsla = rgb(0xffffff).into();
        let spans = cell_spans(&wide_cell_row(fg));
        let summary: Vec<(usize, usize)> = spans.iter().map(|s| (s.byte_start, s.column)).collect();
        assert_eq!(
            summary,
            vec![(0, 0), (1, 1), (4, 3)],
            "byte offsets advance by utf-8 length; the wide spacer column is skipped",
        );
    }

    #[test]
    fn cell_for_glyph_resolves_cluster_to_first_column() {
        let fg: Hsla = rgb(0xffffff).into();
        let spans = cell_spans(&wide_cell_row(fg));
        let column_at = |index| cell_for_glyph(index, &spans).map(|s| s.column);
        assert_eq!(
            column_at(0),
            Some(0),
            "glyph at the first cell maps to column 0"
        );
        assert_eq!(
            column_at(1),
            Some(1),
            "glyph at the wide char maps to its column"
        );
        assert_eq!(
            column_at(3),
            Some(1),
            "a byte inside the wide cluster maps to the cluster's start column",
        );
        assert_eq!(
            column_at(4),
            Some(3),
            "the next glyph maps past the skipped spacer to column 3",
        );
    }
}
