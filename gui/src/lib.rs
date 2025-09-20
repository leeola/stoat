mod buffer_view;
mod components;
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
    run_with_stoat(None)
}

pub fn run_with_stoat(stoat: Option<stoat::Stoat>) -> Result<()> {
    info!("Starting Stoat GUI with integrated editor");

    // Run the GPUI application
    Application::new().run(move |cx: &mut App| {
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
            move |window, cx| {
                // Activate the window to bring it to foreground
                cx.activate(true);

                // Create the editor view with either loaded content or default text
                let editor = cx.new(|cx| {
                    if let Some(stoat) = stoat {
                        let content = stoat.buffer_contents();
                        if !content.is_empty() {
                            info!("Creating editor view with {} characters from loaded file", content.len());
                            EditorView::with_text(&content, cx)
                        } else {
                            EditorView::with_text(
                                "Welcome to Stoat Editor!\n\nPress 'i' to enter insert mode.\nPress 'Esc' to return to normal mode.\nPress ':q' to quit.",
                                cx,
                            )
                        }
                    } else {
                        EditorView::with_text(
                            "Welcome to Stoat Editor!\n\nPress 'i' to enter insert mode.\nPress 'Esc' to return to normal mode.\nPress ':q' to quit.",
                            cx,
                        )
                    }
                });

                // Focus the editor immediately
                editor.focus_handle(cx).focus(window);
                editor
            },
        )
        .expect("Failed to create window");
    });

    Ok(())
}
