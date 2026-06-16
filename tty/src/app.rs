//! The winit application: owns the window, the PTY shell, and the event loop.
//!
//! The reader thread forwards shell output as [`PtyEvent`]s onto the loop, which
//! feeds them to a [`Terminal`], projects the parsed screen onto a [`Grid`], and
//! drives [`stoatty_render`] to draw it. This is the windowing boundary: the
//! window lives here and [`stoatty_render`] receives only its handle, keeping
//! the renderer toolkit-agnostic.

use crate::pty::{self, Pty, PtyOutput};
use std::sync::Arc;
use stoatty_render::gpu::GpuContext;
use stoatty_term::{grid::Grid, term::Terminal};
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
    let event_loop = EventLoop::<PtyEvent>::with_user_event()
        .build()
        .expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App::new(event_loop.create_proxy(), shell);
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
    state: Option<State>,
}

impl App {
    fn new(proxy: EventLoopProxy<PtyEvent>, shell: String) -> App {
        App {
            proxy,
            shell,
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
        let gpu = GpuContext::new(window.clone(), size.width.max(1), size.height.max(1));

        let (rows, cols) = gpu.grid_size();
        let grid = Grid::new(rows, cols);
        let terminal = Terminal::new(rows, cols);

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
                state.terminal.project(&mut state.grid);
                state.gpu.render(&state.grid);
            },
            _ => {},
        }
    }
}
