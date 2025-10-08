use crate::editor_view::EditorView;
use gpui::{
    prelude::*, px, size, App, Application, Bounds, Focusable, WindowBounds, WindowOptions,
};
use stoat_v4::Stoat;

pub fn run_with_paths(paths: Vec<std::path::PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        // Register keybindings
        let keymap = stoat_v4::keymap::create_default_keymap();
        cx.bind_keys(keymap.bindings().cloned());

        // Register global action handlers
        cx.on_action(|_: &stoat_v4::actions::ExitApp, cx: &mut App| {
            cx.quit();
        });

        // Size window to 80% of screen size
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
            move |window, cx| {
                // Create Stoat entity
                let stoat = cx.new(|cx| {
                    let mut stoat = Stoat::new(cx);

                    // Load first file if provided
                    if !paths.is_empty() {
                        if let Err(e) = stoat.load_file(&paths[0], cx) {
                            tracing::error!("Failed to load file: {}", e);
                        }
                    }

                    stoat
                });

                // Create EditorView that renders the Stoat entity
                let editor_view = cx.new(|cx| EditorView::new(stoat, cx));

                // Set the entity reference so EditorView can pass it to EditorElement
                editor_view.update(cx, |view, _| {
                    view.set_entity(editor_view.clone());
                });

                // Focus the editor so input works immediately
                window.focus(&editor_view.read(cx).focus_handle(cx));

                editor_view
            },
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
