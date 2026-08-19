use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    fuzzy,
    host::OffsetEncoding,
    input_view::{InputView, SubmitTarget},
    markdown::StyledLine,
    picker::{self, Preview, PreviewSource},
    theme::Theme,
    workspace::Workspace,
};
use codegraph::SymbolKey;
use lsp_types::{DocumentSymbol, DocumentSymbolResponse, Position, SymbolInformation, SymbolKind};
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
};
use stoat_language::LanguageRegistry;
use stoat_scheduler::{Executor, Task};
use stoat_text::Rope;

/// Whether the finder lists the focused buffer's document symbols or the whole
/// workspace's symbols.
///
/// Document scope filters a fixed list locally. Workspace scope re-issues the
/// server request as the query changes, so its results also come from the
/// server, not just a local filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolFinderScope {
    Document,
    Workspace,
}

/// Where selecting a [`SymbolFinderEntry`] takes the cursor.
///
/// Document-symbol entries carry an [`Self::Offset`] into the finder's source
/// buffer. Workspace entries carry the target file and LSP position, resolved
/// against that file's server encoding when opened.
#[derive(Debug, Clone)]
pub(crate) enum SymbolTarget {
    Offset(usize),
    Workspace {
        path: PathBuf,
        position: Position,
        encoding: OffsetEncoding,
    },
}

/// One row in the [`SymbolFinder`] list.
///
/// `title` is the fuzzy-matched, dotted-path symbol name. `kind` drives the dim
/// kind column and `line` the trailing `:line` suffix, both painted by the
/// renderer. `target` is where selection jumps.
#[derive(Debug, Clone)]
pub(crate) struct SymbolFinderEntry {
    pub(crate) title: String,
    pub(crate) kind: Option<SymbolKind>,
    pub(crate) line: u32,
    pub(crate) target: SymbolTarget,
}

/// Centered finder modal over a buffer's document symbols.
///
/// Holds the flattened symbol list and a fuzzy view of it. `filtered` indexes
/// `entries` in display order and `match_indices` carries the matched character
/// offsets per filtered row for highlighting. An empty query lists every symbol
/// in document order, since symbol order is meaningful (unlike a path list).
pub(crate) struct SymbolFinder {
    pub(crate) input: InputView,
    pub(crate) scope: SymbolFinderScope,
    pub(crate) entries: Vec<SymbolFinderEntry>,
    pub(crate) filtered: Vec<usize>,
    pub(crate) match_indices: Vec<Vec<u32>>,
    /// The parse behind the current ranking, so a painted row past the indexed
    /// block derives its offsets rather than going unhighlighted.
    last_pattern: Option<fuzzy::Pattern>,
    pub(crate) selected: usize,
    pub(crate) viewport_rows: Option<usize>,
    /// Read-only pane showing the selected symbol's source. Kept in sync by
    /// [`crate::action_handlers::lsp::sync_symbol_finder`].
    pub(crate) preview: Preview,
    /// Rows the preview pane rendered last, driving the scroll centering.
    /// `None` until the first render lays the pane out.
    pub(crate) preview_rows: Option<usize>,
    /// Buffer the finder opened over. The workspace scope routes re-issued
    /// requests through it when its named servers no longer resolve.
    pub(crate) buffer_id: BufferId,
    /// Workspace-symbol servers routed at open. Empty for document scope.
    pub(crate) servers: Vec<String>,
    /// Query a workspace re-issue last fired for, so a changed input triggers a
    /// fresh request. Unused for document scope, which filters locally.
    pub(crate) last_query: String,
    /// A query changed while a workspace request was in flight, so the pump
    /// re-fires with the current text once the in-flight request lands.
    pub(crate) query_dirty: bool,
    /// In-flight `textDocument/hover` for the selected symbol's documentation.
    pub(crate) pending_doc: Option<Task<Option<String>>>,
    /// Filtered index the pending or resolved doc corresponds to, so a stale
    /// response landing after the selection moved is discarded. `None` when no
    /// entry is selected.
    pub(crate) doc_for: Option<usize>,
    /// Resolved hover markdown for the selected symbol, rendered above the source
    /// preview. `None` when unresolved, empty, or the request failed.
    pub(crate) doc_markdown: Option<String>,
    /// [`Self::doc_markdown`] rendered, and the theme epoch it was styled under.
    ///
    /// Rendering it parses every fenced code block and builds a style per byte,
    /// and the renderer would otherwise do that on every paint the modal is up
    /// for a document that cannot have changed. Cleared wherever the markdown
    /// is written, and rebuilt by [`Self::styled_doc_lines`] when the theme
    /// moves under it.
    pub(crate) doc_lines: Option<(u64, Vec<StyledLine>)>,
    /// Rows the modal's list would need for the whole symbol set, which the
    /// renderer sizes the box against.
    ///
    /// Recorded only from an unfiltered result set, never from a narrowed one.
    /// Document scope filters [`Self::entries`] locally, but workspace scope
    /// re-issues the server request per keystroke and replaces them with
    /// query-specific hits, so reading the live count would resize the box under
    /// the user as they type.
    pub(crate) content_rows: u16,
    /// Bumped wherever [`Self::set_entries`] replaces the symbol list, so a
    /// ranking memoized against the old list is never reused for the new one.
    entries_generation: u64,
    /// Query and entry-list generation the current ranking came from.
    ///
    /// The ranking is a pure function of those two, so an unchanged key means
    /// an identical outcome. Ranking runs from the per-frame sync and walks
    /// every entry with a fuzzy traceback, so a modal left sitting over a
    /// workspace's symbols would otherwise re-rank thousands of them a frame.
    pub(crate) last_filter_key: Option<u64>,
    /// Ranking, selection, and pane height the preview pane was last built for.
    ///
    /// The ranking key belongs in it because a new query repoints the same
    /// selection index at a different symbol, and the pane height because the
    /// scroll puts the symbol a third of the way down a pane a resize moves.
    preview_for: Option<(Option<u64>, usize, usize)>,
}

