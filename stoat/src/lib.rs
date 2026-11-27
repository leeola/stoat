pub mod action_handlers;
pub(crate) mod agent_goto;
pub mod agent_ipc;
pub mod agent_status;
pub mod alloc_trim;
pub mod apc_emit;
pub mod app;
pub mod attach;
pub mod auto_reload;
pub mod badge;
pub mod blame;
pub mod buffer_lifecycle;
pub mod buffer_registry;
pub mod button_repeat;
mod checkout_confirm;
pub mod claude_plugin;
pub mod cli_spec;
pub mod code_index;
pub mod command_palette;
mod commit_graph;
pub mod commit_list;
mod commit_message;
pub mod commit_picker;
mod commit_preview;
pub mod completion;
pub(crate) mod conflict_session;
pub mod debounce;
pub mod diagnostic_spans;
pub mod diagnostics;
pub(crate) mod diagnostics_picker;
pub mod diff_warm;
pub mod dump;
pub mod editor_state;
pub(crate) mod emoji_expand;
pub mod file_finder;
pub mod image_emit;
mod reload_landing;
mod review_approval;
pub mod review_walk;
// Materializes real sample repositories on disk for the CLI by design.
pub mod code_search;
pub mod diff_changes;
#[cfg(feature = "fixture")]
#[allow(clippy::disallowed_methods)]
pub mod fixture;
pub mod fuzzy;
pub mod git_jobs;
pub(crate) mod git_status;
pub(crate) mod goto_word;
pub mod help;
pub mod host;
pub mod input_history;
pub mod input_parse;
pub mod input_view;
mod job_list;
mod jobs;
mod jumplist;
pub(crate) mod jumplist_picker;
pub mod keymap_state;
pub mod location_picker;
pub mod lsp;
pub(crate) mod markdown;
pub mod minimap;
pub mod mouse;
mod nav_list;
pub(crate) mod osc_cap;
pub mod pane;
pub(crate) mod pane_finder;
mod paths;
#[cfg(feature = "perf")]
pub mod perf;
pub mod picker;
pub(crate) mod project_env;
pub mod quit_all_confirm;
pub mod rebase;
pub mod register;
pub mod render;
pub mod run;
mod session_log;
pub mod session_report;
pub mod smooth_scroll;
pub mod ssh;
mod state_sweep;
pub mod symbol_finder;
mod term_keys;
pub mod term_screen;
pub mod term_session;
pub mod toggle;
pub mod tty;
pub mod ui;
mod user_command;
mod vt_input;
pub mod walkthrough;
pub(crate) mod walkthrough_picker;
pub mod workspace;
pub mod workspace_picker;

pub use app::{Stoat, UpdateEffect};
pub use stoat_buffer::{buffer, diff_map, display_map, multi_buffer};
pub use stoat_keymap as keymap;
pub use stoat_review::{diff, diff_cache, diff_render_cli, merge_view, review_session};
use stoat_review::{review, review_apply};
pub use stoat_selection as selection;
pub(crate) use stoat_syntax as syntax_parse;
pub use stoat_theme as theme;
pub(crate) use stoat_theme::pool as theme_pool;
#[cfg(any(test, feature = "test-support"))]
pub mod test_harness;

#[cfg(any(test, feature = "test-support"))]
pub mod test_fixture;

