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
use stoatty_render::gpu::GpuContext;
use stoatty_term::{
    grid::Grid,
    term::{Cursor, CursorShape, Terminal},
    theme::Theme,
};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::{Window, WindowId},
};

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
    let theme = config::load().expect("load config").resolve_theme();

    let event_loop = EventLoop::<PtyEvent>::with_user_event()
        .build()
        .expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App::new(event_loop.create_proxy(), shell, theme);
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
    state: Option<State>,
}

impl App {
    fn new(proxy: EventLoopProxy<PtyEvent>, shell: String, theme: Theme) -> App {
        App {
            proxy,
            shell,
            theme,
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
    /// The cursor's animated position in fractional cell coordinates, eased
    /// toward the terminal's actual cursor cell each frame.
    cursor_anim: [f32; 2],
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
            cursor_anim: [0.0, 0.0],
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
                let cursor = state.terminal.project(&mut state.grid);

                let animating = match cursor_position(cursor) {
                    Some(target) => {
                        let (next, settled) = ease(state.cursor_anim, target);
                        state.cursor_anim = next;
                        state.gpu.render(&state.grid, Some(next), 0.0);
                        !settled
                    },
                    None => {
                        state.gpu.render(&state.grid, None, 0.0);
                        false
                    },
                };

                // Keep the vsync-paced loop running while the cursor eases; when
                // it settles the loop idles until the next PTY output or resize.
                if animating {
                    state.window.request_redraw();
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

#[cfg(test)]
mod tests {
    use super::ease;

    #[test]
    fn ease_steps_toward_then_settles() {
        let (next, settled) = ease([0.0, 0.0], [4.0, 0.0]);
        assert!(next[0] > 0.0 && next[0] < 4.0);
        assert!(!settled);

        let (next, settled) = ease([3.999, 2.0], [4.0, 2.0]);
        assert_eq!(next, [4.0, 2.0]);
        assert!(settled);
    }
}
