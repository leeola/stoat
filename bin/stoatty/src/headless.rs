//! A stoatty session with no window and no event loop.
//!
//! A child runs on the PTY as it does under a window, and its frames render
//! into an offscreen texture. The caller drives the session. It types keys,
//! pumps the events the reader thread sends, waits for the screen to settle,
//! and reads a frame back as RGBA. This is what lets a scene render on a
//! machine with no display server, for a shot that a person or a test reads.
//!
//! The session holds the window path's [`State`], built by the same
//! [`State::new`], fed by the same [`pty_sink`], and drawn by the same
//! [`redraw`]. Only the host differs: it records what a frame asks of the
//! window instead of asking one.

pub mod script;

use crate::{
    app::{
        apply_font_step, earliest, flush_due_sync_update, grid_pixels, net_font_step, pty_sink,
        redraw, set_window_title, sync_row_anchor, type_press, update_cell_pixels, Host, PtyEvent,
        State, DEFAULT_TITLE,
    },
    cli::VERSION_INFO,
    config::CursorAnimation,
    pty::Pty,
};
use alacritty_terminal::sync::FairMutex;
use portable_pty::ExitStatus;
use script::Step;
use snafu::{Location, OptionExt, ResultExt, Snafu};
use std::{
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
        Arc, Mutex, PoisonError,
    },
    time::{Duration, Instant},
};
use stoat_log::ident;
use stoatty_protocol::{command::IdentReply, PROTOCOL_VERSION};
use stoatty_render::gpu::{headless_gpu, FontConfig, GpuContext};
use stoatty_term::{
    grid::{Grid, Rgb},
    term::{TermEvent, Terminal},
    theme::Theme,
};
use winit::window::{UserAttentionType, WindowId};
#[cfg(unix)]
use {
    crate::signals,
    std::{sync::mpsc::Sender, thread},
};

/// The pace frames draw at while one is wanted, the rate a 60 Hz display holds
/// the window path to.
const FRAME_TICK: Duration = Duration::from_millis(16);

/// The pause after each typed key, the gap the stoat fixture harness leaves
/// between keys.
const KEY_GAP: Duration = Duration::from_millis(20);

/// How long one settle step of a script waits for the screen to come to rest.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long reading the exit status waits for the waiter thread's reap.
const EXIT_STATUS_WAIT: Duration = Duration::from_millis(500);

/// What a headless session runs, and how it draws.
pub struct Scene {
    pub program: String,
    pub args: Vec<String>,
    /// The child's working directory, or the home directory when `None`.
    pub working_directory: Option<PathBuf>,
    /// The directory put first on the child's `PATH`, so a nested bare `stoat`
    /// resolves to the binary stoatty launched.
    pub stoat_dir: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    /// The editor font size, in logical points.
    pub font_size: u32,
    /// The font size of the terminal and agent panes a stoat child draws.
    pub terminal_font_size: u32,
    /// Physical pixels per logical point.
    pub scale_factor: f64,
    pub theme: Theme,
    /// The theme name the child gets as `STOAT_THEME`, empty for none.
    pub theme_name: String,
    pub font_family: Vec<String>,
    pub ligatures: bool,
    pub cursor_animation: CursorAnimation,
    /// Whether only a CPU adapter draws, so that a frame does not depend on
    /// the machine's GPU.
    pub software: bool,
    /// Environment entries the child gets after stoatty's own, such as XDG
    /// directories pointed at a scratch home.
    pub extra_env: Vec<(String, String)>,
}

/// A child on a PTY whose frames render offscreen, driven by its caller.
///
/// The reader thread parses the child's output as it arrives, but events apply
/// and frames draw only inside [`Self::pump`] and the calls built on it.
/// Dropping the session kills the child.
pub struct HeadlessSession {
    state: State,
    events: Receiver<PtyEvent>,
    host: Arc<HeadlessHost>,
    /// When the last frame drew, which paces the next at [`FRAME_TICK`].
    last_tick: Option<Instant>,
    /// How many redraw wakeups the reader has sent, which tells
    /// [`Self::wait_settled`] whether the screen moved in a slice.
    redraws_seen: u64,
    exit: Option<ExitStatus>,
    /// A sender onto [`Self::events`], for the thread that
    /// [`Self::forward_termination_signals`] starts.
    #[cfg(unix)]
    wake: Sender<PtyEvent>,
}

