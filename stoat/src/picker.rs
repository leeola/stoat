use crate::{
    buffer::BufferId,
    editor_state::{EditorId, EditorState},
    fuzzy,
    host::FsHost,
    paths,
    render::sanitize,
    workspace::Workspace,
};
use std::path::{Path, PathBuf};
use stoat_language::LanguageRegistry;
use stoat_scheduler::{Executor, Task};
use stoat_text::{Bias, SelectionGoal};
use tokio::sync::mpsc::{error::TryRecvError, UnboundedReceiver};

/// Preview content cap. Keeps preview reads bounded so a stray large or binary
/// file never stalls the render thread.
pub(crate) const PREVIEW_BYTE_LIMIT: usize = 128 * 1024;

/// Query-driven fuzzy result list over a fixed `base` set of paths, decoupled
/// from any input widget.
///
/// The file finder and the palette's inline pickers drive the same list from a
/// query string. The owner sets `base`, calls [`PickList::refilter`] with the
/// query, and reads `filtered`/`match_indices`/`selected` to render.
#[derive(Default)]
pub(crate) struct PickList {
    /// Candidate paths the query filters over.
    pub(crate) base: Vec<PathBuf>,
    /// Indices into `base`, after filtering, in display order.
    pub(crate) filtered: Vec<usize>,
    /// Per-row matched character offsets into the row's display string,
    /// parallel to `filtered`. A row is empty when no pattern is active. The
    /// offsets are sorted and deduplicated so the renderer can `contains`-test
    /// without further work.
    pub(crate) match_indices: Vec<Vec<u32>>,
    pub(crate) selected: usize,
    /// Rendered list height in rows, refreshed each frame by the owner's render
    /// so [`PickList::page`] can size its half-page step. `None` before the
    /// first render, where the step falls back to a single row.
    pub(crate) viewport_rows: Option<usize>,
}

impl PickList {
    /// Absolute path of the currently selected filtered row, if any.
    pub(crate) fn selected_path(&self) -> Option<&Path> {
        self.filtered
            .get(self.selected)
            .and_then(|i| self.base.get(*i))
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

    /// Page the selection by half the rendered list height in `dir` (negative
    /// up, positive down). Before the first render sets [`Self::viewport_rows`]
    /// the step falls back to a single row.
    pub(crate) fn page(&mut self, dir: i32) {
        let step = self
            .viewport_rows
            .map(|v| v.div_ceil(2).max(1))
            .unwrap_or(1) as i32;
        self.move_selection(dir * step);
    }

    /// Re-run the matcher over `base` for `query` via
    /// [`crate::fuzzy::match_and_rank`], ordering matches by score descending,
    /// ties alphabetical. Empty or whitespace-only input lists every candidate
    /// alphabetically.
    ///
    /// A leading `./` token anchors to the workspace root. Candidates are first
    /// restricted to those whose display path starts with the token's prefix,
    /// and the rest of the query fuzzy-matches within them. The anchored prefix
    /// is highlighted on every surviving row. See [`split_root_anchor`].
    ///
    /// `match_indices` is rebuilt in parallel to `filtered`. Each element is the
    /// sorted, deduplicated set of matched character offsets in that row's
    /// display string, or empty when no pattern or anchor is active.
    pub(crate) fn refilter(&mut self, query: &str, git_root: &Path) {
        self.filtered.clear();
        self.match_indices.clear();

        let (anchor, pattern) = split_root_anchor(query);
        let anchor_len = anchor.map_or(0, |a| a.chars().count()) as u32;

        let items = self
            .base
            .iter()
            .enumerate()
            .map(|(idx, path)| (idx, paths::display_relative(path, git_root)))
            .filter(|(_, display)| anchor.is_none_or(|a| display.starts_with(a)));
        let Some(mut matches) = fuzzy::match_and_rank(pattern, items) else {
            let mut rows: Vec<(usize, String)> = self
                .base
                .iter()
                .enumerate()
                .map(|(idx, path)| (idx, paths::display_relative(path, git_root)))
                .filter(|(_, display)| anchor.is_none_or(|a| display.starts_with(a)))
                .collect();
            rows.sort_by(|a, b| a.1.cmp(&b.1));
            for (idx, _) in &rows {
                self.filtered.push(*idx);
                self.match_indices.push((0..anchor_len).collect());
            }
            self.clamp_selected();
            return;
        };

        matches.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.haystack.cmp(&b.haystack))
        });
        for m in matches {
            self.filtered.push(m.item);
            self.match_indices
                .push(prepend_anchor(anchor_len, m.matched_indices));
        }
        self.clamp_selected();
    }

    fn clamp_selected(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len() - 1;
        }
    }
}

