//! A column table for the modal pickers. Resolves column widths, paints a
//! header row, truncates per cell, and keeps fuzzy-match highlights on the
//! column that holds them.
//!
//! A picker row is a single joined string so the fuzzy matcher can report one
//! character offset per match. Splitting that row into columns therefore has to
//! carry the join with it, which is what [`cell_column`] resolves: given where
//! a cell starts within the join, it maps a match offset onto the screen column
//! showing that character, or reports that truncation cut it away.
//!
//! Nothing here knows what the columns hold, so a picker supplies its own cell
//! text and styles.

use ratatui::{buffer::Buffer, layout::Rect, style::Style};

/// Blank columns between one cell and the next.
const COLUMN_GAP: u16 = 1;

/// How wide a column gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Width {
    /// As wide as the widest cell it holds, clamped to `min..=max`. A column
    /// whose content varies (a branch name, an author) sizes to what is
    /// actually there rather than to a guess.
    Fit { min: u16, max: u16 },
    /// Always this wide, for a column whose content has a known shape.
    Fixed(u16),
    /// Whatever the sized columns leave. Exactly one column should be this.
    Fill,
}

/// One column of a table.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Column {
    /// Header text, painted above the rows.
    pub(crate) label: &'static str,
    pub(crate) width: Width,
}

/// Resolve each column's on-screen width across `total` cells.
///
/// `widest` is the widest cell each column holds, which the caller measures
/// over the rows it is about to paint. Sized columns take theirs first and the
/// fill column takes the remainder, so a table too narrow for its content
/// starves the fill column rather than overflowing the modal. A fill column
/// with nothing left resolves to zero and paints nothing.
pub(crate) fn resolve_widths(columns: &[Column], widest: &[u16], total: u16) -> Vec<u16> {
    let mut widths: Vec<u16> = columns
        .iter()
        .zip(widest.iter().chain(std::iter::repeat(&0)))
        .map(|(column, &widest)| match column.width {
            Width::Fit { min, max } => widest.clamp(min, max),
            Width::Fixed(w) => w,
            Width::Fill => 0,
        })
        .collect();

    let gaps = COLUMN_GAP * (columns.len().saturating_sub(1) as u16);
    let sized: u16 = widths.iter().sum();
    if let Some(fill) = columns.iter().position(|c| c.width == Width::Fill) {
        widths[fill] = total.saturating_sub(sized + gaps);
    }
    widths
}

/// Left edge of each column, relative to the table's own left edge.
pub(crate) fn column_starts(widths: &[u16]) -> Vec<u16> {
    let mut starts = Vec::with_capacity(widths.len());
    let mut x = 0;
    for &width in widths {
        starts.push(x);
        x += width + COLUMN_GAP;
    }
    starts
}

/// Paint the header labels across `area`'s first row, each truncated to its
/// column.
///
/// `style_of` picks a style per column index, so a table that highlights one
/// column can say which without the painter knowing why.
pub(crate) fn paint_header(
    buf: &mut Buffer,
    area: Rect,
    columns: &[Column],
    widths: &[u16],
    style_of: impl Fn(usize) -> Style,
) {
    if area.height == 0 {
        return;
    }
    for (i, ((column, &width), &start)) in columns
        .iter()
        .zip(widths)
        .zip(column_starts(widths).iter())
        .enumerate()
    {
        paint_cell(
            buf,
            area.x + start,
            area.y,
            column.label,
            width,
            style_of(i),
            area,
        );
    }
}

/// Paint one cell's text at `x`, clipped to `width` and to `bounds`.
pub(crate) fn paint_cell(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    text: &str,
    width: u16,
    style: Style,
    bounds: Rect,
) {
    let end = (x + width).min(bounds.x + bounds.width);
    crate::render::text::write_str_clipped(buf, x, y, text, style, end);
}

/// The screen column showing the character at haystack `offset`, or `None`
/// when no cell holds it or its cell truncated it away.
///
/// `cell_starts` is each cell's character offset within the row's joined
/// haystack, and `widths` each cell's on-screen width. A match past a cell's
/// visible width is dropped rather than clamped, so a highlight never lands on
/// a character the user is not looking at.
pub(crate) fn cell_column(
    offset: usize,
    cell_starts: &[usize],
    cell_lens: &[usize],
    widths: &[u16],
    column_starts: &[u16],
) -> Option<u16> {
    for (i, (&start, &len)) in cell_starts.iter().zip(cell_lens).enumerate() {
        if offset < start || offset >= start + len {
            continue;
        }
        let within = offset - start;
        if within >= usize::from(*widths.get(i)?) {
            return None;
        }
        return Some(column_starts.get(i)? + within as u16);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{cell_column, column_starts, resolve_widths, Column, Width};

    const COLUMNS: [Column; 3] = [
        Column {
            label: "Commit",
            width: Width::Fixed(7),
        },
        Column {
            label: "Branch",
            width: Width::Fit { min: 4, max: 10 },
        },
        Column {
            label: "Title",
            width: Width::Fill,
        },
    ];

    #[test]
    fn a_fit_column_sizes_to_its_content_within_its_clamp() {
        assert_eq!(
            resolve_widths(&COLUMNS, &[7, 6, 30], 40),
            [7, 6, 25],
            "a branch shorter than the max takes only what it needs"
        );
        assert_eq!(
            resolve_widths(&COLUMNS, &[7, 40, 30], 40)[1],
            10,
            "and a long one stops at the max"
        );
        assert_eq!(
            resolve_widths(&COLUMNS, &[7, 1, 30], 40)[1],
            4,
            "an empty column still holds its minimum, so the header fits"
        );
    }

    #[test]
    fn the_fill_column_takes_what_is_left_and_never_underflows() {
        // 7 + 6 sized, plus two single-column gaps, leaves 25 of 40.
        assert_eq!(resolve_widths(&COLUMNS, &[7, 6, 30], 40)[2], 25);
        assert_eq!(
            resolve_widths(&COLUMNS, &[7, 6, 30], 10)[2],
            0,
            "a table too narrow for its sized columns starves the fill one"
        );
    }

    #[test]
    fn columns_start_after_the_one_before_plus_a_gap() {
        assert_eq!(column_starts(&[7, 6, 25]), [0, 8, 15]);
    }

    /// A highlight has to land on the character the matcher matched, which
    /// means translating out of the joined haystack and dropping anything the
    /// cell's truncation removed.
    #[test]
    fn a_match_maps_into_its_cell_unless_truncation_cut_it() {
        // Cells "abcdefg" at 0, "main" at 8, "a title" at 13 in the join.
        let starts = [0usize, 8, 13];
        let lens = [7usize, 4, 7];
        let widths = [7u16, 4, 3];
        let column_starts = [0u16, 8, 13];

        assert_eq!(
            cell_column(2, &starts, &lens, &widths, &column_starts),
            Some(2),
            "an offset inside the first cell keeps its position"
        );
        assert_eq!(
            cell_column(9, &starts, &lens, &widths, &column_starts),
            Some(9),
            "and one in a later cell lands at that cell's own start"
        );
        assert_eq!(
            cell_column(17, &starts, &lens, &widths, &column_starts),
            None,
            "a match past the cell's truncated width is dropped, not clamped"
        );
        assert_eq!(
            cell_column(7, &starts, &lens, &widths, &column_starts),
            None,
            "so is one in the gap between cells"
        );
    }
}
