//! GUI Claude chat ItemView.
//!
//! Pane-hosted entity that carries a chat scrollback
//! (`Vec<ChatMessage>` reusing the shared types from
//! `stoat::claude_chat`), an [`Editor::auto_height`] input pinned at
//! the bottom, and an `Arc<dyn ClaudeCodeSession>` populated in the
//! background by [`ClaudeCodeHost::new_session`].
//!
//! `submit` (driven by the `ClaudeSubmit` action) reads the input
//! text, clears the input buffer, appends a User [`ChatMessage`] to
//! the scrollback, and forwards the text to the session -- or queues
//! to `pending_sends` when the session has not landed yet. The
//! queue drains automatically when the session arrives.
//!
//! Once the session is installed and `pending_sends` have drained,
//! the same task awaits `ClaudeCodeSession::recv` in a loop and
//! converts the `AgentMessage::{Text, Error}` variants into
//! Assistant `ChatMessage`s on the scrollback. The richer variants
//! (`Thinking`, `ToolUse`, `ToolResult`, `Usage`, ...) are routed by
//! the sibling tool-card / usage / status items and remain no-ops in
//! this file.

use crate::{
    editor::Editor,
    globals::ClaudeCodeHostGlobal,
    item::{DeserializeSnafu, ItemError, ItemHandle, ItemView},
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, App, AppContext, Context, ElementId, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task, WeakEntity,
    Window,
};
use std::{collections::HashSet, sync::Arc};
use stoat::{
    claude_chat::{ChatMessage, ChatMessageContent, ChatRole},
    host::{AgentMessage, ClaudeCodeHost, ClaudeCodeSession},
};

pub struct ClaudeChat {
    pub(crate) input: gpui::Entity<Editor>,
    pub(crate) messages: Vec<ChatMessage>,
    /// Weak handle to the owning workspace. Used by the
    /// checkpoint-marker click handler to route a restore request
    /// through [`Workspace::restore_to_checkpoint`].
    pub(crate) workspace: WeakEntity<Workspace>,
    session: Option<Arc<dyn ClaudeCodeSession>>,
    /// User submissions that arrived before the host returned a live
    /// session. Drained in arrival order once [`Self::session`] is
    /// installed; cleared mid-drain on the first send failure (the
    /// failure is logged at `tracing::warn` and the queue is dropped
    /// rather than retried).
    pending_sends: Vec<String>,
    /// `ToolUse.id`s whose card renders the full input + result body
    /// instead of the collapsed preview. Set membership is the only
    /// source of truth for expansion; clicks and the focused-card
    /// keyboard path both flip the same set.
    pub(crate) expanded_tool_ids: HashSet<String>,
    /// `ToolUse.id` of the tool card currently focused for keyboard
    /// navigation. `ClaudeFocusNext`/`PrevToolCard` move focus across
    /// cards; `ClaudeToggleToolCardExpand` flips
    /// [`Self::expanded_tool_ids`] membership for this id.
    pub(crate) focused_tool_id: Option<String>,
    /// Kept alive on the entity so the background task that asks the
    /// host for a session is dropped when the chat is dropped.
    _create_task: Option<Task<()>>,
}

