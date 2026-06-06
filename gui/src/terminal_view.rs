//! GUI full-screen terminal ItemView (Way-2 embedded terminal).
//!
//! Pane-hosted entity wrapping a single PTY-backed [`TerminalSession`]
//! and one [`VtermGrid`] rendered to fill the pane. Unlike the
//! Warp-style [`crate::run_pane::Run`], it has no command blocks and no
//! command-line input: the grid is the whole surface, so an interactive
//! TUI running in the PTY paints directly into it.
//!
//! [`Terminal::new`] spawns the session in the background via
//! [`TerminalHost`]; once installed, the same task loops on
//! [`TerminalSession::read`] and feeds each chunk into the grid through
//! [`Terminal::on_read`]. The render measures cell metrics in its canvas
//! prepaint and resizes the PTY to the whole-cell grid, and forwards
//! mouse events to the program as reports. The styled-cell paint reuses
//! [`crate::run_pane::render`].

use crate::{
    globals::TerminalHostGlobal,
    item::{DeserializeSnafu, ItemError, ItemKind, ItemView},
    run_pane::{
        editor_font, mouse, mouse_button_code,
        render::{render_grid_row, CursorRender},
        scroll_is_up, terminal_cells, GPUI_DEFAULT_LINE_HEIGHT_RATIO,
    },
    workspace::Workspace,
};
use gpui::{
    canvas, div, font, point, px, size, App, Bounds, Context, InteractiveElement, IntoElement,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Point, Render, ScrollWheelEvent, SharedString, Size, Styled, Task, WeakEntity, Window,
};
use std::{path::PathBuf, sync::Arc};
use stoat::{
    host::{SpawnArgs, TerminalHost, TerminalSession},
    run::{MouseProtocol, VtermGrid},
};

/// Fixed grid column count. [`VtermGrid`] has no in-place resize, so --
/// like the run pane's blocks -- the grid stays this wide while the PTY
/// is resized to the measured cell dimensions.
const TERMINAL_WIDTH: u16 = 80;

pub(crate) struct Terminal {
    grid: VtermGrid,
    session: Option<Arc<dyn TerminalSession>>,
    cwd: PathBuf,
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
    _spawn_task: Option<Task<()>>,
}

impl Terminal {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        cwd: PathBuf,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let host = cx.global::<TerminalHostGlobal>().0.clone();
        let spawn_task = cx.spawn({
            let cwd = cwd.clone();
            async move |this, cx| {
                install_session(host, cwd, this, cx).await;
            }
        });
        Self {
            grid: VtermGrid::new(TERMINAL_WIDTH),
            session: None,
            cwd,
            workspace,
            output_bounds: None,
            cell_size: None,
            last_terminal_size: None,
            _spawn_task: Some(spawn_task),
        }
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
        cx.notify();
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
    cwd: PathBuf,
    this: WeakEntity<Terminal>,
    cx: &mut gpui::AsyncApp,
) {
    let args = SpawnArgs {
        program: "bash".into(),
        args: Vec::new(),
        env: vec![("TERM".into(), "xterm-256color".into())],
        cwd,
        width: TERMINAL_WIDTH,
        rows: 24,
    };
    let session: Arc<dyn TerminalSession> = match host.spawn(args).await {
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
            body = body.child(render_grid_row(&self.grid, row_idx, None, row_cursor));
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
            .flex()
            .flex_col()
            .size_full()
            .font_family(font_family)
            .text_size(px(font_size))
            .child(output_layer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        globals::{
            ClipboardHostGlobal, ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal,
            TerminalHostGlobal,
        },
        item::ItemHandle,
        workspace::Workspace,
    };
    use gpui::{
        point, px, size, AppContext, Bounds, Entity, Modifiers, TestAppContext, VisualTestContext,
    };
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

    fn open_terminal(h: &mut Harness<'_>) -> Entity<Terminal> {
        h.workspace.update_in(h.vcx, |w, _window, cx| {
            let weak = cx.weak_entity();
            let term = cx.new(|cx| Terminal::new(weak, PathBuf::from("/repo"), cx));
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
}
