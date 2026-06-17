//! The winit application: owns the window, the PTY shell, and the event loop.
//!
//! The reader thread forwards shell output as [`PtyEvent`]s onto the loop, which
//! feeds them to a [`Terminal`], projects the parsed screen onto a [`Grid`], and
//! drives [`stoatty_render`] to draw it. This is the windowing boundary: the
//! window lives here and [`stoatty_render`] receives only its handle, keeping
//! the renderer toolkit-agnostic.

use crate::{
    config,
    pty::{self, Pty, PtyOutput},
};
use std::sync::Arc;
use stoatty_render::gpu::{GpuContext, Scroll};
use stoatty_term::{
    grid::Grid,
    term::{Cursor, CursorShape, Terminal},
    theme::Theme,
};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState},
    window::{Window, WindowId},
};

/// Smallest font size the live zoom allows, so cells never collapse to an
/// unreadable size.
const FONT_SIZE_FLOOR: u32 = 6;

/// Open the stoatty window running the user's default shell.
///
/// Blocks the calling thread for the lifetime of the window. See
/// [`run_with_shell`] for the behavior and for running a specific command.
pub fn run() {
    run_with_shell(pty::default_shell());
}

/// Open the stoatty window running `shell` as the PTY command, and run the
/// event loop until the window closes or that command exits.
///
/// Blocks the calling thread for the lifetime of the window. The loop is
/// idle-driven (`ControlFlow::Wait`): frames are drawn on demand when PTY
/// output arrives or the window is resized, not on a continuous timer.
pub fn run_with_shell(shell: String) {
    let config = config::load().expect("load config");
    let theme = config.resolve_theme();

    let event_loop = EventLoop::<PtyEvent>::with_user_event()
        .build()
        .expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App::new(event_loop.create_proxy(), shell, theme, config.font_size);
    event_loop.run_app(&mut app).expect("run event loop");
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

struct App {
    proxy: EventLoopProxy<PtyEvent>,
    shell: String,
    theme: Theme,
    font_size: u32,
    state: Option<State>,
}

impl App {
    fn new(proxy: EventLoopProxy<PtyEvent>, shell: String, theme: Theme, font_size: u32) -> App {
        App {
            proxy,
            shell,
            theme,
            font_size,
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
    /// The live font size in pixels, seeded from the config and stepped by the
    /// platform zoom combo. Drives the renderer's cell metrics on each change.
    font_size: u32,
    /// The most recent modifier state, tracked from `ModifiersChanged` so a key
    /// press can tell whether the platform zoom modifier is held.
    modifiers: ModifiersState,
    /// The cursor's animated position in fractional cell coordinates, eased
    /// toward the terminal's actual cursor cell each frame.
    cursor_anim: [f32; 2],
    /// The popover content's eased vertical scroll offset, in rows. Ping-pongs
    /// between the top and the overflow bottom while a popover overflows its box.
    popover_scroll: f32,
    /// Ping-pong direction: true while the scroll eases down toward the overflow
    /// bottom, false while easing back up to the top.
    popover_scroll_down: bool,
    /// The grid's eased vertical scroll offset, in rows. Seeded by the term's
    /// per-frame scroll delta and eased toward zero so content glides into place.
    grid_scroll: f32,
}

impl ApplicationHandler<PtyEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }

        let window = Arc::new(
            event_loop
                .create_window(Window::default_attributes().with_title("stoatty"))
                .expect("create window"),
        );

        let size = window.inner_size();
        let gpu = GpuContext::new(
            window.clone(),
            size.width.max(1),
            size.height.max(1),
            self.font_size,
            self.theme.background,
            self.theme.cursor,
        );

        let (rows, cols) = gpu.grid_size();
        let grid = Grid::new(rows, cols);
        let terminal = Terminal::new(rows, cols, self.theme);

        let pty = {
            let proxy = self.proxy.clone();
            Pty::spawn(&self.shell, rows as u16, cols as u16, move |output| {
                let event = match output {
                    PtyOutput::Data(bytes) => PtyEvent::Output(bytes),
                    PtyOutput::Eof => PtyEvent::Exited,
                };
                let _ = proxy.send_event(event);
            })
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
            modifiers: ModifiersState::empty(),
            cursor_anim: [0.0, 0.0],
            popover_scroll: 0.0,
            popover_scroll_down: true,
            grid_scroll: 0.0,
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: PtyEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        match event {
            PtyEvent::Output(bytes) => {
                state.terminal.advance(&bytes);
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
            WindowEvent::RedrawRequested => {
                let (cursor, scroll_delta) = state.terminal.project(&mut state.grid);

                let popover_scrolling = match popover_overflow(&state.grid) {
                    Some(max) => {
                        let (next, down) = step_popover_scroll(
                            state.popover_scroll,
                            state.popover_scroll_down,
                            max,
                        );
                        state.popover_scroll = next;
                        state.popover_scroll_down = down;
                        true
                    },
                    None => {
                        state.popover_scroll = 0.0;
                        false
                    },
                };

                let (grid_scroll, grid_scrolling) =
                    step_grid_scroll(state.grid_scroll, scroll_delta);
                state.grid_scroll = grid_scroll;

                let cursor_easing = match cursor_position(cursor) {
                    Some(target) => {
                        let (next, settled) = ease(state.cursor_anim, target);
                        state.cursor_anim = next;
                        state.gpu.render(
                            &state.grid,
                            Some(next),
                            Scroll {
                                popover: state.popover_scroll,
                                grid: state.grid_scroll,
                            },
                        );
                        !settled
                    },
                    None => {
                        state.gpu.render(
                            &state.grid,
                            None,
                            Scroll {
                                popover: state.popover_scroll,
                                grid: state.grid_scroll,
                            },
                        );
                        false
                    },
                };

                // Keep the vsync-paced loop running while the cursor eases, a
                // popover scrolls, or the grid scrolls. When all settle the loop
                // idles until the next PTY output or resize.
                if cursor_easing || popover_scrolling || grid_scrolling {
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

                let Some(delta) = font_step(platform_mod_held, &event.logical_key) else {
                    return;
                };

                let font_size = (state.font_size as i32 + delta).max(FONT_SIZE_FLOOR as i32) as u32;
                state.font_size = font_size;
                state.gpu.set_font_size(font_size);

                // The surface is unchanged, so skip `gpu.resize`; only the cell
                // metrics moved, so re-read the grid size and resize the rest.
                let (rows, cols) = state.gpu.grid_size();
                state.terminal.resize(rows, cols);
                let _ = state.pty.resize(rows as u16, cols as u16);

                state.window.request_redraw();
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
/// is. With it held, `+` steps up by one and `-` steps down by one.
fn font_step(platform_mod_held: bool, key: &Key) -> Option<i32> {
    if !platform_mod_held {
        return None;
    }

    match key {
        Key::Character(s) if s.as_str() == "+" => Some(1),
        Key::Character(s) if s.as_str() == "-" => Some(-1),
        _ => None,
    }
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

/// The single popover's overflow height in rows, or `None` when there is not
/// exactly one popover or it fits its box.
///
/// This is how far the content can scroll: the line count beyond the box height.
fn popover_overflow(grid: &Grid) -> Option<f32> {
    let [overlay] = grid.overlays() else {
        return None;
    };

    let lines = overlay.content.lines().count();
    let height = overlay.height as usize;
    (lines > height).then(|| (lines - height) as f32)
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

#[cfg(test)]
mod tests {
    use super::{ease, font_step, popover_overflow, step_grid_scroll, step_popover_scroll};
    use stoatty_term::grid::{Grid, Overlay, Rgb};
    use winit::keyboard::Key;

    #[test]
    fn ease_steps_toward_then_settles() {
        let (next, settled) = ease([0.0, 0.0], [4.0, 0.0]);
        assert!(next[0] > 0.0 && next[0] < 4.0);
        assert!(!settled);

        let (next, settled) = ease([3.999, 2.0], [4.0, 2.0]);
        assert_eq!(next, [4.0, 2.0]);
        assert!(settled);
    }

    fn popover(height: u16, content: &str) -> Overlay {
        Overlay {
            top: 0,
            left: 0,
            width: 4,
            height,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(0, 0, 0),
            content: content.to_owned(),
        }
    }

    #[test]
    fn popover_overflow_reports_rows_past_the_box() {
        let mut grid = Grid::new(10, 10);

        grid.set_overlays(vec![popover(2, "a\nb\nc\nd")]);
        assert_eq!(popover_overflow(&grid), Some(2.0));

        grid.set_overlays(vec![popover(4, "a\nb")]);
        assert_eq!(popover_overflow(&grid), None, "fits the box");

        grid.set_overlays(vec![popover(2, "x"), popover(2, "y")]);
        assert_eq!(popover_overflow(&grid), None, "not a single popover");

        grid.set_overlays(vec![]);
        assert_eq!(popover_overflow(&grid), None, "no popover");
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
        assert_eq!(font_step(true, &Key::Character("+".into())), Some(1));
        assert_eq!(font_step(true, &Key::Character("-".into())), Some(-1));
        assert_eq!(
            font_step(false, &Key::Character("+".into())),
            None,
            "no platform modifier held"
        );
        assert_eq!(
            font_step(true, &Key::Character("a".into())),
            None,
            "unrelated key"
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
}
