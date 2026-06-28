use crate::{
    host::{FsHost, GitHost},
    input_view::{InputView, SubmitTarget},
    paths,
    picker::{PickList, Preview, PreviewSource},
    workspace::Workspace,
};
use std::path::{Path, PathBuf};
use stoat_scheduler::{Executor, Task};
use tokio::sync::mpsc::{error::TryRecvError, UnboundedReceiver};

/// Which subset of files the finder currently lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinderScope {
    /// Every file under `git_root` that is not gitignored. Snapshotted at
    /// open time.
    All,
    /// Files with uncommitted git changes. Refreshed on every scope toggle
    /// so the list stays current.
    Modified,
    /// Currently-open path-bound buffers from the workspace's
    /// [`BufferRegistry`]. Captured at open time. Reachable only through
    /// the dedicated `OpenBufferPicker` action; Shift-Tab from this scope
    /// flips back to [`FinderScope::All`].
    Buffers,
}

/// What the finder should do with the selected file when the user submits.
/// Set at open time; consumed by the submit handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenIntent {
    /// Open the file in the focused pane, replacing its current view.
    Replace,
    /// Split the focused pane horizontally first, then open the file in
    /// the new pane below.
    HSplit,
    /// Split the focused pane vertically first, then open the file in
    /// the new pane to the right.
    VSplit,
}

pub struct FileFinder {
    pub(crate) input: InputView,
    /// Mode to restore when the finder closes.
    pub(crate) previous_mode: String,
    /// What submit should do with the selected file.
    pub(crate) open_intent: OpenIntent,
    pub(crate) scope: FinderScope,
    pub(crate) git_root: PathBuf,
    /// Absolute paths of every tracked file. Grows as
    /// [`FileFinder::pump_walk`] drains batches off [`Self::walk_rx`].
    pub(crate) all_paths: Vec<PathBuf>,
    /// Streaming receiver fed by the spawned walker; each message is a
    /// batch of paths discovered since the last batch. Cleared once
    /// the sender drops, signalling the walk is exhausted.
    walk_rx: Option<UnboundedReceiver<Vec<PathBuf>>>,
    /// Walker task held only to keep the spawned worker alive --
    /// dropping the [`Task`] would cancel the in-flight walk on
    /// runtimes that propagate cancellation. The walker reports its
    /// progress through [`Self::walk_rx`], not through the task value.
    _walk_task: Task<()>,
    /// Absolute paths of currently-modified files. Re-queried on scope toggle.
    pub(crate) modified_paths: Vec<PathBuf>,
    /// Absolute paths of currently-open buffers. Captured once at open time;
    /// not re-queried on scope toggle.
    pub(crate) buffer_paths: Vec<PathBuf>,
    /// Fuzzy result list over the active scope's paths. The active scope's
    /// path `Vec` is copied into [`PickList::base`] on each refilter. The
    /// renderer reads its `filtered`/`match_indices`/`selected`.
    pub(crate) picklist: PickList,
    /// Last input text that was run through the matcher. Lets
    /// [`FileFinder::refilter_from_input`] short-circuit when the
    /// render loop ticks without any typing.
    pub(crate) last_filter_text: String,
    pub(crate) last_filter_scope: FinderScope,
    /// Preview pane shown beside the result list, reused across selection
    /// changes. Created at [`FileFinder::new`], disposed in
    /// [`FileFinder::dispose`].
    pub(crate) preview: Preview,
}

