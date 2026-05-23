//! GUI Run pane ItemView.
//!
//! Pane-hosted entity that wraps a PTY-backed shell session. Holds
//! an [`Editor::auto_height`] input pinned at the bottom, a
//! scrollback of [`OutputBlock`]s (reusing the shared `stoat::run`
//! type), and an `Arc<dyn TerminalSession>` populated in the
//! background by [`TerminalHost::spawn`].
//!
//! `submit` (driven by the `RunSubmit` action) reads the input text,
//! appends a new `OutputBlock` with the submitted command, pushes the
//! command (with trailing newline) to the session -- or queues to
//! `pending_writes` when the session has not landed yet. The queue
//! drains automatically when the session arrives.
//!
//! Once the session is installed and `pending_writes` have drained,
//! the same background task awaits `TerminalSession::read` in a loop
//! and routes each chunk back through [`Run::on_read`], which feeds
//! the bytes into the active block's [`VtermGrid`]. The styled-cell
//! paint of those grids lives in the [`render`] submodule.

pub(crate) mod mouse;
pub(crate) mod render;

use crate::{
    editor::Editor,
    globals::TerminalHostGlobal,
    item::{DeserializeSnafu, ItemError, ItemHandle, ItemView},
    settings::Settings,
    theme::{DEFAULT_EDITOR_FONT_FAMILY, DEFAULT_EDITOR_FONT_SIZE},
    workspace::Workspace,
};
use gpui::{
    canvas, div, font, point, px, size, App, AppContext, Bounds, Context, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, Point, Render,
    SharedString, Size, Styled, Task, WeakEntity, Window,
};
use std::{path::PathBuf, sync::Arc};
use stoat::{
    host::{SpawnArgs, TerminalHost, TerminalSession},
    run::{GridSelection, OutputBlock},
};

const SHELL_WIDTH: u16 = 80;

pub struct Run {
    pub(crate) input: gpui::Entity<Editor>,
    pub(crate) blocks: Vec<OutputBlock>,
    cwd: PathBuf,
    pub(crate) history: Vec<String>,
    /// Recall position into [`Self::history`]. `None` means the
    /// input editor holds live user text; `Some(i)` means it
    /// currently shows `history[i]`, walked back via
    /// [`Self::history_prev`] / [`Self::history_next`]. Reset
    /// to `None` whenever a new command is submitted.
    pub(crate) history_idx: Option<usize>,
    pub(crate) session: Option<Arc<dyn TerminalSession>>,
    /// Submits that arrived before the host returned a live
    /// session. Drained in arrival order once
    /// [`Self::session`] is installed; cleared mid-drain on
    /// the first write failure (the failure is logged at
    /// `tracing::warn` and the queue is dropped rather than
    /// retried).
    pub(crate) pending_writes: Vec<String>,
    workspace: WeakEntity<Workspace>,
    /// Output-column pixel bounds captured by the canvas
    /// prepaint each frame. Mouse handlers subtract the
    /// origin to land in element-local coordinates before
    /// dividing by [`Self::cell_size`].
    output_bounds: Option<Bounds<Pixels>>,
    /// Monospace cell metrics measured during the render's
    /// canvas prepaint via `text_system().em_advance`.
    cell_size: Option<Size<Pixels>>,
    _spawn_task: Option<Task<()>>,
}

