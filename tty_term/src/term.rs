//! The bytes-to-grid driver: a VT byte stream parsed onto the cell grid.
//!
//! [`Terminal`] wraps an `alacritty_terminal` terminal and its vte parser.
//! Bytes fed to [`Terminal::advance`] mutate the parsed screen, and
//! [`Terminal::project`] copies that screen onto a [`Grid`]. The copy resolves
//! each cell's terminal-palette color to concrete channels and touches only the
//! lines the terminal reports as damaged.

use crate::{
    grid::{
        Bar, Border, BorderStyle, Borders, Cell, Flags, Grid, Icon, IconKind, Overlay, PagePool,
        Rgb, Scale, ScrollRegion, TextRun, UnderlineStyle,
    },
    theme::Theme,
};
use alacritty_terminal::{
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    term::{
        cell::{Cell as TermCell, Flags as TermFlags},
        color::Colors,
        Config, RenderableCursor, TermDamage, TermMode,
    },
    vte::ansi::{Color, CursorShape as TermCursorShape, NamedColor, Processor},
    Term,
};
use parking_lot::Mutex;
use std::{mem, sync::Arc, time::Instant};
use stoatty_protocol::command::{
    self, BarCommand, BorderCommand, Command, IconCommand, LineLayoutCommand, PopoverCommand,
    ScaleCommand, ScrollRegionCommand, TextRunCommand,
};

const PALETTE_LEN: usize = 256;

/// Number of viewport-sized pages the smooth-scroll pool keeps buffered around
/// the scroll target.
///
/// Bounds the pool's memory. Large enough to cover the pages straddling the
/// viewport edges during a partial-cell scroll plus neighbours for momentum.
const PAGE_POOL_CAPACITY: usize = 5;

/// A live terminal driven by a VT byte stream.
///
/// Owns the parsed screen (an `alacritty_terminal` terminal) and the vte parser
/// that feeds it. No IO lives here: the app crate owns the PTY and pushes bytes
/// in via [`Self::advance`], then calls [`Self::project`] to refresh the render
/// grid.
///
/// Resolves a cell's indexed or named color against its [`Theme`] and the
/// 256-color palette derived from it. A color the program overrode (via OSC)
/// takes precedence over the theme.
pub struct Terminal {
    term: Term<ResponseSink>,
    /// Shares the `term`'s response buffer so [`Self::take_responses`] can drain
    /// the replies the terminal emits to host queries.
    responses: ResponseSink,
    parser: Processor,
    /// Color set the projection resolves named and default colors against.
    theme: Theme,
    palette: [Rgb; PALETTE_LEN],
    apc: ApcScanner,
    /// Recognizes XTVERSION queries the vte parser leaves unanswered, so
    /// [`Self::advance`] can reply with [`XTVERSION_REPLY`].
    xtversion: XtVersionScanner,
    /// Border regions set by `Gstoatty;border` frames, stamped onto the grid by
    /// [`Self::project`]. They persist until a `Gstoatty;reset` frame clears
    /// them, since the VT projection resets each cell's borders every frame.
    borders: Vec<BorderCommand>,
    /// Scale commands set by `Gstoatty;scale` frames, applied to the grid by
    /// [`Self::project`]. Like borders, they persist across the per-frame VT
    /// projection that resets each cell's scale.
    scales: Vec<ScaleCommand>,
    /// Popover regions set by `Gstoatty;popover` frames, applied to the grid's
    /// overlay list by [`Self::project`]. They float above the cells, so they
    /// are grid-level overlays rather than cell attributes.
    popovers: Vec<PopoverCommand>,
    /// The scrollable region set by `Gstoatty;scroll_region` frames, applied to
    /// the grid by [`Self::project`]. Unlike the other commands it does not
    /// accumulate: a region's scroll offset updates over time, so the latest
    /// frame replaces the prior one.
    scroll_region: Option<ScrollRegionCommand>,
    /// Status icons set by `Gstoatty;icon` frames, applied to the grid's icon
    /// list by [`Self::project`]. Like popovers they accumulate and are
    /// grid-level rather than cell attributes.
    icons: Vec<IconCommand>,
    /// Text runs set by `Gstoatty;text_run` frames, applied to the grid's
    /// text-run list by [`Self::project`]. Off-grid components, accumulated and
    /// grid-level like the icons.
    text_runs: Vec<TextRunCommand>,
    /// Color bars set by `Gstoatty;bar` frames, applied to the grid's bar list
    /// by [`Self::project`]. Off-grid components, accumulated and grid-level
    /// like the icons.
    bars: Vec<BarCommand>,
    /// The logical-line layout set by `Gstoatty;line_layout` frames, applied to
    /// the grid by [`Self::project`]. Replaced, not accumulated, like the scroll
    /// region: the latest layout wins.
    line_layout: Option<LineLayoutCommand>,
    /// Which decoration command lists changed since the last [`Self::project`],
    /// so a projection re-stamps only the components that changed rather than all
    /// of them every frame.
    decorations_dirty: DecorationDirty,
    /// The grid rows the cell-stamped decorations (borders, scales) occupied at
    /// the previous [`Self::project`], so a moved or cleared decoration can damage
    /// the rows it used to cover and erase its stale footprint.
    last_decoration_footprint: Vec<bool>,
    /// Accumulated renderer-facing decoration row-damage since the renderer last
    /// drained it via [`Self::take_decoration_damage`]. Distinct from VT
    /// [`Damage`]: it marks rows where an APC border or scale changed, which the
    /// cell-decoration passes gate their per-row rebuilds on.
    decoration_damage: Vec<bool>,
    /// Scrollback line count at the previous [`Self::project`], so the next one
    /// can report how many rows the content scrolled since.
    last_history: usize,
    /// Recycled pool of viewport-sized rich pages the app pushes around its
    /// scroll target, read by the renderer to ease smooth scrolling between
    /// app-declared positions. Rebuilt on resize so pages track the live
    /// viewport. Distinct from the per-frame projected [`Grid`].
    page_pool: PagePool,
    /// The in-progress page fill, set while a `Gstoatty;fill` open marker has
    /// redirected the VT write path onto a pool slot.
    ///
    /// Streamed bytes paint this isolated context's screen instead of the live
    /// grid until the redirect closes (a `fill_end`, the next `fill`, or a
    /// `reset`), when the painted page is committed onto [`Self::page_pool`].
    /// `None` while writing the live grid.
    fill: Option<FillTarget>,
}

/// Per-component "changed since last projection" flags for the accumulated APC
/// decorations, so [`Terminal::project`] re-stamps only what changed.
///
/// Set by [`Terminal::apply_command`] when a command arrives (and all set by
/// [`Terminal::clear_decorations`], which empties every list), cleared once a
/// projection has applied them.
#[derive(Default)]
struct DecorationDirty {
    borders: bool,
    scales: bool,
    popovers: bool,
    scroll_region: bool,
    icons: bool,
    line_layout: bool,
    text_runs: bool,
    bars: bool,
}

impl DecorationDirty {
    /// Every component marked changed, for a reset that empties all lists.
    fn all() -> DecorationDirty {
        DecorationDirty {
            borders: true,
            scales: true,
            popovers: true,
            scroll_region: true,
            icons: true,
            line_layout: true,
            text_runs: true,
            bars: true,
        }
    }
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

/// The isolated VT context a `Gstoatty;fill` redirect paints a page into.
///
/// Holds its own [`Term`] and parser so the streamed page content mutates a
/// private screen, with its own cursor and parser state, while the live terminal
/// stays untouched. On commit the screen's cells are projected onto the pool
/// slot for [`Self::index`].
struct FillTarget {
    index: u64,
    term: Term<ResponseSink>,
    parser: Processor,
}

impl FillTarget {
    /// Create a `rows` by `cols` fill context for document page `index`, with a
    /// blank screen ready to receive the page's streamed bytes.
    fn new(index: u64, rows: usize, cols: usize) -> FillTarget {
        let term = Term::new(
            Config::default(),
            &GridSize { rows, cols },
            ResponseSink::default(),
        );

        FillTarget {
            index,
            term,
            parser: Processor::new(),
        }
    }
}

impl Terminal {
    /// Create a `rows` by `cols` terminal with an empty screen, resolving colors
    /// against `theme`.
    pub fn new(rows: usize, cols: usize, theme: Theme) -> Terminal {
        let responses = ResponseSink::default();
        let term = Term::new(
            Config::default(),
            &GridSize { rows, cols },
            responses.clone(),
        );
        let palette = default_palette(&theme);

        Terminal {
            term,
            responses,
            parser: Processor::new(),
            theme,
            palette,
            apc: ApcScanner::default(),
            xtversion: XtVersionScanner::default(),
            borders: Vec::new(),
            scales: Vec::new(),
            popovers: Vec::new(),
            scroll_region: None,
            icons: Vec::new(),
            text_runs: Vec::new(),
            bars: Vec::new(),
            line_layout: None,
            decorations_dirty: DecorationDirty::default(),
            last_decoration_footprint: Vec::new(),
            decoration_damage: Vec::new(),
            last_history: 0,
            page_pool: PagePool::new(rows, cols, PAGE_POOL_CAPACITY),
            fill: None,
        }
    }

