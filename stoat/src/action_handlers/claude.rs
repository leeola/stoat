use crate::{
    action_handlers::{file::open_file_in_pane, movement::set_cursor_row},
    app::{Stoat, UpdateEffect},
    editor_state::EditorState,
    host::{ToolCallLocation, ToolKind},
    pane::{Axis, DockSide, DockVisibility, FocusTarget, PaneId, View},
    workspace::WorkspaceId,
};
use std::path::PathBuf;

pub(super) fn open_claude(stoat: &mut Stoat) -> UpdateEffect {
    use stoat_config::ClaudePlacement;

    if let Some(effect) = focus_existing_claude(stoat) {
        return effect;
    }

    let session_id = create_claude_session(stoat);

    let placement = stoat
        .settings
        .claude_default_placement
        .unwrap_or(ClaudePlacement::Pane);
    match placement {
        ClaudePlacement::Pane => place_claude_in_pane(stoat, session_id),
        ClaudePlacement::DockLeft => place_claude_in_dock(stoat, session_id, DockSide::Left),
        ClaudePlacement::DockRight => place_claude_in_dock(stoat, session_id, DockSide::Right),
    }

    stoat.mode = "prompt".into();
    UpdateEffect::Redraw
}

fn focus_existing_claude(stoat: &mut Stoat) -> Option<UpdateEffect> {
    let ws = stoat.active_workspace_mut();

    let pane_match = ws
        .panes
        .split_panes()
        .find(|(_, p)| matches!(&p.view, View::Claude(_)))
        .map(|(id, _)| id);
    if let Some(pid) = pane_match {
        ws.panes.set_focus(pid);
        ws.focus = FocusTarget::SplitPane(pid);
        stoat.mode = "normal".into();
        return Some(UpdateEffect::Redraw);
    }

    for (dock_id, dock) in &mut ws.docks {
        if matches!(&dock.view, View::Claude(_)) {
            if matches!(dock.visibility, DockVisibility::Hidden) {
                dock.visibility = DockVisibility::Open {
                    width: dock.default_width,
                };
            }
            ws.focus = FocusTarget::Dock(dock_id);
            stoat.mode = "normal".into();
            return Some(UpdateEffect::Redraw);
        }
    }

    None
}

fn create_claude_session(stoat: &mut Stoat) -> crate::host::ClaudeSessionId {
    use crate::{
        claude_chat::ClaudeChatState,
        input_view::{InputView, SubmitTarget},
    };

    let session_id = stoat.claude_sessions_mut().reserve_slot();
    let _ = stoat
        .claude_tx
        .try_send(crate::host::ClaudeNotification::CreateRequested { session_id });

    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    ws.claude_chat = Some(session_id);

    let input = InputView::create(
        ws,
        executor,
        SubmitTarget::ClaudeChat,
        "",
        "prompt",
        u16::MAX,
    );

    ws.chats.insert(
        session_id,
        ClaudeChatState {
            session_id,
            input,
            messages: Vec::new(),
            streaming_text: None,
            scroll_offset: 0,
            pending_sends: Vec::new(),
            active_since: None,
            protocol_session_id: None,
            follow: false,
            usage: crate::host::TokenUsage::default(),
            cancelled_tool_uses: std::collections::HashSet::new(),
            focused_tool_id: None,
            expanded_tool_ids: std::collections::HashSet::new(),
            layout_cache: std::cell::RefCell::default(),
        },
    );

    session_id
}

fn place_claude_in_pane(stoat: &mut Stoat, session_id: crate::host::ClaudeSessionId) {
    let ws = stoat.active_workspace_mut();
    let pid = ws.panes.focus();
    ws.panes.pane_mut(pid).view = View::Claude(session_id);
    ws.focus = FocusTarget::SplitPane(pid);
}

fn place_claude_in_dock(
    stoat: &mut Stoat,
    session_id: crate::host::ClaudeSessionId,
    side: DockSide,
) {
    use crate::pane::DockPanel;
    let ws = stoat.active_workspace_mut();
    let dock_id = ws.docks.insert(DockPanel {
        view: View::Claude(session_id),
        side,
        visibility: DockVisibility::Open { width: 40 },
        default_width: 40,
        area: ratatui::layout::Rect::default(),
    });
    ws.focus = FocusTarget::Dock(dock_id);
}