/// Split a leading `./` workspace-root anchor off `query`.
///
/// When the first whitespace-delimited token starts with `./`, returns that
/// token minus the `./` as a root-relative path prefix, plus the rest of the
/// query as the fuzzy pattern. A bare `./` yields an empty prefix, which every
/// path matches, so the finder lists all. A `./` in any but the first token is
/// ordinary fuzzy text and yields `(None, query)`.
fn split_root_anchor(query: &str) -> (Option<&str>, &str) {
    let after_ws = query.trim_start();
    let leading = query.len() - after_ws.len();
    let first_len = after_ws.find(char::is_whitespace).unwrap_or(after_ws.len());
    let Some(anchor) = after_ws[..first_len].strip_prefix("./") else {
        return (None, query);
    };
    (Some(anchor), query[leading + first_len..].trim_start())
}

/// Merge the `0..anchor_len` anchored-prefix character offsets into `matched`,
/// returning the sorted, deduplicated union so the renderer highlights the
/// pinned prefix alongside the fuzzy matches.
fn prepend_anchor(anchor_len: u32, matched: Vec<u32>) -> Vec<u32> {
    if anchor_len == 0 {
        return matched;
    }
    let mut indices: Vec<u32> = (0..anchor_len).chain(matched).collect();
    indices.sort_unstable();
    indices.dedup();
    indices
}

/// How a [`PathPicker`] previews its current selection.
pub(crate) enum PreviewPolicy {
    /// Read the selected path from disk.
    File,
    /// Preview the live in-memory buffer when the path has one open, else the
    /// disk file. The finder's Buffers scope and the palette's buffer picker.
    LiveBufferThenFile,
    /// No preview -- e.g. a directory picker, which has nothing to show.
    NoPreview,
}

/// A walk-fed path list, its fuzzy [`PickList`], and a [`Preview`] pane.
///
/// Drives both the file finder and the palette's inline argument picker, so a
/// fix to walk draining, the refilter text-cache, or preview syncing reaches
/// both instead of only the copy it was written against.
pub(crate) struct PathPicker {
    pub(crate) git_root: PathBuf,
    /// Every candidate path. Grows as walk batches arrive via
    /// [`PathPicker::pump_walk`] for a walked source. A caller-fed source leaves
    /// it empty and drives [`PathPicker::refilter_with_base`] instead.
    pub(crate) all_paths: Vec<PathBuf>,
    walk_rx: Option<UnboundedReceiver<Vec<PathBuf>>>,
    _walk_task: Option<Task<()>>,
    pub(crate) picklist: PickList,
    /// Last query run through the matcher, so a render tick with no typing
    /// short-circuits. Cleared by [`PathPicker::invalidate`] when the base set
    /// changes under a stable query.
    pub(crate) last_filter_text: String,
    pub(crate) preview: Preview,
}