impl Run {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let input = cx.new(|cx| Editor::auto_height(1, 8, window, cx));
        let host = cx.global::<TerminalHostGlobal>().0.clone();
        let cwd_clone = cwd.clone();
        let spawn_task = cx.spawn(async move |this, cx| {
            install_session(host, cwd_clone, this, cx).await;
        });
        Self {
            input,
            blocks: Vec::new(),
            cwd,
            history: Vec::new(),
            history_idx: None,
            session: None,
            pending_writes: Vec::new(),
            workspace,
            output_bounds: None,
            cell_size: None,
            _spawn_task: Some(spawn_task),
        }
    }

    pub fn set_output_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<'_, Self>) {
        if self.output_bounds == Some(bounds) {
            return;
        }
        self.output_bounds = Some(bounds);
        cx.notify();
    }

    pub fn set_cell_size(&mut self, size: Size<Pixels>, cx: &mut Context<'_, Self>) {
        if self.cell_size == Some(size) {
            return;
        }
        self.cell_size = Some(size);
        cx.notify();
    }

    fn dispatch_grid_click(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((row, col)) = self.position_to_grid(position) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |w, cx| {
            w.dispatch_action(Box::new(mouse::RunClickAt { row, col }), window, cx);
        });
    }

    fn dispatch_grid_drag(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((row, col)) = self.position_to_grid(position) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |w, cx| {
            w.dispatch_action(Box::new(mouse::RunDragSelectTo { row, col }), window, cx);
        });
    }

    fn position_to_grid(&self, position: Point<Pixels>) -> Option<(u32, u32)> {
        let bounds = self.output_bounds?;
        let cell = self.cell_size?;
        let elem = point(position.x - bounds.origin.x, position.y - bounds.origin.y);
        Some(mouse::point_to_grid(elem, cell))
    }

    pub fn submit(&mut self, cx: &mut Context<'_, Self>) {
        let text = read_input_text(&self.input, cx);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let command = trimmed.to_string();
        clear_input(&self.input, cx);
        self.history.push(command.clone());
        self.history_idx = None;
        self.blocks
            .push(OutputBlock::new(command.clone(), SHELL_WIDTH));
        let line = format!("{command}\n");
        match self.session.as_ref() {
            Some(session) => spawn_write(session.clone(), line, cx),
            None => self.pending_writes.push(line),
        }
        cx.notify();
    }

    /// Send the ETX byte (`\x03`) to the PTY so the terminal
    /// line discipline raises SIGINT against the foreground
    /// process group, interrupting the running command without
    /// killing the shell. No-op when the session has not landed
    /// yet -- nothing is running to interrupt.
    pub fn interrupt(&mut self, cx: &mut Context<'_, Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        spawn_write(session.clone(), "\x03".into(), cx);
    }

    /// Walk one step back through the recall cursor into
    /// [`Self::history`], populating the input editor with that
    /// entry. No-op when history is empty or the cursor is
    /// already at the oldest entry.
    pub fn history_prev(&mut self, cx: &mut Context<'_, Self>) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_idx {
            Some(0) => return,
            Some(i) => i - 1,
            None => self.history.len() - 1,
        };
        self.history_idx = Some(next);
        let entry = self.history[next].clone();
        set_input_text(&self.input, &entry, cx);
        cx.notify();
    }

    /// Walk one step forward through the recall cursor. Past the
    /// newest entry the cursor returns to `None` and the input
    /// clears, restoring the live editing position. No-op when no
    /// entry is currently recalled.
    pub fn history_next(&mut self, cx: &mut Context<'_, Self>) {
        let Some(idx) = self.history_idx else {
            return;
        };
        if idx + 1 < self.history.len() {
            let next = idx + 1;
            self.history_idx = Some(next);
            let entry = self.history[next].clone();
            set_input_text(&self.input, &entry, cx);
        } else {
            self.history_idx = None;
            clear_input(&self.input, cx);
        }
        cx.notify();
    }

    /// Append `chunk` to the active block's `VtermGrid`. Creates a
    /// synthetic empty-command block when bytes arrive before any
    /// submit (e.g. a shell-prompt banner the spawned process emits
    /// before the user types).
    pub fn on_read(&mut self, chunk: &[u8], cx: &mut Context<'_, Self>) {
        if self.blocks.is_empty() {
            self.blocks
                .push(OutputBlock::new(String::new(), SHELL_WIDTH));
        }
        let active = self
            .blocks
            .last_mut()
            .expect("blocks non-empty after push above");
        active.grid.feed(chunk);
        cx.notify();
    }

    /// Seed the active block's selection at `(col, row)`. No-op
    /// when no block exists. Mirrors the TUI mouse-down arm in
    /// `stoat/src/app.rs::handle_run_pane_mouse`.
    pub fn handle_click_at(&mut self, row: u32, col: u32, cx: &mut Context<'_, Self>) {
        let Some(active) = self.blocks.last_mut() else {
            return;
        };
        let pos = (col as u16, row as u16);
        active.selection = Some(GridSelection {
            anchor: pos,
            head: pos,
        });
        cx.notify();
    }

    /// Extend the active block's selection head to `(col, row)`.
    /// No-op when no active block, no prior selection anchor, or
    /// the head did not move. Mirrors the TUI mouse-drag arm.
    pub fn handle_drag_select_to(&mut self, row: u32, col: u32, cx: &mut Context<'_, Self>) {
        let Some(active) = self.blocks.last_mut() else {
            return;
        };
        let Some(selection) = active.selection.as_mut() else {
            return;
        };
        let pos = (col as u16, row as u16);
        if selection.head == pos {
            return;
        }
        selection.head = pos;
        cx.notify();
    }
}

