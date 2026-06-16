//! The bytes-to-grid driver: a VT byte stream parsed onto the cell grid.
//!
//! [`Terminal`] wraps an `alacritty_terminal` terminal and its vte parser.
//! Bytes fed to [`Terminal::advance`] mutate the parsed screen, and
//! [`Terminal::project`] copies that screen onto a [`Grid`]. The copy resolves
//! each cell's terminal-palette color to concrete channels and touches only the
//! lines the terminal reports as damaged.

use crate::grid::{Cell, Flags, Grid, Rgb};
use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    term::{
        cell::{Cell as TermCell, Flags as TermFlags},
        color::Colors,
        Config, RenderableCursor, TermDamage,
    },
    vte::ansi::{Color, CursorShape as TermCursorShape, NamedColor, Processor},
    Term,
};

const PALETTE_LEN: usize = 256;
const DEFAULT_FG: Rgb = Rgb::new(0xcc, 0xcc, 0xcc);
const DEFAULT_BG: Rgb = Rgb::new(0x00, 0x00, 0x00);

/// A live terminal driven by a VT byte stream.
///
/// Owns the parsed screen (an `alacritty_terminal` terminal) and the vte parser
/// that feeds it. No IO lives here: the app crate owns the PTY and pushes bytes
/// in via [`Self::advance`], then calls [`Self::project`] to refresh the render
/// grid.
///
/// Carries a default 256-color palette so [`Self::project`] can resolve a cell's
/// indexed or named color to concrete channels. A color the program overrode
/// (via OSC) takes precedence over the default.
pub struct Terminal {
    term: Term<VoidListener>,
    parser: Processor,
    palette: [Rgb; PALETTE_LEN],
}

/// Where the cursor sits and how it is drawn, as of the last [`Terminal::project`].
///
/// `row` and `col` are zero-based coordinates into the projected [`Grid`]. The
/// grid carries no cursor cell of its own, so the renderer reads this separately
/// to draw the cursor over the cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub shape: CursorShape,
}

/// The shape a cursor is drawn as.
///
/// A stoatty-owned mirror of the VT cursor styles, so the public API does not
/// leak the `alacritty_terminal` enum. [`CursorShape::Hidden`] means the program
/// asked for the cursor not to be shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}

impl Terminal {
    /// Create a `rows` by `cols` terminal with an empty screen and default palette.
    pub fn new(rows: usize, cols: usize) -> Terminal {
        let term = Term::new(Config::default(), &GridSize { rows, cols }, VoidListener);

        Terminal {
            term,
            parser: Processor::new(),
            palette: default_palette(),
        }
    }

    /// Feed `bytes` of the VT stream into the parser, mutating the screen.
    ///
    /// Bytes need not be escape-sequence aligned; the parser retains a partial
    /// sequence across calls.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Copy the parsed screen onto `grid` and return the cursor.
    ///
    /// Only lines the terminal reports as damaged since the previous call are
    /// rewritten, so an unchanged line keeps whatever the prior projection left
    /// in `grid`. When `grid`'s dimensions do not match the terminal it is first
    /// resized, which clears it, and every line is treated as damaged.
    pub fn project(&mut self, grid: &mut Grid) -> Cursor {
        let rows = self.term.screen_lines();
        let cols = self.term.columns();

        let resized = grid.rows() != rows || grid.cols() != cols;
        if resized {
            grid.resize(rows, cols);
        }

        let dirty = self.collect_damage(rows, resized);

        let content = self.term.renderable_content();
        let offset = content.display_offset as i32;

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 {
                continue;
            }

            let (row, col) = (row as usize, indexed.point.column.0);
            if row >= rows || col >= cols || !dirty.is_dirty(row) {
                continue;
            }

            *grid.get_mut(row, col) = project_cell(indexed.cell, content.colors, &self.palette);
        }

