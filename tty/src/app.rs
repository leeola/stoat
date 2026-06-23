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
    config::{self, Config},
    pty::{self, Pty, PtyOutput},
};
use alacritty_terminal::sync::FairMutex;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use stoatty_protocol::command::PoolRegionCommand;
use stoatty_render::{
    gpu::{FontConfig, FontLoad, Frame, GpuContext, PoolComposite, Scroll},
    render,
};
use stoatty_term::{
    grid::{Cell, Grid, Overlay},
    term::{Cursor, CursorShape, Damage, Terminal},
    theme::Theme,
};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{Window, WindowId},
};

/// Smallest font size the live zoom allows, so cells never collapse to an
/// unreadable size.
const FONT_SIZE_FLOOR: u32 = 6;

/// Lines of terminal-owned scrollback the wheel moves per line of wheel travel,
/// the idiomatic multi-line wheel step common terminals use (e.g. alacritty's
/// default scroll multiplier of 3). Applies only to local scrollback, never to
/// the wheel reports forwarded to a mouse-reporting app.
const SCROLLBACK_SCROLL_MULTIPLIER: i32 = 3;

/// Open the stoatty window running the launch command, or the user's default
/// shell when none is given, at the winit default window size.
///
/// Resolves the launch program and its arguments by precedence: `command` (the
/// `-e`/`--command` CLI override) first, then the `[shell]` config, then the
/// default shell with no arguments. Blocks the calling thread for the lifetime
/// of the window. See [`run_with_shell`] to force a specific command instead.
pub fn run(command: Option<(String, Vec<String>)>) {
    let mut config = load_config();
    let (program, args) = command
        .or_else(|| config.shell.take().map(|s| (s.program, s.args)))
        .unwrap_or_else(|| (pty::default_shell(), Vec::new()));
    run_with_config(config, program, args, None);
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
    run_with_config(load_config(), program, args, size);
}