pub(super) fn claude_to_pane(stoat: &mut Stoat) -> UpdateEffect {
    let Some(session_id) = stoat.active_workspace().claude_chat else {
        return UpdateEffect::None;
    };

    {
        let ws = stoat.active_workspace_mut();
        let existing = ws
            .panes
            .split_panes()
            .find(|(_, p)| matches!(&p.view, View::Claude(id) if *id == session_id))
            .map(|(id, _)| id);
        if let Some(pid) = existing {
            ws.panes.set_focus(pid);
            ws.focus = FocusTarget::SplitPane(pid);
            return UpdateEffect::Redraw;
        }
    }

    remove_claude_from_docks(stoat, session_id);
    place_claude_in_pane(stoat, session_id);
    UpdateEffect::Redraw
}

pub(super) fn claude_to_dock(stoat: &mut Stoat, side: DockSide) -> UpdateEffect {
    let Some(session_id) = stoat.active_workspace().claude_chat else {
        return UpdateEffect::None;
    };

    {
        let ws = stoat.active_workspace_mut();
        let existing = ws
            .docks
            .iter()
            .find(|(_, d)| matches!(&d.view, View::Claude(id) if *id == session_id))
            .map(|(id, _)| id);
        if let Some(did) = existing {
            if let Some(dock) = ws.docks.get_mut(did) {
                dock.side = side;
                if matches!(dock.visibility, DockVisibility::Hidden) {
                    dock.visibility = DockVisibility::Open {
                        width: dock.default_width,
                    };
                }
            }
            ws.focus = FocusTarget::Dock(did);
            return UpdateEffect::Redraw;
        }
    }

    remove_claude_from_panes(stoat, session_id);
    place_claude_in_dock(stoat, session_id, side);
    UpdateEffect::Redraw
}

fn remove_claude_from_docks(stoat: &mut Stoat, session_id: crate::host::ClaudeSessionId) {
    let ws = stoat.active_workspace_mut();
    let dids: Vec<_> = ws
        .docks
        .iter()
        .filter(|(_, d)| matches!(&d.view, View::Claude(id) if *id == session_id))
        .map(|(id, _)| id)
        .collect();
    for did in dids {
        ws.docks.remove(did);
    }
}

fn remove_claude_from_panes(stoat: &mut Stoat, session_id: crate::host::ClaudeSessionId) {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let pids: Vec<_> = ws
        .panes
        .split_panes()
        .filter(|(_, p)| matches!(&p.view, View::Claude(id) if *id == session_id))
        .map(|(id, _)| id)
        .collect();
    for pid in pids {
        if !ws.panes.close(pid) {
            let (bid, buffer) = ws.buffers.new_scratch();
            let eid = ws
                .editors
                .insert(EditorState::new(bid, buffer, executor.clone()));
            ws.panes.pane_mut(pid).view = View::Editor(eid);
        }
    }
}

pub(super) fn claude_submit(stoat: &mut Stoat) -> UpdateEffect {
    use crate::claude_chat::{ChatMessage, ChatMessageContent, ChatRole};

    let session_id = match stoat.active_workspace().claude_chat {
        Some(id) => id,
        None => return UpdateEffect::None,
    };

    let text = {
        let ws = stoat.active_workspace();
        let chat = match ws.chats.get(&session_id) {
            Some(c) => c,
            None => return UpdateEffect::None,
        };
        let buffer = match ws.buffers.get(chat.input.buffer_id) {
            Some(b) => b,
            None => return UpdateEffect::None,
        };
        let guard = buffer.read().expect("buffer poisoned");
        guard.snapshot.visible_text.to_string()
    };
    if text.trim().is_empty() {
        return UpdateEffect::None;
    }

    let now = stoat.executor.now();
    let checkpoint_sha = stoat
        .git_host
        .discover(&stoat.active_workspace().git_root)
        .and_then(|repo| repo.stash_create());
    {
        let ws = stoat.active_workspace_mut();
        let Some(chat) = ws.chats.get_mut(&session_id) else {
            return UpdateEffect::None;
        };
        chat.messages.push(ChatMessage {
            role: ChatRole::User,
            content: ChatMessageContent::Text(text.clone()),
            checkpoint_sha,
        });
        chat.active_since = Some(now);

        let Some(buffer) = ws.buffers.get(chat.input.buffer_id) else {
            return UpdateEffect::None;
        };
        {
            let len = buffer.read().expect("poisoned").snapshot.visible_text.len();
            buffer.write().expect("poisoned").edit(0..len, "");
        }
        let Some(editor) = ws.editors.get_mut(chat.input.editor_id) else {
            return UpdateEffect::None;
        };
        editor.selections = crate::selection::SelectionsCollection::new();
    }

    if let Some(host) = stoat.claude_sessions().get(session_id) {
        let host = host.clone();
        stoat
            .executor
            .spawn(async move {
                if let Err(e) = host.send(&text).await {
                    tracing::error!("claude send error: {e}");
                }
            })
            .detach();
    } else {
        let ws = stoat.active_workspace_mut();
        if let Some(chat) = ws.chats.get_mut(&session_id) {
            chat.pending_sends.push(text);
        }
    }

    UpdateEffect::Redraw
}

