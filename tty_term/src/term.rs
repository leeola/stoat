//! The bytes-to-grid driver: a VT byte stream parsed onto the cell grid.
//!
//! [`Terminal`] wraps an `alacritty_terminal` terminal and its vte parser.
//! Bytes fed to [`Terminal::advance`] mutate the parsed screen, and
//! [`Terminal::project`] copies that screen onto a [`Grid`]. The copy resolves
//! each cell's terminal-palette color to concrete channels and touches only the
//! lines the terminal reports as damaged.

/// What a projection rewrote, which every caller of [`Terminal::project`] reads
/// from here even though the type belongs to the grid it describes.
pub use crate::grid::Damage;
use crate::{
    grid::{
        self, from_command::StoredTextRun, whole_row, Cell, DocumentOffset, Flags, Grid,
        MinimapView, PagePool, PoolRegion, Rgb, RowDamage,
    },
    theme::Theme,
};
use alacritty_terminal::{
    event::{Event, EventListener, WindowSize},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionRange, SelectionType},
    term::{viewport_to_point, Config, TermDamage, TermMode},
    vte::ansi::{Handler, NamedColor, Processor, Rgb as VteRgb},
    Term,
};
use decorate::{
    apply_bars, apply_borders, apply_icons, apply_line_layout, apply_minimaps, apply_panels,
    apply_polylines, apply_popovers, apply_scales, apply_scroll_region, apply_sketches,
    apply_text_runs, decoration_footprint, stamp_pool_decorations,
};
use parking_lot::Mutex;
use project::{
    default_palette, detect_shift, indexed, mark_selection_change, named_color, project_cell,
    project_cursor, project_term_cells, row_bounds,
};
use scan::{notification_from_osc, EscEvent, EscScanner, ESC, XTVERSION_REPLY};
use std::{
    collections::{BTreeMap, HashMap},
    mem,
    ops::{Range, RangeInclusive},
    sync::Arc,
    time::Instant,
};
use stoatty_protocol::{
    command::{
        self, BarCommand, BorderCommand, Command, HelloCommand, IconCommand, IdentReply,
        LineLayoutCommand, LineSummary, MinimapCommand, MinimapLinesCommand, PanelCommand,
        PolylineCommand, PoolRegionCommand, PopoverCommand, ScaleCommand, ScrollRegionCommand,
        SketchCommand, TextRunCommand, WindowOpenCommand,
    },
    frame::FrameScratch,
    iterm, kitty,
};

mod decorate;
mod images;
mod project;
mod scan;

const PALETTE_LEN: usize = 256;

/// Number of viewport-sized pages the smooth-scroll pool keeps buffered around
/// the scroll target.
///
/// Bounds the pool's memory. Large enough to cover the pages straddling the
/// viewport edges during a partial-cell scroll plus neighbours for momentum.
const PAGE_POOL_CAPACITY: usize = 5;

/// Denominator the wire's sub-page scroll fraction is expressed over: a
/// `Gstoatty;scroll` fraction of `n` (a `u16`) means `n / 65536` of a page.
const FRACTION_SCALE: f32 = 65536.0;

/// How far past the live viewport a pool region is allowed to reach, per
/// dimension.
///
/// A pool legitimately exceeds the viewport, since its pages straddle the edges
/// during a partial-cell scroll, but not by much. The ceiling exists because
/// the region's dimensions are wire values a writer chooses, and each pool
/// eagerly builds [`PAGE_POOL_CAPACITY`] grids of them. At the `u16` maximum
/// that is hundreds of gigabytes and an abort, so a crafted file catted to the
/// terminal would end the session.
const MAX_REGION_VIEWPORTS: usize = 2;

/// How much streamed text one popover or text run may accumulate before the
/// capture is committed early.
///
/// Streamed content arrives outside the APC frame, so
/// [`MAX_APC_PAYLOAD`](stoatty_protocol::frame::MAX_APC_PAYLOAD) does not bound
/// it, and while a capture is open no byte reaches the live screen.
/// Every close path is something the writer sends, so an emitter that dies
/// mid-capture buffers its shell parent's entire remaining output behind a
/// screen that looks hung. Committing at the cap trades the tail of one
/// oversized run for a session that keeps rendering.
const MAX_CAPTURE_BYTES: usize = 1 << 20;

/// Entries one decoration list may hold between resets.
///
/// Only `Gstoatty;reset` empties these lists, so an emitter that re-stamps its
/// scene without ever sending one grows them without limit, and every
/// projection re-walks the whole list on the way to running out of memory. A
/// real scene stamps tens of decorations per frame, so the ceiling is far above
/// anything a working emitter reaches.
const MAX_DECORATIONS: usize = 4096;

/// Pools one terminal may hold at once.
///
/// A pool is created by any unseen wire id and survives `reset`, retired only
/// by an explicit `pool_drop`, so a writer looping fresh ids grows this for the
/// terminal's lifetime. Each pool also costs per-redraw work, since the
/// renderer walks all of them every frame. A real session runs one pool per
/// visible pane.
const MAX_POOLS: usize = 64;

/// Minimap content stores one terminal may hold at once.
///
/// Created by any unseen `content_id` and, like pools, retired only by an
/// explicit drop. A real session runs one store per minimap strip.
const MAX_MINIMAP_STORES: usize = 256;

/// Minimap thumb views one terminal may hold at once.
///
/// A view is created by any unseen `strip_id`, and only a reset retires the
/// ones the scene has stopped declaring, so a writer that loops ids without
/// ever resetting grows the map for the session. A real scene runs about one
/// strip per pane, the same order as [`MAX_POOLS`].
const MAX_MINIMAP_VIEWS: usize = 64;

/// Row-flag buffers [`Terminal::recycle_damage`] keeps for a later frame.
///
/// A frame carries two, its VT damage and its decoration damage, and hands both
/// back before the next one asks for either. The pair in flight and the pair
/// coming back therefore coexist, which is what this covers. A tighter bound
/// would drop a buffer the very next frame wants, and a looser one holds
/// allocations nothing is going to ask for.
const MAX_ROW_FLAGS_SPARE: usize = 4;

/// Store changes the journal holds before the projection gives up on replaying
/// them.
///
/// A projection drains the journal, so it holds a handful of entries in the
/// ordinary case. Nothing drains it while projections are skipped, though, and
/// they are skipped for a scrolled-back view or an occluded window: a chatty
/// child behind either accumulates one entry per splice for as long as that
/// lasts. Past this the entries cost more than the clone they exist to avoid,
/// and the clone reproduces them exactly, so nothing is lost by taking it.
const MAX_MINIMAP_JOURNAL: usize = 4096;

/// Line handles the journal's entries may hold between them.
///
/// The entry count alone bounds nothing: a splice replacing a whole store holds
/// a handle per line, so a few thousand of those are worth many times the clone
/// the journal exists to avoid. One store's worth is where the journal stops
/// being the cheaper of the two, since past it the journal holds more handles
/// than a full store does. A splice per edit carries a handful of lines, so
/// ordinary use is nowhere near.
const MAX_MINIMAP_JOURNAL_LINES: usize = MAX_MINIMAP_LINES;

/// Line summaries one minimap content store may hold.
///
/// A store grows by splices the writer sends, so without a ceiling one store id
/// is enough to exhaust memory. The bound is far above any real document.
const MAX_MINIMAP_LINES: usize = 2_000_000;

/// A minimap content-store change buffered for the next projection.
///
/// The projection replays these against the grid's stores in arrival order. See
/// [`Terminal::minimap_journal`] for the replay contract.
enum MinimapJournal {
    Splice(MinimapLinesCommand),
    Drop(u32),
}

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
    /// This terminal's own identity, replied to a program's [`Command::Hello`].
    /// `None` until the host installs it via [`Self::set_ident`]. While `None` a
    /// hello is still logged but not answered.
    ident: Option<IdentReply>,
    parser: Processor,
    /// Color set the projection resolves named and default colors against.
    theme: Theme,
    palette: [Rgb; PALETTE_LEN],
    /// Recognizes the sequences the vte parser leaves unreported.
    ///
    /// Those are stoatty's own APC frames, the XTVERSION queries
    /// [`Self::advance`] answers with [`XTVERSION_REPLY`], and the OSC 9 and
    /// OSC 777 notifications it surfaces as [`TermEvent::Notification`].
    esc: EscScanner,
    /// Reused buffer for one advance's decoded APC frames, so the busy decode
    /// path does not allocate a fresh Vec per chunk.
    ///
    /// Each frame is paired with the range its payload occupied in the scanned
    /// slice and the offset one past its terminator.
    frames_scratch: Vec<(Option<Command>, Range<usize>, usize)>,
    /// Reused per-argument decode buffers threaded through
    /// [`command::decode_with`], so the busy decode path allocates nothing per
    /// APC frame argument once warm.
    frame_scratch: FrameScratch,
    /// Whether a pool region past the viewport has already been reported.
    ///
    /// One line per session rather than per command, since the writer that
    /// sends an out-of-range region is liable to send a flood of them and a
    /// log entry each would be its own way to bring the session down.
    warned_region_clamp: bool,
    /// Whether a capture committed early at [`MAX_CAPTURE_BYTES`] has already
    /// been reported.
    ///
    /// Once per session for the same reason as [`Self::warned_region_clamp`]:
    /// the writer that overruns one capture is liable to overrun every
    /// following one.
    warned_capture_cap: bool,
    /// Whether a decoration dropped at [`MAX_DECORATIONS`] has already been
    /// reported.
    ///
    /// Once per session for the same reason as [`Self::warned_region_clamp`].
    /// The emitter that fills one list is the one re-stamping every frame, so
    /// it would otherwise log on every frame forever.
    warned_decoration_cap: bool,
    /// Whether a pool refused at [`MAX_POOLS`] has already been reported.
    warned_pool_cap: bool,
    /// Whether minimap input bounded at [`MAX_MINIMAP_STORES`],
    /// [`MAX_MINIMAP_LINES`], or [`MAX_MINIMAP_VIEWS`] has already been
    /// reported.
    ///
    /// One flag for all three because they are the same defence against the
    /// same writer, and the log line names which of them fired.
    warned_minimap_cap: bool,
    /// Border regions set by `Gstoatty;border` frames, stamped onto the grid by
    /// [`Self::project`]. They persist until a `Gstoatty;reset` frame clears
    /// them, since the VT projection resets each cell's borders every frame.
    borders: Vec<BorderCommand>,
    /// Panel regions set by `Gstoatty;panel` frames, applied to the grid's panel
    /// list by [`Self::project`]. They float above the cells like popovers, but
    /// their row footprint feeds decoration damage like a border's, so the chrome
    /// over live cells rebuilds when a panel appears, moves, or clears.
    panels: Vec<PanelCommand>,
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
    text_runs: Vec<StoredTextRun>,
    /// Color bars set by `Gstoatty;bar` frames, applied to the grid's bar list
    /// by [`Self::project`]. Off-grid components, accumulated and grid-level
    /// like the icons.
    bars: Vec<BarCommand>,
    /// Stroked paths set by `Gstoatty;polyline` frames, applied to the grid's
    /// polyline list by [`Self::project`]. Accumulated and grid-level like the
    /// bars.
    polylines: Vec<PolylineCommand>,
    /// Hand-drawn marks set by `Gstoatty;sketch_*` frames, applied to the
    /// grid's sketch list by [`Self::project`]. Accumulated and grid-level like
    /// the polylines.
    sketches: Vec<SketchCommand>,
    /// The logical-line layout set by `Gstoatty;line_layout` frames, applied to
    /// the grid by [`Self::project`]. Replaced, not accumulated, like the scroll
    /// region: the latest layout wins.
    line_layout: Option<LineLayoutCommand>,
    /// Per-component declaration-order seq, held in lockstep with the four
    /// accumulating decoration lists above so each grid component carries the
    /// z-order the renderer occludes by. Pushed and cleared exactly where its
    /// list is.
    panel_seq: Vec<u32>,
    icon_seq: Vec<u32>,
    text_run_seq: Vec<u32>,
    bar_seq: Vec<u32>,
    polyline_seq: Vec<u32>,
    sketch_seq: Vec<u32>,
    /// Declared minimap strips set by `Gstoatty;minimap` frames, applied to the
    /// grid by [`Self::project`]. A decoration cleared by `Gstoatty;reset` like a
    /// border, with a parallel [`Self::minimap_seq`] for z-order.
    minimaps: Vec<MinimapCommand>,
    minimap_seq: Vec<u32>,
    /// Minimap line-summary stores keyed by content id, spliced by
    /// `Gstoatty;minimap_lines`. Persistent and pool-like: untouched by
    /// `Gstoatty;reset`, retired only by `Gstoatty;minimap_drop`, so incremental
    /// splices need not resend the whole buffer.
    minimap_contents: HashMap<u32, Vec<LineSummary>>,
    /// Minimap viewport thumbs keyed by strip id, set by `Gstoatty;minimap_view`.
    /// Persistent like [`Self::minimap_contents`], so a scroll rides a small
    /// frame that moves the thumb without redeclaring the strip.
    minimap_views: HashMap<u32, MinimapView>,
    /// Whether a content-store splice or drop happened since the last
    /// [`Self::project`], so the projection replays [`Self::minimap_journal`] into
    /// the grid only when the stores changed rather than on every viewport frame.
    minimap_content_dirty: bool,
    /// Whether the grid's stores have to be rebuilt from these rather than by
    /// replaying [`Self::minimap_journal`].
    ///
    /// The replay rests on the grid's stores equalling these as of the last
    /// projection. Anything that breaks that has to say so here, and dropping
    /// journal entries rather than replaying them is what will: the entries a
    /// cap discards are the ones the grid never saw.
    minimap_reclone: bool,
    /// The store changes since the last [`Self::project`], in arrival order. The
    /// grid's stores equal these stores as of that projection, so replaying the
    /// journal against them reproduces the current stores exactly while cloning
    /// only each splice's lines rather than every store.
    minimap_journal: Vec<MinimapJournal>,
    /// Line handles [`Self::minimap_journal`]'s entries hold between them,
    /// summed as they are recorded. Emptied with the journal by
    /// [`Self::clear_minimap_journal`], which is the only thing that empties
    /// either.
    minimap_journal_lines: usize,
    /// Next seq to stamp, incremented per decoration and reset by a
    /// `Gstoatty;reset` frame. Starts at 1 so pool-composited content (seq 0)
    /// sorts below every declared decoration.
    decoration_seq: u32,
    /// Which decoration command lists changed since the last [`Self::project`],
    /// so a projection re-stamps only the components that changed rather than all
    /// of them every frame.
    decorations_dirty: DecorationDirty,
    /// Output has arrived since the last projection. Set from [`Terminal::advance`]
    /// when it reports a redraw, and taken by [`Terminal::take_damage_flag`] for a
    /// frame that renders something other than the projected grid.
    output_since_project: bool,
    /// Something has damaged the terminal since the last projection, so
    /// [`Terminal::project`] has damage worth collecting.
    ///
    /// Separate from [`Self::output_since_project`], which
    /// [`Terminal::take_damage_flag`] consumes. The damage it stands for outlives
    /// that read, accumulating until a projection collects it, so one flag cannot
    /// serve both.
    ///
    /// A projection with this clear skips reading the terminal's damage at all.
    /// That is the point of the flag. The terminal damages the cursor's line on
    /// every read, so an animation frame over unchanged content would otherwise
    /// reproject and re-upload that row.
    ///
    /// Set wherever the terminal's own damage is marked. That is output arriving,
    /// a resize, and a scrollback move, which fully damages the screen on any
    /// change of display offset.
    damage_pending: bool,
    /// The grid rows the cell-stamped decorations (borders, scales) occupied when one
    /// of them last changed, so a moved or cleared decoration can damage the rows it
    /// used to cover and erase its stale footprint.
    ///
    /// Also the current footprint on any frame between changes, since the rows a
    /// decoration covers only move when the decoration does. That is what lets
    /// [`Self::project`] leave this alone rather than rebuild it every frame.
    last_decoration_footprint: Vec<bool>,
    /// Scratch [`Self::project`] builds the new decoration footprint in, so a change
    /// compares against [`Self::last_decoration_footprint`] without allocating.
    ///
    /// Swapped with it once the comparison is done, so the two alternate roles and a
    /// steady-state projection allocates neither. Holds a superseded footprint
    /// between changes, which no reader sees because a rebuild overwrites the whole
    /// buffer.
    footprint_scratch: Vec<bool>,
    /// The decoration lists the last projection stamped, so a redeclare of the
    /// same scene can be recognised and skipped.
    projected_decorations: ProjectedDecorations,
    /// One projected row, reused across the rows of a projection.
    ///
    /// A scrolled frame projects each row here first so it can be compared
    /// against what the grid already holds, and only rows that differ are
    /// written back. Reusing it keeps that comparison from allocating a row per
    /// row per frame.
    row_scratch: Vec<Cell>,
    /// Row-flag buffers handed back by [`Terminal::recycle_damage`], for the next
    /// frame that needs one.
    ///
    /// Bounded at [`MAX_ROW_FLAGS_SPARE`], because a frame gives its buffers
    /// back whether or not anything took one: an animation frame over unchanged
    /// content reads no damage and so asks for no buffer, and the pool would
    /// grow by that frame's pair for as long as the animation ran.
    row_flags_spare: Vec<Vec<RowDamage>>,
    /// The byte buffer an open content capture streams into, held here between
    /// captures so a run of them shares one allocation.
    ///
    /// A gutter emits a text run per visible line and each opens its own
    /// capture, so allocating per capture would allocate per line per frame.
    capture_scratch: Vec<u8>,
    /// The selection as it stood at the previous [`Self::project`], so the next
    /// one damages the rows whose overlay moved. `None` when there was no
    /// selection.
    ///
    /// The range rather than the rows it spans. A drag inside one row moves the
    /// overlay without moving the span, and the terminal deliberately leaves
    /// the selection out of its own damage for exactly this comparison.
    last_selection: Option<SelectionRange>,
    /// Accumulated renderer-facing decoration row-damage since the renderer last
    /// drained it via [`Self::take_decoration_damage`]. Distinct from VT
    /// [`Damage`]: it marks rows where an APC border or scale changed, which the
    /// cell-decoration passes gate their per-row rebuilds on.
    decoration_damage: Vec<RowDamage>,
    /// Scrollback line count at the previous [`Self::project`], so the next one
    /// can report how many rows the content scrolled since.
    last_history: usize,
    /// Smooth-scroll pools keyed by id: each a declared region plus the recycled
    /// pages buffered around its scroll target and that target itself.
    ///
    /// Several pools scroll independently and compose in ascending-id z-order,
    /// so split panes side by side and a modal stacked over an editor each
    /// smooth-scroll at once. Created by `Gstoatty;pool_region`, fed by
    /// `Gstoatty;fill`, moved by `Gstoatty;scroll`/`reposition`, and retired by
    /// `Gstoatty;pool_drop`. A [`BTreeMap`] so [`Self::pools`] yields them in
    /// ascending-id (z) order.
    pools: BTreeMap<u32, Pool>,
    /// Kitty graphics images this terminal holds, and the transmission still
    /// arriving.
    ///
    /// Persistent state like [`Self::pools`], surviving a reset and retired
    /// only by the client or by the store's own quota. A client transmits an
    /// image once and places it repeatedly, so dropping the pixels on a reset
    /// would make every placement after one a re-transmission.
    images: images::ImageStore,
    /// The in-progress page fill, set while a `Gstoatty;fill` open marker has
    /// redirected the VT write path onto a pool slot.
    ///
    /// Streamed bytes paint this isolated context's screen instead of the live
    /// grid until the redirect closes (a `fill_end`, the next `fill`, or a
    /// `reset`), when the painted page is committed onto its pool's buffer.
    /// `None` while writing the live grid.
    fill: Option<FillTarget>,
    /// A committed page's fill context, parked for the next page to paint into.
    ///
    /// Building one constructs an alacritty `Term` with its main and alt grids
    /// and a parser holding a large synchronized-update buffer. The editor emits
    /// a fill per page entering its scroll window and re-requests the window on
    /// every content change, so scrolling paid that construction over and over.
    ///
    /// `None` until the first page commits, and only reused for a page whose
    /// pool region matches the parked screen's size.
    fill_scratch: Option<FillTarget>,
    /// The in-progress content capture, set while a `Gstoatty;popover` or
    /// `Gstoatty;text_run` open marker has redirected the streamed bytes into a
    /// pending command's text.
    ///
    /// Streamed bytes accumulate as that text instead of painting the live grid
    /// until the redirect closes (a matching close marker or a `reset`), when the
    /// command is committed onto its decoration list. `None` while writing the
    /// live grid.
    capture: Option<ContentCapture>,
    /// Host-facing notifications projected from the listener's queued events,
    /// accumulated across parses until the app drains them via
    /// [`Self::take_events`].
    ///
    /// Only the live terminal's listener feeds this. A [`FillTarget`] carries
    /// its own throwaway [`ResponseSink`] that is never drained, so a title or
    /// bell emitted by page-fill content is intentionally ignored.
    pending_events: Vec<TermEvent>,
    /// Physical pixel size of one cell (width, height), fed in by the app so a
    /// CSI 14 t query can report the text area in pixels.
    ///
    /// `(0, 0)` until the app calls [`Self::set_cell_pixels`]. A query is left
    /// unanswered while unset, since a zero-size reply is worse than none.
    cell_pixels: (u16, u16),
    /// Decoration commands deferred while a DEC 2026 synchronized update buffers,
    /// applied in arrival order once it ends.
    ///
    /// A frame that redraws its decoration scene emits `Gstoatty;reset` then
    /// re-stamps every component. Applying that immediately would expose a
    /// cleared or partial scene to any projection landing mid-update, so the
    /// mutations stage here and [`Self::drain_staged`] commits them atomically at
    /// the update's end. Holds only the accumulating decoration commands (see
    /// [`Self::apply_decoration`]); stream-routing and pool commands act at feed
    /// time regardless.
    sync_staged: Vec<Command>,
}

