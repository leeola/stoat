use crate::{
    action_handlers,
    badge::{Anchor, Badge, BadgeSource, BadgeState, BadgeTray},
    buffer::{BufferId, TextBufferSnapshot},
    command_palette::CommandPalette,
    display_map::{highlights::SemanticTokenHighlight, syntax_theme::SyntaxStyles},
    editor_state::EditorId,
    file_finder::FileFinder,
    help::Help,
    host::{
        AgentMessage, ClaudeCodeHost, ClaudeCodeSessions, ClaudeNotification, ClaudeSessionId,
        EnvHost, FsHost, GitHost, LocalEnv, LocalFs, LocalGit, LspHost, NoopLsp,
    },
    keymap::{Keymap, ResolvedAction},
    keymap_state::{normalize_shift_letter, resolve_action, StoatKeymapState},
    pane::{FocusTarget, View},
    rebase::RebasePause,
    run::{GridSelection, PtyNotification, RunId},
    workspace::{Workspace, WorkspaceId},
    workspace_picker::{PickerOutcome, WorkspacePicker},
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{buffer::Buffer, layout::Rect};
use slotmap::SlotMap;
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_action::{OpenFile, OpenReview};
use stoat_config::Settings;
use stoat_language::{self as language, Language, LanguageRegistry, SyntaxState};
use stoat_scheduler::Executor;
use stoat_text::Bias;
use tokio::sync::mpsc::{Receiver, Sender};

pub(crate) const DEFAULT_KEYMAP: &str = include_str!("../../config.stcfg");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateEffect {
    Redraw,
    Quit,
    None,
}

pub struct Stoat {
    size: Rect,
    pub mode: String,
    pub executor: Executor,
    pub(crate) keymap: Keymap,
    pub settings: Settings,
    pub theme: crate::theme::Theme,
    pub(crate) command_palette: Option<CommandPalette>,
    pub(crate) help: Option<Help>,
    pub(crate) file_finder: Option<FileFinder>,
    pub(crate) workspace_picker: Option<WorkspacePicker>,
    /// When true, [`Self::save_workspace`] and the startup load path become
    /// no-ops. Set by the test harness so test runs can't read or write the
    /// real `$XDG_STATE_HOME/stoat/workspaces/` directory.
    pub(crate) persistence_disabled: bool,
    pub(crate) language_registry: Arc<LanguageRegistry>,
    pub(crate) syntax_styles: SyntaxStyles,
    pub(crate) workspaces: SlotMap<WorkspaceId, Workspace>,
    pub(crate) active_workspace: WorkspaceId,
    /// App-level badge tray for cross-workspace notifications. Badges here
    /// render regardless of which workspace is active, complementing each
    /// workspace's own [`Workspace::badges`]. The tray the badge lives in
    /// is the source of truth for its scope.
    pub(crate) badges: BadgeTray,
    pub(crate) claude_host: Option<Arc<dyn ClaudeCodeHost>>,
    claude_sessions: ClaudeCodeSessions,
    pub(crate) claude_tx: Sender<ClaudeNotification>,
    claude_rx: Receiver<ClaudeNotification>,
    pub(crate) pty_tx: Sender<PtyNotification>,
    pty_rx: Receiver<PtyNotification>,
    pub(crate) modal_run: Option<RunId>,
    pub(crate) render_tick: u64,
    /// Accumulated digit prefix for the next motion (Vim-style
    /// `<count>j` etc.). Filled by `handle_key` when a digit press
    /// hits an unbound key in normal mode; consumed once via
    /// `take_pending_count` and cleared after every action dispatch.
    pub(crate) pending_count: Option<u32>,
    /// Pending Vim-style find-char prefix (`f`/`F`/`t`/`T`). When
    /// Some, the next printable char keypress runs the matching
    /// find on the focused editor and clears this field. The
    /// trailing `u32` is the count captured from `pending_count`
    /// at the time the chord was armed; defaults to 1.
    pub(crate) pending_find: Option<(action_handlers::movement::FindKind, bool, u32)>,
    /// Most recent `(FindKind, char)` consumed by `execute_find`.
    /// `RepeatLastMotion` (Alt-.) replays this pair without
    /// reading another keypress.
    pub(crate) last_find: Option<(action_handlers::movement::FindKind, char)>,
    /// Filesystem the UI layer reads through. Swapped to
    /// [`crate::host::FakeFs`] in tests; all IO outside the host module
    /// itself must route through this field.
    pub(crate) fs_host: Arc<dyn FsHost>,
    /// Git operations flow through this trait so tests can use
    /// [`crate::host::FakeGit`] without a real repository.
    pub(crate) git_host: Arc<dyn GitHost>,
    /// Environment-variable lookups go through this trait so tests can
    /// install [`crate::host::FakeEnv`] without leaking real env state.
    pub(crate) env_host: Arc<dyn EnvHost>,
    /// Language-server requests route through this trait. Defaults to
    /// [`NoopLsp`] (every method returns the empty success response)
    /// until a real `LocalLsp` is wired in; tests install
    /// [`crate::host::FakeLsp`] to drive end-to-end LSP scenarios.
    pub(crate) lsp_host: Arc<dyn LspHost>,
    /// System-clipboard writes route through this trait. Defaults to
    /// [`NoopClipboard`] so headless or display-less environments do
    /// not error on the first clipboard event; tests install
    /// [`crate::host::FakeClipboard`] to assert on writes.
    pub(crate) clipboard_host: Arc<dyn crate::host::ClipboardHost>,
    /// Tracks `$/progress` notifications so the status bar can show
    /// the freshest in-progress operation. Drained from
    /// [`crate::host::LspHost::try_recv_notification`] inside
    /// [`Stoat::update`].
    pub(crate) lsp_progress: crate::lsp::progress::LspProgressMap,
}

/// Result of a successful background parse, ready to be installed on the
/// foreground thread.
pub(crate) struct ParseJobOutput {
    pub(crate) buffer_id: BufferId,
    pub(crate) syntax: SyntaxState,
    /// Multi-layer parse state from [`stoat_language::SyntaxMap::reparse`].
    /// Populated alongside [`Self::syntax`] so the legacy single-tree
    /// highlight path and the capture-merging path can run side by side
    /// while consumers migrate.
    pub(crate) syntax_map: stoat_language::SyntaxMap,
    pub(crate) tokens: Arc<[SemanticTokenHighlight]>,
}

impl Stoat {
    #[cfg(test)]
    pub fn test() -> crate::test_harness::TestHarness {
        crate::test_harness::TestHarness::default()
    }

    #[cfg(test)]
    pub(crate) fn active_keys_for_mode(
        &self,
        mode: &str,
    ) -> Vec<(&crate::keymap::CompiledKey, &[ResolvedAction])> {
        let state = StoatKeymapState::new(mode);
        self.keymap.active_keys(&state)
    }

    pub(crate) fn active_bindings_for_current_mode(&self) -> Vec<(String, Vec<ResolvedAction>)> {
        let state = StoatKeymapState::new(&self.mode);
        self.keymap
            .active_bindings(&state)
            .into_iter()
            .map(|(label, actions)| (label, actions.to_vec()))
            .collect()
    }

    pub fn new(executor: Executor, cli_settings: Settings, initial_git_root: PathBuf) -> Self {
        let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
        if !errors.is_empty() {
            tracing::error!(
                "default keymap parse errors: {}",
                stoat_config::format_errors(DEFAULT_KEYMAP, &errors)
            );
        }
        let settings = config
            .as_ref()
            .map(Settings::from_config)
            .unwrap_or_default()
            .merge(cli_settings);

        let theme = {
            let name = settings.theme.as_deref().unwrap_or("default_dark");
            match config.as_ref() {
                Some(c) => crate::theme::Theme::from_config(c, name).unwrap_or_else(|e| {
                    tracing::error!("theme '{name}' load failed: {e}");
                    crate::theme::Theme::empty()
                }),
                None => crate::theme::Theme::empty(),
            }
        };

        let keymap = config.map(|c| Keymap::compile(&c)).unwrap_or_else(|| {
            Keymap::compile(&stoat_config::Config {
                blocks: vec![],
                themes: vec![],
            })
        });

        let syntax_styles = SyntaxStyles::from_theme(&theme);
        let language_registry = Arc::new(LanguageRegistry::standard());
        let theme_keys = syntax_styles.theme_keys();
        for lang in language_registry.languages() {
            let map = stoat_language::HighlightMap::new(lang.highlight_capture_names(), theme_keys);
            lang.set_highlight_map(map);
        }

        let mut workspaces = SlotMap::with_key();
        let workspace = Workspace::new(initial_git_root.clone(), &executor);
        let active_workspace = workspaces.insert(workspace);
        workspaces[active_workspace].id = active_workspace;

        let (pty_tx, pty_rx) = tokio::sync::mpsc::channel(256);
        let (claude_tx, claude_rx) = tokio::sync::mpsc::channel(256);

        Self {
            size: Rect::default(),
            mode: "normal".into(),
            executor,
            keymap,
            settings,
            theme,
            command_palette: None,
            help: None,
            file_finder: None,
            workspace_picker: None,
            persistence_disabled: false,
            language_registry,
            syntax_styles,
            workspaces,
            active_workspace,
            badges: BadgeTray::new(),
            claude_host: None,
            claude_sessions: ClaudeCodeSessions::default(),
            claude_tx,
            claude_rx,
            pty_tx,
            pty_rx,
            modal_run: None,
            render_tick: 0,
            pending_count: None,
            pending_find: None,
            last_find: None,
            fs_host: Arc::new(LocalFs),
            git_host: Arc::new(LocalGit::new()),
            env_host: Arc::new(LocalEnv),
            lsp_host: Arc::new(NoopLsp),
            clipboard_host: Arc::new(crate::host::NoopClipboard),
            lsp_progress: crate::lsp::progress::LspProgressMap::new(),
        }
    }

    /// Swap in an alternative [`FsHost`]. The default is [`LocalFs`]; the
    /// test harness installs [`crate::host::FakeFs`] so review, open-file,
    /// and other IO paths run in-memory.
    pub fn set_fs_host(&mut self, host: Arc<dyn FsHost>) {
        self.fs_host = host;
    }

    /// Swap in an alternative [`GitHost`]. The default is [`LocalGit`];
    /// tests inject [`crate::host::FakeGit`] to drive the review flow
    /// without a real repository.
    pub fn set_git_host(&mut self, host: Arc<dyn GitHost>) {
        self.git_host = host;
    }

    /// Swap in an alternative [`EnvHost`]. The default is [`LocalEnv`];
    /// the test harness installs [`crate::host::FakeEnv`] so env-var
    /// reads do not pull in real process state.
    pub fn set_env_host(&mut self, host: Arc<dyn EnvHost>) {
        self.env_host = host;
    }

    /// Returns the active [`EnvHost`].
    pub fn env_host(&self) -> &Arc<dyn EnvHost> {
        &self.env_host
    }

    /// Swap in an alternative [`crate::host::ClipboardHost`]. The default
    /// is [`crate::host::NoopClipboard`]; production binaries install
    /// [`crate::host::LocalClipboard`] (arboard-backed) and tests
    /// install [`crate::host::FakeClipboard`].
    pub fn set_clipboard_host(&mut self, host: Arc<dyn crate::host::ClipboardHost>) {
        self.clipboard_host = host;
    }

    /// Returns the active [`crate::host::ClipboardHost`].
    pub fn clipboard_host(&self) -> &Arc<dyn crate::host::ClipboardHost> {
        &self.clipboard_host
    }

    /// Swap in an alternative [`LspHost`]. The default is [`NoopLsp`]
    /// (every request returns the empty success response); the test
    /// harness installs [`crate::host::FakeLsp`] so LSP-driven flows
    /// run against programmed responses.
    pub fn set_lsp_host(&mut self, host: Arc<dyn LspHost>) {
        self.lsp_host = host;
    }

    /// Returns the active [`LspHost`].
    pub fn lsp_host(&self) -> &Arc<dyn LspHost> {
        &self.lsp_host
    }

    pub fn active_workspace(&self) -> &Workspace {
        &self.workspaces[self.active_workspace]
    }

    pub fn active_workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active_workspace]
    }

    pub(crate) fn size(&self) -> Rect {
        self.size
    }

    /// Find the workspace that owns a Claude session, by searching for the
    /// session id in each workspace's [`Workspace::chats`] map. A session
    /// always belongs to the workspace where it was created; this lookup
    /// keeps chat-state updates routed correctly when messages arrive while
    /// a different workspace is active.
    pub(crate) fn workspace_owning_session(&self, id: ClaudeSessionId) -> Option<WorkspaceId> {
        self.workspaces
            .iter()
            .find(|(_, ws)| ws.chats.contains_key(&id))
            .map(|(wid, _)| wid)
    }

    /// Whether a Claude session is currently visible in its owning
    /// workspace's panes. A session with no owning workspace is never
    /// visible; a session owned by a non-active workspace is never visible
    /// from the user's perspective (they're looking at a different workspace).
    pub(crate) fn is_claude_visible(&self, id: ClaudeSessionId) -> bool {
        let Some(wid) = self.workspace_owning_session(id) else {
            return false;
        };
        if wid != self.active_workspace {
            return false;
        }
        self.workspaces[wid].is_claude_visible(id)
    }

    /// Badge label for a Claude session. Derived from the basename of the
    /// owning workspace's git root so multiple concurrent sessions remain
    /// distinguishable in the stacked tray. Falls back to `"claude"` when
    /// the git root has no basename (notably for test workspaces built
    /// from [`std::path::PathBuf::new`]).
    pub(crate) fn claude_badge_label(&self, id: ClaudeSessionId) -> String {
        self.workspace_owning_session(id)
            .and_then(|wid| self.workspaces[wid].git_root.file_name())
            .and_then(|name| name.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "claude".to_string())
    }

    /// Drop Claude badges whose sessions are currently visible. Runs after
    /// every action dispatch so opening a session (via pane placement, dock
    /// unhide, or any future show path) immediately dismisses a lingering
    /// badge. The passive visibility check in [`Self::handle_claude_message`]
    /// only fires on protocol events, so a `Complete` badge can otherwise
    /// outlive its session view indefinitely.
    pub(crate) fn sync_claude_badges(&mut self) {
        let ids: Vec<ClaudeSessionId> = self
            .workspaces
            .iter()
            .flat_map(|(_, ws)| ws.chats.keys().copied())
            .collect();
        for id in ids {
            if self.is_claude_visible(id) {
                self.badges.remove_by_source(BadgeSource::Claude(id));
            }
        }
    }

    pub fn set_claude_code_host(&mut self, host: Arc<dyn ClaudeCodeHost>) {
        self.claude_host = Some(host);
    }

    fn any_claude_active(&self) -> bool {
        self.workspaces
            .values()
            .any(|ws| ws.chats.values().any(|c| c.active_since.is_some()))
    }

    pub fn claude_sessions(&self) -> &ClaudeCodeSessions {
        &self.claude_sessions
    }

    pub fn claude_sessions_mut(&mut self) -> &mut ClaudeCodeSessions {
        &mut self.claude_sessions
    }

    /// Convenience wrapper that dispatches the [`OpenFile`] action with `path`.
    ///
    /// The action handler reads the file, creates a buffer, and shows it in
    /// the focused pane. A missing file becomes an empty buffer with the path
    /// attached (vim-style); other IO errors are logged and ignored.
    pub fn open_file(&mut self, path: &Path) {
        let action = OpenFile {
            path: path.to_path_buf(),
        };
        action_handlers::dispatch(self, &action);
    }

    pub fn open_review(&mut self) {
        action_handlers::dispatch(self, &OpenReview);
    }

    pub async fn run(
        &mut self,
        mut events: Receiver<Event>,
        render: Sender<Buffer>,
    ) -> io::Result<()> {
        loop {
            let active = self.any_claude_active();
            let effect = tokio::select! {
                biased;
                event = events.recv() => {
                    let Some(event) = event else { break };
                    self.update(event)
                }
                notif = self.pty_rx.recv() => {
                    let Some(notif) = notif else { continue };
                    self.handle_pty_notification(notif)
                }
                notif = self.claude_rx.recv() => {
                    let Some(notif) = notif else { continue };
                    self.handle_claude_notification(notif)
                }
                _ = async {
                    if active {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await
                    } else {
                        std::future::pending::<()>().await
                    }
                } => UpdateEffect::Redraw,
            };
            match effect {
                UpdateEffect::Redraw => {
                    if render.send(self.render()).await.is_err() {
                        break;
                    }
                },
                UpdateEffect::Quit => {
                    self.save_all_workspaces();
                    break;
                },
                UpdateEffect::None => {},
            }
        }
        Ok(())
    }

    /// Rehydrate the active workspace from its most-recently-modified
    /// persisted file under `$XDG_STATE_HOME/stoat/workspaces/<hash>/`. The
    /// binary only invokes this when the user passes `--continue`; a bare
    /// `stoat` launch leaves the default fresh workspace in place so each
    /// session starts clean. Tests intentionally skip this to stay isolated
    /// from the real state directory.
    pub fn load_active_workspace_state(&mut self) {
        let git_root = self.active_workspace().git_root.clone();
        let files = match crate::workspace::list_workspace_files(&git_root, &*self.fs_host) {
            Ok(files) => files,
            Err(err) => {
                tracing::warn!(?err, "could not resolve workspace state directory");
                return;
            },
        };
        let Some(path) = files.into_iter().next() else {
            return;
        };
        let executor = self.executor.clone();
        let fs_host = self.fs_host.clone();
        if let Err(err) = self
            .active_workspace_mut()
            .restore_state(&path, &*fs_host, &executor)
        {
            tracing::warn!(
                ?path,
                ?err,
                "failed to restore workspace state; starting fresh"
            );
        }
    }

    /// Persist a workspace's state to disk. Failures are logged and swallowed
    /// so a write error never prevents a clean shutdown or workspace switch.
    /// No-op when [`Self::persistence_disabled`] is set (used by the test
    /// harness to keep the real `$XDG_STATE_HOME` pristine) or when the
    /// workspace is still in its freshly-created state per
    /// [`Workspace::is_fresh`], so launches without `--continue` do not
    /// write a throwaway session file on quit.
    pub(crate) fn save_workspace(&self, ws: &Workspace) {
        if self.persistence_disabled {
            return;
        }
        if ws.is_fresh() {
            return;
        }
        let path = match crate::workspace::state_path_for(&ws.git_root, ws.uid, &*self.fs_host) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(?err, "could not resolve workspace state path");
                return;
            },
        };
        if let Err(err) = ws.save_state(&path, &*self.fs_host) {
            tracing::warn!(?path, ?err, "failed to save workspace state");
        }
    }

    /// Persist every open workspace. Invoked on quit so workspaces that were
    /// left in the background get their latest state written out.
    fn save_all_workspaces(&self) {
        for ws in self.workspaces.values() {
            self.save_workspace(ws);
        }
    }

    pub(crate) fn update(&mut self, event: Event) -> UpdateEffect {
        self.drain_lsp_notifications();
        match event {
            Event::Resize(w, h) => {
                self.size = Rect::new(0, 0, w, h);
                let size = self.size;
                self.active_workspace_mut().layout(size);
                UpdateEffect::Redraw
            },
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            _ => UpdateEffect::None,
        }
    }

    /// Drains every notification currently buffered on
    /// [`crate::host::LspHost::try_recv_notification`] and dispatches
    /// each by variant. `Progress` updates the [`crate::lsp::progress::LspProgressMap`];
    /// other variants log via tracing for now and become future
    /// per-feature consumer hooks. Cap is per-tick to avoid starving
    /// the event loop on a pathological notification burst; the
    /// remainder drains on the next update.
    pub(crate) fn drain_lsp_notifications(&mut self) {
        use futures::FutureExt;
        let host = self.lsp_host.clone();
        for _ in 0..256 {
            // try_recv_notification is implemented on top of a
            // non-blocking channel poll, so its future resolves
            // synchronously; now_or_never returns Some immediately.
            // Any host that breaks that contract returns None here
            // and the drain ends safely.
            let Some(slot) = host.try_recv_notification().now_or_never() else {
                break;
            };
            let Some(notification) = slot else {
                break;
            };
            if !self.lsp_progress.update(&notification) {
                tracing::debug!(
                    target: "stoat::app",
                    ?notification,
                    "unhandled LSP notification"
                );
            }
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> UpdateEffect {
        let Some((col, row)) = self.translate_mouse_to_focused(mouse.column, mouse.row) else {
            return UpdateEffect::None;
        };
        if self.handle_run_pane_mouse(mouse.kind, col, row) {
            return UpdateEffect::Redraw;
        }
        tracing::trace!(
            target: "stoat::app",
            kind = ?mouse.kind,
            col,
            row,
            "mouse event routed to focused element"
        );
        UpdateEffect::None
    }

    /// Routes left-button Down/Drag/Up events on a focused run pane into
    /// the active block's [`GridSelection`]. Returns `true` when the event
    /// mutated state. `Up(Left)` finalises the drag by extracting the
    /// row-major selection text and pushing it to the
    /// [`crate::host::ClipboardHost`]; the selection itself persists in
    /// place. Click-without-drag (`anchor == head`) is a no-op.
    fn handle_run_pane_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) -> bool {
        let target = {
            let ws = self.active_workspace();
            match ws.focus {
                FocusTarget::SplitPane(pane_id) => {
                    let pane = ws.panes.pane(pane_id);
                    if let View::Run(id) = pane.view {
                        Some((id, pane.area))
                    } else {
                        None
                    }
                },
                FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).and_then(|dock| {
                    if let View::Run(id) = dock.view {
                        Some((id, dock.area))
                    } else {
                        None
                    }
                }),
            }
        };
        let Some((run_id, area)) = target else {
            return false;
        };
        let clipboard_host = self.clipboard_host.clone();
        let ws = self.active_workspace_mut();
        let Some(run_state) = ws.runs.get_mut(run_id) else {
            return false;
        };
        let pos = run_state.active_block_grid_pos(area, col, row);
        let Some(block) = run_state.active_block_mut() else {
            return false;
        };
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(pos) = pos else {
                    return false;
                };
                block.selection = Some(GridSelection {
                    anchor: pos,
                    head: pos,
                });
                true
            },
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(pos) = pos else {
                    return false;
                };
                let Some(sel) = block.selection.as_mut() else {
                    return false;
                };
                if sel.head == pos {
                    return false;
                }
                sel.head = pos;
                true
            },
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(sel) = block.selection.as_ref() else {
                    return false;
                };
                if sel.anchor == sel.head {
                    return false;
                }
                let text = block.grid.text_for_selection(sel);
                if text.is_empty() {
                    return false;
                }
                if let Err(err) = clipboard_host.set(&text) {
                    tracing::warn!(
                        target: "stoat::app",
                        error = %err,
                        "clipboard write failed"
                    );
                }
                false
            },
            _ => false,
        }
    }

    /// Returns the focused element's area-relative cell for the given
    /// terminal-relative `(column, row)`. Coordinates above or left of
    /// the focused element saturate to `0`. Returns `None` when the
    /// focus points at a dock that no longer exists in the workspace's
    /// dock map.
    pub(crate) fn translate_mouse_to_focused(&self, column: u16, row: u16) -> Option<(u16, u16)> {
        let ws = self.active_workspace();
        let area = match ws.focus {
            FocusTarget::SplitPane(pane_id) => ws.panes.pane(pane_id).area,
            FocusTarget::Dock(dock_id) => ws.docks.get(dock_id)?.area,
        };
        Some((column.saturating_sub(area.x), row.saturating_sub(area.y)))
    }

    fn handle_key(&mut self, key: KeyEvent) -> UpdateEffect {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(run_id) = self.modal_run {
                let ws = self.active_workspace_mut();
                if let Some(run_state) = ws.runs.get_mut(run_id) {
                    if let Some(handle) = &mut run_state.shell_handle {
                        handle.kill();
                    }
                    if let Some(block) = run_state.active_block_mut() {
                        block.finished = true;
                    }
                }
                return UpdateEffect::Redraw;
            }
            if self.help.is_some() {
                action_handlers::close_help(self);
                return UpdateEffect::Redraw;
            }
            if self.file_finder.is_some() {
                action_handlers::close_file_finder(self);
                return UpdateEffect::Redraw;
            }
            if let Some(palette) = self.command_palette.take() {
                let active_idx = self.active_workspace;
                palette.dispose(&mut self.workspaces[active_idx]);
                self.mode = palette.previous_mode;
                return UpdateEffect::Redraw;
            }
            if self.workspace_picker.is_some() {
                self.workspace_picker = None;
                return UpdateEffect::Redraw;
            }
            if self.mode == "run" {
                return action_handlers::dispatch(self, &stoat_action::RunInterrupt);
            }
            return UpdateEffect::Quit;
        }

        let key = normalize_shift_letter(key);

        if let Some(run_id) = self.modal_run {
            let finished = self
                .active_workspace()
                .runs
                .get(run_id)
                .is_some_and(|r| !r.is_running());
            if finished && key.code == KeyCode::Esc {
                self.active_workspace_mut().runs.remove(run_id);
                self.modal_run = None;
                return UpdateEffect::Redraw;
            }
            return UpdateEffect::None;
        }

        if self.workspace_picker.is_some() {
            return self.dispatch_workspace_picker_key(key);
        }

        if self.mode == "insert"
            || self.mode == "reword_insert"
            || self.mode == "prompt"
            || self.mode == "run"
        {
            if let Some(effect) = self.handle_insert_key(key) {
                // If help is open, keep its filtered list in sync after every
                // text mutation in the prompt input.
                if self.help.is_some() {
                    let active_idx = self.active_workspace;
                    let workspaces = &mut self.workspaces;
                    if let Some(help) = self.help.as_mut() {
                        help.sync_filter(&workspaces[active_idx]);
                    }
                }
                return effect;
            }
        }

        if (self.mode == "normal" || self.mode == "select") && self.pending_find.is_some() {
            if let KeyCode::Char(ch) = key.code {
                let (kind, extend, count) = self.pending_find.take().expect("checked above");
                return action_handlers::movement::execute_find(self, kind, ch, extend, count);
            }
            self.pending_find = None;
        }

        let count_active_mode = self.mode == "normal" || self.mode == "select";
        if count_active_mode && self.pending_count.is_some() && key.modifiers.is_empty() {
            if let KeyCode::Char(ch) = key.code {
                if ch.is_ascii_digit() {
                    let digit = ch.to_digit(10).expect("ascii digit");
                    let new_count = self
                        .pending_count
                        .unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add(digit);
                    self.pending_count = Some(new_count);
                    return UpdateEffect::Redraw;
                }
            }
        }

        let state = StoatKeymapState::from_stoat(self);
        let actions = self.keymap.lookup(&state, &key).map(|a| a.to_vec());
        let Some(actions) = actions else {
            if count_active_mode {
                if let KeyCode::Char(ch) = key.code {
                    if ch.is_ascii_digit() && key.modifiers.is_empty() {
                        let digit = ch.to_digit(10).expect("ascii digit");
                        self.pending_count = Some(digit);
                        return UpdateEffect::Redraw;
                    }
                }
            }
            return UpdateEffect::None;
        };

        let mut effect = UpdateEffect::None;
        let mut dispatched_action = false;
        for ra in &actions {
            if ra.name == "SetMode" {
                if let Some(mode_name) = ra.args.first().and_then(crate::keymap_state::arg_as_str) {
                    self.mode = mode_name;
                    effect = UpdateEffect::Redraw;
                }
                continue;
            }
            if let Some(action) = resolve_action(&ra.name, &ra.args) {
                dispatched_action = true;
                let e = action_handlers::dispatch(self, &*action);
                match e {
                    UpdateEffect::Quit => return UpdateEffect::Quit,
                    UpdateEffect::Redraw => effect = UpdateEffect::Redraw,
                    UpdateEffect::None => {},
                }
            }
        }
        if dispatched_action {
            self.pending_count = None;
        }
        effect
    }

    pub(crate) fn take_pending_count(&mut self) -> Option<u32> {
        self.pending_count.take()
    }

    fn focused_editor_ids(&self) -> Option<(EditorId, BufferId)> {
        let ws = self.active_workspace();

        if let Some(finder) = &self.file_finder {
            return Some((finder.input.editor_id, finder.input.buffer_id));
        }

        if let Some(palette) = &self.command_palette {
            if let Some(input) = palette.focused_input() {
                return Some((input.editor_id, input.buffer_id));
            }
        }

        if let Some(help) = &self.help {
            return Some((help.input.editor_id, help.input.buffer_id));
        }

        if let Some((editor_id, buffer_id)) = ws
            .rebase_active
            .as_ref()
            .and_then(|a| a.pause.as_ref())
            .and_then(|p| match p {
                RebasePause::Reword { input, .. } => Some((input.editor_id, input.buffer_id)),
                _ => None,
            })
        {
            return Some((editor_id, buffer_id));
        }

        let view = match ws.focus {
            FocusTarget::SplitPane(_) => {
                let focused = ws.panes.focus();
                ws.panes.pane(focused).view.clone()
            },
            FocusTarget::Dock(dock_id) => match ws.docks.get(dock_id) {
                Some(dock) => dock.view.clone(),
                None => return None,
            },
        };
        match view {
            View::Editor(id) => {
                let editor = ws.editors.get(id)?;
                Some((id, editor.buffer_id))
            },
            View::Claude(session_id) => {
                let chat = ws.chats.get(&session_id)?;
                Some((chat.input.editor_id, chat.input.buffer_id))
            },
            View::Run(id) => {
                let run_state = ws.runs.get(id)?;
                Some((run_state.input.editor_id, run_state.input.buffer_id))
            },
            _ => None,
        }
    }

    fn focused_is_claude(&self) -> bool {
        let ws = self.active_workspace();
        let view = match ws.focus {
            FocusTarget::SplitPane(_) => {
                let focused = ws.panes.focus();
                &ws.panes.pane(focused).view
            },
            FocusTarget::Dock(dock_id) => match ws.docks.get(dock_id) {
                Some(dock) => &dock.view,
                None => return false,
            },
        };
        matches!(view, View::Claude(_))
    }

    fn handle_insert_key(&mut self, key: KeyEvent) -> Option<UpdateEffect> {
        let (editor_id, buffer_id) = self.focused_editor_ids()?;

        match key.code {
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                self.editor_insert(editor_id, buffer_id, s);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Backspace => {
                self.editor_backspace(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Delete => {
                self.editor_delete(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.editor_insert(editor_id, buffer_id, "\n");
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Enter if key.modifiers.is_empty() => {
                if self.focused_is_claude() || self.mode == "prompt" || self.mode == "run" {
                    None
                } else {
                    self.editor_insert(editor_id, buffer_id, "\n");
                    Some(UpdateEffect::Redraw)
                }
            },
            KeyCode::Left => {
                action_handlers::dispatch(self, &stoat_action::MoveLeft);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Right => {
                action_handlers::dispatch(self, &stoat_action::MoveRight);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Up if self.mode != "run" && self.mode != "prompt" => {
                action_handlers::dispatch(self, &stoat_action::MoveUp);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Down if self.mode != "run" && self.mode != "prompt" => {
                action_handlers::dispatch(self, &stoat_action::MoveDown);
                Some(UpdateEffect::Redraw)
            },
            _ => None,
        }
    }

    fn editor_insert(&mut self, editor_id: EditorId, buffer_id: BufferId, text: &str) {
        let ws = self.active_workspace_mut();
        let editor = match ws.editors.get_mut(editor_id) {
            Some(e) => e,
            None => return,
        };
        let buffer = match ws.buffers.get(buffer_id) {
            Some(b) => b,
            None => return,
        };
        let display_snapshot = editor.display_map.snapshot();
        let buf_snapshot = display_snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor().clone();
        let offset = buf_snapshot.resolve_anchor(&sel.head());
        {
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(offset..offset, text);
        }
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let new_offset = offset + text.len();
        let anchor = new_buf.anchor_at(new_offset, Bias::Right);
        editor.selections.transform(new_buf, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, stoat_text::SelectionGoal::None);
            new
        });
    }

    fn editor_backspace(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        let ws = self.active_workspace_mut();
        let editor = match ws.editors.get_mut(editor_id) {
            Some(e) => e,
            None => return,
        };
        let buffer = match ws.buffers.get(buffer_id) {
            Some(b) => b,
            None => return,
        };
        let display_snapshot = editor.display_map.snapshot();
        let buf_snapshot = display_snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor().clone();
        let offset = buf_snapshot.resolve_anchor(&sel.head());
        if offset == 0 {
            return;
        }
        let rope = buf_snapshot.rope();
        let prev_len = rope
            .reversed_chars_at(offset)
            .next()
            .map(|ch| ch.len_utf8())
            .unwrap_or(0);
        let start = offset - prev_len;
        {
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(start..offset, "");
        }
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let anchor = new_buf.anchor_at(start, Bias::Right);
        editor.selections.transform(new_buf, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, stoat_text::SelectionGoal::None);
            new
        });
    }

    fn editor_delete(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        let ws = self.active_workspace_mut();
        let editor = match ws.editors.get_mut(editor_id) {
            Some(e) => e,
            None => return,
        };
        let buffer = match ws.buffers.get(buffer_id) {
            Some(b) => b,
            None => return,
        };
        let display_snapshot = editor.display_map.snapshot();
        let buf_snapshot = display_snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor().clone();
        let offset = buf_snapshot.resolve_anchor(&sel.head());
        let rope = buf_snapshot.rope();
        let next_len = rope
            .chars_at(offset)
            .next()
            .map(|ch| ch.len_utf8())
            .unwrap_or(0);
        if next_len == 0 {
            return;
        }
        let end = offset + next_len;
        let mut guard = buffer.write().expect("poisoned");
        guard.edit(offset..end, "");
    }

    pub(crate) fn handle_pty_notification(&mut self, notif: PtyNotification) -> UpdateEffect {
        let clipboard_host = self.clipboard_host.clone();
        let ws = self.active_workspace_mut();
        match notif {
            PtyNotification::Output { run_id, data } => {
                let Some(run_state) = ws.runs.get_mut(run_id) else {
                    return UpdateEffect::None;
                };
                let Some(block) = run_state.active_block_mut() else {
                    return UpdateEffect::None;
                };
                block.feed(&data);
                for text in block.grid.clipboard_writes.drain(..) {
                    if let Err(err) = clipboard_host.set(&text) {
                        tracing::warn!(
                            target: "stoat::app",
                            error = %err,
                            "clipboard write failed"
                        );
                    }
                }
                if block.grid.alt_screen_detected {
                    block.error = Some("this command requires a full terminal".into());
                    block.finished = true;
                    block.grid.alt_screen_detected = false;
                    if let Some(handle) = &mut run_state.shell_handle {
                        handle.kill();
                    }
                    run_state.shell_handle = None;
                }
                UpdateEffect::Redraw
            },
            PtyNotification::CommandDone {
                run_id,
                exit_status,
            } => {
                let Some(run_state) = ws.runs.get_mut(run_id) else {
                    return UpdateEffect::None;
                };
                let Some(block) = run_state.active_block_mut() else {
                    return UpdateEffect::None;
                };
                if !block.finished {
                    block.finished = true;
                    block.exit_status = exit_status;
                }
                UpdateEffect::Redraw
            },
        }
    }

    fn handle_claude_notification(&mut self, notif: ClaudeNotification) -> UpdateEffect {
        match notif {
            ClaudeNotification::CreateRequested { session_id } => {
                if let Some(host) = self.claude_host.clone() {
                    let tx = self.claude_tx.clone();
                    self.executor
                        .spawn(async move {
                            match host.new_session().await {
                                Ok(session) => {
                                    let _ = tx
                                        .send(ClaudeNotification::SessionReady {
                                            session_id,
                                            session,
                                        })
                                        .await;
                                },
                                Err(e) => {
                                    let _ = tx
                                        .send(ClaudeNotification::SessionError {
                                            session_id,
                                            error: e.to_string(),
                                        })
                                        .await;
                                },
                            }
                        })
                        .detach();
                }
                UpdateEffect::None
            },
            ClaudeNotification::SessionReady {
                session_id,
                session,
            } => {
                let session: Arc<dyn crate::host::ClaudeCodeSession> = Arc::from(session);
                let claude_tx = self.claude_tx.clone();
                self.claude_sessions.fill_slot(session_id, session.clone());
                self.executor
                    .spawn(claude_polling_task(session_id, session.clone(), claude_tx))
                    .detach();

                let pending = {
                    let ws = self.active_workspace_mut();
                    ws.chats
                        .get_mut(&session_id)
                        .map(|chat| std::mem::take(&mut chat.pending_sends))
                        .unwrap_or_default()
                };
                if !pending.is_empty() {
                    self.executor
                        .spawn(async move {
                            for text in pending {
                                if let Err(e) = session.send(&text).await {
                                    tracing::error!("claude pending send error: {e}");
                                    break;
                                }
                            }
                        })
                        .detach();
                }

                UpdateEffect::Redraw
            },
            ClaudeNotification::SessionError { session_id, error } => {
                use crate::claude_chat::{ChatMessage, ChatMessageContent, ChatRole};
                tracing::error!("claude session {session_id:?} failed: {error}");
                self.claude_sessions.remove(session_id);
                let ws = self.active_workspace_mut();
                if let Some(chat) = ws.chats.get_mut(&session_id) {
                    chat.messages.push(ChatMessage {
                        role: ChatRole::Assistant,
                        content: ChatMessageContent::Error(format!(
                            "Failed to start session: {error}"
                        )),
                    });
                }
                UpdateEffect::Redraw
            },
            ClaudeNotification::Message {
                session_id,
                message,
            } => self.handle_claude_message(session_id, &message),
        }
    }

    /// Drain every queued [`ClaudeNotification`] and route it through
    /// [`Self::handle_claude_notification`]. Used by the test harness in its
    /// settle loop to advance the real transport pipeline without the
    /// production `tokio::select!` in [`Self::run`]. Returns `true` if at
    /// least one notification was processed, so callers can loop until the
    /// pipeline reaches a fixed point.
    #[cfg(test)]
    pub(crate) fn drain_claude_notifications(&mut self) -> bool {
        let mut progressed = false;
        while let Ok(notif) = self.claude_rx.try_recv() {
            self.handle_claude_notification(notif);
            progressed = true;
        }
        progressed
    }

    pub(crate) fn handle_claude_message(
        &mut self,
        session_id: ClaudeSessionId,
        message: &AgentMessage,
    ) -> UpdateEffect {
        use crate::claude_chat::{ChatMessage, ChatMessageContent, ChatRole};

        let visible = self.is_claude_visible(session_id);
        let owning = self.workspace_owning_session(session_id);
        let mut follow_action: Option<(
            WorkspaceId,
            crate::host::ToolKind,
            Vec<crate::host::ToolCallLocation>,
        )> = None;

        if let Some(wid) = owning {
            let chat_ws = &mut self.workspaces[wid];
            if let Some(chat) = chat_ws.chats.get_mut(&session_id) {
                match message {
                    AgentMessage::Text { text } => {
                        chat.streaming_text = None;
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            chat.messages.push(ChatMessage {
                                role: ChatRole::Assistant,
                                content: ChatMessageContent::Text(trimmed.to_string()),
                            });
                        }
                    },
                    AgentMessage::PartialText { text } => {
                        chat.streaming_text = Some(text.clone());
                    },
                    AgentMessage::Thinking { text, .. } => {
                        chat.streaming_text = None;
                        chat.messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            content: ChatMessageContent::Thinking { text: text.clone() },
                        });
                    },
                    AgentMessage::ToolUse {
                        id,
                        name,
                        input,
                        kind,
                        locations,
                        ..
                    } => {
                        chat.messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            content: ChatMessageContent::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                            },
                        });
                        if chat.follow {
                            follow_action = Some((wid, *kind, locations.clone()));
                        }
                    },
                    AgentMessage::ToolResult { id, content, .. } => {
                        chat.messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            content: ChatMessageContent::ToolResult {
                                id: id.clone(),
                                content: content.clone(),
                            },
                        });
                    },
                    AgentMessage::Result {
                        cost_usd,
                        duration_ms,
                        num_turns,
                    } => {
                        chat.active_since = None;
                        chat.messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            content: ChatMessageContent::TurnComplete {
                                cost_usd: *cost_usd,
                                duration_ms: *duration_ms,
                                num_turns: *num_turns,
                            },
                        });
                    },
                    AgentMessage::Error { message: msg } => {
                        chat.active_since = None;
                        chat.messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            content: ChatMessageContent::Error(msg.clone()),
                        });
                    },
                    AgentMessage::Init {
                        session_id: proto_id,
                        ..
                    } => {
                        chat.protocol_session_id = Some(proto_id.clone());
                    },
                    AgentMessage::Unknown { .. }
                    | AgentMessage::ServerToolUse { .. }
                    | AgentMessage::ServerToolResult { .. }
                    | AgentMessage::ToolUpdate { .. }
                    | AgentMessage::PartialToolInput { .. }
                    | AgentMessage::Plan { .. }
                    | AgentMessage::Usage { .. }
                    | AgentMessage::ModeChanged { .. }
                    | AgentMessage::ModelChanged { .. }
                    | AgentMessage::FilesPersisted { .. }
                    | AgentMessage::ElicitationComplete { .. }
                    | AgentMessage::AuthRequired { .. }
                    | AgentMessage::SessionState(_)
                    | AgentMessage::TaskEvent(_)
                    | AgentMessage::Hook(_) => {},
                }
            }
        }

        if let Some((wid, kind, locations)) = follow_action {
            action_handlers::handle_follow_tool_use(self, wid, kind, &locations);
        }

        let source = BadgeSource::Claude(session_id);
        let label = self.claude_badge_label(session_id);
        let tray = &mut self.badges;

        match message {
            AgentMessage::Thinking { .. }
            | AgentMessage::ToolUse { .. }
            | AgentMessage::ToolResult { .. }
            | AgentMessage::Text { .. }
            | AgentMessage::PartialText { .. }
            | AgentMessage::ServerToolUse { .. }
            | AgentMessage::ServerToolResult { .. } => {
                if visible {
                    tray.remove_by_source(source);
                } else {
                    match tray.find_by_source(source) {
                        Some(id) => {
                            if let Some(badge) = tray.get_mut(id) {
                                badge.state = BadgeState::Active;
                                badge.detail = detail_for_message(message);
                            }
                        },
                        None => {
                            tray.insert(Badge {
                                source,
                                anchor: Anchor::TopCenter,
                                state: BadgeState::Active,
                                label: label.clone(),
                                detail: detail_for_message(message),
                            });
                        },
                    }
                }
                UpdateEffect::Redraw
            },
            AgentMessage::Result { .. } => {
                if visible {
                    tray.remove_by_source(source);
                } else {
                    match tray.find_by_source(source) {
                        Some(id) => {
                            if let Some(badge) = tray.get_mut(id) {
                                badge.state = BadgeState::Complete;
                                badge.detail = None;
                            }
                        },
                        None => {
                            tray.insert(Badge {
                                source,
                                anchor: Anchor::TopCenter,
                                state: BadgeState::Complete,
                                label: label.clone(),
                                detail: None,
                            });
                        },
                    }
                }
                UpdateEffect::Redraw
            },
            AgentMessage::Error { message: msg } => {
                if visible {
                    tray.remove_by_source(source);
                } else {
                    match tray.find_by_source(source) {
                        Some(id) => {
                            if let Some(badge) = tray.get_mut(id) {
                                badge.state = BadgeState::Error;
                                badge.detail = Some(msg.clone());
                            }
                        },
                        None => {
                            tray.insert(Badge {
                                source,
                                anchor: Anchor::TopCenter,
                                state: BadgeState::Error,
                                label: label.clone(),
                                detail: Some(msg.clone()),
                            });
                        },
                    }
                }
                UpdateEffect::Redraw
            },
            AgentMessage::AuthRequired { reason } => {
                if visible {
                    tray.remove_by_source(source);
                } else {
                    match tray.find_by_source(source) {
                        Some(id) => {
                            if let Some(badge) = tray.get_mut(id) {
                                badge.state = BadgeState::Error;
                                badge.detail = Some(reason.clone());
                            }
                        },
                        None => {
                            tray.insert(Badge {
                                source,
                                anchor: Anchor::TopCenter,
                                state: BadgeState::Error,
                                label: label.clone(),
                                detail: Some(reason.clone()),
                            });
                        },
                    }
                }
                UpdateEffect::Redraw
            },
            AgentMessage::Init { .. }
            | AgentMessage::Unknown { .. }
            | AgentMessage::ToolUpdate { .. }
            | AgentMessage::PartialToolInput { .. }
            | AgentMessage::Plan { .. }
            | AgentMessage::Usage { .. }
            | AgentMessage::ModeChanged { .. }
            | AgentMessage::ModelChanged { .. }
            | AgentMessage::FilesPersisted { .. }
            | AgentMessage::ElicitationComplete { .. }
            | AgentMessage::SessionState(_)
            | AgentMessage::TaskEvent(_)
            | AgentMessage::Hook(_) => UpdateEffect::None,
        }
    }

    /// Drive background parse jobs: poll any in-flight tasks for completion,
    /// install their results, then spawn new jobs for visible buffers whose
    /// stored syntax version is stale.
    ///
    /// At most one job per buffer is in flight at a time. If a buffer advances
    /// past the in-flight job's `target_version`, the new job is queued only
    /// after the old one completes. Anchors in the result are computed using
    /// the parsed snapshot, so they remain valid even if the buffer has been
    /// edited further while the parse was running.
    fn drive_parse_jobs(&mut self) {
        let Self {
            workspaces,
            active_workspace,
            executor,
            syntax_styles,
            ..
        } = self;
        workspaces[*active_workspace].drive_parse_jobs(executor, syntax_styles);
    }

    pub(crate) fn render(&mut self) -> Buffer {
        self.render_tick += 1;
        self.drive_parse_jobs();
        action_handlers::pump_commits(self);
        let mut buf = Buffer::empty(self.size);
        crate::render::frame(self, &mut buf);
        buf
    }

    fn dispatch_workspace_picker_key(&mut self, key: KeyEvent) -> UpdateEffect {
        let outcome = match self.workspace_picker.as_mut() {
            Some(picker) => picker.handle_key(key),
            None => return UpdateEffect::None,
        };
        match outcome {
            PickerOutcome::None => UpdateEffect::Redraw,
            PickerOutcome::Close => {
                self.workspace_picker = None;
                UpdateEffect::Redraw
            },
            PickerOutcome::Select(id) => {
                self.workspace_picker = None;
                if id == self.active_workspace {
                    return UpdateEffect::Redraw;
                }
                self.save_workspace(self.active_workspace());
                self.active_workspace = id;
                let size = self.size;
                self.active_workspace_mut().layout(size);
                UpdateEffect::Redraw
            },
        }
    }
}

