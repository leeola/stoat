//! The bytes-to-grid driver: a VT byte stream parsed onto the cell grid.
//!
//! [`Terminal`] wraps an `alacritty_terminal` terminal and its vte parser.
//! Bytes fed to [`Terminal::advance`] mutate the parsed screen, and
//! [`Terminal::project`] copies that screen onto a [`Grid`]. The copy resolves
//! each cell's terminal-palette color to concrete channels and touches only the
//! lines the terminal reports as damaged.

use crate::{
    grid::{
        self, Bar, Border, BorderStyle, Borders, Cell, DocumentOffset, Flags, Grid, Icon, IconKind,
        Minimap, MinimapView, Overlay, PagePool, Panel, PanelShadow, Polyline, Rgb, Scale,
        ScrollRegion, TextRun, UnderlineStyle,
    },
    theme::Theme,
};
use alacritty_terminal::{
    event::{Event, EventListener, WindowSize},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionRange, SelectionType},
    term::{
        cell::{Cell as TermCell, Flags as TermFlags},
        color::Colors,
        viewport_to_point, Config, RenderableCursor, TermDamage, TermMode,
    },
    vte::ansi::{Color, CursorShape as TermCursorShape, NamedColor, Processor, Rgb as VteRgb},
    Term,
};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashMap},
    mem,
    sync::Arc,
    time::Instant,
};
use stoatty_protocol::{
    command::{
        self, BarCommand, BorderCommand, Command, HelloCommand, IconCommand, IdentReply,
        LineLayoutCommand, LineSummary, MinimapCommand, MinimapLinesCommand, PanelCommand,
        PolylineCommand, PoolRegionCommand, PopoverCommand, ScaleCommand, ScrollRegionCommand,
        TextRunCommand, WindowOpenCommand,
    },
    frame::{FrameScratch, MAX_APC_PAYLOAD},
};

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
    frames_scratch: Vec<(Option<Command>, usize)>,
    /// Reused per-argument decode buffers threaded through
    /// [`command::decode_with`], so the busy decode path allocates nothing per
    /// APC frame argument once warm.
    frame_scratch: FrameScratch,
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
    /// The store changes since the last [`Self::project`], in arrival order. The
    /// grid's stores equal these stores as of that projection, so replaying the
    /// journal against them reproduces the current stores exactly while cloning
    /// only each splice's lines rather than every store.
    minimap_journal: Vec<MinimapJournal>,
    /// Next seq to stamp, incremented per decoration and reset by a
    /// `Gstoatty;reset` frame. Starts at 1 so pool-composited content (seq 0)
    /// sorts below every declared decoration.
    decoration_seq: u32,
    /// Which decoration command lists changed since the last [`Self::project`],
    /// so a projection re-stamps only the components that changed rather than all
    /// of them every frame.
    decorations_dirty: DecorationDirty,
    /// The grid rows the cell-stamped decorations (borders, scales) occupied at
    /// the previous [`Self::project`], so a moved or cleared decoration can damage
    /// the rows it used to cover and erase its stale footprint.
    last_decoration_footprint: Vec<bool>,
    /// Scratch reused by [`Self::project`] to build the current decoration
    /// footprint without allocating each frame. Swapped with
    /// [`Self::last_decoration_footprint`] after the damage comparison, so the
    /// two buffers alternate roles and a steady-state projection allocates
    /// neither.
    footprint_scratch: Vec<bool>,
    /// One projected row, reused across the rows of a projection.
    ///
    /// A scrolled frame projects each row here first so it can be compared
    /// against what the grid already holds, and only rows that differ are
    /// written back. Reusing it keeps that comparison from allocating a row per
    /// row per frame.
    row_scratch: Vec<Cell>,
    /// The byte buffer an open content capture streams into, held here between
    /// captures so a run of them shares one allocation.
    ///
    /// A gutter emits a text run per visible line and each opens its own
    /// capture, so allocating per capture would allocate per line per frame.
    capture_scratch: Vec<u8>,
    /// The inclusive viewport row span the selection covered at the previous
    /// [`Self::project`], so a growing or shrinking drag can damage the rows it
    /// entered or left and repaint their INVERSE overlay. `None` when there was
    /// no selection.
    last_selection_span: Option<(usize, usize)>,
    /// Accumulated renderer-facing decoration row-damage since the renderer last
    /// drained it via [`Self::take_decoration_damage`]. Distinct from VT
    /// [`Damage`]: it marks rows where an APC border or scale changed, which the
    /// cell-decoration passes gate their per-row rebuilds on.
    decoration_damage: Vec<bool>,
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
            minimaps: true,
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
    ZoomCapture(bool),
    /// A program asked the host to step its font size by this many steps,
    /// positive to grow.
    FontStep(i32),
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
    pub region: PoolRegionCommand,
    pub scroll_target: DocumentOffset,
    /// The primary cursor's glide anchor as `(row, col)`, or `None` to ease the
    /// cursor normally.
    ///
    /// `Some` names the document display row and grid-absolute column the cursor
    /// rides while this pool glides, so the renderer draws it at the eased
    /// content offset instead of the VT cursor cell.
    pub cursor_anchor: Option<(u64, u16)>,
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
    region: PoolRegionCommand,
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
}

