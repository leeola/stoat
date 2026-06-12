//! GUI full-screen terminal ItemView (Way-2 embedded terminal).
//!
//! Pane-hosted entity wrapping a single PTY-backed [`TerminalSession`]
//! and one [`VtermGrid`] rendered to fill the pane. Unlike the
//! Warp-style [`crate::run_pane::Run`], it has no command blocks and no
//! command-line input: the grid is the whole surface, so an interactive
//! TUI running in the PTY paints directly into it.
//!
//! [`Terminal::with_command`] spawns the session in the background via
//! [`TerminalHost`]; once installed, the same task loops on
//! [`TerminalSession::read`] and feeds each chunk into the grid through
//! [`Terminal::on_read`]. The render measures cell metrics in its canvas
//! prepaint and resizes the PTY to the whole-cell grid, and forwards
//! mouse events to the program as reports. The styled-cell paint reuses
//! [`crate::run_pane::render`].

use crate::{
    globals::{ClipboardHostGlobal, EnvHostGlobal, TerminalHostGlobal},
    item::{DeserializeSnafu, ItemError, ItemKind, ItemView},
    run_pane::{
        editor_font, mouse, mouse_button_code,
        render::{render_grid_row, CursorRender},
        scroll_is_up, terminal_cells, GPUI_DEFAULT_LINE_HEIGHT_RATIO,
    },
    workspace::Workspace,
};
use gpui::{
    canvas, div, font, point, px, size, App, AppContext, Bounds, Context, FocusHandle, Focusable,
    InteractiveElement, IntoElement, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollWheelEvent, SharedString, Size,
    Styled, Task, WeakEntity, Window,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use stoat::{
    host::{SpawnArgs, TerminalHost, TerminalSession},
    run::{MouseProtocol, VtermGrid},
};

/// Fixed grid column count. [`VtermGrid`] has no in-place resize, so --
/// like the run pane's blocks -- the grid stays this wide while the PTY
/// is resized to the measured cell dimensions.
const TERMINAL_WIDTH: u16 = 80;

/// Minimum interval between foreground-process-name polls.
const FOREGROUND_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Known agent CLIs recognized in the terminal tab label. Matched against
/// the foreground process's command name.
const KNOWN_AGENTS: &[&str] = &[
    "claude", "codex", "aider", "gemini", "copilot", "crush", "amp",
];

/// The canonical agent name if `process_name` is a recognized agent CLI.
fn matched_agent(process_name: &str) -> Option<&'static str> {
    KNOWN_AGENTS
        .iter()
        .copied()
        .find(|&agent| agent == process_name)
}

