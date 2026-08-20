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
    anim::{
        advance_pool_glide, anchored_cursor_pos, anchored_shift, block_corners, compose_gate,
        cursor_in_region, cursor_position, intersect_scissor, refresh_popover_overflows,
        region_scissor, seed_settle_flight, shift_scissor, step_cursor, step_grid_scroll,
        step_popover_scroll, step_region_scroll, step_scrollback_scroll, ActivePool, AnchorRide,
        AnchoredCursor, PoolAnim, PoolStep, EASE_BASELINE_FRAME, MAX_EASE_DT,
    },
    config::{self, Config, CursorAnimation},
    input::{
        alternate_scroll_bytes, cell_at, chord_char, encode_key, font_step, ipc_button,
        modifier_bits, paste_bytes, sgr_button_bytes, sgr_modifier_bits, sgr_motion_bytes,
        sgr_wheel_bytes, stepped_font_size, swallow_super_combo, wheel_lines,
    },
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
    command::WindowOpenCommand,
    window_ipc::{MouseKind, WindowIpcEvent},
};
use stoatty_render::{
    gpu::{
        AnchoredPanel, FontConfig, FontLoad, Frame, GpuContext, PoolComposite, Scroll, SharedFonts,
        SharedGpu,
    },
    render,
};
use stoatty_term::{
    grid::{Grid, Rgb},
    term::{Damage, PoolView, TermEvent, Terminal},
    theme::Theme,
    NON_PANE_POOL_BASE,
};
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState},
    window::{UserAttentionType, Window, WindowAttributes, WindowId},
};

/// Window title shown before a program sets one, and restored when a program
/// resets it via OSC.
const DEFAULT_TITLE: &str = "stoatty";

/// Lines of terminal-owned scrollback the wheel moves per line of wheel travel,
/// the idiomatic multi-line wheel step common terminals use (e.g. alacritty's
/// default scroll multiplier of 3). Applies only to local scrollback, never to
/// the wheel reports forwarded to a mouse-reporting app.
const SCROLLBACK_SCROLL_MULTIPLIER: i32 = 3;

/// Name every window after the application, so a desktop environment can match
/// it to `stoatty.desktop` and paint the icon it names.
///
/// The name is set here rather than left to winit's default, which is argv[0]
/// and so reports whatever path launched the binary. Plasma on Wayland matches
/// by exact app id, and no path matches.
#[cfg(target_os = "linux")]
fn with_app_name(attributes: WindowAttributes) -> WindowAttributes {
    // The trait is named for Wayland, but the general name it writes reaches
    // both display servers: Wayland sends it as the app id, and X11 as the
    // WM_CLASS class the desktop entry's StartupWMClass matches.
    attributes.with_name(DEFAULT_TITLE, DEFAULT_TITLE)
}

#[cfg(not(target_os = "linux"))]
fn with_app_name(attributes: WindowAttributes) -> WindowAttributes {
    attributes
}

/// Bytes of the child's most recent output retained for the exit diagnostic
/// logged when the pty closes, enough to carry a startup error line without
/// holding the whole session's scrollback.
const CHILD_OUTPUT_TAIL_CAP: usize = 2048;

/// Aux OS windows one session may hold open at once.
///
/// Each one costs a real window and a GPU-context build thread, and the count
/// is driven entirely by wire frames a writer chooses to send. One 64KB APC
/// chunk carries roughly two thousand `window_open` frames, so without a
/// ceiling a single crafted file catted to the terminal buries the desktop.
/// Detached panes are a handful in practice, so the limit costs a real session
/// nothing.
const MAX_AUX_WINDOWS: usize = 8;

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
    // Ahead of the config read, so the system-font scan overlaps that too.
    let font_load = FontLoad::spawn();
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
        font_load,
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
    let font_load = FontLoad::spawn();
    run_with_config(
        start,
        font_load,
        load_config(),
        program,
        args,
        size,
        None,
        None,
    );
}

/// Open the window running `program` with `args`, drawing with `config`'s theme
/// and font, and run the event loop until the window closes.
///
/// The shared core of [`run`] and [`run_with_shell`]. It takes an
/// already-loaded `config` so each entry point loads it exactly once, and an
/// already-running `font_load` so the system-font scan overlaps that read as
/// well as the event loop, the window, and the GPU setup after it.
#[allow(clippy::too_many_arguments)]
fn run_with_config(
    start: Instant,
    font_load: FontLoad,
    config: Config,
    program: String,
    args: Vec<String>,
    size: Option<[u16; 2]>,
    working_directory: Option<PathBuf>,
    stoat_dir: Option<PathBuf>,
) {
    let theme = config.resolve_theme();

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
    /// The system-font scan started before the config read, taken in `resumed`
    /// to hand to the renderer. `None` after the window is built.
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
    /// Child output events that arrived before the window existed.
    ///
    /// The child is spawned ahead of GPU setup so its own startup overlaps
    /// adapter, device, and pipeline creation. That leaves a window of tens to
    /// hundreds of milliseconds where it is already writing and [`Self::state`]
    /// is still `None`. Dropping what arrives there would lose a reply the child
    /// blocks on, so it is held and replayed once the window is up.
    ///
    /// Bounded by what a child emits in that window, and the reader parses
    /// inline with the child's writes, so a chatty child throttles itself.
    pending_events: Vec<PtyEvent>,
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
            pending_events: Vec::new(),
        }
    }
}

/// Whether a window is on screen, and whether it owes a redraw it could not
/// draw while it was not.
///
/// An occluded window puts nothing in front of anyone, so projecting the grid
/// and submitting a frame for it is work with no result. Skipping the frame also
/// ends the animation self-request chain, since a redraw that never runs never
/// asks for the next one, so a hidden window with a blinking cursor goes quiet
/// rather than easing against nothing.
///
/// Defaults to visible, since some compositors never report occlusion at all.
/// One that stays silent leaves a window drawing exactly as it would without
/// this, which is the right way to be wrong.
#[derive(Debug, Default, PartialEq, Eq)]
struct Visibility {
    occluded: bool,
    /// A redraw arrived while occluded and still has to happen. Held rather than
    /// dropped so the window is current the moment it comes back.
    owed: bool,
}

impl Visibility {
    /// Whether a redraw may draw now, recording it as owed when it may not.
    fn admit(&mut self) -> bool {
        if self.occluded {
            self.owed = true;
            return false;
        }
        true
    }

    /// Record an occlusion change, reporting whether a redraw is now owed.
    ///
    /// True only on becoming visible with one held, so the caller requests a
    /// frame exactly when there is something to catch up on.
    fn set_occluded(&mut self, occluded: bool) -> bool {
        self.occluded = occluded;
        !occluded && mem::take(&mut self.owed)
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
/// The size a batch of window events settled on, for a caller that fits the
/// surface once rather than per event.
///
/// A drag delivers several `Resized` per frame, and some window managers repeat
/// the current size on a focus change or a move. The chain each one would run
/// reallocates the swapchain, re-derives the cell grid, and tells the child its
/// new size, so it runs on the last size of the batch instead of every size in
/// it.
#[derive(Default)]
struct PendingResize(Option<(u32, u32)>);

impl PendingResize {
    /// Note a size the window reported, displacing any earlier one.
    fn record(&mut self, width: u32, height: u32) {
        self.0 = Some((width, height));
    }

    /// The size to fit to, leaving nothing behind for the next batch.
    fn take(&mut self) -> Option<(u32, u32)> {
        self.0.take()
    }
}

struct AuxWindow {
    id: u32,
    window: Arc<Window>,
    gpu: Option<GpuContext>,
    /// Size this window's surface has yet to be fitted to. See
    /// [`PendingResize`].
    pending_resize: PendingResize,
    /// The window's composed content grid, rendered whole each redraw.
    grid: Grid,
    /// Scratch that [`Terminal::project_pool`] composes one pool into before its
    /// rows are blitted into `grid` at the pool's window-relative region.
    scratch: Grid,
    /// Whether this window holds OS focus, tracked from `WindowEvent::Focused`.
    /// Feeds the app-wide DECSET 1004 report so a switch between stoatty windows
    /// keeps the app focused.
    focused: bool,
    /// Whether this window is on screen, tracked from `WindowEvent::Occluded`.
    visibility: Visibility,
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
    /// Hash of where the last base compose put things: the pool set, their
    /// regions, and the window size. A move here changes which cells any pool
    /// covers at all, so the grid goes back to the window background and every
    /// row is suspect. `None` forces the first frame to compose.
    last_geometry: Option<u64>,
    /// Hash of what the last base compose held: each pool's content version and,
    /// for the ones no overlay covered, its scroll target. A move here leaves
    /// every rectangle where it was, so the compose overwrites in place and
    /// damages only the rows that came back different.
    ///
    /// A redraw where neither hash moved skips the recompose and reuses last
    /// frame's instances. The sub-cell glide overlay still animates.
    last_content: Option<u64>,
    /// Reused buffer for the window's pool snapshots, so a redraw allocates no
    /// fresh Vec to gather them.
    pool_scratch: Vec<PoolView>,
    /// The background this window's GPU clear was last set to, mirroring
    /// [`State::last_clear_bg`] so each window follows the override on its own.
    last_clear_bg: Option<Rgb>,
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
    /// The GPU handles and font database the primary window's context was built
    /// on, for the aux windows to build on rather than each asking the driver
    /// and the font directories the same questions again.
    ///
    /// `None` until the primary context lands, which is before any aux window
    /// can be opened, since opening one is driven by the child over the window
    /// socket the primary serves.
    shared_gpu: Option<(SharedGpu, SharedFonts)>,
    /// The title last pushed to [`Self::window`], so a repeated one costs no platform
    /// write. `None` until the first title lands, which leaves the window showing the
    /// title it was created with.
    last_title: Option<String>,
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
    /// Size the primary surface has yet to be fitted to. See [`PendingResize`].
    pending_resize: PendingResize,
    /// The window's display scale factor (physical pixels per logical point),
    /// tracked from `ScaleFactorChanged` so the cell metrics re-derive when the
    /// window moves to a display of a different density.
    scale_factor: f64,
    /// The most recent modifier state, tracked from `ModifiersChanged` so a key
    /// press can tell whether the platform zoom modifier is held.
    modifiers: ModifiersState,
    /// Whether the child has claimed the platform zoom combo for its session, so
    /// each press is forwarded upstream instead of stepping the font size.
    ///
    /// False until a child asks for it, which is what leaves a plain shell child
    /// with instant native font zoom. A child that claims it and then wedges
    /// keeps the claim, since releasing on silence would make the combo flicker
    /// between meanings whenever the child was merely busy.
    zoom_capture: bool,
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
    /// The background the GPU clear was last set to, so the setter runs only
    /// when the terminal's default background actually moves. `None` until the
    /// first frame reads it.
    last_clear_bg: Option<Rgb>,
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
    rides_scratch: Vec<AnchorRide>,
    overflows_scratch: Vec<Option<f32>>,
    /// Row-flag buffers from the frame before, handed back to the terminal at
    /// the start of the next one so its damage flags land in the same
    /// allocation rather than a fresh one.
    ///
    /// Held here rather than in the terminal because the frame owns them once
    /// it is handed them, and reads them after the lock is released.
    damage_spares: Vec<Damage>,
    /// The grid popovers epoch [`Self::overflows_scratch`] was filled against, so a
    /// frame whose overlays were not re-applied keeps the answers already in it.
    /// `None` until the first redraw fills it.
    last_popovers_epoch: Option<u64>,
    /// Live aux OS windows, each hosting a detached pane's window-bound pools.
    /// Empty until a [`TermEvent::WindowOpen`] opens the first one.
    aux: Vec<AuxWindow>,
    /// Whether a refused `window_open` has already been reported.
    ///
    /// One line per session rather than per frame, since the writer that sends
    /// a refused open is the one liable to send thousands, and a log entry each
    /// would be its own way to bring the session down.
    warned_window_open_refused: bool,
    /// Channel to the thread serving the window-event socket, carrying encoded
    /// [`WindowIpcEvent`] lines to forward to the connected child. `None` when
    /// the socket could not be bound, in which case aux windows still render but
    /// report nothing upstream.
    window_event_tx: Option<Sender<String>>,
    /// Whether a child is connected to the window-event socket and reading it.
    ///
    /// A zoom press is only forwarded when one is. The socket being bound says
    /// nothing about that. Over ssh the child never sees the path at all, and a
    /// child that exits leaves the socket bound behind it. Either way the press
    /// would queue for a reader that is not there, so the combo goes back to
    /// stepping the font.
    window_client_connected: Arc<AtomicBool>,
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
    /// Whether the main window is on screen, tracked from
    /// `WindowEvent::Occluded`.
    visibility: Visibility,
    /// Whether the perf HUD overlay is shown, toggled by the platform modifier
    /// plus Shift+P. Drives both the HUD composite and the redraw keep-alive.
    #[cfg(feature = "perf")]
    show_perf_hud: bool,
}

impl ApplicationHandler<PtyEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }

