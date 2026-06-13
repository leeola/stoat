use crate::{
    app::{Stoat, UpdateEffect},
    input_view::{InputView, SubmitTarget},
};
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
    pub(crate) previous_mode: String,
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
    let previous_mode = stoat.mode.clone();
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(ws, executor, SubmitTarget::Shell, "", "prompt", 1);
    stoat.shell_input = Some(ShellInputState {
        input,
        action,
        previous_mode,
    });
    stoat.mode = "prompt".into();
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
    let previous_mode = state.previous_mode.clone();
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    stoat.mode = previous_mode;
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
    let previous_mode = state.previous_mode.clone();
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    stoat.mode = previous_mode;
    true
}

fn apply_pipe(stoat: &mut Stoat, shell_host: &dyn crate::host::ShellHost, cmd: &str) {
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let outputs: Vec<String> = editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| {
            let start = buffer_snapshot.resolve_anchor(&sel.start);
            let end = buffer_snapshot.resolve_anchor(&sel.end);
            let stdin: String = rope.chunks_in_range(start..end).collect();
            shell_host
                .run(cmd, stdin.as_bytes())
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                .unwrap_or_default()
        })
        .collect();
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
        for (s, e, out) in &indexed {
            guard.edit(*s..*e, out);
        }
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
    editor.selections.replace_with(new_pieces, new_buf);
}

fn apply_pipe_to(stoat: &mut Stoat, shell_host: &dyn crate::host::ShellHost, cmd: &str) {
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    for sel in editor.selections.all_anchors() {
        let start = buffer_snapshot.resolve_anchor(&sel.start);
        let end = buffer_snapshot.resolve_anchor(&sel.end);
        let stdin: String = rope.chunks_in_range(start..end).collect();
        let _ = shell_host.run(cmd, stdin.as_bytes());
    }
}

fn apply_insert_output(stoat: &mut Stoat, shell_host: &dyn crate::host::ShellHost, cmd: &str) {
    let output = match shell_host.run(cmd, b"") {
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        Err(_) => return,
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
    for head in &heads {
        guard.edit(*head..*head, &output);
    }
}

fn apply_append_output(stoat: &mut Stoat, shell_host: &dyn crate::host::ShellHost, cmd: &str) {
    let output = match shell_host.run(cmd, b"") {
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        Err(_) => return,
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
    for end in &ends {
        guard.edit(*end..*end, &output);
    }
}

fn apply_keep_pipe(stoat: &mut Stoat, shell_host: &dyn crate::host::ShellHost, cmd: &str) {
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
        .filter(|sel| {
            let start = buffer_snapshot.resolve_anchor(&sel.start);
            let end = buffer_snapshot.resolve_anchor(&sel.end);
            let stdin: String = rope.chunks_in_range(start..end).collect();
            shell_host
                .run(cmd, stdin.as_bytes())
                .map(|out| out.exit_code == 0)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if kept.is_empty() {
        return;
    }
    editor.selections.replace_with(kept, buffer_snapshot);
}
