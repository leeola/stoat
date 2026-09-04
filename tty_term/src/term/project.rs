//! Terminal state resolved into the values the grid carries.
//!
//! The VT screen holds palette-relative colors, attribute bitflags, and a cursor
//! in terminal coordinates. A [`Grid`] holds concrete channels, stoatty's own
//! flags, and viewport rows. This module is the conversion between them, and it
//! owns the palette the color half resolves against.

use super::{Cursor, CursorShape, ResponseSink, PALETTE_LEN};
use crate::{
    grid::{whole_row, BorderId, Cell, Flags, Grid, Rgb, RowDamage, Scale, UnderlineStyle},
    theme::Theme,
};
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line},
    selection::SelectionRange,
    term::{
        cell::{Cell as TermCell, Flags as TermFlags},
        color::Colors,
        RenderableCursor,
    },
    vte::ansi::{Color, CursorShape as TermCursorShape, NamedColor},
    Term,
};

/// Project a fill context's on-screen cells onto a page grid.
///
/// Returns whether any cell it wrote differed from what the slot held, which is
/// the cell half of a fill's change verdict.
///
/// The pool clears the slot before this runs, so it copies each on-screen cell
/// without damage tracking, resolving colors exactly as [`Terminal::project`]
/// does for the live grid. Cells past the page's bounds are skipped.
///
/// The two grids are sized independently, so the copy is clamped to their overlap
/// rather than assuming they agree, leaving any cell outside it with the pool's
/// clear. They usually agree, since a fill's context is built at its pool region's
/// size. A region re-declaration mid-fill moves the region under the open fill and
/// is the case the clamp exists for.
pub(super) fn project_term_cells(
    grid: &mut Grid,
    term: &Term<ResponseSink>,
    theme: &Theme,
    palette: &[Rgb; PALETTE_LEN],
) -> bool {
    let content = term.renderable_content();
    let offset = content.display_offset as i32;

    // Read per row from the term's own grid rather than filtering a cell iterator,
    // so each row costs one bounds check instead of one per cell. `display_iter`
    // maps grid `line + offset` to viewport `row`, so `line = row - offset` names
    // the same cells, the inversion [`Terminal::project`] uses.
    let term_grid = term.grid();
    let rows = grid.rows().min(term.screen_lines());
    let cols = grid.cols().min(term.columns());

    // The comparison rides the write rather than following it. Every cell this
    // paints is already in a register, so asking whether it differs from the
    // slot costs nothing next to reading the whole page back afterward.
    let mut changed = false;
    for row in 0..rows {
        let source = &term_grid[Line(row as i32 - offset)];
        for (col, out) in grid.row_mut(row)[..cols].iter_mut().enumerate() {
            let cell = project_cell(&source[Column(col)], content.colors, theme, palette);
            changed |= cell != *out;
            *out = cell;
        }
    }
    changed
}

/// A cleared row-flag buffer `rows` long, reusing one a past frame gave back
/// through [`Terminal::recycle_damage`] when `spare` holds one.
///
/// Takes that list alone rather than the whole terminal, so a caller reaches it
/// while the terminal's own content or damage is borrowed. Every call site is
/// in the middle of that, three of them writing the result into a `Damage` on
/// the same statement.
pub(super) fn row_bounds(spare: &mut Vec<Vec<RowDamage>>, rows: usize) -> Vec<RowDamage> {
    let mut bounds = spare.pop().unwrap_or_default();
    bounds.clear();
    bounds.resize(rows, None);
    bounds
}

/// Rows below the probe that a candidate shift has to match as well.
const CONFIRM_ROWS: usize = 4;

/// How far the screen's content moved up since the last projection, found by
/// locating the row of `grid` the new top row now holds.
///
/// A scroll that grows scrollback reports how far it moved. This is for the
/// ones that do not, being an alt-screen scroll and a scroll region below the
/// top line, and for INSERT mode, which damages the whole screen without moving
/// anything. Zero is the answer for that last case and the fallback for any
/// screen the probe fails to place.
///
/// One row of text repeats often enough to match in the wrong place, so a
/// candidate is confirmed against the rows below it. Nothing rests on the
/// answer either way. Every row is still projected and compared, so a wrong
/// shift only marks rows dirty, which is what a full frame did anyway.
///
/// The scan runs ascending, which is what makes a screen of repeated blank rows
/// answer zero rather than wherever the probe happens to match first.
pub(super) fn detect_shift(
    grid: &Grid,
    rows: usize,
    scratch: &mut [Cell],
    project_into: impl Fn(usize, &mut [Cell]),
) -> usize {
    project_into(0, scratch);

    for shift in 0..rows {
        if grid.row(shift) != &scratch[..] {
            continue;
        }

        let mut confirmed = true;
        for probe in 1..=CONFIRM_ROWS {
            if probe >= rows || shift + probe >= rows {
                break;
            }
            project_into(probe, scratch);
            if grid.row(shift + probe) != &scratch[..] {
                confirmed = false;
                break;
            }
        }
        if confirmed {
            return shift;
        }

        // The confirm overwrote the scratch, so the probe row goes back into it
        // before the scan tries the next candidate.
        project_into(0, scratch);
    }

    0
}

