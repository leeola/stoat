//! The winit application: owns the window, the PTY shell, and the event loop.
//!
//! The reader thread parses shell output into a [`Terminal`] it shares with the
//! main thread behind a [`FairMutex`], then wakes the loop with a [`PtyEvent`].
//! The main thread projects the parsed screen onto a [`Grid`] and drives
//! [`stoatty_render`] to draw it, so a flood of output never blocks input
//! handling on the main thread. This is the windowing boundary: the window lives
//! here and [`stoatty_render`] receives only its handle, keeping the renderer
//! toolkit-agnostic.

use crate::{
    config::{self, Config, CursorAnimation},
    pty::{self, Pty, PtyOutput},
    stoat_bin,
};
use alacritty_terminal::sync::FairMutex;
use rustc_hash::FxHasher;
#[cfg(unix)]
use std::process::Command;
use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    mem,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc,
    },
    time::{Duration, Instant},
};
#[cfg(unix)]
use std::{
    io::{self, Write},
    os::unix::net::UnixListener,
    sync::mpsc::{self, Receiver},
};
use stoat_cli::CommonArgs;
use stoatty_protocol::{
    command::{PoolRegionCommand, WindowOpenCommand, NON_PANE_POOL_BASE},
    window_ipc::{MouseButton as IpcMouseButton, MouseKind, WindowIpcEvent},
};
use stoatty_render::{
    gpu::{FontConfig, FontLoad, Frame, GpuContext, PoolComposite, Scroll},
    render,
};
use stoatty_term::{
    grid::{Bar, Grid, Overlay, TextRun},
    term::{Cursor, CursorShape, Damage, PoolView, TermEvent, Terminal},
    theme::Theme,
};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{UserAttentionType, Window, WindowId},
};

/// Window title shown before a program sets one, and restored when a program
/// resets it via OSC.
const DEFAULT_TITLE: &str = "stoatty";

/// Smallest font size the live zoom allows, so cells never collapse to an
/// unreadable size.
const FONT_SIZE_FLOOR: u32 = 6;

/// Lines of terminal-owned scrollback the wheel moves per line of wheel travel,
/// the idiomatic multi-line wheel step common terminals use (e.g. alacritty's
/// default scroll multiplier of 3). Applies only to local scrollback, never to
/// the wheel reports forwarded to a mouse-reporting app.
const SCROLLBACK_SCROLL_MULTIPLIER: i32 = 3;

/// Bytes of the child's most recent output retained for the exit diagnostic
/// logged when the pty closes, enough to carry a startup error line without
/// holding the whole session's scrollback.
const CHILD_OUTPUT_TAIL_CAP: usize = 2048;

/// Open the stoatty window running the launch command, or the resolved stoat
/// editor when none is given, at the winit default window size.
///
/// The launch program and arguments follow a precedence. `command` (the
/// `-e`/`--command` CLI override) wins first, then `--terminal` runs the login
/// shell, then the `[shell]` config, then the stoat editor resolved by
/// [`stoat_bin::resolve`], forwarding the shared `common` arguments (files,
/// `--continue`, `--resume`) to it. When the editor is the chosen default, its
/// directory is prepended to the child's `PATH` so nested bare-`stoat` calls
/// resolve to the same binary. The `common` arguments are ignored under `-e`,
/// `--terminal`, and a `[shell]` child, which take their own arguments.
///
/// The command runs in `working_directory` when it names an existing directory.
/// A non-directory is warned about and ignored, falling back to stoatty's own
/// working directory.
///
/// Blocks the calling thread for the lifetime of the window. See
/// [`run_with_shell`] to force a specific command instead.
pub fn run(
    command: Option<(String, Vec<String>)>,
    working_directory: Option<PathBuf>,
    common: CommonArgs,
    terminal: bool,
) {
    let start = Instant::now();
    let mut config = load_config();
    let (program, args, stoat_dir) = if let Some((program, args)) = command {
        (program, args, None)
    } else if terminal {
        (pty::default_shell(), Vec::new(), None)
    } else if let Some(shell) = config.shell.take() {
        (shell.program, shell.args, None)
    } else {
        let stoat = stoat_bin::resolve(&config);
        let dir = stoat
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf);
        (stoat.to_string_lossy().into_owned(), common.to_argv(), dir)
    };
    let working_directory = working_directory.and_then(|dir| {
        if dir.is_dir() {
            Some(dir)
        } else {
            eprintln!(
                "stoatty: ignoring --working-directory {}: not a directory",
                dir.display()
            );
            None
        }
    });
    run_with_config(
        start,
        config,
        program,
        args,
        None,
        working_directory,
        stoat_dir,
    );
}

/// Open the stoatty window running `program` with `args` as the PTY command,
/// and run the event loop until the window closes or that command exits.
///
/// The command is the one passed in, not the `[shell]` config override; the
/// config supplies only theme and font here. See [`run`] to launch the
/// configured command.
///
/// `size` is the window's content extent in cells (`[cols, rows]`); the window
/// opens sized to it, and `None` keeps the winit default window. Blocks the
/// calling thread for the lifetime of the window. The loop is idle-driven
/// (`ControlFlow::Wait`): frames are drawn on demand when PTY output arrives or
/// the window is resized, not on a continuous timer.
pub fn run_with_shell(program: String, args: Vec<String>, size: Option<[u16; 2]>) {
    let start = Instant::now();
    run_with_config(start, load_config(), program, args, size, None, None);
}

/// Open the window running `program` with `args`, drawing with `config`'s theme
/// and font, and run the event loop until the window closes.
///
/// The shared core of [`run`] and [`run_with_shell`]. It takes an
/// already-loaded `config` so each entry point loads it exactly once.
fn run_with_config(
    start: Instant,
    config: Config,
    program: String,
    args: Vec<String>,
    size: Option<[u16; 2]>,
    working_directory: Option<PathBuf>,
    stoat_dir: Option<PathBuf>,
) {
    let theme = config.resolve_theme();

    // Start the system-font scan before the event loop and window are built, so
    // its enumeration overlaps them rather than only the later GPU setup.
    let font_load = FontLoad::spawn();

    let event_loop = EventLoop::<PtyEvent>::with_user_event()
        .build()
        .expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    // portable_pty defaults an unset cwd to $HOME, so resolve stoatty's own
    // working directory here (the choke point for run() and run_with_shell())
    // to honor run()'s documented fallback and keep env IO out of the PTY layer.
    let working_directory = working_directory.or_else(|| std::env::current_dir().ok());

    let mut app = App::new(
        start,
        font_load,
        event_loop.create_proxy(),
        program,
        args,
        theme,
        config.theme,
        FontSettings {
            size: config.font_size,
            family: config.font_family,
            ligatures: config.ligatures,
        },
        config.cursor_animation,
        size,
        working_directory,
        stoat_dir,
    );
    event_loop.run_app(&mut app).expect("run event loop");
}

/// Load the settled config, falling back to the built-in default (with a
/// warning on stderr) when it cannot be read.
fn load_config() -> Config {
    config::load().unwrap_or_else(|error| {
        eprintln!("stoatty: could not load config, using built-in defaults: {error}");
        config::embedded_default()
    })
}

/// Shell activity delivered from the reader thread to the event loop.
///
/// The reader thread parses output into the shared [`Terminal`] itself, then
/// sends these through the [`EventLoopProxy`] to wake the idle main thread for
/// the follow-up it cannot do off-thread: writing query responses and redrawing.
enum PtyEvent {
    /// Host-query responses a parse produced, for the main thread to write back
    /// to the PTY. Sent only when a parse yields replies, so it never doubles as
    /// the redraw signal.
    Responses(Vec<u8>),
    /// The reader parsed output that changed the screen and asks for a redraw.
    /// Coalesced: the reader sends this on the clean-to-dirty edge of
    /// [`State::dirty`], so a burst of chunks collapses into one wakeup per
    /// render cycle rather than one per read chunk.
    Redraw,
    /// Host-facing notifications a parse produced, for the main thread to apply
    /// off the grid (window title, clipboard). Sent only when a parse yields
    /// events.
    Term(Vec<TermEvent>),
    /// The child closed the pty and the reader thread ended. `last_output` is
    /// the escape-stripped tail of what the child wrote, empty when it produced
    /// nothing, carried so the main thread can log it alongside the exit status.
    Exited { last_output: String },
    /// An aux window's [`GpuContext`] finished building on a background thread.
    /// The main thread installs it into the matching [`AuxWindow`], which then
    /// requests its first redraw. Boxed so the enum stays small.
    AuxGpuReady { window: u32, gpu: Box<GpuContext> },
}

/// The text-rendering configuration read from the config once, which [`App`]
/// seeds the renderer's [`FontConfig`] with when the window opens.
struct FontSettings {
    size: u32,
    family: Vec<String>,
    ligatures: bool,
}

struct App {
    /// Process-start instant captured at the entry point, used to log the total
    /// cold-start time when the first frame is presented.
    start: Instant,
    /// The system-font scan started at `run_with_config` entry, taken in
    /// `resumed` to hand to the renderer. `None` after the window is built.
    font_load: Option<FontLoad>,
    proxy: EventLoopProxy<PtyEvent>,
    program: String,
    args: Vec<String>,
    /// Working directory for the spawned command, or `None` to inherit
    /// stoatty's own. Already validated to an existing directory.
    working_directory: Option<PathBuf>,
    /// Directory prepended to the spawned child's `PATH` when stoatty launches
    /// the resolved stoat editor, so a nested bare-`stoat` call resolves to the
    /// same binary. `None` for a `-e`/shell child or a bare-name stoat.
    stoat_dir: Option<PathBuf>,
    theme: Theme,
    /// The configured name `theme` was resolved from, exported to the spawned
    /// child as `STOAT_THEME` so a child editor adopts the terminal's theme.
    /// Empty when the config names no theme.
    theme_name: String,
    font_size: u32,
    /// Ordered font-family cascade from the config, resolved against the font db
    /// at renderer creation to pick the shaping primary. Read once in `resumed`.
    font_family: Vec<String>,
    /// Whether the renderer shapes cell runs together so ligatures form. Read
    /// once in `resumed` into the renderer's [`FontConfig`].
    ligatures: bool,
    /// Selected cursor motion style, read once into [`State`] at window creation.
    cursor_animation: CursorAnimation,
    /// The window's content size in cells (`[cols, rows]`) to open sized to, or
    /// `None` for the winit default window. Read once at window creation.
    size: Option<[u16; 2]>,
    state: Option<State>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        start: Instant,
        font_load: FontLoad,
        proxy: EventLoopProxy<PtyEvent>,
        program: String,
        args: Vec<String>,
        theme: Theme,
        theme_name: String,
        font: FontSettings,
        cursor_animation: CursorAnimation,
        size: Option<[u16; 2]>,
        working_directory: Option<PathBuf>,
        stoat_dir: Option<PathBuf>,
    ) -> App {
        App {
            start,
            font_load: Some(font_load),
            proxy,
            program,
            args,
            working_directory,
            stoat_dir,
            theme,
            theme_name,
            font_size: font.size,
            font_family: font.family,
            ligatures: font.ligatures,
            cursor_animation,
            size,
            state: None,
        }
    }
}

/// A detached pane's aux OS window, a second render target for the window-bound
/// pools the primary composite omits.
///
/// The winit window is created on the main thread when a
/// [`TermEvent::WindowOpen`] arrives, while its [`GpuContext`] builds on a
/// background thread and installs via [`PtyEvent::AuxGpuReady`], so the primary
/// never stalls on aux GPU setup. Until it arrives `gpu` is `None` and redraw
/// requests find nothing to draw.
struct AuxWindow {
    id: u32,
    window: Arc<Window>,
    gpu: Option<GpuContext>,
    /// The window's composed content grid, rendered whole each redraw.
    grid: Grid,
    /// Scratch that [`Terminal::project_pool`] composes one pool into before its
    /// rows are blitted into `grid` at the pool's window-relative region.
    scratch: Grid,
    /// Whether this window holds OS focus, tracked from `WindowEvent::Focused`.
    /// Feeds the app-wide DECSET 1004 report so a switch between stoatty windows
    /// keeps the app focused.
    focused: bool,
    /// The last pointer cell, tracked from `CursorMoved` so a `MouseInput` press
    /// or release -- which carries no position -- reports at the right cell.
    pointer_cell: (u16, u16),
    /// The button held down, so a `CursorMoved` while pressed reports a drag.
    pressed: Option<MouseButton>,
    /// Sub-line wheel remainder, so pixel-precise trackpad deltas accumulate
    /// into whole-line scroll reports rather than rounding each event to zero.
    wheel_pixels: f64,
    /// Per-pool ease state for this window's pools, in ascending-id (z) order,
    /// so a scroll glides toward its target rather than jumping like the primary.
    pool_anims: BTreeMap<u32, PoolAnim>,
    /// Wall-clock of the previous redraw, driving this window's own ease step.
    /// Aux windows redraw independently, so each keeps its own frame clock.
    last_redraw: Option<Instant>,
    /// Hash of the inputs the last base compose read (each pool's content, target,
    /// and region, plus the window size). A redraw whose inputs hash the same
    /// skips the recompose and reuses last frame's instances. The sub-cell glide
    /// overlay still animates. `None` forces the first frame to compose.
    last_compose: Option<u64>,
    /// Reused buffer for the window's pool snapshots, so a redraw allocates no
    /// fresh Vec to gather them.
    pool_scratch: Vec<PoolView>,
}

/// The renderer-construction inputs an aux window needs, read from [`App`]
/// alongside the live [`State`] so opening one mirrors the primary's setup in
/// [`App::resumed`].
struct AuxWindowConfig<'a> {
    proxy: &'a EventLoopProxy<PtyEvent>,
    theme: Theme,
    font_family: &'a [String],
    ligatures: bool,
}