/// Synchronous core of the parse pipeline. When `deadline` is `Some`, the
/// host parse aborts if it would exceed it and the function returns `None`,
/// signalling that the caller should fall back to the background path.
/// `None` is also returned for ordinary parse failures (unsupported
/// language, etc.); the difference does not matter for the call sites.
pub(crate) fn parse_buffer_step(
    buffer_id: BufferId,
    snapshot: TextBufferSnapshot,
    lang: &Arc<Language>,
    prior: &mut Option<SyntaxState>,
    prior_syntax_map: &mut Option<stoat_language::SyntaxMap>,
    styles: &SyntaxStyles,
    deadline: Option<std::time::Instant>,
) -> Option<ParseJobOutput> {
    let cur_version = snapshot.version;
    let new_rope = snapshot.visible_text.clone();

    // Edit a clone of the prior tree rather than mutating it in place. If
    // the parse aborts (deadline exceeded, etc.) the caller's prior must
    // remain valid for the next attempt; an in-place edit would leave the
    // registry holding a half-edited tree that would double-stamp position
    // offsets when re-edited next call.
    //
    // tree_sitter::Tree::clone is O(1) (refcount bump on the root subtree),
    // and tree.edit goes through ts_subtree_edit which is copy-on-write, so
    // editing the clone leaves the original untouched.
    let edited_tree = prior.as_ref().map(|prev| {
        let mut tree = prev.tree.clone();
        let edits = snapshot.edits_since(prev.version);
        language::edit_tree(&mut tree, edits.edits(), &prev.rope_snapshot, &new_rope);
        tree
    });

    let tree = match edited_tree.as_ref() {
        Some(old_tree) => match deadline {
            Some(dl) => language::parse_rope_within(lang, &new_rope, Some(old_tree), dl)?,
            None => language::parse_rope(lang, &new_rope, Some(old_tree))?,
        },
        None => match deadline {
            Some(dl) => language::parse_rope_within(lang, &new_rope, None, dl)?,
            None => language::parse_rope(lang, &new_rope, None)?,
        },
    };

    let prev_injection_trees = prior
        .take()
        .map(|prev| prev.injection_trees)
        .unwrap_or_default();

    let extracted =
        language::extract_highlights_rope_with_cache(lang, &tree, &new_rope, prev_injection_trees);
    // Theme-driven path: span.id is set to the theme key index by
    // collect_highlights_into via language.highlight_map(). Spans whose id
    // is DEFAULT (capture not in the active theme) are skipped because
    // they have no rendered style.
    let tokens: Arc<[SemanticTokenHighlight]> = extracted
        .spans
        .into_iter()
        .filter_map(|sp| {
            let style_id = styles.id_for_highlight(sp.id)?;
            Some(SemanticTokenHighlight {
                // Insertions at the start of a token attach to the previous
                // span, not this one; insertions at the end attach to the
                // next span. Keeps a typed character from silently extending
                // a keyword or string into neighboring text.
                range: snapshot.anchor_at(sp.byte_range.start, Bias::Right)
                    ..snapshot.anchor_at(sp.byte_range.end, Bias::Left),
                style: style_id,
            })
        })
        .collect();

    // Drive the multi-layer SyntaxMap alongside the legacy SyntaxState.
    // We don't have an interpolation pass on the host side yet (it would
    // need anchored byte offsets), so each parse produces a fresh SyntaxMap
    // from scratch; the prior_syntax_map is consumed but only its captured
    // tree is reused via SyntaxMap::reparse's internal `prior_injections`
    // snapshot.
    let mut syntax_map = prior_syntax_map.take().unwrap_or_default();
    let _ = syntax_map.reparse(&new_rope, lang.clone(), cur_version);

    Some(ParseJobOutput {
        buffer_id,
        syntax: SyntaxState {
            tree,
            version: cur_version,
            rope_snapshot: new_rope,
            injection_trees: extracted.injection_trees,
        },
        syntax_map,
        tokens,
    })
}

