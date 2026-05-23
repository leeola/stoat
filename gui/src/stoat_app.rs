use crate::{
    buffer::Buffer,
    diff_map::DiffMap,
    display_map::DisplayMap,
    editor::{Editor, EditorMode},
    globals::{ExecutorGlobal, FsHostGlobal},
    multi_buffer::MultiBuffer,
    workspace::Workspace,
    RestoreMode,
};
use gpui::{
    div, AppContext, Context, Entity, FocusHandle, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, Window,
};
use std::path::{Path, PathBuf};
use stoat::buffer::BufferId;

pub(crate) struct StoatApp {
    workspace: Entity<Workspace>,
    #[allow(dead_code)]
    focus_handle: FocusHandle,
    /// Drops with the app; keeps the workspace's release observer
    /// registered for as long as the workspace is hosted in this
    /// view so window close flushes a final save before the
    /// workspace entity is dropped.
    _workspace_release: Subscription,
}

impl StoatApp {
    pub(crate) fn new(
        files: Vec<PathBuf>,
        restore: RestoreMode,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let restore_anchor = match restore {
            RestoreMode::None => None,
            RestoreMode::Continue => Some(cwd.clone()),
            RestoreMode::Resume => resume_anchor(&cwd, cx).unwrap_or(None),
        };

        let initial_root = restore_anchor.clone().unwrap_or_else(|| cwd.clone());
        let name = initial_root
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| SharedString::from(s.to_string()))
            .unwrap_or_else(|| SharedString::from("stoat"));
        let workspace = cx.new(|cx| Workspace::new(name, initial_root.clone(), cx));

        let restored = if let Some(anchor) = restore_anchor {
            try_restore_workspace(&workspace, &anchor, cx)
        } else {
            false
        };

        if restored {
            if !files.is_empty() {
                workspace.update(cx, |w, cx| w.open_paths(&files, cx));
            }
        } else if files.is_empty() {
            Self::open_scratch(&workspace, cx);
        } else {
            workspace.update(cx, |w, cx| w.open_paths(&files, cx));
        }

        let _workspace_release =
            gpui::App::observe_release(cx, &workspace, |ws, cx| ws.save_state_to_default_path(cx));

        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            _workspace_release,
        }
    }
}

fn resume_anchor(cwd: &Path, cx: &Context<'_, StoatApp>) -> std::io::Result<Option<PathBuf>> {
    let fs = cx.try_global::<FsHostGlobal>().map(|g| g.0.clone());
    let Some(fs) = fs else {
        return Ok(None);
    };
    stoat::workspace::persist::find_resume_anchor(cwd, &*fs)
}

fn try_restore_workspace(
    workspace: &Entity<Workspace>,
    anchor: &Path,
    cx: &mut Context<'_, StoatApp>,
) -> bool {
    let Some(fs) = cx.try_global::<FsHostGlobal>().map(|g| g.0.clone()) else {
        tracing::warn!("FsHostGlobal not installed; skipping workspace restore");
        return false;
    };
    let anchor = anchor.to_path_buf();
    let fs_dyn: std::sync::Arc<dyn stoat::host::FsHost> = fs;
    let outcome = workspace.update(cx, |w, cx| w.restore_most_recent(&anchor, &*fs_dyn, cx));
    match outcome {
        Ok(true) => true,
        Ok(false) => false,
        Err(err) => {
            tracing::warn!(?err, ?anchor, "workspace restore failed");
            false
        },
    }
}

impl StoatApp {
    /// Borrow the hosted workspace entity. Used by tests that
    /// need to introspect a window opened through the
    /// `CopyWorkspace` / `NewWorkspace` dispatch paths.
    pub(crate) fn workspace(&self) -> &Entity<Workspace> {
        &self.workspace
    }

