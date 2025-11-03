use crate::pane_group::PaneGroupView;
use gpui::{prelude::*, px, size, App, Application, Bounds, WindowBounds, WindowOptions};
use std::{rc::Rc, time::Duration};

#[cfg(debug_assertions)]
pub fn run_with_paths(
    config_path: Option<std::path::PathBuf>,
    input_sequence: Option<String>,
    timeout: Option<u64>,
    paths: Vec<std::path::PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_with_paths_impl(config_path, input_sequence, Some(timeout), paths)
}

#[cfg(not(debug_assertions))]
pub fn run_with_paths(
    config_path: Option<std::path::PathBuf>,
    input_sequence: Option<String>,
    paths: Vec<std::path::PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_with_paths_impl(config_path, input_sequence, None, paths)
}

fn run_with_paths_impl(
    config_path: Option<std::path::PathBuf>,
    input_sequence: Option<String>,
    timeout: Option<Option<u64>>,
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

                // Setup LSP progress tracking to enable automatic UI updates
                pane_group_view.update(cx, |this, cx| {
                    this.app_state
                        .setup_lsp_progress_tracking(pane_group_view.downgrade(), cx);
                });

                // Focus the initial editor so input works immediately
                // This must happen after PaneGroupView is created so the focus chain is
                // established
                pane_group_view.read(cx).focus_active_editor(window, cx);

                // If input simulation was requested, schedule it for next frame
                // This ensures view hierarchy is fully initialized before dispatching
                if let Some(input_str) = input_sequence.clone() {
                    tracing::info!("Input simulation requested: {}", input_str);
                    let keystrokes = crate::input_simulator::parse_input_sequence(&input_str);
                    tracing::info!("Parsed {} keystrokes", keystrokes.len());

                    let window_handle = window.window_handle();
                    window.on_next_frame(move |_, cx| {
                        tracing::debug!("Starting input simulation on next frame");
                        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                            for keystroke in keystrokes {
                                tracing::debug!("Dispatching keystroke: {:?}", keystroke);
                                if let Err(e) = window_handle.update(cx, |_, window, cx| {
                                    window.dispatch_keystroke(keystroke.clone(), cx);
                                }) {
                                    tracing::error!("Failed to dispatch keystroke: {}", e);
                                    break;
                                }

                                // Small delay between keystrokes to allow processing
                                cx.background_executor()
                                    .timer(Duration::from_millis(50))
                                    .await;
                            }

                            tracing::info!("Input simulation complete");
                        })
                        .detach();
                    });
                }

                pane_group_view
            },
        )
        .expect("failed to open window");

        // If timeout was requested (dev builds only), auto-quit after timeout expires
        if let Some(Some(timeout_secs)) = timeout {
            tracing::info!("Auto-quit timeout set: {} seconds", timeout_secs);
            cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                cx.background_executor()
                    .timer(Duration::from_secs(timeout_secs))
                    .await;

                tracing::info!("Timeout reached, quitting");
                let _ = cx.update(|cx| {
                    cx.quit();
                });
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
