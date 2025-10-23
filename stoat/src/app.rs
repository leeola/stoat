use crate::pane_group::PaneGroupView;
use gpui::{prelude::*, px, size, App, Application, Bounds, WindowBounds, WindowOptions};
use std::rc::Rc;

pub fn run_with_paths(
    config_path: Option<std::path::PathBuf>,
    paths: Vec<std::path::PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        // Load configuration with optional override from CLI --config or STOAT_CONFIG env var
        let config = crate::Config::load_with_overrides(config_path.as_deref()).unwrap_or_default();

        // Register keybindings
        let keymap = Rc::new(crate::keymap::create_default_keymap());
        cx.bind_keys(keymap.bindings().cloned());

        // Register global action handlers
        cx.on_action(|_: &crate::actions::QuitAll, cx: &mut App| {
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
                // Create PaneGroupView (handles workspace, stoat, and editor initialization)
                let pane_group_view = cx.new(|cx| {
                    PaneGroupView::new(config.clone(), paths.clone(), keymap.clone(), cx)
                });

                // Focus the initial editor so input works immediately
                // This must happen after PaneGroupView is created so the focus chain is established
                pane_group_view.read(cx).focus_active_editor(window, cx);

                pane_group_view
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