struct State {
    window: Arc<Window>,
    /// Process-start instant, taken once when the first frame is presented to
    /// log the total cold-start time. `None` after that first frame.
    first_frame_start: Option<Instant>,
    gpu: GpuContext,
    /// The parsed screen, shared with the reader thread that advances it. The
    /// [`FairMutex`] lets the main thread lock it to project while the reader
    /// locks it to parse, neither starving the other under heavy output.
    terminal: Arc<FairMutex<Terminal>>,
    /// Set by the reader when it parses output not yet redrawn, cleared when the
    /// main thread services a [`PtyEvent::Redraw`]. The reader sends a redraw
    /// wakeup only on the clean-to-dirty edge, so a flood of chunks coalesces
    /// into one wakeup per render cycle instead of one per chunk.
    dirty: Arc<AtomicBool>,
    /// Set by the reader while a DEC 2026 synchronized update is buffering in the
    /// parser, so [`App::about_to_wait`] arms a wait until the update's timeout
    /// and flushes it if no ESU arrives. Cleared once the update flushes.
    sync_pending: Arc<AtomicBool>,
    grid: Grid,
    pty: Pty,
    /// The live font size in logical points, seeded from the config and stepped
    /// by the platform zoom combo. Drives the renderer's cell metrics on each
    /// change, scaled by [`Self::scale_factor`].
    font_size: u32,
    /// The window's display scale factor (physical pixels per logical point),
    /// tracked from `ScaleFactorChanged` so the cell metrics re-derive when the
    /// window moves to a display of a different density.
    scale_factor: f64,
    /// The most recent modifier state, tracked from `ModifiersChanged` so a key
    /// press can tell whether the platform zoom modifier is held.
    modifiers: ModifiersState,
    /// Whether the primary window currently holds focus, tracked from
    /// `WindowEvent::Focused`. Combined with each aux window's focus into the
    /// app-wide DECSET 1004 report via [`reconcile_app_focus`].
    focused: bool,
    /// The last app-wide focus state reported to the child (true when the
    /// primary or any aux window is focused). A DECSET 1004 report fires only
    /// when this flips, so a switch between stoatty windows sends nothing while
    /// a click to a foreign app reports the app lost focus.
    app_focused: bool,
    /// Instant of the last bell that rang, so a burst of BELs from a catted
    /// binary makes one beep and attention request rather than a storm. `None`
    /// until the first bell.
    last_bell: Option<Instant>,
    /// The cursor's animated position in fractional cell coordinates, eased
    /// toward the terminal's actual cursor cell each frame. Drives the
    /// [`CursorAnimation::Block`] motion.
    cursor_anim: [f32; 2],
    /// Cursor motion style, copied from [`App`] at construction. Selects which
    /// animation [`Self::step_cursor`] advances each frame.
    cursor_animation: CursorAnimation,
    /// The warp cursor's four animated corners [TL, TR, BL, BR] in fractional
    /// cell coordinates, eased independently toward the target cell's block so
    /// the cursor stretches along its path. Drives the
    /// [`CursorAnimation::Warp`] motion.
    cursor_corner_anim: [[f32; 2]; 4],
    /// Whether last frame drew the cursor at a glide anchor rather than easing it.
    ///
    /// Set while a pool glides and the cursor rides its content. On the first
    /// frame after the glide releases it seeds the settle flight, so the cursor
    /// eases from its last anchored position to the landing cell instead of
    /// teleporting there.
    cursor_was_anchored: bool,
    /// Each overlay's eased vertical scroll offset, in rows, indexed by overlay
    /// order. An entry ping-pongs between the top and its overflow bottom while
    /// that popover overflows its box, so several scroll independently.
    popover_scrolls: Vec<f32>,
    /// Each overlay's ping-pong direction: true while easing down toward the
    /// overflow bottom, false while easing back up to the top.
    popover_scroll_downs: Vec<bool>,
    /// The grid's eased vertical scroll offset, in rows. Seeded by the term's
    /// per-frame scroll delta and eased toward zero so content glides into place.
    grid_scroll: f32,
    /// The live smooth-scroll position through the terminal's own scrollback, in
    /// rows back from the live bottom, eased toward [`Self::scrollback_target`].
    /// Like the document offset it tracks an absolute position rather than
    /// decaying to zero, so the history window scrolls through every row at
    /// fractional-pixel granularity and rests on a cell boundary.
    scrollback_visual: f32,
    /// The whole-cell scrollback position the wheel last moved to, in rows back
    /// from the live bottom, that [`Self::scrollback_visual`] eases toward. Kept
    /// in step with the terminal's `display_offset`: the wheel advances both, and
    /// a per-frame check folds in any auto-pin the terminal applied as live
    /// output grew, so output never drags the eased view.
    scrollback_target: f32,
    /// The straddled history window composed at [`Self::scrollback_visual`],
    /// reused across frames. Sized to the viewport plus one top straddle row;
    /// rendered instead of [`Self::grid`] whenever the view is scrolled back.
    scrollback_grid: Grid,
    /// The integer offset [`Self::scrollback_grid`] was last composed at, so a
    /// frame that only changes the sub-cell fraction reuses the cached rows and
    /// re-shifts them. The compose re-runs only when the integer offset changes
    /// or live output redamaged the grid. `None` when the previous frame
    /// rendered the live grid.
    last_scrollback_offset: Option<i32>,
    /// The scroll region's eased vertical offset, in rows. Seeded by the change
    /// in the region's declared offset and eased toward zero, so the region's
    /// content glides when the program scrolls it.
    region_scroll: f32,
    /// The scroll region's declared offset at the previous frame, so the next
    /// one can seed the ease with the change since.
    last_region_offset: f32,
    /// Per-pool smooth-scroll animation state, keyed by pool id.
    ///
    /// Each entry eases its own offset toward the terminal's app-declared target
    /// for that pool and holds the grids the composite reads, so several pools
    /// (split panes, a modal over an editor) glide independently and stack in
    /// ascending-id z-order. An entry is created when a pool first appears and
    /// dropped when the app retires it.
    pool_anims: BTreeMap<u32, PoolAnim>,
    /// Scratch buffers reused across redraws so frame assembly allocates no
    /// per-frame temporary. They hold the pool snapshot, the active-glide pools,
    /// and the per-overlay overflow amounts, each cleared and refilled per frame.
    pools_scratch: Vec<PoolView>,
    active_scratch: Vec<ActivePool>,
    overflows_scratch: Vec<Option<f32>>,
    /// Live aux OS windows, each hosting a detached pane's window-bound pools.
    /// Empty until a [`TermEvent::WindowOpen`] opens the first one.
    aux: Vec<AuxWindow>,
    /// Channel to the thread serving the window-event socket, carrying encoded
    /// [`WindowIpcEvent`] lines to forward to the connected child. `None` when
    /// the socket could not be bound, in which case aux windows still render but
    /// report nothing upstream.
    window_event_tx: Option<Sender<String>>,
    /// Unspent vertical wheel travel in physical pixels, accumulated from
    /// high-resolution `PixelDelta` events until it reaches a whole cell so a
    /// trackpad scrolls scrollback smoothly without losing sub-line motion.
    wheel_pixels: f64,
    /// The grid cell `(col, row)` under the pointer, tracked from `CursorMoved`,
    /// so a mouse-reporting app receives wheel reports at the pointer position.
    pointer_cell: (usize, usize),
    /// The SGR code (0 left, 1 middle, 2 right) of the button currently held,
    /// or `None` when none is, tracked from `MouseInput` so a drag-motion report
    /// can encode which button is being dragged.
    pressed_button: Option<u8>,
    /// Whether the pointer sits in the right half of its cell, tracked from
    /// `CursorMoved` so a native grid selection anchors on the correct edge.
    pointer_side_right: bool,
    /// True while a stoatty-native grid selection is being dragged, so
    /// `CursorMoved` extends it and the left release copies and clears it.
    native_drag: bool,
    /// The OS clipboard handle, opened lazily on the first copy and kept alive
    /// so X11 selection ownership persists between copies. `None` until the
    /// first copy, and reset to `None` after a failed copy so the next one
    /// reopens it.
    clipboard: Option<arboard::Clipboard>,
    /// When the previous `RedrawRequested` ran, so each frame's easing advances
    /// by the wall time actually elapsed rather than a fixed per-frame step.
    /// `None` until the first frame.
    last_redraw: Option<Instant>,
    /// Whether the perf HUD overlay is shown, toggled by the platform modifier
    /// plus Shift+P. Drives both the HUD composite and the redraw keep-alive.
    #[cfg(feature = "perf")]
    show_perf_hud: bool,
}

/// One pool's smooth-scroll animation state, held by [`State::pool_anims`].
struct PoolAnim {
    /// The live eased offset, in document pages, easing toward the pool's
    /// app-declared target. Tracks an absolute position rather than decaying.
    scroll: f32,
    /// The scroll target seen the previous frame, in document pages. A frame
    /// that changed it scrolled the document, so the change feeds the cursor
    /// sweep. Seeded to the creation target so a fresh pool does not sweep.
    last_scroll_target: f32,
    /// Wall time since [`Self::last_scroll_target`] last changed. The follower
    /// converges to a still-moving target every frame in the momentum tail, so
    /// convergence alone cannot separate an active glide from a settled pool.
    /// Once this reaches [`HANDOFF_STABLE_TIME`] the target has held steady
    /// and the region hands back to the live grid. A target still moving holds
    /// the pool composited. Seeded so a fresh at-rest pool hands off at once.
    target_stable_for: Duration,
    /// The region's pooled rows composed at [`Self::scroll`], sized to the
    /// region plus one straddle row. Reused across frames.
    document_grid: Grid,
    /// The viewport-sized grid the pool composites from: the region's pooled
    /// rows copied into the declared sub-rectangle, the rest blank since the
    /// scissor clips the composite to that rectangle over the live grid.
    pool_grid: Grid,
    /// The integer document top row [`Self::document_grid`] was last composed
    /// at. With [`Self::last_version`] and [`Self::last_region_dims`] unchanged,
    /// the composed rows are identical this frame and only the sub-cell fraction
    /// moved, so the recompose is skipped. `None` until the first composed frame.
    last_top: Option<i64>,
    /// The pool content-version the grids were last composed at, so a fill that
    /// committed since forces a recompose even when the top row held steady.
    last_version: Option<u64>,
    /// The region dimensions (width, height) last composed at, so a resize that
    /// reshapes the grids forces a recompose.
    last_region_dims: Option<(u16, u16)>,
    /// Whether the last composed window was buffered. A skip frame reuses this
    /// verdict rather than re-testing, so an unbuffered pool stays degraded to
    /// the live grid without recomposing.
    last_buffered: bool,
    /// The sub-cell fraction [`Self::document_grid`] still shows from its last
    /// successful compose. While a frame's window is unbuffered, the region
    /// holds this last good composite at this offset instead of snapping back
    /// to the live grid. `None` until the first successful compose, and cleared
    /// on a resize (which reshapes the grid) or a settled handoff (after which
    /// the live grid owns the region).
    held_frac: Option<f32>,
}

impl PoolAnim {
    /// A fresh pool resting at `scroll`, so a newly declared pool shows at its
    /// current position rather than gliding in from the document origin.
    fn new(scroll: f32) -> PoolAnim {
        PoolAnim {
            scroll,
            last_scroll_target: scroll,
            target_stable_for: HANDOFF_STABLE_TIME,
            document_grid: Grid::new(0, 0),
            pool_grid: Grid::new(0, 0),
            last_top: None,
            last_version: None,
            last_region_dims: None,
            last_buffered: false,
            held_frac: None,
        }
    }
}

/// A pool that is mid-glide and buffered this frame, so the renderer composites
/// it: which pool, its region, and the sub-cell fraction to shift its rows by.
struct ActivePool {
    id: u32,
    region: PoolRegionCommand,
    frac: f32,
    /// Whether the pool's composed rows changed since the previous frame. When
    /// `false` the glide only advanced the sub-cell fraction, so the copy into
    /// the pool grid and the composite's instance rebuild are both skipped and
    /// only the shift is re-applied.
    content_changed: bool,
}

/// The outcome of stepping one pool's glide for a frame.
enum PoolStep {
    /// The target held steady long enough and the ease has arrived, so the pool
    /// hands its region back to the base grid and stops ticking.
    Settled,
    /// Still gliding, but this frame's window is unbuffered with no held
    /// composite, so the base grid shows through. The loop keeps ticking.
    Degraded,
    /// Still gliding with a composite ready at [`ActivePool::frac`].
    Gliding(ActivePool),
}

/// Advance `anim`'s ease one frame toward `pool`'s scroll target and compose the
/// pool's rows at the eased offset into `anim.document_grid`.
///
/// Both render paths share this step-and-compose. The primary composites the
/// result over the live grid, an aux window over its target-projected base. The
/// caller passes any pending `reposition` (it consumes it from the terminal) and
/// handles the cursor and z-ordered blit around this.
///
/// A [`PoolStep::Settled`] result means the region hands off to the base, so the
/// caller drops the pool from its composite set. The recompose is skipped while
/// only the sub-cell fraction moves, so a settled or shift-only pool costs no
/// projection.
fn advance_pool_glide(
    anim: &mut PoolAnim,
    pool: &PoolView,
    terminal: &Terminal,
    reposition: Option<u64>,
    dt: Duration,
) -> PoolStep {
    let page_rows = (pool.region.height as f32).max(1.0);

    // A frame that moved the target scrolled the document, so reset the stable
    // timer. A frame that left it steady advances toward the handoff.
    let target_pages = pool.scroll_target.pages();
    let jump_rows = (target_pages - anim.last_scroll_target) * page_rows;
    anim.last_scroll_target = target_pages;
    if jump_rows == 0.0 {
        anim.target_stable_for = anim.target_stable_for.saturating_add(dt);
    } else {
        anim.target_stable_for = Duration::ZERO;
    }

    // A reposition jump re-anchors the offset to a local neighbour of the
    // destination, so the ease lands softly within the freshly-buffered window
    // instead of dragging across the unbuffered gap.
    if let Some(target) = reposition {
        anim.scroll = (target as f32 - REPOSITION_LAND_PAGES).max(0.0);
    }

    let (scroll, easing) = step_document_scroll(anim.scroll, target_pages, page_rows, dt);
    let scroll_settled = anim.target_stable_for >= HANDOFF_STABLE_TIME;
    anim.scroll = scroll;
    if !easing && scroll_settled {
        // The base owns the region once it hands off. Drop any held composite so
        // a later re-glide cannot resurrect content the base has since replaced.
        anim.held_frac = None;
        return PoolStep::Settled;
    }

    // The composed rows depend only on the integer top document row, the pooled
    // page bytes, and the region size. While all three hold, the glide advanced
    // only the sub-cell fraction, so the recompose here and the copy downstream
    // are skipped and last frame's grids and buffered verdict are reused.
    let doc_rows = scroll * page_rows;
    let top = doc_rows.floor() as i64;
    let frac = doc_rows - top as f32;
    let version = terminal.pool_content_version(pool.id);
    let region_dims = (pool.region.width, pool.region.height);
    let content_changed = anim.last_top != Some(top)
        || anim.last_version != version
        || anim.last_region_dims != Some(region_dims);

    // A resize reshapes document_grid, so a held composite from the old
    // dimensions no longer fits the region.
    if anim.last_region_dims != Some(region_dims) {
        anim.held_frac = None;
    }

    let buffered = if content_changed {
        let composed = terminal
            .project_pool(pool.id, &mut anim.document_grid, scroll)
            .is_some();
        anim.last_top = Some(top);
        anim.last_version = version;
        anim.last_region_dims = Some(region_dims);
        anim.last_buffered = composed;
        composed
    } else {
        anim.last_buffered
    };

    if buffered {
        anim.held_frac = Some(frac);
        PoolStep::Gliding(ActivePool {
            id: pool.id,
            region: pool.region,
            frac,
            content_changed,
        })
    } else if let Some(held) = anim.held_frac {
        // The window is not buffered this frame. Re-push the last good composite
        // at its held offset so the region holds it instead of snapping back to
        // the base grid.
        PoolStep::Gliding(ActivePool {
            id: pool.id,
            region: pool.region,
            frac: held,
            content_changed: false,
        })
    } else {
        PoolStep::Degraded
    }
}