        // Take the font enumeration started before the config read, which has
        // been running on a background thread through the config, the event
        // loop, and the window build. A second resume finds it already taken
        // and starts a fresh scan rather than panicking.
        let font_load = self.font_load.take().unwrap_or_else(FontLoad::spawn);

        let mut attributes = with_app_name(Window::default_attributes().with_title(DEFAULT_TITLE));
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

        // The grid the surface will hold, resolved before the GPU exists so the
        // child can start while adapter, device, and pipelines are still coming
        // up. The cell rectangle depends on the font size and the scale factor
        // alone, so this is the answer `gpu.grid_size()` gives later rather than
        // a guess at it, and both go through one function to keep it that way.
        let (spawn_rows, spawn_cols) = render::grid_size(
            size.width.max(1),
            size.height.max(1),
            self.font_size,
            scale_factor as f32,
        );
        let terminal = Arc::new(FairMutex::new(Terminal::new(
            spawn_rows, spawn_cols, self.theme,
        )));
        update_cell_pixels(&terminal, self.font_size, scale_factor as f32);
        if let Some(ident) = stoat_log::ident::get() {
            terminal
                .lock()
                .set_ident(stoatty_protocol::command::IdentReply {
                    pid: std::process::id(),
                    log_id: ident.id.to_string(),
                    hostname: stoat_log::ident::hostname(),
                    version: crate::cli::VERSION_INFO.to_string(),
                    protocol: stoatty_protocol::PROTOCOL_VERSION,
                });
        }
        let dirty = Arc::new(AtomicBool::new(false));
        let sync_pending = Arc::new(AtomicBool::new(false));

        // Bind the socket aux windows report focus, resize, and close over, and
        // export its path so the child editor can connect. A bind failure is
        // non-fatal. Aux windows still render, they just report nothing upstream.
        let (window_socket, window_event_tx, window_client_connected) = open_window_event_socket();

