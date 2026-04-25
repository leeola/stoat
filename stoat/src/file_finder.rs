use crate::{
    buffer::BufferId,
    editor_state::{EditorId, EditorState},
    host::{FsHost, GitHost},
    input_view::{InputView, SubmitTarget},
    paths,
    workspace::Workspace,
};
use ignore::{
    gitignore::{Gitignore, GitignoreBuilder},
    WalkBuilder,
};
use nucleo::{
    pattern::{Atom, AtomKind, CaseMatching, Normalization},
    Matcher, Utf32Str,
};
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
use stoat_scheduler::Executor;
use stoat_text::{Bias, SelectionGoal};

/// Preview content cap. Keeps preview reads bounded so a stray large or binary
/// file never stalls the render thread.
pub(crate) const PREVIEW_BYTE_LIMIT: usize = 128 * 1024;

/// Baked-in default ignore patterns applied to every workspace. Parsed with
/// gitignore semantics at walker-build time; supplements (but does not
/// override) any per-repo `.stoatignore` the walker picks up at runtime.
const DEFAULT_STOATIGNORE: &str = include_str!("../../.stoatignore");

fn fuzzy_matcher() -> &'static Mutex<Matcher> {
    static MATCHER: OnceLock<Mutex<Matcher>> = OnceLock::new();
    MATCHER.get_or_init(|| Mutex::new(Matcher::default()))
}

/// Which subset of files the finder currently lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinderScope {
    /// Every file under `git_root` that is not gitignored. Snapshotted at
    /// open time.
    All,
    /// Files with uncommitted git changes. Refreshed on every scope toggle
    /// so the list stays current.
    Modified,
}

pub struct FileFinder {
    pub(crate) input: InputView,
    /// Mode to restore when the finder closes.
    pub(crate) previous_mode: String,
    pub(crate) scope: FinderScope,
    pub(crate) git_root: PathBuf,
    /// Absolute paths of every tracked file, captured once at open time.
    pub(crate) all_paths: Vec<PathBuf>,
    /// Absolute paths of currently-modified files. Re-queried on scope toggle.
    pub(crate) modified_paths: Vec<PathBuf>,
    /// Indices into the currently active scope's `Vec`, after filtering.
    pub(crate) filtered: Vec<usize>,
    pub(crate) selected: usize,
    /// Last input text that was run through the matcher. Lets
    /// [`FileFinder::refilter_from_input`] short-circuit when the
    /// render loop ticks without any typing.
    pub(crate) last_filter_text: String,
    pub(crate) last_filter_scope: FinderScope,
    /// Scratch buffer + editor used to render the preview pane. The buffer
    /// is reused across selection changes: its rope is replaced with the
    /// newly-selected file's content. Created at [`FileFinder::new`];
    /// disposed in [`FileFinder::dispose`].
    pub(crate) preview_editor: EditorId,
    pub(crate) preview_buffer: BufferId,
    /// Absolute path whose content currently lives in the preview buffer,
    /// or `None` if the preview is still empty (first render).
    pub(crate) preview_rendered_for: Option<PathBuf>,
}

impl FileFinder {
    pub fn new(
        ws: &mut Workspace,
        executor: Executor,
        previous_mode: String,
        git_root: PathBuf,
        all_paths: Vec<PathBuf>,
        modified_paths: Vec<PathBuf>,
    ) -> Self {
        let input = InputView::create(
            ws,
            executor.clone(),
            SubmitTarget::FileFinder,
            "",
            "prompt",
            1,
        );
        let (preview_buffer, shared_buffer) = ws.buffers.new_scratch();
        let preview_editor_state = EditorState::new(preview_buffer, shared_buffer, executor);
        let preview_editor = ws.editors.insert(preview_editor_state);

        let scope = FinderScope::All;
        let mut filtered = Vec::new();
        let mut selected = 0;
        refilter(
            "",
            scope,
            &all_paths,
            &modified_paths,
            &git_root,
            &mut filtered,
            &mut selected,
        );

        Self {
            input,
            previous_mode,
            scope,
            git_root,
            all_paths,
            modified_paths,
            filtered,
            selected,
            last_filter_text: String::new(),
            last_filter_scope: scope,
            preview_editor,
            preview_buffer,
            preview_rendered_for: None,
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
        }
    }

