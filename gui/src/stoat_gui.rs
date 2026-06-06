//! Stoat's gpui-backed GUI crate.
//!
//! ## Entity-bound async work
//!
//! Wrappers in this crate must not capture strong [`gpui::Entity`]
//! handles into futures spawned via `cx.spawn`. A strong capture
//! pins its target alive for the lifetime of the future, which
//! combines with the subscription / executor wiring to produce
//! reference cycles that outlive their owning entity. Route async
//! work through [`spawn_with_entity`], which takes
//! [`gpui::WeakEntity`] by signature; the helper upgrades the weak
//! handle on the completion hop and silently drops the callback
//! when the entity has already been released.
#![deny(clippy::disallowed_types, clippy::disallowed_methods)]

mod about_modal;
mod actions;
mod breadcrumbs;
mod buffer;
mod buffer_picker;
mod buffer_registry;
mod claude_chat;
mod claude_checkpoint_picker;
mod claude_permission_modal;
mod claude_tool_card;
mod command_palette;
mod commit_list;
mod conflict_item;
mod conflict_picker;
mod delete_tree_confirm;
mod diagnostics;
mod diagnostics_panel;
mod diagnostics_picker;
mod diff_coordinator;
mod diff_hunk_panel;
mod diff_map;
mod diff_pane;
mod display_map;
mod dock;
mod editor;
mod editor_input;
mod encoding_picker;
mod executor;
mod file_finder;
mod fold_actions;
mod fs_watcher_driver;
mod git;
mod git_status_picker;
mod global_search;
mod globals;
mod goto_line_modal;
mod help;
mod input_driver;
mod input_parse;
mod input_state_machine;
mod item;
mod jumplist_picker;
mod key_hint_banner;
mod keymap_loader;
mod line_ending_picker;
mod lsp;
mod lsp_state;
mod markdown_preview;
mod modal_layer;
mod multi_buffer;
mod outline_panel;
mod pane;
mod pane_tree;
mod panic_hook;
mod picker;
mod project_tree;
mod quit_confirm;
mod rebase_item;
mod rename_workspace_modal;
mod render_stats;
mod review_item;
mod review_move_picker;
mod review_session;
mod reword_modal;
mod run_pane;
mod settings;
mod shell_input_modal;
mod status_bar;
mod sticky_scroll;
mod stoat_app;
mod symbol_picker;
mod syntax_updater;
mod tab_bar;
mod terminal_view;
#[cfg(any(test, feature = "test-support"))]
pub mod test;
mod theme;
mod theme_picker;
mod toast;
mod workspace;
mod workspace_persist;
mod workspace_picker;
mod workspace_symbol_picker;

pub use actions::{ClickAt, DragSelectTo, HoverAt, SetActivePane};
pub use buffer::{Buffer, BufferEvent};
pub use buffer_registry::{BufferRegistry, BufferRegistryEvent};
pub use commit_list::{CommitListDelegate, CommitListItem, CommitListState, CommitListStateEvent};
pub use diagnostics::{DiagnosticSet, DiagnosticSetEvent};
pub use diff_coordinator::DiffCoordinator;
pub use diff_map::{DiffMap, DiffMapEvent};
pub use display_map::{DisplayMap, DisplayMapEvent};
pub use dock::{Dock, DockEvent, DockSide, DockVisibility};
pub use editor::{
    scroll::{OngoingScroll, ScrollAnchor, ScrollManager, ScrollbarThumbState},
    search::{SearchDirection, SearchState},
    Editor, EditorEvent,
};
pub use editor_input::EditorInput;
pub use executor::spawn_with_entity;
pub use fs_watcher_driver::{FsWatcherDriver, FsWatcherDriverEvent};
pub use globals::{
    install_production_globals, ClaudeCodeHostGlobal, ClipboardHostGlobal, EnvHostGlobal,
    ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal, GitHostGlobal, Globals, LanguageRegistry,
    LspHostGlobal, MpscPermissionPromptHost, PermissionPromptHost, PermissionPromptHostGlobal,
    ShellHostGlobal, TerminalHostGlobal, UserSnippetsGlobal,
};

/// Selects whether [`run`] starts in a fresh workspace or rehydrates
/// a previously persisted one. Mirrors the binary's `--continue` /
/// `--resume` flags from `bin/src/commands/default.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreMode {
    /// Default: a fresh workspace anchored at the current
    /// directory. Files passed alongside are opened normally.
    None,
    /// Restore the most-recently-modified workspace whose
    /// `git_root` matches the current directory.
    Continue,
    /// Walk cwd ancestors and restore the most-recently-modified
    /// workspace under any of them. Falls back to a fresh
    /// workspace anchored at cwd when no ancestor has any state.
    Resume,
}
pub use gpui::Keystroke;
use gpui::{
    px, size, App, AppContext, Application, Bounds, SharedString, TitlebarOptions, WindowBounds,
    WindowOptions,
};
pub use input_parse::{parse_input_sequence, InputParseError};
pub use input_state_machine::{InputStateMachine, Operator};
pub use item::{DeserializeSnafu, ItemError, ItemHandle, ItemView, SaveSnafu};
pub use keymap_loader::{
    compile_default_keymap, compile_from_settings, compile_from_source, DEFAULT_KEYMAP,
};
pub use lsp_state::{LspState, LspStateEvent};
pub use modal_layer::ModalLayer;
pub use multi_buffer::{MultiBuffer, MultiBufferEvent};
pub use pane::{Pane, PaneEvent};
pub use pane_tree::{PaneTree, PaneTreeEvent};
pub use panic_hook::install_panic_hook;
pub use review_item::{ReviewFileView, ReviewItem};
pub use review_session::{ReviewSession, ReviewSessionEvent};
pub use settings::Settings;
pub use status_bar::StatusBar;
use stoat_app::StoatApp;
pub use tab_bar::{render_tab_bar, DraggedTab};
pub use theme::Theme;
pub use workspace::{Workspace, WorkspaceEvent};

pub fn run(
    globals: Globals,
    files: Vec<std::path::PathBuf>,
    restore: RestoreMode,
    inputs: Option<Vec<Keystroke>>,
    timeout: Option<f64>,
) {
    Application::new().run(move |cx: &mut App| {
        tracing::info!("stoat gui starting");
        install_production_globals(cx, globals);
        let bounds = Bounds::centered(None, size(px(1200.0), px(800.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(SharedString::from("Stoat")),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                move |_window, cx| cx.new(|cx| StoatApp::new(files, restore, cx)),
            )
            .expect("open root window");
        if let Some(keystrokes) = inputs {
            input_driver::drive_inputs(cx, window, keystrokes);
        }
        if let Some(timeout) = timeout {
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs_f64(timeout))
                    .await;
                cx.update(|app| app.quit()).ok();
            })
            .detach();
        }
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
