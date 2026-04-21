use crate::{
    action_handlers,
    badge::{Anchor, Badge, BadgeSource, BadgeState, BadgeTray, StackDirection},
    buffer::{BufferId, TextBufferSnapshot},
    buffer_registry::BufferRegistry,
    claude_chat::ClaudeChatState,
    command_palette::{CommandPalette, PaletteOutcome},
    commit_list::CommitListState,
    display_map::{highlights::SemanticTokenHighlight, syntax_theme::SyntaxStyles, BlockRowKind},
    editor_state::{EditorId, EditorState},
    help::{Help, HelpOutcome},
    host::{
        AgentMessage, ClaudeCodeHost, ClaudeCodeSessions, ClaudeNotification, ClaudeSessionId,
        CommitFileChange, CommitFileChangeKind, FsHost, GitHost, LocalFs, LocalGit, RebaseTodoOp,
    },
    keymap::{Keymap, KeymapState, ResolvedAction, ResolvedArg, StateValue},
    pane::{Divider, DividerOrientation, DockPanel, DockVisibility, FocusTarget, Pane, View},
    rebase::{ActiveRebase, RebasePause, RebaseState},
    review::ReviewRow,
    review_session::ReviewSession,
    run::{PtyNotification, RunId, RunState},
    workspace::{Workspace, WorkspaceId},
    workspace_picker::{PickerOutcome, WorkspacePicker},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Text,
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};
use slotmap::SlotMap;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_action::{Action, OpenFile, OpenReview};
use stoat_config::Settings;
use stoat_language::{self as language, Language, LanguageRegistry, SyntaxState};
use stoat_scheduler::Executor;
use stoat_text::Bias;
use tokio::sync::mpsc::{Receiver, Sender};