/// Open the window running `program` with `args`, drawing with `config`'s theme
/// and font, and run the event loop until the window closes.
///
/// The shared core of [`run`] and [`run_with_shell`]. It takes an
/// already-loaded `config` so each entry point loads it exactly once.
fn run_with_config(config: Config, program: String, args: Vec<String>, size: Option<[u16; 2]>) {
    let theme = config.resolve_theme();

    let event_loop = EventLoop::<PtyEvent>::with_user_event()
        .build()
        .expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App::new(
        event_loop.create_proxy(),
        program,
        args,
        theme,
        FontSettings {
            size: config.font_size,
            family: config.font_family,
            ligatures: config.ligatures,
        },
        size,
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
    Exited,
}

/// The text-rendering configuration read from the config once, which [`App`]
/// seeds the renderer's [`FontConfig`] with when the window opens.
struct FontSettings {
    size: u32,
    family: Vec<String>,
    ligatures: bool,
}

struct App {
    proxy: EventLoopProxy<PtyEvent>,
    program: String,
    args: Vec<String>,
    theme: Theme,
    font_size: u32,
    /// Ordered font-family cascade from the config, resolved against the font db
    /// at renderer creation to pick the shaping primary. Read once in `resumed`.
    font_family: Vec<String>,
    /// Whether the renderer shapes cell runs together so ligatures form. Read
    /// once in `resumed` into the renderer's [`FontConfig`].
    ligatures: bool,
    /// The window's content size in cells (`[cols, rows]`) to open sized to, or
    /// `None` for the winit default window. Read once at window creation.
    size: Option<[u16; 2]>,
    state: Option<State>,
}

impl App {
    fn new(
        proxy: EventLoopProxy<PtyEvent>,
        program: String,
        args: Vec<String>,
        theme: Theme,
        font: FontSettings,
        size: Option<[u16; 2]>,
    ) -> App {
        App {
            proxy,
            program,
            args,
            theme,
            font_size: font.size,
            font_family: font.family,
            ligatures: font.ligatures,
            size,
            state: None,
        }
    }
}

struct State {
    window: Arc<Window>,
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
    /// The cursor's animated position in fractional cell coordinates, eased
    /// toward the terminal's actual cursor cell each frame.
    cursor_anim: [f32; 2],
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
    /// shifts them, rebuilding only when the integer offset changes. `None` when
    /// the previous frame rendered the live grid.
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
    /// Whether the previous frame composited the document pool, leaving the
    /// shared background and text instance buffers holding its cells. The next
    /// live-grid render rebuilds fully rather than reuse them, so the rows the
    /// pool did not cover (the static chrome) are not painted with its blanks.
    /// Cleared by that rebuild.
    pool_dirty: bool,
    /// Unspent vertical wheel travel in physical pixels, accumulated from
    /// high-resolution `PixelDelta` events until it reaches a whole cell so a
    /// trackpad scrolls scrollback smoothly without losing sub-line motion.
    wheel_pixels: f64,
    /// The grid cell `(col, row)` under the pointer, tracked from `CursorMoved`,
    /// so a mouse-reporting app receives wheel reports at the pointer position.
    pointer_cell: (usize, usize),
}

/// One pool's smooth-scroll animation state, held by [`State::pool_anims`].
struct PoolAnim {
    /// The live eased offset, in document pages, easing toward the pool's
    /// app-declared target. Tracks an absolute position rather than decaying.
    scroll: f32,
    /// The region's pooled rows composed at [`Self::scroll`], sized to the
    /// region plus one straddle row. Reused across frames.
    document_grid: Grid,
    /// The viewport-sized grid the pool composites from: the region's pooled
    /// rows copied into the declared sub-rectangle, the rest blank since the
    /// scissor clips the composite to that rectangle over the live grid.
    pool_grid: Grid,
}

impl PoolAnim {
    /// A fresh pool resting at `scroll`, so a newly declared pool shows at its
    /// current position rather than gliding in from the document origin.
    fn new(scroll: f32) -> PoolAnim {
        PoolAnim {
            scroll,
            document_grid: Grid::new(0, 0),
            pool_grid: Grid::new(0, 0),
        }
    }
}

/// A pool that is mid-glide and buffered this frame, so the renderer composites
/// it: which pool, its region, and the sub-cell fraction to shift its rows by.
struct ActivePool {
    id: u32,
    region: PoolRegionCommand,
    frac: f32,
}

impl ApplicationHandler<PtyEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }

        // Kick off font enumeration before creating the window, so it runs on a
        // background thread concurrently with window and GPU setup rather than
        // blocking the first paint after them.
        let font_load = FontLoad::spawn();

        let mut attributes = Window::default_attributes().with_title("stoatty");
        if let Some([cols, rows]) = self.size {
            let [cell_width, cell_height] = render::cell_size(self.font_size, 1.0);
            attributes = attributes.with_inner_size(LogicalSize::new(
                cols as f32 * cell_width,
                rows as f32 * cell_height,
            ));
        }
        let window = Arc::new(event_loop.create_window(attributes).expect("create window"));

        let size = window.inner_size();
        let scale_factor = window.scale_factor();
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
        let dirty = Arc::new(AtomicBool::new(false));
        let sync_pending = Arc::new(AtomicBool::new(false));

        let pty = {
            let proxy = self.proxy.clone();
            let terminal = terminal.clone();
            let dirty = dirty.clone();
            let sync_pending = sync_pending.clone();
            Pty::spawn(
                &self.program,
                &self.args,
                rows as u16,
                cols as u16,
                move |output| match output {
                    PtyOutput::Data(bytes) => {
                        // Parse on the reader thread under the shared lock.
                        let (redraw, responses) = {
                            let mut terminal = terminal.lock();
                            let redraw = terminal.advance(&bytes);
                            // A buffering synchronized update needs the main
                            // thread to arm and drive its timeout flush.
                            sync_pending
                                .store(terminal.sync_deadline().is_some(), Ordering::Relaxed);
                            (redraw, terminal.take_responses())
                        };
                        if !responses.is_empty() {
                            let _ = proxy.send_event(PtyEvent::Responses(responses));
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
                        let _ = proxy.send_event(PtyEvent::Exited);
                    },
                },
            )
            .expect("spawn shell over pty")
        };

        window.request_redraw();
        self.state = Some(State {
            window,
            gpu,
            terminal,
            dirty,
            sync_pending,
            grid,
            pty,
            font_size: self.font_size,
            scale_factor,
            modifiers: ModifiersState::empty(),
            cursor_anim: [0.0, 0.0],
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
            pool_dirty: false,
            wheel_pixels: 0.0,
            pointer_cell: (0, 0),
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
            PtyEvent::Exited => event_loop.exit(),
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
        let Some(state) = self.state.as_ref() else {
            return;
        };

        if !state.sync_pending.load(Ordering::Relaxed) {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }

        let deadline = {
            let mut terminal = state.terminal.lock();
            match terminal.sync_deadline() {
                Some(deadline) if deadline <= Instant::now() => {
                    terminal.flush_synchronized_update();
                    None
                },
                other => other,
            }
        };

        match deadline {
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => {
                state.sync_pending.store(false, Ordering::Relaxed);
                state.window.request_redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            },
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
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

                // The cell metrics moved with the new density; the surface is
                // re-fitted by the `Resized` that follows. Re-read the grid size
                // and resize the rest now, mirroring the font-zoom chain.
                let (rows, cols) = state.gpu.grid_size();
                state.terminal.lock().resize(rows, cols);
                let _ = state.pty.resize(rows as u16, cols as u16);

                state.window.request_redraw();
            },
            WindowEvent::RedrawRequested => {
                let (
                    cursor,
                    scroll_delta,
                    damage,
                    decoration_damage,
                    display_offset,
                    active,
                    pool_easing,
                ) = {
                    let mut terminal = state.terminal.lock();
                    let (cursor, scroll_delta, damage) = terminal.project(&mut state.grid);
                    let decoration_damage = terminal.take_decoration_damage();
                    let display_offset = terminal.display_offset();
                    let pools = terminal.pools();

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
                    let mut active: Vec<ActivePool> = Vec::new();
                    let mut pool_easing = false;
                    for pool in &pools {
                        let page_rows = (pool.region.height as f32).max(1.0);
                        let anim = state
                            .pool_anims
                            .entry(pool.id)
                            .or_insert_with(|| PoolAnim::new(pool.scroll_target.pages()));

                        // A reposition jump re-anchors the offset to a local
                        // neighbour of the destination, so the ease lands softly
                        // within the freshly-buffered window instead of dragging
                        // across the unbuffered gap.
                        if let Some(target) = terminal.take_reposition(pool.id) {
                            anim.scroll = (target as f32 - REPOSITION_LAND_PAGES).max(0.0);
                        }

                        let (scroll, easing) = step_document_scroll(
                            anim.scroll,
                            pool.scroll_target.pages(),
                            page_rows,
                        );
                        anim.scroll = scroll;
                        if !easing {
                            continue;
                        }
                        pool_easing = true;
                        if let Some((frac, _)) =
                            terminal.project_pool(pool.id, &mut anim.document_grid, scroll)
                        {
                            active.push(ActivePool {
                                id: pool.id,
                                region: pool.region,
                                frac,
                            });
                        }
                    }

                    (
                        cursor,
                        scroll_delta,
                        damage,
                        decoration_damage,
                        display_offset,
                        active,
                        pool_easing,
                    )
                };

                let overflows: Vec<Option<f32>> =
                    state.grid.overlays().iter().map(popover_overflow).collect();
                state.popover_scrolls.resize(overflows.len(), 0.0);
                state.popover_scroll_downs.resize(overflows.len(), true);

                let mut popover_scrolling = false;
                for (index, overflow) in overflows.into_iter().enumerate() {
                    match overflow {
                        Some(max) => {
                            let (next, down) = step_popover_scroll(
                                state.popover_scrolls[index],
                                state.popover_scroll_downs[index],
                                max,
                            );
                            state.popover_scrolls[index] = next;
                            state.popover_scroll_downs[index] = down;
                            popover_scrolling = true;
                        },
                        None => state.popover_scrolls[index] = 0.0,
                    }
                }

                let (grid_scroll, grid_scrolling) =
                    step_grid_scroll(state.grid_scroll, scroll_delta);
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
                    step_scrollback_scroll(state.scrollback_visual, state.scrollback_target);
                state.scrollback_visual = scrollback_visual;

                let (region_scroll, region_scrolling) = match state.grid.scroll_region() {
                    Some(region) => {
                        let offset = region.offset as f32;
                        let delta = offset - state.last_region_offset;
                        state.last_region_offset = offset;
                        step_region_scroll(state.region_scroll, delta)
                    },
                    None => {
                        state.last_region_offset = 0.0;
                        (0.0, false)
                    },
                };
                state.region_scroll = region_scroll;

                let cursor_easing = if active.is_empty() {
                    // No pool is mid-glide: fall to the scrollback window when the
                    // view is scrolled back, else the live grid.
                    let scrollback_view = {
                        let terminal = state.terminal.lock();
                        terminal
                            .project_scrollback(&mut state.scrollback_grid, state.scrollback_visual)
                    };

                    match scrollback_view {
                        Some(scroll_offset) => {
                            // The view is scrolled back: render the composed history
                            // window, gliding it by the sub-cell fraction. The
                            // integer offset selects which rows fill the window;
                            // rebuild on an offset change or when live output
                            // redamaged the grid, otherwise reuse the cached rows.
                            let offset = state.scrollback_visual.floor() as i32;
                            let vt_changed = matches!(&damage, Damage::Full)
                                || matches!(&damage, Damage::Partial(rows) if rows.iter().any(|&d| d));
                            let rebuild =
                                state.last_scrollback_offset != Some(offset) || vt_changed;
                            state.last_scrollback_offset = Some(offset);
                            let sb_damage = if rebuild {
                                Damage::Full
                            } else {
                                Damage::Partial(vec![false; state.scrollback_grid.rows()])
                            };
                            state.gpu.render(
                                &state.scrollback_grid,
                                Frame {
                                    cursor: None,
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
                        },
                        None => {
                            // At the live bottom: render the projected live grid
                            // (cursor and decorations), cursor easing as usual.
                            state.last_scrollback_offset = None;
                            // A pool composite on the previous frame left the shared
                            // instance buffers holding its cells, so rebuild the
                            // whole live grid once -- otherwise the rows no pool
                            // covered (the static chrome) reuse its blank cells and
                            // render black.
                            let full = Damage::Full;
                            let (live_damage, live_decoration_damage) = if state.pool_dirty {
                                state.pool_dirty = false;
                                (&full, &full)
                            } else {
                                (&damage, &decoration_damage)
                            };
                            match cursor_position(cursor) {
                                Some(target) => {
                                    let (next, settled) = ease(state.cursor_anim, target);
                                    state.cursor_anim = next;
                                    state.gpu.render(
                                        &state.grid,
                                        Frame {
                                            cursor: Some(next),
                                            scroll: Scroll {
                                                grid: state.grid_scroll,
                                                document: 0.0,
                                                scrollback: 0.0,
                                                region: state.region_scroll,
                                                popovers: &state.popover_scrolls,
                                            },
                                            damage: live_damage,
                                            decoration_damage: live_decoration_damage,
                                        },
                                    );
                                    !settled
                                },
                                None => {
                                    state.gpu.render(
                                        &state.grid,
                                        Frame {
                                            cursor: None,
                                            scroll: Scroll {
                                                grid: state.grid_scroll,
                                                document: 0.0,
                                                scrollback: 0.0,
                                                region: state.region_scroll,
                                                popovers: &state.popover_scrolls,
                                            },
                                            damage: live_damage,
                                            decoration_damage: live_decoration_damage,
                                        },
                                    );
                                    false
                                },
                            }
                        },
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
                            }
                        })
                        .collect::<Vec<_>>();

                    let (base_cursor, cursor_easing) = match cursor_position(cursor) {
                        Some(target) => {
                            let (next, settled) = ease(state.cursor_anim, target);
                            state.cursor_anim = next;
                            (Some(next), !settled)
                        },
                        None => (None, false),
                    };

                    // Force a full rebuild: composite_pool prepares the shared
                    // background/text instance buffers with each pool's cells, so
                    // the live grid must rebuild every frame rather than reuse those
                    // polluted instances and paint a pool over the chrome.
                    state.gpu.render_with_pools(
                        &state.grid,
                        Frame {
                            cursor: base_cursor,
                            scroll: Scroll {
                                grid: state.grid_scroll,
                                document: 0.0,
                                scrollback: 0.0,
                                region: state.region_scroll,
                                popovers: &state.popover_scrolls,
                            },
                            damage: &Damage::Full,
                            decoration_damage: &Damage::Full,
                        },
                        &composites,
                    );
                    state.pool_dirty = true;
                    cursor_easing
                };

                // Keep the vsync-paced loop running while the cursor eases, a
                // popover scrolls, or the grid, scrollback, a region, or a pool
                // scrolls. When all settle the loop idles until the next PTY
                // output or resize.
                if cursor_easing
                    || popover_scrolling
                    || grid_scrolling
                    || scrollback_scrolling
                    || region_scrolling
                    || pool_easing
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

                if let Some(delta) = font_step(platform_mod_held, &event.logical_key) {
                    let font_size =
                        (state.font_size as i32 + delta).max(FONT_SIZE_FLOOR as i32) as u32;
                    state.font_size = font_size;
                    state
                        .gpu
                        .set_font_size(font_size, state.scale_factor as f32);

                    // The surface is unchanged, so skip `gpu.resize`; only the cell
                    // metrics moved, so re-read the grid size and resize the rest.
                    let (rows, cols) = state.gpu.grid_size();
                    state.terminal.lock().resize(rows, cols);
                    let _ = state.pty.resize(rows as u16, cols as u16);

                    state.window.request_redraw();
                    return;
                }

                if let Some(bytes) = encode_key(&event.logical_key, state.modifiers.control_key()) {
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
                state.pointer_cell = cell_at(position.x, position.y, cell_size, rows, cols);
            },
            _ => {},
        }
    }
}

/// Build the full-viewport `pool_grid` for a region composite: sized to
/// `live_grid`, blank, with `document_grid`'s composed rows copied into the
/// `region` sub-rectangle.
///
/// `document_grid` holds the pooled `region.height + 1` rows by `region.width`
/// columns (one straddle row past the region) the term composed; they land at
/// (`region.top`, `region.left`). Cells of `document_grid` that would fall
/// outside `pool_grid` are skipped, so a region declared past the viewport
/// clips rather than panicking. The scissor the renderer applies clips the
/// composite to the region, so the blank surround is never drawn.
fn copy_pool_region(
    pool_grid: &mut Grid,
    document_grid: &Grid,
    live_grid: &Grid,
    region: PoolRegionCommand,
) {
    if pool_grid.rows() != live_grid.rows() || pool_grid.cols() != live_grid.cols() {
        pool_grid.resize(live_grid.rows(), live_grid.cols());
    } else {
        for row in 0..pool_grid.rows() {
            for col in 0..pool_grid.cols() {
                *pool_grid.get_mut(row, col) = Cell::default();
            }
        }
    }

    for r in 0..document_grid.rows() {
        for c in 0..document_grid.cols() {
            let row = region.top as usize + r;
            let col = region.left as usize + c;
            if row < pool_grid.rows() && col < pool_grid.cols() {
                *pool_grid.get_mut(row, col) = *document_grid.get(r, c);
            }
        }
    }
}

/// The cursor's cell position for the renderer, or `None` when it is hidden.
fn cursor_position(cursor: Cursor) -> Option<[f32; 2]> {
    if cursor.shape == CursorShape::Hidden {
        None
    } else {
        Some([cursor.col as f32, cursor.row as f32])
    }
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

/// Encode a key press into the bytes a terminal sends to the shell, or `None`
/// for a key with no terminal encoding (a bare modifier, a function key) so the
/// caller writes nothing.
///
/// `ctrl` is whether Ctrl is held: with it, an ASCII letter becomes its C0
/// control byte (Ctrl-C is `0x03`). Cursor keys use the normal-mode `CSI` forms
/// (`\x1b[A` through `\x1b[D`); printable keys pass through as their own UTF-8
/// bytes.
fn encode_key(key: &Key, ctrl: bool) -> Option<Vec<u8>> {
    match key {
        Key::Named(NamedKey::Enter) => Some(vec![b'\r']),
        Key::Named(NamedKey::Backspace) => Some(vec![0x7f]),
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
/// A `LineDelta` is already in lines. A high-resolution `PixelDelta` accrues in
/// `pixels` against `cell_height` and yields whole lines once a cell's worth has
/// built up, carrying the sub-line remainder so successive small deltas are not
/// lost. `LineDelta` leaves the accumulator untouched.
fn wheel_lines(delta: MouseScrollDelta, pixels: &mut f64, cell_height: f64) -> i32 {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
        MouseScrollDelta::PixelDelta(position) => {
            *pixels += position.y;
            let lines = (*pixels / cell_height) as i32;
            *pixels -= f64::from(lines) * cell_height;
            lines
        },
    }
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

/// The grid cell `(col, row)` under physical pixel (`x`, `y`), clamped to the
/// `rows` x `cols` grid, for a `cell_size` of `[width, height]` physical pixels.
fn cell_at(x: f64, y: f64, cell_size: [f32; 2], rows: usize, cols: usize) -> (usize, usize) {
    let col = ((x / f64::from(cell_size[0])) as usize).min(cols.saturating_sub(1));
    let row = ((y / f64::from(cell_size[1])) as usize).min(rows.saturating_sub(1));
    (col, row)
}

/// Step the animated cursor toward `target`, returning the new position and
/// whether it has reached the target.
///
/// Each frame closes a fixed fraction of the remaining distance, the
/// exponential ease-out that reads as smooth cursor motion. Within a small
/// epsilon it snaps onto the target so the animation terminates.
fn ease(current: [f32; 2], target: [f32; 2]) -> ([f32; 2], bool) {
    const FACTOR: f32 = 0.35;
    const EPSILON: f32 = 0.01;

    let dx = target[0] - current[0];
    let dy = target[1] - current[1];
    if dx.abs() < EPSILON && dy.abs() < EPSILON {
        return (target, true);
    }

    ([current[0] + dx * FACTOR, current[1] + dy * FACTOR], false)
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

/// Advance the ping-pong popover scroll one frame toward its current end,
/// reversing direction when it settles.
///
/// `down` eases the offset toward `max` (the overflow bottom); once settled it
/// flips, easing back toward the top, so the content glides up and down while
/// the popover is visible.
fn step_popover_scroll(scroll: f32, down: bool, max: f32) -> (f32, bool) {
    let target = if down { max } else { 0.0 };
    let (next, settled) = ease([scroll, 0.0], [target, 0.0]);
    let down = if settled { !down } else { down };
    (next[0], down)
}

/// Advance the grid's eased vertical scroll one frame.
///
/// The new `delta` (rows the content scrolled up) is added to the offset, so the
/// content starts that many rows lower, then the offset eases toward zero so it
/// glides up into place. Returns the new offset and whether it is still easing.
fn step_grid_scroll(scroll: f32, delta: usize) -> (f32, bool) {
    let seeded = scroll + delta as f32;
    let (next, settled) = ease([seeded, 0.0], [0.0, 0.0]);
    (next[0], !settled)
}

/// Floor on the scrollback ease's per-frame step, in rows, so the exponential
/// tail locks in with a quick, even glide instead of crawling the last
/// sub-pixels into the target. A few pixels per frame at a typical cell height;
/// raise for a snappier lock-in, lower for a softer one.
const SCROLLBACK_MIN_STEP: f32 = 0.15;

/// Advance the eased scrollback position one frame toward `target`.
///
/// `scroll` and `target` are positions in rows back from the live bottom: the
/// wheel advances `target` and this eases `scroll` toward it, so the history
/// window scrolls through each row and settles cell-aligned on the target.
///
/// Closes a fixed fraction of the remaining distance each frame for an
/// exponential ease-out, but never moves slower than [`SCROLLBACK_MIN_STEP`], so
/// the tail finishes crisply instead of crawling sub-pixel-by-sub-pixel into the
/// target. Returns the new position and whether it is still easing.
fn step_scrollback_scroll(scroll: f32, target: f32) -> (f32, bool) {
    const FACTOR: f32 = 0.35;
    const EPSILON: f32 = 0.01;

    let remaining = target - scroll;
    if remaining.abs() < EPSILON {
        return (target, false);
    }

    let step = (remaining.abs() * FACTOR)
        .max(SCROLLBACK_MIN_STEP)
        .min(remaining.abs());
    (scroll + step.copysign(remaining), true)
}

/// Advance the scroll region's eased vertical offset one frame.
///
/// `delta` is the change in the region's declared scroll offset since the last
/// frame, signed: positive when the program scrolled the region's content down,
/// negative when up. It seeds the offset, which then eases toward zero so the
/// region's content glides into place. Returns the new offset and whether it is
/// still easing.
fn step_region_scroll(scroll: f32, delta: f32) -> (f32, bool) {
    let seeded = scroll + delta;
    let (next, settled) = ease([seeded, 0.0], [0.0, 0.0]);
    (next[0], !settled)
}

/// Pages before a reposition target the live offset re-anchors to, so a
/// discontinuous jump lands with a one-page soft glide onto the destination
/// rather than appearing instantly. The app buffers from this many pages before
/// the target so the landing glide draws pooled content.
const REPOSITION_LAND_PAGES: f32 = 1.0;

/// Advance the document's eased smooth-scroll offset one frame toward `target`.
///
/// `scroll` and `target` are app-declared absolute positions in document pages;
/// `page_rows` is the rows per page, so the snap epsilon and step floor are
/// expressed in on-screen rows rather than pages. Mirrors
/// [`step_scrollback_scroll`]: closes a fixed fraction of the remaining distance
/// each frame but never less than a row-sized floor, capped at the remaining
/// distance, so the tail lands exactly on the (whole-row) target. A page-unit
/// epsilon would snap a visible fraction of a row when handing back to the live
/// grid, reading as a one-line jump at the end of the glide. Returns the new
/// offset and whether it is still easing.
fn step_document_scroll(scroll: f32, target: f32, page_rows: f32) -> (f32, bool) {
    const FACTOR: f32 = 0.35;
    const EPSILON_ROWS: f32 = 0.01;
    const MIN_STEP_ROWS: f32 = 0.15;

    let remaining = target - scroll;
    if (remaining * page_rows).abs() < EPSILON_ROWS {
        return (target, false);
    }

    let step = (remaining.abs() * FACTOR)
        .max(MIN_STEP_ROWS / page_rows)
        .min(remaining.abs());
    (scroll + step.copysign(remaining), true)
}

#[cfg(test)]
mod tests {
    use super::{
        alternate_scroll_bytes, cell_at, ease, encode_key, font_step, popover_overflow,
        sgr_wheel_bytes, step_document_scroll, step_grid_scroll, step_popover_scroll,
        step_region_scroll, step_scrollback_scroll, wheel_lines, SCROLLBACK_MIN_STEP,
    };
    use stoatty_term::grid::{Overlay, Rgb};
    use winit::{
        dpi::PhysicalPosition,
        event::MouseScrollDelta,
        keyboard::{Key, NamedKey},
    };

    #[test]
    fn ease_steps_toward_then_settles() {
        let (next, settled) = ease([0.0, 0.0], [4.0, 0.0]);
        assert!(next[0] > 0.0 && next[0] < 4.0);
        assert!(!settled);

        let (next, settled) = ease([3.999, 2.0], [4.0, 2.0]);
        assert_eq!(next, [4.0, 2.0]);
        assert!(settled);
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
        let (next, down) = step_popover_scroll(0.0, true, 2.0);
        assert!(next > 0.0 && next < 2.0, "eases down from the top");
        assert!(down);

        let (next, down) = step_popover_scroll(1.999, true, 2.0);
        assert_eq!(next, 2.0, "snaps onto the bottom");
        assert!(!down, "reverses at the bottom");

        let (next, down) = step_popover_scroll(0.001, false, 2.0);
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
        // A LineDelta is lines directly and does not touch the accumulator.
        let mut pixels = 0.0;
        assert_eq!(
            wheel_lines(MouseScrollDelta::LineDelta(0.0, 3.0), &mut pixels, 20.0),
            3
        );
        assert_eq!(
            pixels, 0.0,
            "LineDelta leaves the pixel accumulator untouched"
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
    fn encode_key_maps_keys_to_terminal_bytes() {
        let named = |key| encode_key(&Key::Named(key), false);
        let printable = |s: &str| encode_key(&Key::Character(s.into()), false);

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
        let ctrl = |s: &str| encode_key(&Key::Character(s.into()), true);

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
    fn grid_scroll_eases_a_delta_to_zero() {
        // A new delta seeds the offset and starts easing down toward zero.
        let (next, easing) = step_grid_scroll(0.0, 3);
        assert!(next > 0.0 && next < 3.0, "eases from the seed");
        assert!(easing);

        // No new delta, within the snap epsilon: settles at zero.
        let (next, easing) = step_grid_scroll(0.005, 0);
        assert_eq!(next, 0.0, "snaps onto zero");
        assert!(!easing);
    }

    #[test]
    fn scrollback_scroll_eases_toward_a_target() {
        // A target deeper in history eases toward it without overshooting.
        let (next, easing) = step_scrollback_scroll(0.0, 4.0);
        assert!(next > 0.0 && next < 4.0, "eases toward the target");
        assert!(easing);

        // Within the snap epsilon of the target: settles on it.
        let (next, easing) = step_scrollback_scroll(3.999, 4.0);
        assert_eq!(next, 4.0, "snaps onto the target");
        assert!(!easing);

        // Near the target the per-frame step is floored so the tail does not
        // crawl: from twice the floor out it advances by the floor itself, not
        // the smaller geometric step.
        let (next, easing) = step_scrollback_scroll(0.0, SCROLLBACK_MIN_STEP * 2.0);
        assert!(
            (next - SCROLLBACK_MIN_STEP).abs() < 1e-5,
            "tail advances by the floor"
        );
        assert!(easing);
    }

    #[test]
    fn region_scroll_eases_a_signed_delta_to_zero() {
        // A positive delta (content scrolled down) seeds and eases toward zero.
        let (next, easing) = step_region_scroll(0.0, 3.0);
        assert!(next > 0.0 && next < 3.0, "eases from the positive seed");
        assert!(easing);

        // A negative delta (content scrolled up) eases up from below zero.
        let (next, easing) = step_region_scroll(0.0, -3.0);
        assert!(next < 0.0 && next > -3.0, "eases from the negative seed");
        assert!(easing);

        // No new delta, within the snap epsilon: settles at zero.
        let (next, easing) = step_region_scroll(0.005, 0.0);
        assert_eq!(next, 0.0, "snaps onto zero");
        assert!(!easing);
    }

    #[test]
    fn document_scroll_eases_toward_a_target() {
        // A target ahead of the live offset eases toward it without overshooting.
        let (next, easing) = step_document_scroll(0.0, 4.0, 20.0);
        assert!(next > 0.0 && next < 4.0, "eases toward the target");
        assert!(easing);

        // The row-sized min-step floor, capped at the remaining distance, lands
        // exactly on the whole-row target instead of snapping a visible fraction
        // of a row; the next frame then settles cleanly.
        let (next, easing) = step_document_scroll(4.0 - 0.001, 4.0, 20.0);
        assert_eq!(next, 4.0, "min-step lands exactly on the target");
        assert!(easing);

        // Already within a sub-pixel (in rows) of the target: settles on it.
        let (next, easing) = step_document_scroll(4.0 - 0.0001, 4.0, 20.0);
        assert_eq!(next, 4.0, "snaps onto the target");
        assert!(!easing);
    }
}
