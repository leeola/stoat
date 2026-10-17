//! JSON reports of live session state for the `stoat api` clients, built on
//! the event loop.

use crate::{
    app::Stoat,
    buffer::BufferId,
    buffer_registry::OpenOrigin,
    display_map::DisplayPoint,
    keymap_state,
    pane::{DockSide, FocusTarget, View},
    workspace::{self, Workspace, WorkspaceId, WorkspaceUid},
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process,
    time::UNIX_EPOCH,
};
use stoat_text::{Bias, Point};

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
    let Some(ws_id) = workspace_id(stoat, uid) else {
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
        FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).map(|dock| dock_side_name(dock.side)),
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
    let (path, relative_path) = buffer_paths(ws, buffer_id);
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

/// The `selection` report on every selection of the focused editor in the
/// workspace that `uid` names.
///
/// Each selection gives `start` and `end` as inclusive points, where `end` is
/// the first byte of the last covered character, and `range` in the
/// walkthrough grammar, so a reported range feeds back into a walkthrough stop.
/// The inclusive grammar has no empty form, so a zero-width selection reports
/// `start`, `end`, and `cursor` at one point and sets `empty`. The selections
/// come in the editor's own order. With `with_text`, each one also carries the
/// bytes it covers. An unknown `uid`, or a focused view that is no editor,
/// reports an error object.
pub(crate) fn selection(stoat: &Stoat, uid: WorkspaceUid, with_text: bool) -> Value {
    let Some(ws) = workspace_id(stoat, uid).map(|id| &stoat.workspaces[id]) else {
        return json!({ "error": "unknown session" });
    };
    let Some((editor, buffer_id)) = ws
        .focused_editor()
        .and_then(|(editor_id, buffer_id)| Some((ws.editors.get(editor_id)?, buffer_id)))
    else {
        return json!({ "error": "no editor has focus" });
    };
    let (path, relative_path) = buffer_paths(ws, buffer_id);

    let buffer = editor.display_map.buffer_snapshot();
    let rope = buffer.rope();
    let position = |offset: usize| {
        let point = rope.offset_to_point(offset);
        (point.row + 1, point.column + 1)
    };
    let primary = editor.selections.primary_index();
    let selections: Vec<Value> = editor
        .selections
        .resolved_reads(&buffer)
        .iter()
        .enumerate()
        .map(|(index, read)| {
            let (start, end) = (read.head.min(read.tail), read.head.max(read.tail));
            let empty = start == end;
            let last = if empty {
                start
            } else {
                rope.clip_offset(end - 1, Bias::Left)
            };
            let cursor = stoat_text::cursor_offset(rope, read.tail, read.head);
            let [start_point, end_point, cursor_point] = [start, last, cursor].map(position);

            let mut report = json!({
                "primary": index == primary,
                "reversed": read.reversed,
                "empty": empty,
                "start": point_json(start_point),
                "end": point_json(end_point),
                "cursor": point_json(cursor_point),
                "range": format!(
                    "{}:{}-{}:{}",
                    start_point.0, start_point.1, end_point.0, end_point.1
                ),
            });
            if with_text {
                report["text"] = json!(rope.slice(start..end).to_string());
            }
            report
        })
        .collect();

    json!({
        "path": path,
        "relative_path": relative_path,
        "mode": editor.mode,
        "count": selections.len(),
        "selections": selections,
    })
}

/// The `buffers` report on every buffer of the workspace that `uid` names, and
/// on where each one shows.
///
/// The list holds every buffer the workspace has, including the ones a
/// navigation only passed through and scratch buffers, which report a null
/// `path`. Path-bound buffers come first, by path. `shown` names each tab and
/// split pane, then each dock, whose editor holds the buffer. An unknown `uid`
/// reports an error object.
pub(crate) fn buffers(stoat: &Stoat, uid: WorkspaceUid) -> Value {
    let Some(ws) = workspace_id(stoat, uid).map(|id| &stoat.workspaces[id]) else {
        return json!({ "error": "unknown session" });
    };

    let buffers: Vec<Value> = ws
        .buffers
        .summaries()
        .into_iter()
        .map(|summary| {
            let (path, relative_path) = buffer_paths(ws, summary.id);
            let origin = match summary.origin {
                OpenOrigin::Named => "named",
                OpenOrigin::Visited => "visited",
            };
            json!({
                "path": path,
                "relative_path": relative_path,
                "dirty": summary.dirty,
                "origin": origin,
                "preview": summary.preview,
                "read_only": summary.read_only,
                "shown": shown_in(ws, summary.id),
            })
        })
        .collect();
    json!({ "count": buffers.len(), "buffers": buffers })
}

