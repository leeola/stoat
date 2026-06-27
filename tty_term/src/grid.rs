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
    scroll_region: Option<ScrollRegion>,
    icons: Vec<Icon>,
    text_runs: Vec<TextRun>,
    bars: Vec<Bar>,
    /// Height in rows of each logical line, indexed from the top. A line absent
    /// from the vec is one row tall. The prefix sum gives a line's physical
    /// start row, so an inline expansion pushes later lines down.
    line_heights: Vec<u16>,
}

impl Grid {
    /// Create a `rows` by `cols` grid filled with [`Cell::default`].
    pub fn new(rows: usize, cols: usize) -> Grid {
        Grid {
            cells: vec![Cell::default(); rows * cols],
            rows,
            cols,
            overlays: Vec::new(),
            scroll_region: None,
            icons: Vec::new(),
            text_runs: Vec::new(),
            bars: Vec::new(),
            line_heights: Vec::new(),
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

    /// Borrow row `row` as a contiguous slice of its cells.
    ///
    /// Panics if `row` is not less than [`Self::rows`].
    pub fn row(&self, row: usize) -> &[Cell] {
        assert!(
            row < self.rows,
            "row {row} out of bounds for {} rows",
            self.rows
        );
        &self.cells[row * self.cols..(row + 1) * self.cols]
    }

    /// Mutably borrow row `row` as a contiguous slice of its cells.
    ///
    /// Panics if `row` is not less than [`Self::rows`].
    pub fn row_mut(&mut self, row: usize) -> &mut [Cell] {
        assert!(
            row < self.rows,
            "row {row} out of bounds for {} rows",
            self.rows
        );
        &mut self.cells[row * self.cols..(row + 1) * self.cols]
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
        self.scroll_region = None;
        self.icons.clear();
        self.text_runs.clear();
        self.bars.clear();
        self.line_heights.clear();
    }

    /// Reset every cell to [`Cell::default`] and drop all decorations, keeping
    /// the current dimensions.
    ///
    /// Unlike [`Self::resize`], the cell buffer is cleared in place rather than
    /// reallocated, so recycling a grid to hold new content allocates nothing.
    fn clear(&mut self) {
        self.cells.fill(Cell::default());
        self.overlays.clear();
        self.scroll_region = None;
        self.icons.clear();
        self.text_runs.clear();
        self.bars.clear();
        self.line_heights.clear();
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

    /// The scrollable sub-rectangle, or `None` when no region is declared.
    pub fn scroll_region(&self) -> Option<ScrollRegion> {
        self.scroll_region
    }

    /// Replace the scrollable sub-rectangle.
    ///
    /// Grid-level like the overlays, so the per-cell projection leaves it
    /// untouched; the caller sets it each frame it changes. A region's scroll
    /// offset updates over time, so the latest value replaces the prior one
    /// rather than accumulating.
    pub fn set_scroll_region(&mut self, region: Option<ScrollRegion>) {
        self.scroll_region = region;
    }

    /// The status icons drawn above the cells, in draw order.
    pub fn icons(&self) -> &[Icon] {
        &self.icons
    }

    /// Replace the status icons.
    ///
    /// Grid-level like the overlays, so the per-cell projection leaves them
    /// untouched; the caller sets the full list each frame it changes.
    pub fn set_icons(&mut self, icons: Vec<Icon>) {
        self.icons = icons;
    }

    /// The off-grid text runs drawn above the cells, in draw order.
    pub fn text_runs(&self) -> &[TextRun] {
        &self.text_runs
    }

    /// Replace the off-grid text runs.
    ///
    /// Grid-level like the overlays, so the per-cell projection leaves them
    /// untouched; the caller sets the full list each frame it changes.
    pub fn set_text_runs(&mut self, text_runs: Vec<TextRun>) {
        self.text_runs = text_runs;
    }

    /// The off-grid color bars drawn above the cells, in draw order.
    pub fn bars(&self) -> &[Bar] {
        &self.bars
    }

    /// Replace the off-grid color bars.
    ///
    /// Grid-level like the overlays, so the per-cell projection leaves them
    /// untouched; the caller sets the full list each frame it changes.
    pub fn set_bars(&mut self, bars: Vec<Bar>) {
        self.bars = bars;
    }

    /// Replace the per-logical-line heights, in rows, indexed from the top.
    ///
    /// A line past the end of the list is one row tall. The cell projection is
    /// unaffected; the layout exists for off-grid components to align to.
    pub fn set_line_heights(&mut self, line_heights: Vec<u16>) {
        self.line_heights = line_heights;
    }

    /// The physical row a logical line starts on: the sum of the heights of the
    /// lines above it, with any line past the declared heights counting as one
    /// row. With no expansions this is `line` itself.
    pub fn line_start_row(&self, line: usize) -> usize {
        if self.line_heights.is_empty() {
            return line;
        }
        (0..line)
            .map(|above| self.line_heights.get(above).copied().unwrap_or(1) as usize)
            .sum()
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

/// A bounded, recycled pool of viewport-sized content pages for smooth
/// scrolling.
///
/// The app owns its scroll position and pushes a window of rich pages around
/// the scroll target into this pool, each page a viewport's worth of rows --
/// cells plus their APC decorations -- keyed by the app's document page index.
/// The renderer reads the visible region from the pool at the live scroll
/// offset, drawing the buffered neighbour pages that straddle the viewport
/// edges during a partial-cell scroll.
///
/// Pages map to fixed slots by `index % capacity`, so a contiguous window of up
/// to `capacity` pages fills every slot, and sliding the window one page reuses
/// the slot the departed page vacated for the page entering it -- steady-state
/// scrolling allocates nothing.
///
/// Distinct from the viewport-only projected [`Grid`]: the pool holds several
/// pages of off-screen content, not just what is on screen.
pub struct PagePool {
    pages: Vec<Page>,
}

impl PagePool {
    /// Create a pool of `capacity` viewport-sized pages, clamped to at least
    /// one.
    ///
    /// Pages start empty: [`Self::page`] returns `None` until [`Self::fill`]
    /// populates them.
    pub fn new(rows: usize, cols: usize, capacity: usize) -> PagePool {
        let pages = (0..capacity.max(1))
            .map(|_| Page {
                index: None,
                grid: Grid::new(rows, cols),
            })
            .collect();
        PagePool { pages }
    }

    /// Recycle the slot for document page `index` and return its cleared grid
    /// for the caller to write the page's content into.
    ///
    /// [`Self::page`] resolves `index` to this grid afterward. If the slot held
    /// a different page, that page is dropped and its buffer reused in place, so
    /// a sliding window allocates nothing.
    pub fn fill(&mut self, index: u64) -> &mut Grid {
        let slot = self.slot(index);
        let page = &mut self.pages[slot];
        page.index = Some(index);
        page.grid.clear();
        &mut page.grid
    }

    /// The buffered grid for document page `index`, or `None` when that page is
    /// not currently in the pool's window.
    pub fn page(&self, index: u64) -> Option<&Grid> {
        let page = &self.pages[self.slot(index)];
        (page.index == Some(index)).then_some(&page.grid)
    }

    /// Compose the visible region into `out`, sourcing each row from the pooled
    /// page that holds it, starting at document row `top`.
    ///
    /// Output row `r` is document row `top + r`, which lives in page
    /// `(top + r) / page_rows` at row `(top + r) % page_rows`, so a viewport that
    /// straddles a page boundary draws the neighbour pages on either side. `out`
    /// is sized by the caller to the viewport plus one straddle row, so an upward
    /// fractional shift has a row to reveal at the bottom.
    ///
    /// Returns `false` the moment a needed page is not buffered, or `top` falls
    /// above the first page; `out` is left partly written, so the caller falls
    /// back to the live grid rather than show holes.
    pub fn compose(&self, top: i64, out: &mut Grid) -> bool {
        let page_rows = match self.pages.first() {
            Some(page) if page.grid.rows() > 0 => page.grid.rows(),
            _ => return false,
        };

        for out_row in 0..out.rows() {
            let doc_row = top + out_row as i64;
            if doc_row < 0 {
                return false;
            }

            let page_index = doc_row as u64 / page_rows as u64;
            let row_in_page = doc_row as usize % page_rows;
            let Some(page) = self.page(page_index) else {
                return false;
            };

            let cols = out.cols().min(page.cols());
            out.row_mut(out_row)[..cols].copy_from_slice(&page.row(row_in_page)[..cols]);
        }

        true
    }

    /// Resize every page to a `rows` by `cols` viewport, dropping all buffered
    /// content.
    ///
    /// Called when a resize or font-zoom changes the viewport's row count,
    /// since pages are sized to the live viewport. The window is emptied; the
    /// app refills it for the new size.
    pub fn rebuild(&mut self, rows: usize, cols: usize) {
        for page in &mut self.pages {
            page.index = None;
            page.grid.resize(rows, cols);
        }
    }

    /// The slot document page `index` maps to, modulo the pool capacity.
    fn slot(&self, index: u64) -> usize {
        (index % self.pages.len() as u64) as usize
    }
}

/// A smooth-scroll position in document-page space.
///
/// `page` is a [`PagePool`] document page index and `fraction` is the sub-page
/// position within it, in [0, 1). The renderer eases the live offset toward an
/// app-declared target of this shape and reads the visible region from the pool
/// at the eased position, so a partial-page scroll draws the buffered neighbour
/// pages straddling the viewport edges.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct DocumentOffset {
    pub page: u64,
    pub fraction: f32,
}

impl DocumentOffset {
    /// The offset as a single value in page units (`page + fraction`), for
    /// easing and for mapping onto pool pages.
    pub fn pages(&self) -> f32 {
        self.page as f32 + self.fraction
    }
}

/// One slot of a [`PagePool`]: a viewport-sized grid tagged with the document
/// page it currently holds.
///
/// `index` is `None` for an empty slot and `Some(page)` once filled, so a
/// lookup can tell a slot holding the requested page from a stale or empty one.
/// The grid is reused in place as the slot recycles, so its allocation persists
/// across pages.
struct Page {
    index: Option<u64>,
    grid: Grid,
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

impl Cell {
    /// The foreground and background colors to draw this cell with, as
    /// `(fg, bg)`.
    ///
    /// When [`Flags::INVERSE`] is set the pair is swapped, so a cell that asked
    /// for reverse video paints its background color as text over its
    /// foreground color. Render passes draw with this pair rather than reading
    /// [`Self::fg`] and [`Self::bg`] directly, which is what makes a
    /// reverse-video cell (such as the editor's block cursor) visible.
    pub fn draw_colors(&self) -> (Rgb, Rgb) {
        if self.flags.contains(Flags::INVERSE) {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        }
    }
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
/// [`Self::border`] outline.
///
/// [`Self::content`] is a line of text drawn inside the box in
/// [`Self::content_fg`], drawn at [`Self::scale`] times the cell size from the
/// box's top-left, clipped to the box.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Overlay {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    pub fill: Rgb,
    pub border: Rgb,
    pub content_fg: Rgb,
    /// Integer multiple of the cell size the content text is drawn at, so a
    /// popover can render larger or smaller than the grid. The box itself stays
    /// at the cell size; only the content scales.
    pub scale: u8,
    /// Signed `[x, y]` pixel offset from the anchor cell, so the popover can sit
    /// at a sub-cell position. The box, its shadow, its content, and the content
    /// clip all shift by this offset.
    pub offset: [i16; 2],
    pub content: String,
}

/// A scrollable sub-rectangle of the grid.
///
/// The cells inside the `width` by `height` rectangle anchored at (`top`,
/// `left`) scroll on their own [`Self::offset`] while the rest of the grid stays
/// fixed. The region carries no content of its own: it scopes the scroll of the
/// grid cells it covers, the renderer shifting those cells by the eased offset
/// and clipping them to the rectangle.
///
/// [`Self::offset`] is the region's scroll position in rows. It is an absolute
/// position rather than a delta, so a change between frames is what the renderer
/// animates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrollRegion {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    pub offset: u16,
}

impl ScrollRegion {
    /// Whether the cell at (`row`, `col`) falls within the region's rectangle.
    pub fn contains(&self, row: usize, col: usize) -> bool {
        let top = self.top as usize;
        let left = self.left as usize;
        row >= top
            && row < top + self.height as usize
            && col >= left
            && col < left + self.width as usize
    }
}

/// A fixed renderer-drawn status icon composited above the cells.
///
/// Like an [`Overlay`], it is grid-level rather than a cell attribute: the
/// renderer draws the [`IconKind`] silhouette in [`Self::color`] as a
/// signed-distance shape over a [`Self::size`]-by-[`Self::size`] cell block
/// anchored at (`top`, `left`), rather than from a font or image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Icon {
    pub top: u16,
    pub left: u16,
    pub kind: IconKind,
    pub color: Rgb,
    pub size: u8,
}

/// Which status icon an [`Icon`] draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconKind {
    Error,
    Warning,
    Info,
}

/// A run of text drawn off the cell grid at a fractional scale.
///
/// Like an [`Overlay`] it is grid-level, not a cell attribute: the renderer
/// draws it above the cells so a non-cell component (a gutter line number) can
/// render smaller than the grid yet line up with full-size rows. [`Self::col`]
/// and [`Self::row`] are the anchor in sixteenths of a cell (16 = one cell), so
/// the run can sit at a fractional position; [`Self::scale`] is the glyph size
/// in 256ths of the cell size (256 = grid size). The run advances one scaled
/// cell width per character and is vertically centered within the target row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TextRun {
    pub col: i16,
    pub row: i16,
    pub scale: u16,
    pub color: Rgb,
    pub bg: Rgb,
    pub text: String,
}

/// A thin rectangle filled off the cell grid in a solid color.
///
/// Like an [`Overlay`] it is grid-level, not a cell attribute: a non-cell
/// component (a gutter) packs several variable-width status bars and a hairline
/// separator into a fraction of a cell. [`Self::x`] and [`Self::width`] run
/// along the cell width, [`Self::y`] and [`Self::height`] along the cell height,
/// all in sixteenths of a cell (16 = one cell), so a bar can be a fraction of a
/// cell wide.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bar {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub color: Rgb,
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
    use super::{
        Bar, Cell, Flags, Grid, Icon, IconKind, Overlay, PagePool, Rgb, Scale, ScrollRegion,
        TextRun,
    };

