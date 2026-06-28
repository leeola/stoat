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
use stoat_scheduler::Executor;
use stoat_text::{Bias, SelectionGoal};

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
    /// `match_indices` is rebuilt in parallel to `filtered`. Each element is the
    /// sorted, deduplicated set of matched character offsets in that row's
    /// display string, or empty when no pattern is active.
    pub(crate) fn refilter(&mut self, query: &str, git_root: &Path) {
        self.filtered.clear();
        self.match_indices.clear();

        let items = self
            .base
            .iter()
            .enumerate()
            .map(|(idx, path)| (idx, paths::display_relative(path, git_root)));
        let Some(mut matches) = fuzzy::match_and_rank(query, items) else {
            let mut rows: Vec<(usize, String)> = self
                .base
                .iter()
                .enumerate()
                .map(|(idx, path)| (idx, paths::display_relative(path, git_root)))
                .collect();
            rows.sort_by(|a, b| a.1.cmp(&b.1));
            for (idx, _) in &rows {
                self.filtered.push(*idx);
                self.match_indices.push(Vec::new());
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
            self.match_indices.push(m.matched_indices);
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

/// Where a [`Preview`] pulls its content from.
///
/// `File` reads the path from disk. `Buffer` reads a live, possibly modified
/// in-memory buffer, so the preview reflects unsaved edits rather than the
/// backing file.
#[derive(PartialEq)]
pub(crate) enum PreviewSource {
    File(PathBuf),
    /// Live in-memory buffer. The buffer picker that previews from it in
    /// production lands later, so outside tests this variant is currently
    /// unconstructed.
    #[allow(dead_code)]
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