impl PathPicker {
    /// Create a picker over `git_root`. `walk` is the streaming walker for a
    /// file source, or `None` for a caller-fed fixed set.
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        git_root: PathBuf,
        walk: Option<(UnboundedReceiver<Vec<PathBuf>>, Task<()>)>,
    ) -> Self {
        let (walk_rx, walk_task) = match walk {
            Some((rx, task)) => (Some(rx), Some(task)),
            None => (None, None),
        };
        let preview = Preview::new(ws, executor);
        Self {
            git_root,
            all_paths: Vec::new(),
            walk_rx,
            _walk_task: walk_task,
            picklist: PickList::default(),
            last_filter_text: String::new(),
            preview,
        }
    }

    pub(crate) fn selected_path(&self) -> Option<&Path> {
        self.picklist.selected_path()
    }

    pub(crate) fn move_selection(&mut self, delta: i32) {
        self.picklist.move_selection(delta);
    }

    pub(crate) fn page(&mut self, dir: i32) {
        self.picklist.page(dir);
    }

    /// Drain every walk batch since the last call into [`Self::all_paths`],
    /// invalidating the filter cache when any arrived. Returns whether a batch
    /// was consumed. No-op for a caller-fed source.
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
            self.invalidate();
        }
        received_any
    }

    /// Force the next refilter to re-run the matcher, even under an unchanged
    /// query. Callers whose base set changed (a walk batch, a scope flip) call
    /// this so the stale filtered rows do not survive.
    pub(crate) fn invalidate(&mut self) {
        self.last_filter_text.clear();
        self.picklist.filtered.clear();
        self.picklist.match_indices.clear();
    }

    /// Refilter over this picker's own walk-fed [`Self::all_paths`], skipping
    /// the work when the query is unchanged and rows are present.
    pub(crate) fn refilter(&mut self, query: &str) {
        if query == self.last_filter_text && !self.picklist.filtered.is_empty() {
            return;
        }
        let base = self.all_paths.clone();
        self.refilter_with_base(query, &base);
    }

    /// Refilter over a caller-owned `base` set. The query cache still applies,
    /// so a caller that changes `base` under a stable query must
    /// [`Self::invalidate`] first (the finder does this on a scope flip).
    pub(crate) fn refilter_with_base(&mut self, query: &str, base: &[PathBuf]) {
        if query == self.last_filter_text && !self.picklist.filtered.is_empty() {
            return;
        }
        self.picklist.base = base.to_vec();
        self.picklist.refilter(query, &self.git_root);
        self.last_filter_text = query.to_string();
    }

    /// Sync the preview pane to the current selection per `policy`, clearing it
    /// when nothing is selected.
    pub(crate) fn sync_preview(
        &mut self,
        ws: &mut Workspace,
        fs_host: &dyn FsHost,
        language_registry: &LanguageRegistry,
        policy: PreviewPolicy,
    ) {
        let Some(path) = self.selected_path().map(|p| p.to_path_buf()) else {
            self.preview.clear(ws);
            return;
        };
        let source = match policy {
            PreviewPolicy::File => Some(PreviewSource::File(path)),
            PreviewPolicy::LiveBufferThenFile => Some(match ws.buffers.id_for_path(&path) {
                Some(id) => PreviewSource::Buffer(id),
                None => PreviewSource::File(path),
            }),
            PreviewPolicy::NoPreview => None,
        };
        match source {
            Some(source) => self.preview.sync(ws, fs_host, language_registry, source),
            None => self.preview.clear(ws),
        }
    }

    /// Tear down the preview's owned editor slot. Callers dispose their own
    /// input widget separately.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.preview.dispose(ws);
    }
}

/// Where a [`Preview`] pulls its content from.
///
/// `File` reads the path from disk. `Buffer` reads a live, possibly modified
/// in-memory buffer, so the preview reflects unsaved edits rather than the
/// backing file.
#[derive(PartialEq)]
pub(crate) enum PreviewSource {
    File(PathBuf),
    /// Live in-memory buffer, so the preview reflects unsaved edits rather than
    /// the backing file. Used by the finder's Buffers scope and the palette's
    /// buffer argument picker.
    Buffer(BufferId),
}

/// Read-only preview pane backed by a reusable scratch buffer.
///
/// A picker drives this by calling [`Preview::sync`] with the selected source.
/// The scratch buffer's rope is replaced with that source's content and the
/// source's language is assigned so the parse pipeline highlights it.
pub(crate) struct Preview {
    pub(crate) editor: EditorId,
    pub(crate) buffer: BufferId,
    /// Source currently rendered into the scratch buffer, or `None` when empty.
    /// Lets [`Preview::sync`] skip a redundant reload when the selection is
    /// unchanged.
    rendered_for: Option<PreviewSource>,
}

impl Preview {
    /// Allocate the scratch preview buffer and its editor.
    pub(crate) fn new(ws: &mut Workspace, executor: Executor) -> Self {
        let (buffer, shared_buffer) = ws.buffers.new_scratch_preview();
        let editor_state = EditorState::new(buffer, shared_buffer, executor);
        let editor = ws.editors.insert(editor_state);
        Self {
            editor,
            buffer,
            rendered_for: None,
        }
    }