/// The inclusive viewport row span a selection covers, clamped to the grid, or
/// `None` when it falls entirely below the viewport.
///
/// `offset` is the display offset the projection ran at, converting the
/// selection's terminal lines to viewport rows the same way the cell loop does.
pub(super) fn selection_span(
    range: &SelectionRange,
    offset: i32,
    rows: usize,
) -> Option<(usize, usize)> {
    let top = (range.start.line.0 + offset).max(0) as usize;
    let bottom = (range.end.line.0 + offset).max(0) as usize;
    if top >= rows {
        return None;
    }
    Some((top, bottom.min(rows - 1)))
}

/// Mark every viewport row whose selection overlay differs between `old` and
/// `new`.
///
/// A row inside both ranges is inverted end to end in both, so it did not
/// change, which is what keeps a drag across a tall selection from repainting
/// all of it. Two kinds of row did change: one exactly one range covers, which
/// entered or left the selection, and an endpoint row, which is bounded by its
/// own range's column and so moves under a drag that stays within it.
///
/// A block selection bounds every row it covers by the same columns, so moving
/// those columns moves all of them and both spans mark whole.
pub(super) fn mark_selection_change(
    old: Option<SelectionRange>,
    new: Option<SelectionRange>,
    offset: i32,
    rows: usize,
    cols: usize,
    rows_dirty: &mut [RowDamage],
) {
    let old_span = old.and_then(|range| selection_span(&range, offset, rows));
    let new_span = new.and_then(|range| selection_span(&range, offset, rows));

    let mut mark = |row: usize| {
        if let Some(slot) = rows_dirty.get_mut(row) {
            *slot = whole_row(cols);
        }
    };

    if old.is_some_and(|range| range.is_block) || new.is_some_and(|range| range.is_block) {
        for (lo, hi) in old_span.into_iter().chain(new_span) {
            for row in lo..=hi {
                mark(row);
            }
        }
        return;
    }

    // Walked and tested rather than split into difference intervals. The union
    // of two viewport spans is a few dozen rows at most and the test is a pair
    // of comparisons, while what costs is the marking, which only reaches the
    // rows that changed.
    let covers = |span: Option<(usize, usize)>, row: usize| {
        span.is_some_and(|(lo, hi)| row >= lo && row <= hi)
    };
    let union = old_span.iter().chain(&new_span);
    if let (Some(lo), Some(hi)) = (
        union.clone().map(|&(lo, _)| lo).min(),
        union.map(|&(_, hi)| hi).max(),
    ) {
        for row in lo..=hi {
            if covers(old_span, row) != covers(new_span, row) {
                mark(row);
            }
        }
    }

    for range in old.into_iter().chain(new) {
        for line in [range.start.line, range.end.line] {
            let row = line.0 + offset;
            if row >= 0 {
                mark(row as usize);
            }
        }
    }
}

pub(super) fn project_cell(
    cell: &TermCell,
    overrides: &Colors,
    theme: &Theme,
    palette: &[Rgb; PALETTE_LEN],
) -> Cell {
    let fg = resolve(cell.fg, overrides, theme, palette);
    let underline_color = match cell.underline_color() {
        Some(color) => resolve(color, overrides, theme, palette),
        None => fg,
    };

    Cell {
        ch: cell.c,
        fg,
        bg: resolve(cell.bg, overrides, theme, palette),
        flags: map_flags(cell.flags),
        underline: map_underline(cell.flags),
        underline_color,
        // Borders and scale come from the stoatty APC, not the VT stream, so a
        // projected cell carries neither.
        border_id: BorderId::NONE,
        scale: Scale::Single,
    }
}