    /// Feed `bytes` of the VT stream into the parser, mutating the screen.
    ///
    /// Returns whether the screen changed visibly and a redraw is warranted. It
    /// returns `false` while a DEC 2026 synchronized update is buffering -- when
    /// the whole chunk went into the parser's sync buffer rather than the screen
    /// -- so the caller can skip presenting the frozen frame until the update
    /// flushes (on ESU or via [`Self::flush_synchronized_update`] at the timeout).
    ///
    /// Bytes need not be escape-sequence aligned; the parser retains a partial
    /// sequence across calls.
    ///
    /// Each stoatty `Gstoatty` APC frame in the stream is decoded and applied
    /// before the bytes reach the parser. Outside a fill redirect the bytes are
    /// still fed to the parser verbatim: alacritty consumes the APC string and
    /// ignores it, so feeding it is harmless and avoids the desync that removing
    /// bytes would risk.
    ///
    /// A `Gstoatty;fill` open marker redirects the bytes that follow onto an
    /// isolated page-painting context instead of the live screen, until the
    /// matching `fill_end` (or the next `fill`/`reset`) commits the page. The
    /// chunk is then split at the marker boundaries to route each segment to the
    /// live or the fill parser.
    ///
    /// XTVERSION queries (`CSI > Ps q`) are answered here too. The vte parser
    /// dispatches every other host query, but not this one, so the driver
    /// recognizes it and buffers [`XTVERSION_REPLY`] for [`Self::take_responses`].
    pub fn advance(&mut self, bytes: &[u8]) -> bool {
        let redirecting = self.fill.is_some();

        // The APC and XTVERSION scanners only act on ESC-prefixed sequences, so
        // when neither holds a partial frame a chunk with no ESC carries nothing
        // for them. A SIMD memchr for the first ESC lets the bulk of plain output
        // (cat, yes) skip the two per-byte scans, leaving only the vte parse. A
        // fill redirect must route every byte, so it forgoes the fast path.
        let scan = if !redirecting && self.apc.is_idle() && self.xtversion.is_idle() {
            memchr::memchr(ESC, bytes).map(|esc| &bytes[esc..])
        } else {
            Some(bytes)
        };

        let Some(scan) = scan else {
            self.parser.advance(&mut self.term, bytes);
            return self.parser.sync_bytes_count() < bytes.len();
        };

        let frames: Vec<(Option<Command>, usize)> = self
            .apc
            .scan(scan)
            .into_iter()
            .map(|(payload, end)| (command::decode(&payload), end))
            .collect();

        for _ in 0..self.xtversion.scan(scan) {
            self.responses.push(XTVERSION_REPLY.as_bytes());
        }

        // Without a fill redirect every byte targets the live screen, so apply
        // the commands and feed the whole chunk verbatim, preserving the
        // synchronized-update accounting the redirect path cannot.
        let involves_fill = redirecting
            || frames
                .iter()
                .any(|(command, _)| matches!(command, Some(Command::Fill(_))));
        if !involves_fill {
            for (command, _) in frames {
                if let Some(command) = command {
                    self.apply_command(command);
                }
            }

            self.parser.advance(&mut self.term, bytes);

            // A redraw is warranted unless the whole chunk was held in the
            // parser's synchronized-update buffer (nothing reached the screen).
            return self.parser.sync_bytes_count() < bytes.len();
        }

        // A fill redirect splits the chunk at frame boundaries: each segment up
        // to and including a marker is routed to the target active before that
        // marker, then the marker's command flips the target for the next
        // segment. The marker's own APC bytes are ignored by whichever parser
        // consumes them. `prefix` rebases the scan-relative offsets when a memchr
        // skip left a plain head bound for the live screen.
        let prefix = bytes.len() - scan.len();
        let mut start = 0;
        for (command, end) in frames {
            self.feed_segment(&bytes[start..prefix + end]);
            start = prefix + end;

            if let Some(command) = command {
                // Mid-redirect only the fill-control commands act. A decoration
                // command in a page's stream is page-targeted, so applying it
                // would leak onto the live grid; it is dropped to keep the live
                // state untouched while a page paints.
                // FIXME: capture page-targeted decorations onto the page grid
                // rather than dropping them.
                let control = matches!(
                    command,
                    Command::Fill(_) | Command::FillEnd | Command::Reset
                );
                if control || self.fill.is_none() {
                    self.apply_command(command);
                }
            }
        }
        self.feed_segment(&bytes[start..]);

        true
    }

    /// The instant the in-progress synchronized update must be flushed by, or
    /// `None` when no update is buffering.
    ///
    /// A DEC 2026 update buffers bytes until ESU, but a missing or slow ESU would
    /// freeze the screen, so the host loop must flush at this deadline via
    /// [`Self::flush_synchronized_update`].
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Apply the buffered synchronized-update bytes and end the update.
    ///
    /// Called by the host loop when [`Self::sync_deadline`] passes; a no-op when
    /// no update is buffering. ESU within the stream flushes on its own.
    pub fn flush_synchronized_update(&mut self) {
        self.parser.stop_sync(&mut self.term);
    }

    /// Take the bytes the terminal wants written back to the PTY, leaving none
    /// buffered.
    ///
    /// Host queries fed to [`Self::advance`] (device attributes, XTVERSION,
    /// device-status and cursor-position reports, keyboard-mode queries) produce
    /// replies the shell blocks on; the caller must write them back to the PTY
    /// for an interactive shell to start. Returns empty when the stream held no
    /// query.
    pub fn take_responses(&mut self) -> Vec<u8> {
        self.responses.take()
    }

    /// Drain the decoration row-damage accumulated since the last drain.
    ///
    /// Marks the rows where an APC border or scale changed across the projections
    /// since the previous call, so the renderer's cell-decoration passes rebuild
    /// only those rows. Distinct from the VT [`Damage`] returned by
    /// [`Self::project`]; the caller drains this right after projecting.
    pub fn take_decoration_damage(&mut self) -> Damage {
        Damage::Partial(mem::take(&mut self.decoration_damage))
    }

    /// Apply a decoded stoatty command to the terminal.
    ///
    /// The seam every feature sub-code hooks into. A border command is recorded
    /// and stamped onto the grid by [`Self::project`], since it persists across
    /// frames while the VT projection rewrites cells.
    fn apply_command(&mut self, command: Command) {
        match command {
            Command::Border(border) => {
                self.borders.push(border);
                self.decorations_dirty.borders = true;
            },
            Command::Scale(scale) => {
                self.scales.push(scale);
                self.decorations_dirty.scales = true;
            },
            Command::Popover(popover) => {
                self.popovers.push(popover);
                self.decorations_dirty.popovers = true;
            },
            Command::ScrollRegion(region) => {
                self.scroll_region = Some(region);
                self.decorations_dirty.scroll_region = true;
            },
            Command::Icon(icon) => {
                self.icons.push(icon);
                self.decorations_dirty.icons = true;
            },
            Command::TextRun(text_run) => {
                self.text_runs.push(text_run);
                self.decorations_dirty.text_runs = true;
            },
            Command::Bar(bar) => {
                self.bars.push(bar);
                self.decorations_dirty.bars = true;
            },
            Command::LineLayout(layout) => {
                self.line_layout = Some(layout);
                self.decorations_dirty.line_layout = true;
            },
            Command::Fill(fill) => self.begin_fill(fill.index),
            Command::FillEnd => self.commit_fill(),
            // A reset is also a fill close trigger: commit any open page before
            // clearing decoration state and restoring the live grid.
            Command::Reset => {
                self.commit_fill();
                self.clear_decorations();
            },
        }
    }

    /// Open a page-fill redirect onto the pool slot for document page `index`.
    ///
    /// Any already-open fill is committed first, so a dropped `fill_end` cannot
    /// strand the redirect: the next `fill` (or a `reset`) closes the previous
    /// page. The fresh context is sized to the live viewport, matching the pool
    /// slots [`Self::commit_fill`] writes into.
    fn begin_fill(&mut self, index: u64) {
        self.commit_fill();
        let rows = self.term.screen_lines();
        let cols = self.term.columns();
        self.fill = Some(FillTarget::new(index, rows, cols));
    }

    /// Commit the open page fill onto its pool slot and restore the live grid.
    ///
    /// Projects the fill context's painted cells onto the recycled slot for its
    /// page index. A no-op when no fill is open, so every close trigger
    /// (`fill_end`, the next `fill`, `reset`) can call it unconditionally.
    fn commit_fill(&mut self) {
        let Some(fill) = self.fill.take() else {
            return;
        };

        let grid = self.page_pool.fill(fill.index);
        project_term_cells(grid, &fill.term, &self.theme, &self.palette);
    }

    /// Route a run of VT bytes to the active write target: the open fill
    /// context's parser when a redirect is in effect, otherwise the live one.
    fn feed_segment(&mut self, segment: &[u8]) {
        match &mut self.fill {
            Some(fill) => fill.parser.advance(&mut fill.term, segment),
            None => self.parser.advance(&mut self.term, segment),
        }
    }

    /// Clear all accumulated stoatty decoration state.
    ///
    /// A `Gstoatty;reset` frame lands here. Without it the per-frame decoration
    /// lists only grow, since the VT projection re-stamps them every frame, so a
    /// program that redraws a frame at a new position would leave the old one
    /// behind. Resetting lets a program redraw its decoration scene from scratch.
    fn clear_decorations(&mut self) {
        self.borders.clear();
        self.scales.clear();
        self.popovers.clear();
        self.icons.clear();
        self.text_runs.clear();
        self.bars.clear();
        self.scroll_region = None;
        self.line_layout = None;
        // Every list emptied, so the next projection must re-apply all of them to
        // clear the grid of what they previously stamped.
        self.decorations_dirty = DecorationDirty::all();
    }

