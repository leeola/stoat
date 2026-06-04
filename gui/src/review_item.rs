use crate::{
    buffer::Buffer,
    buffer_registry::BufferRegistry,
    diagnostics::DiagnosticSet,
    diff_map::DiffMap,
    display_map::DisplayMap,
    editor::{Editor, EditorMode},
    globals::ExecutorGlobal,
    item::{DeserializeSnafu, ItemError, ItemView},
    multi_buffer::MultiBuffer,
    review_session::{ReviewSession, ReviewSessionEvent},
    theme::Theme,
    workspace::Workspace,
};
use gpui::{
    div, App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Subscription, WeakEntity, Window,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, ops::Range, path::PathBuf, sync::Arc};
use stoat::{
    buffer::BufferId,
    display_map::{
        highlights::{DecorationHighlight, HighlightStyle},
        BlockPlacement, BlockProperties, BlockStyle,
    },
    review::{ReviewRow, ReviewSide},
    review_session::{
        ChunkFingerprint, ChunkStatus, ReviewChunk, ReviewChunkId, ReviewFile,
        ReviewSession as InnerSession, ReviewSource,
    },
};
use stoat_scheduler::Executor;
use stoat_text::Bias;

/// Pane-hosted review surface. Wraps an [`Entity<ReviewSession>`] and
/// one [`ReviewFileView`] per file in the session.
///
/// `commit_summary` carries the optional commit subject line used by
/// [`ItemView::tab_label`] for the [`ReviewSource::Commit`] variant.
/// The workspace's `OpenReviewCommit` action populates it from the
/// git host after constructing the item; absent a summary the label
/// falls back to the short sha.
pub struct ReviewItem {
    session: Entity<ReviewSession>,
    files: Vec<ReviewFileView>,
    commit_summary: Option<String>,
    buffer_registry: Option<Entity<BufferRegistry>>,
    /// `(chunk, index)` cursor for the `JumpToNextMoveSource` /
    /// `JumpToPrevMoveSource` cycle. Cleared whenever the
    /// session's chunk cursor moves.
    move_cursor: Option<(ReviewChunkId, usize)>,
    /// Workspace handle + diagnostics for installing the LSP feature set on
    /// the right pane. Set by the workspace via [`Self::attach_lsp`]; `None`
    /// for items built without a workspace (tests). Retained so a rebuild
    /// re-installs LSP on the freshly-built right panes.
    lsp: Option<ReviewLsp>,
    _session_subscription: Option<Subscription>,
}

/// The workspace-only state the right review pane needs to host LSP features.
struct ReviewLsp {
    workspace: WeakEntity<Workspace>,
    diagnostics: Entity<DiagnosticSet>,
}

/// One file's two-pane view state. The right pane ([`Self::editor`] over the
/// on-disk [`Self::buffer`]) holds the added/current text and is the pane
/// review-navigation consumers drive; the left pane ([`Self::left_editor`]
/// over the read-only [`Self::left_buffer`]) holds the base/removed text.
/// Both are singleton editors padded with spacer blocks so they stay
/// line-for-line aligned.
pub struct ReviewFileView {
    pub rel_path: String,
    pub editor: Entity<Editor>,
    pub multi_buffer: Entity<MultiBuffer>,
    pub buffer: Entity<Buffer>,
    pub left_editor: Entity<Editor>,
    pub left_buffer: Entity<Buffer>,
    /// Header line shown above the two panes (commit boundary + staged count).
    pub header: String,
}

impl ReviewItem {
    pub fn new(session: Entity<ReviewSession>, files: Vec<ReviewFileView>) -> Self {
        Self {
            session,
            files,
            commit_summary: None,
            buffer_registry: None,
            move_cursor: None,
            lsp: None,
            _session_subscription: None,
        }
    }

    /// Build a [`ReviewItem`] for `session`, materializing one
    /// [`ReviewFileView`] per file in the session.
    ///
    /// For [`ReviewSource::WorkingTree`], each file's buffer comes
    /// from `buffer_registry` so edits and LSP attach to the
    /// workspace's live working-tree buffer. For all other sources
    /// the file's buffer is a fresh read-only [`Buffer`] materialized
    /// from the session's stored `buffer_text`.
    ///
    /// Reads [`ExecutorGlobal`] for the per-file [`DisplayMap`]; the
    /// caller must install it before constructing the entity.
    pub fn from_session(
        session: Entity<ReviewSession>,
        buffer_registry: &Entity<BufferRegistry>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let (source_kind, file_specs) = snapshot_session(&session, cx);
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let files: Vec<ReviewFileView> = file_specs
            .into_iter()
            .enumerate()
            .map(|(file_index, spec)| {
                let view =
                    build_file_view(spec, source_kind, buffer_registry, executor.clone(), cx);
                view.editor.update(cx, |ed, cx| {
                    ed.set_review_session(Some(session.clone()), cx);
                    ed.set_review_file_index(Some(file_index), cx);
                });
                view
            })
            .collect();
        let subscription = cx.subscribe(&session, |this, _, event: &ReviewSessionEvent, cx| {
            match event {
                ReviewSessionEvent::Refreshed => {
                    this.rebuild_files(cx);
                    this.move_cursor = None;
                },
                ReviewSessionEvent::Changed => this.refresh_decorations(cx),
                _ => {},
            }
            cx.notify();
        });
        let item = Self {
            session,
            files,
            commit_summary: None,
            buffer_registry: Some(buffer_registry.clone()),
            move_cursor: None,
            lsp: None,
            _session_subscription: Some(subscription),
        };
        item.refresh_decorations(cx);
        item
    }

    pub fn move_cursor(&self) -> Option<(ReviewChunkId, usize)> {
        self.move_cursor
    }

    pub fn set_move_cursor(&mut self, cursor: Option<(ReviewChunkId, usize)>) {
        self.move_cursor = cursor;
    }

    /// Rebuild every [`ReviewFileView`] from the current
    /// [`ReviewSession`] state. Called when the session emits
    /// [`ReviewSessionEvent::Refreshed`] so excerpts, deletion
    /// blocks, and the file-header text reflect the freshly
    /// extracted hunks. Reuses [`BufferRegistry`]-backed buffers for
    /// working-tree sources via the registry stored at
    /// [`Self::from_session`] time; without a registry (items
    /// constructed via [`Self::new`] in tests) this method is a
    /// no-op.
    pub fn rebuild_files(&mut self, cx: &mut Context<'_, Self>) {
        let Some(registry) = self.buffer_registry.clone() else {
            return;
        };
        let (source_kind, file_specs) = snapshot_session(&self.session, cx);
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let session = self.session.clone();
        let files: Vec<ReviewFileView> = file_specs
            .into_iter()
            .enumerate()
            .map(|(file_index, spec)| {
                let view = build_file_view(spec, source_kind, &registry, executor.clone(), cx);
                view.editor.update(cx, |ed, cx| {
                    ed.set_review_session(Some(session.clone()), cx);
                    ed.set_review_file_index(Some(file_index), cx);
                });
                view
            })
            .collect();
        self.files = files;
        self.refresh_decorations(cx);
        self.install_lsp(cx);
        cx.notify();
    }