/// The final component of `path`, used as a compact label for the cwd.
fn cwd_basename(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

pub(crate) struct Terminal {
    grid: VtermGrid,
    session: Option<Arc<dyn TerminalSession>>,
    cwd: PathBuf,
    /// Program and arguments the session was launched with. Read back by
    /// [`install_session`] to build the spawn args.
    program: String,
    args: Vec<String>,
    workspace: WeakEntity<Workspace>,
    /// Pixel bounds of the grid surface captured by the canvas
    /// prepaint each frame; mouse handlers subtract the origin before
    /// dividing by [`Self::cell_size`].
    output_bounds: Option<Bounds<Pixels>>,
    /// Monospace cell metrics measured during the render's canvas
    /// prepaint via `text_system().em_advance`.
    cell_size: Option<Size<Pixels>>,
    /// Last `(rows, cols)` pushed to the PTY; resizes are skipped while
    /// unchanged so sub-cell pixel jitter does not spam SIGWINCH.
    last_terminal_size: Option<(u16, u16)>,
    /// Cached foreground process name driving the tab label, refreshed
    /// (throttled) as output arrives. `foreground_checked_at` bounds the
    /// refresh rate so a chatty program does not poll every chunk.
    foreground_name: Option<String>,
    foreground_checked_at: Option<Instant>,
    /// Focus handle so the terminal is a focusable element: gpui routes
    /// IME input to the focused element, and the keystroke pipeline runs
    /// at the window level once anything in the window holds focus.
    focus_handle: FocusHandle,
    _spawn_task: Option<Task<()>>,
}

impl Terminal {
    /// Open a terminal running `program` with `args` in `cwd`.
    pub(crate) fn with_command(
        workspace: WeakEntity<Workspace>,
        cwd: PathBuf,
        program: String,
        args: Vec<String>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let host = cx.global::<TerminalHostGlobal>().0.clone();
        let spawn_task = cx.spawn(async move |this, cx| {
            install_session(host, this, cx).await;
        });
        Self {
            grid: VtermGrid::new(TERMINAL_WIDTH),
            session: None,
            cwd,
            program,
            args,
            workspace,
            output_bounds: None,
            cell_size: None,
            last_terminal_size: None,
            foreground_name: None,
            foreground_checked_at: None,
            focus_handle: cx.focus_handle(),
            _spawn_task: Some(spawn_task),
        }
    }

    /// Forward `bytes` to the PTY off the foreground thread. Used by the
    /// input pipeline to write encoded keystrokes to the focused
    /// terminal; a no-op until the session is installed.
    pub(crate) fn write_to_pty(&self, bytes: Vec<u8>, cx: &mut Context<'_, Self>) {
        if let Some(session) = self.session.clone() {
            spawn_write_bytes(session, bytes, cx);
        }
    }

    /// Read the system clipboard and write it to the PTY, wrapped in
    /// bracketed-paste markers when the program enabled `?2004h` (see
    /// [`VtermGrid::wrap_paste`]). A no-op when the clipboard is empty or
    /// unavailable.
    pub(crate) fn paste(&self, cx: &mut Context<'_, Self>) {
        let Some(clipboard) = cx.try_global::<ClipboardHostGlobal>().map(|g| g.0.clone()) else {
            return;
        };
        let text = match clipboard.get() {
            Ok(Some(text)) if !text.is_empty() => text,
            Ok(_) => return,
            Err(err) => {
                tracing::warn!(
                    target: "stoat_gui::terminal_view",
                    ?err,
                    "terminal clipboard get failed"
                );
                return;
            },
        };
        let bytes = self.grid.wrap_paste(&text);
        self.write_to_pty(bytes, cx);
    }

    pub(crate) fn set_output_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<'_, Self>) {
        if self.output_bounds == Some(bounds) {
            return;
        }
        self.output_bounds = Some(bounds);
        self.sync_terminal_size();
        cx.notify();
    }

    pub(crate) fn set_cell_size(&mut self, size: Size<Pixels>, cx: &mut Context<'_, Self>) {
        if self.cell_size == Some(size) {
            return;
        }
        self.cell_size = Some(size);
        self.sync_terminal_size();
        cx.notify();
    }

    /// Append `chunk` to the grid and request a repaint.
    pub(crate) fn on_read(&mut self, chunk: &[u8], cx: &mut Context<'_, Self>) {
        self.grid.feed(chunk);
        self.refresh_foreground_name();
        cx.notify();
    }

    /// Re-read the session's foreground process name, throttled to at most
    /// once per [`FOREGROUND_POLL_INTERVAL`] so a chatty program does not
    /// poll on every output chunk.
    fn refresh_foreground_name(&mut self) {
        let now = Instant::now();
        if self
            .foreground_checked_at
            .is_some_and(|at| now.duration_since(at) < FOREGROUND_POLL_INTERVAL)
        {
            return;
        }
        self.foreground_checked_at = Some(now);
        if let Some(session) = &self.session {
            self.foreground_name = session.foreground_process_name();
        }
    }

    /// Resize the PTY to match the current bounds and cell size, skipping
    /// the call when the whole-cell dimensions are unchanged.
    fn sync_terminal_size(&mut self) {
        let (Some(bounds), Some(cell)) = (self.output_bounds, self.cell_size) else {
            return;
        };
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(size) = terminal_cells(bounds, cell) else {
            return;
        };
        if self.last_terminal_size == Some(size) {
            return;
        }
        self.last_terminal_size = Some(size);
        let (rows, cols) = size;
        self.grid.resize(cols);
        if let Err(err) = session.resize(rows, cols) {
            tracing::warn!(
                target: "stoat_gui::terminal_view",
                ?err,
                "terminal pty resize failed"
            );
        }
    }

    fn position_to_grid(&self, position: Point<Pixels>) -> Option<(u32, u32)> {
        let bounds = self.output_bounds?;
        let cell = self.cell_size?;
        let elem = point(position.x - bounds.origin.x, position.y - bounds.origin.y);
        Some(mouse::point_to_grid(elem, cell))
    }

    /// Whether this terminal is the workspace's focused pane item, so the
    /// render draws a filled rather than hollow cursor.
    fn is_pane_focused(&self, cx: &Context<'_, Self>) -> bool {
        self.workspace
            .upgrade()
            .is_some_and(|ws| ws.read(cx).active_item_id() == Some(cx.entity_id()))
    }

    /// Forward a mouse event to the program as a report when the grid
    /// enabled a mouse mode and Shift is not held. Returns whether the
    /// event was consumed. `button` is the base button or scroll code;
    /// `motion` marks drag/move events, reported only under button-event
    /// and any-event tracking.
    fn report_mouse(
        &mut self,
        button: u8,
        motion: bool,
        pressed: bool,
        position: Point<Pixels>,
        modifiers: Modifiers,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let reporting = match self.grid.mouse_protocol() {
            MouseProtocol::None => false,
            MouseProtocol::Press => !motion,
            MouseProtocol::ButtonEvent | MouseProtocol::AnyEvent => true,
        };
        if !reporting || modifiers.shift {
            return false;
        }
        let Some((row, col)) = self.position_to_grid(position) else {
            return false;
        };
        let mut mods = 0;
        if modifiers.alt {
            mods += 8;
        }
        if modifiers.control {
            mods += 16;
        }
        let code = button + if motion { 32 } else { 0 };
        let Some(bytes) = self
            .grid
            .encode_mouse(code, mods, col as u16, row as u16, pressed)
        else {
            return false;
        };
        if let Some(session) = self.session.clone() {
            spawn_write_bytes(session, bytes, cx);
        }
        true
    }
}