    /// Resize the terminal to `rows` by `cols`.
    ///
    /// The next [`Self::project`] finds its grid no longer matches and repaints
    /// it wholesale at the new size, so the grid follows without a separate call.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.term.resize(GridSize { rows, cols });
        self.page_pool.rebuild(rows, cols);
        // The pool is rebuilt empty for the new viewport, so any half-painted
        // page is abandoned; the app re-pushes its pages at the new size.
        self.fill = None;
    }

    /// Move the viewport `delta` lines through scrollback history: positive
    /// scrolls up toward older output, negative scrolls back down toward the
    /// live bottom, clamped to the saved history.
    ///
    /// The offset change fully damages the screen, so the next [`Self::project`]
    /// repaints the scrolled-back view through the usual path.
    pub fn scroll_display(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    /// Reset the viewport to the live bottom of history, so the next
    /// [`Self::project`] shows current output again. Used to pin the view on
    /// keyboard input.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Whether the alternate screen is active: a fullscreen app (a pager, an
    /// editor) holds it and owns its own scrolling.
    pub fn is_alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Whether alternate-scroll (DECSET 1007) is on, so wheel motion on the
    /// alternate screen should drive the app's arrow keys rather than local
    /// scrollback.
    pub fn alternate_scroll(&self) -> bool {
        self.term.mode().contains(TermMode::ALTERNATE_SCROLL)
    }

    /// Whether the program enabled any mouse reporting, so wheel events should
    /// be reported to it rather than scrolling the local viewport.
    pub fn mouse_mode(&self) -> bool {
        self.term.mode().intersects(TermMode::MOUSE_MODE)
    }

    /// Whether SGR mouse encoding (DECSET 1006) is on, selecting the SGR form
    /// for a mouse report.
    pub fn sgr_mouse(&self) -> bool {
        self.term.mode().contains(TermMode::SGR_MOUSE)
    }

    /// Copy the parsed screen onto `grid` and return the cursor, the number of
    /// rows the content scrolled since the previous call, and which rows changed.
    ///
    /// Only lines the terminal reports as damaged since the previous call are
    /// rewritten, so an unchanged line keeps whatever the prior projection left
    /// in `grid`. When `grid`'s dimensions do not match the terminal it is first
    /// resized, which clears it, and every line is treated as damaged.
    ///
    /// The scroll delta is the growth in scrollback since the previous call: the
    /// rows live output pushed off the top. It is the renderer's signal to ease
    /// vertical scrolling. It is reported as zero while the user has scrolled
    /// back (a non-zero display offset), since the viewport is then pinned to its
    /// content as history grows and must not glide, and it saturates to zero once
    /// the scrollback history fills.
    ///
    /// The returned [`Damage`] reports the VT cell rows this call rewrote. It does
    /// not account for the stoatty APC overlays (borders, scales, popovers, icons,
    /// bars, line layout, text runs) re-stamped every projection, so a consumer
    /// caching by row must treat those as able to change any row.
    pub fn project(&mut self, grid: &mut Grid) -> (Cursor, usize, Damage) {
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

            *grid.get_mut(row, col) =
                project_cell(indexed.cell, content.colors, &self.theme, &self.palette);
        }

        let cursor = project_cursor(content.cursor, offset);

        // Re-stamp a decoration only when it changed, when a resize cleared the
        // grid's lists and cells, or -- for the per-cell borders and scales --
        // when any row's cells were reset by VT damage above. Text runs and bars
        // resolve their row through the line layout, so they also re-stamp when
        // the layout changed. A `Damage::Partial([])` idle frame re-stamps none.
        let vt_damaged = match &dirty {
            Damage::Full => true,
            Damage::Partial(rows) => rows.iter().any(|&row| row),
        };
        let layout_changed = self.decorations_dirty.line_layout || resized;

        if self.decorations_dirty.borders || resized || vt_damaged {
            apply_borders(grid, &self.borders);
        }
        if self.decorations_dirty.scales || resized || vt_damaged {
            apply_scales(grid, &self.scales);
        }
        if self.decorations_dirty.popovers || resized {
            apply_popovers(grid, &self.popovers);
        }
        if self.decorations_dirty.scroll_region || resized {
            apply_scroll_region(grid, self.scroll_region);
        }
        if self.decorations_dirty.icons || resized {
            apply_icons(grid, &self.icons);
        }
        if layout_changed {
            apply_line_layout(grid, self.line_layout.as_ref());
        }
        if self.decorations_dirty.text_runs || layout_changed {
            apply_text_runs(grid, &self.text_runs);
        }
        if self.decorations_dirty.bars || layout_changed {
            apply_bars(grid, &self.bars);
        }

        // Accumulate renderer-facing decoration row-damage for the cell-stamped
        // borders and scales: when one changed, damage the rows it covers now and
        // the rows it covered last projection, so the cell-decoration passes
        // rebuild the new footprint and erase a moved or cleared one. A VT
        // re-stamp re-applies the same decorations, leaving this signal untouched.
        if self.last_decoration_footprint.len() != rows {
            self.last_decoration_footprint = vec![false; rows];
        }
        if self.decoration_damage.len() != rows {
            self.decoration_damage = vec![false; rows];
        }
        let footprint = decoration_footprint(&self.borders, &self.scales, rows);
        if self.decorations_dirty.borders || self.decorations_dirty.scales || resized {
            for ((damage, &now), &before) in self
                .decoration_damage
                .iter_mut()
                .zip(&footprint)
                .zip(&self.last_decoration_footprint)
            {
                if now || before {
                    *damage = true;
                }
            }
        }
        self.last_decoration_footprint = footprint;

        self.decorations_dirty = DecorationDirty::default();

        let history = self.term.history_size();
        let grew = history.saturating_sub(self.last_history);
        self.last_history = history;
        // A non-zero display offset means the user has scrolled back: the
        // viewport is pinned to its content as history grows, so report no
        // scroll and leave the renderer's auto-scroll ease idle.
        let scrolled = if offset > 0 { 0 } else { grew };

        self.term.reset_damage();
        (cursor, scrolled, dirty)
    }

    /// Resolve which rows [`Self::project`] must rewrite this frame.
    ///
    /// `force_full` short-circuits to [`Damage::Full`] when the grid was just
    /// resized and holds no valid prior content, bypassing the terminal's own
    /// damage which may report only a partial change.
    fn collect_damage(&mut self, rows: usize, force_full: bool) -> Damage {
        if force_full {
            return Damage::Full;
        }

        match self.term.damage() {
            TermDamage::Full => Damage::Full,
            TermDamage::Partial(lines) => {
                let mut rows_dirty = vec![false; rows];
                for bounds in lines {
                    if let Some(slot) = rows_dirty.get_mut(bounds.line) {
                        *slot = true;
                    }
                }
                Damage::Partial(rows_dirty)
            },
        }
    }
}

/// Captures the bytes the terminal wants written back to the PTY.
///
/// `alacritty_terminal` reports replies to host queries (device attributes,
/// device-status and cursor-position reports, keyboard-mode queries) as
/// [`Event::PtyWrite`] events through its [`EventListener`]. The trait method
/// takes `&self`, so the buffer lives behind a shared [`Mutex`]: the `Term`
/// holds the listener while the owning [`Terminal`] keeps a clone to drain.
/// Other event variants (title, clipboard, bell) are dropped.
///
/// The buffer is [`Arc`]/[`Mutex`] rather than `Rc`/`RefCell` so [`Terminal`]
/// stays [`Send`], letting the app parse the byte stream on a thread off the
/// render loop.
#[derive(Clone, Default)]
struct ResponseSink {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl ResponseSink {
    /// Drain the buffered response bytes, leaving the buffer empty.
    fn take(&self) -> Vec<u8> {
        mem::take(&mut *self.bytes.lock())
    }

    /// Append `bytes` to the buffer, for a reply the driver synthesizes itself
    /// (XTVERSION) rather than receiving from the `Term` listener.
    fn push(&self, bytes: &[u8]) {
        self.bytes.lock().extend_from_slice(bytes);
    }
}

impl EventListener for ResponseSink {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            self.bytes.lock().extend_from_slice(text.as_bytes());
        }
    }
}

const ESC: u8 = 0x1b;
/// Byte after `ESC` that opens an APC string (`ESC _`).
const APC_INTRODUCER: u8 = b'_';
/// Byte after `ESC` that closes a string control (`ESC \`, the ST).
const STRING_TERMINATOR: u8 = b'\\';
/// Bell, accepted as an alternate string terminator.
const BEL: u8 = 0x07;

/// Cap on a buffered APC payload, bounding memory against an APC string that
/// never terminates. Stoatty frames are far smaller, so an overrun is discarded.
const MAX_APC_BYTES: usize = 64 * 1024;