    /// Load `source` into the scratch buffer, unless it is already shown.
    ///
    /// `File` reads disk through `fs_host` and resolves the language via
    /// `language_registry`. `Buffer` reads the live in-memory text and copies
    /// the source buffer's own language, ignoring both arguments. Stale syntax
    /// state is reset on every swap so an in-flight parse of the previous
    /// source cannot paint onto the new one. Read errors render a placeholder
    /// so the pane always shows something.
    pub(crate) fn sync(
        &mut self,
        ws: &mut Workspace,
        fs_host: &dyn FsHost,
        language_registry: &LanguageRegistry,
        source: PreviewSource,
    ) {
        if self.rendered_for.as_ref() == Some(&source) {
            return;
        }
        let (content, language) = match &source {
            PreviewSource::File(path) => (
                read_preview(fs_host, path),
                language_registry.for_path(path),
            ),
            PreviewSource::Buffer(id) => {
                let content = ws
                    .buffers
                    .get(*id)
                    .map(|b| {
                        b.read()
                            .expect("preview source buffer poisoned")
                            .rope()
                            .to_string()
                    })
                    .unwrap_or_default();
                (content, ws.buffers.language_for(*id))
            },
        };
        replace_preview_text(ws, self.editor, self.buffer, &content);
        ws.reset_preview_syntax(self.buffer);
        if let Some(language) = language {
            ws.buffers.set_language(self.buffer, language);
        }
        self.rendered_for = Some(source);
    }

    /// Blank the preview when nothing is selected. No-op when already empty.
    pub(crate) fn clear(&mut self, ws: &mut Workspace) {
        if self.rendered_for.is_some() {
            replace_preview_text(ws, self.editor, self.buffer, "");
            ws.reset_preview_syntax(self.buffer);
            self.rendered_for = None;
        }
    }

    /// Remove the owned editor and scratch buffer from the workspace.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        ws.editors.remove(self.editor);
        ws.buffers.remove(self.buffer);
    }
}

