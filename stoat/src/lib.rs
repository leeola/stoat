pub mod action_handlers;
pub mod agent_ipc;
pub mod agent_status;
pub mod app;
pub mod badge;
pub mod buffer;
mod buffer_registry;
mod code_index;
pub mod command_palette;
mod commit_list;
pub mod completion;
pub mod diagnostics;
pub(crate) mod diagnostics_picker;
pub mod diff;
pub mod diff_cache;
pub mod diff_map;
pub mod diff_render_cli;
pub mod display_map;
pub mod dump;
mod editor_state;
pub mod file_finder;
#[cfg(feature = "fixture")]
pub mod fixture;
pub mod fuzzy;
pub(crate) mod global_search;
pub(crate) mod goto_word;
pub mod help;
pub mod host;
pub mod input_parse;
mod input_view;
mod jumplist;
pub(crate) mod jumplist_picker;
pub mod keymap;
mod keymap_state;
pub(crate) mod location_picker;
pub mod lsp;
pub mod multi_buffer;
pub mod pane;
mod paths;
#[cfg(feature = "perf")]
pub mod perf;
pub(crate) mod picker;
pub(crate) mod quit_all_confirm;
mod rebase;
mod register;
pub(crate) mod render;
mod review;
mod review_apply;
mod review_session;
pub mod run;
mod selection;
mod smooth_scroll;
pub mod term_screen;
pub mod term_session;
pub mod theme;
pub mod ui;
pub mod workspace;
pub mod workspace_picker;

pub use app::{Stoat, UpdateEffect};
#[cfg(test)]
mod test_harness;
pub use badge::{Anchor, BadgeId, BadgeSource, BadgeState};
pub use buffer::{BufferId, SharedBuffer, TextBuffer, TextBufferSnapshot};
pub use diff_map::{ChangeKind, ChangeSpan, DiffHunk, DiffHunkStatus, DiffMap, TokenDetail};
pub use display_map::{
    BlockMap, BlockPoint, BlockRow, BlockRowKind, BlockSnapshot, Chunk, ChunkRenderer,
    ChunkRendererId, ChunkReplacement, Crease, CreaseId, CreaseMap, CreaseSnapshot, DisplayMap,
    DisplayMapId, DisplayPoint, DisplayRow, DisplaySnapshot, FoldMap, FoldPlaceholder, FoldPoint,
    FoldSnapshot, HighlightKey, HighlightLayer, HighlightStyle, HighlightStyleId,
    HighlightStyleInterner, HighlightedChunk, Highlights, InlayHighlight, InlayHighlights, InlayId,
    InlayKind, InlayMap, InlayOffset, InlayPoint, InlaySnapshot, SemanticTokenHighlight,
    SemanticTokensHighlights, TabMap, TabPoint, TabRow, TabSnapshot, TextHighlights, WrapMap,
    WrapPoint, WrapSnapshot,
};
pub use host::DiffStatus;
pub use multi_buffer::{
    ExcerptBoundary, ExcerptId, ExcerptInfo, MultiBuffer, MultiBufferAnchor, MultiBufferPoint,
    MultiBufferRow, MultiBufferSnapshot,
};
pub use pane::{
    Axis, Direction, DockId, DockPanel, DockSide, DockVisibility, FocusTarget, Pane, PaneId,
    PaneTree, Placement, View,
};
pub use run::RunId;
pub use stoat_config::{MouseCapturePolicy, Settings};
pub use stoat_log as log;

/// Resolves the [`MouseCapturePolicy`] from the compiled-in default
/// keymap. Returns [`MouseCapturePolicy::Auto`] when the setting is
/// unset or the keymap has parse errors, since the UI thread starts
/// before the main `Stoat` instance and must not block on config
/// failure.
pub fn default_mouse_capture_policy() -> MouseCapturePolicy {
    let (config, _errors) = stoat_config::parse(app::DEFAULT_KEYMAP);
    config
        .as_ref()
        .map(Settings::from_config)
        .and_then(|s| s.mouse_capture)
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
