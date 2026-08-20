//! Stoatty's cell grid: the render-facing data model.
//!
//! A [`Grid`] is a rectangular block of [`Cell`]s, each holding one character
//! plus its foreground/background [`Rgb`] and a [`Flags`] attribute set. The
//! renderer reads this grid to draw and the terminal driver writes it; colors
//! are stored fully resolved, so the renderer needs no palette of its own.

use from_command::{bar_from_command, fill_polyline, text_run_from_command, StoredTextRun};
use rustc_hash::FxHasher;
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    mem,
    ops::{BitOr, BitOrAssign, Range},
    sync::Arc,
};
use stoatty_protocol::command::{BarCommand, PolylineCommand, TextRunCommand};

pub(crate) mod from_command;

/// A minimap line's run summary, re-exported so a consumer of
/// [`Grid::minimap_content`] has a name for what it gets back.
///
/// Not projected into a grid type like the colors around it, because a run
/// carries nothing wire-specific to project. It is a start column, a length,
/// and a palette class, which is what the renderer reads either way.
pub use stoatty_protocol::command::{LineSummary, MinimapRun};

/// A rectangular grid of [`Cell`]s addressed by row and column.
///
/// Stoatty's central render model: the terminal driver writes parsed content
/// into it and the renderer reads it to draw. Cells are stored row-major in a
/// single allocation, so [`Self::resize`] reallocates rather than preserving
/// content.
pub struct Grid {
    cells: Vec<Cell>,
    /// The distinct border sets this grid's cells refer to, indexed by
    /// [`BorderId`] less one. The borderless set is [`BorderId::NONE`] and is
    /// not stored, so this holds only sets some cell actually carries.
    border_table: Vec<Borders>,
    rows: usize,
    cols: usize,
    overlays: Vec<Overlay>,
    panels: Vec<Panel>,
    scroll_region: Option<ScrollRegion>,
    icons: Vec<Icon>,
    text_runs: Vec<TextRun>,
    bars: Vec<Bar>,
    polylines: Vec<Polyline>,
    /// Declared minimap strips, each joined with its viewport thumb. Kept apart
    /// from [`Self::minimap_contents`] so a viewport-only change re-projects this
    /// small list without re-cloning the line summaries.
    minimaps: Vec<Minimap>,
    /// Minimap line-summary stores keyed by content id, the whole-buffer run
    /// blocks a strip renders. A strip finds its store via its
    /// [`MinimapCommand::content_id`].
    minimap_contents: HashMap<u32, Vec<LineSummary>>,
    /// Physical start row of each logical line, indexed from the top, so an
    /// inline expansion pushes later lines down.
    ///
    /// The running sum of the declared heights rather than the heights
    /// themselves, since summing them per lookup makes a re-stamp quadratic in
    /// the lines on screen. It runs one longer than the heights it was built
    /// from, the last entry being the total, which is what a line past the end
    /// extrapolates from. Empty while no layout is declared, where every line
    /// is one row and starts on its own index.
    line_start_rows: Vec<usize>,
    /// Change counters bumped by [`crate::Terminal::project`] when it re-applies
    /// the overlay/popover, off-grid text-run, or minimap decorations. A render
    /// pass compares each against its last-seen value to skip rebuilding and
    /// re-uploading a decoration that did not change. Monotonic across resizes,
    /// since a resize re-applies every decoration and so bumps all three.
    popovers_epoch: u64,
    text_runs_epoch: u64,
    minimap_epoch: u64,
    /// Images placed on this grid, in declaration order.
    ///
    /// Held apart from the cells because a placement covers a box of them and
    /// draws from its own pixels, so nothing about it fits in a cell.
    images: Vec<PlacedImage>,
    /// Change counter for [`Self::images`], read like [`Self::popovers_epoch`].
    images_epoch: u64,
}

impl Grid {
    /// Create a `rows` by `cols` grid filled with [`Cell::default`].
    pub fn new(rows: usize, cols: usize) -> Grid {
        Grid {
            cells: vec![Cell::default(); rows * cols],
            border_table: Vec::new(),
            rows,
            cols,
            overlays: Vec::new(),
            panels: Vec::new(),
            scroll_region: None,
            icons: Vec::new(),
            text_runs: Vec::new(),
            bars: Vec::new(),
            polylines: Vec::new(),
            minimaps: Vec::new(),
            minimap_contents: HashMap::new(),
            line_start_rows: Vec::new(),
            popovers_epoch: 0,
            text_runs_epoch: 0,
            minimap_epoch: 0,
            images: Vec::new(),
            images_epoch: 0,
        }
    }

    /// Change counter for the overlay/popover decorations, moved by every entry
    /// point that replaces the list.
    ///
    /// A renderer caches shaped overlay content against this, so a list that
    /// changed without moving the counter leaves the stale content drawn. The
    /// counter is maintained here rather than by the caller replacing the list,
    /// because a caller that forgets is invisible until something paints wrong.
    pub fn popovers_epoch(&self) -> u64 {
        self.popovers_epoch
    }

    /// Change counter for the off-grid text-run decorations, moved by every
    /// entry point that replaces the list.
    ///
    /// Read like [`Self::popovers_epoch`], and maintained here for the same
    /// reason.
    pub fn text_runs_epoch(&self) -> u64 {
        self.text_runs_epoch
    }

    /// Change counter for the minimap decorations, covering both the strip list
    /// and the line-summary content stores.
    ///
    /// Read like [`Self::popovers_epoch`], and maintained here for the same
    /// reason.
    pub fn minimap_epoch(&self) -> u64 {
        self.minimap_epoch
    }

    /// Images placed on this grid, in declaration order.
    pub fn images(&self) -> &[PlacedImage] {
        &self.images
    }

    /// Change counter for [`Self::images`], moved by every replacement.
    ///
    /// Read like [`Self::popovers_epoch`], and maintained here for the same
    /// reason.
    pub fn images_epoch(&self) -> u64 {
        self.images_epoch
    }

    /// Replace the placed images, moving [`Self::images_epoch`].
    pub fn set_images(&mut self, images: Vec<PlacedImage>) {
        self.images = images;
        self.images_epoch += 1;
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

    /// Move every row by `rows`, up the screen when positive and down when
    /// negative, blanking the rows the move vacates.
    ///
    /// A terminal scroll moves the screen's content without changing it, so a
    /// projector can slide the grid to match and then rewrite only the rows that
    /// really differ. Live output moves content up. Gliding back through
    /// scrollback moves it down. A move of at least the height keeps nothing and
    /// blanks the grid.
    pub fn scroll_by(&mut self, rows: isize) {
        if rows == 0 {
            return;
        }
        let magnitude = rows.unsigned_abs();
        if magnitude >= self.rows {
            self.cells.fill(Cell::default());
            return;
        }

        // Copied rather than rotated. A rotation would carry the vacated rows
        // to the far end for the fill below to overwrite, which at a full
        // terminal is a screen's worth of cells moved for nothing.
        let moved = magnitude * self.cols;
        let kept = self.cells.len() - moved;
        if rows > 0 {
            self.cells.copy_within(moved.., 0);
            self.cells[kept..].fill(Cell::default());
        } else {
            self.cells.copy_within(..kept, moved);
            self.cells[..moved].fill(Cell::default());
        }
    }

    /// Resize to `rows` by `cols`, resetting every cell to [`Cell::default`].
    ///
    /// Content is not preserved. The driver repopulates the grid afterward.
    ///
    /// Moves every decoration epoch, since dropping those lists is as much a
    /// change as replacing them.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows;
        self.cols = cols;
        self.cells.clear();
        self.cells.resize(rows * cols, Cell::default());
        self.border_table.clear();
        self.overlays.clear();
        self.scroll_region = None;
        self.icons.clear();
        self.text_runs.clear();
        self.bars.clear();
        self.polylines.clear();
        self.minimaps.clear();
        self.minimap_contents.clear();
        self.line_start_rows.clear();
        self.images.clear();

        self.popovers_epoch += 1;
        self.text_runs_epoch += 1;
        self.minimap_epoch += 1;
        self.images_epoch += 1;
    }

    /// Reset every cell to [`Cell::default`] and drop all decorations, keeping
    /// the current dimensions.
    ///
    /// Unlike [`Self::resize`], the cell buffer is cleared in place rather than
    /// reallocated, so recycling a grid to hold new content allocates nothing.
    ///
    /// Moves the three decoration epochs, as [`Self::resize`] does.
    pub fn clear(&mut self) {
        self.clear_except(0, 0);
    }

    /// Reset everything the grid holds except its cells.
    ///
    /// For a grid recomposed in place, where the cells are overwritten by the
    /// compose and only what sits beside them has to go. The decorations are
    /// the reason: a compose that adds each source's runs and bars to this
    /// grid's would otherwise stack this pass's on the last one's.
    ///
    /// Moves the three decoration epochs, as [`Self::clear`] does, so a render
    /// pass gating on them rebuilds a decoration that changed however the cells
    /// compare.
    pub fn clear_decorations(&mut self) {
        self.clear_except(self.rows, self.cols);
    }

