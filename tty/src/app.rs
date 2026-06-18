//! The winit application: owns the window, the PTY shell, and the event loop.
//!
//! The reader thread forwards shell output as [`PtyEvent`]s onto the loop, which
//! feeds them to a [`Terminal`], projects the parsed screen onto a [`Grid`], and
//! drives [`stoatty_render`] to draw it. This is the windowing boundary: the
//! window lives here and [`stoatty_render`] receives only its handle, keeping
//! the renderer toolkit-agnostic.

use crate::{
    config::{self, Config},
    pty::{self, Pty, PtyOutput},
};
use std::sync::Arc;
use stoatty_render::{
    gpu::{FontConfig, GpuContext, Scroll},
    render,
};
use stoatty_term::{
    grid::{Grid, Overlay},
    term::{Cursor, CursorShape, Terminal},
    theme::Theme,
};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{Window, WindowId},
};

/// Smallest font size the live zoom allows, so cells never collapse to an
/// unreadable size.
const FONT_SIZE_FLOOR: u32 = 6;

/// Open the stoatty window running the configured command, or the user's
/// default shell when the config sets none, at the winit default window size.
///
/// Reads the `[shell]` config override for the program and its arguments,
/// falling back to the default shell with no arguments. Blocks the calling
/// thread for the lifetime of the window. See [`run_with_shell`] to force a
/// specific command instead.
pub fn run() {
    let mut config = load_config();
    let (program, args) = match config.shell.take() {
        Some(shell) => (shell.program, shell.args),
        None => (pty::default_shell(), Vec::new()),
    };
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
/// The PTY reader runs off the main thread, so it cannot touch the terminal
/// directly; it sends these through the [`EventLoopProxy`] instead, which wakes
/// the idle loop.
enum PtyEvent {
    Output(Vec<u8>),
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
    terminal: Terminal,
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
    /// The scroll region's eased vertical offset, in rows. Seeded by the change
    /// in the region's declared offset and eased toward zero, so the region's
    /// content glides when the program scrolls it.
    region_scroll: f32,
    /// The scroll region's declared offset at the previous frame, so the next
    /// one can seed the ease with the change since.
    last_region_offset: f32,
}

impl ApplicationHandler<PtyEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }

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
        let terminal = Terminal::new(rows, cols, self.theme);

        let pty = {
            let proxy = self.proxy.clone();
            Pty::spawn(
                &self.program,
                &self.args,
                rows as u16,
                cols as u16,
                move |output| {
                    let event = match output {
                        PtyOutput::Data(bytes) => PtyEvent::Output(bytes),
                        PtyOutput::Eof => PtyEvent::Exited,
                    };
                    let _ = proxy.send_event(event);
                },
            )
            .expect("spawn shell over pty")
        };

        window.request_redraw();
        self.state = Some(State {
            window,
            gpu,
            terminal,
            grid,
            pty,
            font_size: self.font_size,
            scale_factor,
            modifiers: ModifiersState::empty(),
            cursor_anim: [0.0, 0.0],
            popover_scrolls: Vec::new(),
            popover_scroll_downs: Vec::new(),
            grid_scroll: 0.0,
            region_scroll: 0.0,
            last_region_offset: 0.0,
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: PtyEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        match event {
            PtyEvent::Output(bytes) => {
                state.terminal.advance(&bytes);

                let responses = state.terminal.take_responses();
                if !responses.is_empty() {
                    let _ = state.pty.write(&responses);
                }

                state.window.request_redraw();
            },
            PtyEvent::Exited => event_loop.exit(),
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
                state.terminal.resize(rows, cols);
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
                state.terminal.resize(rows, cols);
                let _ = state.pty.resize(rows as u16, cols as u16);

                state.window.request_redraw();
            },
            WindowEvent::RedrawRequested => {
                let (cursor, scroll_delta, _damage) = state.terminal.project(&mut state.grid);

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

                let cursor_easing = match cursor_position(cursor) {
                    Some(target) => {
                        let (next, settled) = ease(state.cursor_anim, target);
                        state.cursor_anim = next;
                        state.gpu.render(
                            &state.grid,
                            Some(next),
                            Scroll {
                                grid: state.grid_scroll,
                                region: state.region_scroll,
                                popovers: &state.popover_scrolls,
                            },
                        );
                        !settled
                    },
                    None => {
                        state.gpu.render(
                            &state.grid,
                            None,
                            Scroll {
                                grid: state.grid_scroll,
                                region: state.region_scroll,
                                popovers: &state.popover_scrolls,
                            },
                        );
                        false
                    },
                };

                // Keep the vsync-paced loop running while the cursor eases, a
                // popover scrolls, or the grid or a region scrolls. When all
                // settle the loop idles until the next PTY output or resize.
                if cursor_easing || popover_scrolling || grid_scrolling || region_scrolling {
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
                    state.terminal.resize(rows, cols);
                    let _ = state.pty.resize(rows as u16, cols as u16);

                    state.window.request_redraw();
                    return;
                }

                if let Some(bytes) = encode_key(&event.logical_key, state.modifiers.control_key()) {
                    let _ = state.pty.write(&bytes);
                }
            },
            _ => {},
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

#[cfg(test)]
mod tests {
    use super::{
        ease, encode_key, font_step, popover_overflow, step_grid_scroll, step_popover_scroll,
        step_region_scroll,
    };
    use stoatty_term::grid::{Overlay, Rgb};
    use winit::keyboard::{Key, NamedKey};

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
}