        let (pixel_width, pixel_height) =
            grid_pixels(self.font_size, scale_factor as f32, spawn_rows, spawn_cols);
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
                spawn_rows as u16,
                spawn_cols as u16,
                pixel_width,
                pixel_height,
                move |output| match output {
                    PtyOutput::Data { bytes, may_refuse } => {
                        // Parse on the reader thread under the shared lock. The
                        // main thread holds it to render, and waiting here would
                        // stop the reader draining the pty, which stops the
                        // child. Better to leave the bytes with the reader and
                        // take them once the frame is out.
                        let (redraw, responses, events) = {
                            let Some(mut terminal) = (if may_refuse {
                                terminal.try_lock_unfair()
                            } else {
                                Some(terminal.lock())
                            }) else {
                                return false;
                            };

                            // Recorded once the bytes are ours, since refused
                            // ones come back and would otherwise land twice.
                            pty::push_tail(&mut tail, bytes, CHILD_OUTPUT_TAIL_CAP);

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
                        true
                    },
                    PtyOutput::Eof => {
                        tracing::info!("child closed the pty");
                        let last_output = pty::strip_escapes(&String::from_utf8_lossy(&tail));
                        let _ = proxy.send_event(PtyEvent::Exited { last_output });
                        true
                    },
                },
            )
            .expect("spawn shell over pty")
        };
        let pty_time = t_pty.elapsed();

        let t_gpu = Instant::now();
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
        let gpu_time = t_gpu.elapsed();

        // The surface a platform grants can differ from the size asked for, and
        // the child is already running against the size that was. A startup
        // SIGWINCH is routine for a child, so correcting here costs less than
        // waiting for the GPU to answer first.
        let (rows, cols) = gpu.grid_size();
        let grid = Grid::new(rows, cols);
        if (rows, cols) != (spawn_rows, spawn_cols) {
            let (pixel_width, pixel_height) =
                grid_pixels(self.font_size, scale_factor as f32, rows, cols);
            terminal.lock().resize(rows, cols);
            let _ = pty.resize(rows as u16, cols as u16, pixel_width, pixel_height);
            tracing::info!(
                spawned = ?(spawn_rows, spawn_cols),
                surface = ?(rows, cols),
                "the surface differed from the pre-GPU grid",
            );
        }

        tracing::info!(
            window = ?window_time,
            pty = ?pty_time,
            gpu = ?gpu_time,
            "window, pty, and gpu ready",
        );

        window.request_redraw();
        let state = State {
            window,
            shared_gpu: Some((gpu.shared(), gpu.fonts())),
            pending_resize: PendingResize::default(),
            last_title: None,
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
            zoom_capture: false,
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
            last_clear_bg: None,
            region_scroll: 0.0,
            last_region_offset: 0.0,
            pool_anims: BTreeMap::new(),
            damage_spares: Vec::new(),
            pools_scratch: Vec::new(),
            active_scratch: Vec::new(),
            rides_scratch: Vec::new(),
            overflows_scratch: Vec::new(),
            last_popovers_epoch: None,
            aux: Vec::new(),
            warned_window_open_refused: false,
            window_event_tx,
            window_client_connected,
            wheel_pixels: 0.0,
            pointer_cell: (0, 0),
            pressed_button: None,
            pointer_side_right: false,
            native_drag: false,
            clipboard: None,
            last_redraw: None,
            visibility: Visibility::default(),
            #[cfg(feature = "perf")]
            show_perf_hud: false,
        };
        self.state = Some(state);

        // Whatever the child said while the GPU was coming up. Replaying it
        // through the same arm that would have taken it live keeps one handling
        // of each event kind, and an early query gets its reply.
        for event in mem::take(&mut self.pending_events) {
            self.user_event(event_loop, event);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: PtyEvent) {
        let Some(state) = self.state.as_mut() else {
            self.pending_events.push(event);
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
            PtyEvent::Term(mut events) => {
                // The reload runs before the rest, so an aux window opened in
                // the same batch is built against the new config rather than
                // the one being replaced.
                if events.contains(&TermEvent::ConfigReload) {
                    events.retain(|event| *event != TermEvent::ConfigReload);
                    apply_config_reload(
                        state,
                        &mut self.theme,
                        &mut self.theme_name,
                        &mut self.font_family,
                        &mut self.ligatures,
                        &mut self.cursor_animation,
                    );
                }
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
            },
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
        apply_pending_resizes(state);

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
                    let mut report = None;
                    if let Some(aux) = state.aux.iter_mut().find(|aux| aux.window.id() == id) {
                        if !aux.visibility.admit() {
                            return;
                        }
                        // Same ordering as the primary window's frame: the
                        // redraw a resize asked for arrives before the batch
                        // ends, so it fits itself first.
                        report = fit_aux(aux).map(|(cols, rows)| (aux.id, cols, rows));
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
                    // Sent out here, since the socket lives on the whole state
                    // and the fit above borrows one window out of it.
                    if let Some((window, cols, rows)) = report {
                        send_window_event(state, WindowIpcEvent::Resized { window, cols, rows });
                    }
                    return;
                },
                WindowEvent::Resized(size) => {
                    let size = *size;
                    if let Some(aux) = state.aux.iter_mut().find(|aux| aux.window.id() == id) {
                        aux.pending_resize.record(size.width, size.height);
                        aux.window.request_redraw();
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
                WindowEvent::Occluded(hidden) => {
                    let hidden = *hidden;
                    if let Some(aux) = state.aux.iter_mut().find(|aux| aux.window.id() == id)
                        && aux.visibility.set_occluded(hidden)
                    {
                        // The ease step is the gap since the last frame, and the
                        // gap across a hiding is not one anyone watched, so the
                        // resumed frame starts its clock fresh.
                        aux.last_redraw = None;
                        aux.window.request_redraw();
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
                            let cell = (col as u16, row as u16);
                            let event =
                                aux_drag_event(aux.id, aux.pointer_cell, cell, aux.pressed, mods);
                            // Recorded wherever the pointer is, not only where a
                            // drag was reported, since the wheel and button arms
                            // read it for the cell under the pointer.
                            aux.pointer_cell = cell;
                            event
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
            WindowEvent::Occluded(hidden) => {
                if state.visibility.set_occluded(hidden) {
                    // The ease step is the gap since the last frame, and the gap
                    // across a hiding is not one anyone watched, so the resumed
                    // frame starts its clock fresh.
                    state.last_redraw = None;
                    state.window.request_redraw();
                }
            },
            WindowEvent::Resized(size) => {
                state.pending_resize.record(size.width, size.height);
                state.window.request_redraw();
            },
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.scale_factor = scale_factor;
                state
                    .gpu
                    .set_font_size(state.font_size, scale_factor as f32);
                update_cell_pixels(&state.terminal, state.font_size, scale_factor as f32);

                // The cell metrics moved with the new density, so the grid has
                // to be re-derived even though the surface has not changed
                // size. Recording the size it already has does that, and costs
                // nothing at the surface, which refuses a size it is at.
                let size = state.window.inner_size();
                state.pending_resize.record(size.width, size.height);
                state.window.request_redraw();
            },
            WindowEvent::RedrawRequested => redraw(state),
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
                    if forwards_zoom(
                        state.zoom_capture,
                        state.window_client_connected.load(Ordering::Relaxed),
                    ) {
                        send_window_event(state, WindowIpcEvent::Zoom { window: 0, delta });
                    } else {
                        apply_font_step(state, delta);
                    }
                    return;
                }

                // A digit chord has no terminal encoding, so a program can only
                // hear it over the socket. Without the claim it falls through to
                // encode_key, which yields nothing for it, exactly as before.
                if primary
                    && let Some(ch) = chord_char(platform_mod_held, &event.logical_key)
                    && forwards_zoom(
                        state.zoom_capture,
                        state.window_client_connected.load(Ordering::Relaxed),
                    )
                {
                    send_window_event(state, WindowIpcEvent::Chord { window: 0, ch });
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
                    // A failed read leaves the handle cached, unlike a failed
                    // write. X11 reports an error for a clipboard that is empty
                    // or holds something other than text, which says nothing
                    // about the handle, and reopening per paste from an empty
                    // clipboard is the cost this avoids.
                    let pasted = clipboard_handle(state).and_then(|cb| cb.get_text().ok());

                    // Consume the combo whether or not the clipboard read
                    // succeeds, so encode_key never sends a stray "v".
                    if let Some(text) = pasted {
                        Input {
                            terminal: &state.terminal,
                            pty: &mut state.pty,
                        }
                        .paste(&text);
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
                    Input {
                        terminal: &state.terminal,
                        pty: &mut state.pty,
                    }
                    .key(&bytes);
                }
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let cell_height =
                    render::cell_size(state.font_size, state.scale_factor as f32)[1] as f64;
                let lines = wheel_lines(delta, &mut state.wheel_pixels, cell_height);
                if lines != 0 {
                    let moved = Input {
                        terminal: &state.terminal,
                        pty: &mut state.pty,
                    }
                    .wheel(
                        lines,
                        state.pointer_cell,
                        state.modifiers.shift_key(),
                        sgr_modifier_bits(state.modifiers),
                    );

                    // A notch the scrollback took is one this window has to
                    // repaint for. One the shell took redraws on its answer.
                    if let Some(moved) = moved {
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

                Input {
                    terminal: &state.terminal,
                    pty: &mut state.pty,
                }
                .motion(state.pointer_cell, state.pressed_button);
            },
            WindowEvent::MouseInput {
                state: element_state,
                button,
                ..
            } => {
                // The side buttons ride the window socket rather than the pty.
                // Their xterm encoding sets the 128 bit, which the child's
                // parser rejects outright, so no in-band report could reach it.
                if matches!(button, MouseButton::Back | MouseButton::Forward)
                    && let Some(ipc) = ipc_button(button)
                {
                    if element_state == ElementState::Pressed {
                        let (col, row) = state.pointer_cell;
                        send_window_event(
                            state,
                            WindowIpcEvent::Mouse {
                                window: 0,
                                kind: MouseKind::Press(ipc),
                                col: col as u16,
                                row: row as u16,
                                mods: modifier_bits(state.modifiers),
                            },
                        );
                    }
                    return;
                }

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

/// The text area's pixel extent, which the pty reports so an image client can
/// size what it draws.
///
/// The grid rather than the window. A winsize's pixel fields describe the text
/// area, and the window carries padding and chrome that no cell occupies.
///
/// A grid too wide for the field saturates rather than wrapping, since the cast
/// does that on its own. No display reaches such a grid, but reporting a tiny
/// area for an enormous one would be the worst answer available.
fn grid_pixels(font_size: u32, scale_factor: f32, rows: usize, cols: usize) -> (u16, u16) {
    let [width, height] = render::cell_size(font_size, scale_factor);
    let extent = |cell: f32, count: usize| (cell * count as f32).round() as u16;
    (extent(width, cols), extent(height, rows))
}

/// Whether a zoom-combo press goes to the child instead of stepping the font.
///
/// The claim alone is not enough. A press with no reader on the other end
/// queues for a client that may never arrive and simply vanishes, so a claimed
/// combo with nothing upstream falls back to font zoom rather than doing
/// nothing.
///
/// `client_connected` rather than the socket being bound, because those differ
/// exactly where this goes wrong. A child reached over ssh never sees the
/// socket path, and one that exits leaves the socket behind it.
fn forwards_zoom(zoom_capture: bool, client_connected: bool) -> bool {
    zoom_capture && client_connected
}

/// Step the terminal's font size by `delta` and re-fit everything measured in
/// cells.
///
/// Shared by the zoom combo and a child's `font_step` request, so both paths
/// land the same metrics. The surface itself does not change, only the cell
/// size, so the grid is re-read and the terminal and pty resized without a
/// `gpu.resize`.
fn apply_font_step(state: &mut State, delta: i32) {
    let font_size = stepped_font_size(state.font_size, delta);
    state.font_size = font_size;
    state
        .gpu
        .set_font_size(font_size, state.scale_factor as f32);
    update_cell_pixels(&state.terminal, font_size, state.scale_factor as f32);

    let (rows, cols) = state.gpu.grid_size();
    let (pixel_width, pixel_height) = grid_pixels(font_size, state.scale_factor as f32, rows, cols);
    state.terminal.lock().resize(rows, cols);
    let _ = state
        .pty
        .resize(rows as u16, cols as u16, pixel_width, pixel_height);

    state.window.request_redraw();
}

/// Somewhere the shell's input goes.
///
/// [`Pty`] is the one, and a test cannot have one. The trait is what lets the
/// input arms below be called with a buffer standing in for the shell.
pub(crate) trait PtyWrite {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;
}

impl PtyWrite for Pty {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        Pty::write(self, bytes)
    }
}

/// The terminal and the shell, borrowed for the length of one input event.
///
/// The arms of the winit event loop cannot be reached without a real window, so
/// what they do lives here instead, where a test can call it with a terminal it
/// built and a writer it can read back.
struct Input<'a, W: PtyWrite> {
    terminal: &'a FairMutex<Terminal>,
    pty: &'a mut W,
}

impl<W: PtyWrite> Input<'_, W> {
    /// Send `bytes` to the shell as typing, which supersedes a live selection
    /// and returns the view to the live prompt.
    ///
    /// The write happens outside the terminal lock, so the lock the reader
    /// thread wants is not held across a syscall.
    fn key(&mut self, bytes: &[u8]) {
        {
            let mut terminal = self.terminal.lock();
            terminal.clear_selection();
            terminal.scroll_to_bottom();
        }
        let _ = self.pty.write(bytes);
    }

    /// Route a wheel notch of `lines` at pointer cell `at`, reporting the rows
    /// the viewport moved through history.
    ///
    /// `None` when the notch went to the shell rather than the scrollback,
    /// which is what a mouse-reporting app gets and what an alt-screen pager
    /// with alternate scroll on gets unless `shift` overrides it. Those redraw
    /// on the shell's answer, so there is nothing for the caller to do.
    ///
    /// `Some(0)` still means the scrollback handled it, the viewport having
    /// been at the edge of the history it could reach.
    fn wheel(&mut self, lines: i32, at: (usize, usize), shift: bool, mods: u8) -> Option<i32> {
        // Snapshot the routing modes under one lock so the branch below reads a
        // consistent terminal state.
        let (mouse_report, alternate_scroll) = {
            let terminal = self.terminal.lock();
            (
                terminal.mouse_mode() && terminal.sgr_mouse(),
                terminal.is_alt_screen() && terminal.alternate_scroll(),
            )
        };

        if mouse_report {
            let _ = self.pty.write(&sgr_wheel_bytes(lines, at.0, at.1, mods));
            return None;
        }
        if !shift && alternate_scroll {
            // An alt-screen pager with alternate-scroll on expects arrow keys.
            let _ = self.pty.write(&alternate_scroll_bytes(lines));
            return None;
        }

        // The whole-cell scrollback target advances by the rows the move
        // actually shifted the viewport, which is an idiomatic multiple of the
        // wheel's delta until the history edge clamps it. The render loop eases
        // the visual position toward it, so the motion lands cell-aligned.
        let mut terminal = self.terminal.lock();
        let before = terminal.display_offset() as i32;
        terminal.scroll_display(lines * SCROLLBACK_SCROLL_MULTIPLIER);
        Some(terminal.display_offset() as i32 - before)
    }

    /// Report the pointer at `at` to a mouse-reporting app, when it asked for
    /// motion of the kind being made. Tells whether anything was sent.
    ///
    /// A held `button` makes the move a drag, which an app subscribes to
    /// separately from bare motion, so one wanting only drags hears nothing
    /// while the pointer is loose and the other way about.
    fn motion(&mut self, at: (usize, usize), button: Option<u8>) -> bool {
        // Snapshot the routing modes under one lock, matching the wheel path,
        // so the decision reads a consistent terminal state.
        let (sgr, drag, motion) = {
            let terminal = self.terminal.lock();
            (
                terminal.mouse_mode() && terminal.sgr_mouse(),
                terminal.mouse_drag(),
                terminal.mouse_motion(),
            )
        };

        let asked_for = if button.is_some() { drag } else { motion };
        if !sgr || !asked_for {
            return false;
        }

        let _ = self.pty.write(&sgr_motion_bytes(button, at.0, at.1));
        true
    }

    /// Send `text` to the shell as a paste, bracketed when the shell asked for
    /// it, resetting the view the way typing does.
    fn paste(&mut self, text: &str) {
        let bracketed = {
            let mut terminal = self.terminal.lock();
            terminal.clear_selection();
            terminal.scroll_to_bottom();
            terminal.bracketed_paste()
        };
        let _ = self.pty.write(&paste_bytes(text, bracketed));
    }
}

/// The drag an aux window reports for a pointer that moved from `previous` to
/// `cell`, or `None` when there is nothing to say.
///
/// A drag names a cell, so a pointer travelling inside one carries nothing the
/// last report did not. Winit delivers a move per pointer poll, and each report
/// is a formatted socket line and a write, so the ones that would repeat
/// themselves are worth not making. The primary window's pointer path gates on
/// the same crossing.
fn aux_drag_event(
    window: u32,
    previous: (u16, u16),
    cell: (u16, u16),
    pressed: Option<MouseButton>,
    mods: u8,
) -> Option<WindowIpcEvent> {
    if cell == previous {
        return None;
    }

    let button = ipc_button(pressed?)?;
    Some(WindowIpcEvent::Mouse {
        window,
        kind: MouseKind::Drag(button),
        col: cell.0,
        row: cell.1,
        mods,
    })
}

/// Re-read the config file and apply what a running window can change.
///
/// Theme, font size, and cursor animation take effect on the next frame. Font
/// family and ligatures are baked into the primary window's text pass when it is
/// built, so a change to either only reaches windows opened afterward and is
/// logged rather than silently dropped.
///
/// A config that fails to load leaves everything running as it was. Startup
/// falls back to the embedded default, which is right when there is nothing to
/// lose, but mid-session that would replace a working setup with the shipped
/// one over a typo.
fn apply_config_reload(
    state: &mut State,
    theme: &mut Theme,
    theme_name: &mut String,
    font_family: &mut Vec<String>,
    ligatures: &mut bool,
    cursor_animation: &mut CursorAnimation,
) {
    let config = match config::load() {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, "config reload failed; keeping the running config");
            return;
        },
    };

    *theme = config.resolve_theme();
    theme_name.clone_from(&config.theme);
    state.terminal.lock().set_theme(*theme);
    state.gpu.set_theme_colors(theme.background, theme.cursor);

    if config.font_size != state.font_size {
        state.font_size = config.font_size;
        state
            .gpu
            .set_font_size(state.font_size, state.scale_factor as f32);
        update_cell_pixels(&state.terminal, state.font_size, state.scale_factor as f32);

        // The surface is unchanged, so only the cell metrics moved. Re-read the
        // grid size and resize the rest to match, as the font-zoom path does.
        let (rows, cols) = state.gpu.grid_size();
        let (pixel_width, pixel_height) =
            grid_pixels(state.font_size, state.scale_factor as f32, rows, cols);
        state.terminal.lock().resize(rows, cols);
        let _ = state
            .pty
            .resize(rows as u16, cols as u16, pixel_width, pixel_height);
    }

    state.cursor_animation = config.cursor_animation;
    *cursor_animation = config.cursor_animation;

    if *font_family != config.font_family || *ligatures != config.ligatures {
        tracing::info!("font family and ligature changes apply to this window on next launch");
        font_family.clone_from(&config.font_family);
        *ligatures = config.ligatures;
    }

    state.window.request_redraw();
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
            TermEvent::Title(title) => {
                set_window_title(&state.window, &mut state.last_title, &title)
            },
            TermEvent::ResetTitle => {
                set_window_title(&state.window, &mut state.last_title, DEFAULT_TITLE)
            },
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
            // Filtered out before this fan-out, since re-applying a config
            // touches window state this function has no handle on.
            TermEvent::ConfigReload => {},
            TermEvent::ZoomCapture(on) => state.zoom_capture = on,
            TermEvent::FontStep(delta) => apply_font_step(state, delta),
        }
    }
}