/// One frame read back from a session's texture.
pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    /// The frame's pixels, four bytes each in RGBA order, row-major with no
    /// padding.
    pub rgba: Vec<u8>,
}

/// Failure starting or driving a [`HeadlessSession`].
#[derive(Debug, Snafu)]
pub enum HeadlessError {
    #[snafu(display("no wgpu adapter answered for a headless render"))]
    NoAdapter {
        #[snafu(implicit)]
        location: Location,
    },

    #[snafu(display("failed to spawn the child over a pty"))]
    Spawn {
        source: io::Error,
        #[snafu(implicit)]
        location: Location,
    },

    #[snafu(display("timed out waiting for {step}"))]
    Timeout {
        step: String,
        #[snafu(implicit)]
        location: Location,
    },

    #[snafu(display("the child exited before the script ended"))]
    ChildExited {
        status: Option<ExitStatus>,
        #[snafu(implicit)]
        location: Location,
    },

    #[snafu(display("interrupted by signal {signal}"))]
    Interrupted {
        signal: i32,
        #[snafu(implicit)]
        location: Location,
    },
}

/// The [`Host`] a headless session's state draws for, which records what a
/// frame asks of the window instead of asking one.
struct HeadlessHost {
    redraw_wanted: AtomicBool,
    title: Mutex<String>,
    /// The texture's size in physical pixels, which [`HeadlessSession::resize`]
    /// moves.
    size: Mutex<(u32, u32)>,
}

impl HeadlessSession {
    /// Start `scene`'s child on a PTY of `scene.cols` by `scene.rows` cells,
    /// over an offscreen texture sized to hold that grid.
    ///
    /// Blocks on the GPU adapter and device requests. The first frame draws in
    /// the first pump.
    pub fn spawn(scene: Scene) -> Result<HeadlessSession, HeadlessError> {
        let scale = scene.scale_factor as f32;
        let (width, height) = {
            let (width, height) = grid_pixels(
                scene.font_size,
                scale,
                usize::from(scene.rows),
                usize::from(scene.cols),
            );
            (u32::from(width), u32::from(height))
        };
        let shared = headless_gpu(scene.software).context(NoAdapterSnafu)?;
        let gpu = GpuContext::offscreen(
            &shared,
            [width, height],
            FontConfig {
                size: scene.font_size,
                scale_factor: scale,
                family: &scene.font_family,
                ligatures: scene.ligatures,
            },
            scene.theme.background,
            scene.theme.cursor,
        );

        let (rows, cols) = gpu.grid_size();
        let terminal = Arc::new(FairMutex::new(Terminal::new(rows, cols, scene.theme)));
        update_cell_pixels(&terminal, scene.font_size, scene.terminal_font_size, scale);
        sync_row_anchor(&terminal, gpu.grid_layout());
        // Answered with an empty log id when this process names none, so a
        // child that asks who its terminal is still gets an answer.
        terminal.lock().set_ident(IdentReply {
            pid: std::process::id(),
            log_id: ident::get()
                .map(|current| current.id.to_string())
                .unwrap_or_default(),
            hostname: ident::hostname(),
            version: VERSION_INFO.to_string(),
            protocol: PROTOCOL_VERSION,
        });

        let dirty = Arc::new(AtomicBool::new(false));
        let sync_pending = Arc::new(AtomicBool::new(false));
        let ingest_at = Arc::new(AtomicU64::new(0));
        let ingest_epoch = Instant::now();
        let (wake, events) = mpsc::channel();

        let (pixel_width, pixel_height) = grid_pixels(scene.font_size, scale, rows, cols);
        let pty = Pty::spawn(
            &scene.program,
            &scene.args,
            scene.working_directory.as_deref(),
            scene.stoat_dir.as_deref(),
            ident::get().map(|current| current.id.as_str()),
            None,
            &scene.theme_name,
            rows as u16,
            cols as u16,
            pixel_width,
            pixel_height,
            &scene.extra_env,
            pty_sink(
                {
                    let wake = wake.clone();
                    move |event| {
                        let _ = wake.send(event);
                    }
                },
                terminal.clone(),
                dirty.clone(),
                sync_pending.clone(),
                ingest_at.clone(),
                ingest_epoch,
            ),
            {
                let wake = wake.clone();
                move || {
                    let _ = wake.send(PtyEvent::Exited {
                        last_output: String::new(),
                    });
                }
            },
        )
        .context(SpawnSnafu)?;

        let host = Arc::new(HeadlessHost::new((width, height)));
        let state = State::new(
            host.clone(),
            gpu,
            terminal,
            pty,
            Grid::new(rows, cols),
            dirty,
            sync_pending,
            ingest_at,
            ingest_epoch,
            None,
            None,
            Arc::new(AtomicBool::new(false)),
            scene.font_size,
            scene.terminal_font_size,
            scene.scale_factor,
            scene.cursor_animation,
            Instant::now(),
        );
        host.request_redraw();

        Ok(HeadlessSession {
            state,
            events,
            host,
            last_tick: None,
            redraws_seen: 0,
            exit: None,
            #[cfg(unix)]
            wake,
        })
    }

