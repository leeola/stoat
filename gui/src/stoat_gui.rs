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

mod actions;
mod buffer;
mod buffer_picker;
mod buffer_registry;
mod command_palette;
mod commit_list;
mod conflict_item;
mod diagnostics;
mod diff_coordinator;
mod diff_map;
mod display_map;
mod dock;
mod editor;
mod editor_input;
mod executor;
mod file_finder;
mod fs_watcher_driver;
mod git;
mod globals;
mod input_state_machine;
mod item;
mod keymap_loader;
mod lsp;
mod lsp_state;
mod modal_layer;
mod multi_buffer;
mod pane;
mod pane_tree;
mod panic_hook;
mod picker;
mod rebase_item;
mod review_item;
mod review_move_picker;
mod review_session;
mod reword_modal;
mod settings;
mod status_bar;
mod stoat_app;
mod symbol_picker;
mod tab_bar;
#[cfg(any(test, feature = "test-support"))]
pub mod test;
mod theme;
mod workspace;
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
    LspHostGlobal, ShellHostGlobal, TerminalHostGlobal,
};
use gpui::{
    px, size, App, AppContext, Application, Bounds, SharedString, TitlebarOptions, WindowBounds,
    WindowOptions,
};
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

pub fn run(globals: Globals, files: Vec<std::path::PathBuf>) {
    Application::new().run(move |cx: &mut App| {
        tracing::info!("stoat gui starting");
        install_production_globals(cx, globals);
        let bounds = Bounds::centered(None, size(px(1200.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Stoat")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_window, cx| cx.new(|cx| StoatApp::new(files, cx)),
        )
        .expect("open root window");
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
