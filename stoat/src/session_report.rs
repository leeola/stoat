//! JSON reports of live session state for the `stoat api` clients, built on
//! the event loop.

use crate::{
    app::Stoat,
    display_map::DisplayPoint,
    keymap_state,
    pane::{DockSide, FocusTarget},
    workspace::{self, Workspace, WorkspaceUid},
};
use serde_json::{json, Value};
use std::{path::Path, process, time::UNIX_EPOCH};
use stoat_text::Point;

/// The `session-info` report on the workspace that `uid` names.
///
/// It names the session, where it is rooted, whether it is on screen, when it
/// was last active, and the file in its focused pane. The `uid` field is the
/// hex form that `STOAT_SESSION` carries. An unknown `uid` reports an error
/// object.
pub(crate) fn session_info(stoat: &Stoat, uid: WorkspaceUid) -> Value {
    let Some((id, ws)) = stoat.workspaces.iter().find(|(_, ws)| ws.uid == uid) else {
        return json!({ "error": "unknown session" });
    };

    let focus = ws
        .focused_editor()
        .and_then(|(_, buffer)| ws.buffers.path_for(buffer));
    let last_active_ms = ws
        .last_active
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);

    json!({
        "uid": uid.to_string(),
        "pid": process::id(),
        "name": ws.display_name(),
        "git_root": ws.git_root,
        "repo_root": ws.repo_root,
        "active": id == stoat.active_workspace,
        "last_active_ms": last_active_ms,
        "tab": ws.active_tab,
        "tabs": ws.tabs.len(),
        "workspaces": stoat.workspaces.len(),
        "focus": focus,
    })
}

/// The `focus` report on what has focus in the workspace that `uid` names, and
/// on what its focused editor shows.
///
/// A modal belongs to the process rather than to a workspace, so only the
/// workspace on screen reports one. `dock` names the side of a focused dock.
/// `editor` follows the focus into a dock, and it is null when the focused view
/// is no editor. With `with_text`, the editor report also carries the text of
/// its lines on screen. An unknown `uid` reports an error object.
pub(crate) fn focus(stoat: &mut Stoat, uid: WorkspaceUid, with_text: bool) -> Value {
    let Some(ws_id) = stoat
        .workspaces
        .iter()
        .find(|(_, ws)| ws.uid == uid)
        .map(|(id, _)| id)
    else {
        return json!({ "error": "unknown session" });
    };
    let active = ws_id == stoat.active_workspace;
    let modal = active
        .then(|| keymap_state::active_modal(stoat))
        .flatten()
        .map(|modal| modal.context_str());

    let ws = &mut stoat.workspaces[ws_id];
    let editor = editor_report(ws, with_text);
    let pane = ws.panes.pane(ws.panes.focus());
    let dock = match ws.focus {
        FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).map(|dock| match dock.side {
            DockSide::Left => "left",
            DockSide::Right => "right",
        }),
        FocusTarget::SplitPane => None,
    };

    json!({
        "workspace": {
            "uid": uid.to_string(),
            "name": ws.display_name(),
            "git_root": ws.git_root,
            "active": active,
        },
        "tab": {
            "index": ws.active_tab,
            "title": ws.tab_title(ws.active_tab),
            "count": ws.tabs.len(),
        },
        "pane": { "index": pane.index, "kind": workspace::view_kind(&pane.view) },
        "dock": dock,
        "modal": modal,
        "editor": editor,
    })
}