impl SymbolFinder {
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        buffer_id: BufferId,
        scope: SymbolFinderScope,
        servers: Vec<String>,
    ) -> Self {
        let preview = Preview::new(ws, executor.clone());
        let input = InputView::create(ws, executor, SubmitTarget::SymbolFinder, "", "insert", 1);
        Self {
            input,
            scope,
            entries: Vec::new(),
            filtered: Vec::new(),
            match_indices: Vec::new(),
            last_pattern: None,
            selected: 0,
            viewport_rows: None,
            preview,
            preview_rows: None,
            buffer_id,
            servers,
            last_query: String::new(),
            query_dirty: false,
            pending_doc: None,
            doc_for: None,
            doc_markdown: None,
            doc_lines: None,
            content_rows: 0,
            entries_generation: 0,
            last_filter_key: None,
            preview_for: None,
        }
    }

    /// Replace the symbol list and re-run the current `query` over it.
    pub(crate) fn set_entries(&mut self, entries: Vec<SymbolFinderEntry>, query: &str) {
        if query.is_empty() {
            self.content_rows = u16::try_from(entries.len()).unwrap_or(u16::MAX);
        }
        self.entries = entries;
        self.entries_generation = self.entries_generation.wrapping_add(1);
        self.refilter(query);
    }

    /// Re-rank `entries` for `query`, matches first by score descending then
    /// title ascending. An empty or whitespace-only query lists every entry in
    /// document order with no highlights.
    ///
    /// Re-ranking the same list for the same query is skipped, so the per-frame
    /// sync a modal drives costs nothing while the query sits still.
    pub(crate) fn refilter(&mut self, query: &str) {
        let key = {
            let mut hasher = DefaultHasher::new();
            query.hash(&mut hasher);
            self.entries_generation.hash(&mut hasher);
            hasher.finish()
        };
        if self.last_filter_key == Some(key) {
            return;
        }
        self.last_filter_key = Some(key);

        let (filtered, match_indices, pattern) = rank_entries(&self.entries, query);
        self.filtered = filtered;
        self.match_indices = match_indices;
        self.last_pattern = pattern;
        self.clamp_selected();
    }

    /// Whether the preview pane has to be rebuilt for a pane `rows` tall, and
    /// record that it was.
    ///
    /// True once per ranking, selection, and pane height, so a modal sitting
    /// still reloads and re-scrolls the pane once rather than every frame the
    /// run loop drives.
    pub(crate) fn preview_needs_sync(&mut self, rows: usize) -> bool {
        let key = (self.last_filter_key, self.selected, rows);
        if self.preview_for == Some(key) {
            return false;
        }
        self.preview_for = Some(key);
        true
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        picker::nav_move(self.filtered.len(), &mut self.selected, delta);
    }

    /// Page the selection by half the rendered list height in `dir` (negative
    /// up, positive down). Falls back to a single row before the first render
    /// sets [`Self::viewport_rows`].
    pub(crate) fn page(&mut self, dir: i32) {
        self.move_selection(dir * picker::nav_page_step(self.viewport_rows));
    }

    /// The entry under the selection cursor, or `None` for an empty list.
    pub(crate) fn selected_entry(&self) -> Option<&SymbolFinderEntry> {
        let idx = *self.filtered.get(self.selected)?;
        self.entries.get(idx)
    }

    /// The selected symbol's documentation as styled lines, or `None` when it
    /// has none.
    ///
    /// Renders [`Self::doc_markdown`] only when nothing is held for
    /// `theme_epoch`, so a modal painting frame after frame over one symbol
    /// renders it once. The styles are baked into the lines, which is why a
    /// theme change rebuilds them rather than reusing what it finds.
    pub(crate) fn styled_doc_lines(
        &mut self,
        theme_epoch: u64,
        theme: &Theme,
        languages: &LanguageRegistry,
    ) -> Option<&[StyledLine]> {
        let markdown = self.doc_markdown.as_deref()?;

        let held = self
            .doc_lines
            .as_ref()
            .is_some_and(|(epoch, _)| *epoch == theme_epoch);
        if !held {
            let lines = crate::markdown::render_markdown(markdown, theme, languages);
            self.doc_lines = Some((theme_epoch, lines));
        }

        self.doc_lines.as_ref().map(|(_, lines)| lines.as_slice())
    }

    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
        self.preview.dispose(ws);
    }

    /// Matched offsets to highlight in filtered `row`'s title.
    ///
    /// See [`picker::row_indices`] for what `scratch` and `matching` are for.
    pub(crate) fn row_indices<'a>(
        &'a self,
        row: usize,
        scratch: &'a mut Vec<u32>,
        matching: &mut fuzzy::Scratch,
    ) -> &'a [u32] {
        let title = self
            .filtered
            .get(row)
            .and_then(|&idx| self.entries.get(idx))
            .map(|entry| entry.title.as_str())
            .unwrap_or_default();
        picker::row_indices(
            &self.match_indices,
            self.last_pattern.as_ref(),
            row,
            title,
            scratch,
            matching,
        )
    }

    fn clamp_selected(&mut self) {
        picker::nav_clamp(self.filtered.len(), &mut self.selected);
    }
}