pub(super) fn toggle_claude_follow(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(session_id) = ws.claude_chat else {
        return UpdateEffect::None;
    };
    let Some(chat) = ws.chats.get_mut(&session_id) else {
        return UpdateEffect::None;
    };
    chat.follow = !chat.follow;
    UpdateEffect::Redraw
}

/// Cancel the in-flight Claude turn. Marks every pending tool-use
/// (a `ToolUse` without a matching `ToolResult` later in the
/// transcript) as cancelled in the chat scrollback so the
/// `(cancelled)` badge renders, then dispatches `interrupt` on the
/// active session over the control protocol. Silent no-op when no
/// claude chat is active. Interrupt failures log warn but the chat
/// state still records the cancellation.
pub(super) fn claude_interrupt(stoat: &mut Stoat) -> UpdateEffect {
    use crate::claude_chat::ChatMessageContent;

    let session_id = match stoat.active_workspace().claude_chat {
        Some(id) => id,
        None => return UpdateEffect::None,
    };
    let in_flight: Vec<String> = {
        let ws = stoat.active_workspace();
        let Some(chat) = ws.chats.get(&session_id) else {
            return UpdateEffect::None;
        };
        let mut completed: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for msg in &chat.messages {
            if let ChatMessageContent::ToolResult { id, .. } = &msg.content {
                completed.insert(id.as_str());
            }
        }
        chat.messages
            .iter()
            .filter_map(|msg| match &msg.content {
                ChatMessageContent::ToolUse { id, .. } if !completed.contains(id.as_str()) => {
                    Some(id.clone())
                },
                _ => None,
            })
            .collect()
    };
    let session = stoat.claude_sessions().get(session_id).cloned();
    let ws = stoat.active_workspace_mut();
    let Some(chat) = ws.chats.get_mut(&session_id) else {
        return UpdateEffect::None;
    };
    for id in in_flight {
        chat.cancelled_tool_uses.insert(id);
    }
    chat.active_since = None;
    if let Some(session) = session {
        stoat
            .executor
            .spawn(async move {
                if let Err(err) = session.interrupt().await {
                    tracing::warn!(target: "stoat::claude", ?err, "claude interrupt failed");
                }
            })
            .detach();
    }
    UpdateEffect::Redraw
}

/// Move card focus to the next-older tool call in the chat scrollback.
/// Engages focus on the most-recent card when none is set; wraps to
/// the most recent after the oldest. Silent no-op when no chat is
/// active or no tool calls exist.
pub(super) fn claude_focus_next_tool_card(stoat: &mut Stoat) -> UpdateEffect {
    move_tool_card_focus(stoat, FocusMove::Older)
}

/// Mirror of [`claude_focus_next_tool_card`] that walks toward the
/// most-recent card.
pub(super) fn claude_focus_prev_tool_card(stoat: &mut Stoat) -> UpdateEffect {
    move_tool_card_focus(stoat, FocusMove::Newer)
}

/// Inspect the focused tool card and return the file path it
/// references, plus the 1-based line number to jump to when one is
/// encoded in the input. Returns `None` when no chat is active, no
/// card is focused, the focused message is not a `ToolUse`, the
/// input is not parseable JSON, or no `file_path` field is present.
pub(crate) fn focused_tool_card_location(stoat: &Stoat) -> Option<(PathBuf, Option<u32>)> {
    use crate::claude_chat::ChatMessageContent;
    let session_id = stoat.active_workspace().claude_chat?;
    let chat = stoat.active_workspace().chats.get(&session_id)?;
    let focused = chat.focused_tool_id.as_deref()?;
    let input = chat.messages.iter().find_map(|msg| match &msg.content {
        ChatMessageContent::ToolUse { id, input, .. } if id == focused => Some(input.as_str()),
        _ => None,
    })?;
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    let obj = value.as_object()?;
    let path = obj.get("file_path").and_then(|v| v.as_str())?;
    let line = obj
        .get("offset")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok());
    Some((PathBuf::from(path), line))
}

