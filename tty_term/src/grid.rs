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
    overlays: Vec<Overlay>,
}

impl Grid {
    /// Create a `rows` by `cols` grid filled with [`Cell::default`].
    pub fn new(rows: usize, cols: usize) -> Grid {
        Grid {
            cells: vec![Cell::default(); rows * cols],
            rows,
            cols,
            overlays: Vec::new(),
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
        self.overlays.clear();
    }

    /// The floating overlay regions drawn above the cells, in draw order.
    pub fn overlays(&self) -> &[Overlay] {
        &self.overlays
    }

    /// Replace the floating overlay regions.
    ///
    /// Overlays are grid-level rather than per-cell, so the projection that
    /// rewrites cells leaves them untouched; the caller sets the full list each
    /// frame it changes.
    pub fn set_overlays(&mut self, overlays: Vec<Overlay>) {
        self.overlays = overlays;
    }

    /// Claim a `scale` by `scale` block of cells for a glyph drawn at (`row`,
    /// `col`) scaled by `scale`.
    ///
    /// The origin cell becomes [`Scale::Origin`] and the rest of the block
    /// [`Scale::Covered`]. Cells of the block past the grid edge are skipped, so
    /// a glyph near the boundary claims only what fits. A `scale` below 2 just
    /// marks the origin [`Scale::Single`], since there is no block to claim.
    ///
    /// Only the scale roles are set; the caller writes the origin cell's glyph
    /// and colors separately.
    pub fn place_scaled(&mut self, row: usize, col: usize, scale: u8) {
        if scale < 2 {
            self.get_mut(row, col).scale = Scale::Single;
            return;
        }

        let span = scale as usize;
        for delta_row in 0..span {
            for delta_col in 0..span {
                let (r, c) = (row + delta_row, col + delta_col);
                if r >= self.rows || c >= self.cols {
                    continue;
                }
                self.get_mut(r, c).scale = if delta_row == 0 && delta_col == 0 {
                    Scale::Origin(scale)
                } else {
                    Scale::Covered
                };
            }
        }
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
/// attributes (border edges, popover anchors) are added by later feature items.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub flags: Flags,
    pub underline: UnderlineStyle,
    /// Color the underline is drawn in, independent of [`Self::fg`].
    ///
    /// Defaults to the foreground when the program does not set one (SGR 58),
    /// so an underline with no explicit color matches the text.
    pub underline_color: Rgb,
    pub borders: Borders,
    /// This cell's role in a scaled glyph block.
    ///
    /// [`Scale::Single`] for an ordinary 1x1 cell; the other variants mark the
    /// origin and covered cells of a glyph drawn larger than one cell.
    pub scale: Scale,
}

impl Default for Cell {
    fn default() -> Cell {
        Cell {
            ch: ' ',
            fg: Rgb::new(0xcc, 0xcc, 0xcc),
            bg: Rgb::new(0x00, 0x00, 0x00),
            flags: Flags::empty(),
            underline: UnderlineStyle::None,
            underline_color: Rgb::new(0xcc, 0xcc, 0xcc),
            borders: Borders::default(),
            scale: Scale::Single,
        }
    }
}

/// The renderer-native border on each of a cell's four edges.
///
/// Each edge is independently present or absent. The renderer draws a line
/// along every present edge, so a region framed by setting the perimeter cells'
/// outer edges reads as a panel border without any box-drawing glyphs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Borders {
    pub top: Option<Border>,
    pub right: Option<Border>,
    pub bottom: Option<Border>,
    pub left: Option<Border>,
}

/// A border drawn along one cell edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Border {
    pub style: BorderStyle,
    pub color: Rgb,
}

/// How a cell-edge border is drawn, as renderer primitives rather than glyphs.
///
/// [`BorderStyle::Light`], [`BorderStyle::Heavy`], and [`BorderStyle::Double`]
/// mirror the box-drawing line weights. [`BorderStyle::Rounded`] is a light line
/// whose corners arc where two adjacent edges of a cell meet, so a framed region
/// reads as a panel with rounded corners.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BorderStyle {
    Light,
    Heavy,
    Double,
    Rounded,
}