/// Rank `entries` for `query`, returning the parallel `(filtered,
/// match_indices)` vectors and the parse that painting rows past the indexed
/// block needs. `filtered` holds indices into `entries`. Empty query yields
/// document order and no highlights.
fn rank_entries(
    entries: &[SymbolFinderEntry],
    query: &str,
) -> (Vec<usize>, Vec<Vec<u32>>, Option<fuzzy::Pattern>) {
    let items = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| (idx, entry.title.as_str()));

    let mut filtered = Vec::new();
    let mut match_indices = Vec::new();
    let pattern = picker::rank_into(
        query,
        items,
        entries.len(),
        &mut filtered,
        &mut match_indices,
    );
    (filtered, match_indices, pattern)
}

/// One entry in the graph-navigation [`SymbolPicker`]. `title` is the symbol
/// name as painted in the popup and `symbol` the graph node the entry jumps to
/// on selection.
#[derive(Debug, Clone)]
pub(crate) struct SymbolEntry {
    pub(crate) title: String,
    pub(crate) symbol: SymbolKey,
}

/// Cursor-anchored graph-navigation picker. Painted as a numbered
/// popup over a viewport of up to 9 visible entries that follows
/// [`Self::selected_idx`]. The user navigates with `j`/`k`, picks
/// the selected entry with Enter, picks visible entries 1..=9 with
/// the corresponding digit keys, and dismisses with Escape or any
/// other action.
///
/// Document symbols use the [`SymbolFinder`] modal
/// instead. Only code-graph navigation still populates this popup.
#[derive(Debug, Clone)]
pub(crate) struct SymbolPicker {
    pub(crate) entries: Vec<SymbolEntry>,
    pub(crate) anchor_offset: usize,
    pub(crate) selected_idx: usize,
}

