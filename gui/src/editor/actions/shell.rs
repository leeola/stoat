//! Shell-integration action handlers.
//!
//! Each `handle_shell_*` opener pushes a [`ShellInputModal`] onto
//! the workspace's modal layer; the modal's confirm path runs the
//! typed command via [`crate::globals::ShellHostGlobal`] and
//! dispatches into the matching `apply_*` function here. Mirrors
//! the TUI's `stoat/src/action_handlers/shell.rs` per-action
//! semantics: `Pipe` replaces selection bodies with stdout,
//! `PipeTo` is side-effect-only, `InsertOutput` /
//! `AppendOutput` insert a one-shot command's output at each
//! cursor or selection end, and `KeepPipe` filters selections by
//! exit code.

use crate::{editor::Editor, globals::ShellHostGlobal, workspace::Workspace};
use gpui::{Context, Entity, Window};
use stoat::host::ShellHost;
use stoat_text::{Anchor, Bias, Selection, SelectionGoal};

/// Which shell-integration operation a [`ShellInputModal`] will
/// perform on submit. Mirrors `stoat::action_handlers::shell::ShellAction`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ShellAction {
    Pipe,
    PipeTo,
    InsertOutput,
    AppendOutput,
    KeepPipe,
}

pub fn handle_shell_pipe(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    open_shell_modal(workspace, ShellAction::Pipe, window, cx);
}

pub fn handle_shell_pipe_to(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    open_shell_modal(workspace, ShellAction::PipeTo, window, cx);
}

pub fn handle_shell_insert_output(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    open_shell_modal(workspace, ShellAction::InsertOutput, window, cx);
}

pub fn handle_shell_append_output(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    open_shell_modal(workspace, ShellAction::AppendOutput, window, cx);
}

pub fn handle_shell_keep_pipe(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    open_shell_modal(workspace, ShellAction::KeepPipe, window, cx);
}

fn open_shell_modal(
    workspace: &mut Workspace,
    action: ShellAction,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let weak = cx.weak_entity();
    workspace.show_shell_input_modal(action, weak, window, cx);
}

/// Dispatch helper used by [`ShellInputModal::confirm`] to fan out
/// to the matching `apply_*` function.
pub fn apply(
    workspace: &mut Workspace,
    action: ShellAction,
    cmd: &str,
    cx: &mut Context<'_, Workspace>,
) {
    if cmd.trim().is_empty() {
        return;
    }
    match action {
        ShellAction::Pipe => apply_pipe(workspace, cmd, cx),
        ShellAction::PipeTo => apply_pipe_to(workspace, cmd, cx),
        ShellAction::InsertOutput => apply_insert_output(workspace, cmd, cx),
        ShellAction::AppendOutput => apply_append_output(workspace, cmd, cx),
        ShellAction::KeepPipe => apply_keep_pipe(workspace, cmd, cx),
    }
}

fn active_editor(workspace: &Workspace, cx: &Context<'_, Workspace>) -> Option<Entity<Editor>> {
    workspace
        .input_state_machine()
        .read(cx)
        .active_editor()
        .cloned()
        .and_then(|w| w.upgrade())
}

fn shell_host(cx: &mut Context<'_, Workspace>) -> std::sync::Arc<dyn ShellHost> {
    cx.global::<ShellHostGlobal>().0.clone()
}

fn selection_ranges(editor: &Editor, cx: &Context<'_, Workspace>) -> Vec<(usize, usize)> {
    let snapshot = editor.multi_buffer().read(cx).snapshot();
    editor
        .selections()
        .all_anchors()
        .iter()
        .map(|sel| {
            let start = snapshot.resolve_anchor(&sel.start);
            let end = snapshot.resolve_anchor(&sel.end);
            if start <= end {
                (start, end)
            } else {
                (end, start)
            }
        })
        .collect()
}

fn read_range_text(editor: &Editor, range: (usize, usize), cx: &Context<'_, Workspace>) -> String {
    let snapshot = editor.multi_buffer().read(cx).snapshot();
    let rope = snapshot.rope();
    rope.chunks_in_range(range.0..range.1).collect()
}