impl Pool {
    /// Create a pool for `region`, its page buffer sized to the region.
    fn new(region: PoolRegionCommand) -> Pool {
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
        let term = Term::new(
            Config::default(),
            &GridSize { rows, cols },
            responses.clone(),
        );

        FillTarget {
            pool,
            index,
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

/// A declared text run as the terminal holds it between projections.
///
/// Mirrors [`TextRunCommand`], the wire shape, but shares its text rather than
/// owning a `String`. Every dirty projection hands the whole run list to the
/// grid, so an owned string would be rebuilt into the grid's shared text once
/// per frame per run, and a gutter declares one run per visible line.
struct StoredTextRun {
    col: i16,
    row: i16,
    scale: u16,
    color: [u8; 3],
    bg: Option<[u8; 3]>,
    text: Arc<str>,
}

impl From<TextRunCommand> for StoredTextRun {
    fn from(command: TextRunCommand) -> Self {
        StoredTextRun {
            col: command.col,
            row: command.row,
            scale: command.scale,
            color: command.color,
            bg: command.bg,
            text: Arc::from(command.text),
        }
    }
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
            line_layout: None,
            panel_seq: Vec::new(),
            icon_seq: Vec::new(),
            text_run_seq: Vec::new(),
            bar_seq: Vec::new(),
            polyline_seq: Vec::new(),
            minimaps: Vec::new(),
            minimap_seq: Vec::new(),
            minimap_contents: HashMap::new(),
            minimap_views: HashMap::new(),
            minimap_content_dirty: false,
            minimap_journal: Vec::new(),
            decoration_seq: 1,
            decorations_dirty: DecorationDirty::default(),
            last_decoration_footprint: Vec::new(),
            footprint_scratch: Vec::new(),
            row_scratch: Vec::new(),
            capture_scratch: Vec::new(),
            last_selection_span: None,
            decoration_damage: Vec::new(),
            last_history: 0,
            pools: BTreeMap::new(),
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
        let redraw = self.advance_inner(bytes);
        self.drain_listener_events();
        self.drain_staged();
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
        self.esc.scan(scan, &mut |event| match event {
            EscEvent::Apc { payload, end } => {
                frames.push((command::decode_with(payload, scratch), end));
            },
            EscEvent::XtVersion => responses.push(XTVERSION_REPLY.as_bytes()),
            EscEvent::OscNotify { code, payload } => {
                if let Some(event) = notification_from_osc(code, payload) {
                    events.push(event);
                }
            },
        });

        // Without a redirect every byte targets the live screen, so apply the
        // commands and feed the whole chunk verbatim, preserving the
        // synchronized-update accounting the redirect path cannot.
        let involves_redirect = redirecting
            || frames.iter().any(|(command, _)| {
                matches!(
                    command,
                    Some(Command::Fill(_)) | Some(Command::Popover(_)) | Some(Command::TextRun(_))
                )
            });
        if !involves_redirect {
            for (command, _) in frames.drain(..) {
                if let Some(command) = command {
                    self.apply_command(command);
                }
            }
            self.frames_scratch = frames;

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
        for (command, end) in frames.drain(..) {
            self.feed_segment(&bytes[start..prefix + end]);
            start = prefix + end;

            if let Some(command) = command {
                // Fill and capture controls always act. A Bar and a TextRun's
                // capture are page-targeted decorations the open fill stores on
                // its slot (the Bar arm and commit_capture route them there), so
                // they act too. The minimap content, view, and drop commands are
                // persistent state whose incremental splices must not be dropped
                // mid-fill, so they act as well. Every other decoration is
                // target-bound and would leak onto the live grid, so it is dropped
                // while a page paints or a content capture runs.
                let routed = matches!(
                    command,
                    Command::Fill(_)
                        | Command::FillEnd
                        | Command::Reset
                        | Command::Popover(_)
                        | Command::PopoverEnd
                        | Command::TextRun(_)
                        | Command::TextRunEnd
                        | Command::Bar(_)
                        | Command::Polyline(_)
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
            | Command::LineLayout(_) => self.stage_or_apply(command),
            Command::Popover(popover) => self.begin_capture(CaptureTarget::Popover(popover)),
            Command::PopoverEnd => self.commit_capture(),
            Command::TextRun(text_run) => self.begin_capture(CaptureTarget::TextRun(text_run)),
            Command::TextRunEnd => self.commit_capture(),
            // A page-targeted bar rides the open fill's slot at feed time. A live
            // bar accumulates onto the grid and stages like the other decorations.
            Command::Bar(bar) => match &mut self.fill {
                Some(fill) => fill.bars.push(bar),
                None => self.stage_or_apply(Command::Bar(bar)),
            },
            // Stroked paths take the same fork as bars. They land on the open
            // fill's slot when one is capturing, and on the live grid otherwise.
            Command::Polyline(polyline) => match &mut self.fill {
                Some(fill) => fill.polylines.push(polyline),
                None => self.stage_or_apply(Command::Polyline(polyline)),
            },
            Command::PoolRegion(region) => {
                let window = region.window;
                match self.pools.get_mut(&region.pool) {
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
                    },
                    None => {
                        self.pools.insert(region.pool, Pool::new(region));
                    },
                }
                self.mark_window_dirty(window);
            },
            Command::Fill(fill) => self.begin_fill(fill.pool, fill.index),
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
                if self.fill.as_ref().map(|fill| fill.pool) == Some(drop.pool) {
                    self.fill = None;
                }
            },
            // A minimap declaration is a decoration cleared by reset, staged like
            // a border. Its content store, views, and drop are persistent state
            // that acts immediately, so an incremental splice is never lost.
            Command::Minimap(_) => self.stage_or_apply(command),
            Command::MinimapLines(lines) => self.splice_minimap_content(lines),
            Command::MinimapView(view) => {
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
                self.minimap_content_dirty = true;
                self.minimap_journal
                    .push(MinimapJournal::Drop(drop.content_id));
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
            Command::ZoomCapture { on } => self.pending_events.push(TermEvent::ZoomCapture(on)),
            Command::FontStep { delta } => self.pending_events.push(TermEvent::FontStep(delta)),
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
    /// Creates the store when absent, so a first splice populates it. Records the
    /// command on [`Self::minimap_journal`] so the next projection replays the
    /// same splice into the grid instead of re-cloning every store.
    fn splice_minimap_content(&mut self, command: MinimapLinesCommand) {
        let store = self.minimap_contents.entry(command.content_id).or_default();
        grid::splice_summaries(store, command.start, command.removed, &command.lines);
        self.minimap_content_dirty = true;
        self.minimap_journal.push(MinimapJournal::Splice(command));
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
                self.borders.push(border);
                self.decorations_dirty.borders = true;
            },
            Command::Panel(panel) => {
                self.panels.push(panel);
                self.panel_seq.push(self.decoration_seq);
                self.decoration_seq += 1;
                self.decorations_dirty.panels = true;
            },
            Command::Scale(scale) => {
                self.scales.push(scale);
                self.decorations_dirty.scales = true;
            },
            Command::ScrollRegion(region) => {
                self.scroll_region = Some(region);
                self.decorations_dirty.scroll_region = true;
            },
            Command::Icon(icon) => {
                self.icons.push(icon);
                self.icon_seq.push(self.decoration_seq);
                self.decoration_seq += 1;
                self.decorations_dirty.icons = true;
            },
            Command::LineLayout(layout) => {
                self.line_layout = Some(layout);
                self.decorations_dirty.line_layout = true;
            },
            Command::Bar(bar) => {
                self.bars.push(bar);
                self.bar_seq.push(self.decoration_seq);
                self.decoration_seq += 1;
                self.decorations_dirty.bars = true;
            },
            Command::Polyline(polyline) => {
                self.polylines.push(polyline);
                self.polyline_seq.push(self.decoration_seq);
                self.decoration_seq += 1;
                self.decorations_dirty.polylines = true;
            },
            Command::Popover(popover) => {
                self.popovers.push(popover);
                self.decorations_dirty.popovers = true;
            },
            Command::TextRun(text_run) => {
                self.text_runs.push(text_run.into());
                self.text_run_seq.push(self.decoration_seq);
                self.decoration_seq += 1;
                self.decorations_dirty.text_runs = true;
            },
            Command::Minimap(minimap) => {
                self.minimaps.push(minimap);
                self.minimap_seq.push(self.decoration_seq);
                self.decoration_seq += 1;
                self.decorations_dirty.minimaps = true;
            },
            Command::Reset => self.clear_decorations(),
            // None of these reach here. apply_command routes them at feed time:
            // the stream and pool controls, and the immediate minimap content,
            // view, and drop commands.
            Command::Fill(_)
            | Command::FillEnd
            | Command::PopoverEnd
            | Command::TextRunEnd
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
            | Command::FontStep { .. } => {},
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
    /// A wrong `moved_rows` costs redundant dirty rows, never a wrong window. Every
    /// row is projected and compared whatever it says.
    ///
    /// Zero is the ordinary value for live output arriving while the window holds
    /// still, since the view is pinned to its content. Those frames leave almost
    /// every row clean.
    pub fn project_scrollback(
        &mut self,
        out: &mut Grid,
        visual: f32,
        moved_rows: isize,
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
            *damage = Damage::Partial(vec![false; out_rows]);
        }

        // Row 0 is the straddle row one line older than the offset's top, so a
        // downward sub-cell shift always has an older row to reveal at the top.
        let top_line = -offset - 1;
        let mut projected = mem::take(&mut self.row_scratch);
        for out_row in 0..out_rows {
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
                rows_dirty[out_row] = true;
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
    fn begin_fill(&mut self, pool: u32, index: u64) {
        self.commit_fill();
        let (rows, cols) = match self.pools.get(&pool) {
            Some(pool) => (
                pool.region.height.max(1) as usize,
                pool.region.width.max(1) as usize,
            ),
            None => (self.term.screen_lines(), self.term.columns()),
        };
        // A committed page's context is reset and parked, so an ordinary scroll
        // paints every page through the same screen and parser. A region resize
        // is the only thing that invalidates it, and that is cold.
        let recycled = self
            .fill_scratch
            .take_if(|fill| fill.term.screen_lines() == rows && fill.term.columns() == cols);

        self.fill = Some(match recycled {
            Some(mut fill) => {
                fill.pool = pool;
                fill.index = index;
                fill
            },
            None => FillTarget::new(pool, index, rows, cols),
        });
    }

    /// Commit the open page fill onto its pool's slot and restore the live grid.
    ///
    /// Projects the fill context's painted cells onto the recycled slot for its
    /// page index in the target pool. The bars and text runs captured while the
    /// page painted move onto the slot with it. A no-op when no fill is open, so
    /// every close trigger (`fill_end`, the next `fill`, `reset`) can call it
    /// unconditionally. The painted page is discarded if its pool was dropped
    /// mid-fill.
    ///
    /// The fill context itself is reset and parked in [`Self::fill_scratch`] for
    /// the next page to paint through.
    fn commit_fill(&mut self) {
        let Some(mut fill) = self.fill.take() else {
            return;
        };

        // Taken before the pool lookup so a page whose pool vanished leaves the
        // context as empty as a committed one does.
        let text_runs = mem::take(&mut fill.text_runs);
        let bars = mem::take(&mut fill.bars);
        let polylines = mem::take(&mut fill.polylines);

        if let Some(pool) = self.pools.get_mut(&fill.pool) {
            let grid = pool.page_pool.fill(fill.index);
            project_term_cells(grid, &fill.term, &self.theme, &self.palette);
            pool.page_pool
                .set_decorations(fill.index, text_runs, bars, polylines);
            pool.content_version = pool.content_version.wrapping_add(1);
            let window = pool.region.window;
            self.mark_window_dirty(window);
        }

        // Parked whether or not the page landed, since a pool dropped mid-fill
        // leaves the context just as reusable as a committed one.
        fill.recycle();
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
                    Some(fill) => fill.text_runs.push(command),
                    None => self.stage_or_apply(Command::TextRun(command)),
                }
            },
        }

        self.capture_scratch = capture.content;
    }

    /// Route a run of VT bytes to the active write target.
    ///
    /// A content capture takes precedence over an open fill, so a text run
    /// nested inside a page captures its text rather than painting it into the
    /// page cells. An open fill with no capture takes the bytes, and with
    /// neither open the live parser does.
    fn feed_segment(&mut self, segment: &[u8]) {
        if let Some(capture) = &mut self.capture {
            capture.content.extend_from_slice(segment);
        } else if let Some(fill) = &mut self.fill {
            fill.parser.advance(&mut fill.term, segment);
        } else {
            self.parser.advance(&mut self.term, segment);
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
        self.panels.clear();
        self.scales.clear();
        self.popovers.clear();
        self.icons.clear();
        self.text_runs.clear();
        self.bars.clear();
        self.polylines.clear();
        self.panel_seq.clear();
        self.icon_seq.clear();
        self.text_run_seq.clear();
        self.bar_seq.clear();
        self.polyline_seq.clear();
        // A minimap declaration is a decoration and clears here. Its content
        // stores and views are persistent state and survive, retired only by
        // their own drop.
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
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.term.resize(GridSize { rows, cols });
        // Pages are sized to each pool's region, not the viewport, so a viewport
        // resize only empties them: the app re-declares regions and refills as
        // its layout recomputes. Any half-painted page is abandoned.
        for pool in self
            .pools
            .values_mut()
            .filter(|pool| pool.region.window == 0)
        {
            pool.page_pool.rebuild(
                pool.region.height.max(1) as usize,
                pool.region.width.max(1) as usize,
            );
            pool.content_version = pool.content_version.wrapping_add(1);
        }
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

    /// The viewport's offset back into scrollback history, in rows: zero at the
    /// live bottom, growing as the view scrolls toward older output.
    ///
    /// Read before and after [`Self::scroll_display`] to recover the rows a
    /// wheel move actually shifted the viewport, so a move clamped at the
    /// history edge is measured by the clamped amount.
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Reset the viewport to the live bottom of history, so the next
    /// [`Self::project`] shows current output again. Used to pin the view on
    /// keyboard input.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
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

        let mut dirty = self.collect_damage(rows, resized);

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

        // Repaint rows the selection entered or left since the last projection,
        // so its INVERSE overlay tracks a drag even on rows VT damage did not
        // touch. A `Damage::Full` frame already covers every row.
        let span = selection.and_then(|s| selection_span(&s, offset, rows));
        if span != self.last_selection_span
            && let Damage::Partial(rows_dirty) = &mut dirty
        {
            // An idle frame carries an empty vec, so grow it before marking the
            // entered rows. A genuinely-damaged frame already sizes it to rows.
            rows_dirty.resize(rows, false);
            for (lo, hi) in span.into_iter().chain(self.last_selection_span) {
                for slot in rows_dirty.iter_mut().skip(lo).take(hi - lo + 1) {
                    *slot = true;
                }
            }
        }
        self.last_selection_span = span;

        // A scroll damages the whole terminal even though the content only moved
        // up. Sliding the grid by the same amount and then keeping the rows that
        // come out identical turns that into damage naming the few rows that
        // really changed, which is what lets the render passes reuse the rest.
        //
        // The slide is only a guess at how far the content moved, and nothing
        // rests on it. Every row is still projected and compared, so a wrong
        // guess marks every row dirty, which is what a full frame did anyway.
        let sliding =
            matches!(dirty, Damage::Full) && !resized && offset == 0 && grew > 0 && grew < rows;
        if sliding {
            grid.scroll_by(grew as isize);
            dirty = Damage::Partial(vec![false; rows]);
        }

        // Visit only the rows damage marked, and read their cells straight from
        // the terminal grid rather than filtering the whole viewport per cell.
        // `display_iter` maps grid `line + offset` to viewport `row`. Inverting
        // it as `line = row - offset` yields the same cells and points.
        let term_grid = self.term.grid();
        let mut projected = mem::take(&mut self.row_scratch);
        for row in 0..rows {
            if !sliding && !dirty.is_dirty(row) {
                continue;
            }
            let line = Line(row as i32 - offset);
            let source = &term_grid[line];
            let project = |col: usize| {
                let mut cell = project_cell(
                    &source[Column(col)],
                    content.colors,
                    &self.theme,
                    &self.palette,
                );
                if selection.is_some_and(|s| s.contains(Point::new(line, Column(col)))) {
                    cell.flags = cell.flags.toggle(Flags::INVERSE);
                }
                cell
            };

            // Without a slide nothing compares the row before it lands, so it goes
            // straight into the grid rather than through the scratch and out again.
            if !sliding {
                for (col, out) in grid.row_mut(row).iter_mut().enumerate() {
                    *out = project(col);
                }
                continue;
            }

            projected.clear();
            projected.extend((0..cols).map(project));

            if grid.row(row) == projected.as_slice() {
                continue;
            }
            if let Damage::Partial(rows_dirty) = &mut dirty {
                rows_dirty[row] = true;
            }
            grid.row_mut(row).copy_from_slice(&projected);
        }
        self.row_scratch = projected;

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
            grid.bump_popovers_epoch();
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
            grid.bump_text_runs_epoch();
        }
        if self.decorations_dirty.bars || layout_changed {
            apply_bars(grid, &self.bars, &self.bar_seq);
        }
        if self.decorations_dirty.polylines || layout_changed {
            apply_polylines(grid, &self.polylines, &self.polyline_seq);
        }
        if self.decorations_dirty.minimaps || resized {
            apply_minimaps(grid, &self.minimaps, &self.minimap_seq, &self.minimap_views);
            grid.bump_minimap_epoch();
        }
        // A resize empties the grid's stores, so re-clone them wholesale and drop
        // the journal the clone subsumes. Otherwise replay only the splices and
        // drops since the last projection. The grid's stores match the term's as
        // of then, so the replay reproduces the current stores while cloning just
        // each splice's lines. A viewport-only frame touches neither.
        if resized {
            grid.set_minimap_contents(self.minimap_contents.clone());
            self.minimap_journal.clear();
            grid.bump_minimap_epoch();
            self.minimap_content_dirty = false;
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
            grid.bump_minimap_epoch();
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
            self.decoration_damage = vec![false; rows];
        }
        decoration_footprint(
            &self.borders,
            &self.panels,
            &self.scales,
            rows,
            &mut self.footprint_scratch,
        );
        if self.decorations_dirty.borders
            || self.decorations_dirty.panels
            || self.decorations_dirty.scales
            || resized
        {
            for ((damage, &now), &before) in self
                .decoration_damage
                .iter_mut()
                .zip(&self.footprint_scratch)
                .zip(&self.last_decoration_footprint)
            {
                if now || before {
                    *damage = true;
                }
            }
        }
        mem::swap(
            &mut self.last_decoration_footprint,
            &mut self.footprint_scratch,
        );

        self.decorations_dirty = DecorationDirty::default();

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
                let mut lines = lines.peekable();
                if lines.peek().is_none() {
                    return Damage::Partial(Vec::new());
                }
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

const ESC: u8 = 0x1b;
/// Byte after `ESC` that opens an APC string (`ESC _`).
const APC_INTRODUCER: u8 = b'_';
/// Byte after `ESC` that closes a string control (`ESC \`, the ST).
const STRING_TERMINATOR: u8 = b'\\';
/// Bell, accepted as an alternate string terminator.
const BEL: u8 = 0x07;
/// A sequence the fused scanner recognized, emitted as it completes.
///
/// The payloads borrow the scanner's reused buffer, so each is valid only for the
/// call that yields it.
enum EscEvent<'a> {
    /// An APC string's payload, the bytes between the introducer and the
    /// terminator, paired with the offset one past that terminator.
    ///
    /// The offset lets a caller split the input at frame boundaries to route the
    /// content between frames. A payload split across calls is retained until its
    /// terminator arrives, and its offset is then relative to the call it
    /// completes in.
    Apc { payload: &'a [u8], end: usize },
    /// An XTVERSION query (`CSI > Ps q`) completed. The parameters are dropped,
    /// since the reply is fixed.
    XtVersion,
    /// An OSC 9 or OSC 777 notification, as its code and the bytes after the
    /// code's `;`.
    OscNotify { code: u32, payload: &'a [u8] },
}

/// Recognizes the escape sequences the vte parser does not hand back, in one walk
/// of the stream.
///
/// Three kinds go unreported by `alacritty_terminal`, so the driver watches the
/// bytes for them itself: APC strings (`ESC _ ... ESC \`), which carry stoatty's own
/// protocol; XTVERSION queries (`CSI > Ps q`), which vte dispatches in the plain
/// `CSI Ps q` form but not this one; and OSC 9 / OSC 777 desktop notifications,
/// where vte handles OSC 0/1/2/4/52 and drops these. Each is framed across
/// [`Terminal::advance`] calls, so a sequence split between chunks completes on the
/// chunk that ends it.
///
/// All three open on the byte after `ESC`, and no two can be open at once, so one
/// state machine recognizes all of them for the cost of the one walk. A sequence
/// written inside another's payload is part of that payload rather than a sequence
/// of its own, which is how the vte parser reads the same bytes.
///
/// Recognizing a stoatty frame among the APC payloads is the decoder's job, not this
/// scanner's. Mapping a notification to an event is [`notification_from_osc`]'s.
#[derive(Default)]
struct EscScanner {
    state: EscState,
    /// The payload of whichever sequence is open, shared because the APC and OSC
    /// states that fill it are mutually exclusive.
    payload: Vec<u8>,
    /// The OSC code being accumulated, then the one a buffered payload belongs to.
    code: u32,
    /// Set when an OSC payload outgrew its cap, so it is dropped at its terminator
    /// rather than reported truncated.
    overflow: bool,
}

#[derive(Clone, Copy, Default)]
enum EscState {
    #[default]
    Ground,
    Escape,
    /// Seen `ESC _`, buffering the APC payload.
    Apc,
    /// Seen `ESC` inside an APC payload, awaiting the `\` that terminates it.
    ApcEscape,
    /// Seen `ESC [`, waiting on the `>` that marks the private query.
    CsiEntry,
    /// Seen `ESC [ >`, consuming parameter bytes until the final byte.
    CsiGt,
    /// Seen `ESC ]`, accumulating the numeric code up to its `;`.
    OscPrefix,
    /// Inside an OSC 9 or 777 payload, which is buffered.
    OscBuffer,
    /// Seen `ESC` inside a buffered OSC payload.
    OscBufferEscape,
    /// Inside any other OSC, skipped to its terminator so a large clipboard write
    /// is never copied.
    OscSkip,
    /// Seen `ESC` inside a skipped OSC.
    OscSkipEscape,
}

impl EscScanner {
    /// Whether no sequence is open, so a caller may skip a chunk containing no
    /// `ESC` without missing one.
    fn is_idle(&self) -> bool {
        matches!(self.state, EscState::Ground)
    }

    /// Feed `bytes`, invoking `emit` for each sequence that completes within them.
    fn scan(&mut self, bytes: &[u8], emit: &mut impl FnMut(EscEvent<'_>)) {
        let mut i = 0;

        while i < bytes.len() {
            let byte = bytes[i];
            match self.state {
                // Nothing is open, so jump to the next ESC and skip the plain bytes
                // between rather than stepping each one.
                EscState::Ground => match memchr::memchr(ESC, &bytes[i..]) {
                    Some(off) => {
                        self.state = EscState::Escape;
                        i += off;
                    },
                    None => break,
                },
                EscState::Escape => {
                    self.state = match byte {
                        APC_INTRODUCER => {
                            self.payload.clear();
                            EscState::Apc
                        },
                        b'[' => EscState::CsiEntry,
                        OSC_INTRODUCER => {
                            self.code = 0;
                            self.payload.clear();
                            self.overflow = false;
                            EscState::OscPrefix
                        },
                        ESC => EscState::Escape,
                        _ => EscState::Ground,
                    };
                },

                EscState::Apc => match byte {
                    ESC => self.state = EscState::ApcEscape,
                    BEL => self.finish_apc(i + 1, emit),
                    _ => self.push_apc(byte),
                },
                EscState::ApcEscape => match byte {
                    STRING_TERMINATOR => self.finish_apc(i + 1, emit),
                    ESC => self.state = EscState::ApcEscape,
                    _ => {
                        self.payload.clear();
                        self.state = EscState::Ground;
                    },
                },

                EscState::CsiEntry => {
                    self.state = match byte {
                        b'>' => EscState::CsiGt,
                        _ => EscState::Ground,
                    }
                },
                EscState::CsiGt => {
                    self.state = match byte {
                        // Parameter and intermediate bytes keep the sequence open.
                        0x20..=0x3f => EscState::CsiGt,
                        b'q' => {
                            emit(EscEvent::XtVersion);
                            EscState::Ground
                        },
                        ESC => EscState::Escape,
                        _ => EscState::Ground,
                    }
                },

                EscState::OscPrefix => match byte {
                    b'0'..=b'9' => {
                        self.code = self
                            .code
                            .saturating_mul(10)
                            .saturating_add(u32::from(byte - b'0'));
                    },
                    b';' => {
                        self.state = if self.code == 9 || self.code == 777 {
                            EscState::OscBuffer
                        } else {
                            EscState::OscSkip
                        };
                    },
                    ESC => self.state = EscState::OscSkipEscape,
                    BEL => self.state = EscState::Ground,
                    _ => self.state = EscState::OscSkip,
                },
                EscState::OscBuffer => match byte {
                    ESC => self.state = EscState::OscBufferEscape,
                    BEL => self.finish_osc(emit),
                    _ => self.push_osc(byte),
                },
                EscState::OscBufferEscape => match byte {
                    STRING_TERMINATOR => self.finish_osc(emit),
                    ESC => self.state = EscState::OscBufferEscape,
                    _ => {
                        self.payload.clear();
                        self.state = EscState::Ground;
                    },
                },
                EscState::OscSkip => match byte {
                    ESC => self.state = EscState::OscSkipEscape,
                    BEL => self.state = EscState::Ground,
                    _ => {},
                },
                EscState::OscSkipEscape => match byte {
                    STRING_TERMINATOR => self.state = EscState::Ground,
                    ESC => self.state = EscState::OscSkipEscape,
                    _ => self.state = EscState::OscSkip,
                },
            }
            i += 1;
        }
    }

    /// Buffer one APC payload byte, abandoning the frame if it overruns the cap.
    ///
    /// The cap is [`MAX_APC_PAYLOAD`], shared with the encoders so a frame they emit
    /// can never be the thing this discards.
    fn push_apc(&mut self, byte: u8) {
        if self.payload.len() < MAX_APC_PAYLOAD {
            self.payload.push(byte);
        } else {
            self.payload.clear();
            self.state = EscState::Ground;
        }
    }

    /// Emit the buffered APC payload, ending one past its terminator at `end`.
    fn finish_apc(&mut self, end: usize, emit: &mut impl FnMut(EscEvent<'_>)) {
        emit(EscEvent::Apc {
            payload: &self.payload,
            end,
        });
        self.payload.clear();
        self.state = EscState::Ground;
    }

    /// Buffer one OSC payload byte, marking overflow past the cap so the sequence
    /// is dropped at its terminator rather than reported truncated.
    fn push_osc(&mut self, byte: u8) {
        if self.payload.len() < MAX_OSC_NOTIFY_BYTES {
            self.payload.push(byte);
        } else {
            self.overflow = true;
        }
    }

    /// Emit the buffered OSC payload unless it overran the cap, then reset.
    fn finish_osc(&mut self, emit: &mut impl FnMut(EscEvent<'_>)) {
        if !self.overflow {
            emit(EscEvent::OscNotify {
                code: self.code,
                payload: &self.payload,
            });
        }
        self.payload.clear();
        self.overflow = false;
        self.state = EscState::Ground;
    }
}

/// The XTVERSION reply naming this terminal, answering `CSI > Ps q`.
///
/// A DCS string (`ESC P > | name ESC \`) carrying the terminal name and version,
/// the response xterm defined for XTVERSION. Programs such as fish query it at
/// startup and gate optional features on the answer; the vte parser does not
/// dispatch it, so the driver synthesizes this reply itself.
const XTVERSION_REPLY: &str = concat!("\x1bP>|stoatty(", env!("CARGO_PKG_VERSION"), ")\x1b\\");

/// Byte after `ESC` that opens an OSC string (`ESC ]`).
const OSC_INTRODUCER: u8 = b']';

/// Cap on a buffered OSC 9 / OSC 777 notification payload, bounding memory
/// against a sequence that never terminates. A larger notification is discarded.
const MAX_OSC_NOTIFY_BYTES: usize = 4096;

/// Map a scanned OSC notification `(code, payload)` to a [`TermEvent::Notification`].
///
/// OSC 9 carries only a body. OSC 777's payload is `kind;title;body`, where only
/// the `notify` kind yields an event and a `;` inside the body is preserved. A
/// code or kind that is not a notification yields `None`.
fn notification_from_osc(code: u32, payload: &[u8]) -> Option<TermEvent> {
    match code {
        9 => Some(TermEvent::Notification {
            title: None,
            body: String::from_utf8_lossy(payload).into_owned(),
        }),
        777 => {
            let text = String::from_utf8_lossy(payload);
            let mut parts = text.splitn(3, ';');
            if parts.next()? != "notify" {
                return None;
            }
            let title = parts.next()?.to_owned();
            let body = parts.next().unwrap_or_default().to_owned();
            Some(TermEvent::Notification {
                title: Some(title),
                body,
            })
        },
        _ => None,
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

/// The inclusive viewport row span a selection covers, clamped to the grid, or
/// `None` when it falls entirely below the viewport.
///
/// `offset` is the display offset the projection ran at, converting the
/// selection's terminal lines to viewport rows the same way the cell loop does.
fn selection_span(range: &SelectionRange, offset: i32, rows: usize) -> Option<(usize, usize)> {
    let top = (range.start.line.0 + offset).max(0) as usize;
    let bottom = (range.end.line.0 + offset).max(0) as usize;
    if top >= rows {
        return None;
    }
    Some((top, bottom.min(rows - 1)))
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

/// Mark the grid rows the damage-tracked decorations occupy, clamped to `rows`.
///
/// Each border and panel region spans `top..=top+height-1` and each scale block
/// spans `top..top+scale`. Borders and scales stamp those rows' cells, so the
/// span mirrors what [`frame_region`] and [`apply_scales`] touch. A panel does
/// not stamp cells, but its chrome covers the same rows, so it is tracked here
/// too.
fn decoration_footprint(
    borders: &[BorderCommand],
    panels: &[PanelCommand],
    scales: &[ScaleCommand],
    rows: usize,
    out: &mut Vec<bool>,
) {
    out.clear();
    out.resize(rows, false);

    for border in borders {
        if border.width == 0 || border.height == 0 {
            continue;
        }
        let top = border.top as usize;
        if top >= rows {
            continue;
        }
        let bottom = (top + border.height as usize - 1).min(rows - 1);
        out[top..=bottom].fill(true);
    }

    for panel in panels {
        if panel.width == 0 || panel.height == 0 {
            continue;
        }
        let top = panel.top as usize;
        if top >= rows {
            continue;
        }
        let bottom = (top + panel.height as usize - 1).min(rows - 1);
        out[top..=bottom].fill(true);
    }

    for scale in scales {
        let top = scale.top as usize;
        let end = (top + scale.scale as usize).min(rows);
        if top < end {
            out[top..end].fill(true);
        }
    }
}

/// Whether any row of the half-open span `top..end` is dirty in `rows_dirty`.
///
/// Clamped to `rows`, so a region declared past the screen tests only the part
/// that lands on it. Wire coordinates are untrusted and may point anywhere.
fn span_is_dirty(top: usize, end: usize, rows: usize, rows_dirty: &Damage) -> bool {
    (top..end.min(rows)).any(|row| rows_dirty.is_dirty(row))
}

/// Stamp every stored border region's perimeter edges onto `grid`, over the rows
/// `rows_dirty` marks.
///
/// The cell projection resets borders to none, so a stamped row has to be restamped
/// after it. Only the projected rows lost theirs, which is what `rows_dirty` names.
/// Pass [`Damage::Full`] to stamp regardless, for a resize that cleared the cells or
/// a command list that no longer matches the grid.
///
/// Edges outside the grid are skipped, so a region may extend past it.
fn apply_borders(grid: &mut Grid, commands: &[BorderCommand], rows_dirty: &Damage) {
    let rows = grid.rows();
    for command in commands {
        let top = command.top as usize;
        let end = top + command.height as usize;
        if span_is_dirty(top, end, rows, rows_dirty) {
            frame_region(grid, command, rows_dirty);
        }
    }
}

fn frame_region(grid: &mut Grid, command: &BorderCommand, rows_dirty: &Damage) {
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
    let last_col = right.min(cols.saturating_sub(1));

    // A horizontal edge is one contiguous run, so it takes one bounds check for the
    // row rather than one per column.
    if left < cols {
        if top < rows && rows_dirty.is_dirty(top) {
            for cell in &mut grid.row_mut(top)[left..=last_col] {
                cell.borders.top = Some(border);
            }
        }
        if bottom < rows && rows_dirty.is_dirty(bottom) {
            for cell in &mut grid.row_mut(bottom)[left..=last_col] {
                cell.borders.bottom = Some(border);
            }
        }
    }

    for row in top..=bottom.min(rows.saturating_sub(1)) {
        if !rows_dirty.is_dirty(row) {
            continue;
        }
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

fn grid_panel_shadow(shadow: command::PanelShadow) -> PanelShadow {
    match shadow {
        command::PanelShadow::None_ => PanelShadow::None_,
        command::PanelShadow::Drop => PanelShadow::Drop,
        command::PanelShadow::Tucked => PanelShadow::Tucked,
        command::PanelShadow::Overhang => PanelShadow::Overhang,
    }
}

/// Claim each stored scale command's block on `grid`, over the rows `rows_dirty`
/// marks.
///
/// The cell projection resets every cell to [`Scale::Single`], so a claimed row has
/// to be reclaimed after it, and only the projected rows lost theirs. Pass
/// [`Damage::Full`] to claim regardless, as [`apply_borders`] documents.
///
/// An origin outside the grid is skipped, since wire coordinates are untrusted and
/// may point past the screen. A block is claimed whole or not at all, since one call
/// places all of it.
fn apply_scales(grid: &mut Grid, commands: &[ScaleCommand], rows_dirty: &Damage) {
    let rows = grid.rows();
    for command in commands {
        let (row, col) = (command.top as usize, command.left as usize);
        if row >= rows || col >= grid.cols() {
            continue;
        }
        // A scale below 2 claims the origin cell alone, anything above it a square.
        let end = row + (command.scale as usize).max(1);
        if span_is_dirty(row, end, rows, rows_dirty) {
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
    grid.fill_overlays(commands.len(), |index| popover_overlay(&commands[index]));
}

fn apply_panels(grid: &mut Grid, commands: &[PanelCommand], seqs: &[u32]) {
    grid.fill_panels(commands.len(), |index| {
        panel_grid(&commands[index], seqs[index])
    });
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
fn apply_icons(grid: &mut Grid, commands: &[IconCommand], seqs: &[u32]) {
    grid.fill_icons(commands.len(), |index| {
        let command = &commands[index];
        Icon {
            top: command.top,
            left: command.left,
            kind: grid_icon_kind(command.kind),
            color: Rgb::new(command.color[0], command.color[1], command.color[2]),
            size: command.size,
            offset: command.offset,
            seq: seqs[index],
        }
    });
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

/// Stamp the buffered pages' text runs and bars into the composed `out` grid,
/// translated from page-local rows to the window's rows.
///
/// Each page the window straddles contributes its slot decorations shifted by
/// the whole-row gap between the page's document start and the window top `top`,
/// in sixteenth-cell units. A decoration lying fully above or below the composed
/// rows is dropped. The sub-cell scroll fraction stays with the renderer, so
/// these shift by the same pixel offset as the cells [`PagePool::compose`]
/// copied.
///
/// The page decorations are already in grid form (see
/// [`PagePool::set_decorations`]), so re-stamping is Copy-field arithmetic plus
/// an `Arc` bump per run, with no per-frame decode. They carry `seq` 0 as the
/// base layer of the composite, so any declared panel above occludes them.
fn stamp_pool_decorations(pool: &PagePool, out: &mut Grid, top: i64, page_rows: usize) {
    let out_rows = out.rows() as i64;
    let out_rows_16 = out_rows * 16;
    let first_page = top.div_euclid(page_rows as i64);
    let last_page = (top + out_rows - 1).div_euclid(page_rows as i64);

    // Built straight into the target's own lists rather than into fresh ones it
    // would then adopt. A gliding pool stamps every frame, so what is already
    // here is exactly what would otherwise be dropped and reallocated.
    let mut text_runs = mem::take(out.text_runs_mut());
    let mut bars = mem::take(out.bars_mut());
    let mut polylines = mem::take(out.polylines_mut());
    text_runs.clear();
    bars.clear();
    // Paths keep their entries, so each one's point list is refilled rather than
    // collected fresh. `kept` counts the survivors, and the tail is dropped once
    // the pages are walked.
    let mut kept = 0;

    for page in first_page..=last_page {
        let Some((page_runs, page_bars, page_polylines)) = pool.page_decorations(page as u64)
        else {
            continue;
        };
        let shift = 16 * (page * page_rows as i64 - top);

        for run in page_runs {
            let row = run.row as i64 + shift;
            if row + 16 <= 0 || row >= out_rows_16 {
                continue;
            }
            if let Ok(row) = i16::try_from(row) {
                text_runs.push(TextRun { row, ..run.clone() });
            }
        }

        for bar in page_bars {
            let y = bar.y as i64 + shift;
            if y + bar.height as i64 <= 0 || y >= out_rows_16 {
                continue;
            }
            if let Ok(y) = i16::try_from(y) {
                bars.push(Bar { y, ..*bar });
            }
        }

        for polyline in page_polylines {
            // Shifted whole rather than per point. A path that straddles the
            // edge stays intact for the renderer to scissor, since clipping a
            // segment here would need its pixel geometry. A point that cannot
            // survive the shift drops the whole path, because a partial point
            // list would draw a wrong line rather than none.
            if kept == polylines.len() {
                polylines.push(Polyline {
                    points: Vec::new(),
                    width: 0,
                    color: Rgb::new(0, 0, 0),
                    seq: 0,
                });
            }
            let slot = &mut polylines[kept];

            slot.points.clear();
            let shifted = polyline.points.iter().try_for_each(|&[x, y]| {
                match i16::try_from(y as i64 + shift) {
                    Ok(y) => {
                        slot.points.push([x, y]);
                        Ok(())
                    },
                    Err(_) => Err(()),
                }
            });
            if shifted.is_err() {
                continue;
            }
            if slot
                .points
                .iter()
                .all(|&[_, y]| (y as i64) < 0 || y as i64 >= out_rows_16)
            {
                continue;
            }

            slot.width = polyline.width;
            slot.color = polyline.color;
            slot.seq = polyline.seq;
            kept += 1;
        }
    }

    polylines.truncate(kept);
    out.set_text_runs(text_runs);
    out.set_bars(bars);
    out.set_polylines(polylines);
}

/// Replace the grid's text-run list with each stored text-run command's run.
///
/// Grid-level like the overlays, so the full list is set each projection rather
/// than stamped per cell. The declared row is a logical row resolved through the
/// line layout, so a run tracks expansions above it. The renderer clamps an
/// out-of-grid anchor, so wire coordinates need no guard here.
fn apply_text_runs(grid: &mut Grid, runs: &[StoredTextRun], seqs: &[u32]) {
    grid.fill_text_runs(runs.len(), |grid, index| {
        let run = &runs[index];
        TextRun {
            col: run.col,
            row: resolve_logical_row(grid, run.row),
            scale: run.scale,
            color: Rgb::new(run.color[0], run.color[1], run.color[2]),
            bg: run.bg.map(|b| Rgb::new(b[0], b[1], b[2])),
            text: Arc::clone(&run.text),
            seq: seqs[index],
        }
    });
}

/// Replace the grid's bar list with each stored bar command's rectangle.
///
/// Grid-level like the overlays, so the full list is set each projection rather
/// than stamped per cell. The declared `y` is a logical row resolved through the
/// line layout, so a bar tracks expansions above it.
fn apply_bars(grid: &mut Grid, commands: &[BarCommand], seqs: &[u32]) {
    grid.fill_bars(commands.len(), |grid, index| {
        let command = &commands[index];
        Bar {
            x: command.x,
            y: resolve_logical_row(grid, command.y),
            width: command.width,
            height: command.height,
            color: Rgb::new(command.color[0], command.color[1], command.color[2]),
            seq: seqs[index],
        }
    });
}

/// Replace the grid's polyline list with each stored command's path.
///
/// Grid-level like the bars, so the full list is set each projection. Points
/// pass through unresolved, because a path is free geometry rather than a
/// component anchored to a logical row.
///
/// Each path's point list is refilled where it sits rather than cloned from the
/// command, since this re-runs on every line-layout change and a commit graph
/// declares a path per lane.
fn apply_polylines(grid: &mut Grid, commands: &[PolylineCommand], seqs: &[u32]) {
    let mut polylines = mem::take(grid.polylines_mut());

    polylines.resize_with(commands.len(), || Polyline {
        points: Vec::new(),
        width: 0,
        color: Rgb::new(0, 0, 0),
        seq: 0,
    });
    for ((slot, command), &seq) in polylines.iter_mut().zip(commands).zip(seqs) {
        slot.points.clear();
        slot.points.extend_from_slice(&command.points);
        slot.width = command.width;
        slot.color = Rgb::new(command.color[0], command.color[1], command.color[2]);
        slot.seq = seq;
    }

    grid.set_polylines(polylines);
}

/// Project the declared minimap strips onto `grid`, each joined with its view.
///
/// The line summaries a strip renders are projected separately (see
/// [`Grid::set_minimap_contents`]), so this small join runs on every strip or
/// view change without re-cloning the content stores.
fn apply_minimaps(
    grid: &mut Grid,
    commands: &[MinimapCommand],
    seqs: &[u32],
    views: &HashMap<u32, MinimapView>,
) {
    grid.fill_minimaps(commands.len(), |index| {
        let command = &commands[index];
        Minimap {
            command: command.clone(),
            seq: seqs[index],
            view: views.get(&command.strip_id).copied(),
        }
    });
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
        bold: command.bold,
        content: command.content.clone(),
    }
}

fn panel_grid(command: &PanelCommand, seq: u32) -> Panel {
    Panel {
        top: command.top,
        left: command.left,
        width: command.width,
        height: command.height,
        style: grid_border_style(command.style),
        border: Rgb::new(command.border[0], command.border[1], command.border[2]),
        corner_radius: command.corner_radius,
        fill: command.fill.map(|[r, g, b]| Rgb::new(r, g, b)),
        shadow: grid_panel_shadow(command.shadow),
        inset_x: command.inset_x,
        above_pools: command.above_pools,
        seq,
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
    use super::{
        Cursor, CursorShape, Damage, EscEvent, EscScanner, TermEvent, Terminal,
        MAX_OSC_NOTIFY_BYTES, XTVERSION_REPLY,
    };
    use crate::{
        grid::{
            Bar, Border, BorderStyle, Cell, DocumentOffset, Flags, Grid, Icon, IconKind, Minimap,
            MinimapView, Overlay, Panel, PanelShadow, Polyline, Rgb, Scale, ScrollRegion, TextRun,
            UnderlineStyle,
        },
        theme::Theme,
    };
    use stoatty_protocol::command::{
        encode_bar, encode_border, encode_config_reload, encode_fill, encode_fill_end,
        encode_font_step, encode_hello, encode_icon, encode_ident_reply, encode_line_layout,
        encode_minimap, encode_minimap_drop, encode_minimap_lines, encode_minimap_view,
        encode_panel, encode_polyline, encode_pool_cursor, encode_pool_drop, encode_pool_region,
        encode_popover, encode_reposition, encode_reset, encode_scale, encode_scroll,
        encode_scroll_region, encode_text_run, encode_window_open, encode_zoom_capture, BarCommand,
        BorderCommand, BorderStyle as ProtoBorderStyle, FillCommand, HelloCommand, IconCommand,
        IconKind as ProtoIconKind, IdentReply, LineLayoutCommand, MinimapCommand,
        MinimapDropCommand, MinimapLinesCommand, MinimapRun, MinimapViewCommand, PanelCommand,
        PanelShadow as ProtoPanelShadow, PolylineCommand, PoolCursorCommand, PoolDropCommand,
        PoolRegionCommand, PopoverCommand, RepositionCommand, ScaleCommand, ScrollCommand,
        ScrollRegionCommand, TextRunCommand, WindowOpenCommand,
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
    fn idle_frame_leaves_clean_rows_untouched() {
        let mut terminal = Terminal::new(3, 4, Theme::default());
        let mut grid = Grid::new(3, 4);
        terminal.advance(b"hi");
        terminal.project(&mut grid);

        // An idle frame with no new input damages only the cursor's line (row
        // 0), so the rows below it stay out of the projection. Sentinel them and
        // confirm the re-projection never touches a clean row.
        grid.get_mut(1, 0).ch = 'Z';
        grid.get_mut(2, 0).ch = 'Z';
        let row1 = *grid.get(1, 0);
        let row2 = *grid.get(2, 0);

        terminal.project(&mut grid);

        assert_eq!(
            *grid.get(1, 0),
            row1,
            "a clean row is untouched by an idle frame"
        );
        assert_eq!(
            *grid.get(2, 0),
            row2,
            "a clean row is untouched by an idle frame"
        );
    }

    #[test]
    fn single_dirty_row_reprojects_only_that_row() {
        let mut terminal = Terminal::new(3, 4, Theme::default());
        let mut grid = Grid::new(3, 4);
        terminal.advance(b"aa\r\nbb\r\ncc");
        terminal.project(&mut grid);

        // Sentinel each row's last cell, then damage only the cursor row (row 2,
        // where "cc" left the cursor) by writing in place so the cursor never
        // leaves it.
        for r in 0..3 {
            grid.get_mut(r, 3).ch = 'Z';
        }
        let row0 = *grid.get(0, 3);
        let row1 = *grid.get(1, 3);

        terminal.advance(b"d");
        terminal.project(&mut grid);

        assert_eq!(grid.get(2, 2).ch, 'd', "the dirty row is re-projected");
        assert_ne!(
            grid.get(2, 3).ch,
            'Z',
            "the dirty row's sentinel is overwritten"
        );
        assert_eq!(*grid.get(0, 3), row0, "an unchanged row keeps its cells");
        assert_eq!(*grid.get(1, 3), row1, "an unchanged row keeps its cells");
    }

    #[test]
    fn projection_without_decoration_change_keeps_every_epoch() {
        let mut terminal = Terminal::new(3, 4, Theme::default());
        let mut grid = Grid::new(3, 4);
        terminal.project(&mut grid);
        let popovers = grid.popovers_epoch();
        let text_runs = grid.text_runs_epoch();
        let minimap = grid.minimap_epoch();

        terminal.project(&mut grid);

        assert_eq!(grid.popovers_epoch(), popovers, "popovers epoch stable");
        assert_eq!(grid.text_runs_epoch(), text_runs, "text-runs epoch stable");
        assert_eq!(grid.minimap_epoch(), minimap, "minimap epoch stable");
    }

    #[test]
    fn a_decoration_change_bumps_only_its_epoch() {
        let mut terminal = Terminal::new(3, 4, Theme::default());
        let mut grid = Grid::new(3, 4);
        terminal.project(&mut grid);
        let popovers = grid.popovers_epoch();
        let text_runs = grid.text_runs_epoch();
        let minimap = grid.minimap_epoch();

        terminal.decorations_dirty.popovers = true;
        terminal.project(&mut grid);

        assert_eq!(
            grid.popovers_epoch(),
            popovers + 1,
            "the changed decoration bumps its epoch"
        );
        assert_eq!(
            grid.text_runs_epoch(),
            text_runs,
            "an unchanged decoration is left alone"
        );
        assert_eq!(
            grid.minimap_epoch(),
            minimap,
            "an unchanged decoration is left alone"
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
    fn set_theme_recolors_already_written_cells() {
        let before = Theme {
            foreground: Rgb::new(4, 5, 6),
            background: Rgb::new(1, 2, 3),
            cursor: Rgb::new(7, 8, 9),
            ansi: [Rgb::new(10, 11, 12); 16],
        };
        let mut terminal = Terminal::new(1, 4, before);
        let mut grid = Grid::new(1, 4);
        terminal.advance(b"a\x1b[31mb");

        terminal.set_theme(Theme {
            foreground: Rgb::new(40, 50, 60),
            background: Rgb::new(10, 20, 30),
            cursor: Rgb::new(70, 80, 90),
            ansi: [Rgb::new(100, 110, 120); 16],
        });
        terminal.project(&mut grid);

        assert_eq!(
            grid.get(0, 0).fg,
            Rgb::new(40, 50, 60),
            "default fg re-resolves against the new theme"
        );
        assert_eq!(
            grid.get(0, 0).bg,
            Rgb::new(10, 20, 30),
            "default bg re-resolves against the new theme"
        );
        assert_eq!(
            grid.get(0, 1).fg,
            Rgb::new(100, 110, 120),
            "the rebuilt palette recolors an ANSI cell written before the swap"
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
    fn hello_yields_event_and_replies_when_ident_set() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        let ident = IdentReply {
            pid: 42,
            log_id: "20260718-143000-42".to_string(),
            hostname: "host".to_string(),
            version: "v".to_string(),
        };
        terminal.set_ident(ident.clone());

        let hello = HelloCommand {
            pid: 99,
            log_id: "20260718-143022-99".to_string(),
            hostname: "remote".to_string(),
            version: "pv".to_string(),
        };
        terminal.advance(&encode_hello(&hello));

        assert_eq!(terminal.take_events(), vec![TermEvent::Hello(hello)]);
        assert_eq!(terminal.take_responses(), encode_ident_reply(&ident));
    }

    #[test]
    fn hello_yields_event_but_no_reply_without_ident() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        let hello = HelloCommand {
            pid: 99,
            log_id: "20260718-143022-99".to_string(),
            hostname: "remote".to_string(),
            version: "pv".to_string(),
        };
        terminal.advance(&encode_hello(&hello));

        assert_eq!(terminal.take_events(), vec![TermEvent::Hello(hello)]);
        assert!(
            terminal.take_responses().is_empty(),
            "no reply without an installed ident"
        );
    }

    #[test]
    fn surfaces_title_event() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b]2;hi\x07");
        assert_eq!(terminal.take_events(), vec![TermEvent::Title("hi".into())]);
        assert!(
            terminal.take_events().is_empty(),
            "queue drained after taking"
        );
    }

    #[test]
    fn surfaces_bell_event() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x07");
        assert_eq!(terminal.take_events(), vec![TermEvent::Bell]);
    }

    #[test]
    fn surfaces_clipboard_store_event() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b]52;c;aGk=\x07");
        assert_eq!(
            terminal.take_events(),
            vec![TermEvent::ClipboardStore("hi".into())],
            "OSC 52 payload is base64-decoded"
        );
    }

    #[test]
    fn title_stack_round_trip_restores_saved_title() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b]2;A\x07\x1b[22t\x1b]2;B\x07\x1b[23t");
        assert_eq!(
            terminal.take_events(),
            vec![
                TermEvent::Title("A".into()),
                TermEvent::Title("B".into()),
                TermEvent::Title("A".into()),
            ],
            "push saves A, B replaces it, pop restores A"
        );
    }

    #[test]
    fn answers_osc11_background_query() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b]11;?\x1b\\");
        assert_eq!(
            terminal.take_responses(),
            b"\x1b]11;rgb:0000/0000/0000\x1b\\".to_vec(),
            "OSC 11 answers the default background with the ST terminator echoed"
        );
    }

    #[test]
    fn answers_osc4_palette_query() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b]4;1;?\x07");
        assert_eq!(
            terminal.take_responses(),
            b"\x1b]4;1;rgb:cdcd/0000/0000\x07".to_vec(),
            "OSC 4 answers palette entry 1 (ANSI red) with the BEL terminator echoed"
        );
    }

    #[test]
    fn osc10_query_reflects_override_then_reset() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b]10;#ff0000\x07");
        terminal.advance(b"\x1b]10;?\x07");
        assert_eq!(
            terminal.take_responses(),
            b"\x1b]10;rgb:ffff/0000/0000\x07".to_vec(),
            "the OSC 10 override wins over the theme foreground"
        );

        terminal.advance(b"\x1b]110\x07");
        terminal.advance(b"\x1b]10;?\x07");
        assert_eq!(
            terminal.take_responses(),
            b"\x1b]10;rgb:cccc/cccc/cccc\x07".to_vec(),
            "OSC 110 reset restores the theme foreground"
        );
    }

    /// The window clear reads `default_background`, so an OSC 11 override has
    /// to reach it or the gutter past the cell grid strands the old color.
    #[test]
    fn default_background_follows_the_osc11_override() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        let themed = Theme::default().background;

        assert_eq!(
            terminal.default_background(),
            themed,
            "with no override the theme's background stands"
        );

        terminal.advance(b"\x1b]11;#ff0000\x07");
        assert_eq!(
            terminal.default_background(),
            Rgb::new(0xff, 0, 0),
            "a program's OSC 11 wins over the theme"
        );

        terminal.advance(b"\x1b]111\x07");
        assert_eq!(
            terminal.default_background(),
            themed,
            "OSC 111 hands the theme's background back"
        );
    }

    #[test]
    fn default_cursor_follows_the_osc12_override() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        let themed = Theme::default().cursor;

        terminal.advance(b"\x1b]12;#00ff00\x07");
        assert_eq!(terminal.default_cursor(), Rgb::new(0, 0xff, 0));

        terminal.advance(b"\x1b]112\x07");
        assert_eq!(terminal.default_cursor(), themed);
    }

    #[test]
    fn answers_text_area_pixel_query() {
        let mut terminal = Terminal::new(24, 80, Theme::default());

        terminal.advance(b"\x1b[14t");
        assert!(
            terminal.take_responses().is_empty(),
            "no reply until the cell pixel size is known"
        );

        terminal.set_cell_pixels(8, 16);
        terminal.advance(b"\x1b[14t");
        assert_eq!(
            terminal.take_responses(),
            b"\x1b[4;384;640t".to_vec(),
            "CSI 14 t reports 24 lines * 16px by 80 cols * 8px"
        );
    }

    #[test]
    fn tracks_bracketed_paste_mode() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        assert!(!terminal.bracketed_paste(), "off by default");

        terminal.advance(b"\x1b[?2004h");
        assert!(terminal.bracketed_paste(), "DECSET 2004 enables it");

        terminal.advance(b"\x1b[?2004l");
        assert!(!terminal.bracketed_paste(), "DECRST 2004 disables it");
    }

    #[test]
    fn tracks_focus_reporting_mode() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        assert!(!terminal.report_focus_in_out(), "off by default");

        terminal.advance(b"\x1b[?1004h");
        assert!(terminal.report_focus_in_out(), "DECSET 1004 enables it");

        terminal.advance(b"\x1b[?1004l");
        assert!(!terminal.report_focus_in_out(), "DECRST 1004 disables it");
    }

    #[test]
    fn osc_notify_scan_takes_both_terminators() {
        let mut bel = EscScanner::default();
        assert_eq!(
            scan_osc(&mut bel, b"\x1b]9;hi\x07"),
            vec![(9, b"hi".to_vec())]
        );

        let mut st = EscScanner::default();
        assert_eq!(
            scan_osc(&mut st, b"\x1b]9;hi\x1b\\"),
            vec![(9, b"hi".to_vec())]
        );
    }

    #[test]
    fn osc_notify_scan_retains_a_split_sequence() {
        let mut scanner = EscScanner::default();
        assert!(scan_osc(&mut scanner, b"\x1b]9;he").is_empty());
        assert_eq!(
            scan_osc(&mut scanner, b"llo\x07"),
            vec![(9, b"hello".to_vec())]
        );
    }

    #[test]
    fn osc_notify_scan_skips_other_codes_unbuffered() {
        let mut scanner = EscScanner::default();
        assert_eq!(
            scan_osc(&mut scanner, b"\x1b]52;c;QUJD\x07\x1b]9;ping\x07"),
            vec![(9, b"ping".to_vec())],
            "an OSC 52 write is skipped and the following OSC 9 is still found"
        );
    }

    #[test]
    fn osc_notify_scan_discards_over_the_cap() {
        let mut scanner = EscScanner::default();
        let mut seq = b"\x1b]9;".to_vec();
        seq.resize(seq.len() + MAX_OSC_NOTIFY_BYTES + 1, b'a');
        seq.push(0x07);
        assert!(
            scan_osc(&mut scanner, &seq).is_empty(),
            "a payload past the cap is dropped"
        );
        assert_eq!(
            scan_osc(&mut scanner, b"\x1b]9;ok\x07"),
            vec![(9, b"ok".to_vec())],
            "the scanner recovers for the next sequence"
        );
    }

    #[test]
    fn osc_notify_scan_skips_long_plain_runs() {
        let mut scanner = EscScanner::default();
        let mut input = vec![b'.'; 8192];
        input.extend_from_slice(b"\x1b]9;ping\x07");

        assert_eq!(scan_osc(&mut scanner, &input), vec![(9, b"ping".to_vec())]);
    }

    #[test]
    fn osc_notify_scan_output_is_split_invariant() {
        let input = b"log\x1b]9;alpha\x07gap\x1b]52;c;QQ==\x07\x1b]777;notify;t;b\x1b\\end";
        let whole = scan_osc(&mut EscScanner::default(), input);

        for split in 0..=input.len() {
            let mut scanner = EscScanner::default();
            let mut got = scan_osc(&mut scanner, &input[..split]);
            got.extend(scan_osc(&mut scanner, &input[split..]));
            assert_eq!(got, whole, "split at {split} changed the notifications");
        }
    }

    #[test]
    fn xtversion_scan_skips_long_plain_runs() {
        let mut scanner = EscScanner::default();
        let mut input = vec![b'.'; 8192];
        input.extend_from_slice(b"\x1b[>q");

        assert_eq!(scan_xtversion(&mut scanner, &input), 1);
    }

    #[test]
    fn xtversion_scan_output_is_split_invariant() {
        let input = b"a\x1b[>4;1mb\x1b[>qc\x1b[>0q";
        let whole = scan_xtversion(&mut EscScanner::default(), input);
        assert_eq!(whole, 2, "two queries; the modify-keys CSI is not one");

        for split in 0..=input.len() {
            let mut scanner = EscScanner::default();
            let got = scan_xtversion(&mut scanner, &input[..split])
                + scan_xtversion(&mut scanner, &input[split..]);
            assert_eq!(got, whole, "split at {split} changed the hit count");
        }
    }

    #[test]
    fn surfaces_osc9_notification() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b]9;build done\x07");
        assert_eq!(
            terminal.take_events(),
            vec![TermEvent::Notification {
                title: None,
                body: "build done".into()
            }]
        );
    }

    #[test]
    fn surfaces_osc777_notification_keeping_semicolons_in_body() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b]777;notify;Done;a;b;c\x07");
        assert_eq!(
            terminal.take_events(),
            vec![TermEvent::Notification {
                title: Some("Done".into()),
                body: "a;b;c".into()
            }],
            "OSC 777 splits kind/title/body but keeps later semicolons in the body"
        );
    }

    #[test]
    fn ignores_non_notify_osc777() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b]777;precmd;ignored\x07");
        assert!(
            terminal.take_events().is_empty(),
            "only the notify kind of OSC 777 delivers"
        );
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

    /// A line of output damages the whole terminal, but the screen only moved
    /// up, so the projection has to come back naming the rows that actually
    /// differ rather than all of them.
    #[test]
    fn a_scroll_reports_only_the_rows_that_changed() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        let mut grid = Grid::new(4, 8);

        terminal.advance(b"aaa\r\nbbb\r\nccc\r\nddd");
        terminal.project(&mut grid);

        // One more line scrolls the screen up by one. Rows zero to two now
        // hold what rows one to three held, and only the bottom row is new.
        terminal.advance(b"\r\neee");
        let (_, scrolled, damage) = terminal.project(&mut grid);

        assert_eq!(scrolled, 1, "the screen moved up by one row");
        let Damage::Partial(rows) = damage else {
            panic!("a scroll must not report the whole screen damaged");
        };
        assert_eq!(
            rows,
            vec![false, false, false, true],
            "only the row the scroll exposed is rewritten",
        );

        let row_text = |row: usize| grid.row(row).iter().map(|cell| cell.ch).collect::<String>();
        assert_eq!(
            row_text(0),
            "bbb     ",
            "the slide moved row one to row zero"
        );
        assert_eq!(row_text(3), "eee     ", "the new row holds the new output");
    }