async fn install_session(
    host: Arc<dyn TerminalHost>,
    this: WeakEntity<Terminal>,
    cx: &mut gpui::AsyncApp,
) {
    let Ok((program, args, cwd)) = this.update(cx, |term, _| {
        (term.program.clone(), term.args.clone(), term.cwd.clone())
    }) else {
        return;
    };
    let spawn_args = SpawnArgs {
        program,
        args,
        env: vec![("TERM".into(), "xterm-256color".into())],
        cwd,
        width: TERMINAL_WIDTH,
        rows: 24,
    };
    let session: Arc<dyn TerminalSession> = match host.spawn(spawn_args).await {
        Ok(s) => Arc::from(s),
        Err(err) => {
            tracing::warn!(
                target: "stoat_gui::terminal_view",
                ?err,
                "terminal spawn failed"
            );
            return;
        },
    };
    if this
        .update(cx, |term, _| {
            term.session = Some(session.clone());
            term.sync_terminal_size();
        })
        .is_err()
    {
        return;
    }
    let mut buf = [0u8; 4096];
    loop {
        let n = match session.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(
                    target: "stoat_gui::terminal_view",
                    ?err,
                    "terminal read loop terminated"
                );
                break;
            },
        };
        let chunk = buf[..n].to_vec();
        if this
            .update(cx, |term, cx| term.on_read(&chunk, cx))
            .is_err()
        {
            break;
        }
    }
}

/// Write raw bytes to the session off the foreground thread. Mouse
/// reports are not always valid UTF-8, so this takes a byte vector.
fn spawn_write_bytes(
    session: Arc<dyn TerminalSession>,
    bytes: Vec<u8>,
    cx: &mut Context<'_, Terminal>,
) {
    cx.spawn(async move |_, _| {
        if let Err(err) = session.write(&bytes).await {
            tracing::warn!(
                target: "stoat_gui::terminal_view",
                ?err,
                "terminal write failed"
            );
        }
    })
    .detach();
}

