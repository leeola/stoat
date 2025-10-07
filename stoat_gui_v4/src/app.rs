use crate::editor_view::EditorView;
use gpui::{prelude::*, px, size, App, Application, Bounds, WindowBounds, WindowOptions};
use stoat_v4::Stoat;

pub fn run_with_paths(_paths: Vec<std::path::PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        // Register keybindings
        let keymap = stoat_v4::keymap::create_default_keymap();
        cx.bind_keys(keymap.bindings().cloned());

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
            move |_, cx| {
                // Create Stoat entity
                let stoat = cx.new(|cx| Stoat::new(cx));

                // Create EditorView that renders the Stoat entity
                let editor_view = cx.new(|cx| EditorView::new(stoat, cx));

                // Set the entity reference so EditorView can pass it to EditorElement
                editor_view.update(cx, |view, _| {
                    view.set_entity(editor_view.clone());
                });

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