const DEFAULT_KEYMAP: &str = include_str!("../../config.stcfg");

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
    keymap: Keymap,
    pub settings: Settings,
    pub theme: crate::theme::Theme,
    pub(crate) command_palette: Option<CommandPalette>,
    pub(crate) help: Option<Help>,
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
    claude_host: Option<Arc<dyn ClaudeCodeHost>>,
    claude_sessions: ClaudeCodeSessions,
    pub(crate) claude_tx: Sender<ClaudeNotification>,
    claude_rx: Receiver<ClaudeNotification>,
    pub(crate) pty_tx: Sender<PtyNotification>,
    pty_rx: Receiver<PtyNotification>,
    pub(crate) modal_run: Option<RunId>,
    render_tick: u64,
    /// Filesystem the UI layer reads through. Swapped to
    /// [`crate::host::FakeFs`] in tests; all IO outside the host module
    /// itself must route through this field.
    pub(crate) fs_host: Arc<dyn FsHost>,
    /// Git operations flow through this trait so tests can use
    /// [`crate::host::FakeGit`] without a real repository.
    pub(crate) git_host: Arc<dyn GitHost>,
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
        // Install a theme-driven HighlightMap on every loaded language.
        // Done at registry-init time because adding new languages later
        // would also need a fresh HighlightMap; today the registry is
        // static so this one-shot install is sufficient.
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
            fs_host: Arc::new(LocalFs),
            git_host: Arc::new(LocalGit::new()),
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
    /// persisted file under `$XDG_STATE_HOME/stoat/workspaces/<hash>/`. Call
    /// this once after [`Self::new`] in the main binary so an existing
    /// workspace at `initial_git_root` is restored before the first frame.
    /// Tests intentionally skip this to stay isolated from the real state
    /// directory.
    pub fn load_active_workspace_state(&mut self) {
        let git_root = self.active_workspace().git_root.clone();
        let files = match crate::workspace::list_workspace_files(&git_root) {
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
        if let Err(err) = self.active_workspace_mut().restore_state(&path, &executor) {
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
    /// harness to keep the real `$XDG_STATE_HOME` pristine).
    pub(crate) fn save_workspace(&self, ws: &Workspace) {
        if self.persistence_disabled {
            return;
        }
        let path = match crate::workspace::state_path_for(&ws.git_root, ws.uid) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(?err, "could not resolve workspace state path");
                return;
            },
        };
        if let Err(err) = ws.save_state(&path) {
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
        match event {
            Event::Resize(w, h) => {
                self.size = Rect::new(0, 0, w, h);
                let size = self.size;
                self.active_workspace_mut().layout(size);
                UpdateEffect::Redraw
            },
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            _ => UpdateEffect::None,
        }
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
                self.help = None;
                return UpdateEffect::Redraw;
            }
            if self.command_palette.is_some() {
                self.command_palette = None;
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

        if self.help.is_some() {
            return self.dispatch_help_key(key);
        }

        if self.command_palette.is_some() {
            return self.dispatch_palette_key(key);
        }

        if self.workspace_picker.is_some() {
            return self.dispatch_workspace_picker_key(key);
        }

        if self.mode == "run" {
            if let Some(effect) = self.handle_run_key(key) {
                return effect;
            }
        }

        if self.mode == "insert" || self.mode == "reword_insert" {
            if let Some(effect) = self.handle_insert_key(key) {
                return effect;
            }
        }

        let state = StoatKeymapState::new(&self.mode);
        let Some(actions) = self.keymap.lookup(&state, &key) else {
            return UpdateEffect::None;
        };
        let actions = actions.to_vec();

        let mut effect = UpdateEffect::None;
        for ra in &actions {
            if ra.name == "SetMode" {
                if let Some(mode_name) = ra.args.first().and_then(arg_as_str) {
                    self.mode = mode_name;
                    effect = UpdateEffect::Redraw;
                }
                continue;
            }
            if let Some(action) = resolve_action(&ra.name, &ra.args) {
                let e = action_handlers::dispatch(self, &*action);
                match e {
                    UpdateEffect::Quit => return UpdateEffect::Quit,
                    UpdateEffect::Redraw => effect = UpdateEffect::Redraw,
                    UpdateEffect::None => {},
                }
            }
        }
        effect
    }

    fn handle_run_key(&mut self, key: KeyEvent) -> Option<UpdateEffect> {
        let ws = self.active_workspace_mut();
        let focused = ws.panes.focus();
        let View::Run(id) = ws.panes.pane(focused).view else {
            return None;
        };
        let run_state = ws.runs.get_mut(id)?;

        match key.code {
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                run_state.input.insert_char(ch);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Backspace => {
                run_state.input.delete_backward();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Delete => {
                run_state.input.delete_forward();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                run_state.input.move_word_left();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                run_state.input.move_word_right();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Left => {
                run_state.input.move_left();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Right => {
                run_state.input.move_right();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Home => {
                run_state.input.move_home();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::End => {
                run_state.input.move_end();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Up => {
                run_state.history_up();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Down => {
                run_state.history_down();
                Some(UpdateEffect::Redraw)
            },
            // Enter and Escape fall through to keymap dispatch
            _ => None,
        }
    }

    fn focused_editor_ids(&self) -> Option<(EditorId, BufferId)> {
        use crate::rebase::RebasePause;
        let ws = self.active_workspace();

        // While a reword pause is active, the reword scratch editor is
        // the effective focus target for insert/motion routing. This
        // mirrors the override in `action_handlers::focused_editor_mut`.
        if let Some((editor_id, buffer_id)) = ws
            .rebase_active
            .as_ref()
            .and_then(|a| a.pause.as_ref())
            .and_then(|p| match p {
                RebasePause::Reword {
                    editor_id,
                    buffer_id,
                    ..
                } => Some((*editor_id, *buffer_id)),
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
                Some((chat.input_editor_id, chat.input_buffer_id))
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
                if self.focused_is_claude() {
                    // Fall through to keymap which dispatches ClaudeSubmit.
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
            KeyCode::Up => {
                action_handlers::dispatch(self, &stoat_action::MoveUp);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Down => {
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
                        id, name, input, ..
                    } => {
                        chat.messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            content: ChatMessageContent::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                            },
                        });
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
                    | AgentMessage::Hook(_) => {
                        // Phase 3 lands these variants structurally. UI
                        // integration (diff rendering, plan widget, usage
                        // meter, etc.) is a follow-up; for now the chat
                        // state model ignores them.
                    },
                }
            }
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
        let ws = &mut self.workspaces[self.active_workspace];

        ws.layout(self.size);

        let commits_mode = self.mode == "commits";
        let rebase_mode = self.mode == "rebase";
        let reword_mode = self.mode == "reword" || self.mode == "reword_insert";
        let conflict_mode = self.mode == "conflict";
        let overlay_pane = if (commits_mode && ws.commits.is_some())
            || (rebase_mode && ws.rebase.is_some())
            || ((reword_mode || conflict_mode) && ws.rebase_active.is_some())
        {
            Some(ws.panes.focus())
        } else {
            None
        };

        let workspace_name = ws
            .git_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("(unnamed)")
            .to_string();

        // Render split panes first so docks overlay on top.
        let split_focused = ws.panes.focus();
        for (id, pane) in ws.panes.split_panes() {
            let is_focused = matches!(ws.focus, FocusTarget::SplitPane(_)) && id == split_focused;
            if Some(id) == overlay_pane {
                continue;
            }
            render_pane(
                pane,
                is_focused,
                PaneCtx {
                    editors: &mut ws.editors,
                    buffers: &ws.buffers,
                    runs: &ws.runs,
                    chats: &ws.chats,
                },
                &workspace_name,
                &self.mode,
                self.render_tick,
                &self.theme,
                &mut buf,
            );
        }

        render_pane_dividers(&ws.panes.dividers(), &self.theme, &mut buf);

        if let Some(pane_id) = overlay_pane {
            let pane = ws.panes.pane(pane_id);
            let is_focused = matches!(ws.focus, FocusTarget::SplitPane(id) if id == pane_id);
            if commits_mode {
                if let Some(state) = ws.commits.as_mut() {
                    render_commits(
                        pane,
                        is_focused,
                        state,
                        &workspace_name,
                        &self.mode,
                        &self.theme,
                        &mut buf,
                    );
                }
            } else if rebase_mode {
                if let Some(state) = ws.rebase.as_ref() {
                    render_rebase(
                        pane,
                        is_focused,
                        state,
                        &workspace_name,
                        &self.mode,
                        &self.theme,
                        &mut buf,
                    );
                }
            } else if reword_mode {
                let reword_ctx = ws
                    .rebase_active
                    .as_ref()
                    .and_then(|a| a.pause.as_ref())
                    .and_then(|p| match p {
                        RebasePause::Reword {
                            cherry_picked_commit,
                            original_message,
                            editor_id,
                            ..
                        } => Some((
                            cherry_picked_commit.clone(),
                            original_message.clone(),
                            *editor_id,
                        )),
                        _ => None,
                    });
                if let Some((sha, orig, editor_id)) = reword_ctx {
                    if let Some(editor) = ws.editors.get_mut(editor_id) {
                        render_reword(
                            pane,
                            is_focused,
                            editor,
                            &sha,
                            &orig,
                            &self.mode,
                            &workspace_name,
                            &self.theme,
                            &mut buf,
                        );
                    }
                }
            } else if conflict_mode {
                if let Some(active) = ws.rebase_active.as_ref() {
                    render_conflict(
                        pane,
                        is_focused,
                        active,
                        &workspace_name,
                        &self.mode,
                        &self.theme,
                        &mut buf,
                    );
                }
            }
        }

        // Render dock panels over the panes.
        for (dock_id, dock) in &ws.docks {
            if matches!(dock.visibility, DockVisibility::Hidden) {
                continue;
            }
            let is_focused = matches!(ws.focus, FocusTarget::Dock(id) if id == dock_id);
            if matches!(dock.visibility, DockVisibility::Minimized) {
                render_dock_minimized(dock, is_focused, &self.theme, &mut buf);
            } else {
                render_dock_open(
                    dock,
                    is_focused,
                    &mut ws.editors,
                    &ws.buffers,
                    &ws.chats,
                    self.render_tick,
                    &self.theme,
                    &mut buf,
                );
            }
        }
        render_badges(
            &ws.badges,
            &self.badges,
            self.size,
            self.render_tick,
            &self.theme,
            &mut buf,
        );
        if let Some(run_id) = self.modal_run {
            if let Some(run_state) = ws.runs.get(run_id) {
                render_modal_run(run_state, &self.theme, self.size, &mut buf);
            }
        } else if let Some(help) = &self.help {
            render_help(help, &self.theme, self.size, &mut buf);
        } else if let Some(palette) = &self.command_palette {
            render_command_palette(palette, &self.theme, self.size, &mut buf);
        } else if let Some(picker) = &self.workspace_picker {
            render_workspace_picker(picker, &self.theme, self.size, &mut buf);
            let bindings = picker.hint_bindings();
            render_hints("picker", &bindings, None, &self.theme, self.size, &mut buf);
        } else if !PRIMARY_MODES.contains(&self.mode.as_str()) {
            let state = StoatKeymapState::new(&self.mode);
            let raw = self.keymap.active_bindings(&state);
            let bindings: Vec<_> = raw
                .iter()
                .map(|(key, actions)| {
                    let desc = actions.first().map(action_display_desc).unwrap_or_default();
                    (key.as_str(), desc)
                })
                .collect();
            let footer = if self.mode == "review" {
                ws.review.as_ref().map(|session| {
                    let p = session.progress();
                    let complete = session.is_complete();
                    let text = if complete {
                        format!("all {} reviewed", p.total)
                    } else {
                        let current = p.current_index.unwrap_or(0);
                        format!(
                            "{}/{} · {} staged · {} unstaged · {} pending",
                            current, p.total, p.staged, p.unstaged, p.pending
                        )
                    };
                    let style = if complete {
                        self.theme.get(crate::theme::scope::UI_BADGE_COMPLETE)
                    } else {
                        self.theme.get(crate::theme::scope::UI_TEXT)
                    };
                    HintsFooter { text, style }
                })
            } else {
                None
            };
            render_hints(
                &self.mode,
                &bindings,
                footer.as_ref(),
                &self.theme,
                self.size,
                &mut buf,
            );
        }
        buf
    }

    fn dispatch_palette_key(&mut self, key: KeyEvent) -> UpdateEffect {
        let outcome = match self.command_palette.as_mut() {
            Some(palette) => palette.handle_key(key),
            None => return UpdateEffect::None,
        };
        match outcome {
            PaletteOutcome::None => UpdateEffect::Redraw,
            PaletteOutcome::Close => {
                self.command_palette = None;
                UpdateEffect::Redraw
            },
            PaletteOutcome::Dispatch(entry, params) => {
                self.command_palette = None;
                match (entry.create)(&params) {
                    Ok(action) => action_handlers::dispatch(self, &*action),
                    Err(e) => {
                        tracing::warn!("palette dispatch `{}`: {e}", entry.def.name());
                        UpdateEffect::Redraw
                    },
                }
            },
        }
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

    fn dispatch_help_key(&mut self, key: KeyEvent) -> UpdateEffect {
        let outcome = match self.help.as_mut() {
            Some(help) => help.handle_key(key),
            None => return UpdateEffect::None,
        };
        match outcome {
            HelpOutcome::None => UpdateEffect::Redraw,
            HelpOutcome::Close => {
                self.help = None;
                UpdateEffect::Redraw
            },
            HelpOutcome::Dispatch(entry, params) => {
                self.help = None;
                match (entry.create)(&params) {
                    Ok(action) => action_handlers::dispatch(self, &*action),
                    Err(e) => {
                        tracing::warn!("help dispatch `{}`: {e}", entry.def.name());
                        UpdateEffect::Redraw
                    },
                }
            },
        }
    }
}

struct StoatKeymapState {
    mode_value: StateValue,
}

impl StoatKeymapState {
    fn new(mode: &str) -> Self {
        Self {
            mode_value: StateValue::String(mode.into()),
        }
    }
}

impl KeymapState for StoatKeymapState {
    fn get(&self, field: &str) -> Option<&StateValue> {
        match field {
            "mode" => Some(&self.mode_value),
            _ => None,
        }
    }
}

/// Collapse Shift+letter events onto the bare uppercase form so keymap bindings
/// written as `A` or `S-a` both match what terminals emit.
///
/// Default crossterm without the kitty keyboard protocol reports Shift+a as
/// `(Char('A'), SHIFT)`, but a binding written as `A` compiles to
/// `(Char('A'), NONE)`, and modifier comparison is strict. Normalizing the
/// event up-front keeps bindings terminal-agnostic.
fn normalize_shift_letter(key: KeyEvent) -> KeyEvent {
    if !key.modifiers.contains(KeyModifiers::SHIFT) {
        return key;
    }
    let KeyCode::Char(ch) = key.code else {
        return key;
    };
    if !ch.is_ascii_alphabetic() {
        return key;
    }
    let mut modifiers = key.modifiers;
    modifiers.remove(KeyModifiers::SHIFT);
    KeyEvent::new(KeyCode::Char(ch.to_ascii_uppercase()), modifiers)
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

    // Parse succeeded; from here on we consume the prior state.
    let prev_injection_trees = prior
        .take()
        .map(|prev| prev.injection_trees)
        .unwrap_or_default();

    let extracted =
        language::extract_highlights_rope_with_cache(lang, &tree, &new_rope, prev_injection_trees);
    // Theme-driven path: span.id is set to the theme key index by
    // collect_highlights_into via language.highlight_map(). Spans
    // whose id is DEFAULT (capture not in the active theme) are
    // skipped because they have no rendered style.
    let tokens: Arc<[SemanticTokenHighlight]> = extracted
        .spans
        .into_iter()
        .filter_map(|sp| {
            let style_id = styles.id_for_highlight(sp.id)?;
            Some(SemanticTokenHighlight {
                // Insertions at the start of a token attach to the
                // previous span, not this one; insertions at the end
                // attach to the next span. Keeps a typed character
                // from silently extending a keyword or string into
                // neighboring text.
                range: snapshot.anchor_at(sp.byte_range.start, Bias::Right)
                    ..snapshot.anchor_at(sp.byte_range.end, Bias::Left),
                style: style_id,
            })
        })
        .collect();

    // Drive the multi-layer SyntaxMap alongside the legacy
    // SyntaxState. We don't have an interpolation pass on the host
    // side yet (it would need anchored byte offsets), so each parse
    // produces a fresh SyntaxMap from scratch; the prior_syntax_map
    // is consumed but only its captured tree is reused via
    // SyntaxMap::reparse's internal `prior_injections` snapshot.
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

pub(crate) fn arg_as_str(arg: &ResolvedArg) -> Option<String> {
    match &arg.value {
        stoat_config::Value::String(s) => Some(s.clone()),
        stoat_config::Value::Ident(s) => Some(s.clone()),
        _ => None,
    }
}

fn arg_to_param_value(arg: &ResolvedArg) -> Option<stoat_action::ParamValue> {
    match &arg.value {
        stoat_config::Value::String(s) => Some(stoat_action::ParamValue::String(s.clone())),
        stoat_config::Value::Ident(s) => Some(stoat_action::ParamValue::String(s.clone())),
        stoat_config::Value::Number(n) => Some(stoat_action::ParamValue::Number(*n)),
        stoat_config::Value::Bool(b) => Some(stoat_action::ParamValue::Bool(*b)),
        _ => None,
    }
}

const PRIMARY_MODES: &[&str] = &[
    "normal",
    "insert",
    "run",
    "commits",
    "rebase",
    "reword",
    "reword_insert",
    "conflict",
];

fn action_display_desc(action: &ResolvedAction) -> String {
    if action.name == "SetMode" {
        let target = action.args.first().and_then(arg_as_str).unwrap_or_default();
        return format!("{target} mode");
    }
    stoat_action::registry::lookup(&action.name)
        .map(|e| e.def.short_desc().to_string())
        .unwrap_or_else(|| action.name.clone())
}

fn resolve_action(name: &str, args: &[ResolvedArg]) -> Option<Box<dyn Action>> {
    let entry = stoat_action::registry::lookup(name)?;
    let mut params = Vec::with_capacity(args.len());
    for arg in args {
        match arg_to_param_value(arg) {
            Some(value) => params.push(value),
            None => {
                tracing::warn!("action `{name}`: cannot convert arg {:?}", arg.value);
                return None;
            },
        }
    }
    match (entry.create)(&params) {
        Ok(action) => Some(action),
        Err(e) => {
            tracing::warn!("action `{name}`: {e}");
            None
        },
    }
}

fn render_hints(
    mode: &str,
    bindings: &[(&str, String)],
    footer: Option<&HintsFooter>,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    if bindings.is_empty() || area.width < 10 || area.height < 4 {
        return;
    }

    let key_width = bindings.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let action_width = bindings.iter().map(|(_, a)| a.len()).max().unwrap_or(0);
    let gap = 3;
    let bindings_width = key_width + gap + action_width;
    let border_pad = 2;
    let title_width = mode.len() + 4;
    let footer_width = footer.map(|f| f.text.len()).unwrap_or(0);
    let content_width = bindings_width.max(title_width).max(footer_width);
    let extra_rows = footer.map(|_| 2).unwrap_or(0); // separator + footer line
    let box_width = (content_width + border_pad) as u16;
    let box_height = (bindings.len() + border_pad + extra_rows) as u16;

    if box_width > area.width || box_height > area.height {
        return;
    }

    let x = area.x + area.width.saturating_sub(box_width);
    let y = area.y + area.height.saturating_sub(box_height);
    let help_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HINTS);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(format!(" {mode} "))
        .title_style(modal_style);
    let inner = block.inner(help_area);
    block.render(help_area, buf);

    let key_style = theme.get(crate::theme::scope::UI_KEY_LABEL);
    let action_style = theme.get(crate::theme::scope::UI_TEXT);

    for (i, (key, action)) in bindings.iter().enumerate() {
        let row = inner.y + i as u16;
        if row >= inner.y + inner.height {
            break;
        }
        let padded_key = format!("{key:>width$}", width = key_width);
        let line = format!("{padded_key}   {action}");

        for (j, ch) in line.chars().enumerate() {
            let col = inner.x + j as u16;
            if col >= inner.x + inner.width {
                break;
            }
            let style = if j < key_width {
                key_style
            } else {
                action_style
            };
            buf[(col, row)].set_char(ch).set_style(style);
        }
    }

    if let Some(footer) = footer {
        let sep_row = inner.y + bindings.len() as u16;
        let text_row = sep_row + 1;
        if sep_row < inner.y + inner.height {
            let sep_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
            for col_offset in 0..inner.width {
                let col = inner.x + col_offset;
                buf[(col, sep_row)].set_char('─').set_style(sep_style);
            }
        }
        if text_row < inner.y + inner.height {
            for (j, ch) in footer.text.chars().enumerate() {
                let col = inner.x + j as u16;
                if col >= inner.x + inner.width {
                    break;
                }
                buf[(col, text_row)].set_char(ch).set_style(footer.style);
            }
        }
    }
}

struct HintsFooter {
    text: String,
    style: Style,
}

fn render_workspace_picker(
    picker: &WorkspacePicker,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.width < 60 || area.height < 8 {
        return;
    }

    let entries = picker.entries();
    if entries.is_empty() {
        return;
    }
    let max_entries = 10u16;
    let entry_rows = (entries.len() as u16).min(max_entries);

    let box_width = 90u16.min(area.width.saturating_sub(4));
    if box_width < 60 {
        return;
    }
    let box_height = 3 + entry_rows;
    if box_height > area.height {
        return;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let picker_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(" workspaces ")
        .title_style(modal_style);
    let inner = block.inner(picker_area);
    block.render(picker_area, buf);

    const NAME_W: u16 = 12;
    const BUF_W: u16 = 5;
    const CHAT_W: u16 = 6;
    const RUN_W: u16 = 5;
    const EDIT_W: u16 = 6;

    let edit_col_x = inner.x + inner.width.saturating_sub(1 + EDIT_W);
    let run_col_x = edit_col_x.saturating_sub(RUN_W);
    let chat_col_x = run_col_x.saturating_sub(CHAT_W);
    let buf_col_x = chat_col_x.saturating_sub(BUF_W);
    let marker_x = inner.x + 1;
    let name_x = marker_x + 2;
    let path_x = name_x + NAME_W + 2;
    let path_w = buf_col_x.saturating_sub(2).saturating_sub(path_x);

    let right_pad = |label: &str, width: u16| format!("{:>w$}", label, w = width as usize);

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let current_style = theme.get(crate::theme::scope::UI_PROMPT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let header_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let header_row = inner.y;
    write_str(buf, name_x, header_row, "name", header_style);
    write_str(buf, path_x, header_row, "path", header_style);
    write_str(
        buf,
        buf_col_x,
        header_row,
        &right_pad("buf", BUF_W),
        header_style,
    );
    write_str(
        buf,
        chat_col_x,
        header_row,
        &right_pad("chat", CHAT_W),
        header_style,
    );
    write_str(
        buf,
        run_col_x,
        header_row,
        &right_pad("run", RUN_W),
        header_style,
    );
    write_str(
        buf,
        edit_col_x,
        header_row,
        &right_pad("edit", EDIT_W),
        header_style,
    );

    let entries_top = inner.y + 1;
    let selected = picker.selected();

    for (i, entry) in entries.iter().take(max_entries as usize).enumerate() {
        let row = entries_top + i as u16;
        let is_selected = i == selected;
        let base_style = if is_selected {
            selected_style
        } else if entry.is_current {
            current_style
        } else {
            row_style
        };

        for col in inner.x..inner.x + inner.width {
            buf[(col, row)].set_char(' ').set_style(base_style);
        }

        let marker = if entry.is_current { "*" } else { " " };
        write_str(buf, marker_x, row, marker, base_style);
        let name: String = entry.basename.chars().take(NAME_W as usize).collect();
        write_str(buf, name_x, row, &name, base_style);
        let path = entry.git_root.display().to_string();
        let path_trimmed: String = path.chars().take(path_w as usize).collect();
        write_str(buf, path_x, row, &path_trimmed, base_style);
        write_str(
            buf,
            buf_col_x,
            row,
            &right_pad(&entry.buffer_count.to_string(), BUF_W),
            base_style,
        );
        write_str(
            buf,
            chat_col_x,
            row,
            &right_pad(&entry.chat_count.to_string(), CHAT_W),
            base_style,
        );
        write_str(
            buf,
            run_col_x,
            row,
            &right_pad(&entry.run_count.to_string(), RUN_W),
            base_style,
        );
        write_str(
            buf,
            edit_col_x,
            row,
            &right_pad(&entry.editor_count.to_string(), EDIT_W),
            base_style,
        );
    }
}

fn render_command_palette(
    palette: &CommandPalette,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    match palette.phase() {
        crate::command_palette::PalettePhase::Filter {
            input,
            filtered,
            selected,
        } => render_palette_filter(input, filtered, *selected, theme, area, buf),
        crate::command_palette::PalettePhase::CollectArgs {
            entry,
            collected,
            current,
            input,
            error,
        } => render_palette_collect_args(
            entry,
            collected,
            *current,
            input,
            error.as_deref(),
            theme,
            area,
            buf,
        ),
    }
}

fn render_palette_filter(
    input: &str,
    filtered: &[&'static stoat_action::registry::RegistryEntry],
    selected: usize,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.width < 30 || area.height < 10 {
        return;
    }

    let box_width = 80u16.min(area.width.saturating_sub(4));
    if box_width < 20 {
        return;
    }
    let inner_width = box_width.saturating_sub(2) as usize;
    let max_rows = 10u16;
    let row_count = (filtered.len() as u16).min(max_rows).max(1);

    let doc_lines: Vec<String> = filtered
        .get(selected)
        .map(|e| wrap_text(e.def.long_desc(), inner_width))
        .unwrap_or_default();
    let doc_height = doc_lines.len() as u16;
    let doc_section: u16 = if doc_height == 0 { 0 } else { doc_height + 1 };

    let box_height = 1 + 1 + 1 + row_count + doc_section + 1;
    if box_height > area.height {
        return;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let palette_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(" command palette ")
        .title_style(modal_style);
    let inner = block.inner(palette_area);
    block.render(palette_area, buf);

    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let input_style = theme.get(crate::theme::scope::UI_TEXT);
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let desc_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let cursor_style = theme.get(crate::theme::scope::UI_CURSOR_INPUT);

    let input_row = inner.y;
    write_str(buf, inner.x, input_row, ":", prompt_style);
    write_str(buf, inner.x + 2, input_row, input, input_style);
    let cursor_col = inner.x + 2 + input.chars().count() as u16;
    if cursor_col < inner.x + inner.width {
        buf[(cursor_col, input_row)]
            .set_char(' ')
            .set_style(cursor_style);
    }

    let separator_row = inner.y + 1;
    let separator_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    for col in inner.x..inner.x + inner.width {
        buf[(col, separator_row)]
            .set_char('─')
            .set_style(separator_style);
    }

    let list_top = inner.y + 2;
    let name_col_width: usize = filtered
        .iter()
        .take(max_rows as usize)
        .map(|e| e.def.name().len())
        .max()
        .unwrap_or(0);

    for (i, entry) in filtered.iter().take(max_rows as usize).enumerate() {
        let row = list_top + i as u16;
        let is_selected = i == selected;
        let style = if is_selected {
            selected_style
        } else {
            row_style
        };

        for col in inner.x..inner.x + inner.width {
            buf[(col, row)].set_char(' ').set_style(style);
        }

        let name = entry.def.name();
        write_str(buf, inner.x + 1, row, name, style);
        let desc_col = inner.x + 1 + name_col_width as u16 + 2;
        if desc_col < inner.x + inner.width {
            let desc_style = if is_selected { style } else { desc_style };
            write_str(buf, desc_col, row, entry.def.short_desc(), desc_style);
        }
    }

    if doc_section > 0 {
        let doc_separator_row = list_top + row_count;
        for col in inner.x..inner.x + inner.width {
            buf[(col, doc_separator_row)]
                .set_char('─')
                .set_style(separator_style);
        }
        let doc_top = doc_separator_row + 1;
        let doc_style = theme.get(crate::theme::scope::UI_TEXT_DIM);
        for (i, line) in doc_lines.iter().enumerate() {
            write_str(buf, inner.x, doc_top + i as u16, line, doc_style);
        }
    }
}

fn render_palette_collect_args(
    entry: &'static stoat_action::registry::RegistryEntry,
    collected: &[stoat_action::ParamValue],
    current: usize,
    input: &str,
    error: Option<&str>,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.width < 30 || area.height < 10 {
        return;
    }

    let box_width = 80u16.min(area.width.saturating_sub(4));
    if box_width < 20 {
        return;
    }
    let inner_width = box_width.saturating_sub(2) as usize;

    let params = entry.def.params();
    let current_param = &params[current];
    let body_lines = wrap_text(current_param.description, inner_width);
    let body_height = body_lines.len() as u16;
    // header line + body lines
    let doc_height = 1 + body_height;

    let collected_lines = collected.len() as u16;
    let error_lines: u16 = if error.is_some() { 1 } else { 0 };
    // chrome: top + collected + input + (error?) + separator + doc + bottom
    let box_height = 1 + collected_lines + 1 + error_lines + 1 + doc_height + 1;
    if box_height > area.height {
        return;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let palette_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    let title = format!(" {} ", entry.def.name());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(title)
        .title_style(modal_style);
    let inner = block.inner(palette_area);
    block.render(palette_area, buf);

    let label_style = theme.get(crate::theme::scope::UI_PROMPT);
    let value_style = theme.get(crate::theme::scope::UI_TEXT);
    let cursor_style = theme.get(crate::theme::scope::UI_CURSOR_INPUT);
    let error_style = theme.get(crate::theme::scope::UI_ERROR);
    let muted_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let mut row = inner.y;

    for (i, value) in collected.iter().enumerate() {
        let label = format!("{}: ", params[i].name);
        write_str(buf, inner.x, row, &label, muted_style);
        let value_col = inner.x + label.chars().count() as u16;
        write_str(buf, value_col, row, &format_param_value(value), muted_style);
        row += 1;
    }

    let label = format!("{}: ", current_param.name);
    write_str(buf, inner.x, row, &label, label_style);
    let value_col = inner.x + label.chars().count() as u16;
    write_str(buf, value_col, row, input, value_style);
    let cursor_col = value_col + input.chars().count() as u16;
    if cursor_col < inner.x + inner.width {
        buf[(cursor_col, row)].set_char(' ').set_style(cursor_style);
    }
    row += 1;

    if let Some(msg) = error {
        write_str(buf, inner.x, row, msg, error_style);
        row += 1;
    }

    let separator_row = row;
    for col in inner.x..inner.x + inner.width {
        buf[(col, separator_row)]
            .set_char('─')
            .set_style(muted_style);
    }
    let doc_top = separator_row + 1;

    let header = format!(
        "{} ({}{})",
        current_param.name,
        current_param.kind,
        if current_param.required {
            ", required"
        } else {
            ""
        },
    );
    write_str(buf, inner.x, doc_top, &header, muted_style);

    let body_top = doc_top + 1;
    let body_body_style = theme.get(crate::theme::scope::UI_TEXT_DIM);
    for (i, line) in body_lines.iter().enumerate() {
        write_str(buf, inner.x, body_top + i as u16, line, body_body_style);
    }
}

fn format_param_value(v: &stoat_action::ParamValue) -> String {
    match v {
        stoat_action::ParamValue::String(s) => s.clone(),
        stoat_action::ParamValue::Number(n) => n.to_string(),
        stoat_action::ParamValue::Bool(b) => b.to_string(),
    }
}

fn render_help(help: &Help, theme: &crate::theme::Theme, area: Rect, buf: &mut Buffer) {
    use crate::help::{HelpInput, HelpScope};

    if area.width < 40 || area.height < 12 {
        return;
    }

    let box_width = 120u16.min(area.width.saturating_sub(4));
    let box_height = 36u16.min(area.height.saturating_sub(4));
    if box_width < 40 || box_height < 12 {
        return;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let help_area = Rect::new(x, y, box_width, box_height);

    let title = match help.scope() {
        HelpScope::Active => format!(" help: active ({}) ", help.snapshot_mode()),
        HelpScope::All => " help: all actions ".to_string(),
    };
    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HELP);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(title)
        .title_style(modal_style);
    let inner = block.inner(help_area);
    block.render(help_area, buf);

    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let input_style = theme.get(crate::theme::scope::UI_TEXT);
    let cursor_style = theme.get(crate::theme::scope::UI_CURSOR_INPUT);
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let key_style = theme.get(crate::theme::scope::UI_KEY_LABEL);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let heading = theme.get(crate::theme::scope::UI_HEADING);

    let search_row = inner.y;
    let prompt = match help.input_mode() {
        HelpInput::Insert => "> ",
        HelpInput::Normal => ": ",
    };
    write_str(buf, inner.x, search_row, prompt, prompt_style);
    write_str(buf, inner.x + 2, search_row, help.input(), input_style);
    if matches!(help.input_mode(), HelpInput::Insert) {
        let cursor_col = inner.x + 2 + help.input_cursor_column() as u16;
        if cursor_col < inner.x + inner.width {
            buf[(cursor_col, search_row)]
                .set_char(' ')
                .set_style(cursor_style);
        }
    }
    let mode_hint = match help.input_mode() {
        HelpInput::Insert => "[insert]",
        HelpInput::Normal => "[normal]",
    };
    let hint_col = inner.x + inner.width.saturating_sub(mode_hint.chars().count() as u16);
    write_str(buf, hint_col, search_row, mode_hint, muted);

    let sep_top = search_row + 1;
    for col in inner.x..inner.x + inner.width {
        buf[(col, sep_top)].set_char('─').set_style(muted);
    }

    let footer_row = inner.y + inner.height.saturating_sub(1);
    let sep_bottom = footer_row.saturating_sub(1);
    for col in inner.x..inner.x + inner.width {
        buf[(col, sep_bottom)].set_char('─').set_style(muted);
    }
    let footer_text = match help.input_mode() {
        HelpInput::Insert => "Enter dispatch | Esc normal | Shift-Tab scope | C-u/d scroll | Bksp",
        HelpInput::Normal => "i insert | j/k select | g/G top/end | C-u/d scroll | Esc close",
    };
    write_str(buf, inner.x, footer_row, footer_text, muted);

    let body_top = sep_top + 1;
    let body_height = sep_bottom.saturating_sub(body_top);
    if body_height == 0 {
        return;
    }

    let body_width = inner.width;
    let list_width = (body_width * 42 / 100).max(20);
    let detail_width = body_width.saturating_sub(list_width + 1);
    let list_rect = Rect::new(inner.x, body_top, list_width, body_height);
    let detail_rect = Rect::new(
        inner.x + list_width + 1,
        body_top,
        detail_width,
        body_height,
    );

    for row in list_rect.y..list_rect.y + list_rect.height {
        buf[(list_rect.x + list_rect.width, row)]
            .set_char('│')
            .set_style(muted);
    }

    render_help_list(
        help,
        list_rect,
        buf,
        row_style,
        selected_style,
        key_style,
        muted,
    );
    render_help_detail(help, detail_rect, buf, heading, row_style, muted, key_style);
}

fn render_help_list(
    help: &Help,
    area: Rect,
    buf: &mut Buffer,
    row_style: Style,
    selected_style: Style,
    key_style: Style,
    muted: Style,
) {
    let filtered = help.filtered();
    let entries = help.entries();
    let selected = help.selected();
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let scroll = selected.saturating_sub(rows.saturating_sub(1));

    let key_col_width: usize = filtered
        .iter()
        .skip(scroll)
        .take(rows)
        .map(|&i| {
            entries[i]
                .key_label
                .as_deref()
                .map(|s| s.chars().count())
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
        .min(10);

    let row_end = area.x + area.width;
    for (row_idx, &entry_idx) in filtered.iter().skip(scroll).take(rows).enumerate() {
        let row = area.y + row_idx as u16;
        let entry = &entries[entry_idx];
        let is_selected = entry_idx == *filtered.get(selected).unwrap_or(&usize::MAX);
        let base = if is_selected {
            selected_style
        } else {
            row_style
        };

        for col in area.x..row_end {
            buf[(col, row)].set_char(' ').set_style(base);
        }

        let key_text = entry.key_label.as_deref().unwrap_or("");
        let padded = format!("{key_text:>width$}", width = key_col_width);
        let key_display_style = if is_selected { base } else { key_style };
        write_str_clipped(buf, area.x + 1, row, &padded, key_display_style, row_end);
        let name_col = area.x + 1 + key_col_width as u16 + 2;
        if name_col < row_end {
            write_str_clipped(buf, name_col, row, entry.def.name(), base, row_end);
        }
        let name_w = entry.def.name().chars().count() as u16;
        let desc_col = name_col + name_w + 2;
        if desc_col < row_end {
            let desc_style = if is_selected { base } else { muted };
            write_str_clipped(
                buf,
                desc_col,
                row,
                entry.def.short_desc(),
                desc_style,
                row_end,
            );
        }
    }
}

fn write_str_clipped(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style, end_x: u16) {
    for (i, ch) in s.chars().enumerate() {
        let col = x + i as u16;
        if col >= end_x || col >= buf.area.x + buf.area.width {
            break;
        }
        if y >= buf.area.y + buf.area.height {
            break;
        }
        buf[(col, y)].set_char(ch).set_style(style);
    }
}

fn render_help_detail(
    help: &Help,
    area: Rect,
    buf: &mut Buffer,
    heading: Style,
    body_style: Style,
    muted: Style,
    key_style: Style,
) {
    let Some(entry) = help.selected_entry() else {
        return;
    };
    let width = area.width.saturating_sub(1) as usize;
    if width == 0 || area.height == 0 {
        return;
    }

    let mut lines: Vec<(String, Style)> = Vec::new();
    lines.push((entry.def.name().to_string(), heading));
    if let Some(label) = entry.key_label.as_deref() {
        lines.push((format!("bound: {label}"), key_style));
    } else {
        lines.push(("(unbound)".to_string(), muted));
    }
    lines.push((entry.def.short_desc().to_string(), body_style));
    lines.push((String::new(), body_style));

    for wrapped in wrap_text(entry.def.long_desc(), width) {
        lines.push((wrapped, body_style));
    }

    let params = entry.def.params();
    if !params.is_empty() {
        lines.push((String::new(), body_style));
        lines.push(("Parameters:".to_string(), heading));
        for p in params {
            let required = if p.required { "*" } else { "" };
            let head = format!("  {}{}: {}: {}", p.name, required, p.kind, p.description);
            for wrapped in wrap_text(&head, width) {
                lines.push((wrapped, body_style));
            }
        }
    }

    lines.push((String::new(), body_style));
    lines.push(("Example:".to_string(), heading));
    let example = format_example(entry);
    lines.push((format!("  {example}"), muted));

    let scroll = help.detail_scroll() as usize;
    let rows = area.height as usize;
    let end_x = area.x + area.width;
    for (row_idx, (text, style)) in lines.iter().skip(scroll).take(rows).enumerate() {
        let row = area.y + row_idx as u16;
        write_str_clipped(buf, area.x + 1, row, text, *style, end_x);
    }
}

fn format_example(entry: &crate::help::HelpEntry) -> String {
    let name = entry.def.name();
    let params = entry.def.params();
    if params.is_empty() {
        return format!("{name}()");
    }
    if !entry.bound_args.is_empty() {
        let args: Vec<String> = entry
            .bound_args
            .iter()
            .filter_map(crate::help::format_arg)
            .collect();
        return format!("{name}({})", args.join(", "));
    }
    let placeholders: Vec<String> = params.iter().map(|p| p.name.to_string()).collect();
    format!("{name}({})", placeholders.join(", "))
}

fn write_cell(buf: &mut Buffer, x: u16, y: u16, ch: char, style: Style) {
    if x < buf.area.x + buf.area.width && y < buf.area.y + buf.area.height {
        buf[(x, y)].set_char(ch).set_style(style);
    }
}

fn write_str(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style) {
    for (i, ch) in s.chars().enumerate() {
        let col = x + i as u16;
        if col >= buf.area.x + buf.area.width {
            break;
        }
        if y >= buf.area.y + buf.area.height {
            break;
        }
        buf[(col, y)].set_char(ch).set_style(style);
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let trimmed_start = text.trim_start();
    if trimmed_start.is_empty() {
        return Vec::new();
    }
    let indent_byte_len = text.len() - trimmed_start.len();
    let indent = text[..indent_byte_len].to_string();
    let indent_w = indent.chars().count();
    if indent_w >= width {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current = indent.clone();
    let mut current_w = indent_w;
    for word in trimmed_start.split_whitespace() {
        let needs_space = current_w > indent_w;
        let word_w = word.chars().count();
        let add_w = word_w + usize::from(needs_space);
        if current_w + add_w <= width {
            if needs_space {
                current.push(' ');
            }
            current.push_str(word);
            current_w += add_w;
        } else {
            lines.push(std::mem::take(&mut current));
            current = indent.clone();
            current.push_str(word);
            current_w = indent_w + word_w;
        }
    }
    if current_w > indent_w {
        lines.push(current);
    }
    lines
}

fn badge_size(badge: &Badge) -> (u16, u16) {
    let label_w = badge.label.chars().count() as u16;
    (label_w + 2, 3)
}

fn border_char_at(col: u16, row: u16, w: u16, h: u16) -> char {
    let top = row == 0;
    let bot = row == h - 1;
    let left = col == 0;
    let right = col == w - 1;
    match (top, bot, left, right) {
        (true, _, true, _) => '\u{256d}',
        (true, _, _, true) => '\u{256e}',
        (_, true, true, _) => '\u{2570}',
        (_, true, _, true) => '\u{256f}',
        (true, _, _, _) | (_, true, _, _) => '\u{2500}',
        _ => '\u{2502}',
    }
}

/// Braille character that visually traces the box-drawing line at this
/// border position. Dot placement matches the line direction:
///
/// ```text
///   braille grid        used for
///   1 4                 ╭ → ⣰  (bottom-right quadrant: right then down)
///   2 5                 ╮ → ⣆  (bottom-left quadrant: left then down)
///   3 6                 ╰ → ⠙  (top-right quadrant: right then up)
///   7 8                 ╯ → ⠋  (top-left quadrant: left then up)
///                       ─ top  → ⠉  (dots 1,4)
///                       ─ bot  → ⣀  (dots 7,8)
///                       │ left → ⡇  (dots 1,2,3,7)
///                       │ right→ ⢸  (dots 4,5,6,8)
/// ```
fn spinner_char_at(col: u16, row: u16, w: u16, h: u16) -> char {
    let top = row == 0;
    let bot = row == h - 1;
    let left = col == 0;
    let right = col == w - 1;
    match (top, bot, left, right) {
        (true, _, true, _) => '\u{28f0}', // ⣰
        (true, _, _, true) => '\u{28c6}', // ⣆
        (_, true, true, _) => '\u{2819}', // ⠙
        (_, true, _, true) => '\u{280b}', // ⠋
        (true, _, _, _) => '\u{2809}',    // ⠉
        (_, true, _, _) => '\u{28c0}',    // ⣀
        (_, _, true, _) => '\u{2847}',    // ⡇
        _ => '\u{28b8}',                  // ⢸
    }
}

fn perimeter_position(index: usize, w: u16, h: u16) -> (u16, u16) {
    let w = w as usize;
    let h = h as usize;
    let top = w;
    let right = top + h.saturating_sub(2);
    let bottom = right + w;
    if index < top {
        (index as u16, 0)
    } else if index < right {
        ((w - 1) as u16, (index - top + 1) as u16)
    } else if index < bottom {
        ((w - 1 - (index - right)) as u16, (h - 1) as u16)
    } else {
        (0, (h - 1 - (index - bottom + 1)) as u16)
    }
}

fn render_single_badge(
    badge: &Badge,
    x: u16,
    y: u16,
    render_tick: u64,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let (w, h) = badge_size(badge);
    let border_style = badge_border_style(badge.state, theme);

    let perimeter_len = 2 * (w as usize) + 2 * (h as usize) - 4;
    let spinner_pos = if badge.state == BadgeState::Active {
        Some(render_tick as usize % perimeter_len)
    } else {
        None
    };

    for col in x..x + w {
        write_cell(buf, col, y, border_char_at(col - x, 0, w, h), border_style);
    }
    for col in x..x + w {
        write_cell(
            buf,
            col,
            y + h - 1,
            border_char_at(col - x, h - 1, w, h),
            border_style,
        );
    }
    for row in y + 1..y + h - 1 {
        write_cell(buf, x, row, border_char_at(0, row - y, w, h), border_style);
        write_cell(
            buf,
            x + w - 1,
            row,
            border_char_at(w - 1, row - y, w, h),
            border_style,
        );
    }

    if let Some(pos) = spinner_pos {
        let (sc, sr) = perimeter_position(pos, w, h);
        let ch = spinner_char_at(sc, sr, w, h);
        write_cell(buf, x + sc, y + sr, ch, border_style);
    }

    let content_style = theme.get(crate::theme::scope::UI_TEXT);
    write_str(buf, x + 1, y + 1, &badge.label, content_style);
}

fn render_badges(
    workspace: &BadgeTray,
    global: &BadgeTray,
    area: Rect,
    render_tick: u64,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    if workspace.is_empty() && global.is_empty() {
        return;
    }

    for anchor in Anchor::ALL {
        let tray = workspace.tray(anchor);
        let visible: Vec<&Badge> = workspace
            .at_anchor(anchor)
            .chain(global.at_anchor(anchor))
            .map(|(_, b)| b)
            .take(tray.max_visible as usize)
            .collect();
        if visible.is_empty() {
            continue;
        }

        let sizes: Vec<(u16, u16)> = visible.iter().map(|b| badge_size(b)).collect();
        let (origin_x, origin_y) = anchor_origin(anchor, area);
        let grows_left = matches!(
            anchor,
            Anchor::TopRight | Anchor::MidRight | Anchor::BottomRight
        );
        let grows_up = matches!(
            anchor,
            Anchor::BottomLeft | Anchor::BottomCenter | Anchor::BottomRight
        );
        let centered = matches!(anchor, Anchor::TopCenter | Anchor::BottomCenter);

        let (mut cx, mut cy) = (origin_x, origin_y);

        if centered && tray.stack == StackDirection::Horizontal {
            let total_w: u16 =
                sizes.iter().map(|(w, _)| w).sum::<u16>() + sizes.len().saturating_sub(1) as u16;
            cx = origin_x.saturating_sub(total_w / 2);
        }

        for (i, badge) in visible.iter().enumerate() {
            let (bw, bh) = sizes[i];

            let draw_x = if grows_left {
                cx.saturating_sub(bw)
            } else if centered && tray.stack == StackDirection::Vertical {
                cx.saturating_sub(bw / 2)
            } else {
                cx
            };
            let draw_y = if grows_up {
                cy.saturating_sub(bh - 1)
            } else {
                cy
            };

            render_single_badge(badge, draw_x, draw_y, render_tick, theme, buf);

            match tray.stack {
                StackDirection::Horizontal => {
                    if grows_left {
                        cx = cx.saturating_sub(bw + 1);
                    } else {
                        cx += bw + 1;
                    }
                },
                StackDirection::Vertical => {
                    if grows_up {
                        cy = cy.saturating_sub(bh);
                    } else {
                        cy += bh;
                    }
                },
            }
        }
    }
}

fn anchor_origin(anchor: Anchor, area: Rect) -> (u16, u16) {
    let x = match anchor {
        Anchor::TopLeft | Anchor::MidLeft | Anchor::BottomLeft => area.x,
        Anchor::TopCenter | Anchor::BottomCenter => area.x + area.width / 2,
        Anchor::TopRight | Anchor::MidRight | Anchor::BottomRight => {
            (area.x + area.width).saturating_sub(1)
        },
    };
    let y = match anchor {
        Anchor::TopLeft | Anchor::TopCenter | Anchor::TopRight => area.y,
        Anchor::MidLeft | Anchor::MidRight => area.y + area.height / 2,
        Anchor::BottomLeft | Anchor::BottomCenter | Anchor::BottomRight => {
            area.y + area.height.saturating_sub(1)
        },
    };
    (x, y)
}

fn badge_border_style(state: BadgeState, theme: &crate::theme::Theme) -> Style {
    use crate::theme::scope;
    match state {
        BadgeState::Active => theme.get(scope::UI_BADGE_ACTIVE),
        BadgeState::Complete => theme.get(scope::UI_BADGE_COMPLETE),
        BadgeState::Error => theme.get(scope::UI_BADGE_ERROR),
    }
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

fn render_dock_minimized(
    dock: &DockPanel,
    is_focused: bool,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let area = dock.area;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = if is_focused {
        theme.get(crate::theme::scope::UI_BORDER_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_BORDER_INACTIVE)
    };
    for y in area.y..area.y + area.height {
        if let Some(cell) = buf.cell_mut((area.x, y)) {
            cell.set_char('│').set_style(style);
        }
    }
}

fn render_dock_open(
    dock: &DockPanel,
    is_focused: bool,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    chats: &HashMap<ClaudeSessionId, ClaudeChatState>,
    render_tick: u64,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let area = dock.area;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let border_style = if is_focused {
        theme.get(crate::theme::scope::UI_BORDER_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_BORDER_INACTIVE)
    };

    Clear.render(area, buf);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    block.render(area, buf);

    match &dock.view {
        View::Claude(session_id) => {
            if let Some(chat) = chats.get(session_id) {
                render_claude_pane(
                    chat,
                    editors,
                    buffers,
                    inner,
                    is_focused,
                    render_tick,
                    theme,
                    buf,
                );
            }
        },
        View::Editor(editor_id) => {
            if let Some(editor) = editors.get_mut(*editor_id) {
                render_editor(editor, inner, border_style, theme, buf, is_focused);
            }
        },
        _ => {},
    }
}

fn render_claude_pane(
    chat: &ClaudeChatState,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    area: Rect,
    is_focused: bool,
    render_tick: u64,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    use crate::{
        badge::THROBBER_FRAMES,
        claude_chat::{ChatMessageContent, ChatRole},
    };

    if area.height < 4 || area.width < 4 {
        return;
    }

    let input_lines = buffers
        .get(chat.input_buffer_id)
        .map(|b| {
            let guard = b.read().expect("poisoned");
            guard.snapshot.visible_text.max_point().row + 1
        })
        .unwrap_or(1);
    let max_input = (area.height / 3).max(1);
    let input_height = (input_lines as u16).clamp(1, max_input);
    let separator_y = area.y + area.height - input_height - 1;
    let msg_area = Rect::new(area.x, area.y, area.width, separator_y - area.y);
    let input_area = Rect::new(area.x, separator_y + 1, area.width, input_height);

    use crate::theme::scope as s;
    let sep_style = theme.get(s::CHAT_SEPARATOR);
    for x in area.x..area.x + area.width {
        write_cell(buf, x, separator_y, '-', sep_style);
    }

    let meta_style = theme.get(s::CHAT_META);
    let time_style = theme.get(s::CHAT_TIME);
    write_str(buf, msg_area.x, msg_area.y, "Claude", meta_style);

    let body_area = Rect::new(
        msg_area.x,
        msg_area.y + 1,
        msg_area.width,
        msg_area.height.saturating_sub(1),
    );
    if body_area.height == 0 {
        return;
    }

    let user_style = theme.get(s::CHAT_USER);
    let text_style = theme.get(s::CHAT_TEXT);
    let thinking_style = theme.get(s::CHAT_THINKING);
    let tool_header_style = theme.get(s::CHAT_TOOL_HEADER);
    let tool_body_style = theme.get(s::CHAT_TOOL_BODY);
    let error_style = theme.get(s::CHAT_ERROR);
    let turn_sep_style = theme.get(s::CHAT_SEPARATOR);
    let throbber_style = theme.get(s::CHAT_THROBBER);

    const TOOL_MARK: &str = "\u{23fa}";
    const TOOL_RESULT_ELBOW: &str = "\u{2514}\u{2500}";

    let result_map = build_tool_result_map(&chat.messages);
    let body_width = body_area.width as usize;
    let mut lines: Vec<(Style, String)> = Vec::new();

    let push_block = |lines: &mut Vec<(Style, String)>, block: Vec<(Style, String)>| {
        if block.is_empty() {
            return;
        }
        if !lines.is_empty() {
            lines.push((text_style, String::new()));
        }
        lines.extend(block);
    };

    let render_flowing =
        |t: &str, style: Style, width: usize, prefix: &str| -> Vec<(Style, String)> {
            let inner = width.saturating_sub(prefix.chars().count());
            let mut block = Vec::new();
            let push_line = |block: &mut Vec<(Style, String)>, body: &str| {
                if body.is_empty() {
                    block.push((style, prefix.trim_end().to_string()));
                } else {
                    block.push((style, format!("{prefix}{body}")));
                }
            };
            if t.is_empty() {
                if !prefix.is_empty() {
                    push_line(&mut block, "");
                }
                return block;
            }
            for raw_line in t.lines() {
                if raw_line.trim().is_empty() {
                    push_line(&mut block, "");
                } else {
                    for wrapped in wrap_text(raw_line, inner) {
                        push_line(&mut block, &wrapped);
                    }
                }
            }
            block
        };

    for msg in &chat.messages {
        match (&msg.role, &msg.content) {
            (ChatRole::User, ChatMessageContent::Text(t)) => {
                push_block(&mut lines, render_flowing(t, user_style, body_width, "> "));
            },
            (ChatRole::Assistant, ChatMessageContent::Text(t)) => {
                push_block(&mut lines, render_flowing(t, text_style, body_width, ""));
            },
            (ChatRole::Assistant, ChatMessageContent::Thinking { text }) => {
                let n = text.lines().count().max(1);
                push_block(
                    &mut lines,
                    vec![(thinking_style, format!("~ Thinking... ({n} lines)"))],
                );
            },
            (ChatRole::Assistant, ChatMessageContent::ToolUse { id, name, input }) => {
                let header = format_tool_header(name, input);
                let mut block = vec![(tool_header_style, format!("{TOOL_MARK} {header}"))];
                if let Some(content) = result_map.get(id.as_str()) {
                    let preview = format_tool_result_preview(content);
                    block.push((tool_body_style, format!("  {TOOL_RESULT_ELBOW} {preview}")));
                }
                push_block(&mut lines, block);
            },
            (ChatRole::Assistant, ChatMessageContent::ToolResult { .. }) => {},
            (ChatRole::Assistant, ChatMessageContent::Error(m)) => {
                push_block(&mut lines, vec![(error_style, format!("! {m}"))]);
            },
            (ChatRole::Assistant, ChatMessageContent::TurnComplete { duration_ms, .. }) => {
                push_block(
                    &mut lines,
                    vec![
                        (turn_sep_style, "-".repeat(body_width)),
                        (
                            time_style,
                            format!("  {:.1}s", *duration_ms as f64 / 1000.0),
                        ),
                    ],
                );
            },
            (ChatRole::User, _) => {},
        }
    }

    if let Some(partial) = &chat.streaming_text {
        push_block(
            &mut lines,
            render_flowing(partial, text_style, body_width, ""),
        );
    }

    if let Some(since) = chat.active_since {
        let frame = THROBBER_FRAMES[(render_tick as usize) % THROBBER_FRAMES.len()];
        let elapsed = since.elapsed().as_secs();
        let label = compute_throbber_label(&chat.messages, &result_map);
        push_block(
            &mut lines,
            vec![(throbber_style, format!("{frame} {label} ({elapsed}s)"))],
        );
    }

    let visible_lines = body_area.height as usize;
    let skip = lines
        .len()
        .saturating_sub(visible_lines + chat.scroll_offset);
    let take = visible_lines;
    let display: Vec<_> = lines.iter().skip(skip).take(take).collect();
    let start_row = body_area.y + body_area.height.saturating_sub(display.len() as u16);
    for (i, (style, text)) in display.iter().enumerate() {
        let y = start_row + i as u16;
        let max_w = body_area.width as usize;
        for (j, ch) in text.chars().take(max_w).enumerate() {
            let x = body_area.x + j as u16;
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(ch).set_style(*style);
            }
        }
    }

    if let Some(editor) = editors.get_mut(chat.input_editor_id) {
        let input_style = if is_focused {
            theme.get(crate::theme::scope::UI_TEXT)
        } else {
            theme.get(crate::theme::scope::UI_TEXT_MUTED)
        };
        render_editor(editor, input_area, input_style, theme, buf, is_focused);
    }
}

fn format_tool_header(name: &str, input_json: &str) -> String {
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(input_json);
    let Ok(value) = parsed else {
        return format!("{name}(...)");
    };
    let obj = value.as_object();

    match name {
        "Bash" => {
            if let Some(cmd) = obj.and_then(|o| o.get("command")).and_then(|v| v.as_str()) {
                return format!("Bash({})", truncate(cmd, 60));
            }
        },
        "Read" => {
            if let Some(p) = obj
                .and_then(|o| o.get("file_path"))
                .and_then(|v| v.as_str())
            {
                return format!("Read({})", short_path(p));
            }
        },
        "Edit" | "Write" | "NotebookEdit" => {
            if let Some(p) = obj
                .and_then(|o| o.get("file_path"))
                .and_then(|v| v.as_str())
            {
                return format!("{name}({})", short_path(p));
            }
        },
        "Grep" => {
            if let Some(p) = obj.and_then(|o| o.get("pattern")).and_then(|v| v.as_str()) {
                return format!("Grep({})", truncate(p, 60));
            }
        },
        "Glob" => {
            if let Some(p) = obj.and_then(|o| o.get("pattern")).and_then(|v| v.as_str()) {
                return format!("Glob({})", truncate(p, 60));
            }
        },
        _ => {},
    }

    if let Some(o) = obj {
        if let Some((k, v)) = o.iter().next() {
            let vs = match v {
                serde_json::Value::String(s) => truncate(s, 60),
                other => truncate(&other.to_string(), 60),
            };
            return format!("{name}({k}={vs})");
        }
    }
    format!("{name}(...)")
}

fn format_tool_result_preview(content: &str) -> String {
    let first = content.lines().next().unwrap_or("");
    let total_lines = content.lines().count();
    let preview = truncate(first, 80);
    if total_lines > 1 {
        format!("{preview} (+{} more lines)", total_lines - 1)
    } else {
        preview
    }
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        format!(
            "{}...",
            s.chars().take(max.saturating_sub(3)).collect::<String>()
        )
    }
}

fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    match parts.len() {
        0 => p.to_string(),
        1 => parts[0].to_string(),
        n => format!("{}/{}", parts[n - 2], parts[n - 1]),
    }
}

fn build_tool_result_map(messages: &[crate::claude_chat::ChatMessage]) -> HashMap<&str, &str> {
    use crate::claude_chat::ChatMessageContent;
    let mut m = HashMap::new();
    for msg in messages {
        if let ChatMessageContent::ToolResult { id, content } = &msg.content {
            m.insert(id.as_str(), content.as_str());
        }
    }
    m
}

fn compute_throbber_label(
    messages: &[crate::claude_chat::ChatMessage],
    result_map: &HashMap<&str, &str>,
) -> String {
    use crate::claude_chat::{ChatMessageContent, ChatRole};
    for msg in messages.iter().rev() {
        if !matches!(msg.role, ChatRole::Assistant) {
            break;
        }
        match &msg.content {
            ChatMessageContent::ToolUse { id, name, .. } => {
                if !result_map.contains_key(id.as_str()) {
                    return format!("Running {name}...");
                }
            },
            ChatMessageContent::Thinking { .. } | ChatMessageContent::Text(_) => {
                return "Thinking...".to_string();
            },
            ChatMessageContent::TurnComplete { .. }
            | ChatMessageContent::ToolResult { .. }
            | ChatMessageContent::Error(_) => continue,
        }
    }
    "Thinking...".to_string()
}

struct PaneCtx<'a> {
    editors: &'a mut SlotMap<EditorId, EditorState>,
    buffers: &'a BufferRegistry,
    runs: &'a SlotMap<RunId, RunState>,
    chats: &'a HashMap<ClaudeSessionId, ClaudeChatState>,
}

fn render_pane(
    pane: &Pane,
    is_focused: bool,
    ctx: PaneCtx<'_>,
    workspace_name: &str,
    mode: &str,
    render_tick: u64,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let text_style = if is_focused {
        theme.get(crate::theme::scope::UI_TEXT)
    } else {
        theme.get(crate::theme::scope::UI_TEXT_MUTED)
    };
    let (content_area, status_area) = split_pane_status(pane.area);

    let PaneCtx {
        editors,
        buffers,
        runs,
        chats,
    } = ctx;

    match &pane.view {
        View::Label(label) => {
            Paragraph::new(Text::styled(label.clone(), text_style))
                .centered()
                .render(content_area, buf);
        },
        View::Editor(editor_id) => {
            if let Some(editor) = editors.get_mut(*editor_id) {
                render_editor(editor, content_area, text_style, theme, buf, is_focused);
            }
        },
        View::Run(run_id) => {
            if let Some(run_state) = runs.get(*run_id) {
                render_run_pane(run_state, theme, content_area, is_focused, buf);
            }
        },
        View::Claude(session_id) => {
            if let Some(chat) = chats.get(session_id) {
                render_claude_pane(
                    chat,
                    editors,
                    buffers,
                    content_area,
                    is_focused,
                    render_tick,
                    theme,
                    buf,
                );
            }
        },
    }

    render_pane_status(
        &pane.view,
        is_focused,
        status_area,
        workspace_name,
        mode,
        editors,
        buffers,
        theme,
        buf,
    );
}

/// Minimal status bar for overlay panes (commits/rebase/reword/conflict).
/// Does not know about editors or buffers; shows only mode + workspace +
/// a short label identifying the overlay. Matches the visual style of
/// [`render_pane_status`] for a focused pane.
fn render_overlay_status(
    area: Rect,
    is_focused: bool,
    workspace_name: &str,
    mode: &str,
    label: &str,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let base_style = if is_focused {
        theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };
    let y = area.y;
    let end_x = area.x + area.width;
    for x in area.x..end_x {
        buf[(x, y)].set_char(' ').set_style(base_style);
    }

    let mut cursor = area.x;
    if is_focused {
        let (mode_label, mode_bg) = mode_segment(mode, theme);
        let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
        cursor = paint_segment(
            buf,
            y,
            cursor,
            end_x,
            &format!(" {mode_label} "),
            mode_style,
        );
        cursor = paint_segment(
            buf,
            y,
            cursor,
            end_x,
            &format!(" {workspace_name} "),
            base_style.add_modifier(Modifier::BOLD),
        );
    }
    let left_pad = if cursor == area.x { " " } else { "" };
    paint_segment(
        buf,
        y,
        cursor,
        end_x,
        &format!("{left_pad}{label} "),
        base_style,
    );
}

fn render_pane_dividers(dividers: &[Divider], theme: &crate::theme::Theme, buf: &mut Buffer) {
    let dim = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);
    let lit = theme.get(crate::theme::scope::UI_BORDER_FOCUSED);
    for d in dividers {
        let style = if d.touches_focus { lit } else { dim };
        let buf_end_x = buf.area.x + buf.area.width;
        let buf_end_y = buf.area.y + buf.area.height;
        match d.orientation {
            DividerOrientation::Vertical => {
                if d.x >= buf_end_x {
                    continue;
                }
                for yy in d.y..d.y.saturating_add(d.len).min(buf_end_y) {
                    buf[(d.x, yy)].set_char('│').set_style(style);
                }
            },
            DividerOrientation::Horizontal => {
                if d.y >= buf_end_y {
                    continue;
                }
                for xx in d.x..d.x.saturating_add(d.len).min(buf_end_x) {
                    buf[(xx, d.y)].set_char('─').set_style(style);
                }
            },
        }
    }
}

/// Partition a pane's area into its content region and the 1-row status bar
/// at the bottom. For panes shorter than 2 rows there is no room for a
/// status bar, so the full area is returned as content.
fn split_pane_status(area: Rect) -> (Rect, Rect) {
    if area.height < 2 {
        return (
            area,
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 0,
            },
        );
    }
    let content = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height - 1,
    };
    let status = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    (content, status)
}

fn render_pane_status(
    view: &View,
    is_focused: bool,
    area: Rect,
    workspace_name: &str,
    mode: &str,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let base_style = if is_focused {
        theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };

    let y = area.y;
    let end_x = area.x + area.width;
    for x in area.x..end_x {
        buf[(x, y)].set_char(' ').set_style(base_style);
    }

    let mut cursor = area.x;
    if is_focused {
        let (label, mode_bg) = mode_segment(mode, theme);
        let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
        cursor = paint_segment(buf, y, cursor, end_x, &format!(" {label} "), mode_style);
        let ws_style = base_style.add_modifier(Modifier::BOLD);
        cursor = paint_segment(
            buf,
            y,
            cursor,
            end_x,
            &format!(" {workspace_name} "),
            ws_style,
        );
    }

    let (filename, dirty, cursor_pos) = pane_status_info(view, editors, buffers);
    if let Some(name) = filename {
        let left_pad = if cursor == area.x { " " } else { "" };
        let text = if dirty {
            format!("{left_pad}{name} [+] ")
        } else {
            format!("{left_pad}{name} ")
        };
        cursor = paint_segment(buf, y, cursor, end_x, &text, base_style);
    }

    if let Some((line, col)) = cursor_pos {
        let text = format!(" {line}:{col} ");
        let width = text.chars().count() as u16;
        let start = end_x.saturating_sub(width);
        if start >= cursor {
            paint_segment(buf, y, start, end_x, &text, base_style);
        }
    }
    let _ = cursor;
}

fn paint_segment(
    buf: &mut Buffer,
    y: u16,
    start_x: u16,
    end_x: u16,
    text: &str,
    style: Style,
) -> u16 {
    let mut x = start_x;
    for ch in text.chars() {
        if x >= end_x {
            break;
        }
        buf[(x, y)].set_char(ch).set_style(style);
        x += 1;
    }
    x
}

fn mode_segment(mode: &str, theme: &crate::theme::Theme) -> (&'static str, Color) {
    use crate::theme::scope;
    let (label, default, scope_name) = match mode {
        "normal" => ("NOR", Color::Blue, scope::UI_STATUSLINE_NORMAL),
        "insert" => ("INS", Color::Green, scope::UI_STATUSLINE_INSERT),
        "run" => ("RUN", Color::Magenta, scope::UI_STATUSLINE_RUN),
        "commits" => ("COM", Color::Yellow, scope::UI_STATUSLINE_COMMITS),
        "rebase" => ("REB", Color::Red, scope::UI_STATUSLINE_REBASE),
        "reword" | "reword_insert" => ("RWD", Color::Red, scope::UI_STATUSLINE_REWORD),
        "conflict" => ("CNF", Color::LightRed, scope::UI_STATUSLINE_CONFLICT),
        "review" => ("REV", Color::Cyan, scope::UI_STATUSLINE_REVIEW),
        _ => ("---", Color::Gray, scope::UI_STATUSLINE_DEFAULT),
    };
    let color = theme.get(scope_name).fg.unwrap_or(default);
    (label, color)
}

fn pane_status_info(
    view: &View,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
) -> (Option<String>, bool, Option<(u32, u32)>) {
    match view {
        View::Editor(editor_id) => {
            let Some(editor) = editors.get_mut(*editor_id) else {
                return (None, false, None);
            };
            let buffer_id = editor.buffer_id;
            let path = buffers.path_for(buffer_id);
            let filename = path
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(str::to_string)
                .or_else(|| Some("[scratch]".to_string()));
            let dirty = buffers
                .get(buffer_id)
                .and_then(|b| b.read().ok().map(|g| g.dirty))
                .unwrap_or(false);
            let cursor_pos = editor_cursor_position(editor);
            (filename, dirty, cursor_pos)
        },
        View::Run(_) => (Some("[run]".to_string()), false, None),
        View::Claude(_) => (Some("[claude]".to_string()), false, None),
        View::Label(label) => (Some(label.clone()), false, None),
    }
}

fn editor_cursor_position(editor: &mut EditorState) -> Option<(u32, u32)> {
    if editor.review_view.is_some() {
        return None;
    }
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let sel = editor.selections.newest_anchor();
    let point = buffer_snapshot.point_for_anchor(&sel.head());
    Some((point.row + 1, point.column + 1))
}

fn render_run_pane(
    run_state: &RunState,
    theme: &crate::theme::Theme,
    area: Rect,
    is_focused: bool,
    buf: &mut Buffer,
) {
    if area.height < 2 || area.width < 4 {
        return;
    }

    let input_row = area.y + area.height - 1;
    let output_height = area.height.saturating_sub(1);

    // Collect all output lines from blocks (command headers + grid rows)
    let mut output_lines: Vec<OutputLine<'_>> = Vec::new();
    for block in &run_state.blocks {
        output_lines.push(OutputLine::CommandHeader(block.command.as_str()));
        for row_idx in 0..block.grid.line_count() {
            output_lines.push(OutputLine::GridRow(&block.grid, row_idx));
        }
        if let Some(err) = &block.error {
            output_lines.push(OutputLine::Error(err.as_str()));
        }
        if block.finished {
            let status = block.exit_status.unwrap_or(-1);
            output_lines.push(OutputLine::Status(status));
        }
        output_lines.push(OutputLine::Blank);
    }

    // Render output lines (bottom-aligned: show most recent output)
    let total = output_lines.len();
    let visible = output_height as usize;
    let start = total.saturating_sub(visible + run_state.scroll_offset);
    for (i, line) in output_lines.iter().skip(start).take(visible).enumerate() {
        let y = area.y + i as u16;
        match line {
            OutputLine::CommandHeader(cmd) => {
                let cmd_style = theme.get(crate::theme::scope::UI_BADGE_COMPLETE);
                write_str(buf, area.x, y, "$ ", cmd_style);
                let max_w = (area.width as usize).saturating_sub(2);
                let display: String = cmd.chars().take(max_w).collect();
                write_str(buf, area.x + 2, y, &display, cmd_style);
            },
            OutputLine::GridRow(grid, row_idx) => {
                let row = grid.row(*row_idx);
                let w = (area.width as usize).min(grid.width() as usize);
                for (col, cell) in row.iter().enumerate().take(w) {
                    if cell.ch == ' '
                        && cell.fg.is_none()
                        && cell.bg.is_none()
                        && cell.modifiers.is_empty()
                    {
                        continue;
                    }
                    let mut style = Style::default();
                    if let Some(fg) = cell.fg {
                        style = style.fg(fg);
                    }
                    if let Some(bg) = cell.bg {
                        style = style.bg(bg);
                    }
                    style = style.add_modifier(cell.modifiers);
                    let x = area.x + col as u16;
                    if x < area.x + area.width {
                        buf[(x, y)].set_char(cell.ch).set_style(style);
                    }
                }
            },
            OutputLine::Error(msg) => {
                let max_w = area.width as usize;
                let display: String = msg.chars().take(max_w).collect();
                write_str(
                    buf,
                    area.x,
                    y,
                    &display,
                    theme.get(crate::theme::scope::UI_ERROR),
                );
            },
            OutputLine::Status(code) => {
                let label = if *code == 0 {
                    String::new()
                } else {
                    format!("[exit {}]", code)
                };
                if !label.is_empty() {
                    write_str(
                        buf,
                        area.x,
                        y,
                        &label,
                        theme.get(crate::theme::scope::UI_TEXT_MUTED),
                    );
                }
            },
            OutputLine::Blank => {},
        }
    }

    let prompt_style = theme.get(crate::theme::scope::UI_KEY_LABEL);
    let input_style = theme.get(crate::theme::scope::UI_TEXT);
    let cursor_style = theme.get(crate::theme::scope::UI_CURSOR_INPUT);

    write_str(buf, area.x, input_row, "$ ", prompt_style);
    let input_text = run_state.input.as_str();
    let max_input = (area.width as usize).saturating_sub(2);
    let display_input: String = input_text.chars().take(max_input).collect();
    write_str(buf, area.x + 2, input_row, &display_input, input_style);

    if is_focused {
        let cursor_col = run_state.input.cursor_column();
        let cx = area.x + 2 + cursor_col as u16;
        if cx < area.x + area.width {
            buf[(cx, input_row)].set_style(cursor_style);
        }
    }
}

enum OutputLine<'a> {
    CommandHeader(&'a str),
    GridRow(&'a crate::run::VtermGrid, usize),
    Error(&'a str),
    Status(i32),
    Blank,
}

fn render_modal_run(
    run_state: &RunState,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.width < 20 || area.height < 8 {
        return;
    }

    let box_width = (area.width * 7 / 10).min(area.width.saturating_sub(4));
    let box_height = (area.height * 8 / 10).min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal_area = Rect::new(x, y, box_width, box_height);

    let title = {
        let raw = run_state
            .title
            .as_deref()
            .or_else(|| run_state.active_block().map(|b| b.command.as_str()))
            .unwrap_or("run");
        let max = (box_width as usize).saturating_sub(4);
        let display: String = raw.chars().take(max).collect();
        format!(" {display} ")
    };
    let modal_style = theme.get(crate::theme::scope::UI_MODAL_RUN);
    let border = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(title)
        .title_style(modal_style);
    let inner = border.inner(modal_area);
    border.render(modal_area, buf);

    let Some(active) = run_state.active_block() else {
        return;
    };

    let grid = &active.grid;
    let visible_rows = (inner.height as usize).saturating_sub(1);
    let total = grid.line_count();
    let start = total.saturating_sub(visible_rows + run_state.scroll_offset);
    let w = (inner.width as usize).min(grid.width() as usize);

    for (i, row_idx) in (start..total).take(visible_rows).enumerate() {
        let y = inner.y + i as u16;
        let row = grid.row(row_idx);
        for (col, cell) in row.iter().enumerate().take(w) {
            if cell.ch == ' ' && cell.fg.is_none() && cell.bg.is_none() && cell.modifiers.is_empty()
            {
                continue;
            }
            let mut style = Style::default();
            if let Some(fg) = cell.fg {
                style = style.fg(fg);
            }
            if let Some(bg) = cell.bg {
                style = style.bg(bg);
            }
            style = style.add_modifier(cell.modifiers);
            let cx = inner.x + col as u16;
            if cx < inner.x + inner.width {
                buf[(cx, y)].set_char(cell.ch).set_style(style);
            }
        }
    }

    let status_row = inner.y + inner.height.saturating_sub(1);
    let status = if active.finished {
        let code = active.exit_status.unwrap_or(-1);
        if code == 0 {
            "done -- press Escape to dismiss".to_owned()
        } else {
            format!("exited {} -- press Escape to dismiss", code)
        }
    } else {
        "running...".to_owned()
    };
    let status_style = if active.finished {
        theme.get(crate::theme::scope::UI_TEXT_MUTED)
    } else {
        theme.get(crate::theme::scope::UI_BADGE_ACTIVE)
    };
    write_str(buf, inner.x, status_row, &status, status_style);
}

fn render_editor(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    is_focused: bool,
) {
    if editor.review_view.is_some() {
        render_review(editor, inner, fallback_style, theme, buf);
        return;
    }

    let snapshot = editor.display_map.snapshot();
    let visible_rows = inner.height as u32;
    let total_rows = snapshot.line_count();
    let end_row = (editor.scroll_row + visible_rows).min(total_rows);
    if end_row <= editor.scroll_row {
        return;
    }

    let right = inner.x + inner.width;
    let bottom = inner.y + inner.height;

    {
        let mut x = inner.x;
        let mut y = inner.y;
        'chunks: for chunk in snapshot.highlighted_chunks(editor.scroll_row..end_row) {
            let style = chunk
                .highlight_style
                .as_ref()
                .map(|hs| hs.to_ratatui_style())
                .unwrap_or(fallback_style);
            for ch in chunk.text.chars() {
                if ch == '\n' {
                    y += 1;
                    x = inner.x;
                    if y >= bottom {
                        break 'chunks;
                    }
                    continue;
                }
                if x >= right {
                    continue;
                }
                buf[(x, y)].set_char(ch).set_style(style);
                x += 1;
            }
        }
    }

    if !is_focused {
        return;
    }

    let buffer_snapshot = snapshot.buffer_snapshot();
    let selection_style = theme.get(crate::theme::scope::UI_SELECTION_EDITOR);
    let cursor_style = theme.get(crate::theme::scope::UI_CURSOR);
    for selection in editor.selections.all_anchors() {
        let start_offset = buffer_snapshot.resolve_anchor(&selection.start);
        let end_offset = buffer_snapshot.resolve_anchor(&selection.end);
        let head_offset = buffer_snapshot.resolve_anchor(&selection.head());
        let rope = buffer_snapshot.rope();

        if start_offset != end_offset {
            let mut offset = start_offset;
            let mut chars = rope.chars_at(offset);
            while offset < end_offset {
                let Some(ch) = chars.next() else {
                    break;
                };
                if ch != '\n' && offset != head_offset {
                    let point = rope.offset_to_point(offset);
                    let display = snapshot.buffer_to_display(point);
                    if display.row >= editor.scroll_row && display.row < end_row {
                        let y = inner.y + (display.row - editor.scroll_row) as u16;
                        let x = inner.x + display.column as u16;
                        if x < right && y < bottom {
                            let cell = &mut buf[(x, y)];
                            cell.set_style(selection_style);
                        }
                    }
                }
                offset += ch.len_utf8();
            }
        }

        let head_point = buffer_snapshot.point_for_anchor(&selection.head());
        let display = snapshot.buffer_to_display(head_point);
        if display.row >= editor.scroll_row && display.row < end_row {
            let y = inner.y + (display.row - editor.scroll_row) as u16;
            let x = inner.x + display.column as u16;
            if x < right && y < bottom {
                let cell = &mut buf[(x, y)];
                let existing_char = cell.symbol().chars().next().unwrap_or(' ');
                let char_to_paint = if existing_char == '\0' {
                    ' '
                } else {
                    existing_char
                };
                cell.set_char(char_to_paint);
                cell.set_style(cursor_style);
            }
        }
    }
}

/// Side-by-side review renderer.
///
/// Layout per row:
/// ```text
///  NNN  <left content>           │  NNN  <right content>
/// ```
///
/// Changed tokens within a line are highlighted; the rest of the line
/// is rendered in the default style so only the structural diff is
/// visually emphasised (matching difftastic behaviour).
/// Modal reword editor: bordered frame with a header, an original-message
/// reference line, and the editable commit message rendered through the
/// real [`render_editor`] so the user gets full normal/insert modal
/// editing (motions, multi-line, selections).
///
/// `current_mode` is the live `Stoat::mode` string and is shown in the
/// help footer so users can see whether they're in the normal or insert
/// sub-mode.
fn render_reword(
    pane: &Pane,
    is_focused: bool,
    editor: &mut EditorState,
    cherry_picked_commit: &str,
    original_message: &str,
    current_mode: &str,
    workspace_name: &str,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(
        status_area,
        is_focused,
        workspace_name,
        current_mode,
        "reword",
        theme,
        buf,
    );
    if inner.width < 10 || inner.height < 4 {
        return;
    }

    let header_style = theme
        .get(crate::theme::scope::VCS_REBASE_REWORD)
        .add_modifier(Modifier::BOLD);
    let dim = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let body_style = theme.get(crate::theme::scope::UI_TEXT);

    let short = cherry_picked_commit.chars().take(7).collect::<String>();
    write_str(
        buf,
        inner.x,
        inner.y,
        &format!("reword {short} [{current_mode}]"),
        header_style,
    );
    let help = if current_mode == "reword_insert" {
        "Escape normal   Ctrl-s save   (empty message aborts)"
    } else {
        "i insert   h/j/k/l move   Ctrl-s save   Escape abort"
    };
    write_str(buf, inner.x, inner.y + 1, help, dim);
    write_str(
        buf,
        inner.x,
        inner.y + 2,
        &truncate_to_cols(
            &format!(
                "original: {}",
                original_message.lines().next().unwrap_or("")
            ),
            inner.width as usize,
        ),
        dim,
    );

    // Reserve the first four rows for header/help/original/spacer, then
    // hand the rest of the inner rect to `render_editor` so the editable
    // message renders with the full editor (cursor, selections, syntax).
    let editor_top = inner.y + 4;
    if editor_top >= inner.y + inner.height {
        return;
    }
    let editor_rect = Rect {
        x: inner.x,
        y: editor_top,
        width: inner.width,
        height: inner.y + inner.height - editor_top,
    };
    render_editor(editor, editor_rect, body_style, theme, buf, is_focused);
}

fn render_conflict(
    pane: &Pane,
    is_focused: bool,
    active: &ActiveRebase,
    workspace_name: &str,
    mode: &str,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    use crate::rebase::ConflictResolution;

    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(
        status_area,
        is_focused,
        workspace_name,
        mode,
        "conflict",
        theme,
        buf,
    );
    if inner.width < 20 || inner.height < 4 {
        return;
    }

    let (source_sha, files, selected, resolutions) = match active.pause.as_ref() {
        Some(RebasePause::Conflict {
            source_sha,
            files,
            selected,
            resolutions,
        }) => (source_sha, files, *selected, resolutions),
        _ => return,
    };

    let left_w = (inner.width / 3).max(20);
    let left_w = left_w.min(inner.width.saturating_sub(20));
    let sep_x = inner.x + left_w;
    let right_x = sep_x + 1;
    let right_w = inner.width.saturating_sub(left_w + 1);

    use crate::theme::scope as s;
    let dim = theme.get(s::UI_TEXT_MUTED);
    let header_style = theme.get(s::VCS_CONFLICT_HEADER);
    let sel_style = theme.get(crate::theme::scope::UI_SELECTION_REVERSED);
    let ours_style = theme.get(s::VCS_CONFLICT_OURS);
    let theirs_style = theme.get(s::VCS_CONFLICT_THEIRS);
    let file_style = theme.get(s::UI_TEXT);
    let add_hl = theme.get(s::DIFF_ADDED);
    let del_hl = theme.get(s::DIFF_DELETED);

    // Separator column.
    for y in inner.y..inner.y + inner.height {
        buf[(sep_x, y)].set_char('│').set_style(dim);
    }

    // Header row.
    let short = source_sha.chars().take(7).collect::<String>();
    write_str(
        buf,
        inner.x,
        inner.y,
        &truncate_to_cols(&format!("conflict picking {short}"), inner.width as usize),
        header_style,
    );
    write_str(
        buf,
        inner.x,
        inner.y + 1,
        &truncate_to_cols(
            "o take ours  t take theirs  s skip entry  Enter commit  a abort",
            inner.width as usize,
        ),
        dim,
    );

    // File list on the left.
    let list_top = inner.y + 3;
    for (i, file) in files.iter().enumerate() {
        let y = list_top + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let is_selected = i == selected;
        let row_style = if is_selected { sel_style } else { file_style };
        if is_selected {
            for x in inner.x..inner.x + left_w {
                buf[(x, y)].set_style(sel_style);
            }
        }
        let marker = match resolutions.get(&file.path).copied() {
            Some(ConflictResolution::TakeOurs) => 'O',
            Some(ConflictResolution::TakeTheirs) => 'T',
            Some(ConflictResolution::SkipEntry) => 'S',
            None => '?',
        };
        let marker_style = match resolutions.get(&file.path).copied() {
            Some(ConflictResolution::TakeOurs) => ours_style,
            Some(ConflictResolution::TakeTheirs) => theirs_style,
            _ => dim,
        };
        write_str(
            buf,
            inner.x,
            y,
            &format!("{marker} "),
            if is_selected { sel_style } else { marker_style },
        );
        let path_x = inner.x + 2;
        let path_max = (left_w as usize).saturating_sub(2);
        write_str(
            buf,
            path_x,
            y,
            &truncate_to_cols(&file.path.display().to_string(), path_max),
            row_style,
        );
    }

    // Right pane: content of the selected file with standard conflict
    // markers. Unconfigured files show `<<<<<<<`/`=======`/`>>>>>>>`
    // with ours then theirs (matching git rebase's default output).
    if let Some(file) = files.get(selected) {
        let mut y = inner.y + 3;
        let max_y = inner.y + inner.height;
        let max_w = right_w as usize;

        let draw_line = |y: &mut u16, text: &str, style: Style, buf: &mut Buffer| {
            if *y >= max_y {
                return;
            }
            write_str(buf, right_x, *y, &truncate_to_cols(text, max_w), style);
            *y += 1;
        };

        let header = match resolutions.get(&file.path).copied() {
            Some(ConflictResolution::TakeOurs) => ("will take OURS", ours_style),
            Some(ConflictResolution::TakeTheirs) => ("will take THEIRS", theirs_style),
            Some(ConflictResolution::SkipEntry) => ("entry skipped", dim),
            None => ("unresolved (defaults to theirs)", dim),
        };
        draw_line(&mut y, header.0, header.1, buf);
        y += 1;

        // Render OURS block.
        draw_line(&mut y, "<<<<<<< ours", del_hl, buf);
        for line in file.ours.as_deref().unwrap_or("").lines() {
            draw_line(&mut y, line, file_style, buf);
        }
        draw_line(&mut y, "=======", dim, buf);
        for line in file.theirs.as_deref().unwrap_or("").lines() {
            draw_line(&mut y, line, file_style, buf);
        }
        draw_line(&mut y, ">>>>>>> theirs", add_hl, buf);
        if let Some(ancestor) = &file.ancestor {
            y += 1;
            draw_line(&mut y, "--- ancestor ---", dim, buf);
            for line in ancestor.lines() {
                draw_line(&mut y, line, dim, buf);
            }
        }
    }
}

fn render_rebase(
    pane: &Pane,
    is_focused: bool,
    state: &RebaseState,
    workspace_name: &str,
    mode: &str,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(
        status_area,
        is_focused,
        workspace_name,
        mode,
        "rebase",
        theme,
        buf,
    );

    if inner.width < 10 || inner.height == 0 {
        return;
    }

    use crate::theme::scope as s;
    let sel_style = theme.get(crate::theme::scope::UI_SELECTION_REVERSED);
    let pick_style = theme.get(s::VCS_REBASE_PICK);
    let squash_style = theme.get(s::VCS_REBASE_SQUASH);
    let fixup_style = theme.get(s::VCS_REBASE_FIXUP);
    let reword_style = theme.get(s::VCS_REBASE_REWORD);
    let edit_style = theme.get(s::VCS_REBASE_EDIT);
    let drop_style = theme.get(s::VCS_REBASE_DROP);
    let summary_style = theme.get(s::UI_TEXT);
    let sha_style = theme.get(s::UI_KEY_LABEL);

    let help_rows: u16 = 2;
    let list_height = inner.height.saturating_sub(help_rows);

    for (i, entry) in state.todo.iter().take(list_height as usize).enumerate() {
        let y = inner.y + i as u16;
        let is_selected = i == state.selected;
        if is_selected {
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_style(sel_style);
            }
        }
        let (label, op_style) = match entry.op {
            RebaseTodoOp::Pick => ("pick  ", pick_style),
            RebaseTodoOp::Squash => ("squash", squash_style),
            RebaseTodoOp::Fixup => ("fixup ", fixup_style),
            RebaseTodoOp::Drop => ("drop  ", drop_style),
            RebaseTodoOp::Reword => ("reword", reword_style),
            RebaseTodoOp::Edit => ("edit  ", edit_style),
        };
        let row_style = if is_selected {
            sel_style
        } else {
            summary_style
        };
        write_str(
            buf,
            inner.x,
            y,
            label,
            if is_selected { sel_style } else { op_style },
        );
        let sha_x = inner.x + label.len() as u16 + 1;
        write_str(
            buf,
            sha_x,
            y,
            &entry.commit.short_sha,
            if is_selected { sel_style } else { sha_style },
        );
        let summary_x = sha_x + entry.commit.short_sha.len() as u16 + 1;
        let remaining = (inner.x + inner.width).saturating_sub(summary_x);
        if remaining > 0 {
            let summary = truncate_to_cols(&entry.commit.summary, remaining as usize);
            write_str(buf, summary_x, y, &summary, row_style);
        }
    }

    if list_height < inner.height {
        let help_y = inner.y + inner.height - help_rows;
        let help1 = "j/k move  K/J reorder  p/s/f/d set op  Enter run  q abort";
        let help2 = format!(
            "{} entries, onto {}",
            state.todo.len(),
            if state.onto.is_empty() {
                "<root>".to_string()
            } else {
                state.onto.chars().take(7).collect::<String>()
            }
        );
        let help_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
        write_str(
            buf,
            inner.x,
            help_y,
            &truncate_to_cols(help1, inner.width as usize),
            help_style,
        );
        write_str(
            buf,
            inner.x,
            help_y + 1,
            &truncate_to_cols(&help2, inner.width as usize),
            help_style,
        );
    }
}

fn render_commits(
    pane: &Pane,
    is_focused: bool,
    state: &mut CommitListState,
    workspace_name: &str,
    mode: &str,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(
        status_area,
        is_focused,
        workspace_name,
        mode,
        "commits",
        theme,
        buf,
    );

    if inner.width < 10 || inner.height == 0 {
        return;
    }

    let left_w = commit_list_width(inner.width);
    let sep_x = inner.x + left_w;
    let right_x = sep_x + 1;
    let right_w = inner.width.saturating_sub(left_w + 1);

    let sep_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    for y in inner.y..inner.y + inner.height {
        buf[(sep_x, y)].set_char('│').set_style(sep_style);
    }

    let left_area = Rect::new(inner.x, inner.y, left_w, inner.height);
    state.viewport_rows = left_area.height as usize;
    state.ensure_selected_visible(state.viewport_rows);
    render_commit_list_pane(state, theme, left_area, buf);

    if right_w > 0 {
        let right_area = Rect::new(right_x, inner.y, right_w, inner.height);
        render_commit_detail_pane(state, theme, right_area, buf);
    }
}

fn commit_list_width(total: u16) -> u16 {
    let target = (total as u32 * 2 / 5) as u16;
    target.clamp(22, 48).min(total.saturating_sub(12))
}

fn render_commit_list_pane(
    state: &CommitListState,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    use crate::theme::scope as s;
    let dim = theme.get(s::VCS_COMMIT_METADATA);
    if state.commits.is_empty() {
        let msg = if state.pending_load.is_some() {
            "loading commits..."
        } else {
            "no commits"
        };
        write_str(buf, area.x, area.y, msg, dim);
        return;
    }

    let sel_style = theme.get(crate::theme::scope::UI_SELECTION_REVERSED);
    let sha_style = theme.get(s::VCS_COMMIT_SHA);
    let summary_style = theme.get(s::VCS_COMMIT_SUMMARY);

    let top = state.scroll_top.min(state.commits.len().saturating_sub(1));
    let rows_visible = area.height as usize;
    let end = (top + rows_visible).min(state.commits.len());

    for (i, commit) in state.commits[top..end].iter().enumerate() {
        let y = area.y + i as u16;
        let is_selected = top + i == state.selected;
        let row_style = if is_selected {
            sel_style
        } else {
            summary_style
        };

        if is_selected && area.width > 0 {
            for x in area.x..area.x + area.width {
                buf[(x, y)].set_style(sel_style);
            }
        }

        let sha_x = area.x;
        let sha = &commit.short_sha;
        let sha_len = sha.len().min(area.width as usize);
        write_str(
            buf,
            sha_x,
            y,
            &sha[..sha_len],
            if is_selected { sel_style } else { sha_style },
        );

        let summary_x = sha_x + sha_len as u16 + 1;
        let remaining = (area.x + area.width).saturating_sub(summary_x);
        if remaining > 0 {
            let summary = truncate_to_cols(&commit.summary, remaining as usize);
            write_str(buf, summary_x, y, &summary, row_style);
        }
    }

    if state.pending_load.is_some() && end == state.commits.len() && end - top < rows_visible {
        let y = area.y + (end - top) as u16;
        write_str(buf, area.x, y, "loading more...", dim);
    } else if state.reached_end && end == state.commits.len() && end - top < rows_visible {
        let y = area.y + (end - top) as u16;
        write_str(buf, area.x, y, "(end of history)", dim);
    }
}

fn render_commit_detail_pane(
    state: &CommitListState,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    let dim = theme.get(crate::theme::scope::VCS_COMMIT_METADATA);
    let Some(sha) = state.selected_sha() else {
        write_str(buf, area.x, area.y, "no selection", dim);
        return;
    };

    let summary_rows = match state.summaries.get(sha) {
        Some(changes) => render_commit_summary(changes, theme, area, buf),
        None => {
            write_str(buf, area.x, area.y, "loading summary...", dim);
            1
        },
    };

    let preview_y = area.y + summary_rows as u16 + 1;
    if preview_y >= area.y + area.height {
        return;
    }
    let preview_area = Rect::new(
        area.x,
        preview_y,
        area.width,
        area.y + area.height - preview_y,
    );
    match state.preview_sessions.get(sha) {
        Some(session) => render_commit_preview(session, theme, preview_area, buf),
        None => {
            if preview_area.height > 0 {
                write_str(
                    buf,
                    preview_area.x,
                    preview_area.y,
                    "loading preview...",
                    dim,
                );
            }
        },
    }
}

fn render_commit_summary(
    changes: &[CommitFileChange],
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) -> usize {
    use crate::theme::scope as s;
    let header_style = theme.get(s::UI_TEXT).add_modifier(Modifier::BOLD);
    let path_style = theme.get(s::UI_TEXT);
    let add_style = theme.get(s::DIFF_ADDED);
    let del_style = theme.get(s::DIFF_DELETED);

    let total_add: u32 = changes.iter().map(|c| c.additions).sum();
    let total_del: u32 = changes.iter().map(|c| c.deletions).sum();
    let header = format!(
        "{} file{}, +{total_add} -{total_del}",
        changes.len(),
        if changes.len() == 1 { "" } else { "s" }
    );
    write_str(buf, area.x, area.y, &header, header_style);

    let mut rows_used = 1;
    let max_rows = (area.height as usize).saturating_sub(1);
    for (i, change) in changes.iter().take(max_rows).enumerate() {
        let y = area.y + 1 + i as u16;
        let kind_char = match change.kind {
            CommitFileChangeKind::Added => 'A',
            CommitFileChangeKind::Modified => 'M',
            CommitFileChangeKind::Deleted => 'D',
            CommitFileChangeKind::Renamed => 'R',
            CommitFileChangeKind::TypeChange => 'T',
        };
        write_str(buf, area.x, y, &format!("{kind_char} "), path_style);
        let rel = change.rel_path.display().to_string();
        let path_width = area.width.saturating_sub(2 + 12) as usize;
        let rel = truncate_to_cols(&rel, path_width);
        write_str(buf, area.x + 2, y, &rel, path_style);

        let stats = format!(" +{} -{}", change.additions, change.deletions);
        let stats_x = area.x + area.width.saturating_sub(stats.len() as u16);
        let split = stats.find('-').unwrap_or(stats.len());
        write_str(buf, stats_x, y, &stats[..split], add_style);
        write_str(buf, stats_x + split as u16, y, &stats[split..], del_style);
        rows_used += 1;
    }
    rows_used
}

/// Render a compact preview of a [`ReviewSession`]: each chunk's rows
/// painted sequentially with a yellow file/chunk header, top-to-bottom
/// within `area`. Does not rely on editor machinery; used by the
/// commits view's right pane.
fn render_commit_preview(
    session: &ReviewSession,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    use crate::theme::scope as s;
    let dim = theme.get(s::UI_TEXT_MUTED);
    let header_style = theme.get(s::VCS_COMMIT_SHA);
    let del_hl = theme.get(s::DIFF_DELETED);
    let add_hl = theme.get(s::DIFF_ADDED);
    let move_hl = theme.get(s::DIFF_MOVED).add_modifier(Modifier::ITALIC);
    let fallback_style = Style::default();

    let full_w = area.width as usize;
    let status_w: usize = 1;
    let num_w: usize = 5;
    let gutter_w = status_w + num_w;
    let sep: usize = 1;
    let half_w = (full_w.saturating_sub(sep)) / 2;
    let left_content_w = half_w.saturating_sub(gutter_w);
    let right_start = area.x + half_w as u16 + sep as u16;
    let right_content_w = (full_w - half_w - sep).saturating_sub(gutter_w);
    let sep_x = area.x + half_w as u16;

    let mut y = area.y;
    let end_y = area.y + area.height;

    for file in &session.files {
        for chunk_id in &file.chunks {
            let Some(chunk) = session.chunks.get(chunk_id) else {
                continue;
            };
            if y >= end_y {
                return;
            }
            let file_total = file.chunks.len();
            let lang_str = file
                .language
                .as_ref()
                .map(|l| l.name.to_string())
                .unwrap_or_default();
            let label = format!(
                "{} --- {}/{} --- {}",
                file.rel_path,
                chunk.chunk_index_in_file + 1,
                file_total,
                lang_str
            );
            let label_trunc = truncate_to_cols(&label, area.width as usize);
            write_str(buf, area.x, y, &label_trunc, header_style);
            y += 1;

            for row in &chunk.hunk.rows {
                if y >= end_y {
                    return;
                }
                if sep_x < area.x + area.width {
                    buf[(sep_x, y)].set_char('│').set_style(dim);
                }
                let left_num_x = area.x + status_w as u16;
                let right_num_x = right_start + status_w as u16;
                let left_text_x = left_num_x + num_w as u16;
                let right_text_x = right_num_x + num_w as u16;
                match row {
                    ReviewRow::Context { left, right } => {
                        render_side_num(buf, left_num_x, y, left.line_num, dim);
                        render_side_text(
                            buf,
                            left_text_x,
                            y,
                            &left.text,
                            left_content_w,
                            fallback_style,
                            &[],
                            fallback_style,
                            &[],
                            move_hl,
                        );
                        render_side_num(buf, right_num_x, y, right.line_num, dim);
                        render_side_text(
                            buf,
                            right_text_x,
                            y,
                            &right.text,
                            right_content_w,
                            fallback_style,
                            &[],
                            fallback_style,
                            &[],
                            move_hl,
                        );
                    },
                    ReviewRow::Changed { left, right } => {
                        if let Some(l) = left {
                            render_side_num(buf, left_num_x, y, l.line_num, dim);
                            render_side_text(
                                buf,
                                left_text_x,
                                y,
                                &l.text,
                                left_content_w,
                                fallback_style,
                                &l.change_spans,
                                del_hl,
                                &l.moved_spans,
                                move_hl,
                            );
                        } else {
                            render_empty_num(buf, left_num_x, y, dim);
                        }
                        if let Some(r) = right {
                            render_side_num(buf, right_num_x, y, r.line_num, dim);
                            render_side_text(
                                buf,
                                right_text_x,
                                y,
                                &r.text,
                                right_content_w,
                                fallback_style,
                                &r.change_spans,
                                add_hl,
                                &r.moved_spans,
                                move_hl,
                            );
                        } else {
                            render_empty_num(buf, right_num_x, y, dim);
                        }
                    },
                }
                y += 1;
            }
        }
    }
}

fn truncate_to_cols(text: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > max_cols {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

fn render_review(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let snapshot = editor.display_map.snapshot();
    let view = match editor.review_view.as_ref() {
        Some(v) => v,
        None => return,
    };
    let rows = &view.rows;
    let total_rows = snapshot.line_count();
    let visible = inner.height as u32;
    let end_row = (editor.scroll_row + visible).min(total_rows);
    if end_row <= editor.scroll_row {
        return;
    }

    let full_w = inner.width as usize;
    // One-char status column + 5-char line-number column per side.
    let status_w: usize = 1;
    let num_w: usize = 5;
    let gutter_w: usize = status_w + num_w;
    // Separator column (1 char) between the two sides.
    let sep: usize = 1;
    let half_w = (full_w.saturating_sub(sep)) / 2;
    let left_content_w = half_w.saturating_sub(gutter_w);
    let right_start = inner.x + half_w as u16 + sep as u16;
    let right_content_w = (full_w - half_w - sep).saturating_sub(gutter_w);

    use crate::theme::scope as s;
    let dim_style = theme.get(s::DIFF_CONTEXT);
    let del_hl = theme.get(s::DIFF_DELETED);
    let add_hl = theme.get(s::DIFF_ADDED);
    // Moved lines render in the theme's diff.moved color. Both sides of
    // a review use the same style so it reads as "relocated" rather than
    // gain/loss. See stoat::display_map::syntax_theme::DiffTheme.
    let move_hl = theme.get(s::DIFF_MOVED).add_modifier(Modifier::ITALIC);
    let current_style = theme.get(s::DIFF_CURRENT_HUNK);

    for display_row in editor.scroll_row..end_row {
        let y = inner.y + (display_row - editor.scroll_row) as u16;
        if y >= inner.y + inner.height {
            break;
        }

        // Render separator column
        let sep_x = inner.x + half_w as u16;
        if sep_x < inner.x + inner.width {
            buf[(sep_x, y)].set_char('│').set_style(dim_style);
        }

        match snapshot.classify_row(display_row) {
            BlockRowKind::BufferRow { buffer_row } => {
                let Some(row) = rows.get(buffer_row as usize) else {
                    continue;
                };
                if let Some((chunk_id, status)) = view.chunk_and_status_at_row(buffer_row) {
                    let is_current = Some(chunk_id) == view.current_chunk;
                    paint_status_gutter(buf, inner.x, y, status, is_current, current_style, theme);
                    paint_status_gutter(
                        buf,
                        right_start,
                        y,
                        status,
                        is_current,
                        current_style,
                        theme,
                    );
                }
                let left_num_x = inner.x + status_w as u16;
                let right_num_x = right_start + status_w as u16;
                let left_text_x = left_num_x + num_w as u16;
                let right_text_x = right_num_x + num_w as u16;
                match row {
                    ReviewRow::Context { left, right } => {
                        render_side_num(buf, left_num_x, y, left.line_num, dim_style);
                        render_side_text(
                            buf,
                            left_text_x,
                            y,
                            &left.text,
                            left_content_w,
                            fallback_style,
                            &[],
                            fallback_style,
                            &[],
                            move_hl,
                        );
                        render_side_num(buf, right_num_x, y, right.line_num, dim_style);
                        render_side_text(
                            buf,
                            right_text_x,
                            y,
                            &right.text,
                            right_content_w,
                            fallback_style,
                            &[],
                            fallback_style,
                            &[],
                            move_hl,
                        );
                    },
                    ReviewRow::Changed { left, right } => {
                        if let Some(l) = left {
                            render_side_num(buf, left_num_x, y, l.line_num, dim_style);
                            render_side_text(
                                buf,
                                left_text_x,
                                y,
                                &l.text,
                                left_content_w,
                                fallback_style,
                                &l.change_spans,
                                del_hl,
                                &l.moved_spans,
                                move_hl,
                            );
                        } else {
                            render_empty_num(buf, left_num_x, y, dim_style);
                        }
                        if let Some(r) = right {
                            render_side_num(buf, right_num_x, y, r.line_num, dim_style);
                            render_side_text(
                                buf,
                                right_text_x,
                                y,
                                &r.text,
                                right_content_w,
                                fallback_style,
                                &r.change_spans,
                                add_hl,
                                &r.moved_spans,
                                move_hl,
                            );
                        } else {
                            render_empty_num(buf, right_num_x, y, dim_style);
                        }
                    },
                }
            },
            BlockRowKind::Block { block, line_index } => {
                let line = block.get_line(line_index);
                let block_style = theme.get(crate::theme::scope::UI_PROMPT);
                for (i, ch) in line.chars().enumerate() {
                    let x = inner.x + i as u16;
                    if x >= inner.x + inner.width {
                        break;
                    }
                    buf[(x, y)].set_char(ch).set_style(block_style);
                }
            },
        }
    }
}

fn render_side_num(buf: &mut Buffer, x: u16, y: u16, num: u32, style: Style) {
    let s = format!("{num:>4} ");
    for (i, ch) in s.chars().enumerate() {
        let col = x + i as u16;
        if col >= buf.area.x + buf.area.width {
            break;
        }
        buf[(col, y)].set_char(ch).set_style(style);
    }
}

fn paint_status_gutter(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    status: crate::review_session::ChunkStatus,
    is_current: bool,
    current_style: Style,
    theme: &crate::theme::Theme,
) {
    use crate::{review_session::ChunkStatus, theme::scope as s};

    if x >= buf.area.x + buf.area.width {
        return;
    }
    if is_current {
        buf[(x, y)].set_char('│').set_style(current_style);
        return;
    }
    let (ch, style) = match status {
        ChunkStatus::Pending => (' ', theme.get(s::UI_TEXT_MUTED)),
        ChunkStatus::Staged => ('+', theme.get(s::DIFF_ADDED)),
        ChunkStatus::Unstaged => ('-', theme.get(s::DIFF_DELETED)),
        ChunkStatus::Skipped => ('~', theme.get(s::UI_TEXT_MUTED)),
    };
    buf[(x, y)].set_char(ch).set_style(style);
}

fn render_empty_num(buf: &mut Buffer, x: u16, y: u16, style: Style) {
    for i in 0..5u16 {
        let col = x + i;
        if col >= buf.area.x + buf.area.width {
            break;
        }
        buf[(col, y)].set_char('.').set_style(style);
    }
}

/// Render text with sub-line change span highlighting. Characters
/// within any `spans` range get `highlight_style`; characters within
/// any `moved_spans` range get the diff theme's move color (cyan)
/// regardless of which side they live on. The rest get `base_style`.
///
/// Move highlighting takes precedence over change highlighting: if a
/// byte falls in both a change span and a moved span, the move color
/// wins so users see at a glance that the token relocated rather than
/// was replaced.
#[allow(clippy::too_many_arguments)]
fn render_side_text(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text: &str,
    max_cols: usize,
    base_style: Style,
    spans: &[std::ops::Range<usize>],
    highlight_style: Style,
    moved_spans: &[std::ops::Range<usize>],
    moved_style: Style,
) {
    for (col, (byte_idx, ch)) in text.char_indices().enumerate() {
        if col >= max_cols {
            break;
        }
        let x = start_x + col as u16;
        if x >= buf.area.x + buf.area.width {
            break;
        }
        let in_moved = moved_spans
            .iter()
            .any(|s| byte_idx >= s.start && byte_idx < s.end);
        let in_span = spans
            .iter()
            .any(|s| byte_idx >= s.start && byte_idx < s.end);
        let style = if in_moved {
            moved_style
        } else if in_span {
            highlight_style
        } else {
            base_style
        };
        buf[(x, y)].set_char(ch).set_style(style);
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

        // Large enough that tree-sitter's progress callback fires at least
        // once during parsing. ~100k bytes of valid rust.
        let text = "fn a() {}\n".repeat(10_000);
        let mut buf = TextBuffer::with_text(buffer_id, &text);
        let snap1 = buf.snapshot.clone();

        // Successful parse with no deadline.
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

        // Reinstall the parse output as the prior, then reparse against a
        // bumped snapshot with an already-expired deadline.
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

        // The surviving prior must still be usable for a successful reparse.
        // If the prior tree had been mutated by edit_tree on the failed
        // attempt, this call would double-stamp the input edit.
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
}