impl ClaudeChat {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let input = cx.new(|cx| Editor::auto_height(1, 8, window, cx));
        let host = cx.global::<ClaudeCodeHostGlobal>().0.clone();
        let create_task = cx.spawn(async move |this, cx| {
            install_session(host, this, cx).await;
        });
        Self {
            input,
            messages: Vec::new(),
            workspace,
            session: None,
            pending_sends: Vec::new(),
            expanded_tool_ids: HashSet::new(),
            focused_tool_id: None,
            _create_task: Some(create_task),
        }
    }

    /// Toggle expansion of the tool card identified by `id`. Powers
    /// the header-click path.
    pub fn toggle_expanded(&mut self, id: &str, cx: &mut Context<'_, Self>) {
        if !self.expanded_tool_ids.remove(id) {
            self.expanded_tool_ids.insert(id.to_string());
        }
        cx.notify();
    }

    /// Engage focus on the most-recent `ToolUse` when no card is
    /// focused; otherwise advance focus toward older cards and wrap
    /// back to the most-recent card after the oldest.
    pub fn focus_next_tool_card(&mut self, cx: &mut Context<'_, Self>) {
        let ids = tool_use_ids(&self.messages);
        if ids.is_empty() {
            return;
        }
        let next = match &self.focused_tool_id {
            None => ids.last().cloned(),
            Some(current) => {
                let idx = ids.iter().position(|id| id == current);
                match idx {
                    Some(0) => ids.last().cloned(),
                    Some(i) => Some(ids[i - 1].clone()),
                    None => ids.last().cloned(),
                }
            },
        };
        self.focused_tool_id = next;
        cx.notify();
    }

    /// Symmetric counterpart to [`Self::focus_next_tool_card`] that
    /// cycles toward newer cards and wraps to the oldest after the
    /// most-recent.
    pub fn focus_prev_tool_card(&mut self, cx: &mut Context<'_, Self>) {
        let ids = tool_use_ids(&self.messages);
        if ids.is_empty() {
            return;
        }
        let next = match &self.focused_tool_id {
            None => ids.first().cloned(),
            Some(current) => {
                let idx = ids.iter().position(|id| id == current);
                match idx {
                    Some(i) if i + 1 == ids.len() => ids.first().cloned(),
                    Some(i) => Some(ids[i + 1].clone()),
                    None => ids.first().cloned(),
                }
            },
        };
        self.focused_tool_id = next;
        cx.notify();
    }

    /// Toggle expansion of the focused tool card. No-op when no
    /// card is focused.
    pub fn toggle_focused_expansion(&mut self, cx: &mut Context<'_, Self>) {
        let Some(id) = self.focused_tool_id.clone() else {
            return;
        };
        self.toggle_expanded(&id, cx);
    }

    pub fn submit(&mut self, cx: &mut Context<'_, Self>) {
        let text = read_input_text(&self.input, cx);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let outgoing = trimmed.to_string();
        clear_input(&self.input, cx);
        self.messages.push(ChatMessage {
            role: ChatRole::User,
            content: ChatMessageContent::Text(outgoing.clone()),
            checkpoint_sha: None,
        });
        match self.session.as_ref() {
            Some(session) => spawn_send(session.clone(), outgoing, cx),
            None => self.pending_sends.push(outgoing),
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn push_message(&mut self, message: ChatMessage, cx: &mut Context<'_, Self>) {
        self.messages.push(message);
        cx.notify();
    }
}

/// Collect every `ToolUse` id from the scrollback in arrival order
/// (oldest first). Used by [`ClaudeChat`]'s focus-cycling methods to
/// turn `focused_tool_id` walks into deterministic index motion.
fn tool_use_ids(messages: &[ChatMessage]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match &m.content {
            ChatMessageContent::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

async fn install_session(
    host: Arc<dyn ClaudeCodeHost>,
    this: gpui::WeakEntity<ClaudeChat>,
    cx: &mut gpui::AsyncApp,
) {
    let session: Arc<dyn ClaudeCodeSession> = match host.new_session().await {
        Ok(s) => Arc::from(s),
        Err(err) => {
            tracing::warn!(
                target: "stoat_gui::claude_chat",
                ?err,
                "claude session creation failed"
            );
            return;
        },
    };
    let pending = {
        let Ok(pending) = this.update(cx, |chat, _| {
            chat.session = Some(session.clone());
            std::mem::take(&mut chat.pending_sends)
        }) else {
            return;
        };
        pending
    };
    for text in pending {
        if let Err(err) = session.send(&text).await {
            tracing::warn!(
                target: "stoat_gui::claude_chat",
                ?err,
                "claude pending-send drain failed"
            );
            break;
        }
    }
    loop {
        let Some(msg) = session.recv().await else {
            break;
        };
        if this
            .update(cx, |chat, cx| handle_recv_message(chat, &msg, cx))
            .is_err()
        {
            break;
        }
    }
}

/// Per-message recv dispatch. Owns assistant `Text` (trimmed;
/// empty drops), `Error`, `ToolUse`, and `ToolResult`. Other
/// variants (`Thinking`, `Usage`, ...) are routed by their own
/// sibling items and remain no-ops here.
fn handle_recv_message(
    chat: &mut ClaudeChat,
    message: &AgentMessage,
    cx: &mut Context<'_, ClaudeChat>,
) {
    match message {
        AgentMessage::Text { text } => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return;
            }
            chat.messages.push(ChatMessage {
                role: ChatRole::Assistant,
                content: ChatMessageContent::Text(trimmed.to_string()),
                checkpoint_sha: None,
            });
            cx.notify();
        },
        AgentMessage::Error { message } => {
            chat.messages.push(ChatMessage {
                role: ChatRole::Assistant,
                content: ChatMessageContent::Error(message.clone()),
                checkpoint_sha: None,
            });
            cx.notify();
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
                checkpoint_sha: None,
            });
            cx.notify();
        },
        AgentMessage::ToolResult {
            id,
            content,
            status,
            ..
        } => {
            chat.messages.push(ChatMessage {
                role: ChatRole::Assistant,
                content: ChatMessageContent::ToolResult {
                    id: id.clone(),
                    content: content.clone(),
                    status: *status,
                },
                checkpoint_sha: None,
            });
            cx.notify();
        },
        _ => {},
    }
}

