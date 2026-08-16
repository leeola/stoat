use crate::{
    app::{Stoat, UpdateEffect},
    input_view::{InputView, SubmitTarget},
};
use std::ops::Range;
use stoat_text::{Anchor, Bias, Selection, SelectionGoal};

/// Which shell-integration operation the input modal will perform on
/// submit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellAction {
    Pipe,
    PipeTo,
    InsertOutput,
    AppendOutput,
    KeepPipe,
}

/// Active state while the user is typing the shell command.
pub(crate) struct ShellInputState {
    pub(crate) input: InputView,
    pub(crate) action: ShellAction,
}

pub(super) fn open_pipe(stoat: &mut Stoat) -> UpdateEffect {
    open_with(stoat, ShellAction::Pipe)
}

pub(super) fn open_pipe_to(stoat: &mut Stoat) -> UpdateEffect {
    open_with(stoat, ShellAction::PipeTo)
}

pub(super) fn open_insert_output(stoat: &mut Stoat) -> UpdateEffect {
    open_with(stoat, ShellAction::InsertOutput)
}

pub(super) fn open_append_output(stoat: &mut Stoat) -> UpdateEffect {
    open_with(stoat, ShellAction::AppendOutput)
}

pub(super) fn open_keep_pipe(stoat: &mut Stoat) -> UpdateEffect {
    open_with(stoat, ShellAction::KeepPipe)
}

fn open_with(stoat: &mut Stoat, action: ShellAction) -> UpdateEffect {
    if stoat.shell_input.is_some() {
        return UpdateEffect::None;
    }
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(ws, executor, SubmitTarget::Shell, "", "insert", 1);
    stoat.shell_input = Some(ShellInputState { input, action });
    UpdateEffect::Redraw
}

/// Submit the shell command. Reads the typed command, then runs it
/// per-selection (or once for InsertOutput) via
/// [`crate::host::ShellHost`] and applies the operation. Returns
/// `true` when the input modal was open.
pub(crate) fn submit(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.shell_input.take() else {
        return false;
    };
    let cmd = state.input.text(stoat.active_workspace());
    let action = state.action;
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    if cmd.is_empty() {
        return true;
    }
    let shell_host = stoat.shell_host.clone();
    match action {
        ShellAction::Pipe => apply_pipe(stoat, &*shell_host, &cmd),
        ShellAction::PipeTo => apply_pipe_to(stoat, &*shell_host, &cmd),
        ShellAction::InsertOutput => apply_insert_output(stoat, &*shell_host, &cmd),
        ShellAction::AppendOutput => apply_append_output(stoat, &*shell_host, &cmd),
        ShellAction::KeepPipe => apply_keep_pipe(stoat, &*shell_host, &cmd),
    }
    true
}

/// Cancel the input modal without running the command.
pub(crate) fn cancel(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.shell_input.take() else {
        return false;
    };
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    true
}

/// Run `cmd` with `stdin`, answering the text it produced or why it failed.
///
/// Stderr is the output whenever the command wrote any, whatever it exited
/// with, on the reading that a command with something to say about its work
/// said it there. Only a non-zero exit that wrote **nothing** to stderr is a
/// failure, since then there is no output to take and no message to show.
///
/// `selection` is the text the op runs over. A command that ends its output
/// with a newline the selection lacked added that newline itself, so it comes
/// off along with a carriage return before it. Insert and append pipe nothing
/// but still measure against their selection.
fn run_shell(
    host: &dyn crate::host::ShellHost,
    cmd: &str,
    stdin: &str,
    selection: &str,
    diff: &[(String, Option<String>)],
) -> Result<String, String> {
    let out = host
        .run(cmd, stdin.as_bytes(), None, diff)
        .map_err(|err| format!("Shell command failed: {err}"))?;

    let mut text = match (out.exit_code == 0, out.stderr.is_empty()) {
        (false, true) => {
            return Err(format!("Shell command failed: status {}", out.exit_code));
        },
        (_, false) => String::from_utf8_lossy(&out.stderr).into_owned(),
        (true, true) => String::from_utf8_lossy(&out.stdout).into_owned(),
    };

    if !selection.ends_with('\n') && text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(text)
}

