use crate::{
    buffer::BufferId,
    editor_state::{EditorId, EditorState},
    host::{FsHost, GitHost},
    input_view::{InputView, SubmitTarget},
    paths,
    workspace::Workspace,
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
    /// Absolute paths of every tracked file, captured once at open time.
    pub(crate) all_paths: Vec<PathBuf>,
    /// Absolute paths of currently-modified files. Re-queried on scope toggle.
    pub(crate) modified_paths: Vec<PathBuf>,
    /// Absolute paths of currently-open buffers. Captured once at open time;
    /// not re-queried on scope toggle.
    pub(crate) buffer_paths: Vec<PathBuf>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ws: &mut Workspace,
        executor: Executor,
        previous_mode: String,
        open_intent: OpenIntent,
        initial_scope: FinderScope,
        git_root: PathBuf,
        all_paths: Vec<PathBuf>,
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
        let (preview_buffer, shared_buffer) = ws.buffers.new_scratch();
        let preview_editor_state = EditorState::new(preview_buffer, shared_buffer, executor);
        let preview_editor = ws.editors.insert(preview_editor_state);

        let scope = initial_scope;
        let mut filtered = Vec::new();
        let mut selected = 0;
        refilter(
            "",
            scope,
            &all_paths,
            &modified_paths,
            &buffer_paths,
            &git_root,
            &mut filtered,
            &mut selected,
        );

        Self {
            input,
            previous_mode,
            open_intent,
            scope,
            git_root,
            all_paths,
            modified_paths,
            buffer_paths,
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
            FinderScope::Buffers => &self.buffer_paths,
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
            &self.buffer_paths,
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
/// content so the preview pane always renders. Output is run through
/// [`sanitize_preview_text`] so unsanitized bytes never reach the rope.
fn read_preview(fs_host: &dyn FsHost, path: &Path) -> String {
    let mut buf = Vec::new();
    if fs_host.read(path, &mut buf).is_err() {
        return "<unreadable>".to_string();
    }
    let limit = PREVIEW_BYTE_LIMIT.min(buf.len());
    let raw = match std::str::from_utf8(&buf[..limit]) {
        Ok(s) => s.to_string(),
        Err(err) => {
            let valid = err.valid_up_to();
            String::from_utf8_lossy(&buf[..valid]).into_owned()
        },
    };
    sanitize_preview_text(&raw)
}

/// Replace C0 control characters (other than `\n` and `\t`) and DEL with
/// `·`. The preview pane writes its content into a ratatui buffer one cell
/// per char; an unfiltered ESC starts a real CSI sequence in the host
/// terminal, BEL beeps, CR jumps the cursor to column 0. The editor's
/// display map expands `\t` and the renderer treats `\n` as a row break,
/// so those two characters pass through unchanged.
fn sanitize_preview_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\n' | '\t' => out.push(ch),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push('·'),
            _ => out.push(ch),
        }
    }
    out
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn refilter(
    text: &str,
    scope: FinderScope,
    all_paths: &[PathBuf],
    modified_paths: &[PathBuf],
    buffer_paths: &[PathBuf],
    git_root: &Path,
    filtered: &mut Vec<usize>,
    selected: &mut usize,
) {
    let base: &[PathBuf] = match scope {
        FinderScope::All => all_paths,
        FinderScope::Modified => modified_paths,
        FinderScope::Buffers => buffer_paths,
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
        buffers: &[PathBuf],
        git_root: &Path,
    ) -> Vec<String> {
        let mut filtered = Vec::new();
        let mut selected = 0;
        refilter(
            text,
            scope,
            all,
            modified,
            buffers,
            git_root,
            &mut filtered,
            &mut selected,
        );
        let base: &[PathBuf] = match scope {
            FinderScope::All => all,
            FinderScope::Modified => modified,
            FinderScope::Buffers => buffers,
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
        let buffers = vec![];
        let listed = names("", FinderScope::All, &all, &modified, &buffers, &git_root);
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
        let listed = names("file", FinderScope::All, &all, &[], &[], &git_root);
        assert_eq!(listed, vec!["file.rs", "sub/file.rs", "fee/nile.rs"]);
    }

    #[test]
    fn case_insensitive_filter() {
        let git_root = p("/r");
        let all = vec![p("/r/Foo.rs"), p("/r/bar.rs")];
        let listed = names("foo", FinderScope::All, &all, &[], &[], &git_root);
        assert_eq!(listed, vec!["Foo.rs"]);
    }

    #[test]
    fn modified_scope_filters_against_modified_list() {
        let git_root = p("/r");
        let all = vec![p("/r/a.rs"), p("/r/b.rs"), p("/r/c.rs")];
        let modified = vec![p("/r/b.rs")];
        let listed = names("", FinderScope::Modified, &all, &modified, &[], &git_root);
        assert_eq!(listed, vec!["b.rs"]);
    }

    #[test]
    fn modified_scope_empty_when_no_modifications() {
        let git_root = p("/r");
        let all = vec![p("/r/a.rs"), p("/r/b.rs")];
        let modified = vec![];
        let listed = names("", FinderScope::Modified, &all, &modified, &[], &git_root);
        assert!(listed.is_empty());
    }

    #[test]
    fn buffers_scope_filters_against_buffer_list() {
        let git_root = p("/r");
        let all = vec![p("/r/a.rs"), p("/r/b.rs"), p("/r/c.rs")];
        let buffers = vec![p("/r/a.rs"), p("/r/c.rs")];
        let listed = names("", FinderScope::Buffers, &all, &[], &buffers, &git_root);
        assert_eq!(listed, vec!["a.rs", "c.rs"]);
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
    fn sanitize_preview_text_passes_plain_ascii_through() {
        assert_eq!(sanitize_preview_text("hello world"), "hello world");
    }

    #[test]
    fn sanitize_preview_text_keeps_newline_and_tab() {
        assert_eq!(sanitize_preview_text("a\nb\tc"), "a\nb\tc");
    }

    #[test]
    fn sanitize_preview_text_redacts_esc_csi_sequence() {
        assert_eq!(sanitize_preview_text("\x1b[31mhi\x1b[0m"), "·[31mhi·[0m",);
    }

    #[test]
    fn sanitize_preview_text_redacts_cr_bel_nul_del() {
        assert_eq!(sanitize_preview_text("\r\x07\x00\x7f"), "····");
    }

    #[test]
    fn sanitize_preview_text_passes_multibyte_utf8_through() {
        assert_eq!(sanitize_preview_text("café naïve"), "café naïve");
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