    /// Apply the child's output and draw the frames that come due until
    /// `until`.
    ///
    /// A frame draws at most once per [`FRAME_TICK`], and only while one is
    /// wanted. Fails with [`HeadlessError::ChildExited`] once the child exits,
    /// and with [`HeadlessError::Interrupted`] once a forwarded termination
    /// signal arrives.
    pub fn pump(&mut self, until: Instant) -> Result<(), HeadlessError> {
        loop {
            let now = Instant::now();
            if now >= until {
                return Ok(());
            }
            let deadline = self.next_wake(until);
            match self
                .events
                .recv_timeout(deadline.saturating_duration_since(now))
            {
                Ok(event) => self.apply(event)?,
                Err(RecvTimeoutError::Timeout) => {},
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            self.service();
        }
    }

    /// Play `steps` in order. Each key is typed and followed by a pump of
    /// [`KEY_GAP`], each wait pumps for its length, and each settle waits for
    /// the screen to come to rest.
    ///
    /// A shot step is the caller's to take, so it plays as nothing here.
    pub fn type_keys(&mut self, steps: &[Step]) -> Result<(), HeadlessError> {
        for step in steps {
            match step {
                Step::Key { key, mods } => {
                    type_press(&mut self.state, &script::press(key, *mods), *mods);
                    self.pump(Instant::now() + KEY_GAP)?;
                },
                Step::Wait(duration) => self.pump(Instant::now() + *duration)?,
                Step::Settle(hold) => self.wait_settled(*hold, STEP_TIMEOUT)?,
                Step::Shot(_) => {},
            }
        }
        Ok(())
    }

    /// Pump in slices of `hold` until one slice passes with the screen at rest.
    ///
    /// At rest means the last frame drawn shows content. It also means the
    /// slice saw no redraw wakeup, left no synchronized update pending, and
    /// ended with no frame wanted. Without content, a blank frame reads as at
    /// rest when a child starts slower than `hold`. The same happens when a
    /// child's terminal setup only clears the screen, as stoat's does before
    /// its first frame. Fails with [`HeadlessError::Timeout`] when the screen
    /// still moves, or shows nothing, after `timeout`.
    pub fn wait_settled(&mut self, hold: Duration, timeout: Duration) -> Result<(), HeadlessError> {
        let give_up = Instant::now() + timeout;
        loop {
            let seen = self.redraws_seen;
            self.pump(Instant::now() + hold)?;
            let background = self.state.terminal.lock().default_background();
            let at_rest = shows_content(&self.state.grid, background)
                && self.redraws_seen == seen
                && !self.state.sync_pending.load(Ordering::Relaxed)
                && !self.host.redraw_wanted.load(Ordering::Relaxed);
            if at_rest {
                return Ok(());
            }
            if Instant::now() >= give_up {
                return TimeoutSnafu { step: "settle" }.fail();
            }
        }
    }

    /// Draw the wanted frame, if one is wanted, and read the texture back.
    pub fn snapshot(&mut self) -> Result<Snapshot, HeadlessError> {
        if self.host.take_redraw() {
            redraw(&mut self.state);
            self.last_tick = Some(Instant::now());
        }
        let (width, height) = self.host.inner_size();
        let rgba = self
            .state
            .gpu
            .read_back()
            .expect("a headless session draws into an offscreen texture");
        Ok(Snapshot {
            width,
            height,
            rgba,
        })
    }

    /// Resize the session to `cols` by `rows` cells.
    ///
    /// The next frame fits the texture, the terminal, and the child to the new
    /// size.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (width, height) = {
            let (width, height) = grid_pixels(
                self.state.font_size,
                self.state.scale_factor as f32,
                usize::from(rows),
                usize::from(cols),
            );
            (u32::from(width), u32::from(height))
        };
        *self
            .host
            .size
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = (width, height);
        self.state.pending_resize.record(width, height);
        self.host.request_redraw();
    }

    /// The title the child last set, or the default before it sets one.
    pub fn title(&self) -> String {
        self.host
            .title
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The child's exit status, once a pump has seen the child exit.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit.clone()
    }

    /// Make a termination signal end the next pump with
    /// [`HeadlessError::Interrupted`].
    ///
    /// Starts a thread that takes the first SIGHUP, SIGINT, or SIGTERM. Call it
    /// only in a process that blocks those signals on every thread, as
    /// [`signals::block_termination`] does at the start of the stoatty binary.
    /// In a process that does not, a signal's default action ends the process
    /// first.
    #[cfg(unix)]
    pub fn forward_termination_signals(&self) -> io::Result<()> {
        let wake = self.wake.clone();
        thread::Builder::new()
            .name("signals".to_owned())
            .spawn(move || {
                if let Ok(signal) = signals::wait_termination() {
                    let _ = wake.send(PtyEvent::TerminationSignal(signal));
                }
            })
            .map(|_| ())
    }

    /// The earliest instant at which the session has work, no later than
    /// `until`.
    fn next_wake(&self, until: Instant) -> Instant {
        let frame = self.host.redraw_wanted.load(Ordering::Relaxed).then(|| {
            self.last_tick
                .map_or_else(Instant::now, |tick| tick + FRAME_TICK)
        });
        let sync = match self.state.sync_pending.load(Ordering::Relaxed) {
            true => self.state.terminal.lock().sync_deadline(),
            false => None,
        };
        let wake = earliest(
            earliest(frame, sync),
            earliest(self.state.popover_wake, self.state.sketch_wake),
        );
        wake.map_or(until, |at| at.min(until))
    }

    /// Act on one event that the reader or the exit waiter sent.
    fn apply(&mut self, event: PtyEvent) -> Result<(), HeadlessError> {
        match event {
            PtyEvent::Redraw => {
                // Cleared before the request, so a chunk parsed during this
                // cycle arms the next wakeup.
                self.state.dirty.store(false, Ordering::Relaxed);
                self.host.request_redraw();
                self.redraws_seen += 1;
            },
            PtyEvent::Term(events) => self.handle_events(events),
            PtyEvent::Exited { last_output } => {
                if !last_output.is_empty() {
                    tracing::info!(%last_output, "headless child exited");
                }
                self.exit = self.state.pty.exit_status(EXIT_STATUS_WAIT);
                return ChildExitedSnafu {
                    status: self.exit.clone(),
                }
                .fail();
            },
            PtyEvent::TerminationSignal(signal) => return InterruptedSnafu { signal }.fail(),
            // A session opens no aux window and starts no font scan, so neither
            // of these arrives.
            PtyEvent::FontsScanned(_) | PtyEvent::AuxGpuReady { .. } => {},
        }
        Ok(())
    }

    /// Flush a due synchronized update, wake a due popover or sketch, and draw
    /// the wanted frame once its tick has passed.
    fn service(&mut self) {
        if self.state.sync_pending.load(Ordering::Relaxed) {
            let (deadline, events) = flush_due_sync_update(&mut self.state);
            if !events.is_empty() {
                self.handle_events(events);
            }
            if deadline.is_none() {
                self.state.sync_pending.store(false, Ordering::Relaxed);
                self.host.request_redraw();
            }
        }

        let now = Instant::now();
        if self.state.popover_wake.is_some_and(|at| at <= now) {
            self.state.popover_wake = None;
            self.host.request_redraw();
        }
        if self.state.sketch_wake.is_some_and(|at| at <= now) {
            self.state.sketch_wake = None;
            self.host.request_redraw();
        }

        let tick_due = self.last_tick.is_none_or(|tick| now >= tick + FRAME_TICK);
        if tick_due && self.host.take_redraw() {
            redraw(&mut self.state);
            self.last_tick = Some(now);
        }
    }

    /// Apply the host events a parse raised, for each one a session with no
    /// window acts on as the window path does.
    fn handle_events(&mut self, mut events: Vec<TermEvent>) {
        let font_step = net_font_step(&mut events);

        for event in events {
            match event {
                TermEvent::Title(title) => {
                    set_window_title(self.host.as_ref(), &mut self.state.last_title, &title)
                },
                TermEvent::ResetTitle => set_window_title(
                    self.host.as_ref(),
                    &mut self.state.last_title,
                    DEFAULT_TITLE,
                ),
                TermEvent::ZoomCapture { on, inband } => {
                    self.state.zoom_capture = on;
                    self.state.zoom_inband = inband;
                },
                // The claim belonged to the program that just left, as in the
                // window path.
                TermEvent::AltScreenLeft => {
                    self.state.zoom_capture = false;
                    self.state.zoom_inband = false;
                },
                TermEvent::WindowOpen(_) => {
                    tracing::warn!("a headless session opens no aux window")
                },
                other => tracing::debug!(?other, "a headless session ignores this event"),
            }
        }

        if font_step != 0 {
            apply_font_step(&mut self.state, font_step);
        }
    }
}