/// Where `buffer_id` shows in `ws`, as each tab and split pane whose editor
/// holds it, in tab order, then each dock whose editor holds it.
fn shown_in(ws: &Workspace, buffer_id: BufferId) -> Vec<Value> {
    let holds = |view: &View| match view {
        View::Editor(id) => ws
            .editors
            .get(*id)
            .is_some_and(|editor| editor.buffer_id == buffer_id),
        _ => false,
    };

    let mut shown = Vec::new();
    for tab in 0..ws.tabs.len() {
        // The active tab's tree lives in `ws.panes`, and its own slot parks
        // nothing.
        let tree = if tab == ws.active_tab {
            Some(&ws.panes)
        } else {
            ws.tabs[tab].parked.as_ref()
        };
        let Some(tree) = tree else {
            continue;
        };
        for (_, pane) in tree.split_panes() {
            if holds(&pane.view) {
                shown.push(json!({ "tab": tab, "pane": pane.index }));
            }
        }
    }

    for dock in ws.docks.values() {
        if holds(&dock.view) {
            shown.push(json!({ "dock": dock_side_name(dock.side) }));
        }
    }
    shown
}

/// The id of the open workspace that `uid` names.
fn workspace_id(stoat: &Stoat, uid: WorkspaceUid) -> Option<WorkspaceId> {
    stoat
        .workspaces
        .iter()
        .find(|(_, ws)| ws.uid == uid)
        .map(|(id, _)| id)
}

/// The path of the file `buffer_id` holds, and that path below the workspace
/// root.
///
/// The first is `None` for a scratch buffer. The second is also `None` for a
/// file outside the root.
fn buffer_paths(ws: &Workspace, buffer_id: BufferId) -> (Option<PathBuf>, Option<PathBuf>) {
    let path = ws.buffers.path_for(buffer_id).map(Path::to_path_buf);
    let relative_path = path
        .as_deref()
        .and_then(|path| path.strip_prefix(&ws.git_root).ok())
        .map(Path::to_path_buf);
    (path, relative_path)
}

fn point_json((line, col): (u32, u32)) -> Value {
    json!({ "line": line, "col": col })
}