/// Per-component "changed since last projection" flags for the accumulated APC
/// decorations, so [`Terminal::project`] re-stamps only what changed.
///
/// Set by [`Terminal::apply_decoration`] when a command lands (and all set by
/// [`Terminal::clear_decorations`], which empties every list), cleared once a
/// projection has applied them.
#[derive(Default)]
struct DecorationDirty {
    borders: bool,
    panels: bool,
    scales: bool,
    popovers: bool,
    scroll_region: bool,
    icons: bool,
    line_layout: bool,
    text_runs: bool,
    bars: bool,
    polylines: bool,
    sketches: bool,
    minimaps: bool,
}

impl DecorationDirty {
    /// Every component marked changed, for a reset that empties all lists.
    fn all() -> DecorationDirty {
        DecorationDirty {
            borders: true,
            panels: true,
            scales: true,
            popovers: true,
            scroll_region: true,
            icons: true,
            line_layout: true,
            text_runs: true,
            bars: true,
            polylines: true,
            sketches: true,
            minimaps: true,
        }
    }
}

/// The decoration lists as the last projection stamped them.
///
/// A scene redeclare empties every list and raises every dirty flag, because the
/// wire protocol dedups whole scenes and so re-sends all of them when any byte
/// differs. Without something to compare against, a bar moving one row re-stamps
/// every border cell, bumps every epoch, and damages every row a decoration
/// covers, which is what defeats the renderer's row caches.
///
/// Sequence numbers are held beside their lists because a list can come back
/// identical while its numbers move. Comparing them is sound only because
/// `clear_decorations` resets the counter, so a redeclare of the same scene
/// reproduces the same numbers.
#[derive(Default)]
struct ProjectedDecorations {
    borders: Vec<BorderCommand>,
    panels: Vec<PanelCommand>,
    panel_seq: Vec<u32>,
    scales: Vec<ScaleCommand>,
    popovers: Vec<PopoverCommand>,
    icons: Vec<IconCommand>,
    icon_seq: Vec<u32>,
    text_runs: Vec<StoredTextRun>,
    text_run_seq: Vec<u32>,
    bars: Vec<BarCommand>,
    bar_seq: Vec<u32>,
    polylines: Vec<PolylineCommand>,
    polyline_seq: Vec<u32>,
    sketches: Vec<SketchCommand>,
    sketch_seq: Vec<u32>,
    minimaps: Vec<MinimapCommand>,
    minimap_seq: Vec<u32>,
    /// Held because the minimap stamp reads the views as well as the strips, so
    /// a view-only advance moves the thumb while leaving the strip list equal.
    minimap_views: HashMap<u32, MinimapView>,
    scroll_region: Option<ScrollRegionCommand>,
    line_layout: Option<LineLayoutCommand>,
}

/// Push `item` onto `list` unless it already holds [`MAX_DECORATIONS`],
/// reporting the first refusal through `warned` and returning whether the push
/// landed.
///
/// The newest is what gets dropped. A list at the cap is one an emitter is
/// still stamping into, so the entries already there are the scene as it was
/// last coherent, and keeping them beats replacing them with the tail of a
/// flood.
///
/// A free function rather than a method so a caller can pass a list borrowed
/// out of an open fill and the flag borrowed from the terminal in one call.
fn push_capped<T>(list: &mut Vec<T>, item: T, warned: &mut bool) -> bool {
    if list.len() >= MAX_DECORATIONS {
        if !*warned {
            *warned = true;
            tracing::warn!(cap = MAX_DECORATIONS, "decoration list full, dropping");
        }
        return false;
    }

    list.push(item);
    true
}

/// The rows a slide of `moved_rows` uncovered in a window `rows` tall.
///
/// A positive move carries the content up the screen, so what enters does so at
/// the bottom; a negative one carries it down and the rows enter at the top. A
/// move at or past the height uncovers every row, though a caller that far out
/// has nothing to carry and reprojects whole anyway.
fn uncovered_rows(moved_rows: isize, rows: usize) -> Range<usize> {
    let magnitude = moved_rows.unsigned_abs().min(rows);
    match moved_rows > 0 {
        true => rows - magnitude..rows,
        false => 0..magnitude,
    }
}

/// Whether a journal holding `entries` entries carrying `lines` line handles has
/// stopped being the cheaper way to bring the grid's stores up to date.
///
/// Both bounds answer the same question from different ends. A drop is one entry
/// and no handles, so a stream of them is bounded only by the count. A splice
/// replacing a whole store is one entry and a store's worth of handles, so a
/// stream of those is bounded only by the total.
///
/// Read as a function of two numbers rather than off the journal, because the
/// line bound is far too large to reach in a test by pushing lines.
fn journal_past_bounds(entries: usize, lines: usize) -> bool {
    entries > MAX_MINIMAP_JOURNAL || lines > MAX_MINIMAP_JOURNAL_LINES
}

/// How many lines a splice may insert into a store of `store_len` before it
/// would pass [`MAX_MINIMAP_LINES`].
///
/// `start` and the removal end clamp to the store length exactly as
/// [`grid::splice_summaries`] clamps them, so the count this returns is the one
/// that leaves the spliced store at the cap rather than over it. A splice that
/// removes as much as it inserts always has room.
fn insert_room(store_len: usize, start: u32, removed: u32) -> usize {
    let start = (start as usize).min(store_len);
    let end = start.saturating_add(removed as usize).min(store_len);
    let surviving = store_len - (end - start);

    MAX_MINIMAP_LINES.saturating_sub(surviving)
}

/// Clear `dirty` when `current` already matches `retained`, and take a copy
/// otherwise.
///
/// `clone_from` rather than `clone` so a category that genuinely changed reuses
/// the allocation it had last frame instead of freeing and taking a new one
/// every time.
fn reconcile<T: Clone + PartialEq>(dirty: &mut bool, current: &T, retained: &mut T) {
    if !*dirty {
        return;
    }
    if current == retained {
        *dirty = false;
    } else {
        retained.clone_from(current);
    }
}

