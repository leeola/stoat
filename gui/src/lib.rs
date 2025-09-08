mod editor;
mod theme;

use anyhow::Result;
use editor::Editor;
use gpui::{
    prelude::*, px, size, App, Application, Bounds, Focusable, WindowBounds, WindowOptions,
};
use std::process;
use theme::ThemeSettings;
use tracing::info;

pub fn run() -> Result<()> {
    info!("Starting Stoat GUI");

    // Run the GPUI application
    Application::new().run(|cx: &mut App| {
        // Initialize global theme settings
        cx.set_global(ThemeSettings::new());

        // Create the main window
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("Stoat GUI - Keyboard Observer".into()),
                        ..Default::default()
                    }),
                    focus: true,
                    ..Default::default()
                },
                |window, cx| {
                    cx.activate(false);
                    // Create the simple keyboard observer
                    let editor = cx.new(|cx| Editor::new(cx));
                    // Focus it immediately
                    editor.focus_handle(cx).focus(window);
                    editor
                },
            )
            .unwrap();

        // Get the editor entity for keystroke observation
        let editor = window.update(cx, |_, _, cx| cx.entity()).unwrap();

        // Observe all keystrokes and forward to the editor
        cx.observe_keystrokes(move |event, _, cx| {
            info!("Keystroke observed in lib.rs: {:?}", event.keystroke);
            window
                .update(cx, |_, window, cx| {
                    editor.update(cx, |editor, cx| {
                        editor.on_keystroke(event.keystroke.clone(), window, cx)
                    })
                })
                .ok();
        })
        .detach();

        // Force exit when window closes - use detach() to avoid warning
        cx.on_window_closed(|_cx| {
            info!("Window closed, exiting");
            process::exit(0);
        })
        .detach();
    });

    Ok(())
}
