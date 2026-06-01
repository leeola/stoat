//! App globals: types stored via [`gpui::App::set_global`] so any
//! entity can read them with `cx.global::<T>()` without entity-to-
//! entity plumbing. Cross-cutting state like the language registry,
//! settings, the active theme, the host traits, and the canonical
//! [`Executor`] lives here.
//!
//! Production wiring calls [`install_production_globals`] during
//! startup with a fully-constructed [`Globals`] aggregate; tests
//! register their own values via
//! [`crate::test::TestHarness::set_global`] or directly through
//! `cx.set_global(...)`.

use crate::{
    settings::Settings,
    theme::{self, Theme},
};
use gpui::{App, Global};
use std::sync::{Arc, Mutex};
use stoat::host::{
    ClaudeCodeHost, ClipboardHost, EnvHost, FsHost, FsWatchHost, GitHost, LspHost,
    PermissionPrompt, ShellHost, TerminalHost,
};
use stoat_scheduler::Executor;
use tokio::sync::mpsc;

/// App-global wrapper around [`stoat_language::LanguageRegistry`].
pub struct LanguageRegistry(pub stoat_language::LanguageRegistry);

impl Global for LanguageRegistry {}

impl LanguageRegistry {
    /// Standard registry with the production grammars wired in.
    pub fn standard() -> Self {
        Self(stoat_language::LanguageRegistry::standard())
    }
}

/// App-global wrapper for [`Arc<dyn FsHost>`].
pub struct FsHostGlobal(pub Arc<dyn FsHost>);

impl Global for FsHostGlobal {}

/// App-global wrapper for [`Arc<dyn FsWatchHost>`].
pub struct FsWatchHostGlobal(pub Arc<dyn FsWatchHost>);

impl Global for FsWatchHostGlobal {}

/// App-global wrapper for [`Arc<dyn EnvHost>`].
pub struct EnvHostGlobal(pub Arc<dyn EnvHost>);

impl Global for EnvHostGlobal {}

/// App-global wrapper for [`Arc<dyn ShellHost>`].
pub struct ShellHostGlobal(pub Arc<dyn ShellHost>);

impl Global for ShellHostGlobal {}

/// App-global wrapper for [`Arc<dyn LspHost>`]. The factory side of
/// the LSP host split; entities resolve a per-language
/// [`stoat::host::LspServer`] via [`stoat::host::LspHost::launch`].
/// Production wires [`stoat::host::LocalLspHost`] constructed from a
/// snapshot of `Settings::language_servers`; languages without a
/// configured entry surface as [`std::io::ErrorKind::NotFound`] at
/// launch time rather than a silent noop server.
pub struct LspHostGlobal(pub Arc<dyn LspHost>);

impl Global for LspHostGlobal {}

/// App-global wrapper for [`Arc<dyn GitHost>`].
pub struct GitHostGlobal(pub Arc<dyn GitHost>);

impl Global for GitHostGlobal {}

/// App-global wrapper for [`Arc<dyn ClaudeCodeHost>`].
pub struct ClaudeCodeHostGlobal(pub Arc<dyn ClaudeCodeHost>);

impl Global for ClaudeCodeHostGlobal {}

/// App-global wrapper for [`Arc<dyn ClipboardHost>`].
pub struct ClipboardHostGlobal(pub Arc<dyn ClipboardHost>);

impl Global for ClipboardHostGlobal {}

/// App-global wrapper for [`Arc<dyn TerminalHost>`].
pub struct TerminalHostGlobal(pub Arc<dyn TerminalHost>);

impl Global for TerminalHostGlobal {}

/// Drain interface for queued Claude permission prompts. Production
/// wraps the receiver end of the mpsc channel paired with a
/// [`stoat::host::RuleBasedPolicy::with_prompt_channel`] callback;
/// the workspace's poll task drains it on each tick and routes each
/// prompt to its modal queue.
pub trait PermissionPromptHost: Send + Sync {
    /// Pop one queued prompt, or `None` if the queue is empty. Does
    /// not block; the workspace polls on a foreground tick.
    fn try_recv(&self) -> Option<PermissionPrompt>;
}