/// Give `window` the title `title`, skipping the platform write when it already
/// carries it.
///
/// A shell's prompt hook emits the title on every prompt and the terminal forwards
/// each one, so most arrivals repeat what the window already shows. `last` records
/// what was pushed, and is updated here rather than by the caller so a write cannot
/// land without the record moving with it.
fn set_window_title(window: &Window, last: &mut Option<String>, title: &str) {
    if last.as_deref() == Some(title) {
        return;
    }

    window.set_title(title);
    *last = Some(title.to_owned());
}

/// What a `window_open` frame is allowed to do, decided before any OS window
/// exists.
enum WindowOpenVerdict {
    /// No open window carries the id and there is room for another.
    Open,
    /// A window already carries the id, so the frame names one that exists.
    Duplicate,
    /// [`MAX_AUX_WINDOWS`] are already open.
    AtCapacity,
}

/// Rule a `window_open` for `window_id` in or out, given the ids already open.
///
/// A duplicate is reported rather than opened because every aux lookup resolves
/// by first match. A second window carrying a live id would permanently shadow
/// the first, which could then never again be focused, redrawn, or handed its
/// GPU.
fn classify_window_open(
    open_ids: impl IntoIterator<Item = u32>,
    window_id: u32,
) -> WindowOpenVerdict {
    let mut open = 0;
    for id in open_ids {
        if id == window_id {
            return WindowOpenVerdict::Duplicate;
        }
        open += 1;
    }

    if open >= MAX_AUX_WINDOWS {
        return WindowOpenVerdict::AtCapacity;
    }
    WindowOpenVerdict::Open
}

/// Report the first `window_open` this session to be refused, naming `reason`.
fn warn_window_open_refused(state: &mut State, window_id: u32, reason: &str) {
    if state.warned_window_open_refused {
        return;
    }
    state.warned_window_open_refused = true;
    tracing::warn!(window = window_id, reason, "refused a window_open");
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
///
/// A frame [`classify_window_open`] rules out opens nothing. A duplicate id
/// focuses the window already carrying it, which is what the frame names.
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

    match classify_window_open(state.aux.iter().map(|aux| aux.id), window_id) {
        WindowOpenVerdict::Open => {},
        WindowOpenVerdict::Duplicate => {
            warn_window_open_refused(state, window_id, "id is already open");
            if let Some(aux) = state.aux.iter().find(|aux| aux.id == window_id) {
                aux.window.focus_window();
            }
            return;
        },
        WindowOpenVerdict::AtCapacity => {
            warn_window_open_refused(state, window_id, "aux window limit reached");
            return;
        },
    }

    let [cell_w, cell_h] = render::cell_size(state.font_size, state.scale_factor as f32);
    let attributes = with_app_name(
        Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(cols as f32 * cell_w, rows as f32 * cell_h)),
    );
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
        pending_resize: PendingResize::default(),
        grid: Grid::new(0, 0),
        scratch: Grid::new(0, 0),
        focused: false,
        visibility: Visibility::default(),
        pointer_cell: (0, 0),
        pressed: None,
        wheel_pixels: 0.0,
        pool_anims: BTreeMap::new(),
        last_redraw: None,
        last_geometry: None,
        last_content: None,
        pool_scratch: Vec::new(),
        last_clear_bg: None,
    });

    let size = window.inner_size();
    let font_size = state.font_size;
    let scale_factor = state.scale_factor as f32;
    let proxy = config.proxy.clone();
    let theme = config.theme;
    let font_family = config.font_family.to_vec();
    let ligatures = config.ligatures;
    let shared = state.shared_gpu.clone();
    let spawn = std::thread::Builder::new()
        .name(format!("aux-gpu-{window_id}"))
        .spawn(move || {
            let dims = [size.width.max(1), size.height.max(1)];
            let font = || FontConfig {
                size: font_size,
                scale_factor,
                family: &font_family,
                ligatures,
            };

            // The primary's adapter may not present to this window's surface,
            // and the shared path says so rather than guessing. A full init
            // picks an adapter for this surface in particular, at the cost of
            // the asking and a second font scan.
            let shared = shared.as_ref().and_then(|(gpu, fonts)| {
                GpuContext::with_shared(
                    gpu,
                    window.clone(),
                    dims,
                    fonts,
                    font(),
                    theme.background,
                    theme.cursor,
                )
            });
            let gpu = match shared {
                Some(gpu) => gpu,
                None => {
                    tracing::info!(
                        window = window_id,
                        "aux window building its own gpu context",
                    );
                    GpuContext::new(
                        window,
                        dims[0],
                        dims[1],
                        FontLoad::spawn(),
                        font(),
                        theme.background,
                        theme.cursor,
                    )
                },
            };
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

/// Assemble and present one primary-window frame.
///
/// Steps the frame's easing by the wall time since the last one, projects the
/// terminal under the lock, and draws the pools, the cursor, and the
/// decorations.
///
/// Returns without drawing when the window is not visible. That one gate
/// covers the terminal lock, the projection, the render, and the ease
/// self-request together, so a hidden window stops asking for frames.
///
/// See also:
/// - [`redraw_aux`] for the aux windows' counterpart.
fn redraw(state: &mut State) {
    // Every frame goes through here, so one gate covers the lock, the
    // projection, the render, and the ease self-request below that
    // would otherwise keep asking for frames nobody sees.
    if !state.visibility.admit() {
        return;
    }

    // A Resized event asks for this frame, and winit delivers the request
    // before the batch ends, so fitting here is what keeps the frame from
    // going out against the surface the resize was meant to replace.
    apply_primary_resize(state);

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
        rides,
        clear_colors,
    ) = {
        let mut terminal = state.terminal.lock();
        // Read under the projection's lock, so the clear color and
        // the cells it surrounds come from one view of the terminal.
        let clear_colors = (terminal.default_background(), terminal.default_cursor());
        // Last frame's row flags, back before anything asks for
        // this frame's.
        for spare in state.damage_spares.drain(..) {
            terminal.recycle_damage(spare);
        }
        let display_offset = terminal.display_offset();
        // Scrolled back, the frame renders the composed history
        // window instead of this grid, so projecting into it is a
        // full pass nothing draws. The damage the skipped
        // projections would have consumed accumulates, so returning
        // to the bottom repaints exactly the rows that moved.
        //
        // Zero scroll is what the projection reports at a non-zero
        // offset anyway, the viewport being pinned to its content,
        // so standing in for it costs nothing.
        let (cursor, scroll_delta, damage) = if display_offset > 0 {
            let changed = terminal.take_damage_flag();
            let damage = if changed {
                Damage::Full
            } else {
                Damage::Partial(Vec::new())
            };
            (terminal.cursor(), 0, damage)
        } else {
            terminal.project(&mut state.grid)
        };
        let decoration_damage = terminal.take_decoration_damage();
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
        // Which pools moved this frame. The anchored pass below reads it to tell
        // an anchor whose host rides from one whose host sits still.
        let mut glided: Vec<u32> = Vec::new();
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
            glided.push(pool.id);

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

        // Resolve every pool riding a host that moved this frame, to the pixel
        // shift that keeps it over the text it was laid out against and the host
        // region that clips it.
        let mut rides = mem::take(&mut state.rides_scratch);
        rides.clear();
        for pool in &pools {
            let Some((host, top_rows)) = pool.anchor else {
                continue;
            };
            if !glided.contains(&host) {
                continue;
            }
            let Some(host_view) = pools.iter().find(|candidate| candidate.id == host) else {
                continue;
            };
            let Some(host_anim) = state.pool_anims.get(&host) else {
                continue;
            };
            rides.push(AnchorRide {
                pool: pool.id,
                top_rows,
                host_scroll: host_anim.scroll,
                host_region: host_view.region,
            });
        }

        // A ridden pool has to composite even when its own scroll settled. The
        // base grid still holds it where the last live frame drew it, and no live
        // frame ships mid-glide, so without this its body stays put and tears
        // away from the shifted frame.
        for &AnchorRide { pool: id, .. } in &rides {
            if active.iter().any(|tile| tile.id == id) {
                continue;
            }
            let Some(view) = pools.iter().find(|pool| pool.id == id) else {
                continue;
            };
            let anim = state
                .pool_anims
                .entry(id)
                .or_insert_with(|| PoolAnim::new(view.scroll_target.pages()));

            // A ride moves where the pool is drawn, never what it holds, so the
            // same gate the glide uses answers here. Without it every host frame
            // re-projects the pool and tells the renderer to re-shape every row
            // of it.
            let gate = compose_gate(anim, view, &terminal, anim.scroll);
            if gate.content_changed {
                let composed = terminal
                    .project_pool(id, &mut anim.document_grid, anim.scroll)
                    .is_some();
                anim.record_compose(&gate, composed);
            }
            if !anim.last_buffered {
                continue;
            }

            pool_easing = true;
            active.push(ActivePool {
                id,
                region: view.region,
                frac: gate.frac,
                content_changed: gate.content_changed,
                scrolled_rows: gate.scrolled_rows.or(Some(0)),
            });
        }
        // Pools composite in ascending id, which is their z-order, and a forced
        // one is appended out of turn.
        active.sort_by_key(|tile| tile.id);

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
            rides,
            clear_colors,
        )
    };

    // A program that set OSC 11 recolors the cells, but the gutter
    // past the grid keeps whatever the clear was last set to, so
    // follow the override here rather than only at config reload.
    let (clear_bg, clear_cursor) = clear_colors;
    if state.last_clear_bg != Some(clear_bg) {
        state.last_clear_bg = Some(clear_bg);
        state.gpu.set_theme_colors(clear_bg, clear_cursor);
    }

    let mut overflows = mem::take(&mut state.overflows_scratch);
    refresh_popover_overflows(
        state.grid.overlays(),
        state.grid.popovers_epoch(),
        &mut state.last_popovers_epoch,
        &mut overflows,
    );
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

    let (grid_scroll, grid_scrolling) = step_grid_scroll(state.grid_scroll, scroll_delta, dt);
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
                || matches!(&damage, Damage::Partial(rows) if rows.iter().any(|d| d.is_some()));
            let rebuild = state.last_scrollback_offset != Some(offset) || vt_changed;
            // A larger offset reaches further back, which pushes the
            // window's content down the screen, so the rows the content
            // moved is the previous offset less this one.
            //
            // Growing history raises the offset too, through the pin
            // above, but that only keeps the window on the content it
            // already showed. Adding the pin back leaves the movement a
            // wheel actually made, which is zero while the window holds
            // still under output.
            let moved_rows = match state.last_scrollback_offset {
                Some(last) => (last - offset) as isize + pin as isize,
                None => 0,
            };
            state.last_scrollback_offset = Some(offset);

            let mut sb_damage = Damage::Partial(Vec::new());
            if rebuild {
                let mut terminal = state.terminal.lock();
                terminal.project_scrollback(
                    &mut state.scrollback_grid,
                    state.scrollback_visual,
                    moved_rows,
                    &mut sb_damage,
                );
            }

            // The sub-cell shift project_scrollback returns, recomputed
            // locally so a fraction-only frame needs no lock or compose.
            let scroll_offset = (state.scrollback_visual - state.scrollback_visual.floor()) - 1.0;

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
                    scrolled_rows: moved_rows,
                },
            );
            // Read now, so the row flags go back for the next
            // projection to fill rather than being dropped and
            // allocated again a frame later.
            state.damage_spares.push(sb_damage);
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
                    // The projection slid the grid by this much and
                    // reported damage naming only the rows that
                    // really changed, so the row caches have to
                    // rotate to match or the clean ones redraw from
                    // their pre-slide instances.
                    scrolled_rows: scroll_delta as isize,
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

                // An anchored pool rides its host's ease. The shift moves both
                // the drawn origin and the scissor, and the host's own scissor
                // then clips it, so the surface slides out of the pane edge
                // rather than over the neighbour.
                let ride = rides.iter().find(|ride| ride.pool == pool.id);
                let (ride_rows, scissor) = match ride {
                    Some(ride) => {
                        let dy_px = anchored_shift(
                            ride.top_rows,
                            ride.host_scroll,
                            (ride.host_region.height as f32).max(1.0),
                            ch,
                        );
                        (
                            dy_px / ch,
                            intersect_scissor(
                                shift_scissor([x0, y0, x1 - x0, y1 - y0], dy_px),
                                region_scissor(ride.host_region, cw, ch),
                            ),
                        )
                    },
                    None => (0.0, [x0, y0, x1 - x0, y1 - y0]),
                };

                PoolComposite {
                    id: pool.id,
                    grid: &state.pool_anims[&pool.id].document_grid,
                    // The ride rides the shift below rather than this origin.
                    // Every composite shader snaps against the origin and adds
                    // its shift after, so the two land in the same pixel, while
                    // an origin left on the whole-cell grid is one the renderer's
                    // instance cache still recognizes a frame later.
                    origin_cells: [region.left as f32, region.top as f32],
                    scissor,
                    // Snapped once over the sum. Two roundings of two fractions
                    // land a pixel from where their sum does.
                    shift_rows: snap_shift_to_pixels(ride_rows - pool.frac, ch),
                    content_changed: pool.content_changed,
                    scrolled_rows: pool.scrolled_rows,
                    occludable: pool.id < NON_PANE_POOL_BASE,
                }
            })
            .collect::<Vec<_>>();

        // The panel half of a ride. The renderer matches these against each
        // panel's own anchor, so one entry per host carries every frame riding it.
        let anchored_panels = rides
            .iter()
            .map(|ride| AnchoredPanel {
                host: ride.host_region.pool,
                dy_px: anchored_shift(
                    ride.top_rows,
                    ride.host_scroll,
                    (ride.host_region.height as f32).max(1.0),
                    ch,
                ),
                scissor: region_scissor(ride.host_region, cw, ch),
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
                scrolled_rows: scroll_delta as isize,
            },
            &composites,
            &anchored_panels,
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
    state.rides_scratch = rides;
    // Held for the next frame, which hands them back before it asks
    // the terminal for its damage.
    state.damage_spares.push(damage);
    state.damage_spares.push(decoration_damage);

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
    // What the base compose changed, when one ran at all.
    let mut composed = None;
    {
        let mut terminal = terminal.lock();
        // The aux clear follows the terminal's default background for the same
        // reason the primary one does, so an OSC 11 override reaches the gutter
        // of every window rather than only the first.
        let clear_bg = terminal.default_background();
        if aux.last_clear_bg != Some(clear_bg) {
            aux.last_clear_bg = Some(clear_bg);
            gpu.set_theme_colors(clear_bg, terminal.default_cursor());
        }

        let mut pools = mem::take(&mut aux.pool_scratch);
        terminal.window_pools_into(aux.id, &mut pools);

        // Step each window pool's ease and collect the ones still gliding, in
        // ascending-id z-order, dropping anim state for pools the app retired.
        // Ahead of the compose below, which reads which pools came out of this
        // with a composite over them.
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

        // Recomposed only when a hash of what it reads moves. A pure sub-cell
        // glide leaves the base untouched and rides the overlay below, and so
        // does a whole-row one under a pool the overlay covers.
        //
        // Which hash moved decides how much of the frame the recompose costs. A
        // geometry move changes which cells any pool covers, so the grid is
        // blanked and every row rebuilt. A content move leaves every rectangle
        // where it was, so the same rows are overwritten in place and only the
        // ones that came back different are rebuilt.
        let geometry = aux_geometry_hash(&pools, rows, cols);
        let content = aux_content_hash(&pools, &active);
        if aux.last_geometry != Some(geometry) || aux.last_content != Some(content) {
            let mut damage = match aux.last_geometry == Some(geometry) {
                true => Damage::Partial(vec![None; rows]),
                false => Damage::Full,
            };
            aux.last_geometry = Some(geometry);
            aux.last_content = Some(content);
            compose_aux_grid(
                &terminal,
                &pools,
                &mut aux.grid,
                &mut aux.scratch,
                rows,
                cols,
                &mut damage,
            );
            composed = Some(damage);
        }

        aux.pool_scratch = pools;
    }

    // Composite each gliding pool from its region-sized grid, placed at the
    // region origin, scissored to it and shifted by the sub-cell fraction.
    let composites = active
        .iter()
        .map(|pool| PoolComposite {
            id: pool.id,
            grid: &aux.pool_anims[&pool.id].document_grid,
            origin_cells: [pool.region.left as f32, pool.region.top as f32],
            scissor: region_scissor(pool.region, cw, ch),
            shift_rows: -snap_shift_to_pixels(pool.frac, ch),
            content_changed: pool.content_changed,
            scrolled_rows: pool.scrolled_rows,
            occludable: pool.id < NON_PANE_POOL_BASE,
        })
        .collect::<Vec<_>>();

    // A skipped recompose reuses last frame's instances, so empty partial damage
    // leaves them untouched.
    let damage = composed.unwrap_or(Damage::Partial(Vec::new()));
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
            scrolled_rows: 0,
        },
        &composites,
        &[],
        None,
    );
    easing || heal
}