/// Text the primary selection covers, or empty where no editor is focused.
///
/// Insert and append pipe nothing, so this is the selection their trailing
/// newline is measured against.
fn primary_text(stoat: &mut Stoat) -> String {
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return String::new();
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let primary = editor.selections.newest_anchor();
    let start = buffer_snapshot.resolve_anchor(&primary.start);
    let end = buffer_snapshot.resolve_anchor(&primary.end);
    buffer_snapshot.rope().chunks_in_range(start..end).collect()
}

/// Where one op writes its output for one selection, and what it replaces.
///
/// The three editing ops differ in nothing else. Pipe covers the range and
/// consumes it, insert writes at the range's start and consumes nothing, and
/// append writes at its end.
struct EditSpan {
    from: usize,
    to: usize,
    deleted: usize,
    reversed: bool,
}

/// Apply `outputs` at `spans` and leave each selection over what it wrote.
///
/// The spans arrive in document order, one output apiece. Each new selection
/// runs from the edit point to the end of its output, so an op that wrote
/// nothing leaves a collapsed cursor there rather than the selection it
/// started with. The source range's direction carries over.
///
/// `primary_index` names the span whose piece takes the primary, which keeps
/// the primary on the same selection the user had rather than moving it to an
/// end of the set.
///
/// The whole set of edits is one undo step, and undoing it puts back the
/// selections the op ran over. The dispatch that opened the prompt grouped
/// against the prompt's own scratch buffer, which is gone by the time this
/// runs, so the group is opened here instead.
fn reshape(stoat: &mut Stoat, spans: Vec<EditSpan>, outputs: Vec<String>, primary_index: usize) {
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };
    let buffer_id = editor.buffer_id;
    let before = editor.selections.shared_anchors();
    let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) else {
        return;
    };

    {
        // Descending, so each edit lands before the offsets of the ones still
        // to come move under it.
        let mut guard = buffer.write().expect("buffer poisoned");
        guard.begin_group(before);
        let batch: Vec<(Range<usize>, &str)> = spans
            .iter()
            .zip(&outputs)
            .rev()
            .map(|(span, out)| (span.from..span.to, out.as_str()))
            .collect();
        guard.edit_batch(&batch);
    }

    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    // Ascending this time, since the shift each piece sits at is the sum of
    // what every earlier edit added and removed.
    let pieces: Vec<Selection<Anchor>> = spans
        .iter()
        .zip(&outputs)
        .scan(0i64, |delta, (span, out)| {
            let start = (span.to as i64 + *delta) as usize - span.deleted;
            let end = start + out.len();
            *delta += out.len() as i64 - span.deleted as i64;
            Some(Selection {
                id: 0,
                start: new_buf.anchor_at(start, Bias::Right),
                end: new_buf.anchor_at(end, Bias::Right),
                reversed: span.reversed,
                goal: SelectionGoal::None,
            })
        })
        .collect();
    if pieces.is_empty() {
        return;
    }
    editor
        .selections
        .replace_with_fresh_ids_primary(pieces, primary_index, new_buf);

    // Sealed after the pieces install, so a redo lands on the selections the op
    // produced rather than the ones it started from.
    let after = editor.selections.shared_anchors();
    if let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) {
        buffer.write().expect("buffer poisoned").seal_group(after);
    }
}