impl ApplicationHandler<PtyEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }

        // Take the font enumeration started at `run_with_config` entry, which has
        // been running on a background thread through the event-loop and window
        // build. A second resume finds it already taken and starts a fresh scan
        // rather than panicking.
        let font_load = self.font_load.take().unwrap_or_else(FontLoad::spawn);

        let mut attributes = Window::default_attributes().with_title(DEFAULT_TITLE);
        if let Some([cols, rows]) = self.size {
            let [cell_width, cell_height] = render::cell_size(self.font_size, 1.0);
            attributes = attributes.with_inner_size(LogicalSize::new(
                cols as f32 * cell_width,
                rows as f32 * cell_height,
            ));
        }
        let t_window = Instant::now();
        let window = Arc::new(event_loop.create_window(attributes).expect("create window"));
        let window_time = t_window.elapsed();

        let size = window.inner_size();
        let scale_factor = window.scale_factor();

        let monitor = window.current_monitor();
        tracing::info!(
            monitor = ?monitor.as_ref().and_then(|m| m.name()),
            monitor_size = ?monitor.as_ref().map(|m| m.size()),
            refresh_mhz = ?monitor.as_ref().and_then(|m| m.refresh_rate_millihertz()),
            scale_factor,
            window_size = ?size,
            "display",
        );

        let gpu = GpuContext::new(
            window.clone(),
            size.width.max(1),
            size.height.max(1),
            font_load,
            FontConfig {
                size: self.font_size,
                scale_factor: scale_factor as f32,
                family: &self.font_family,
                ligatures: self.ligatures,
            },
            self.theme.background,
            self.theme.cursor,
        );

        let (rows, cols) = gpu.grid_size();
        let grid = Grid::new(rows, cols);
        let terminal = Arc::new(FairMutex::new(Terminal::new(rows, cols, self.theme)));
        update_cell_pixels(&terminal, self.font_size, scale_factor as f32);
        if let Some(ident) = stoat_log::ident::get() {
            terminal
                .lock()
                .set_ident(stoatty_protocol::command::IdentReply {
                    pid: std::process::id(),
                    log_id: ident.id.to_string(),
                    hostname: stoat_log::ident::hostname(),
                    version: crate::cli::VERSION_INFO.to_string(),
                });
        }
        let dirty = Arc::new(AtomicBool::new(false));
        let sync_pending = Arc::new(AtomicBool::new(false));

        // Bind the socket aux windows report focus, resize, and close over, and
        // export its path so the child editor can connect. A bind failure is
        // non-fatal. Aux windows still render, they just report nothing upstream.
        let (window_socket, window_event_tx) = open_window_event_socket();

        let t_pty = Instant::now();
        let pty = {
            let proxy = self.proxy.clone();
            let terminal = terminal.clone();
            let dirty = dirty.clone();
            let sync_pending = sync_pending.clone();
            let mut tail: Vec<u8> = Vec::new();
            Pty::spawn(
                &self.program,
                &self.args,
                self.working_directory.as_deref(),
                self.stoat_dir.as_deref(),
                stoat_log::ident::get().map(|i| i.id.as_str()),
                window_socket.as_deref().and_then(Path::to_str),
                &self.theme_name,
                rows as u16,
                cols as u16,
                move |output| match output {
                    PtyOutput::Data(bytes) => {
                        pty::push_tail(&mut tail, bytes, CHILD_OUTPUT_TAIL_CAP);
                        // Parse on the reader thread under the shared lock.
                        let (redraw, responses, events) = {
                            let mut terminal = terminal.lock();
                            let redraw = terminal.advance(bytes);
                            // A buffering synchronized update needs the main
                            // thread to arm and drive its timeout flush.
                            sync_pending
                                .store(terminal.sync_deadline().is_some(), Ordering::Relaxed);
                            (redraw, terminal.take_responses(), terminal.take_events())
                        };
                        if !responses.is_empty() {
                            let _ = proxy.send_event(PtyEvent::Responses(responses));
                        }
                        if !events.is_empty() {
                            let _ = proxy.send_event(PtyEvent::Term(events));
                        }
                        // Wake the main thread to redraw, but only on the
                        // clean-to-dirty edge so a burst of chunks collapses into
                        // one wakeup per render cycle. A chunk wholly held in the
                        // synchronized-update buffer changes nothing on screen, so
                        // it skips the wakeup.
                        if redraw && !dirty.swap(true, Ordering::Relaxed) {
                            let _ = proxy.send_event(PtyEvent::Redraw);
                        }
                    },
                    PtyOutput::Eof => {
                        tracing::info!("child closed the pty");
                        let last_output = pty::strip_escapes(&String::from_utf8_lossy(&tail));
                        let _ = proxy.send_event(PtyEvent::Exited { last_output });
                    },
                },
            )
            .expect("spawn shell over pty")
        };
        let pty_time = t_pty.elapsed();

        tracing::info!(
            window = ?window_time,
            pty = ?pty_time,
            "window and pty ready",
        );

        window.request_redraw();
        self.state = Some(State {
            window,
            first_frame_start: Some(self.start),
            gpu,
            terminal,
            dirty,
            sync_pending,
            grid,
            pty,
            font_size: self.font_size,
            scale_factor,
            modifiers: ModifiersState::empty(),
            focused: true,
            app_focused: true,
            last_bell: None,
            cursor_anim: [0.0, 0.0],
            cursor_animation: self.cursor_animation,
            cursor_corner_anim: [[0.0, 0.0]; 4],
            cursor_was_anchored: false,
            popover_scrolls: Vec::new(),
            popover_scroll_downs: Vec::new(),
            grid_scroll: 0.0,
            scrollback_visual: 0.0,
            scrollback_target: 0.0,
            scrollback_grid: Grid::new(0, 0),
            last_scrollback_offset: None,
            region_scroll: 0.0,
            last_region_offset: 0.0,
            pool_anims: BTreeMap::new(),
            pools_scratch: Vec::new(),
            active_scratch: Vec::new(),
            overflows_scratch: Vec::new(),
            aux: Vec::new(),
            window_event_tx,
            wheel_pixels: 0.0,
            pointer_cell: (0, 0),
            pressed_button: None,
            pointer_side_right: false,
            native_drag: false,
            clipboard: None,
            last_redraw: None,
            #[cfg(feature = "perf")]
            show_perf_hud: false,
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: PtyEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        match event {
            PtyEvent::Responses(responses) => {
                let _ = state.pty.write(&responses);
            },
            PtyEvent::Redraw => {
                // Clear before requesting so a chunk parsed during this cycle
                // re-arms the next wakeup; the redraw projects the latest state.
                state.dirty.store(false, Ordering::Relaxed);
                state.window.request_redraw();
            },
            PtyEvent::Term(events) => handle_term_events(
                state,
                event_loop,
                &AuxWindowConfig {
                    proxy: &self.proxy,
                    theme: self.theme,
                    font_family: &self.font_family,
                    ligatures: self.ligatures,
                },
                events,
            ),
            PtyEvent::AuxGpuReady { window, gpu } => {
                if let Some(aux) = state.aux.iter_mut().find(|aux| aux.id == window) {
                    aux.gpu = Some(*gpu);
                    aux.window.request_redraw();
                }
            },
            PtyEvent::Exited { last_output } => {
                let status = state.pty.exit_status(Duration::from_millis(500));
                if status.as_ref().is_none_or(|status| !status.success()) {
                    let exit_code = status.as_ref().map(|status| status.exit_code());
                    let signal = status.as_ref().and_then(|status| status.signal());
                    if last_output.is_empty() {
                        tracing::warn!(?exit_code, ?signal, "child exited with error");
                    } else {
                        tracing::warn!(?exit_code, ?signal, %last_output, "child exited with error");
                    }
                } else {
                    tracing::info!("child exited cleanly");
                }
                event_loop.exit();
            },
        }
    }

    /// Drive the DEC 2026 synchronized-update timeout.
    ///
    /// While an update buffers in the parser, wait until its deadline and then
    /// flush it, so a missing or slow ESU cannot freeze the screen; the redraw
    /// the flush warrants is requested here. With no update pending the loop waits
    /// idle for the next event. The chunk that opens an update always warrants a
    /// redraw (its BSU bytes reach the screen ahead of the buffer), so the reader
    /// always wakes the main thread once to arm this.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        // Reconcile app-wide focus after the current event batch, so a switch
        // between stoatty windows (one focus-out plus one focus-in) settles to
        // no net report while a click to a foreign app reports the app unfocused.
        reconcile_app_focus(state);

        if !state.sync_pending.load(Ordering::Relaxed) {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }

        // Flushing a synchronized update dispatches its buffered bytes, which
        // can carry host queries and notifications the parse held back, so
        // drain both alongside the flush rather than losing them.
        let (deadline, drained) = {
            let mut terminal = state.terminal.lock();
            match terminal.sync_deadline() {
                Some(deadline) if deadline <= Instant::now() => {
                    terminal.flush_synchronized_update();
                    (
                        None,
                        Some((terminal.take_responses(), terminal.take_events())),
                    )
                },
                other => (other, None),
            }
        };

        if let Some((responses, events)) = drained {
            if !responses.is_empty() {
                let _ = state.pty.write(&responses);
            }
            if !events.is_empty() {
                handle_term_events(
                    state,
                    event_loop,
                    &AuxWindowConfig {
                        proxy: &self.proxy,
                        theme: self.theme,
                        font_family: &self.font_family,
                        ligatures: self.ligatures,
                    },
                    events,
                );
            }
        }

        match deadline {
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => {
                state.sync_pending.store(false, Ordering::Relaxed);
                state.window.request_redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            },
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        // Aux windows are second render targets for detached panes. Their
        // lifecycle and pointer events are handled here and consumed, the
        // pointer ones translated to window-relative cells and reported over the
        // socket so they never reach the primary arms with aux coordinates.
        // Keyboard and modifier events fall through to the primary handling
        // below so an aux keypress drives the one PTY like a primary keypress.
        let primary = id == state.window.id();
        if !primary {
            match &event {
                WindowEvent::KeyboardInput { .. } | WindowEvent::ModifiersChanged(_) => {},
                WindowEvent::RedrawRequested => {
                    let font_size = state.font_size;
                    let scale = state.scale_factor as f32;
                    if let Some(aux) = state.aux.iter_mut().find(|aux| aux.window.id() == id) {
                        let now = Instant::now();
                        let dt = aux
                            .last_redraw
                            .map(|prev| now.duration_since(prev).min(MAX_EASE_DT))
                            .unwrap_or(EASE_BASELINE_FRAME);
                        aux.last_redraw = Some(now);
                        if redraw_aux(aux, &state.terminal, font_size, scale, dt) {
                            aux.window.request_redraw();
                        }
                    }
                    return;
                },
                WindowEvent::Resized(size) => {
                    let size = *size;
                    let resized = state
                        .aux
                        .iter_mut()
                        .find(|aux| aux.window.id() == id)
                        .and_then(|aux| {
                            let dims = aux.gpu.as_mut().map(|gpu| {
                                gpu.resize(size.width, size.height);
                                gpu.grid_size()
                            });
                            aux.window.request_redraw();
                            dims.map(|(rows, cols)| (aux.id, cols as u16, rows as u16))
                        });
                    if let Some((window, cols, rows)) = resized {
                        send_window_event(state, WindowIpcEvent::Resized { window, cols, rows });
                    }
                    return;
                },
                WindowEvent::Focused(gained) => {
                    let gained = *gained;
                    let window =
                        state
                            .aux
                            .iter_mut()
                            .find(|aux| aux.window.id() == id)
                            .map(|aux| {
                                aux.focused = gained;
                                aux.id
                            });
                    if gained && let Some(window) = window {
                        send_window_event(state, WindowIpcEvent::Focused { window });
                    }
                    return;
                },
                WindowEvent::CloseRequested => {
                    if let Some(window) = state
                        .aux
                        .iter()
                        .find(|aux| aux.window.id() == id)
                        .map(|aux| aux.id)
                    {
                        send_window_event(state, WindowIpcEvent::Closed { window });
                    }
                    state.aux.retain(|aux| aux.window.id() != id);
                    return;
                },
                WindowEvent::CursorMoved { position, .. } => {
                    let position = *position;
                    let cell_size = render::cell_size(state.font_size, state.scale_factor as f32);
                    let mods = modifier_bits(state.modifiers);
                    let event = state
                        .aux
                        .iter_mut()
                        .find(|aux| aux.window.id() == id)
                        .and_then(|aux| {
                            let (rows, cols) = aux.gpu.as_ref()?.grid_size();
                            let (col, row) = cell_at(position.x, position.y, cell_size, rows, cols);
                            aux.pointer_cell = (col as u16, row as u16);
                            aux.pressed
                                .and_then(ipc_button)
                                .map(|button| WindowIpcEvent::Mouse {
                                    window: aux.id,
                                    kind: MouseKind::Drag(button),
                                    col: col as u16,
                                    row: row as u16,
                                    mods,
                                })
                        });
                    if let Some(event) = event {
                        send_window_event(state, event);
                    }
                    return;
                },
                WindowEvent::MouseInput {
                    state: element_state,
                    button,
                    ..
                } => {
                    let pressed = *element_state == ElementState::Pressed;
                    let button = *button;
                    let mods = modifier_bits(state.modifiers);
                    let event = state
                        .aux
                        .iter_mut()
                        .find(|aux| aux.window.id() == id)
                        .and_then(|aux| {
                            let ipc = ipc_button(button)?;
                            aux.pressed = pressed.then_some(button);
                            let (col, row) = aux.pointer_cell;
                            let kind = if pressed {
                                MouseKind::Press(ipc)
                            } else {
                                MouseKind::Release(ipc)
                            };
                            Some(WindowIpcEvent::Mouse {
                                window: aux.id,
                                kind,
                                col,
                                row,
                                mods,
                            })
                        });
                    if let Some(event) = event {
                        send_window_event(state, event);
                    }
                    return;
                },
                WindowEvent::MouseWheel { delta, .. } => {
                    let delta = *delta;
                    let cell_height =
                        render::cell_size(state.font_size, state.scale_factor as f32)[1] as f64;
                    let mods = modifier_bits(state.modifiers);
                    let report = state
                        .aux
                        .iter_mut()
                        .find(|aux| aux.window.id() == id)
                        .and_then(|aux| {
                            let lines = wheel_lines(delta, &mut aux.wheel_pixels, cell_height);
                            (lines != 0).then(|| {
                                let kind = if lines > 0 {
                                    MouseKind::WheelUp
                                } else {
                                    MouseKind::WheelDown
                                };
                                let (col, row) = aux.pointer_cell;
                                (aux.id, kind, col, row, lines.unsigned_abs())
                            })
                        });
                    if let Some((window, kind, col, row, count)) = report {
                        for _ in 0..count {
                            send_window_event(
                                state,
                                WindowIpcEvent::Mouse {
                                    window,
                                    kind,
                                    col,
                                    row,
                                    mods,
                                },
                            );
                        }
                    }
                    return;
                },
                _ => return,
            }
        }

        match event {
            WindowEvent::CloseRequested => {
                tracing::info!("window close requested");
                event_loop.exit();
            },
            WindowEvent::Focused(gained) => {
                state.focused = gained;
                // The app-wide DECSET 1004 report is reconciled in about_to_wait
                // so a switch between stoatty windows nets no report.
                if gained {
                    send_window_event(state, WindowIpcEvent::Focused { window: 0 });
                    // Regaining focus clears any pending attention request, e.g.
                    // a dock bounce a bell raised while the window was in back.
                    state.window.request_user_attention(None);
                }
            },
            WindowEvent::Resized(size) => {
                state.gpu.resize(size.width, size.height);

                let (rows, cols) = state.gpu.grid_size();
                state.terminal.lock().resize(rows, cols);
                let _ = state.pty.resize(rows as u16, cols as u16);

                state.window.request_redraw();
            },
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.scale_factor = scale_factor;
                state
                    .gpu
                    .set_font_size(state.font_size, scale_factor as f32);
                update_cell_pixels(&state.terminal, state.font_size, scale_factor as f32);

                // The cell metrics moved with the new density; the surface is
                // re-fitted by the `Resized` that follows. Re-read the grid size
                // and resize the rest now, mirroring the font-zoom chain.
                let (rows, cols) = state.gpu.grid_size();
                state.terminal.lock().resize(rows, cols);
                let _ = state.pty.resize(rows as u16, cols as u16);

                state.window.request_redraw();
            },
            WindowEvent::RedrawRequested => {
                // The first redraw drives the first present, so report the total
                // cold-start time once, then never again.
                if let Some(start) = state.first_frame_start.take() {
                    tracing::info!(elapsed = ?start.elapsed(), "first frame");
                }

                // Each frame's easing advances by the wall time since the
                // previous frame, so animation speed stays refresh-rate
                // independent. The cap bounds the step after an idle gap, when
                // the elapsed time spans the whole idle period.
                let dt = {
                    let now = Instant::now();
                    let dt = state
                        .last_redraw
                        .map(|prev| now.duration_since(prev).min(MAX_EASE_DT))
                        .unwrap_or(EASE_BASELINE_FRAME);
                    state.last_redraw = Some(now);
                    dt
                };

                let (
                    cursor,
                    scroll_delta,
                    damage,
                    decoration_damage,
                    display_offset,
                    active,
                    pool_easing,
                    cursor_anchor,
                ) = {
                    let mut terminal = state.terminal.lock();
                    let (cursor, scroll_delta, damage) = terminal.project(&mut state.grid);
                    let decoration_damage = terminal.take_decoration_damage();
                    let display_offset = terminal.display_offset();
                    let mut pools = mem::take(&mut state.pools_scratch);
                    terminal.pools_into(&mut pools);

                    // Drop animation state for pools the app has retired, so a
                    // closed pane or dismissed modal stops compositing and frees
                    // its grids.
                    state
                        .pool_anims
                        .retain(|id, _| pools.iter().any(|pool| pool.id == *id));

                    // Step each pool's ease toward its target and project the ones
                    // still gliding and buffered, in ascending-id (z) order. A pool
                    // that just settled is left out so the live grid takes over; one
                    // easing but not yet buffered keeps the loop ticking via
                    // `pool_easing` until the app fills its window.
                    let mut active = mem::take(&mut state.active_scratch);
                    active.clear();
                    let mut pool_easing = false;
                    let mut cursor_anchor: Option<AnchoredCursor> = None;
                    for pool in &pools {
                        let anim = state
                            .pool_anims
                            .entry(pool.id)
                            .or_insert_with(|| PoolAnim::new(pool.scroll_target.pages()));
                        let reposition = terminal.take_reposition(pool.id);
                        let step = advance_pool_glide(anim, pool, &terminal, reposition, dt);
                        if matches!(step, PoolStep::Settled) {
                            continue;
                        }
                        pool_easing = true;

                        // While the focused pane glides it ships the primary
                        // cursor's document anchor, so place the cursor riding
                        // this pool's eased content offset instead of easing it
                        // toward the VT cell.
                        if let Some((row, col)) = pool.cursor_anchor {
                            let (pos, in_region) = anchored_cursor_pos(
                                pool.region.top as f32,
                                (pool.region.height as f32).max(1.0),
                                row as f32,
                                col as f32,
                                anim.scroll,
                            );
                            cursor_anchor = Some(AnchoredCursor {
                                pos,
                                in_region,
                                region: pool.region,
                            });
                        }

                        if let PoolStep::Gliding(tile) = step {
                            active.push(tile);
                        }
                    }

                    state.pools_scratch = pools;

                    (
                        cursor,
                        scroll_delta,
                        damage,
                        decoration_damage,
                        display_offset,
                        active,
                        pool_easing,
                        cursor_anchor,
                    )
                };

                let mut overflows = mem::take(&mut state.overflows_scratch);
                overflows.clear();
                overflows.extend(state.grid.overlays().iter().map(popover_overflow));
                state.popover_scrolls.resize(overflows.len(), 0.0);
                state.popover_scroll_downs.resize(overflows.len(), true);

                let mut popover_scrolling = false;
                for (index, overflow) in overflows.iter().copied().enumerate() {
                    match overflow {
                        Some(max) => {
                            let (next, down) = step_popover_scroll(
                                state.popover_scrolls[index],
                                state.popover_scroll_downs[index],
                                max,
                                dt,
                            );
                            state.popover_scrolls[index] = next;
                            state.popover_scroll_downs[index] = down;
                            popover_scrolling = true;
                        },
                        None => state.popover_scrolls[index] = 0.0,
                    }
                }
                state.overflows_scratch = overflows;

                let (grid_scroll, grid_scrolling) =
                    step_grid_scroll(state.grid_scroll, scroll_delta, dt);
                state.grid_scroll = grid_scroll;

                // Fold any auto-pin the terminal applied as live output grew into
                // both the target and the eased position, so growing history drags
                // neither -- only a wheel move, which advances the target alone,
                // starts an ease. Comparing against the target's integer part
                // folds whole-row pins while leaving any sub-cell offset intact.
                let pin = display_offset as f32 - state.scrollback_target.floor();
                state.scrollback_target += pin;
                state.scrollback_visual += pin;

                let (scrollback_visual, scrollback_scrolling) =
                    step_scrollback_scroll(state.scrollback_visual, state.scrollback_target, dt);
                state.scrollback_visual = scrollback_visual;

                let (region_scroll, region_scrolling) = match state.grid.scroll_region() {
                    Some(region) => {
                        let offset = region.offset as f32;
                        let delta = offset - state.last_region_offset;
                        state.last_region_offset = offset;
                        step_region_scroll(state.region_scroll, delta, dt)
                    },
                    None => {
                        state.last_region_offset = 0.0;
                        (0.0, false)
                    },
                };
                state.region_scroll = region_scroll;

                let cursor_easing = if active.is_empty() {
                    // With no pool mid-glide, fall to the scrollback window when
                    // the view is scrolled back, else the live grid.
                    let in_scrollback = state.scrollback_visual > 0.0 && state.grid.rows() > 0;

                    if in_scrollback {
                        // The view is scrolled back, so render the composed history
                        // window, gliding it by the sub-cell fraction. The integer
                        // offset selects which rows fill the window. Re-compose on an
                        // offset change or when live output redamaged the grid.
                        // Otherwise reuse the cached rows and only re-shift them.
                        let offset = state.scrollback_visual.floor() as i32;
                        let vt_changed = matches!(&damage, Damage::Full)
                            || matches!(&damage, Damage::Partial(rows) if rows.iter().any(|&d| d));
                        let rebuild = state.last_scrollback_offset != Some(offset) || vt_changed;
                        state.last_scrollback_offset = Some(offset);

                        if rebuild {
                            let terminal = state.terminal.lock();
                            terminal.project_scrollback(
                                &mut state.scrollback_grid,
                                state.scrollback_visual,
                            );
                        }

                        let sb_damage = if rebuild {
                            Damage::Full
                        } else {
                            Damage::Partial(Vec::new())
                        };

                        // The sub-cell shift project_scrollback returns, recomputed
                        // locally so a fraction-only frame needs no lock or compose.
                        let scroll_offset =
                            (state.scrollback_visual - state.scrollback_visual.floor()) - 1.0;

                        state.gpu.render(
                            &state.scrollback_grid,
                            Frame {
                                cursor: None,
                                cursor_corners: None,
                                scroll: Scroll {
                                    grid: 0.0,
                                    document: 0.0,
                                    scrollback: scroll_offset,
                                    region: 0.0,
                                    popovers: &[],
                                },
                                damage: &sb_damage,
                                decoration_damage: &sb_damage,
                            },
                        );
                        false
                    } else {
                        // At the live bottom, render the projected live grid (cursor
                        // and decorations), cursor easing as usual. No lock or compose
                        // here, so the cached scrollback rows are left untouched.
                        state.last_scrollback_offset = None;
                        seed_settle_flight(
                            &mut state.cursor_was_anchored,
                            &mut state.cursor_anim,
                            &mut state.cursor_corner_anim,
                            state.grid.rows(),
                        );
                        let (cursor, cursor_corners, easing) = step_cursor(
                            state.cursor_animation,
                            &mut state.cursor_anim,
                            &mut state.cursor_corner_anim,
                            cursor_position(cursor),
                            dt,
                        );
                        state.gpu.render(
                            &state.grid,
                            Frame {
                                cursor,
                                cursor_corners,
                                scroll: Scroll {
                                    grid: state.grid_scroll,
                                    document: 0.0,
                                    scrollback: 0.0,
                                    region: state.region_scroll,
                                    popovers: &state.popover_scrolls,
                                },
                                damage: &damage,
                                decoration_damage: &decoration_damage,
                            },
                        );
                        easing
                    }
                } else {
                    // One or more pools are mid-glide and buffered: render the live
                    // grid as the static chrome base (cursor and all), then
                    // composite each pool's eased rows over its region in
                    // ascending-id z-order, gliding by the sub-cell fraction and
                    // clipping to the region. The live grid -- which the app keeps
                    // painted at each pool's rested position -- shows again the
                    // instant every pool settles, so an edit, a modal, or the shell
                    // after the app exits appears at once instead of under a frozen
                    // pool.
                    let [cw, ch] = render::cell_size(state.font_size, state.scale_factor as f32);

                    for pool in &active {
                        // A shift-only frame reuses the pool grid copied on the
                        // last content-changed frame, so only recopy when the
                        // composed rows actually changed.
                        if !pool.content_changed {
                            continue;
                        }
                        let anim = state
                            .pool_anims
                            .get_mut(&pool.id)
                            .expect("active pool has anim state");
                        copy_pool_region(
                            &mut anim.pool_grid,
                            &anim.document_grid,
                            &state.grid,
                            pool.region,
                        );
                    }

                    // Floor each edge to the grid-row boundary the renderer lays
                    // cells on, then take the span, so each scissor covers exactly
                    // its region's rows. Flooring width and height on their own
                    // would round the far edge to a different pixel than the
                    // adjacent row, leaking a sliver of one surface into the next.
                    //
                    // Unlike the pool, active, and overflow buffers, this one holds
                    // borrows into pool_anims, so it cannot be a reused state field
                    // without a self-referential borrow and stays freshly allocated.
                    let composites = active
                        .iter()
                        .map(|pool| {
                            let region = pool.region;
                            let x0 = (region.left as f32 * cw) as u32;
                            let y0 = (region.top as f32 * ch) as u32;
                            let x1 = ((region.left as f32 + region.width as f32) * cw) as u32;
                            let y1 = ((region.top as f32 + region.height as f32) * ch) as u32;
                            PoolComposite {
                                grid: &state.pool_anims[&pool.id].pool_grid,
                                scissor: [x0, y0, x1 - x0, y1 - y0],
                                shift_rows: -pool.frac,
                                content_changed: pool.content_changed,
                                occludable: pool.id < NON_PANE_POOL_BASE,
                            }
                        })
                        .collect::<Vec<_>>();

                    let (base_cursor, base_corners, cursor_easing) = match cursor_anchor {
                        Some(anchor) => {
                            // The anchor is frame-locked to the pool's eased
                            // content offset, so the cursor is placed directly
                            // rather than eased toward the VT cell. Once its line
                            // has scrolled off the pool it leaves the region and
                            // hides. Keep the anim in sync for a clean settle.
                            state.cursor_anim = anchor.pos;
                            state.cursor_corner_anim = block_corners(anchor.pos);
                            state.cursor_was_anchored = true;
                            if anchor.in_region {
                                (Some(anchor.pos), Some(block_corners(anchor.pos)), false)
                            } else {
                                (None, None, false)
                            }
                        },
                        None => {
                            seed_settle_flight(
                                &mut state.cursor_was_anchored,
                                &mut state.cursor_anim,
                                &mut state.cursor_corner_anim,
                                state.grid.rows(),
                            );
                            step_cursor(
                                state.cursor_animation,
                                &mut state.cursor_anim,
                                &mut state.cursor_corner_anim,
                                cursor_position(cursor),
                                dt,
                            )
                        },
                    };

                    // The pool composites paint over the cursor's cell, so the
                    // cursor draws on top of them, clipped to the pool it sits in
                    // (topmost when they stack) so its block does not bleed past
                    // that pane. An anchored cursor rides a known pool, so clip to
                    // that region rather than the stale VT cell.
                    let cursor_scissor = match cursor_anchor {
                        Some(anchor) => Some(region_scissor(anchor.region, cw, ch)),
                        None => active
                            .iter()
                            .rev()
                            .find(|pool| cursor_in_region(cursor, pool.region))
                            .map(|pool| region_scissor(pool.region, cw, ch)),
                    };

                    if state.gpu.render_with_pools(
                        &state.grid,
                        Frame {
                            cursor: base_cursor,
                            cursor_corners: base_corners,
                            scroll: Scroll {
                                grid: state.grid_scroll,
                                document: 0.0,
                                scrollback: 0.0,
                                region: state.region_scroll,
                                popovers: &state.popover_scrolls,
                            },
                            damage: &damage,
                            decoration_damage: &decoration_damage,
                        },
                        &composites,
                        cursor_scissor,
                    ) {
                        // A pool composite grew or evicted from the atlas after
                        // the live grid was drawn, so the live buffers now hold
                        // stale UVs. Schedule the heal frame an idle screen would
                        // otherwise skip. The next prepare rebuilds them.
                        state.window.request_redraw();
                    }
                    cursor_easing
                };

                state.active_scratch = active;

                // Keep the vsync-paced loop running while the cursor eases, a
                // popover scrolls, or the grid, scrollback, a region, or a pool
                // scrolls. When all settle the loop idles until the next PTY
                // output or resize.
                // The perf HUD updates every frame while shown, so it keeps the
                // loop alive like an easing animation does.
                #[cfg(feature = "perf")]
                let hud_streaming = state.show_perf_hud;
                #[cfg(not(feature = "perf"))]
                let hud_streaming = false;
                if cursor_easing
                    || popover_scrolling
                    || grid_scrolling
                    || scrollback_scrolling
                    || region_scrolling
                    || pool_easing
                    || hud_streaming
                {
                    state.window.request_redraw();
                }
            },
            WindowEvent::ModifiersChanged(modifiers) => {
                state.modifiers = modifiers.state();
            },
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }

                let platform_mod_held = if cfg!(target_os = "macos") {
                    state.modifiers.super_key()
                } else {
                    state.modifiers.control_key()
                };

                // Font zoom resizes the primary surface, so it stays a primary
                // control. Per-window font zoom is out of scope, so an aux zoom
                // combo falls through and reaches the PTY as a plain keystroke.
                if primary && let Some(delta) = font_step(platform_mod_held, &event.logical_key) {
                    let font_size =
                        (state.font_size as i32 + delta).max(FONT_SIZE_FLOOR as i32) as u32;
                    state.font_size = font_size;
                    state
                        .gpu
                        .set_font_size(font_size, state.scale_factor as f32);
                    update_cell_pixels(&state.terminal, font_size, state.scale_factor as f32);

                    // The surface is unchanged, so skip `gpu.resize`; only the cell
                    // metrics moved, so re-read the grid size and resize the rest.
                    let (rows, cols) = state.gpu.grid_size();
                    state.terminal.lock().resize(rows, cols);
                    let _ = state.pty.resize(rows as u16, cols as u16);

                    state.window.request_redraw();
                    return;
                }

                #[cfg(feature = "perf")]
                if primary
                    && platform_mod_held
                    && state.modifiers.shift_key()
                    && matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("p"))
                {
                    state.show_perf_hud = !state.show_perf_hud;
                    state.gpu.set_perf_hud(state.show_perf_hud);
                    state.window.request_redraw();
                    return;
                }

                // The clipboard combo is super on macOS, ctrl+shift elsewhere,
                // shared by copy (c) and paste (v).
                let clip_combo = if cfg!(target_os = "macos") {
                    state.modifiers.super_key()
                } else {
                    state.modifiers.control_key() && state.modifiers.shift_key()
                };
                let is_paste_key = matches!(
                    &event.logical_key,
                    Key::Character(c) if c.eq_ignore_ascii_case("v")
                );
                if clip_combo && is_paste_key {
                    // Consume the combo whether or not the clipboard read
                    // succeeds, so encode_key never sends a stray "v".
                    if let Ok(text) = arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
                        let bracketed = {
                            let mut terminal = state.terminal.lock();
                            terminal.clear_selection();
                            terminal.bracketed_paste()
                        };
                        let _ = state.pty.write(&paste_bytes(&text, bracketed));
                        // Pasting jumps the view back to the live prompt, like typing.
                        state.terminal.lock().scroll_to_bottom();
                    }
                    return;
                }

                let is_copy_key = matches!(
                    &event.logical_key,
                    Key::Character(c) if c.eq_ignore_ascii_case("c")
                );
                if clip_combo && is_copy_key {
                    // Re-copy the live selection, keeping it highlighted. An empty
                    // selection falls through so a bare Ctrl-C still SIGINTs.
                    if let Some(text) = selection_copy_text(&state.terminal) {
                        copy_to_clipboard(state, &text);
                        return;
                    }
                }

                // A Cmd-combo stoatty did not handle above (copy on an empty
                // selection, or any other Cmd-key) must not reach encode_key as a
                // bare character on macOS, matching Terminal.app and iTerm2.
                if swallow_super_combo(state.modifiers) {
                    return;
                }

                if let Some(bytes) = encode_key(
                    &event.logical_key,
                    state.modifiers.control_key(),
                    state.modifiers.shift_key(),
                ) {
                    // Typing supersedes a live selection, so drop the highlight
                    // before it sits over fresh output.
                    state.terminal.lock().clear_selection();
                    let _ = state.pty.write(&bytes);
                    // Typing jumps the view back to the live prompt, the way a
                    // terminal resets scrollback on input.
                    state.terminal.lock().scroll_to_bottom();
                }
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let cell_height =
                    render::cell_size(state.font_size, state.scale_factor as f32)[1] as f64;
                let lines = wheel_lines(delta, &mut state.wheel_pixels, cell_height);
                if lines != 0 {
                    // Snapshot the wheel-routing modes under one lock so the
                    // branch reads a consistent terminal state.
                    let (mouse_report, alternate_scroll) = {
                        let terminal = state.terminal.lock();
                        (
                            terminal.mouse_mode() && terminal.sgr_mouse(),
                            terminal.is_alt_screen() && terminal.alternate_scroll(),
                        )
                    };
                    if mouse_report {
                        // A mouse-reporting app wants the wheel as a button press
                        // at the pointer, not scrolling; its redraw follows its
                        // response.
                        let (col, row) = state.pointer_cell;
                        let _ = state.pty.write(&sgr_wheel_bytes(lines, col, row));
                    } else if !state.modifiers.shift_key() && alternate_scroll {
                        // An alt-screen pager with alternate-scroll on expects
                        // arrow keys; Shift forces local scrollback instead.
                        let _ = state.pty.write(&alternate_scroll_bytes(lines));
                    } else {
                        // Advance the whole-cell scrollback target by the rows the
                        // move actually shifted the viewport, an idiomatic multiple
                        // of the wheel's line delta (clamped at the history edge).
                        // The render loop eases the visual position toward it,
                        // scrolling the history window through every row, so the
                        // motion is smooth and lands cell-aligned.
                        let moved = {
                            let mut terminal = state.terminal.lock();
                            let before = terminal.display_offset() as i32;
                            terminal.scroll_display(lines * SCROLLBACK_SCROLL_MULTIPLIER);
                            terminal.display_offset() as i32 - before
                        };
                        state.scrollback_target += moved as f32;
                        state.window.request_redraw();
                    }
                }
            },
            WindowEvent::CursorMoved { position, .. } => {
                let cell_size = render::cell_size(state.font_size, state.scale_factor as f32);
                let (rows, cols) = state.gpu.grid_size();
                let previous = state.pointer_cell;
                let previous_side = state.pointer_side_right;
                state.pointer_cell = cell_at(position.x, position.y, cell_size, rows, cols);
                state.pointer_side_right = position.x
                    - state.pointer_cell.0 as f64 * cell_size[0] as f64
                    > cell_size[0] as f64 / 2.0;

                // A native grid selection extends as the pointer crosses a cell
                // or cell-half boundary, and owns the pointer until release.
                if state.native_drag {
                    if state.pointer_cell != previous || state.pointer_side_right != previous_side {
                        let (col, row) = state.pointer_cell;
                        state
                            .terminal
                            .lock()
                            .update_selection(row, col, state.pointer_side_right);
                        state.window.request_redraw();
                    }
                    return;
                }

                if state.pointer_cell == previous {
                    return;
                }

                // Snapshot the motion-routing modes under one lock, matching the
                // wheel path, so the branch reads a consistent terminal state.
                let (sgr, drag, motion) = {
                    let terminal = state.terminal.lock();
                    (
                        terminal.mouse_mode() && terminal.sgr_mouse(),
                        terminal.mouse_drag(),
                        terminal.mouse_motion(),
                    )
                };
                let button = state.pressed_button;
                let report = sgr && ((button.is_some() && drag) || (button.is_none() && motion));
                if report {
                    let (col, row) = state.pointer_cell;
                    let _ = state.pty.write(&sgr_motion_bytes(button, col, row));
                }
            },
            WindowEvent::MouseInput {
                state: element_state,
                button,
                ..
            } => {
                let code = match button {
                    MouseButton::Left => 0,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                    _ => return,
                };
                let pressed = element_state == ElementState::Pressed;
                state.pressed_button = pressed.then_some(code);
                let sgr = {
                    let terminal = state.terminal.lock();
                    terminal.mouse_mode() && terminal.sgr_mouse()
                };

                // The left button drives a native grid selection when the child
                // is not SGR-reporting, or always on shift (the escape from a
                // mouse-capturing app). Otherwise the press is reported below.
                if code == 0 && (!sgr || state.modifiers.shift_key()) {
                    let (col, row) = state.pointer_cell;
                    if pressed {
                        state
                            .terminal
                            .lock()
                            .start_selection(row, col, state.pointer_side_right);
                        state.native_drag = true;
                    } else if state.native_drag {
                        state.native_drag = false;
                        // Copy on release but keep the selection highlighted, so
                        // Cmd-C can re-copy and the highlight does not flash away.
                        // It is cleared on supersession by a new drag, typing, or
                        // a paste.
                        if let Some(text) = selection_copy_text(&state.terminal) {
                            copy_to_clipboard(state, &text);
                        }
                    }
                    state.window.request_redraw();
                    return;
                }

                if sgr {
                    let (col, row) = state.pointer_cell;
                    let _ = state.pty.write(&sgr_button_bytes(code, pressed, col, row));
                }
            },
            _ => {},
        }
    }
}

