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

    {
        let ws = stoat.active_workspace_mut();
        let Some(chat) = ws.chats.get_mut(&session_id) else {
            return UpdateEffect::None;
        };
        chat.messages.push(ChatMessage {
            role: ChatRole::User,
            content: ChatMessageContent::Text(text.clone()),
        });
        chat.active_since = Some(std::time::Instant::now());

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

        let canonical_abs = std::fs::canonicalize(&absolute).unwrap_or_else(|_| absolute.clone());
        let canonical_root =
            std::fs::canonicalize(&ws.git_root).unwrap_or_else(|_| ws.git_root.clone());
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