    /// Reset the grid, leaving the cells in the top-left `rows` by `cols` box
    /// as they are.
    ///
    /// A caller that recycles a grid by painting a box over it writes those
    /// cells twice, once to zero them and once to paint them. Naming the box
    /// leaves them to the paint. The box clamps to the grid, so naming one
    /// larger than the grid keeps every cell.
    ///
    /// Everything that is not a cell goes regardless. A cell write replaces
    /// none of it, so the border table, the decorations, the minimap stores,
    /// and the line starts are reset whatever box the caller names.
    fn clear_except(&mut self, rows: usize, cols: usize) {
        let kept_rows = rows.min(self.rows);
        let kept_cols = cols.min(self.cols);
        for row in 0..kept_rows {
            self.row_mut(row)[kept_cols..].fill(Cell::default());
        }
        self.cells[kept_rows * self.cols..].fill(Cell::default());

        self.border_table.clear();
        self.overlays.clear();
        self.scroll_region = None;
        self.icons.clear();
        self.text_runs.clear();
        self.bars.clear();
        self.polylines.clear();
        self.minimaps.clear();
        self.minimap_contents.clear();
        self.line_start_rows.clear();

        self.popovers_epoch += 1;
        self.text_runs_epoch += 1;
        self.minimap_epoch += 1;
    }

    /// Feed everything this grid's cells put on screen into `hasher`.
    ///
    /// The border table goes in alongside the cells because [`Self::clear`]
    /// resets it, so the same [`BorderId`] names different borders across two
    /// fills. Hashing the id alone reads two different grids as one.
    pub(crate) fn hash_content(&self, hasher: &mut impl Hasher) {
        self.cells.hash(hasher);
        self.border_table.hash(hasher);
    }

    /// The border set `id` names, or the borderless set for an id this grid
    /// never interned.
    ///
    /// An id is only meaningful against the grid that produced it, so one
    /// carried over from another grid resolves to whatever sits at that index
    /// here. See [`Self::set_border_edge`] for why no cell copied between grids
    /// carries one.
    pub fn borders(&self, id: BorderId) -> Borders {
        match id.0.checked_sub(1) {
            Some(index) => self.border_table.get(index as usize).copied(),
            None => None,
        }
        .unwrap_or_default()
    }

    /// The border set on the cell at `row`, `col`.
    pub fn cell_borders(&self, row: usize, col: usize) -> Borders {
        self.borders(self.get(row, col).border_id)
    }

    /// Stamp `border` onto one `edge` of every cell in `cols` on `row`, leaving
    /// each cell's other edges as they were.
    ///
    /// Columns past the grid are skipped, so a caller may name a region wider
    /// than the screen. Wire coordinates are untrusted and may point anywhere.
    ///
    /// Every cell a grid copies in from elsewhere comes from a pool page grid,
    /// and those never carry borders, since a `Border` command is dropped while
    /// a fill paints. So an id never has to be translated between tables.
    pub fn set_border_edge(
        &mut self,
        row: usize,
        cols: Range<usize>,
        edge: BorderEdge,
        border: Border,
    ) {
        if row >= self.rows {
            return;
        }
        for col in cols.start..cols.end.min(self.cols) {
            let index = row * self.cols + col;
            let mut updated = self.borders(self.cells[index].border_id);
            match edge {
                BorderEdge::Top => updated.top = Some(border),
                BorderEdge::Right => updated.right = Some(border),
                BorderEdge::Bottom => updated.bottom = Some(border),
                BorderEdge::Left => updated.left = Some(border),
            }
            self.cells[index].border_id = self.intern_borders(updated);
        }
    }

    /// The id for `borders` in this grid's table, adding it if it is new.
    ///
    /// A linear scan, which suits a frame declaring a handful of distinct edge
    /// sets over the pane edges it draws. A full table returns
    /// [`BorderId::NONE`], since 65,535 distinct sets on one screen means
    /// something upstream has gone wrong, and losing a border line is a better
    /// answer than handing back an id that names someone else's set.
    fn intern_borders(&mut self, borders: Borders) -> BorderId {
        if borders == Borders::default() {
            return BorderId::NONE;
        }
        if let Some(index) = self.border_table.iter().position(|set| *set == borders) {
            return BorderId(index as u16 + 1);
        }
        if self.border_table.len() >= usize::from(u16::MAX) {
            return BorderId::NONE;
        }
        self.border_table.push(borders);
        BorderId(self.border_table.len() as u16)
    }

    /// The floating overlay regions drawn above the cells, in draw order.
    pub fn overlays(&self) -> &[Overlay] {
        &self.overlays
    }

    /// Replace the floating overlay regions.
    ///
    /// Overlays are grid-level rather than per-cell, so the projection that
    /// rewrites cells leaves them untouched. The caller sets the full list each
    /// frame it changes.
    ///
    /// Moves [`Self::popovers_epoch`].
    pub fn set_overlays(&mut self, overlays: Vec<Overlay>) {
        self.overlays = overlays;
        self.popovers_epoch += 1;
    }

    /// The modal-chrome panels drawn with the cells, in draw order.
    pub fn panels(&self) -> &[Panel] {
        &self.panels
    }