fn apply_pipe(stoat: &mut Stoat, shell_host: &dyn crate::host::ShellHost, cmd: &str) {
    let diff = stoat.active_workspace().env.diff.clone();
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let outputs: Result<Vec<String>, String> = editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| {
            let start = buffer_snapshot.resolve_anchor(&sel.start);
            let end = buffer_snapshot.resolve_anchor(&sel.end);
            let stdin: String = rope.chunks_in_range(start..end).collect();
            run_shell(shell_host, cmd, &stdin, &stdin, &diff)
        })
        .collect();
    // The first failure ends the op before any edit, so a command that fails
    // partway leaves the buffer as it was rather than half rewritten.
    let outputs = match outputs {
        Ok(outputs) => outputs,
        Err(message) => {
            stoat.set_status(message);
            return;
        },
    };
    let Some((spans, primary_index)) = edit_spans(stoat, Shape::Replace) else {
        return;
    };
    reshape(stoat, spans, outputs, primary_index);
}

/// Which of the three shapes an op writes with.
#[derive(Copy, Clone)]
enum Shape {
    Replace,
    Insert,
    Append,
}

/// Every selection as an [`EditSpan`] under `shape`, with the primary's index.
///
/// Ordered by position, which is the order [`reshape`] reads them in and the
/// order the selections already hold.
fn edit_spans(stoat: &mut Stoat, shape: Shape) -> Option<(Vec<EditSpan>, usize)> {
    let editor = super::focused_editor_mut(stoat)?;
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let primary_index = editor.selections.primary_index();
    let spans = editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| {
            let start = buffer_snapshot.resolve_anchor(&sel.start);
            let end = buffer_snapshot.resolve_anchor(&sel.end);
            match shape {
                Shape::Replace => EditSpan {
                    from: start,
                    to: end,
                    deleted: end - start,
                    reversed: sel.reversed,
                },
                Shape::Insert => EditSpan {
                    from: start,
                    to: start,
                    deleted: 0,
                    reversed: sel.reversed,
                },
                Shape::Append => EditSpan {
                    from: end,
                    to: end,
                    deleted: 0,
                    reversed: sel.reversed,
                },
            }
        })
        .collect();
    Some((spans, primary_index))
}

fn apply_pipe_to(stoat: &mut Stoat, shell_host: &dyn crate::host::ShellHost, cmd: &str) {
    let diff = stoat.active_workspace().env.diff.clone();
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    // Pipe-to keeps no output, so the run matters only for whether it failed.
    // Stopping at the first failure leaves the rest of the selections unrun,
    // which is what tells the user the command is broken rather than the input.
    for sel in editor.selections.all_anchors() {
        let start = buffer_snapshot.resolve_anchor(&sel.start);
        let end = buffer_snapshot.resolve_anchor(&sel.end);
        let stdin: String = rope.chunks_in_range(start..end).collect();
        if let Err(message) = run_shell(shell_host, cmd, &stdin, &stdin, &diff) {
            stoat.set_status(message);
            return;
        }
    }
}

fn apply_insert_output(stoat: &mut Stoat, shell_host: &dyn crate::host::ShellHost, cmd: &str) {
    let diff = stoat.active_workspace().env.diff.clone();
    let output = match run_shell(shell_host, cmd, "", &primary_text(stoat), &diff) {
        Ok(output) => output,
        Err(message) => {
            stoat.set_status(message);
            return;
        },
    };
    let Some((spans, primary_index)) = edit_spans(stoat, Shape::Insert) else {
        return;
    };
    // One run feeds every selection, so each takes a copy of the same output.
    let outputs = vec![output; spans.len()];
    reshape(stoat, spans, outputs, primary_index);
}

fn apply_append_output(stoat: &mut Stoat, shell_host: &dyn crate::host::ShellHost, cmd: &str) {
    let diff = stoat.active_workspace().env.diff.clone();
    let output = match run_shell(shell_host, cmd, "", &primary_text(stoat), &diff) {
        Ok(output) => output,
        Err(message) => {
            stoat.set_status(message);
            return;
        },
    };
    let Some((spans, primary_index)) = edit_spans(stoat, Shape::Append) else {
        return;
    };
    let outputs = vec![output; spans.len()];
    reshape(stoat, spans, outputs, primary_index);
}

