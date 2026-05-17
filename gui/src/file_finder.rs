//! File finder picker delegate.
//!
//! Walks the workspace root via [`FsHost::walk_workspace_files_streaming`]
//! on a background blocking thread, streams the resulting batches
//! into the delegate through a [`tokio::sync::mpsc::unbounded_channel`],
//! and fuzzy-ranks the accumulated set against the picker query.
//! On confirm the selected path is opened in the focused pane via
//! [`Workspace::open_paths`].

use crate::{
    globals::{ExecutorGlobal, FsHostGlobal},
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    theme::statusbar_text_color,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, DismissEvent, HighlightStyle, IntoElement, ParentElement,
    SharedString, Styled, StyledText, Task, WeakEntity, Window,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::host::FsHost;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

pub struct FileFinderDelegate {
    workspace: WeakEntity<Workspace>,
    git_root: PathBuf,
    /// Absolute paths of every file the streaming walker has produced
    /// so far. Extended in walker order by [`Self::extend_paths`];
    /// fuzzy-ranking sorts the indexed view separately so the natural
    /// walker order does not bleed into the match list.
    paths: Vec<PathBuf>,
    /// Indices into [`Self::paths`] selected by the current
    /// [`Self::query`], paired with the per-match character indices
    /// the renderer highlights. Sorted by rank.
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    /// Cached query string. The picker re-runs the matcher on every
    /// keystroke via [`PickerDelegate::update_matches`], but the
    /// drain task also has to refilter when fresh batches arrive
    /// between keystrokes; caching here is the obvious place to read
    /// the query from without going back through the picker entity.
    query: String,
    /// Drain task that forwards walker batches into [`Self::paths`].
    /// Kept alive on the delegate so dropping the modal drops the
    /// task, which drops the receiver and signals the walker's
    /// `send`-failure exit path.
    _drain_task: Option<Task<()>>,
}

impl FileFinderDelegate {
    pub fn new(workspace: WeakEntity<Workspace>, git_root: PathBuf) -> Self {
        Self {
            workspace,
            git_root,
            paths: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            query: String::new(),
            _drain_task: None,
        }
    }

    /// Append `batch` to [`Self::paths`] and re-run the filter so
    /// the newly-arrived paths participate in the visible matches.
    /// Called from the drain task on every batch the walker emits.
    pub fn extend_paths(&mut self, batch: Vec<PathBuf>) {
        if batch.is_empty() {
            return;
        }
        self.paths.extend(batch);
        self.refilter();
    }

    fn set_drain_task(&mut self, task: Task<()>) {
        self._drain_task = Some(task);
    }

    fn refilter(&mut self) {
        let trimmed = self.query.trim();
        if trimmed.is_empty() {
            self.matches = (0..self.paths.len()).map(|i| (i, Vec::new())).collect();
            if self.selected >= self.matches.len() {
                self.selected = self.matches.len().saturating_sub(1);
            }
            return;
        }

        let items = self.paths.iter().enumerate().map(|(i, path)| {
            let display = display_path(path, &self.git_root);
            (i, display)
        });
        let Some(mut ranked) = rank_matches(trimmed, items) else {
            self.matches.clear();
            self.selected = 0;
            return;
        };
        ranked.sort_by_key(|m| std::cmp::Reverse(m.score));
        self.matches = ranked
            .into_iter()
            .map(|m| (m.item, m.matched_indices))
            .collect();
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    fn selected_path(&self) -> Option<&Path> {
        let (idx, _) = self.matches.get(self.selected)?;
        self.paths.get(*idx).map(|p| p.as_path())
    }
}

impl PickerDelegate for FileFinderDelegate {
    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
        if ix < self.matches.len() {
            self.selected = ix;
        }
    }

    fn update_matches(&mut self, query: String, _cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        self.query = query;
        self.refilter();
        Task::ready(())
    }

    fn confirm(
        &mut self,
        _secondary: Option<PickerSecondary>,
        _window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        let Some(path) = self.selected_path().map(Path::to_path_buf) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |ws, cx| ws.open_paths(&[path], cx));
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> AnyElement {
        let Some((path_idx, matched)) = self.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(path) = self.paths.get(*path_idx) else {
            return div().into_any_element();
        };
        let display = display_path(path, &self.git_root);
        let color = statusbar_text_color(cx);
        let runs = match_highlight_runs(
            &display,
            matched,
            HighlightStyle {
                color: Some(gpui::white()),
                ..Default::default()
            },
        );
        let label = StyledText::new(SharedString::from(display)).with_highlights(runs);
        let mut row = div().px_2().text_color(color).child(label);
        if selected {
            row = row.bg(gpui::white().opacity(0.1));
        }
        row.into_any_element()
    }
}

/// Format `path` for display: strip the `git_root` prefix when
/// possible so users see repo-relative paths instead of absolute
/// ones. Falls back to the absolute path's lossy form when the
/// prefix doesn't match (paths outside the workspace root, which
/// the walker shouldn't produce but we accept defensively).
fn display_path(path: &Path, git_root: &Path) -> String {
    match path.strip_prefix(git_root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Open the file finder as a modal picker, scheduling the walker
/// on the background blocking pool and streaming batches into the
/// delegate.
pub fn open_file_finder(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let git_root = workspace.git_root().clone();
    let weak_workspace = cx.weak_entity();
    let fs_host = cx.global::<FsHostGlobal>().0.clone();
    let executor = cx.global::<ExecutorGlobal>().0.clone();

    let (walk_tx, walk_rx) = unbounded_channel();
    let walk_task = spawn_walker(executor.clone(), fs_host, git_root.clone(), walk_tx);

    workspace.toggle_modal::<Picker<FileFinderDelegate>, _>(window, cx, move |window, cx| {
        let delegate = FileFinderDelegate::new(weak_workspace, git_root);
        Picker::new(delegate, window, cx)
    });
    walk_task.detach();
    let Some(picker) = workspace
        .modal_layer()
        .read(cx)
        .active_modal::<Picker<FileFinderDelegate>>()
    else {
        return;
    };
    let weak_picker = picker.downgrade();
    let drain_task = cx.spawn(async move |_workspace, async_cx| {
        drain_walker_batches(weak_picker, walk_rx, async_cx).await;
    });
    picker.update(cx, |p, _| {
        p.delegate_mut().set_drain_task(drain_task);
    });
}

fn spawn_walker(
    executor: stoat_scheduler::Executor,
    fs_host: Arc<dyn FsHost>,
    git_root: PathBuf,
    walk_tx: UnboundedSender<Vec<PathBuf>>,
) -> stoat_scheduler::Task<()> {
    executor.spawn_blocking(move || {
        fs_host.walk_workspace_files_streaming(&git_root, &mut |batch| {
            if walk_tx.send(batch).is_err() {
                // Receiver dropped; bail out and let the walker exit early.
            }
        });
    })
}

async fn drain_walker_batches(
    weak_picker: WeakEntity<Picker<FileFinderDelegate>>,
    mut walk_rx: UnboundedReceiver<Vec<PathBuf>>,
    cx: &mut gpui::AsyncApp,
) {
    while let Some(batch) = walk_rx.recv().await {
        let updated = weak_picker.update(cx, |picker, cx| {
            picker.delegate_mut().extend_paths(batch);
            cx.notify();
        });
        if updated.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, FsHostGlobal};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::sync::Arc;
    use stoat::host::FakeFs;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext, fake_fs: Arc<FakeFs>) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(FsHostGlobal(fake_fs));
        });
    }

    fn new_delegate(git_root: &str) -> FileFinderDelegate {
        FileFinderDelegate::new(WeakEntity::new_invalid(), PathBuf::from(git_root))
    }

    fn names(delegate: &FileFinderDelegate) -> Vec<String> {
        delegate
            .matches
            .iter()
            .map(|(i, _)| display_path(&delegate.paths[*i], &delegate.git_root))
            .collect()
    }

    #[test]
    fn delegate_lists_no_paths_when_constructed() {
        let delegate = new_delegate("/repo");
        assert_eq!(delegate.match_count(), 0);
    }

    #[test]
    fn extend_paths_appends_and_refilters() {
        let mut delegate = new_delegate("/repo");
        delegate.extend_paths(vec![
            PathBuf::from("/repo/src/main.rs"),
            PathBuf::from("/repo/src/lib.rs"),
        ]);
        let listed = names(&delegate);
        assert_eq!(listed, vec!["src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn refilter_narrows_against_query() {
        let mut delegate = new_delegate("/repo");
        delegate.extend_paths(vec![
            PathBuf::from("/repo/src/main.rs"),
            PathBuf::from("/repo/src/lib.rs"),
            PathBuf::from("/repo/tests/integration.rs"),
        ]);
        delegate.query = "main".to_string();
        delegate.refilter();
        let listed = names(&delegate);
        assert_eq!(listed, vec!["src/main.rs"]);
    }

    #[test]
    fn display_path_strips_git_root_prefix() {
        let root = PathBuf::from("/repo");
        assert_eq!(
            display_path(&PathBuf::from("/repo/src/main.rs"), &root),
            "src/main.rs"
        );
    }

    #[test]
    fn display_path_falls_back_to_absolute_outside_root() {
        let root = PathBuf::from("/repo");
        assert_eq!(
            display_path(&PathBuf::from("/elsewhere/file.rs"), &root),
            "/elsewhere/file.rs"
        );
    }

    #[test]
    fn extend_paths_with_empty_batch_is_noop() {
        let mut delegate = new_delegate("/repo");
        delegate.extend_paths(vec![PathBuf::from("/repo/a.rs")]);
        let before = delegate.paths.len();
        delegate.extend_paths(Vec::new());
        assert_eq!(delegate.paths.len(), before);
    }

    fn fake_fs_with_files(files: &[(&str, &str)]) -> Arc<FakeFs> {
        let fs = FakeFs::new();
        let root = Path::new("/repo");
        fs.insert_files(
            files
                .iter()
                .map(|(rel, content)| (root.join(rel), content.as_bytes())),
        );
        Arc::new(fs)
    }

    struct Harness<'a> {
        workspace: Entity<Workspace>,
        vcx: &'a mut VisualTestContext,
    }

    fn new_harness<'a>(cx: &'a mut TestAppContext, fake_fs: Arc<FakeFs>) -> Harness<'a> {
        install_globals(cx, fake_fs);
        let (workspace, vcx) =
            cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/repo"), cx));
        Harness { workspace, vcx }
    }

    #[test]
    fn open_file_finder_makes_picker_modal_active() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_file_finder(w, window, cx);
        });
        h.vcx.run_until_parked();

        let active = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<FileFinderDelegate>>()
                .is_some()
        });
        assert!(active, "file finder picker should be the active modal");
    }

    #[test]
    fn streamed_walker_populates_delegate_paths() {
        let mut cx = TestAppContext::single();
        let fake_fs =
            fake_fs_with_files(&[("src/main.rs", ""), ("src/lib.rs", ""), ("readme", "")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_file_finder(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker: Entity<Picker<FileFinderDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("file finder modal is open");
        let mut listed = picker.read_with(h.vcx, |p, _| {
            p.delegate()
                .paths
                .iter()
                .map(|path| display_path(path, p.delegate().git_root.as_path()))
                .collect::<Vec<_>>()
        });
        listed.sort();
        assert_eq!(listed, vec!["readme", "src/lib.rs", "src/main.rs"]);
    }
}
