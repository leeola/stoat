//! File finder picker delegate.
//!
//! Walks the workspace root via [`FsHost::walk_workspace_files_streaming`]
//! on a background blocking thread, streams the resulting batches
//! into the delegate through a [`tokio::sync::mpsc::unbounded_channel`],
//! and fuzzy-ranks the accumulated set against the picker query.
//! On confirm the selected path is opened in the focused pane via
//! [`Workspace::open_paths`].

use crate::{
    buffer::Buffer,
    editor::Editor,
    globals::{ExecutorGlobal, FsHostGlobal, GitHostGlobal},
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, App, Context, DismissEvent, Entity, HighlightStyle, IntoElement,
    ParentElement, SharedString, Styled, StyledText, Task, WeakEntity, Window,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{host::FsHost, pane::Axis};

/// Upper bound on bytes the preview pane reads from a selected
/// file. Files larger than this are truncated to the prefix.
/// Mirrors `stoat/src/file_finder.rs:17`.
pub const PREVIEW_BYTE_LIMIT: usize = 128 * 1024;
use stoat_action::{Action, ActionKind};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// Which subset of workspace files the finder lists when it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinderScope {
    /// Every non-ignored file under the workspace root, streamed in
    /// from a background walk.
    All,
    /// Files carrying uncommitted git changes (modified, staged, or
    /// untracked), snapshotted from the repository at open time.
    Modified,
}

pub struct FileFinderDelegate {
    workspace: WeakEntity<Workspace>,
    git_root: PathBuf,
    /// Active scope deciding which list [`Self::base_paths`] feeds the
    /// matcher. Flipped by [`Self::toggle_scope`].
    scope: FinderScope,
    /// Absolute paths of every file the streaming walker has produced
    /// so far. Extended in walker order by [`Self::extend_paths`];
    /// fuzzy-ranking sorts the indexed view separately so the natural
    /// walker order does not bleed into the match list. The walker runs
    /// regardless of scope so a toggle to [`FinderScope::All`] always has
    /// a populated list ready.
    all_paths: Vec<PathBuf>,
    /// Absolute paths of files with uncommitted git changes,
    /// (re)snapshotted whenever the finder enters [`FinderScope::Modified`].
    modified_paths: Vec<PathBuf>,
    /// Indices into [`Self::base_paths`] selected by the current
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
    /// When `Some`, the primary confirm path opens the chosen file
    /// in a freshly split pane along this axis instead of replacing
    /// the current pane. Set by openers that want split-on-confirm
    /// behavior (e.g. `OpenFileFinderHSplit`); a secondary modifier
    /// at confirm time still overrides this field.
    intended_split: Option<Axis>,
    /// Drain task that forwards walker batches into [`Self::paths`].
    /// Kept alive on the delegate so dropping the modal drops the
    /// task, which drops the receiver and signals the walker's
    /// `send`-failure exit path.
    _drain_task: Option<Task<()>>,
    /// Scratch [`Buffer`] backing the preview pane. The next slice
    /// writes the selected file's content into it on
    /// `selection_changed`. `None` when the delegate runs without a
    /// preview (test-only code path).
    pub(crate) preview_buffer: Option<Entity<Buffer>>,
    /// Stand-alone [`Editor`] rendering [`Self::preview_buffer`].
    /// `None` when the delegate runs without a preview (test-only
    /// code path).
    pub(crate) preview_editor: Option<Entity<Editor>>,
    /// Path the preview was last rendered for. Selection moves that
    /// resolve to the same path short-circuit so a repeat
    /// `selection_changed` does not re-read the file.
    preview_rendered_for: Option<PathBuf>,
}

impl FileFinderDelegate {
    pub fn new(workspace: WeakEntity<Workspace>, git_root: PathBuf) -> Self {
        Self {
            workspace,
            git_root,
            scope: FinderScope::All,
            all_paths: Vec::new(),
            modified_paths: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            query: String::new(),
            intended_split: None,
            _drain_task: None,
            preview_buffer: None,
            preview_editor: None,
            preview_rendered_for: None,
        }
    }

    /// Build a delegate whose primary confirm opens the chosen file
    /// in a fresh split along `axis` rather than the focused pane.
    pub fn with_split(workspace: WeakEntity<Workspace>, git_root: PathBuf, axis: Axis) -> Self {
        let mut delegate = Self::new(workspace, git_root);
        delegate.intended_split = Some(axis);
        delegate
    }