impl HeadlessHost {
    fn new(size: (u32, u32)) -> HeadlessHost {
        HeadlessHost {
            redraw_wanted: AtomicBool::new(false),
            title: Mutex::new(DEFAULT_TITLE.to_owned()),
            size: Mutex::new(size),
        }
    }

    /// Whether a frame was asked for since the last take, clearing the request.
    fn take_redraw(&self) -> bool {
        self.redraw_wanted.swap(false, Ordering::Relaxed)
    }
}

impl Host for HeadlessHost {
    fn request_redraw(&self) {
        self.redraw_wanted.store(true, Ordering::Relaxed);
    }

    fn set_title(&self, title: &str) {
        title.clone_into(&mut self.title.lock().unwrap_or_else(PoisonError::into_inner));
    }

    fn request_user_attention(&self, _request: Option<UserAttentionType>) {}

    fn inner_size(&self) -> (u32, u32) {
        *self.size.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn window_id(&self) -> Option<WindowId> {
        None
    }
}

/// Whether `grid` holds anything an erased screen does not.
///
/// That is a cell with a character other than a space, or a cell drawn on a
/// background other than `background`, the terminal's default.
fn shows_content(grid: &Grid, background: Rgb) -> bool {
    (0..grid.rows()).any(|row| {
        grid.row(row)
            .iter()
            .any(|cell| cell.ch != ' ' || cell.draw_colors().1 != background)
    })
}

#[cfg(test)]
mod tests {
    use super::{shows_content, HeadlessError, HeadlessHost, HeadlessSession, Scene};
    use crate::{
        app::{grid_pixels, Host},
        config::CursorAnimation,
    };
    use std::{
        path::Path,
        time::{Duration, Instant},
    };
    use stoatty_term::{
        grid::{Cell, Flags, Grid, Rgb},
        theme::Theme,
    };