pub fn apply_pipe(workspace: &mut Workspace, cmd: &str, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let host = shell_host(cx);
    let ranges = selection_ranges(editor.read(cx), cx);
    if ranges.is_empty() {
        return;
    }
    let outputs: Vec<String> = ranges
        .iter()
        .map(|range| {
            let stdin = read_range_text(editor.read(cx), *range, cx);
            host.run(cmd, stdin.as_bytes())
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                .unwrap_or_default()
        })
        .collect();
    let Some(buffer) = editor
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .cloned()
    else {
        return;
    };
    let mut indexed: Vec<(usize, usize, String)> = ranges
        .iter()
        .zip(outputs)
        .map(|(&(s, e), out)| (s, e, out))
        .collect();
    indexed.sort_by_key(|(s, _, _)| std::cmp::Reverse(*s));
    buffer.update(cx, |b, cx| {
        for (s, e, out) in &indexed {
            b.edit(*s..*e, out.as_str(), cx);
        }
    });
    let new_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
    let mut new_pieces: Vec<Selection<Anchor>> = indexed
        .iter()
        .rev()
        .scan(0i64, |delta, (s, e, out)| {
            let new_start = (*s as i64 + *delta) as usize;
            let new_end = new_start + out.len();
            *delta += out.len() as i64 - (*e as i64 - *s as i64);
            Some(Selection {
                id: 0,
                start: new_snapshot.anchor_at(new_start, Bias::Right),
                end: new_snapshot.anchor_at(new_end, Bias::Right),
                reversed: false,
                goal: SelectionGoal::None,
            })
        })
        .collect();
    if new_pieces.is_empty() {
        return;
    }
    new_pieces.reverse();
    editor.update(cx, |ed, _| {
        ed.selections_mut().replace_with(new_pieces, &new_snapshot);
    });
}

pub fn apply_pipe_to(workspace: &mut Workspace, cmd: &str, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let host = shell_host(cx);
    let editor_ref = editor.read(cx);
    let ranges = selection_ranges(editor_ref, cx);
    for range in ranges {
        let stdin = read_range_text(editor_ref, range, cx);
        let _ = host.run(cmd, stdin.as_bytes());
    }
}

pub fn apply_insert_output(workspace: &mut Workspace, cmd: &str, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let host = shell_host(cx);
    let output = match host.run(cmd, b"") {
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        Err(_) => return,
    };
    if output.is_empty() {
        return;
    }
    let Some(buffer) = editor
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .cloned()
    else {
        return;
    };
    let snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
    let mut heads: Vec<usize> = editor
        .read(cx)
        .selections()
        .all_anchors()
        .iter()
        .map(|sel| snapshot.resolve_anchor(&sel.head()))
        .collect();
    heads.sort_unstable();
    heads.dedup();
    heads.reverse();
    buffer.update(cx, |b, cx| {
        for head in &heads {
            b.edit(*head..*head, output.as_str(), cx);
        }
    });
}

pub fn apply_append_output(workspace: &mut Workspace, cmd: &str, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let host = shell_host(cx);
    let output = match host.run(cmd, b"") {
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        Err(_) => return,
    };
    if output.is_empty() {
        return;
    }
    let Some(buffer) = editor
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .cloned()
    else {
        return;
    };
    let snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
    let mut ends: Vec<usize> = editor
        .read(cx)
        .selections()
        .all_anchors()
        .iter()
        .map(|sel| snapshot.resolve_anchor(&sel.end))
        .collect();
    ends.sort_unstable();
    ends.dedup();
    ends.reverse();
    buffer.update(cx, |b, cx| {
        for end in &ends {
            b.edit(*end..*end, output.as_str(), cx);
        }
    });
}