async fn install_session(
    host: Arc<dyn TerminalHost>,
    cwd: PathBuf,
    this: WeakEntity<Run>,
    cx: &mut gpui::AsyncApp,
) {
    let args = SpawnArgs {
        program: "bash".into(),
        args: vec!["--noediting".into(), "--noprofile".into(), "--norc".into()],
        env: vec![
            ("PS1".into(), String::new()),
            ("PS2".into(), String::new()),
            ("TERM".into(), "dumb".into()),
        ],
        cwd,
        width: SHELL_WIDTH,
    };
    let session: Arc<dyn TerminalSession> = match host.spawn(args).await {
        Ok(s) => Arc::from(s),
        Err(err) => {
            tracing::warn!(
                target: "stoat_gui::run_pane",
                ?err,
                "run pane terminal spawn failed"
            );
            return;
        },
    };
    let pending = {
        let Ok(pending) = this.update(cx, |run, _| {
            run.session = Some(session.clone());
            std::mem::take(&mut run.pending_writes)
        }) else {
            return;
        };
        pending
    };
    for text in pending {
        if let Err(err) = session.write(text.as_bytes()).await {
            tracing::warn!(
                target: "stoat_gui::run_pane",
                ?err,
                "run pane pending-write drain failed"
            );
            break;
        }
    }
    let mut buf = [0u8; 4096];
    loop {
        let n = match session.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(
                    target: "stoat_gui::run_pane",
                    ?err,
                    "run pane read loop terminated"
                );
                break;
            },
        };
        let chunk = buf[..n].to_vec();
        if this.update(cx, |run, cx| run.on_read(&chunk, cx)).is_err() {
            break;
        }
    }
}

fn spawn_write(session: Arc<dyn TerminalSession>, text: String, cx: &mut Context<'_, Run>) {
    cx.spawn(async move |_, _| {
        if let Err(err) = session.write(text.as_bytes()).await {
            tracing::warn!(
                target: "stoat_gui::run_pane",
                ?err,
                "run submit write failed"
            );
        }
    })
    .detach();
}

fn read_input_text(input: &gpui::Entity<Editor>, cx: &App) -> String {
    let editor = input.read(cx);
    editor
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .map(|b| b.read(cx).text())
        .unwrap_or_default()
}

fn clear_input(input: &gpui::Entity<Editor>, cx: &mut Context<'_, Run>) {
    set_input_text(input, "", cx);
}

fn set_input_text(input: &gpui::Entity<Editor>, text: &str, cx: &mut Context<'_, Run>) {
    let Some(buffer) = input
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .cloned()
    else {
        return;
    };
    buffer.update(cx, |b, cx| {
        let len = b.text().len();
        if len == 0 && text.is_empty() {
            return;
        }
        b.edit(0..len, text, cx);
    });
}

impl ItemView for Run {
    fn tab_label(&self, _cx: &App) -> SharedString {
        SharedString::from("Run")
    }

    fn deserialize(
        _value: serde_json::Value,
        _cx: &mut Context<'_, Self>,
    ) -> Result<Self, ItemError> {
        DeserializeSnafu {
            reason: "run pane deserialize not yet implemented",
        }
        .fail()
    }

    fn item_kind(&self) -> crate::item::ItemKind {
        crate::item::ItemKind::Run
    }

    fn serialize(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({
            "cwd": self.cwd.to_string_lossy(),
        })
    }
}

