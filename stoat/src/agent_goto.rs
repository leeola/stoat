//! Show a range an agent names in the user's editor.
//!
//! An agent that read or wrote a range points the user at it with
//! `stoat api goto`, and the range lands selected and framed in an editor pane.
//! The user is often in the agent's own pane, typing the next message. So
//! keyboard focus stays where it is, and no pane that runs a process takes the
//! file.

use crate::{
    action_handlers,
    app::Stoat,
    buffer_lifecycle,
    buffer_registry::OpenOrigin,
    jumplist::JumpEntry,
    pane::{Axis, PaneId, View},
    walkthrough::{Point, Range, RangeSpec},
    workspace::Workspace,
};
use serde_json::{json, Value};
use std::{ops, path::Path};
use stoat_text::{Bias, Rope, SelectionGoal};

/// Show `path` in the active workspace with the range `spec` names selected
/// and framed on screen, and return the reply for the request.
///
/// The file shows in the focused pane when that pane shows an editor, else in
/// the first split pane that shows one, else in a fresh split beside the
/// focused pane. A pane that shows the agent, a shell, or a run never takes
/// the file, because the file then hides the process that the pane shows.
/// Keyboard focus stays where it is, and the goto sets no mode.
///
/// A pane that already showed an editor records where it was on its
/// jumplist, so a backward jump there returns to it.
///
/// The reply names the absolute path and the range selected, in the form the
/// `selection` report gives. A path with no file, a file that opens as no
/// text, and a range the text does not have each reply an error.
pub(crate) fn goto(stoat: &mut Stoat, path: &Path, spec: RangeSpec) -> Value {
    let absolute = buffer_lifecycle::absolute_path(stoat, path);
    // The open of a missing path makes an empty buffer, which replaces the
    // pane's view and fits no range.
    let known = stoat
        .active_workspace()
        .buffers
        .id_for_path(&absolute)
        .is_some()
        || matches!(stoat.fs_host.metadata(&absolute), Ok(Some(_)));
    if !known {
        return json!({ "error": format!("no file {}", absolute.display()) });
    }

    let cannot_open = || json!({ "error": format!("cannot open {} as text", absolute.display()) });
    let target = target_pane(stoat);
    if buffer_lifecycle::open_file_in_pane_now(stoat, target, &absolute, OpenOrigin::Named)
        .is_none()
    {
        return cannot_open();
    }

    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    let ws = stoat.active_workspace_mut();
    let View::Editor(editor_id) = ws.panes.pane(target).view else {
        return cannot_open();
    };
    let Some(editor) = ws.editors.get_mut(editor_id) else {
        return cannot_open();
    };

    let buffer = editor.display_map.buffer_snapshot();
    let rope = buffer.rope();
    let span = match span_in(rope, spec) {
        Ok(span) => span,
        Err(reason) => return json!({ "error": reason }),
    };
    let selected = Range {
        start: point_at(rope, span.start),
        end: point_at(rope, rope.clip_offset(span.end - 1, Bias::Left)),
    };

    editor.selections.set_single_range(
        buffer.anchor_at(span.start, Bias::Left),
        buffer.anchor_at(span.end, Bias::Right),
        false,
        SelectionGoal::None,
    );
    action_handlers::walkthrough::frame_lines(editor, selected, scrolloff, false);

    json!({
        "reply": "goto",
        "path": absolute,
        "range": format!(
            "{}:{}-{}:{}",
            selected.start.line, selected.start.col, selected.end.line, selected.end.col
        ),
    })
}

/// The pane [`goto`] shows its file in.
///
/// A pane that shows an editor records its position first. A pane a split
/// makes shows only a scratch editor, which is no place to return to.
fn target_pane(stoat: &mut Stoat) -> PaneId {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_pane = std::iter::once(focused)
        .chain(ws.panes.split_pane_ids())
        .find(|&pane| matches!(ws.panes.pane(pane).view, View::Editor(_)));

    let Some(pane) = editor_pane else {
        let pane = ws.panes.split(Axis::Vertical);
        // The split copies the focused view, and an open that fails keeps that
        // copy in a second pane.
        let scratch = action_handlers::scratch_editor_view(ws, executor);
        ws.panes.pane_mut(pane).view = scratch;
        ws.panes.set_focus(focused);
        return pane;
    };

    // A widen covers every other pane. A focus move to a covered pane ends the
    // widen, and the goto ends it for the same reason.
    if ws.panes.widened().is_some_and(|widened| widened != pane) {
        ws.panes.unwiden();
    }
    push_jump(ws, pane);
    pane
}

/// Record the position of the editor that `pane` shows on the pane's own
/// jumplist.
///
/// The open records this too when it swaps in another file, and the list
/// absorbs the second push. A goto within the file the pane shows swaps
/// nothing, so this push is its only record.
fn push_jump(ws: &mut Workspace, pane: PaneId) {
    let View::Editor(editor_id) = ws.panes.pane(pane).view else {
        return;
    };
    let Some(editor) = ws.editors.get(editor_id) else {
        return;
    };
    let entry = JumpEntry {
        buffer_id: editor.buffer_id,
        selections: editor.selections.all_anchors().to_vec(),
    };
    let buffers = &ws.buffers;
    ws.panes.pane_mut(pane).jumplist.push(entry, buffers);
}