    /// Replace the modal-chrome panels.
    ///
    /// Grid-level like the overlays, so the per-cell projection leaves them
    /// untouched. The caller sets the full list each frame it changes.
    pub fn set_panels(&mut self, panels: Vec<Panel>) {
        self.panels = panels;
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
    /// untouched. The caller sets the full list each frame it changes.
    ///
    /// Moves [`Self::text_runs_epoch`].
    pub fn set_text_runs(&mut self, text_runs: Vec<TextRun>) {
        self.text_runs = text_runs;
        self.text_runs_epoch += 1;
    }

    /// Replace the run list with `count` runs built by `run`, reusing the
    /// vector already there.
    ///
    /// The projection rebuilds this list whenever the runs are dirty, and a
    /// gutter declares one run per visible line, so collecting into a fresh
    /// vector would allocate one per frame. `run` is called once per index in
    /// order.
    ///
    /// The list is held out of the grid while `run` fills it, so `run` can read the
    /// grid it is building into. A run resolves its declared logical row through the
    /// line layout, which lives here.
    ///
    /// Moves [`Self::text_runs_epoch`].
    pub fn fill_text_runs(&mut self, count: usize, run: impl FnMut(&Grid, usize) -> TextRun) {
        let mut text_runs = mem::take(&mut self.text_runs);
        self.fill_list(&mut text_runs, count, run);
        self.text_runs = text_runs;
        self.text_runs_epoch += 1;
    }

    /// Replace the bar list with `count` bars built by `bar`, reusing the vector
    /// already there.
    ///
    /// Rebuilt whenever the bars or the line layout are dirty, and an editor
    /// re-sends its layout on any wrap change, so this runs far more often than the
    /// bars themselves change. `bar` reads the grid for the same reason
    /// [`Self::fill_text_runs`] does.
    pub fn fill_bars(&mut self, count: usize, bar: impl FnMut(&Grid, usize) -> Bar) {
        let mut bars = mem::take(&mut self.bars);
        self.fill_list(&mut bars, count, bar);
        self.bars = bars;
    }

    /// Replace the overlay list with `count` overlays built by `overlay`, reusing
    /// the vector already there.
    ///
    /// Moves [`Self::popovers_epoch`].
    pub fn fill_overlays(&mut self, count: usize, overlay: impl FnMut(usize) -> Overlay) {
        fill_owned(&mut self.overlays, count, overlay);
        self.popovers_epoch += 1;
    }

    /// Replace the panel list with `count` panels built by `panel`, reusing the
    /// vector already there.
    pub fn fill_panels(&mut self, count: usize, panel: impl FnMut(usize) -> Panel) {
        fill_owned(&mut self.panels, count, panel);
    }

    /// Replace the icon list with `count` icons built by `icon`, reusing the vector
    /// already there.
    pub fn fill_icons(&mut self, count: usize, icon: impl FnMut(usize) -> Icon) {
        fill_owned(&mut self.icons, count, icon);
    }

    /// Replace the minimap list with `count` strips built by `minimap`, reusing the
    /// vector already there.
    ///
    /// Moves [`Self::minimap_epoch`].
    pub fn fill_minimaps(&mut self, count: usize, minimap: impl FnMut(usize) -> Minimap) {
        fill_owned(&mut self.minimaps, count, minimap);
        self.minimap_epoch += 1;
    }

    /// Fill `list` with `count` items built from this grid, for a list already taken
    /// out of it so the closure can read it.
    fn fill_list<T>(
        &self,
        list: &mut Vec<T>,
        count: usize,
        mut item: impl FnMut(&Grid, usize) -> T,
    ) {
        list.clear();
        list.reserve(count);
        list.extend((0..count).map(|index| item(self, index)));
    }

    /// The run list, for a caller rewriting it from another grid's.
    ///
    /// A pool composite replaces the whole list every frame, so it wants to
    /// overwrite what is here rather than hand over a freshly collected vector
    /// and drop this one.
    ///
    /// Moves [`Self::text_runs_epoch`] on the way out. What the caller does with
    /// the borrow is not observable from here, so it counts as a change.
    pub fn text_runs_mut(&mut self) -> &mut Vec<TextRun> {
        self.text_runs_epoch += 1;
        &mut self.text_runs
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

    /// The bar list, for a caller rewriting it from another grid's. See
    /// [`Self::text_runs_mut`].
    pub fn bars_mut(&mut self) -> &mut Vec<Bar> {
        &mut self.bars
    }

    /// The off-grid stroked paths drawn above the cells, in draw order.
    pub fn polylines(&self) -> &[Polyline] {
        &self.polylines
    }

    /// Replace the off-grid stroked paths.
    ///
    /// Grid-level like the bars, so the per-cell projection leaves them
    /// untouched and the caller sets the full list each frame it changes.
    pub fn set_polylines(&mut self, polylines: Vec<Polyline>) {
        self.polylines = polylines;
    }

    /// The path list, for a caller rewriting it from another grid's. See
    /// [`Self::text_runs_mut`].
    pub fn polylines_mut(&mut self) -> &mut Vec<Polyline> {
        &mut self.polylines
    }

    /// Copy `rows` rows of `src` in at (`top`, `left`), replacing whatever
    /// decorations this grid held with `src`'s, shifted to land there.
    ///
    /// For a destination one source fills on its own. The decoration lists are
    /// overwritten rather than added to, so a source that has dropped a bar
    /// clears the one this grid was showing instead of leaving it behind.
    ///
    /// `rows` is the caller's, not `src`'s. A pool composes one row past its
    /// region to cover the sliver a sub-cell glide reveals, and whether that row
    /// belongs in the destination is the caller's question, not this one's.
    /// Rows and columns past this grid's edge are dropped, so a region declared
    /// past the viewport clips rather than panicking.
    pub fn blit_region(&mut self, src: &Grid, top: usize, left: usize, rows: usize) {
        let Some((rows, cols)) = self.region_extent(src, top, left, rows) else {
            return;
        };
        self.copy_cells(src, top, left, rows, cols);

        let (dx, dy) = decoration_shift(top, left);
        copy_translated(&mut self.text_runs, src.text_runs(), |run| {
            run.col += dx;
            run.row += dy;
        });
        copy_translated(&mut self.bars, src.bars(), |bar| {
            bar.x += dx;
            bar.y += dy;
        });
        copy_translated(&mut self.polylines, src.polylines(), |polyline| {
            shift_polyline(polyline, dx, dy);
        });
    }

    /// Copy `rows` rows of `src` in at (`top`, `left`), adding `src`'s
    /// decorations to this grid's rather than replacing them, and mark in
    /// `damage` every destination row the copy changed.
    ///
    /// For a destination several sources compose into, each owning its own
    /// rectangle. The caller resets the grid once before the run, so what
    /// accumulates is that run's and nothing older.
    ///
    /// A row already holding what the copy would write is left alone and
    /// unmarked, which is what lets a caller recompose in place and rebuild only
    /// what moved. A caller that blanked the grid first has nothing to compare
    /// against and passes [`Damage::Full`], where marking is a no-op.
    ///
    /// The decorations are appended whatever the cells compare, since nothing
    /// here can tell whether the caller reset them.
    ///
    /// See [`Self::blit_region`] for what `rows` means and how the edges clip.
    pub fn append_region(
        &mut self,
        src: &Grid,
        top: usize,
        left: usize,
        rows: usize,
        damage: &mut Damage,
    ) {
        let Some((rows, cols)) = self.region_extent(src, top, left, rows) else {
            return;
        };
        self.copy_damaged_cells(src, top, left, rows, cols, damage);

        let (dx, dy) = decoration_shift(top, left);
        self.text_runs.extend(src.text_runs().iter().map(|run| {
            let mut run = run.clone();
            run.col += dx;
            run.row += dy;
            run
        }));
        self.bars.extend(src.bars().iter().map(|bar| {
            let mut bar = *bar;
            bar.x += dx;
            bar.y += dy;
            bar
        }));
        self.polylines
            .extend(src.polylines().iter().map(|polyline| {
                let mut polyline = polyline.clone();
                shift_polyline(&mut polyline, dx, dy);
                polyline
            }));
    }

    /// The rows and columns of `src` that fit at (`top`, `left`), or `None`
    /// when nothing does.
    fn region_extent(
        &self,
        src: &Grid,
        top: usize,
        left: usize,
        rows: usize,
    ) -> Option<(usize, usize)> {
        let rows = rows.min(src.rows()).min(self.rows().saturating_sub(top));
        let cols = src.cols().min(self.cols().saturating_sub(left));
        (cols > 0).then_some((rows, cols))
    }

    fn copy_cells(&mut self, src: &Grid, top: usize, left: usize, rows: usize, cols: usize) {
        for r in 0..rows {
            self.row_mut(top + r)[left..left + cols].copy_from_slice(&src.row(r)[..cols]);
        }
    }

    /// [`Self::copy_cells`], marking each destination row the copy changed.
    ///
    /// The compare is per row rather than per cell because the damage names
    /// rows: a row that differs anywhere is rebuilt over its whole width, so
    /// finding where it differs would buy nothing.
    fn copy_damaged_cells(
        &mut self,
        src: &Grid,
        top: usize,
        left: usize,
        rows: usize,
        cols: usize,
        damage: &mut Damage,
    ) {
        let width = self.cols;
        for r in 0..rows {
            let source = &src.row(r)[..cols];
            let into = &mut self.row_mut(top + r)[left..left + cols];
            if into == source {
                continue;
            }

            into.copy_from_slice(source);
            if let Damage::Partial(marked) = damage
                && let Some(row) = marked.get_mut(top + r)
            {
                *row = whole_row(width);
            }
        }
    }

    /// The declared minimap strips, each carrying its viewport thumb.
    ///
    /// Resolve a strip's line summaries with [`Self::minimap_content`] keyed by
    /// its [`MinimapCommand::content_id`].
    pub fn minimaps(&self) -> &[Minimap] {
        &self.minimaps
    }

    /// The line summaries stored under `content_id`, or an empty slice when no
    /// store exists for it.
    pub fn minimap_content(&self, content_id: u32) -> &[LineSummary] {
        self.minimap_contents
            .get(&content_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Replace the declared minimap strips.
    ///
    /// Moves [`Self::minimap_epoch`].
    pub fn set_minimaps(&mut self, minimaps: Vec<Minimap>) {
        self.minimaps = minimaps;
        self.minimap_epoch += 1;
    }

    /// Replace the minimap line-summary stores.
    ///
    /// Moves [`Self::minimap_epoch`].
    pub fn set_minimap_contents(&mut self, contents: HashMap<u32, Vec<LineSummary>>) {
        self.minimap_contents = contents;
        self.minimap_epoch += 1;
    }

    /// Splice `lines` into store `content_id`, replacing `removed` lines from
    /// `start` and creating the store when absent.
    ///
    /// Replays one `minimap_lines` change against the grid's stores, which equal
    /// the term's as of the last projection, so this clamps exactly as the term's
    /// splice did.
    ///
    /// Moves [`Self::minimap_epoch`], so replaying a journal of several changes
    /// moves it once per change. Only inequality is read, so that costs a
    /// renderer nothing beyond the rebuild it was owed anyway.
    pub fn splice_minimap_content(
        &mut self,
        content_id: u32,
        start: u32,
        removed: u32,
        lines: &[LineSummary],
    ) {
        let store = self.minimap_contents.entry(content_id).or_default();
        splice_summaries(store, start, removed, lines);
        self.minimap_epoch += 1;
    }

    /// Remove the store under `content_id`, replaying a `minimap_drop`.
    ///
    /// Moves [`Self::minimap_epoch`], as [`Self::splice_minimap_content`] does.
    pub fn drop_minimap_content(&mut self, content_id: u32) {
        self.minimap_contents.remove(&content_id);
        self.minimap_epoch += 1;
    }

    /// Replace the per-logical-line heights, in rows, indexed from the top.
    ///
    /// A line past the end of the list is one row tall. The cell projection is
    /// unaffected; the layout exists for off-grid components to align to.
    pub fn set_line_heights(&mut self, line_heights: Vec<u16>) {
        self.line_start_rows.clear();
        if line_heights.is_empty() {
            return;
        }

        // One entry per line plus the total, which is where a line past the
        // declared ones starts counting from.
        self.line_start_rows.reserve(line_heights.len() + 1);
        let mut above = 0usize;
        self.line_start_rows.push(above);
        for height in line_heights {
            above += height as usize;
            self.line_start_rows.push(above);
        }
    }

    /// The physical row a logical line starts on, being the heights of the lines
    /// above it summed, with any line past the declared heights counting as one
    /// row. With no expansions this is `line` itself.
    pub fn line_start_row(&self, line: usize) -> usize {
        match self.line_start_rows.get(line) {
            Some(&start) => start,
            // Past the declared lines, or none declared at all. Each of those
            // is one row, so the answer is the total plus however many lines
            // beyond the last declared one this is.
            None => match self.line_start_rows.last() {
                Some(&total) => total + line - (self.line_start_rows.len() - 1),
                None => line,
            },
        }
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

/// Splice `lines` into `store`, replacing `removed` lines from `start`.
///
/// `start` and the removal end clamp to the store length, so an out-of-range
/// splice appends or truncates rather than panicking. Lines are cloned from the
/// slice, so the same splice can feed a separate store than the one it came from.
pub(crate) fn splice_summaries(
    store: &mut Vec<LineSummary>,
    start: u32,
    removed: u32,
    lines: &[LineSummary],
) {
    let start = (start as usize).min(store.len());
    let end = start.saturating_add(removed as usize).min(store.len());
    store.splice(start..end, lines.iter().cloned());
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
                refilled: false,
                content_hash: 0,
                grid: Grid::new(rows, cols),
                text_runs: Vec::new(),
                bars: Vec::new(),
                polylines: Vec::new(),
            })
            .collect();
        PagePool { pages }
    }

    /// Recycle the slot for document page `index` and return its grid for the
    /// caller to write the page's content into.
    ///
    /// `painted_rows` by `painted_cols` is the top-left box the caller
    /// overwrites in full. Everything outside it is cleared; inside it the last
    /// page is left standing for the paint to replace, which is what keeps an
    /// ordinary fill from writing every cell twice. A caller that paints less
    /// than it claims leaves the last page showing there.
    ///
    /// [`Self::page`] resolves `index` to this grid afterward. If the slot held
    /// a different page, that page is dropped and its buffer reused in place, so
    /// a sliding window allocates nothing. Any page-targeted decorations from
    /// the departed page are dropped too. The caller writes the new page's via
    /// [`Self::set_decorations`].
    pub fn fill(&mut self, index: u64, painted_rows: usize, painted_cols: usize) -> &mut Grid {
        let slot = self.slot(index);
        let page = &mut self.pages[slot];
        page.refilled = page.index == Some(index);
        page.index = Some(index);
        page.grid.clear_except(painted_rows, painted_cols);
        page.text_runs.clear();
        page.bars.clear();
        page.polylines.clear();
        &mut page.grid
    }

    /// Whether the page just written into `index`'s slot renders differently
    /// from what that slot last held, recording the new content for the next
    /// comparison either way.
    ///
    /// Called once per fill, after the cells are painted and
    /// [`Self::set_decorations`] has run, since it digests both. `true` for a
    /// slot that changed page, whose composition moved whatever the page holds.
    ///
    /// A pool refills pages that did not change, which is what this exists for.
    /// A caller watching one version refills every buffered page when it moves,
    /// and most of those repaint the same bytes. Reporting that as a change
    /// costs a recompose of everything the pool feeds.
    pub fn content_changed(&mut self, index: u64) -> bool {
        let slot = self.slot(index);
        let hash = self.pages[slot].content_hash();
        let page = &mut self.pages[slot];
        let same = page.refilled && page.content_hash == hash;
        page.content_hash = hash;
        !same
    }

    /// Store the page-targeted decorations captured for document page `index`,
    /// replacing any already on its slot.
    ///
    /// Written by the terminal when a fill commits, after [`Self::fill`] has
    /// recycled the slot and its cells are painted. The commands are page-local.
    /// The terminal translates them to the window when it projects the pool.
    pub fn set_decorations(
        &mut self,
        index: u64,
        text_runs: Vec<TextRunCommand>,
        bars: Vec<BarCommand>,
        polylines: Vec<PolylineCommand>,
    ) {
        let slot = self.slot(index);
        let page = &mut self.pages[slot];

        page.text_runs = text_runs
            .into_iter()
            .map(|command| {
                let run = StoredTextRun::from(command);
                text_run_from_command(&run, run.row, 0)
            })
            .collect();
        page.bars = bars
            .iter()
            .map(|command| bar_from_command(command, command.y, 0))
            .collect();

        page.polylines.resize_with(polylines.len(), Polyline::empty);
        for (slot, command) in page.polylines.iter_mut().zip(&polylines) {
            fill_polyline(slot, command, 0);
        }
    }

    /// The page-targeted decorations buffered for document page `index`, or
    /// `None` when that page is not currently in the pool's window.
    pub fn page_decorations(&self, index: u64) -> Option<(&[TextRun], &[Bar], &[Polyline])> {
        let page = &self.pages[self.slot(index)];
        (page.index == Some(index)).then_some((&page.text_runs, &page.bars, &page.polylines))
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
    /// Returns `false` when a needed page is not buffered, or `top` falls above
    /// the first page. `out` is left untouched on failure, so a caller holding a
    /// previous composite keeps it rather than seeing it half-overwritten.
    pub fn compose(&self, top: i64, out: &mut Grid) -> bool {
        let page_rows = match self.pages.first() {
            Some(page) if page.grid.rows() > 0 => page.grid.rows(),
            _ => return false,
        };

        // Validate the whole window before writing anything. A partial copy
        // interrupted by an unbuffered page would corrupt the caller's held
        // composite, which the hold-last-offset path relies on staying intact.
        for out_row in 0..out.rows() {
            let doc_row = top + out_row as i64;
            if doc_row < 0 {
                return false;
            }
            let page_index = doc_row as u64 / page_rows as u64;
            if self.page(page_index).is_none() {
                return false;
            }
        }

        for out_row in 0..out.rows() {
            let doc_row = top + out_row as i64;
            let page_index = doc_row as u64 / page_rows as u64;
            let row_in_page = doc_row as usize % page_rows;
            let page = self.page(page_index).expect("validated above");

            // A cell's border_id indexes its own grid's table, so copying one
            // across would misname a set. Page grids never carry borders, a
            // Border command being dropped while a fill paints, so every id
            // moved here is the borderless one.
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
            page.text_runs.clear();
            page.bars.clear();
            page.polylines.clear();
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
    /// Whether the fill now open recycled this slot onto the page it already
    /// held, so a commit tells a refill from a slide onto a new page.
    ///
    /// A slide always counts as a change. The pool composes by document row
    /// through the page index, so reading a different page out of this slot
    /// moves the composition whatever that page holds.
    refilled: bool,
    /// Digest of what this slot last committed, so a refill that paints the
    /// same bytes is recognized as one.
    ///
    /// A digest rather than the content itself because the caller paints over
    /// the slot in place, so there is nothing left to compare against by the
    /// time there is something to compare.
    content_hash: u64,
    grid: Grid,
    /// Page-targeted text runs captured from the fill that painted this slot,
    /// page-local and pre-converted to the grid form at capture time. Empty for a
    /// plain page. The terminal's pool projection stamps them into the composite
    /// grid, translated to the window's rows.
    text_runs: Vec<TextRun>,
    /// Page-targeted bars captured from the fill that painted this slot,
    /// page-local. See [`Self::text_runs`].
    bars: Vec<Bar>,
    /// Page-targeted stroked paths captured from the fill that painted this
    /// slot, page-local. See [`Self::text_runs`].
    polylines: Vec<Polyline>,
}

impl Page {
    /// A digest of everything this slot puts on screen.
    fn content_hash(&self) -> u64 {
        let mut hasher = FxHasher::default();
        self.grid.hash_content(&mut hasher);
        self.text_runs.hash(&mut hasher);
        self.bars.hash(&mut hasher);
        self.polylines.hash(&mut hasher);
        hasher.finish()
    }
}

/// A single grid cell: one character and how to render it.
///
/// The base attribute set every cell carries. stoatty-specific per-cell
/// attributes (border edges, popover anchors) are added by later feature items.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
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
    /// This cell's border set, held in its grid's table rather than inline.
    ///
    /// Resolve it with [`Grid::borders`], or read a cell's set directly with
    /// [`Grid::cell_borders`]. An id only means anything against the grid that
    /// interned it.
    pub border_id: BorderId,
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
            border_id: BorderId::NONE,
            scale: Scale::Single,
        }
    }
}

/// A cell's border set, as an index into the grid that interned it.
///
/// Borders sit on pane edges, a few dozen cells against the thousands a screen
/// holds, so a set stored inline would cost every cell four optional borders for
/// the sake of the handful that carry any. Held in a table instead, a cell costs
/// two bytes and the sets themselves are shared across every edge that matches.
///
/// [`BorderId::NONE`] means no borders and occupies no table entry, so the
/// common cell needs no lookup at all.
///
/// See also:
/// - [`Grid::cell_borders`] to read a cell's set.
/// - [`Grid::set_border_edge`] to stamp one edge of a run of cells.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct BorderId(u16);

impl BorderId {
    /// The id of a cell with no borders, which every cell starts at.
    pub const NONE: BorderId = BorderId(0);
}

/// Which of a cell's four edges a border is being stamped on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BorderEdge {
    Top,
    Right,
    Bottom,
    Left,
}

/// The renderer-native border on each of a cell's four edges.
///
/// Each edge is independently present or absent. The renderer draws a line
/// along every present edge, so a region framed by setting the perimeter cells'
/// outer edges reads as a panel border without any box-drawing glyphs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Borders {
    pub top: Option<Border>,
    pub right: Option<Border>,
    pub bottom: Option<Border>,
    pub left: Option<Border>,
}

/// A border drawn along one cell edge.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
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
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BorderStyle {
    Light,
    Heavy,
    Double,
    Rounded,
}

/// How a [`Panel`]'s shadow is drawn, as renderer primitives.
///
/// [`PanelShadow::None_`] draws no shadow. [`PanelShadow::Drop`] displaces and
/// blurs the shadow so the panel reads as floating above the grid.
/// [`PanelShadow::Tucked`] leaves it undisplaced and clipped above the panel's
/// bottom edge, so the panel reads as emerging from beneath what sits below it.
/// [`PanelShadow::Overhang`] drops the exterior halo entirely for a small shadow
/// band inside the panel's bottom edge, so it reads as tucked under what overhangs
/// it above.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelShadow {
    None_,
    Drop,
    Tucked,
    Overhang,
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
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
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
    /// Shape the content text at bold weight rather than the default. Only the
    /// content is affected. The box chrome is unchanged.
    pub bold: bool,
    pub content: String,
}