/// Open the file referenced by the focused tool card in an editor
/// pane and move the cursor to the referenced line. Silent no-op
/// when the focused card has no `file_path` in its tool input or
/// when the path resolves outside the active workspace's git root.
pub(super) fn claude_jump_to_focused_card(stoat: &mut Stoat) -> UpdateEffect {
    use crate::host::ToolKind;
    let (path, line) = match focused_tool_card_location(stoat) {
        Some(loc) => loc,
        None => return UpdateEffect::None,
    };
    let wid = stoat.active_workspace;
    let location = ToolCallLocation { path, line };
    handle_follow_tool_use(stoat, wid, ToolKind::Read, std::slice::from_ref(&location));
    UpdateEffect::Redraw
}

/// Toggle expansion of the focused tool card. Silent no-op when no
/// chat is active or no card is focused.
pub(super) fn claude_toggle_tool_card_expand(stoat: &mut Stoat) -> UpdateEffect {
    let Some(session_id) = stoat.active_workspace().claude_chat else {
        return UpdateEffect::None;
    };
    let ws = stoat.active_workspace_mut();
    let Some(chat) = ws.chats.get_mut(&session_id) else {
        return UpdateEffect::None;
    };
    let Some(focused) = chat.focused_tool_id.clone() else {
        return UpdateEffect::None;
    };
    if !chat.expanded_tool_ids.remove(&focused) {
        chat.expanded_tool_ids.insert(focused);
    }
    UpdateEffect::Redraw
}

#[derive(Copy, Clone)]
enum FocusMove {
    /// Toward older messages (Tab).
    Older,
    /// Toward newer messages (Shift-Tab).
    Newer,
}