/// Background parse worker. Owns all inputs by value so the future is `Send`
/// and can run on any executor thread.
pub(crate) async fn parse_buffer_async(
    buffer_id: BufferId,
    snapshot: TextBufferSnapshot,
    lang: Arc<Language>,
    mut prior: Option<SyntaxState>,
    mut prior_syntax_map: Option<stoat_language::SyntaxMap>,
    styles: SyntaxStyles,
) -> Option<ParseJobOutput> {
    parse_buffer_step(
        buffer_id,
        snapshot,
        &lang,
        &mut prior,
        &mut prior_syntax_map,
        &styles,
        None,
    )
}

pub(crate) async fn claude_polling_task(
    session_id: ClaudeSessionId,
    host: Arc<dyn crate::host::ClaudeCodeSession>,
    tx: Sender<ClaudeNotification>,
) {
    while let Some(message) = host.recv().await {
        if tx
            .send(ClaudeNotification::Message {
                session_id,
                message,
            })
            .await
            .is_err()
        {
            break;
        }
    }
}

fn detail_for_message(message: &AgentMessage) -> Option<String> {
    match message {
        AgentMessage::ToolUse { name, .. } => Some(name.clone()),
        AgentMessage::Thinking { .. } => Some("thinking".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::TextBuffer;
    use std::path::Path;

    /// When `parse_buffer_step` aborts on the deadline, the prior state
    /// passed via `&mut Option<_>` must remain populated so the caller
    /// can hand it to a follow-up parse without losing incrementality.
    #[test]
    fn parse_buffer_step_preserves_prior_on_deadline_abort() {
        let lang = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .unwrap();
        let styles = SyntaxStyles::from_theme(&crate::theme::Theme::empty());
        let buffer_id = BufferId::new(1);

        let text = "fn a() {}\n".repeat(10_000);
        let mut buf = TextBuffer::with_text(buffer_id, &text);
        let snap1 = buf.snapshot.clone();

        let mut prior: Option<SyntaxState> = None;
        let mut prior_map: Option<stoat_language::SyntaxMap> = None;
        let out = parse_buffer_step(
            buffer_id,
            snap1,
            &lang,
            &mut prior,
            &mut prior_map,
            &styles,
            None,
        )
        .expect("first parse should succeed");
        let initial_version = out.syntax.version;

        let mut prior: Option<SyntaxState> = Some(out.syntax);
        let mut prior_map: Option<stoat_language::SyntaxMap> = Some(out.syntax_map);
        buf.edit(0..0, "// edit\n");
        let snap2 = buf.snapshot.clone();

        let result = parse_buffer_step(
            buffer_id,
            snap2.clone(),
            &lang,
            &mut prior,
            &mut prior_map,
            &styles,
            Some(std::time::Instant::now()),
        );
        assert!(result.is_none(), "expected deadline abort to return None");
        let prior_state = prior
            .as_ref()
            .expect("prior must survive deadline abort, was consumed");
        assert_eq!(
            prior_state.version, initial_version,
            "prior version must be unchanged",
        );
        assert!(
            prior_map.is_some(),
            "prior_syntax_map must survive deadline abort",
        );

        let recovery = parse_buffer_step(
            buffer_id,
            snap2,
            &lang,
            &mut prior,
            &mut prior_map,
            &styles,
            None,
        )
        .expect("recovery parse should succeed");
        assert!(recovery.syntax.version > initial_version);
        assert!(prior.is_none(), "successful parse must consume the prior");
        assert!(prior_map.is_none());
    }

    #[test]
    fn snapshot_initial_plain() {
        let mut h = Stoat::test();
        h.assert_snapshot("initial_plain");
    }

    #[test]
    fn snapshot_initial_styled() {
        let mut h = Stoat::test();
        h.assert_snapshot("initial");
    }

    #[test]
    fn snapshot_space_mode() {
        let mut h = Stoat::test();
        h.type_keys("space");
        h.assert_snapshot("space_mode");
    }

    #[test]
    fn mouse_translates_to_focused_pane_coords() {
        let mut h = Stoat::test();
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(10, 5, 20, 8);
        let translated = h.stoat.translate_mouse_to_focused(15, 9);
        assert_eq!(translated, Some((5, 4)));
    }

    #[test]
    fn mouse_above_focused_pane_saturates_to_zero() {
        let mut h = Stoat::test();
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(10, 5, 20, 8);
        let translated = h.stoat.translate_mouse_to_focused(3, 2);
        assert_eq!(translated, Some((0, 0)));
    }

    #[test]
    fn mouse_routes_to_focused_dock_when_focus_is_dock() {
        use crate::pane::{DockPanel, DockSide, DockVisibility, View};
        let mut h = Stoat::test();
        let dock_id = h.stoat.active_workspace_mut().docks.insert(DockPanel {
            view: View::Label("dock".into()),
            side: DockSide::Right,
            visibility: DockVisibility::Open { width: 30 },
            default_width: 30,
            area: Rect::new(50, 0, 30, 24),
        });
        h.stoat.active_workspace_mut().focus = FocusTarget::Dock(dock_id);
        let translated = h.stoat.translate_mouse_to_focused(60, 7);
        assert_eq!(translated, Some((10, 7)));
    }

    #[test]
    fn mouse_returns_none_when_focused_dock_missing() {
        use crate::pane::DockId;
        let mut h = Stoat::test();
        let dangling = DockId::default();
        h.stoat.active_workspace_mut().focus = FocusTarget::Dock(dangling);
        let translated = h.stoat.translate_mouse_to_focused(10, 10);
        assert_eq!(translated, None);
    }

    #[test]
    fn snapshot_lsp_progress_indexing() {
        use crate::host::LspNotification;
        use lsp_types::{NumberOrString, WorkDoneProgress, WorkDoneProgressBegin};
        let mut h = Stoat::test();
        h.fake_lsp().push_notification(LspNotification::Progress {
            token: NumberOrString::Number(1),
            value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "indexing".into(),
                cancellable: None,
                message: None,
                percentage: Some(25),
            }),
        });
        h.drain_lsp();
        h.assert_snapshot("lsp_progress_indexing");
    }

    fn open_run_with_output(h: &mut crate::test_harness::TestHarness, output: &[u8]) -> RunId {
        let run_id = h.open_run();
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(0, 0, 40, 10);
        h.submit_run("ls");
        h.inject_run_output(run_id, output);
        run_id
    }

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn mouse_down_anchors_run_pane_selection() {
        let mut h = Stoat::test();
        let run_id = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 1));
        let block = h
            .stoat
            .active_workspace()
            .runs
            .get(run_id)
            .expect("run state exists")
            .active_block()
            .expect("active block exists");
        assert_eq!(
            block.selection,
            Some(GridSelection {
                anchor: (2, 0),
                head: (2, 0),
            }),
        );
    }

    #[test]
    fn mouse_drag_updates_run_pane_selection_head() {
        let mut h = Stoat::test();
        let run_id = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 4, 2));
        let block = h
            .stoat
            .active_workspace()
            .runs
            .get(run_id)
            .expect("run state exists")
            .active_block()
            .expect("active block exists");
        assert_eq!(
            block.selection,
            Some(GridSelection {
                anchor: (1, 0),
                head: (4, 1),
            }),
        );
    }

    #[test]
    fn mouse_up_leaves_run_pane_selection_in_place() {
        let mut h = Stoat::test();
        let run_id = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 3, 1));
        let block = h
            .stoat
            .active_workspace()
            .runs
            .get(run_id)
            .expect("run state exists")
            .active_block()
            .expect("active block exists");
        assert_eq!(
            block.selection,
            Some(GridSelection {
                anchor: (3, 0),
                head: (3, 0),
            }),
        );
    }

    #[test]
    fn mouse_down_outside_active_block_does_not_select() {
        let mut h = Stoat::test();
        let run_id = open_run_with_output(&mut h, b"hello\n");
        for (col, row) in [(2u16, 0u16), (2, 3), (2, 9), (50, 1)] {
            h.stoat.update(mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                col,
                row,
            ));
            let block = h
                .stoat
                .active_workspace()
                .runs
                .get(run_id)
                .expect("run state exists")
                .active_block()
                .expect("active block exists");
            assert_eq!(
                block.selection, None,
                "click at ({col},{row}) should not anchor",
            );
        }
    }

    #[test]
    fn mouse_drag_without_prior_down_is_noop() {
        let mut h = Stoat::test();
        let run_id = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 2, 1));
        let block = h
            .stoat
            .active_workspace()
            .runs
            .get(run_id)
            .expect("run state exists")
            .active_block()
            .expect("active block exists");
        assert_eq!(block.selection, None);
    }

    #[test]
    fn mouse_on_non_run_view_is_noop() {
        let mut h = Stoat::test();
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(0, 0, 40, 10);
        let effect = h
            .stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
        assert_eq!(effect, UpdateEffect::None);
    }

    #[test]
    fn mouse_up_after_drag_writes_selection_to_clipboard() {
        let mut h = Stoat::test();
        let _ = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 3, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 3, 1));
        assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
    }

    #[test]
    fn mouse_up_without_drag_skips_clipboard() {
        let mut h = Stoat::test();
        let _ = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 1));
        assert!(h.fake_clipboard().writes().is_empty());
    }

    #[test]
    fn mouse_up_with_no_selection_skips_clipboard() {
        let mut h = Stoat::test();
        let _ = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 1));
        assert!(h.fake_clipboard().writes().is_empty());
    }

    #[test]
    fn mouse_up_multi_row_drag_writes_joined_lines() {
        let mut h = Stoat::test();
        let _ = open_run_with_output(&mut h, b"foo\nbar\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 1, 2));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 1, 2));
        assert_eq!(h.fake_clipboard().writes(), vec!["oo\nba"]);
    }
}
