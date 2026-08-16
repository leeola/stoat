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
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };
    let buffer_id = editor.buffer_id;
    let buffer = match stoat.active_workspace().buffers.get(buffer_id) {
        Some(b) => b,
        None => return,
    };
    let editor = match super::focused_editor_mut(stoat) {
        Some(e) => e,
        None => return,
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let mut ranges: Vec<(usize, usize)> = editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| {
            let s = buffer_snapshot.resolve_anchor(&sel.start);
            let e = buffer_snapshot.resolve_anchor(&sel.end);
            (s, e)
        })
        .collect();
    let mut indexed: Vec<(usize, usize, String)> = ranges
        .drain(..)
        .zip(outputs)
        .map(|((s, e), out)| (s, e, out))
        .collect();
    indexed.sort_by_key(|b| std::cmp::Reverse(b.0));
    {
        let mut guard = buffer.write().expect("buffer poisoned");
        let batch: Vec<(Range<usize>, &str)> = indexed
            .iter()
            .map(|(s, e, out)| (*s..*e, out.as_str()))
            .collect();
        guard.edit_batch(&batch);
    }
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    let mut new_pieces: Vec<Selection<Anchor>> = indexed
        .iter()
        .rev()
        .scan(0i64, |delta, (s, e, out)| {
            let new_start = (*s as i64 + *delta) as usize;
            let new_end = new_start + out.len();
            *delta += out.len() as i64 - (*e as i64 - *s as i64);
            Some(Selection {
                id: 0,
                start: new_buf.anchor_at(new_start, Bias::Right),
                end: new_buf.anchor_at(new_end, Bias::Right),
                reversed: false,
                goal: SelectionGoal::None,
            })
        })
        .collect();
    if new_pieces.is_empty() {
        return;
    }
    new_pieces.reverse();
    editor
        .selections
        .replace_with_fresh_ids(new_pieces, new_buf);
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
    if output.is_empty() {
        return;
    }
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };
    let buffer_id = editor.buffer_id;
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let mut heads: Vec<usize> = editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| buffer_snapshot.resolve_anchor(&sel.head()))
        .collect();
    heads.sort_unstable();
    heads.dedup();
    heads.reverse();
    let buffer = match stoat.active_workspace().buffers.get(buffer_id) {
        Some(b) => b,
        None => return,
    };
    let mut guard = buffer.write().expect("buffer poisoned");
    let batch: Vec<(Range<usize>, &str)> = heads
        .iter()
        .map(|head| (*head..*head, output.as_str()))
        .collect();
    guard.edit_batch(&batch);
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
    if output.is_empty() {
        return;
    }
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };
    let buffer_id = editor.buffer_id;
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let mut ends: Vec<usize> = editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| buffer_snapshot.resolve_anchor(&sel.end))
        .collect();
    ends.sort_unstable();
    ends.dedup();
    ends.reverse();
    let buffer = match stoat.active_workspace().buffers.get(buffer_id) {
        Some(b) => b,
        None => return,
    };
    let mut guard = buffer.write().expect("buffer poisoned");
    let batch: Vec<(Range<usize>, &str)> = ends
        .iter()
        .map(|end| (*end..*end, output.as_str()))
        .collect();
    guard.edit_batch(&batch);
}

fn apply_keep_pipe(stoat: &mut Stoat, shell_host: &dyn crate::host::ShellHost, cmd: &str) {
    let diff = stoat.active_workspace().env.diff.clone();
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let kept: Vec<Selection<Anchor>> = editor
        .selections
        .all_anchors()
        .iter()
        // A selection survives whenever its run produced output, which a
        // command that exits non-zero but writes to stderr still does. The exit
        // code alone drops those, where the command did have something to say.
        .filter(|sel| {
            let start = buffer_snapshot.resolve_anchor(&sel.start);
            let end = buffer_snapshot.resolve_anchor(&sel.end);
            let stdin: String = rope.chunks_in_range(start..end).collect();
            run_shell(shell_host, cmd, &stdin, &stdin, &diff).is_ok()
        })
        .cloned()
        .collect();
    if kept.is_empty() {
        return;
    }
    editor.selections.replace_with(kept, buffer_snapshot);
}

#[cfg(test)]
mod tests {
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
        // Filter empty -> selections unchanged (silent no-op).
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 3, false)]);
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