/// The name a report gives a dock on `side`.
fn dock_side_name(side: DockSide) -> &'static str {
    match side {
        DockSide::Left => "left",
        DockSide::Right => "right",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        action_handlers,
        pane::Axis,
        test_fixture::{open_buffer, seed},
        test_harness::TestHarness,
    };
    use std::time::Duration;
    use stoat_text::SelectionGoal;

    /// A fresh harness with `text` as `main.rs`, open in its focused pane and
    /// with no paint yet, and the session's uid.
    fn open_text(text: &str) -> (TestHarness, WorkspaceUid) {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", text)]);
        open_buffer(&mut h, root.join("main.rs"));
        let uid = h.stoat.active_workspace().uid;
        (h, uid)
    }

    /// [`open_text`] on a 60-line file, with the file's lines.
    fn sixty_line_file() -> (TestHarness, WorkspaceUid, Vec<String>) {
        let lines: Vec<String> = (1..=60).map(|n| format!("line {n}")).collect();
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let (h, uid) = open_text(&text);
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

    #[test]
    fn a_forward_selection_reports_its_first_and_last_byte() {
        let (mut h, uid) = open_text("abc\ndef\n");
        h.type_keys("v l l");

        assert_eq!(
            selection(&h.stoat, uid, false)["selections"],
            json!([{
                "primary": true,
                "reversed": false,
                "empty": false,
                "start": { "line": 1, "col": 1 },
                "end": { "line": 1, "col": 3 },
                "cursor": { "line": 1, "col": 3 },
                "range": "1:1-1:3",
            }]),
        );
    }

    #[test]
    fn a_reversed_selection_keeps_its_cursor_on_the_start() {
        let (mut h, uid) = open_text("abc\ndef\n");
        h.type_keys("l l v h h");

        let report = selection(&h.stoat, uid, false);
        let first = &report["selections"][0];
        assert_eq!(
            (&first["reversed"], &first["cursor"]),
            (&json!(true), &first["start"]),
        );
    }

    #[test]
    fn selection_text_returns_the_covered_bytes() {
        let (mut h, uid) = open_text("abc\ndef\n");
        h.type_keys("v l l");

        assert_eq!(
            selection(&h.stoat, uid, true)["selections"][0]["text"],
            "abc"
        );
    }

    #[test]
    fn every_cursor_is_reported_with_the_primary_marked() {
        let (mut h, uid) = open_text("abc\ndef\n");
        h.type_action("AddSelectionBelow()");

        let report = selection(&h.stoat, uid, false);
        let primaries: Vec<&Value> = report["selections"]
            .as_array()
            .expect("a selection list")
            .iter()
            .map(|selection| &selection["primary"])
            .collect();
        assert_eq!(
            (&report["count"], primaries),
            (&json!(2), vec![&json!(false), &json!(true)]),
        );
    }

    #[test]
    fn a_multibyte_column_counts_bytes() {
        let (mut h, uid) = open_text("\u{e9}a\n");
        h.type_keys("l");

        assert_eq!(
            selection(&h.stoat, uid, false)["selections"][0]["start"],
            json!({ "line": 1, "col": 3 }),
        );
    }

    #[test]
    fn a_zero_width_selection_reports_empty() {
        let (mut h, uid) = open_text("abc\ndef\n");
        {
            let editor =
                action_handlers::focused_editor_mut(&mut h.stoat).expect("an editor has focus");
            let anchor = editor
                .display_map
                .buffer_snapshot()
                .anchor_at(0, Bias::Left);
            editor
                .selections
                .set_single_range(anchor, anchor, false, SelectionGoal::None);
        }

        let report = selection(&h.stoat, uid, false);
        let only = &report["selections"][0];
        let origin = json!({ "line": 1, "col": 1 });
        assert_eq!(
            [
                &only["empty"],
                &only["start"],
                &only["end"],
                &only["cursor"]
            ],
            [&json!(true), &origin, &origin, &origin],
        );
    }

    #[test]
    fn buffers_lists_each_file_with_its_dirty_state_and_panes() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        h.stoat.active_workspace_mut().panes.split(Axis::Vertical);
        open_buffer(&mut h, root.join("b.rs"));
        h.edit_focused(0..0, "x");

        let uid = h.stoat.active_workspace().uid;
        assert_eq!(
            buffers(&h.stoat, uid)["buffers"],
            json!([
                {
                    "path": "/lsp-did-open-test/a.rs",
                    "relative_path": "a.rs",
                    "dirty": false,
                    "origin": "named",
                    "preview": false,
                    "read_only": false,
                    "shown": [{ "tab": 0, "pane": 0 }],
                },
                {
                    "path": "/lsp-did-open-test/b.rs",
                    "relative_path": "b.rs",
                    "dirty": true,
                    "origin": "named",
                    "preview": false,
                    "read_only": false,
                    "shown": [{ "tab": 0, "pane": 1 }],
                },
            ]),
        );
    }

    #[test]
    fn a_scratch_buffer_lists_with_no_path() {
        let h = TestHarness::with_size(80, 24);

        let uid = h.stoat.active_workspace().uid;
        assert_eq!(
            buffers(&h.stoat, uid),
            json!({
                "count": 1,
                "buffers": [{
                    "path": null,
                    "relative_path": null,
                    "dirty": false,
                    "origin": "visited",
                    "preview": false,
                    "read_only": false,
                    "shown": [{ "tab": 0, "pane": 0 }],
                }],
            }),
        );
    }
}