/// A redraw handle for tests, where nothing listens for the wake.
///
/// Constructors take one so no production site can be built without a live
/// handle. A test that only inspects state has no run loop to wake, so it
/// passes a fresh one rather than plumbing the app's.
#[cfg(any(test, feature = "test-support"))]
pub fn test_notify() -> std::sync::Arc<tokio::sync::Notify> {
    std::sync::Arc::new(tokio::sync::Notify::new())
}
pub use badge::{Anchor, BadgeId, BadgeSource, BadgeState};
pub use buffer::{BufferId, SharedBuffer, TextBuffer, TextBufferSnapshot};
pub use diff_map::{
    ChangeKind, ChangeSpan, DiffHunk, DiffHunkStatus, DiffMap, DiffStatus, TokenDetail,
};
pub use display_map::{
    BlockMap, BlockPoint, BlockRow, BlockRowKind, BlockSnapshot, Chunk, ChunkRenderer,
    ChunkRendererId, ChunkReplacement, Crease, CreaseId, CreaseMap, CreaseSnapshot, DisplayMap,
    DisplayPoint, DisplayRow, DisplaySnapshot, FoldMap, FoldPlaceholder, FoldPoint, FoldSnapshot,
    HighlightKey, HighlightLayer, HighlightStyle, HighlightStyleId, HighlightStyleInterner,
    HighlightedChunk, Highlights, InlayHighlight, InlayHighlights, InlayId, InlayKind, InlayMap,
    InlayOffset, InlayPoint, InlaySnapshot, SemanticTokenHighlight, SemanticTokensHighlights,
    TabMap, TabPoint, TabRow, TabSnapshot, TextHighlights, WrapMap, WrapPoint, WrapSnapshot,
};
pub use multi_buffer::{MultiBuffer, MultiBufferSnapshot};
pub use pane::{
    Axis, Direction, DockId, DockPanel, DockSide, DockVisibility, FocusTarget, Pane, PaneId,
    PaneTree, Placement, View,
};
pub use paths::{user_config_path, user_themes_dir};
pub use run::RunId;
pub use stoat_config::{LineNumbers, MouseCapturePolicy, Settings};
pub use stoat_log as log;

/// Resolves the [`MouseCapturePolicy`] from the compiled-in defaults, which the
/// UI thread needs before the main [`Stoat`] exists.
///
/// Returns [`MouseCapturePolicy::Auto`] when the setting is unset, which is the
/// policy a config naming none asks for.
///
/// Reads the process-wide parse of the defaults rather than parsing them again
/// here. This runs ahead of the terminal setup and the stoatty handshake, so a
/// second parse of the 55 KB source delays both by what it costs.
pub fn default_mouse_capture_policy() -> MouseCapturePolicy {
    Settings::from_config(app::embedded_config())
        .mouse_capture
        .unwrap_or(MouseCapturePolicy::Auto)
}

/// Resolves `policy` to a concrete capture decision. `Auto` consults
/// `env_host` for `TMUX` / `ZELLIJ` and skips capture when either is
/// set, since a parent multiplexer owns the mouse drag-select
/// gesture and stealing its events breaks the user's workflow. The
/// UI thread starts before [`Stoat`] is constructed, so callers
/// thread an `EnvHost` from the bin layer instead of reading
/// [`Stoat::env_host`].
pub fn resolve_mouse_captured(policy: MouseCapturePolicy, env_host: &dyn host::EnvHost) -> bool {
    match policy {
        MouseCapturePolicy::Always => true,
        MouseCapturePolicy::Never => false,
        MouseCapturePolicy::Auto => {
            !(env_host.var("TMUX").is_some() || env_host.var("ZELLIJ").is_some())
        },
    }
}

#[cfg(test)]
mod resolve_mouse_captured_tests {
    use super::*;
    use host::FakeEnv;

    #[test]
    fn always_returns_true() {
        let env = FakeEnv::new();
        assert!(resolve_mouse_captured(MouseCapturePolicy::Always, &env));
    }

    #[test]
    fn never_returns_false() {
        let env = FakeEnv::new();
        assert!(!resolve_mouse_captured(MouseCapturePolicy::Never, &env));
    }

    #[test]
    fn auto_with_no_mux_captures() {
        let env = FakeEnv::new();
        assert!(resolve_mouse_captured(MouseCapturePolicy::Auto, &env));
    }

    #[test]
    fn auto_with_tmux_skips() {
        let env = FakeEnv::new();
        env.set("TMUX", "/tmp/tmux-1000/default,1234,0");
        assert!(!resolve_mouse_captured(MouseCapturePolicy::Auto, &env));
    }

    #[test]
    fn auto_with_zellij_skips() {
        let env = FakeEnv::new();
        env.set("ZELLIJ", "1");
        assert!(!resolve_mouse_captured(MouseCapturePolicy::Auto, &env));
    }
}
