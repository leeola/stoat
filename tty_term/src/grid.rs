//! Stoatty's cell grid: the render-facing data model.
//!
//! A [`Grid`] is a rectangular block of [`Cell`]s, each holding one character
//! plus its foreground/background [`Rgb`] and a [`Flags`] attribute set. The
//! renderer reads this grid to draw and the terminal driver writes it; colors
//! are stored fully resolved, so the renderer needs no palette of its own.

use std::ops::{BitOr, BitOrAssign};

/// A rectangular grid of [`Cell`]s addressed by row and column.
///
/// Stoatty's central render model: the terminal driver writes parsed content
/// into it and the renderer reads it to draw. Cells are stored row-major in a
/// single allocation, so [`Self::resize`] reallocates rather than preserving
/// content.
pub struct Grid {
    cells: Vec<Cell>,
    rows: usize,
    cols: usize,
}

impl Grid {
    /// Create a `rows` by `cols` grid filled with [`Cell::default`].
    pub fn new(rows: usize, cols: usize) -> Grid {
        Grid {
            cells: vec![Cell::default(); rows * cols],
            rows,
            cols,
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Borrow the cell at (`row`, `col`).
    ///
    /// Panics if `row` is not less than [`Self::rows`] or `col` is not less
    /// than [`Self::cols`].
    pub fn get(&self, row: usize, col: usize) -> &Cell {
        &self.cells[self.index(row, col)]
    }

    /// Mutably borrow the cell at (`row`, `col`).
    ///
    /// Panics if `row` is not less than [`Self::rows`] or `col` is not less
    /// than [`Self::cols`].
    pub fn get_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        let index = self.index(row, col);
        &mut self.cells[index]
    }

    /// Resize to `rows` by `cols`, resetting every cell to [`Cell::default`].
    ///
    /// Content is not preserved; the driver repopulates the grid afterward.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows;
        self.cols = cols;
        self.cells.clear();
        self.cells.resize(rows * cols, Cell::default());
    }

    /// Map a (`row`, `col`) coordinate to its row-major index.
    ///
    /// Bounds-checks both axes so an out-of-range column cannot silently
    /// resolve to a valid index in another row.
    fn index(&self, row: usize, col: usize) -> usize {
        assert!(
            row < self.rows && col < self.cols,
            "cell ({row}, {col}) out of bounds for {}x{} grid",
            self.rows,
            self.cols,
        );
        row * self.cols + col
    }
}

/// A single grid cell: one character and how to render it.
///
/// The base attribute set every cell carries. stoatty-specific per-cell
/// attributes (border edges, glyph scale, popover anchors) and underline
/// styling are added by later feature items.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub flags: Flags,
}

impl Default for Cell {
    fn default() -> Cell {
        Cell {
            ch: ' ',
            fg: Rgb::new(0xcc, 0xcc, 0xcc),
            bg: Rgb::new(0x00, 0x00, 0x00),
            flags: Flags::empty(),
        }
    }
}

/// A fully-resolved 24-bit color.
///
/// The grid stores resolved colors rather than terminal-palette references:
/// named and indexed colors are resolved upstream when the driver projects
/// parsed content onto the grid, so the renderer consumes concrete channels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }
}

/// The text-rendering attributes a cell carries simultaneously.
///
/// A compact bitset rather than a struct of bools so a [`Cell`] stays small
/// and `Copy`. Holds the base SGR attributes only; underline styles (curly,
/// dotted, and the like) and stoatty decoration are layered on by later items.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Flags(u8);

impl Flags {
    pub const BOLD: Flags = Flags(0b0000_0001);
    pub const ITALIC: Flags = Flags(0b0000_0010);
    pub const DIM: Flags = Flags(0b0000_0100);
    pub const UNDERLINE: Flags = Flags(0b0000_1000);
    pub const INVERSE: Flags = Flags(0b0001_0000);
    pub const HIDDEN: Flags = Flags(0b0010_0000);
    pub const STRIKEOUT: Flags = Flags(0b0100_0000);

    /// The empty set, carrying no attributes.
    pub const fn empty() -> Flags {
        Flags(0)
    }

    /// Whether every attribute in `other` is also set in `self`.
    pub const fn contains(self, other: Flags) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl BitOr for Flags {
    type Output = Flags;

    fn bitor(self, rhs: Flags) -> Flags {
        Flags(self.0 | rhs.0)
    }
}

impl BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Flags) {
        self.0 |= rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use super::{Cell, Flags, Grid, Rgb};

    #[test]
    fn grid_writes_are_addressable() {
        let mut grid = Grid::new(2, 3);
        assert_eq!((grid.rows(), grid.cols()), (2, 3));

        grid.get_mut(1, 2).ch = 'x';
        grid.get_mut(0, 0).fg = Rgb::new(1, 2, 3);

        assert_eq!(grid.get(1, 2).ch, 'x');
        assert_eq!(grid.get(0, 0).fg, Rgb::new(1, 2, 3));
        assert_eq!(*grid.get(0, 1), Cell::default());
    }

    #[test]
    fn resize_resets_cells_to_default() {
        let mut grid = Grid::new(1, 1);
        grid.get_mut(0, 0).ch = 'z';

        grid.resize(3, 4);

        assert_eq!((grid.rows(), grid.cols()), (3, 4));
        assert_eq!(*grid.get(2, 3), Cell::default());
    }

    #[test]
    fn flags_combine_and_query() {
        let styled = Flags::BOLD | Flags::ITALIC;

        assert!(styled.contains(Flags::BOLD));
        assert!(styled.contains(Flags::ITALIC));
        assert!(!styled.contains(Flags::DIM));
        assert!(!Flags::empty().contains(Flags::BOLD));
    }

    #[test]
    #[should_panic]
    fn out_of_bounds_access_panics() {
        let grid = Grid::new(2, 2);
        let _ = grid.get(2, 0);
    }
}