/// [`reconcile`] for a list whose sequence numbers are held separately.
///
/// Both have to match for the category to count as unchanged, since the same
/// commands can arrive carrying different numbers, and both are refreshed
/// together so a later comparison reads one consistent pair.
fn reconcile_seq<T: Clone + PartialEq>(
    dirty: &mut bool,
    current: (&Vec<T>, &Vec<u32>),
    retained: (&mut Vec<T>, &mut Vec<u32>),
) {
    if !*dirty {
        return;
    }
    if current.0 == retained.0 && current.1 == retained.1 {
        *dirty = false;
    } else {
        retained.0.clone_from(current.0);
        retained.1.clone_from(current.1);
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

/// A host-facing notification the running program emitted that the app must act
/// on outside the cell grid.
///
/// A stoatty-owned projection of the terminal notifications the app acts on off
/// the grid. These retitle the window, ring the bell, copy to the system
/// clipboard, and raise desktop notifications. Most come from the
/// `alacritty_terminal` listener. The desktop notification is scanned out of the
/// stream directly, since vte drops OSC 9 / OSC 777. Keeping a local enum means
/// the public API does not leak the upstream event type. Drained by
/// [`Terminal::take_events`] after each parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermEvent {
    /// Set the window title (OSC 0 / OSC 2, or a restored entry from the title
    /// stack).
    Title(String),
    /// Reset the window title to its default (OSC with an empty title).
    ResetTitle,
    /// Ring the bell (BEL).
    Bell,
    /// Copy the given text to the system clipboard (OSC 52), already
    /// base64-decoded upstream.
    ClipboardStore(String),
    /// Raise a desktop notification (OSC 9, or OSC 777;notify). `title` is
    /// `None` for OSC 9, which carries only a body.
    Notification { title: Option<String>, body: String },
    /// A program identified itself with a [`Command::Hello`]. The host logs it so
    /// a remote editor is attributable to this terminal's log.
    Hello(HelloCommand),
    /// A program asked to open an aux OS window as a second render target.
    WindowOpen(WindowOpenCommand),
    /// A program asked to close the aux OS window with this id.
    WindowClose(u32),
    /// A program asked to raise and OS-focus the aux window with this id.
    WindowFocus(u32),
    /// A window-bound pool's content changed, so the aux window with this id
    /// needs a redraw. Coalesced to one per window within an advance.
    WindowDirty(u32),
    /// A program reported that the terminal's config file changed on disk, so
    /// the host should re-read and re-apply it.
    ConfigReload,
    /// A program claimed the platform zoom combo for its session, or released
    /// it. While claimed the host forwards each press upstream rather than
    /// stepping its own font size.
    ///
    /// `inband` asks for the press down the PTY rather than over the window
    /// socket, which is the only route a program on the far end of a link has.
    /// Always false on a release, which has no delivery to ask about.
    ZoomCapture { on: bool, inband: bool },
    /// A program asked the host to step its font size by this many steps,
    /// positive to grow.
    FontStep(i32),
    /// The child left the alternate screen, which is what a full-screen program
    /// does on its way out and what a shell restored after one crashed does too.
    ///
    /// Carries nothing: what it means for the host depends on what the host was
    /// holding on that program's behalf, not on anything the terminal knows
    /// about the leave.
    AltScreenLeft,
}

/// A snapshot of one smooth-scroll pool, for the render loop's per-pool ease.
///
/// Carries the pool's id, its latest declared region, its scroll target, and the
/// primary cursor's glide anchor. The renderer steps an eased offset toward
/// [`Self::scroll_target`] and composites [`Self::region`]. Pools compose in
/// ascending [`Self::id`] order, which is their z-order.
#[derive(Clone, Copy, Debug)]
pub struct PoolView {
    pub id: u32,
    pub region: PoolRegion,
    pub scroll_target: DocumentOffset,
    /// The primary cursor's glide anchor as `(row, col)`, or `None` to ease the
    /// cursor normally.
    ///
    /// `Some` names the document display row and grid-absolute column the cursor
    /// rides while this pool glides, so the renderer draws it at the eased
    /// content offset instead of the VT cursor cell.
    pub cursor_anchor: Option<(u64, u16)>,
    /// The host pool this one rides and the document top row its layout assumed,
    /// or `None` for a pool that floats free.
    ///
    /// `Some((host, top_rows))` asks the renderer to draw this pool shifted by
    /// the gap between `top_rows` and the host's eased top, so a popup laid out
    /// over a scrolling pane travels with the text beneath it.
    pub anchor: Option<(u32, f32)>,
    /// Bumped whenever the pooled page bytes change, so a renderer can tell a
    /// pure sub-cell glide from a frame whose composed content actually changed.
    pub content_version: u64,
}

/// One smooth-scroll surface the document pool tracks.
///
/// A declared region, the recycled pages buffered around the surface's scroll
/// target, and that target. One per `Gstoatty;pool_region` id; the renderer
/// reads its visible region from [`Self::page_pool`] at the eased offset.
struct Pool {
    region: PoolRegion,
    page_pool: PagePool,
    scroll_target: DocumentOffset,
    /// A pending discontinuous-jump destination from `Gstoatty;reposition`,
    /// taken once via [`Terminal::take_reposition`].
    reposition: Option<u64>,
    /// Bumped whenever the pooled page bytes change (a fill commits, a resize
    /// empties the window). A renderer easing this pool sub-cell compares it
    /// across frames to tell a pure glide, where only the fraction moved and the
    /// composed rows are identical, from a frame whose content actually changed.
    content_version: u64,
    /// The primary cursor's anchor for the active glide, set by
    /// `Gstoatty;pool_cursor`. `Some((row, col))` names the document display row
    /// and grid-absolute column the cursor rides, so a renderer draws it at the
    /// eased content offset rather than the VT cursor cell. `None` when no anchor
    /// has arrived. Cleared when the pool is dropped, since the pool is removed.
    cursor_anchor: Option<(u64, u16)>,
    /// This pool's tie to a host pool it must ride, set by
    /// `Gstoatty;pool_anchor`. `Some((host, top_rows))` names the host pool and
    /// the document top row this pool's layout assumed, so a renderer draws it
    /// shifted by the gap between that assumption and the host's eased offset.
    /// `None` when the pool floats free, which is every pool that is not a
    /// popup riding a pane. Cleared when the pool is dropped.
    anchor: Option<(u32, f32)>,
}

impl Pool {
    /// Create a pool for `region`, its page buffer sized to the region.
    fn new(region: PoolRegion) -> Pool {
        Pool {
            page_pool: PagePool::new(
                region.height.max(1) as usize,
                region.width.max(1) as usize,
                PAGE_POOL_CAPACITY,
            ),
            region,
            scroll_target: DocumentOffset::default(),
            reposition: None,
            content_version: 0,
            cursor_anchor: None,
            anchor: None,
        }
    }
}

/// The isolated VT context a `Gstoatty;fill` redirect paints a page into.
///
/// Holds its own [`Term`] and parser so the streamed page content mutates a
/// private screen, with its own cursor and parser state, while the live terminal
/// stays untouched. On commit the screen's cells are projected onto pool
/// [`Self::pool`]'s slot for [`Self::index`].
struct FillTarget {
    pool: u32,
    index: u64,
    /// Whether the page's cells are painted, or only its decorations replaced.
    ///
    /// A `fill_decorations` scope keeps the cells its slot already holds, so
    /// the VT inside it reaches no parser and the commit projects nothing.
    cells: bool,
    /// Whether the painted page is thrown away rather than committed.
    ///
    /// A viewport resize empties every pool's pages, so the page in flight has
    /// nothing left to land on. The context stays open regardless, because
    /// closing it would route the rest of the page to the live parser and paint
    /// it onto the screen.
    discard: bool,
    term: Term<ResponseSink>,
    parser: Processor,
    /// The sink [`Self::term`]'s listener writes into, kept reachable so a
    /// recycled context can be drained.
    ///
    /// Its buffers sit behind an `Arc`, so this shares them with the copy the
    /// `Term` holds. Nothing reads what page content emits, but a context that
    /// outlives its page has to be emptied or the sink grows for the life of the
    /// terminal.
    responses: ResponseSink,
    /// Page-targeted text runs captured while this page paints, moved onto the
    /// pool slot when the fill commits.
    text_runs: Vec<TextRunCommand>,
    /// Page-targeted bars captured while this page paints. See
    /// [`Self::text_runs`].
    bars: Vec<BarCommand>,
    /// Page-targeted stroked paths captured while this page paints. See
    /// [`Self::text_runs`].
    polylines: Vec<PolylineCommand>,
}

impl FillTarget {
    /// Create a `rows` by `cols` fill context for page `index` of pool `pool`,
    /// with a blank screen ready to receive the page's streamed bytes.
    fn new(pool: u32, index: u64, rows: usize, cols: usize) -> FillTarget {
        let responses = ResponseSink::default();
        // No scrollback, unlike the live screen. A page is painted once and
        // projected onto its slot, and nothing ever scrolls this context back,
        // so the default ten thousand lines of history would be allocated per
        // fill context and never read.
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        let term = Term::new(config, &GridSize { rows, cols }, responses.clone());

        FillTarget {
            pool,
            index,
            cells: true,
            discard: false,
            term,
            parser: Processor::new(),
            responses,
            text_runs: Vec::new(),
            bars: Vec::new(),
            polylines: Vec::new(),
        }
    }

    /// Blank the screen and empty the sink, leaving the context ready to paint
    /// another page.
    ///
    /// `CAN` abandons any escape the page left half-written, so the `RIS` after
    /// it is read as a command rather than swallowed as part of that sequence.
    /// `RIS` then resets the screen, cursor, and SGR state while the grid keeps
    /// its row allocations, which is the whole point of holding the context.
    ///
    /// Ending a buffered synchronized update first is unconditional, since
    /// `stop_sync` does nothing when none is open.
    ///
    /// The captured decorations are not this method's to clear. [`Terminal::commit_fill`]
    /// moves them out on its way here, so they are already empty.
    fn recycle(&mut self) {
        self.discard = false;
        self.parser.stop_sync(&mut self.term);
        self.parser.advance(&mut self.term, b"\x18\x1bc");

        let _ = self.responses.take();
        let _ = self.responses.take_events();
    }
}

/// An in-progress capture of a content-bearing marker's streamed text.
///
/// Opened by a `Gstoatty;popover` or `Gstoatty;text_run` marker, it holds the
/// decoded head in `target` while the streamed bytes accumulate in `content`,
/// moved into the command's text field when the close marker commits it.
struct ContentCapture {
    target: CaptureTarget,
    content: Vec<u8>,
}

/// The command awaiting its streamed text in an open [`ContentCapture`].
enum CaptureTarget {
    Popover(PopoverCommand),
    TextRun(TextRunCommand),
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
            ident: None,
            parser: Processor::new(),
            theme,
            palette,
            esc: EscScanner::default(),
            warned_region_clamp: false,
            warned_capture_cap: false,
            warned_decoration_cap: false,
            warned_pool_cap: false,
            warned_minimap_cap: false,
            frames_scratch: Vec::new(),
            frame_scratch: FrameScratch::default(),
            borders: Vec::new(),
            panels: Vec::new(),
            scales: Vec::new(),
            popovers: Vec::new(),
            scroll_region: None,
            icons: Vec::new(),
            text_runs: Vec::new(),
            bars: Vec::new(),
            polylines: Vec::new(),
            sketches: Vec::new(),
            line_layout: None,
            panel_seq: Vec::new(),
            icon_seq: Vec::new(),
            text_run_seq: Vec::new(),
            bar_seq: Vec::new(),
            polyline_seq: Vec::new(),
            sketch_seq: Vec::new(),
            minimaps: Vec::new(),
            minimap_seq: Vec::new(),
            minimap_contents: HashMap::new(),
            minimap_views: HashMap::new(),
            minimap_content_dirty: false,
            minimap_reclone: false,
            minimap_journal: Vec::new(),
            minimap_journal_lines: 0,
            decoration_seq: 1,
            decorations_dirty: DecorationDirty::default(),
            output_since_project: false,
            // A new terminal is fully damaged and the grid it projects into holds
            // nothing, so the first projection has to collect and paint all of it.
            damage_pending: true,
            last_decoration_footprint: Vec::new(),
            footprint_scratch: Vec::new(),
            projected_decorations: ProjectedDecorations::default(),
            row_scratch: Vec::new(),
            row_flags_spare: Vec::new(),
            capture_scratch: Vec::new(),
            last_selection: None,
            decoration_damage: Vec::new(),
            last_history: 0,
            pools: BTreeMap::new(),
            images: images::ImageStore::new(),
            fill: None,
            fill_scratch: None,
            capture: None,
            pending_events: Vec::new(),
            cell_pixels: (0, 0),
            sync_staged: Vec::new(),
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
    /// Each APC frame in the stream is decoded and applied before the bytes
    /// reach the parser, and its interior is then excised so the parser never
    /// sees it. Everything between frames is fed through untouched, which is
    /// what keeps a partial escape sequence around a frame intact.
    ///
    /// Both the `Gstoatty` sub-protocol and Kitty graphics frames ride this
    /// path.
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
    ///
    /// An OSC that trips its cap after earlier chunks gave the parser part of it
    /// replaces the parser that holds it, since every way out of the string
    /// dispatches the truncated part. A fresh parser also ends a DEC 2026 update
    /// in flight. The bytes that update buffered are lost until the program's
    /// next redraw, and the decorations it deferred commit once no update
    /// buffers.
    pub fn advance(&mut self, bytes: &[u8]) -> bool {
        let was_alt_screen = self.is_alt_screen();
        let redraw = self.advance_inner(bytes);
        self.output_since_project |= redraw;
        self.damage_pending |= redraw;
        self.drain_listener_events();
        self.drain_staged();
        // The alternate screen is its own surface for placements, and nothing
        // reports entering or leaving it. Polled here rather than inside the
        // parse body, because the mode only changes once the parser has run.
        let alt_screen = self.is_alt_screen();
        self.images.set_alt_screen(alt_screen);
        // Polled rather than watched for as `1049l`, so the ways out that never
        // name the mode are caught too: a reset leaves it, and so does `47l`.
        //
        // Reported after everything else this chunk produced, rather than where
        // the leave fell among it. A program that claimed something earlier in
        // the same chunk therefore loses the claim to its own exit, which is
        // right, and one that claims after the leave loses it too, which is not.
        // The second needs a program claiming from outside the alternate screen:
        // one that enters it in this chunk ends the chunk inside it, and no
        // leave is reported at all.
        if was_alt_screen && !alt_screen {
            self.pending_events.push(TermEvent::AltScreenLeft);
        }
        redraw
    }

    /// The parse body of [`Self::advance`], split out so the wrapper drains the
    /// listener events once no matter which of the three return points fires.
    fn advance_inner(&mut self, bytes: &[u8]) -> bool {
        let was_syncing = self.syncing();
        let redirecting = self.fill.is_some() || self.capture.is_some();

        // The scanner only acts on ESC-prefixed sequences, so a chunk with no ESC
        // carries nothing for it while it holds no partial sequence. A SIMD memchr
        // for the first ESC lets the bulk of plain output (cat, yes) skip the
        // per-byte scan, leaving only the vte parse. A fill or content redirect must
        // route every byte (captured content is plain text with no ESC), so it
        // forgoes the fast path.
        let scan = if !redirecting && self.esc.is_idle() {
            memchr::memchr(ESC, bytes).map(|esc| &bytes[esc..])
        } else {
            Some(bytes)
        };

        let Some(scan) = scan else {
            self.parser.advance(&mut self.term, bytes);
            return self.parser.sync_bytes_count() < bytes.len();
        };

        let mut frames = mem::take(&mut self.frames_scratch);
        frames.clear();
        let scratch = &mut self.frame_scratch;
        let responses = &self.responses;
        let events = &mut self.pending_events;
        let mut reset = false;
        let mut reset_parser = false;
        self.esc.scan(scan, &mut |event| match event {
            EscEvent::Apc {
                payload,
                interior,
                end,
            } => {
                frames.push((command::decode_with(payload, scratch), interior, end));
            },
            // Excised like an APC frame, and for a harder reason: an image on an
            // OSC is bytes the vte parser buffers without bound. A payload the
            // scanner refused still has to go, which is why it excises whether
            // or not anything parsed.
            EscEvent::OscImage {
                payload,
                interior,
                end,
            } => {
                let command = payload.and_then(iterm::parse_file).map(Command::ItermFile);
                frames.push((command, interior, end));
            },
            // Cut like a refused image payload, and for the same reason. Nothing
            // is read from these bytes, so no command travels with the range.
            EscEvent::OscOverrun {
                interior,
                end,
                reset: replace,
            } => {
                reset_parser |= replace;
                frames.push((None, interior, end));
            },
            EscEvent::XtVersion => responses.push(XTVERSION_REPLY.as_bytes()),
            EscEvent::OscNotify { code, payload } => {
                if let Some(event) = notification_from_osc(code, payload) {
                    events.push(event);
                }
            },
            // Flagged rather than acted on here, so the reset lands after the
            // parser has applied its own half and cannot be undone by it.
            EscEvent::Ris => reset = true,
        });

        if reset {
            self.images.reset();
            self.damage_pending = true;
        }
        // The string the reset drops opened in an earlier chunk, so it starts
        // this chunk and no frame comes before it. A parser replaced here is
        // replaced before the cut, on either path below.
        if reset_parser {
            self.reset_target_parser();
        }

        // Without a redirect every byte targets the live screen, so apply the
        // commands and feed the chunk to the one parser, preserving the
        // synchronized-update accounting the redirect path cannot. Only the
        // frame payloads are held back, which that parser would walk past.
        let involves_redirect = redirecting
            || frames.iter().any(|(command, _, _)| {
                matches!(
                    command,
                    Some(Command::Fill(_))
                        | Some(Command::FillDecorations(_))
                        | Some(Command::Popover(_))
                        | Some(Command::TextRun(_))
                )
            });
        let prefix = bytes.len() - scan.len();
        if !involves_redirect {
            let mut fed = 0;
            let mut start = 0;
            for (command, interior, _) in frames.drain(..) {
                fed += self.feed_live(&bytes[start..prefix + interior.start]);
                start = prefix + interior.end;

                if let Some(command) = command {
                    self.apply_command(command);
                }
            }
            fed += self.feed_live(&bytes[start..]);
            self.frames_scratch = frames;

            // A redraw is warranted unless everything fed was held in the
            // parser's synchronized-update buffer (nothing reached the screen).
            // Measured against what was fed rather than what arrived, since the
            // payloads left out never had a screen to reach.
            return self.parser.sync_bytes_count() < fed;
        }

        // A fill redirect splits the chunk at frame boundaries: each segment up
        // to and including a marker is routed to the target active before that
        // marker, then the marker's command flips the target for the next
        // segment. The marker's introducer and terminator are ignored by
        // whichever parser consumes them, and its payload is left out entirely.
        // `prefix` rebases the scan-relative offsets when a memchr skip left a
        // plain head bound for the live screen.
        let mut start = 0;
        for (command, interior, end) in frames.drain(..) {
            self.feed_segment(&bytes[start..prefix + interior.start]);
            self.feed_segment(&bytes[prefix + interior.end..prefix + end]);
            start = prefix + end;

            if let Some(command) = command {
                // Fill and capture controls always act. A Bar and a TextRun's
                // capture are page-targeted decorations the open fill stores on
                // its slot (the Bar arm and commit_capture route them there), so
                // they act too. The minimap content, view, and drop commands are
                // persistent state whose incremental splices must not be dropped
                // mid-fill, so they act as well. The pool commands are persistent
                // state too, and none of them touches a live-grid decoration, so
                // the leak below does not reach them. Dropping a pool_drop here
                // would strand the pool and its page grids instead. Every other
                // decoration is target-bound and would leak onto the live grid,
                // so it is dropped while a page paints or a content capture runs.
                let routed = matches!(
                    command,
                    Command::Fill(_)
                        | Command::FillDecorations(_)
                        | Command::FillEnd
                        | Command::Reset
                        | Command::Popover(_)
                        | Command::PopoverEnd
                        | Command::TextRun(_)
                        | Command::TextRunEnd
                        | Command::Bar(_)
                        | Command::Polyline(_)
                        | Command::PoolRegion(_)
                        | Command::Scroll(_)
                        | Command::PoolCursor(_)
                        | Command::Reposition(_)
                        | Command::PoolDrop(_)
                        | Command::MinimapLines(_)
                        | Command::MinimapView(_)
                        | Command::MinimapDrop(_)
                        | Command::WindowOpen(_)
                        | Command::WindowClose(_)
                        | Command::WindowFocus(_)
                        | Command::Hello(_)
                        | Command::ConfigReload
                        | Command::ZoomCapture { .. }
                        | Command::FontStep { .. }
                        // The image commands mutate the image store, which is
                        // persistent state a page paint must not swallow, the
                        // same reason the pool commands act here.
                        | Command::Kitty(_)
                        | Command::ItermFile(_)
                );
                if routed || (self.fill.is_none() && self.capture.is_none()) {
                    self.apply_command(command);
                }
            }
        }
        self.feed_segment(&bytes[start..]);
        self.frames_scratch = frames;

        // Mirror the non-redirect sync gate (above): a chunk that began and
        // ended inside an active update presented nothing, so it warrants no
        // redraw. A chunk that opened the update still returns true, so the
        // reader wakes the main loop to arm the timeout flush.
        !(was_syncing && self.syncing())
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
        self.drain_listener_events();
        self.drain_staged();
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

    /// Take the host-facing notifications accumulated since the last call,
    /// leaving none buffered.
    ///
    /// Surfaces window-title, bell, and clipboard-store events the running
    /// program emitted, which the app applies outside the grid (window title,
    /// system clipboard). Each [`Self::advance`] and
    /// [`Self::flush_synchronized_update`] refreshes the buffer, so the caller
    /// drains it right after feeding a chunk. Returns empty when the stream held
    /// no such event.
    pub fn take_events(&mut self) -> Vec<TermEvent> {
        mem::take(&mut self.pending_events)
    }

    /// Install this terminal's identity, so an arriving [`Command::Hello`] is
    /// answered with an ident reply. Without it a hello is still logged (as a
    /// [`TermEvent::Hello`]) but not answered.
    pub fn set_ident(&mut self, ident: IdentReply) {
        self.ident = Some(ident);
    }

    /// Record the physical pixel size of one cell so a CSI 14 t query can report
    /// the text area in pixels.
    ///
    /// The app recomputes this whenever the font size or display scale factor
    /// changes. Until it is called, a pixel-size query goes unanswered.
    pub fn set_cell_pixels(&mut self, width: u16, height: u16) {
        self.cell_pixels = (width, height);
    }

    /// Swap the color set the projection resolves against.
    ///
    /// Rebuilds the indexed palette too, since its ANSI slots and its default
    /// fill both come from the theme. Cells resolve their colors on each
    /// projection rather than storing them, so already-written content takes the
    /// new theme on the next frame with no repaint from the program.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.palette = default_palette(&theme);
    }

    /// Project the listener's queued `alacritty_terminal` events into
    /// [`TermEvent`]s, appending them to [`Self::pending_events`].
    ///
    /// Title, reset-title, bell, and clipboard-store events become
    /// [`TermEvent`]s. Color and text-area-size queries are answered in place
    /// by pushing the formatter's reply into the response bytes.
    fn drain_listener_events(&mut self) {
        for event in self.responses.take_events() {
            match event {
                Event::Title(title) => self.pending_events.push(TermEvent::Title(title)),
                Event::ResetTitle => self.pending_events.push(TermEvent::ResetTitle),
                Event::Bell => self.pending_events.push(TermEvent::Bell),
                Event::ClipboardStore(_, text) => {
                    self.pending_events.push(TermEvent::ClipboardStore(text))
                },
                Event::ColorRequest(index, formatter) => {
                    if let Some(rgb) = self.query_color(index) {
                        self.responses.push(formatter(rgb).as_bytes());
                    }
                },
                Event::TextAreaSizeRequest(formatter) => {
                    let (cell_width, cell_height) = self.cell_pixels;
                    if cell_width > 0 && cell_height > 0 {
                        let window_size = WindowSize {
                            num_lines: self.term.screen_lines() as u16,
                            num_cols: self.term.columns() as u16,
                            cell_width,
                            cell_height,
                        };
                        self.responses.push(formatter(window_size).as_bytes());
                    }
                },
                _ => {},
            }
        }
    }

    /// The background the terminal currently resolves a default-background cell
    /// to.
    ///
    /// A program that set OSC 11 gets its own color back, and OSC 111 restores
    /// the theme's. The window clear reads this rather than the theme directly,
    /// so the sub-cell gutter at the window edges matches the cells beside it
    /// instead of stranding the old background around a recolored grid.
    pub fn default_background(&self) -> Rgb {
        named_color(
            NamedColor::Background,
            self.term.colors(),
            &self.theme,
            &self.palette,
        )
    }

    /// The color the terminal currently resolves the cursor to, honoring an
    /// OSC 12 override the way [`Self::default_background`] honors OSC 11.
    pub fn default_cursor(&self) -> Rgb {
        match self.term.colors()[NamedColor::Cursor as usize] {
            Some(over) => Rgb::new(over.r, over.g, over.b),
            None => self.theme.cursor,
        }
    }

    /// Resolve a color-query index to the concrete channels for its reply.
    ///
    /// An OSC 4 query carries a palette index below [`PALETTE_LEN`]; OSC 10, 11,
    /// and 12 carry the [`NamedColor`] foreground, background, and cursor slots
    /// (256, 257, 258). Each honors the program's OSC override first, then the
    /// theme or palette, so a reply reflects what the cell projection would
    /// actually draw. An index outside those ranges has no answer and yields
    /// `None`.
    fn query_color(&self, index: usize) -> Option<VteRgb> {
        let overrides = self.term.colors();
        let rgb = if index < PALETTE_LEN {
            indexed(index, overrides, &self.palette)
        } else if index == NamedColor::Foreground as usize {
            named_color(
                NamedColor::Foreground,
                overrides,
                &self.theme,
                &self.palette,
            )
        } else if index == NamedColor::Background as usize {
            named_color(
                NamedColor::Background,
                overrides,
                &self.theme,
                &self.palette,
            )
        } else if index == NamedColor::Cursor as usize {
            match overrides[index] {
                Some(over) => Rgb::new(over.r, over.g, over.b),
                None => self.theme.cursor,
            }
        } else {
            return None;
        };

        Some(VteRgb {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        })
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

    /// Give a [`Damage`] back once the frame that read it is done, so its row
    /// buffer serves the next one.
    ///
    /// A frame's damage is read and dropped, and building the flags for the one
    /// after it would then allocate again. Returning it instead is what makes
    /// that allocation once rather than per damaged frame. A caller that
    /// forgets simply pays what it paid before.
    ///
    /// Keeps a buffer only when it carries an allocation and the pool is under
    /// [`MAX_ROW_FLAGS_SPARE`]. A frame that read no damage hands back an empty
    /// vector, which has nothing to lend and would displace a buffer that does.
    pub fn recycle_damage(&mut self, damage: Damage) {
        if let Damage::Partial(rows) = damage
            && rows.capacity() > 0
            && self.row_flags_spare.len() < MAX_ROW_FLAGS_SPARE
        {
            self.row_flags_spare.push(rows);
        }
    }

    /// Apply a decoded stoatty command to the terminal.
    ///
    /// The seam every feature sub-code hooks into. Commands that steer stream
    /// routing (fill and capture open/close) or pool state act immediately. The
    /// accumulating decoration commands route through [`Self::stage_or_apply`],
    /// so they defer while a DEC 2026 synchronized update buffers and commit
    /// atomically when it ends.
    fn apply_command(&mut self, command: Command) {
        match command {
            Command::Border(_)
            | Command::Panel(_)
            | Command::Scale(_)
            | Command::ScrollRegion(_)
            | Command::Icon(_)
            | Command::LineLayout(_)
            | Command::Sketch(_) => self.stage_or_apply(command),
            Command::Popover(popover) => self.begin_capture(CaptureTarget::Popover(popover)),
            Command::PopoverEnd => self.commit_capture(),
            Command::TextRun(text_run) => self.begin_capture(CaptureTarget::TextRun(text_run)),
            Command::TextRunEnd => self.commit_capture(),
            // A page-targeted bar rides the open fill's slot at feed time. A live
            // bar accumulates onto the grid and stages like the other decorations.
            Command::Bar(bar) => match &mut self.fill {
                Some(fill) => {
                    push_capped(&mut fill.bars, bar, &mut self.warned_decoration_cap);
                },
                None => self.stage_or_apply(Command::Bar(bar)),
            },
            // Stroked paths take the same fork as bars. They land on the open
            // fill's slot when one is capturing, and on the live grid otherwise.
            Command::Polyline(polyline) => match &mut self.fill {
                Some(fill) => {
                    push_capped(
                        &mut fill.polylines,
                        polyline,
                        &mut self.warned_decoration_cap,
                    );
                },
                None => self.stage_or_apply(Command::Polyline(polyline)),
            },
            Command::PoolRegion(region) => {
                // Clamped on arrival rather than at the allocation, so the
                // stored region the renderer places and sizes by is the same
                // one the pages were built for.
                let region =
                    grid::from_command::pool_region_from_command(self.clamp_region(region));
                let window = region.window;
                // Whether the declare left the pool's pages newly built or
                // rebuilt, so a page painting into them lost the slot it started
                // on. A re-declare at the size already held moves nothing.
                let pages_replaced = match self.pools.get_mut(&region.pool) {
                    Some(pool) => {
                        let resized = pool.region.width != region.width
                            || pool.region.height != region.height;
                        pool.region = region;
                        if resized {
                            pool.page_pool.rebuild(
                                region.height.max(1) as usize,
                                region.width.max(1) as usize,
                            );
                        }
                        resized
                    },
                    None => {
                        if self.pools.len() >= MAX_POOLS {
                            self.warn_pool_cap(region.pool);
                            return;
                        }
                        self.pools.insert(region.pool, Pool::new(region));
                        true
                    },
                };

                // The open page was built for geometry this declare just moved,
                // so it no longer describes the slot it would land on. Doomed
                // rather than closed, since closing it would route the rest of
                // the page to the live parser and paint it on the screen.
                if pages_replaced
                    && let Some(fill) = &mut self.fill
                    && fill.pool == region.pool
                {
                    fill.discard = true;
                }
                self.mark_window_dirty(window);
            },
            Command::Fill(fill) => self.begin_fill(fill.pool, fill.index, true),
            Command::FillDecorations(fill) => self.begin_fill(fill.pool, fill.index, false),
            Command::FillEnd => self.commit_fill(),
            Command::Scroll(scroll) => {
                let window = self.pools.get_mut(&scroll.pool).map(|pool| {
                    pool.scroll_target = DocumentOffset {
                        page: scroll.page,
                        fraction: scroll.fraction as f32 / FRACTION_SCALE,
                    };
                    pool.region.window
                });
                if let Some(window) = window {
                    self.mark_window_dirty(window);
                }
            },
            Command::PoolCursor(cursor) => {
                let window = self.pools.get_mut(&cursor.pool).map(|pool| {
                    pool.cursor_anchor = Some((cursor.row, cursor.col));
                    pool.region.window
                });
                if let Some(window) = window {
                    self.mark_window_dirty(window);
                }
            },
            Command::PoolAnchor(anchor) => {
                let window = self.pools.get_mut(&anchor.pool).map(|pool| {
                    pool.anchor = Some((anchor.host, anchor.top_rows));
                    pool.region.window
                });
                if let Some(window) = window {
                    self.mark_window_dirty(window);
                }
            },
            Command::Reposition(reposition) => {
                let window = self.pools.get_mut(&reposition.pool).map(|pool| {
                    pool.scroll_target = DocumentOffset {
                        page: reposition.page,
                        fraction: 0.0,
                    };
                    pool.reposition = Some(reposition.page);
                    pool.region.window
                });
                if let Some(window) = window {
                    self.mark_window_dirty(window);
                }
            },
            Command::PoolDrop(drop) => {
                self.pools.remove(&drop.pool);
                // Doomed rather than closed, since the page has lost the pool it
                // was painting for. Closing the context would route the rest of
                // the page to the live parser and paint it on the screen.
                if let Some(fill) = &mut self.fill
                    && fill.pool == drop.pool
                {
                    fill.discard = true;
                }
            },
            // A minimap declaration is a decoration cleared by reset, staged like
            // a border. Its content store, views, and drop are persistent state
            // that acts immediately, so an incremental splice is never lost.
            Command::Minimap(_) => self.stage_or_apply(command),
            Command::MinimapLines(lines) => self.splice_minimap_content(lines),
            Command::MinimapView(view) => {
                // An id already drawn keeps advancing its thumb at the cap, so
                // the limit costs a real scene nothing and refuses only the
                // fresh ids a looping writer invents.
                if !self.minimap_views.contains_key(&view.strip_id)
                    && self.minimap_views.len() >= MAX_MINIMAP_VIEWS
                {
                    self.warn_minimap_cap("minimap view limit reached, refusing a new strip id");
                    return;
                }
                self.minimap_views.insert(
                    view.strip_id,
                    MinimapView {
                        top_256: view.top_256,
                        visible: view.visible_lines,
                    },
                );
                // A view-only advance still marks the strip dirty so the next
                // projection re-stamps the thumb. Otherwise it freezes at its
                // last position until a re-declare or resize marks it.
                self.decorations_dirty.minimaps = true;
            },
            Command::MinimapDrop(drop) => {
                self.minimap_contents.remove(&drop.content_id);
                self.journal_minimap(MinimapJournal::Drop(drop.content_id));
            },
            // A reset is also a fill/capture close trigger. The fill and capture
            // commits must run at feed time, but the decoration clear stages so a
            // mid-update reset does not blank the live scene before the re-stamp.
            Command::Reset => {
                self.commit_fill();
                self.commit_capture();
                self.stage_or_apply(Command::Reset);
            },
            // Hello identifies the program behind the terminal and touches no
            // grid state. The host logs the event, and the terminal answers with
            // its own ident when one is installed.
            Command::Hello(hello) => {
                if let Some(ident) = &self.ident {
                    self.responses.push(&command::encode_ident_reply(ident));
                }
                self.pending_events.push(TermEvent::Hello(hello));
            },
            // Window lifecycle commands surface as events for the host's
            // windowing path, which owns the actual aux OS windows.
            Command::WindowOpen(open) => self.pending_events.push(TermEvent::WindowOpen(open)),
            Command::WindowClose(close) => self
                .pending_events
                .push(TermEvent::WindowClose(close.window)),
            Command::WindowFocus(focus) => self
                .pending_events
                .push(TermEvent::WindowFocus(focus.window)),
            // Not a decoration, so it never stages behind a synchronized
            // update. Rereading a config has nothing to do with the frame being
            // composed, and delaying it would only defer the user's edit.
            Command::ConfigReload => self.pending_events.push(TermEvent::ConfigReload),
            // Neither is a decoration either. A zoom claim and a font step both
            // act on the window rather than the frame being composed, so
            // holding them behind an update would only defer what the user
            // pressed.
            Command::ZoomCapture { on, inband } => self
                .pending_events
                .push(TermEvent::ZoomCapture { on, inband }),
            Command::FontStep { delta } => self.pending_events.push(TermEvent::FontStep(delta)),
            // Graphics frames touch no grid state. They feed a store the client
            // places from later, and the reply goes back on the response path
            // the ident handshake uses.
            Command::Kitty(graphics) => {
                let cursor = self.cursor();
                let applied = self.images.apply(
                    graphics,
                    images::Screen {
                        cursor: (cursor.row, cursor.col),
                        cell: (u32::from(self.cell_pixels.0), u32::from(self.cell_pixels.1)),
                    },
                );

                if let Some(response) = applied.response {
                    let mut out = Vec::new();
                    kitty::encode_response_into(
                        &mut out,
                        response.id,
                        response.number,
                        response.placement,
                        &response.result,
                    );
                    self.responses.push(&out);
                }

                // A placement leaves the cursor past its last row, so text the
                // client writes next lands below the image rather than over it.
                if let Some((row, col)) = applied.cursor {
                    let cols = self.term.columns();
                    self.term.goto(
                        row.min(self.term.screen_lines() - 1) as i32,
                        col.min(cols - 1),
                    );
                }

                // The placement list is grid state a projection has to rebuild,
                // and nothing else marks it dirty.
                self.damage_pending = true;
            },
            // An inline image carries the pixels and the size in one escape and
            // has no id to answer under, so this stores and places it in one
            // step and says nothing back.
            Command::ItermFile(file) => {
                let cursor = self.cursor();
                let landed = self.images.apply_inline(
                    &file,
                    images::Screen {
                        cursor: (cursor.row, cursor.col),
                        cell: (u32::from(self.cell_pixels.0), u32::from(self.cell_pixels.1)),
                    },
                    (self.term.screen_lines(), self.term.columns()),
                );

                if let Some((row, col)) = landed {
                    let cols = self.term.columns();
                    self.term.goto(
                        row.min(self.term.screen_lines() - 1) as i32,
                        col.min(cols - 1),
                    );
                }
                self.damage_pending = true;
            },
        }
    }

    /// Queue a [`TermEvent::WindowDirty`] for aux window `window`, coalescing
    /// repeats within one advance.
    ///
    /// A no-op for the primary window (`0`), which composites on the main grid,
    /// and when the last pending event is already the same dirty request, so a
    /// region re-declare, a scroll, and a fill on one window in a single advance
    /// yield a single redraw.
    fn mark_window_dirty(&mut self, window: u32) {
        if window == 0 || self.pending_events.last() == Some(&TermEvent::WindowDirty(window)) {
            return;
        }
        self.pending_events.push(TermEvent::WindowDirty(window));
    }

    /// Splice the command's lines into the content store `content_id`, replacing
    /// `removed` lines from `start` with the inserted ones.
    ///
    /// Creates the store when absent, so a first splice populates it, unless
    /// [`MAX_MINIMAP_STORES`] already exist. Records the command on
    /// [`Self::minimap_journal`] so the next projection replays the same splice
    /// into the grid instead of re-cloning every store.
    ///
    /// Lines past [`MAX_MINIMAP_LINES`] are trimmed from the command itself,
    /// before both the splice and the journal push, so the grid replays the same
    /// bounded splice and its stores keep matching these.
    fn splice_minimap_content(&mut self, mut command: MinimapLinesCommand) {
        let Some(store_len) = self.minimap_store_len(command.content_id) else {
            return;
        };

        let room = insert_room(store_len, command.start, command.removed);
        if command.lines.len() > room {
            command.lines.truncate(room);
            self.warn_minimap_cap("minimap content store past the line cap, trimming");
        }

        let store = self.minimap_contents.entry(command.content_id).or_default();
        grid::splice_summaries(store, command.start, command.removed, &command.lines);
        self.journal_minimap(MinimapJournal::Splice(command));
    }

    /// Record `change` for the next projection to replay, or give up on
    /// replaying and ask it for the whole map instead.
    ///
    /// Past either bound the journal goes and the reclone flag takes over.
    /// Nothing is lost, since the clone reproduces exactly what the replay
    /// would have. While that flag stands the journal is dead weight, so
    /// nothing more is recorded into it.
    fn journal_minimap(&mut self, change: MinimapJournal) {
        self.minimap_content_dirty = true;
        if self.minimap_reclone {
            return;
        }

        self.minimap_journal_lines += match &change {
            MinimapJournal::Splice(command) => command.lines.len(),
            MinimapJournal::Drop(_) => 0,
        };
        self.minimap_journal.push(change);

        if journal_past_bounds(self.minimap_journal.len(), self.minimap_journal_lines) {
            self.clear_minimap_journal();
            self.minimap_reclone = true;
        }
    }

    /// Empty the journal and forget what it was holding.
    ///
    /// The only thing that empties either, so the count cannot outlive the
    /// entries it counted. Three paths reach here: the bounds above, the
    /// projection's wholesale clone, and the projection's replay.
    fn clear_minimap_journal(&mut self) {
        self.minimap_journal.clear();
        self.minimap_journal_lines = 0;
    }

    /// The length of store `content_id`, or `None` when it does not exist and
    /// [`MAX_MINIMAP_STORES`] already do.
    ///
    /// An absent store reports zero rather than `None` while there is room, so
    /// the caller creates it as before.
    fn minimap_store_len(&mut self, content_id: u32) -> Option<usize> {
        if let Some(store) = self.minimap_contents.get(&content_id) {
            return Some(store.len());
        }

        if self.minimap_contents.len() >= MAX_MINIMAP_STORES {
            self.warn_minimap_cap("minimap store limit reached, refusing a new id");
            return None;
        }
        Some(0)
    }

    /// Report the first pool this session refused at [`MAX_POOLS`].
    fn warn_pool_cap(&mut self, pool: u32) {
        if self.warned_pool_cap {
            return;
        }
        self.warned_pool_cap = true;
        tracing::warn!(
            pool,
            cap = MAX_POOLS,
            "pool limit reached, refusing a new id"
        );
    }

    /// Report the first minimap input this session to be bounded, naming which
    /// limit fired through `reason`.
    fn warn_minimap_cap(&mut self, reason: &str) {
        if self.warned_minimap_cap {
            return;
        }
        self.warned_minimap_cap = true;
        tracing::warn!(
            max_stores = MAX_MINIMAP_STORES,
            max_lines = MAX_MINIMAP_LINES,
            max_views = MAX_MINIMAP_VIEWS,
            "{reason}"
        );
    }

    /// Route a decoration command to the live lists now, or defer it while a DEC
    /// 2026 synchronized update is buffering.
    ///
    /// Deferred commands accumulate in [`Self::sync_staged`] and replay in
    /// arrival order through [`Self::apply_decoration`] once the update ends (see
    /// [`Self::drain_staged`]), so a frame's reset-then-re-stamp scene cycle
    /// commits atomically rather than exposing a cleared or partial scene.
    fn stage_or_apply(&mut self, command: Command) {
        if self.syncing() {
            self.sync_staged.push(command);
        } else {
            self.apply_decoration(command);
        }
    }

    /// Apply a decoration command to its live list, marking that list dirty so the
    /// next [`Self::project`] re-stamps it.
    ///
    /// The completed `Popover` and `TextRun` commands arrive here already carrying
    /// their captured text from [`Self::commit_capture`], so they push directly
    /// rather than reopening a capture. Only the accumulating decoration commands
    /// and `Reset` reach this method. The stream-routing and pool commands act at
    /// feed time in [`Self::apply_command`] and never route here.
    fn apply_decoration(&mut self, command: Command) {
        match command {
            Command::Border(border) => {
                if push_capped(&mut self.borders, border, &mut self.warned_decoration_cap) {
                    self.decorations_dirty.borders = true;
                }
            },
            Command::Panel(panel) => {
                if push_capped(&mut self.panels, panel, &mut self.warned_decoration_cap) {
                    self.panel_seq.push(self.decoration_seq);
                    self.decoration_seq += 1;
                    self.decorations_dirty.panels = true;
                }
            },
            Command::Scale(scale) => {
                if push_capped(&mut self.scales, scale, &mut self.warned_decoration_cap) {
                    self.decorations_dirty.scales = true;
                }
            },
            Command::ScrollRegion(region) => {
                self.scroll_region = Some(region);
                self.decorations_dirty.scroll_region = true;
            },
            Command::Icon(icon) => {
                if push_capped(&mut self.icons, icon, &mut self.warned_decoration_cap) {
                    self.icon_seq.push(self.decoration_seq);
                    self.decoration_seq += 1;
                    self.decorations_dirty.icons = true;
                }
            },
            Command::LineLayout(layout) => {
                self.line_layout = Some(layout);
                self.decorations_dirty.line_layout = true;
            },
            Command::Bar(bar) => {
                if push_capped(&mut self.bars, bar, &mut self.warned_decoration_cap) {
                    self.bar_seq.push(self.decoration_seq);
                    self.decoration_seq += 1;
                    self.decorations_dirty.bars = true;
                }
            },
            Command::Polyline(polyline) => {
                if push_capped(
                    &mut self.polylines,
                    polyline,
                    &mut self.warned_decoration_cap,
                ) {
                    self.polyline_seq.push(self.decoration_seq);
                    self.decoration_seq += 1;
                    self.decorations_dirty.polylines = true;
                }
            },
            Command::Sketch(sketch) => {
                if push_capped(&mut self.sketches, sketch, &mut self.warned_decoration_cap) {
                    self.sketch_seq.push(self.decoration_seq);
                    self.decoration_seq += 1;
                    self.decorations_dirty.sketches = true;
                }
            },
            Command::Popover(popover) => {
                if push_capped(&mut self.popovers, popover, &mut self.warned_decoration_cap) {
                    self.decorations_dirty.popovers = true;
                }
            },
            Command::TextRun(text_run) => {
                if push_capped(
                    &mut self.text_runs,
                    text_run.into(),
                    &mut self.warned_decoration_cap,
                ) {
                    self.text_run_seq.push(self.decoration_seq);
                    self.decoration_seq += 1;
                    self.decorations_dirty.text_runs = true;
                }
            },
            Command::Minimap(minimap) => {
                if push_capped(&mut self.minimaps, minimap, &mut self.warned_decoration_cap) {
                    self.minimap_seq.push(self.decoration_seq);
                    self.decoration_seq += 1;
                    self.decorations_dirty.minimaps = true;
                }
            },
            Command::Reset => self.clear_decorations(),
            // None of these reach here. apply_command routes them at feed time:
            // the stream and pool controls, and the immediate minimap content,
            // view, and drop commands.
            Command::Fill(_)
            | Command::FillDecorations(_)
            | Command::FillEnd
            | Command::PopoverEnd
            | Command::TextRunEnd
            | Command::PoolRegion(_)
            | Command::Scroll(_)
            | Command::PoolCursor(_)
            | Command::PoolAnchor(_)
            | Command::Reposition(_)
            | Command::PoolDrop(_)
            | Command::MinimapLines(_)
            | Command::MinimapView(_)
            | Command::MinimapDrop(_)
            | Command::WindowOpen(_)
            | Command::WindowClose(_)
            | Command::WindowFocus(_)
            | Command::Hello(_)
            | Command::ConfigReload
            | Command::ZoomCapture { .. }
            | Command::FontStep { .. }
            | Command::Kitty(_)
            | Command::ItermFile(_) => {},
        }
    }

    /// Whether a DEC 2026 synchronized update is currently buffering.
    fn syncing(&self) -> bool {
        self.sync_deadline().is_some()
    }

    /// Commit the decoration commands staged during a synchronized update, in
    /// arrival order, once the update has ended.
    ///
    /// A no-op while an update is still buffering or nothing was staged. Runs
    /// after every [`Self::advance`] and [`Self::flush_synchronized_update`], so
    /// the staged scene lands the moment the update's ESU or timeout flush ends
    /// it.
    fn drain_staged(&mut self) {
        if self.syncing() || self.sync_staged.is_empty() {
            return;
        }

        // Drained rather than consumed so the list keeps its allocation for the
        // next update, since an editor opens one update per frame.
        //
        // Restoring by assignment cannot lose a staged command. The only push
        // site stages solely while `syncing()` holds, and `syncing()` is false
        // for the whole drain. The early return above establishes that, and
        // `syncing()` reads parser state that only feeding bytes can move.
        let mut staged = mem::take(&mut self.sync_staged);
        for command in staged.drain(..) {
            self.apply_decoration(command);
        }
        self.sync_staged = staged;
    }

    /// Snapshots of every declared smooth-scroll pool, in ascending-id (z) order.
    ///
    /// Each carries the pool's id, latest region, and scroll target, so the
    /// render loop can step each pool's ease and composite it. Empty until a
    /// `Gstoatty;pool_region` declares the first pool.
    pub fn pools(&self) -> Vec<PoolView> {
        let mut out = Vec::new();
        self.pools_into(&mut out);
        out
    }

    /// Snapshot every declared pool into `out`, clearing it first so a reused
    /// buffer holds only the current pools. See [`Self::pools`] for the ordering
    /// and contents.
    pub fn pools_into(&self, out: &mut Vec<PoolView>) {
        out.clear();
        out.extend(
            self.pools
                .values()
                .filter(|pool| pool.region.window == 0)
                .map(Self::pool_view),
        );
    }

    /// Snapshots of every pool bound to aux `window`, in ascending-id (z) order.
    ///
    /// The counterpart to [`Self::pools`], which yields only primary-grid pools.
    /// A window-bound pool is kept out of the primary composite and drawn into
    /// its own window from this list instead.
    pub fn window_pools(&self, window: u32) -> Vec<PoolView> {
        let mut out = Vec::new();
        self.window_pools_into(window, &mut out);
        out
    }

    /// Snapshot every pool bound to aux `window` into `out`, clearing it first so
    /// a reused buffer holds only the current pools. See [`Self::window_pools`]
    /// for the ordering and contents.
    pub fn window_pools_into(&self, window: u32, out: &mut Vec<PoolView>) {
        out.clear();
        out.extend(
            self.pools
                .values()
                .filter(|pool| pool.region.window == window)
                .map(Self::pool_view),
        );
    }

    fn pool_view(pool: &Pool) -> PoolView {
        PoolView {
            id: pool.region.pool,
            region: pool.region,
            scroll_target: pool.scroll_target,
            cursor_anchor: pool.cursor_anchor,
            anchor: pool.anchor,
            content_version: pool.content_version,
        }
    }

    /// Take pool `id`'s pending discontinuous-jump destination, clearing it.
    ///
    /// Set by `Gstoatty;reposition`. The render loop consumes it once per arrival
    /// to re-anchor that pool's live offset near the destination before easing
    /// onto the target, so a far jump lands softly instead of dragging across the
    /// unbuffered gap. `None` for an unknown id or when no jump is pending.
    pub fn take_reposition(&mut self, id: u32) -> Option<u64> {
        self.pools.get_mut(&id)?.reposition.take()
    }

    /// Pool `id`'s content-version, or `None` for an unknown pool.
    ///
    /// The version bumps whenever the pool's composed rows would differ (a fill
    /// commits, a resize empties the window). Paired with the composed top row,
    /// it lets a caller easing this pool sub-cell skip recomposing a frame whose
    /// version and top both held steady, since only the sub-cell fraction moved.
    pub fn pool_content_version(&self, id: u32) -> Option<u64> {
        Some(self.pools.get(&id)?.content_version)
    }

    /// The newest page stamp under the `rows` document rows from `top` in pool
    /// `id`, or `None` for an unknown pool or a row with no buffered page.
    ///
    /// A caller that composed those rows and recorded [`grid::page_stamp_now`]
    /// with them keeps them while this answers at or below the record. A fill
    /// elsewhere in the pool, such as the page a glide buffers ahead of itself,
    /// leaves the answer as it was. See [`PagePool::window_stamp`].
    pub fn pool_window_stamp(&self, id: u32, top: i64, rows: usize) -> Option<u64> {
        self.pools.get(&id)?.page_pool.window_stamp(top, rows)
    }

    /// Compose pool `id`'s visible region into `out` at the eased page offset,
    /// or `None` to fall back to the live grid.
    ///
    /// `doc_scroll` is the pool's live smooth-scroll position in document pages.
    /// Sizes `out` to the pool's region plus one straddle row and fills it from
    /// the pooled pages straddling the offset, returning the sub-cell fraction to
    /// shift the rendered rows by and the top document row composed.
    ///
    /// Returns `None` for an unknown id, or when the straddled pages are not all
    /// buffered -- the degradation path taken whenever no pool window covers the
    /// offset, so the renderer shows the live grid for that region instead of
    /// holes.
    pub fn project_pool(&self, id: u32, out: &mut Grid, doc_scroll: f32) -> Option<(f32, i64)> {
        let pool = self.pools.get(&id)?;
        let page_rows = pool.region.height as usize;
        let cols = pool.region.width as usize;
        if page_rows == 0 {
            return None;
        }

        if out.rows() != page_rows + 1 || out.cols() != cols {
            out.resize(page_rows + 1, cols);
        }

        let doc_rows = doc_scroll * page_rows as f32;
        let top = doc_rows.floor() as i64;
        let frac = doc_rows - top as f32;

        if !pool.page_pool.compose(top, out) {
            return None;
        }
        stamp_pool_decorations(&pool.page_pool, out, top, page_rows);
        Some((frac, top))
    }

    /// Compose a straddled scrollback-history window into `out` at the eased
    /// offset, or `None` to fall back to the live grid.
    ///
    /// `visual` is the live smooth-scroll position in rows back from the live
    /// bottom: zero at the bottom, growing toward older history. Sizes `out` to
    /// the viewport plus one straddle row at the top and fills it from the
    /// history rows straddling the offset, returning the fractional row offset to
    /// shift the rendered window by (in `[-1, 0)`) -- gap-free at both edges.
    ///
    /// Returns `None` at the live bottom (`visual` at or below zero), so the
    /// renderer shows the live grid -- the cursor- and decoration-bearing
    /// projection -- rather than a history snapshot.
    ///
    /// `moved_rows` is how far the window's content moved since the last compose,
    /// positive when it moved up the screen. `out` slides by it, and only the rows
    /// that then differ from the history are rewritten and reported through
    /// `damage`. A move clearing the whole window leaves nothing worth comparing
    /// against, so it rewrites every row and reports [`Damage::Full`].
    ///
    /// Zero is the ordinary value for live output arriving while the window holds
    /// still, since the view is pinned to its content. Those frames leave almost
    /// every row clean.
    ///
    /// `vt_changed` says whether anything the VT holds moved since the last
    /// compose. When it did not, a slide reads only the rows it uncovered at the
    /// edge it vacated: the carried rows show history that did not change, and
    /// the slide is what put them where they are. Every other frame reads and
    /// compares the whole window.
    ///
    /// That makes `moved_rows` load-bearing on exactly those frames. Elsewhere a
    /// wrong one costs redundant dirty rows and nothing else, since every row is
    /// compared; on a slide the VT stood still through, a wrong one leaves a
    /// stale row standing until some later frame reads the window whole.
    pub fn project_scrollback(
        &mut self,
        out: &mut Grid,
        visual: f32,
        moved_rows: isize,
        vt_changed: bool,
        damage: &mut Damage,
    ) -> Option<f32> {
        *damage = Damage::Full;
        if visual <= 0.0 {
            return None;
        }

        let rows = self.term.screen_lines();
        let cols = self.term.columns();
        if rows == 0 {
            return None;
        }
        if out.rows() != rows + 1 || out.cols() != cols {
            out.resize(rows + 1, cols);
        }

        let offset = visual.floor();
        let frac = visual - offset;
        let offset = offset as i32;

        let grid = self.term.grid();
        let colors = self.term.colors();
        let topmost = grid.topmost_line().0;
        let bottommost = grid.bottommost_line().0;

        // Sliding the window by what the caller says it moved leaves most rows
        // already holding the right content, so the compare below finds only the
        // rows the move revealed. A move clearing the window keeps nothing worth
        // comparing against, and reprojects whole.
        //
        // A move of nothing still compares. Live output arriving while the window
        // holds still is pinned to its content, so it reports a zero move, and those
        // are the frames where the fewest rows differ.
        let out_rows = out.rows();
        let diffing = moved_rows.unsigned_abs() < out_rows;
        if diffing {
            out.scroll_by(moved_rows);
            *damage = Damage::Partial(row_bounds(&mut self.row_flags_spare, out_rows));
        }

        // A slide the VT stood still through carries every row it kept onto the
        // same history it already held, so those rows need no reading at all.
        // What is left is the rows the slide uncovered, which is a handful
        // against the whole window a wheel would otherwise convert per frame. A
        // move that cleared the window uncovered all of them, so the range comes
        // back whole and the rewrite below runs as it always did.
        //
        // A rebuild asked for with neither a move nor a VT change is a caller
        // contradicting itself: it wants the window redone and names nothing
        // that changed. Reading it whole is the safe answer to that.
        let reading = match !vt_changed && moved_rows != 0 {
            true => uncovered_rows(moved_rows, out_rows),
            false => 0..out_rows,
        };

        // Row 0 is the straddle row one line older than the offset's top, so a
        // downward sub-cell shift always has an older row to reveal at the top.
        let top_line = -offset - 1;
        let mut projected = mem::take(&mut self.row_scratch);
        for out_row in reading {
            let line = top_line + out_row as i32;
            let source = (line >= topmost && line <= bottommost).then(|| &grid[Line(line)]);
            let project = |col: usize| match source {
                Some(row) => project_cell(&row[Column(col)], colors, &self.theme, &self.palette),
                None => Cell::default(),
            };

            // Without a compare to make, the row goes straight into the window
            // rather than through the scratch and out again.
            if !diffing {
                for (col, out) in out.row_mut(out_row).iter_mut().enumerate() {
                    *out = project(col);
                }
                continue;
            }

            projected.clear();
            projected.extend((0..cols).map(project));

            if out.row(out_row) == projected.as_slice() {
                continue;
            }
            if let Damage::Partial(rows_dirty) = damage {
                rows_dirty[out_row] = whole_row(cols);
            }
            out.row_mut(out_row).copy_from_slice(&projected);
        }
        self.row_scratch = projected;

        // The window begins one row above the offset's top, so it rests shifted
        // up a full row with that straddle hidden above the viewport; as the
        // fraction grows the window slides down, revealing the older row and
        // advancing one whole row by the time it reaches 1.
        Some(frac - 1.0)
    }

    /// Bound `region`'s dimensions to [`MAX_REGION_VIEWPORTS`] times the live
    /// viewport, warning the first time anything is cut.
    ///
    /// The dimensions arrive from the wire and are multiplied by
    /// [`PAGE_POOL_CAPACITY`] grids of cells, so a writer that names the `u16`
    /// maximum asks for hundreds of gigabytes. Clamping turns that into a
    /// bounded allocation instead of an abort, and costs a legitimate pool
    /// nothing, since a region that far past the viewport paints nothing that
    /// could be shown.
    ///
    /// The warning is once per session because a hostile writer sends these in
    /// a flood, and a line per command is its own way to bring the session
    /// down.
    fn clamp_region(&mut self, region: PoolRegionCommand) -> PoolRegionCommand {
        let rows = (self.term.screen_lines() * MAX_REGION_VIEWPORTS).min(u16::MAX as usize) as u16;
        let cols = (self.term.columns() * MAX_REGION_VIEWPORTS).min(u16::MAX as usize) as u16;
        if region.height <= rows && region.width <= cols {
            return region;
        }

        if !self.warned_region_clamp {
            self.warned_region_clamp = true;
            tracing::warn!(
                asked_width = region.width,
                asked_height = region.height,
                max_width = cols,
                max_height = rows,
                "pool region past the viewport, clamping"
            );
        }
        PoolRegionCommand {
            width: region.width.min(cols),
            height: region.height.min(rows),
            ..region
        }
    }

    /// Open a page-fill redirect onto pool `pool`'s slot for document page
    /// `index`.
    ///
    /// Any already-open fill is committed first, so a dropped `fill_end` cannot
    /// strand the redirect: the next `fill` (or a `reset`) closes the previous
    /// page. The context is sized to the pool's region, matching the slots
    /// [`Self::commit_fill`] writes into. An unknown pool falls back to the
    /// viewport so the redirect still captures (and later discards) the bytes
    /// rather than leaking them onto the live grid.
    ///
    /// The context parked by the last page serves this one when their sizes match,
    /// so only a differently shaped region builds a new one.
    ///
    /// `cells` is false for a `fill_decorations` scope, which keeps the cells
    /// the slot holds and replaces only its decorations.
    fn begin_fill(&mut self, pool: u32, index: u64, cells: bool) {
        self.commit_fill();
        // A pool's region is already clamped, so this only bounds the fallback
        // and any pool declared before a resize shrank the viewport under it.
        let (rows, cols) = match self.pools.get(&pool) {
            Some(pool) => (
                pool.region.height.max(1) as usize,
                pool.region.width.max(1) as usize,
            ),
            None => (self.term.screen_lines(), self.term.columns()),
        };
        let rows = rows.min(self.term.screen_lines() * MAX_REGION_VIEWPORTS);
        let cols = cols.min(self.term.columns() * MAX_REGION_VIEWPORTS);
        // A committed page's context is reset and parked, so an ordinary scroll
        // paints every page through the same screen and parser. A region resize
        // is the only thing that invalidates it, and that is cold.
        let recycled = self
            .fill_scratch
            .take_if(|fill| fill.term.screen_lines() == rows && fill.term.columns() == cols);

        let mut fill = match recycled {
            Some(mut fill) => {
                fill.pool = pool;
                fill.index = index;
                fill
            },
            None => FillTarget::new(pool, index, rows, cols),
        };
        fill.cells = cells;
        self.fill = Some(fill);
    }

    /// Commit the open page fill onto its pool's slot and restore the live grid.
    ///
    /// Projects the fill context's painted cells onto the recycled slot for its
    /// page index in the target pool. The bars and text runs captured while the
    /// page painted move onto the slot with it. A no-op when no fill is open, so
    /// every close trigger (`fill_end`, the next `fill`, `reset`) can call it
    /// unconditionally. The painted page is discarded if its pool was dropped
    /// mid-fill, or if a resize emptied the pages it would have landed on.
    ///
    /// The fill context itself is reset and parked in [`Self::fill_scratch`] for
    /// the next page to paint through.
    ///
    /// A `fill_decorations` scope projects no cells. Its decorations replace the
    /// ones on the slot only while that slot still buffers the page, since the
    /// runs mean nothing without the cells under them.
    fn commit_fill(&mut self) {
        let Some(mut fill) = self.fill.take() else {
            return;
        };

        // Taken before the pool lookup so a page whose pool vanished leaves the
        // context as empty as a committed one does.
        let text_runs = mem::take(&mut fill.text_runs);
        let bars = mem::take(&mut fill.bars);
        let polylines = mem::take(&mut fill.polylines);

        if !fill.discard
            && let Some(pool) = self.pools.get_mut(&fill.pool)
        {
            // A caller watching this version refills every page it buffers
            // whenever the version moves, so most fills repaint bytes that did
            // not change. Reporting one of those costs a recompose of
            // everything the pool feeds, which is why the version moves only
            // when the page does.
            let changed = match fill.cells {
                true => {
                    // The box project_term_cells writes, so the slot clears only
                    // what the paint below will not reach.
                    let grid = pool.page_pool.fill(
                        fill.index,
                        fill.term.screen_lines(),
                        fill.term.columns(),
                    );
                    let cells_changed =
                        project_term_cells(grid, &fill.term, &self.theme, &self.palette);
                    pool.page_pool
                        .set_decorations(fill.index, text_runs, bars, polylines);
                    pool.page_pool.content_changed(fill.index, cells_changed)
                },
                false => pool
                    .page_pool
                    .redecorate(fill.index, text_runs, bars, polylines),
            };
            if changed {
                pool.content_version = pool.content_version.wrapping_add(1);
                let window = pool.region.window;
                self.mark_window_dirty(window);
            }
        }

        // Parked whether or not the page landed, since a pool dropped mid-fill
        // leaves the context just as reusable as a committed one. A
        // decorations-only scope fed its parser nothing, so there is nothing to
        // reset.
        match fill.cells {
            true => fill.recycle(),
            false => fill.discard = false,
        }
        self.fill_scratch = Some(fill);
    }

    /// Open a content capture for the command described by `target`.
    ///
    /// Any already-open capture is committed first, so a dropped close marker
    /// cannot strand it: the next open marker (or a `reset`) closes the previous
    /// one. The streamed bytes that follow accumulate as its text until
    /// [`Self::commit_capture`].
    fn begin_capture(&mut self, target: CaptureTarget) {
        self.commit_capture();
        // A gutter opens one capture per visible line, so the byte buffer is
        // borrowed from the terminal and handed back on commit rather than
        // allocated per run.
        let mut content = mem::take(&mut self.capture_scratch);
        content.clear();
        self.capture = Some(ContentCapture { target, content });
    }

    /// Commit the open content capture onto its decoration list with the streamed
    /// text.
    ///
    /// A no-op when no capture is open, so every close trigger (a close marker,
    /// the next open marker, `reset`) can call it unconditionally. The redirect
    /// feeds the trailing close marker's bytes into the buffer along with the
    /// text; captured content is plain text, so the first `ESC` is that marker's
    /// introducer, and everything from it is dropped.
    fn commit_capture(&mut self) {
        let Some(mut capture) = self.capture.take() else {
            return;
        };

        let content_end = capture
            .content
            .iter()
            .position(|&byte| byte == ESC)
            .unwrap_or(capture.content.len());
        capture.content.truncate(content_end);

        // Valid UTF-8 is the ordinary case and borrows, so it builds the string
        // straight from the bytes. Only malformed input needs rebuilding around
        // replacement characters.
        let text = match std::str::from_utf8(&capture.content) {
            Ok(text) => text.to_owned(),
            Err(_) => String::from_utf8_lossy(&capture.content).into_owned(),
        };

        match capture.target {
            CaptureTarget::Popover(mut command) => {
                command.content = text;
                self.stage_or_apply(Command::Popover(command));
            },
            CaptureTarget::TextRun(mut command) => {
                command.text = text;
                match &mut self.fill {
                    Some(fill) => {
                        push_capped(
                            &mut fill.text_runs,
                            command,
                            &mut self.warned_decoration_cap,
                        );
                    },
                    None => self.stage_or_apply(Command::TextRun(command)),
                }
            },
        }

        self.capture_scratch = capture.content;
    }

    /// Report the first capture this session to overrun [`MAX_CAPTURE_BYTES`].
    fn warn_capture_cap(&mut self) {
        if self.warned_capture_cap {
            return;
        }
        self.warned_capture_cap = true;
        tracing::warn!(
            max_bytes = MAX_CAPTURE_BYTES,
            "streamed content past the capture cap, committing early"
        );
    }

    /// Bytes the parser is holding back in its synchronized-update buffer.
    #[cfg(test)]
    fn buffered_sync_bytes(&self) -> usize {
        self.parser.sync_bytes_count()
    }

    /// Hand `segment` to the live parser and report its length, for a caller
    /// summing what a chunk actually presented.
    fn feed_live(&mut self, segment: &[u8]) -> usize {
        self.parser.advance(&mut self.term, segment);
        segment.len()
    }

    /// Route a run of VT bytes to the active write target.
    ///
    /// A content capture takes precedence over an open fill, so a text run
    /// nested inside a page captures its text rather than painting it into the
    /// page cells. An open fill with no capture takes the bytes, and with
    /// neither open the live parser does. A decorations-only fill drops them,
    /// since it paints no cells.
    ///
    /// A capture that reaches [`MAX_CAPTURE_BYTES`] is committed with the text
    /// it has, which restores live routing for everything after it.
    fn feed_segment(&mut self, segment: &[u8]) {
        if let Some(capture) = &mut self.capture {
            capture.content.extend_from_slice(segment);
            if capture.content.len() >= MAX_CAPTURE_BYTES {
                self.warn_capture_cap();
                self.commit_capture();
            }
        } else if let Some(fill) = &mut self.fill {
            if fill.cells {
                fill.parser.advance(&mut fill.term, segment);
            }
        } else {
            self.parser.advance(&mut self.term, segment);
        }
    }

    /// Replace the parser [`Self::feed_segment`] routes to, dropping the string
    /// it holds part of.
    ///
    /// A capture holds bytes rather than parsing them, so it has no parser to
    /// replace. A skipped OSC swallows the frames that open and close a fill,
    /// so the target stays the same while the string is open.
    fn reset_target_parser(&mut self) {
        if self.capture.is_some() {
            return;
        }
        match &mut self.fill {
            Some(fill) => fill.parser = Processor::new(),
            None => self.parser = Processor::new(),
        }
    }

    /// Clear all accumulated stoatty decoration state.
    ///
    /// A `Gstoatty;reset` frame lands here. Without it the per-frame decoration
    /// lists only grow, since the VT projection re-stamps them every frame, so a
    /// program that redraws a frame at a new position would leave the old one
    /// behind. Resetting lets a program redraw its decoration scene from scratch.
    /// Drop the dirty flags of categories a redeclare left unchanged.
    ///
    /// Every flag is raised together by a scene reset, so without this a scene
    /// differing in one bar re-stamps the borders, re-bumps the epochs, and
    /// damages every row a decoration covers. Each category is compared against
    /// what the last projection stamped and only survives if it really moved.
    ///
    /// Lists carrying sequence numbers are compared with them, since the same
    /// commands can arrive under different numbers.
    fn reconcile_decorations(&mut self) {
        let dirty = &mut self.decorations_dirty;
        let last = &mut self.projected_decorations;

        reconcile(&mut dirty.borders, &self.borders, &mut last.borders);
        reconcile(&mut dirty.scales, &self.scales, &mut last.scales);
        reconcile(&mut dirty.popovers, &self.popovers, &mut last.popovers);
        reconcile(
            &mut dirty.scroll_region,
            &self.scroll_region,
            &mut last.scroll_region,
        );
        reconcile(
            &mut dirty.line_layout,
            &self.line_layout,
            &mut last.line_layout,
        );

        reconcile_seq(
            &mut dirty.panels,
            (&self.panels, &self.panel_seq),
            (&mut last.panels, &mut last.panel_seq),
        );
        reconcile_seq(
            &mut dirty.icons,
            (&self.icons, &self.icon_seq),
            (&mut last.icons, &mut last.icon_seq),
        );
        reconcile_seq(
            &mut dirty.text_runs,
            (&self.text_runs, &self.text_run_seq),
            (&mut last.text_runs, &mut last.text_run_seq),
        );
        reconcile_seq(
            &mut dirty.bars,
            (&self.bars, &self.bar_seq),
            (&mut last.bars, &mut last.bar_seq),
        );
        reconcile_seq(
            &mut dirty.polylines,
            (&self.polylines, &self.polyline_seq),
            (&mut last.polylines, &mut last.polyline_seq),
        );
        reconcile_seq(
            &mut dirty.sketches,
            (&self.sketches, &self.sketch_seq),
            (&mut last.sketches, &mut last.sketch_seq),
        );
        // The strips alone are not the whole stamp. A view-only advance moves
        // the thumb over an unchanged strip list, so the views are part of what
        // decides whether this category still matches.
        if dirty.minimaps {
            if self.minimaps == last.minimaps
                && self.minimap_seq == last.minimap_seq
                && self.minimap_views == last.minimap_views
            {
                dirty.minimaps = false;
            } else {
                last.minimaps.clone_from(&self.minimaps);
                last.minimap_seq.clone_from(&self.minimap_seq);
                last.minimap_views.clone_from(&self.minimap_views);
            }
        }
    }

    fn clear_decorations(&mut self) {
        self.borders.clear();
        self.panels.clear();
        self.scales.clear();
        self.popovers.clear();
        self.icons.clear();
        self.text_runs.clear();
        self.bars.clear();
        self.polylines.clear();
        self.sketches.clear();
        self.panel_seq.clear();
        self.icon_seq.clear();
        self.text_run_seq.clear();
        self.bar_seq.clear();
        self.polyline_seq.clear();
        self.sketch_seq.clear();
        // A minimap declaration is a decoration and clears here. Its content
        // store is persistent state that survives, retired by its own drop.
        //
        // A view has no drop of its own, since minimap_drop keys on the content
        // id, so this is the only place one can be retired. The prune runs
        // against the outgoing declarations, before they are cleared. An emitter
        // prefixes a reset to every re-stamp, so a strip the scene still draws
        // has to keep its thumb across that reset, while one it has stopped
        // declaring is what this retires.
        let declared = &self.minimaps;
        self.minimap_views
            .retain(|strip_id, _| declared.iter().any(|strip| strip.strip_id == *strip_id));
        self.minimaps.clear();
        self.minimap_seq.clear();
        self.decoration_seq = 1;
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
    ///
    /// Resizing to the size already held does nothing. A window drag reports a
    /// resize per pixel of travel, and most of those land on the same cell
    /// dimensions. Emptying the pools for one would abandon a half-painted page
    /// with nothing to prompt a repaint, since the shell is signalled only when
    /// the geometry actually moves.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        if rows == self.term.screen_lines() && cols == self.term.columns() {
            return;
        }

        self.term.resize(GridSize { rows, cols });
        self.damage_pending = true;
        // Pages are sized to each pool's region, not the viewport, so a viewport
        // resize only empties them. The app re-declares regions and refills as
        // its layout recomputes, and that redeclare is what resizes the pages.
        // Any half-painted page is abandoned.
        for pool in self
            .pools
            .values_mut()
            .filter(|pool| pool.region.window == 0)
        {
            pool.page_pool.invalidate();
            pool.content_version = pool.content_version.wrapping_add(1);
        }
        // Doomed rather than closed. The page in flight has nowhere to land now,
        // but the context has to stay open to swallow the rest of it, since the
        // bytes would otherwise route to the live parser and paint the screen.
        if let Some(fill) = &mut self.fill {
            fill.discard = true;
        }
    }

    /// Move the viewport `delta` lines through scrollback history: positive
    /// scrolls up toward older output, negative scrolls back down toward the
    /// live bottom, clamped to the saved history.
    ///
    /// The offset change fully damages the screen, so the next [`Self::project`]
    /// repaints the scrolled-back view through the usual path.
    pub fn scroll_display(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
        self.damage_pending = true;
    }

    /// The viewport's offset back into scrollback history, in rows: zero at the
    /// live bottom, growing as the view scrolls toward older output.
    ///
    /// Read before and after [`Self::scroll_display`] to recover the rows a
    /// wheel move actually shifted the viewport, so a move clamped at the
    /// history edge is measured by the clamped amount.
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Whether output has arrived since the last projection, without touching
    /// the grid.
    ///
    /// Stands in for [`Self::project`] on a frame rendering something other
    /// than the projected grid, which a scrolled-back view does. Projecting
    /// there is a full pass into cells nothing draws.
    ///
    /// The damage is deliberately left where it is. It accumulates until a
    /// projection resets it, so the one that eventually runs repaints exactly
    /// the rows that moved while it was being skipped, and nothing has to
    /// remember a repaint owed. Reading it here would not work anyway: the
    /// terminal damages the cursor's row on every read, so a frame with no
    /// output looks the same as one with some.
    pub fn take_damage_flag(&mut self) -> bool {
        mem::take(&mut self.output_since_project)
    }

    /// The placements the renderer should draw, joined with their pixels.
    ///
    /// A placement whose anchor row sits outside the screen is left out rather
    /// than clamped, so an image scrolled off the top does not reappear pinned
    /// to row zero. One whose image is gone is left out too, which is what a
    /// delete that freed the data leaves behind for a frame.
    fn placed_images(&self, rows: usize, cols: usize) -> Vec<grid::PlacedImage> {
        self.images
            .placements()
            .iter()
            .filter(|placement| {
                placement.row >= 0 && (placement.row as usize) < rows && placement.col < cols
            })
            .filter_map(|placement| {
                let image = self.images.image(placement.image)?;
                Some(grid::PlacedImage {
                    image: placement.image,
                    placement: placement.placement,
                    generation: image.generation,
                    rgba: image.rgba.clone(),
                    width: image.width,
                    height: image.height,
                    row: placement.row as usize,
                    col: placement.col,
                    cols: placement.cols,
                    rows: placement.rows,
                    crop: grid::ImageCrop {
                        x: placement.crop_x,
                        y: placement.crop_y,
                        width: placement.crop_w,
                        height: placement.crop_h,
                    },
                    offset_x: placement.offset_x,
                    offset_y: placement.offset_y,
                    z: placement.z,
                })
            })
            .collect()
    }

    /// The cursor [`Self::project`] would report, without projecting.
    ///
    /// For a frame that skipped the projection but still has to place something
    /// against the cursor's cell.
    pub fn cursor(&self) -> Cursor {
        let content = self.term.renderable_content();
        project_cursor(content.cursor, content.display_offset as i32)
    }

    /// Reset the viewport to the live bottom of history, so the next
    /// [`Self::project`] shows current output again. Used to pin the view on
    /// keyboard input.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
        self.damage_pending = true;
    }

    /// Begin a simple text selection anchored at viewport cell `(row, col)`.
    ///
    /// `side_right` picks the right half of the cell for the anchor, so a drag
    /// that starts past a glyph's midpoint excludes it, matching the usual
    /// terminal feel. Replaces any prior selection.
    pub fn start_selection(&mut self, row: usize, col: usize, side_right: bool) {
        let point = viewport_to_point(self.display_offset(), Point::new(row, Column(col)));
        let side = if side_right { Side::Right } else { Side::Left };
        self.term.selection = Some(Selection::new(SelectionType::Simple, point, side));
    }

    /// Extend the active selection to viewport cell `(row, col)`. A no-op when
    /// no selection is active.
    pub fn update_selection(&mut self, row: usize, col: usize, side_right: bool) {
        let point = viewport_to_point(self.display_offset(), Point::new(row, Column(col)));
        let side = if side_right { Side::Right } else { Side::Left };
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    /// Drop any active selection, so the next [`Self::project`] repaints without
    /// the INVERSE overlay.
    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// The selected text, or `None` when there is no selection or it is empty
    /// (a click without a drag).
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string()
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

    /// Whether pointer motion should be reported while a button is held, i.e.
    /// the program enabled button-event (1002) or any-motion (1003) tracking.
    pub fn mouse_drag(&self) -> bool {
        self.term
            .mode()
            .intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
    }

    /// Whether pointer motion should be reported with no button held, i.e. the
    /// program enabled any-motion (1003) tracking.
    pub fn mouse_motion(&self) -> bool {
        self.term.mode().contains(TermMode::MOUSE_MOTION)
    }

    /// Whether bracketed-paste mode (DECSET 2004) is on, so pasted text should be
    /// wrapped in the paste-guard markers rather than sent to the program raw.
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Whether focus reporting (DECSET 1004) is on, so the app should send a
    /// focus-in or focus-out report as the window gains or loses focus.
    pub fn report_focus_in_out(&self) -> bool {
        self.term.mode().contains(TermMode::FOCUS_IN_OUT)
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

        self.output_since_project = false;
        // Nothing has damaged the terminal since the last projection, so there is
        // nothing to read out of it. Reading anyway would report the cursor's row
        // as dirty, which it always does, and buy a reprojection and a re-upload
        // of that row on a frame whose content stood still.
        //
        // Skipping the reset below with it is what keeps a missed damage source
        // from losing a repaint outright. Whatever accumulated stays there for the
        // next projection that does collect, so the cost of a miss is one frame of
        // delay.
        let collected = mem::take(&mut self.damage_pending) || resized;
        let mut dirty = match collected {
            true => self.collect_damage(rows, resized),
            false => Damage::Partial(Vec::new()),
        };

        let content = self.term.renderable_content();
        let offset = content.display_offset as i32;
        let selection = content.selection;

        // Read before the projection below, which wants to know how far the
        // content moved so it can slide the grid to match.
        let history = self.term.history_size();
        let grew = history.saturating_sub(self.last_history);
        self.last_history = history;
        // A non-zero display offset means the user has scrolled back: the
        // viewport is pinned to its content as history grows, so report no
        // scroll and leave the renderer's auto-scroll ease idle.
        let scrolled = if offset > 0 { 0 } else { grew };

        // Repaint the rows whose selection overlay changed since the last
        // projection, so it tracks a drag even where VT damage did not land. A
        // `Damage::Full` frame already covers every row.
        //
        // Compared by range rather than by the rows it spans. A drag inside one
        // row leaves the span equal while the overlay moves, and a drag that
        // crosses a row changes only its edges however tall the selection is.
        if selection != self.last_selection
            && let Damage::Partial(rows_dirty) = &mut dirty
        {
            // An idle frame carries an empty vec, so grow it before marking. A
            // genuinely-damaged frame already sizes it to rows.
            rows_dirty.resize(rows, None);
            mark_selection_change(
                self.last_selection,
                selection,
                offset,
                rows,
                cols,
                rows_dirty,
            );
        }
        self.last_selection = selection;

        // Read the cells straight from the terminal grid rather than filtering
        // the whole viewport per cell. `display_iter` maps grid `line + offset`
        // to viewport `row`. Inverting it as `line = row - offset` yields the
        // same cells and points.
        let term_grid = self.term.grid();
        let theme = &self.theme;
        let palette = &self.palette;
        // `out` is the whole row either way, so a bounded call indexes it by
        // column rather than from the range's start.
        let project_range = |row: usize, columns: RangeInclusive<usize>, out: &mut [Cell]| {
            let line = Line(row as i32 - offset);
            let source = &term_grid[line];
            for col in columns {
                let cell = &mut out[col];
                *cell = project_cell(&source[Column(col)], content.colors, theme, palette);
                if selection.is_some_and(|s| s.contains(Point::new(line, Column(col)))) {
                    cell.flags = cell.flags.toggle(Flags::INVERSE);
                }
            }
        };
        let project_into = |row: usize, out: &mut [Cell]| project_range(row, 0..=cols - 1, out);

        let mut projected = mem::take(&mut self.row_scratch);

        // A whole-screen damage rarely means the whole screen changed. A scroll
        // moves the content up, and INSERT mode raises full damage for a single
        // typed cell. Sliding the grid to where the content now sits and then
        // keeping the rows that come out identical turns either into damage
        // naming the few rows that really changed, which is what lets the render
        // passes reuse the rest.
        //
        // The slide is only a guess, and nothing rests on it. Every row is still
        // projected and compared, so a wrong guess marks every row dirty, which
        // is what a full frame did anyway.
        let sliding = matches!(dirty, Damage::Full) && !resized && offset == 0;
        if sliding {
            projected.clear();
            projected.resize(cols, Cell::default());

            // Growth in scrollback says how far the content moved. An alt-screen
            // scroll, a scroll region below the top line, and INSERT mode all
            // damage the screen without growing it, so those are found by probe.
            let shift = match grew {
                grown if grown > 0 && grown < rows => grown,
                _ => detect_shift(grid, rows, &mut projected, project_into),
            };

            grid.scroll_by(shift as isize);
            dirty = Damage::Partial(row_bounds(&mut self.row_flags_spare, rows));
        }

        for row in 0..rows {
            // Without a slide nothing compares the row before it lands, so it goes
            // straight into the grid rather than through the scratch and out again.
            if !sliding {
                // Only the columns the terminal reported. A cell blinking in
                // place otherwise costs a projection of the row holding it.
                if let Some((left, right)) = dirty.columns(row, cols) {
                    project_range(row, left..=right, grid.row_mut(row));
                }
                continue;
            }

            project_into(row, &mut projected);
            if grid.row(row) == projected.as_slice() {
                continue;
            }
            if let Damage::Partial(rows_dirty) = &mut dirty {
                rows_dirty[row] = whole_row(cols);
            }
            grid.row_mut(row).copy_from_slice(&projected);
        }
        self.row_scratch = projected;

        let cursor = project_cursor(content.cursor, offset);

        // A scene redeclare raised every flag without regard to whether the
        // lists changed, so settle that here, before anything reads them.
        self.reconcile_decorations();

        // Re-stamp a decoration only when it changed, when a resize cleared the
        // grid's lists and cells, or -- for the per-cell borders and scales --
        // when any row's cells were reset by VT damage above. Text runs and bars
        // resolve their row through the line layout, so they also re-stamp when
        // the layout changed. A `Damage::Partial([])` idle frame re-stamps none.
        let vt_damaged = match &dirty {
            Damage::Full => true,
            Damage::Partial(rows) => rows.iter().any(|row| row.is_some()),
        };
        let layout_changed = self.decorations_dirty.line_layout || resized;

        // A stamp is only lost on the rows the projection rewrote, so those are the
        // rows to restamp. A resize cleared every cell, and a changed command list
        // leaves the grid holding regions that are no longer declared, so both have
        // to stamp regardless of what the projection touched.
        let all_rows = Damage::Full;
        if self.decorations_dirty.borders || resized {
            apply_borders(grid, &self.borders, &all_rows);
        } else if vt_damaged {
            apply_borders(grid, &self.borders, &dirty);
        }
        if self.decorations_dirty.scales || resized {
            apply_scales(grid, &self.scales, &all_rows);
        } else if vt_damaged {
            apply_scales(grid, &self.scales, &dirty);
        }
        if self.decorations_dirty.popovers || resized {
            apply_popovers(grid, &self.popovers);
        }
        if self.decorations_dirty.panels || resized {
            apply_panels(grid, &self.panels, &self.panel_seq);
        }
        if self.decorations_dirty.scroll_region || resized {
            apply_scroll_region(grid, self.scroll_region);
        }
        if self.decorations_dirty.icons || resized {
            apply_icons(grid, &self.icons, &self.icon_seq);
        }
        if layout_changed {
            apply_line_layout(grid, self.line_layout.as_ref());
        }
        if self.decorations_dirty.text_runs || layout_changed {
            apply_text_runs(grid, &self.text_runs, &self.text_run_seq);
        }
        if self.decorations_dirty.bars || layout_changed {
            apply_bars(grid, &self.bars, &self.bar_seq);
        }
        if self.decorations_dirty.polylines || layout_changed {
            apply_polylines(grid, &self.polylines, &self.polyline_seq);
        }
        if self.decorations_dirty.sketches || layout_changed {
            apply_sketches(grid, &self.sketches, &self.sketch_seq);
        }
        if self.decorations_dirty.minimaps || resized {
            apply_minimaps(grid, &self.minimaps, &self.minimap_seq, &self.minimap_views);
        }
        // Placements follow the scroll the terminal can vouch for, which is the
        // history it grew. The slide above infers a shift when the whole screen
        // is damaged, and that guess is deliberately allowed to be wrong. A wrong
        // guess costs the projection a row comparison, where it would cost a
        // placement its existence. A region scroll inside the margins grows no
        // history either, so placements do not follow that.
        self.images.scroll(scrolled as i32, rows);
        if resized {
            self.images.clamp_to(rows, cols);
        }

        // The placement list is rebuilt whole rather than diffed. A placement
        // carries a refcount on its pixels rather than a copy, so rebuilding is
        // a walk of a short list, and a projection that skipped it would leave
        // the renderer drawing an image the client deleted.
        grid.set_images(self.placed_images(rows, cols));
        // Normally replay only the splices and drops since the last projection.
        // The grid's stores match the term's as of then, so the replay reproduces
        // the current stores while cloning just each splice's lines. A
        // viewport-only frame touches neither, and a resize keeps its stores so
        // it replays like any other frame.
        //
        // Only a break in that equality forces the wholesale clone, which drops
        // the journal it subsumes.
        if self.minimap_reclone {
            grid.set_minimap_contents(self.minimap_contents.clone());
            self.clear_minimap_journal();
            self.minimap_content_dirty = false;
            self.minimap_reclone = false;
        } else if self.minimap_content_dirty {
            for change in self.minimap_journal.drain(..) {
                match change {
                    MinimapJournal::Splice(command) => grid.splice_minimap_content(
                        command.content_id,
                        command.start,
                        command.removed,
                        &command.lines,
                    ),
                    MinimapJournal::Drop(content_id) => grid.drop_minimap_content(content_id),
                }
            }
            self.clear_minimap_journal();
            self.minimap_content_dirty = false;
        }

        // Accumulate renderer-facing decoration row-damage for the borders,
        // panels, and scales. When one changed, damage the rows it covers now and
        // the rows it covered last projection, so the decoration passes rebuild
        // the new footprint and erase a moved or cleared one. Panels are
        // grid-level rather than cell-stamped, but their chrome sits over live
        // cells, so a change still repaints the rows it spans. A VT re-stamp
        // re-applies the same decorations, leaving this signal untouched.
        if self.last_decoration_footprint.len() != rows {
            self.last_decoration_footprint.clear();
            self.last_decoration_footprint.resize(rows, false);
        }
        if self.decoration_damage.len() != rows {
            self.decoration_damage = row_bounds(&mut self.row_flags_spare, rows);
        }
        // The footprint is a pure function of the three decoration lists and the row
        // count, and none of those can move while this is false, so the retained
        // footprint is already what a recompute would produce. Rebuilding it costs a
        // clear, a resize, and a fill-span per decoration, all of it unread.
        let decorations_moved = self.decorations_dirty.borders
            || self.decorations_dirty.panels
            || self.decorations_dirty.scales
            || resized;
        if decorations_moved {
            decoration_footprint(
                &self.borders,
                &self.panels,
                &self.scales,
                rows,
                &mut self.footprint_scratch,
            );
            for ((damage, &now), &before) in self
                .decoration_damage
                .iter_mut()
                .zip(&self.footprint_scratch)
                .zip(&self.last_decoration_footprint)
            {
                if now || before {
                    *damage = whole_row(cols);
                }
            }
            mem::swap(
                &mut self.last_decoration_footprint,
                &mut self.footprint_scratch,
            );
        }

        self.decorations_dirty = DecorationDirty::default();

        if collected {
            self.term.reset_damage();
        }
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

        let cols = self.term.columns();
        match self.term.damage() {
            TermDamage::Full => Damage::Full,
            TermDamage::Partial(lines) => {
                let mut lines = lines.peekable();
                if lines.peek().is_none() {
                    return Damage::Partial(Vec::new());
                }
                let mut rows_dirty = row_bounds(&mut self.row_flags_spare, rows);
                for bounds in lines {
                    let Some(slot) = rows_dirty.get_mut(bounds.line) else {
                        continue;
                    };
                    // Clamped to the screen because the bounds index the cells a
                    // renderer patches, and widened when one line is reported
                    // twice so the union covers both reports.
                    let last = cols.saturating_sub(1);
                    let left = bounds.left.min(last) as u16;
                    let right = bounds.right.min(last) as u16;
                    *slot = Some(match *slot {
                        Some((held_left, held_right)) => {
                            (held_left.min(left), held_right.max(right))
                        },
                        None => (left, right),
                    });
                }
                Damage::Partial(rows_dirty)
            },
        }
    }
}

/// Captures both what the terminal wants written to the PTY and the host-facing
/// notifications the app acts on.
///
/// `alacritty_terminal` reports replies to host queries (device attributes,
/// device-status and cursor-position reports, keyboard-mode queries) as
/// [`Event::PtyWrite`], appended to [`Self::bytes`] for the owning [`Terminal`]
/// to write back. Title, bell, clipboard-store, color, and text-area-size
/// events carry no reply bytes, but the app still needs them, so they queue in
/// [`Self::events`] for [`Terminal::drain_listener_events`] to project into
/// [`TermEvent`]s. The remaining variants (mouse-cursor dirty, cursor-blink
/// change, clipboard load) are dropped.
///
/// Both buffers are [`Arc`]/[`Mutex`] rather than `Rc`/`RefCell` so [`Terminal`]
/// stays [`Send`], letting the app parse the byte stream on a thread off the
/// render loop. The trait method takes `&self`, so the `Term` holds the listener
/// while the owning [`Terminal`] keeps a clone to drain.
#[derive(Clone, Default)]
struct ResponseSink {
    bytes: Arc<Mutex<Vec<u8>>>,
    events: Arc<Mutex<Vec<Event>>>,
}

impl ResponseSink {
    /// Drain the buffered response bytes, leaving the buffer empty.
    fn take(&self) -> Vec<u8> {
        mem::take(&mut *self.bytes.lock())
    }

    /// Drain the queued listener events, leaving the queue empty.
    fn take_events(&self) -> Vec<Event> {
        mem::take(&mut *self.events.lock())
    }

    /// Append `bytes` to the buffer, for a reply the driver synthesizes itself
    /// (XTVERSION) rather than receiving from the `Term` listener.
    fn push(&self, bytes: &[u8]) {
        self.bytes.lock().extend_from_slice(bytes);
    }
}

impl EventListener for ResponseSink {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self.bytes.lock().extend_from_slice(text.as_bytes()),
            Event::Title(_)
            | Event::ResetTitle
            | Event::Bell
            | Event::ClipboardStore(..)
            | Event::ColorRequest(..)
            | Event::TextAreaSizeRequest(_) => self.events.lock().push(event),
            _ => {},
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

#[cfg(test)]
mod tests;