impl FileFinder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ws: &mut Workspace,
        executor: Executor,
        previous_mode: String,
        open_intent: OpenIntent,
        initial_scope: FinderScope,
        git_root: PathBuf,
        walk_rx: UnboundedReceiver<Vec<PathBuf>>,
        walk_task: Task<()>,
        modified_paths: Vec<PathBuf>,
        buffer_paths: Vec<PathBuf>,
    ) -> Self {
        let input = InputView::create(
            ws,
            executor.clone(),
            SubmitTarget::FileFinder,
            "",
            "prompt",
            1,
        );
        let preview = Preview::new(ws, executor);

        let scope = initial_scope;
        let all_paths: Vec<PathBuf> = Vec::new();
        let mut picklist = PickList {
            base: match scope {
                FinderScope::All => all_paths.clone(),
                FinderScope::Modified => modified_paths.clone(),
                FinderScope::Buffers => buffer_paths.clone(),
            },
            ..PickList::default()
        };
        picklist.refilter("", &git_root);

        Self {
            input,
            previous_mode,
            open_intent,
            scope,
            git_root,
            all_paths,
            walk_rx: Some(walk_rx),
            _walk_task: walk_task,
            modified_paths,
            buffer_paths,
            picklist,
            last_filter_text: String::new(),
            last_filter_scope: scope,
            preview,
        }
    }

    pub(crate) fn scope(&self) -> FinderScope {
        self.scope
    }

    /// Base list for the current scope.
    pub(crate) fn base_paths(&self) -> &[PathBuf] {
        match self.scope {
            FinderScope::All => &self.all_paths,
            FinderScope::Modified => &self.modified_paths,
            FinderScope::Buffers => &self.buffer_paths,
        }
    }

    /// Absolute path of the currently selected filtered row, if any.
    pub(crate) fn selected_path(&self) -> Option<&Path> {
        self.picklist.selected_path()
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        self.picklist.move_selection(delta);
    }

    /// Flip the scope, optionally refreshing the Modified list before
    /// rerunning the filter against the new base. `git_host` is borrowed
    /// only when switching *to* Modified so the caller can skip the
    /// discover call when switching away. From [`FinderScope::Buffers`]
    /// the toggle returns to [`FinderScope::All`]; the Buffers scope is
    /// reachable only through the dedicated `OpenBufferPicker` action,
    /// not through this toggle.
    pub(crate) fn toggle_scope(&mut self, git_host: &dyn GitHost) {
        self.scope = match self.scope {
            FinderScope::All => {
                self.modified_paths = query_modified(git_host, &self.git_root);
                FinderScope::Modified
            },
            FinderScope::Modified => FinderScope::All,
            FinderScope::Buffers => FinderScope::All,
        };
        self.picklist.selected = 0;
        self.last_filter_text = String::new();
        // Force refilter + preview resync on next render.
        self.picklist.filtered.clear();
        self.picklist.match_indices.clear();
    }

    /// Drain every batch the walker has emitted since the last call,
    /// extending [`Self::all_paths`]. The receiver closes when the walker
    /// is exhausted, after which the file finder no longer polls.
    /// Returns `true` when at least one batch was consumed; the
    /// caller invalidates filter caches in that case so the next
    /// [`Self::refilter_from_input`] re-runs the matcher against the
    /// now-larger base.
    pub(crate) fn pump_walk(&mut self) -> bool {
        let Some(rx) = self.walk_rx.as_mut() else {
            return false;
        };
        let mut received_any = false;
        loop {
            match rx.try_recv() {
                Ok(batch) => {
                    self.all_paths.extend(batch);
                    received_any = true;
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.walk_rx = None;
                    break;
                },
            }
        }
        if received_any {
            self.last_filter_text.clear();
            self.picklist.filtered.clear();
            self.picklist.match_indices.clear();
        }
        received_any
    }

    /// Re-run the matcher if the input text or scope has changed since last
    /// filter. Called from the renderer so typing picks up without a dedicated
    /// sync hook. Drains any pending walk result first so freshly arrived
    /// paths participate in the same render tick.
    pub(crate) fn refilter_from_input(&mut self, ws: &Workspace) {
        self.pump_walk();
        let text = self.input.text(ws);
        if text == self.last_filter_text
            && self.scope == self.last_filter_scope
            && !self.picklist.filtered.is_empty()
        {
            return;
        }
        self.picklist.base = self.base_paths().to_vec();
        self.picklist.refilter(&text, &self.git_root);
        self.last_filter_text = text;
        self.last_filter_scope = self.scope;
    }

    /// Sync the preview pane to the current selection, reading the selected
    /// file from disk. Clears the pane when nothing is selected.
    pub(crate) fn sync_preview(
        &mut self,
        ws: &mut Workspace,
        fs_host: &dyn FsHost,
        language_registry: &stoat_language::LanguageRegistry,
    ) {
        match self
            .selected_path()
            .map(|p| PreviewSource::File(p.to_path_buf()))
        {
            Some(source) => self.preview.sync(ws, fs_host, language_registry, source),
            None => self.preview.clear(ws),
        }
    }

    /// Tear down owned editor slots. Called on every finder-close path.
    /// Removes the preview buffer from [`crate::buffer_registry::BufferRegistry`]
    /// so each file finder lifetime returns the registry to its
    /// pre-open size; without this the preview entry would accumulate
    /// across opens.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
        self.preview.dispose(ws);
    }
}