/// Build the full-viewport `pool_grid` for a region composite, sized to
/// `live_grid`, with `document_grid`'s composed rows copied into the `region`
/// sub-rectangle.
///
/// `document_grid` holds the pooled `region.height + 1` rows by `region.width`
/// columns (one straddle row past the region, covering the sliver a sub-cell
/// glide reveals at the bottom edge) the term composed; they land at
/// (`region.top`, `region.left`). Cells of `document_grid` that would fall
/// outside `pool_grid` are skipped, so a region declared past the viewport
/// clips rather than panicking.
///
/// The scissor the renderer applies clips the composite to the region, so the
/// surround outside it is never drawn and is left as it was rather than
/// recleared each frame. Only a size change reblanks, via `Grid::resize`.
///
/// `document_grid`'s off-grid decorations (the pooled gutter's text runs and
/// bars) are region-local sixteenths, so they are translated by the region
/// origin and set on `pool_grid`, replacing any prior list. A decoration-free
/// `document_grid` therefore clears stale decorations rather than leaving them.
fn copy_pool_region(
    pool_grid: &mut Grid,
    document_grid: &Grid,
    live_grid: &Grid,
    region: PoolRegionCommand,
) {
    if pool_grid.rows() != live_grid.rows() || pool_grid.cols() != live_grid.cols() {
        pool_grid.resize(live_grid.rows(), live_grid.cols());
    }

    let top = region.top as usize;
    let left = region.left as usize;
    let cols = document_grid
        .cols()
        .min(pool_grid.cols().saturating_sub(left));
    let rows = document_grid
        .rows()
        .min(pool_grid.rows().saturating_sub(top));
    if cols == 0 {
        return;
    }

    for r in 0..rows {
        pool_grid.row_mut(top + r)[left..left + cols]
            .copy_from_slice(&document_grid.row(r)[..cols]);
    }

    let dx = region.left as i16 * 16;
    let dy = region.top as i16 * 16;
    pool_grid.set_text_runs(
        document_grid
            .text_runs()
            .iter()
            .map(|run| TextRun {
                col: run.col + dx,
                row: run.row + dy,
                ..run.clone()
            })
            .collect(),
    );
    pool_grid.set_bars(
        document_grid
            .bars()
            .iter()
            .map(|bar| Bar {
                x: bar.x + dx,
                y: bar.y + dy,
                ..*bar
            })
            .collect(),
    );
}

/// The cursor's cell position for the renderer, or `None` when it is hidden.
fn cursor_position(cursor: Cursor) -> Option<[f32; 2]> {
    if cursor.shape == CursorShape::Hidden {
        None
    } else {
        Some([cursor.col as f32, cursor.row as f32])
    }
}

/// Whether the cursor cell falls within `region`.
fn cursor_in_region(cursor: Cursor, region: PoolRegionCommand) -> bool {
    let col = cursor.col;
    let row = cursor.row;
    col >= region.left as usize
        && col < region.left as usize + region.width as usize
        && row >= region.top as usize
        && row < region.top as usize + region.height as usize
}

