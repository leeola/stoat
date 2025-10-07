use crate::editor_view::EditorView;
use gpui::{prelude::*, px, size, App, Application, Bounds, WindowBounds, WindowOptions};
use stoat_v4::Stoat;

pub fn run_with_paths(_paths: Vec<std::path::PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
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
                cx.new(|cx| EditorView::new(stoat, cx))
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
