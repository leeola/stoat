//! One-shot run overlay: a modal that spawns a single shell command,
//! streams its output into a terminal grid, and shows it until
//! dismissed. Opened by the `Run` action. Distinct from the persistent,
//! interactive run pane ([`crate::run_pane`]); this overlay takes no
//! input and runs exactly one command.

use crate::{
    globals::TerminalHostGlobal, modal_layer::ModalView, run_pane::render, theme::ActiveTheme,
};
use gpui::{
    div, App, AsyncApp, Context, DismissEvent, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Task, WeakEntity,
    Window,
};
use std::{path::PathBuf, sync::Arc};
use stoat::{
    host::{SpawnArgs, TerminalHost, TerminalSession},
    run::OutputBlock,
};
use stoat_action::ActionKind;

const SHELL_WIDTH: u16 = 80;

/// Modal overlay running a single command. Holds the streamed output as
/// terminal-grid [`OutputBlock`]s (rendered with the run pane's shared
/// [`render::render_block`]) and a flag set once the process exits.
pub struct RunModal {
    command: String,
    blocks: Vec<OutputBlock>,
    focus_handle: FocusHandle,
    finished: bool,
    _spawn_task: Option<Task<()>>,
}

impl RunModal {
    /// Open a modal that runs `command` in `cwd` and streams its output.
    /// The command is spawned through the [`TerminalHost`] global; the
    /// read loop feeds output into the block until the process exits.
    pub fn new(command: String, cwd: PathBuf, cx: &mut Context<'_, Self>) -> Self {
        let blocks = vec![OutputBlock::new(command.clone(), SHELL_WIDTH)];
        let host = cx.global::<TerminalHostGlobal>().0.clone();
        let spawn_command = command.clone();
        let spawn_task = cx.spawn(async move |this, cx| {
            run_oneshot(host, spawn_command, cwd, this, cx).await;
        });
        Self {
            command,
            blocks,
            focus_handle: cx.focus_handle(),
            finished: false,
            _spawn_task: Some(spawn_task),
        }
    }

    fn on_read(&mut self, chunk: &[u8], cx: &mut Context<'_, Self>) {
        if let Some(active) = self.blocks.last_mut() {
            active.grid.feed(chunk);
        }
        cx.notify();
    }

    fn finish(&mut self, cx: &mut Context<'_, Self>) {
        self.finished = true;
        cx.notify();
    }

    fn dismiss(&mut self, cx: &mut Context<'_, Self>) -> bool {
        cx.emit(DismissEvent);
        true
    }

    #[cfg(test)]
    fn output_text(&self) -> String {
        self.blocks
            .iter()
            .map(|block| {
                block
                    .grid
                    .text_in(0..block.grid.width() as usize, 0..block.grid.line_count())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Spawn `command` as a one-shot `bash -c` process and drain its output
/// into the modal until EOF. Failures are logged and finish the modal so
/// it does not appear stuck "running".
async fn run_oneshot(
    host: Arc<dyn TerminalHost>,
    command: String,
    cwd: PathBuf,
    this: WeakEntity<RunModal>,
    cx: &mut AsyncApp,
) {
    let args = SpawnArgs {
        program: "bash".into(),
        args: vec!["--noprofile".into(), "--norc".into(), "-c".into(), command],
        env: vec![("TERM".into(), "dumb".into())],
        cwd,
        width: SHELL_WIDTH,
    };
    let session: Arc<dyn TerminalSession> = match host.spawn(args).await {
        Ok(session) => Arc::from(session),
        Err(err) => {
            tracing::warn!(target: "stoat_gui::run_modal", ?err, "run overlay spawn failed");
            let _ = this.update(cx, |modal, cx| modal.finish(cx));
            return;
        },
    };

    let mut buf = [0u8; 4096];
    loop {
        let n = match session.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(target: "stoat_gui::run_modal", ?err, "run overlay read failed");
                break;
            },
        };
        let chunk = buf[..n].to_vec();
        if this
            .update(cx, |modal, cx| modal.on_read(&chunk, cx))
            .is_err()
        {
            return;
        }
    }
    let _ = this.update(cx, |modal, cx| modal.finish(cx));
}

impl Render for RunModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let status = if self.finished { "done" } else { "running" };
        let header = SharedString::from(format!("run ({status}): {}", self.command));
        let accent = cx.theme().ui_modal_run;
        let mut body = div()
            .flex()
            .flex_col()
            .size_full()
            .border_t_1()
            .border_color(accent)
            .track_focus(&self.focus_handle)
            .child(div().px_2().py_1().text_color(accent).child(header));
        for block in &self.blocks {
            body = body.child(render::render_block(block));
        }
        body
    }
}

impl Focusable for RunModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for RunModal {}

impl ModalView for RunModal {
    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match action.kind() {
            ActionKind::DismissModal => self.dismiss(cx),
            _ => false,
        }
    }

    fn submit_prompt(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        self.dismiss(cx)
    }

    fn cancel_prompt(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        self.dismiss(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::ExecutorGlobal;
    use gpui::TestAppContext;
    use stoat::host::fake::terminal::{FakeTerminalHost, FakeTerminalSession};
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext, session: Arc<FakeTerminalSession>) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let terminal: Arc<dyn TerminalHost> = Arc::new(FakeTerminalHost::new(session));
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(TerminalHostGlobal(terminal));
        });
    }

    #[test]
    fn streams_command_output_into_the_overlay() {
        let mut cx = TestAppContext::single();
        let session = Arc::new(FakeTerminalSession::new());
        install_globals(&mut cx, session.clone());
        let (modal, vcx) = cx.add_window_view(|_window, cx| {
            RunModal::new("echo hi".into(), PathBuf::from("/repo"), cx)
        });
        vcx.run_until_parked();

        session.push_output(b"hi\r\n");
        vcx.run_until_parked();

        let output = modal.read_with(vcx, |m, _| m.output_text());
        assert!(output.contains("hi"), "overlay output was {output:?}");
        assert_eq!(
            modal.read_with(vcx, |m, _| m.command.clone()),
            "echo hi".to_string()
        );
    }

    #[test]
    fn dismiss_emits_dismiss_event() {
        let mut cx = TestAppContext::single();
        let session = Arc::new(FakeTerminalSession::new());
        install_globals(&mut cx, session);
        let (modal, vcx) = cx.add_window_view(|_window, cx| {
            RunModal::new("echo hi".into(), PathBuf::from("/repo"), cx)
        });

        let handled = modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::DismissModal, window, cx)
        });
        assert!(handled, "DismissModal must be handled");
    }
}
