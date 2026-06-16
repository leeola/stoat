//! The winit application: owns the window and event loop, and drives
//! [`stoatty_render`]'s GPU context to clear the surface each frame.
//!
//! This is the windowing boundary. The window lives here; [`stoatty_render`]
//! receives only its handle, keeping the renderer toolkit-agnostic.

use std::sync::Arc;
use stoatty_render::gpu::GpuContext;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};

/// Open the stoatty window and run the event loop until the window closes.
///
/// Blocks the calling thread for the lifetime of the window. The loop is
/// idle-driven (`ControlFlow::Wait`): frames are drawn on demand in response
/// to resize and redraw requests, not on a continuous timer.
pub fn run() {
    let event_loop = EventLoop::new().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App::default();
    event_loop.run_app(&mut app).expect("run event loop");
}

#[derive(Default)]
struct App {
    state: Option<State>,
}

struct State {
    window: Arc<Window>,
    gpu: GpuContext,
}

impl ApplicationHandler for App {
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

        window.request_redraw();
        self.state = Some(State { window, gpu });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                state.gpu.resize(size.width, size.height);
                state.window.request_redraw();
            },
            WindowEvent::RedrawRequested => state.gpu.render(),
            _ => {},
        }
    }
}