/// Query git for currently-modified files (staged + unstaged), returning
/// absolute paths. Empty when no repo or no changes.
pub(crate) fn query_modified(git_host: &dyn GitHost, git_root: &Path) -> Vec<PathBuf> {
    let Some(repo) = git_host.discover(git_root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = repo.changed_files().into_iter().map(|c| c.path).collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Produce the user-facing display form of an absolute path relative to
/// `git_root`. Falls back to [`paths::display_relative`] so anything outside
/// the root still renders something readable.
fn display_for(path: &Path, git_root: &Path) -> String {
    paths::display_relative(path, git_root)
}

/// Display string for a filtered row: the repo-relative path. Used by the
/// renderer and by tests.
pub(crate) fn display_row(path: &Path, git_root: &Path) -> String {
    display_for(path, git_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn display_row_strips_git_root_prefix() {
        let git_root = p("/work/stoat");
        assert_eq!(
            display_row(&p("/work/stoat/src/main.rs"), &git_root),
            "src/main.rs"
        );
    }

    /// Path used as the workspace root in walker unit tests. Every entry
    /// inserted into the FakeFs lives under this prefix; the helper below
    /// strips it so assertions compare repo-relative paths.
    const WALK_ROOT: &str = "/repo";

    fn seeded_fake_fs(files: &[(&str, &str)]) -> crate::host::FakeFs {
        let fs = crate::host::FakeFs::new();
        let root = Path::new(WALK_ROOT);
        fs.insert_files(
            files
                .iter()
                .map(|(rel, content)| (root.join(rel), content.as_bytes())),
        );
        fs
    }

    fn walked_rels(fs: &dyn FsHost) -> Vec<String> {
        let root = Path::new(WALK_ROOT);
        let mut rels: Vec<String> = fs
            .walk_workspace_files(root)
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        rels.sort();
        rels
    }

    #[test]
    fn walk_workspace_files_returns_files_not_dirs() {
        let fs = seeded_fake_fs(&[("a.rs", "a"), ("sub/b.rs", "b")]);
        assert_eq!(walked_rels(&fs), vec!["a.rs", "sub/b.rs"]);
    }

    #[test]
    fn walk_workspace_files_ignores_dot_git() {
        let fs = seeded_fake_fs(&[
            (".git/HEAD", "ref: refs/heads/main"),
            (".git/config", "[core]"),
            (".git/refs/heads/main", "deadbeef"),
            ("src/main.rs", "fn main() {}"),
        ]);
        assert_eq!(walked_rels(&fs), vec!["src/main.rs"]);
    }

    #[test]
    fn walk_workspace_files_ignores_baked_in_dirs() {
        let fs = seeded_fake_fs(&[
            ("target/debug/foo", "bin"),
            ("node_modules/pkg/index.js", "module.exports = {}"),
            ("src/main.rs", "fn main() {}"),
        ]);
        assert_eq!(walked_rels(&fs), vec!["src/main.rs"]);
    }

    #[test]
    fn walk_workspace_files_honors_stoatignore() {
        let fs = seeded_fake_fs(&[
            (".stoatignore", "vendor/\n"),
            ("vendor/blob.rs", "// generated"),
            ("src/main.rs", "fn main() {}"),
        ]);
        assert_eq!(
            walked_rels(&fs),
            vec![".stoatignore".to_string(), "src/main.rs".to_string()],
        );
    }

    #[test]
    fn walk_workspace_files_honors_nested_gitignore() {
        let fs = seeded_fake_fs(&[
            ("src/main.rs", "fn main() {}"),
            ("src/generated/.gitignore", "*.rs\n"),
            ("src/generated/auto.rs", "// auto"),
            ("src/generated/keep.txt", "keep"),
        ]);
        assert_eq!(
            walked_rels(&fs),
            vec![
                "src/generated/.gitignore".to_string(),
                "src/generated/keep.txt".to_string(),
                "src/main.rs".to_string(),
            ],
        );
    }

    #[test]
    fn walk_workspace_files_inner_negation_overrides_outer_ignore() {
        let fs = seeded_fake_fs(&[
            (".gitignore", "*.log\n"),
            ("trace.log", "outer"),
            ("logs/.gitignore", "!*.log\n"),
            ("logs/trace.log", "inner"),
        ]);
        assert_eq!(
            walked_rels(&fs),
            vec![
                ".gitignore".to_string(),
                "logs/.gitignore".to_string(),
                "logs/trace.log".to_string(),
            ],
        );
    }

    #[test]
    fn walk_workspace_files_still_walks_non_git_dotfiles() {
        let fs = seeded_fake_fs(&[
            (".claude/settings.json", "{}"),
            (".vscode/launch.json", "{}"),
            ("src/main.rs", "fn main() {}"),
        ]);
        assert_eq!(
            walked_rels(&fs),
            vec![
                ".claude/settings.json".to_string(),
                ".vscode/launch.json".to_string(),
                "src/main.rs".to_string(),
            ],
        );
    }

    // ----- TestHarness integration tests -----

    use crate::test_harness::TestHarness;

    /// Insert `files` into the harness' [`crate::host::FakeFs`] under a
    /// fixed virtual root and point the active workspace at it. Returns the
    /// virtual root so callers that need to seed extra git state (or assert
    /// on absolute paths) can join against it.
    fn seed_finder_workspace(h: &mut TestHarness, files: &[(&str, &str)]) -> PathBuf {
        let root = PathBuf::from("/stoat-finder-test");
        h.fake_fs().insert_files(
            files
                .iter()
                .map(|(rel, content)| (root.join(rel), content.as_bytes())),
        );
        h.stoat.active_workspace_mut().git_root = root.clone();
        root
    }

    #[test]
    fn space_p_opens_finder_and_switches_to_prompt_mode() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "fn a() {}")]);
        h.type_keys("space p");
        assert!(h.stoat.file_finder.is_some(), "finder not opened");
        assert_eq!(h.snapshot().mode, "prompt");
    }

    #[test]
    fn escape_closes_finder_and_restores_mode() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "")]);
        h.type_keys("space p");
        h.type_keys("escape");
        assert!(h.stoat.file_finder.is_none(), "finder still open");
        assert_eq!(h.snapshot().mode, "normal");
    }

    #[test]
    fn ctrl_c_closes_finder() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "")]);
        h.type_keys("space p");
        h.type_keys("Ctrl-c");
        assert!(h.stoat.file_finder.is_none());
        assert_eq!(h.snapshot().mode, "normal");
    }

    #[test]
    fn second_open_is_noop() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "")]);
        h.type_keys("space p");
        let ptr_before = h.stoat.file_finder.as_ref().unwrap() as *const FileFinder;
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        let ptr_after = h.stoat.file_finder.as_ref().unwrap() as *const FileFinder;
        assert_eq!(ptr_before, ptr_after, "re-open should not replace state");
    }

    #[test]
    fn enter_dispatches_open_file_for_selected_path() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("target.txt", "loaded via finder")]);
        h.type_keys("space p");
        // Only one file in the workspace, so it is the selected row.
        h.type_keys("enter");
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        assert!(
            frame.content.contains("loaded via finder"),
            "file content missing from pane:\n{}",
            frame.content
        );
        assert!(h.stoat.file_finder.is_none());
    }

    #[test]
    fn space_a_f_opens_finder_with_hsplit_intent() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("target.txt", "loaded via finder")]);
        h.type_keys("space a f");
        let finder = h.stoat.file_finder.as_ref().expect("finder should be open");
        assert_eq!(finder.open_intent, OpenIntent::HSplit);
        assert_eq!(h.snapshot().mode, "prompt");
    }

    #[test]
    fn space_a_capital_f_opens_finder_with_vsplit_intent() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("target.txt", "loaded via finder")]);
        h.type_keys("space a F");
        let finder = h.stoat.file_finder.as_ref().expect("finder should be open");
        assert_eq!(finder.open_intent, OpenIntent::VSplit);
    }

    #[test]
    fn enter_with_hsplit_intent_creates_new_pane_and_opens_file() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("target.txt", "loaded via finder")]);
        assert_eq!(h.snapshot().pane_count, 1);

        h.type_keys("space a f");
        h.type_keys("enter");

        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 2, "split should create a second pane");
        assert!(
            frame.content.contains("loaded via finder"),
            "file content missing from frame:\n{}",
            frame.content
        );
        assert!(h.stoat.file_finder.is_none());
    }

    #[test]
    fn space_g_opens_finder_in_modified_scope() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[("a.rs", "v1\n"), ("b.rs", "v1\n"), ("c.rs", "v1\n")],
        );
        {
            let mut builder = h.fake_git().add_repo(&root).with_fs(h.fake_fs());
            builder.head_file("a.rs", "v1\n");
            builder.modified("b.rs", "v1\n", "v2\n");
            builder.head_file("c.rs", "v1\n");
        }

        h.type_keys("space g");
        let finder = h.stoat.file_finder.as_ref().expect("finder should be open");
        assert_eq!(finder.scope(), FinderScope::Modified);
        let base: Vec<PathBuf> = finder.base_paths().to_vec();
        assert_eq!(base.len(), 1, "Modified scope should list only b.rs");
        assert!(base[0].ends_with("b.rs"));
        assert_eq!(h.snapshot().mode, "prompt");
    }

    #[test]
    fn space_b_b_opens_finder_in_buffers_scope() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[
                ("a.rs", "fn a() {}"),
                ("b.rs", "fn b() {}"),
                ("c.rs", "fn c() {}"),
            ],
        );
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("a.rs"),
            },
        );
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("c.rs"),
            },
        );

        h.type_keys("space b b");
        let finder = h.stoat.file_finder.as_ref().expect("finder should be open");
        assert_eq!(finder.scope(), FinderScope::Buffers);
        let base: Vec<PathBuf> = finder.base_paths().to_vec();
        assert_eq!(base.len(), 2, "Buffers scope should list only open buffers");
        assert!(base.iter().any(|p| p.ends_with("a.rs")));
        assert!(base.iter().any(|p| p.ends_with("c.rs")));
        assert!(!base.iter().any(|p| p.ends_with("b.rs")));
        assert_eq!(h.snapshot().mode, "prompt");
    }

    #[test]
    fn backtab_from_buffer_picker_toggles_to_all() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[
                ("a.rs", "fn a() {}"),
                ("b.rs", "fn b() {}"),
                ("c.rs", "fn c() {}"),
            ],
        );
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("a.rs"),
            },
        );

        h.type_keys("space b b");
        h.type_keys("backtab");
        let finder = h.stoat.file_finder.as_ref().unwrap();
        assert_eq!(finder.scope(), FinderScope::All);
        assert_eq!(finder.base_paths().len(), 3);
    }

    #[test]
    fn backtab_from_changed_file_picker_toggles_back_to_all() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[("a.rs", "v1\n"), ("b.rs", "v1\n"), ("c.rs", "v1\n")],
        );
        {
            let mut builder = h.fake_git().add_repo(&root).with_fs(h.fake_fs());
            builder.head_file("a.rs", "v1\n");
            builder.modified("b.rs", "v1\n", "v2\n");
            builder.head_file("c.rs", "v1\n");
        }

        h.type_keys("space g");
        h.type_keys("backtab");
        let finder = h.stoat.file_finder.as_ref().unwrap();
        assert_eq!(finder.scope(), FinderScope::All);
        assert_eq!(finder.base_paths().len(), 3);
    }

    #[test]
    fn backtab_toggles_scope_to_modified() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[("a.rs", "v1\n"), ("b.rs", "v1\n"), ("c.rs", "v1\n")],
        );
        // Seed the fake git repo so only b.rs is reported as modified.
        {
            let mut builder = h.fake_git().add_repo(&root).with_fs(h.fake_fs());
            builder.head_file("a.rs", "v1\n");
            builder.modified("b.rs", "v1\n", "v2\n");
            builder.head_file("c.rs", "v1\n");
        }

        h.type_keys("space p");
        {
            let finder = h.stoat.file_finder.as_ref().unwrap();
            assert_eq!(finder.scope(), FinderScope::All);
            let base: Vec<PathBuf> = finder.base_paths().to_vec();
            assert_eq!(base.len(), 3, "All scope should list all 3 files");
        }
        h.type_keys("backtab");
        {
            let finder = h.stoat.file_finder.as_ref().unwrap();
            assert_eq!(finder.scope(), FinderScope::Modified);
            let base: Vec<PathBuf> = finder.base_paths().to_vec();
            assert_eq!(base.len(), 1);
            assert!(base[0].ends_with("b.rs"));
        }
    }

    #[test]
    fn walk_completion_signals_redraw_notify() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", ""), ("b.rs", "")]);
        h.type_keys("space p");
        let notified = h.stoat.redraw_notify.notified();
        tokio::pin!(notified);
        assert!(
            notified.enable(),
            "walk task should signal redraw_notify on completion so the \
             main loop wakes up and renders the populated list",
        );
    }

    #[test]
    fn typing_narrows_filtered_list() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(
            &mut h,
            &[("alpha.rs", ""), ("beta.rs", ""), ("gamma.rs", "")],
        );
        h.type_keys("space p");
        // refilter is driven by the render loop; force a snapshot so
        // filtered reflects the current (empty) query.
        let _ = h.snapshot();
        assert_eq!(
            h.stoat
                .file_finder
                .as_ref()
                .unwrap()
                .picklist
                .filtered
                .len(),
            3
        );
        h.type_text("alp");
        let _ = h.snapshot();
        let finder = h.stoat.file_finder.as_ref().unwrap();
        assert_eq!(finder.picklist.filtered.len(), 1);
        let idx = finder.picklist.filtered[0];
        assert!(finder.base_paths()[idx].ends_with("alpha.rs"));
    }

    // ----- Snapshot tests -----

    /// Point the active workspace at a fixed, nonexistent path so the walker
    /// returns nothing. Produces a stable empty-list snapshot regardless of
    /// test-run cwd; the workspace basename also renders deterministically
    /// in the status bar.
    fn seed_empty_finder_workspace(h: &mut TestHarness) {
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/stoat-finder-test-empty");
    }

    #[test]
    fn snapshot_file_finder_empty() {
        let mut h = crate::Stoat::test();
        seed_empty_finder_workspace(&mut h);
        h.type_keys("space p");
        h.assert_snapshot("file_finder_empty");
    }

    #[test]
    fn snapshot_file_finder_tiny_terminal_no_render() {
        let mut h = TestHarness::with_size(30, 8);
        seed_empty_finder_workspace(&mut h);
        h.type_keys("space p");
        h.assert_snapshot("file_finder_tiny_terminal_no_render");
    }

    #[test]
    fn snapshot_file_finder_multi_token_highlight() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(
            &mut h,
            &[
                ("src/foo.rs", "fn foo() {}"),
                ("src/bar.rs", "fn bar() {}"),
                ("docs/foo.md", "foo"),
            ],
        );
        h.type_keys("space p");
        h.type_text(".rs foo");
        h.assert_snapshot("file_finder_multi_token_highlight");
    }

    /// The finder modal is opaque, so a short preview file's blank rows render
    /// blank rather than leaking the editor content the centered modal is drawn
    /// over.
    #[test]
    fn snapshot_finder_preview_clears_short_file_background() {
        let mut h = TestHarness::with_size(120, 30);
        let filler: String = (0..40)
            .map(|i| format!("background row {i:02} {}\n", "=".repeat(100)))
            .collect();
        let root = seed_finder_workspace(
            &mut h,
            &[("short.txt", "alpha\nbravo\n"), ("filler.txt", &filler)],
        );
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("filler.txt"),
            },
        );
        h.settle();

        h.type_keys("space p");
        h.type_text("short");
        h.assert_snapshot("finder_preview_clears_short_file_background");
    }

    #[test]
    fn preview_buffer_assigned_language_for_selected_path() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("main.rs", "fn main() {}\n")]);
        h.type_keys("space p");
        h.snapshot();
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        let preview_id = finder.preview.buffer;
        let ws = h.stoat.active_workspace();
        let lang = ws.buffers.language_for(preview_id).expect("language set");
        assert_eq!(lang.name, "rust");
    }

    #[test]
    fn switching_preview_clears_prior_syntax_state() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "fn a() {}\n"), ("b.toml", "[pkg]\n")]);
        h.type_keys("space p");
        h.snapshot();

        let preview_id = h
            .stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .preview
            .buffer;
        let lang_first = h
            .stoat
            .active_workspace()
            .buffers
            .language_for(preview_id)
            .expect("first language");

        h.type_keys("down");
        h.snapshot();

        let lang_second = h
            .stoat
            .active_workspace()
            .buffers
            .language_for(preview_id)
            .expect("second language");
        assert_ne!(
            lang_first.name, lang_second.name,
            "language should reflect new path",
        );
    }

    #[test]
    fn preview_buffer_evicted_on_close() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("main.rs", "fn main() {}\n")]);
        h.type_keys("space p");
        let preview_id = h
            .stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .preview
            .buffer;
        assert!(h.stoat.active_workspace().buffers.get(preview_id).is_some());

        h.type_keys("escape");

        assert!(h.stoat.file_finder.is_none());
        assert!(
            h.stoat.active_workspace().buffers.get(preview_id).is_none(),
            "preview buffer should be evicted on close",
        );
        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .preview_buffer_ids()
                .is_empty(),
            "no preview buffers remain after close",
        );
    }

    #[test]
    fn finder_input_scratch_not_left_dirty_on_close() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("main.rs", "fn main() {}\n")]);
        let baseline = h.stoat.active_workspace().buffers.dirty_buffers().len();

        h.type_keys("space p");
        h.type_text("main");
        h.type_keys("escape");

        assert!(h.stoat.file_finder.is_none());
        assert_eq!(
            h.stoat.active_workspace().buffers.dirty_buffers().len(),
            baseline,
            "input scratch must not linger as a dirty buffer after the finder closes",
        );
    }

    #[test]
    fn buffer_preview_source_shows_live_text_not_disk() {
        let mut h = crate::Stoat::test();
        let executor = h.stoat.executor.clone();
        let language_registry = h.stoat.language_registry.clone();
        // The fs is empty, so a stray disk read would render a placeholder.
        // Matching the live text proves the Buffer source never touches disk.
        let fs = crate::host::FakeFs::new();

        let ws = h.stoat.active_workspace_mut();
        let (id, _) = ws.buffers.open(&PathBuf::from("/mem/note.txt"), "saved\n");
        {
            let buffer = ws.buffers.get(id).expect("source buffer");
            let mut guard = buffer.write().expect("source buffer poisoned");
            let len = guard.snapshot.visible_text.len();
            guard.edit(0..len, "edited in memory\n");
        }

        let mut preview = Preview::new(ws, executor);
        preview.sync(ws, &fs, &language_registry, PreviewSource::Buffer(id));

        let shown = {
            let buffer = ws.buffers.get(preview.buffer).expect("preview buffer");
            let guard = buffer.read().expect("preview buffer poisoned");
            guard.rope().to_string()
        };
        assert_eq!(shown, "edited in memory\n");

        preview.dispose(ws);
    }
}