/// A cell's role in a scaled glyph block.
///
/// A glyph drawn at `n` times the cell size owns an `n` by `n` block of cells.
/// Its top-left cell is [`Scale::Origin`] and carries the glyph; the rest of the
/// block is [`Scale::Covered`] and draws no glyph of its own, so the scaled
/// glyph owns the block without a neighbor drawing into it. Every other cell is
/// [`Scale::Single`].
///
/// See also [`Grid::place_scaled`], which stamps a block.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Scale {
    #[default]
    Single,
    Origin(u8),
    Covered,
}

/// A floating rectangular region drawn above the cells.
///
/// A popover or completion menu composites over the grid with its own z-order
/// rather than living in the cell model. It is anchored at a cell and sized in
/// cells, but is not part of the character grid: it floats above it, occluding
/// whatever cells it covers. The region is a [`Self::fill`] box with a
/// [`Self::border`] outline; text inside it is not part of this type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Overlay {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    pub fill: Rgb,
    pub border: Rgb,
}

/// How a cell's underline is decorated, or [`UnderlineStyle::None`] for no
/// underline.
///
/// Mirrors the standard VT underline styles (SGR `4:1`-`4:5`); the renderer
/// draws each as a distinct shape rather than a glyph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnderlineStyle {
    None,
    Straight,
    Double,
    Curly,
    Dotted,
    Dashed,
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

/// The boolean text-rendering attributes a cell carries simultaneously.
///
/// A compact bitset rather than a struct of bools so a [`Cell`] stays small and
/// `Copy`. Underline is not here: it is a styled, separately-colored decoration,
/// so it rides on [`Cell::underline`] and [`Cell::underline_color`] instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Flags(u8);

impl Flags {
    pub const BOLD: Flags = Flags(0b0000_0001);
    pub const ITALIC: Flags = Flags(0b0000_0010);
    pub const DIM: Flags = Flags(0b0000_0100);
    pub const INVERSE: Flags = Flags(0b0000_1000);
    pub const HIDDEN: Flags = Flags(0b0001_0000);
    pub const STRIKEOUT: Flags = Flags(0b0010_0000);

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
    use super::{Cell, Flags, Grid, Overlay, Rgb, Scale};

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

    #[test]
    fn place_scaled_claims_the_block() {
        let mut grid = Grid::new(3, 3);
        grid.place_scaled(0, 0, 2);

        assert_eq!(grid.get(0, 0).scale, Scale::Origin(2));
        assert_eq!(grid.get(0, 1).scale, Scale::Covered);
        assert_eq!(grid.get(1, 0).scale, Scale::Covered);
        assert_eq!(grid.get(1, 1).scale, Scale::Covered);
        assert_eq!(grid.get(2, 2).scale, Scale::Single, "outside the block");
    }

    #[test]
    fn place_scaled_clamps_at_grid_edge() {
        let mut grid = Grid::new(2, 2);
        grid.place_scaled(1, 1, 2);

        assert_eq!(grid.get(1, 1).scale, Scale::Origin(2));
        assert_eq!(
            grid.get(0, 0).scale,
            Scale::Single,
            "off-block cell untouched"
        );
    }

    #[test]
    fn overlays_round_trip_and_clear_on_resize() {
        let mut grid = Grid::new(2, 2);
        let overlay = Overlay {
            top: 1,
            left: 0,
            width: 3,
            height: 2,
            fill: Rgb::new(10, 20, 30),
            border: Rgb::new(40, 50, 60),
        };
        grid.set_overlays(vec![overlay]);

        assert_eq!(grid.overlays(), [overlay]);

        grid.resize(3, 3);
        assert!(grid.overlays().is_empty(), "resize clears overlays");
    }
}