    /// While scrolled back the viewport is pinned to its content as history
    /// grows, so there is no slide to make and the projection stays as it was.
    #[test]
    fn a_scroll_while_scrolled_back_still_reports_full_damage() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        terminal.advance(b"a\r\nb\r\nc\r\nd");
        terminal.project(&mut grid);
        terminal.scroll_display(1);
        terminal.project(&mut grid);

        terminal.advance(b"\r\ne");
        let (_, scrolled, damage) = terminal.project(&mut grid);

        assert_eq!(scrolled, 0, "a pinned viewport reports no scroll");
        assert!(
            matches!(damage, Damage::Full),
            "and takes the unchanged full-projection path",
        );
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
        assert!(!terminal.mouse_drag(), "click reporting is not drag");
        assert!(!terminal.mouse_motion(), "click reporting is not motion");

        // Button-event tracking (1002) reports motion during a drag. Any-motion
        // tracking (1003) reports motion with no button held.
        terminal.advance(b"\x1b[?1002h");
        assert!(terminal.mouse_drag(), "1002 enables drag reporting");
        assert!(!terminal.mouse_motion(), "1002 is not buttonless motion");

        terminal.advance(b"\x1b[?1003h");
        assert!(terminal.mouse_motion(), "1003 enables motion reporting");
        assert!(terminal.mouse_drag(), "1003 also satisfies drag reporting");
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