/// Resolve a terminal [`Color`] to concrete channels.
///
/// A program-set `overrides` entry wins over the default palette for the same
/// slot, mirroring how a VT terminal lets OSC redefine palette colors.
fn resolve(color: Color, overrides: &Colors, theme: &Theme, palette: &[Rgb; PALETTE_LEN]) -> Rgb {
    match color {
        Color::Spec(rgb) => Rgb::new(rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => indexed(index as usize, overrides, palette),
        Color::Named(named) => named_color(named, overrides, theme, palette),
    }
}

pub(super) fn named_color(
    named: NamedColor,
    overrides: &Colors,
    theme: &Theme,
    palette: &[Rgb; PALETTE_LEN],
) -> Rgb {
    if let Some(rgb) = overrides[named as usize] {
        return Rgb::new(rgb.r, rgb.g, rgb.b);
    }

    match named {
        NamedColor::Background => theme.background,
        NamedColor::Foreground | NamedColor::BrightForeground => theme.foreground,
        ansi if (ansi as usize) < PALETTE_LEN => palette[ansi as usize],
        _ => theme.foreground,
    }
}

pub(super) fn indexed(index: usize, overrides: &Colors, palette: &[Rgb; PALETTE_LEN]) -> Rgb {
    match overrides[index] {
        Some(rgb) => Rgb::new(rgb.r, rgb.g, rgb.b),
        None => palette[index],
    }
}

/// Map the terminal's cell flags to the boolean attributes stoatty's grid
/// carries.
///
/// Underline is not among them; it is mapped separately by [`map_underline`].
/// `INVERSE` and `DIM` stay flags rather than being baked into the colors, so
/// the renderer applies them at draw time.
fn map_flags(flags: TermFlags) -> Flags {
    let mut mapped = Flags::empty();

    if flags.contains(TermFlags::BOLD) {
        mapped |= Flags::BOLD;
    }
    if flags.contains(TermFlags::ITALIC) {
        mapped |= Flags::ITALIC;
    }
    if flags.contains(TermFlags::DIM) {
        mapped |= Flags::DIM;
    }
    if flags.contains(TermFlags::INVERSE) {
        mapped |= Flags::INVERSE;
    }
    if flags.contains(TermFlags::HIDDEN) {
        mapped |= Flags::HIDDEN;
    }
    if flags.contains(TermFlags::STRIKEOUT) {
        mapped |= Flags::STRIKEOUT;
    }
    if flags.contains(TermFlags::WIDE_CHAR) {
        mapped |= Flags::WIDE;
    }

    mapped
}

/// Map the terminal's underline flags to a stoatty [`UnderlineStyle`].
///
/// A cell carries at most one underline flag, so the most specific match wins;
/// a plain `UNDERLINE` is the straight fallback.
fn map_underline(flags: TermFlags) -> UnderlineStyle {
    if flags.contains(TermFlags::DOUBLE_UNDERLINE) {
        UnderlineStyle::Double
    } else if flags.contains(TermFlags::UNDERCURL) {
        UnderlineStyle::Curly
    } else if flags.contains(TermFlags::DOTTED_UNDERLINE) {
        UnderlineStyle::Dotted
    } else if flags.contains(TermFlags::DASHED_UNDERLINE) {
        UnderlineStyle::Dashed
    } else if flags.contains(TermFlags::UNDERLINE) {
        UnderlineStyle::Straight
    } else {
        UnderlineStyle::None
    }
}

pub(super) fn project_cursor(cursor: RenderableCursor, offset: i32) -> Cursor {
    Cursor {
        row: (cursor.point.line.0 + offset).max(0) as usize,
        col: cursor.point.column.0,
        shape: map_shape(cursor.shape),
    }
}

fn map_shape(shape: TermCursorShape) -> CursorShape {
    match shape {
        TermCursorShape::Block => CursorShape::Block,
        TermCursorShape::Underline => CursorShape::Underline,
        TermCursorShape::Beam => CursorShape::Beam,
        TermCursorShape::HollowBlock => CursorShape::HollowBlock,
        TermCursorShape::Hidden => CursorShape::Hidden,
    }
}

/// Build the 256-color palette for `theme`.
///
/// Indices 0..16 are the theme's ANSI colors, 16..232 the 6x6x6 color cube, and
/// 232..256 the 24-step grayscale ramp.
pub(super) fn default_palette(theme: &Theme) -> [Rgb; PALETTE_LEN] {
    let mut palette = [theme.background; PALETTE_LEN];
    palette[..16].copy_from_slice(&theme.ansi);

    let mut index = 16;
    for r in 0..6u8 {
        for g in 0..6u8 {
            for b in 0..6u8 {
                palette[index] = Rgb::new(cube_channel(r), cube_channel(g), cube_channel(b));
                index += 1;
            }
        }
    }

    for (step, slot) in palette[232..].iter_mut().enumerate() {
        let level = 8 + step as u8 * 10;
        *slot = Rgb::new(level, level, level);
    }

    palette
}

/// Map a 0..6 cube coordinate to its channel value (0, then 95..255 by 40).
fn cube_channel(level: u8) -> u8 {
    if level == 0 {
        0
    } else {
        55 + level * 40
    }
}