/// The sub-cell glide shift `frac` rounded so it moves the pool a whole number
/// of physical pixels, over a cell `cell_h` pixels tall.
///
/// Glyph masks are sampled nearest with no subpixel variants, so a quad landing
/// mid-pixel reads its bitmap off-center. Font glyphs sit at a fractional
/// baseline phase and procedural cell-fill glyphs at none, so a fractional shift
/// carries the two across the half-pixel boundary on different frames and the
/// text re-snaps mid-glide while the cell backgrounds hold. A whole-pixel shift
/// makes each glide frame the rested frame merely translated, so text,
/// backgrounds, and pool widgets step together.
///
/// A cell with no height has no pixel to round to, so the shift passes through.
fn snap_shift_to_pixels(frac: f32, cell_h: f32) -> f32 {
    if cell_h <= 0.0 {
        return frac;
    }
    (frac * cell_h).round() / cell_h
}

/// Hash where [`compose_aux_grid`] puts things: the window size and, per pool in
/// z-order, its id and region rectangle.
///
/// A move here changes which cells any pool covers at all, so the grid has to go
/// back to the window background before the compose and every row of it is
/// suspect. [`aux_content_hash`] covers what the pools hold, which is the
/// cheaper half.
fn aux_geometry_hash(pools: &[PoolView], rows: usize, cols: usize) -> u64 {
    let mut hasher = FxHasher::default();
    rows.hash(&mut hasher);
    cols.hash(&mut hasher);
    for pool in pools {
        pool.id.hash(&mut hasher);
        pool.region.top.hash(&mut hasher);
        pool.region.left.hash(&mut hasher);
        pool.region.width.hash(&mut hasher);
        pool.region.height.hash(&mut hasher);
    }
    hasher.finish()
}