    /// One walk finds all three sequence kinds, whatever order they arrive in.
    #[test]
    fn one_scan_finds_every_kind_interleaved() {
        let mut scanner = EscScanner::default();
        let input =
            b"a\x1b_Gstoatty;x\x1b\\b\x1b[>0q\x1b]9;ping\x07c\x1b_second\x07\x1b[>q\x1b]777;notify;t;b\x1b\\";

        let mut apc = Vec::new();
        let mut queries = 0;
        let mut notes = Vec::new();
        scanner.scan(input, &mut |event| match event {
            EscEvent::Apc { payload, .. } => apc.push(payload.to_vec()),
            EscEvent::XtVersion => queries += 1,
            EscEvent::OscNotify { code, payload } => notes.push((code, payload.to_vec())),
        });

        assert_eq!(
            (apc, queries, notes),
            (
                vec![b"Gstoatty;x".to_vec(), b"second".to_vec()],
                2,
                vec![(9, b"ping".to_vec()), (777, b"notify;t;b".to_vec()),],
            ),
            "each sequence is recognized once, in one pass over the chunk",
        );
    }

    /// A sequence written inside another's payload belongs to that payload.
    ///
    /// One state machine reads the stream the way the vte parser does, so an APC
    /// payload holding what looks like a query is payload, not a query. Only
    /// malformed input can reach this, since a well-formed stream closes a sequence
    /// before opening the next.
    #[test]
    fn a_sequence_inside_a_payload_is_not_its_own() {
        let mut scanner = EscScanner::default();

        // The interior ESC abandons the APC frame, and the query bytes after it are
        // the abandoned payload's rather than a query of their own.
        assert_eq!(
            scan_xtversion(&mut scanner, b"\x1b_pay\x1b[>qload\x1b\\"),
            0,
            "a query inside an APC payload is part of the payload"
        );

        // An OSC 52 write is skipped to its terminator, so an APC frame drawn inside
        // it is skipped with it.
        let mut scanner = EscScanner::default();
        assert!(
            scan_collect(&mut scanner, b"\x1b]52;c;\x1b_Gstoatty;x\x1b\\\x07").is_empty(),
            "an APC frame inside an OSC payload is part of that payload"
        );
    }