    /// Attach the workspace state the right pane needs for LSP and install the
    /// feature set. Called once by the workspace after the item is built; the
    /// context is retained so [`Self::rebuild_files`] re-installs LSP on the
    /// freshly-built right panes after an external-edit refresh.
    pub fn attach_lsp(
        &mut self,
        workspace: WeakEntity<Workspace>,
        diagnostics: Entity<DiagnosticSet>,
        cx: &mut Context<'_, Self>,
    ) {
        self.lsp = Some(ReviewLsp {
            workspace,
            diagnostics,
        });
        self.install_lsp(cx);
    }

    /// Install the normal editor's LSP feature set on each file's right pane,
    /// mirroring [`crate::workspace::Workspace`]'s `build_editor_for_path`.
    /// No-op without an attached [`ReviewLsp`] (tests) or when the source is
    /// not [`ReviewSource::WorkingTree`] -- every other source backs the right
    /// pane with a synthetic, pathless buffer that is not the on-disk file the
    /// language server sees. The left pane is never touched, so its
    /// `file_path`-gated requests no-op.
    fn install_lsp(&self, cx: &mut Context<'_, Self>) {
        let Some((workspace, diagnostics)) = self
            .lsp
            .as_ref()
            .map(|lsp| (lsp.workspace.clone(), lsp.diagnostics.clone()))
        else {
            return;
        };
        let paths: Vec<PathBuf> = {
            let session = self.session.read(cx);
            let inner = session.inner();
            if !matches!(inner.source, ReviewSource::WorkingTree { .. }) {
                return;
            }
            inner.files.iter().map(|file| file.path.clone()).collect()
        };
        for (view, path) in self.files.iter().zip(paths) {
            let workspace = workspace.clone();
            let diagnostics = diagnostics.clone();
            view.editor.update(cx, |ed, cx| {
                ed.set_workspace(Some(workspace));
                ed.set_file_path(Some(path), cx);
                ed.set_diagnostic_set(Some(diagnostics), cx);
                ed.install_hover_popup(cx);
                ed.install_completion_popup(cx);
                ed.install_inlay_hints(cx);
                ed.install_code_lens(cx);
                ed.install_semantic_tokens(cx);
                ed.install_signature_help(cx);
                ed.install_syntax_map_updater(cx);
            });
        }
    }

    /// Recompute and install the per-side diff highlights -- intra-line change
    /// spans (added color on the right pane, deleted on the left), move spans,
    /// and the staged-color variant for staged chunks -- on both panes of every
    /// file. Run after the panes are built and whenever the session changes so
    /// staging re-colors live.
    fn refresh_decorations(&self, cx: &mut Context<'_, Self>) {
        let per_file: Vec<(Vec<DecorationSpan>, Vec<DecorationSpan>)> = {
            let session = self.session.read(cx);
            let inner = session.inner();
            inner
                .files
                .iter()
                .map(|file| file_decoration_spans(file, &inner.chunks))
                .collect()
        };
        for (view, (left_spans, right_spans)) in self.files.iter().zip(&per_file) {
            apply_pane_decorations(&view.left_buffer, &view.left_editor, left_spans, cx);
            apply_pane_decorations(&view.buffer, &view.editor, right_spans, cx);
        }
    }

    /// Attach the commit subject line consumed by [`ItemView::tab_label`]
    /// for [`ReviewSource::Commit`]. Other variants ignore this field.
    pub fn set_commit_summary(&mut self, summary: Option<String>, cx: &mut Context<'_, Self>) {
        if self.commit_summary == summary {
            return;
        }
        self.commit_summary = summary;
        cx.notify();
    }

    pub fn commit_summary(&self) -> Option<&str> {
        self.commit_summary.as_deref()
    }

    pub fn session(&self) -> &Entity<ReviewSession> {
        &self.session
    }

    pub fn files(&self) -> &[ReviewFileView] {
        &self.files
    }