/// Sync the finder's preview pane to the selected entry.
///
/// Loads the entry's source (the focused buffer for document scope, the target
/// file or its live buffer for workspace scope), scrolls it so the symbol line
/// sits a third down the pane, and lands the preview cursor at the symbol offset
/// so the target line is markable. Clears the pane when nothing is selected.
///
/// Runs once per ranking, selection, and pane height. The per-frame sync above
/// would otherwise redo all of it every frame the modal stays open.
pub(crate) fn sync_symbol_finder_preview(stoat: &mut Stoat) {
    let Some((buffer_id, rows, entry)) = stoat.symbol_finder.as_mut().and_then(|finder| {
        let rows = finder.preview_rows.unwrap_or(Preview::ROWS_FALLBACK);
        finder.preview_needs_sync(rows).then(|| {
            (
                finder.buffer_id,
                rows,
                finder.selected_entry().map(|e| (e.line, e.target.clone())),
            )
        })
    }) else {
        return;
    };

    let idx = stoat.active_workspace;
    let fs_host = &*stoat.fs_host;
    let language_registry = &stoat.language_registry;
    let ws = &mut stoat.workspaces[idx];
    let Some(finder) = stoat.symbol_finder.as_mut() else {
        return;
    };

    let Some((line, target)) = entry else {
        finder.preview.clear(ws);
        return;
    };

    let source = match &target {
        SymbolTarget::Offset(_) => PreviewSource::Buffer(buffer_id),
        SymbolTarget::Workspace { path, .. } => match ws.buffers.id_for_path(path) {
            Some(id) => PreviewSource::Buffer(id),
            None => PreviewSource::File(path.clone()),
        },
    };
    finder
        .preview
        .sync_at_line(ws, fs_host, language_registry, source, line, rows);

    let editor_id = finder.preview.editor;
    if let Some(editor) = ws.editors.get_mut(editor_id) {
        let offset = {
            let snapshot = editor.display_map.snapshot();
            let buf_snap = snapshot.buffer_snapshot();
            match &target {
                SymbolTarget::Offset(off) => *off,
                SymbolTarget::Workspace {
                    position, encoding, ..
                } => {
                    crate::lsp::util::lsp_pos_to_byte_offset(buf_snap.rope(), *position, *encoding)
                },
            }
        };
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();
        let clamped = offset.min(rope.len());
        crate::action_handlers::movement::move_cursors(
            &mut editor.selections,
            buffer_snapshot,
            false,
            |_| Some((clamped, stoat_text::SelectionGoal::None)),
        );
    }
}

/// Complete the highlighted symbol's title into the finder input, replacing
/// what was typed, and leave that symbol selected.
///
/// No-op when the finder is closed or its list is empty. Under the workspace
/// scope the completed query re-issues `workspace/symbol` through the per-frame
/// [`sync_symbol_finder`] path, so this only touches local state.
///
/// Re-ranking against the completed title moves it to the top of the list while
/// the selection cursor keeps its old index, so the cursor is repositioned onto
/// the completed entry afterwards. It is tracked by its index into `entries`
/// rather than by assuming the exact match ranks first, since symbol titles are
/// not unique and equal scores tie-break on title alone.
pub(crate) fn symbol_finder_complete(stoat: &mut Stoat) -> UpdateEffect {
    let active_idx = stoat.active_workspace;

    let Some((entry_idx, title)) = stoat.symbol_finder.as_ref().and_then(|finder| {
        let entry_idx = *finder.filtered.get(finder.selected)?;
        let title = finder.entries.get(entry_idx)?.title.clone();
        Some((entry_idx, title))
    }) else {
        return UpdateEffect::None;
    };

    {
        let ws = &mut stoat.workspaces[active_idx];
        if let Some(finder) = stoat.symbol_finder.as_ref() {
            finder.input.replace_text(ws, &title);
        }
    }

    if let Some(finder) = stoat.symbol_finder.as_mut() {
        finder.refilter(&title);
        if let Some(row) = finder.filtered.iter().position(|&i| i == entry_idx) {
            finder.selected = row;
        }
    }
    UpdateEffect::Redraw
}

/// Move the symbol finder selection by `delta`, saturating at list bounds.
pub(crate) fn symbol_finder_move_selection(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    match stoat.symbol_finder.as_mut() {
        Some(finder) => {
            finder.move_selection(delta);
            UpdateEffect::Redraw
        },
        None => UpdateEffect::None,
    }
}