    /// The APC payloads and their end offsets one scan of `bytes` completes.
    fn scan_collect(scanner: &mut EscScanner, bytes: &[u8]) -> Vec<(Vec<u8>, usize)> {
        let mut out = Vec::new();
        scanner.scan(bytes, &mut |event| {
            if let EscEvent::Apc { payload, end } = event {
                out.push((payload.to_vec(), end));
            }
        });
        out
    }

    /// How many XTVERSION queries one scan of `bytes` completes.
    fn scan_xtversion(scanner: &mut EscScanner, bytes: &[u8]) -> usize {
        let mut hits = 0;
        scanner.scan(bytes, &mut |event| {
            if matches!(event, EscEvent::XtVersion) {
                hits += 1;
            }
        });
        hits
    }

    /// The OSC notifications one scan of `bytes` completes, as `(code, payload)`.
    fn scan_osc(scanner: &mut EscScanner, bytes: &[u8]) -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        scanner.scan(bytes, &mut |event| {
            if let EscEvent::OscNotify { code, payload } = event {
                out.push((code, payload.to_vec()));
            }
        });
        out
    }

    #[test]
    fn scans_single_apc_frame() {
        let mut scanner = EscScanner::default();

        assert_eq!(
            scan_collect(&mut scanner, b"\x1b_Gstoatty;border\x1b\\"),
            vec![(b"Gstoatty;border".to_vec(), 19)]
        );
    }

    #[test]
    fn scans_frame_split_across_calls() {
        let mut scanner = EscScanner::default();

        assert!(scan_collect(&mut scanner, b"\x1b_Gstoat").is_empty());
        assert_eq!(
            scan_collect(&mut scanner, b"ty;x\x1b\\"),
            vec![(b"Gstoatty;x".to_vec(), 6)]
        );
    }

    #[test]
    fn scans_bel_terminated_frame() {
        let mut scanner = EscScanner::default();

        assert_eq!(
            scan_collect(&mut scanner, b"\x1b_foo\x07"),
            vec![(b"foo".to_vec(), 6)]
        );
    }

    #[test]
    fn scans_frame_between_text() {
        let mut scanner = EscScanner::default();

        assert_eq!(
            scan_collect(&mut scanner, b"a\x1b_foo\x1b\\b"),
            vec![(b"foo".to_vec(), 8)]
        );
    }

    #[test]
    fn scans_two_frames_in_one_chunk() {
        let mut scanner = EscScanner::default();

        assert_eq!(
            scan_collect(&mut scanner, b"\x1b_a\x1b\\\x1b_b\x1b\\"),
            vec![(b"a".to_vec(), 5), (b"b".to_vec(), 10)]
        );
    }

    #[test]
    fn csi_and_plain_text_yield_no_frames() {
        let mut scanner = EscScanner::default();

        assert!(scan_collect(&mut scanner, b"hello\x1b[31mworld").is_empty());
    }

    #[test]
    fn apc_scan_skips_long_plain_runs() {
        let mut scanner = EscScanner::default();
        let mut input = vec![b'.'; 8192];
        input.extend_from_slice(b"\x1b_frame\x07");
        let offset = input.len();

        assert_eq!(
            scan_collect(&mut scanner, &input),
            vec![(b"frame".to_vec(), offset)]
        );
    }

    #[test]
    fn apc_scan_payloads_are_split_invariant() {
        let input = b"pre\x1b_alpha\x1b\\mid\x1b_beta\x07post";
        let whole: Vec<Vec<u8>> = scan_collect(&mut EscScanner::default(), input)
            .into_iter()
            .map(|(payload, _)| payload)
            .collect();

        for split in 0..=input.len() {
            let mut scanner = EscScanner::default();
            let mut got: Vec<Vec<u8>> = scan_collect(&mut scanner, &input[..split])
                .into_iter()
                .map(|(payload, _)| payload)
                .collect();
            got.extend(
                scan_collect(&mut scanner, &input[split..])
                    .into_iter()
                    .map(|(p, _)| p),
            );
            assert_eq!(got, whole, "split at {split} changed the payloads");
        }
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
    fn panel_apc_frame_sets_a_grid_panel() {
        let frame = encode_panel(&PanelCommand {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            style: ProtoBorderStyle::Rounded,
            border: [40, 50, 60],
            corner_radius: 6,
            fill: Some([10, 20, 30]),
            shadow: ProtoPanelShadow::Drop,
            inset_x: 0,
            above_pools: false,
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.panels(),
            [Panel {
                top: 1,
                left: 2,
                width: 4,
                height: 3,
                style: BorderStyle::Rounded,
                border: Rgb::new(40, 50, 60),
                corner_radius: 6,
                fill: Some(Rgb::new(10, 20, 30)),
                shadow: PanelShadow::Drop,
                inset_x: 0,
                above_pools: false,
                seq: 1,
            }]
        );
    }

    #[test]
    fn panel_apc_frame_carries_the_above_pools_flag() {
        let frame = encode_panel(&PanelCommand {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            style: ProtoBorderStyle::Rounded,
            border: [40, 50, 60],
            corner_radius: 6,
            fill: Some([10, 20, 30]),
            shadow: ProtoPanelShadow::Drop,
            inset_x: 0,
            above_pools: true,
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.panels(),
            [Panel {
                top: 1,
                left: 2,
                width: 4,
                height: 3,
                style: BorderStyle::Rounded,
                border: Rgb::new(40, 50, 60),
                corner_radius: 6,
                fill: Some(Rgb::new(10, 20, 30)),
                shadow: PanelShadow::Drop,
                inset_x: 0,
                above_pools: true,
                seq: 1,
            }]
        );
    }

    #[test]
    fn reset_clears_accumulated_panels() {
        let panel = encode_panel(&PanelCommand {
            top: 0,
            left: 0,
            width: 3,
            height: 2,
            style: ProtoBorderStyle::Light,
            border: [1, 2, 3],
            corner_radius: 0,
            fill: None,
            shadow: ProtoPanelShadow::None_,
            inset_x: 0,
            above_pools: false,
        });

        let mut terminal = Terminal::new(4, 4, Theme::default());
        let mut grid = Grid::new(4, 4);
        terminal.advance(&panel);
        terminal.advance(&encode_reset());
        terminal.project(&mut grid);

        assert!(grid.panels().is_empty());
    }

    #[test]
    fn decoration_seq_stamps_declaration_order_and_resets() {
        let mut stream = encode_panel(&PanelCommand {
            top: 0,
            left: 0,
            width: 3,
            height: 2,
            style: ProtoBorderStyle::Light,
            border: [1, 2, 3],
            corner_radius: 0,
            fill: None,
            shadow: ProtoPanelShadow::None_,
            inset_x: 0,
            above_pools: false,
        });
        stream.extend_from_slice(&encode_icon(&IconCommand {
            top: 0,
            left: 0,
            kind: ProtoIconKind::Error,
            color: [1, 2, 3],
            size: 1,
            offset: [0, 0],
        }));
        stream.extend_from_slice(&encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: Some([0, 0, 0]),
            text: "x".to_owned(),
        }));
        stream.extend_from_slice(&encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 1,
            height: 16,
            color: [1, 2, 3],
        }));

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&stream);
        terminal.project(&mut grid);

        assert_eq!(
            (
                grid.panels()[0].seq,
                grid.icons()[0].seq,
                grid.text_runs()[0].seq,
                grid.bars()[0].seq,
            ),
            (1, 2, 3, 4),
            "one counter stamps declaration order across all four decoration lists"
        );

        terminal.advance(&encode_reset());
        terminal.advance(&stream);
        terminal.project(&mut grid);

        assert_eq!(grid.panels()[0].seq, 1, "reset renumbers the seq from 1");
    }

    #[test]
    fn panel_change_damages_its_rows() {
        let panel = encode_panel(&PanelCommand {
            top: 1,
            left: 0,
            width: 3,
            height: 2,
            style: ProtoBorderStyle::Light,
            border: [1, 2, 3],
            corner_radius: 0,
            fill: None,
            shadow: ProtoPanelShadow::None_,
            inset_x: 0,
            above_pools: false,
        });

        let mut terminal = Terminal::new(4, 3, Theme::default());
        let mut grid = Grid::new(4, 3);
        terminal.advance(&panel);
        terminal.project(&mut grid);

        let damage = terminal.take_decoration_damage();
        assert!(!damage.is_dirty(0), "row above the panel stays clean");
        assert!(damage.is_dirty(1), "panel top row damaged");
        assert!(damage.is_dirty(2), "panel bottom row damaged");
        assert!(!damage.is_dirty(3), "row below the panel stays clean");
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
            bold: true,
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
                bold: true,
                content: "ok".to_owned(),
            }]
        );
    }

    #[test]
    fn popover_content_streams_across_advance_chunks() {
        let frame = encode_popover(&PopoverCommand {
            top: 1,
            left: 2,
            width: 6,
            height: 3,
            fill: [10, 20, 30],
            border: [40, 50, 60],
            content_fg: [70, 80, 90],
            scale: 1,
            offset: [0, 0],
            bold: false,
            content: "streamed".to_owned(),
        });

        // Split just inside the content run (past the open marker's ESC \
        // terminator) so the content arrives across two advance calls, exercising
        // the cross-chunk popover capture.
        let open_end = frame.windows(2).position(|w| w == [0x1b, b'\\']).unwrap() + 2;
        let split = open_end + 3;

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&frame[..split]);
        terminal.advance(&frame[split..]);
        terminal.project(&mut grid);

        assert_eq!(
            grid.overlays(),
            [Overlay {
                top: 1,
                left: 2,
                width: 6,
                height: 3,
                fill: Rgb::new(10, 20, 30),
                border: Rgb::new(40, 50, 60),
                content_fg: Rgb::new(70, 80, 90),
                scale: 1,
                offset: [0, 0],
                bold: false,
                content: "streamed".to_owned(),
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
            offset: [3, 6],
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
                offset: [3, 6],
                seq: 1,
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
            bg: Some([24, 26, 32]),
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
                bg: Some(Rgb::new(24, 26, 32)),
                text: "42".into(),
                seq: 1,
            }]
        );
    }

    /// The capture buffer is handed back and reused, so a run must not read
    /// what an earlier one left in it, and a shorter run must not trail the
    /// longer one before it.
    #[test]
    fn runs_captured_through_one_buffer_keep_their_own_text() {
        let run = |text: &str| {
            encode_text_run(&TextRunCommand {
                col: 0,
                row: 0,
                scale: 256,
                color: [1, 2, 3],
                bg: None,
                text: text.to_owned(),
            })
        };

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&run("a long first run"));
        terminal.advance(&run("short"));
        terminal.advance(&run(""));
        terminal.project(&mut grid);

        let texts: Vec<&str> = grid
            .text_runs()
            .iter()
            .map(|run| run.text.as_ref())
            .collect();
        assert_eq!(texts, ["a long first run", "short", ""]);
    }

    /// Valid bytes take a path that skips the lossy rebuild, so the malformed
    /// case has to keep working through the fallback.
    #[test]
    fn a_run_of_invalid_bytes_still_captures_lossily() {
        let mut frame = encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 256,
            color: [1, 2, 3],
            bg: None,
            text: "ok".to_owned(),
        });
        // Replace the payload's second byte with a lone continuation byte,
        // which no valid sequence can start with.
        let at = frame
            .windows(2)
            .position(|pair| pair == b"ok")
            .expect("the payload is in the frame");
        frame[at + 1] = 0x80;

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.text_runs()[0].text.as_ref(),
            "o\u{fffd}",
            "the invalid byte becomes a replacement character",
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
                seq: 1,
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
            bg: Some([0, 0, 0]),
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

    /// A border over rows the projection left alone keeps its edges.
    ///
    /// Those rows were not reprojected, so nothing reset them and the stamp still
    /// stands. Skipping them is what stops a keystroke re-stamping every region on
    /// the grid, and the window must come out as a full re-stamp would leave it.
    #[test]
    fn a_border_clear_of_the_damaged_rows_keeps_its_edges() {
        let frame = encode_border(&BorderCommand {
            top: 2,
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

        let mut terminal = Terminal::new(4, 3, Theme::default());
        let mut grid = Grid::new(4, 3);
        terminal.advance(&frame);
        terminal.project(&mut grid);
        assert_eq!(
            (grid.get(2, 0).borders.top, grid.get(3, 0).borders.bottom),
            (edge, edge),
            "the declared border stamps on arrival",
        );

        // Text on row 0 damages that row alone, well clear of the border's rows.
        terminal.advance(b"X");
        terminal.project(&mut grid);

        assert_eq!(
            (
                grid.get(0, 0).ch,
                grid.get(2, 0).borders.top,
                grid.get(3, 0).borders.bottom,
                grid.get(2, 2).borders.right,
            ),
            ('X', edge, edge, edge),
            "the border survives a projection that never touched its rows",
        );
    }

    #[test]
    fn text_runs_reresolve_when_only_the_line_layout_changes() {
        let run = encode_text_run(&TextRunCommand {
            col: 0,
            row: 48,
            scale: 256,
            color: [150, 160, 170],
            bg: Some([0, 0, 0]),
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
            offset: [0, 0],
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

    /// A `Gstoatty;text_run` frame carrying `text`, so a test can stamp a
    /// distinguishable run and read it back off the projected grid.
    fn text_run_frame(text: &str) -> Vec<u8> {
        encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 160,
            color: [200, 200, 200],
            bg: Some([0, 0, 0]),
            text: text.to_owned(),
        })
    }

    /// The text of each run on the projected grid, in order.
    fn run_labels(grid: &Grid) -> Vec<&str> {
        grid.text_runs().iter().map(|run| &*run.text).collect()
    }

    /// Commit a one-run scene "A", then open a synchronized update and stage a
    /// reset plus a replacement run "B" without closing it, asserting the
    /// on-screen scene still reads "A" mid-update. The caller then commits it.
    fn stage_reset_and_run_under_sync(terminal: &mut Terminal, grid: &mut Grid) {
        terminal.advance(&text_run_frame("A"));
        terminal.project(grid);
        assert_eq!(run_labels(grid), ["A"], "the initial scene commits");

        terminal.advance(b"\x1b[?2026h");
        terminal.advance(&encode_reset());
        terminal.advance(&text_run_frame("B"));
        terminal.project(grid);
        assert_eq!(run_labels(grid), ["A"], "the prior scene holds mid-update");
    }

    #[test]
    fn synchronized_update_commits_staged_decorations_on_esu() {
        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        stage_reset_and_run_under_sync(&mut terminal, &mut grid);

        terminal.advance(b"\x1b[?2026l");
        terminal.project(&mut grid);
        assert_eq!(
            run_labels(&grid),
            ["B"],
            "ESU commits the staged reset and run atomically"
        );
    }

    #[test]
    fn synchronized_update_commits_staged_decorations_on_flush() {
        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        stage_reset_and_run_under_sync(&mut terminal, &mut grid);

        terminal.flush_synchronized_update();
        terminal.project(&mut grid);
        assert_eq!(
            run_labels(&grid),
            ["B"],
            "the timeout flush commits the staged scene"
        );
    }

    #[test]
    fn a_committed_update_keeps_its_staging_capacity() {
        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        stage_reset_and_run_under_sync(&mut terminal, &mut grid);

        terminal.flush_synchronized_update();

        assert!(
            terminal.sync_staged.capacity() > 0,
            "the committed list keeps its allocation for the next update"
        );
    }

    #[test]
    fn advance_defers_redraw_for_decorations_inside_an_update() {
        let mut terminal = Terminal::new(8, 8, Theme::default());

        assert!(
            terminal.advance(b"\x1b[?2026h"),
            "the BSU chunk wakes the main loop"
        );
        assert!(
            !terminal.advance(&text_run_frame("B")),
            "a decoration chunk wholly inside the update warrants no redraw"
        );
        assert!(
            terminal.advance(b"\x1b[?2026l"),
            "the ESU chunk warrants a redraw"
        );
    }

    /// Declare pool `id` over a `rows` by `cols` region at the origin, so the
    /// pool tests have a pool to fill, scroll, and project before driving it.
    fn declare_pool(terminal: &mut Terminal, id: u32, rows: u16, cols: u16) {
        terminal.advance(&encode_pool_region(&PoolRegionCommand {
            pool: id,
            top: 0,
            left: 0,
            width: cols,
            height: rows,
            window: 0,
        }));
    }

    /// The buffered page `index` of pool `id`, panicking if it is not present.
    fn pool_page(terminal: &Terminal, id: u32, index: u64) -> &Grid {
        terminal.pools[&id]
            .page_pool
            .page(index)
            .expect("page buffered")
    }

    fn declare_window_pool(terminal: &mut Terminal, id: u32, window: u32) {
        terminal.advance(&encode_pool_region(&PoolRegionCommand {
            pool: id,
            top: 0,
            left: 0,
            width: 4,
            height: 2,
            window,
        }));
    }

    #[test]
    fn window_bound_pool_is_excluded_from_the_primary_composite() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        declare_pool(&mut terminal, 1, 2, 4);
        declare_window_pool(&mut terminal, 2, 3);

        let primary: Vec<u32> = terminal.pools().iter().map(|view| view.id).collect();
        assert_eq!(
            primary,
            vec![1],
            "the aux pool stays out of the primary list"
        );

        let aux: Vec<u32> = terminal
            .window_pools(3)
            .iter()
            .map(|view| view.id)
            .collect();
        assert_eq!(aux, vec![2], "the aux pool appears under its window");
        assert!(
            terminal.window_pools(9).is_empty(),
            "no pools for an unused window"
        );
    }

    #[test]
    fn a_fill_on_a_window_pool_marks_the_window_dirty_once() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        declare_window_pool(&mut terminal, 2, 3);
        terminal.take_events();

        let mut stream = encode_fill(&FillCommand { pool: 2, index: 0 });
        stream.extend_from_slice(b"hi");
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        assert_eq!(terminal.take_events(), vec![TermEvent::WindowDirty(3)]);
    }

    #[test]
    fn window_open_command_surfaces_as_a_term_event() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        let open = WindowOpenCommand {
            window: 2,
            cols: 80,
            rows: 24,
            title: "editor".to_string(),
        };
        terminal.advance(&encode_window_open(&open));

        assert_eq!(terminal.take_events(), vec![TermEvent::WindowOpen(open)]);
    }

    #[test]
    fn config_reload_command_surfaces_as_a_term_event() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(&encode_config_reload());

        assert_eq!(terminal.take_events(), vec![TermEvent::ConfigReload]);
    }

    #[test]
    fn config_reload_survives_an_active_fill() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        terminal.advance(&encode_fill(&FillCommand { pool: 1, index: 0 }));

        terminal.advance(&encode_config_reload());

        assert_eq!(
            terminal.take_events(),
            vec![TermEvent::ConfigReload],
            "a routed command is not swallowed by a fill redirect"
        );
    }

    #[test]
    fn zoom_capture_command_surfaces_as_a_term_event() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(&encode_zoom_capture(true));
        terminal.advance(&encode_zoom_capture(false));

        assert_eq!(
            terminal.take_events(),
            vec![TermEvent::ZoomCapture(true), TermEvent::ZoomCapture(false)],
            "a claim and its release both reach the host, in order"
        );
    }

    #[test]
    fn font_step_command_surfaces_as_a_term_event() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(&encode_font_step(-2));

        assert_eq!(terminal.take_events(), vec![TermEvent::FontStep(-2)]);
    }

    /// A zoom press acts on the window, not on the frame being composed, so
    /// holding it behind a redirect would drop what the user just pressed.
    #[test]
    fn zoom_commands_survive_an_active_fill() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        terminal.advance(&encode_fill(&FillCommand { pool: 1, index: 0 }));

        terminal.advance(&encode_zoom_capture(true));
        terminal.advance(&encode_font_step(1));

        assert_eq!(
            terminal.take_events(),
            vec![TermEvent::ZoomCapture(true), TermEvent::FontStep(1)],
            "both route past a fill redirect"
        );
    }

    #[test]
    fn a_scroll_on_a_primary_pool_marks_nothing_dirty() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        declare_pool(&mut terminal, 1, 2, 4);
        terminal.take_events();

        terminal.advance(&encode_scroll(&ScrollCommand {
            pool: 1,
            page: 3,
            fraction: 0,
        }));

        assert!(
            terminal.take_events().is_empty(),
            "a primary pool raises no window-dirty event"
        );
    }

    #[test]
    fn fill_paints_page_and_spares_live_grid() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        declare_pool(&mut terminal, 0, 2, 4);
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(b"hi");
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        let page = pool_page(&terminal, 0, 0);
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

        declare_pool(&mut terminal, 0, 2, 4);
        terminal.advance(&encode_fill(&FillCommand { pool: 0, index: 3 }));
        terminal.advance(b"ab");
        terminal.advance(&encode_fill_end());

        let page = pool_page(&terminal, 0, 3);
        assert_eq!((page.get(0, 0).ch, page.get(0, 1).ch), ('a', 'b'));
    }

    #[test]
    fn next_fill_marker_auto_commits_the_previous_page() {
        let mut terminal = Terminal::new(2, 4, Theme::default());

        declare_pool(&mut terminal, 0, 2, 4);
        // No fill_end between the two pages: opening the second must commit the
        // first, so a dropped close cannot strand the redirect.
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(b"AA");
        stream.extend_from_slice(&encode_fill(&FillCommand { pool: 0, index: 1 }));
        stream.extend_from_slice(b"BB");
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        let page0 = pool_page(&terminal, 0, 0);
        assert_eq!(page0.get(0, 0).ch, 'A');
        let page1 = pool_page(&terminal, 0, 1);
        assert_eq!(page1.get(0, 0).ch, 'B');
    }

    #[test]
    fn reset_commits_an_in_progress_page() {
        let mut terminal = Terminal::new(2, 4, Theme::default());

        declare_pool(&mut terminal, 0, 2, 4);
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 2 });
        stream.extend_from_slice(b"zz");
        stream.extend_from_slice(&encode_reset());
        terminal.advance(&stream);

        let page = pool_page(&terminal, 0, 2);
        assert_eq!((page.get(0, 0).ch, page.get(0, 1).ch), ('z', 'z'));
    }

    #[test]
    fn fill_decoration_does_not_leak_to_the_live_grid() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        declare_pool(&mut terminal, 0, 2, 4);
        // A decoration command inside a page's stream is page-targeted; it must
        // not stamp the live grid.
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
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

    #[test]
    fn fill_captures_a_polyline_onto_the_slot() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        declare_pool(&mut terminal, 0, 2, 4);
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(&encode_polyline(&PolylineCommand {
            points: vec![[8, 0], [8, 16], [24, 32]],
            width: 6,
            color: [220, 50, 47],
        }));
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        let (_, _, polylines) = terminal.pools[&0]
            .page_pool
            .page_decorations(0)
            .expect("page buffered");
        assert_eq!(
            polylines,
            [Polyline {
                points: vec![[8, 0], [8, 16], [24, 32]],
                width: 6,
                color: Rgb::new(220, 50, 47),
                seq: 0,
            }]
        );

        terminal.project(&mut grid);
        assert!(
            grid.polylines().is_empty(),
            "a page-targeted path spares the live grid"
        );
    }

    #[test]
    fn a_polyline_outside_a_fill_lands_on_the_live_grid() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        terminal.advance(&encode_polyline(&PolylineCommand {
            points: vec![[0, 0], [16, 16]],
            width: 4,
            color: [1, 2, 3],
        }));
        terminal.project(&mut grid);

        assert_eq!(
            grid.polylines(),
            [Polyline {
                points: vec![[0, 0], [16, 16]],
                width: 4,
                color: Rgb::new(1, 2, 3),
                seq: 1,
            }]
        );
    }

    /// Reprojecting refills each path's point list where it sits, so the points must
    /// come out as declared rather than appended to what the last projection left.
    ///
    /// A path list that shrinks must lose its tail too, or a cleared lane would keep
    /// drawing.
    #[test]
    fn reprojected_paths_hold_only_what_is_declared() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        let path = |x: i16| {
            encode_polyline(&PolylineCommand {
                points: vec![[x, 0], [x + 16, 16]],
                width: 4,
                color: [1, 2, 3],
            })
        };
        terminal.advance(&path(0));
        terminal.advance(&path(32));
        terminal.project(&mut grid);

        // A re-sent line layout re-applies the paths without redeclaring them, which
        // is what an editor does on every wrap change.
        terminal.advance(&encode_line_layout(&LineLayoutCommand {
            heights: vec![1, 1],
        }));
        terminal.project(&mut grid);

        assert_eq!(
            grid.polylines()
                .iter()
                .map(|line| line.points.as_slice())
                .collect::<Vec<_>>(),
            [[[0, 0], [16, 16]], [[32, 0], [48, 16]]],
            "a second projection refills the points rather than growing them",
        );

        // A reset clears the declared paths, so the projection has none to place.
        terminal.advance(&encode_reset());
        terminal.project(&mut grid);

        assert!(
            grid.polylines().is_empty(),
            "a path no longer declared leaves no entry behind, found {}",
            grid.polylines().len(),
        );
    }

    #[test]
    fn fill_captures_bar_and_text_run_onto_the_slot() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        declare_pool(&mut terminal, 0, 2, 4);
        // A bar and a text run streamed inside the page ride onto its slot, not
        // the live grid. The text run's content proves the capture wins over the
        // fill parser.
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(&encode_bar(&BarCommand {
            x: 0,
            y: 16,
            width: 3,
            height: 16,
            color: [220, 50, 47],
        }));
        stream.extend_from_slice(&encode_text_run(&TextRunCommand {
            col: 0,
            row: 16,
            scale: 160,
            color: [150, 160, 170],
            bg: Some([24, 26, 32]),
            text: "42".to_owned(),
        }));
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        let (runs, bars, _) = terminal.pools[&0]
            .page_pool
            .page_decorations(0)
            .expect("page buffered");
        assert_eq!(
            runs,
            [TextRun {
                col: 0,
                row: 16,
                scale: 160,
                color: Rgb::new(150, 160, 170),
                bg: Some(Rgb::new(24, 26, 32)),
                text: "42".into(),
                seq: 0,
            }]
        );
        assert_eq!(
            bars,
            [Bar {
                x: 0,
                y: 16,
                width: 3,
                height: 16,
                color: Rgb::new(220, 50, 47),
                seq: 0,
            }]
        );

        terminal.project(&mut grid);
        assert!(
            grid.text_runs().is_empty() && grid.bars().is_empty(),
            "page decorations spare the live grid"
        );
    }

    #[test]
    fn project_pool_stamps_translated_decorations_and_culls_off_window() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut out = Grid::new(3, 4);

        declare_pool(&mut terminal, 0, 2, 4);
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(&encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: Some([0, 0, 0]),
            text: "aa".to_owned(),
        }));
        stream.extend_from_slice(&encode_fill(&FillCommand { pool: 0, index: 1 }));
        stream.extend_from_slice(&encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 160,
            color: [4, 5, 6],
            bg: Some([0, 0, 0]),
            text: "bb".to_owned(),
        }));
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        // Half a page down puts the window over page 0's last row and all of
        // page 1, so page 1's run (page row 0 to window row 1, y 16) is stamped
        // and page 0's (page row 0, above the window) is culled.
        let projected = terminal.project_pool(0, &mut out, 0.5);
        assert_eq!(projected, Some((0.0, 1)));
        assert_eq!(
            out.text_runs(),
            [TextRun {
                col: 0,
                row: 16,
                scale: 160,
                color: Rgb::new(4, 5, 6),
                bg: Some(Rgb::new(0, 0, 0)),
                text: "bb".into(),
                seq: 0,
            }]
        );
    }

    /// A gliding pool stamps into the same grid every frame, and the lists it
    /// writes are now the ones already there rather than fresh ones. A stamp
    /// therefore has to replace what it finds, including a path list left
    /// longer by the stamp before it.
    #[test]
    fn restamping_a_pool_replaces_what_the_last_stamp_left() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut out = Grid::new(3, 4);
        declare_pool(&mut terminal, 0, 2, 4);

        let path = |points: Vec<[i16; 2]>, width: u16| {
            encode_polyline(&PolylineCommand {
                points,
                width,
                color: [1, 2, 3],
            })
        };
        // Page one is filled once and left alone, so the window always holds
        // two paths and only page zero's changes shape between stamps.
        let fill_page_zero = |terminal: &mut Terminal, points: Vec<[i16; 2]>| {
            let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
            stream.extend_from_slice(&path(points, 8));
            stream.extend_from_slice(&encode_fill_end());
            terminal.advance(&stream);
        };

        let mut stream = encode_fill(&FillCommand { pool: 0, index: 1 });
        stream.extend_from_slice(&path(vec![[48, 0]], 4));
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        fill_page_zero(&mut terminal, vec![[0, 0], [16, 0], [32, 0]]);
        terminal.project_pool(0, &mut out, 0.0);
        let long = out.polylines().to_vec();
        assert_eq!(
            long.iter().map(|p| p.points.len()).collect::<Vec<_>>(),
            [3, 1],
            "page zero's three points, then page one's single one",
        );

        // The shorter path refills the entry the longer one left, so a stale
        // tail point would ride along into it.
        fill_page_zero(&mut terminal, vec![[64, 0]]);
        terminal.project_pool(0, &mut out, 0.0);
        assert_eq!(
            out.polylines()[0].points,
            [[64, 0]],
            "the shortened path holds only its own point",
        );

        // Back to the longer path, which must land what it did the first time.
        fill_page_zero(&mut terminal, vec![[0, 0], [16, 0], [32, 0]]);
        terminal.project_pool(0, &mut out, 0.0);
        assert_eq!(
            out.polylines(),
            long,
            "a restamp is unaffected by the reuse"
        );
    }

    #[test]
    fn refilling_a_slot_clears_its_decorations() {
        let mut terminal = Terminal::new(2, 4, Theme::default());

        declare_pool(&mut terminal, 0, 2, 4);
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(&encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 2,
            height: 16,
            color: [1, 2, 3],
        }));
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        // Refilling the same slot with a decoration-free page drops the old bar.
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(b"x");
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        let (runs, bars, _) = terminal.pools[&0]
            .page_pool
            .page_decorations(0)
            .expect("page buffered");
        assert!(
            runs.is_empty() && bars.is_empty(),
            "recycled slot drops the previous page's decorations"
        );
    }

    /// A committed page's VT context is reset and reused for the next page.
    ///
    /// The reset has to clear everything a page leaves behind. That means its
    /// cells, where it parked the cursor, the colors it was still painting in,
    /// and the decorations it captured.
    #[test]
    fn a_page_paints_into_a_reused_context_without_the_previous_page() {
        let mut terminal = Terminal::new(2, 4, Theme::default());

        declare_pool(&mut terminal, 0, 2, 4);

        // The first page fills both rows, leaves the cursor on the second one,
        // captures a bar, and closes mid-SGR with a red foreground still set.
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(b"\x1b[31mAAAA\r\nBBBB");
        stream.extend_from_slice(&encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 2,
            height: 16,
            color: [1, 2, 3],
        }));
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);
        assert_eq!(pool_page(&terminal, 0, 0).get(1, 0).ch, 'B', "first page");

        // The second page writes one character into a different slot. Anything
        // the reset missed shows up here.
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 1 });
        stream.extend_from_slice(b"z");
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        let page = pool_page(&terminal, 0, 1);
        let row = |r: usize| (0..4).map(|c| page.get(r, c).ch).collect::<String>();
        assert_eq!(
            (row(0), row(1)),
            ("z   ".to_string(), "    ".to_string()),
            "the character lands at the origin on an otherwise blank screen",
        );
        assert_ne!(
            page.get(0, 0).fg,
            pool_page(&terminal, 0, 0).get(0, 0).fg,
            "and in the default foreground, not the color the last page left set",
        );

        let (runs, bars, polylines) = terminal.pools[&0]
            .page_pool
            .page_decorations(1)
            .expect("page buffered");
        assert!(
            runs.is_empty() && bars.is_empty() && polylines.is_empty(),
            "and carries none of the previous page's decorations",
        );
    }

    /// A parked context is sized to the pool it last painted, so it is only good
    /// for a page of the same shape. A larger region gets a fresh one, or the page
    /// would paint into a screen too small to hold it.
    #[test]
    fn a_pool_of_another_size_does_not_paint_through_the_parked_context() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        declare_pool(&mut terminal, 0, 2, 4);
        declare_pool(&mut terminal, 1, 4, 8);

        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(b"AAAA");
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        // The last row and column of the wider region lie outside the narrow
        // screen the first page parked.
        let mut stream = encode_fill(&FillCommand { pool: 1, index: 0 });
        stream.extend_from_slice(b"\x1b[4;8Hz");
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        assert_eq!(
            pool_page(&terminal, 1, 0).get(3, 7).ch,
            'z',
            "the wider page paints its far corner",
        );
    }

    /// Nothing reads what a page's content replies, so a parked context has to be
    /// drained anyway. One sink now serves every page, and an undrained one would
    /// grow for the life of the terminal.
    #[test]
    fn a_parked_context_holds_no_replies_from_the_page_it_painted() {
        let mut terminal = Terminal::new(2, 4, Theme::default());

        declare_pool(&mut terminal, 0, 2, 4);

        // DA1 makes the screen reply toward the PTY, and an OSC title queues a
        // listener event.
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(b"\x1b[c\x1b]0;page\x07x");
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        let parked = terminal.fill_scratch.as_ref().expect("context parked");
        let (bytes, events) = (parked.responses.take(), parked.responses.take_events());
        assert!(
            bytes.is_empty() && events.is_empty(),
            "parked context holds {} reply bytes and {} events",
            bytes.len(),
            events.len(),
        );
    }

    /// A page that opens a synchronized update and never closes it leaves the
    /// parser buffering. Carried into the next page, that would swallow its
    /// content instead of painting it.
    #[test]
    fn a_page_left_mid_synchronized_update_does_not_hold_the_next_one_back() {
        let mut terminal = Terminal::new(2, 4, Theme::default());

        declare_pool(&mut terminal, 0, 2, 4);

        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(b"\x1b[?2026hA");
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        let mut stream = encode_fill(&FillCommand { pool: 0, index: 1 });
        stream.extend_from_slice(b"z");
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        assert_eq!(
            pool_page(&terminal, 0, 1).get(0, 0).ch,
            'z',
            "the second page paints through a parser the first one left syncing",
        );
    }

    #[test]
    fn scroll_command_sets_the_target() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        declare_pool(&mut terminal, 0, 4, 8);
        terminal.advance(&encode_scroll(&ScrollCommand {
            pool: 0,
            page: 9,
            fraction: 16_384,
        }));

        assert_eq!(
            terminal.pools().first().map(|pool| pool.scroll_target),
            Some(DocumentOffset {
                page: 9,
                fraction: 0.25,
            }),
        );
    }

    #[test]
    fn pools_into_clears_prior_contents() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        declare_pool(&mut terminal, 0, 4, 8);

        let mut buf = Vec::new();
        terminal.pools_into(&mut buf);
        assert_eq!(buf.len(), 1, "one declared pool yields one view");

        // A reused buffer is cleared before refilling, so it never accumulates.
        terminal.pools_into(&mut buf);
        assert_eq!(
            buf.len(),
            1,
            "reuse clears the prior view rather than appending"
        );
    }

    #[test]
    fn pool_cursor_exposes_the_anchor_through_pools_into() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        declare_pool(&mut terminal, 0, 4, 8);
        terminal.advance(&encode_pool_cursor(&PoolCursorCommand {
            pool: 0,
            row: 42,
            col: 7,
        }));

        assert_eq!(
            terminal.pools().first().map(|pool| pool.cursor_anchor),
            Some(Some((42, 7))),
        );
    }

    #[test]
    fn pool_drop_clears_the_cursor_anchor() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        declare_pool(&mut terminal, 0, 4, 8);
        terminal.advance(&encode_pool_cursor(&PoolCursorCommand {
            pool: 0,
            row: 42,
            col: 7,
        }));
        terminal.advance(&encode_pool_drop(&PoolDropCommand { pool: 0 }));

        declare_pool(&mut terminal, 0, 4, 8);
        assert_eq!(
            terminal.pools().first().map(|pool| pool.cursor_anchor),
            Some(None),
            "a re-declared pool does not inherit the dropped pool's anchor",
        );
    }

    #[test]
    fn pool_cursor_for_an_undeclared_pool_is_ignored() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        terminal.advance(&encode_pool_cursor(&PoolCursorCommand {
            pool: 9,
            row: 1,
            col: 2,
        }));

        assert!(
            terminal.pools().is_empty(),
            "a stray anchor creates no pool"
        );
    }

    #[test]
    fn reposition_sets_the_target_and_a_one_shot_jump() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        declare_pool(&mut terminal, 0, 4, 8);
        terminal.advance(&encode_reposition(&RepositionCommand {
            pool: 0,
            page: 1_000,
        }));

        assert_eq!(
            terminal.pools().first().map(|pool| pool.scroll_target),
            Some(DocumentOffset {
                page: 1_000,
                fraction: 0.0,
            }),
        );
        assert_eq!(terminal.take_reposition(0), Some(1_000));
        assert_eq!(
            terminal.take_reposition(0),
            None,
            "the jump is consumed once"
        );
    }

    #[test]
    fn project_pool_composes_from_the_pool_with_the_sub_cell_fraction() {
        let mut terminal = Terminal::new(2, 4, Theme::default());

        declare_pool(&mut terminal, 0, 2, 4);
        // Buffer pages 0 and 1 so a 3-row compose at the top has every row.
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(&encode_fill(&FillCommand { pool: 0, index: 1 }));
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        let mut out = Grid::new(0, 0);
        // 0.25 pages over a 2-row page is half a row: top row 0, half-cell shift.
        let composed = terminal.project_pool(0, &mut out, 0.25);

        assert_eq!(composed, Some((0.5, 0)));
        assert_eq!(
            (out.rows(), out.cols()),
            (3, 4),
            "region height plus a straddle row"
        );
    }

    #[test]
    fn project_pool_composes_into_the_declared_pool_region() {
        let mut terminal = Terminal::new(2, 4, Theme::default());

        // A 3x2 pool region narrower than the 4-col viewport; buffer pages 0 and
        // 1 so a 3-row compose at the top has every row.
        let mut stream = encode_pool_region(&PoolRegionCommand {
            pool: 0,
            top: 0,
            left: 0,
            width: 3,
            height: 2,
            window: 0,
        });
        stream.extend_from_slice(&encode_fill(&FillCommand { pool: 0, index: 0 }));
        stream.extend_from_slice(&encode_fill(&FillCommand { pool: 0, index: 1 }));
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);

        let mut out = Grid::new(0, 0);
        let composed = terminal.project_pool(0, &mut out, 0.25);

        assert_eq!(composed, Some((0.5, 0)));
        assert_eq!(
            (out.rows(), out.cols()),
            (3, 3),
            "region height plus a straddle row, by region width"
        );
    }

    #[test]
    fn project_pool_degrades_when_no_window_is_buffered() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        declare_pool(&mut terminal, 0, 2, 4);

        // Pre-size out to the projection shape (region height + 1 straddle row,
        // by width) and seed sentinels. A degraded projection must leave the
        // caller's held composite intact rather than resizing or half-writing
        // it, so the compositor can keep showing the last good frame.
        let mut out = Grid::new(3, 4);
        for row in 0..out.rows() {
            for col in 0..out.cols() {
                out.get_mut(row, col).ch = 'Z';
            }
        }
        assert_eq!(terminal.project_pool(0, &mut out, 0.0), None);
        let untouched = (0..out.rows()).all(|r| (0..out.cols()).all(|c| out.get(r, c).ch == 'Z'));
        assert!(untouched, "a degraded projection leaves out untouched");
    }

    /// Stepping one row deeper into history moves the whole window down by a
    /// row, so only the older line revealed at the top is actually new.
    #[test]
    fn a_scrollback_step_reports_only_the_row_it_revealed() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        // a, b, c, d scroll into history; e, f stay on the live screen.
        terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");

        let mut out = Grid::new(0, 0);
        let mut damage = Damage::Full;

        terminal.project_scrollback(&mut out, 1.0, 0, &mut damage);
        assert_eq!(
            [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
            ['c', 'd', 'e'],
        );

        // One row deeper. The window slides down, so c and d keep their content
        // at rows one and two, and only the newly revealed b is written.
        terminal.project_scrollback(&mut out, 2.0, -1, &mut damage);

        let Damage::Partial(rows) = &damage else {
            panic!("a one-row step must not report the whole window damaged");
        };
        assert_eq!(
            rows,
            &vec![true, false, false],
            "only the row the step revealed at the top is rewritten",
        );
        assert_eq!(
            [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
            ['b', 'c', 'd'],
            "and the window shows the older line above what it already held",
        );
    }

    /// Live output arriving while the window holds still leaves every row clean.
    ///
    /// The view is pinned to its content as history grows, so the caller raises the
    /// offset by the appended rows and reports no movement. Those frames are the
    /// common case while a child streams and the user reads back, and reprojecting
    /// the window on each would re-upload it at output rate.
    #[test]
    fn output_under_a_still_window_dirties_nothing() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        // a, b, c, d scroll into history; e, f stay on the live screen.
        terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");

        let mut out = Grid::new(0, 0);
        let mut damage = Damage::Full;

        terminal.project_scrollback(&mut out, 1.0, 0, &mut damage);
        let window = |out: &Grid| [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch];
        assert_eq!(window(&out), ['c', 'd', 'e']);

        // One more line of output pushes e into history. The offset rises with it,
        // which is the pin keeping the window on the rows it already showed.
        terminal.advance(b"\r\ng");
        terminal.project_scrollback(&mut out, 2.0, 0, &mut damage);

        let Damage::Partial(rows) = &damage else {
            panic!("a still window must not report itself wholly damaged");
        };
        assert_eq!(
            (rows, window(&out)),
            (&vec![false, false, false], ['c', 'd', 'e']),
            "the window holds the same rows, so none is dirty",
        );
    }

    /// A move at or past the window height leaves nothing worth comparing against,
    /// so the window is reprojected whole and reported wholly damaged.
    #[test]
    fn a_move_clearing_the_window_reprojects_it_whole() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");

        let mut out = Grid::new(0, 0);
        let mut damage = Damage::Partial(vec![false; 3]);

        // Three rows of window, so a move of three has nothing left to keep.
        terminal.project_scrollback(&mut out, 1.0, 3, &mut damage);

        assert!(
            matches!(damage, Damage::Full),
            "a cleared window cannot report rows it kept"
        );
        assert_eq!(
            [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
            ['c', 'd', 'e'],
            "and it still holds the rows the offset names",
        );
    }

    #[test]
    fn project_scrollback_composes_a_straddled_history_window() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        // a, b, c scroll into history; d, e stay on the live screen.
        terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne");

        let mut out = Grid::new(0, 0);

        // At the live bottom nothing is scrolled back: fall back to the live grid.
        assert_eq!(
            terminal.project_scrollback(&mut out, 0.0, 0, &mut Damage::Full),
            None
        );

        // One row back: the window is the older straddle row (b) above the
        // offset-1 view (c, d), shifted up a whole row so the straddle hides.
        assert_eq!(
            terminal.project_scrollback(&mut out, 1.0, 0, &mut Damage::Full),
            Some(-1.0)
        );
        assert_eq!(
            (out.rows(), out.cols()),
            (3, 4),
            "viewport plus a straddle row"
        );
        assert_eq!(
            [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
            ['b', 'c', 'd'],
        );

        // Half a row deeper keeps the same window, shifted by the sub-cell frac.
        assert_eq!(
            terminal.project_scrollback(&mut out, 1.5, 0, &mut Damage::Full),
            Some(-0.5)
        );
        assert_eq!(
            [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
            ['b', 'c', 'd'],
        );

        // At the oldest line the straddle falls above history and stays blank.
        assert_eq!(
            terminal.project_scrollback(&mut out, 3.0, 0, &mut Damage::Full),
            Some(-1.0)
        );
        assert_eq!(*out.get(0, 0), Cell::default(), "no row older than the top");
        assert_eq!([out.get(1, 0).ch, out.get(2, 0).ch], ['a', 'b']);
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

    #[test]
    fn selection_text_returns_the_dragged_range() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        terminal.advance(b"hello");
        terminal.start_selection(0, 0, false);
        terminal.update_selection(0, 4, true);
        assert_eq!(terminal.selection_text().as_deref(), Some("hello"));
    }

    #[test]
    fn project_inverts_selected_cells() {
        let mut terminal = Terminal::new(4, 8, Theme::default());
        let mut grid = Grid::new(4, 8);
        terminal.advance(b"hello");
        terminal.start_selection(0, 0, false);
        terminal.update_selection(0, 2, true);
        terminal.project(&mut grid);

        for col in 0..=2 {
            assert!(
                grid.get(0, col).flags.contains(Flags::INVERSE),
                "selected col {col} is inverted"
            );
        }
        assert!(
            !grid.get(0, 3).flags.contains(Flags::INVERSE),
            "col past the selection is not inverted"
        );
    }

    #[test]
    fn selection_growth_damages_the_entered_rows() {
        let mut terminal = Terminal::new(6, 8, Theme::default());
        let mut grid = Grid::new(6, 8);
        terminal.advance(b"a\r\nb\r\nc");
        terminal.start_selection(0, 0, false);
        terminal.update_selection(0, 0, true);
        terminal.project(&mut grid);

        // Grow the selection to row 1. That row is neither the anchor nor the
        // cursor row, so it repaints only because the selection reached it.
        terminal.update_selection(1, 0, true);
        let (_, _, damage) = terminal.project(&mut grid);
        assert!(damage.is_dirty(1), "row 1 newly entered the selection");
        assert!(
            !damage.is_dirty(5),
            "a row outside the selection stays clean"
        );
    }

    fn minimap_cmd(strip_id: u32, content_id: u32) -> MinimapCommand {
        MinimapCommand {
            top: 0,
            left: 72,
            width: 8,
            height: 40,
            strip_id,
            content_id,
            lines_per_cell: 8,
            max_columns: 120,
            bg: [0, 0, 0, 0],
            thumb: [200, 200, 200, 48],
            thumb_border: [255, 255, 255],
            palette: vec![[0, 0, 0], [1, 2, 3]],
        }
    }

    fn run(start_col: u8, len: u8, class: u8) -> MinimapRun {
        MinimapRun {
            start_col,
            len,
            class,
        }
    }

    fn splice(content_id: u32, start: u32, removed: u32, lines: Vec<Vec<MinimapRun>>) -> Vec<u8> {
        encode_minimap_lines(&MinimapLinesCommand {
            content_id,
            start,
            removed,
            lines,
        })
    }

    #[test]
    fn minimap_lines_splices_at_boundaries() {
        let mut terminal = Terminal::new(4, 4, Theme::default());

        terminal.advance(&splice(
            9,
            0,
            0,
            vec![vec![run(0, 2, 1)], vec![run(0, 3, 2)], vec![run(1, 1, 3)]],
        ));
        assert_eq!(
            terminal.minimap_contents[&9].len(),
            3,
            "three lines inserted"
        );

        terminal.advance(&splice(9, 1, 1, vec![vec![run(2, 2, 4)]]));
        assert_eq!(
            terminal.minimap_contents[&9],
            vec![vec![run(0, 2, 1)], vec![run(2, 2, 4)], vec![run(1, 1, 3)]],
            "the middle line is replaced in place",
        );

        terminal.advance(&splice(9, 2, 1, vec![]));
        assert_eq!(
            terminal.minimap_contents[&9].len(),
            2,
            "a pure deletion removes the last line",
        );

        terminal.advance(&splice(9, 99, 5, vec![vec![run(0, 1, 0)]]));
        assert_eq!(
            terminal.minimap_contents[&9],
            vec![vec![run(0, 2, 1)], vec![run(2, 2, 4)], vec![run(0, 1, 0)]],
            "an out-of-range start clamps and appends",
        );
    }

    #[test]
    fn reset_clears_minimap_strips_but_keeps_content_and_view() {
        let mut terminal = Terminal::new(4, 4, Theme::default());
        terminal.advance(&encode_minimap(&minimap_cmd(5, 9)));
        terminal.advance(&splice(9, 0, 0, vec![vec![run(0, 2, 1)]]));
        terminal.advance(&encode_minimap_view(&MinimapViewCommand {
            strip_id: 5,
            top_256: 256,
            visible_lines: 20,
        }));
        assert_eq!(terminal.minimaps.len(), 1, "the strip is declared");

        terminal.advance(&encode_reset());

        assert!(
            terminal.minimaps.is_empty(),
            "reset clears the strip declaration",
        );
        assert_eq!(
            terminal.minimap_contents[&9].len(),
            1,
            "the content store survives reset",
        );
        assert!(
            terminal.minimap_views.contains_key(&5),
            "the view survives reset",
        );
    }

    #[test]
    fn minimap_drop_removes_content_store() {
        let mut terminal = Terminal::new(4, 4, Theme::default());
        terminal.advance(&splice(9, 0, 0, vec![vec![run(0, 2, 1)]]));
        assert!(terminal.minimap_contents.contains_key(&9));

        terminal.advance(&encode_minimap_drop(&MinimapDropCommand { content_id: 9 }));

        assert!(
            !terminal.minimap_contents.contains_key(&9),
            "drop retires the content store",
        );
    }

    #[test]
    fn minimap_view_advance_reprojects_the_thumb() {
        let mut terminal = Terminal::new(4, 4, Theme::default());
        terminal.advance(&encode_minimap(&minimap_cmd(5, 9)));
        terminal.advance(&encode_minimap_view(&MinimapViewCommand {
            strip_id: 5,
            top_256: 256,
            visible_lines: 20,
        }));

        let mut grid = Grid::new(4, 4);
        terminal.project(&mut grid);
        assert_eq!(
            grid.minimaps()[0].view.as_ref().map(|v| v.top_256),
            Some(256),
            "the first projection stamps the initial thumb position",
        );

        // A view-only advance re-projects the strip so the thumb tracks the
        // scroll instead of freezing at its first position until a settle.
        terminal.advance(&encode_minimap_view(&MinimapViewCommand {
            strip_id: 5,
            top_256: 768,
            visible_lines: 20,
        }));
        terminal.project(&mut grid);
        assert_eq!(
            grid.minimaps()[0].view.as_ref().map(|v| v.top_256),
            Some(768),
            "the advanced view re-stamps the thumb at its new position",
        );
    }

    #[test]
    fn minimap_projects_to_grid_joined_with_view_and_content() {
        let mut terminal = Terminal::new(4, 4, Theme::default());
        terminal.advance(&encode_minimap(&minimap_cmd(5, 9)));
        terminal.advance(&encode_minimap_view(&MinimapViewCommand {
            strip_id: 5,
            top_256: 512,
            visible_lines: 20,
        }));
        terminal.advance(&splice(
            9,
            0,
            0,
            vec![vec![run(0, 2, 1)], vec![run(1, 3, 2)]],
        ));

        let mut grid = Grid::new(4, 4);
        terminal.project(&mut grid);

        assert_eq!(grid.minimaps().len(), 1);
        let strip: &Minimap = &grid.minimaps()[0];
        assert_eq!(strip.command.strip_id, 5);
        assert_eq!(
            strip.view,
            Some(MinimapView {
                top_256: 512,
                visible: 20,
            }),
        );
        assert_eq!(
            grid.minimap_content(9),
            &[vec![run(0, 2, 1)], vec![run(1, 3, 2)]],
        );
    }

    #[test]
    fn projection_replays_splices_incrementally_into_the_grid() {
        let mut terminal = Terminal::new(4, 4, Theme::default());
        let mut grid = Grid::new(4, 4);

        terminal.advance(&splice(
            9,
            0,
            0,
            vec![vec![run(0, 2, 1)], vec![run(1, 3, 2)]],
        ));
        terminal.project(&mut grid);
        let epoch = grid.minimap_epoch();
        assert_eq!(
            grid.minimap_content(9),
            &terminal.minimap_contents[&9][..],
            "the first splice projects into the grid",
        );

        // A second splice replays against the grid store rather than re-cloning it.
        terminal.advance(&splice(9, 1, 1, vec![vec![run(2, 2, 4)]]));
        terminal.project(&mut grid);
        assert_eq!(
            grid.minimap_content(9),
            &terminal.minimap_contents[&9][..],
            "the replayed grid store equals the term store",
        );
        assert!(
            grid.minimap_epoch() > epoch,
            "a store change bumps the epoch"
        );
    }

    #[test]
    fn projection_replays_a_drop_to_an_empty_grid_store() {
        let mut terminal = Terminal::new(4, 4, Theme::default());
        let mut grid = Grid::new(4, 4);

        terminal.advance(&splice(9, 0, 0, vec![vec![run(0, 2, 1)]]));
        terminal.project(&mut grid);
        assert_eq!(grid.minimap_content(9).len(), 1);

        terminal.advance(&encode_minimap_drop(&MinimapDropCommand { content_id: 9 }));
        terminal.project(&mut grid);
        assert!(
            grid.minimap_content(9).is_empty(),
            "a replayed drop empties the grid store",
        );
    }

    #[test]
    fn a_resize_restores_the_grid_store_via_the_full_clone() {
        let mut terminal = Terminal::new(4, 4, Theme::default());
        let mut grid = Grid::new(4, 4);

        terminal.advance(&splice(
            9,
            0,
            0,
            vec![vec![run(0, 2, 1)], vec![run(1, 3, 2)]],
        ));
        terminal.project(&mut grid);

        // A resize empties the grid stores, so the next projection re-clones them.
        terminal.resize(8, 8);
        terminal.project(&mut grid);
        assert_eq!(
            grid.minimap_content(9),
            &terminal.minimap_contents[&9][..],
            "the resize fallback restores the grid store",
        );
    }

    #[test]
    fn a_projection_with_no_store_change_leaves_the_grid_untouched() {
        let mut terminal = Terminal::new(4, 4, Theme::default());
        let mut grid = Grid::new(4, 4);

        terminal.advance(&splice(9, 0, 0, vec![vec![run(0, 2, 1)]]));
        terminal.project(&mut grid);
        let epoch = grid.minimap_epoch();
        let store = grid.minimap_content(9).to_vec();

        // A viewport-only projection re-projects neither the store nor the epoch.
        terminal.project(&mut grid);
        assert_eq!(
            grid.minimap_content(9),
            &store[..],
            "the grid store is untouched"
        );
        assert_eq!(grid.minimap_epoch(), epoch, "the epoch is untouched");
    }
}