    /// File index of the chunk under the session's cursor, or `None`
    /// when the session has no current chunk or the cursor's chunk is
    /// missing from the chunk map.
    pub fn active_file_index(&self, cx: &App) -> Option<usize> {
        let session = self.session.read(cx).inner();
        let id = session.cursor.current?;
        session.chunks.get(&id).map(|chunk| chunk.file_index)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    WorkingTree,
    ReadOnly,
}

struct FileSpec {
    path: PathBuf,
    rel_path: String,
    base_text: Arc<String>,
    buffer_text: Arc<String>,
    /// Spacer blocks padding the left/base pane so it stays row-aligned with
    /// the right pane (one blank row per added line on the right).
    left_fillers: Vec<BlockProperties>,
    /// Spacer blocks padding the right/on-disk pane (one blank row per
    /// removed line on the left).
    right_fillers: Vec<BlockProperties>,
    /// Header line rendered above the file's two panes (commit boundary +
    /// `> rel_path  N/M staged`).
    header: String,
}

/// A blank spacer of `height` rows at `placement`, used to keep the two
/// review panes line-for-line aligned.
fn spacer_block(placement: BlockPlacement, height: u32) -> BlockProperties {
    BlockProperties::from_text(
        placement,
        vec![String::new(); height.max(1) as usize],
        BlockStyle::Fixed,
    )
}

/// Walk one chunk's [`ReviewRow`]s and append the spacer blocks that keep the
/// base (left) and current (right) panes row-aligned: a removed row
/// (`left:Some,right:None`) inserts a blank on the right at the current-side
/// position; an added row (`left:None,right:Some`) inserts a blank on the
/// left at the base-side position. `base_start`/`buffer_start` are the chunk's
/// 0-based start rows, used to anchor a pure-deletion / pure-addition chunk
/// that has no opposite-side line to flush against.
///
/// [`ReviewSide::line_num`] is 1-based; [`BlockPlacement::Above`] is 0-based.
fn append_chunk_fillers(
    rows: &[ReviewRow],
    base_start: u32,
    buffer_start: u32,
    left_fillers: &mut Vec<BlockProperties>,
    right_fillers: &mut Vec<BlockProperties>,
) {
    let mut pending_left = 0u32;
    let mut pending_right = 0u32;
    let mut last_left_row: Option<u32> = None;
    let mut last_right_row: Option<u32> = None;

    for row in rows {
        let (left, right) = match row {
            ReviewRow::Context { left, right } => (Some(left), Some(right)),
            ReviewRow::Changed { left, right } => (left.as_ref(), right.as_ref()),
        };
        if let Some(left) = left {
            let row = left.line_num.saturating_sub(1);
            if pending_left > 0 {
                left_fillers.push(spacer_block(BlockPlacement::Above(row), pending_left));
                pending_left = 0;
            }
            last_left_row = Some(row);
        }
        if let Some(right) = right {
            let row = right.line_num.saturating_sub(1);
            if pending_right > 0 {
                right_fillers.push(spacer_block(BlockPlacement::Above(row), pending_right));
                pending_right = 0;
            }
            last_right_row = Some(row);
        }
        match (left, right) {
            (Some(_), None) => pending_right += 1,
            (None, Some(_)) => pending_left += 1,
            _ => {},
        }
    }

    if pending_left > 0 {
        let placement = match last_left_row {
            Some(row) => BlockPlacement::Below(row),
            None => BlockPlacement::Above(base_start),
        };
        left_fillers.push(spacer_block(placement, pending_left));
    }
    if pending_right > 0 {
        let placement = match last_right_row {
            Some(row) => BlockPlacement::Below(row),
            None => BlockPlacement::Above(buffer_start),
        };
        right_fillers.push(spacer_block(placement, pending_right));
    }
}

/// Per-file commit-boundary header line: `Some(header)` for the first file
/// of a commit group that carries a source commit, `None` otherwise (later
/// files of a group and combined-diff files). One entry per session file,
/// in file order.
fn commit_headers(session: &InnerSession) -> Vec<Option<String>> {
    let progress = session.commit_progress();
    let mut headers = Vec::with_capacity(session.files.len());
    let mut prev: Option<Option<String>> = None;
    for file in &session.files {
        let boundary = prev.as_ref() != Some(&file.commit);
        prev = Some(file.commit.clone());
        let header = file.commit.as_deref().filter(|_| boundary).map(|sha| {
            let (reviewed, total) = progress
                .iter()
                .find(|g| g.commit.as_deref() == Some(sha))
                .map(|g| (g.reviewed, g.total))
                .unwrap_or((0, 0));
            let summary = session.commit_summary(sha).unwrap_or_default();
            format!(
                "{}  {}  {}/{} reviewed",
                short_sha(sha),
                summary,
                reviewed,
                total
            )
        });
        headers.push(header);
    }
    headers
}

/// `Some("Commit X/Y")` when the review spans more than one commit group,
/// `None` for single-commit and combined-diff reviews.
fn commit_position_label(session: &InnerSession) -> Option<String> {
    match session.commit_position() {
        Some((current, total)) if total > 1 => Some(format!("Commit {current}/{total}")),
        _ => None,
    }
}

fn snapshot_session(session: &Entity<ReviewSession>, cx: &App) -> (SourceKind, Vec<FileSpec>) {
    let session_ref = session.read(cx);
    let inner = session_ref.inner();
    let source_kind = match &inner.source {
        ReviewSource::WorkingTree { .. } => SourceKind::WorkingTree,
        _ => SourceKind::ReadOnly,
    };
    let file_specs = inner
        .files
        .iter()
        .zip(commit_headers(inner))
        .map(|(file, commit_header)| {
            let mut left_fillers = Vec::new();
            let mut right_fillers = Vec::new();
            let mut staged_count = 0usize;
            for chunk_id in &file.chunks {
                let Some(chunk) = inner.chunks.get(chunk_id) else {
                    continue;
                };
                if matches!(chunk.status, ChunkStatus::Staged) {
                    staged_count += 1;
                }
                append_chunk_fillers(
                    &chunk.hunk.rows,
                    chunk.base_line_range.start,
                    chunk.buffer_line_range.start,
                    &mut left_fillers,
                    &mut right_fillers,
                );
            }
            let file_line = format!(
                "> {}   {}/{} staged",
                file.rel_path,
                staged_count,
                file.chunks.len()
            );
            let header = match commit_header {
                Some(commit_header) => format!("{commit_header}   {file_line}"),
                None => file_line,
            };
            FileSpec {
                path: file.path.clone(),
                rel_path: file.rel_path.clone(),
                base_text: file.base_text.clone(),
                buffer_text: file.buffer_text.clone(),
                left_fillers,
                right_fillers,
                header,
            }
        })
        .collect();
    (source_kind, file_specs)
}

fn build_file_view(
    spec: FileSpec,
    source_kind: SourceKind,
    buffer_registry: &Entity<BufferRegistry>,
    executor: Executor,
    cx: &mut Context<'_, ReviewItem>,
) -> ReviewFileView {
    let buffer = match source_kind {
        SourceKind::WorkingTree => {
            let (_, shared) =
                buffer_registry.update(cx, |reg, cx| reg.open(&spec.path, &spec.buffer_text, cx));
            let buffer = cx.new(|_| Buffer::from_shared(shared));
            buffer.update(cx, |b, cx| b.set_file_path(Some(spec.path.clone()), cx));
            buffer
        },
        SourceKind::ReadOnly => cx.new(|_| Buffer::with_text(BufferId::new(0), &spec.buffer_text)),
    };
    let left_buffer = cx.new(|_| Buffer::with_text(BufferId::new(1), &spec.base_text));

    let (multi_buffer, editor) =
        build_pane_editor(buffer.clone(), spec.right_fillers, executor.clone(), cx);
    let (_, left_editor) = build_pane_editor(left_buffer.clone(), spec.left_fillers, executor, cx);

    let left_weak = left_editor.downgrade();
    let right_weak = editor.downgrade();
    editor.update(cx, |ed, _| ed.link_scroll(left_weak));
    left_editor.update(cx, |ed, _| ed.link_scroll(right_weak));

    ReviewFileView {
        rel_path: spec.rel_path,
        editor,
        multi_buffer,
        buffer,
        left_editor,
        left_buffer,
        header: spec.header,
    }
}

/// Build a single review pane: a singleton [`MultiBuffer`] over `buffer`
/// padded with `fillers` spacer blocks. The singleton (no excerpts) keeps the
/// editor's tree-sitter syntax overlay active.
fn build_pane_editor(
    buffer: Entity<Buffer>,
    fillers: Vec<BlockProperties>,
    executor: Executor,
    cx: &mut Context<'_, ReviewItem>,
) -> (Entity<MultiBuffer>, Entity<Editor>) {
    let multi_buffer = {
        let buffer = buffer.clone();
        cx.new(|cx| MultiBuffer::singleton(buffer, cx))
    };
    let display_map = {
        let buffer = buffer.clone();
        cx.new(|cx| DisplayMap::new(buffer, executor, cx))
    };
    let diff_map = cx.new(|cx| DiffMap::new(buffer, cx));

    if !fillers.is_empty() {
        display_map.update(cx, |dm, cx| dm.insert_blocks(fillers, cx));
    }

    let editor = cx.new(|cx| {
        Editor::new(
            multi_buffer.clone(),
            display_map,
            diff_map,
            EditorMode::full(),
            cx,
        )
    });
    (multi_buffer, editor)
}

/// Which diff highlight a span carries. The theme scope is resolved from this
/// plus the chunk's staged state by [`decoration_style`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DiffSpan {
    Added,
    Deleted,
    Moved,
}

/// One decoration span: an absolute byte range into a side's full text, the
/// highlight it carries, and whether its chunk is staged.
type DecorationSpan = (Range<usize>, DiffSpan, bool);