/// Off-grid modal chrome framing a cell rectangle.
///
/// Like an [`Overlay`], the panel is grid-level rather than a cell attribute: a
/// hairline frame in [`Self::border`] at [`Self::style`] weight with
/// [`Self::corner_radius`] logical-pixel rounded corners and a [`Self::shadow`] in
/// the selected [`PanelShadow`] style, composited around the `width` by `height`
/// cell rectangle at
/// (`top`, `left`). The framed cells keep rendering their own content, so unlike
/// an opaque overlay the panel is chrome layered with the grid rather than over
/// it. The frame itself is the one part that draws over everything it
/// surrounds, so a glyph reaching a cell edge cannot break the line.
///
/// [`Self::fill`] is [`Some`] to paint the interior that color, or [`None`] to
/// leave the cells' own backgrounds showing through.
// `Eq` is absent because [`Self::anchor`] carries a fractional row offset and
// `f32` is only `PartialEq`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Panel {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    pub style: BorderStyle,
    pub border: Rgb,
    pub corner_radius: u8,
    pub fill: Option<Rgb>,
    pub shadow: PanelShadow,
    /// Logical pixels shaved off each horizontal edge, so the box draws narrower
    /// than its cell rect. Carried from the panel command and applied by the
    /// renderer to the frame, fill, corners, and shadow.
    pub inset_x: u8,
    /// The panel floats above every pooled surface, so pool composites must not
    /// paint over its rect. `false` layers the panel with the grid, where a pool
    /// composite covering the same cells draws over it.
    pub above_pools: bool,
    /// The host pool this panel rides and the document top row its layout
    /// assumed, or `None` for a panel fixed to the screen.
    ///
    /// Carried from the panel command so the renderer draws the frame shifted
    /// by the gap between `top_rows` and the host's eased top.
    pub anchor: Option<(u32, f32)>,
    /// Monotonic declaration-order index across all non-cell components.
    ///
    /// It orders nothing. Draw order is fixed by the renderer's pass chain, one
    /// pass per kind, so a bar declared after a path still draws beneath it.
    /// What `seq` decides is occlusion. A fragment is discarded where a panel
    /// declared later than its own component covers it.
    ///
    /// A box's own runs and bars are declared after its panel and so carry a
    /// higher `seq`, which is what lets them survive over a body that hides the
    /// lower chrome beneath it.
    pub seq: u32,
}