/// The bytes `spec` names in `rope`, widened to whole characters.
///
/// The rules and their error text are those of
/// [`crate::walkthrough::snippet_for`] and of the walkthrough command's
/// whole-line form. A trailing newline ends the last line rather than opening
/// an empty one. A whole-line form runs from column 1 of its first line to the
/// last byte of its last line, so an empty last line has no such form.
///
/// A start inside a character moves back to the character's first byte, and
/// an end inside one moves on past its last byte, so the selection never
/// splits a character.
fn span_in(rope: &Rope, spec: RangeSpec) -> Result<ops::Range<usize>, String> {
    let lines = line_count(rope);
    let line_bytes = |line: u32| match line > lines {
        true => Err(format!("line {line} is past the {lines} lines of content")),
        false => Ok(rope.line_len(line - 1)),
    };

    let range = match spec {
        RangeSpec::Points(range) => range,
        RangeSpec::Lines { start, end } => {
            let bytes = line_bytes(end)?;
            if bytes == 0 {
                return Err(format!("line {end} is empty, so no range covers it"));
            }
            Range {
                start: Point {
                    line: start,
                    col: 1,
                },
                end: Point {
                    line: end,
                    col: bytes,
                },
            }
        },
    };

    let offset = |point: Point| -> Result<usize, String> {
        let bytes = line_bytes(point.line)?;
        if point.col > bytes {
            return Err(format!(
                "column {} is past the {bytes} bytes of line {}",
                point.col, point.line
            ));
        }
        Ok(rope.point_to_offset(stoat_text::Point::new(point.line - 1, point.col - 1)))
    };
    let (start, last) = (offset(range.start)?, offset(range.end)?);
    if last < start {
        return Err("range ends before it starts".into());
    }
    Ok(rope.clip_offset(start, Bias::Left)..rope.clip_offset(last + 1, Bias::Right))
}

/// The number of lines an editor shows for `rope`.
fn line_count(rope: &Rope) -> u32 {
    let end = rope.max_point();
    match end.column == 0 && end.row > 0 {
        true => end.row,
        false => end.row + 1,
    }
}