impl ItemView for Terminal {
    fn tab_label(&self, _cx: &App) -> SharedString {
        if let Some(agent) = self.foreground_name.as_deref().and_then(matched_agent) {
            return SharedString::from(agent);
        }
        if let Some(dir) = self.grid.cwd().and_then(cwd_basename) {
            return SharedString::from(dir);
        }
        SharedString::from("Terminal")
    }

    fn deserialize(
        _value: serde_json::Value,
        _cx: &mut Context<'_, Self>,
    ) -> Result<Self, ItemError> {
        DeserializeSnafu {
            reason: "terminal restore is materialized by the workspace dispatch, not deserialize",
        }
        .fail()
    }

    fn item_kind(&self) -> ItemKind {
        ItemKind::Terminal
    }

    fn serialize(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({
            "cwd": self.cwd.to_string_lossy(),
            "program": self.program,
            "args": self.args,
        })
    }
}

impl Render for Terminal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let (font_family, font_size) = editor_font(cx);
        let focused = self.is_pane_focused(cx);
        let cursor = self.cell_size.map(|cell| {
            let (row, col) = self.grid.cursor_position();
            CursorRender {
                row,
                col,
                shape: self.grid.cursor_shape(),
                cell,
                focused,
            }
        });
        let mut body = div().flex().flex_col().flex_grow().w_full();
        for row_idx in 0..self.grid.line_count() {
            let row_cursor = cursor.as_ref().filter(|c| c.row == row_idx);
            body = body.child(render_grid_row(&self.grid, row_idx, None, None, row_cursor));
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
                let _ = bounds_handle.update(cx, |term, cx| {
                    term.set_output_bounds(bounds, cx);
                    if let Some(cell) = measured {
                        term.set_cell_size(cell, cx);
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
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    this.report_mouse(0, false, true, event.position, event.modifiers, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    this.report_mouse(1, false, true, event.position, event.modifiers, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    this.report_mouse(2, false, true, event.position, event.modifiers, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.report_mouse(0, false, false, event.position, event.modifiers, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.report_mouse(1, false, false, event.position, event.modifiers, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.report_mouse(2, false, false, event.position, event.modifiers, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                match event.pressed_button.and_then(mouse_button_code) {
                    Some(code) => {
                        this.report_mouse(code, true, true, event.position, event.modifiers, cx);
                    },
                    None => {
                        this.report_mouse(3, true, true, event.position, event.modifiers, cx);
                    },
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                let code = if scroll_is_up(&event.delta) { 64 } else { 65 };
                this.report_mouse(code, false, true, event.position, event.modifiers, cx);
            }));
        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .font_family(font_family)
            .text_size(px(font_size))
            .child(output_layer)
    }
}

impl Focusable for Terminal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Dispatch [`stoat_action::OpenClaudeTerminal`]. Opens a full-screen
/// terminal running the `claude` CLI in the workspace's git root and adds
/// it to the focused pane.
pub fn dispatch_open_claude_terminal(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    let pane_id = workspace.pane_tree().read(cx).focus();
    let Some(pane) = workspace.pane_tree().read(cx).pane(pane_id).cloned() else {
        return;
    };
    let cwd = workspace.git_root().clone();
    let weak_workspace = cx.weak_entity();
    let terminal =
        cx.new(|cx| Terminal::with_command(weak_workspace, cwd, "claude".into(), Vec::new(), cx));
    pane.update(cx, |p, cx| {
        p.add_item(Box::new(terminal), cx);
    });
}

/// Dispatch [`stoat_action::OpenTerminal`]. Opens a full-screen terminal
/// running the user's shell -- `$SHELL` read through the installed
/// [`EnvHostGlobal`], falling back to `/bin/sh` when unset -- in the
/// workspace's git root and adds it to the focused pane.
pub fn dispatch_open_terminal(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    let pane_id = workspace.pane_tree().read(cx).focus();
    let Some(pane) = workspace.pane_tree().read(cx).pane(pane_id).cloned() else {
        return;
    };
    let shell = cx
        .try_global::<EnvHostGlobal>()
        .and_then(|env| env.0.var("SHELL"))
        .unwrap_or_else(|| "/bin/sh".to_string());
    let cwd = workspace.git_root().clone();
    let weak_workspace = cx.weak_entity();
    let terminal = cx.new(|cx| Terminal::with_command(weak_workspace, cwd, shell, Vec::new(), cx));
    pane.update(cx, |p, cx| {
        p.add_item(Box::new(terminal), cx);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        globals::{
            ClipboardHostGlobal, EnvHostGlobal, ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal,
            TerminalHostGlobal,
        },
        item::ItemHandle,
        workspace::Workspace,
    };
    use gpui::{
        point, px, size, AppContext, Bounds, Entity, Keystroke, Modifiers, TestAppContext,
        VisualTestContext,
    };
    use stoat::host::{
        fake::{terminal::FakeTerminalSession, FakeClipboard, FakeEnv, FakeFs, FakeTerminalHost},
        ClipboardHost, EnvHost, FsHost, FsWatchHost,
    };
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(
        cx: &mut TestAppContext,
        terminal: Arc<FakeTerminalSession>,
        clipboard: Arc<FakeClipboard>,
        env: Arc<FakeEnv>,
    ) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        let terminal_host: Arc<dyn TerminalHost> = Arc::new(FakeTerminalHost::new(terminal));
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(FsHostGlobal(fs));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
            cx.set_global(ClipboardHostGlobal(clipboard as Arc<dyn ClipboardHost>));
            cx.set_global(TerminalHostGlobal(terminal_host));
            cx.set_global(EnvHostGlobal(env as Arc<dyn EnvHost>));
        });
    }

    struct Harness<'a> {
        workspace: Entity<Workspace>,
        terminal: Arc<FakeTerminalSession>,
        clipboard: Arc<FakeClipboard>,
        env: Arc<FakeEnv>,
        vcx: &'a mut VisualTestContext,
    }

    fn new_harness(cx: &mut TestAppContext) -> Harness<'_> {
        let terminal = Arc::new(FakeTerminalSession::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let env = Arc::new(FakeEnv::new());
        install_globals(cx, terminal.clone(), clipboard.clone(), env.clone());
        let (workspace, vcx) =
            cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/repo"), cx));
        Harness {
            workspace,
            terminal,
            clipboard,
            env,
            vcx,
        }
    }

    fn open_terminal(h: &mut Harness<'_>) -> Entity<Terminal> {
        h.workspace.update_in(h.vcx, |w, _window, cx| {
            let weak = cx.weak_entity();
            let term = cx.new(|cx| {
                Terminal::with_command(weak, PathBuf::from("/repo"), "bash".into(), Vec::new(), cx)
            });
            let pane_id = w.pane_tree().read(cx).focus();
            if let Some(pane) = w.pane_tree().read(cx).pane(pane_id).cloned() {
                pane.update(cx, |p, cx| p.add_item(Box::new(term), cx));
            }
        });
        focused_terminal(h).expect("terminal is the focused active item")
    }

    fn focused_terminal(h: &mut Harness<'_>) -> Option<Entity<Terminal>> {
        h.workspace.read_with(h.vcx, |w, cx| {
            let pane_id = w.pane_tree().read(cx).focus();
            let pane = w.pane_tree().read(cx).pane(pane_id).cloned()?;
            let view = pane.read(cx).active_item().map(ItemHandle::to_any_view)?;
            view.downcast::<Terminal>().ok()
        })
    }

    fn arm_mouse_mode(term: &Entity<Terminal>, h: &mut Harness<'_>, modes: &'static [u8]) {
        term.update(h.vcx, |t, cx| {
            t.on_read(modes, cx);
            t.set_cell_size(size(px(10.), px(20.)), cx);
            t.set_output_bounds(
                Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(800.), px(400.)),
                },
                cx,
            );
        });
    }

    #[test]
    fn item_kind_is_terminal() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let term = open_terminal(&mut h);
        let kind = term.read_with(h.vcx, |t, _| t.item_kind());
        assert_eq!(kind, ItemKind::Terminal);
    }

    #[test]
    fn open_claude_terminal_runs_the_claude_cli() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        h.workspace.update(h.vcx, |w, cx| {
            dispatch_open_claude_terminal(w, cx);
        });

        let term = focused_terminal(&mut h).expect("claude terminal is the focused item");
        let (program, args) = term.read_with(h.vcx, |t, _| (t.program.clone(), t.args.clone()));
        assert_eq!(program, "claude");
        assert!(args.is_empty(), "claude is launched with no extra args");
    }

    #[test]
    fn open_terminal_runs_the_shell_from_env() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        h.env.set("SHELL", "/usr/bin/fish");
        h.workspace.update(h.vcx, |w, cx| {
            dispatch_open_terminal(w, cx);
        });

        let term = focused_terminal(&mut h).expect("shell terminal is the focused item");
        let (program, args) = term.read_with(h.vcx, |t, _| (t.program.clone(), t.args.clone()));
        assert_eq!(program, "/usr/bin/fish");
        assert!(args.is_empty(), "the shell is launched with no extra args");
    }

    #[test]
    fn open_terminal_falls_back_to_sh_without_shell_env() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        h.workspace.update(h.vcx, |w, cx| {
            dispatch_open_terminal(w, cx);
        });

        let term = focused_terminal(&mut h).expect("shell terminal is the focused item");
        let program = term.read_with(h.vcx, |t, _| t.program.clone());
        assert_eq!(program, "/bin/sh");
    }

    #[test]
    fn matched_agent_recognizes_known_clis() {
        assert_eq!(matched_agent("claude"), Some("claude"));
        assert_eq!(matched_agent("codex"), Some("codex"));
        assert_eq!(matched_agent("bash"), None);
        assert_eq!(matched_agent("coreutils"), None);
    }

    #[test]
    fn tab_label_reflects_recognized_agent() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let term = open_terminal(&mut h);

        term.update(h.vcx, |t, _| t.foreground_name = Some("claude".into()));
        assert_eq!(
            term.read_with(h.vcx, |t, cx| t.tab_label(cx)),
            SharedString::from("claude"),
        );

        term.update(h.vcx, |t, _| t.foreground_name = Some("bash".into()));
        assert_eq!(
            term.read_with(h.vcx, |t, cx| t.tab_label(cx)),
            SharedString::from("Terminal"),
            "non-agent foreground keeps the default label",
        );
    }

    #[test]
    fn tab_label_falls_back_to_cwd_basename() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let term = open_terminal(&mut h);

        term.update(h.vcx, |t, cx| {
            t.on_read(b"\x1b]7;file://host/Users/lee/stoat\x07", cx)
        });
        assert_eq!(
            term.read_with(h.vcx, |t, cx| t.tab_label(cx)),
            SharedString::from("stoat"),
            "with no agent, the tab shows the cwd directory name",
        );
    }

    #[test]
    fn on_read_refreshes_foreground_name_from_session() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let term = open_terminal(&mut h);
        h.vcx.run_until_parked();

        h.terminal.set_foreground_name("claude");
        term.update(h.vcx, |t, cx| t.on_read(b"x", cx));

        assert_eq!(
            term.read_with(h.vcx, |t, _| t.foreground_name.clone()),
            Some("claude".to_string()),
        );
    }

    #[test]
    fn read_loop_feeds_bytes_into_grid() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let term = open_terminal(&mut h);
        h.vcx.run_until_parked();

        h.terminal.push_output(b"hello");
        h.vcx.run_until_parked();

        let first_row: String = term.read_with(h.vcx, |t, _| {
            t.grid.row(0).iter().take(5).map(|c| c.ch).collect()
        });
        assert_eq!(first_row, "hello");
    }

    #[test]
    fn resize_pushes_cell_size_to_session() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let term = open_terminal(&mut h);
        h.vcx.run_until_parked();

        let expected = term.read_with(h.vcx, |t, _| {
            let bounds = t.output_bounds?;
            let cell = t.cell_size?;
            terminal_cells(bounds, cell)
        });
        assert!(expected.is_some(), "render should measure the pane");
        assert_eq!(h.terminal.last_size(), expected);
    }

    #[test]
    fn grid_width_tracks_measured_columns() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let term = open_terminal(&mut h);
        h.vcx.run_until_parked();

        let (grid_width, measured_cols) = term.read_with(h.vcx, |t, _| {
            let cols = t
                .output_bounds
                .zip(t.cell_size)
                .and_then(|(bounds, cell)| terminal_cells(bounds, cell))
                .map(|(_, cols)| cols);
            (t.grid.width(), cols)
        });
        assert_eq!(
            Some(grid_width),
            measured_cols,
            "grid width tracks the measured PTY column count"
        );
    }

    #[test]
    fn mouse_press_reports_in_active_mode() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let term = open_terminal(&mut h);
        h.vcx.run_until_parked();
        arm_mouse_mode(&term, &mut h, b"\x1b[?1000h\x1b[?1006h");

        let consumed = term.update(h.vcx, |t, cx| {
            t.report_mouse(
                0,
                false,
                true,
                point(px(25.), px(30.)),
                Modifiers::default(),
                cx,
            )
        });
        h.vcx.run_until_parked();

        assert!(consumed);
        let report = h.terminal.sent_bytes().last().cloned().unwrap_or_default();
        assert!(
            report.starts_with(b"\x1b[<0;") && report.ends_with(b"M"),
            "expected an SGR button-0 press report, got {report:?}"
        );
    }

    #[test]
    fn mouse_without_mode_falls_through() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let term = open_terminal(&mut h);
        h.vcx.run_until_parked();

        let consumed = term.update(h.vcx, |t, cx| {
            t.report_mouse(
                0,
                false,
                true,
                point(px(25.), px(30.)),
                Modifiers::default(),
                cx,
            )
        });
        assert!(!consumed, "no mouse mode means the report is not consumed");
    }

    #[test]
    fn focused_terminal_routes_control_keys_through_the_workspace() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        open_terminal(&mut h);
        // Adding the terminal makes it the active item; the pane event
        // drives the workspace broadcast that sets `active_terminal`.
        h.vcx.run_until_parked();

        let sm = h
            .workspace
            .read_with(h.vcx, |w, _| w.input_state_machine().clone());
        sm.update_in(h.vcx, |sm, window, cx| {
            let ctrl_c = Keystroke {
                modifiers: Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                key: "c".into(),
                key_char: None,
            };
            sm.feed(&ctrl_c, window, cx);
        });
        h.vcx.run_until_parked();

        h.terminal.assert_sent(0, b"\x03");
    }

    fn feed_cmd_v(h: &mut Harness<'_>) {
        let sm = h
            .workspace
            .read_with(h.vcx, |w, _| w.input_state_machine().clone());
        sm.update_in(h.vcx, |sm, window, cx| {
            let cmd_v = Keystroke {
                modifiers: Modifiers {
                    platform: true,
                    ..Modifiers::default()
                },
                key: "v".into(),
                key_char: None,
            };
            sm.feed(&cmd_v, window, cx);
        });
    }

    #[test]
    fn cmd_v_pastes_clipboard_raw_when_not_bracketed() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        open_terminal(&mut h);
        h.vcx.run_until_parked();
        h.clipboard.set("hello").expect("seed clipboard");

        feed_cmd_v(&mut h);
        h.vcx.run_until_parked();

        h.terminal.assert_sent(0, b"hello");
    }

    #[test]
    fn cmd_v_wraps_paste_when_program_enabled_bracketed_mode() {
        let mut cx = TestAppContext::single();
        let mut h = new_harness(&mut cx);
        let term = open_terminal(&mut h);
        h.vcx.run_until_parked();
        term.update(h.vcx, |t, cx| t.on_read(b"\x1b[?2004h", cx));
        h.clipboard.set("hi").expect("seed clipboard");

        feed_cmd_v(&mut h);
        h.vcx.run_until_parked();

        h.terminal.assert_sent(0, b"\x1b[200~hi\x1b[201~");
    }
}