/// The sixteenths a decoration moves by to land at cell (`top`, `left`).
///
/// Off-grid decorations are placed in sixteenths of a cell, so a whole-cell
/// origin is sixteen of them.
fn decoration_shift(top: usize, left: usize) -> (i16, i16) {
    (left as i16 * 16, top as i16 * 16)
}

fn shift_polyline(polyline: &mut Polyline, dx: i16, dy: i16) {
    for point in &mut polyline.points {
        point[0] += dx;
        point[1] += dy;
    }
}

/// Overwrite `dst` with `src`, then shift each entry into place.
///
/// A gliding pool rewrites these lists every frame, so the entries `dst`
/// already holds are overwritten rather than dropped for freshly collected
/// ones. `clone_from` is what carries that down into an entry's own
/// allocations, keeping a path's point list off the allocator too.
fn copy_translated<T: Clone>(dst: &mut Vec<T>, src: &[T], mut shift: impl FnMut(&mut T)) {
    dst.truncate(src.len());
    for (out, item) in dst.iter_mut().zip(src) {
        out.clone_from(item);
    }
    dst.extend_from_slice(&src[dst.len()..]);

    for item in dst {
        shift(item);
    }
}

/// What one row of a partial [`Damage`] changed over.
///
/// `None` is a clean row. `Some((left, right))` names the inclusive column
/// bounds, which is what lets a fixed-size instance buffer patch the cells that
/// moved instead of the row holding them. A cell blinking in place is the case
/// this exists for.
pub type RowDamage = Option<(u16, u16)>;

/// The [`RowDamage`] covering a whole row `cols` wide.
///
/// Three kinds of damage have no narrower answer. A selection overlay enters or
/// leaves a whole row, a slide-and-compare compares whole rows, and a
/// decoration spans the row it sits on.
pub fn whole_row(cols: usize) -> RowDamage {
    (cols > 0).then(|| (0, (cols - 1).min(u16::MAX as usize) as u16))
}

/// The set of viewport rows a projection rewrote, so a renderer rebuilds only
/// the rows that changed.
///
/// [`Damage::Full`] means every row did over its whole width, which is what a
/// resize or a terminal-reported full damage produces. [`Damage::Partial`]
/// carries one [`RowDamage`] per row, indexed by row.
pub enum Damage {
    Full,
    Partial(Vec<RowDamage>),
}

impl Damage {
    /// Whether `row` changed this projection. Every row under [`Damage::Full`]
    /// reads as dirty, and a row past a partial vector reads as clean.
    pub fn is_dirty(&self, row: usize) -> bool {
        match self {
            Damage::Full => true,
            Damage::Partial(rows) => rows.get(row).copied().flatten().is_some(),
        }
    }

    /// The inclusive column bounds `row` changed over, or `None` when it is
    /// clean.
    ///
    /// `cols` is the width a [`Damage::Full`] covers. The damage names rows
    /// alone, so the caller supplies how wide one is.
    pub fn columns(&self, row: usize, cols: usize) -> Option<(usize, usize)> {
        match self {
            Damage::Full => (cols > 0).then(|| (0, cols - 1)),
            Damage::Partial(rows) => rows
                .get(row)
                .copied()
                .flatten()
                .map(|(left, right)| (left as usize, right as usize)),
        }
    }
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

/// One smooth-scroll surface's declared rectangle, and which pool and window
/// it belongs to.
///
/// The pool's content lives in a [`PagePool`] rather than in the grid cells, so
/// this is where the renderer places and clips that content. [`Self::window`]
/// is `0` for the primary grid, where the coordinates are grid-absolute, and a
/// nonzero `N` binds the pool to aux window `N`, where they are relative to
/// that window's own grid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PoolRegion {
    pub pool: u32,
    pub window: u32,
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
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
    /// Signed `[x, y]` pixel offset from the anchor cell, carried from the
    /// `IconCommand` so the icon can shift inside a popover's inset content.
    pub offset: [i16; 2],
    /// Monotonic declaration-order index across all non-cell components. See
    /// [`Panel::seq`].
    pub seq: u32,
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
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TextRun {
    pub col: i16,
    pub row: i16,
    pub scale: u16,
    pub color: Rgb,
    /// Opaque background box painted behind the glyphs, or `None` to blend the
    /// glyphs directly over the surface behind the run with no backing box.
    pub bg: Option<Rgb>,
    pub text: Arc<str>,
    /// Monotonic declaration-order index across all non-cell components. See
    /// [`Panel::seq`].
    pub seq: u32,
}

/// A thin rectangle filled off the cell grid in a solid color.
///
/// Like an [`Overlay`] it is grid-level, not a cell attribute: a non-cell
/// component (a gutter) packs several variable-width status bars and a hairline
/// separator into a fraction of a cell. [`Self::x`] and [`Self::width`] run
/// along the cell width, [`Self::y`] and [`Self::height`] along the cell height,
/// all in sixteenths of a cell (16 = one cell), so a bar can be a fraction of a
/// cell wide.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Bar {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub color: Rgb,
    /// Monotonic declaration-order index across all non-cell components. See
    /// [`Panel::seq`].
    pub seq: u32,
}

/// A stroked path drawn off the cell grid in a solid color.
///
/// Grid-level like [`Bar`], and the only decoration whose geometry is not
/// axis-aligned. Every coordinate and [`Self::width`] is in sixteenths of a
/// cell (16 = one cell). A single point, or two equal ones, draws a dot, which
/// is how the commit graph marks a node.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Polyline {
    /// Vertices in draw order, each `[x, y]`.
    pub points: Vec<[i16; 2]>,
    /// Stroke thickness in sixteenths of the cell's width, centered on the
    /// path. Measured against the width on both axes, so a diagonal is as thick
    /// as a vertical and 16 draws exactly one column wide.
    pub width: u16,
    pub color: Rgb,
    /// Monotonic declaration-order index across all non-cell components. See
    /// [`Panel::seq`].
    pub seq: u32,
}

impl Polyline {
    /// A path with no points, for a caller growing a list it refills in place.
    ///
    /// The lists of paths are rebuilt far more often than their contents change,
    /// so each one keeps its slots and their point vectors across rebuilds. A
    /// grown list needs a slot to refill, which is what this is.
    pub(crate) fn empty() -> Polyline {
        Polyline {
            points: Vec::new(),
            width: 0,
            color: Rgb::new(0, 0, 0),
            seq: 0,
        }
    }
}

/// Fill `list` with `count` items built by `item`, keeping the vector's allocation.
///
/// For a list whose builder needs nothing from the grid. The ones that do go through
/// [`Grid::fill_list`], which hands the grid to the builder.
fn fill_owned<T>(list: &mut Vec<T>, count: usize, mut item: impl FnMut(usize) -> T) {
    list.clear();
    list.reserve(count);
    list.extend((0..count).map(&mut item));
}

/// A declared minimap strip, in the grid's own colors.
///
/// The renderer draws from this rather than from the [`MinimapCommand`] it was
/// declared by, which is what keeps the wire format out of the render crate.
/// Geometry and ids pass through unchanged. What the projection does is
/// resolve the color triples and the palette.
///
/// The line summaries a strip renders are not here. They live in the grid's
/// content store, keyed by [`Self::content_id`], because several strips share
/// one store and a splice replaces lines without touching the declaration.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MinimapStrip {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    pub strip_id: u32,
    /// The line-summary store this strip renders, read through
    /// [`Grid::minimap_content`].
    pub content_id: u32,
    pub lines_per_cell: u8,
    pub max_columns: u8,
    /// The strip's ground, drawn under the runs.
    pub bg: Rgba,
    /// The viewport overlay, drawn over them.
    pub thumb: Rgba,
    pub thumb_border: Rgb,
    /// Colors a run's class indexes, up to 64 entries.
    pub palette: Vec<Rgb>,
}