    /// Construct a [`StoatApp`] that hosts a workspace
    /// reconstructed from a previously captured
    /// [`crate::workspace_persist::WorkspaceStateV1`]. The
    /// workspace's name and git root come from `state`; pane
    /// tree, items, docks, and buffers all rehydrate via
    /// [`Workspace::apply_state`]. Used by the `CopyWorkspace`
    /// dispatch path to clone the current workspace into a new
    /// window.
    pub(crate) fn new_with_state(
        state: crate::workspace_persist::WorkspaceStateV1,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let name = SharedString::from(state.name.clone());
        let git_root = state.git_root.clone();
        let workspace = cx.new(|cx| Workspace::new(name, git_root, cx));
        workspace.update(cx, |w, cx| w.apply_state(state, cx));

        let _workspace_release =
            gpui::App::observe_release(cx, &workspace, |ws, cx| ws.save_state_to_default_path(cx));

        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            _workspace_release,
        }
    }

    fn open_scratch(workspace: &Entity<Workspace>, cx: &mut Context<'_, Self>) {
        let buffer = cx.new(|_| Buffer::with_text(BufferId::new(0), ""));
        let multi_buffer = cx.new({
            let buffer = buffer.clone();
            |cx| MultiBuffer::singleton(buffer, cx)
        });
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let display_map = cx.new({
            let buffer = buffer.clone();
            |cx| DisplayMap::new(buffer, executor, cx)
        });
        let diff_map = cx.new(|cx| DiffMap::new(buffer, cx));
        let editor =
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx));
        editor.update(cx, |ed, _| ed.set_workspace(Some(workspace.downgrade())));

        let pane_tree = workspace.read(cx).pane_tree().clone();
        let focus_id = pane_tree.read(cx).focus();
        let pane = pane_tree
            .read(cx)
            .pane(focus_id)
            .expect("focused pane registered in PaneTree::panes")
            .clone();
        pane.update(cx, |p, cx| {
            p.add_item(Box::new(editor), cx);
        });
    }
}

impl Render for StoatApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full().child(self.workspace.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::sync::Arc;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor_global(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    #[test]
    fn new_constructs_workspace_anchored_to_cwd() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let (app, _vcx) =
            cx.add_window_view(|_window, cx| StoatApp::new(Vec::new(), RestoreMode::None, cx));
        let cwd = std::env::current_dir().expect("current_dir");
        let expected_name: SharedString = cwd
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| SharedString::from(s.to_string()))
            .unwrap_or_else(|| SharedString::from("stoat"));

        app.read_with(&cx, |app, cx| {
            let ws = app.workspace.read(cx);
            assert_eq!(ws.git_root(), &cwd);
            assert_eq!(ws.name(), &expected_name);
        });
    }

    #[test]
    fn new_seeds_focused_pane_with_scratch_editor() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let (app, _vcx) =
            cx.add_window_view(|_window, cx| StoatApp::new(Vec::new(), RestoreMode::None, cx));

        app.read_with(&cx, |app, cx| {
            let ws = app.workspace.read(cx);
            let pane_tree = ws.pane_tree().read(cx);
            let focus_id = pane_tree.focus();
            let pane = pane_tree.pane(focus_id).expect("focused pane").read(cx);
            assert_eq!(pane.len(), 1);
            let editor = pane
                .active_item()
                .expect("scratch editor active in focused pane");
            assert_eq!(editor.tab_label(cx), SharedString::from("(scratch)"));
        });
    }

    #[test]
    fn release_observer_writes_workspace_state_on_release() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        let fs_global: Arc<dyn stoat::host::FsHost> = fs.clone();
        cx.update(|cx| cx.set_global(FsHostGlobal(fs_global.clone())));

        let cwd = std::env::current_dir().expect("current_dir");
        let workspace = cx.update(|cx| cx.new(|cx| Workspace::new("main", cwd.clone(), cx)));
        workspace.update(&mut cx, |w, _| w.mark_dirty());
        let uid = workspace.read_with(&cx, |w, _| w.uid());
        let expected_path =
            stoat::workspace::persist::state_path_for(&cwd, uid, &*fs_global).expect("state path");

        let _subscription = cx.update(|cx| {
            gpui::App::observe_release(cx, &workspace, |ws, cx| {
                ws.save_state_to_default_path(cx);
            })
        });

        assert!(!stoat::host::FsHost::exists(&*fs_global, &expected_path));

        drop(workspace);
        cx.update(|_| {});

        assert!(
            stoat::host::FsHost::exists(&*fs_global, &expected_path),
            "release observer should have saved state at {}",
            expected_path.display(),
        );
    }

    #[test]
    fn new_continue_with_no_persisted_state_falls_back_to_scratch() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let fs: Arc<dyn stoat::host::FsHost> = Arc::new(stoat::host::FakeFs::new());
        cx.update(|cx| cx.set_global(FsHostGlobal(fs)));
        let (app, _vcx) =
            cx.add_window_view(|_window, cx| StoatApp::new(Vec::new(), RestoreMode::Continue, cx));

        app.read_with(&cx, |app, cx| {
            let ws = app.workspace.read(cx);
            let pane_tree = ws.pane_tree().read(cx);
            let focus_id = pane_tree.focus();
            let pane = pane_tree.pane(focus_id).expect("focused pane").read(cx);
            assert_eq!(pane.len(), 1);
            let editor = pane
                .active_item()
                .expect("scratch editor active in focused pane");
            assert_eq!(editor.tab_label(cx), SharedString::from("(scratch)"));
        });
    }
}