    /// A frame asks the host for the next frame and sets the title through it,
    /// so the host keeps both for the session to read.
    #[test]
    fn a_headless_host_records_a_redraw_request_and_the_title() {
        let host = HeadlessHost::new((40, 20));
        let title = |host: &HeadlessHost| host.title.lock().unwrap().clone();
        let fresh = title(&host);

        host.request_redraw();
        host.set_title("vim");
        assert_eq!(
            (
                fresh,
                host.take_redraw(),
                host.take_redraw(),
                title(&host),
                host.inner_size(),
                host.window_id(),
            ),
            (
                "stoatty".to_owned(),
                true,
                false,
                "vim".to_owned(),
                (40, 20),
                None
            ),
            "the default title, one redraw taken once, the set title, the size, no window",
        );
    }

    /// The child's output reaches the texture and the title. A cell the child
    /// paints with a truecolor background reads back in that color, and a
    /// resize reads back at the new size.
    ///
    /// The child enters and clears the alternate screen at once, which draws
    /// nothing, and paints only after a pause longer than the hold. So the wait
    /// has to see content on the screen before it takes a quiet slice for rest.
    #[test]
    fn a_session_renders_the_child_s_background_fill() {
        let dir = tempfile::tempdir().expect("a scratch working directory");
        let mut session = spawn(shell_scene(
            "printf '\\033[?1049h\\033[2J'; sleep 0.3; \
             printf '\\033]0;scene\\007\\033[48;2;200;40;40m  '; sleep 60",
            dir.path(),
        ));

        session
            .wait_settled(Duration::from_millis(100), Duration::from_secs(10))
            .expect("the screen comes to rest");
        let shot = session.snapshot().expect("a snapshot");
        let at = 4 * (shot.width as usize + 1);
        assert_eq!(
            (session.title(), &shot.rgba[at..at + 3]),
            ("scene".to_owned(), &[200, 40, 40][..]),
            "the title the child set, and the pixel one in from the corner of cell (0, 0)",
        );

        session.resize(10, 2);
        let resized = session.snapshot().expect("a snapshot");
        let (width, height) = grid_pixels(14, 1.0, 2, 10);
        assert_eq!(
            (resized.width, resized.height, resized.rgba.len()),
            (
                u32::from(width),
                u32::from(height),
                usize::from(width) * usize::from(height) * 4,
            ),
            "a resized session reads back at the new size",
        );
    }

