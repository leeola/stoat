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
    div, AnyElement, App, AppContext, Context, IntoElement, ParentElement, Render, SharedString,
    Styled, Task, Window,
};
use std::sync::Arc;
use stoat::{
    claude_chat::{ChatMessage, ChatMessageContent, ChatRole},
    host::{AgentMessage, ClaudeCodeHost, ClaudeCodeSession},
};

pub struct ClaudeChat {
    input: gpui::Entity<Editor>,
    messages: Vec<ChatMessage>,
    session: Option<Arc<dyn ClaudeCodeSession>>,
    /// User submissions that arrived before the host returned a live
    /// session. Drained in arrival order once [`Self::session`] is
    /// installed; cleared mid-drain on the first send failure (the
    /// failure is logged at `tracing::warn` and the queue is dropped
    /// rather than retried).
    pending_sends: Vec<String>,
    /// Kept alive on the entity so the background task that asks the
    /// host for a session is dropped when the chat is dropped.
    _create_task: Option<Task<()>>,
}

impl ClaudeChat {
    pub fn new(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let input = cx.new(|cx| Editor::auto_height(1, 8, window, cx));
        let host = cx.global::<ClaudeCodeHostGlobal>().0.clone();
        let create_task = cx.spawn(async move |this, cx| {
            install_session(host, this, cx).await;
        });
        Self {
            input,
            messages: Vec::new(),
            session: None,
            pending_sends: Vec::new(),
            _create_task: Some(create_task),
        }
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

/// Per-message recv dispatch. Handles the two variants this v1 file
/// owns -- assistant `Text` (trimmed; empty drops) and `Error` --
/// and silently ignores every other variant. Sibling items hook in
/// their own variants (tool cards, usage header, ...) without
/// touching the existing arms here.
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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        let mut scrollback = div().flex().flex_col().flex_grow().w_full();
        for message in &self.messages {
            scrollback = scrollback.child(render_message_row(message));
        }
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(scrollback)
            .child(self.input.clone())
    }
}

fn render_message_row(message: &ChatMessage) -> AnyElement {
    let text = message_text(message);
    let bubble = div().px_2().py_1().child(text);
    match message.role {
        ChatRole::User => div().flex().flex_row_reverse().w_full().child(bubble),
        ChatRole::Assistant => div().flex().w_full().child(bubble),
    }
    .into_any_element()
}

fn message_text(message: &ChatMessage) -> SharedString {
    match &message.content {
        ChatMessageContent::Text(text) => SharedString::from(text.clone()),
        ChatMessageContent::Thinking { text } => SharedString::from(format!("(thinking) {text}")),
        ChatMessageContent::ToolUse { name, input, .. } => {
            SharedString::from(format!("{name}({input})"))
        },
        ChatMessageContent::ToolResult { content, .. } => SharedString::from(content.clone()),
        ChatMessageContent::Error(err) => SharedString::from(format!("error: {err}")),
        ChatMessageContent::TurnComplete { .. } => SharedString::from("(turn complete)"),
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
    let chat = cx.new(|cx| ClaudeChat::new(window, cx));
    pane.update(cx, |p, cx| {
        p.add_item(Box::new(chat), cx);
    });
}

/// Dispatch the [`stoat_action::ClaudeSubmit`] action. Finds the
/// focused pane's active item, downcasts to [`ClaudeChat`], and
/// invokes `submit` on it. No-op when the active item is not a
/// chat.
pub fn dispatch_claude_submit(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    let pane_id = workspace.pane_tree().read(cx).focus();
    let Some(pane) = workspace.pane_tree().read(cx).pane(pane_id).cloned() else {
        return;
    };
    let active_view = pane.read(cx).active_item().map(ItemHandle::to_any_view);
    let Some(chat) = active_view.and_then(|v| v.downcast::<ClaudeChat>().ok()) else {
        return;
    };
    chat.update(cx, |c, cx| c.submit(cx));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        globals::{
            ClaudeCodeHostGlobal, ClipboardHostGlobal, ExecutorGlobal, FsHostGlobal,
            FsWatchHostGlobal,
        },
        workspace::Workspace,
    };
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat::host::{
        fake::{FakeClipboard, FakeFs},
        ClipboardHost, FsHost, FsWatchHost,
    };
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext, host: Arc<dyn ClaudeCodeHost>) {
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
        });
    }

    struct Harness<'a> {
        workspace: Entity<Workspace>,
        vcx: &'a mut VisualTestContext,
    }

    fn new_harness<'a>(cx: &'a mut TestAppContext, host: Arc<dyn ClaudeCodeHost>) -> Harness<'a> {
        install_globals(cx, host);
        let (workspace, vcx) =
            cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/repo"), cx));
        Harness { workspace, vcx }
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
    fn recv_tool_use_is_ignored() {
        let mut cx = TestAppContext::single();
        let host = Arc::new(stoat::host::fake::FakeClaudeCodeHost::new());
        let fake_session = host.push_session(stoat::host::fake::FakeClaudeCode::new());
        let mut h = new_harness(&mut cx, host as Arc<dyn ClaudeCodeHost>);
        let chat = open_chat(&mut h);

        fake_session.push_tool_use("Bash", "{\"cmd\":\"ls\"}");
        h.vcx.run_until_parked();

        let count = chat.read_with(h.vcx, |c, _| c.messages.len());
        assert_eq!(count, 0, "tool_use is owned by the tool-card sibling item");
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
}