/// The four block corners [TL, TR, BL, BR] for a one-cell cursor block at
/// fractional cell origin `origin`.
fn block_corners(origin: [f32; 2]) -> [[f32; 2]; 4] {
    let [x, y] = origin;
    [[x, y], [x + 1.0, y], [x, y + 1.0], [x + 1.0, y + 1.0]]
}

/// The primary cursor's placement while it rides a gliding pool.
///
/// Frame-locked to the pool's eased content offset rather than eased toward the
/// VT cursor cell, so the cursor slides with the text under it.
#[derive(Clone, Copy)]
struct AnchoredCursor {
    /// Fractional cell position [col, row] the cursor draws at this frame.
    pos: [f32; 2],
    /// Whether [`Self::pos`] sits within the pool. The cursor hides once its line
    /// has scrolled off either edge.
    in_region: bool,
    /// The gliding pool's region, used to clip the drawn cursor to the pane.
    region: PoolRegionCommand,
}

/// The screen position a cursor anchored to a document row draws at while its
/// pool glides, and whether it still falls within the pool.
///
/// `top` and `page_rows` are the pool region's top row and height, `row` and
/// `col` the cursor's document display row and column, and `scroll` the pool's
/// eased scroll in pages. The cursor rides the eased content, so it leaves the
/// region once its line scrolls past either edge.
fn anchored_cursor_pos(
    top: f32,
    page_rows: f32,
    row: f32,
    col: f32,
    scroll: f32,
) -> ([f32; 2], bool) {
    let y = top + row - scroll * page_rows;
    let in_region = y >= top && y < top + page_rows;
    ([col, y], in_region)
}

/// The pixel scissor rect [x, y, width, height] covering pool `region`, laid out
/// on a `cw` by `ch` cell grid.
fn region_scissor(region: PoolRegionCommand, cw: f32, ch: f32) -> [u32; 4] {
    let x0 = (region.left as f32 * cw) as u32;
    let y0 = (region.top as f32 * ch) as u32;
    let x1 = ((region.left as f32 + region.width as f32) * cw) as u32;
    let y1 = ((region.top as f32 + region.height as f32) * ch) as u32;
    [x0, y0, x1 - x0, y1 - y0]
}

/// Seed the settle flight on the first non-anchored frame after a glide.
///
/// Leaves `point` at its last anchored position so [`step_cursor`] eases from
/// there to the landing cell rather than teleporting. When that position slid
/// off the viewport during the glide, it is pulled to just beyond the edge it
/// exited (`rows` is the viewport height), so the flight is a consistent length
/// regardless of how far the content scrolled.
fn seed_settle_flight(
    was_anchored: &mut bool,
    point: &mut [f32; 2],
    corners: &mut [[f32; 2]; 4],
    rows: usize,
) {
    if !*was_anchored {
        return;
    }
    *was_anchored = false;
    point[1] = point[1].clamp(-1.0, rows as f32);
    *corners = block_corners(*point);
}

/// The centroid of a quad's four corners.
fn centroid(corners: [[f32; 2]; 4]) -> [f32; 2] {
    [
        (corners[0][0] + corners[1][0] + corners[2][0] + corners[3][0]) / 4.0,
        (corners[0][1] + corners[1][1] + corners[2][1] + corners[3][1]) / 4.0,
    ]
}

/// Record the physical cell pixel size in the terminal so a CSI 14 t query can
/// report the text area in pixels.
///
/// Re-run whenever the font size or display scale factor changes, since the
/// cell metrics move with both.
fn update_cell_pixels(terminal: &FairMutex<Terminal>, font_size: u32, scale_factor: f32) {
    let [width, height] = render::cell_size(font_size, scale_factor);
    terminal
        .lock()
        .set_cell_pixels(width.round() as u16, height.round() as u16);
}

/// The font-size step a key press maps to, or `None` when it is not the
/// platform zoom combo.
///
/// `platform_mod_held` is whether the platform zoom modifier (Cmd on macOS,
/// Ctrl elsewhere) is held; the caller resolves which physical modifier that
/// is. With it held, `=` steps up by one and `-` steps down by one.
fn font_step(platform_mod_held: bool, key: &Key) -> Option<i32> {
    if !platform_mod_held {
        return None;
    }

    match key {
        Key::Character(s) if s.as_str() == "=" => Some(1),
        Key::Character(s) if s.as_str() == "-" => Some(-1),
        _ => None,
    }
}

/// Encode clipboard `text` for the PTY on paste.
///
/// In bracketed-paste mode the payload is wrapped in the DECSET 2004 guard
/// markers, with any embedded end-guard stripped so pasted bytes cannot close
/// the bracket early and inject input. Otherwise newlines are normalized to
/// carriage returns, matching what the Enter key sends.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let guarded = text.replace("\x1b[201~", "");
        format!("\x1b[200~{guarded}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Whether an unhandled key press is a macOS Cmd-combo that should be swallowed
/// rather than forwarded to the child.
///
/// True only on macOS while the super (Command) modifier is held. Terminal.app
/// and iTerm2 eat a Cmd-combo the terminal itself does not act on rather than
/// leak its bare character to the child, so a Cmd-C over an empty selection does
/// not reach the child editor as a `c`. Ctrl-based combos are never swallowed,
/// so a bare Ctrl-C still delivers SIGINT and the Linux ctrl+shift clipboard
/// chord is untouched.
fn swallow_super_combo(modifiers: ModifiersState) -> bool {
    cfg!(target_os = "macos") && modifiers.super_key()
}

/// Encode a key press into the bytes a terminal sends to the shell, or `None`
/// for a key with no terminal encoding (a bare modifier, a function key) so the
/// caller writes nothing.
///
/// `ctrl` is whether Ctrl is held: with it, an ASCII letter becomes its C0
/// control byte (Ctrl-C is `0x03`). Cursor keys use the normal-mode `CSI` forms
/// (`\x1b[A` through `\x1b[D`); printable keys pass through as their own UTF-8
/// bytes.
fn encode_key(key: &Key, ctrl: bool, shift: bool) -> Option<Vec<u8>> {
    match key {
        Key::Named(NamedKey::Enter) => Some(vec![b'\r']),
        Key::Named(NamedKey::Backspace) => Some(vec![0x7f]),
        Key::Named(NamedKey::Tab) if shift => Some(b"\x1b[Z".to_vec()),
        Key::Named(NamedKey::Tab) => Some(vec![b'\t']),
        Key::Named(NamedKey::Space) => Some(vec![b' ']),
        Key::Named(NamedKey::Escape) => Some(vec![0x1b]),
        Key::Named(NamedKey::ArrowUp) => Some(b"\x1b[A".to_vec()),
        Key::Named(NamedKey::ArrowDown) => Some(b"\x1b[B".to_vec()),
        Key::Named(NamedKey::ArrowRight) => Some(b"\x1b[C".to_vec()),
        Key::Named(NamedKey::ArrowLeft) => Some(b"\x1b[D".to_vec()),
        Key::Character(s) if ctrl => ctrl_byte(s),
        Key::Character(s) => Some(s.as_str().as_bytes().to_vec()),
        _ => None,
    }
}

/// The C0 control byte for Ctrl held with a single ASCII letter (Ctrl-C is
/// `0x03`), or `None` when `s` is not one such letter.
fn ctrl_byte(s: &str) -> Option<Vec<u8>> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() || !c.is_ascii_alphabetic() {
        return None;
    }

    Some(vec![(c.to_ascii_uppercase() as u8) & 0x1f])
}

/// Resolve a wheel `delta` to whole lines of scrollback to move, positive
/// scrolling up into history.
///
/// Both delta kinds accrue in `pixels` against `cell_height` and yield whole
/// lines once a cell's worth has built up, carrying the sub-line remainder so a
/// stream of small deltas is not lost. A `LineDelta` scales its line count by
/// `cell_height` into the same accumulator, so a whole-notch mouse (`y = 1.0`)
/// still moves one line per event while a hi-res wheel's fractional line deltas
/// carry across events instead of rounding to zero.
/// Map a winit pointer button to the protocol button, or `None` for a button
/// the pane mouse path does not act on (back, forward, extra).
fn ipc_button(button: MouseButton) -> Option<IpcMouseButton> {
    match button {
        MouseButton::Left => Some(IpcMouseButton::Left),
        MouseButton::Middle => Some(IpcMouseButton::Middle),
        MouseButton::Right => Some(IpcMouseButton::Right),
        _ => None,
    }
}

/// Pack the active keyboard modifiers into a bitmask, with shift at `0x1`,
/// control at `0x2`, alt at `0x4`, and super at `0x8`.
fn modifier_bits(mods: ModifiersState) -> u8 {
    u8::from(mods.shift_key())
        | u8::from(mods.control_key()) << 1
        | u8::from(mods.alt_key()) << 2
        | u8::from(mods.super_key()) << 3
}

fn wheel_lines(delta: MouseScrollDelta, pixels: &mut f64, cell_height: f64) -> i32 {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => *pixels += f64::from(y) * cell_height,
        MouseScrollDelta::PixelDelta(position) => *pixels += position.y,
    }
    let lines = (*pixels / cell_height) as i32;
    *pixels -= f64::from(lines) * cell_height;
    lines
}

/// Encode `lines` of wheel scroll as the application-cursor arrow keys an
/// alt-screen pager reads under alternate-scroll mode: one `ESC O A` (up) per
/// line when `lines` is positive, one `ESC O B` (down) per line when negative.
fn alternate_scroll_bytes(lines: i32) -> Vec<u8> {
    let arrow: &[u8] = if lines > 0 { b"\x1bOA" } else { b"\x1bOB" };
    arrow.repeat(lines.unsigned_abs() as usize)
}

/// Encode `lines` of wheel scroll as SGR mouse-wheel reports at cell
/// (`col`, `row`): one button-press report per line, button 64 (up) when
/// `lines` is positive, 65 (down) when negative, with 1-based coordinates.
fn sgr_wheel_bytes(lines: i32, col: usize, row: usize) -> Vec<u8> {
    let button = if lines > 0 { 64 } else { 65 };
    let report = format!("\x1b[<{button};{};{}M", col + 1, row + 1);
    report.repeat(lines.unsigned_abs() as usize).into_bytes()
}

/// Encode a mouse button press or release at cell (`col`, `row`) as an SGR
/// (1006) report: `button` (0 left, 1 middle, 2 right) with 1-based
/// coordinates, terminated by `M` on press and `m` on release. SGR reports the
/// real button on release, unlike legacy mouse encodings.
fn sgr_button_bytes(button: u8, pressed: bool, col: usize, row: usize) -> Vec<u8> {
    let terminator = if pressed { 'M' } else { 'm' };
    format!("\x1b[<{button};{};{}{terminator}", col + 1, row + 1).into_bytes()
}

/// Encode pointer motion at cell (`col`, `row`) as an SGR (1006) motion report.
///
/// The code is the held button (0 left, 1 middle, 2 right) plus the 32 motion
/// flag, or code 3 (no button) plus 32 for buttonless any-motion (1003)
/// tracking, with 1-based coordinates and a trailing `M`.
fn sgr_motion_bytes(button: Option<u8>, col: usize, row: usize) -> Vec<u8> {
    let code = button.unwrap_or(3) + 32;
    format!("\x1b[<{code};{};{}M", col + 1, row + 1).into_bytes()
}

/// Apply host-facing terminal notifications off the grid.
///
/// Title and reset-title set the window title. Clipboard-store copies to the
/// system clipboard. Bell rings the terminal bell. Notification raises a desktop
/// notification.
///
/// The window-lifecycle events drive the aux OS windows detached panes render
/// into. Open creates one, building its GPU off-thread per `config`. Close,
/// focus, and dirty act on the matching live window, closing it, OS-focusing
/// it, or requesting its redraw.
fn handle_term_events(
    state: &mut State,
    event_loop: &ActiveEventLoop,
    config: &AuxWindowConfig<'_>,
    events: Vec<TermEvent>,
) {
    for event in events {
        match event {
            TermEvent::Title(title) => state.window.set_title(&title),
            TermEvent::ResetTitle => state.window.set_title(DEFAULT_TITLE),
            TermEvent::ClipboardStore(text) => copy_to_clipboard(state, &text),
            TermEvent::Bell => ring_bell(state, Instant::now()),
            TermEvent::Notification { title, body } => {
                deliver_notification(title.as_deref(), &body)
            },
            TermEvent::Hello(hello) => tracing::info!(
                pid = hello.pid,
                log_id = %hello.log_id,
                hostname = %hello.hostname,
                version = %hello.version,
                "program hello"
            ),
            TermEvent::WindowOpen(cmd) => open_aux_window(state, event_loop, config, cmd),
            TermEvent::WindowClose(window) => state.aux.retain(|aux| aux.id != window),
            TermEvent::WindowFocus(window) => {
                if let Some(aux) = state.aux.iter().find(|aux| aux.id == window) {
                    aux.window.focus_window();
                }
            },
            TermEvent::WindowDirty(window) => {
                if let Some(aux) = state.aux.iter().find(|aux| aux.id == window) {
                    aux.window.request_redraw();
                }
            },
        }
    }
}

/// Create the aux OS window a [`WindowOpenCommand`] asks for and start building
/// its renderer off the main thread.
///
/// The winit window is made on the main thread (winit requires it) sized to the
/// command's cell grid at the primary's current cell metrics, and pushed with no
/// GPU yet. Its [`GpuContext`] is built on a named background thread -- adapter
/// and device acquisition block there, never on the run loop -- and installed via
/// [`PtyEvent::AuxGpuReady`], so opening a window never stalls the primary. Until
/// it arrives the window's redraws find no GPU and draw nothing.
fn open_aux_window(
    state: &mut State,
    event_loop: &ActiveEventLoop,
    config: &AuxWindowConfig<'_>,
    cmd: WindowOpenCommand,
) {
    let WindowOpenCommand {
        window: window_id,
        cols,
        rows,
        title,
    } = cmd;

    let [cell_w, cell_h] = render::cell_size(state.font_size, state.scale_factor as f32);
    let attributes = Window::default_attributes()
        .with_title(title)
        .with_inner_size(LogicalSize::new(cols as f32 * cell_w, rows as f32 * cell_h));
    let window = match event_loop.create_window(attributes) {
        Ok(window) => Arc::new(window),
        Err(error) => {
            tracing::warn!(window = window_id, %error, "failed to create aux window");
            return;
        },
    };

    state.aux.push(AuxWindow {
        id: window_id,
        window: window.clone(),
        gpu: None,
        grid: Grid::new(0, 0),
        scratch: Grid::new(0, 0),
        focused: false,
        pointer_cell: (0, 0),
        pressed: None,
        wheel_pixels: 0.0,
        pool_anims: BTreeMap::new(),
        last_redraw: None,
        last_compose: None,
        pool_scratch: Vec::new(),
    });

    let size = window.inner_size();
    let font_size = state.font_size;
    let scale_factor = state.scale_factor as f32;
    let proxy = config.proxy.clone();
    let theme = config.theme;
    let font_family = config.font_family.to_vec();
    let ligatures = config.ligatures;
    let spawn = std::thread::Builder::new()
        .name(format!("aux-gpu-{window_id}"))
        .spawn(move || {
            let gpu = GpuContext::new(
                window,
                size.width.max(1),
                size.height.max(1),
                FontLoad::spawn(),
                FontConfig {
                    size: font_size,
                    scale_factor,
                    family: &font_family,
                    ligatures,
                },
                theme.background,
                theme.cursor,
            );
            let _ = proxy.send_event(PtyEvent::AuxGpuReady {
                window: window_id,
                gpu: Box::new(gpu),
            });
        });
    if let Err(error) = spawn {
        tracing::warn!(window = window_id, %error, "failed to spawn aux gpu thread");
        state.aux.retain(|aux| aux.id != window_id);
    }
}