    /// Absolute path of the currently selected filtered row, if any.
    pub(crate) fn selected_path(&self) -> Option<&Path> {
        let base = self.base_paths();
        self.filtered
            .get(self.selected)
            .and_then(|i| base.get(*i))
            .map(|p| p.as_path())
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.filtered.len() - 1) as i32;
        let next = (self.selected as i32 + delta).clamp(0, max);
        self.selected = next as usize;
    }

    /// Flip the scope, optionally refreshing the Modified list before
    /// rerunning the filter against the new base. `git_host` is borrowed
    /// only when switching *to* Modified so the caller can skip the
    /// discover call when switching away.
    pub(crate) fn toggle_scope(&mut self, git_host: &dyn GitHost) {
        self.scope = match self.scope {
            FinderScope::All => {
                self.modified_paths = query_modified(git_host, &self.git_root);
                FinderScope::Modified
            },
            FinderScope::Modified => FinderScope::All,
        };
        self.selected = 0;
        self.last_filter_text = String::new();
        // Force refilter + preview resync on next render.
        self.filtered.clear();
    }

    /// Re-run the matcher if the input text or scope has changed since last
    /// filter. Called from the renderer so typing picks up without a dedicated
    /// sync hook.
    pub(crate) fn refilter_from_input(&mut self, ws: &Workspace) {
        let text = self.input.text(ws);
        if text == self.last_filter_text
            && self.scope == self.last_filter_scope
            && !self.filtered.is_empty()
        {
            return;
        }
        refilter(
            &text,
            self.scope,
            &self.all_paths,
            &self.modified_paths,
            &self.git_root,
            &mut self.filtered,
            &mut self.selected,
        );
        self.last_filter_text = text;
        self.last_filter_scope = self.scope;
    }

    /// Copy the currently-selected file into the preview scratch buffer.
    /// No-op when the selection already matches [`Self::preview_rendered_for`].
    /// Reads through `fs_host`; errors are swallowed and surfaced as a
    /// placeholder so the preview always renders something.
    pub(crate) fn sync_preview(&mut self, ws: &mut Workspace, fs_host: &dyn FsHost) {
        let path = match self.selected_path() {
            Some(p) => p.to_path_buf(),
            None => {
                if self.preview_rendered_for.is_some() {
                    replace_preview_text(ws, self.preview_editor, self.preview_buffer, "");
                    self.preview_rendered_for = None;
                }
                return;
            },
        };
        if self.preview_rendered_for.as_deref() == Some(path.as_path()) {
            return;
        }
        let content = read_preview(fs_host, &path);
        replace_preview_text(ws, self.preview_editor, self.preview_buffer, &content);
        self.preview_rendered_for = Some(path);
    }

    /// Tear down owned editor slots. Called on every finder-close path.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
        ws.editors.remove(self.preview_editor);
    }
}

/// Read `path` through `fs_host`, truncating at [`PREVIEW_BYTE_LIMIT`] on a
/// UTF-8 char boundary. Returns a placeholder for read errors or non-UTF-8
/// content so the preview pane always renders.
fn read_preview(fs_host: &dyn FsHost, path: &Path) -> String {
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

/// Overwrite the preview scratch buffer with `text` and reset the editor's
/// viewport to the top.
fn replace_preview_text(ws: &mut Workspace, editor_id: EditorId, buffer_id: BufferId, text: &str) {
    let Some(buffer) = ws.buffers.get(buffer_id) else {
        return;
    };
    let old_len = {
        let guard = buffer.read().expect("preview buffer poisoned");
        guard.snapshot.visible_text.len()
    };
    {
        let mut guard = buffer.write().expect("preview buffer poisoned");
        guard.edit(0..old_len, text);
    }
    if let Some(editor) = ws.editors.get_mut(editor_id) {
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let anchor = buf_snap.anchor_at(0, Bias::Left);
        editor.selections.transform(buf_snap, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        });
        editor.scroll_row = 0;
    }
}