/// Hash what [`compose_aux_grid`] holds: per pool in z-order, its content
/// version and, for the ones no overlay covers, its scroll target.
///
/// A move here leaves every rectangle where it was, so the compose overwrites
/// the same rows in place and damages only the ones that came back different.
/// The sub-cell glide fraction is not covered, since it rides the overlay rather
/// than the base.
///
/// `covered` are the pools drawing a composite over their own region this
/// frame, whose scroll target is left out. Such a pool hides the base beneath
/// it whole, straddle row and all, so where the base holds it is not an input
/// to anything on screen, and a glide moves that target on every tick.
///
/// Entering and leaving that set is what a glide costs: one recompose as the
/// pool takes a composite, which puts the base at the destination, and one as
/// it settles and its target is read again, which catches the base up to where
/// the glide actually landed. The ticks between read nothing and cost nothing.
///
/// A content version is read whatever the pool is doing, since output arriving
/// mid-glide has to reach the base before a settle reveals it.
fn aux_content_hash(pools: &[PoolView], covered: &[ActivePool]) -> u64 {
    let mut hasher = FxHasher::default();
    for pool in pools {
        pool.content_version.hash(&mut hasher);
        if !covered.iter().any(|tile| tile.id == pool.id) {
            pool.scroll_target.pages().to_bits().hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Compose every pool in `pools` into `grid`, sized to `rows` x `cols`, each
/// pool's cells and decorations placed at its window-relative region.
///
/// `scratch` is reused to project each pool before its rows are blitted. Pools
/// compose in ascending-id order, their off-grid text runs and bars translated
/// from region-local to window coordinates. v1 projects at each pool's scroll
/// target directly, with no sub-cell glide, so only the region's own rows are
/// copied and the straddle row `project_pool` composes is dropped.
///
/// `damage` says how much of the last compose survives, and collects what this
/// one changed. Under a [`Damage::Full`] the grid is blanked first, so a cell no
/// pool covers shows the window background. Under a partial one the cells stand
/// and the pools overwrite them in place, marking the rows that came back
/// different; only what sits beside the cells is reset, since a compose that
/// kept it would stack this pass's decorations on the last one's.
///
/// A grid that had to be resized holds nothing worth comparing, so it is damaged
/// whole whatever the caller asked for.
fn compose_aux_grid(
    terminal: &Terminal,
    pools: &[PoolView],
    grid: &mut Grid,
    scratch: &mut Grid,
    rows: usize,
    cols: usize,
    damage: &mut Damage,
) {
    if grid.rows() != rows || grid.cols() != cols {
        grid.resize(rows, cols);
        *damage = Damage::Full;
    } else if matches!(damage, Damage::Full) {
        grid.clear();
    } else {
        grid.clear_decorations();
    }

    for pool in pools {
        if terminal
            .project_pool(pool.id, scratch, pool.scroll_target.pages())
            .is_none()
        {
            continue;
        }

        // The region's own rows only. v1 projects at the scroll target with no
        // sub-cell glide, so the straddle row `project_pool` composes covers a
        // sliver nothing here reveals.
        grid.append_region(
            scratch,
            pool.region.top as usize,
            pool.region.left as usize,
            pool.region.height as usize,
            damage,
        );
    }
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

/// Fit every window whose size moved during the event batch just handled.
///
/// Once per batch rather than per event, so a drag pays one swapchain
/// reallocation and one pty ioctl per frame instead of one per size the window
/// manager reported on the way. The terminal already refuses a resize to the
/// cell dimensions it has, so only the reaching of it moved.
///
/// This is the backstop, not the main path. winit dispatches
/// `RedrawRequested` before `AboutToWait`, so the redraw a `Resized` event asks
/// for arrives first and fits itself ([`apply_primary_resize`] at the top of
/// [`redraw`], [`fit_aux`] in the aux redraw arm). What is left for here is a
/// batch that scheduled no frame at all. An occluded window is that case. It
/// draws nothing, and its pty still has to learn the new size.
fn apply_pending_resizes(state: &mut State) {
    apply_primary_resize(state);

    let mut reports: Vec<(u32, u16, u16)> = Vec::new();
    for aux in &mut state.aux {
        if let Some((cols, rows)) = fit_aux(aux) {
            reports.push((aux.id, cols, rows));
        }
    }
    for (window, cols, rows) in reports {
        send_window_event(state, WindowIpcEvent::Resized { window, cols, rows });
    }
}

/// Fit the primary window's surface, terminal, and pty to the size its last
/// batch settled on.
///
/// Does nothing when no size waits, so a frame that follows no resize pays one
/// `Option` check for the guarantee that it draws against the surface the
/// window actually has.
fn apply_primary_resize(state: &mut State) {
    let Some((width, height)) = state.pending_resize.take() else {
        return;
    };
    state.gpu.resize(width, height);
    let (rows, cols) = state.gpu.grid_size();
    let (pixel_width, pixel_height) =
        grid_pixels(state.font_size, state.scale_factor as f32, rows, cols);
    state.terminal.lock().resize(rows, cols);
    let _ = state
        .pty
        .resize(rows as u16, cols as u16, pixel_width, pixel_height);
}

/// Fit one aux window's surface to the size its last batch settled on,
/// returning the cell dimensions to report to the child.
///
/// `None` when no size waits or the window's GPU has not arrived yet. The
/// caller sends the report, since the socket lives on the whole [`State`] while
/// this borrows one window out of it.
fn fit_aux(aux: &mut AuxWindow) -> Option<(u16, u16)> {
    let (width, height) = aux.pending_resize.take()?;
    let gpu = aux.gpu.as_mut()?;
    gpu.resize(width, height);
    let (rows, cols) = gpu.grid_size();
    Some((cols as u16, rows as u16))
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
/// Returns the path to export as `STOATTY_WINDOW_SOCKET`, the channel aux
/// windows report on, and the flag saying whether a child is connected to read
/// them. The first two are `None` on a bind failure or a non-unix build, where
/// aux windows render but report nothing upstream, and the flag stays false.
fn open_window_event_socket() -> (Option<PathBuf>, Option<Sender<String>>, Arc<AtomicBool>) {
    #[cfg(unix)]
    {
        match bind_window_socket() {
            Ok((path, tx, connected)) => (Some(path), Some(tx), connected),
            Err(error) => {
                tracing::warn!(%error, "window-event socket unavailable");
                (None, None, Arc::new(AtomicBool::new(false)))
            },
        }
    }
    #[cfg(not(unix))]
    {
        (None, None, Arc::new(AtomicBool::new(false)))
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
// Creating the log directory and clearing a stale socket are socket lifecycle,
// and the terminal holds no FsHost to route them through.
#[cfg(unix)]
#[allow(clippy::disallowed_methods)]
fn bind_window_socket() -> io::Result<(PathBuf, Sender<String>, Arc<AtomicBool>)> {
    let dir = stoat_log::log_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = window_socket_path(&dir, std::process::id());
    // A prior process at this pid may have left its socket behind, and bind
    // fails on an existing path, so clear a stale one first.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let (tx, rx) = mpsc::channel::<String>();
    let connected = Arc::new(AtomicBool::new(false));
    std::thread::Builder::new()
        .name("window-events".to_string())
        .spawn({
            let connected = connected.clone();
            move || serve_window_events(listener, rx, &connected)
        })?;
    Ok((path, tx, connected))
}

/// Forward queued window-event lines to the connected child.
///
/// Serves one client at a time. Each accepted stream receives every subsequent
/// line terminated by '\n' until a write fails, then the thread re-accepts.
/// Events sent while no client is connected queue on the channel and flush to
/// the next one. Returns when the channel closes as the app exits.
///
/// `connected` tracks whether a client is on the other end, which is what
/// decides whether a zoom press has anywhere to go. It is raised on accept and
/// dropped when the write loop breaks, so a child that exits hands the combo
/// back to font zoom without waiting to be told.
#[cfg(unix)]
fn serve_window_events(listener: UnixListener, rx: Receiver<String>, connected: &AtomicBool) {
    for client in listener.incoming() {
        let Ok(mut client) = client else { continue };
        connected.store(true, Ordering::Relaxed);
        loop {
            let Ok(line) = rx.recv() else {
                connected.store(false, Ordering::Relaxed);
                return;
            };
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            if client.write_all(&bytes).is_err() {
                break;
            }
        }
        connected.store(false, Ordering::Relaxed);
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

/// The clipboard handle cached on [`State`], opening one on first use.
///
/// The handle is held across uses rather than reopened each time. Opening one
/// costs a display-server connection and a background thread, which is several
/// milliseconds inside the event loop on X11 and Wayland.
///
/// A copy needs it held for a further reason. X11 selection ownership lasts
/// only while an `arboard::Clipboard` and its server thread stay alive, so a
/// handle opened and dropped per copy releases ownership at once, losing the
/// copied text before any paste unless a clipboard manager races to grab it,
/// and arboard's debug build prints a drop warning to the launching terminal.
///
/// `None` when the clipboard cannot be opened, which is reported rather than
/// fatal and leaves nothing cached, so the next call tries again.
fn clipboard_handle(state: &mut State) -> Option<&mut arboard::Clipboard> {
    if state.clipboard.is_none() {
        match arboard::Clipboard::new() {
            Ok(clipboard) => state.clipboard = Some(clipboard),
            Err(err) => {
                eprintln!("stoatty: failed to open clipboard: {err}");
                return None;
            },
        }
    }
    state.clipboard.as_mut()
}

/// Copy `text` to the OS clipboard through the handle cached on [`State`].
///
/// A failed copy is reported rather than fatal, and drops the handle so the
/// next call opens a fresh one.
fn copy_to_clipboard(state: &mut State, text: &str) {
    let Some(clipboard) = clipboard_handle(state) else {
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

#[cfg(test)]
mod tests {
    use super::{
        app_has_focus, aux_content_hash, aux_drag_event, aux_geometry_hash, bell_should_ring,
        classify_window_open, compose_aux_grid, forwards_zoom, grid_pixels, selection_copy_text,
        snap_shift_to_pixels, swallow_super_combo, ActivePool, Input, PendingResize, PoolView,
        PtyWrite, Visibility, WindowOpenVerdict, MAX_AUX_WINDOWS,
    };
    #[cfg(unix)]
    use super::{
        serve_window_events, window_socket_path, AtomicBool, Ordering, PathBuf, UnixListener,
    };
    use crate::input::{alternate_scroll_bytes, sgr_motion_bytes, sgr_wheel_bytes};
    use alacritty_terminal::sync::FairMutex;
    #[cfg(unix)]
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};
    use stoatty_term::{
        grid::{Damage, DocumentOffset, Grid, PoolRegion},
        term::Terminal,
        theme::Theme,
    };
    use winit::keyboard::ModifiersState;

    /// A pool drawing a composite over its own region hides the base beneath it
    /// whole, so where the base holds that pool is not an input to anything on
    /// screen. A glide moves the target every tick, and reading it would cost a
    /// full recompose and a full-damage render on each of them.
    #[test]
    fn a_covered_pool_s_target_is_not_read() {
        let (mut pool, covered) = gliding_pool();
        let before = aux_content_hash(&[pool], &covered);

        pool.scroll_target = DocumentOffset {
            page: 3,
            fraction: 0.25,
        };
        assert_eq!(
            aux_content_hash(&[pool], &covered),
            before,
            "the target of a pool an overlay covers moves nothing",
        );
        assert_ne!(
            aux_content_hash(&[pool], &[]),
            before,
            "and the same move on an uncovered pool is what the base rests on",
        );
    }

    /// A glide costs one recompose as it starts and one as it settles, and none
    /// in between. Entering and leaving the covered set is what moves the hash
    /// at each end, and the settle is the catch-up: the pool drops its composite
    /// onto a base that has to hold where the glide left it.
    #[test]
    fn a_glide_moves_the_hash_at_each_end_and_not_between() {
        let (mut pool, covered) = gliding_pool();
        let at_rest = aux_content_hash(&[pool], &[]);

        // The wheel moves the target and the pool takes a composite, both on the
        // frame the glide starts.
        pool.scroll_target = DocumentOffset {
            page: 5,
            fraction: 0.0,
        };
        let starting = aux_content_hash(&[pool], &covered);
        assert_ne!(at_rest, starting, "the base is composed at the destination");

        // Every tick after moves the target again and reads none of it.
        pool.scroll_target = DocumentOffset {
            page: 5,
            fraction: 0.5,
        };
        assert_eq!(
            aux_content_hash(&[pool], &covered),
            starting,
            "a tick under the overlay costs nothing",
        );

        assert_ne!(
            aux_content_hash(&[pool], &[]),
            starting,
            "and the settle is read, so the base catches up to the landing",
        );
    }

    /// Output arriving mid-glide has to reach the base before the settle reveals
    /// it, so a content version is read whether an overlay covers the pool or
    /// not.
    #[test]
    fn a_covered_pool_still_reads_its_content() {
        let (pool, covered) = gliding_pool();
        let before = aux_content_hash(&[pool], &covered);

        let mut printed = pool;
        printed.content_version = pool.content_version + 1;
        assert_ne!(
            aux_content_hash(&[printed], &covered),
            before,
            "content arriving under an overlay still has to reach the base",
        );
    }

    /// The two hashes divide the compose's inputs between them, because what
    /// moved decides how much of the frame the recompose costs. Where the pools
    /// sit is the half that makes the grid go back to the window background and
    /// every row rebuild.
    #[test]
    fn the_two_hashes_split_where_the_pools_sit_from_what_they_hold() {
        let (pool, covered) = gliding_pool();
        let (geometry, content) = (
            aux_geometry_hash(&[pool], 24, 80),
            aux_content_hash(&[pool], &covered),
        );

        let mut moved = pool;
        moved.region.top += 1;
        assert_ne!(
            aux_geometry_hash(&[moved], 24, 80),
            geometry,
            "a region that moved uncovers base the pool no longer draws over",
        );
        assert_eq!(
            aux_content_hash(&[moved], &covered),
            content,
            "and says nothing about what the pool holds",
        );

        assert_ne!(
            aux_geometry_hash(&[pool], 25, 80),
            geometry,
            "so does a window that resized",
        );

        let mut printed = pool;
        printed.content_version = pool.content_version + 1;
        printed.scroll_target = DocumentOffset {
            page: 9,
            fraction: 0.0,
        };
        assert_eq!(
            aux_geometry_hash(&[printed], 24, 80),
            geometry,
            "while new content and a new target leave every rectangle where it was",
        );
        assert_ne!(
            aux_content_hash(&[printed], &[]),
            content,
            "and are read by the half that overwrites those rectangles in place",
        );
    }

    /// A geometry move changes which cells any pool covers, so the grid goes
    /// back to the window background first. A content move leaves every
    /// rectangle where it was, so the cells stand and the pools overwrite them,
    /// which is what makes the row compare below worth anything.
    #[test]
    fn a_full_compose_blanks_the_grid_and_a_partial_one_overwrites_it() {
        let (terminal, pools) = composed_pool(b"aa");
        let (mut grid, mut scratch) = (Grid::new(4, 4), Grid::new(1, 1));

        // A cell outside every region, which only a blanking compose clears.
        grid.get_mut(3, 3).ch = 'x';
        compose_aux_grid(
            &terminal,
            &pools,
            &mut grid,
            &mut scratch,
            4,
            4,
            &mut Damage::Full,
        );
        assert_eq!(
            (grid.get(0, 0).ch, grid.get(3, 3).ch),
            ('a', ' '),
            "the pool lands and the cell no pool covers goes back to background",
        );

        grid.get_mut(3, 3).ch = 'x';
        let mut damage = Damage::Partial(vec![None; 4]);
        compose_aux_grid(
            &terminal,
            &pools,
            &mut grid,
            &mut scratch,
            4,
            4,
            &mut damage,
        );
        assert_eq!(
            (grid.get(0, 0).ch, grid.get(3, 3).ch),
            ('a', 'x'),
            "an in-place compose touches only what the pools cover",
        );
        assert_eq!(
            marked_rows(&damage),
            Vec::<usize>::new(),
            "and marks nothing, since every row came back as it was",
        );
    }

    /// The rows a pool overwrites with the bytes they already held are the ones
    /// worth not rebuilding, which is the whole point of composing in place.
    #[test]
    fn an_in_place_compose_marks_only_the_rows_that_changed() {
        let (mut terminal, pools) = composed_pool(b"aa\r\nbb");
        let (mut grid, mut scratch) = (Grid::new(4, 4), Grid::new(1, 1));
        compose_aux_grid(
            &terminal,
            &pools,
            &mut grid,
            &mut scratch,
            4,
            4,
            &mut Damage::Full,
        );

        fill_page(&mut terminal, 1, 0, b"aa\r\nzz");
        let mut damage = Damage::Partial(vec![None; 4]);
        compose_aux_grid(
            &terminal,
            &pools,
            &mut grid,
            &mut scratch,
            4,
            4,
            &mut damage,
        );

        assert_eq!(grid.get(1, 0).ch, 'z', "the row that changed is written");
        assert_eq!(
            marked_rows(&damage),
            vec![1],
            "and it is the only one marked",
        );
    }

    /// A resized grid holds nothing the compare could read, so the compose says
    /// so however little the caller thought had moved.
    #[test]
    fn a_compose_that_resizes_damages_the_whole_grid() {
        let (terminal, pools) = composed_pool(b"aa");
        let (mut grid, mut scratch) = (Grid::new(2, 2), Grid::new(1, 1));

        let mut damage = Damage::Partial(vec![None; 4]);
        compose_aux_grid(
            &terminal,
            &pools,
            &mut grid,
            &mut scratch,
            4,
            4,
            &mut damage,
        );

        assert!(
            matches!(damage, Damage::Full),
            "a grid that came back a different size has no clean rows to keep",
        );
    }

    /// A terminal holding one two-row pool with `text` painted into its first
    /// page, and the view of it a compose reads.
    fn composed_pool(text: &[u8]) -> (Terminal, Vec<PoolView>) {
        use stoatty_protocol::command::{encode_pool_region, PoolRegionCommand};

        let mut terminal = Terminal::new(4, 4, Theme::default());
        terminal.advance(&encode_pool_region(&PoolRegionCommand {
            pool: 1,
            top: 0,
            left: 0,
            width: 2,
            height: 2,
            window: 1,
        }));
        // A two-row page, and the one below it, since the compose spans a row
        // more than the region is tall.
        fill_page(&mut terminal, 1, 0, text);
        fill_page(&mut terminal, 1, 1, b"..");

        let pools = vec![PoolView {
            id: 1,
            region: PoolRegion {
                pool: 1,
                window: 1,
                top: 0,
                left: 0,
                width: 2,
                height: 2,
            },
            scroll_target: DocumentOffset::default(),
            cursor_anchor: None,
            anchor: None,
            content_version: 0,
        }];
        (terminal, pools)
    }

    fn fill_page(terminal: &mut Terminal, pool: u32, index: u64, text: &[u8]) {
        use stoatty_protocol::command::{encode_fill, encode_fill_end, FillCommand};

        let mut stream = encode_fill(&FillCommand { pool, index });
        stream.extend_from_slice(text);
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);
    }

    /// The rows a partial damage names, ascending.
    fn marked_rows(damage: &Damage) -> Vec<usize> {
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

    /// A pool at rest, and the one entry that says an overlay covers it.
    fn gliding_pool() -> (PoolView, Vec<ActivePool>) {
        let region = PoolRegion {
            pool: 1,
            window: 1,
            top: 0,
            left: 0,
            width: 40,
            height: 12,
        };
        let pool = PoolView {
            id: 1,
            region,
            scroll_target: DocumentOffset::default(),
            cursor_anchor: None,
            anchor: None,
            content_version: 7,
        };
        let covered = vec![ActivePool {
            id: 1,
            region,
            frac: 0.0,
            content_changed: false,
            scrolled_rows: Some(0),
        }];
        (pool, covered)
    }

    /// Every shift a glide ships moves the pool a whole number of pixels.
    ///
    /// Glyph masks sample nearest with no subpixel variants, so a fractional
    /// shift re-rasterizes font and procedural glyphs on different frames and
    /// the text visibly re-snaps under motion. Whole pixels make each frame the
    /// rested frame translated.
    #[test]
    fn a_glide_shift_moves_the_pool_a_whole_pixel() {
        // font_size 16 at scale factor 1 gives a 19.2px cell. The height has to
        // be fractional here. At an integer one every shift already lands on a
        // pixel and this pins nothing.
        let cell_h = 19.2;

        for step in 0..=20 {
            let frac = step as f32 / 20.0;
            let snapped = snap_shift_to_pixels(frac, cell_h);
            let pixels = snapped * cell_h;

            assert!(
                (pixels - pixels.round()).abs() < 1e-3,
                "shift {frac} moves {pixels} pixels, which is not a whole one",
            );
            assert!(
                (snapped - frac).abs() * cell_h <= 0.5 + 1e-3,
                "shift {frac} snapped to {snapped}, further than the nearest pixel",
            );
        }

        assert_eq!(
            snap_shift_to_pixels(0.0, cell_h),
            0.0,
            "a rested pool does not move",
        );
        assert_eq!(
            snap_shift_to_pixels(0.4, 0.0),
            0.4,
            "a cell with no height has no pixel to round to",
        );
    }

    /// A burst of sizes costs one fitting, on the size it ended at.
    ///
    /// A drag reports several per frame, and fitting each one reallocates the
    /// swapchain and tells the child a size it is about to be told again. Only
    /// the last of them describes the window.
    #[test]
    fn a_burst_of_sizes_settles_on_the_last_one() {
        let mut pending = PendingResize::default();
        assert_eq!(pending.take(), None, "a window nobody resized fits nothing");

        pending.record(800, 600);
        pending.record(802, 600);
        pending.record(806, 604);

        assert_eq!(pending.take(), Some((806, 604)));
        assert_eq!(
            pending.take(),
            None,
            "and the batch after it fits nothing again",
        );
    }

    #[test]
    fn a_visible_window_draws_every_frame_it_is_asked_for() {
        // The default has to be draw-everything, since a platform that never
        // reports occlusion leaves the flag where it started, and a window that
        // silently stopped drawing there would just be blank.
        let mut visibility = Visibility::default();

        assert!(visibility.admit(), "nothing has hidden it");
        assert!(visibility.admit(), "and it stays that way");
        assert_eq!(visibility, Visibility::default(), "nothing owed either");
    }

    #[test]
    fn a_frame_asked_for_while_hidden_arrives_when_it_comes_back() {
        // Skipping is only safe because the request is kept. Dropping it would
        // leave the window showing whatever it last drew before hiding, with
        // nothing scheduled to correct it.
        let mut visibility = Visibility::default();
        assert!(!visibility.set_occluded(true), "hiding owes nothing");

        assert!(!visibility.admit(), "hidden, so no frame runs");
        assert!(!visibility.admit(), "still hidden, and still none");

        assert!(visibility.set_occluded(false), "the held frame comes due");
        assert!(visibility.admit(), "and it draws");
        assert!(
            !visibility.set_occluded(true) && !visibility.set_occluded(false),
            "the debt was settled, so hiding again owes nothing on its own",
        );
    }

    #[test]
    fn coming_back_without_a_missed_frame_asks_for_nothing() {
        // A window hidden and shown with nothing happening in between has
        // nothing to catch up on, and requesting a frame anyway would restart
        // the ease self-request chain for no reason.
        let mut visibility = Visibility::default();
        visibility.set_occluded(true);

        assert!(
            !visibility.set_occluded(false),
            "no redraw was asked for while it was away",
        );
    }

    #[test]
    fn window_open_is_refused_when_the_id_is_open_or_the_list_is_full() {
        let full: Vec<u32> = (0..MAX_AUX_WINDOWS as u32).collect();

        assert!(matches!(
            classify_window_open([7, 9], 4),
            WindowOpenVerdict::Open
        ));
        assert!(matches!(
            classify_window_open(std::iter::empty(), 4),
            WindowOpenVerdict::Open
        ));
        assert!(matches!(
            classify_window_open([7, 9], 9),
            WindowOpenVerdict::Duplicate
        ));
        assert!(matches!(
            classify_window_open(full.iter().copied(), 99),
            WindowOpenVerdict::AtCapacity
        ));
        // A duplicate outranks the cap, so a full list still reports the id
        // rather than hiding it behind the limit.
        assert!(matches!(
            classify_window_open(full.iter().copied(), 0),
            WindowOpenVerdict::Duplicate
        ));
        assert!(
            matches!(
                classify_window_open(full[1..].iter().copied(), 99),
                WindowOpenVerdict::Open
            ),
            "one below the cap still opens"
        );
    }

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

    /// A claimed combo with nowhere to send it must not vanish, so a connected
    /// reader is as much a precondition as the claim itself.
    ///
    /// A connected client rather than a bound socket, because those come apart
    /// exactly where the combo goes dead. A child reached over ssh never sees
    /// the socket path, and one that exits leaves the socket bound behind it.
    #[test]
    fn a_zoom_combo_forwards_only_with_both_a_claim_and_a_reader() {
        assert!(forwards_zoom(true, true), "claimed with a reader forwards");
        assert!(
            !forwards_zoom(true, false),
            "claimed with nobody reading falls back to font zoom"
        );
        assert!(
            !forwards_zoom(false, true),
            "an unclaimed combo zooms the font even though a reader is there"
        );
        assert!(!forwards_zoom(false, false), "neither forwards");
    }

    /// The connected flag follows a client across its whole life, so the combo
    /// goes back to font zoom the moment the child stops reading.
    ///
    /// A client that goes away is the case a bound socket cannot see, and the
    /// one that leaves zoom dead in a stoatty outliving its editor.
    #[cfg(unix)]
    #[test]
    fn the_connected_flag_tracks_a_client_across_its_life() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("win.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let (tx, rx) = mpsc::channel::<String>();
        let connected = Arc::new(AtomicBool::new(false));

        let served = std::thread::spawn({
            let connected = connected.clone();
            move || serve_window_events(listener, rx, &connected)
        });
        assert!(
            !connected.load(Ordering::Relaxed),
            "nothing is connected before a client arrives",
        );

        // A round trip is what proves the thread reached its accept, since the
        // flag is raised there and nothing else reports it.
        let round_trip = |tx: &mpsc::Sender<String>,
                          client: &mut std::os::unix::net::UnixStream| {
            tx.send("hello".to_string()).expect("send");
            let mut got = [0u8; 6];
            std::io::Read::read_exact(client, &mut got).expect("read");
            assert_eq!(&got, b"hello\n");
        };
        // Bounded so a flag that never moves fails here rather than hanging.
        let settles_to = |want: bool| {
            (0..200).any(|_| {
                if connected.load(Ordering::Relaxed) == want {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(1));
                false
            })
        };

        let mut client = std::os::unix::net::UnixStream::connect(&path).expect("connect");
        round_trip(&tx, &mut client);
        assert!(
            connected.load(Ordering::Relaxed),
            "a client that is reading raises the flag",
        );

        drop(client);
        // A write is what discovers the closed peer, so the thread needs one to
        // fail on before it can notice.
        tx.send("bye".to_string()).expect("send");
        assert!(settles_to(false), "a client that went away drops the flag");

        // The thread is parked in accept again by now, so a second client is
        // what both proves it re-accepts and lets it reach the closed channel.
        let mut next = std::os::unix::net::UnixStream::connect(&path).expect("reconnect");
        round_trip(&tx, &mut next);
        assert!(settles_to(true), "and the next client raises it again");

        drop(tx);
        served.join().expect("serving thread");
        assert!(
            !connected.load(Ordering::Relaxed),
            "serving ending leaves nothing claimed",
        );
    }

    #[test]
    fn an_aux_drag_reports_only_where_the_pointer_crosses_a_cell() {
        use stoatty_protocol::window_ipc::{
            MouseButton as IpcMouseButton, MouseKind, WindowIpcEvent,
        };
        use winit::event::MouseButton;

        let held = Some(MouseButton::Left);
        let reported = |previous, cell, pressed| {
            aux_drag_event(7, previous, cell, pressed, 0).map(|event| match event {
                WindowIpcEvent::Mouse { col, row, .. } => (col, row),
                other => panic!("expected a mouse event, got {other:?}"),
            })
        };

        assert_eq!(
            [
                reported((3, 4), (3, 4), held),
                reported((3, 4), (4, 4), held),
                reported((3, 4), (3, 5), held),
                reported((3, 4), (4, 4), None),
            ],
            [None, Some((4, 4)), Some((3, 5)), None],
            "a move inside a cell says nothing, a crossing names the new cell, \
             and nothing is dragging with no button down"
        );

        assert!(
            matches!(
                aux_drag_event(7, (0, 0), (1, 0), held, 0),
                Some(WindowIpcEvent::Mouse {
                    window: 7,
                    kind: MouseKind::Drag(IpcMouseButton::Left),
                    ..
                })
            ),
            "the report carries its window and the button being held"
        );
    }

    /// Collects what an input arm sends instead of a shell reading it.
    #[derive(Default)]
    struct SpyPty {
        written: Vec<u8>,
    }

    impl PtyWrite for SpyPty {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.written.extend_from_slice(bytes);
            Ok(())
        }
    }

    /// A terminal holding `text`, scrolled back with a selection over it, which
    /// is the state an input event is supposed to reset.
    fn scrolled_back_with_a_selection() -> FairMutex<Terminal> {
        let mut terminal = Terminal::new(2, 8, Theme::default());
        terminal.advance(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        terminal.start_selection(0, 0, false);
        terminal.update_selection(0, 3, true);
        terminal.scroll_display(2);
        assert!(
            terminal.display_offset() > 0,
            "the fixture is scrolled back"
        );
        FairMutex::new(terminal)
    }

    #[test]
    fn typing_reaches_the_shell_and_resets_the_view() {
        let terminal = scrolled_back_with_a_selection();
        let mut pty = SpyPty::default();

        Input {
            terminal: &terminal,
            pty: &mut pty,
        }
        .key(b"x");

        assert_eq!(pty.written, b"x", "the keystroke reaches the shell");
        let terminal = terminal.lock();
        assert_eq!(
            terminal.display_offset(),
            0,
            "typing returns the view to the live prompt"
        );
        assert!(
            terminal.selection_text().is_none(),
            "typing drops the selection it would otherwise sit over"
        );
    }

    #[test]
    fn a_paste_is_bracketed_only_when_the_shell_asked_for_it() {
        let plain = {
            let terminal = scrolled_back_with_a_selection();
            let mut pty = SpyPty::default();
            Input {
                terminal: &terminal,
                pty: &mut pty,
            }
            .paste("hi");
            pty.written
        };

        let bracketed = {
            let terminal = scrolled_back_with_a_selection();
            terminal.lock().advance(b"\x1b[?2004h");
            let mut pty = SpyPty::default();
            Input {
                terminal: &terminal,
                pty: &mut pty,
            }
            .paste("hi");
            pty.written
        };

        assert_eq!(plain, b"hi", "an unasked paste goes as its own text");
        assert_eq!(
            bracketed, b"\x1b[200~hi\x1b[201~",
            "a shell that set bracketed paste gets the markers"
        );
    }

    #[test]
    fn a_wheel_notch_scrolls_the_history_when_nothing_claims_it() {
        let terminal = scrolled_back_with_a_selection();
        let before = terminal.lock().display_offset() as i32;
        let mut pty = SpyPty::default();

        let moved = Input {
            terminal: &terminal,
            pty: &mut pty,
        }
        .wheel(1, (0, 0), false, 0);

        assert_eq!(
            terminal.lock().display_offset() as i32 - before,
            moved.expect("the scrollback took the notch"),
            "the rows reported are the rows the viewport moved"
        );
        assert!(pty.written.is_empty(), "nothing went to the shell");
    }

    #[test]
    fn a_mouse_reporting_app_gets_the_wheel_as_a_button() {
        let terminal = scrolled_back_with_a_selection();
        // SGR mouse reporting on, which is what routes the wheel to the shell.
        terminal.lock().advance(b"\x1b[?1000h\x1b[?1006h");
        let before = terminal.lock().display_offset();
        let mut pty = SpyPty::default();

        let moved = Input {
            terminal: &terminal,
            pty: &mut pty,
        }
        .wheel(1, (2, 3), false, 0);

        assert_eq!(moved, None, "the shell took the notch, so nothing to ease");
        assert_eq!(
            pty.written,
            sgr_wheel_bytes(1, 2, 3, 0),
            "the notch goes as a button press at the pointer"
        );
        assert_eq!(
            terminal.lock().display_offset(),
            before,
            "the viewport stays where it was"
        );
    }

    #[test]
    fn shift_takes_the_wheel_back_from_an_alternate_scroll_pager() {
        let routed = |shift: bool| {
            let terminal = scrolled_back_with_a_selection();
            // Alt screen with alternate scroll on, which a pager sets.
            terminal.lock().advance(b"\x1b[?1049h\x1b[?1007h");
            let mut pty = SpyPty::default();
            let moved = Input {
                terminal: &terminal,
                pty: &mut pty,
            }
            .wheel(1, (0, 0), shift, 0);
            (moved.is_some(), pty.written)
        };

        let (plain_local, plain_sent) = routed(false);
        let (shifted_local, shifted_sent) = routed(true);

        assert_eq!(
            (plain_local, plain_sent),
            (false, alternate_scroll_bytes(1)),
            "the pager gets arrow keys"
        );
        assert_eq!(
            (shifted_local, shifted_sent),
            (true, Vec::new()),
            "shift keeps the notch for the scrollback"
        );
    }

    /// An app subscribes to drags and to bare motion separately, so what it
    /// hears has to match which it asked for and which the pointer is doing.
    #[test]
    fn motion_reaches_only_the_app_that_asked_for_that_kind_of_move() {
        // 1002 is drag reporting, 1003 any motion, 1006 the SGR encoding the
        // routing also requires.
        let reported = |modes: &[u16], button: Option<u8>| {
            let terminal = scrolled_back_with_a_selection();
            {
                let mut terminal = terminal.lock();
                for mode in modes {
                    terminal.advance(format!("\x1b[?{mode}h").as_bytes());
                }
            }
            let mut pty = SpyPty::default();
            let sent = Input {
                terminal: &terminal,
                pty: &mut pty,
            }
            .motion((1, 2), button);
            assert_eq!(
                sent,
                !pty.written.is_empty(),
                "what it reports and what it sent have to agree"
            );
            sent
        };

        let held = Some(0);
        assert_eq!(
            [
                reported(&[1002, 1006], held),
                reported(&[1002, 1006], None),
                reported(&[1003, 1006], held),
                reported(&[1003, 1006], None),
                reported(&[1002], held),
                reported(&[], held),
            ],
            [true, false, true, true, false, false],
            "drag reporting hears only drags, any-motion hears both, and neither \
             hears anything without the SGR encoding"
        );
    }

    #[test]
    fn a_reported_motion_carries_the_pointer_cell_and_the_button() {
        let terminal = scrolled_back_with_a_selection();
        terminal.lock().advance(b"\x1b[?1003h\x1b[?1006h");
        let mut pty = SpyPty::default();

        Input {
            terminal: &terminal,
            pty: &mut pty,
        }
        .motion((4, 9), Some(0));

        assert_eq!(
            pty.written,
            sgr_motion_bytes(Some(0), 4, 9),
            "the report names where the pointer is and what is held"
        );
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

    /// An image client sizes what it draws from these two numbers, so they have
    /// to be the text area rather than anything else on screen.
    #[test]
    fn the_reported_extent_is_the_cell_size_times_the_grid() {
        let [cell_w, cell_h] = stoatty_render::render::cell_size(14, 1.0);

        assert_eq!(
            grid_pixels(14, 1.0, 24, 80),
            (
                (cell_w * 80.0).round() as u16,
                (cell_h * 24.0).round() as u16,
            ),
        );
        assert_eq!(grid_pixels(14, 1.0, 0, 0), (0, 0), "an empty grid has none");
    }

    /// The cell rectangle scales with the display, so the extent has to as well.
    /// Reporting logical pixels to a client drawing physical ones halves the
    /// image on a 2x screen.
    #[test]
    fn the_extent_follows_the_display_scale() {
        let (one_x, one_y) = grid_pixels(14, 1.0, 24, 80);
        let (two_x, two_y) = grid_pixels(14, 2.0, 24, 80);

        assert_eq!((two_x, two_y), (one_x * 2, one_y * 2));
    }

    /// A grid wide enough to overflow the field is unreachable, but wrapping one
    /// would report a tiny area for an enormous one, which is the worst answer
    /// available to the question.
    #[test]
    fn an_unreachable_grid_saturates_rather_than_wrapping() {
        assert_eq!(grid_pixels(14, 1.0, usize::MAX, usize::MAX), (65535, 65535));
    }
}