/// Redraw one aux window, easing its window-bound pools toward their scroll
/// targets and presenting the result. Returns whether any pool is still gliding,
/// so the caller reschedules the next frame.
///
/// A [`compose_aux_grid`] base holds every pool at its target, so a settled pool
/// shows there while the gliding ones composite over their regions at the eased
/// offset -- a pool drops its composite the instant it settles and the base's
/// target content (its rested position) shows through, with no live grid to hand
/// back to.
///
/// The terminal is locked only for the read-only compose and step, then released
/// before the GPU present, so the reader thread and the primary redraw path are
/// never held off by an aux frame. Returns `false` without drawing when the GPU
/// is still building.
fn redraw_aux(
    aux: &mut AuxWindow,
    terminal: &FairMutex<Terminal>,
    font_size: u32,
    scale: f32,
    dt: Duration,
) -> bool {
    let Some(gpu) = aux.gpu.as_mut() else {
        return false;
    };
    let (rows, cols) = gpu.grid_size();
    let [cw, ch] = render::cell_size(font_size, scale);

    let mut easing = false;
    let mut active: Vec<ActivePool> = Vec::new();
    let mut recomposed = false;
    {
        let mut terminal = terminal.lock();
        let mut pools = mem::take(&mut aux.pool_scratch);
        terminal.window_pools_into(aux.id, &mut pools);

        // The base compose reads only each pool's content, scroll target, and
        // region plus the window size, so recompose it only when their hash
        // moves. A pure sub-cell glide leaves the base untouched and rides the
        // overlay below.
        let compose_hash = aux_compose_hash(&pools, rows, cols);
        if aux.last_compose != Some(compose_hash) {
            aux.last_compose = Some(compose_hash);
            recomposed = true;
            compose_aux_grid(
                &terminal,
                &pools,
                &mut aux.grid,
                &mut aux.scratch,
                rows,
                cols,
            );
        }

        // Step each window pool's ease and collect the ones still gliding, in
        // ascending-id z-order, dropping anim state for pools the app retired.
        aux.pool_anims
            .retain(|id, _| pools.iter().any(|pool| pool.id == *id));
        for pool in &pools {
            let anim = aux
                .pool_anims
                .entry(pool.id)
                .or_insert_with(|| PoolAnim::new(pool.scroll_target.pages()));
            let reposition = terminal.take_reposition(pool.id);
            match advance_pool_glide(anim, pool, &terminal, reposition, dt) {
                PoolStep::Settled => {},
                PoolStep::Degraded => easing = true,
                PoolStep::Gliding(tile) => {
                    easing = true;
                    active.push(tile);
                },
            }
        }

        aux.pool_scratch = pools;
    }

    // Copy each gliding pool's composed rows over the base, then composite them
    // scissored to their regions and shifted by the sub-cell fraction.
    for pool in &active {
        if !pool.content_changed {
            continue;
        }
        let anim = aux
            .pool_anims
            .get_mut(&pool.id)
            .expect("active pool has anim state");
        copy_pool_region(
            &mut anim.pool_grid,
            &anim.document_grid,
            &aux.grid,
            pool.region,
        );
    }
    let composites = active
        .iter()
        .map(|pool| PoolComposite {
            grid: &aux.pool_anims[&pool.id].pool_grid,
            scissor: region_scissor(pool.region, cw, ch),
            shift_rows: -pool.frac,
            content_changed: pool.content_changed,
            occludable: pool.id < NON_PANE_POOL_BASE,
        })
        .collect::<Vec<_>>();

    // A recomposed base rebuilds every instance. A skipped one reuses last
    // frame's, so empty partial damage leaves them untouched.
    let damage = if recomposed {
        Damage::Full
    } else {
        Damage::Partial(Vec::new())
    };
    let heal = gpu.render_with_pools(
        &aux.grid,
        Frame {
            cursor: None,
            cursor_corners: None,
            scroll: Scroll {
                grid: 0.0,
                document: 0.0,
                scrollback: 0.0,
                region: 0.0,
                popovers: &[],
            },
            damage: &damage,
            decoration_damage: &damage,
        },
        &composites,
        None,
    );
    easing || heal
}

/// Hash the inputs [`compose_aux_grid`] reads, so [`redraw_aux`] can skip the
/// recompose when nothing they cover moved.
///
/// Covers the window size and, per pool in z-order, its id, content version,
/// scroll target, and region rectangle. The sub-cell glide fraction is not
/// covered, since it rides the overlay rather than the base.
fn aux_compose_hash(pools: &[PoolView], rows: usize, cols: usize) -> u64 {
    let mut hasher = FxHasher::default();
    rows.hash(&mut hasher);
    cols.hash(&mut hasher);
    for pool in pools {
        pool.id.hash(&mut hasher);
        pool.content_version.hash(&mut hasher);
        pool.scroll_target.pages().to_bits().hash(&mut hasher);
        pool.region.top.hash(&mut hasher);
        pool.region.left.hash(&mut hasher);
        pool.region.width.hash(&mut hasher);
        pool.region.height.hash(&mut hasher);
    }
    hasher.finish()
}

/// Compose every pool in `pools` into `grid`, sized to `rows` x `cols`, each
/// pool's cells and decorations placed at its window-relative region.
///
/// `grid` is blanked first, so a cell no pool covers shows the window
/// background, and `scratch` is reused to project each pool before its rows are
/// blitted. Pools compose in ascending-id order, their off-grid text runs and
/// bars translated from region-local to window coordinates. v1 projects at each
/// pool's scroll target directly, with no sub-cell glide, so only the region's
/// own rows are copied and the straddle row `project_pool` composes is dropped.
fn compose_aux_grid(
    terminal: &Terminal,
    pools: &[PoolView],
    grid: &mut Grid,
    scratch: &mut Grid,
    rows: usize,
    cols: usize,
) {
    if grid.rows() != rows || grid.cols() != cols {
        grid.resize(rows, cols);
    } else {
        grid.clear();
    }

    let mut text_runs = Vec::new();
    let mut bars = Vec::new();
    for pool in pools {
        if terminal
            .project_pool(pool.id, scratch, pool.scroll_target.pages())
            .is_none()
        {
            continue;
        }

        let top = pool.region.top as usize;
        let left = pool.region.left as usize;
        let region_rows = (pool.region.height as usize)
            .min(scratch.rows())
            .min(grid.rows().saturating_sub(top));
        let region_cols = (pool.region.width as usize)
            .min(scratch.cols())
            .min(grid.cols().saturating_sub(left));
        if region_cols == 0 {
            continue;
        }

        for r in 0..region_rows {
            grid.row_mut(top + r)[left..left + region_cols]
                .copy_from_slice(&scratch.row(r)[..region_cols]);
        }

        let dx = pool.region.left as i16 * 16;
        let dy = pool.region.top as i16 * 16;
        text_runs.extend(scratch.text_runs().iter().map(|run| TextRun {
            col: run.col + dx,
            row: run.row + dy,
            ..run.clone()
        }));
        bars.extend(scratch.bars().iter().map(|bar| Bar {
            x: bar.x + dx,
            y: bar.y + dy,
            ..*bar
        }));
    }

    grid.set_text_runs(text_runs);
    grid.set_bars(bars);
}

/// Send a window-lifecycle event to the child over the window-event socket.
///
/// A no-op when the socket did not bind. The line is only queued on the channel,
/// so this never blocks on socket IO. The serving thread forwards it to the
/// connected child.
fn send_window_event(state: &State, event: WindowIpcEvent) {
    if let Some(tx) = &state.window_event_tx {
        let _ = tx.send(event.encode_line());
    }
}

/// Report an app-wide DECSET 1004 focus change to the child when it flips.
///
/// The app counts as focused while the primary or any aux window holds focus, so
/// a switch between stoatty windows nets no report and only a move to or from a
/// foreign app crosses the boundary. Gated on the child having requested focus
/// reporting.
fn reconcile_app_focus(state: &mut State) {
    let focused = app_has_focus(state.focused, state.aux.iter().map(|aux| aux.focused));
    if focused == state.app_focused {
        return;
    }
    state.app_focused = focused;
    if state.terminal.lock().report_focus_in_out() {
        let report: &[u8] = if focused { b"\x1b[I" } else { b"\x1b[O" };
        let _ = state.pty.write(report);
    }
}

/// Whether the app as a whole holds focus, true when the primary window or any
/// aux window is focused.
fn app_has_focus(primary: bool, aux: impl IntoIterator<Item = bool>) -> bool {
    primary || aux.into_iter().any(|focused| focused)
}

/// Bind the window-event socket and start its serving thread, or report that no
/// socket is available.
///
/// Returns the path to export as `STOATTY_WINDOW_SOCKET` and the channel aux
/// windows report on. Both are `None` on a bind failure or a non-unix build,
/// where aux windows render but report nothing upstream.
fn open_window_event_socket() -> (Option<PathBuf>, Option<Sender<String>>) {
    #[cfg(unix)]
    {
        match bind_window_socket() {
            Ok((path, tx)) => (Some(path), Some(tx)),
            Err(error) => {
                tracing::warn!(%error, "window-event socket unavailable");
                (None, None)
            },
        }
    }
    #[cfg(not(unix))]
    {
        (None, None)
    }
}

/// The window-event socket path for a stoatty process, `stoatty-win-<pid>.sock`
/// in `dir`. Per-pid so concurrent stoatty processes never collide.
#[cfg(unix)]
fn window_socket_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("stoatty-win-{pid}.sock"))
}

/// Bind the per-pid window-event socket under the log directory and spawn the
/// thread forwarding queued events to the connected child.
#[cfg(unix)]
fn bind_window_socket() -> io::Result<(PathBuf, Sender<String>)> {
    let dir = stoat_log::log_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = window_socket_path(&dir, std::process::id());
    // A prior process at this pid may have left its socket behind, and bind
    // fails on an existing path, so clear a stale one first.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("window-events".to_string())
        .spawn(move || serve_window_events(listener, rx))?;
    Ok((path, tx))
}

/// Forward queued window-event lines to the connected child.
///
/// Serves one client at a time. Each accepted stream receives every subsequent
/// line terminated by '\n' until a write fails, then the thread re-accepts.
/// Events sent while no client is connected queue on the channel and flush to
/// the next one. Returns when the channel closes as the app exits.
#[cfg(unix)]
fn serve_window_events(listener: UnixListener, rx: Receiver<String>) {
    for client in listener.incoming() {
        let Ok(mut client) = client else { continue };
        loop {
            let Ok(line) = rx.recv() else { return };
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            if client.write_all(&bytes).is_err() {
                break;
            }
        }
    }
}

/// Minimum spacing between bells, so a catted binary's burst of BELs rings once
/// rather than storming the speakers and the dock.
const BELL_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// Ring the terminal bell for a BEL byte.
///
/// Requests window attention while unfocused (a dock bounce on macOS, an urgency
/// hint on X11/Wayland) and, on macOS, plays the system alert sound. Rate-limited
/// via [`bell_should_ring`], so a burst makes one beep and one attention request.
fn ring_bell(state: &mut State, now: Instant) {
    if !bell_should_ring(state.last_bell, now) {
        return;
    }
    state.last_bell = Some(now);

    if !state.focused {
        state
            .window
            .request_user_attention(Some(UserAttentionType::Informational));
    }

    play_system_bell();
}

/// Whether a bell should ring now, given the instant the previous one rang.
///
/// Rings when none has rung yet, or when at least [`BELL_MIN_INTERVAL`] has
/// passed since the last, collapsing a BEL burst to a single ring.
fn bell_should_ring(last_bell: Option<Instant>, now: Instant) -> bool {
    match last_bell {
        Some(prev) => now.duration_since(prev) >= BELL_MIN_INTERVAL,
        None => true,
    }
}

/// Play the system alert sound. macOS runs `osascript -e beep`, honoring the
/// user's chosen alert sound and volume. Other platforms have no portable beep
/// without an audio dependency, so this is a no-op there.
#[cfg(target_os = "macos")]
fn play_system_bell() {
    let mut command = Command::new("osascript");
    command.args(["-e", "beep"]);
    spawn_reaped(command);
}

#[cfg(not(target_os = "macos"))]
fn play_system_bell() {}