/// The 1-based line and byte column of the byte at `offset`.
fn point_at(rope: &Rope, offset: usize) -> Point {
    let point = rope.offset_to_point(offset);
    Point {
        line: point.row + 1,
        col: point.column + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::span_in;
    use crate::{
        action_handlers,
        agent_ipc::AgentControl,
        pane::{PaneId, View},
        session_report,
        test_fixture::{open_buffer, seed},
        test_harness::TestHarness,
        walkthrough,
    };
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};
    use stoat_text::Rope;
    use tokio::sync::oneshot;

    /// A harness whose root holds `a.rs`, three lines long, and `b.rs`, with
    /// `b.rs` open in the focused pane.
    fn two_files() -> (TestHarness, PathBuf) {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(
            &mut h,
            &[("a.rs", "abc\ndef\nghi\n"), ("b.rs", "fn b() {}\n")],
        );
        open_buffer(&mut h, root.join("b.rs"));
        (h, root)
    }

    /// Send a goto for `path` and `range`, and return its reply.
    fn goto(h: &mut TestHarness, path: &Path, range: &str) -> Value {
        let uid = h.stoat.active_workspace().uid;
        let (reply, mut replies) = oneshot::channel();
        h.stoat.handle_agent_control(AgentControl::Goto {
            uid,
            path: path.to_path_buf(),
            spec: walkthrough::parse_range(range).expect("a range"),
            reply,
        });
        replies.try_recv().expect("a goto reply")
    }

    /// The file `pane` shows, or `None` when it shows anything but an editor.
    fn shown_path(h: &TestHarness, pane: PaneId) -> Option<PathBuf> {
        let ws = h.stoat.active_workspace();
        let View::Editor(editor_id) = ws.panes.pane(pane).view else {
            return None;
        };
        let buffer_id = ws.editors.get(editor_id)?.buffer_id;
        ws.buffers.path_for(buffer_id).map(Path::to_path_buf)
    }

    #[test]
    fn goto_selects_the_range_and_puts_its_cursor_on_the_last_byte() {
        let (mut h, root) = two_files();

        let reply = goto(&mut h, &root.join("a.rs"), "2:1-3:2");

        let uid = h.stoat.active_workspace().uid;
        let report = session_report::selection(&h.stoat, uid, false);
        assert_eq!(
            (reply, &report["path"], &report["selections"]),
            (
                json!({ "reply": "goto", "path": root.join("a.rs"), "range": "2:1-3:2" }),
                &json!(root.join("a.rs")),
                &json!([{
                    "primary": true,
                    "reversed": false,
                    "empty": false,
                    "start": { "line": 2, "col": 1 },
                    "end": { "line": 3, "col": 2 },
                    "cursor": { "line": 3, "col": 2 },
                    "range": "2:1-3:2",
                }]),
            ),
        );
    }

    #[test]
    fn goto_with_whole_lines_covers_them() {
        let (mut h, root) = two_files();

        assert_eq!(goto(&mut h, &root.join("a.rs"), "1-2")["range"], "1:1-2:3");
    }

    #[test]
    fn goto_replies_the_range_widened_to_whole_characters() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("e.rs", "ab\u{e9}\n")]);

        assert_eq!(
            goto(&mut h, &root.join("e.rs"), "1:4-1:4")["range"],
            "1:3-1:3"
        );
    }

    #[test]
    fn goto_past_the_last_line_replies_an_error() {
        let (mut h, root) = two_files();

        assert_eq!(
            goto(&mut h, &root.join("a.rs"), "4"),
            json!({ "error": "line 4 is past the 3 lines of content" }),
        );
    }

    #[test]
    fn a_goto_to_a_missing_file_replies_an_error_and_leaves_the_pane() {
        let (mut h, root) = two_files();
        let missing = root.join("nope.rs");

        let reply = goto(&mut h, &missing, "1");

        let focus = h.stoat.active_workspace().panes.focus();
        assert_eq!(
            (reply, shown_path(&h, focus)),
            (
                json!({ "error": format!("no file {}", missing.display()) }),
                Some(root.join("b.rs")),
            ),
        );
    }

    #[test]
    fn a_backward_jump_returns_from_a_goto_within_the_file() {
        let (mut h, root) = two_files();
        let a = root.join("a.rs");
        goto(&mut h, &a, "1");
        goto(&mut h, &a, "3");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);

        let uid = h.stoat.active_workspace().uid;
        let report = session_report::selection(&h.stoat, uid, false);
        assert_eq!(
            (&report["path"], &report["selections"][0]["range"]),
            (&json!(a), &json!("1:1-1:3")),
        );
    }

    #[test]
    fn goto_frames_a_range_below_the_screen() {
        let mut h = TestHarness::with_size(80, 24);
        let text: String = (1..=60).map(|n| format!("line {n}\n")).collect();
        let root = seed(&mut h, &[("long.rs", &text)]);
        open_buffer(&mut h, root.join("long.rs"));
        h.snapshot();

        goto(&mut h, &root.join("long.rs"), "50");

        let ws = h.stoat.active_workspace();
        let (editor_id, _) = ws.focused_editor().expect("an editor has focus");
        let editor = &ws.editors[editor_id];
        let rows = editor.viewport_rows.expect("a paint sized the editor");
        let shown = editor.scroll_row + 1..=editor.scroll_row + rows;
        assert!(shown.contains(&50), "line 50 is on screen in {shown:?}");
    }

    #[test]
    fn a_goto_from_the_agent_pane_keeps_the_agent_and_its_focus() {
        let (mut h, root) = two_files();
        h.stoat.set_agent_socket_dir("/state".into());
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SpawnClaude);
        let agent_pane = h.stoat.active_workspace().panes.focus();
        let agent_view = h
            .stoat
            .active_workspace()
            .panes
            .pane(agent_pane)
            .view
            .clone();

        let first = goto(&mut h, &root.join("a.rs"), "1")["range"].clone();
        let widened = h.stoat.active_workspace_mut().panes.widen(agent_pane);
        let second = goto(&mut h, &root.join("a.rs"), "2")["range"].clone();

        let ws = h.stoat.active_workspace();
        let panes = ws.panes.split_pane_ids();
        let beside = panes.iter().copied().find(|&pane| pane != agent_pane);
        assert_eq!(
            (
                panes.len(),
                ws.panes.focus(),
                (widened, ws.panes.widened()),
                matches!(agent_view, View::Agent(_))
                    && ws.panes.pane(agent_pane).view == agent_view,
                beside.and_then(|pane| shown_path(&h, pane)),
                [first, second],
            ),
            (
                2,
                agent_pane,
                (true, None),
                true,
                Some(root.join("a.rs")),
                [json!("1:1-1:3"), json!("2:1-2:3")],
            ),
            "a split beside the agent, then the same pane with the widen ended",
        );
    }

    #[test]
    fn a_span_covers_whole_characters_and_refuses_what_the_text_lacks() {
        let rope = Rope::from("ab\u{e9}\n\nxyz\n");
        let span = |range: &str| span_in(&rope, walkthrough::parse_range(range).expect("a range"));

        assert_eq!(
            ["1:3-1:3", "1:4-1:4", "1-3", "4", "1:5-1:5", "2", "2:1-3:1", "3:1-1:1"].map(span),
            [
                Ok(2..4),
                Ok(2..4),
                Ok(0..9),
                Err("line 4 is past the 3 lines of content".into()),
                Err("column 5 is past the 4 bytes of line 1".into()),
                Err("line 2 is empty, so no range covers it".into()),
                Err("column 1 is past the 0 bytes of line 2".into()),
                Err("range ends before it starts".into()),
            ],
            "U+00E9 takes columns 3 and 4 of line 1, and line 2 is empty",
        );
    }
}