/// Extracts APC string payloads from a VT byte stream as they complete.
///
/// `alacritty_terminal` consumes APC strings without surfacing them, so the
/// driver watches the bytes itself: this tracks the `ESC _ ... ESC \` (or
/// `BEL`) framing across [`Terminal::advance`] calls and yields each completed
/// payload, the bytes between the introducer and the terminator. Recognizing a
/// stoatty frame among the payloads is the decoder's job, not this scanner's.
#[derive(Default)]
struct ApcScanner {
    state: ApcState,
    payload: Vec<u8>,
}

#[derive(Clone, Copy, Default)]
enum ApcState {
    #[default]
    Ground,
    Escape,
    Apc,
    ApcEscape,
}

impl ApcScanner {
    /// Whether the scanner holds no partial frame, so a caller may skip feeding
    /// it a chunk that contains no `ESC` without missing a frame.
    fn is_idle(&self) -> bool {
        matches!(self.state, ApcState::Ground)
    }

    /// Feed `bytes`, returning every APC payload that completes within them,
    /// each paired with the byte offset one past its terminator.
    ///
    /// The offset lets a caller split the input at frame boundaries to route the
    /// content between frames. A payload split across calls is retained until its
    /// terminator arrives; its offset is then relative to the call it completes
    /// in.
    fn scan(&mut self, bytes: &[u8]) -> Vec<(Vec<u8>, usize)> {
        let mut payloads = Vec::new();

        for (i, &byte) in bytes.iter().enumerate() {
            match self.state {
                ApcState::Ground => {
                    if byte == ESC {
                        self.state = ApcState::Escape;
                    }
                },
                ApcState::Escape => {
                    self.state = match byte {
                        APC_INTRODUCER => {
                            self.payload.clear();
                            ApcState::Apc
                        },
                        ESC => ApcState::Escape,
                        _ => ApcState::Ground,
                    };
                },
                ApcState::Apc => match byte {
                    ESC => self.state = ApcState::ApcEscape,
                    BEL => {
                        payloads.push((mem::take(&mut self.payload), i + 1));
                        self.state = ApcState::Ground;
                    },
                    _ => self.push(byte),
                },
                ApcState::ApcEscape => match byte {
                    STRING_TERMINATOR => {
                        payloads.push((mem::take(&mut self.payload), i + 1));
                        self.state = ApcState::Ground;
                    },
                    ESC => self.state = ApcState::ApcEscape,
                    _ => {
                        self.payload.clear();
                        self.state = ApcState::Ground;
                    },
                },
            }
        }

        payloads
    }

    /// Buffer one payload byte, abandoning the frame if it overruns the cap.
    fn push(&mut self, byte: u8) {
        if self.payload.len() < MAX_APC_BYTES {
            self.payload.push(byte);
        } else {
            self.payload.clear();
            self.state = ApcState::Ground;
        }
    }
}

/// The XTVERSION reply naming this terminal, answering `CSI > Ps q`.
///
/// A DCS string (`ESC P > | name ESC \`) carrying the terminal name and version,
/// the response xterm defined for XTVERSION. Programs such as fish query it at
/// startup and gate optional features on the answer; the vte parser does not
/// dispatch it, so the driver synthesizes this reply itself.
const XTVERSION_REPLY: &str = concat!("\x1bP>|stoatty(", env!("CARGO_PKG_VERSION"), ")\x1b\\");

/// Recognizes XTVERSION queries (`CSI > Ps q`) in a VT byte stream.
///
/// The vte parser dispatches `CSI Ps q` (DECSCUSR) but not the `>`-prefixed
/// XTVERSION form, so the driver watches the bytes for it the way [`ApcScanner`]
/// watches for APC frames, tracking the `ESC [ > ... q` framing across
/// [`Terminal::advance`] calls. The parameter bytes are ignored since the reply
/// is fixed; a `>`-CSI ending in any other final byte (such as the `\x1b[>4;1m`
/// modify-other-keys sequence) is not mistaken for a query.
#[derive(Default)]
struct XtVersionScanner {
    state: CsiState,
}

#[derive(Clone, Copy, Default)]
enum CsiState {
    #[default]
    Ground,
    Escape,
    /// Seen `ESC [`, waiting on the `>` that marks the private query.
    CsiEntry,
    /// Seen `ESC [ >`, consuming parameter bytes until the final byte.
    CsiGt,
}

impl XtVersionScanner {
    /// Whether the scanner holds no partial query, so a caller may skip a chunk
    /// that contains no `ESC` without missing a query.
    fn is_idle(&self) -> bool {
        matches!(self.state, CsiState::Ground)
    }

    /// Feed `bytes`, returning how many XTVERSION queries completed within them.
    ///
    /// A query split across calls is retained until its final `q` arrives.
    fn scan(&mut self, bytes: &[u8]) -> usize {
        let mut hits = 0;

        for &byte in bytes {
            self.state = match self.state {
                CsiState::Ground => match byte {
                    ESC => CsiState::Escape,
                    _ => CsiState::Ground,
                },
                CsiState::Escape => match byte {
                    b'[' => CsiState::CsiEntry,
                    ESC => CsiState::Escape,
                    _ => CsiState::Ground,
                },
                CsiState::CsiEntry => match byte {
                    b'>' => CsiState::CsiGt,
                    _ => CsiState::Ground,
                },
                CsiState::CsiGt => match byte {
                    // CSI parameter and intermediate bytes keep the sequence open.
                    0x20..=0x3f => CsiState::CsiGt,
                    b'q' => {
                        hits += 1;
                        CsiState::Ground
                    },
                    ESC => CsiState::Escape,
                    _ => CsiState::Ground,
                },
            };
        }

        hits
    }
}

/// The set of viewport rows a projection rewrote, returned by
/// [`Terminal::project`] so a renderer can rebuild only the rows that changed.
///
/// [`Damage::Full`] means every row changed (a resize or a terminal-reported
/// full damage); [`Damage::Partial`] carries a per-row flag indexed by row.
pub enum Damage {
    Full,
    Partial(Vec<bool>),
}