/// Spawn `command` and reap it on a detached thread, so a short-lived helper
/// process leaves no zombie once it exits.
#[cfg(unix)]
fn spawn_reaped(mut command: Command) {
    if let Ok(mut child) = command.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Show a desktop notification for an OSC 9 / OSC 777 sequence.
///
/// macOS runs `osascript`, passing the title and body as argv items the script
/// reads back, so the payload is never interpolated into the script text and
/// cannot inject AppleScript. Other unix runs `notify-send`. The notification
/// shows regardless of window focus, and a spawn failure is ignored.
#[cfg(target_os = "macos")]
fn deliver_notification(title: Option<&str>, body: &str) {
    let mut command = Command::new("osascript");
    command.args([
        "-e",
        "on run argv",
        "-e",
        "display notification (item 1 of argv) with title (item 2 of argv)",
        "-e",
        "end run",
        body,
        title.unwrap_or(DEFAULT_TITLE),
    ]);
    spawn_reaped(command);
}

#[cfg(all(unix, not(target_os = "macos")))]
fn deliver_notification(title: Option<&str>, body: &str) {
    let mut command = Command::new("notify-send");
    command.arg(title.unwrap_or(DEFAULT_TITLE));
    command.arg(body);
    spawn_reaped(command);
}

#[cfg(not(unix))]
fn deliver_notification(_title: Option<&str>, _body: &str) {}

/// Copy `text` to the OS clipboard through the handle cached on [`State`].
///
/// The handle is held across copies rather than reopened each time because X11
/// selection ownership lasts only while an `arboard::Clipboard` and its
/// background server thread stay alive. A handle opened and dropped per copy
/// releases ownership at once, losing the copied text before any paste unless a
/// clipboard manager races to grab it, and arboard's debug build prints a drop
/// warning to the launching terminal.
///
/// A failed open or copy is reported rather than fatal. A failed copy also
/// clears the cache so the next call reopens the handle.
fn copy_to_clipboard(state: &mut State, text: &str) {
    if state.clipboard.is_none() {
        match arboard::Clipboard::new() {
            Ok(clipboard) => state.clipboard = Some(clipboard),
            Err(err) => {
                eprintln!("stoatty: failed to open clipboard: {err}");
                return;
            },
        }
    }

    let Some(clipboard) = state.clipboard.as_mut() else {
        return;
    };
    if let Err(err) = clipboard.set_text(text.to_owned()) {
        eprintln!("stoatty: failed to copy selection to clipboard: {err}");
        state.clipboard = None;
    }
}

/// The current selection's text for a copy, or `None` when nothing non-empty is
/// selected. Reads the selection without clearing it, so the highlight persists
/// for a later re-copy.
fn selection_copy_text(terminal: &FairMutex<Terminal>) -> Option<String> {
    terminal.lock().selection_text().filter(|t| !t.is_empty())
}

/// The grid cell `(col, row)` under physical pixel (`x`, `y`), clamped to the
/// `rows` x `cols` grid, for a `cell_size` of `[width, height]` physical pixels.
fn cell_at(x: f64, y: f64, cell_size: [f32; 2], rows: usize, cols: usize) -> (usize, usize) {
    let col = ((x / f64::from(cell_size[0])) as usize).min(cols.saturating_sub(1));
    let row = ((y / f64::from(cell_size[1])) as usize).min(rows.saturating_sub(1));
    (col, row)
}

/// The reference frame duration the easing factors are expressed against. A
/// factor closes that fraction of the remaining distance per baseline frame,
/// and [`ease_alpha`] rescales it to the frame time actually elapsed, so the
/// motion traces the same curve at any refresh rate.
const EASE_BASELINE_FRAME: Duration = Duration::from_micros(16_667);

/// Cap on the per-frame easing step. The first frame after an idle gap sees an
/// elapsed time spanning the whole gap, which would otherwise snap every ease
/// to its target in one step.
const MAX_EASE_DT: Duration = Duration::from_millis(40);

/// Rescale a per-baseline-frame easing factor to the elapsed frame time `dt`.
///
/// Compounds the per-frame decay continuously, so two half-length frames
/// advance an ease exactly as far as one baseline frame. At `dt` equal to
/// [`EASE_BASELINE_FRAME`] this returns `factor` unchanged.
fn ease_alpha(factor: f32, dt: Duration) -> f32 {
    let frames = dt.as_secs_f32() / EASE_BASELINE_FRAME.as_secs_f32();
    1.0 - (1.0 - factor).powf(frames)
}

/// Scale a per-baseline-frame minimum step to the elapsed frame time `dt`, so
/// an ease's floor speed is a velocity rather than a per-frame distance.
fn min_step(step: f32, dt: Duration) -> f32 {
    step * dt.as_secs_f32() / EASE_BASELINE_FRAME.as_secs_f32()
}

/// Step the animated cursor toward `target` by the elapsed frame time `dt`,
/// returning the new position and whether it has reached the target.
///
/// Closes a fixed fraction of the remaining distance per baseline frame,
/// rescaled to `dt`, the exponential ease-out that reads as smooth cursor
/// motion. Within a small epsilon it snaps onto the target so the animation
/// terminates.
fn ease(current: [f32; 2], target: [f32; 2], dt: Duration) -> ([f32; 2], bool) {
    const FACTOR: f32 = 0.35;
    const EPSILON: f32 = 0.01;

    let dx = target[0] - current[0];
    let dy = target[1] - current[1];
    if dx.abs() < EPSILON && dy.abs() < EPSILON {
        return (target, true);
    }

    let alpha = ease_alpha(FACTOR, dt);
    ([current[0] + dx * alpha, current[1] + dy * alpha], false)
}

/// Step the warp cursor's four corners toward the block at `target_cell` by
/// the elapsed frame time `dt`, returning the new corners and whether they
/// have settled.
///
/// Each corner eases toward the corresponding corner of the target cell's
/// block. A corner on the leading side of travel, its offset from the current
/// centroid pointing the same way as the centroid's path to the target, closes
/// a larger fraction of its gap than a trailing one, so the quad stretches along
/// the motion path and collapses back to a square as it arrives. Snaps onto the
/// exact target block and reports settled once every corner is within `EPSILON`.
fn ease_corners(
    current: [[f32; 2]; 4],
    target_cell: [f32; 2],
    dt: Duration,
) -> ([[f32; 2]; 4], bool) {
    const LEADING: f32 = 0.45;
    const TRAILING: f32 = 0.22;
    const EPSILON: f32 = 0.01;

    let target = block_corners(target_cell);

    let settled = (0..4).all(|i| {
        (target[i][0] - current[i][0]).abs() < EPSILON
            && (target[i][1] - current[i][1]).abs() < EPSILON
    });
    if settled {
        return (target, true);
    }

    let cur_centroid = centroid(current);
    let travel = [
        centroid(target)[0] - cur_centroid[0],
        centroid(target)[1] - cur_centroid[1],
    ];

    let mut next = current;
    for i in 0..4 {
        let offset = [
            current[i][0] - cur_centroid[0],
            current[i][1] - cur_centroid[1],
        ];
        let leading = offset[0] * travel[0] + offset[1] * travel[1] > 0.0;
        let alpha = ease_alpha(if leading { LEADING } else { TRAILING }, dt);
        next[i] = [
            current[i][0] + (target[i][0] - current[i][0]) * alpha,
            current[i][1] + (target[i][1] - current[i][1]) * alpha,
        ];
    }
    (next, false)
}

/// One frame's cursor render inputs from [`step_cursor`]. Holds the
/// ligature-break cell, the cursor block's four corners, and whether the
/// animation is still moving, all absent when the cursor is hidden.
type CursorStep = (Option<[f32; 2]>, Option<[[f32; 2]; 4]>, bool);

/// Advance the cursor animation by the elapsed frame time `dt` toward `target`
/// (the cursor's cell origin, or `None` when hidden), returning the cell for
/// the ligature break, the cursor block's four corners, and whether the
/// animation is still moving.
///
/// [`CursorAnimation::Block`] eases the single point `point` and derives a rigid
/// one-cell quad from it. [`CursorAnimation::Warp`] eases the four `corners`
/// independently so the block stretches along its path and collapses back to a
/// square as it arrives, taking the eased centroid as the ligature-break cell.
/// Only the state matching `animation` is advanced.
fn step_cursor(
    animation: CursorAnimation,
    point: &mut [f32; 2],
    corners: &mut [[f32; 2]; 4],
    target: Option<[f32; 2]>,
    dt: Duration,
) -> CursorStep {
    let Some(target) = target else {
        return (None, None, false);
    };
    match animation {
        CursorAnimation::Block => {
            let (next, settled) = ease(*point, target, dt);
            *point = next;
            (Some(next), Some(block_corners(next)), !settled)
        },
        CursorAnimation::Warp => {
            let (next, settled) = ease_corners(*corners, target, dt);
            *corners = next;
            (Some(centroid(next)), Some(next), !settled)
        },
    }
}

/// A popover's content overflow height in rows, or `None` when its content fits
/// its box.
///
/// This is how far the content can scroll. Content draws one line per `scale`
/// rows, so it occupies `lines * scale` rows; the overflow is that height beyond
/// the box, in rows, which the scroll offset is also measured in.
fn popover_overflow(overlay: &Overlay) -> Option<f32> {
    let content_rows = overlay.content.lines().count() * overlay.scale.max(1) as usize;
    let height = overlay.height as usize;
    (content_rows > height).then(|| (content_rows - height) as f32)
}

/// Advance the ping-pong popover scroll by the elapsed frame time `dt` toward
/// its current end, reversing direction when it settles.
///
/// `down` eases the offset toward `max` (the overflow bottom); once settled it
/// flips, easing back toward the top, so the content glides up and down while
/// the popover is visible.
fn step_popover_scroll(scroll: f32, down: bool, max: f32, dt: Duration) -> (f32, bool) {
    let target = if down { max } else { 0.0 };
    let (next, settled) = ease([scroll, 0.0], [target, 0.0], dt);
    let down = if settled { !down } else { down };
    (next[0], down)
}

/// Advance the grid's eased vertical scroll by the elapsed frame time `dt`.
///
/// The new `delta` (rows the content scrolled up) is added to the offset, so the
/// content starts that many rows lower, then the offset eases toward zero so it
/// glides up into place. Returns the new offset and whether it is still easing.
fn step_grid_scroll(scroll: f32, delta: usize, dt: Duration) -> (f32, bool) {
    let seeded = scroll + delta as f32;
    let (next, settled) = ease([seeded, 0.0], [0.0, 0.0], dt);
    (next[0], !settled)
}

/// Floor on the scrollback ease's per-baseline-frame step, in rows, so the
/// exponential tail locks in with a quick, even glide instead of crawling the
/// last sub-pixels into the target. A few pixels per frame at a typical cell
/// height; raise for a snappier lock-in, lower for a softer one.
const SCROLLBACK_MIN_STEP: f32 = 0.15;

/// Advance the eased scrollback position toward `target` by the elapsed frame
/// time `dt`.
///
/// `scroll` and `target` are positions in rows back from the live bottom: the
/// wheel advances `target` and this eases `scroll` toward it, so the history
/// window scrolls through each row and settles cell-aligned on the target.
///
/// Closes a fixed fraction of the remaining distance per baseline frame,
/// rescaled to `dt`, but never moves slower than [`SCROLLBACK_MIN_STEP`], so
/// the tail finishes crisply instead of crawling sub-pixel-by-sub-pixel into the
/// target. Returns the new position and whether it is still easing.
fn step_scrollback_scroll(scroll: f32, target: f32, dt: Duration) -> (f32, bool) {
    const FACTOR: f32 = 0.35;
    const EPSILON: f32 = 0.01;

    let remaining = target - scroll;
    if remaining.abs() < EPSILON {
        return (target, false);
    }

    let step = (remaining.abs() * ease_alpha(FACTOR, dt))
        .max(min_step(SCROLLBACK_MIN_STEP, dt))
        .min(remaining.abs());
    (scroll + step.copysign(remaining), true)
}

/// Advance the scroll region's eased vertical offset by the elapsed frame time
/// `dt`.
///
/// `delta` is the change in the region's declared scroll offset since the last
/// frame, signed: positive when the program scrolled the region's content down,
/// negative when up. It seeds the offset, which then eases toward zero so the
/// region's content glides into place. Returns the new offset and whether it is
/// still easing.
fn step_region_scroll(scroll: f32, delta: f32, dt: Duration) -> (f32, bool) {
    let seeded = scroll + delta;
    let (next, settled) = ease([seeded, 0.0], [0.0, 0.0], dt);
    (next[0], !settled)
}

/// Pages before a reposition target the live offset re-anchors to, so a
/// discontinuous jump lands with a one-page soft glide onto the destination
/// rather than appearing instantly. The app buffers from this many pages before
/// the target so the landing glide draws pooled content.
const REPOSITION_LAND_PAGES: f32 = 1.0;

/// Wall time the scroll target must hold steady before the pool hands its
/// region back to the live grid.
///
/// The follower catches a still-moving target every frame through the momentum
/// tail, so a bare convergence test reads as "settled" mid-glide. The live grid
/// stays frozen at its pre-scroll row until the app repaints at the true settle,
/// so handing off then snaps the view back to that stale row. Waiting for the
/// target to hold steady lets the settle repaint arrive first, so the region
/// only returns to a live grid that already matches the pool.
const HANDOFF_STABLE_TIME: Duration = Duration::from_millis(50);

/// Advance the document's eased smooth-scroll offset toward `target` by the
/// elapsed frame time `dt`.
///
/// `scroll` and `target` are app-declared absolute positions in document pages;
/// `page_rows` is the rows per page, so the snap epsilon and step floor are
/// expressed in on-screen rows rather than pages. Mirrors
/// [`step_scrollback_scroll`]: closes a fixed fraction of the remaining distance
/// per baseline frame, rescaled to `dt`, but never less than a row-sized floor,
/// capped at the remaining distance, so the tail lands exactly on the
/// (whole-row) target. A page-unit epsilon would snap a visible fraction of a
/// row when handing back to the live grid, reading as a one-line jump at the
/// end of the glide. Returns the new offset and whether it is still easing.
fn step_document_scroll(scroll: f32, target: f32, page_rows: f32, dt: Duration) -> (f32, bool) {
    const FACTOR: f32 = 0.7;
    const EPSILON_ROWS: f32 = 0.01;
    const MIN_STEP_ROWS: f32 = 0.15;

    let remaining = target - scroll;
    if (remaining * page_rows).abs() < EPSILON_ROWS {
        return (target, false);
    }

    let step = (remaining.abs() * ease_alpha(FACTOR, dt))
        .max(min_step(MIN_STEP_ROWS, dt) / page_rows)
        .min(remaining.abs());
    (scroll + step.copysign(remaining), true)
}

#[cfg(test)]
mod tests {
    use super::{
        alternate_scroll_bytes, anchored_cursor_pos, app_has_focus, bell_should_ring,
        block_corners, cell_at, copy_pool_region, cursor_in_region, ease, ease_corners, encode_key,
        font_step, modifier_bits, paste_bytes, popover_overflow, seed_settle_flight,
        selection_copy_text, sgr_button_bytes, sgr_motion_bytes, sgr_wheel_bytes, step_cursor,
        step_document_scroll, step_grid_scroll, step_popover_scroll, step_region_scroll,
        step_scrollback_scroll, swallow_super_combo, wheel_lines, CursorAnimation,
        EASE_BASELINE_FRAME, SCROLLBACK_MIN_STEP,
    };
    #[cfg(unix)]
    use super::{window_socket_path, PathBuf};
    use alacritty_terminal::sync::FairMutex;
    use std::time::{Duration, Instant};
    use stoatty_protocol::command::PoolRegionCommand;
    use stoatty_term::{
        grid::{Bar, Grid, Overlay, Rgb, TextRun},
        term::{Cursor, CursorShape, Terminal},
        theme::Theme,
    };
    use winit::{
        dpi::PhysicalPosition,
        event::MouseScrollDelta,
        keyboard::{Key, ModifiersState, NamedKey},
    };

    #[test]
    fn app_has_focus_tracks_any_window() {
        assert!(app_has_focus(true, [false, false]));
        assert!(app_has_focus(false, [false, true]));
        assert!(!app_has_focus(false, [false, false]));
        assert!(!app_has_focus(false, std::iter::empty()));
    }

    #[cfg(unix)]
    #[test]
    fn window_socket_path_is_per_pid() {
        assert_eq!(
            window_socket_path(std::path::Path::new("/run/stoat"), 42),
            PathBuf::from("/run/stoat/stoatty-win-42.sock"),
        );
    }

    #[test]
    fn super_combo_swallowed_only_on_macos() {
        // A held Command is swallowed on macOS and forwarded everywhere else.
        assert_eq!(
            swallow_super_combo(ModifiersState::SUPER),
            cfg!(target_os = "macos"),
        );
        // Ctrl (SIGINT, the Linux clipboard chord) and no modifier never are.
        assert!(!swallow_super_combo(ModifiersState::CONTROL));
        assert!(!swallow_super_combo(ModifiersState::empty()));
    }

    #[test]
    fn cursor_in_region_uses_exclusive_far_edges() {
        let region = PoolRegionCommand {
            pool: 0,
            top: 2,
            left: 3,
            width: 4,
            height: 5,
            window: 0,
        };
        let at = |col, row| {
            cursor_in_region(
                Cursor {
                    row,
                    col,
                    shape: CursorShape::Block,
                },
                region,
            )
        };
        assert!(at(3, 2), "near corner is inside");
        assert!(at(6, 6), "far interior cell is inside");
        assert!(!at(2, 2), "a column left of the region is outside");
        assert!(!at(7, 2), "the right edge is exclusive");
        assert!(!at(3, 7), "the bottom edge is exclusive");
    }

    #[test]
    fn copy_pool_region_fills_region_and_leaves_surround() {
        let region = PoolRegionCommand {
            pool: 0,
            top: 1,
            left: 1,
            width: 2,
            height: 2,
            window: 0,
        };

        // The term composes region.height + 1 rows (one straddle row) by width.
        let mut document = Grid::new(region.height as usize + 1, region.width as usize);
        for r in 0..document.rows() {
            for c in 0..document.cols() {
                document.get_mut(r, c).ch = 'd';
            }
        }

        let live = Grid::new(5, 5);

        // A sentinel in every cell distinguishes copied cells from untouched ones.
        let mut pool = Grid::new(5, 5);
        for r in 0..pool.rows() {
            for c in 0..pool.cols() {
                pool.get_mut(r, c).ch = 's';
            }
        }

        copy_pool_region(&mut pool, &document, &live, region);

        for r in 1..4 {
            for c in 1..3 {
                assert_eq!(
                    pool.get(r, c).ch,
                    'd',
                    "region cell ({r}, {c}) holds the document"
                );
            }
        }
        assert_eq!(
            pool.get(0, 0).ch,
            's',
            "the surround is left untouched, not blanked"
        );
        assert_eq!(
            pool.get(4, 4).ch,
            's',
            "the surround is left untouched, not blanked"
        );
    }

    #[test]
    fn copy_pool_region_translates_decorations_and_clears_stale() {
        let region = PoolRegionCommand {
            pool: 0,
            top: 1,
            left: 1,
            width: 2,
            height: 2,
            window: 0,
        };
        let live = Grid::new(5, 5);

        let mut document = Grid::new(region.height as usize + 1, region.width as usize);
        document.set_text_runs(vec![TextRun {
            col: 5,
            row: 3,
            scale: 160,
            color: Rgb::new(1, 2, 3),
            bg: Some(Rgb::new(4, 5, 6)),
            text: "42".into(),
            seq: 0,
        }]);
        document.set_bars(vec![Bar {
            x: 2,
            y: 4,
            width: 8,
            height: 1,
            color: Rgb::new(7, 8, 9),
            seq: 0,
        }]);

        let mut pool = Grid::new(5, 5);
        copy_pool_region(&mut pool, &document, &live, region);

        assert_eq!(
            pool.text_runs(),
            [TextRun {
                col: 21,
                row: 19,
                scale: 160,
                color: Rgb::new(1, 2, 3),
                bg: Some(Rgb::new(4, 5, 6)),
                text: "42".into(),
                seq: 0,
            }],
            "the text run shifts by the region origin (left*16, top*16)"
        );
        assert_eq!(
            pool.bars(),
            [Bar {
                x: 18,
                y: 20,
                width: 8,
                height: 1,
                color: Rgb::new(7, 8, 9),
                seq: 0,
            }],
            "the bar shifts by the region origin (left*16, top*16)"
        );

        let bare = Grid::new(region.height as usize + 1, region.width as usize);
        copy_pool_region(&mut pool, &bare, &live, region);
        assert!(
            pool.text_runs().is_empty(),
            "a decoration-free document clears stale text runs"
        );
        assert!(
            pool.bars().is_empty(),
            "a decoration-free document clears stale bars"
        );
    }

    #[test]
    fn copy_pool_region_clips_past_the_viewport() {
        let document = {
            let mut g = Grid::new(5, 4);
            for r in 0..g.rows() {
                for c in 0..g.cols() {
                    g.get_mut(r, c).ch = 'd';
                }
            }
            g
        };
        let live = Grid::new(5, 5);
        let sentinel = || {
            let mut g = Grid::new(5, 5);
            for r in 0..g.rows() {
                for c in 0..g.cols() {
                    g.get_mut(r, c).ch = 's';
                }
            }
            g
        };

        let mut pool = sentinel();
        copy_pool_region(
            &mut pool,
            &document,
            &live,
            PoolRegionCommand {
                pool: 0,
                top: 3,
                left: 3,
                width: 4,
                height: 4,
                window: 0,
            },
        );
        for r in 0..5 {
            for c in 0..5 {
                let want = if (3..5).contains(&r) && (3..5).contains(&c) {
                    'd'
                } else {
                    's'
                };
                assert_eq!(pool.get(r, c).ch, want, "clipped copy cell ({r}, {c})");
            }
        }

        let mut past = sentinel();
        copy_pool_region(
            &mut past,
            &document,
            &live,
            PoolRegionCommand {
                pool: 0,
                top: 9,
                left: 9,
                width: 4,
                height: 4,
                window: 0,
            },
        );
        for r in 0..5 {
            for c in 0..5 {
                assert_eq!(
                    past.get(r, c).ch,
                    's',
                    "region past the viewport no-op ({r}, {c})"
                );
            }
        }
    }

    #[test]
    fn anchored_cursor_rides_the_glide_and_hides_off_pane() {
        // A pool at top row 4 and 40 rows tall has eased a quarter page (10 rows)
        // down. The cursor's document row 20 draws 10 rows higher, still in pane.
        let (pos, in_region) = anchored_cursor_pos(4.0, 40.0, 20.0, 7.0, 0.25);
        assert_eq!(pos, [7.0, 14.0]);
        assert!(in_region);

        // Eased far enough, the line rides off the top edge and hides.
        let (pos, in_region) = anchored_cursor_pos(4.0, 40.0, 6.0, 7.0, 0.25);
        assert_eq!(pos, [7.0, 0.0]);
        assert!(!in_region);

        // A line below the pane's bottom edge is hidden too.
        let (_, in_region) = anchored_cursor_pos(0.0, 40.0, 45.0, 0.0, 0.0);
        assert!(!in_region);
    }

    #[test]
    fn ease_steps_toward_then_settles() {
        let (next, settled) = ease([0.0, 0.0], [4.0, 0.0], EASE_BASELINE_FRAME);
        assert!(next[0] > 0.0 && next[0] < 4.0);
        assert!(!settled);

        let (next, settled) = ease([3.999, 2.0], [4.0, 2.0], EASE_BASELINE_FRAME);
        assert_eq!(next, [4.0, 2.0]);
        assert!(settled);
    }

    /// Two half-length frames must advance an ease as far as one baseline
    /// frame, so animation speed is refresh-rate independent.
    #[test]
    fn ease_is_frame_rate_invariant() {
        let half = EASE_BASELINE_FRAME / 2;

        let (whole, _) = ease([0.0, 0.0], [4.0, 0.0], EASE_BASELINE_FRAME);
        let (halfway, _) = ease([0.0, 0.0], [4.0, 0.0], half);
        let (twice, _) = ease(halfway, [4.0, 0.0], half);

        assert!(
            (twice[0] - whole[0]).abs() < 1e-4,
            "two half frames ({}) land where one whole frame does ({})",
            twice[0],
            whole[0]
        );
        assert!(halfway[0] < whole[0], "a half frame advances less");
    }

    #[test]
    fn seed_settle_flight_clamps_an_offscreen_start_to_the_edge() {
        let rows = 40usize;

        // A cursor that slid far below the viewport starts the flight from just
        // beyond the bottom edge, not from ninety rows down.
        let mut anchored = true;
        let mut point = [5.0, 90.0];
        let mut corners = block_corners(point);
        seed_settle_flight(&mut anchored, &mut point, &mut corners, rows);
        assert_eq!(
            point,
            [5.0, 40.0],
            "off the bottom clamps to just past the edge"
        );
        assert!(!anchored, "the anchor flag is cleared");

        // Above the top clamps to just above it.
        let mut anchored = true;
        let mut point = [5.0, -20.0];
        let mut corners = block_corners(point);
        seed_settle_flight(&mut anchored, &mut point, &mut corners, rows);
        assert_eq!(
            point,
            [5.0, -1.0],
            "off the top clamps to just past the edge"
        );

        // An on-screen position is the flight origin, left untouched.
        let mut anchored = true;
        let mut point = [5.0, 12.0];
        let mut corners = block_corners(point);
        seed_settle_flight(&mut anchored, &mut point, &mut corners, rows);
        assert_eq!(
            point,
            [5.0, 12.0],
            "an on-screen start eases from where it is"
        );

        // No anchor means no seeding.
        let mut anchored = false;
        let mut point = [5.0, 90.0];
        let mut corners = block_corners(point);
        seed_settle_flight(&mut anchored, &mut point, &mut corners, rows);
        assert_eq!(
            point,
            [5.0, 90.0],
            "without an anchor the position is untouched"
        );
    }

    #[test]
    fn settle_flight_eases_from_the_edge_toward_the_landing() {
        // Seeding from off-screen then stepping must move the cursor part-way,
        // not teleport it onto the landing cell.
        let mut anchored = true;
        let mut point = [5.0, 90.0];
        let mut corners = block_corners(point);
        seed_settle_flight(&mut anchored, &mut point, &mut corners, 40);

        let landing = [5.0, 22.0];
        let (_, _, easing) = step_cursor(
            CursorAnimation::Block,
            &mut point,
            &mut corners,
            Some(landing),
            EASE_BASELINE_FRAME,
        );
        assert!(easing, "the cursor is still in flight, not settled");
        assert!(
            point[1] < 40.0 && point[1] > landing[1],
            "it advanced from the edge toward the landing: {point:?}",
        );
    }

    #[test]
    fn ease_corners_leading_edge_outruns_trailing() {
        let rest = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]];
        let (stepped, settled) = ease_corners(rest, [5.0, 0.0], EASE_BASELINE_FRAME);

        assert!(!settled, "a step toward a distant cell has not settled");

        let trailing = stepped[0][0] - rest[0][0];
        let leading = stepped[1][0] - rest[1][0];
        assert!(
            leading > trailing,
            "leading edge {leading} outruns trailing {trailing}"
        );
        assert!(
            stepped[1][0] - stepped[0][0] > 1.0,
            "the quad spans wider than one cell along the motion axis"
        );
    }

    #[test]
    fn ease_corners_snaps_onto_the_target_block() {
        let near = [[3.0, 2.0], [4.0, 2.0], [3.0, 3.0], [4.0, 2.995]];
        let (snapped, settled) = ease_corners(near, [3.0, 2.0], EASE_BASELINE_FRAME);

        assert!(settled, "within epsilon of the target reports settled");
        assert_eq!(
            snapped,
            [[3.0, 2.0], [4.0, 2.0], [3.0, 3.0], [4.0, 3.0]],
            "snaps onto the exact target block"
        );
    }

    fn popover(height: u16, scale: u8, content: &str) -> Overlay {
        Overlay {
            top: 0,
            left: 0,
            width: 4,
            height,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(0, 0, 0),
            scale,
            offset: [0, 0],
            bold: false,
            content: content.to_owned(),
        }
    }

    #[test]
    fn popover_overflow_reports_rows_past_the_box() {
        assert_eq!(
            popover_overflow(&popover(2, 1, "a\nb\nc\nd")),
            Some(2.0),
            "two lines past the box"
        );
        assert_eq!(
            popover_overflow(&popover(4, 1, "a\nb")),
            None,
            "fits the box"
        );
        assert_eq!(
            popover_overflow(&popover(2, 1, "a\nb")),
            None,
            "exactly fills the box"
        );
    }

    #[test]
    fn popover_overflow_accounts_for_content_scale() {
        // At scale 2 each line is two rows tall, so three lines span six rows
        // and overflow a four-row box by two even though the line count fits it.
        assert_eq!(
            popover_overflow(&popover(4, 2, "a\nb\nc")),
            Some(2.0),
            "scaled content overflows the box"
        );
        assert_eq!(
            popover_overflow(&popover(6, 2, "a\nb\nc")),
            None,
            "scaled content exactly fills the box"
        );
    }

    #[test]
    fn popover_scroll_ping_pongs_between_ends() {
        let (next, down) = step_popover_scroll(0.0, true, 2.0, EASE_BASELINE_FRAME);
        assert!(next > 0.0 && next < 2.0, "eases down from the top");
        assert!(down);

        let (next, down) = step_popover_scroll(1.999, true, 2.0, EASE_BASELINE_FRAME);
        assert_eq!(next, 2.0, "snaps onto the bottom");
        assert!(!down, "reverses at the bottom");

        let (next, down) = step_popover_scroll(0.001, false, 2.0, EASE_BASELINE_FRAME);
        assert_eq!(next, 0.0, "snaps onto the top");
        assert!(down, "reverses at the top");
    }

    #[test]
    fn font_step_maps_the_platform_zoom_combo() {
        assert_eq!(font_step(true, &Key::Character("=".into())), Some(1));
        assert_eq!(font_step(true, &Key::Character("-".into())), Some(-1));
        assert_eq!(
            font_step(false, &Key::Character("=".into())),
            None,
            "no platform modifier held"
        );
        assert_eq!(
            font_step(true, &Key::Character("a".into())),
            None,
            "unrelated key"
        );
        assert_eq!(
            font_step(true, &Key::Character("+".into())),
            None,
            "shifted plus no longer zooms"
        );
    }

    #[test]
    fn wheel_lines_resolves_line_and_pixel_deltas() {
        // A whole-notch LineDelta scrolls its lines directly and, being whole,
        // leaves no sub-line remainder behind.
        let mut pixels = 0.0;
        assert_eq!(
            wheel_lines(MouseScrollDelta::LineDelta(0.0, 3.0), &mut pixels, 20.0),
            3
        );
        assert_eq!(pixels, 0.0, "a whole-line delta leaves no remainder");

        // A hi-res wheel's fractional line deltas accrue instead of rounding to
        // zero. Five 0.4-line deltas over a 20px cell yield two whole lines.
        let mut pixels = 0.0;
        let frac = |y| MouseScrollDelta::LineDelta(0.0, y);
        assert_eq!(
            [
                wheel_lines(frac(0.4), &mut pixels, 20.0),
                wheel_lines(frac(0.4), &mut pixels, 20.0),
                wheel_lines(frac(0.4), &mut pixels, 20.0),
                wheel_lines(frac(0.4), &mut pixels, 20.0),
                wheel_lines(frac(0.4), &mut pixels, 20.0),
            ],
            [0, 0, 1, 0, 1],
            "fractional line deltas carry across events"
        );

        // A PixelDelta steps whole lines once a cell's worth accrues, carrying
        // the remainder so a following small delta completes the next line.
        let mut pixels = 0.0;
        let px = |y| MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, y));
        assert_eq!(
            wheel_lines(px(50.0), &mut pixels, 20.0),
            2,
            "50px over a 20px cell is two lines"
        );
        assert_eq!(pixels, 10.0, "the sub-line remainder carries over");
        assert_eq!(
            wheel_lines(px(10.0), &mut pixels, 20.0),
            1,
            "the carried 10px completes a line"
        );
        assert_eq!(pixels, 0.0);

        assert_eq!(
            wheel_lines(px(5.0), &mut pixels, 20.0),
            0,
            "below a line scrolls nothing yet"
        );
    }

    #[test]
    fn alternate_scroll_bytes_emits_one_arrow_per_line() {
        assert_eq!(
            alternate_scroll_bytes(3),
            b"\x1bOA\x1bOA\x1bOA".to_vec(),
            "scrolling up sends one up arrow per line"
        );
        assert_eq!(
            alternate_scroll_bytes(-2),
            b"\x1bOB\x1bOB".to_vec(),
            "scrolling down sends one down arrow per line"
        );
        assert_eq!(
            alternate_scroll_bytes(0),
            b"".to_vec(),
            "no lines, no bytes"
        );
    }

    #[test]
    fn sgr_wheel_bytes_reports_one_press_per_line_at_the_cell() {
        // Two lines up: button 64, one press per line, 1-based cell (3,7)->(4,8).
        assert_eq!(
            sgr_wheel_bytes(2, 3, 7),
            b"\x1b[<64;4;8M\x1b[<64;4;8M".to_vec(),
            "wheel up reports button 64 once per line"
        );
        assert_eq!(
            sgr_wheel_bytes(-1, 0, 0),
            b"\x1b[<65;1;1M".to_vec(),
            "wheel down at the origin cell reports button 65"
        );
    }

    #[test]
    fn sgr_button_bytes_reports_press_and_release_at_the_cell() {
        // Left button (0) at 1-based cell (3,7)->(4,8): M on press, m on release.
        assert_eq!(
            sgr_button_bytes(0, true, 3, 7),
            b"\x1b[<0;4;8M".to_vec(),
            "press reports the button with a trailing M"
        );
        assert_eq!(
            sgr_button_bytes(0, false, 3, 7),
            b"\x1b[<0;4;8m".to_vec(),
            "release reports the same button with a trailing m"
        );
    }

    #[test]
    fn sgr_motion_bytes_encodes_button_and_motion_flag() {
        assert_eq!(
            sgr_motion_bytes(Some(0), 3, 7),
            b"\x1b[<32;4;8M".to_vec(),
            "held left button (0) drags as code 0+32=32 at 1-based (4,8)"
        );
        assert_eq!(
            sgr_motion_bytes(None, 0, 0),
            b"\x1b[<35;1;1M".to_vec(),
            "buttonless any-motion is the no-button code 3+32=35 at the origin"
        );
    }

    #[test]
    fn modifier_bits_packs_each_modifier() {
        assert_eq!(modifier_bits(ModifiersState::empty()), 0);
        assert_eq!(modifier_bits(ModifiersState::SHIFT), 0x1);
        assert_eq!(modifier_bits(ModifiersState::CONTROL), 0x2);
        assert_eq!(modifier_bits(ModifiersState::ALT), 0x4);
        assert_eq!(modifier_bits(ModifiersState::SUPER), 0x8);
        assert_eq!(
            modifier_bits(ModifiersState::SHIFT | ModifiersState::SUPER),
            0x9
        );
    }

    #[test]
    fn cell_at_maps_pixels_to_a_clamped_cell() {
        let cell = [10.0, 20.0];
        assert_eq!(
            cell_at(25.0, 50.0, cell, 5, 8),
            (2, 2),
            "x 25/10 is col 2, y 50/20 is row 2"
        );
        assert_eq!(
            cell_at(1000.0, 1000.0, cell, 5, 8),
            (7, 4),
            "a pointer past the grid clamps to the last cell"
        );
        assert_eq!(
            cell_at(-5.0, -5.0, cell, 5, 8),
            (0, 0),
            "a negative position saturates to the origin"
        );
    }

    #[test]
    fn paste_bytes_wraps_in_bracketed_guards() {
        assert_eq!(paste_bytes("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
    }

    #[test]
    fn paste_bytes_strips_embedded_end_guard() {
        assert_eq!(
            paste_bytes("a\x1b[201~b", true),
            b"\x1b[200~ab\x1b[201~".to_vec(),
            "an embedded end-guard cannot break out of the bracket"
        );
    }

    #[test]
    fn paste_bytes_normalizes_newlines_when_unbracketed() {
        assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc".to_vec());
    }

    #[test]
    fn selection_copy_text_reads_without_clearing() {
        let terminal = FairMutex::new(Terminal::new(4, 8, Theme::default()));

        assert_eq!(
            selection_copy_text(&terminal),
            None,
            "no selection yields nothing to copy"
        );

        {
            let mut t = terminal.lock();
            t.advance(b"hello");
            t.start_selection(0, 0, false);
            t.update_selection(0, 4, true);
        }

        assert_eq!(selection_copy_text(&terminal).as_deref(), Some("hello"));
        assert_eq!(
            selection_copy_text(&terminal).as_deref(),
            Some("hello"),
            "reading the selection for a copy leaves it intact for a re-copy"
        );

        terminal.lock().clear_selection();
        assert_eq!(
            selection_copy_text(&terminal),
            None,
            "the supersession path clears the highlight"
        );
    }

    #[test]
    fn bell_rate_limits_a_burst() {
        let t0 = Instant::now();
        assert!(bell_should_ring(None, t0), "the first bell rings");
        assert!(
            !bell_should_ring(Some(t0), t0 + Duration::from_millis(199)),
            "a bell within the interval is suppressed"
        );
        assert!(
            bell_should_ring(Some(t0), t0 + Duration::from_millis(200)),
            "a bell at the interval boundary rings again"
        );
    }

    #[test]
    fn encode_key_maps_keys_to_terminal_bytes() {
        let named = |key| encode_key(&Key::Named(key), false, false);
        let printable = |s: &str| encode_key(&Key::Character(s.into()), false, false);

        assert_eq!(
            printable("a"),
            Some(b"a".to_vec()),
            "printable passes through"
        );
        assert_eq!(
            printable("A"),
            Some(b"A".to_vec()),
            "shifted char passes through"
        );

        assert_eq!(named(NamedKey::Enter), Some(vec![b'\r']));
        assert_eq!(named(NamedKey::Backspace), Some(vec![0x7f]));
        assert_eq!(named(NamedKey::Tab), Some(vec![b'\t']));
        assert_eq!(named(NamedKey::Space), Some(vec![b' ']));
        assert_eq!(named(NamedKey::Escape), Some(vec![0x1b]));

        assert_eq!(named(NamedKey::ArrowUp), Some(b"\x1b[A".to_vec()));
        assert_eq!(named(NamedKey::ArrowDown), Some(b"\x1b[B".to_vec()));
        assert_eq!(named(NamedKey::ArrowRight), Some(b"\x1b[C".to_vec()));
        assert_eq!(named(NamedKey::ArrowLeft), Some(b"\x1b[D".to_vec()));

        assert_eq!(named(NamedKey::F1), None, "unmapped named key");
    }

    #[test]
    fn encode_key_maps_ctrl_letters_to_control_bytes() {
        let ctrl = |s: &str| encode_key(&Key::Character(s.into()), true, false);

        assert_eq!(ctrl("c"), Some(vec![0x03]), "Ctrl-C");
        assert_eq!(ctrl("a"), Some(vec![0x01]), "Ctrl-A");
        assert_eq!(ctrl("C"), Some(vec![0x03]), "folds case");
        assert_eq!(
            ctrl("1"),
            None,
            "Ctrl with a non-letter has no control byte"
        );
    }

    #[test]
    fn encode_key_shift_tab_sends_csi_z() {
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Tab), false, true),
            Some(b"\x1b[Z".to_vec()),
            "Shift-Tab sends CSI Z so stoat decodes BackTab"
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Tab), false, false),
            Some(vec![b'\t']),
            "plain Tab still sends a tab"
        );
    }

    #[test]
    fn grid_scroll_eases_a_delta_to_zero() {
        // A new delta seeds the offset and starts easing down toward zero.
        let (next, easing) = step_grid_scroll(0.0, 3, EASE_BASELINE_FRAME);
        assert!(next > 0.0 && next < 3.0, "eases from the seed");
        assert!(easing);

        // No new delta, within the snap epsilon: settles at zero.
        let (next, easing) = step_grid_scroll(0.005, 0, EASE_BASELINE_FRAME);
        assert_eq!(next, 0.0, "snaps onto zero");
        assert!(!easing);
    }

    #[test]
    fn scrollback_scroll_eases_toward_a_target() {
        // A target deeper in history eases toward it without overshooting.
        let (next, easing) = step_scrollback_scroll(0.0, 4.0, EASE_BASELINE_FRAME);
        assert!(next > 0.0 && next < 4.0, "eases toward the target");
        assert!(easing);

        // Within the snap epsilon of the target: settles on it.
        let (next, easing) = step_scrollback_scroll(3.999, 4.0, EASE_BASELINE_FRAME);
        assert_eq!(next, 4.0, "snaps onto the target");
        assert!(!easing);

        // Near the target the per-frame step is floored so the tail does not
        // crawl: from twice the floor out it advances by the floor itself, not
        // the smaller geometric step.
        let (next, easing) =
            step_scrollback_scroll(0.0, SCROLLBACK_MIN_STEP * 2.0, EASE_BASELINE_FRAME);
        assert!(
            (next - SCROLLBACK_MIN_STEP).abs() < 1e-5,
            "tail advances by the floor"
        );
        assert!(easing);
    }

    #[test]
    fn region_scroll_eases_a_signed_delta_to_zero() {
        // A positive delta (content scrolled down) seeds and eases toward zero.
        let (next, easing) = step_region_scroll(0.0, 3.0, EASE_BASELINE_FRAME);
        assert!(next > 0.0 && next < 3.0, "eases from the positive seed");
        assert!(easing);

        // A negative delta (content scrolled up) eases up from below zero.
        let (next, easing) = step_region_scroll(0.0, -3.0, EASE_BASELINE_FRAME);
        assert!(next < 0.0 && next > -3.0, "eases from the negative seed");
        assert!(easing);

        // No new delta, within the snap epsilon: settles at zero.
        let (next, easing) = step_region_scroll(0.005, 0.0, EASE_BASELINE_FRAME);
        assert_eq!(next, 0.0, "snaps onto zero");
        assert!(!easing);
    }

    #[test]
    fn document_scroll_eases_toward_a_target() {
        // A target ahead of the live offset eases toward it without overshooting.
        let (next, easing) = step_document_scroll(0.0, 4.0, 20.0, EASE_BASELINE_FRAME);
        assert!(next > 0.0 && next < 4.0, "eases toward the target");
        assert!(easing);

        // The row-sized min-step floor, capped at the remaining distance, lands
        // exactly on the whole-row target instead of snapping a visible fraction
        // of a row; the next frame then settles cleanly.
        let (next, easing) = step_document_scroll(4.0 - 0.001, 4.0, 20.0, EASE_BASELINE_FRAME);
        assert_eq!(next, 4.0, "min-step lands exactly on the target");
        assert!(easing);

        // Already within a sub-pixel (in rows) of the target: settles on it.
        let (next, easing) = step_document_scroll(4.0 - 0.0001, 4.0, 20.0, EASE_BASELINE_FRAME);
        assert_eq!(next, 4.0, "snaps onto the target");
        assert!(!easing);
    }
}