/// Page the symbol finder selection by half the list height in `dir`.
pub(crate) fn symbol_finder_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    match stoat.symbol_finder.as_mut() {
        Some(finder) => {
            finder.page(dir);
            UpdateEffect::Redraw
        },
        None => UpdateEffect::None,
    }
}

/// Jump to the selected symbol and close the finder.
///
/// Returns `None` when no finder is open so [`crate::action_handlers::lsp::submit_prompt_input`]
/// falls through to the next probe. An empty list closes without jumping.
pub(crate) fn symbol_finder_submit(stoat: &mut Stoat) -> Option<UpdateEffect> {
    stoat.symbol_finder.as_ref()?;
    let target = stoat
        .symbol_finder
        .as_ref()
        .and_then(|finder| finder.selected_entry())
        .map(|entry| entry.target.clone());
    close_symbol_finder(stoat);
    match target {
        Some(SymbolTarget::Offset(offset)) => {
            crate::action_handlers::movement::jump_to_offset(stoat, offset);
        },
        Some(SymbolTarget::Workspace {
            path,
            position,
            encoding,
        }) => {
            crate::action_handlers::lsp::open_workspace_symbol_target(
                stoat, &path, position, encoding,
            );
        },
        None => {},
    }
    Some(UpdateEffect::Redraw)
}

/// Close the symbol finder on Escape.
///
/// Returns `None` when no finder is open so [`crate::action_handlers::lsp::cancel_prompt_input`]
/// falls through to the next probe.
pub(crate) fn symbol_finder_cancel(stoat: &mut Stoat) -> Option<UpdateEffect> {
    if stoat.symbol_finder.is_some() {
        close_symbol_finder(stoat);
        return Some(UpdateEffect::Redraw);
    }
    None
}

/// Close the symbol finder, disposing its input editor and dropping any
/// in-flight document or workspace request so a late response is discarded.
pub(crate) fn close_symbol_finder(stoat: &mut Stoat) {
    stoat.pending_symbol_picker_request = None;
    stoat.pending_workspace_symbol_request = None;
    if let Some(finder) = stoat.symbol_finder.take() {
        finder.dispose(stoat.active_workspace_mut());
    }
}

/// Convert a [`DocumentSymbolResponse`] into a flat list of picker
/// entries, resolving each symbol's LSP position to a byte offset
/// in the supplied rope. Nested responses are flattened DFS with a
/// dotted ancestor-path prefix on the title (e.g. `outer.inner`) so
/// the picker conveys hierarchy. The full list is returned; the
/// renderer paints a 9-row viewport over `entries`.
pub(crate) fn symbol_picker_entries(
    rope: &Rope,
    encoding: OffsetEncoding,
    response: DocumentSymbolResponse,
) -> Vec<SymbolFinderEntry> {
    let mut entries: Vec<SymbolFinderEntry> = Vec::new();
    match response {
        DocumentSymbolResponse::Flat(items) => {
            for SymbolInformation {
                name,
                location,
                kind,
                ..
            } in items
            {
                let offset =
                    crate::lsp::util::lsp_pos_to_byte_offset(rope, location.range.start, encoding);
                entries.push(finder_entry(rope, name, kind, offset));
            }
        },
        DocumentSymbolResponse::Nested(items) => {
            fn walk(
                rope: &Rope,
                encoding: OffsetEncoding,
                items: Vec<DocumentSymbol>,
                ancestors: &mut Vec<String>,
                out: &mut Vec<SymbolFinderEntry>,
            ) {
                for symbol in items {
                    let offset = crate::lsp::util::lsp_pos_to_byte_offset(
                        rope,
                        symbol.selection_range.start,
                        encoding,
                    );
                    let title = if ancestors.is_empty() {
                        symbol.name.clone()
                    } else {
                        format!("{}.{}", ancestors.join("."), symbol.name)
                    };
                    out.push(finder_entry(rope, title, symbol.kind, offset));
                    if let Some(children) = symbol.children {
                        ancestors.push(symbol.name);
                        walk(rope, encoding, children, ancestors, out);
                        ancestors.pop();
                    }
                }
            }
            let mut ancestors: Vec<String> = Vec::new();
            walk(rope, encoding, items, &mut ancestors, &mut entries);
        },
    }
    entries
}