/// Decoration spans for one file's left (base) and right (current) panes,
/// walked from the file's chunks' per-side change/move spans. A line's change
/// spans are relative to the line text; the absolute byte range is the line's
/// start offset in the full side text plus the span.
fn file_decoration_spans(
    file: &ReviewFile,
    chunks: &HashMap<ReviewChunkId, ReviewChunk>,
) -> (Vec<DecorationSpan>, Vec<DecorationSpan>) {
    let base_starts = line_start_offsets(&file.base_text);
    let buffer_starts = line_start_offsets(&file.buffer_text);
    let mut left = Vec::new();
    let mut right = Vec::new();
    for chunk_id in &file.chunks {
        let Some(chunk) = chunks.get(chunk_id) else {
            continue;
        };
        let staged = matches!(chunk.status, ChunkStatus::Staged);
        for row in &chunk.hunk.rows {
            let (l, r) = match row {
                ReviewRow::Context { left, right } => (Some(left), Some(right)),
                ReviewRow::Changed { left, right } => (left.as_ref(), right.as_ref()),
            };
            if let Some(side) = l {
                push_side_spans(side, &base_starts, DiffSpan::Deleted, staged, &mut left);
            }
            if let Some(side) = r {
                push_side_spans(side, &buffer_starts, DiffSpan::Added, staged, &mut right);
            }
        }
    }
    (left, right)
}

/// Append one side's change spans (as `change_kind`) and move spans (as
/// [`DiffSpan::Moved`]) to `out`, mapping each line-relative span to an
/// absolute byte range via `line_starts`. [`ReviewSide::line_num`] is 1-based.
fn push_side_spans(
    side: &ReviewSide,
    line_starts: &[usize],
    change_kind: DiffSpan,
    staged: bool,
    out: &mut Vec<DecorationSpan>,
) {
    let Some(&start) = line_starts.get(side.line_num.saturating_sub(1) as usize) else {
        return;
    };
    for span in &side.change_spans {
        out.push((start + span.start..start + span.end, change_kind, staged));
    }
    for span in &side.moved_spans {
        out.push((
            start + span.start..start + span.end,
            DiffSpan::Moved,
            staged,
        ));
    }
}

/// Byte offset of the start of each 0-based line in `text`.
fn line_start_offsets(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// Resolve a span's theme scope to a [`HighlightStyle`]. Staged change spans
/// use the `diff.staged_*` scope variants so staged hunks read distinctly;
/// move spans always use `diff.moved`.
fn decoration_style(cx: &App, span: DiffSpan, staged: bool) -> HighlightStyle {
    use stoat::theme::scope;
    let scope = match (span, staged) {
        (DiffSpan::Added, false) => scope::DIFF_ADDED,
        (DiffSpan::Added, true) => scope::DIFF_STAGED_ADDED,
        (DiffSpan::Deleted, false) => scope::DIFF_DELETED,
        (DiffSpan::Deleted, true) => scope::DIFF_STAGED_DELETED,
        (DiffSpan::Moved, _) => scope::DIFF_MOVED,
    };
    let foreground = cx
        .try_global::<Theme>()
        .and_then(|t| t.0.try_get(scope))
        .and_then(|style| style.fg);
    HighlightStyle {
        foreground,
        ..Default::default()
    }
}

/// Anchor `spans` against `buffer`'s snapshot and install them as decoration
/// highlights on `editor`'s display map, clearing the set when empty. Mirrors
/// the conflict view's decoration refresh.
fn apply_pane_decorations(
    buffer: &Entity<Buffer>,
    editor: &Entity<Editor>,
    spans: &[DecorationSpan],
    cx: &mut Context<'_, ReviewItem>,
) {
    let buffer_id = buffer.read(cx).read(|b| b.buffer_id());
    let display_map = editor.read(cx).display_map().clone();
    if spans.is_empty() {
        display_map.update(cx, |dm, cx| dm.clear_decoration_highlights(buffer_id, cx));
        return;
    }
    let styles: Vec<HighlightStyle> = spans
        .iter()
        .map(|(_, kind, staged)| decoration_style(cx, *kind, *staged))
        .collect();
    let decorations: Vec<DecorationHighlight> = display_map.update(cx, |dm, _| {
        let snap = dm.snapshot();
        let buffer_snap = snap.buffer_snapshot();
        spans
            .iter()
            .zip(styles)
            .map(|((range, _, _), style)| {
                let start = buffer_snap.anchor_at(range.start, Bias::Right);
                let end = buffer_snap.anchor_at(range.end, Bias::Left);
                DecorationHighlight {
                    range: start..end,
                    style,
                }
            })
            .collect()
    });
    let decorations: Arc<[DecorationHighlight]> = Arc::from(decorations);
    display_map.update(cx, |dm, cx| {
        dm.set_decoration_highlights(buffer_id, decorations, cx)
    });
}

impl Render for ReviewItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let active = self.active_file_index(cx);
        let position = commit_position_label(self.session.read(cx).inner());
        let children: Vec<_> = self
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let dimmed = active.is_some_and(|a| a != index);
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .opacity(if dimmed { 0.6 } else { 1.0 })
                    .child(div().px_2().child(SharedString::from(file.header.clone())))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .child(div().flex_1().child(file.left_editor.clone()))
                            .child(div().flex_1().child(file.editor.clone())),
                    )
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .size_full()
            .children(position.map(|label| div().px_2().py_1().child(SharedString::from(label))))
            .children(children)
    }
}

/// Re-scannable subset of [`ReviewSource`] persisted in the workspace
/// blob. The ephemeral sources (agent edits, in-memory) cannot be
/// reconstructed across a restart and are not represented.
#[derive(Serialize, Deserialize)]
pub(crate) enum ReviewSourcePersist {
    WorkingTree {
        workdir: PathBuf,
    },
    WorkingTreeUnstaged {
        workdir: PathBuf,
    },
    WorkingTreeStaged {
        workdir: PathBuf,
    },
    WorkspaceWatch {
        workdir: PathBuf,
    },
    Commit {
        workdir: PathBuf,
        sha: String,
    },
    CommitRange {
        workdir: PathBuf,
        from: String,
        to: String,
    },
    Branch {
        workdir: PathBuf,
        base: Option<String>,
    },
}

/// Persisted review state: the source to re-scan plus the per-chunk
/// decisions keyed by [`ChunkFingerprint`] so they re-key onto freshly
/// scanned chunks on restore. Decisions are stored as `Vec` because a JSON
/// map cannot have a non-string key.
#[derive(Serialize, Deserialize)]
pub(crate) struct ReviewPersist {
    pub(crate) source: ReviewSourcePersist,
    pub(crate) statuses: Vec<(ChunkFingerprint, ChunkStatus)>,
    pub(crate) approvals: Vec<ChunkFingerprint>,
}

/// Re-scannable sources map to `Some`; ephemeral sources to `None`.
fn source_to_persist(source: &ReviewSource) -> Option<ReviewSourcePersist> {
    Some(match source {
        ReviewSource::WorkingTree { workdir } => ReviewSourcePersist::WorkingTree {
            workdir: workdir.clone(),
        },
        ReviewSource::WorkingTreeUnstaged { workdir } => ReviewSourcePersist::WorkingTreeUnstaged {
            workdir: workdir.clone(),
        },
        ReviewSource::WorkingTreeStaged { workdir } => ReviewSourcePersist::WorkingTreeStaged {
            workdir: workdir.clone(),
        },
        ReviewSource::WorkspaceWatch { workdir } => ReviewSourcePersist::WorkspaceWatch {
            workdir: workdir.clone(),
        },
        ReviewSource::Commit { workdir, sha } => ReviewSourcePersist::Commit {
            workdir: workdir.clone(),
            sha: sha.clone(),
        },
        ReviewSource::CommitRange { workdir, from, to } => ReviewSourcePersist::CommitRange {
            workdir: workdir.clone(),
            from: from.clone(),
            to: to.clone(),
        },
        ReviewSource::Branch { workdir, base } => ReviewSourcePersist::Branch {
            workdir: workdir.clone(),
            base: base.clone(),
        },
        ReviewSource::AgentEdits { .. } | ReviewSource::InMemory { .. } => return None,
    })
}