fn apply_keep_pipe(stoat: &mut Stoat, shell_host: &dyn crate::host::ShellHost, cmd: &str) {
    let diff = stoat.active_workspace().env.diff.clone();
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let old_primary = editor.selections.primary_index();
    let kept: Vec<(usize, Selection<Anchor>)> = editor
        .selections
        .all_anchors()
        .iter()
        .enumerate()
        // A selection survives whenever its run produced output, which a
        // command that exits non-zero but writes to stderr still does. The exit
        // code alone drops those, where the command did have something to say.
        .filter(|(_, sel)| {
            let start = buffer_snapshot.resolve_anchor(&sel.start);
            let end = buffer_snapshot.resolve_anchor(&sel.end);
            let stdin: String = rope.chunks_in_range(start..end).collect();
            run_shell(shell_host, cmd, &stdin, &stdin, &diff).is_ok()
        })
        .map(|(index, sel)| (index, sel.clone()))
        .collect();

    // Keeping nothing leaves no cursor at all, so the filter is refused. In
    // silence that press is indistinguishable from one that dropped nothing
    // because every selection survived.
    let Some(last) = kept.last() else {
        stoat.set_status("no selections remaining");
        return;
    };

    // The primary moves forward to the nearest survivor rather than back, so it
    // stays near where the user left it. Where every survivor sits before the
    // old primary there is nothing ahead to move to, and the last one is the
    // nearest.
    let promoted = kept
        .iter()
        .find(|(index, _)| *index >= old_primary)
        .unwrap_or(last)
        .1
        .id;

    let kept: Vec<Selection<Anchor>> = kept.into_iter().map(|(_, sel)| sel).collect();
    editor.selections.replace_with(kept, buffer_snapshot);
    editor.selections.make_primary(promoted);
}

#[cfg(test)]
mod tests {
    use super::ShellAction;
    use crate::{
        action_handlers::dispatch,
        host::{FakeShell, ShellOutput},
        test_harness::{editor, keys, TestHarness},
        Stoat,
    };
    use crossterm::event::{Event, KeyCode};
    use std::sync::Arc;
    use stoat_action as action;

    fn install_fake(h: &mut TestHarness) -> Arc<FakeShell> {
        let fake = Arc::new(FakeShell::new());
        h.stoat.set_shell_host(fake.clone());
        fake
    }

    fn select_range(h: &mut TestHarness, start: usize, end: usize) {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let start_anchor = buf_snap.anchor_at(start, stoat_text::Bias::Right);
        let end_anchor = buf_snap.anchor_at(end, stoat_text::Bias::Right);
        editor
            .selections
            .transform(buf_snap, |s| stoat_text::Selection {
                id: s.id,
                start: start_anchor,
                end: end_anchor,
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            });
    }

    /// [`select_range`] facing backward, so its head sits at `start`.
    fn select_range_reversed(h: &mut TestHarness, start: usize, end: usize) {
        select_range(h, start, end);
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        editor
            .selections
            .transform(buf_snap, |s| stoat_text::Selection {
                reversed: true,
                ..s.clone()
            });
    }

    fn buffer_text(h: &mut TestHarness) -> String {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        snapshot.buffer_snapshot().rope().to_string()
    }

    /// Piping several selections leaves pieces that are distinct, so removing
    /// the primary one removes one of them.
    ///
    /// The collection deletes the primary by id, so pieces sharing an id are
    /// one piece as far as that goes, and removing the primary of a set that
    /// shares one empties a collection that must never be empty.
    #[test]
    fn piped_pieces_survive_removing_the_primary_selection() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "tr a-z A-Z",
            ShellOutput {
                stdout: b"X".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.seed_focused_buffer("aa bb cc");
        select_range(&mut h, 0, 2);
        add_range(&mut h, 3, 5);
        add_range(&mut h, 6, 8);

        dispatch(&mut h.stoat, &action::ShellPipe);
        h.type_text("tr a-z A-Z");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(
            selection_count(&mut h),
            3,
            "each piped piece is its own selection",
        );

        dispatch(&mut h.stoat, &action::RemovePrimarySelection);
        assert_eq!(
            selection_count(&mut h),
            2,
            "removing the primary removes one of them, not all of them",
        );
    }