/// A declared minimap strip joined with its viewport thumb.
///
/// [`Self::view`] is the thumb position, absent until a viewport update
/// arrives.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Minimap {
    pub strip: MinimapStrip,
    /// Monotonic declaration-order index across all non-cell components. See
    /// [`Panel::seq`].
    pub seq: u32,
    pub view: Option<MinimapView>,
}

/// A minimap's viewport thumb position.
///
/// [`Self::top_256`] is the fractional top buffer line in 1/256ths of a line and
/// [`Self::visible`] the viewport height in lines, together sizing and placing
/// the thumb over the strip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MinimapView {
    pub top_256: u32,
    pub visible: u16,
}

/// How a cell's underline is decorated, or [`UnderlineStyle::None`] for no
/// underline.
///
/// Mirrors the standard VT underline styles (SGR `4:1`-`4:5`); the renderer
/// draws each as a distinct shape rather than a glyph.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
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
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
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

/// A resolved color with an alpha channel, for a decoration drawn over the grid
/// rather than into it.
///
/// [`Rgb`] covers everything painted as a cell, where the grid is the backdrop
/// and there is nothing to blend against. A minimap strip floats above the
/// content it summarizes, so its ground and its viewport thumb both carry an
/// alpha the renderer composites with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        Rgba { r, g, b, a }
    }
}

/// The boolean text-rendering attributes a cell carries simultaneously.
///
/// A compact bitset rather than a struct of bools so a [`Cell`] stays small and
/// `Copy`. Underline is not here: it is a styled, separately-colored decoration,
/// so it rides on [`Cell::underline`] and [`Cell::underline_color`] instead.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Flags(u8);

impl Flags {
    pub const BOLD: Flags = Flags(0b0000_0001);
    pub const ITALIC: Flags = Flags(0b0000_0010);
    pub const DIM: Flags = Flags(0b0000_0100);
    pub const INVERSE: Flags = Flags(0b0000_1000);
    pub const HIDDEN: Flags = Flags(0b0001_0000);
    pub const STRIKEOUT: Flags = Flags(0b0010_0000);
    /// The cell holds a character two cells wide, the second of which is the
    /// blank spacer beside it.
    ///
    /// Carried so a renderer can give the glyph the box it actually occupies. A
    /// bitmap rasterized larger than one cell otherwise lands unclamped, drawn
    /// from one cell's origin across whatever sits beside it.
    pub const WIDE: Flags = Flags(0b0100_0000);

    /// The empty set, carrying no attributes.
    pub const fn empty() -> Flags {
        Flags(0)
    }

    /// Whether every attribute in `other` is also set in `self`.
    pub const fn contains(self, other: Flags) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Flip every attribute in `other`, so a selection overlay inverts a cell
    /// and composes correctly over an already-inverse one (double-invert
    /// cancels back to normal video).
    pub const fn toggle(self, other: Flags) -> Flags {
        Flags(self.0 ^ other.0)
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
        Bar, Border, BorderEdge, BorderId, BorderStyle, Borders, Cell, Flags, Grid, HashMap, Icon,
        IconKind, LineSummary, Minimap, MinimapRun, MinimapStrip, Overlay, PagePool, Rgb, Rgba,
        Scale, ScrollRegion, TextRun,
    };

    /// The rows the slide vacates have to come back blank rather than holding
    /// what the rows above them held, since a projector then rewrites only what
    /// it finds different.
    #[test]
    fn scrolling_up_moves_rows_and_blanks_the_tail() {
        let mut grid = Grid::new(4, 2);
        for row in 0..4 {
            for col in 0..2 {
                grid.get_mut(row, col).ch = char::from(b'a' + (row * 2 + col) as u8);
            }
        }

        grid.scroll_by(1);

        let row_text =
            |grid: &Grid, row: usize| grid.row(row).iter().map(|cell| cell.ch).collect::<String>();
        assert_eq!(row_text(&grid, 0), "cd", "row one moved up to row zero");
        assert_eq!(row_text(&grid, 2), "gh", "row three moved up to row two");
        assert_eq!(
            grid.row(3),
            &[Cell::default(), Cell::default()],
            "the row the slide vacated is blank",
        );
    }

    /// Every row read after a multi-row slide, since a one-row slide read at
    /// two of its rows is satisfied by a source or destination off by a row.
    #[test]
    fn a_multi_row_slide_lands_every_row_where_it_belongs() {
        let rows_of = |grid: &Grid| -> Vec<String> {
            (0..grid.rows())
                .map(|row| grid.row(row).iter().map(|cell| cell.ch).collect())
                .collect()
        };
        let filled = || {
            let mut grid = Grid::new(5, 3);
            for row in 0..5 {
                for col in 0..3 {
                    grid.get_mut(row, col).ch = char::from(b'a' + (row * 3 + col) as u8);
                }
            }
            grid
        };

        let mut up = filled();
        up.scroll_by(2);
        assert_eq!(
            rows_of(&up),
            ["ghi", "jkl", "mno", "   ", "   "],
            "two rows up leaves the last two blank and the rest shifted",
        );

        let mut down = filled();
        down.scroll_by(-2);
        assert_eq!(
            rows_of(&down),
            ["   ", "   ", "abc", "def", "ghi"],
            "two rows down leaves the first two blank and the rest shifted",
        );
    }

    /// Gliding back through scrollback moves the window's content down, so a
    /// negative slide runs the other way and blanks the top.
    #[test]
    fn scrolling_down_moves_rows_the_other_way() {
        let mut grid = Grid::new(4, 2);
        for row in 0..4 {
            for col in 0..2 {
                grid.get_mut(row, col).ch = char::from(b'a' + (row * 2 + col) as u8);
            }
        }

        grid.scroll_by(-1);

        let row_text =
            |grid: &Grid, row: usize| grid.row(row).iter().map(|cell| cell.ch).collect::<String>();
        assert_eq!(
            grid.row(0),
            &[Cell::default(), Cell::default()],
            "the row the slide vacated is blank",
        );
        assert_eq!(row_text(&grid, 1), "ab", "row zero moved down to row one");
        assert_eq!(row_text(&grid, 3), "ef", "row two moved down to row three");
    }

    /// A slide of at least the height leaves nothing that was on screen, so
    /// every row blanks rather than wrapping around.
    #[test]
    fn scrolling_up_past_the_height_blanks_every_row() {
        let mut grid = Grid::new(2, 2);
        grid.get_mut(0, 0).ch = 'x';
        grid.get_mut(1, 1).ch = 'y';

        grid.scroll_by(2);

        assert!(
            (0..2).all(|row| grid.row(row) == [Cell::default(), Cell::default()]),
            "nothing survives a slide of the whole screen",
        );
    }

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
    fn cells_sharing_a_border_set_share_one_id() {
        // The whole point of the table. A pane edge stamps the same border down a
        // run of cells, so storing it once is what keeps the table to the handful
        // of distinct sets a frame declares.
        let mut grid = Grid::new(2, 4);
        let light = Border {
            style: BorderStyle::Light,
            color: Rgb::new(1, 2, 3),
        };
        grid.set_border_edge(0, 0..4, BorderEdge::Top, light);

        let ids: Vec<_> = (0..4).map(|col| grid.get(0, col).border_id).collect();
        assert_eq!(ids, vec![ids[0]; 4], "one id across the run");
        assert_ne!(ids[0], BorderId::NONE, "and not the borderless one");
        assert_eq!(grid.cell_borders(0, 0).top, Some(light));

        // A second edge on one cell is a different set, so it takes its own id
        // while the rest of the run keeps theirs.
        grid.set_border_edge(0, 1..2, BorderEdge::Left, light);
        assert_ne!(grid.get(0, 1).border_id, ids[0], "two edges is a new set");
        assert_eq!(grid.get(0, 2).border_id, ids[0], "its neighbour is unmoved");
        assert_eq!(grid.cell_borders(0, 1).top, Some(light), "kept its top");
        assert_eq!(grid.cell_borders(0, 1).left, Some(light), "gained a left");
    }

    #[test]
    fn an_untouched_cell_reads_as_borderless() {
        let mut grid = Grid::new(1, 2);
        grid.set_border_edge(
            0,
            0..1,
            BorderEdge::Top,
            Border {
                style: BorderStyle::Light,
                color: Rgb::new(1, 2, 3),
            },
        );

        assert_eq!(grid.get(0, 1).border_id, BorderId::NONE);
        assert_eq!(grid.cell_borders(0, 1), Borders::default());
    }

    #[test]
    fn clearing_drops_the_border_table_with_the_cells() {
        // A table that outlived its cells would keep handing sets to ids that
        // nothing refers to, and grow a little on every recycled frame.
        let mut grid = Grid::new(1, 1);
        grid.set_border_edge(
            0,
            0..1,
            BorderEdge::Top,
            Border {
                style: BorderStyle::Light,
                color: Rgb::new(1, 2, 3),
            },
        );
        assert_ne!(grid.get(0, 0).border_id, BorderId::NONE);

        grid.clear();
        assert_eq!(grid.get(0, 0).border_id, BorderId::NONE);
        assert_eq!(grid.cell_borders(0, 0), Borders::default());
        assert_eq!(grid.border_table, Vec::new(), "the table went with them");
    }

