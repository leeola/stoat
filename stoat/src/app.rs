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
    host::{
        AgentMessage, ClaudeCodeHost, ClaudeCodeSessions, ClaudeNotification, ClaudeSessionId,
        CommitFileChange, CommitFileChangeKind, FsHost, GitHost, LocalFs, LocalGit, RebaseTodoOp,
    },
    keymap::{Keymap, KeymapState, ResolvedAction, ResolvedArg, StateValue},
    pane::{DockPanel, DockVisibility, FocusTarget, Pane, View},
    rebase::RebaseState,
    review::ReviewRow,
    review_session::ReviewSession,
    run::{PtyNotification, RunId, RunState},
    workspace::{Workspace, WorkspaceId},
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
    pub(crate) command_palette: Option<CommandPalette>,
    pub(crate) language_registry: Arc<LanguageRegistry>,
    pub(crate) syntax_styles: SyntaxStyles,
    pub(crate) workspaces: SlotMap<WorkspaceId, Workspace>,
    pub(crate) active_workspace: WorkspaceId,
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
        let keymap = config
            .map(|c| Keymap::compile(&c))
            .unwrap_or_else(|| Keymap::compile(&stoat_config::Config { blocks: vec![] }));

        let syntax_styles = SyntaxStyles::standard();
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
        let active_workspace = workspaces.insert(Workspace::new(initial_git_root, &executor));
        workspaces[active_workspace].id = active_workspace;

        let (pty_tx, pty_rx) = tokio::sync::mpsc::channel(256);
        let (claude_tx, claude_rx) = tokio::sync::mpsc::channel(256);

        Self {
            size: Rect::default(),
            mode: "normal".into(),
            executor,
            keymap,
            settings,
            command_palette: None,
            language_registry,
            syntax_styles,
            workspaces,
            active_workspace,
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
                UpdateEffect::Quit => break,
                UpdateEffect::None => {},
            }
        }
        Ok(())
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

        if self.command_palette.is_some() {
            return self.dispatch_palette_key(key);
        }

        if self.mode == "run" {
            if let Some(effect) = self.handle_run_key(key) {
                return effect;
            }
        }

        if self.mode == "insert" {
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
        let ws = self.active_workspace();
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

        let ws = self.active_workspace_mut();

        // Populate chat state.
        if let Some(chat) = ws.chats.get_mut(&session_id) {
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
                AgentMessage::Init { .. }
                | AgentMessage::Unknown { .. }
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

        let source = BadgeSource::Claude(session_id);
        let visible = ws.is_claude_visible(session_id);

        match message {
            AgentMessage::Thinking { .. }
            | AgentMessage::ToolUse { .. }
            | AgentMessage::ToolResult { .. }
            | AgentMessage::Text { .. }
            | AgentMessage::PartialText { .. }
            | AgentMessage::ServerToolUse { .. }
            | AgentMessage::ServerToolResult { .. } => {
                if visible {
                    ws.badges.remove_by_source(source);
                } else {
                    match ws.badges.find_by_source(source) {
                        Some(id) => {
                            if let Some(badge) = ws.badges.get_mut(id) {
                                badge.state = BadgeState::Active;
                                badge.detail = detail_for_message(message);
                            }
                        },
                        None => {
                            ws.badges.insert(Badge {
                                source,
                                anchor: Anchor::TopCenter,
                                state: BadgeState::Active,
                                label: "claude".into(),
                                detail: detail_for_message(message),
                            });
                        },
                    }
                }
                UpdateEffect::Redraw
            },
            AgentMessage::Result { .. } => {
                if visible {
                    ws.badges.remove_by_source(source);
                } else {
                    match ws.badges.find_by_source(source) {
                        Some(id) => {
                            if let Some(badge) = ws.badges.get_mut(id) {
                                badge.state = BadgeState::Complete;
                                badge.detail = None;
                            }
                        },
                        None => {
                            ws.badges.insert(Badge {
                                source,
                                anchor: Anchor::TopCenter,
                                state: BadgeState::Complete,
                                label: "claude".into(),
                                detail: None,
                            });
                        },
                    }
                }
                UpdateEffect::Redraw
            },
            AgentMessage::Error { message: msg } => {
                if visible {
                    ws.badges.remove_by_source(source);
                } else {
                    match ws.badges.find_by_source(source) {
                        Some(id) => {
                            if let Some(badge) = ws.badges.get_mut(id) {
                                badge.state = BadgeState::Error;
                                badge.detail = Some(msg.clone());
                            }
                        },
                        None => {
                            ws.badges.insert(Badge {
                                source,
                                anchor: Anchor::TopCenter,
                                state: BadgeState::Error,
                                label: "claude".into(),
                                detail: Some(msg.clone()),
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
            | AgentMessage::AuthRequired { .. }
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
        let overlay_pane =
            if (commits_mode && ws.commits.is_some()) || (rebase_mode && ws.rebase.is_some()) {
                Some(ws.panes.focus())
            } else {
                None
            };

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
                self.render_tick,
                &mut buf,
            );
        }

        if let Some(pane_id) = overlay_pane {
            let pane = ws.panes.pane(pane_id);
            let is_focused = matches!(ws.focus, FocusTarget::SplitPane(id) if id == pane_id);
            if commits_mode {
                if let Some(state) = ws.commits.as_mut() {
                    render_commits(pane, is_focused, state, &mut buf);
                }
            } else if rebase_mode {
                if let Some(state) = ws.rebase.as_ref() {
                    render_rebase(pane, is_focused, state, &mut buf);
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
                render_dock_minimized(dock, is_focused, &mut buf);
            } else {
                render_dock_open(
                    dock,
                    is_focused,
                    &mut ws.editors,
                    &ws.buffers,
                    &ws.chats,
                    self.render_tick,
                    &mut buf,
                );
            }
        }
        render_badges(&ws.badges, self.size, self.render_tick, &mut buf);
        if let Some(run_id) = self.modal_run {
            if let Some(run_state) = ws.runs.get(run_id) {
                render_modal_run(run_state, self.size, &mut buf);
            }
        } else if let Some(palette) = &self.command_palette {
            render_command_palette(palette, self.size, &mut buf);
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
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    MiniHelpFooter { text, style }
                })
            } else {
                None
            };
            render_mini_help(&self.mode, &bindings, footer.as_ref(), self.size, &mut buf);
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

const PRIMARY_MODES: &[&str] = &["normal", "insert", "run", "commits", "rebase"];

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

fn render_mini_help(
    mode: &str,
    bindings: &[(&str, String)],
    footer: Option<&MiniHelpFooter>,
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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(format!(" {mode} "))
        .title_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(help_area);
    block.render(help_area, buf);

    let key_style = Style::default().fg(Color::Cyan);
    let action_style = Style::default().fg(Color::White);

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
            for col_offset in 0..inner.width {
                let col = inner.x + col_offset;
                buf[(col, sep_row)]
                    .set_char('─')
                    .set_style(Style::default().fg(Color::DarkGray));
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

struct MiniHelpFooter {
    text: String,
    style: Style,
}

fn render_command_palette(palette: &CommandPalette, area: Rect, buf: &mut Buffer) {
    match palette.phase() {
        crate::command_palette::PalettePhase::Filter {
            input,
            filtered,
            selected,
        } => render_palette_filter(input, filtered, *selected, area, buf),
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
            area,
            buf,
        ),
    }
}

fn render_palette_filter(
    input: &str,
    filtered: &[&'static stoat_action::registry::RegistryEntry],
    selected: usize,
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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .title(" command palette ")
        .title_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(palette_area);
    block.render(palette_area, buf);

    let prompt_style = Style::default().fg(Color::Yellow);
    let input_style = Style::default().fg(Color::White);
    let row_style = Style::default().fg(Color::White);
    let selected_style = Style::default().fg(Color::Black).bg(Color::Cyan);
    let desc_style = Style::default().fg(Color::DarkGray);
    let cursor_style = Style::default().fg(Color::Black).bg(Color::White);

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
    for col in inner.x..inner.x + inner.width {
        buf[(col, separator_row)]
            .set_char('─')
            .set_style(Style::default().fg(Color::DarkGray));
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
                .set_style(Style::default().fg(Color::DarkGray));
        }
        let doc_top = doc_separator_row + 1;
        for (i, line) in doc_lines.iter().enumerate() {
            write_str(
                buf,
                inner.x,
                doc_top + i as u16,
                line,
                Style::default().fg(Color::Gray),
            );
        }
    }
}

fn render_palette_collect_args(
    entry: &'static stoat_action::registry::RegistryEntry,
    collected: &[stoat_action::ParamValue],
    current: usize,
    input: &str,
    error: Option<&str>,
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

    let title = format!(" {} ", entry.def.name());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .title(title)
        .title_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(palette_area);
    block.render(palette_area, buf);

    let label_style = Style::default().fg(Color::Yellow);
    let value_style = Style::default().fg(Color::White);
    let cursor_style = Style::default().fg(Color::Black).bg(Color::White);
    let error_style = Style::default().fg(Color::Red);
    let muted_style = Style::default().fg(Color::DarkGray);

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
    for (i, line) in body_lines.iter().enumerate() {
        write_str(
            buf,
            inner.x,
            body_top + i as u16,
            line,
            Style::default().fg(Color::Gray),
        );
    }
}

fn format_param_value(v: &stoat_action::ParamValue) -> String {
    match v {
        stoat_action::ParamValue::String(s) => s.clone(),
        stoat_action::ParamValue::Number(n) => n.to_string(),
        stoat_action::ParamValue::Bool(b) => b.to_string(),
    }
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

fn render_single_badge(badge: &Badge, x: u16, y: u16, render_tick: u64, buf: &mut Buffer) {
    let (w, h) = badge_size(badge);
    let border_style = badge_border_style(badge.state);

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

    let content_style = Style::default().fg(Color::White);
    write_str(buf, x + 1, y + 1, &badge.label, content_style);
}

fn render_badges(badges: &BadgeTray, area: Rect, render_tick: u64, buf: &mut Buffer) {
    if badges.is_empty() {
        return;
    }

    for anchor in Anchor::ALL {
        let tray = badges.tray(anchor);
        let visible: Vec<_> = badges
            .at_anchor(anchor)
            .take(tray.max_visible as usize)
            .collect();
        if visible.is_empty() {
            continue;
        }

        let sizes: Vec<(u16, u16)> = visible.iter().map(|(_, b)| badge_size(b)).collect();
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

        for (i, (_, badge)) in visible.iter().enumerate() {
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

            render_single_badge(badge, draw_x, draw_y, render_tick, buf);

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

fn badge_border_style(state: BadgeState) -> Style {
    match state {
        BadgeState::Active => Style::default().fg(Color::Yellow),
        BadgeState::Complete => Style::default().fg(Color::Green),
        BadgeState::Error => Style::default().fg(Color::Red),
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

fn render_dock_minimized(dock: &DockPanel, is_focused: bool, buf: &mut Buffer) {
    let area = dock.area;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
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
    buf: &mut Buffer,
) {
    let area = dock.area;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let border_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
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
                render_claude_pane(chat, editors, buffers, inner, is_focused, render_tick, buf);
            }
        },
        View::Editor(editor_id) => {
            if let Some(editor) = editors.get_mut(*editor_id) {
                render_editor(editor, inner, border_style, buf, is_focused);
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

    let sep_style = Style::default().fg(Color::DarkGray);
    for x in area.x..area.x + area.width {
        write_cell(buf, x, separator_y, '-', sep_style);
    }

    let meta_style = Style::default().fg(Color::DarkGray);
    let time_style = Style::default().fg(Color::Gray);
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

    let user_style = Style::default().fg(Color::Green);
    let text_style = Style::default().fg(Color::White);
    let thinking_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);
    let tool_header_style = Style::default().fg(Color::Blue);
    let tool_body_style = Style::default().fg(Color::DarkGray);
    let error_style = Style::default().fg(Color::Red);
    let turn_sep_style = Style::default().fg(Color::DarkGray);
    let throbber_style = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);

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
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        render_editor(editor, input_area, input_style, buf, is_focused);
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
    render_tick: u64,
    buf: &mut Buffer,
) {
    let border_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let text_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(pane.area);
    block.render(pane.area, buf);

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
                .render(inner, buf);
        },
        View::Editor(editor_id) => {
            if let Some(editor) = editors.get_mut(*editor_id) {
                render_editor(editor, inner, text_style, buf, is_focused);
            }
            let _ = buffers;
        },
        View::Run(run_id) => {
            if let Some(run_state) = runs.get(*run_id) {
                render_run_pane(run_state, inner, is_focused, buf);
            }
        },
        View::Claude(session_id) => {
            if let Some(chat) = chats.get(session_id) {
                render_claude_pane(chat, editors, buffers, inner, is_focused, render_tick, buf);
            }
        },
    }
}

fn render_run_pane(run_state: &RunState, area: Rect, is_focused: bool, buf: &mut Buffer) {
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
                write_str(buf, area.x, y, "$ ", Style::default().fg(Color::Green));
                let max_w = (area.width as usize).saturating_sub(2);
                let display: String = cmd.chars().take(max_w).collect();
                write_str(
                    buf,
                    area.x + 2,
                    y,
                    &display,
                    Style::default().fg(Color::Green),
                );
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
                write_str(buf, area.x, y, &display, Style::default().fg(Color::Red));
            },
            OutputLine::Status(code) => {
                let label = if *code == 0 {
                    String::new()
                } else {
                    format!("[exit {}]", code)
                };
                if !label.is_empty() {
                    write_str(buf, area.x, y, &label, Style::default().fg(Color::DarkGray));
                }
            },
            OutputLine::Blank => {},
        }
    }

    // Render input line
    let prompt_style = Style::default().fg(Color::Cyan);
    let input_style = Style::default().fg(Color::White);
    let cursor_style = Style::default().fg(Color::Black).bg(Color::White);

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

fn render_modal_run(run_state: &RunState, area: Rect, buf: &mut Buffer) {
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
    let border = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(title)
        .title_style(Style::default().fg(Color::Yellow));
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
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Yellow)
    };
    write_str(buf, inner.x, status_row, &status, status_style);
}

fn render_editor(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    buf: &mut Buffer,
    is_focused: bool,
) {
    if editor.review_view.is_some() {
        render_review(editor, inner, fallback_style, buf);
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
    let selection_style = Style::default().bg(Color::DarkGray);
    let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
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
fn render_rebase(pane: &Pane, is_focused: bool, state: &RebaseState, buf: &mut Buffer) {
    let border_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(pane.area);
    block.render(pane.area, buf);

    if inner.width < 10 || inner.height == 0 {
        return;
    }

    let sel_style = Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::REVERSED);
    let pick_style = Style::default().fg(Color::Green);
    let squash_style = Style::default().fg(Color::Yellow);
    let fixup_style = Style::default().fg(Color::Yellow);
    let drop_style = Style::default().fg(Color::Red);
    let summary_style = Style::default().fg(Color::White);
    let sha_style = Style::default().fg(Color::Cyan);

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
        write_str(
            buf,
            inner.x,
            help_y,
            &truncate_to_cols(help1, inner.width as usize),
            Style::default().fg(Color::DarkGray),
        );
        write_str(
            buf,
            inner.x,
            help_y + 1,
            &truncate_to_cols(&help2, inner.width as usize),
            Style::default().fg(Color::DarkGray),
        );
    }
}

fn render_commits(pane: &Pane, is_focused: bool, state: &mut CommitListState, buf: &mut Buffer) {
    let border_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(pane.area);
    block.render(pane.area, buf);

    if inner.width < 10 || inner.height == 0 {
        return;
    }

    let left_w = commit_list_width(inner.width);
    let sep_x = inner.x + left_w;
    let right_x = sep_x + 1;
    let right_w = inner.width.saturating_sub(left_w + 1);

    for y in inner.y..inner.y + inner.height {
        buf[(sep_x, y)]
            .set_char('│')
            .set_style(Style::default().fg(Color::DarkGray));
    }

    let left_area = Rect::new(inner.x, inner.y, left_w, inner.height);
    state.viewport_rows = left_area.height as usize;
    state.ensure_selected_visible(state.viewport_rows);
    render_commit_list_pane(state, left_area, buf);

    if right_w > 0 {
        let right_area = Rect::new(right_x, inner.y, right_w, inner.height);
        render_commit_detail_pane(state, right_area, buf);
    }
}

fn commit_list_width(total: u16) -> u16 {
    let target = (total as u32 * 2 / 5) as u16;
    target.clamp(22, 48).min(total.saturating_sub(12))
}

fn render_commit_list_pane(state: &CommitListState, area: Rect, buf: &mut Buffer) {
    let dim = Style::default().fg(Color::DarkGray);
    if state.commits.is_empty() {
        let msg = if state.pending_load.is_some() {
            "loading commits..."
        } else {
            "no commits"
        };
        write_str(buf, area.x, area.y, msg, dim);
        return;
    }

    let sel_style = Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::REVERSED);
    let sha_style = Style::default().fg(Color::Yellow);
    let summary_style = Style::default().fg(Color::White);

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

fn render_commit_detail_pane(state: &CommitListState, area: Rect, buf: &mut Buffer) {
    let dim = Style::default().fg(Color::DarkGray);
    let Some(sha) = state.selected_sha() else {
        write_str(buf, area.x, area.y, "no selection", dim);
        return;
    };

    let summary_rows = match state.summaries.get(sha) {
        Some(changes) => render_commit_summary(changes, area, buf),
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
        Some(session) => render_commit_preview(session, preview_area, buf),
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

fn render_commit_summary(changes: &[CommitFileChange], area: Rect, buf: &mut Buffer) -> usize {
    let header_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let path_style = Style::default().fg(Color::White);
    let add_style = Style::default().fg(Color::Green);
    let del_style = Style::default().fg(Color::Red);

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
fn render_commit_preview(session: &ReviewSession, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let dim = Style::default().fg(Color::DarkGray);
    let header_style = Style::default().fg(Color::Yellow);
    let del_hl = Style::default().fg(Color::Red);
    let add_hl = Style::default().fg(Color::Green);
    let move_hl = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::ITALIC);
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

fn render_review(editor: &mut EditorState, inner: Rect, fallback_style: Style, buf: &mut Buffer) {
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

    let dim_style = Style::default().fg(Color::DarkGray);
    let del_hl = Style::default().fg(Color::Red);
    let add_hl = Style::default().fg(Color::Green);
    // Moved lines render in cyan, matching the central DiffTheme. Both
    // sides of a review use the same color so it reads as "relocated"
    // rather than gain/loss. See stoat::display_map::syntax_theme::DiffTheme.
    let move_hl = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::ITALIC);
    let current_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

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
                    paint_status_gutter(buf, inner.x, y, status, is_current, current_style);
                    paint_status_gutter(buf, right_start, y, status, is_current, current_style);
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
                for (i, ch) in line.chars().enumerate() {
                    let x = inner.x + i as u16;
                    if x >= inner.x + inner.width {
                        break;
                    }
                    buf[(x, y)]
                        .set_char(ch)
                        .set_style(Style::default().fg(Color::Yellow));
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
) {
    use crate::review_session::ChunkStatus;

    if x >= buf.area.x + buf.area.width {
        return;
    }
    if is_current {
        buf[(x, y)].set_char('│').set_style(current_style);
        return;
    }
    let (ch, style) = match status {
        ChunkStatus::Pending => (' ', Style::default().fg(Color::DarkGray)),
        ChunkStatus::Staged => ('+', Style::default().fg(Color::Green)),
        ChunkStatus::Unstaged => ('-', Style::default().fg(Color::Red)),
        ChunkStatus::Skipped => ('~', Style::default().fg(Color::DarkGray)),
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
        let styles = SyntaxStyles::standard();
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