impl Damage {
    /// Whether `row` changed this projection. Rows past the flag vector, and
    /// every row under [`Damage::Full`], read as dirty.
    pub fn is_dirty(&self, row: usize) -> bool {
        match self {
            Damage::Full => true,
            Damage::Partial(rows) => rows.get(row).copied().unwrap_or(false),
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

/// Project a fill context's on-screen cells onto a page grid.
///
/// The pool clears the slot before this runs, so it copies each on-screen cell
/// without damage tracking, resolving colors exactly as [`Terminal::project`]
/// does for the live grid. Cells past the page's bounds are skipped.
fn project_term_cells(
    grid: &mut Grid,
    term: &Term<ResponseSink>,
    theme: &Theme,
    palette: &[Rgb; PALETTE_LEN],
) {
    let content = term.renderable_content();
    let offset = content.display_offset as i32;

    for indexed in content.display_iter {
        let row = indexed.point.line.0 + offset;
        if row < 0 {
            continue;
        }

        let (row, col) = (row as usize, indexed.point.column.0);
        if row >= grid.rows() || col >= grid.cols() {
            continue;
        }

        *grid.get_mut(row, col) = project_cell(indexed.cell, content.colors, theme, palette);
    }
}

fn project_cell(
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
        borders: Borders::default(),
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

fn named_color(
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

fn indexed(index: usize, overrides: &Colors, palette: &[Rgb; PALETTE_LEN]) -> Rgb {
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

/// The grid rows the cell-stamped decorations occupy: each border region's
/// perimeter span (`top..=top+height-1`) and each scale block's rows
/// (`top..top+scale`), clamped to `rows`.
///
/// Mirrors the row ranges [`frame_region`] and [`apply_scales`] stamp, so the
/// decoration-damage signal covers exactly the rows those appliers touch.
fn decoration_footprint(
    borders: &[BorderCommand],
    scales: &[ScaleCommand],
    rows: usize,
) -> Vec<bool> {
    let mut footprint = vec![false; rows];

    for border in borders {
        if border.width == 0 || border.height == 0 {
            continue;
        }
        let top = border.top as usize;
        if top >= rows {
            continue;
        }
        let bottom = (top + border.height as usize - 1).min(rows - 1);
        footprint[top..=bottom].fill(true);
    }

    for scale in scales {
        let top = scale.top as usize;
        let end = (top + scale.scale as usize).min(rows);
        if top < end {
            footprint[top..end].fill(true);
        }
    }

    footprint
}

/// Stamp every stored border region's perimeter edges onto `grid`.
///
/// Runs each projection because the cell projection resets borders to none;
/// edges outside the grid are skipped so a region may extend past it.
fn apply_borders(grid: &mut Grid, commands: &[BorderCommand]) {
    for command in commands {
        frame_region(grid, command);
    }
}

fn frame_region(grid: &mut Grid, command: &BorderCommand) {
    if command.width == 0 || command.height == 0 {
        return;
    }

    let border = Border {
        style: grid_border_style(command.style),
        color: Rgb::new(command.color[0], command.color[1], command.color[2]),
    };

    let rows = grid.rows();
    let cols = grid.cols();
    let top = command.top as usize;
    let left = command.left as usize;
    let bottom = top + command.height as usize - 1;
    let right = left + command.width as usize - 1;

    for col in left..=right.min(cols.saturating_sub(1)) {
        if top < rows {
            grid.get_mut(top, col).borders.top = Some(border);
        }
        if bottom < rows {
            grid.get_mut(bottom, col).borders.bottom = Some(border);
        }
    }

    for row in top..=bottom.min(rows.saturating_sub(1)) {
        if left < cols {
            grid.get_mut(row, left).borders.left = Some(border);
        }
        if right < cols {
            grid.get_mut(row, right).borders.right = Some(border);
        }
    }
}

fn grid_border_style(style: command::BorderStyle) -> BorderStyle {
    match style {
        command::BorderStyle::Light => BorderStyle::Light,
        command::BorderStyle::Heavy => BorderStyle::Heavy,
        command::BorderStyle::Double => BorderStyle::Double,
        command::BorderStyle::Rounded => BorderStyle::Rounded,
    }
}

/// Claim each stored scale command's block on `grid`.
///
/// Runs each projection because the cell projection resets every cell to
/// [`Scale::Single`]. An origin outside the grid is skipped, since wire
/// coordinates are untrusted and may point past the screen.
fn apply_scales(grid: &mut Grid, commands: &[ScaleCommand]) {
    for command in commands {
        let (row, col) = (command.top as usize, command.left as usize);
        if row < grid.rows() && col < grid.cols() {
            grid.place_scaled(row, col, command.scale);
        }
    }
}

/// Replace the grid's overlay list with each stored popover command's region.
///
/// Overlays are grid-level rather than per-cell, so the full list is set each
/// projection rather than stamped per cell. The region is clamped or clipped by
/// the renderer, so out-of-grid anchors need no guard here.
fn apply_popovers(grid: &mut Grid, commands: &[PopoverCommand]) {
    let overlays = commands.iter().map(popover_overlay).collect();
    grid.set_overlays(overlays);
}

/// Set the grid's scrollable region from the stored command, or clear it.
///
/// Runs each projection like the other command appliers, since the grid's
/// scroll region is set rather than derived from cells. The renderer clamps or
/// clips an out-of-grid rectangle, so wire coordinates need no guard here.
fn apply_scroll_region(grid: &mut Grid, command: Option<ScrollRegionCommand>) {
    grid.set_scroll_region(command.map(|command| ScrollRegion {
        top: command.top,
        left: command.left,
        width: command.width,
        height: command.height,
        offset: command.offset,
    }));
}

/// Replace the grid's icon list with each stored icon command's icon.
///
/// Grid-level like the overlays, so the full list is set each projection rather
/// than stamped per cell. The renderer clamps an out-of-grid anchor, so wire
/// coordinates need no guard here.
fn apply_icons(grid: &mut Grid, commands: &[IconCommand]) {
    let icons = commands
        .iter()
        .map(|command| Icon {
            top: command.top,
            left: command.left,
            kind: grid_icon_kind(command.kind),
            color: Rgb::new(command.color[0], command.color[1], command.color[2]),
            size: command.size,
        })
        .collect();
    grid.set_icons(icons);
}

fn grid_icon_kind(kind: command::IconKind) -> IconKind {
    match kind {
        command::IconKind::Error => IconKind::Error,
        command::IconKind::Warning => IconKind::Warning,
        command::IconKind::Info => IconKind::Info,
    }
}

/// Apply the stored logical-line layout to the grid, or clear it when none is
/// set, so [`apply_text_runs`] and [`apply_bars`] can resolve against it.
fn apply_line_layout(grid: &mut Grid, command: Option<&LineLayoutCommand>) {
    grid.set_line_heights(
        command
            .map(|command| command.heights.clone())
            .unwrap_or_default(),
    );
}

/// Replace the grid's text-run list with each stored text-run command's run.
///
/// Grid-level like the overlays, so the full list is set each projection rather
/// than stamped per cell. The declared row is a logical row resolved through the
/// line layout, so a run tracks expansions above it. The renderer clamps an
/// out-of-grid anchor, so wire coordinates need no guard here.
fn apply_text_runs(grid: &mut Grid, commands: &[TextRunCommand]) {
    let text_runs = commands
        .iter()
        .map(|command| TextRun {
            col: command.col,
            row: resolve_logical_row(grid, command.row),
            scale: command.scale,
            color: Rgb::new(command.color[0], command.color[1], command.color[2]),
            bg: Rgb::new(command.bg[0], command.bg[1], command.bg[2]),
            text: command.text.clone(),
        })
        .collect();
    grid.set_text_runs(text_runs);
}

/// Replace the grid's bar list with each stored bar command's rectangle.
///
/// Grid-level like the overlays, so the full list is set each projection rather
/// than stamped per cell. The declared `y` is a logical row resolved through the
/// line layout, so a bar tracks expansions above it.
fn apply_bars(grid: &mut Grid, commands: &[BarCommand]) {
    let bars = commands
        .iter()
        .map(|command| Bar {
            x: command.x,
            y: resolve_logical_row(grid, command.y),
            width: command.width,
            height: command.height,
            color: Rgb::new(command.color[0], command.color[1], command.color[2]),
        })
        .collect();
    grid.set_bars(bars);
}

/// Resolve a component's declared logical row, in sixteenth-cell units, to the
/// physical row it sits on by adding the whole-row expansion above its line.
///
/// A negative row is off the top with no logical line, so it passes through.
fn resolve_logical_row(grid: &Grid, row: i16) -> i16 {
    if row < 0 {
        return row;
    }

    let logical_line = (row / 16) as usize;
    let expansion = grid
        .line_start_row(logical_line)
        .saturating_sub(logical_line);
    let shift = i16::try_from(expansion.saturating_mul(16)).unwrap_or(i16::MAX);
    row.saturating_add(shift)
}

fn popover_overlay(command: &PopoverCommand) -> Overlay {
    Overlay {
        top: command.top,
        left: command.left,
        width: command.width,
        height: command.height,
        fill: Rgb::new(command.fill[0], command.fill[1], command.fill[2]),
        border: Rgb::new(command.border[0], command.border[1], command.border[2]),
        content_fg: Rgb::new(
            command.content_fg[0],
            command.content_fg[1],
            command.content_fg[2],
        ),
        scale: command.scale,
        offset: command.offset,
        content: command.content.clone(),
    }
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

/// Build the 256-color palette for `theme`.
///
/// Indices 0..16 are the theme's ANSI colors, 16..232 the 6x6x6 color cube, and
/// 232..256 the 24-step grayscale ramp.
fn default_palette(theme: &Theme) -> [Rgb; PALETTE_LEN] {
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

#[cfg(test)]
mod tests {
    use super::{ApcScanner, Cursor, CursorShape, Terminal, XTVERSION_REPLY};
    use crate::{
        grid::{
            Bar, Border, BorderStyle, Cell, Flags, Grid, Icon, IconKind, Overlay, Rgb, Scale,
            ScrollRegion, TextRun, UnderlineStyle,
        },
        theme::Theme,
    };
    use stoatty_protocol::command::{
        encode_bar, encode_border, encode_fill, encode_fill_end, encode_icon, encode_line_layout,
        encode_popover, encode_reset, encode_scale, encode_scroll_region, encode_text_run,
        BarCommand, BorderCommand, BorderStyle as ProtoBorderStyle, FillCommand, IconCommand,
        IconKind as ProtoIconKind, LineLayoutCommand, PopoverCommand, ScaleCommand,
        ScrollRegionCommand, TextRunCommand,
    };

    fn project(rows: usize, cols: usize, bytes: &[u8]) -> (Grid, Cursor) {
        let mut terminal = Terminal::new(rows, cols, Theme::default());
        let mut grid = Grid::new(rows, cols);

        terminal.advance(bytes);
        let (cursor, _scroll, _damage) = terminal.project(&mut grid);

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
    fn projects_underline_style_and_color() {
        let (grid, _) = project(1, 3, b"\x1b[4:3;58:2::0:255:0mU");
        let cell = grid.get(0, 0);

        assert_eq!(cell.ch, 'U');
        assert_eq!(cell.underline, UnderlineStyle::Curly);
        assert_eq!(cell.underline_color, Rgb::new(0, 255, 0));
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
    fn project_resolves_colors_against_theme() {
        let theme = Theme {
            foreground: Rgb::new(4, 5, 6),
            background: Rgb::new(1, 2, 3),
            cursor: Rgb::new(7, 8, 9),
            ansi: [Rgb::new(10, 11, 12); 16],
        };
        let mut terminal = Terminal::new(1, 4, theme);
        let mut grid = Grid::new(1, 4);

        terminal.advance(b"a\x1b[31mb");
        terminal.project(&mut grid);

        assert_eq!(
            grid.get(0, 0).fg,
            Rgb::new(4, 5, 6),
            "default fg from theme"
        );
        assert_eq!(
            grid.get(0, 0).bg,
            Rgb::new(1, 2, 3),
            "default bg from theme"
        );
        assert_eq!(
            grid.get(0, 1).fg,
            Rgb::new(10, 11, 12),
            "ANSI red from theme palette"
        );
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
    fn captures_host_query_responses_for_the_pty() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b[6n");
        assert_eq!(
            terminal.take_responses(),
            b"\x1b[1;1R".to_vec(),
            "cursor position report"
        );

        terminal.advance(b"\x1b[c");
        assert_eq!(
            terminal.take_responses(),
            b"\x1b[?6c".to_vec(),
            "primary device attributes"
        );

        assert!(
            terminal.take_responses().is_empty(),
            "buffer drained after taking"
        );
    }

    #[test]
    fn answers_da1_param_zero_form() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b[0c");
        assert_eq!(
            terminal.take_responses(),
            b"\x1b[?6c".to_vec(),
            "the param-0 DA1 form fish sends is answered like the bare form"
        );
    }

    #[test]
    fn answers_xtversion_query() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b[>0q");
        assert_eq!(terminal.take_responses(), XTVERSION_REPLY.as_bytes());
    }

    #[test]
    fn does_not_mistake_other_private_csi_for_xtversion() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        // modifyOtherKeys (`CSI > 4 ; 1 m`) is a `>`-prefixed CSI that does not
        // end in `q`, so it must not draw an XTVERSION reply.
        terminal.advance(b"\x1b[>4;1m");
        assert!(terminal.take_responses().is_empty());
    }

    #[test]
    fn answers_fish_startup_handshake() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        // fish's startup burst: kitty-keyboard query (unanswered by default),
        // XTVERSION, then DA1 as its sentinel. The replies come back in order.
        terminal.advance(b"\x1b[?u\x1b[>0q\x1b[0c");

        let mut expected = XTVERSION_REPLY.as_bytes().to_vec();
        expected.extend_from_slice(b"\x1b[?6c");
        assert_eq!(terminal.take_responses(), expected);
    }

    #[test]
    fn xtversion_query_split_across_advances_is_answered() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b[>0");
        assert!(terminal.take_responses().is_empty(), "query incomplete");

        terminal.advance(b"q");
        assert_eq!(terminal.take_responses(), XTVERSION_REPLY.as_bytes());
    }

    #[test]
    fn project_reports_rows_scrolled() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        // Four lines into a two-row screen push the top two off into history.
        terminal.advance(b"a\r\nb\r\nc\r\nd");
        let (_, scrolled, _) = terminal.project(&mut grid);
        assert_eq!(scrolled, 2, "rows scrolled into history");

        // A projection with no new output reports no scroll.
        let (_, scrolled, _) = terminal.project(&mut grid);
        assert_eq!(scrolled, 0, "no further scroll");
    }

    #[test]
    fn scroll_display_moves_the_viewport_into_history_and_back() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        // Four lines into a two-row screen push "a" and "b" into history.
        terminal.advance(b"a\r\nb\r\nc\r\nd");
        terminal.project(&mut grid);
        assert_eq!(
            (grid.get(0, 0).ch, grid.get(1, 0).ch),
            ('c', 'd'),
            "the live view shows the bottom of output"
        );

        terminal.scroll_display(1);
        terminal.project(&mut grid);
        assert_eq!(
            (grid.get(0, 0).ch, grid.get(1, 0).ch),
            ('b', 'c'),
            "scrolling up one line slides the prior line into view"
        );

        terminal.scroll_to_bottom();
        terminal.project(&mut grid);
        assert_eq!(
            (grid.get(0, 0).ch, grid.get(1, 0).ch),
            ('c', 'd'),
            "scroll_to_bottom restores the live view"
        );
    }

    #[test]
    fn project_reports_no_scroll_while_scrolled_back() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        terminal.advance(b"a\r\nb\r\nc\r\nd");
        terminal.project(&mut grid);

        // Scrolled back, output grows history but the pinned view must not ease.
        terminal.scroll_display(1);
        terminal.advance(b"\r\ne\r\nf");
        let (_, scrolled, _) = terminal.project(&mut grid);
        assert_eq!(scrolled, 0, "no auto-scroll while the view is pinned back");

        // Back at the bottom, live growth counts again.
        terminal.scroll_to_bottom();
        terminal.advance(b"\r\ng");
        let (_, scrolled, _) = terminal.project(&mut grid);
        assert!(scrolled > 0, "live growth resumes counting at the bottom");
    }

    #[test]
    fn mode_queries_reflect_terminal_mode() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        // alacritty enables alternate-scroll by default; the rest start off.
        assert!(!terminal.is_alt_screen(), "alt screen off at startup");
        assert!(
            terminal.alternate_scroll(),
            "alternate scroll on by default"
        );
        assert!(!terminal.mouse_mode(), "mouse reporting off at startup");
        assert!(!terminal.sgr_mouse(), "sgr mouse off at startup");

        // Enter the alt screen (1049), enable mouse click reporting (1000) and
        // SGR encoding (1006), and turn alternate scroll off (1007), so every
        // query flips from its startup value.
        terminal.advance(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?1007l");
        assert!(terminal.is_alt_screen(), "alt screen on");
        assert!(!terminal.alternate_scroll(), "alternate scroll off");
        assert!(terminal.mouse_mode(), "mouse reporting on");
        assert!(terminal.sgr_mouse(), "sgr mouse on");
    }

    #[test]
    fn project_skips_undamaged_rows() {
        let mut terminal = Terminal::new(3, 4, Theme::default());
        let mut grid = Grid::new(3, 4);

        terminal.advance(b"AB\r\nCD");
        terminal.project(&mut grid);
        assert_eq!(grid.get(1, 1).ch, 'D');

        grid.get_mut(2, 0).ch = 'Z';

        terminal.advance(b"E");
        let (_, _, damage) = terminal.project(&mut grid);

        assert_eq!(grid.get(1, 2).ch, 'E');
        assert_eq!(grid.get(2, 0).ch, 'Z');
        assert_eq!(grid.get(0, 0).ch, 'A');

        assert!(
            damage.is_dirty(1),
            "the row 'E' landed on is reported damaged"
        );
        assert!(
            !damage.is_dirty(2),
            "the untouched row is undamaged, so its manual 'Z' survives"
        );
    }

    #[test]
    fn project_resizes_grid_to_terminal() {
        let mut terminal = Terminal::new(2, 6, Theme::default());
        let mut grid = Grid::new(1, 1);

        terminal.advance(b"hello");
        terminal.project(&mut grid);

        assert_eq!((grid.rows(), grid.cols()), (2, 6));
        assert_eq!(grid.get(0, 0).ch, 'h');
    }

    #[test]
    fn resize_propagates_to_grid_on_next_project() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        terminal.advance(b"hi");
        terminal.project(&mut grid);
        assert_eq!((grid.rows(), grid.cols()), (2, 4));

        terminal.resize(5, 10);
        terminal.project(&mut grid);

        assert_eq!((grid.rows(), grid.cols()), (5, 10));
    }

    #[test]
    fn scans_single_apc_frame() {
        let mut scanner = ApcScanner::default();

        assert_eq!(
            scanner.scan(b"\x1b_Gstoatty;border\x1b\\"),
            vec![(b"Gstoatty;border".to_vec(), 19)]
        );
    }

    #[test]
    fn scans_frame_split_across_calls() {
        let mut scanner = ApcScanner::default();

        assert!(scanner.scan(b"\x1b_Gstoat").is_empty());
        assert_eq!(
            scanner.scan(b"ty;x\x1b\\"),
            vec![(b"Gstoatty;x".to_vec(), 6)]
        );
    }

    #[test]
    fn scans_bel_terminated_frame() {
        let mut scanner = ApcScanner::default();

        assert_eq!(scanner.scan(b"\x1b_foo\x07"), vec![(b"foo".to_vec(), 6)]);
    }

    #[test]
    fn scans_frame_between_text() {
        let mut scanner = ApcScanner::default();

        assert_eq!(
            scanner.scan(b"a\x1b_foo\x1b\\b"),
            vec![(b"foo".to_vec(), 8)]
        );
    }

    #[test]
    fn scans_two_frames_in_one_chunk() {
        let mut scanner = ApcScanner::default();

        assert_eq!(
            scanner.scan(b"\x1b_a\x1b\\\x1b_b\x1b\\"),
            vec![(b"a".to_vec(), 5), (b"b".to_vec(), 10)]
        );
    }

    #[test]
    fn csi_and_plain_text_yield_no_frames() {
        let mut scanner = ApcScanner::default();

        assert!(scanner.scan(b"hello\x1b[31mworld").is_empty());
    }

    #[test]
    fn apc_frame_is_not_rendered_as_text() {
        let (grid, _) = project(1, 8, b"\x1b_Gstoatty;border\x1b\\hi");

        assert_eq!(grid.get(0, 0).ch, 'h');
        assert_eq!(grid.get(0, 1).ch, 'i');
        assert_eq!(*grid.get(0, 2), Cell::default());
    }

    #[test]
    fn border_apc_frame_frames_the_region() {
        let frame = encode_border(&BorderCommand {
            top: 0,
            left: 0,
            width: 3,
            height: 2,
            style: ProtoBorderStyle::Light,
            color: [255, 0, 0],
        });

        let mut terminal = Terminal::new(2, 3, Theme::default());
        let mut grid = Grid::new(2, 3);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        let edge = Some(Border {
            style: BorderStyle::Light,
            color: Rgb::new(255, 0, 0),
        });
        assert_eq!(grid.get(0, 0).borders.top, edge);
        assert_eq!(grid.get(0, 0).borders.left, edge);
        assert_eq!(grid.get(1, 2).borders.bottom, edge);
        assert_eq!(grid.get(1, 2).borders.right, edge);
        assert_eq!(grid.get(1, 1).borders.top, None);
    }

    #[test]
    fn memchr_prescan_preserves_frame_and_query_detection() {
        let frame = encode_border(&BorderCommand {
            top: 0,
            left: 0,
            width: 3,
            height: 2,
            style: ProtoBorderStyle::Light,
            color: [255, 0, 0],
        });
        let edge = Some(Border {
            style: BorderStyle::Light,
            color: Rgb::new(255, 0, 0),
        });

        // An ESC-free chunk takes the memchr fast path; a frame in the next
        // chunk is still detected.
        let mut terminal = Terminal::new(2, 3, Theme::default());
        let mut grid = Grid::new(2, 3);
        terminal.advance(b"hi");
        terminal.advance(&frame);
        terminal.project(&mut grid);
        assert_eq!(grid.get(0, 0).borders.top, edge, "frame after plain output");

        // A query preceded by plain bytes in one chunk: memchr seeks to the ESC.
        let mut terminal = Terminal::new(2, 8, Theme::default());
        terminal.advance(b"ab\x1b[>0q");
        assert_eq!(
            terminal.take_responses(),
            XTVERSION_REPLY.as_bytes(),
            "query after a plain prefix"
        );
    }

    #[test]
    fn reset_clears_accumulated_borders() {
        let border = encode_border(&BorderCommand {
            top: 0,
            left: 0,
            width: 3,
            height: 2,
            style: ProtoBorderStyle::Light,
            color: [255, 0, 0],
        });

        let mut terminal = Terminal::new(2, 3, Theme::default());
        let mut grid = Grid::new(2, 3);
        terminal.advance(&border);
        terminal.advance(&encode_reset());
        terminal.project(&mut grid);

        assert_eq!(grid.get(0, 0).borders.top, None);
        assert_eq!(grid.get(0, 0).borders.left, None);
    }

    #[test]
    fn rounded_border_command_maps_to_rounded_style() {
        let frame = encode_border(&BorderCommand {
            top: 0,
            left: 0,
            width: 2,
            height: 2,
            style: ProtoBorderStyle::Rounded,
            color: [1, 2, 3],
        });

        let mut terminal = Terminal::new(2, 2, Theme::default());
        let mut grid = Grid::new(2, 2);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.get(0, 0).borders.top,
            Some(Border {
                style: BorderStyle::Rounded,
                color: Rgb::new(1, 2, 3),
            })
        );
    }

    #[test]
    fn scale_apc_frame_claims_the_block() {
        let frame = encode_scale(&ScaleCommand {
            top: 0,
            left: 0,
            scale: 2,
        });

        let mut terminal = Terminal::new(2, 2, Theme::default());
        let mut grid = Grid::new(2, 2);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(grid.get(0, 0).scale, Scale::Origin(2));
        assert_eq!(grid.get(0, 1).scale, Scale::Covered);
        assert_eq!(grid.get(1, 0).scale, Scale::Covered);
        assert_eq!(grid.get(1, 1).scale, Scale::Covered);
    }

    #[test]
    fn scroll_region_apc_frame_sets_and_replaces_the_region() {
        let region = |offset| ScrollRegion {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            offset,
        };
        let frame = |offset| {
            encode_scroll_region(&ScrollRegionCommand {
                top: 1,
                left: 2,
                width: 4,
                height: 3,
                offset,
            })
        };

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);

        terminal.advance(&frame(5));
        terminal.project(&mut grid);
        assert_eq!(grid.scroll_region(), Some(region(5)));

        // A later frame replaces the offset rather than adding a second region.
        terminal.advance(&frame(9));
        terminal.project(&mut grid);
        assert_eq!(grid.scroll_region(), Some(region(9)));
    }

    #[test]
    fn popover_apc_frame_sets_a_grid_overlay() {
        let frame = encode_popover(&PopoverCommand {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            fill: [10, 20, 30],
            border: [40, 50, 60],
            content_fg: [70, 80, 90],
            scale: 2,
            offset: [4, -2],
            content: "ok".to_owned(),
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.overlays(),
            [Overlay {
                top: 1,
                left: 2,
                width: 4,
                height: 3,
                fill: Rgb::new(10, 20, 30),
                border: Rgb::new(40, 50, 60),
                content_fg: Rgb::new(70, 80, 90),
                scale: 2,
                offset: [4, -2],
                content: "ok".to_owned(),
            }]
        );
    }

    #[test]
    fn icon_apc_frame_sets_a_grid_icon() {
        let frame = encode_icon(&IconCommand {
            top: 4,
            left: 1,
            kind: ProtoIconKind::Warning,
            color: [255, 200, 0],
            size: 2,
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.icons(),
            [Icon {
                top: 4,
                left: 1,
                kind: IconKind::Warning,
                color: Rgb::new(255, 200, 0),
                size: 2,
            }]
        );
    }

    #[test]
    fn text_run_apc_frame_sets_a_grid_text_run() {
        let frame = encode_text_run(&TextRunCommand {
            col: -8,
            row: 48,
            scale: 192,
            color: [150, 160, 170],
            bg: [24, 26, 32],
            text: "42".to_owned(),
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.text_runs(),
            [TextRun {
                col: -8,
                row: 48,
                scale: 192,
                color: Rgb::new(150, 160, 170),
                bg: Rgb::new(24, 26, 32),
                text: "42".to_owned(),
            }]
        );
    }

    #[test]
    fn bar_apc_frame_sets_a_grid_bar() {
        let frame = encode_bar(&BarCommand {
            x: -4,
            y: 32,
            width: 3,
            height: 16,
            color: [220, 50, 47],
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.bars(),
            [Bar {
                x: -4,
                y: 32,
                width: 3,
                height: 16,
                color: Rgb::new(220, 50, 47),
            }]
        );
    }

    #[test]
    fn line_layout_shifts_a_bound_component_past_an_expansion() {
        // Line 1 is three rows tall, so its two extra rows push logical line 3
        // down to physical row 5 (80 sixteenths).
        let layout = encode_line_layout(&LineLayoutCommand {
            heights: vec![1, 3, 1],
        });
        let run = encode_text_run(&TextRunCommand {
            col: 0,
            row: 48,
            scale: 256,
            color: [150, 160, 170],
            bg: [0, 0, 0],
            text: "4".to_owned(),
        });
        let bar = encode_bar(&BarCommand {
            x: 0,
            y: 48,
            width: 2,
            height: 16,
            color: [220, 50, 47],
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&layout);
        terminal.advance(&run);
        terminal.advance(&bar);
        terminal.project(&mut grid);

        assert_eq!(grid.text_runs()[0].row, 80, "run shifts down two rows");
        assert_eq!(grid.bars()[0].y, 80, "bar shifts down two rows");
    }

    #[test]
    fn border_re_stamps_on_a_vt_damaged_row() {
        let frame = encode_border(&BorderCommand {
            top: 0,
            left: 0,
            width: 3,
            height: 2,
            style: ProtoBorderStyle::Light,
            color: [255, 0, 0],
        });
        let edge = Some(Border {
            style: BorderStyle::Light,
            color: Rgb::new(255, 0, 0),
        });

        let mut terminal = Terminal::new(2, 3, Theme::default());
        let mut grid = Grid::new(2, 3);
        terminal.advance(&frame);
        terminal.project(&mut grid);
        assert_eq!(grid.get(0, 0).borders.top, edge);

        // Writing text damages row 0, so its cells are reset; the border must be
        // re-stamped even though no new border command arrived.
        terminal.advance(b"X");
        terminal.project(&mut grid);
        assert_eq!(grid.get(0, 0).ch, 'X');
        assert_eq!(
            grid.get(0, 0).borders.top,
            edge,
            "border re-stamped on the damaged row"
        );
    }

    #[test]
    fn text_runs_reresolve_when_only_the_line_layout_changes() {
        let run = encode_text_run(&TextRunCommand {
            col: 0,
            row: 48,
            scale: 256,
            color: [150, 160, 170],
            bg: [0, 0, 0],
            text: "4".to_owned(),
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&encode_line_layout(&LineLayoutCommand {
            heights: vec![1, 3, 1],
        }));
        terminal.advance(&run);
        terminal.project(&mut grid);
        assert_eq!(grid.text_runs()[0].row, 80, "run shifts past the tall line");

        // Flatten the layout without re-sending the run; it depends on the layout
        // so it must re-resolve to its unshifted row.
        terminal.advance(&encode_line_layout(&LineLayoutCommand {
            heights: vec![1, 1, 1],
        }));
        terminal.project(&mut grid);
        assert_eq!(
            grid.text_runs()[0].row,
            48,
            "run re-resolves when the layout flattens"
        );
    }

    #[test]
    fn resize_re_applies_decorations() {
        let frame = encode_border(&BorderCommand {
            top: 0,
            left: 0,
            width: 3,
            height: 2,
            style: ProtoBorderStyle::Light,
            color: [255, 0, 0],
        });
        let edge = Some(Border {
            style: BorderStyle::Light,
            color: Rgb::new(255, 0, 0),
        });

        let mut terminal = Terminal::new(2, 3, Theme::default());
        let mut grid = Grid::new(2, 3);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        // A resize clears the grid, so the border must be re-applied even with no
        // new command.
        terminal.resize(4, 6);
        terminal.project(&mut grid);
        assert_eq!((grid.rows(), grid.cols()), (4, 6));
        assert_eq!(
            grid.get(0, 0).borders.top,
            edge,
            "border re-applied after resize"
        );
    }

    #[test]
    fn idle_projection_keeps_grid_level_decorations() {
        let icon = encode_icon(&IconCommand {
            top: 1,
            left: 1,
            kind: ProtoIconKind::Warning,
            color: [255, 200, 0],
            size: 2,
        });

        let mut terminal = Terminal::new(4, 4, Theme::default());
        let mut grid = Grid::new(4, 4);
        terminal.advance(&icon);
        terminal.project(&mut grid);
        assert_eq!(grid.icons().len(), 1);

        // A projection with no new command and no damage skips re-applying the
        // icon, but the grid's list persists.
        terminal.project(&mut grid);
        assert_eq!(grid.icons().len(), 1, "icon survives an idle projection");
    }

    #[test]
    fn synchronized_update_buffers_until_esu() {
        let mut terminal = Terminal::new(2, 3, Theme::default());
        let mut grid = Grid::new(2, 3);

        terminal.advance(b"\x1b[?2026h");
        terminal.advance(b"AB");
        assert!(terminal.sync_deadline().is_some(), "update is buffering");
        terminal.project(&mut grid);
        assert_eq!(grid.get(0, 0).ch, ' ', "buffered content not yet on screen");

        terminal.advance(b"\x1b[?2026l");
        assert!(terminal.sync_deadline().is_none(), "ESU ended the update");
        terminal.project(&mut grid);
        assert_eq!(grid.get(0, 0).ch, 'A');
        assert_eq!(grid.get(0, 1).ch, 'B');
    }

    #[test]
    fn flush_synchronized_update_applies_buffered_bytes() {
        let mut terminal = Terminal::new(2, 3, Theme::default());
        let mut grid = Grid::new(2, 3);

        terminal.advance(b"\x1b[?2026hAB");
        assert!(terminal.sync_deadline().is_some());

        terminal.flush_synchronized_update();
        assert!(terminal.sync_deadline().is_none(), "flush ended the update");
        terminal.project(&mut grid);
        assert_eq!(grid.get(0, 0).ch, 'A');
        assert_eq!(grid.get(0, 1).ch, 'B');
    }

    #[test]
    fn advance_reports_no_redraw_while_buffering() {
        let mut terminal = Terminal::new(2, 3, Theme::default());

        assert!(terminal.advance(b"hi"), "normal output warrants a redraw");
        assert!(
            terminal.advance(b"\x1b[?2026h"),
            "the BSU chunk itself is not all buffered"
        );
        assert!(
            !terminal.advance(b"X"),
            "a fully buffered chunk warrants no redraw"
        );
        assert!(
            terminal.advance(b"\x1b[?2026l"),
            "the ESU flush warrants a redraw"
        );
    }

    #[test]
    fn fill_paints_page_and_spares_live_grid() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        let mut stream = encode_fill(&FillCommand { index: 0 });
        stream.extend_from_slice(b"hi");
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        let page = terminal.page_pool.page(0).expect("page 0 buffered");
        assert_eq!((page.get(0, 0).ch, page.get(0, 1).ch), ('h', 'i'));

        terminal.project(&mut grid);
        assert_eq!(
            grid.get(0, 0).ch,
            ' ',
            "page content never reaches the live grid"
        );
    }

    #[test]
    fn fill_persists_across_advance_calls() {
        let mut terminal = Terminal::new(2, 4, Theme::default());

        terminal.advance(&encode_fill(&FillCommand { index: 3 }));
        terminal.advance(b"ab");
        terminal.advance(&encode_fill_end());

        let page = terminal.page_pool.page(3).expect("page 3 buffered");
        assert_eq!((page.get(0, 0).ch, page.get(0, 1).ch), ('a', 'b'));
    }

    #[test]
    fn next_fill_marker_auto_commits_the_previous_page() {
        let mut terminal = Terminal::new(2, 4, Theme::default());

        // No fill_end between the two pages: opening the second must commit the
        // first, so a dropped close cannot strand the redirect.
        let mut stream = encode_fill(&FillCommand { index: 0 });
        stream.extend_from_slice(b"AA");
        stream.extend_from_slice(&encode_fill(&FillCommand { index: 1 }));
        stream.extend_from_slice(b"BB");
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        let page0 = terminal.page_pool.page(0).expect("page 0 committed");
        assert_eq!(page0.get(0, 0).ch, 'A');
        let page1 = terminal.page_pool.page(1).expect("page 1 committed");
        assert_eq!(page1.get(0, 0).ch, 'B');
    }

    #[test]
    fn reset_commits_an_in_progress_page() {
        let mut terminal = Terminal::new(2, 4, Theme::default());

        let mut stream = encode_fill(&FillCommand { index: 2 });
        stream.extend_from_slice(b"zz");
        stream.extend_from_slice(&encode_reset());
        terminal.advance(&stream);

        let page = terminal
            .page_pool
            .page(2)
            .expect("reset committed the page");
        assert_eq!((page.get(0, 0).ch, page.get(0, 1).ch), ('z', 'z'));
    }

    #[test]
    fn fill_decoration_does_not_leak_to_the_live_grid() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        // A decoration command inside a page's stream is page-targeted; it must
        // not stamp the live grid.
        let mut stream = encode_fill(&FillCommand { index: 0 });
        stream.extend_from_slice(&encode_border(&BorderCommand {
            top: 0,
            left: 0,
            width: 2,
            height: 2,
            style: ProtoBorderStyle::Light,
            color: [255, 0, 0],
        }));
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        terminal.project(&mut grid);
        assert_eq!(
            grid.get(0, 0).borders.top,
            None,
            "page border spares the live grid"
        );
    }

    fn light_border(top: u16, height: u16) -> Vec<u8> {
        encode_border(&BorderCommand {
            top,
            left: 0,
            width: 3,
            height,
            style: ProtoBorderStyle::Light,
            color: [255, 0, 0],
        })
    }

    #[test]
    fn border_change_damages_its_rows() {
        let mut terminal = Terminal::new(4, 3, Theme::default());
        let mut grid = Grid::new(4, 3);
        terminal.advance(&light_border(1, 2));
        terminal.project(&mut grid);

        let damage = terminal.take_decoration_damage();
        assert!(!damage.is_dirty(0), "row above the border stays clean");
        assert!(damage.is_dirty(1), "border top row damaged");
        assert!(damage.is_dirty(2), "border bottom row damaged");
        assert!(!damage.is_dirty(3), "row below the border stays clean");
    }

    #[test]
    fn clearing_a_border_damages_its_prior_rows() {
        let mut terminal = Terminal::new(4, 3, Theme::default());
        let mut grid = Grid::new(4, 3);
        terminal.advance(&light_border(1, 2));
        terminal.project(&mut grid);
        terminal.take_decoration_damage();

        terminal.advance(&encode_reset());
        terminal.project(&mut grid);
        let damage = terminal.take_decoration_damage();
        assert!(damage.is_dirty(1), "cleared border's prior top row damaged");
        assert!(
            damage.is_dirty(2),
            "cleared border's prior bottom row damaged"
        );
    }

    #[test]
    fn scale_change_damages_its_block_rows() {
        let mut terminal = Terminal::new(4, 3, Theme::default());
        let mut grid = Grid::new(4, 3);
        terminal.advance(&encode_scale(&ScaleCommand {
            top: 1,
            left: 0,
            scale: 2,
        }));
        terminal.project(&mut grid);

        let damage = terminal.take_decoration_damage();
        assert!(!damage.is_dirty(0));
        assert!(damage.is_dirty(1), "scale block top row damaged");
        assert!(damage.is_dirty(2), "scale block bottom row damaged");
        assert!(!damage.is_dirty(3));
    }

    #[test]
    fn unchanged_projection_yields_no_decoration_damage() {
        let mut terminal = Terminal::new(4, 3, Theme::default());
        let mut grid = Grid::new(4, 3);
        terminal.advance(&light_border(1, 2));
        terminal.project(&mut grid);
        terminal.take_decoration_damage();

        terminal.project(&mut grid);
        let damage = terminal.take_decoration_damage();
        assert!(
            (0..4).all(|row| !damage.is_dirty(row)),
            "an unchanged projection damages no decoration rows"
        );
    }

    #[test]
    fn vt_damage_alone_yields_no_decoration_damage() {
        let mut terminal = Terminal::new(4, 3, Theme::default());
        let mut grid = Grid::new(4, 3);
        terminal.advance(&light_border(0, 2));
        terminal.project(&mut grid);
        terminal.take_decoration_damage();

        // Writing text re-stamps the same border onto the reset cells; the border
        // instances are unchanged, so no decoration damage.
        terminal.advance(b"X");
        terminal.project(&mut grid);
        let damage = terminal.take_decoration_damage();
        assert!(
            (0..4).all(|row| !damage.is_dirty(row)),
            "a VT re-stamp leaves the border unchanged"
        );
    }

    #[test]
    #[ignore = "throughput benchmark; run with: cargo test -p stoatty_term --lib -- --ignored advance_plain_throughput"]
    fn advance_plain_throughput() {
        let mut terminal = Terminal::new(50, 200, Theme::default());
        let mut buf = Vec::with_capacity(64 * 1024);
        while buf.len() < 64 * 1024 {
            buf.extend_from_slice(b"the quick brown fox jumps over the lazy dog 0123456789\r\n");
        }

        let iterations = 400;
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            terminal.advance(&buf);
        }
        let per = start.elapsed() / iterations;

        eprintln!("advance() {}KB ESC-free: {per:?}/call", buf.len() / 1024);
    }
}