    #[test]
    fn a_cell_stays_small() {
        // Every damaged-row copy in project, every slide-diff row compare, every
        // scroll_by rotate and every pool page grid moves cells in bulk, so the
        // struct's width is bandwidth. A field added here costs all of them, and
        // a rare attribute belongs behind an id like BorderId rather than inline.
        assert_eq!(
            size_of::<Cell>(),
            20,
            "grew past its budget; hold rare attributes out of line",
        );
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
            bold: false,
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
            offset: [0, 0],
            seq: 0,
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
            bg: Some(Rgb::new(24, 26, 32)),
            text: "42".into(),
            seq: 0,
        };
        grid.set_text_runs(vec![run.clone()]);

        assert_eq!(grid.text_runs(), [run]);

        grid.resize(2, 2);
        assert!(grid.text_runs().is_empty(), "resize clears the text runs");
    }

    /// A named mutation of a grid, for a table pinning what each one does.
    type NamedChange = (&'static str, fn(&mut Grid));

    fn epoch_probe_run() -> TextRun {
        TextRun {
            col: 0,
            row: 0,
            scale: 256,
            color: Rgb::new(1, 2, 3),
            bg: None,
            text: "x".into(),
            seq: 0,
        }
    }

    fn epoch_probe_minimap() -> Minimap {
        Minimap {
            strip: MinimapStrip {
                top: 0,
                left: 0,
                width: 1,
                height: 1,
                strip_id: 1,
                content_id: 7,
                lines_per_cell: 1,
                max_columns: 1,
                bg: Rgba::new(0, 0, 0, 255),
                thumb: Rgba::new(0, 0, 0, 255),
                thumb_border: Rgb::new(0, 0, 0),
                palette: Vec::new(),
            },
            seq: 0,
            view: None,
        }
    }

    fn epoch_probe_summary() -> LineSummary {
        std::sync::Arc::from([MinimapRun {
            start_col: 0,
            len: 1,
            class: 0,
        }])
    }

    fn epoch_probe_overlay() -> Overlay {
        Overlay {
            top: 0,
            left: 0,
            width: 1,
            height: 1,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(0, 0, 0),
            scale: 1,
            offset: [0, 0],
            bold: false,
            content: String::new(),
        }
    }

    /// A renderer caches against these counters, so an entry point that changes
    /// a list without moving one leaves last frame's decorations on screen.
    #[test]
    fn every_entry_point_that_changes_a_decoration_list_moves_its_epoch() {
        let runs: [NamedChange; 5] = [
            ("set_text_runs", |g| g.set_text_runs(vec![])),
            ("fill_text_runs", |g| {
                g.fill_text_runs(1, |_, _| epoch_probe_run())
            }),
            ("text_runs_mut", |g| g.text_runs_mut().clear()),
            ("resize", |g| g.resize(4, 4)),
            ("clear", |g| g.clear()),
        ];
        for (name, change) in runs {
            let mut grid = Grid::new(4, 4);
            grid.set_text_runs(vec![epoch_probe_run()]);

            let before = grid.text_runs_epoch();
            change(&mut grid);

            assert!(
                grid.text_runs_epoch() > before,
                "{name} left text_runs_epoch at {before}"
            );
        }

        let overlays: [NamedChange; 4] = [
            ("set_overlays", |g| g.set_overlays(vec![])),
            ("fill_overlays", |g| {
                g.fill_overlays(1, |_| epoch_probe_overlay())
            }),
            ("resize", |g| g.resize(4, 4)),
            ("clear", |g| g.clear()),
        ];
        let minimaps: [NamedChange; 7] = [
            ("set_minimaps", |g| g.set_minimaps(vec![])),
            ("fill_minimaps", |g| {
                g.fill_minimaps(1, |_| epoch_probe_minimap())
            }),
            ("set_minimap_contents", |g| {
                g.set_minimap_contents(HashMap::new())
            }),
            ("splice_minimap_content", |g| {
                g.splice_minimap_content(7, 0, 0, &[epoch_probe_summary()])
            }),
            ("drop_minimap_content", |g| g.drop_minimap_content(7)),
            ("resize", |g| g.resize(4, 4)),
            ("clear", |g| g.clear()),
        ];
        for (name, change) in minimaps {
            let mut grid = Grid::new(4, 4);
            grid.set_minimaps(vec![epoch_probe_minimap()]);
            grid.splice_minimap_content(7, 0, 0, &[epoch_probe_summary()]);

            let before = grid.minimap_epoch();
            change(&mut grid);

            assert!(
                grid.minimap_epoch() > before,
                "{name} left minimap_epoch at {before}"
            );
        }

        for (name, change) in overlays {
            let mut grid = Grid::new(4, 4);
            grid.set_overlays(vec![epoch_probe_overlay()]);

            let before = grid.popovers_epoch();
            change(&mut grid);

            assert!(
                grid.popovers_epoch() > before,
                "{name} left popovers_epoch at {before}"
            );
        }
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
            seq: 0,
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
        // The sums run one past the heights, so line 3 reads the total where
        // line 4 is the first that has to extrapolate. Both sit where the
        // declared part ends, which is where an off-by-one would show.
        assert_eq!(
            grid.line_start_row(3),
            5,
            "the row the declared lines end on"
        );
        assert_eq!(grid.line_start_row(4), 6, "undeclared lines count as one");
        assert_eq!(grid.line_start_row(9), 11, "and go on counting as one");

        // Replacing a layout must not leave the previous sums behind it.
        grid.set_line_heights(vec![2]);
        assert_eq!(grid.line_start_row(1), 2, "the shorter layout replaced it");
        assert_eq!(
            grid.line_start_row(3),
            4,
            "and extrapolates from its own end"
        );

        grid.set_line_heights(Vec::new());
        assert_eq!(
            grid.line_start_row(3),
            3,
            "an empty layout is one row a line"
        );

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

        pool.fill(7, 0, 0).get_mut(1, 2).ch = 'Z';

        assert_eq!(pool.page(7).map(|g| g.get(1, 2).ch), Some('Z'));
        assert!(
            pool.page(3).is_none(),
            "index 3 shares a slot with 7, which holds it"
        );
    }

    #[test]
    fn page_pool_recycles_the_slot_a_slid_page_vacated() {
        let mut pool = PagePool::new(2, 2, 2);
        pool.fill(0, 0, 0).get_mut(0, 0).ch = 'a';
        pool.fill(1, 0, 0).get_mut(0, 0).ch = 'b';

        // Index 2 maps to index 0's slot (2 % 2 == 0), so it recycles 0's
        // buffer in place.
        let recycled = pool.fill(2, 0, 0);
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
        pool.fill(0, 0, 0).set_icons(vec![Icon {
            top: 0,
            left: 0,
            kind: IconKind::Error,
            color: Rgb::new(1, 2, 3),
            size: 1,
            offset: [0, 0],
            seq: 0,
        }]);

        assert!(
            pool.fill(1, 1, 1).icons().is_empty(),
            "recycling drops the prior page's decorations, even where the \
             incoming paint covers every cell and nothing is cleared",
        );
    }

    #[test]
    fn page_pool_rebuild_resizes_pages_and_drops_content() {
        let mut pool = PagePool::new(2, 2, 2);
        pool.fill(0, 0, 0);

        pool.rebuild(3, 5);

        assert!(pool.page(0).is_none(), "rebuild drops buffered pages");
        let page = pool.fill(0, 0, 0);
        assert_eq!(
            (page.rows(), page.cols()),
            (3, 5),
            "pages track the new viewport"
        );
    }

    /// A fill names the box its caller paints, so the paint does not write
    /// every cell twice over. Everything outside that box is still the last
    /// page's, and still has to go.
    #[test]
    fn a_fill_clears_around_the_box_its_caller_paints() {
        let mut pool = PagePool::new(3, 4, 2);
        let first = pool.fill(0, 0, 0);
        for row in 0..3 {
            for col in 0..4 {
                first.get_mut(row, col).ch = 'a';
            }
        }

        // Index 2 shares index 0's slot, and this fill paints only the top-left
        // two by two of it.
        let second = pool.fill(2, 2, 2);
        let kept = (0..2)
            .map(|row| {
                (0..2)
                    .map(|col| second.get(row, col).ch)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            kept,
            ["aa", "aa"],
            "the box is left for the paint to replace"
        );

        let page = pool.page(2).expect("the slot holds the page");
        let rows = (0..3)
            .map(|row| (0..4).map(|col| page.get(row, col).ch).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            ["aa  ", "aa  ", "    "],
            "the column tails and the rows past the box are cleared",
        );
    }

    /// A box larger than the grid names every cell, which is the ordinary fill:
    /// a page painted at the size of the slot it lands on.
    #[test]
    fn a_fill_box_past_the_grid_keeps_every_cell() {
        let mut pool = PagePool::new(2, 2, 1);
        let first = pool.fill(0, 0, 0);
        first.get_mut(1, 1).ch = 'z';

        let second = pool.fill(1, 9, 9);
        assert_eq!(
            second.get(1, 1).ch,
            'z',
            "nothing is cleared out from under a paint that covers the grid",
        );
    }

    #[test]
    fn page_pool_capacity_is_at_least_one() {
        let mut pool = PagePool::new(1, 1, 0);
        pool.fill(0, 0, 0).get_mut(0, 0).ch = 'x';
        assert_eq!(
            pool.page(0).map(|g| g.get(0, 0).ch),
            Some('x'),
            "a zero-capacity request still yields a usable slot"
        );
    }

    fn fill_page_rows(pool: &mut PagePool, index: u64, rows: &[char]) {
        let grid = pool.fill(index, 0, 0);
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
        // Seed it so a failed compose is shown to leave the caller's composite
        // intact rather than half-overwriting it.
        let mut out = Grid::new(3, 1);
        for row in 0..out.rows() {
            out.get_mut(row, 0).ch = 'Z';
        }
        assert!(!pool.compose(1, &mut out));
        assert_eq!(
            composed_rows(&out),
            ['Z', 'Z', 'Z'],
            "a failed compose leaves out untouched"
        );
    }

    #[test]
    fn compose_fails_above_the_first_page() {
        let mut pool = PagePool::new(2, 1, 4);
        fill_page_rows(&mut pool, 0, &['a', 'b']);

        let mut out = Grid::new(2, 1);
        assert!(!pool.compose(-1, &mut out));
    }
}

#[cfg(test)]
mod region_blit_tests {
    use super::{whole_row, Bar, Damage, Grid, Polyline, Rgb, RowDamage, TextRun};

    /// A 2x2 source with a sentinel in every cell and one of each decoration
    /// kind at the region's own origin.
    fn source() -> Grid {
        let mut grid = Grid::new(2, 2);
        for r in 0..grid.rows() {
            for c in 0..grid.cols() {
                grid.get_mut(r, c).ch = 'd';
            }
        }
        grid.set_text_runs(vec![TextRun {
            col: 0,
            row: 0,
            scale: 16,
            color: Rgb::new(1, 2, 3),
            bg: None,
            text: "x".into(),
            seq: 0,
        }]);
        grid.set_bars(vec![Bar {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
            color: Rgb::new(1, 2, 3),
            seq: 0,
        }]);
        grid.set_polylines(vec![Polyline {
            points: vec![[0, 0], [16, 16]],
            width: 16,
            color: Rgb::new(1, 2, 3),
            seq: 0,
        }]);
        grid
    }

    /// The renderer draws a window grid's polylines, and pool fills carry them,
    /// so a blit that carries the cells and drops the paths loses commit-graph
    /// lanes and merge edges with nothing to say it did.
    #[test]
    fn a_blit_carries_every_decoration_kind_translated() {
        let mut dst = Grid::new(4, 4);
        dst.blit_region(&source(), 1, 2, 2);

        assert_eq!(dst.get(1, 2).ch, 'd', "the cells land at the origin");
        assert_eq!(
            (dst.text_runs()[0].col, dst.text_runs()[0].row),
            (32, 16),
            "a text run moves by the origin in sixteenths"
        );
        assert_eq!(
            (dst.bars()[0].x, dst.bars()[0].y),
            (32, 16),
            "a bar moves with it"
        );
        assert_eq!(
            dst.polylines()[0].points,
            [[32, 16], [48, 32]],
            "every point of a path moves with it"
        );
    }

    /// A destination recomposed in place is overwritten row for row, and most of
    /// those rows come back holding what they already held. Marking only the
    /// ones that changed is what lets a renderer rebuild only those.
    #[test]
    fn an_append_marks_the_rows_it_changed_and_no_others() {
        let mut dst = Grid::new(4, 4);
        dst.append_region(&source(), 1, 2, 2, &mut Damage::Full);

        // The same source again, over the rows it already wrote.
        let mut damage = Damage::Partial(vec![None; 4]);
        dst.append_region(&source(), 1, 2, 2, &mut damage);
        assert_eq!(
            marked(&damage),
            Vec::<usize>::new(),
            "a copy that writes the bytes already there changes no row",
        );

        // One cell of the source moved, so one destination row did.
        let mut moved = source();
        moved.get_mut(1, 0).ch = 'z';
        let mut damage = Damage::Partial(vec![None; 4]);
        dst.append_region(&moved, 1, 2, 2, &mut damage);
        assert_eq!(
            marked(&damage),
            vec![2],
            "only the row the cell sits on, and over its whole width",
        );
        assert_eq!(
            damage_at(&damage, 2),
            whole_row(4),
            "the damage names the destination's width, not the region's",
        );
        assert_eq!(dst.get(2, 2).ch, 'z', "and the cell landed");
    }

    /// A caller that blanked the grid first has nothing to compare against, so
    /// it asks for none of this and the marking is a no-op.
    #[test]
    fn an_append_into_a_full_damage_marks_nothing_of_its_own() {
        let mut dst = Grid::new(4, 4);
        let mut damage = Damage::Full;
        dst.append_region(&source(), 1, 2, 2, &mut damage);

        assert!(
            matches!(damage, Damage::Full),
            "a full damage already covers every row and stays as it was",
        );
        assert_eq!(dst.get(1, 2).ch, 'd', "and the cells still land");
    }

    /// The rows a partial damage names, ascending.
    fn marked(damage: &Damage) -> Vec<usize> {
        match damage {
            Damage::Full => Vec::new(),
            Damage::Partial(rows) => rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.is_some())
                .map(|(at, _)| at)
                .collect(),
        }
    }

    fn damage_at(damage: &Damage, row: usize) -> RowDamage {
        match damage {
            Damage::Full => None,
            Damage::Partial(rows) => rows.get(row).copied().flatten(),
        }
    }

    #[test]
    fn an_append_carries_every_decoration_kind_translated() {
        let mut dst = Grid::new(4, 4);
        dst.append_region(&source(), 1, 2, 2, &mut Damage::Full);

        assert_eq!(dst.get(1, 2).ch, 'd', "the cells land at the origin");
        assert_eq!(
            (dst.text_runs()[0].col, dst.text_runs()[0].row),
            (32, 16),
            "a text run moves by the origin in sixteenths"
        );
        assert_eq!(
            (dst.bars()[0].x, dst.bars()[0].y),
            (32, 16),
            "a bar moves with it"
        );
        assert_eq!(
            dst.polylines()[0].points,
            [[32, 16], [48, 32]],
            "every point of a path moves with it"
        );
    }

    #[test]
    fn a_blit_replaces_the_decorations_it_finds_and_an_append_adds_to_them() {
        let counts = |mut dst: Grid, append: bool| {
            dst.set_polylines(vec![Polyline {
                points: vec![[99, 99]],
                width: 1,
                color: Rgb::new(0, 0, 0),
                seq: 0,
            }]);
            if append {
                dst.append_region(&source(), 0, 0, 2, &mut Damage::Full);
            } else {
                dst.blit_region(&source(), 0, 0, 2);
            }
            dst.polylines().len()
        };

        assert_eq!(
            [
                counts(Grid::new(4, 4), false),
                counts(Grid::new(4, 4), true)
            ],
            [1, 2],
            "one pool owns the list it writes, several each add their own"
        );
    }

    /// The renderer scissors the composite to the region, so what surrounds it
    /// is never drawn and must not be reblanked each frame either.
    #[test]
    fn a_blit_leaves_the_surround_as_it_found_it() {
        let mut dst = Grid::new(4, 4);
        for r in 0..dst.rows() {
            for c in 0..dst.cols() {
                dst.get_mut(r, c).ch = 's';
            }
        }

        dst.blit_region(&source(), 1, 1, 2);

        assert_eq!(
            [dst.get(0, 0).ch, dst.get(3, 3).ch, dst.get(1, 1).ch],
            ['s', 's', 'd'],
            "only the region's own cells are written"
        );
    }

    #[test]
    fn a_region_past_the_edge_clips_rather_than_panicking() {
        let mut dst = Grid::new(4, 4);
        dst.blit_region(&source(), 3, 3, 2);
        assert_eq!(dst.get(3, 3).ch, 'd', "the row and column that fit land");

        dst.blit_region(&source(), 0, 4, 2);
        assert_eq!(
            dst.get(0, 0).ch,
            ' ',
            "a region starting past the last column copies nothing"
        );
    }

    #[test]
    fn the_caller_says_how_many_rows_to_take() {
        let mut dst = Grid::new(4, 4);
        dst.blit_region(&source(), 0, 0, 1);

        assert_eq!(
            (dst.get(0, 0).ch, dst.get(1, 0).ch),
            ('d', ' '),
            "the straddle row a caller does not want is left out"
        );
    }
}