    /// A grid shows content once one cell holds a character or draws a
    /// background other than the default, and an erased grid shows none.
    #[test]
    fn a_grid_shows_content_by_character_or_background() {
        let background = Rgb::new(0x28, 0x2c, 0x34);
        let shows = |paint: fn(&mut Cell)| {
            let mut grid = Grid::new(2, 3);
            for row in 0..2 {
                grid.row_mut(row).fill(Cell {
                    bg: background,
                    ..Cell::default()
                });
            }
            paint(grid.get_mut(1, 2));
            shows_content(&grid, background)
        };
        assert_eq!(
            [
                shows(|_| {}),
                shows(|cell| cell.ch = 'x'),
                shows(|cell| cell.bg = Rgb::new(200, 40, 40)),
                shows(|cell| cell.flags = Flags::INVERSE),
            ],
            [false, true, true, true],
            "an erased grid, a character, a painted background, reverse video",
        );
    }

    /// A child that exits ends the pump with its status, which the session
    /// keeps for the caller.
    #[test]
    fn a_session_reports_the_child_s_exit() {
        let dir = tempfile::tempdir().expect("a scratch working directory");
        let mut session = spawn(shell_scene("exit 3", dir.path()));

        let reported = match session.pump(Instant::now() + Duration::from_secs(10)) {
            Err(HeadlessError::ChildExited { status, .. }) => {
                status.map(|status| status.exit_code())
            },
            other => panic!("the pump ends with the child's exit, not {other:?}"),
        };
        assert_eq!(
            (
                reported,
                session.exit_status().map(|status| status.exit_code())
            ),
            (Some(3), Some(3)),
            "the error and the session both carry the child's exit code",
        );
    }

    /// A `/bin/sh -c script` scene of 20 by 4 cells at a 14-point font, run in
    /// `dir`.
    fn shell_scene(script: &str, dir: &Path) -> Scene {
        Scene {
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), script.to_owned()],
            working_directory: Some(dir.to_owned()),
            stoat_dir: None,
            cols: 20,
            rows: 4,
            font_size: 14,
            terminal_font_size: 14,
            scale_factor: 1.0,
            theme: Theme::default(),
            theme_name: String::new(),
            font_family: vec!["JetBrains Mono".to_owned()],
            ligatures: true,
            cursor_animation: CursorAnimation::default(),
            software: false,
            extra_env: Vec::new(),
        }
    }

    /// Start `scene`, failing the test with a hint when no adapter answers.
    fn spawn(scene: Scene) -> HeadlessSession {
        HeadlessSession::spawn(scene).unwrap_or_else(|error| {
            panic!(
                "{error}. With no wgpu adapter, enter the devshell with `nix develop`, which \
                 registers lavapipe, or install a Vulkan driver"
            )
        })
    }
}
