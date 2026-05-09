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
}
