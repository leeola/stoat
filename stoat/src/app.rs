//! Application entry point for the GPUI-based GUI.

use crate::workspace::Workspace;
use gpui::{
    actions, prelude::*, px, size, App, Application, Bounds, KeyBinding, WindowBounds,
    WindowOptions,
};

actions!(stoat, [Quit]);

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(|cx: &mut App| {
        cx.bind_keys([KeyBinding::new("Escape", Quit, None)]);

        cx.on_action(|_: &Quit, cx: &mut App| {
            cx.quit();
        });

        let window_size = cx
            .primary_display()
            .map(|display| {
                let screen = display.bounds().size;
                size(screen.width * 0.8, screen.height * 0.8)
            })
            .unwrap_or_else(|| size(px(1200.0), px(800.0)));

        let bounds = Bounds::centered(None, window_size, cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_cx| Workspace::new()),
        )
        .expect("failed to open window");

        cx.on_window_closed(|cx| {
            cx.quit();
        })
        .detach();

        cx.activate(true);
    });

    Ok(())
}