/// Reconstruct a re-scannable [`ReviewSource`] from its persisted form.
/// Inverse of [`source_to_persist`].
pub(crate) fn source_from_persist(persist: ReviewSourcePersist) -> ReviewSource {
    match persist {
        ReviewSourcePersist::WorkingTree { workdir } => ReviewSource::WorkingTree { workdir },
        ReviewSourcePersist::WorkingTreeUnstaged { workdir } => {
            ReviewSource::WorkingTreeUnstaged { workdir }
        },
        ReviewSourcePersist::WorkingTreeStaged { workdir } => {
            ReviewSource::WorkingTreeStaged { workdir }
        },
        ReviewSourcePersist::WorkspaceWatch { workdir } => ReviewSource::WorkspaceWatch { workdir },
        ReviewSourcePersist::Commit { workdir, sha } => ReviewSource::Commit { workdir, sha },
        ReviewSourcePersist::CommitRange { workdir, from, to } => {
            ReviewSource::CommitRange { workdir, from, to }
        },
        ReviewSourcePersist::Branch { workdir, base } => ReviewSource::Branch { workdir, base },
    }
}

/// Parse a persisted [`ReviewPersist`] out of a pane item's serialized
/// blob, returning `None` when the blob is absent or its shape does not
/// match (e.g. the `Value::Null` an ephemeral source serializes to).
pub(crate) fn review_persist_from_blob(blob: &Value) -> Option<ReviewPersist> {
    serde_json::from_value(blob.clone()).ok()
}

impl ItemView for ReviewItem {
    fn tab_label(&self, cx: &App) -> SharedString {
        review_source_label(
            &self.session.read(cx).inner().source,
            self.commit_summary.as_deref(),
        )
        .into()
    }

    fn serialize(&self, cx: &App) -> Value {
        let session = self.session.read(cx);
        let inner = session.inner();
        let Some(source) = source_to_persist(&inner.source) else {
            return Value::Null;
        };
        let persist = ReviewPersist {
            source,
            statuses: inner.snapshot_statuses().into_iter().collect(),
            approvals: inner.snapshot_approvals().into_keys().collect(),
        };
        serde_json::to_value(persist).unwrap_or(Value::Null)
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError> {
        DeserializeSnafu {
            reason: "ReviewItem deserialize requires workspace-persistence wiring \
                     that has not yet landed",
        }
        .fail()
    }

    fn item_kind(&self) -> crate::item::ItemKind {
        crate::item::ItemKind::Review
    }
}

fn review_source_label(source: &ReviewSource, commit_summary: Option<&str>) -> String {
    match source {
        ReviewSource::WorkingTree { workdir } => {
            let name = workdir
                .file_name()
                .and_then(|n| n.to_str())
                .map(String::from)
                .unwrap_or_else(|| workdir.display().to_string());
            format!("Review: {name}")
        },
        ReviewSource::WorkingTreeUnstaged { workdir } => {
            let name = workdir
                .file_name()
                .and_then(|n| n.to_str())
                .map(String::from)
                .unwrap_or_else(|| workdir.display().to_string());
            format!("Review (unstaged): {name}")
        },
        ReviewSource::WorkingTreeStaged { workdir } => {
            let name = workdir
                .file_name()
                .and_then(|n| n.to_str())
                .map(String::from)
                .unwrap_or_else(|| workdir.display().to_string());
            format!("Review (staged): {name}")
        },
        ReviewSource::WorkspaceWatch { workdir } => {
            let name = workdir
                .file_name()
                .and_then(|n| n.to_str())
                .map(String::from)
                .unwrap_or_else(|| workdir.display().to_string());
            format!("Watch: {name}")
        },
        ReviewSource::Commit { sha, .. } => match commit_summary {
            Some(summary) => format!("Commit: {}: {}", short_sha(sha), summary),
            None => format!("Commit: {}", short_sha(sha)),
        },
        ReviewSource::CommitRange { from, to, .. } => {
            format!("Range: {}..{}", short_sha(from), short_sha(to))
        },
        ReviewSource::Branch { base, .. } => match base {
            Some(base) => format!("Branch: {base}"),
            None => String::from("Branch review"),
        },
        ReviewSource::AgentEdits { .. } => String::from("Agent edits"),
        ReviewSource::InMemory { .. } => String::from("Review: in-memory"),
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat::review::{ReviewFileInput, ReviewSide};
    use stoat_scheduler::TestScheduler;

    fn install_executor(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
        });
    }

    fn make_session(cx: &mut TestAppContext, source: ReviewSource) -> Entity<ReviewSession> {
        cx.update(|cx| cx.new(|_| ReviewSession::new(InnerSession::new(source))))
    }

    fn session_with_file(
        cx: &mut TestAppContext,
        source: ReviewSource,
        path: &str,
        base_text: &str,
        buffer_text: &str,
    ) -> Entity<ReviewSession> {
        cx.update(|cx| {
            cx.new(|_| {
                let mut inner = InnerSession::new(source);
                inner.add_files(vec![ReviewFileInput {
                    path: PathBuf::from(path),
                    rel_path: path.to_string(),
                    language: None,
                    base_text: Arc::new(base_text.to_string()),
                    buffer_text: Arc::new(buffer_text.to_string()),
                }]);
                ReviewSession::new(inner)
            })
        })
    }

    fn input(name: &str) -> ReviewFileInput {
        ReviewFileInput {
            path: PathBuf::from(name),
            rel_path: name.to_string(),
            language: None,
            base_text: Arc::new("a\n".to_string()),
            buffer_text: Arc::new("B\n".to_string()),
        }
    }

    #[test]
    fn commit_headers_label_group_boundaries() {
        let mut s = InnerSession::new(ReviewSource::InMemory {
            files: Arc::new(Vec::new()),
        });
        s.set_commit_summary("c1".into(), "first change".into());
        s.set_commit_summary("c2".into(), "second change".into());
        let g1 = s.add_commit_files("c1".into(), vec![input("a.txt"), input("b.txt")]);
        s.add_commit_files("c2".into(), vec![input("c.txt")]);
        s.set_approved(g1.into_iter().flatten().next().unwrap(), true);

        assert_eq!(
            commit_headers(&s),
            vec![
                Some("c1  first change  1/2 reviewed".to_string()),
                None,
                Some("c2  second change  0/1 reviewed".to_string()),
            ]
        );
    }