fn spawn_send(session: Arc<dyn ClaudeCodeSession>, text: String, cx: &mut Context<'_, ClaudeChat>) {
    cx.spawn(async move |_, _| {
        if let Err(err) = session.send(&text).await {
            tracing::warn!(
                target: "stoat_gui::claude_chat",
                ?err,
                "claude submit send failed"
            );
        }
    })
    .detach();
}

fn read_input_text(input: &gpui::Entity<Editor>, cx: &App) -> String {
    let editor = input.read(cx);
    editor
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .map(|b| b.read(cx).text())
        .unwrap_or_default()
}

fn clear_input(input: &gpui::Entity<Editor>, cx: &mut Context<'_, ClaudeChat>) {
    let Some(buffer) = input
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .cloned()
    else {
        return;
    };
    buffer.update(cx, |b, cx| {
        let len = b.text().len();
        if len > 0 {
            b.edit(0..len, "", cx);
        }
    });
}

impl ItemView for ClaudeChat {
    fn tab_label(&self, _cx: &App) -> SharedString {
        SharedString::from("Claude")
    }

    fn deserialize(
        _value: serde_json::Value,
        _cx: &mut Context<'_, Self>,
    ) -> Result<Self, ItemError> {
        DeserializeSnafu {
            reason: "claude chat deserialize not yet implemented",
        }
        .fail()
    }
}

impl Render for ClaudeChat {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let count = self.messages.len();
        let mut scrollback = div().flex().flex_col().flex_grow().w_full();
        for idx in 0..count {
            if let Some(row) = render_message_row(self, idx, cx) {
                scrollback = scrollback.child(row);
            }
        }
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(scrollback)
            .child(self.input.clone())
    }
}

fn render_message_row(
    chat: &ClaudeChat,
    idx: usize,
    cx: &mut Context<'_, ClaudeChat>,
) -> Option<AnyElement> {
    let message = chat.messages.get(idx)?;
    if let ChatMessageContent::ToolUse { id, name, input } = &message.content {
        let card = crate::claude_tool_card::render_tool_card(chat, id, name, input, cx);
        return Some(div().flex().w_full().child(card).into_any_element());
    }
    let text = message_text(message)?;
    let bubble = div().px_2().py_1().child(text);
    Some(match message.role {
        ChatRole::User => {
            let mut row = div().flex().flex_row_reverse().w_full().child(bubble);
            if let Some(sha) = restorable_sha(message) {
                row = row.child(render_checkpoint_marker(idx, sha.to_string(), cx));
            }
            row.into_any_element()
        },
        ChatRole::Assistant => div().flex().w_full().child(bubble).into_any_element(),
    })
}

/// Restorable-message sha when the message originates from the user
/// and the workspace captured a stash sha at submit time. Returns
/// `None` for assistant messages and for user messages without a
/// captured sha.
fn restorable_sha(message: &ChatMessage) -> Option<&str> {
    if !matches!(message.role, ChatRole::User) {
        return None;
    }
    if !matches!(message.content, ChatMessageContent::Text(_)) {
        return None;
    }
    message.checkpoint_sha.as_deref()
}

fn render_checkpoint_marker(
    idx: usize,
    sha: String,
    cx: &mut Context<'_, ClaudeChat>,
) -> AnyElement {
    let element_id: ElementId = SharedString::from(format!("claude_checkpoint:{idx}")).into();
    div()
        .id(element_id)
        .px_2()
        .py_1()
        .child(SharedString::from("o"))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            let Some(workspace) = this.workspace.upgrade() else {
                return;
            };
            let sha = sha.clone();
            workspace.update(cx, |w, cx| w.restore_to_checkpoint(sha, cx));
        }))
        .into_any_element()
}

fn message_text(message: &ChatMessage) -> Option<SharedString> {
    match &message.content {
        ChatMessageContent::Text(text) => Some(SharedString::from(text.clone())),
        ChatMessageContent::Thinking { text } => {
            Some(SharedString::from(format!("(thinking) {text}")))
        },
        ChatMessageContent::Error(err) => Some(SharedString::from(format!("error: {err}"))),
        ChatMessageContent::TurnComplete { .. } => Some(SharedString::from("(turn complete)")),
        ChatMessageContent::ToolUse { .. } | ChatMessageContent::ToolResult { .. } => None,
    }
}

