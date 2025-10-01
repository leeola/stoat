use crate::editor::view::EditorView;
use gpui::{
    App, Application, Bounds, Focusable, WindowBounds, WindowOptions, prelude::*, px, size,
};
use stoat::Stoat;

pub fn run_with_stoat(stoat: Option<Stoat>) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        let stoat = stoat.unwrap_or_else(|| Stoat::new(cx));

        // For the simple modal system, we'll handle keys directly through the editor view
        // rather than registering individual keybindings for each character.
        // Future enhancement: add customizable keybindings here.

        let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);

        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(|cx| EditorView::new(stoat, cx)),
            )
            .unwrap();

        // Focus the editor after window creation
        window
            .update(cx, |view, window, cx| {
                window.focus(&view.focus_handle(cx));
            })
            .unwrap();

        cx.on_window_closed(|cx| {
            cx.quit();
        })
        .detach();

        cx.activate(true);
    });

    Ok(())
}

pub fn run_with_paths(
    paths: Vec<std::path::PathBuf>,
    input_sequence: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        let mut stoat = Stoat::new(cx);

        // Load files if provided
        if !paths.is_empty() {
            let path_refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_ref()).collect();
            stoat.load_files(&path_refs, cx);
        }

        // For the simple modal system, we'll handle keys directly through the editor view
        // rather than registering individual keybindings for each character.
        // Future enhancement: add customizable keybindings here.

        let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);

        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(|cx| EditorView::new(stoat, cx)),
            )
            .unwrap();

        // Focus the editor after window creation
        window
            .update(cx, |view, window, cx| {
                window.focus(&view.focus_handle(cx));
            })
            .unwrap();

        // Simulate input sequence if provided
        // We need to defer this to avoid updating while already updating
        if let Some(input) = input_sequence {
            cx.spawn(async move |cx| {
                // Small delay to ensure window is fully initialized
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;

                cx.update_window(window.into(), |_, window, cx| {
                    crate::external_input::simulate_input_sequence(&input, window, cx);
                })
                .ok();
            })
            .detach();
        }

        cx.on_window_closed(|cx| {
            cx.quit();
        })
        .detach();

        cx.activate(true);
    });

    Ok(())
}