/// Read `path` through `fs_host`, truncating at [`PREVIEW_BYTE_LIMIT`] on a
/// UTF-8 char boundary. Returns a placeholder for read errors or non-UTF-8
/// content so the preview pane always renders. Output is run through
/// [`sanitize::sanitize_preview_text`] so unsanitized bytes never reach the
/// rope.
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
    sanitize::sanitize_preview_text(&raw)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn live_buffer_then_file_previews_disk_when_no_buffer() {
        let mut h = crate::Stoat::test();
        let executor = h.stoat.executor.clone();
        let language_registry = h.stoat.language_registry.clone();
        let fs = crate::host::FakeFs::new();
        fs.insert_files(std::iter::once((
            p("/repo/on_disk.txt"),
            b"disk content\n".as_slice(),
        )));

        let ws = h.stoat.active_workspace_mut();
        let mut picker = PathPicker::new(ws, executor, p("/repo"), None);
        picker.all_paths = vec![p("/repo/on_disk.txt")];
        picker.refilter("");

        // The selected path has no open buffer, so the unified LiveBufferThenFile
        // policy -- the palette's buffer picker among them -- falls back to disk
        // rather than clearing the pane.
        picker.sync_preview(
            ws,
            &fs,
            &language_registry,
            PreviewPolicy::LiveBufferThenFile,
        );

        let shown = {
            let buffer = ws
                .buffers
                .get(picker.preview.buffer)
                .expect("preview buffer");
            let guard = buffer.read().expect("preview buffer poisoned");
            guard.rope().to_string()
        };
        assert!(
            shown.contains("disk content"),
            "no-buffer path falls back to the disk file, got {shown:?}"
        );

        picker.dispose(ws);
    }

    /// Display strings of the filtered rows after running `query` over `base`.
    fn names(query: &str, base: Vec<PathBuf>, git_root: &Path) -> Vec<String> {
        let mut list = PickList {
            base,
            ..PickList::default()
        };
        list.refilter(query, git_root);
        list.filtered
            .iter()
            .map(|i| paths::display_relative(&list.base[*i], git_root))
            .collect()
    }

    #[test]
    fn empty_input_lists_all_base_paths_sorted() {
        let git_root = p("/r");
        let base = vec![p("/r/b.rs"), p("/r/a.rs"), p("/r/sub/c.rs")];
        assert_eq!(names("", base, &git_root), vec!["a.rs", "b.rs", "sub/c.rs"]);
    }

    #[test]
    fn prefix_ranks_before_substring_before_fuzzy() {
        let git_root = p("/r");
        let base = vec![
            p("/r/file.rs"),      // prefix
            p("/r/sub/file.rs"),  // substring
            p("/r/fee/nile.rs"),  // fuzzy (f..i..l..e)
            p("/r/unrelated.rs"), // filtered out
        ];
        assert_eq!(
            names("file", base, &git_root),
            vec!["file.rs", "sub/file.rs", "fee/nile.rs"]
        );
    }

    #[test]
    fn case_insensitive_filter() {
        let git_root = p("/r");
        let base = vec![p("/r/Foo.rs"), p("/r/bar.rs")];
        assert_eq!(names("foo", base, &git_root), vec!["Foo.rs"]);
    }

    #[test]
    fn root_anchor_lists_only_prefixed_paths() {
        let git_root = p("/r");
        let base = vec![p("/r/docs/a.md"), p("/r/src/b.rs")];
        assert_eq!(names("./docs", base, &git_root), vec!["docs/a.md"]);
    }

    #[test]
    fn root_anchor_matches_a_partial_prefix() {
        let git_root = p("/r");
        let base = vec![p("/r/docs/a.md"), p("/r/docs/b.md"), p("/r/src/c.rs")];
        assert_eq!(
            names("./do", base, &git_root),
            vec!["docs/a.md", "docs/b.md"]
        );
    }

    #[test]
    fn root_anchor_narrows_with_a_trailing_pattern() {
        let git_root = p("/r");
        let base = vec![
            p("/r/docs/readme.md"),
            p("/r/docs/other.md"),
            p("/r/src/x.rs"),
        ];
        assert_eq!(names("./docs rea", base, &git_root), vec!["docs/readme.md"]);
    }

    #[test]
    fn bare_root_anchor_lists_all_sorted() {
        let git_root = p("/r");
        let base = vec![p("/r/b.rs"), p("/r/a.rs")];
        assert_eq!(names("./", base, &git_root), vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn root_anchor_highlights_the_prefix() {
        let git_root = p("/r");
        let mut list = PickList {
            base: vec![p("/r/docs/readme.md")],
            ..PickList::default()
        };
        list.refilter("./docs", &git_root);
        assert_eq!(list.match_indices, vec![vec![0, 1, 2, 3]]);
    }

    #[test]
    fn split_root_anchor_only_anchors_the_first_token() {
        assert_eq!(split_root_anchor("./docs"), (Some("docs"), ""));
        assert_eq!(split_root_anchor("./docs rea"), (Some("docs"), "rea"));
        assert_eq!(split_root_anchor("./"), (Some(""), ""));
        assert_eq!(split_root_anchor("foo ./docs"), (None, "foo ./docs"));
        assert_eq!(split_root_anchor("foo"), (None, "foo"));
    }

    #[test]
    fn trailing_space_does_not_eliminate_matches() {
        let git_root = p("/r");
        let base = vec![p("/r/foo.rs"), p("/r/bar.rs")];
        assert_eq!(names(".rs ", base, &git_root), vec!["bar.rs", "foo.rs"]);
    }

    #[test]
    fn multi_token_query_matches_in_either_order() {
        let git_root = p("/r");
        let base = vec![p("/r/src/foo.rs"), p("/r/src/bar.rs")];
        let forward = names(".rs foo", base.clone(), &git_root);
        let reverse = names("foo .rs", base, &git_root);
        assert_eq!(forward, vec!["src/foo.rs"]);
        assert_eq!(forward, reverse);
    }

    #[test]
    fn whitespace_only_query_lists_all_paths() {
        let git_root = p("/r");
        let base = vec![p("/r/b.rs"), p("/r/a.rs")];
        assert_eq!(names("   ", base, &git_root), vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn exact_basename_match_outranks_longer_prefix_match() {
        let git_root = p("/r");
        let base = vec![p("/r/food_handler.rs"), p("/r/foo.rs")];
        assert_eq!(
            names("foo", base, &git_root),
            vec!["foo.rs", "food_handler.rs"]
        );
    }

    #[test]
    fn filters_against_a_subset_base() {
        let git_root = p("/r");
        let base = vec![p("/r/b.rs")];
        assert_eq!(names("", base, &git_root), vec!["b.rs"]);
    }

    #[test]
    fn empty_base_lists_nothing() {
        let git_root = p("/r");
        assert!(names("", vec![], &git_root).is_empty());
    }

    #[test]
    fn lists_every_base_path_on_empty_query() {
        let git_root = p("/r");
        let base = vec![p("/r/a.rs"), p("/r/c.rs")];
        assert_eq!(names("", base, &git_root), vec!["a.rs", "c.rs"]);
    }

    #[test]
    fn refilter_clamps_selected_when_results_shrink() {
        let git_root = p("/r");
        let mut list = PickList {
            base: vec![p("/r/a.rs"), p("/r/b.rs"), p("/r/c.rs")],
            selected: 2,
            ..PickList::default()
        };
        list.refilter("b", &git_root);
        assert_eq!(list.filtered.len(), 1);
        assert_eq!(list.selected, 0);
    }
}