/// App-global wrapper for [`Arc<dyn PermissionPromptHost>`]. Absent
/// in tests and headless runs that do not install a permission
/// callback; the workspace's poll task becomes a no-op in that case.
pub struct PermissionPromptHostGlobal(pub Arc<dyn PermissionPromptHost>);

impl Global for PermissionPromptHostGlobal {}

/// Production [`PermissionPromptHost`] backed by a tokio mpsc
/// receiver. The sender side feeds
/// [`stoat::host::RuleBasedPolicy::with_prompt_channel`]; the
/// receiver lives here, drained by the GUI workspace's poll task.
pub struct MpscPermissionPromptHost {
    rx: Mutex<mpsc::Receiver<PermissionPrompt>>,
}

impl MpscPermissionPromptHost {
    pub fn new(rx: mpsc::Receiver<PermissionPrompt>) -> Self {
        Self { rx: Mutex::new(rx) }
    }
}

impl PermissionPromptHost for MpscPermissionPromptHost {
    fn try_recv(&self) -> Option<PermissionPrompt> {
        self.rx
            .lock()
            .expect("permission prompt rx mutex poisoned")
            .try_recv()
            .ok()
    }
}

/// App-global wrapper for the canonical [`Executor`]. Tokio-bound
/// hosts (LSP, Claude Code, fs watcher) and any entity-bound async
/// work share this single runtime; tests substitute one driven by
/// [`stoat_scheduler::TestScheduler`].
pub struct ExecutorGlobal(pub Executor);

impl Global for ExecutorGlobal {}

/// All app globals registered at startup. Grows additively as new
/// global types are introduced; new fields are added by sibling
/// items in this parent (host-trait globals).
pub struct Globals {
    pub settings: Settings,
    pub theme: Theme,
    pub language_registry: LanguageRegistry,
    pub fs_host: FsHostGlobal,
    pub fs_watch_host: FsWatchHostGlobal,
    pub env_host: EnvHostGlobal,
    pub shell_host: ShellHostGlobal,
    pub lsp_host: LspHostGlobal,
    pub git_host: GitHostGlobal,
    pub claude_code_host: ClaudeCodeHostGlobal,
    pub permission_prompt_host: PermissionPromptHostGlobal,
    pub clipboard_host: ClipboardHostGlobal,
    pub terminal_host: TerminalHostGlobal,
    pub executor: ExecutorGlobal,
}

/// Register the production set of app globals on `cx`.
pub fn install_production_globals(cx: &mut App, globals: Globals) {
    seed_language_highlight_maps(&globals.language_registry.0);
    cx.set_global(globals.settings);
    theme::set_active_theme(cx, globals.theme);
    cx.set_global(globals.language_registry);
    cx.set_global(globals.fs_host);
    cx.set_global(globals.fs_watch_host);
    cx.set_global(globals.env_host);
    cx.set_global(globals.shell_host);
    cx.set_global(globals.lsp_host);
    cx.set_global(globals.git_host);
    cx.set_global(globals.claude_code_host);
    cx.set_global(globals.permission_prompt_host);
    cx.set_global(globals.clipboard_host);
    cx.set_global(globals.terminal_host);
    cx.set_global(globals.executor);
}

/// Install a [`stoat_language::HighlightMap`] on every language in
/// `registry`, keyed by the static syntax-theme key list from
/// [`stoat::display_map::syntax_theme::SyntaxStyles`]. The keys are
/// `&'static`, so this is a one-time setup that survives theme
/// reloads -- only the style table changes when the active theme
/// changes; the capture-index -> [`stoat_language::HighlightId`]
/// mapping does not.
///
/// Without this setup every capture resolves to
/// [`stoat_language::HighlightId::DEFAULT`] and the renderer paints
/// no syntax color.
pub fn seed_language_highlight_maps(registry: &stoat_language::LanguageRegistry) {
    let styles =
        stoat::display_map::syntax_theme::SyntaxStyles::from_theme(&stoat::theme::Theme::empty());
    let theme_keys = styles.theme_keys();
    for lang in registry.languages() {
        let map = stoat_language::HighlightMap::new(lang.highlight_capture_names(), theme_keys);
        lang.set_highlight_map(map);
    }
}