/// Dispatch the [`stoat_action::OpenClaude`] action. Creates a fresh
/// [`ClaudeChat`] entity and adds it to the focused pane's item
/// list. The chat's `new` method asks the host for a session on a
/// background task; no further wiring happens here.
pub fn dispatch_open_claude(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let pane_id = workspace.pane_tree().read(cx).focus();
    let Some(pane) = workspace.pane_tree().read(cx).pane(pane_id).cloned() else {
        return;
    };
    let weak_workspace = cx.weak_entity();
    let chat = cx.new(|cx| ClaudeChat::new(weak_workspace, window, cx));
    pane.update(cx, |p, cx| {
        p.add_item(Box::new(chat), cx);
    });
}

/// Dispatch the [`stoat_action::ClaudeSubmit`] action. Finds the
/// focused pane's active item, downcasts to [`ClaudeChat`], and
/// invokes `submit` on it. No-op when the active item is not a
/// chat.
pub fn dispatch_claude_submit(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(chat) = focused_chat(workspace, cx) {
        chat.update(cx, |c, cx| c.submit(cx));
    }
}

/// Dispatch the [`stoat_action::ClaudeFocusNextToolCard`] action.
/// Advances the focused tool card on the active chat toward older
/// cards; no-op when the active item is not a chat.
pub fn dispatch_claude_focus_next_tool_card(
    workspace: &mut Workspace,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(chat) = focused_chat(workspace, cx) {
        chat.update(cx, |c, cx| c.focus_next_tool_card(cx));
    }
}

/// Dispatch the [`stoat_action::ClaudeFocusPrevToolCard`] action.
/// Advances the focused tool card on the active chat toward newer
/// cards; no-op when the active item is not a chat.
pub fn dispatch_claude_focus_prev_tool_card(
    workspace: &mut Workspace,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(chat) = focused_chat(workspace, cx) {
        chat.update(cx, |c, cx| c.focus_prev_tool_card(cx));
    }
}

/// Dispatch the [`stoat_action::ClaudeToggleToolCardExpand`] action.
/// Toggles expansion of the focused tool card on the active chat;
/// no-op when the active item is not a chat or when no card is
/// focused.
pub fn dispatch_claude_toggle_tool_card_expand(
    workspace: &mut Workspace,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(chat) = focused_chat(workspace, cx) {
        chat.update(cx, |c, cx| c.toggle_focused_expansion(cx));
    }
}

