mod actions;
mod buffer;
mod editor;
mod element;
mod theme;
mod vim;

use anyhow::Result;
use editor::Editor;
use gpui::{
    prelude::*, px, size, App, Application, Bounds, Context, SharedString, Window, WindowBounds,
    WindowOptions,
};
use theme::ThemeSettings;
use tracing_subscriber::{self, EnvFilter};

pub fn run() -> Result<()> {
    // Run the GPUI application
    Application::new().run(|cx: &mut App| {
        // Initialize global theme settings
        cx.set_global(ThemeSettings::new());

        // Register key bindings
        editor::register_key_bindings(cx);

        // Create the main window
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Stoat Editor".into()),
                    ..Default::default()
                }),
                focus: true,
                ..Default::default()
            },
            |_window, cx| {
                // Create the editor with some initial text
                cx.new(|cx| {
                    Editor::new(
                        Some("Welcome to Stoat Editor!\n\nA high-performance modal text editor built with GPUI.\n\nPress 'i' to enter insert mode, 'Esc' to return to normal mode.\nUse h/j/k/l to navigate in normal mode.".to_string()),
                        cx,
                    )
                })
            },
        )
        .unwrap();

        // Activate the application
        cx.activate(true);
    });

    Ok(())
}