        let cursor = project_cursor(content.cursor, offset);
        self.term.reset_damage();
        cursor
    }

    /// Resolve which rows [`Self::project`] must rewrite this frame.
    ///
    /// `force_full` short-circuits to [`Dirty::Full`] when the grid was just
    /// resized and holds no valid prior content, bypassing the terminal's own
    /// damage which may report only a partial change.
    fn collect_damage(&mut self, rows: usize, force_full: bool) -> Dirty {
        if force_full {
            return Dirty::Full;
        }

        match self.term.damage() {
            TermDamage::Full => Dirty::Full,
            TermDamage::Partial(lines) => {
                let mut rows_dirty = vec![false; rows];
                for bounds in lines {
                    if let Some(slot) = rows_dirty.get_mut(bounds.line) {
                        *slot = true;
                    }
                }
                Dirty::Partial(rows_dirty)
            },
        }
    }
}

/// The set of viewport rows a projection must rewrite.
enum Dirty {
    Full,
    Partial(Vec<bool>),
}

impl Dirty {
    fn is_dirty(&self, row: usize) -> bool {
        match self {
            Dirty::Full => true,
            Dirty::Partial(rows) => rows.get(row).copied().unwrap_or(false),
        }
    }
}

/// Adapts stoatty's row/column count to `alacritty_terminal`'s [`Dimensions`].
///
/// `total_lines` equals `screen_lines`: the terminal grows its own scrollback
/// from the config, so no history rows are declared up front.
struct GridSize {
    rows: usize,
    cols: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

fn project_cell(cell: &TermCell, overrides: &Colors, palette: &[Rgb; PALETTE_LEN]) -> Cell {
    Cell {
        ch: cell.c,
        fg: resolve(cell.fg, overrides, palette),
        bg: resolve(cell.bg, overrides, palette),
        flags: map_flags(cell.flags),
    }
}

/// Resolve a terminal [`Color`] to concrete channels.
///
/// A program-set `overrides` entry wins over the default palette for the same
/// slot, mirroring how a VT terminal lets OSC redefine palette colors.
fn resolve(color: Color, overrides: &Colors, palette: &[Rgb; PALETTE_LEN]) -> Rgb {
    match color {
        Color::Spec(rgb) => Rgb::new(rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => indexed(index as usize, overrides, palette),
        Color::Named(named) => named_color(named, overrides, palette),
    }
}

fn named_color(named: NamedColor, overrides: &Colors, palette: &[Rgb; PALETTE_LEN]) -> Rgb {
    if let Some(rgb) = overrides[named as usize] {
        return Rgb::new(rgb.r, rgb.g, rgb.b);
    }

    match named {
        NamedColor::Background => DEFAULT_BG,
        NamedColor::Foreground | NamedColor::BrightForeground => DEFAULT_FG,
        ansi if (ansi as usize) < PALETTE_LEN => palette[ansi as usize],
        _ => DEFAULT_FG,
    }
}

fn indexed(index: usize, overrides: &Colors, palette: &[Rgb; PALETTE_LEN]) -> Rgb {
    match overrides[index] {
        Some(rgb) => Rgb::new(rgb.r, rgb.g, rgb.b),
        None => palette[index],
    }
}

/// Map the terminal's cell flags to the base attributes stoatty's grid carries.
///
/// Every underline variant (straight, double, curly, dotted, dashed) collapses
/// to the single [`Flags::UNDERLINE`]; distinguishing them is a later item.
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
    if flags.intersects(TermFlags::ALL_UNDERLINES) {
        mapped |= Flags::UNDERLINE;
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

    mapped
}

fn project_cursor(cursor: RenderableCursor, offset: i32) -> Cursor {
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

/// The 16 standard ANSI colors (xterm defaults), palette indices 0..16.
const ANSI_16: [Rgb; 16] = [
    Rgb::new(0x00, 0x00, 0x00),
    Rgb::new(0xcd, 0x00, 0x00),
    Rgb::new(0x00, 0xcd, 0x00),
    Rgb::new(0xcd, 0xcd, 0x00),
    Rgb::new(0x00, 0x00, 0xee),
    Rgb::new(0xcd, 0x00, 0xcd),
    Rgb::new(0x00, 0xcd, 0xcd),
    Rgb::new(0xe5, 0xe5, 0xe5),
    Rgb::new(0x7f, 0x7f, 0x7f),
    Rgb::new(0xff, 0x00, 0x00),
    Rgb::new(0x00, 0xff, 0x00),
    Rgb::new(0xff, 0xff, 0x00),
    Rgb::new(0x5c, 0x5c, 0xff),
    Rgb::new(0xff, 0x00, 0xff),
    Rgb::new(0x00, 0xff, 0xff),
    Rgb::new(0xff, 0xff, 0xff),
];

/// Build the default 256-color xterm palette.
///
/// Indices 0..16 are the ANSI colors, 16..232 the 6x6x6 color cube, and
/// 232..256 the 24-step grayscale ramp.
fn default_palette() -> [Rgb; PALETTE_LEN] {
    let mut palette = [DEFAULT_BG; PALETTE_LEN];
    palette[..16].copy_from_slice(&ANSI_16);

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

#[cfg(test)]
mod tests {
    use super::{Cursor, CursorShape, Terminal};
    use crate::grid::{Cell, Flags, Grid, Rgb};

    fn project(rows: usize, cols: usize, bytes: &[u8]) -> (Grid, Cursor) {
        let mut terminal = Terminal::new(rows, cols);
        let mut grid = Grid::new(rows, cols);

        terminal.advance(bytes);
        let cursor = terminal.project(&mut grid);

        (grid, cursor)
    }

    #[test]
    fn projects_plain_text() {
        let (grid, cursor) = project(2, 4, b"hi");

        assert_eq!(grid.get(0, 0).ch, 'h');
        assert_eq!(grid.get(0, 1).ch, 'i');
        assert_eq!(*grid.get(0, 2), Cell::default());
        assert_eq!(*grid.get(1, 0), Cell::default());
        assert_eq!(
            cursor,
            Cursor {
                row: 0,
                col: 2,
                shape: CursorShape::Block
            }
        );
    }

    #[test]
    fn projects_sgr_color_and_bold() {
        let (grid, _) = project(1, 3, b"\x1b[1;31mX");
        let cell = grid.get(0, 0);

        assert_eq!(cell.ch, 'X');
        assert_eq!(cell.fg, Rgb::new(0xcd, 0x00, 0x00));
        assert!(cell.flags.contains(Flags::BOLD));
    }

    #[test]
    fn projects_background_color() {
        let (grid, _) = project(1, 3, b"\x1b[42mY");

        assert_eq!(grid.get(0, 0).bg, Rgb::new(0x00, 0xcd, 0x00));
    }

    #[test]
    fn projects_indexed_color() {
        let (grid, _) = project(1, 2, b"\x1b[38;5;231mZ");

        assert_eq!(grid.get(0, 0).fg, Rgb::new(0xff, 0xff, 0xff));
    }

    #[test]
    fn projects_cursor_position() {
        let (_, cursor) = project(3, 5, b"\x1b[2;3H");

        assert_eq!(
            cursor,
            Cursor {
                row: 1,
                col: 2,
                shape: CursorShape::Block
            }
        );
    }

    #[test]
    fn project_skips_undamaged_rows() {
        let mut terminal = Terminal::new(3, 4);
        let mut grid = Grid::new(3, 4);

        terminal.advance(b"AB\r\nCD");
        terminal.project(&mut grid);
        assert_eq!(grid.get(1, 1).ch, 'D');

        grid.get_mut(2, 0).ch = 'Z';

        terminal.advance(b"E");
        terminal.project(&mut grid);

        assert_eq!(grid.get(1, 2).ch, 'E');
        assert_eq!(grid.get(2, 0).ch, 'Z');
        assert_eq!(grid.get(0, 0).ch, 'A');
    }

    #[test]
    fn project_resizes_grid_to_terminal() {
        let mut terminal = Terminal::new(2, 6);
        let mut grid = Grid::new(1, 1);

        terminal.advance(b"hello");
        terminal.project(&mut grid);

        assert_eq!((grid.rows(), grid.cols()), (2, 6));
        assert_eq!(grid.get(0, 0).ch, 'h');
    }
}