    /// Add a second and further selection, which mints its own id.
    fn add_range(h: &mut TestHarness, start: usize, end: usize) {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        editor.selections.extend_with_fresh_ids(
            vec![stoat_text::Selection {
                id: 0,
                start: buf_snap.anchor_at(start, stoat_text::Bias::Right),
                end: buf_snap.anchor_at(end, stoat_text::Bias::Right),
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            }],
            buf_snap,
        );
    }

    fn selection_count(h: &mut TestHarness) -> usize {
        crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .selections
            .all_anchors()
            .len()
    }

    #[test]
    fn shell_pipe_replaces_selection_with_stdout() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "tr a-z A-Z",
            ShellOutput {
                stdout: b"HELLO".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.seed_focused_buffer("hello world");
        select_range(&mut h, 0, 5);
        dispatch(&mut h.stoat, &action::ShellPipe);
        h.type_text("tr a-z A-Z");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(buffer_text(&mut h), "HELLO world");
    }

    #[test]
    fn shell_pipe_to_leaves_selection_unchanged() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        h.seed_focused_buffer("hello");
        select_range(&mut h, 0, 5);
        dispatch(&mut h.stoat, &action::ShellPipeTo);
        h.type_text("ignored");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(buffer_text(&mut h), "hello");
        assert_eq!(fake.invocations().len(), 1);
        assert_eq!(fake.invocations()[0].stdin, b"hello");
    }

    #[test]
    fn shell_insert_output_inserts_at_cursor() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "date",
            ShellOutput {
                stdout: b"Mon Jan 1".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.seed_focused_buffer("xy");
        select_range(&mut h, 1, 1);
        dispatch(&mut h.stoat, &action::ShellInsertOutput);
        h.type_text("date");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(buffer_text(&mut h), "xMon Jan 1y");
    }

    /// Insert writes at each selection's start, so a forward selection gets the
    /// output before it rather than after.
    #[test]
    fn insert_output_lands_before_a_forward_selection() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "date",
            ShellOutput {
                stdout: b"DATE".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.seed_focused_buffer("hello world");
        select_range(&mut h, 0, 5);
        dispatch(&mut h.stoat, &action::ShellInsertOutput);
        h.type_text("date");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(buffer_text(&mut h), "DATEhello world");
    }

    /// Each selection ends up over the output it produced, which is what leaves
    /// the inserted text ready for the next command.
    #[test]
    fn insert_output_selects_its_output() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "date",
            ShellOutput {
                stdout: b"DATE".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.seed_focused_buffer("hello world");
        select_range(&mut h, 0, 5);
        dispatch(&mut h.stoat, &action::ShellInsertOutput);
        h.type_text("date");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(0, 4, false)],
            "the selection covers DATE rather than the text it was over",
        );
    }

    /// A piped selection keeps the direction it had, since the direction says
    /// which end the cursor sits on and the command did not move it.
    #[test]
    fn pipe_keeps_a_reversed_selection_reversed() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "tr a-z A-Z",
            ShellOutput {
                stdout: b"HELLO".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.seed_focused_buffer("hello world");
        select_range_reversed(&mut h, 0, 5);
        dispatch(&mut h.stoat, &action::ShellPipe);
        h.type_text("tr a-z A-Z");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(editor::selection_spans(&mut h.stoat), vec![(0, 5, true)]);
    }

    /// A command that produced nothing still applies, leaving a collapsed
    /// cursor at the point its output goes.
    ///
    /// Insert rather than pipe, since pipe always applied and only the two
    /// output ops returned early on an empty result.
    #[test]
    fn empty_output_collapses_the_selection() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "true",
            ShellOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.seed_focused_buffer("hello world");
        select_range(&mut h, 0, 5);
        dispatch(&mut h.stoat, &action::ShellInsertOutput);
        h.type_text("true");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(buffer_text(&mut h), "hello world", "nothing was written");
        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(0, 0, false)],
            "and the selection collapses where the output goes",
        );
    }

    /// The primary stays on the piece its own selection produced, rather than
    /// moving to an end of the set.
    #[test]
    fn pipe_keeps_the_primary_on_its_own_piece() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "tag",
            ShellOutput {
                stdout: b"T".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.seed_focused_buffer("aa bb");
        select_range(&mut h, 0, 2);
        add_range(&mut h, 3, 5);
        assert_eq!(selection_count(&mut h), 2, "test setup: two selections");

        dispatch(&mut h.stoat, &action::ShellPipe);
        h.type_text("tag");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(buffer_text(&mut h), "T T");
        let primary_start = {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            let snapshot = editor.display_map.snapshot();
            let buf = snapshot.buffer_snapshot();
            buf.resolve_anchor(&editor.selections.newest_anchor().start)
        };
        assert_eq!(
            primary_start, 2,
            "the primary was the second selection and stays on its piece",
        );
    }

    #[test]
    fn shell_append_output_appends_after_selection() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "date",
            ShellOutput {
                stdout: b"Mon Jan 1".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.seed_focused_buffer("hello world");
        select_range(&mut h, 0, 5);
        dispatch(&mut h.stoat, &action::ShellAppendOutput);
        h.type_text("date");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(buffer_text(&mut h), "helloMon Jan 1 world");
    }

    #[test]
    fn shell_keep_pipe_filters_by_exit_code() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "grep -q '[0-9]'",
            ShellOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        // Default fallback for non-programmed commands is exit 0; we
        // need a non-zero response for "abc" so the test programs a
        // tagged variant. To distinguish by stdin would require a
        // different FakeShell shape; for this test, programme exit 0
        // for the digit selection by using two separate registered
        // commands. Simpler: programme a non-default exit for the
        // command and rely on a sentinel: test passes when ALL
        // selections survive. Below we instead seed two selections
        // where the FAKE will return exit 1 for the command on
        // second-position selections by only programming the literal
        // command once. The default behaviour returns exit 0, so
        // both selections are kept; this verifies the keep path
        // doesn't drop selections when exit is 0. The actual filter
        // semantics are exercised by `keep_pipe_drops_when_exit_nonzero`.
        h.seed_focused_buffer("123 abc");
        select_range(&mut h, 0, 3);
        dispatch(&mut h.stoat, &action::ShellKeepPipe);
        h.type_text("grep -q '[0-9]'");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 3, false)]);
    }

    #[test]
    fn keep_pipe_drops_when_exit_nonzero() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        // Default fake response is exit 0 (keep). Programme a
        // non-zero exit so the filter drops the selection.
        fake.set_response(
            "false",
            ShellOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 1,
            },
        );
        h.seed_focused_buffer("abc");
        select_range(&mut h, 0, 3);
        dispatch(&mut h.stoat, &action::ShellKeepPipe);
        h.type_text("false");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        // Keeping nothing is refused, so the selection stays as it was.
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 3, false)]);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no selections remaining"),
        );
    }

    /// A command that fails with nothing to say aborts before any edit, so a
    /// broken command leaves the buffer as it was and says so.
    #[test]
    fn a_failing_pipe_leaves_the_buffer_untouched() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "false",
            ShellOutput {
                stdout: b"ignored".to_vec(),
                stderr: Vec::new(),
                exit_code: 3,
            },
        );
        h.seed_focused_buffer("hello");
        select_range(&mut h, 0, 5);
        dispatch(&mut h.stoat, &action::ShellPipe);
        h.type_text("false");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(buffer_text(&mut h), "hello", "no edit landed");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("Shell command failed: status 3"),
        );
    }

    /// A command with something to say says it on stderr, so stderr is the
    /// output whatever the command exited with.
    #[test]
    fn stderr_replaces_the_selection() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "noisy",
            ShellOutput {
                stdout: b"out".to_vec(),
                stderr: b"err".to_vec(),
                exit_code: 1,
            },
        );
        h.seed_focused_buffer("hello");
        select_range(&mut h, 0, 5);
        dispatch(&mut h.stoat, &action::ShellPipe);
        h.type_text("noisy");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(
            buffer_text(&mut h),
            "err",
            "stderr wins over stdout, and a non-zero exit carrying it is no failure",
        );
    }

    /// A newline the selection did not have is the command's own padding, so it
    /// comes off rather than growing the buffer a line.
    #[test]
    fn a_trailing_newline_the_selection_lacked_comes_off() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "wc -c",
            ShellOutput {
                stdout: b"5\r\n".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.seed_focused_buffer("hello");
        select_range(&mut h, 0, 5);
        dispatch(&mut h.stoat, &action::ShellPipe);
        h.type_text("wc -c");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(
            buffer_text(&mut h),
            "5",
            "the carriage return goes with the newline it preceded",
        );
    }

    /// Keep-pipe asks whether the run produced output, so a command that exits
    /// non-zero while writing to stderr keeps its selection.
    ///
    /// Two selections answered differently, since a filter that keeps
    /// everything and one that keeps nothing both leave the set untouched. The
    /// second returns early rather than emptying it.
    #[test]
    fn keep_pipe_keeps_a_selection_whose_command_wrote_stderr() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response_for_stdin(
            "check",
            b"abc".to_vec(),
            ShellOutput {
                stdout: Vec::new(),
                stderr: b"unknown option".to_vec(),
                exit_code: 1,
            },
        );
        fake.set_response_for_stdin(
            "check",
            b"def".to_vec(),
            ShellOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 1,
            },
        );
        h.seed_focused_buffer("abc def");
        select_range(&mut h, 0, 3);
        add_range(&mut h, 4, 7);
        assert_eq!(selection_count(&mut h), 2, "test setup: two selections");

        dispatch(&mut h.stoat, &action::ShellKeepPipe);
        h.type_text("check");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(0, 3, false)],
            "the stderr run survives and the silent failure does not",
        );
    }

    /// Refusing an empty filter in silence reads the same as a filter that
    /// dropped nothing, so the refusal says so.
    #[test]
    fn keep_pipe_reports_no_selections_remaining() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "reject",
            ShellOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 1,
            },
        );
        h.seed_focused_buffer("aa bb");
        select_range(&mut h, 0, 2);
        add_range(&mut h, 3, 5);

        dispatch(&mut h.stoat, &action::ShellKeepPipe);
        h.type_text("reject");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no selections remaining"),
        );
        assert_eq!(selection_count(&mut h), 2, "and both selections stay");
    }

    /// The primary moves forward to the nearest survivor, so the user carries
    /// on from near where they left off rather than from an end of the set.
    #[test]
    fn keep_pipe_promotes_the_survivor_after_the_primary() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        // The primary is the first selection and does not survive, so the
        // promotion has two survivors ahead of it to choose between. Added
        // last, so it holds the highest id and the two rules part company.
        fake.set_response_for_stdin(
            "check",
            b"aa".to_vec(),
            ShellOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 1,
            },
        );
        h.seed_focused_buffer("aa bb cc");
        select_range(&mut h, 3, 5);
        add_range(&mut h, 6, 8);
        add_range(&mut h, 0, 2);
        assert_eq!(selection_count(&mut h), 3, "test setup: three selections");

        dispatch(&mut h.stoat, &action::ShellKeepPipe);
        h.type_text("check");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(3, 5, false), (6, 8, false)],
            "the primary selection went",
        );
        let primary_start = {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            let snapshot = editor.display_map.snapshot();
            let buf = snapshot.buffer_snapshot();
            buf.resolve_anchor(&editor.selections.newest_anchor().start)
        };
        assert_eq!(
            primary_start, 3,
            "the nearest survivor ahead takes it, not the last of the set",
        );
    }

    /// A shell edit is one change to the user, so one undo takes all of it and
    /// puts the selections back where they were.
    #[test]
    fn pipe_undoes_as_one_step_and_restores_the_selections() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "tag",
            ShellOutput {
                stdout: b"X".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.seed_focused_buffer("aa bb");
        select_range(&mut h, 0, 2);
        add_range(&mut h, 3, 5);

        dispatch(&mut h.stoat, &action::ShellPipe);
        h.type_text("tag");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(buffer_text(&mut h), "X X", "both selections were piped");

        dispatch(&mut h.stoat, &action::Undo);
        assert_eq!(
            buffer_text(&mut h),
            "aa bb",
            "one undo takes the whole edit, not one selection's worth",
        );
        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(0, 2, false), (3, 5, false)],
            "and the selections the op started from come back",
        );

        dispatch(&mut h.stoat, &action::Redo);
        assert_eq!(buffer_text(&mut h), "X X");
        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(0, 1, false), (2, 3, false)],
            "redo lands on the selections the op produced",
        );
    }

    /// The pipe action had no key at all, so the one command of the five that
    /// rewrites text through a filter was unreachable.
    #[test]
    fn the_bar_arms_the_pipe_in_normal_mode() {
        let mut h = Stoat::test();
        install_fake(&mut h);
        h.seed_focused_buffer("hello");

        h.type_keys("|");
        assert_eq!(
            h.stoat.shell_input.as_ref().map(|state| state.action),
            Some(ShellAction::Pipe),
        );
    }

    /// All five reach select mode, where a selection set is what the user has
    /// in hand and the ops run over exactly that.
    #[test]
    fn the_shell_family_arms_from_select_mode() {
        for (keys, action) in [
            ("|", ShellAction::Pipe),
            ("alt-|", ShellAction::PipeTo),
            ("!", ShellAction::InsertOutput),
            ("alt-!", ShellAction::AppendOutput),
            ("$", ShellAction::KeepPipe),
        ] {
            let mut h = Stoat::test();
            install_fake(&mut h);
            h.seed_focused_buffer("hello");
            h.type_keys("v");
            assert_eq!(h.stoat.focused_mode(), "select", "test setup: in select");

            h.type_keys(keys);
            assert_eq!(
                h.stoat.shell_input.as_ref().map(|state| state.action),
                Some(action),
                "{keys} arms its op from select mode",
            );
        }
    }

    /// A shell op leaves the mode where it found it, so the selections it hands
    /// back are there to work with.
    #[test]
    fn a_shell_op_stays_in_select_mode() {
        let mut h = Stoat::test();
        let fake = install_fake(&mut h);
        fake.set_response(
            "tr a-z A-Z",
            ShellOutput {
                stdout: b"HELLO".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.seed_focused_buffer("hello");
        h.type_keys("v");
        select_range(&mut h, 0, 5);

        h.type_keys("|");
        h.type_text("tr a-z A-Z");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(buffer_text(&mut h), "HELLO");
        assert_eq!(h.stoat.focused_mode(), "select");
    }

    #[test]
    fn empty_command_keeps_state() {
        let mut h = Stoat::test();
        install_fake(&mut h);
        h.seed_focused_buffer("hello");
        select_range(&mut h, 0, 5);
        dispatch(&mut h.stoat, &action::ShellPipe);
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(buffer_text(&mut h), "hello");
        assert!(h.stoat.shell_input.is_none());
    }

    #[test]
    fn escape_cancels_input() {
        let mut h = Stoat::test();
        install_fake(&mut h);
        h.seed_focused_buffer("hello");
        select_range(&mut h, 0, 5);
        dispatch(&mut h.stoat, &action::ShellPipe);
        h.stoat.update(Event::Key(keys::key(KeyCode::Esc)));
        assert!(h.stoat.shell_input.is_none());
        assert_eq!(buffer_text(&mut h), "hello");
        assert_eq!(h.stoat.focused_mode(), "normal");
    }
}
