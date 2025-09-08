mod buffer_view;
mod editor_view;
mod help_dialog;
mod stoat_bridge;
mod theme;

use anyhow::Result;
use editor_view::EditorView;
use gpui::{
    prelude::*, px, size, App, Application, Bounds, Focusable, WindowBounds, WindowOptions,
};
use theme::ThemeSettings;
use tracing::info;

pub fn run() -> Result<()> {
    info!("Starting Stoat GUI with integrated editor");

    // Run the GPUI application
    Application::new().run(|cx: &mut App| {
        // Initialize global theme settings
        cx.set_global(ThemeSettings::new());

        // Create the main window with the editor view
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
            |window, cx| {
                // Activate the window to bring it to foreground
                cx.activate(true);

                // Create the editor view with some sample text
                let editor = cx.new(|cx| {
                    EditorView::with_text(
                        "Welcome to Stoat Editor!\n\nPress 'i' to enter insert mode.\nPress 'Esc' to return to normal mode.\nPress ':q' to quit.",
                        cx,
                    )
                });

                // Focus the editor immediately
                editor.focus_handle(cx).focus(window);
                editor
            },
        )
        .unwrap();
    });

    Ok(())
}