/// The focused editor's file, state, cursor, and lines on screen, or null when
/// the focused view is no editor.
///
/// Lines and columns are 1-based, and a column counts bytes. The cursor is the
/// block-cursor cell the status line reports. `visible` is null for an editor
/// that no paint has sized, such as one in a workspace not yet shown.
fn editor_report(ws: &mut Workspace, with_text: bool) -> Value {
    let Some((editor_id, buffer_id)) = ws.focused_editor() else {
        return Value::Null;
    };
    let path = ws.buffers.path_for(buffer_id).map(Path::to_path_buf);
    let relative_path = path
        .as_deref()
        .and_then(|path| path.strip_prefix(&ws.git_root).ok())
        .map(Path::to_path_buf);
    let dirty = ws
        .buffers
        .get(buffer_id)
        .is_some_and(|buffer| buffer.read().expect("buffer poisoned").dirty);
    let Some(editor) = ws.editors.get_mut(editor_id) else {
        return Value::Null;
    };

    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    let rope = buffer.rope();
    let cursor = {
        let selection = editor.selections.newest_anchor();
        let offset = stoat_text::cursor_offset(
            rope,
            buffer.resolve_anchor(&selection.tail()),
            buffer.resolve_anchor(&selection.head()),
        );
        let point = rope.offset_to_point(offset);
        json!({ "line": point.row + 1, "col": point.column + 1 })
    };

    // A block's own row maps to no buffer line, so each end scans in from its
    // screen edge to the nearest row that does.
    let visible = editor.viewport_rows.and_then(|rows| {
        let screen = editor.scroll_row..(editor.scroll_row + rows).min(snapshot.line_count());
        let buffer_row = |row: u32| {
            snapshot
                .display_to_buffer(DisplayPoint::new(row, 0))
                .map(|point| point.row)
        };
        Some((
            screen.clone().find_map(buffer_row)?,
            screen.rev().find_map(buffer_row)?,
        ))
    });

    let mut report = json!({
        "path": path,
        "relative_path": relative_path,
        "dirty": dirty,
        "mode": editor.mode,
        "pinned": editor.pinned,
        "diff_view": editor.diff_view,
        "line_count": rope.max_point().row + 1,
        "cursor": cursor,
        "visible": visible.map(|(first, last)| json!({ "first": first + 1, "last": last + 1 })),
    });
    if with_text {
        let line_start = |row: u32| {
            if row > rope.max_point().row {
                rope.len()
            } else {
                rope.point_to_offset(Point::new(row, 0))
            }
        };
        let text = visible.map(|(first, last)| {
            rope.slice(line_start(first)..line_start(last + 1))
                .to_string()
        });
        report["visible_text"] = json!(text);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_fixture::{open_buffer, seed},
        test_harness::TestHarness,
    };
    use std::time::Duration;

    /// A fresh harness with a 60-line `main.rs` open in its focused pane and
    /// no paint yet, with the session's uid and the file's lines.
    fn sixty_line_file() -> (TestHarness, WorkspaceUid, Vec<String>) {
        let mut h = TestHarness::with_size(80, 24);
        let lines: Vec<String> = (1..=60).map(|n| format!("line {n}")).collect();
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let root = seed(&mut h, &[("main.rs", text.as_str())]);
        open_buffer(&mut h, root.join("main.rs"));
        let uid = h.stoat.active_workspace().uid;
        (h, uid, lines)
    }

    #[test]
    fn session_info_names_the_workspace_and_its_focused_file() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("src/a.rs", "fn a() {}\n")]);
        open_buffer(&mut h, root.join("src/a.rs"));

        let ws = h.stoat.active_workspace();
        let last_active_ms = ws
            .last_active
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert_eq!(
            session_info(&h.stoat, ws.uid),
            json!({
                "uid": ws.uid.to_string(),
                "pid": process::id(),
                "name": "lsp-did-open-test",
                "git_root": "/lsp-did-open-test",
                "repo_root": null,
                "active": true,
                "last_active_ms": last_active_ms,
                "tab": 0,
                "tabs": 1,
                "workspaces": 1,
                "focus": "/lsp-did-open-test/src/a.rs",
            }),
        );
    }

    #[test]
    fn focus_reports_the_file_its_cursor_and_the_lines_on_screen() {
        let (mut h, uid, _) = sixty_line_file();
        h.type_keys("3 0 j");
        h.snapshot();

        let (scroll_row, rows) = {
            let ws = h.stoat.active_workspace();
            let (editor_id, _) = ws.focused_editor().expect("an editor has focus");
            let editor = &ws.editors[editor_id];
            let rows = editor.viewport_rows.expect("a paint sized the editor");
            (editor.scroll_row, rows)
        };
        assert_eq!(
            focus(&mut h.stoat, uid, false)["editor"],
            json!({
                "path": "/lsp-did-open-test/main.rs",
                "relative_path": "main.rs",
                "dirty": false,
                "mode": "normal",
                "pinned": false,
                "diff_view": false,
                "line_count": 61,
                "cursor": { "line": 31, "col": 1 },
                "visible": { "first": scroll_row + 1, "last": scroll_row + rows },
            }),
        );
    }

    #[test]
    fn focus_text_returns_exactly_the_visible_lines() {
        let (mut h, uid, lines) = sixty_line_file();
        h.type_keys("3 0 j");
        h.snapshot();

        let report = focus(&mut h.stoat, uid, true);
        let editor = &report["editor"];
        let line = |end: &str| editor["visible"][end].as_u64().expect("a visible line") as usize;
        let expected: String = lines[line("first") - 1..line("last")]
            .iter()
            .map(|line| format!("{line}\n"))
            .collect();
        assert_eq!(editor["visible_text"], expected);
    }

    #[test]
    fn focus_before_any_paint_reports_no_visible_lines() {
        let (mut h, uid, _) = sixty_line_file();

        let report = focus(&mut h.stoat, uid, true);
        assert_eq!(
            [
                &report["editor"]["visible"],
                &report["editor"]["visible_text"]
            ],
            [&Value::Null, &Value::Null],
        );
    }

    #[test]
    fn focus_with_the_palette_open_names_it() {
        let mut h = TestHarness::with_size(80, 24);
        h.type_keys(":");

        let uid = h.stoat.active_workspace().uid;
        assert_eq!(focus(&mut h.stoat, uid, false)["modal"], "palette");
    }

    #[test]
    fn focus_on_a_background_workspace_reports_inactive() {
        let mut h = TestHarness::with_size(80, 24);
        let background = h.stoat.active_workspace().uid;
        // WorkspaceUid::now reads the fake clock. Without this step, the new
        // workspace gets the same uid as the first one.
        h.advance_clock(Duration::from_secs(1));
        h.type_action("NewWorkspace()");
        h.type_keys(":");

        let report = focus(&mut h.stoat, background, false);
        assert_eq!(
            [&report["workspace"]["active"], &report["modal"]],
            [&json!(false), &Value::Null],
        );
    }
}