pub fn apply_keep_pipe(workspace: &mut Workspace, cmd: &str, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let host = shell_host(cx);
    let snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
    let kept: Vec<Selection<Anchor>> = editor
        .read(cx)
        .selections()
        .all_anchors()
        .iter()
        .filter(|sel| {
            let start = snapshot.resolve_anchor(&sel.start);
            let end = snapshot.resolve_anchor(&sel.end);
            let rope = snapshot.rope();
            let stdin: String = rope.chunks_in_range(start..end).collect();
            host.run(cmd, stdin.as_bytes())
                .map(|out| out.exit_code == 0)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if kept.is_empty() {
        return;
    }
    editor.update(cx, |ed, _| {
        ed.selections_mut().replace_with(kept, &snapshot);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        globals::ExecutorGlobal, multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, TestAppContext, VisualTestContext};
    use std::sync::Arc;
    use stoat::buffer::BufferId;
    use stoat_host::{FakeShell, ShellOutput};
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext) -> Arc<FakeShell> {
        let fake = Arc::new(FakeShell::new());
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let shell = fake.clone() as Arc<dyn ShellHost>;
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(ShellHostGlobal(shell));
        });
        fake
    }

    fn new_workspace_in_window<'a>(
        cx: &'a mut TestAppContext,
        text: &str,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext, Entity<Editor>) {
        let (workspace, vcx) = cx.add_window_view(|_window, cx| {
            Workspace::new("main", std::path::PathBuf::from("/tmp/repo"), cx)
        });
        let editor = build_editor(vcx, text);
        let weak = editor.downgrade();
        workspace.update(vcx, |w, cx| {
            w.input_state_machine()
                .clone()
                .update(cx, |sm, _| sm.set_active_editor(Some(weak)));
        });
        vcx.run_until_parked();
        (workspace, vcx, editor)
    }

    fn build_editor(vcx: &mut VisualTestContext, text: &str) -> Entity<Editor> {
        let buffer =
            vcx.update(|_window, cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let multi_buffer = {
            let buffer = buffer.clone();
            vcx.update(|_window, cx| cx.new(|cx| MultiBuffer::singleton(buffer, cx)))
        };
        let display_map = {
            let buffer = buffer.clone();
            vcx.update(|_window, cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        let diff_map = vcx.update(|_window, cx| cx.new(|cx| DiffMap::new(buffer, cx)));
        vcx.update(|_window, cx| {
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        })
    }

    fn set_selections(
        editor: &Entity<Editor>,
        vcx: &mut VisualTestContext,
        ranges: &[(usize, usize)],
    ) {
        editor.update(vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let selections: Vec<Selection<Anchor>> = ranges
                .iter()
                .enumerate()
                .map(|(i, &(s, e))| Selection {
                    id: i + 1,
                    start: snapshot.anchor_at(s, Bias::Right),
                    end: snapshot.anchor_at(e, Bias::Right),
                    reversed: false,
                    goal: SelectionGoal::None,
                })
                .collect();
            ed.selections_mut().replace_with(selections, &snapshot);
        });
    }

    fn buffer_text(editor: &Entity<Editor>, vcx: &VisualTestContext) -> String {
        editor.read_with(vcx, |ed, cx| {
            ed.multi_buffer().read(cx).snapshot().rope().to_string()
        })
    }

    #[test]
    fn apply_pipe_replaces_selection_with_stdout() {
        let mut cx = TestAppContext::single();
        let fake = install_globals(&mut cx);
        fake.set_response(
            "tr a-z A-Z",
            ShellOutput {
                stdout: b"HELLO".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        let (workspace, vcx, editor) = new_workspace_in_window(&mut cx, "hello world");
        set_selections(&editor, vcx, &[(0, 5)]);

        workspace.update(vcx, |w, cx| apply_pipe(w, "tr a-z A-Z", cx));
        vcx.run_until_parked();

        assert_eq!(buffer_text(&editor, vcx), "HELLO world");
    }

    #[test]
    fn apply_pipe_to_runs_command_without_buffer_change() {
        let mut cx = TestAppContext::single();
        let fake = install_globals(&mut cx);
        let (workspace, vcx, editor) = new_workspace_in_window(&mut cx, "hello");
        set_selections(&editor, vcx, &[(0, 5)]);

        workspace.update(vcx, |w, cx| apply_pipe_to(w, "any-cmd", cx));
        vcx.run_until_parked();

        assert_eq!(buffer_text(&editor, vcx), "hello");
        let inv = fake.invocations();
        assert_eq!(inv.len(), 1);
        assert_eq!(inv[0].stdin, b"hello");
    }

    #[test]
    fn apply_insert_output_inserts_at_cursor() {
        let mut cx = TestAppContext::single();
        let fake = install_globals(&mut cx);
        fake.set_response(
            "date",
            ShellOutput {
                stdout: b"NOW".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        let (workspace, vcx, editor) = new_workspace_in_window(&mut cx, "xy");
        set_selections(&editor, vcx, &[(1, 1)]);

        workspace.update(vcx, |w, cx| apply_insert_output(w, "date", cx));
        vcx.run_until_parked();

        assert_eq!(buffer_text(&editor, vcx), "xNOWy");
    }

    #[test]
    fn apply_append_output_inserts_after_selection_end() {
        let mut cx = TestAppContext::single();
        let fake = install_globals(&mut cx);
        fake.set_response(
            "date",
            ShellOutput {
                stdout: b"NOW".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        let (workspace, vcx, editor) = new_workspace_in_window(&mut cx, "hello world");
        set_selections(&editor, vcx, &[(0, 5)]);

        workspace.update(vcx, |w, cx| apply_append_output(w, "date", cx));
        vcx.run_until_parked();

        assert_eq!(buffer_text(&editor, vcx), "helloNOW world");
    }

    #[test]
    fn apply_keep_pipe_drops_when_exit_nonzero() {
        let mut cx = TestAppContext::single();
        let fake = install_globals(&mut cx);
        fake.set_response(
            "false",
            ShellOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 1,
            },
        );
        let (workspace, vcx, editor) = new_workspace_in_window(&mut cx, "abc");
        set_selections(&editor, vcx, &[(0, 3)]);

        workspace.update(vcx, |w, cx| apply_keep_pipe(w, "false", cx));
        vcx.run_until_parked();

        assert_eq!(buffer_text(&editor, vcx), "abc");
    }
}
