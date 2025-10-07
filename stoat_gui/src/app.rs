use crate::{editor::view::EditorView, pane_group::PaneGroupView};
use gpui::{
    prelude::*, px, size, App, Application, Bounds, Focusable, WindowBounds, WindowOptions,
};
use std::rc::Rc;
use stoat::Stoat;

pub fn run_with_paths(
    paths: Vec<std::path::PathBuf>,
    input_sequence: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        // Register Stoat keybindings
        let keymap = Rc::new(stoat::keymap::create_default_keymap());
        let bindings = keymap.bindings().cloned().collect::<Vec<_>>();
        cx.bind_keys(bindings);

        // Size window to 80% of screen size for better default experience
        let window_size = cx
            .primary_display()
            .map(|display| {
                let screen = display.bounds().size;
                size(screen.width * 0.8, screen.height * 0.8)
            })
            .unwrap_or_else(|| size(px(1200.0), px(800.0)));

        let bounds = Bounds::centered(None, window_size, cx);

        let keymap_clone = keymap.clone();
        let paths_clone = paths.clone();
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |_, cx| {
                    // Create Stoat Entity inside window context
                    let stoat_entity = cx.new(|cx| {
                        let mut stoat = Stoat::new(cx);

                        // Load files if provided
                        if !paths_clone.is_empty() {
                            let path_refs: Vec<&std::path::Path> =
                                paths_clone.iter().map(|p| p.as_ref()).collect();
                            stoat.load_files(&path_refs, cx);
                        }

                        stoat
                    });

                    let initial_editor = cx.new(|cx| EditorView::new(stoat_entity, cx));
                    cx.new(|cx| PaneGroupView::new(initial_editor, keymap_clone.clone(), cx))
                },
            )
            .expect("failed to open/update window");

        // Focus the active editor after window creation
        window
            .update(cx, |view, window, cx| {
                if let Some(editor) = view.active_editor() {
                    window.focus(&editor.read(cx).focus_handle(cx));
                }
            })
            .expect("failed to open/update window");

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