    /// Install a scratch [`Buffer`] and its [`Editor`] as the
    /// preview pair. Production callers always chain this onto
    /// [`Self::new`] / [`Self::with_split`] so the layout switches
    /// to the horizontal split with a populated preview slot.
    pub fn with_preview(mut self, buffer: Entity<Buffer>, editor: Entity<Editor>) -> Self {
        self.preview_buffer = Some(buffer);
        self.preview_editor = Some(editor);
        self
    }

    /// Append `batch` to [`Self::all_paths`]. Re-runs the filter when
    /// [`FinderScope::All`] is active so the newly-arrived paths join the
    /// visible matches; under [`FinderScope::Modified`] the batch is
    /// retained for a later toggle without disturbing the changed-file
    /// matches. Called from the drain task on every batch the walker emits.
    pub fn extend_paths(&mut self, batch: Vec<PathBuf>) {
        if batch.is_empty() {
            return;
        }
        self.all_paths.extend(batch);
        if self.scope == FinderScope::All {
            self.refilter();
        }
    }

    fn set_drain_task(&mut self, task: Task<()>) {
        self._drain_task = Some(task);
    }

    /// The path list backing the current [`Self::scope`]; match
    /// indices in [`Self::matches`] index into this slice.
    fn base_paths(&self) -> &[PathBuf] {
        match self.scope {
            FinderScope::All => &self.all_paths,
            FinderScope::Modified => &self.modified_paths,
        }
    }

    /// Flip between listing all workspace files and only the files with
    /// uncommitted git changes. Entering [`FinderScope::Modified`]
    /// re-snapshots the changed-file set so it reflects the working tree
    /// at toggle time; the background walk keeps the all-files list fresh
    /// either way.
    fn toggle_scope(&mut self, cx: &mut Context<'_, Picker<Self>>) {
        self.scope = match self.scope {
            FinderScope::All => {
                self.modified_paths = collect_changed_paths(&self.git_root, cx);
                FinderScope::Modified
            },
            FinderScope::Modified => FinderScope::All,
        };
        self.selected = 0;
        self.refilter();
        cx.notify();
    }