    #[test]
    fn draw_colors_swaps_only_under_inverse() {
        let fg = Rgb::new(10, 20, 30);
        let bg = Rgb::new(40, 50, 60);
        let cell = Cell {
            fg,
            bg,
            ..Cell::default()
        };
        assert_eq!(cell.draw_colors(), (fg, bg));

        let inverse = Cell {
            flags: Flags::INVERSE,
            ..cell
        };
        assert_eq!(inverse.draw_colors(), (bg, fg));
    }

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
            content_fg: Rgb::new(70, 80, 90),
            scale: 1,
            offset: [0, 0],
            content: "hi".to_owned(),
        };
        grid.set_overlays(vec![overlay.clone()]);

        assert_eq!(grid.overlays(), [overlay]);

        grid.resize(3, 3);
        assert!(grid.overlays().is_empty(), "resize clears overlays");
    }

    #[test]
    fn scroll_region_round_trips_and_clears_on_resize() {
        let mut grid = Grid::new(4, 4);
        let region = ScrollRegion {
            top: 1,
            left: 2,
            width: 2,
            height: 2,
            offset: 5,
        };
        grid.set_scroll_region(Some(region));

        assert_eq!(grid.scroll_region(), Some(region));

        grid.resize(2, 2);
        assert_eq!(
            grid.scroll_region(),
            None,
            "resize clears the scroll region"
        );
    }

    #[test]
    fn icons_round_trip_and_clear_on_resize() {
        let mut grid = Grid::new(4, 4);
        let icon = Icon {
            top: 2,
            left: 0,
            kind: IconKind::Error,
            color: Rgb::new(220, 50, 47),
            size: 1,
        };
        grid.set_icons(vec![icon]);

        assert_eq!(grid.icons(), [icon]);

        grid.resize(2, 2);
        assert!(grid.icons().is_empty(), "resize clears the icons");
    }

    #[test]
    fn text_runs_round_trip_and_clear_on_resize() {
        let mut grid = Grid::new(4, 4);
        let run = TextRun {
            col: 0,
            row: 32,
            scale: 192,
            color: Rgb::new(150, 160, 170),
            bg: Rgb::new(24, 26, 32),
            text: "42".to_owned(),
        };
        grid.set_text_runs(vec![run.clone()]);

        assert_eq!(grid.text_runs(), [run]);

        grid.resize(2, 2);
        assert!(grid.text_runs().is_empty(), "resize clears the text runs");
    }

    #[test]
    fn bars_round_trip_and_clear_on_resize() {
        let mut grid = Grid::new(4, 4);
        let bar = Bar {
            x: 0,
            y: 16,
            width: 3,
            height: 16,
            color: Rgb::new(220, 50, 47),
        };
        grid.set_bars(vec![bar]);

        assert_eq!(grid.bars(), [bar]);

        grid.resize(2, 2);
        assert!(grid.bars().is_empty(), "resize clears the bars");
    }

    #[test]
    fn line_start_row_is_the_prefix_sum_of_heights() {
        let mut grid = Grid::new(8, 8);

        // With no declared heights every line is one row, so the start row is
        // the line index.
        assert_eq!(grid.line_start_row(0), 0);
        assert_eq!(grid.line_start_row(3), 3);

        // Line 1 is three rows tall, so it adds two rows to every later line,
        // while lines past the declared heights stay one row.
        grid.set_line_heights(vec![1, 3, 1]);
        assert_eq!(grid.line_start_row(1), 1, "the expanded line itself");
        assert_eq!(grid.line_start_row(2), 4, "shifted past the expansion");
        assert_eq!(grid.line_start_row(4), 6, "undeclared lines count as one");

        grid.resize(2, 2);
        assert_eq!(grid.line_start_row(3), 3, "resize clears the layout");
    }

    #[test]
    fn scroll_region_contains_its_rectangle_only() {
        let region = ScrollRegion {
            top: 1,
            left: 2,
            width: 2,
            height: 3,
            offset: 0,
        };

        assert!(region.contains(1, 2), "top-left corner");
        assert!(region.contains(3, 3), "bottom-right corner");
        assert!(!region.contains(0, 2), "row above");
        assert!(!region.contains(4, 2), "row below");
        assert!(!region.contains(1, 1), "column left");
        assert!(!region.contains(1, 4), "column right");
    }

    #[test]
    fn page_pool_fills_and_looks_up_by_index() {
        let mut pool = PagePool::new(2, 3, 4);
        assert!(pool.page(0).is_none(), "an unfilled pool has no pages");

        pool.fill(7).get_mut(1, 2).ch = 'Z';

        assert_eq!(pool.page(7).map(|g| g.get(1, 2).ch), Some('Z'));
        assert!(
            pool.page(3).is_none(),
            "index 3 shares a slot with 7, which holds it"
        );
    }

    #[test]
    fn page_pool_recycles_the_slot_a_slid_page_vacated() {
        let mut pool = PagePool::new(2, 2, 2);
        pool.fill(0).get_mut(0, 0).ch = 'a';
        pool.fill(1).get_mut(0, 0).ch = 'b';

        // Index 2 maps to index 0's slot (2 % 2 == 0), so it recycles 0's
        // buffer in place.
        let recycled = pool.fill(2);
        assert_eq!(recycled.get(0, 0).ch, ' ', "the recycled buffer is cleared");
        assert_eq!(
            (recycled.rows(), recycled.cols()),
            (2, 2),
            "recycling keeps the page size"
        );

        assert!(pool.page(0).is_none(), "the slid-out page is gone");
        assert_eq!(
            pool.page(1).map(|g| g.get(0, 0).ch),
            Some('b'),
            "the neighbour page is untouched"
        );
        assert!(pool.page(2).is_some(), "the entering page is present");
    }

    #[test]
    fn page_pool_clears_decorations_on_recycle() {
        let mut pool = PagePool::new(1, 1, 1);
        pool.fill(0).set_icons(vec![Icon {
            top: 0,
            left: 0,
            kind: IconKind::Error,
            color: Rgb::new(1, 2, 3),
            size: 1,
        }]);

        assert!(
            pool.fill(1).icons().is_empty(),
            "recycling drops the prior page's decorations"
        );
    }

    #[test]
    fn page_pool_rebuild_resizes_pages_and_drops_content() {
        let mut pool = PagePool::new(2, 2, 2);
        pool.fill(0);

        pool.rebuild(3, 5);

        assert!(pool.page(0).is_none(), "rebuild drops buffered pages");
        let page = pool.fill(0);
        assert_eq!(
            (page.rows(), page.cols()),
            (3, 5),
            "pages track the new viewport"
        );
    }

    #[test]
    fn page_pool_capacity_is_at_least_one() {
        let mut pool = PagePool::new(1, 1, 0);
        pool.fill(0).get_mut(0, 0).ch = 'x';
        assert_eq!(
            pool.page(0).map(|g| g.get(0, 0).ch),
            Some('x'),
            "a zero-capacity request still yields a usable slot"
        );
    }

    fn fill_page_rows(pool: &mut PagePool, index: u64, rows: &[char]) {
        let grid = pool.fill(index);
        for (row, &ch) in rows.iter().enumerate() {
            grid.get_mut(row, 0).ch = ch;
        }
    }

    fn composed_rows(out: &Grid) -> Vec<char> {
        (0..out.rows()).map(|row| out.get(row, 0).ch).collect()
    }

    #[test]
    fn compose_aligned_top_reads_one_page() {
        let mut pool = PagePool::new(2, 1, 4);
        fill_page_rows(&mut pool, 0, &['a', 'b']);
        fill_page_rows(&mut pool, 1, &['c', 'd']);

        let mut out = Grid::new(2, 1);
        assert!(pool.compose(0, &mut out));
        assert_eq!(composed_rows(&out), ['a', 'b']);
    }

    #[test]
    fn compose_straddles_a_page_boundary() {
        let mut pool = PagePool::new(2, 1, 4);
        fill_page_rows(&mut pool, 0, &['a', 'b']);
        fill_page_rows(&mut pool, 1, &['c', 'd']);

        // top=1 reads page 0's second row, then both of page 1's rows.
        let mut out = Grid::new(3, 1);
        assert!(pool.compose(1, &mut out));
        assert_eq!(composed_rows(&out), ['b', 'c', 'd']);
    }

    #[test]
    fn compose_fails_when_a_straddled_page_is_unbuffered() {
        let mut pool = PagePool::new(2, 1, 4);
        fill_page_rows(&mut pool, 0, &['a', 'b']);

        // out needs page 0's last row plus page 1, which was never filled.
        let mut out = Grid::new(3, 1);
        assert!(!pool.compose(1, &mut out));
    }

    #[test]
    fn compose_fails_above_the_first_page() {
        let mut pool = PagePool::new(2, 1, 4);
        fill_page_rows(&mut pool, 0, &['a', 'b']);

        let mut out = Grid::new(2, 1);
        assert!(!pool.compose(-1, &mut out));
    }
}