    #[test]
    fn commit_position_label_only_for_multi_commit() {
        let mut multi = InnerSession::new(ReviewSource::InMemory {
            files: Arc::new(Vec::new()),
        });
        multi.add_commit_files("c1".into(), vec![input("a.txt")]);
        multi.add_commit_files("c2".into(), vec![input("b.txt")]);
        assert_eq!(
            commit_position_label(&multi),
            Some("Commit 1/2".to_string())
        );

        let mut single = InnerSession::new(ReviewSource::InMemory {
            files: Arc::new(Vec::new()),
        });
        single.add_commit_files("c1".into(), vec![input("a.txt")]);
        assert_eq!(commit_position_label(&single), None);
    }

    #[test]
    fn serialize_persists_rescannable_source_and_decisions() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = cx.update(|cx| {
            cx.new(|_| {
                let mut inner = InnerSession::new(ReviewSource::Commit {
                    workdir: PathBuf::from("/repo"),
                    sha: "abc1234".to_string(),
                });
                let ids = inner.add_files(vec![input("a.txt")]);
                let id = ids[0][0];
                inner.set_status(id, ChunkStatus::Staged);
                inner.set_approved(id, true);
                ReviewSession::new(inner)
            })
        });
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));
        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        let blob = item.read_with(&cx, |item, app| item.serialize(app));
        let persist: ReviewPersist = serde_json::from_value(blob).expect("blob parses");

        match persist.source {
            ReviewSourcePersist::Commit { workdir, sha } => {
                assert_eq!(workdir, PathBuf::from("/repo"));
                assert_eq!(sha, "abc1234");
            },
            _ => panic!("expected Commit source"),
        }
        assert_eq!(persist.statuses.len(), 1, "staged chunk persists a status");
        assert_eq!(persist.approvals.len(), 1, "approved chunk persists");
    }

    #[test]
    fn serialize_is_null_for_ephemeral_source() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = cx.update(|cx| {
            cx.new(|_| {
                let mut inner = InnerSession::new(ReviewSource::InMemory {
                    files: Arc::new(Vec::new()),
                });
                inner.add_files(vec![input("a.txt")]);
                ReviewSession::new(inner)
            })
        });
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));
        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });
        let blob = item.read_with(&cx, |item, app| item.serialize(app));
        assert_eq!(blob, Value::Null);
    }

    #[test]
    fn persist_round_trips_source_through_blob() {
        let original = ReviewSource::CommitRange {
            workdir: PathBuf::from("/repo"),
            from: "1111111".to_string(),
            to: "2222222".to_string(),
        };
        let blob = serde_json::to_value(ReviewPersist {
            source: source_to_persist(&original).expect("rescannable source persists"),
            statuses: Vec::new(),
            approvals: Vec::new(),
        })
        .expect("persist serializes");

        let restored = review_persist_from_blob(&blob).expect("blob parses");
        match source_from_persist(restored.source) {
            ReviewSource::CommitRange { workdir, from, to } => {
                assert_eq!(workdir, PathBuf::from("/repo"));
                assert_eq!(from, "1111111");
                assert_eq!(to, "2222222");
            },
            other => panic!("expected CommitRange, got {other:?}"),
        }
    }

    #[test]
    fn review_persist_from_blob_is_none_for_null() {
        assert!(review_persist_from_blob(&Value::Null).is_none());
    }

    fn new_item(cx: &mut TestAppContext, source: ReviewSource) -> Entity<ReviewItem> {
        cx.update(|cx| {
            let session = cx.new(|_| ReviewSession::new(InnerSession::new(source)));
            cx.new(|_| ReviewItem::new(session, Vec::new()))
        })
    }

    #[test]
    fn tab_label_reflects_working_tree_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::WorkingTree {
                workdir: PathBuf::from("/repos/stoat"),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Review: stoat"));
        });
    }

    #[test]
    fn tab_label_reflects_commit_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::Commit {
                workdir: PathBuf::from("/repos/stoat"),
                sha: "abcdef1234567890".to_string(),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Commit: abcdef1"));
        });
    }

    #[test]
    fn tab_label_reflects_commit_source_with_summary() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::Commit {
                workdir: PathBuf::from("/repos/stoat"),
                sha: "abcdef1234567890".to_string(),
            },
        );
        item.update(&mut cx, |item, cx| {
            item.set_commit_summary(Some("fix the thing".to_string()), cx);
        });
        item.read_with(&cx, |item, app| {
            assert_eq!(
                item.tab_label(app),
                SharedString::from("Commit: abcdef1: fix the thing"),
            );
        });
    }

    #[test]
    fn set_commit_summary_clears_when_passed_none() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::Commit {
                workdir: PathBuf::from("/repos/stoat"),
                sha: "abc".to_string(),
            },
        );
        item.update(&mut cx, |item, cx| {
            item.set_commit_summary(Some("first".to_string()), cx);
            item.set_commit_summary(None, cx);
        });
        item.read_with(&cx, |item, _| {
            assert_eq!(item.commit_summary(), None);
        });
    }

    #[test]
    fn tab_label_reflects_commit_range_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::CommitRange {
                workdir: PathBuf::from("/repos/stoat"),
                from: "1111111aaaa".to_string(),
                to: "2222222bbbb".to_string(),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(
                item.tab_label(app),
                SharedString::from("Range: 1111111..2222222")
            );
        });
    }

    #[test]
    fn tab_label_reflects_agent_edits_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::AgentEdits {
                edits: Arc::new(Vec::new()),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Agent edits"));
        });
    }

    #[test]
    fn tab_label_reflects_in_memory_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Review: in-memory"));
        });
    }

    #[test]
    fn is_dirty_is_false() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        item.read_with(&cx, |item, app| {
            assert!(!item.is_dirty(app));
        });
    }

    #[test]
    fn deserialize_returns_error_until_persistence_wires_through() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        let err = item.update(&mut cx, |_, cx| {
            ReviewItem::deserialize(Value::Null, cx).err()
        });
        assert!(matches!(err, Some(ItemError::Deserialize { .. })));
    }

    #[test]
    fn from_session_with_empty_session_creates_empty_files_list() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = make_session(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        item.read_with(&cx, |item, _| {
            assert!(item.files().is_empty());
        });
    }

    #[test]
    fn from_session_with_in_memory_source_creates_one_view_per_file() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "alpha\nbeta\n",
            "alpha modified\nbeta\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        item.read_with(&cx, |item, _| {
            assert_eq!(item.files().len(), 1);
            assert_eq!(item.files()[0].rel_path, "a.txt");
        });
        registry.read_with(&cx, |r, _| {
            assert_eq!(r.len(), 0, "in-memory source must not register buffers");
        });
    }

    #[test]
    fn from_session_with_working_tree_source_registers_buffer() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::WorkingTree {
                workdir: PathBuf::from("/repos/stoat"),
            },
            "/repos/stoat/a.txt",
            "alpha\n",
            "alpha modified\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let _item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        registry.read_with(&cx, |r, _| {
            assert_eq!(r.len(), 1);
            assert!(r
                .id_for_path(&PathBuf::from("/repos/stoat/a.txt"))
                .is_some());
        });
    }

    #[test]
    fn from_session_builds_singleton_panes_per_file() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "alpha\nbeta\ngamma\n",
            "alpha modified\nbeta\ngamma\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        item.read_with(&cx, |item, cx| {
            let view = &item.files()[0];
            assert!(
                view.multi_buffer.read(cx).is_singleton(),
                "right pane is a singleton so the tree-sitter overlay stays active",
            );
            assert_ne!(
                view.buffer.entity_id(),
                view.left_buffer.entity_id(),
                "the left/base pane is a distinct buffer from the right/on-disk pane",
            );
        });
    }

    #[test]
    fn from_session_aligns_left_and_right_panes() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "a\nb\nc\n",
            "a\nB\nc\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        let (left_dm, right_dm) = item.read_with(&cx, |item, cx| {
            let view = &item.files()[0];
            (
                view.left_editor.read(cx).display_map().clone(),
                view.editor.read(cx).display_map().clone(),
            )
        });
        let left_rows = left_dm.update(&mut cx, |dm, _| dm.snapshot().max_point().row);
        let right_rows = right_dm.update(&mut cx, |dm, _| dm.snapshot().max_point().row);

        assert_eq!(
            left_rows, right_rows,
            "spacer fillers keep the base and current panes line-for-line aligned",
        );
    }

    #[test]
    fn from_session_sets_file_header() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "",
            "alpha\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        item.read_with(&cx, |item, _| {
            let header = &item.files()[0].header;
            assert!(
                header.contains("a.txt") && header.contains("staged"),
                "file header names the path and staged count: {header:?}",
            );
        });
    }

    #[test]
    fn from_session_links_left_and_right_pane_scrolling() {
        use gpui::{px, size, Modifiers, Point, ScrollDelta, ScrollWheelEvent, TouchPhase};
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let tall: String = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            &tall,
            &tall.replace("line 5", "LINE 5"),
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));
        let vcx = cx.add_empty_window();
        let item = {
            let session = session.clone();
            let registry = registry.clone();
            vcx.update(|_, cx| cx.new(|cx| ReviewItem::from_session(session, &registry, cx)))
        };
        let (right, left) = item.read_with(vcx, |item, _| {
            let view = &item.files()[0];
            (view.editor.clone(), view.left_editor.clone())
        });
        let cell = size(px(8.0), px(16.0));
        right.update_in(vcx, |ed, _, cx| ed.set_cell_size(cell, cx));
        left.update_in(vcx, |ed, _, cx| ed.set_cell_size(cell, cx));
        vcx.run_until_parked();

        right.update_in(vcx, |ed, window, cx| {
            ed.handle_scroll_wheel(
                &ScrollWheelEvent {
                    position: Point::new(px(0.), px(0.)),
                    delta: ScrollDelta::Lines(Point::new(0., -4.)),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Moved,
                },
                window,
                cx,
            );
        });
        vcx.run_until_parked();

        let right_row = right.read_with(vcx, |ed, _| ed.scroll_row());
        assert_eq!(right_row, 4, "the right pane scrolled");
        assert_eq!(
            left.read_with(vcx, |ed, _| ed.scroll_row()),
            right_row,
            "the left pane mirrors the right pane's scroll",
        );
    }

    #[test]
    fn line_start_offsets_marks_each_line() {
        assert_eq!(line_start_offsets("ab\ncd\n"), vec![0, 3, 6]);
        assert_eq!(line_start_offsets("solo"), vec![0]);
    }

    #[test]
    fn push_side_spans_maps_change_and_move_spans_to_absolute_bytes() {
        let side = ReviewSide {
            text: "hello world".to_string(),
            line_num: 2,
            change_spans: std::iter::once(6..11).collect(),
            moved_spans: std::iter::once(0..5).collect(),
            move_provenance: None,
        };
        let mut out = Vec::new();
        push_side_spans(&side, &[0, 6, 18], DiffSpan::Deleted, true, &mut out);
        assert_eq!(
            out,
            vec![
                (12..17, DiffSpan::Deleted, true),
                (6..11, DiffSpan::Moved, true),
            ],
            "line 2 starts at byte 6, so spans shift by 6 and keep their kind/staged",
        );
    }

    #[test]
    fn file_decoration_spans_colors_each_side_by_change() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "hello world\n",
            "hello WORLD\n",
        );
        session.read_with(&cx, |s, _| {
            let inner = s.inner();
            let (left, right) = file_decoration_spans(&inner.files[0], &inner.chunks);
            assert!(
                !right.is_empty() && right.iter().all(|(_, kind, _)| *kind == DiffSpan::Added),
                "right pane carries added-change spans: {right:?}",
            );
            assert!(
                !left.is_empty() && left.iter().all(|(_, kind, _)| *kind == DiffSpan::Deleted),
                "left pane carries deleted-change spans: {left:?}",
            );
            assert!(
                right.iter().all(|(_, _, staged)| !staged),
                "a pending chunk is not staged",
            );
        });
    }

    #[test]
    fn file_decoration_spans_marks_staged_chunks() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "hello world\n",
            "hello WORLD\n",
        );
        let id = session.read_with(&cx, |s, _| s.inner().order[0]);
        session.update(&mut cx, |s, cx| s.set_status(id, ChunkStatus::Staged, cx));
        session.read_with(&cx, |s, _| {
            let inner = s.inner();
            let (_, right) = file_decoration_spans(&inner.files[0], &inner.chunks);
            assert!(
                !right.is_empty() && right.iter().all(|(_, _, staged)| *staged),
                "staging the chunk marks its spans staged: {right:?}",
            );
        });
    }

    #[test]
    fn from_session_installs_decorations_on_the_right_pane() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "hello world\n",
            "hello WORLD\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));
        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });
        let dm = item.read_with(&cx, |item, cx| {
            item.files()[0].editor.read(cx).display_map().clone()
        });
        let has_decorations = dm.update(&mut cx, |dm, _| {
            !dm.snapshot().decoration_highlights().is_empty()
        });
        assert!(
            has_decorations,
            "the right pane installs diff decoration highlights on build",
        );
    }

    fn side(text: &str, line_num: u32) -> ReviewSide {
        ReviewSide {
            text: text.to_string(),
            line_num,
            change_spans: Vec::new(),
            moved_spans: Vec::new(),
            move_provenance: None,
        }
    }

    /// `(placement, height)` of each spacer block, the asserted projection of
    /// the opaque [`BlockProperties`].
    type FillerLayout = Vec<(BlockPlacement, Option<u32>)>;

    /// Run [`append_chunk_fillers`] and project each produced block to its
    /// `(placement, height)` so the spacer layout can be asserted without
    /// comparing the opaque render closures.
    fn fillers(
        rows: &[ReviewRow],
        base_start: u32,
        buffer_start: u32,
    ) -> (FillerLayout, FillerLayout) {
        let mut left = Vec::new();
        let mut right = Vec::new();
        append_chunk_fillers(rows, base_start, buffer_start, &mut left, &mut right);
        let project =
            |v: Vec<BlockProperties>| v.into_iter().map(|b| (b.placement, b.height)).collect();
        (project(left), project(right))
    }

    #[test]
    fn fillers_pad_removed_line_on_the_right() {
        let rows = vec![
            ReviewRow::Context {
                left: side("a", 1),
                right: side("a", 1),
            },
            ReviewRow::Changed {
                left: Some(side("b", 2)),
                right: None,
            },
            ReviewRow::Context {
                left: side("c", 3),
                right: side("c", 2),
            },
        ];
        let (left, right) = fillers(&rows, 0, 0);
        assert!(left.is_empty(), "no added lines means no left spacer");
        assert_eq!(
            right,
            vec![(BlockPlacement::Above(1), Some(1))],
            "removed line b spaces the right pane above current line c (row 1)",
        );
    }

    #[test]
    fn fillers_pad_added_line_on_the_left() {
        let rows = vec![
            ReviewRow::Context {
                left: side("a", 1),
                right: side("a", 1),
            },
            ReviewRow::Changed {
                left: None,
                right: Some(side("B", 2)),
            },
            ReviewRow::Context {
                left: side("c", 2),
                right: side("c", 3),
            },
        ];
        let (left, right) = fillers(&rows, 0, 0);
        assert_eq!(
            left,
            vec![(BlockPlacement::Above(1), Some(1))],
            "added line B spaces the left pane above base line c (row 1)",
        );
        assert!(right.is_empty(), "no removed lines means no right spacer");
    }

    #[test]
    fn fillers_interleave_remove_then_add() {
        let rows = vec![
            ReviewRow::Context {
                left: side("a", 1),
                right: side("a", 1),
            },
            ReviewRow::Changed {
                left: Some(side("b", 2)),
                right: None,
            },
            ReviewRow::Changed {
                left: None,
                right: Some(side("B", 2)),
            },
            ReviewRow::Context {
                left: side("c", 3),
                right: side("c", 3),
            },
        ];
        let (left, right) = fillers(&rows, 0, 0);
        assert_eq!(left, vec![(BlockPlacement::Above(2), Some(1))]);
        assert_eq!(right, vec![(BlockPlacement::Above(1), Some(1))]);
    }

    #[test]
    fn fillers_trailing_removed_at_eof() {
        let rows = vec![
            ReviewRow::Context {
                left: side("a", 1),
                right: side("a", 1),
            },
            ReviewRow::Changed {
                left: Some(side("b", 2)),
                right: None,
            },
        ];
        let (left, right) = fillers(&rows, 0, 0);
        assert!(left.is_empty());
        assert_eq!(
            right,
            vec![(BlockPlacement::Below(0), Some(1))],
            "a removal at EOF spaces below the last current line (row 0)",
        );
    }

    #[test]
    fn fillers_pure_addition_anchors_at_buffer_start() {
        let rows = vec![
            ReviewRow::Changed {
                left: None,
                right: Some(side("x", 1)),
            },
            ReviewRow::Changed {
                left: None,
                right: Some(side("y", 2)),
            },
        ];
        let (left, right) = fillers(&rows, 0, 0);
        assert_eq!(
            left,
            vec![(BlockPlacement::Above(0), Some(2))],
            "a pure addition with no base line anchors the left spacer at base_start",
        );
        assert!(right.is_empty());
    }

    #[test]
    fn active_file_index_returns_none_when_no_cursor() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.active_file_index(app), None);
        });
    }

    #[test]
    fn active_file_index_tracks_session_cursor() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = cx.update(|cx| {
            cx.new(|_| {
                let mut inner = InnerSession::new(ReviewSource::InMemory {
                    files: Arc::new(Vec::new()),
                });
                inner.add_files(vec![
                    ReviewFileInput {
                        path: PathBuf::from("a.txt"),
                        rel_path: "a.txt".to_string(),
                        language: None,
                        base_text: Arc::new("a\n".to_string()),
                        buffer_text: Arc::new("aa\n".to_string()),
                    },
                    ReviewFileInput {
                        path: PathBuf::from("b.txt"),
                        rel_path: "b.txt".to_string(),
                        language: None,
                        base_text: Arc::new("b\n".to_string()),
                        buffer_text: Arc::new("bb\n".to_string()),
                    },
                ]);
                ReviewSession::new(inner)
            })
        });
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        item.read_with(&cx, |item, app| {
            assert_eq!(
                item.active_file_index(app),
                Some(0),
                "first chunk is in file 0; cursor defaults to first chunk on add_files",
            );
        });

        // Advance the cursor and assert it moves to the next chunk.
        session.update(&mut cx, |s, cx| {
            s.next(cx);
        });
        cx.run_until_parked();

        item.read_with(&cx, |item, app| {
            assert_eq!(item.active_file_index(app), Some(1));
        });
    }

    #[test]
    fn from_session_attaches_session_and_file_index_to_each_editor() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = cx.update(|cx| {
            cx.new(|_| {
                let mut inner = InnerSession::new(ReviewSource::InMemory {
                    files: Arc::new(Vec::new()),
                });
                inner.add_files(vec![
                    ReviewFileInput {
                        path: PathBuf::from("a.txt"),
                        rel_path: "a.txt".to_string(),
                        language: None,
                        base_text: Arc::new("a\n".to_string()),
                        buffer_text: Arc::new("aa\n".to_string()),
                    },
                    ReviewFileInput {
                        path: PathBuf::from("b.txt"),
                        rel_path: "b.txt".to_string(),
                        language: None,
                        base_text: Arc::new("b\n".to_string()),
                        buffer_text: Arc::new("bb\n".to_string()),
                    },
                ]);
                ReviewSession::new(inner)
            })
        });
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        item.read_with(&cx, |item, cx| {
            let session_id = item.session().entity_id();
            for (expected_index, file) in item.files().iter().enumerate() {
                let editor = file.editor.read(cx);
                assert_eq!(
                    editor.review_file_index(),
                    Some(expected_index),
                    "editor for file {expected_index} should know its index",
                );
                assert_eq!(
                    editor.review_session().map(|s| s.entity_id()),
                    Some(session_id),
                    "editor for file {expected_index} should share the same session",
                );
            }
        });
    }

    #[test]
    fn rebuild_files_updates_file_views_after_refresh() {
        use stoat::review::ReviewFileInput as Input;
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "alpha\nbeta\n",
            "alpha modified\nbeta\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });
        item.read_with(&cx, |item, _| assert_eq!(item.files().len(), 1));

        session.update(&mut cx, |s, cx| {
            s.refresh_files(
                vec![
                    Input {
                        path: PathBuf::from("a.txt"),
                        rel_path: "a.txt".to_string(),
                        language: None,
                        base_text: Arc::new("alpha\nbeta\n".to_string()),
                        buffer_text: Arc::new("alpha modified\nbeta\n".to_string()),
                    },
                    Input {
                        path: PathBuf::from("b.txt"),
                        rel_path: "b.txt".to_string(),
                        language: None,
                        base_text: Arc::new("".to_string()),
                        buffer_text: Arc::new("brand new\n".to_string()),
                    },
                ],
                cx,
            );
        });
        cx.run_until_parked();

        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.files().len(),
                2,
                "Refreshed event must rebuild file views to reflect the new file count",
            );
            assert_eq!(item.files()[0].rel_path, "a.txt");
            assert_eq!(item.files()[1].rel_path, "b.txt");
        });
    }
}