    fn refilter(&mut self) {
        let trimmed = self.query.trim();
        if trimmed.is_empty() {
            self.matches = (0..self.base_paths().len())
                .map(|i| (i, Vec::new()))
                .collect();
            if self.selected >= self.matches.len() {
                self.selected = self.matches.len().saturating_sub(1);
            }
            return;
        }

        let items = self.base_paths().iter().enumerate().map(|(i, path)| {
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
        self.base_paths().get(*idx).map(|p| p.as_path())
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
        secondary: Option<PickerSecondary>,
        window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        let Some(path) = self.selected_path().map(Path::to_path_buf) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let split_axis = secondary.map(secondary_to_axis).or(self.intended_split);
        // The keyboard confirm path reaches here inside the keystroke
        // observer's `Workspace` update lease (observer -> dispatch_action
        // -> modal layer -> picker confirm), so calling `workspace.update`
        // directly would re-enter it and panic. Defer until that lease
        // releases.
        window.defer(cx, move |_window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(axis) = split_axis {
                    ws.pane_tree().update(cx, |tree, cx| {
                        tree.split(axis, cx);
                    });
                }
                ws.open_paths(&[path], cx);
                ws.reset_input_mode_for_navigation(cx);
            });
        });
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, cx: &mut Context<'_, Picker<Self>>) {
        let Some(buffer) = self.preview_buffer.take() else {
            return;
        };
        let id = buffer.read(cx).read(|b| b.buffer_id());
        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            let _ = workspace.update(cx, |ws, cx| {
                ws.buffer_registry()
                    .update(cx, |reg, cx| reg.remove(id, cx));
            });
        });
    }

    fn selection_changed(&mut self, cx: &mut Context<'_, Picker<Self>>) {
        let Some(path) = self.selected_path().map(Path::to_path_buf) else {
            return;
        };
        if self.preview_rendered_for.as_deref() == Some(path.as_path()) {
            return;
        }
        let Some(editor) = self.preview_editor.clone() else {
            return;
        };
        let fs_host = cx.global::<FsHostGlobal>().0.clone();
        let text = read_preview_text(&*fs_host, &path);
        editor.update(cx, |ed, cx| ed.set_preview_target(path.clone(), &text, cx));
        self.preview_rendered_for = Some(path);
    }

    fn handle_action(
        &mut self,
        action: &dyn Action,
        _window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> bool {
        if action.kind() == ActionKind::FileFinderScopeToggle {
            self.toggle_scope(cx);
            return true;
        }
        false
    }

    fn render_header(&self, cx: &mut Context<'_, Picker<Self>>) -> Option<AnyElement> {
        Some(
            div()
                .px_2()
                .py_1()
                .text_color(cx.theme().muted_text)
                .child(SharedString::from(scope_label(self.scope)))
                .into_any_element(),
        )
    }

    fn render_preview(&self, cx: &mut Context<'_, Picker<Self>>) -> Option<AnyElement> {
        let editor = self.preview_editor.as_ref()?;
        let border = cx.theme().border_focused;
        Some(
            div()
                .border_1()
                .border_color(border)
                .p_2()
                .size_full()
                .overflow_hidden()
                .child(editor.clone())
                .into_any_element(),
        )
    }

    fn render_match(&self, ix: usize, cx: &mut Context<'_, Picker<Self>>) -> AnyElement {
        let Some((path_idx, matched)) = self.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(path) = self.base_paths().get(*path_idx) else {
            return div().into_any_element();
        };
        let display = display_path(path, &self.git_root);
        let theme = cx.theme();
        let color = theme.modal_picker;
        let runs = match_highlight_runs(
            &display,
            matched,
            HighlightStyle {
                color: Some(gpui::white()),
                ..Default::default()
            },
        );
        let label = StyledText::new(SharedString::from(display)).with_highlights(runs);
        div()
            .flex()
            .items_center()
            .px_2()
            .child(div().truncate().text_color(color).child(label))
            .into_any_element()
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

/// The section-header label for the finder's current [`FinderScope`].
fn scope_label(scope: FinderScope) -> &'static str {
    match scope {
        FinderScope::All => "All files",
        FinderScope::Modified => "Modified files",
    }
}

/// Read `path` through `fs_host`, truncating at
/// [`PREVIEW_BYTE_LIMIT`] on a UTF-8 char boundary. Returns
/// `"<unreadable>"` on read error so the preview pane always has
/// content to render. Mirrors `stoat/src/file_finder.rs:336-350`.
fn read_preview_text(fs_host: &dyn FsHost, path: &Path) -> String {
    let mut buf = Vec::new();
    if fs_host.read(path, &mut buf).is_err() {
        return "<unreadable>".to_string();
    }
    let limit = PREVIEW_BYTE_LIMIT.min(buf.len());
    match std::str::from_utf8(&buf[..limit]) {
        Ok(s) => s.to_string(),
        Err(err) => {
            let valid = err.valid_up_to();
            String::from_utf8_lossy(&buf[..valid]).into_owned()
        },
    }
}

fn secondary_to_axis(secondary: PickerSecondary) -> Axis {
    match secondary {
        PickerSecondary::OpenInRight => Axis::Vertical,
        PickerSecondary::OpenInDown => Axis::Horizontal,
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
    open_file_finder_internal(workspace, FinderScope::All, None, window, cx);
}

/// Open the file finder with a default split-on-confirm axis. The
/// chosen file lands in a freshly split pane along `axis` instead
/// of replacing the focused pane's content.
pub fn open_file_finder_split(
    workspace: &mut Workspace,
    axis: Axis,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    open_file_finder_internal(workspace, FinderScope::All, Some(axis), window, cx);
}

/// Open the file finder scoped to files with uncommitted git changes.
/// The changed-file set is snapshotted at open time; the chosen file
/// replaces the focused pane's content on confirm. Lists nothing when
/// the workspace root is not inside a discoverable repository.
pub fn open_changed_file_finder(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    open_file_finder_internal(workspace, FinderScope::Modified, None, window, cx);
}

fn open_file_finder_internal(
    workspace: &mut Workspace,
    scope: FinderScope,
    intended_split: Option<Axis>,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let git_root = workspace.git_root().clone();
    let weak_workspace = cx.weak_entity();

    let fs_host = cx.global::<FsHostGlobal>().0.clone();
    let executor = cx.global::<ExecutorGlobal>().0.clone();
    let (walk_tx, walk_rx) = unbounded_channel();
    spawn_walker(executor, fs_host, git_root.clone(), walk_tx).detach();

    let modified_paths = match scope {
        FinderScope::All => Vec::new(),
        FinderScope::Modified => collect_changed_paths(&git_root, cx),
    };

    let (preview_buffer, preview_editor) = workspace.build_preview_editor(cx);

    workspace.toggle_modal::<Picker<FileFinderDelegate>, _>(window, cx, move |window, cx| {
        let mut delegate = match intended_split {
            Some(axis) => FileFinderDelegate::with_split(weak_workspace, git_root, axis),
            None => FileFinderDelegate::new(weak_workspace, git_root),
        }
        .with_preview(preview_buffer, preview_editor);
        delegate.scope = scope;
        delegate.modified_paths = modified_paths;
        delegate.refilter();
        Picker::new(delegate, window, cx)
    });

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

/// Snapshot the absolute paths of every file with uncommitted git
/// changes in the workspace repository, sorted and deduplicated.
/// Empty when `git_root` is not inside a discoverable repository.
fn collect_changed_paths(git_root: &Path, cx: &App) -> Vec<PathBuf> {
    let git = cx.global::<GitHostGlobal>().0.clone();
    let Some(repo) = git.discover(git_root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = repo.changed_files().into_iter().map(|c| c.path).collect();
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::sync::Arc;
    use stoat::host::{FakeFs, FsWatchHost};
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext, fake_fs: Arc<FakeFs>) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(FsHostGlobal(fake_fs));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
        });
    }

    fn new_delegate(git_root: &str) -> FileFinderDelegate {
        FileFinderDelegate::new(WeakEntity::new_invalid(), PathBuf::from(git_root))
    }

    fn names(delegate: &FileFinderDelegate) -> Vec<String> {
        delegate
            .matches
            .iter()
            .map(|(i, _)| display_path(&delegate.base_paths()[*i], &delegate.git_root))
            .collect()
    }

    #[test]
    fn delegate_lists_no_paths_when_constructed() {
        let delegate = new_delegate("/repo");
        assert_eq!(delegate.match_count(), 0);
    }

    #[test]
    fn scope_label_reflects_finder_scope() {
        assert_eq!(scope_label(FinderScope::All), "All files");
        assert_eq!(scope_label(FinderScope::Modified), "Modified files");
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
        let before = delegate.all_paths.len();
        delegate.extend_paths(Vec::new());
        assert_eq!(delegate.all_paths.len(), before);
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
    fn selection_changed_loads_preview_content() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("hello.txt", "hello stoat\n")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_file_finder(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker: Entity<Picker<FileFinderDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("file finder modal is open");

        picker.update(h.vcx, |p, cx| p.set_selected_index(0, cx));
        h.vcx.run_until_parked();

        let (content, rendered_for) = picker.read_with(h.vcx, |p, cx| {
            let buffer = p
                .delegate()
                .preview_buffer
                .clone()
                .expect("preview buffer set");
            let content = buffer.read(cx).read(|b| b.rope().to_string());
            let rendered_for = p.delegate().preview_rendered_for.clone();
            (content, rendered_for)
        });
        assert_eq!(content, "hello stoat\n");
        assert_eq!(rendered_for, Some(PathBuf::from("/repo/hello.txt")));
    }

    #[test]
    fn selection_changed_short_circuits_on_repeat_path() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("only.txt", "data\n")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_file_finder(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker: Entity<Picker<FileFinderDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("file finder modal is open");

        picker.update(h.vcx, |p, cx| p.set_selected_index(0, cx));
        h.vcx.run_until_parked();

        let version_after_first = picker.read_with(h.vcx, |p, cx| {
            let buf = p
                .delegate()
                .preview_buffer
                .clone()
                .expect("preview buffer set");
            buf.read(cx).read(|b| b.rope().to_string())
        });
        assert_eq!(version_after_first, "data\n");

        // Mutate the on-disk file behind the picker's back; the
        // short-circuit means the preview should NOT pick up the new
        // content because the path hasn't changed.
        h.workspace.read_with(h.vcx, |_, cx| {
            cx.global::<FsHostGlobal>()
                .0
                .write(Path::new("/repo/only.txt"), b"new content\n")
                .expect("FakeFs::write");
        });
        picker.update(h.vcx, |p, cx| p.set_selected_index(0, cx));
        h.vcx.run_until_parked();

        let version_after_second = picker.read_with(h.vcx, |p, cx| {
            let buf = p
                .delegate()
                .preview_buffer
                .clone()
                .expect("preview buffer set");
            buf.read(cx).read(|b| b.rope().to_string())
        });
        assert_eq!(
            version_after_second, version_after_first,
            "repeat selection_changed on the same path must short-circuit",
        );
    }

    #[test]
    fn dismissed_removes_preview_buffer_from_registry() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_file_finder(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker: Entity<Picker<FileFinderDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("file finder modal is open");

        let preview_id = picker.read_with(h.vcx, |p, cx| {
            let buffer = p
                .delegate()
                .preview_buffer
                .clone()
                .expect("preview installed");
            buffer.read(cx).read(|b| b.buffer_id())
        });

        picker.update(h.vcx, |p, cx| p.delegate_mut().dismissed(cx));
        h.vcx.run_until_parked();

        let has_buffer = h.workspace.read_with(h.vcx, |w, cx| {
            w.buffer_registry().read(cx).get(preview_id).is_some()
        });
        assert!(
            !has_buffer,
            "preview buffer must be dropped from the registry on dismiss",
        );
    }

    #[test]
    fn open_file_finder_installs_preview_editor() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_file_finder(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker: Entity<Picker<FileFinderDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("file finder modal is open");
        picker.read_with(h.vcx, |p, _| {
            assert!(
                p.delegate().preview_buffer.is_some(),
                "preview buffer should be installed by open_file_finder",
            );
            assert!(
                p.delegate().preview_editor.is_some(),
                "preview editor should be installed by open_file_finder",
            );
        });
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
    fn with_split_stores_intended_axis() {
        let delegate = FileFinderDelegate::with_split(
            WeakEntity::new_invalid(),
            PathBuf::from("/repo"),
            Axis::Vertical,
        );
        assert_eq!(delegate.intended_split, Some(Axis::Vertical));
    }

    #[test]
    fn open_file_finder_vsplit_confirm_opens_in_new_right_split() {
        use stoat::pane::Axis;
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "alpha"), ("b.rs", "beta")]);
        let h = new_harness(&mut cx, fake_fs);

        let pane_tree = h.workspace.read_with(h.vcx, |w, _| w.pane_tree().clone());
        assert_eq!(pane_tree.read_with(h.vcx, |t, _| t.pane_count()), 1);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_file_finder_split(w, Axis::Vertical, window, cx);
        });
        h.vcx.run_until_parked();

        let picker: Entity<Picker<FileFinderDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("file finder modal is open");
        picker.update(h.vcx, |p, _| {
            let delegate = p.delegate_mut();
            delegate.all_paths = vec![PathBuf::from("/repo/b.rs")];
            delegate.matches = vec![(0, Vec::new())];
            delegate.selected = 0;
        });

        picker.update_in(h.vcx, |p, window, cx| {
            p.delegate_mut().confirm(None, window, cx);
        });
        h.vcx.run_until_parked();

        assert_eq!(
            pane_tree.read_with(h.vcx, |t, _| t.pane_count()),
            2,
            "vsplit confirm should grow pane count to 2",
        );
        let focused_id = pane_tree.read_with(h.vcx, |t, _| t.focus());
        let focused_pane = pane_tree
            .read_with(h.vcx, |t, _| t.pane(focused_id).cloned())
            .expect("focused pane registered");
        assert_eq!(
            focused_pane.read_with(h.vcx, |p, _| p.items().len()),
            1,
            "new pane should host one editor for the chosen file",
        );
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
                .all_paths
                .iter()
                .map(|path| display_path(path, p.delegate().git_root.as_path()))
                .collect::<Vec<_>>()
        });
        listed.sort();
        assert_eq!(listed, vec!["readme", "src/lib.rs", "src/main.rs"]);
    }
}