fn move_tool_card_focus(stoat: &mut Stoat, dir: FocusMove) -> UpdateEffect {
    use crate::claude_chat::ChatMessageContent;
    let Some(session_id) = stoat.active_workspace().claude_chat else {
        return UpdateEffect::None;
    };
    let ws = stoat.active_workspace_mut();
    let Some(chat) = ws.chats.get_mut(&session_id) else {
        return UpdateEffect::None;
    };
    let tool_ids: Vec<String> = chat
        .messages
        .iter()
        .filter_map(|msg| match &msg.content {
            ChatMessageContent::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    if tool_ids.is_empty() {
        return UpdateEffect::None;
    }
    let next = match (chat.focused_tool_id.as_deref(), dir) {
        (None, FocusMove::Older) => Some(tool_ids[tool_ids.len() - 1].clone()),
        (None, FocusMove::Newer) => Some(tool_ids[0].clone()),
        (Some(current), dir) => {
            let pos = tool_ids.iter().position(|id| id == current);
            match (pos, dir) {
                (Some(0), FocusMove::Older) => Some(tool_ids[tool_ids.len() - 1].clone()),
                (Some(idx), FocusMove::Older) => Some(tool_ids[idx - 1].clone()),
                (Some(idx), FocusMove::Newer) if idx + 1 == tool_ids.len() => {
                    Some(tool_ids[0].clone())
                },
                (Some(idx), FocusMove::Newer) => Some(tool_ids[idx + 1].clone()),
                (None, FocusMove::Older) => Some(tool_ids[tool_ids.len() - 1].clone()),
                (None, FocusMove::Newer) => Some(tool_ids[0].clone()),
            }
        },
    };
    chat.focused_tool_id = next;
    UpdateEffect::Redraw
}

/// Open the per-message checkpoint picker over the active claude
/// chat. Silent no-op when there is no claude chat in the active
/// workspace, no chat state for the chosen session, or zero
/// restorable checkpoints among the messages.
pub(super) fn open_checkpoint_picker(stoat: &mut Stoat) -> UpdateEffect {
    use crate::claude_checkpoint_picker::CheckpointPicker;
    let session_id = match stoat.active_workspace().claude_chat {
        Some(id) => id,
        None => return UpdateEffect::None,
    };
    let messages = match stoat.active_workspace().chats.get(&session_id) {
        Some(chat) => chat.messages.clone(),
        None => return UpdateEffect::None,
    };
    let picker = CheckpointPicker::new(&messages);
    if picker.entries().is_empty() {
        return UpdateEffect::None;
    }
    stoat.pending_checkpoint_picker = Some(picker);
    UpdateEffect::Redraw
}

/// Acts on a `ToolUse` whose chat has follow enabled: opens the tool's target
/// file in an editor pane of the chat's workspace and moves the cursor to
/// `loc.line` when present. Silent no-op when any guard fails (non-file tool
/// kind, missing location, workspace not active, path outside the workspace
/// cwd). If no editor pane exists, splits the focused pane and creates one.
pub(crate) fn handle_follow_tool_use(
    stoat: &mut Stoat,
    wid: WorkspaceId,
    kind: ToolKind,
    locations: &[ToolCallLocation],
) {
    if !matches!(kind, ToolKind::Read | ToolKind::Edit) {
        return;
    }
    let Some(loc) = locations.first() else {
        return;
    };

    if stoat.active_workspace != wid {
        return;
    }

    let Some((target_pane, absolute)) = resolve_follow_target(stoat, wid, &loc.path) else {
        return;
    };

    open_file_in_pane(stoat, target_pane, &absolute);

    if let Some(line) = loc.line {
        let row = line.saturating_sub(1);
        let ws = stoat.active_workspace_mut();
        let View::Editor(eid) = ws.panes.pane(target_pane).view else {
            return;
        };
        let Some(editor) = ws.editors.get_mut(eid) else {
            return;
        };
        let max_row = editor
            .display_map
            .snapshot()
            .buffer_snapshot()
            .rope()
            .max_point()
            .row;
        set_cursor_row(editor, row.min(max_row));
    }
}

fn resolve_follow_target(
    stoat: &mut Stoat,
    wid: WorkspaceId,
    path: &std::path::Path,
) -> Option<(PaneId, PathBuf)> {
    let absolute = {
        let ws = &stoat.workspaces[wid];
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            ws.git_root.join(path)
        };

        let canonical_abs = stoat
            .fs_host
            .canonicalize(&absolute)
            .unwrap_or_else(|_| absolute.clone());
        let canonical_root = stoat
            .fs_host
            .canonicalize(&ws.git_root)
            .unwrap_or_else(|_| ws.git_root.clone());
        if !canonical_abs.starts_with(&canonical_root) {
            return None;
        }

        let focused = ws.panes.focus();
        if matches!(ws.panes.pane(focused).view, View::Editor(_)) {
            return Some((focused, absolute));
        }

        if let Some(fallback) = ws
            .panes
            .split_panes()
            .find(|(_, p)| matches!(p.view, View::Editor(_)))
            .map(|(id, _)| id)
        {
            return Some((fallback, absolute));
        }

        absolute
    };

    let executor = stoat.executor.clone();
    let ws = &mut stoat.workspaces[wid];
    let prev_focus = ws.panes.focus();
    let new_pane = ws.panes.split(Axis::Vertical);
    let (bid, buffer) = ws.buffers.new_scratch();
    let eid = ws.editors.insert(EditorState::new(bid, buffer, executor));
    ws.panes.pane_mut(new_pane).view = View::Editor(eid);
    ws.panes.set_focus(prev_focus);
    Some((new_pane, absolute))
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::dispatch,
        claude_chat::{ChatMessageContent, ChatRole},
        test_harness::TestHarness,
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;
    use stoat_action::{ClaudeSubmit, OpenCheckpointPicker};

    fn write_input(h: &mut TestHarness, text: &str) {
        let session_id = h
            .stoat
            .active_workspace()
            .claude_chat
            .expect("claude chat open");
        let ws = h.stoat.active_workspace();
        let chat = ws.chats.get(&session_id).expect("chat state");
        let buffer = ws.buffers.get(chat.input.buffer_id).expect("input buffer");
        let len = buffer.read().expect("poisoned").snapshot.visible_text.len();
        buffer.write().expect("poisoned").edit(0..len, text);
    }

    fn last_user_message(h: &TestHarness) -> &crate::claude_chat::ChatMessage {
        let session_id = h
            .stoat
            .active_workspace()
            .claude_chat
            .expect("claude chat open");
        let chat = h
            .stoat
            .active_workspace()
            .chats
            .get(&session_id)
            .expect("chat state");
        chat.messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, ChatRole::User))
            .expect("user message")
    }

    #[test]
    fn claude_submit_captures_checkpoint_sha_when_workdir_dirty() {
        let mut h = TestHarness::with_size(80, 20);
        let workdir = PathBuf::from("/checkpoint-dirty");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        h.fake_git()
            .add_repo(&workdir)
            .modified("foo.rs", "old\n", "new\n");
        h.claude().open();

        write_input(&mut h, "hello claude");
        dispatch(&mut h.stoat, &ClaudeSubmit);

        let stashes = h.fake_git().stashes(&workdir);
        assert_eq!(stashes.len(), 1, "fake git captured exactly one stash");
        let msg = last_user_message(&h);
        assert!(matches!(&msg.content, ChatMessageContent::Text(t) if t == "hello claude"));
        assert_eq!(msg.checkpoint_sha.as_deref(), Some(stashes[0].as_str()));
    }

    #[test]
    fn claude_submit_skips_checkpoint_when_no_repo() {
        let mut h = TestHarness::with_size(80, 20);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/no-repo-here");
        h.claude().open();

        write_input(&mut h, "hello");
        dispatch(&mut h.stoat, &ClaudeSubmit);

        let msg = last_user_message(&h);
        assert!(matches!(&msg.content, ChatMessageContent::Text(t) if t == "hello"));
        assert_eq!(msg.checkpoint_sha, None);
    }

    #[test]
    fn claude_submit_skips_checkpoint_when_workdir_clean() {
        let mut h = TestHarness::with_size(80, 20);
        let workdir = PathBuf::from("/checkpoint-clean");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        h.fake_git().add_repo(&workdir);
        h.claude().open();

        write_input(&mut h, "hello");
        dispatch(&mut h.stoat, &ClaudeSubmit);

        let stashes = h.fake_git().stashes(&workdir);
        assert!(
            stashes.is_empty(),
            "clean workdir produced no stash: {stashes:?}",
        );
        let msg = last_user_message(&h);
        assert_eq!(msg.checkpoint_sha, None);
    }

    fn submit_message(h: &mut TestHarness, text: &str) {
        write_input(h, text);
        dispatch(&mut h.stoat, &ClaudeSubmit);
    }

    fn key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn open_checkpoint_picker_lists_user_messages_with_sha() {
        let mut h = TestHarness::with_size(80, 20);
        let workdir = PathBuf::from("/picker-list");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        h.fake_git()
            .add_repo(&workdir)
            .modified("foo.rs", "v1\n", "v2\n");
        h.claude().open();
        submit_message(&mut h, "first prompt");
        submit_message(&mut h, "second prompt");

        dispatch(&mut h.stoat, &OpenCheckpointPicker);

        let picker = h
            .stoat
            .pending_checkpoint_picker
            .as_ref()
            .expect("picker open");
        let entries = picker.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].label, "first prompt");
        assert_eq!(entries[1].label, "second prompt");
        assert_eq!(picker.selected(), 1, "default selection is latest entry");
    }

    #[test]
    fn open_checkpoint_picker_skips_when_no_checkpoints() {
        let mut h = TestHarness::with_size(80, 20);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/picker-empty");
        h.claude().open();

        dispatch(&mut h.stoat, &OpenCheckpointPicker);
        assert!(h.stoat.pending_checkpoint_picker.is_none());
    }

    #[test]
    fn checkpoint_picker_select_restores_via_git_host() {
        let mut h = TestHarness::with_size(80, 20);
        let workdir = PathBuf::from("/picker-select");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        h.fake_git()
            .add_repo(&workdir)
            .modified("foo.rs", "v1\n", "v2\n");
        h.claude().open();
        submit_message(&mut h, "snapshot one");
        submit_message(&mut h, "snapshot two");

        dispatch(&mut h.stoat, &OpenCheckpointPicker);
        let picker = h
            .stoat
            .pending_checkpoint_picker
            .as_ref()
            .expect("picker open");
        let expected_sha = picker.entries()[picker.selected()].sha.clone();

        h.stoat.update(key_event(KeyCode::Enter));

        assert!(h.stoat.pending_checkpoint_picker.is_none());
        assert_eq!(h.fake_git().restored_shas(&workdir), vec![expected_sha]);
    }

    #[test]
    fn checkpoint_picker_esc_closes_without_restore() {
        let mut h = TestHarness::with_size(80, 20);
        let workdir = PathBuf::from("/picker-esc");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        h.fake_git()
            .add_repo(&workdir)
            .modified("foo.rs", "v1\n", "v2\n");
        h.claude().open();
        submit_message(&mut h, "only prompt");

        dispatch(&mut h.stoat, &OpenCheckpointPicker);
        assert!(h.stoat.pending_checkpoint_picker.is_some());

        h.stoat.update(key_event(KeyCode::Esc));

        assert!(h.stoat.pending_checkpoint_picker.is_none());
        assert!(h.fake_git().restored_shas(&workdir).is_empty());
    }

    fn mouse_up_left(col: u16, row: u16) -> Event {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn user_message_screen_row(h: &mut TestHarness, msg_idx: usize) -> u16 {
        let _ = h.stoat.render();
        let session_id = h
            .stoat
            .active_workspace()
            .claude_chat
            .expect("claude chat open");
        let chat = h
            .stoat
            .active_workspace()
            .chats
            .get(&session_id)
            .expect("chat state");
        let pane_area = {
            let ws = h.stoat.active_workspace();
            let pane_id = ws.panes.focus();
            ws.panes.pane(pane_id).area
        };
        let input_lines = h
            .stoat
            .active_workspace()
            .buffers
            .get(chat.input.buffer_id)
            .map(|b| {
                let guard = b.read().expect("poisoned");
                guard.snapshot.visible_text.max_point().row as u16 + 1
            })
            .unwrap_or(1);
        let body_area =
            crate::render::claude_pane::chat_body_area(pane_area, input_lines).expect("body area");
        let layout = crate::render::claude_pane::build_chat_pane_layout(
            chat,
            body_area.width as usize,
            &h.stoat.theme,
            h.stoat.render_tick,
        );
        let visible_lines = body_area.height as usize;
        let skip = layout
            .lines
            .len()
            .saturating_sub(visible_lines + chat.scroll_offset);
        let display_count = layout.lines.len().saturating_sub(skip).min(visible_lines);
        let start_row = body_area.y + body_area.height.saturating_sub(display_count as u16);
        let (line_start, _) = layout
            .message_ranges
            .get(msg_idx)
            .expect("message exists")
            .expect("message renders to lines");
        let display_idx = line_start - skip;
        start_row + display_idx as u16
    }

    #[test]
    fn claude_pane_click_on_restorable_user_message_restores() {
        let mut h = TestHarness::with_size(80, 20);
        let workdir = PathBuf::from("/marker-click-restore");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        h.fake_git()
            .add_repo(&workdir)
            .modified("foo.rs", "v1\n", "v2\n");
        h.claude().open();
        submit_message(&mut h, "first prompt");
        submit_message(&mut h, "second prompt");

        let row = user_message_screen_row(&mut h, 0);
        let stashes = h.fake_git().stashes(&workdir);
        let expected_sha = stashes[0].clone();

        h.stoat.update(mouse_up_left(0, row));

        assert_eq!(h.fake_git().restored_shas(&workdir), vec![expected_sha]);
    }

    #[test]
    fn claude_pane_click_outside_message_rows_is_noop() {
        let mut h = TestHarness::with_size(80, 20);
        let workdir = PathBuf::from("/marker-click-empty");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        h.fake_git()
            .add_repo(&workdir)
            .modified("foo.rs", "v1\n", "v2\n");
        h.claude().open();
        submit_message(&mut h, "only prompt");

        let _ = h.stoat.render();
        h.stoat.update(mouse_up_left(0, 0));

        assert!(h.fake_git().restored_shas(&workdir).is_empty());
    }

    #[test]
    fn claude_pane_click_when_no_repo_is_noop() {
        let mut h = TestHarness::with_size(80, 20);
        let workdir = PathBuf::from("/marker-click-no-repo");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        h.fake_git()
            .add_repo(&workdir)
            .modified("foo.rs", "v1\n", "v2\n");
        h.claude().open();
        submit_message(&mut h, "only prompt");

        let row = user_message_screen_row(&mut h, 0);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/no-repo-here");
        h.stoat.update(mouse_up_left(0, row));

        assert!(h.fake_git().restored_shas(&workdir).is_empty());
    }

    #[test]
    fn claude_pane_click_on_user_message_without_checkpoint_is_noop() {
        let mut h = TestHarness::with_size(80, 20);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/marker-click-no-checkpoint");
        h.claude().open();
        submit_message(&mut h, "only prompt");

        let row = user_message_screen_row(&mut h, 0);
        h.stoat.update(mouse_up_left(0, row));

        assert!(h
            .fake_git()
            .restored_shas(&PathBuf::from("/marker-click-no-checkpoint"))
            .is_empty());
    }

    fn push_tool_use(h: &mut TestHarness, id: &str, name: &str) {
        use crate::claude_chat::{ChatMessage, ChatMessageContent, ChatRole};
        let session_id = h
            .stoat
            .active_workspace()
            .claude_chat
            .expect("claude chat open");
        let now = h.stoat.executor.now();
        let chat = h
            .stoat
            .active_workspace_mut()
            .chats
            .get_mut(&session_id)
            .expect("chat state");
        chat.messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: ChatMessageContent::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: "{}".to_string(),
            },
            checkpoint_sha: None,
        });
        chat.active_since = Some(now);
    }

    fn push_tool_result(h: &mut TestHarness, id: &str, content: &str) {
        use crate::claude_chat::{ChatMessage, ChatMessageContent, ChatRole};
        let session_id = h
            .stoat
            .active_workspace()
            .claude_chat
            .expect("claude chat open");
        let chat = h
            .stoat
            .active_workspace_mut()
            .chats
            .get_mut(&session_id)
            .expect("chat state");
        chat.messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: ChatMessageContent::ToolResult {
                id: id.to_string(),
                content: content.to_string(),
                status: crate::host::ToolCallStatus::Completed,
            },
            checkpoint_sha: None,
        });
    }

    fn cancelled_tool_uses(h: &TestHarness) -> std::collections::HashSet<String> {
        let session_id = h
            .stoat
            .active_workspace()
            .claude_chat
            .expect("claude chat open");
        h.stoat
            .active_workspace()
            .chats
            .get(&session_id)
            .expect("chat state")
            .cancelled_tool_uses
            .clone()
    }

    fn active_since(h: &TestHarness) -> Option<std::time::Instant> {
        let session_id = h
            .stoat
            .active_workspace()
            .claude_chat
            .expect("claude chat open");
        h.stoat
            .active_workspace()
            .chats
            .get(&session_id)
            .expect("chat state")
            .active_since
    }

    #[test]
    fn claude_interrupt_marks_in_flight_tool_uses_cancelled() {
        use stoat_action::ClaudeInterrupt;
        let mut h = TestHarness::with_size(80, 20);
        let id = h.claude().open();
        push_tool_use(&mut h, "tool-1", "Bash");

        dispatch(&mut h.stoat, &ClaudeInterrupt);
        h.settle();

        let cancelled = cancelled_tool_uses(&h);
        assert_eq!(cancelled.len(), 1);
        assert!(cancelled.contains("tool-1"));
        assert_eq!(h.claude().get_session(id).interrupt_count(), 1);
    }

    #[test]
    fn claude_interrupt_skips_completed_tool_uses() {
        use stoat_action::ClaudeInterrupt;
        let mut h = TestHarness::with_size(80, 20);
        let _ = h.claude().open();
        push_tool_use(&mut h, "tool-done", "Bash");
        push_tool_result(&mut h, "tool-done", "ok");

        dispatch(&mut h.stoat, &ClaudeInterrupt);

        assert!(cancelled_tool_uses(&h).is_empty());
    }

    #[test]
    fn claude_interrupt_clears_active_since() {
        use stoat_action::ClaudeInterrupt;
        let mut h = TestHarness::with_size(80, 20);
        let _ = h.claude().open();
        push_tool_use(&mut h, "tool-active", "Bash");
        assert!(active_since(&h).is_some());

        dispatch(&mut h.stoat, &ClaudeInterrupt);

        assert_eq!(active_since(&h), None);
    }

    #[test]
    fn claude_interrupt_no_active_session_is_noop() {
        use stoat_action::ClaudeInterrupt;
        let mut h = TestHarness::with_size(80, 20);

        let effect = dispatch(&mut h.stoat, &ClaudeInterrupt);
        assert_eq!(effect, crate::app::UpdateEffect::None);
    }

    #[test]
    fn chat_pane_renders_cancelled_badge_for_in_flight_tool_use() {
        use stoat_action::ClaudeInterrupt;
        let mut h = TestHarness::with_size(80, 20);
        let _ = h.claude().open();
        push_tool_use(&mut h, "tool-cancel", "Bash");

        dispatch(&mut h.stoat, &ClaudeInterrupt);

        let frame = h.snapshot();
        assert!(
            frame.content.contains("cancelled"),
            "expected cancelled status badge on tool-use row: {}",
            frame.content,
        );
    }
}