/// Enumerate every non-ignored file under `git_root`. Respects `.gitignore`,
/// `.git/info/exclude`, the global gitignore, a per-repo `.stoatignore`,
/// and the baked-in [`DEFAULT_STOATIGNORE`] patterns. Does not require
/// `git_root` to be a git repo.
pub(crate) fn walk_workspace_files(git_root: &Path) -> Vec<PathBuf> {
    let defaults = build_default_ignore(git_root);

    let mut out = Vec::new();
    let walker = WalkBuilder::new(git_root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .require_git(false)
        .add_custom_ignore_filename(".stoatignore")
        .filter_entry(move |dent| {
            let is_dir = dent.file_type().is_some_and(|ft| ft.is_dir());
            !defaults.matched(dent.path(), is_dir).is_ignore()
        })
        .build();
    for entry in walker.flatten() {
        let ft = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if !ft.is_file() {
            continue;
        }
        out.push(entry.path().to_path_buf());
    }
    out.sort();
    out
}

fn build_default_ignore(git_root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(git_root);
    for line in DEFAULT_STOATIGNORE.lines() {
        builder
            .add_line(None, line)
            .expect("default .stoatignore parses");
    }
    builder.build().expect("default .stoatignore builds")
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

/// Core matcher. Splits candidates into three tiers -- prefix / substring /
/// fuzzy -- matched against the repo-relative display form. Each tier is
/// lexicographically sorted. Mirrors the palette's three-tier layout so
/// users feel the same ranking behavior across modals.
pub(crate) fn refilter(
    text: &str,
    scope: FinderScope,
    all_paths: &[PathBuf],
    modified_paths: &[PathBuf],
    git_root: &Path,
    filtered: &mut Vec<usize>,
    selected: &mut usize,
) {
    let base: &[PathBuf] = match scope {
        FinderScope::All => all_paths,
        FinderScope::Modified => modified_paths,
    };

    let needle = text.to_lowercase();
    let mut prefix: Vec<(usize, String)> = Vec::new();
    let mut substring: Vec<(usize, String)> = Vec::new();
    let mut fuzzy: Vec<(usize, String)> = Vec::new();

    let fuzzy_atom = (!needle.is_empty()).then(|| {
        Atom::new(
            text,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
            false,
        )
    });
    let mut hay_buf: Vec<char> = Vec::new();
    let mut matcher_guard = fuzzy_atom
        .as_ref()
        .map(|_| fuzzy_matcher().lock().expect("fuzzy matcher poisoned"));

    for (idx, path) in base.iter().enumerate() {
        let display = display_for(path, git_root);
        if needle.is_empty() {
            prefix.push((idx, display));
            continue;
        }
        let display_lc = display.to_lowercase();
        if display_lc.starts_with(&needle) {
            prefix.push((idx, display));
        } else if display_lc.contains(&needle) {
            substring.push((idx, display));
        } else if let (Some(atom), Some(matcher)) = (&fuzzy_atom, matcher_guard.as_deref_mut()) {
            let hay = Utf32Str::new(&display, &mut hay_buf);
            if atom.score(hay, matcher).is_some() {
                fuzzy.push((idx, display));
            }
        }
    }

    prefix.sort_by(|a, b| a.1.cmp(&b.1));
    substring.sort_by(|a, b| a.1.cmp(&b.1));
    fuzzy.sort_by(|a, b| a.1.cmp(&b.1));

    filtered.clear();
    filtered.extend(prefix.into_iter().map(|(i, _)| i));
    filtered.extend(substring.into_iter().map(|(i, _)| i));
    filtered.extend(fuzzy.into_iter().map(|(i, _)| i));

    if filtered.is_empty() {
        *selected = 0;
    } else if *selected >= filtered.len() {
        *selected = filtered.len() - 1;
    }
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

    fn names(
        text: &str,
        scope: FinderScope,
        all: &[PathBuf],
        modified: &[PathBuf],
        git_root: &Path,
    ) -> Vec<String> {
        let mut filtered = Vec::new();
        let mut selected = 0;
        refilter(
            text,
            scope,
            all,
            modified,
            git_root,
            &mut filtered,
            &mut selected,
        );
        let base: &[PathBuf] = match scope {
            FinderScope::All => all,
            FinderScope::Modified => modified,
        };
        filtered
            .iter()
            .map(|i| display_for(&base[*i], git_root))
            .collect()
    }

    #[test]
    fn empty_input_lists_all_base_paths_sorted() {
        let git_root = p("/r");
        let all = vec![p("/r/b.rs"), p("/r/a.rs"), p("/r/sub/c.rs")];
        let modified = vec![];
        let listed = names("", FinderScope::All, &all, &modified, &git_root);
        assert_eq!(listed, vec!["a.rs", "b.rs", "sub/c.rs"]);
    }

    #[test]
    fn prefix_ranks_before_substring_before_fuzzy() {
        let git_root = p("/r");
        let all = vec![
            p("/r/file.rs"),      // prefix
            p("/r/sub/file.rs"),  // substring
            p("/r/fee/nile.rs"),  // fuzzy (f..i..l..e)
            p("/r/unrelated.rs"), // filtered out
        ];
        let listed = names("file", FinderScope::All, &all, &[], &git_root);
        assert_eq!(listed, vec!["file.rs", "sub/file.rs", "fee/nile.rs"]);
    }

    #[test]
    fn case_insensitive_filter() {
        let git_root = p("/r");
        let all = vec![p("/r/Foo.rs"), p("/r/bar.rs")];
        let listed = names("foo", FinderScope::All, &all, &[], &git_root);
        assert_eq!(listed, vec!["Foo.rs"]);
    }

    #[test]
    fn modified_scope_filters_against_modified_list() {
        let git_root = p("/r");
        let all = vec![p("/r/a.rs"), p("/r/b.rs"), p("/r/c.rs")];
        let modified = vec![p("/r/b.rs")];
        let listed = names("", FinderScope::Modified, &all, &modified, &git_root);
        assert_eq!(listed, vec!["b.rs"]);
    }

    #[test]
    fn modified_scope_empty_when_no_modifications() {
        let git_root = p("/r");
        let all = vec![p("/r/a.rs"), p("/r/b.rs")];
        let modified = vec![];
        let listed = names("", FinderScope::Modified, &all, &modified, &git_root);
        assert!(listed.is_empty());
    }

    #[test]
    fn refilter_clamps_selected_when_results_shrink() {
        let git_root = p("/r");
        let all = vec![p("/r/a.rs"), p("/r/b.rs"), p("/r/c.rs")];
        let mut filtered = Vec::new();
        let mut selected = 2;
        refilter(
            "b",
            FinderScope::All,
            &all,
            &[],
            &git_root,
            &mut filtered,
            &mut selected,
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(selected, 0);
    }

    #[test]
    fn display_row_strips_git_root_prefix() {
        let git_root = p("/work/stoat");
        assert_eq!(
            display_row(&p("/work/stoat/src/main.rs"), &git_root),
            "src/main.rs"
        );
    }

    #[test]
    fn walk_workspace_files_returns_files_not_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("a.rs"), "a").unwrap();
        std::fs::write(tmp.path().join("sub/b.rs"), "b").unwrap();
        let files = walk_workspace_files(tmp.path());
        // All returned entries must be regular files.
        assert!(files.iter().all(|p| p.is_file()));
        assert!(files.iter().any(|p| p.ends_with("a.rs")));
        assert!(files.iter().any(|p| p.ends_with("sub/b.rs")));
    }

    fn seed_fs(base: &Path, files: &[(&str, &str)]) {
        for (rel, content) in files {
            let abs = base.join(rel);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&abs, content).unwrap();
        }
    }

    fn walked_rels(base: &Path) -> Vec<String> {
        let mut rels: Vec<String> = walk_workspace_files(base)
            .iter()
            .map(|p| p.strip_prefix(base).unwrap().to_string_lossy().into_owned())
            .collect();
        rels.sort();
        rels
    }

    #[test]
    fn walk_workspace_files_ignores_dot_git() {
        let tmp = tempfile::tempdir().unwrap();
        seed_fs(
            tmp.path(),
            &[
                (".git/HEAD", "ref: refs/heads/main"),
                (".git/config", "[core]"),
                (".git/refs/heads/main", "deadbeef"),
                ("src/main.rs", "fn main() {}"),
            ],
        );
        assert_eq!(walked_rels(tmp.path()), vec!["src/main.rs"]);
    }

    #[test]
    fn walk_workspace_files_ignores_baked_in_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        seed_fs(
            tmp.path(),
            &[
                ("target/debug/foo", "bin"),
                ("node_modules/pkg/index.js", "module.exports = {}"),
                ("src/main.rs", "fn main() {}"),
            ],
        );
        assert_eq!(walked_rels(tmp.path()), vec!["src/main.rs"]);
    }

    #[test]
    fn walk_workspace_files_honors_stoatignore() {
        let tmp = tempfile::tempdir().unwrap();
        seed_fs(
            tmp.path(),
            &[
                (".stoatignore", "vendor/\n"),
                ("vendor/blob.rs", "// generated"),
                ("src/main.rs", "fn main() {}"),
            ],
        );
        assert_eq!(
            walked_rels(tmp.path()),
            vec![".stoatignore".to_string(), "src/main.rs".to_string()],
        );
    }

    #[test]
    fn walk_workspace_files_still_walks_non_git_dotfiles() {
        // Synthetic dotfile names so the test doesn't depend on the user's
        // global gitignore (which on real machines often excludes `.claude`,
        // `.vscode`, etc.). Confirms that disabling `hidden(false)` keeps
        // ordinary dotfiles visible -- only the baked-in defaults filter
        // them out.
        let tmp = tempfile::tempdir().unwrap();
        seed_fs(
            tmp.path(),
            &[
                (".stoat_test_alpha/data", "{}"),
                (".stoat_test_betarc", ""),
                ("src/main.rs", "fn main() {}"),
            ],
        );
        assert_eq!(
            walked_rels(tmp.path()),
            vec![
                ".stoat_test_alpha/data".to_string(),
                ".stoat_test_betarc".to_string(),
                "src/main.rs".to_string(),
            ],
        );
    }

    // ----- TestHarness integration tests -----

    use crate::test_harness::TestHarness;

    /// Seed a tempdir with `files`, mirror them into the harness' FakeFs so
    /// preview reads find the same content, and point the active workspace
    /// at the tempdir. The returned [`tempfile::TempDir`] must stay alive
    /// for the duration of the test; drop on scope exit cleans up.
    fn seed_finder_workspace(h: &mut TestHarness, files: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        for (rel, content) in files {
            let abs = tmp.path().join(rel);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::write(&abs, content).expect("write real file");
            h.fake_fs().insert_file(&abs, content.as_bytes());
        }
        h.stoat.active_workspace_mut().git_root = tmp.path().to_path_buf();
        tmp
    }

    #[test]
    fn space_p_opens_finder_and_switches_to_prompt_mode() {
        let mut h = crate::Stoat::test();
        let _tmp = seed_finder_workspace(&mut h, &[("a.rs", "fn a() {}")]);
        h.type_keys("space p");
        assert!(h.stoat.file_finder.is_some(), "finder not opened");
        assert_eq!(h.snapshot().mode, "prompt");
    }

    #[test]
    fn escape_closes_finder_and_restores_mode() {
        let mut h = crate::Stoat::test();
        let _tmp = seed_finder_workspace(&mut h, &[("a.rs", "")]);
        h.type_keys("space p");
        h.type_keys("escape");
        assert!(h.stoat.file_finder.is_none(), "finder still open");
        assert_eq!(h.snapshot().mode, "normal");
    }

    #[test]
    fn ctrl_c_closes_finder() {
        let mut h = crate::Stoat::test();
        let _tmp = seed_finder_workspace(&mut h, &[("a.rs", "")]);
        h.type_keys("space p");
        h.type_keys("Ctrl-c");
        assert!(h.stoat.file_finder.is_none());
        assert_eq!(h.snapshot().mode, "normal");
    }

    #[test]
    fn second_open_is_noop() {
        let mut h = crate::Stoat::test();
        let _tmp = seed_finder_workspace(&mut h, &[("a.rs", "")]);
        h.type_keys("space p");
        let ptr_before = h.stoat.file_finder.as_ref().unwrap() as *const FileFinder;
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        let ptr_after = h.stoat.file_finder.as_ref().unwrap() as *const FileFinder;
        assert_eq!(ptr_before, ptr_after, "re-open should not replace state");
    }

    #[test]
    fn enter_dispatches_open_file_for_selected_path() {
        let mut h = crate::Stoat::test();
        let _tmp = seed_finder_workspace(&mut h, &[("target.txt", "loaded via finder")]);
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
    fn backtab_toggles_scope_to_modified() {
        use std::path::PathBuf;
        let mut h = crate::Stoat::test();
        let tmp = seed_finder_workspace(
            &mut h,
            &[("a.rs", "v1\n"), ("b.rs", "v1\n"), ("c.rs", "v1\n")],
        );
        // Seed the fake git repo so only b.rs is reported as modified.
        {
            let mut builder = h.fake_git().add_repo(tmp.path()).with_fs(h.fake_fs());
            builder.head_file("a.rs", "v1\n");
            builder.modified("b.rs", "v1\n", "v2\n");
            builder.head_file("c.rs", "v1\n");
        }
        // Also mirror the "modified" content into the real fs so the
        // walker's picture matches the git modification state.
        std::fs::write(tmp.path().join("b.rs"), "v2\n").unwrap();

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
    fn typing_narrows_filtered_list() {
        let mut h = crate::Stoat::test();
        let _tmp = seed_finder_workspace(
            &mut h,
            &[("alpha.rs", ""), ("beta.rs", ""), ("gamma.rs", "")],
        );
        h.type_keys("space p");
        // refilter is driven by the render loop; force a snapshot so
        // filtered reflects the current (empty) query.
        let _ = h.snapshot();
        assert_eq!(h.stoat.file_finder.as_ref().unwrap().filtered.len(), 3);
        h.type_text("alp");
        let _ = h.snapshot();
        let finder = h.stoat.file_finder.as_ref().unwrap();
        assert_eq!(finder.filtered.len(), 1);
        let idx = finder.filtered[0];
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
}