fn focused_chat(
    workspace: &Workspace,
    cx: &mut Context<'_, Workspace>,
) -> Option<gpui::Entity<ClaudeChat>> {
    let pane_id = workspace.pane_tree().read(cx).focus();
    let pane = workspace.pane_tree().read(cx).pane(pane_id).cloned()?;
    let active_view = pane.read(cx).active_item().map(ItemHandle::to_any_view)?;
    active_view.downcast::<ClaudeChat>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        globals::{
            ClaudeCodeHostGlobal, ClipboardHostGlobal, ExecutorGlobal, FsHostGlobal,
            FsWatchHostGlobal, GitHostGlobal,
        },
        workspace::Workspace,
    };
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat::host::{
        fake::{FakeClipboard, FakeFs, FakeGit},
        ClipboardHost, FsHost, FsWatchHost, GitHost,
    };
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext, host: Arc<dyn ClaudeCodeHost>, git: Arc<FakeGit>) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        let clipboard: Arc<dyn ClipboardHost> = Arc::new(FakeClipboard::new());
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(FsHostGlobal(fs));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
            cx.set_global(ClaudeCodeHostGlobal(host));
            cx.set_global(ClipboardHostGlobal(clipboard));
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
        });
    }

    struct Harness<'a> {
        workspace: Entity<Workspace>,
        git: Arc<FakeGit>,
        vcx: &'a mut VisualTestContext,
    }

    fn new_harness<'a>(cx: &'a mut TestAppContext, host: Arc<dyn ClaudeCodeHost>) -> Harness<'a> {
        new_harness_with_root(cx, host, "/repo")
    }

    fn new_harness_with_root<'a>(
        cx: &'a mut TestAppContext,
        host: Arc<dyn ClaudeCodeHost>,
        repo_root: &str,
    ) -> Harness<'a> {
        let git = Arc::new(FakeGit::new());
        install_globals(cx, host, git.clone());
        let root = PathBuf::from(repo_root);
        let (workspace, vcx) = cx.add_window_view({
            let root = root.clone();
            move |_window, cx| Workspace::new("main", root, cx)
        });
        Harness {
            workspace,
            git,
            vcx,
        }
    }

    fn open_chat(h: &mut Harness<'_>) -> Entity<ClaudeChat> {
        h.workspace.update_in(h.vcx, |w, window, cx| {
            dispatch_open_claude(w, window, cx);
        });
        focused_chat(h).expect("chat is focused active item")
    }

    fn focused_chat(h: &mut Harness<'_>) -> Option<Entity<ClaudeChat>> {
        h.workspace.read_with(h.vcx, |w, cx| {
            let pane_id = w.pane_tree().read(cx).focus();
            let pane = w.pane_tree().read(cx).pane(pane_id).cloned()?;
            let view = pane.read(cx).active_item().map(ItemHandle::to_any_view)?;
            view.downcast::<ClaudeChat>().ok()
        })
    }

    fn type_into_input(chat: &Entity<ClaudeChat>, h: &mut Harness<'_>, text: &str) {
        let buffer = chat.read_with(h.vcx, |c, cx| {
            c.input
                .read(cx)
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("auto-height editor has singleton buffer")
                .clone()
        });
        buffer.update(h.vcx, |b, cx| {
            let len = b.text().len();
            b.edit(0..len, text, cx);
        });
    }

    fn input_text(chat: &Entity<ClaudeChat>, h: &mut Harness<'_>) -> String {
        chat.read_with(h.vcx, |c, cx| read_input_text(&c.input, cx))
    }

    #[test]
    fn open_claude_adds_chat_to_focused_pane() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            dispatch_open_claude(w, window, cx);
        });

        let chat = focused_chat(&mut h);
        assert!(
            chat.is_some(),
            "OpenClaude should add a ClaudeChat to the focused pane"
        );
    }

    #[test]
    fn submit_appends_user_message_and_clears_input() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);
        type_into_input(&chat, &mut h, "hello world");

        chat.update(h.vcx, |c, cx| c.submit(cx));

        let messages = chat.read_with(h.vcx, |c, _| c.messages.clone());
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.role, ChatRole::User);
        match &msg.content {
            ChatMessageContent::Text(text) => assert_eq!(text, "hello world"),
            other => panic!("expected text content, got {other:?}"),
        }
        assert_eq!(input_text(&chat, &mut h), "");
    }

    #[test]
    fn submit_with_empty_input_is_noop() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        chat.update(h.vcx, |c, cx| c.submit(cx));
        chat.update(h.vcx, |c, cx| {
            // whitespace-only input is also a no-op
            let buffer = c
                .input
                .read(cx)
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("auto-height editor has singleton buffer")
                .clone();
            buffer.update(cx, |b, cx| b.edit(0..0, "   ", cx));
            c.submit(cx);
        });

        let messages = chat.read_with(h.vcx, |c, _| c.messages.clone());
        assert_eq!(messages.len(), 0);
    }

    #[test]
    fn submit_calls_session_send_when_session_ready() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let fake_session = host.push_session(stoat::host::fake::FakeClaudeCode::new());
        let mut h = new_harness(&mut cx, host.clone() as Arc<dyn ClaudeCodeHost>);

        let chat = open_chat(&mut h);
        h.vcx.run_until_parked();
        let session_ready = chat.read_with(h.vcx, |c, _| c.session.is_some());
        assert!(session_ready, "session should be installed after parking");

        type_into_input(&chat, &mut h, "ping");
        chat.update(h.vcx, |c, cx| c.submit(cx));
        h.vcx.run_until_parked();

        assert_eq!(fake_session.sent_messages(), vec!["ping".to_string()]);
        let pending = chat.read_with(h.vcx, |c, _| c.pending_sends.clone());
        assert!(pending.is_empty(), "session-ready path skips the queue");
    }

    #[test]
    fn submit_queues_when_session_not_ready() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        type_into_input(&chat, &mut h, "queued");
        chat.update(h.vcx, |c, cx| c.submit(cx));

        let (pending, session_present) =
            chat.read_with(h.vcx, |c, _| (c.pending_sends.clone(), c.session.is_some()));
        assert_eq!(pending, vec!["queued".to_string()]);
        assert!(!session_present, "no session means submits queue");
    }

    #[test]
    fn pending_sends_drain_when_session_arrives() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let fake_session = host.push_session(stoat::host::fake::FakeClaudeCode::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        type_into_input(&chat, &mut h, "queued-before-session");
        chat.update(h.vcx, |c, cx| c.submit(cx));

        let pending_before = chat.read_with(h.vcx, |c, _| c.pending_sends.clone());
        assert_eq!(
            pending_before,
            vec!["queued-before-session".to_string()],
            "submit before session installs queues the text",
        );

        h.vcx.run_until_parked();

        assert_eq!(
            fake_session.sent_messages(),
            vec!["queued-before-session".to_string()]
        );
        let pending_after = chat.read_with(h.vcx, |c, _| c.pending_sends.clone());
        assert!(
            pending_after.is_empty(),
            "queue should drain when session lands",
        );
    }

    #[test]
    fn dispatch_claude_submit_routes_to_focused_chat() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);
        type_into_input(&chat, &mut h, "via dispatch");

        h.workspace.update_in(h.vcx, |w, _window, cx| {
            dispatch_claude_submit(w, cx);
        });

        let messages = chat.read_with(h.vcx, |c, _| c.messages.clone());
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn push_message_extends_scrollback() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        chat.update(h.vcx, |c, cx| {
            c.push_message(
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: ChatMessageContent::Text("hello".into()),
                    checkpoint_sha: None,
                },
                cx,
            );
        });

        let roles: Vec<ChatRole> =
            chat.read_with(h.vcx, |c, _| c.messages.iter().map(|m| m.role).collect());
        assert_eq!(roles, vec![ChatRole::Assistant]);
    }

    fn assistant_texts(chat: &Entity<ClaudeChat>, h: &Harness<'_>) -> Vec<String> {
        chat.read_with(h.vcx, |c, _| {
            c.messages
                .iter()
                .filter(|m| m.role == ChatRole::Assistant)
                .filter_map(|m| match &m.content {
                    ChatMessageContent::Text(text) => Some(text.clone()),
                    _ => None,
                })
                .collect()
        })
    }

    fn assistant_errors(chat: &Entity<ClaudeChat>, h: &Harness<'_>) -> Vec<String> {
        chat.read_with(h.vcx, |c, _| {
            c.messages
                .iter()
                .filter(|m| m.role == ChatRole::Assistant)
                .filter_map(|m| match &m.content {
                    ChatMessageContent::Error(err) => Some(err.clone()),
                    _ => None,
                })
                .collect()
        })
    }

    #[test]
    fn recv_text_appends_assistant_message() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let fake_session = host.push_session(stoat::host::fake::FakeClaudeCode::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        fake_session.push_text("hi from claude");
        h.vcx.run_until_parked();

        assert_eq!(
            assistant_texts(&chat, &h),
            vec!["hi from claude".to_string()]
        );
    }

    #[test]
    fn recv_error_appends_assistant_error() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let fake_session = host.push_session(stoat::host::fake::FakeClaudeCode::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        fake_session.push_error("something went wrong");
        h.vcx.run_until_parked();

        assert_eq!(
            assistant_errors(&chat, &h),
            vec!["something went wrong".to_string()]
        );
    }

    #[test]
    fn recv_text_trims_and_drops_empty() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let fake_session = host.push_session(stoat::host::fake::FakeClaudeCode::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        fake_session.push_text("\n   \n");
        fake_session.push_text("  real reply  ");
        h.vcx.run_until_parked();

        assert_eq!(
            assistant_texts(&chat, &h),
            vec!["real reply".to_string()],
            "whitespace-only frames drop; surviving text is trimmed",
        );
    }

    #[test]
    fn recv_tool_use_appends_assistant_tool_card() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let fake_session = host.push_session(stoat::host::fake::FakeClaudeCode::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        fake_session.push_tool_use("Bash", "{\"command\":\"ls\"}");
        h.vcx.run_until_parked();

        let cards: Vec<(String, String)> = chat.read_with(h.vcx, |c, _| {
            c.messages
                .iter()
                .filter_map(|m| match &m.content {
                    ChatMessageContent::ToolUse { name, input, .. } => {
                        Some((name.clone(), input.clone()))
                    },
                    _ => None,
                })
                .collect()
        });
        assert_eq!(
            cards,
            vec![("Bash".to_string(), "{\"command\":\"ls\"}".to_string())],
        );
    }

    #[test]
    fn recv_tool_result_appends_completion() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let fake_session = host.push_session(stoat::host::fake::FakeClaudeCode::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        fake_session.push_tool_use("Bash", "{\"command\":\"ls\"}");
        fake_session.push_tool_result("toolu_Bash", "file1\nfile2\n");
        h.vcx.run_until_parked();

        let results: Vec<String> = chat.read_with(h.vcx, |c, _| {
            c.messages
                .iter()
                .filter_map(|m| match &m.content {
                    ChatMessageContent::ToolResult { content, .. } => Some(content.clone()),
                    _ => None,
                })
                .collect()
        });
        assert_eq!(results, vec!["file1\nfile2\n".to_string()]);
    }

    fn push_tool_use(chat: &Entity<ClaudeChat>, h: &mut Harness<'_>, id: &str, name: &str) {
        chat.update(h.vcx, |c, cx| {
            c.push_message(
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: ChatMessageContent::ToolUse {
                        id: id.into(),
                        name: name.into(),
                        input: "{}".into(),
                    },
                    checkpoint_sha: None,
                },
                cx,
            );
        });
    }

    #[test]
    fn focus_next_tool_card_engages_then_cycles_older() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        push_tool_use(&chat, &mut h, "toolu_a", "Bash");
        push_tool_use(&chat, &mut h, "toolu_b", "Bash");

        chat.update(h.vcx, |c, cx| c.focus_next_tool_card(cx));
        let focused = chat.read_with(h.vcx, |c, _| c.focused_tool_id.clone());
        assert_eq!(focused.as_deref(), Some("toolu_b"));

        chat.update(h.vcx, |c, cx| c.focus_next_tool_card(cx));
        let focused = chat.read_with(h.vcx, |c, _| c.focused_tool_id.clone());
        assert_eq!(focused.as_deref(), Some("toolu_a"));

        chat.update(h.vcx, |c, cx| c.focus_next_tool_card(cx));
        let focused = chat.read_with(h.vcx, |c, _| c.focused_tool_id.clone());
        assert_eq!(focused.as_deref(), Some("toolu_b"), "wraps to newest");
    }

    #[test]
    fn focus_prev_tool_card_engages_then_cycles_newer() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        push_tool_use(&chat, &mut h, "toolu_a", "Bash");
        push_tool_use(&chat, &mut h, "toolu_b", "Bash");

        chat.update(h.vcx, |c, cx| c.focus_prev_tool_card(cx));
        let focused = chat.read_with(h.vcx, |c, _| c.focused_tool_id.clone());
        assert_eq!(focused.as_deref(), Some("toolu_a"));

        chat.update(h.vcx, |c, cx| c.focus_prev_tool_card(cx));
        let focused = chat.read_with(h.vcx, |c, _| c.focused_tool_id.clone());
        assert_eq!(focused.as_deref(), Some("toolu_b"));

        chat.update(h.vcx, |c, cx| c.focus_prev_tool_card(cx));
        let focused = chat.read_with(h.vcx, |c, _| c.focused_tool_id.clone());
        assert_eq!(focused.as_deref(), Some("toolu_a"), "wraps to oldest");
    }

    #[test]
    fn focus_next_on_empty_scrollback_is_noop() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        chat.update(h.vcx, |c, cx| c.focus_next_tool_card(cx));
        let focused = chat.read_with(h.vcx, |c, _| c.focused_tool_id.clone());
        assert_eq!(focused, None);
    }

    #[test]
    fn toggle_expanded_inserts_then_removes_by_id() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        push_tool_use(&chat, &mut h, "toolu_a", "Bash");
        chat.update(h.vcx, |c, cx| c.toggle_expanded("toolu_a", cx));
        let ids: Vec<String> =
            chat.read_with(h.vcx, |c, _| c.expanded_tool_ids.iter().cloned().collect());
        assert_eq!(ids, vec!["toolu_a".to_string()]);

        chat.update(h.vcx, |c, cx| c.toggle_expanded("toolu_a", cx));
        let ids: Vec<String> =
            chat.read_with(h.vcx, |c, _| c.expanded_tool_ids.iter().cloned().collect());
        assert!(ids.is_empty(), "second toggle removes the id");
    }

    #[test]
    fn toggle_focused_expansion_is_noop_without_focus() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        push_tool_use(&chat, &mut h, "toolu_a", "Bash");
        chat.update(h.vcx, |c, cx| c.toggle_focused_expansion(cx));
        let ids: Vec<String> =
            chat.read_with(h.vcx, |c, _| c.expanded_tool_ids.iter().cloned().collect());
        assert!(ids.is_empty());
    }

    #[test]
    fn toggle_focused_expansion_flips_focused_card() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        push_tool_use(&chat, &mut h, "toolu_a", "Bash");
        chat.update(h.vcx, |c, cx| {
            c.focus_next_tool_card(cx);
            c.toggle_focused_expansion(cx);
        });
        let ids: Vec<String> =
            chat.read_with(h.vcx, |c, _| c.expanded_tool_ids.iter().cloned().collect());
        assert_eq!(ids, vec!["toolu_a".to_string()]);
    }

    #[test]
    fn dispatch_focus_next_routes_to_focused_chat() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        push_tool_use(&chat, &mut h, "toolu_a", "Bash");
        h.workspace.update_in(h.vcx, |w, _window, cx| {
            dispatch_claude_focus_next_tool_card(w, cx);
        });

        let focused = chat.read_with(h.vcx, |c, _| c.focused_tool_id.clone());
        assert_eq!(focused.as_deref(), Some("toolu_a"));
    }

    #[test]
    fn dispatch_toggle_expand_routes_to_focused_chat() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        push_tool_use(&chat, &mut h, "toolu_a", "Bash");
        chat.update(h.vcx, |c, cx| c.focus_next_tool_card(cx));
        h.workspace.update_in(h.vcx, |w, _window, cx| {
            dispatch_claude_toggle_tool_card_expand(w, cx);
        });

        let ids: Vec<String> =
            chat.read_with(h.vcx, |c, _| c.expanded_tool_ids.iter().cloned().collect());
        assert_eq!(ids, vec!["toolu_a".to_string()]);
    }

    #[test]
    fn recv_partial_text_is_ignored() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let fake_session = host.push_session(stoat::host::fake::FakeClaudeCode::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        fake_session.push_partial_text("streaming chunk");
        h.vcx.run_until_parked();

        let count = chat.read_with(h.vcx, |c, _| c.messages.len());
        assert_eq!(
            count, 0,
            "streaming projection is out of scope for the v1 recv loop"
        );
    }

    #[test]
    fn recv_stops_when_session_disconnects() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let fake_session = host.push_session(stoat::host::fake::FakeClaudeCode::new());
        fake_session.disconnect_on_recv(0);
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let _chat = open_chat(&mut h);

        // The recv loop should drain to completion without panicking.
        // No assertion on messages; this is a liveness check.
        h.vcx.run_until_parked();
    }

    fn seed_repo_with_sha(git: &FakeGit, workdir: &str, sha: &str) {
        let workdir = PathBuf::from(workdir);
        git.add_repo(&workdir).commit(sha, &[]);
    }

    fn push_user_text_with_checkpoint(
        chat: &Entity<ClaudeChat>,
        h: &mut Harness<'_>,
        text: &str,
        sha: Option<&str>,
    ) {
        chat.update(h.vcx, |c, cx| {
            c.push_message(
                ChatMessage {
                    role: ChatRole::User,
                    content: ChatMessageContent::Text(text.into()),
                    checkpoint_sha: sha.map(String::from),
                },
                cx,
            );
        });
    }

    #[test]
    fn restorable_user_message_click_invokes_restore_tree() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h =
            new_harness_with_root(&mut cx, host as Arc<dyn ClaudeCodeHost>, "/checkpoint-repo");
        seed_repo_with_sha(&h.git, "/checkpoint-repo", "stashsha");

        let chat = open_chat(&mut h);
        push_user_text_with_checkpoint(&chat, &mut h, "edit before submit", Some("stashsha"));

        chat.update(h.vcx, |c, cx| {
            let workspace = c.workspace.upgrade().expect("workspace live");
            workspace.update(cx, |w, cx| w.restore_to_checkpoint("stashsha".into(), cx));
        });

        let restored = h.git.restored_shas(&PathBuf::from("/checkpoint-repo"));
        assert_eq!(restored, vec!["stashsha".to_string()]);
    }

    #[test]
    fn non_restorable_user_message_has_no_marker() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h =
            new_harness_with_root(&mut cx, host as Arc<dyn ClaudeCodeHost>, "/no-checkpoint");
        let chat = open_chat(&mut h);
        push_user_text_with_checkpoint(&chat, &mut h, "clean tree submit", None);

        let marker: Vec<String> = chat.read_with(h.vcx, |c, _| {
            c.messages
                .iter()
                .filter_map(|m| restorable_sha(m).map(str::to_string))
                .collect()
        });
        assert!(marker.is_empty(), "no marker for None checkpoint_sha");
    }

    #[test]
    fn assistant_message_with_checkpoint_field_has_no_marker() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        chat.update(h.vcx, |c, cx| {
            c.push_message(
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: ChatMessageContent::Text("assistant reply".into()),
                    checkpoint_sha: Some("ignored".into()),
                },
                cx,
            );
        });

        let marker: Vec<String> = chat.read_with(h.vcx, |c, _| {
            c.messages
                .iter()
                .filter_map(|m| restorable_sha(m).map(str::to_string))
                .collect()
        });
        assert!(
            marker.is_empty(),
            "assistant role excludes the checkpoint marker",
        );
    }

    #[test]
    fn workspace_restore_to_checkpoint_calls_git_host() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let h = new_harness_with_root(&mut cx, host as Arc<dyn ClaudeCodeHost>, "/ws-restore");
        seed_repo_with_sha(&h.git, "/ws-restore", "deadbeef");

        h.workspace.update_in(h.vcx, |w, _window, cx| {
            w.restore_to_checkpoint("deadbeef".into(), cx);
        });

        let restored = h.git.restored_shas(&PathBuf::from("/ws-restore"));
        assert_eq!(restored, vec!["deadbeef".to_string()]);
    }
}