/// An image placed on the grid, as the renderer needs to draw it.
///
/// Carries its own pixels rather than an id into a store, so a render pass
/// reads one list and needs nothing else. The pixels are shared, so carrying
/// them costs a refcount rather than a copy of the image.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlacedImage {
    /// The image these pixels came from, so a consumer can tell two placements
    /// of one image apart from two images.
    pub image: u32,
    /// The client's id for this placement, or zero when it named none.
    pub placement: u32,
    /// Which transmission of [`Self::image`] this is, so a cache holding a
    /// placement can tell a re-transmission from the pixels it drew.
    pub generation: u64,
    /// Decoded RGBA, the whole image rather than the cropped part.
    pub rgba: Arc<[u8]>,
    /// The image's own pixel size, which [`Self::crop`] indexes into.
    pub width: u32,
    pub height: u32,
    /// Top-left cell of the box the image is drawn into.
    pub row: usize,
    pub col: usize,
    /// Size of that box in cells. The image stretches to fill it.
    pub cols: usize,
    pub rows: usize,
    /// The part of the source image to draw, as pixels. A zero width or height
    /// means the rest of the image from that edge.
    pub crop: ImageCrop,
    /// Pixel offset inside the top-left cell, so a client can place an image
    /// off the cell grid.
    pub offset_x: u32,
    pub offset_y: u32,
    /// Where the placement sits relative to the text. Negative draws behind it.
    pub z: i32,
}

/// The rectangle of a source image a placement draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ImageCrop {
    pub x: u32,
    pub y: u32,
    /// Zero means the rest of the image from [`Self::x`].
    pub width: u32,
    /// Zero means the rest of the image from [`Self::y`].
    pub height: u32,
}
