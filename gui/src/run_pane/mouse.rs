//! Run pane mouse pipeline.
//!
//! Defines the two non-keymap-bindable actions the rendered grid
//! emits on mouse-down / mouse-drag, plus the workspace-side
//! handlers that translate them into [`OutputBlock::selection`]
//! updates on the focused pane's [`Run`]. Mirrors the editor's
//! [`crate::actions::ClickAt`] / [`crate::actions::DragSelectTo`]
//! plumbing (`gui/src/editor.rs:2252-2295`) -- the action is
//! synthesised by the render's mouse listeners, routed through
//! [`crate::workspace::Workspace::dispatch_action`], and consumed
//! by the run pane.

use crate::{item::ItemHandle, run_pane::Run, workspace::Workspace};
use gpui::{Context, Pixels, Point, Size};
use std::any::Any;
use stoat_action::{Action, ActionDef, ActionKind, ActionPriority, ParamDef};

#[derive(Debug, Clone, Copy)]
pub struct RunClickAt {
    pub row: u32,
    pub col: u32,
}

#[derive(Debug)]
pub struct RunClickAtDef;

impl ActionDef for RunClickAtDef {
    fn name(&self) -> &'static str {
        "RunClickAt"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::RunClickAt
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "click at run grid position"
    }

    fn long_desc(&self) -> &'static str {
        "Seed the active output block's selection at the row/column inside the focused run pane's rendered grid. Dispatched by mouse-down on the run pane; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl RunClickAt {
    pub const DEF: &RunClickAtDef = &RunClickAtDef;
}

impl Action for RunClickAt {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RunDragSelectTo {
    pub row: u32,
    pub col: u32,
}

#[derive(Debug)]
pub struct RunDragSelectToDef;

impl ActionDef for RunDragSelectToDef {
    fn name(&self) -> &'static str {
        "RunDragSelectTo"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::RunDragSelectTo
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "drag-select to run grid position"
    }

    fn long_desc(&self) -> &'static str {
        "Extend the active output block's selection head to the row/column inside the focused run pane's rendered grid. Dispatched by mouse-drag on the run pane while the left button is held; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl RunDragSelectTo {
    pub const DEF: &RunDragSelectToDef = &RunDragSelectToDef;
}

impl Action for RunDragSelectTo {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Convert an element-local pixel position into a `(col, row)`
/// pair using the rendered cell metrics. Mirrors
/// `crate::editor::mouse::point_to_grid` so the run pane does
/// not pull editor internals.
pub fn point_to_grid(elem: Point<Pixels>, cell: Size<Pixels>) -> (u32, u32) {
    let cell_w: f32 = cell.width.into();
    let cell_h: f32 = cell.height.into();
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return (0, 0);
    }
    let x: f32 = elem.x.into();
    let y: f32 = elem.y.into();
    let col = (x.max(0.0) / cell_w) as u32;
    let row = (y.max(0.0) / cell_h) as u32;
    (row, col)
}

fn focused_run(
    workspace: &Workspace,
    cx: &mut Context<'_, Workspace>,
) -> Option<gpui::Entity<Run>> {
    let pane_id = workspace.pane_tree().read(cx).focus();
    let pane = workspace.pane_tree().read(cx).pane(pane_id).cloned()?;
    let active_view = pane.read(cx).active_item().map(ItemHandle::to_any_view)?;
    active_view.downcast::<Run>().ok()
}

pub fn handle_run_click_at(
    workspace: &mut Workspace,
    row: u32,
    col: u32,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(run) = focused_run(workspace, cx) {
        run.update(cx, |r, cx| r.handle_click_at(row, col, cx));
    }
}

pub fn handle_run_drag_select_to(
    workspace: &mut Workspace,
    row: u32,
    col: u32,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(run) = focused_run(workspace, cx) {
        run.update(cx, |r, cx| r.handle_drag_select_to(row, col, cx));
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
        workspace::Workspace,
    };
    use gpui::{px, size, Entity, TestAppContext, VisualTestContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat::{
        host::{
            fake::{terminal::FakeTerminalSession, FakeClipboard, FakeFs, FakeTerminalHost},
            ClipboardHost, FsHost, FsWatchHost, TerminalHost,
        },
        run::{GridSelection, OutputBlock},
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

    fn new_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        install_globals(cx, Arc::new(FakeTerminalSession::new()));
        cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/repo"), cx))
    }

    fn open_run(workspace: &Entity<Workspace>, vcx: &mut VisualTestContext) -> Entity<Run> {
        workspace.update_in(vcx, |w, window, cx| {
            crate::run_pane::dispatch_open_run(w, window, cx);
        });
        workspace
            .read_with(vcx, |w, cx| {
                let pane_id = w.pane_tree().read(cx).focus();
                let pane = w.pane_tree().read(cx).pane(pane_id).cloned()?;
                let view = pane.read(cx).active_item().map(ItemHandle::to_any_view)?;
                view.downcast::<Run>().ok()
            })
            .expect("run pane open")
    }

    fn push_block(run: &Entity<Run>, vcx: &mut VisualTestContext, command: &str) {
        run.update(vcx, |r, _| {
            r.blocks.push(OutputBlock::new(command.into(), 80));
        });
    }

    fn active_selection(run: &Entity<Run>, vcx: &mut VisualTestContext) -> Option<GridSelection> {
        run.read_with(vcx, |r, _| r.blocks.last().and_then(|b| b.selection))
    }

    #[test]
    fn run_click_at_seeds_selection_on_active_block() {
        let mut cx = TestAppContext::single();
        let (workspace, vcx) = new_workspace(&mut cx);
        let run = open_run(&workspace, vcx);
        push_block(&run, vcx, "ls");

        workspace.update_in(vcx, |w, _window, cx| {
            handle_run_click_at(w, 1, 3, cx);
        });

        assert_eq!(
            active_selection(&run, vcx),
            Some(GridSelection {
                anchor: (3, 1),
                head: (3, 1),
            }),
        );
    }

    #[test]
    fn run_drag_select_to_updates_head_only() {
        let mut cx = TestAppContext::single();
        let (workspace, vcx) = new_workspace(&mut cx);
        let run = open_run(&workspace, vcx);
        push_block(&run, vcx, "ls");

        workspace.update_in(vcx, |w, _window, cx| {
            handle_run_click_at(w, 1, 3, cx);
            handle_run_drag_select_to(w, 4, 7, cx);
        });

        assert_eq!(
            active_selection(&run, vcx),
            Some(GridSelection {
                anchor: (3, 1),
                head: (7, 4),
            }),
        );
    }

    #[test]
    fn run_drag_select_to_without_prior_click_is_noop() {
        let mut cx = TestAppContext::single();
        let (workspace, vcx) = new_workspace(&mut cx);
        let run = open_run(&workspace, vcx);
        push_block(&run, vcx, "ls");

        workspace.update_in(vcx, |w, _window, cx| {
            handle_run_drag_select_to(w, 4, 7, cx);
        });

        assert_eq!(active_selection(&run, vcx), None);
    }

    #[test]
    fn point_to_grid_translates_pixels() {
        let cell = size(px(8.0), px(16.0));
        assert_eq!(point_to_grid(Point::new(px(0.0), px(0.0)), cell), (0, 0));
        assert_eq!(point_to_grid(Point::new(px(20.0), px(16.0)), cell), (1, 2));
        assert_eq!(
            point_to_grid(Point::new(px(-5.0), px(-2.0)), cell),
            (0, 0),
            "negative coords clamp to origin",
        );
    }
}