/// Build a document-symbol finder entry, deriving the display line from the
/// resolved byte `offset`.
fn finder_entry(rope: &Rope, title: String, kind: SymbolKind, offset: usize) -> SymbolFinderEntry {
    SymbolFinderEntry {
        title,
        kind: Some(kind),
        line: rope.offset_to_point(offset).row,
        target: SymbolTarget::Offset(offset),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        test_fixture::{
            enable_document_symbols, enable_document_symbols_and_hover, enable_workspace_symbols,
            flat_symbol, open_buffer, seed,
        },
        test_harness::TestHarness,
    };
    use lsp_types::DocumentSymbolResponse;
    #[test]
    fn symbol_finder_previews_document_scrolled_to_line() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let src = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nfn target() {}\n";
        let root = seed(&mut h, &[("main.rs", src)]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![flat_symbol(
                "target",
                path.to_str().unwrap(),
                10,
                3,
            )]),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();

        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        let preview_editor = finder.preview.editor;
        let preview_buffer = finder.preview.buffer;
        let ws = h.stoat.active_workspace();
        let content = ws
            .buffers
            .get(preview_buffer)
            .expect("preview buffer")
            .read()
            .expect("preview buffer")
            .rope()
            .to_string();
        assert_eq!(content, src, "the preview mirrors the focused buffer");
        assert_eq!(
            ws.editors
                .get(preview_editor)
                .expect("preview editor")
                .scroll_row,
            2,
            "line 10 sits a third down a 24-row pane (10 - 8)",
        );
    }

    #[test]
    fn symbol_finder_previews_workspace_file_from_disk() {
        use lsp_types::SymbolKind;
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(
            &mut h,
            &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
        );
        let lib = root.join("lib.rs");
        open_buffer(&mut h, root.join("main.rs"));
        h.fake_lsp().add_workspace_symbol(
            "bar",
            "bar",
            SymbolKind::FUNCTION,
            lib.to_str().unwrap(),
            0,
            3,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("b a r");
        h.settle();

        let preview_buffer = h
            .stoat
            .symbol_finder
            .as_ref()
            .expect("finder")
            .preview
            .buffer;
        let content = h
            .stoat
            .active_workspace()
            .buffers
            .get(preview_buffer)
            .expect("preview buffer")
            .read()
            .expect("preview buffer")
            .rope()
            .to_string();
        assert_eq!(
            content, "fn bar() {}\n",
            "the workspace preview shows the unopened target file from disk",
        );
    }

    #[test]
    fn symbol_finder_close_disposes_preview() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![flat_symbol("foo", path.to_str().unwrap(), 0, 3)]),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();

        let (editor_id, buffer_id) = {
            let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
            (finder.preview.editor, finder.preview.buffer)
        };
        assert!(h.stoat.active_workspace().editors.get(editor_id).is_some());

        h.type_keys("escape");
        assert!(h.stoat.symbol_finder.is_none());
        assert!(
            h.stoat.active_workspace().editors.get(editor_id).is_none(),
            "the preview editor is disposed on close",
        );
        assert!(
            h.stoat.active_workspace().buffers.get(buffer_id).is_none(),
            "the preview scratch buffer is disposed on close",
        );
    }

    /// The symbols are ordered so completing reshuffles the list rather than
    /// narrowing it to one row. An empty query lists them in document order, so
    /// `aaa` sits second. Completing it re-ranks the exact match to the top
    /// while `aaa_longer` still matches and stays on the list. A cursor left at
    /// its old index would therefore land on `aaa_longer`, and clamping cannot
    /// rescue it because the index is still in range.
    #[test]
    fn symbol_finder_tab_completes_and_keeps_the_entry_selected() {
        let mut h = TestHarness::with_size(120, 30);
        enable_document_symbols_and_hover(&h);
        let root = seed(&mut h, &[("main.rs", "fn aaa_longer() {}\nfn aaa() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![
                flat_symbol("aaa_longer", path.to_str().unwrap(), 0, 3),
                flat_symbol("aaa", path.to_str().unwrap(), 1, 3),
            ]),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();

        h.type_keys("down");
        h.settle();
        assert_eq!(
            h.stoat
                .symbol_finder
                .as_ref()
                .unwrap()
                .selected_entry()
                .map(|e| e.title.as_str()),
            Some("aaa"),
            "the second row is highlighted before completing"
        );

        h.type_keys("tab");
        h.settle();

        let finder = h.stoat.symbol_finder.as_ref().unwrap();
        assert_eq!(
            finder.input.text(h.stoat.active_workspace()),
            "aaa",
            "Tab completes the highlighted title into the input"
        );
        assert_eq!(
            finder.filtered.len(),
            2,
            "the completed query still matches both symbols, so the list reshuffles"
        );
        assert_eq!(
            finder.selected_entry().map(|e| e.title.as_str()),
            Some("aaa"),
            "the completed symbol stays selected after the re-rank"
        );
    }

    #[test]
    fn symbol_finder_tab_with_an_empty_list_is_a_noop() {
        let mut h = TestHarness::with_size(120, 30);
        enable_document_symbols_and_hover(&h);
        let root = seed(&mut h, &[("main.rs", "fn aaa() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![flat_symbol("aaa", path.to_str().unwrap(), 0, 3)]),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();

        h.type_text("zzz");
        h.settle();
        assert!(
            h.stoat.symbol_finder.as_ref().unwrap().filtered.is_empty(),
            "the query matches nothing"
        );

        h.type_keys("tab");
        h.settle();

        assert_eq!(
            h.stoat
                .symbol_finder
                .as_ref()
                .unwrap()
                .input
                .text(h.stoat.active_workspace()),
            "zzz",
            "Tab with no selectable row leaves the query unchanged"
        );
    }

    use super::{
        rank_entries, StyledLine, SymbolFinder, SymbolFinderEntry, SymbolFinderScope, SymbolTarget,
    };
    use crate::{
        buffer::BufferId,
        editor_state::EditorId,
        fuzzy,
        input_view::{InputView, SubmitTarget},
        picker::{self, Preview},
    };

    fn entry(title: &str) -> SymbolFinderEntry {
        SymbolFinderEntry {
            title: title.to_string(),
            kind: None,
            line: 0,
            target: SymbolTarget::Offset(0),
        }
    }

    fn finder(titles: &[&str]) -> SymbolFinder {
        let input = InputView {
            editor_id: EditorId::default(),
            buffer_id: BufferId::new(0),
            target: SubmitTarget::SymbolFinder,
            max_height: 1,
        };
        let mut f = SymbolFinder {
            input,
            scope: SymbolFinderScope::Document,
            entries: titles.iter().map(|t| entry(t)).collect(),
            filtered: Vec::new(),
            match_indices: Vec::new(),
            last_pattern: None,
            selected: 0,
            viewport_rows: None,
            preview: Preview::test_dummy(),
            preview_rows: None,
            buffer_id: BufferId::new(0),
            servers: Vec::new(),
            last_query: String::new(),
            query_dirty: false,
            pending_doc: None,
            doc_for: None,
            doc_markdown: None,
            doc_lines: None,
            content_rows: titles.len() as u16,
            entries_generation: 0,
            last_filter_key: None,
            preview_for: None,
        };
        f.refilter("");
        f
    }

    /// The renderer asks per frame while the modal is up, and rendering the
    /// markdown parses every fenced code block in it, so a second ask over the
    /// same document must not render again.
    ///
    /// The held lines are overwritten between the asks, which a re-render would
    /// replace and a reuse hands straight back.
    #[test]
    fn doc_lines_render_once_until_the_theme_moves() {
        use ratatui::style::Style;
        use stoat_language::LanguageRegistry;

        let mut f = finder(&["alpha"]);
        f.doc_markdown = Some("# heading\n\n```rust\nfn a() {}\n```".to_string());
        let theme = crate::theme::Theme::empty();
        let languages = LanguageRegistry::standard();

        assert!(
            f.styled_doc_lines(3, &theme, &languages).is_some(),
            "the first ask renders the markdown"
        );

        let held: Vec<StyledLine> = vec![vec![("held".to_string(), Style::default())]];
        f.doc_lines = Some((3, held.clone()));
        assert_eq!(
            f.styled_doc_lines(3, &theme, &languages),
            Some(held.as_slice()),
            "the same theme reuses what is held rather than rendering again"
        );

        assert_ne!(
            f.styled_doc_lines(4, &theme, &languages),
            Some(held.as_slice()),
            "a new theme restyles every span, so the held lines no longer describe it"
        );
    }

    #[test]
    fn empty_query_lists_in_document_order() {
        let entries: Vec<_> = ["zeta", "alpha", "mu"].iter().map(|t| entry(t)).collect();
        let (filtered, indices, _) = rank_entries(&entries, "");
        assert_eq!(filtered, vec![0, 1, 2]);
        assert_eq!(indices, vec![Vec::<u32>::new(); 3]);
    }

    #[test]
    fn query_ranks_matches_by_score_then_title() {
        let entries: Vec<_> = ["format_all", "fmt", "unrelated"]
            .iter()
            .map(|t| entry(t))
            .collect();
        let (filtered, indices, _) = rank_entries(&entries, "fmt");
        assert_eq!(
            filtered
                .iter()
                .map(|&i| entries[i].title.as_str())
                .collect::<Vec<_>>(),
            vec!["fmt", "format_all"],
            "both fuzzy-match fmt, the unrelated symbol is dropped"
        );
        assert!(
            !indices[0].is_empty(),
            "the top match carries highlight offsets"
        );
    }

    /// Deriving offsets is the expensive half of a match, so a workspace's
    /// symbols are ranked with only the leading block indexed. A reader who
    /// scrolls below it still gets highlights, derived from the kept parse when
    /// the row is painted.
    #[test]
    fn a_row_below_the_indexed_block_derives_its_offsets() {
        let titles: Vec<String> = (0..picker::INDEXED_ROWS + 8)
            .map(|i| format!("fmt_{i:04}"))
            .collect();
        let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
        let mut f = finder(&refs);
        f.refilter("fmt");

        assert_eq!(f.filtered.len(), titles.len(), "every title matches");
        assert_eq!(
            f.match_indices.len(),
            picker::INDEXED_ROWS,
            "the leading block alone carries stored offsets",
        );

        let mut scratch = Vec::new();
        let mut matching = fuzzy::Scratch::default();
        assert_eq!(
            f.row_indices(picker::INDEXED_ROWS + 4, &mut scratch, &mut matching),
            [0, 1, 2],
            "a row past the block still highlights its matched prefix",
        );
    }

    #[test]
    fn an_unchanged_query_reuses_the_ranking() {
        let mut f = finder(&["alpha", "beta"]);
        f.filtered.clear();

        f.refilter("");
        assert!(f.filtered.is_empty(), "the same query ranks nothing again");

        f.refilter("alph");
        assert_eq!(f.filtered, vec![0], "a changed query ranks afresh");
    }

    #[test]
    fn replacing_the_entries_ranks_the_same_query_again() {
        let mut f = finder(&["alpha"]);
        f.set_entries(vec![entry("beta"), entry("gamma")], "");
        assert_eq!(
            f.filtered,
            vec![0, 1],
            "a new list outranks a memo held for the old one"
        );
    }

    #[test]
    fn the_preview_rebuilds_once_per_ranking_selection_and_pane_height() {
        let mut f = finder(&["alpha", "alpine"]);
        assert!(f.preview_needs_sync(20), "the first sync builds the pane");
        assert!(!f.preview_needs_sync(20), "a still modal rebuilds nothing");

        f.move_selection(1);
        assert!(
            f.preview_needs_sync(20),
            "a moved selection is a new symbol"
        );
        assert!(f.preview_needs_sync(24), "a resized pane re-centers it");

        f.refilter("al");
        assert_eq!(f.selected, 1, "both entries still match, so it holds");
        assert!(
            f.preview_needs_sync(24),
            "a new ranking points the held index at a different symbol"
        );
    }

    #[test]
    fn move_selection_clamps_to_bounds() {
        let mut f = finder(&["a", "b", "c"]);
        f.move_selection(-1);
        assert_eq!(f.selected, 0);
        f.move_selection(5);
        assert_eq!(f.selected, 2);
        f.move_selection(-1);
        assert_eq!(f.selected, 1);
    }

    #[test]
    fn page_steps_by_half_viewport() {
        let mut f = finder(&["a", "b", "c", "d", "e", "f"]);
        f.viewport_rows = Some(4);
        f.page(1);
        assert_eq!(f.selected, 2, "half of a 4-row viewport is 2");
        f.page(-1);
        assert_eq!(f.selected, 0);
    }

    #[test]
    fn refilter_clamps_stale_selection() {
        let mut f = finder(&["apple", "apricot", "banana"]);
        f.selected = 2;
        f.refilter("ap");
        assert_eq!(f.filtered.len(), 2);
        assert_eq!(f.selected, 1, "selection past the shorter list clamps");
    }

    #[test]
    fn selected_entry_follows_the_cursor() {
        let mut f = finder(&["a", "b", "c"]);
        f.move_selection(1);
        assert_eq!(f.selected_entry().map(|e| e.title.as_str()), Some("b"));
    }
}