impl Render for Run {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let cwd_label = SharedString::from(self.cwd.display().to_string());
        let (font_family, font_size) = editor_font(cx);
        let mut body = div().flex().flex_col().flex_grow().w_full();
        for block in &self.blocks {
            body = body.child(render::render_block(block));
        }
        let bounds_handle = cx.weak_entity();
        let cell_family = font_family.clone();
        let bounds_capture = canvas(
            move |bounds, window, cx| {
                let font_id = window
                    .text_system()
                    .resolve_font(&font(cell_family.clone()));
                let font_size_px = px(font_size);
                let line_height = px((font_size * GPUI_DEFAULT_LINE_HEIGHT_RATIO).round());
                let measured = window
                    .text_system()
                    .em_advance(font_id, font_size_px)
                    .ok()
                    .map(|width| size(width, line_height));
                let _ = bounds_handle.update(cx, |run, cx| {
                    run.set_output_bounds(bounds, cx);
                    if let Some(cell) = measured {
                        run.set_cell_size(cell, cx);
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let output_layer = div()
            .relative()
            .flex_grow()
            .w_full()
            .child(body)
            .child(bounds_capture)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.dispatch_grid_click(event.position, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if event.dragging() {
                    this.dispatch_grid_drag(event.position, window, cx);
                }
            }));
        div()
            .flex()
            .flex_col()
            .size_full()
            .font_family(font_family)
            .text_size(px(font_size))
            .child(div().px_2().py_1().child(cwd_label))
            .child(output_layer)
            .child(self.input.clone())
    }
}

/// Matches the GPUI `TextStyle::default` `line_height` (golden
/// ratio) applied when no `with_text_style` refinement overrides
/// it. The editor uses the same constant for cell-height math; the
/// run pane mirrors it so cached cell rows line up with the
/// rendered grid.
const GPUI_DEFAULT_LINE_HEIGHT_RATIO: f32 = 1.618_034;

fn editor_font(cx: &App) -> (SharedString, f32) {
    let (family, size) = match cx.try_global::<Settings>() {
        Some(settings) => (
            settings.resolved.editor_font_family.clone(),
            settings.resolved.editor_font_size,
        ),
        None => (None, None),
    };
    (
        family
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from(DEFAULT_EDITOR_FONT_FAMILY)),
        size.unwrap_or(DEFAULT_EDITOR_FONT_SIZE),
    )
}

/// Dispatch the [`stoat_action::OpenRun`] action. Creates a fresh
/// [`Run`] entity anchored at the workspace's git root and adds it
/// to the focused pane's item list.
pub fn dispatch_open_run(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let pane_id = workspace.pane_tree().read(cx).focus();
    let Some(pane) = workspace.pane_tree().read(cx).pane(pane_id).cloned() else {
        return;
    };
    let cwd = workspace.git_root().clone();
    let weak_workspace = cx.weak_entity();
    let run = cx.new(|cx| Run::new(weak_workspace, cwd, window, cx));
    pane.update(cx, |p, cx| {
        p.add_item(Box::new(run), cx);
    });
}

/// Dispatch the [`stoat_action::RunSubmit`] action. Finds the focused
/// pane's active item, downcasts to [`Run`], and invokes `submit`.
/// No-op when the active item is not a Run pane.
pub fn dispatch_run_submit(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    with_focused_run(workspace, cx, |r, cx| r.submit(cx));
}

/// Dispatch [`stoat_action::RunHistoryPrev`]. Replaces the focused
/// run pane's input with the previous command in history; no-op
/// when the focused pane is not a run pane or history is exhausted.
pub fn dispatch_run_history_prev(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    with_focused_run(workspace, cx, |r, cx| r.history_prev(cx));
}

/// Dispatch [`stoat_action::RunHistoryNext`]. Walks the run pane's
/// history cursor forward; past the newest entry the input clears
/// and the cursor returns to the live editing position. No-op when
/// the focused pane is not a run pane or no entry is recalled.
pub fn dispatch_run_history_next(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    with_focused_run(workspace, cx, |r, cx| r.history_next(cx));
}

/// Dispatch [`stoat_action::RunInterrupt`]. Sends Ctrl-C to the
/// focused run pane's PTY, interrupting the foreground command.
/// No-op when the focused pane is not a run pane or no session
/// has installed yet.
pub fn dispatch_run_interrupt(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    with_focused_run(workspace, cx, |r, cx| r.interrupt(cx));
}

fn with_focused_run(
    workspace: &mut Workspace,
    cx: &mut Context<'_, Workspace>,
    f: impl FnOnce(&mut Run, &mut Context<'_, Run>),
) {
    let pane_id = workspace.pane_tree().read(cx).focus();
    let Some(pane) = workspace.pane_tree().read(cx).pane(pane_id).cloned() else {
        return;
    };
    let active_view = pane.read(cx).active_item().map(ItemHandle::to_any_view);
    let Some(run) = active_view.and_then(|v| v.downcast::<Run>().ok()) else {
        return;
    };
    run.update(cx, f);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        globals::{
            ClipboardHostGlobal, ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal,
            TerminalHostGlobal,
        },
        workspace::Workspace,
    };
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::sync::Arc;
    use stoat::host::{
        fake::{terminal::FakeTerminalSession, FakeClipboard, FakeFs, FakeTerminalHost},
        ClipboardHost, FsHost, FsWatchHost,
    };
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext, terminal: Arc<FakeTerminalSession>) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        let clipboard: Arc<dyn ClipboardHost> = Arc::new(FakeClipboard::new());
        let terminal_host: Arc<dyn TerminalHost> = Arc::new(FakeTerminalHost::new(terminal));
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(FsHostGlobal(fs));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
            cx.set_global(ClipboardHostGlobal(clipboard));
            cx.set_global(TerminalHostGlobal(terminal_host));
        });
    }

    struct Harness<'a> {
        workspace: Entity<Workspace>,
        terminal: Arc<FakeTerminalSession>,
        vcx: &'a mut VisualTestContext,
    }

    fn new_harness(cx: &mut TestAppContext) -> Harness<'_> {
        let terminal = Arc::new(FakeTerminalSession::new());
        install_globals(cx, terminal.clone());
        let (workspace, vcx) =
            cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/repo"), cx));
        Harness {
            workspace,
            terminal,
            vcx,
        }
    }

    fn open_run(h: &mut Harness<'_>) -> Entity<Run> {
        h.workspace.update_in(h.vcx, |w, window, cx| {
            dispatch_open_run(w, window, cx);
        });
        focused_run(h).expect("run pane is focused active item")
    }

    fn focused_run(h: &mut Harness<'_>) -> Option<Entity<Run>> {
        h.workspace.read_with(h.vcx, |w, cx| {
            let pane_id = w.pane_tree().read(cx).focus();
            let pane = w.pane_tree().read(cx).pane(pane_id).cloned()?;
            let view = pane.read(cx).active_item().map(ItemHandle::to_any_view)?;
            view.downcast::<Run>().ok()
        })
    }

    fn type_into_input(run: &Entity<Run>, h: &mut Harness<'_>, text: &str) {
        let buffer = run.read_with(h.vcx, |r, cx| {
            r.input
                .read(cx)
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("auto-height editor has singleton buffer")
                .clone()
        });
        buffer.update(h.vcx, |b, cx| {
            let len = b.text().len();
            b.edit(0..len, text, cx);
        });
    }

    #[test]
    fn open_run_adds_run_pane_to_focused_pane() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            dispatch_open_run(w, window, cx);
        });

        let run = focused_run(&mut h);
        assert!(
            run.is_some(),
            "OpenRun should add a Run pane to the focused pane"
        );
    }

    #[test]
    fn submit_writes_command_to_session() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);
        h.vcx.run_until_parked();
        let session_ready = run.read_with(h.vcx, |r, _| r.session.is_some());
        assert!(session_ready, "session should install after parking");

        type_into_input(&run, &mut h, "echo hi");
        run.update(h.vcx, |r, cx| r.submit(cx));
        h.vcx.run_until_parked();

        assert_eq!(h.terminal.sent_strings(), vec!["echo hi\n".to_string()]);
    }

    #[test]
    fn submit_queues_when_session_not_ready() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);

        type_into_input(&run, &mut h, "queued");
        run.update(h.vcx, |r, cx| r.submit(cx));

        let (pending, session_present) = run.read_with(h.vcx, |r, _| {
            (r.pending_writes.clone(), r.session.is_some())
        });
        assert_eq!(pending, vec!["queued\n".to_string()]);
        assert!(!session_present, "no session means submits queue");
    }

    #[test]
    fn pending_writes_drain_when_session_arrives() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);

        type_into_input(&run, &mut h, "before-session");
        run.update(h.vcx, |r, cx| r.submit(cx));

        let pending_before = run.read_with(h.vcx, |r, _| r.pending_writes.clone());
        assert_eq!(pending_before, vec!["before-session\n".to_string()]);

        h.vcx.run_until_parked();

        assert_eq!(
            h.terminal.sent_strings(),
            vec!["before-session\n".to_string()]
        );
        let pending_after = run.read_with(h.vcx, |r, _| r.pending_writes.clone());
        assert!(pending_after.is_empty(), "queue drains once session lands");
    }

    #[test]
    fn submit_appends_block_for_grid_render() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);
        h.vcx.run_until_parked();

        type_into_input(&run, &mut h, "ls -la");
        run.update(h.vcx, |r, cx| r.submit(cx));

        let commands: Vec<String> = run.read_with(h.vcx, |r, _| {
            r.blocks.iter().map(|b| b.command.clone()).collect()
        });
        assert_eq!(commands, vec!["ls -la".to_string()]);
    }

    #[test]
    fn read_loop_feeds_bytes_into_active_block_grid() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);
        h.vcx.run_until_parked();

        h.terminal.push_output(b"hello");
        h.vcx.run_until_parked();

        let first_row: String = run.read_with(h.vcx, |r, _| {
            r.blocks
                .last()
                .map(|b| b.grid.row(0).iter().take(5).map(|c| c.ch).collect())
                .unwrap_or_default()
        });
        assert_eq!(first_row, "hello");
    }

    fn input_text(run: &Entity<Run>, h: &mut Harness<'_>) -> String {
        run.read_with(h.vcx, |r, cx| read_input_text(&r.input, cx))
    }

    fn submit(run: &Entity<Run>, h: &mut Harness<'_>, text: &str) {
        type_into_input(run, h, text);
        run.update(h.vcx, |r, cx| r.submit(cx));
    }

    fn history_state(run: &Entity<Run>, h: &mut Harness<'_>) -> (Vec<String>, Option<usize>) {
        run.read_with(h.vcx, |r, _| (r.history.clone(), r.history_idx))
    }

    #[test]
    fn history_prev_walks_back_newest_first() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);
        h.vcx.run_until_parked();

        submit(&run, &mut h, "ls");
        submit(&run, &mut h, "pwd");

        run.update(h.vcx, |r, cx| r.history_prev(cx));
        assert_eq!(input_text(&run, &mut h), "pwd");
        assert_eq!(history_state(&run, &mut h).1, Some(1));

        run.update(h.vcx, |r, cx| r.history_prev(cx));
        assert_eq!(input_text(&run, &mut h), "ls");
        assert_eq!(history_state(&run, &mut h).1, Some(0));
    }

    #[test]
    fn history_prev_at_oldest_is_noop() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);
        h.vcx.run_until_parked();

        submit(&run, &mut h, "only");
        run.update(h.vcx, |r, cx| r.history_prev(cx));
        run.update(h.vcx, |r, cx| r.history_prev(cx));

        assert_eq!(input_text(&run, &mut h), "only");
        assert_eq!(history_state(&run, &mut h).1, Some(0));
    }

    #[test]
    fn history_prev_empty_history_is_noop() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);
        h.vcx.run_until_parked();

        type_into_input(&run, &mut h, "draft");
        run.update(h.vcx, |r, cx| r.history_prev(cx));

        assert_eq!(input_text(&run, &mut h), "draft");
        assert_eq!(history_state(&run, &mut h), (vec![], None));
    }

    #[test]
    fn history_next_walks_forward_and_returns_to_live() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);
        h.vcx.run_until_parked();

        submit(&run, &mut h, "ls");
        submit(&run, &mut h, "pwd");

        run.update(h.vcx, |r, cx| r.history_prev(cx));
        run.update(h.vcx, |r, cx| r.history_prev(cx));
        assert_eq!(input_text(&run, &mut h), "ls");

        run.update(h.vcx, |r, cx| r.history_next(cx));
        assert_eq!(input_text(&run, &mut h), "pwd");
        assert_eq!(history_state(&run, &mut h).1, Some(1));

        run.update(h.vcx, |r, cx| r.history_next(cx));
        assert_eq!(input_text(&run, &mut h), "");
        assert_eq!(history_state(&run, &mut h).1, None);
    }

    #[test]
    fn history_next_at_live_is_noop() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);
        h.vcx.run_until_parked();

        submit(&run, &mut h, "ls");
        type_into_input(&run, &mut h, "draft");
        run.update(h.vcx, |r, cx| r.history_next(cx));

        assert_eq!(input_text(&run, &mut h), "draft");
        assert_eq!(history_state(&run, &mut h).1, None);
    }

    #[test]
    fn submit_resets_history_cursor() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);
        h.vcx.run_until_parked();

        submit(&run, &mut h, "ls");
        submit(&run, &mut h, "pwd");
        run.update(h.vcx, |r, cx| r.history_prev(cx));
        assert_eq!(history_state(&run, &mut h).1, Some(1));

        submit(&run, &mut h, "echo");
        assert_eq!(
            history_state(&run, &mut h),
            (vec!["ls".into(), "pwd".into(), "echo".into()], None)
        );
    }

    #[test]
    fn interrupt_sends_etx_byte_to_session() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);
        h.vcx.run_until_parked();

        run.update(h.vcx, |r, cx| r.interrupt(cx));
        h.vcx.run_until_parked();

        assert_eq!(h.terminal.sent_bytes(), vec![vec![0x03u8]]);
    }

    #[test]
    fn interrupt_before_session_is_noop() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let run = open_run(&mut h);

        run.update(h.vcx, |r, cx| r.interrupt(cx));

        assert!(h.terminal.sent_bytes().is_empty());
    }
}
