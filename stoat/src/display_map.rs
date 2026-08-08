mod block_map;
mod crease_map;
mod fold_map;
pub mod highlights;
pub mod inlay_map;
pub mod invisibles;
pub mod syntax_theme;
pub mod tab_map;
mod wrap_map;

use crate::{
    buffer::BufferId,
    diff_map::{DiffHunkStatus, DiffMap, TokenDetail},
    host::DiffStatus,
    multi_buffer::{ExcerptId, MultiBuffer, MultiBufferSnapshot},
};
pub use block_map::{
    balancing_block, Block, BlockContext, BlockId, BlockMap, BlockPlacement, BlockPoint,
    BlockProperties, BlockRow, BlockRowKind, BlockSnapshot, BlockStyle, CompanionView, CustomBlock,
    CustomBlockId, RenderBlock,
};
pub use crease_map::{
    Crease, CreaseId, CreaseMap, CreaseMetadata, CreaseSnapshot, RenderToggleFn, RenderTrailerFn,
};
pub use fold_map::{FoldMap, FoldMetadata, FoldOffset, FoldPlaceholder, FoldPoint, FoldSnapshot};
use highlights::{prefix_max_end_indices, AnchorResolver};
pub use highlights::{
    BufferSemanticTokens, CachedHighlightEndpoints, Chunk, ChunkRenderer, ChunkRendererId,
    ChunkReplacement, HighlightKey, HighlightLayer, HighlightStyle, HighlightStyleId,
    HighlightStyleInterner, HighlightedChunk, Highlights, InlayHighlight, InlayHighlights,
    SemanticTokenHighlight, SemanticTokenSpans, SemanticTokensHighlights, TextHighlights,
};
pub use inlay_map::{InlayId, InlayKind, InlayMap, InlayOffset, InlayPoint, InlaySnapshot};
use std::{
    collections::{BTreeMap, HashMap},
    mem,
    ops::Range,
    sync::{
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        Arc, LazyLock,
    },
};
use stoat_scheduler::Executor;
use stoat_text::{patch::Patch, Anchor, Bias, CharsAt, Point, ReversedCharsAt, Rope};
pub use tab_map::{TabMap, TabPoint, TabRow, TabSnapshot};
use tokio::sync::Notify;
use unicode_width::UnicodeWidthChar;
pub use wrap_map::{WrapMap, WrapPoint, WrapSnapshot};

/// Shared empty text-highlight map, used as the `unwrap_or` fallback when an
/// endpoint build carries no text highlights. Every live caller passes its own
/// highlights, so this only spares the per-frame chunk path a throwaway
/// `Arc<HashMap>` allocation on the rare `None` case.
static EMPTY_TEXT_HIGHLIGHTS: LazyLock<TextHighlights> = LazyLock::new(|| Arc::new(HashMap::new()));

pub(crate) fn display_width(ch: char) -> u32 {
    ch.width().unwrap_or(0) as u32
}

/// Restate `edits` in buffer rows, reading each side against the text that side
/// indexes into.
///
/// Each range widens to the rows it touches, taking the row holding the last
/// affected byte and adding one. That is the convention the inlay layer's row
/// patch uses, so the two descriptions of one edit agree on how far the rows
/// below it moved.
fn buffer_row_patch(
    edits: &Patch<usize>,
    before: &MultiBufferSnapshot,
    after: &MultiBufferSnapshot,
) -> Patch<u32> {
    let mut patch = Patch::empty();
    for edit in edits {
        let old_rope = before.rope();
        let new_rope = after.rope();
        patch.push(stoat_text::patch::Edit {
            old: old_rope.offset_to_point(edit.old.start).row
                ..old_rope.offset_to_point(edit.old.end).row + 1,
            new: new_rope.offset_to_point(edit.new.start).row
                ..new_rope.offset_to_point(edit.new.end).row + 1,
        });
    }
    patch
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DisplayPoint {
    pub row: u32,
    pub column: u32,
}

impl DisplayPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self { row, column }
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DisplayRow(pub u32);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct DisplayMapId(u64);

static NEXT_DISPLAY_MAP_ID: AtomicU64 = AtomicU64::new(0);

impl DisplayMapId {
    pub fn next() -> Self {
        Self(NEXT_DISPLAY_MAP_ID.fetch_add(1, AtomicOrdering::Relaxed))
    }
}

pub type ConvertMultiBufferRows = fn(
    excerpt_map: &HashMap<ExcerptId, ExcerptId>,
    companion_snapshot: &MultiBufferSnapshot,
    our_snapshot: &MultiBufferSnapshot,
    bounds: (std::ops::Bound<Point>, std::ops::Bound<Point>),
) -> Vec<CompanionExcerptPatch>;

#[derive(Debug)]
pub struct CompanionExcerptPatch {
    pub patch: Patch<Point>,
    pub edited_range: Range<Point>,
    pub source_excerpt_range: Range<Point>,
    pub target_excerpt_range: Range<Point>,
}

#[allow(dead_code)]
pub struct Companion {
    pub(crate) rhs_display_map_id: DisplayMapId,
    pub(crate) rhs_buffer_to_lhs_buffer: HashMap<BufferId, BufferId>,
    pub(crate) lhs_buffer_to_rhs_buffer: HashMap<BufferId, BufferId>,
    pub(crate) rhs_excerpt_to_lhs_excerpt: HashMap<ExcerptId, ExcerptId>,
    pub(crate) lhs_excerpt_to_rhs_excerpt: HashMap<ExcerptId, ExcerptId>,
    pub(crate) rhs_rows_to_lhs_rows: ConvertMultiBufferRows,
    pub(crate) lhs_rows_to_rhs_rows: ConvertMultiBufferRows,
    pub(crate) rhs_custom_block_to_balancing_block: HashMap<CustomBlockId, CustomBlockId>,
    pub(crate) lhs_custom_block_to_balancing_block: HashMap<CustomBlockId, CustomBlockId>,
}

#[allow(dead_code)]
impl Companion {
    fn is_rhs(&self, id: DisplayMapId) -> bool {
        self.rhs_display_map_id == id
    }

    fn excerpt_map(&self, id: DisplayMapId) -> &HashMap<ExcerptId, ExcerptId> {
        if self.is_rhs(id) {
            &self.rhs_excerpt_to_lhs_excerpt
        } else {
            &self.lhs_excerpt_to_rhs_excerpt
        }
    }

    fn rows_to_companion(&self, id: DisplayMapId) -> ConvertMultiBufferRows {
        if self.is_rhs(id) {
            self.rhs_rows_to_lhs_rows
        } else {
            self.lhs_rows_to_rhs_rows
        }
    }

    fn convert_point_from_companion(
        &self,
        display_map_id: DisplayMapId,
        our_snapshot: &MultiBufferSnapshot,
        companion_snapshot: &MultiBufferSnapshot,
        point: Point,
    ) -> Range<Point> {
        let convert_fn = self.rows_to_companion(display_map_id);
        let excerpt_map = self.excerpt_map(display_map_id);
        let patches = convert_fn(
            excerpt_map,
            companion_snapshot,
            our_snapshot,
            (
                std::ops::Bound::Included(point),
                std::ops::Bound::Included(point),
            ),
        );
        match patches.into_iter().next() {
            Some(ep) => {
                for edit in ep.patch.edits() {
                    if edit.old.start <= point && point <= edit.old.end {
                        return edit.new.clone();
                    }
                }
                ep.edited_range
            },
            None => Point::zero()..Point::new(our_snapshot.line_count(), 0),
        }
    }

    pub fn custom_block_to_balancing_block(
        &self,
        id: DisplayMapId,
    ) -> &HashMap<CustomBlockId, CustomBlockId> {
        if self.is_rhs(id) {
            &self.rhs_custom_block_to_balancing_block
        } else {
            &self.lhs_custom_block_to_balancing_block
        }
    }

    pub fn insert_balancing_mapping(
        &mut self,
        id: DisplayMapId,
        source: CustomBlockId,
        balancing: CustomBlockId,
    ) {
        if self.is_rhs(id) {
            self.rhs_custom_block_to_balancing_block
                .insert(source, balancing);
        } else {
            self.lhs_custom_block_to_balancing_block
                .insert(source, balancing);
        }
    }
}

/// Threshold for which diagnostic severities to display.
///
/// Ordered by severity: Error < Warning < Information < Hint.
/// Filtering by "max severity" means: show diagnostics where `severity <= threshold`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiagnosticSeverity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

pub struct DisplayMap {
    id: DisplayMapId,
    multi_buffer: MultiBuffer,
    inlay_map: InlayMap,
    fold_map: FoldMap,
    tab_map: TabMap,
    wrap_map: WrapMap,
    block_map: BlockMap,
    crease_map: CreaseMap,
    text_highlights: TextHighlights,
    semantic_token_highlights: SemanticTokensHighlights,
    lsp_token_highlights: SemanticTokensHighlights,
    inlay_highlights: Arc<InlayHighlights>,
    companion: Option<Companion>,
    lsp_folding_crease_ids: HashMap<BufferId, Vec<CreaseId>>,
    masked: bool,
    /// When false, tree-sitter syntax coloring is suppressed for this
    /// editor. [`Self::highlighted_chunks`] then withholds the semantic-
    /// token highlights that carry it, leaving text and inlay highlights
    /// (search, LSP) unaffected. Defaults to true.
    syntax_highlighting: bool,
    clip_at_line_ends: bool,
    diagnostics_max_severity: Option<DiagnosticSeverity>,
    last_buffer_version: u64,
    /// The buffer as of the last sync, kept so an edit patch of offsets can be
    /// restated in rows.
    ///
    /// An offset's row is only readable from the text that offset indexes into,
    /// and the old side of a patch indexes into the buffer as it was. Block
    /// placements are buffer rows, so without this the block map has no way to
    /// learn how far an edit moved them.
    last_buffer_snapshot: Option<MultiBufferSnapshot>,
    /// Buffer content version the crease map was last resolved against.
    ///
    /// The crease sync in [`Self::snapshot_with_companion`] is skipped while
    /// this matches the live buffer version. Anchor offsets move only on a
    /// buffer edit, and `insert`/`remove` resolve creases eagerly at the
    /// current version, so an unchanged version guarantees every crease is
    /// already resolved and a re-sync would reproduce the same offsets.
    last_crease_sync_version: u64,
    inserted_diff_block_ids: Vec<CustomBlockId>,
    /// The hunks the currently installed deleted-line blocks were built from.
    ///
    /// A diff recompute stamps a new version even when it found exactly the
    /// same hunks, so the version alone cannot say whether the blocks need
    /// replacing. This can, and a refresh that matches it does nothing.
    inserted_diff_block_signature: Vec<(DiffHunkStatus, u32, Range<usize>)>,
    /// Ids of the spacer blocks the conflict view installs to pad a picked
    /// chunk whose center shrank below its taller side, tracked so each refresh
    /// replaces the previous set rather than stacking duplicates.
    conflict_padding_block_ids: Vec<CustomBlockId>,
    last_diff_version: usize,
    /// When false, `Deleted`/`Modified` diff hunks do not splice inline
    /// deleted-line block rows into the display. A plain editor with a populated
    /// diff map still shows gutter indicators via
    /// [`DisplaySnapshot::line_diff_status`] but gains no extra rows. The
    /// side-by-side diff view sets this to render the removed base lines.
    show_deleted_blocks: bool,
    /// The `show_deleted_blocks` value applied at the last block re-splice, so a
    /// mid-session toggle re-splices even when the diff version is unchanged.
    last_show_deleted_blocks: bool,
    cached_snapshot: Option<DisplaySnapshot>,
    /// Set when any highlight collection is mutated. Checked inside
    /// [`DisplayMap::snapshot_with_companion`] so a single rebuild
    /// covers any number of highlight setters fired in the same frame.
    highlights_dirty: bool,
}

impl DisplayMap {
    /// `redraw` is woken when a background rewrap settles. Nothing else marks
    /// that moment, so an editor built with a handle nobody listens to shows
    /// its long lines unwrapped until the next unrelated event.
    pub fn new(multi_buffer: MultiBuffer, executor: Executor, redraw: Arc<Notify>) -> Self {
        let buffer_snapshot = multi_buffer.snapshot();
        let version = buffer_snapshot.version();
        let (inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (fold_map, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).expect("non-zero literal"));
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (wrap_map, _wrap_snapshot) = WrapMap::new(tab_snapshot, None, executor, redraw);
        let block_map = BlockMap::new();

        Self {
            id: DisplayMapId::next(),
            multi_buffer,
            inlay_map,
            fold_map,
            tab_map,
            wrap_map,
            block_map,
            crease_map: CreaseMap::new(),
            text_highlights: Arc::new(HashMap::new()),
            semantic_token_highlights: Arc::new(HashMap::new()),
            lsp_token_highlights: Arc::new(HashMap::new()),
            inlay_highlights: Arc::new(BTreeMap::new()),
            companion: None,
            lsp_folding_crease_ids: HashMap::new(),
            masked: false,
            syntax_highlighting: true,
            clip_at_line_ends: false,
            diagnostics_max_severity: None,
            last_buffer_version: version,
            last_buffer_snapshot: Some(buffer_snapshot),
            last_crease_sync_version: version,
            inserted_diff_block_ids: Vec::new(),
            inserted_diff_block_signature: Vec::new(),
            conflict_padding_block_ids: Vec::new(),
            last_diff_version: 0,
            show_deleted_blocks: false,
            last_show_deleted_blocks: false,
            cached_snapshot: None,
            highlights_dirty: false,
        }
    }

    pub fn id(&self) -> DisplayMapId {
        self.id
    }

    /// Version of the underlying buffer's diff map, or 0 when it has none.
    ///
    /// Cheap (a buffer read, no snapshot), so the smooth-scroll page assembly can
    /// fold it into a diff-view page's content version to refill on hunk changes.
    pub(crate) fn diff_version(&self) -> usize {
        self.multi_buffer.diff_version()
    }

    /// Enable or disable inline deleted-line block rows for this editor's diff
    /// map. Off by default. The side-by-side diff view turns it on. Nulls the
    /// snapshot cache so the next snapshot re-splices under the new setting.
    pub(crate) fn set_show_deleted_blocks(&mut self, show: bool) {
        self.show_deleted_blocks = show;
        self.cached_snapshot = None;
    }

    pub fn folded_buffers(&self) -> &std::collections::HashSet<BufferId> {
        self.block_map.folded_buffers()
    }

    pub fn set_companion(&mut self, companion: Option<Companion>) {
        if companion.is_none() {
            if let Some(old) = self.companion.take() {
                let ids: std::collections::HashSet<CustomBlockId> = old
                    .rhs_custom_block_to_balancing_block
                    .values()
                    .chain(old.lhs_custom_block_to_balancing_block.values())
                    .copied()
                    .collect();
                self.block_map.remove(&ids);
            }
            return;
        }
        self.companion = companion;
        self.block_map.mark_dirty();
    }

    pub fn set_masked(&mut self, masked: bool) {
        self.masked = masked;
    }

    /// Enable or disable tree-sitter syntax coloring for this editor.
    ///
    /// Marks the highlight cache dirty only on a real change, so callers may
    /// invoke it every frame to keep an editor in sync with a session toggle
    /// without forcing a snapshot rebuild each time.
    pub fn set_syntax_highlighting(&mut self, on: bool) {
        if self.syntax_highlighting != on {
            self.syntax_highlighting = on;
            self.highlights_dirty = true;
        }
    }

    pub fn set_clip_at_line_ends(&mut self, clip: bool) {
        self.clip_at_line_ends = clip;
    }

    pub fn set_diagnostics_max_severity(&mut self, severity: Option<DiagnosticSeverity>) {
        self.diagnostics_max_severity = severity;
    }

    pub fn insert_blocks(&mut self, blocks: Vec<BlockProperties>) {
        self.block_map.insert(blocks);
        // A block insert marks the block map dirty but touches no buffer, fold,
        // or inlay version, so the cached snapshot must be dropped explicitly or
        // snapshot_with_companion short-circuits to it and the new blocks stay
        // invisible until an unrelated version bump forces a rebuild.
        self.cached_snapshot = None;
    }

    /// Replace the conflict view's padding spacer blocks with `blocks`.
    ///
    /// Removes the spacers installed by the previous call before inserting the
    /// new set, so a pick that reshapes a chunk refreshes its padding without
    /// stacking stale blocks. Pass an empty vector to clear them.
    pub fn set_conflict_padding_blocks(&mut self, blocks: Vec<BlockProperties>) {
        let stale: std::collections::HashSet<CustomBlockId> =
            self.conflict_padding_block_ids.drain(..).collect();
        self.block_map.remove(&stale);
        self.conflict_padding_block_ids = self.block_map.insert(blocks);
        self.cached_snapshot = None;
    }

    pub fn fold(&mut self, ranges: Vec<Range<Point>>) {
        let buffer_snapshot = self.multi_buffer.snapshot();
        let anchor_ranges = ranges
            .into_iter()
            .map(|r| {
                let start_off = buffer_snapshot.rope().point_to_offset(r.start);
                let end_off = buffer_snapshot.rope().point_to_offset(r.end);
                buffer_snapshot.anchor_at(start_off, Bias::Right)
                    ..buffer_snapshot.anchor_at(end_off, Bias::Left)
            })
            .collect();
        self.fold_map
            .fold(anchor_ranges, FoldPlaceholder::default(), &buffer_snapshot);
    }

    pub fn unfold(&mut self, ranges: Vec<Range<Point>>) {
        let buffer_snapshot = self.multi_buffer.snapshot();
        let offset_ranges = ranges
            .into_iter()
            .map(|r| {
                let start_off = buffer_snapshot.rope().point_to_offset(r.start);
                let end_off = buffer_snapshot.rope().point_to_offset(r.end);
                start_off..end_off
            })
            .collect();
        self.fold_map.unfold(offset_ranges, &buffer_snapshot);
    }

    pub fn toggle_fold(&mut self, ranges: Vec<Range<Point>>) {
        let buffer_snapshot = self.multi_buffer.snapshot();
        let any_folded = ranges.iter().any(|r| {
            let offset = buffer_snapshot.rope().point_to_offset(r.start);
            self.fold_map.is_folded_at_offset(offset, &buffer_snapshot)
        });
        if any_folded {
            self.unfold(ranges);
        } else {
            self.fold(ranges);
        }
    }

    pub fn set_wrap_width(&mut self, width: Option<u32>) {
        // The snapshot fast-path keys only off buffer, fold, and inlay versions,
        // so a wrap-width change with an otherwise-unchanged buffer would be
        // served a stale snapshot. Drop the cache when the width actually moves.
        if self
            .cached_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.wrap_width() != width)
        {
            self.cached_snapshot = None;
        }
        self.wrap_map.set_wrap_width(width);
    }

    /// The wrap width most recently stamped by [`Self::set_wrap_width`], before
    /// the next snapshot applies it. `None` disables wrapping.
    pub fn wrap_width(&self) -> Option<u32> {
        self.wrap_map.wrap_width()
    }

    pub fn highlight_text(
        &mut self,
        key: HighlightKey,
        ranges: Vec<Range<Anchor>>,
        style: HighlightStyle,
    ) {
        let sorted_ranges = {
            let buffer_snapshot = self.multi_buffer.snapshot();
            let starts: Vec<Anchor> = ranges.iter().map(|range| range.start).collect();

            let mut by_start: Vec<(usize, Range<Anchor>)> = buffer_snapshot
                .resolve_anchors_batch(&starts)
                .into_iter()
                .zip(ranges)
                .collect();
            by_start.sort_by_key(|(start, _)| *start);

            by_start.into_iter().map(|(_, range)| range).collect()
        };

        Arc::make_mut(&mut self.text_highlights).insert(key, Arc::new((style, sorted_ranges)));
        self.highlights_dirty = true;
    }

    /// Remove `key`'s ranges, reporting whether anything was there to remove.
    ///
    /// Absent keys cost nothing. Every snapshot holds a clone of the text
    /// highlight map, so taking a mutable borrow of it deep-clones, and the
    /// callers that run per cursor motion mostly have nothing to clear.
    pub fn clear_highlights(&mut self, key: HighlightKey) -> bool {
        let mut cleared = false;
        if self.text_highlights.contains_key(&key) {
            cleared = Arc::make_mut(&mut self.text_highlights)
                .remove(&key)
                .is_some();
        }

        if self.inlay_highlights.contains_key(&key) {
            cleared |= Arc::make_mut(&mut self.inlay_highlights)
                .remove(&key)
                .is_some();
        }

        if cleared {
            self.highlights_dirty = true;
        }
        cleared
    }

    pub fn set_semantic_token_highlights(
        &mut self,
        buffer_id: BufferId,
        tokens: Arc<[SemanticTokenHighlight]>,
        interner: Arc<HighlightStyleInterner>,
    ) {
        let channel = self.batched_token_channel(tokens, interner);
        self.set_semantic_token_channel(buffer_id, channel);
    }

    /// Build a token channel, resolving every token end in one batch.
    ///
    /// The channel's search index is an argmax over resolved ends, so building
    /// it needs each end's offset. Taken one at a time that is a root descent
    /// per token, which is what makes installing a large file's tokens
    /// expensive.
    fn batched_token_channel(
        &self,
        tokens: Arc<[SemanticTokenHighlight]>,
        interner: Arc<HighlightStyleInterner>,
    ) -> BufferSemanticTokens {
        let snapshot = self.multi_buffer.snapshot();
        let ends: Vec<Anchor> = tokens.iter().map(|token| token.range.end).collect();
        let prefix_max_end = prefix_max_end_indices(&snapshot.resolve_anchors_batch(&ends));

        BufferSemanticTokens::with_prefix_max_end(tokens, interner, prefix_max_end)
    }

    /// Install a channel the caller already built.
    ///
    /// The parse pipeline builds one channel per buffer and installs that same
    /// value into every editor viewing it, rather than having each editor
    /// rebuild it from the token list. Building it costs a resolve per token,
    /// so the rebuild was paid once per editor per keystroke.
    pub fn set_semantic_token_channel(
        &mut self,
        buffer_id: BufferId,
        channel: BufferSemanticTokens,
    ) {
        Arc::make_mut(&mut self.semantic_token_highlights).insert(buffer_id, channel);
        self.highlights_dirty = true;
    }

    pub fn invalidate_semantic_highlights(&mut self, buffer_id: BufferId) {
        Arc::make_mut(&mut self.semantic_token_highlights).remove(&buffer_id);
        self.highlights_dirty = true;
    }

    /// Install LSP semantic tokens for `buffer_id`. They render on a higher layer
    /// than the tree-sitter tokens set by [`Self::set_semantic_token_highlights`],
    /// so their styles merge over the syntactic baseline.
    pub fn set_lsp_token_highlights(
        &mut self,
        buffer_id: BufferId,
        tokens: Arc<[SemanticTokenHighlight]>,
        interner: Arc<HighlightStyleInterner>,
    ) {
        let channel = self.batched_token_channel(tokens, interner);
        Arc::make_mut(&mut self.lsp_token_highlights).insert(buffer_id, channel);
        self.highlights_dirty = true;
    }

    /// Re-resolve every retained token channel through `interner`.
    ///
    /// Style ids are stable across themes, so a theme switch changes what a
    /// token paints as without changing which scope it names. This recolors
    /// tokens already on screen with no reparse and no fresh LSP request.
    ///
    /// The channel maps are rebuilt into new [`Arc`]s rather than mutated in
    /// place, because [`CachedHighlightEndpoints`] validates by `Arc` pointer
    /// and bakes a resolved style into each endpoint. `Arc::make_mut` would
    /// only change that pointer while a second reference happens to exist, so
    /// it would leave the cache serving the previous theme's colors whenever it
    /// did not. Allocating unconditionally does not depend on a refcount that
    /// nothing here guarantees.
    pub fn swap_style_interner(&mut self, interner: &Arc<HighlightStyleInterner>) {
        let reintern = |channels: &SemanticTokensHighlights| -> SemanticTokensHighlights {
            Arc::new(
                channels
                    .iter()
                    .map(|(id, channel)| (*id, channel.with_interner(interner.clone())))
                    .collect(),
            )
        };

        self.semantic_token_highlights = reintern(&self.semantic_token_highlights);
        self.lsp_token_highlights = reintern(&self.lsp_token_highlights);
        self.highlights_dirty = true;
    }

    pub fn highlight_inlays(
        &mut self,
        key: HighlightKey,
        highlights: Vec<InlayHighlight>,
        style: HighlightStyle,
    ) {
        let entry = Arc::make_mut(&mut self.inlay_highlights)
            .entry(key)
            .or_default();
        for highlight in highlights {
            entry.insert(highlight.inlay, (style.clone(), highlight));
        }
        self.highlights_dirty = true;
    }

    /// Remove the inlays named by `remove` and add `insert` (each an anchor
    /// position, display text, and kind), returning the ids of the added
    /// inlays. A full replace passes the prior ids as `remove`.
    ///
    /// Syncs the display layers on both sides of the splice, so the added
    /// inlays are placed before control returns. [`InlayMap`] resolves a
    /// splice against the buffer as it stands and can only place those offsets
    /// while its own text still reads the same, so a splice left unsynced is
    /// stranded by the next edit and costs a rebuild of every row.
    pub fn splice_inlays(
        &mut self,
        remove: Vec<InlayId>,
        insert: Vec<(Anchor, String, InlayKind)>,
    ) -> Vec<InlayId> {
        // Brings the layers to the version the splice below resolves against.
        // Costs a refcount bump when they are already there, which is the usual
        // case, since a caller needs a snapshot to anchor the inlays it passes.
        self.snapshot();

        let ids = {
            let buffer_snapshot = self.multi_buffer.snapshot();
            self.inlay_map.splice(&buffer_snapshot, remove, insert)
        };

        // The splice bumped the inlay version, so this re-syncs rather than
        // being served the snapshot the call above cached.
        self.snapshot();
        ids
    }

    pub fn insert_creases(
        &mut self,
        creases: impl IntoIterator<Item = Crease<Anchor>>,
    ) -> Vec<CreaseId> {
        let ids = {
            let buffer_snapshot = self.multi_buffer.snapshot();
            let resolve = |a: &Anchor| buffer_snapshot.resolve_anchor(a);
            self.crease_map.insert(creases, &resolve)
        };
        // A crease change alters the crease snapshot without touching buffer,
        // fold, or inlay versions, so the cached snapshot must be dropped
        // explicitly or the new creases stay invisible.
        self.cached_snapshot = None;
        ids
    }

    pub fn remove_creases(&mut self, ids: impl IntoIterator<Item = CreaseId>) {
        self.crease_map.remove(ids);
        self.cached_snapshot = None;
    }

    pub fn set_lsp_folding_ranges(
        &mut self,
        buffer_id: BufferId,
        ranges: Vec<(Range<Anchor>, Option<String>)>,
    ) {
        if let Some(old_ids) = self.lsp_folding_crease_ids.remove(&buffer_id) {
            self.crease_map.remove(old_ids);
        }
        let creases = ranges.into_iter().map(|(range, collapsed_text)| {
            Crease::inline(
                range,
                FoldPlaceholder {
                    text: Arc::from("..."),
                    collapsed_text: collapsed_text.map(|t| Arc::from(t.as_str())),
                    ..Default::default()
                },
            )
        });
        let ids = self.insert_creases(creases);
        self.lsp_folding_crease_ids.insert(buffer_id, ids);
    }

    /// Bring the installed deleted-line blocks in line with `signature`,
    /// keeping the block already standing for each hunk that survived.
    ///
    /// Most refreshes change one hunk out of many, and replacing the whole set
    /// would mark every one of their rows for rebuild and hand back fresh ids
    /// for blocks that never moved.
    fn resplice_diff_blocks(
        &mut self,
        signature: Vec<(DiffHunkStatus, u32, Range<usize>)>,
        diff_map: Option<&DiffMap>,
    ) {
        let mut standing: HashMap<(DiffHunkStatus, u32, Range<usize>), CustomBlockId> = self
            .inserted_diff_block_signature
            .drain(..)
            .zip(self.inserted_diff_block_ids.drain(..))
            .collect();

        // Built in the same order as the signature, since both walk one filtered
        // pass over the hunks.
        let props = match diff_map.filter(|_| self.show_deleted_blocks) {
            Some(dm) => dm.deleted_blocks(),
            None => Vec::new(),
        };

        let mut ids: Vec<Option<CustomBlockId>> = Vec::with_capacity(signature.len());
        let mut fresh_props = Vec::new();
        let mut fresh_slots = Vec::new();
        for (slot, key) in signature.iter().enumerate() {
            match standing.remove(key) {
                Some(id) => ids.push(Some(id)),
                None => {
                    ids.push(None);
                    fresh_slots.push(slot);
                    fresh_props.push(props[slot].clone());
                },
            }
        }

        self.block_map.remove(&standing.into_values().collect());
        for (slot, id) in fresh_slots
            .into_iter()
            .zip(self.block_map.insert(fresh_props))
        {
            ids[slot] = Some(id);
        }

        self.inserted_diff_block_ids = ids.into_iter().flatten().collect();
        self.inserted_diff_block_signature = signature;
    }

    /// Sync the layers up to wrapping, returning the wrap snapshot, the wrap
    /// rows the sync changed, the same edits restated in buffer rows, and the
    /// buffer snapshot the whole sync ran against.
    ///
    /// The buffer-row patch is what the block map needs. Block placements are
    /// buffer rows, so a wrap-row patch cannot tell it how far an edit moved
    /// them, and by the time the wrap layer has run the buffer patch is gone.
    ///
    /// The buffer snapshot comes back so a caller with more to do against the
    /// same text reuses it. Building another would rebuild the excerpt tree
    /// against the live buffers to arrive at the same answer, this sync leaving
    /// the multi-buffer untouched.
    pub fn sync_through_wrap(
        &mut self,
    ) -> (
        Arc<WrapSnapshot>,
        Patch<u32>,
        Patch<u32>,
        MultiBufferSnapshot,
    ) {
        let buffer_snapshot = self.multi_buffer.snapshot();
        let buffer_edits = buffer_snapshot.edits_since(self.last_buffer_version);
        let buffer_row_edits = match self.last_buffer_snapshot.take() {
            Some(previous) => buffer_row_patch(&buffer_edits, &previous, &buffer_snapshot),
            None => Patch::empty(),
        };

        self.last_buffer_version = buffer_snapshot.version();
        self.last_buffer_snapshot = Some(buffer_snapshot.clone());

        let (inlay_snapshot, inlay_edits) =
            self.inlay_map.sync(buffer_snapshot.clone(), &buffer_edits);
        let (fold_snapshot, fold_edits) = self.fold_map.sync(inlay_snapshot, &inlay_edits);
        let (tab_snapshot, tab_edits) = self.tab_map.sync(fold_snapshot, fold_edits);
        let (wrap_snapshot, wrap_edits) = self.wrap_map.sync(tab_snapshot, &tab_edits);
        (wrap_snapshot, wrap_edits, buffer_row_edits, buffer_snapshot)
    }

    pub fn snapshot(&mut self) -> DisplaySnapshot {
        self.snapshot_with_companion(None)
    }

    /// The buffer as it stands now, without syncing the display layers.
    ///
    /// For a caller whose question is buffer-level -- where an anchor resolves,
    /// how many lines there are -- and so needs none of the fold, wrap, or block
    /// mapping a [`DisplaySnapshot`] carries. Costs a few refcount bumps against
    /// [`Self::snapshot`]'s wrap sync, and answers about the current buffer
    /// rather than the one the display layers were last synced against.
    pub fn buffer_snapshot(&self) -> MultiBufferSnapshot {
        self.multi_buffer.snapshot()
    }

    pub fn snapshot_with_companion(
        &mut self,
        companion_wrap_data: Option<(&WrapSnapshot, &Patch<u32>)>,
    ) -> DisplaySnapshot {
        let highlights_dirty = mem::take(&mut self.highlights_dirty);
        let buffer_version = self.multi_buffer.buffer_version();
        let diff_version_now = self.multi_buffer.diff_version();
        // A settled background rewrap moves none of these versions, so the
        // cache would keep serving the interpolated wrapping. Re-syncing while
        // one is outstanding is what lets it land.
        if buffer_version == self.last_buffer_version
            && diff_version_now == self.last_diff_version
            && self.fold_map.version_unchanged()
            && self.inlay_map.version_unchanged()
            && !self.wrap_map.background_pending()
            && companion_wrap_data.is_none()
            && let Some(cached) = self.cached_snapshot.clone()
        {
            // Highlights are the one thing a snapshot carries that no layer's
            // geometry depends on, so a change to them is answered by rewriting
            // those fields on the cached snapshot. A parse installs a token
            // channel per keystroke, and rebuilding five layers to receive an
            // Arc is what that used to cost.
            if !highlights_dirty {
                return cached;
            }
            let mut refreshed = cached;
            self.refresh_highlights(&mut refreshed);
            self.cached_snapshot = Some(refreshed.clone());
            return refreshed;
        }

        let (wrap_snapshot, wrap_edits, buffer_row_edits, buffer_snapshot) =
            self.sync_through_wrap();
        let diff_map = buffer_snapshot.diff_map.clone();
        let diff_version = diff_map.as_ref().map(|dm| dm.version()).unwrap_or(0);
        if diff_version != self.last_diff_version
            || self.show_deleted_blocks != self.last_show_deleted_blocks
        {
            let signature = if self.show_deleted_blocks {
                diff_map
                    .as_ref()
                    .map(|dm| dm.deleted_block_signature())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            // A recompute that found the same hunks yields the same blocks, and
            // re-splicing them would only mint new ids for identical content
            // while forcing the transform tree to be patched around them.
            if signature != self.inserted_diff_block_signature {
                self.resplice_diff_blocks(signature, diff_map.as_ref());
            }

            self.last_diff_version = diff_version;
            self.last_show_deleted_blocks = self.show_deleted_blocks;
        }
        let companion_view =
            self.companion
                .as_ref()
                .zip(companion_wrap_data)
                .map(|(c, (snap, edits))| CompanionView {
                    display_map_id: self.id,
                    companion_wrap_snapshot: snap,
                    companion_wrap_edits: edits,
                    companion: c,
                });
        let block_snapshot = self.block_map.sync(
            wrap_snapshot,
            &wrap_edits,
            &buffer_row_edits,
            companion_view,
        );

        if buffer_version != self.last_crease_sync_version {
            self.crease_map.sync(&buffer_snapshot);
            self.last_crease_sync_version = buffer_version;
        }

        let snapshot = DisplaySnapshot {
            companion_display_snapshot: None,
            block_snapshot,
            diff_map,
            text_highlights: self.text_highlights.clone(),
            semantic_token_highlights: self.semantic_token_highlights.clone(),
            lsp_token_highlights: self.lsp_token_highlights.clone(),
            inlay_highlights: self.inlay_highlights.clone(),
            crease_snapshot: self.crease_map.snapshot(),
            fold_placeholder: FoldPlaceholder::default(),
            masked: self.masked,
            syntax_highlighting: self.syntax_highlighting,
            clip_at_line_ends: self.clip_at_line_ends,
            diagnostics_max_severity: self.diagnostics_max_severity,
        };
        self.cached_snapshot = Some(snapshot.clone());
        snapshot
    }

    /// Rewrite `snapshot`'s highlight fields from the current ones.
    ///
    /// These are exactly the fields the construction above reads off `self`
    /// rather than off a layer, which is what makes them safe to replace on a
    /// snapshot whose geometry is still current. The two lists have to agree, so
    /// a field added to one belongs in the other.
    fn refresh_highlights(&self, snapshot: &mut DisplaySnapshot) {
        snapshot.text_highlights = self.text_highlights.clone();
        snapshot.semantic_token_highlights = self.semantic_token_highlights.clone();
        snapshot.lsp_token_highlights = self.lsp_token_highlights.clone();
        snapshot.inlay_highlights = self.inlay_highlights.clone();
        snapshot.syntax_highlighting = self.syntax_highlighting;
    }
}

#[derive(Clone)]
pub struct DisplaySnapshot {
    companion_display_snapshot: Option<Arc<DisplaySnapshot>>,
    block_snapshot: BlockSnapshot,
    diff_map: Option<DiffMap>,
    text_highlights: TextHighlights,
    semantic_token_highlights: SemanticTokensHighlights,
    lsp_token_highlights: SemanticTokensHighlights,
    inlay_highlights: Arc<InlayHighlights>,
    crease_snapshot: CreaseSnapshot,
    fold_placeholder: FoldPlaceholder,
    masked: bool,
    syntax_highlighting: bool,
    clip_at_line_ends: bool,
    diagnostics_max_severity: Option<DiagnosticSeverity>,
}

impl DisplaySnapshot {
    pub fn version(&self) -> usize {
        self.fold_snapshot().version()
    }

    pub fn tab_snapshot(&self) -> &TabSnapshot {
        self.block_snapshot.wrap_snapshot().tab_snapshot()
    }

    pub fn fold_snapshot(&self) -> &FoldSnapshot {
        self.tab_snapshot().fold_snapshot()
    }

    pub fn inlay_snapshot(&self) -> &InlaySnapshot {
        self.fold_snapshot().inlay_snapshot()
    }

    pub fn companion_snapshot(&self) -> Option<&DisplaySnapshot> {
        self.companion_display_snapshot.as_deref()
    }

    pub fn fold_placeholder(&self) -> &FoldPlaceholder {
        &self.fold_placeholder
    }

    pub fn chunk_renderer_at_fold_point(&self, fold_point: FoldPoint) -> Option<ChunkRenderer> {
        self.fold_snapshot()
            .fold_id_at_point(fold_point)
            .map(|id| ChunkRenderer {
                id: ChunkRendererId::Fold(id.0),
            })
    }

    pub fn crease_snapshot(&self) -> &CreaseSnapshot {
        &self.crease_snapshot
    }

    pub fn text_highlights(&self) -> &TextHighlights {
        &self.text_highlights
    }

    pub fn semantic_token_highlights(&self) -> &SemanticTokensHighlights {
        &self.semantic_token_highlights
    }

    pub fn lsp_token_highlights(&self) -> &SemanticTokensHighlights {
        &self.lsp_token_highlights
    }

    /// The inlay highlights, as the shared handle rather than the map behind
    /// it. Two snapshots taken without an intervening highlight change hold the
    /// same allocation, which is what a caller comparing them is asking about.
    pub fn inlay_highlights(&self) -> &Arc<InlayHighlights> {
        &self.inlay_highlights
    }

    pub fn is_masked(&self) -> bool {
        self.masked
    }

    pub fn wrap_snapshot(&self) -> &WrapSnapshot {
        self.block_snapshot.wrap_snapshot()
    }

    pub fn buffer_snapshot(&self) -> &MultiBufferSnapshot {
        self.block_snapshot.buffer_snapshot()
    }

    pub fn chunks(
        &self,
        display_rows: Range<u32>,
        highlights: Highlights<'_>,
    ) -> block_map::BlockChunks<'_> {
        let byte_range = self
            .block_snapshot
            .row_range_to_buffer_byte_range(display_rows.clone());
        let endpoints = self.build_endpoints(highlights, byte_range);
        self.block_snapshot.chunks(display_rows, endpoints)
    }

    /// The semantic-token highlights (tree-sitter coloring) to feed the
    /// endpoint builder, or `None` when syntax highlighting is off for this
    /// editor. Withholding them is what suppresses the coloring. The endpoint
    /// cache keys on the collection's pointer, so `None` versus `Some`
    /// invalidates it across a toggle.
    fn syntax_token_highlights(&self) -> Option<&SemanticTokensHighlights> {
        self.syntax_highlighting
            .then_some(&self.semantic_token_highlights)
    }

    /// LSP semantic tokens gated on the same syntax-highlighting toggle as
    /// [`Self::syntax_token_highlights`], since both are semantic coloring.
    fn lsp_syntax_token_highlights(&self) -> Option<&SemanticTokensHighlights> {
        self.syntax_highlighting
            .then_some(&self.lsp_token_highlights)
    }

    pub fn highlighted_chunks(&self, display_rows: Range<u32>) -> block_map::BlockChunks<'_> {
        let endpoints = self.highlighted_endpoints(display_rows.clone());
        self.highlighted_chunks_with_endpoints(display_rows, endpoints)
    }

    /// Resolve the syntax-highlight endpoints spanning `display_rows` in one
    /// pass.
    ///
    /// A caller painting a range row by row builds these once and hands each row
    /// the shared set through [`Self::highlighted_chunks_with_endpoints`],
    /// rather than rebuilding them per row.
    pub fn highlighted_endpoints(
        &self,
        display_rows: Range<u32>,
    ) -> Arc<[highlights::HighlightEndpoint]> {
        let highlights = Highlights {
            text_highlights: Some(&self.text_highlights),
            inlay_highlights: Some(self.inlay_highlights.as_ref()),
            semantic_token_highlights: self.syntax_token_highlights(),
            lsp_token_highlights: self.lsp_syntax_token_highlights(),
        };
        let byte_range = self
            .block_snapshot
            .row_range_to_buffer_byte_range(display_rows);
        self.build_endpoints(highlights, byte_range)
    }

    /// Like [`Self::highlighted_endpoints`] but memoizes the result in `cache`,
    /// rebuilding only when the buffer version, the identity of a highlight
    /// collection, or the byte range changes.
    ///
    /// A repaint that changed none of those, which is what a cursor blink or a
    /// glide frame is, gets the previous resolve back rather than walking the
    /// highlight maps again.
    pub fn highlighted_endpoints_cached(
        &self,
        display_rows: Range<u32>,
        cache: &mut Option<CachedHighlightEndpoints>,
    ) -> Arc<[highlights::HighlightEndpoint]> {
        let highlights = Highlights {
            text_highlights: Some(&self.text_highlights),
            inlay_highlights: Some(self.inlay_highlights.as_ref()),
            semantic_token_highlights: self.syntax_token_highlights(),
            lsp_token_highlights: self.lsp_syntax_token_highlights(),
        };
        let byte_range = self
            .block_snapshot
            .row_range_to_buffer_byte_range(display_rows);
        self.build_endpoints_cached(highlights, byte_range, cache)
    }

    /// Chunk `display_rows` using endpoints already resolved by
    /// [`Self::highlighted_endpoints`], skipping the per-call endpoint build.
    ///
    /// Endpoints spanning a wider range than `display_rows` are valid because
    /// the returned [`block_map::BlockChunks`] seeks the endpoints intersecting
    /// each row, so one set built for a viewport paints any single row within it.
    pub fn highlighted_chunks_with_endpoints(
        &self,
        display_rows: Range<u32>,
        endpoints: Arc<[highlights::HighlightEndpoint]>,
    ) -> block_map::BlockChunks<'_> {
        self.block_snapshot.chunks(display_rows, endpoints)
    }

    /// Like [`Self::highlighted_chunks`] but memoizes the resolved endpoints in
    /// `cache`, recomputing only when the buffer version, highlight identity, or
    /// visible byte range changes.
    pub fn highlighted_chunks_cached(
        &self,
        display_rows: Range<u32>,
        cache: &mut Option<CachedHighlightEndpoints>,
    ) -> block_map::BlockChunks<'_> {
        let highlights = Highlights {
            text_highlights: Some(&self.text_highlights),
            inlay_highlights: Some(self.inlay_highlights.as_ref()),
            semantic_token_highlights: self.syntax_token_highlights(),
            lsp_token_highlights: self.lsp_syntax_token_highlights(),
        };
        let byte_range = self
            .block_snapshot
            .row_range_to_buffer_byte_range(display_rows.clone());
        let endpoints = self.build_endpoints_cached(highlights, byte_range, cache);
        self.block_snapshot.chunks(display_rows, endpoints)
    }

    fn build_endpoints(
        &self,
        highlights: Highlights<'_>,
        range: Range<usize>,
    ) -> Arc<[highlights::HighlightEndpoint]> {
        let buffer = self.buffer_snapshot();
        let text_highlights_ref = highlights.text_highlights.unwrap_or(&EMPTY_TEXT_HIGHLIGHTS);
        let semantic_ref = highlights.semantic_token_highlights;
        let lsp_ref = highlights.lsp_token_highlights;
        let resolve = |a: &Anchor| buffer.resolve_anchor(a);
        let resolve_batch = |a: &[Anchor]| buffer.resolve_anchors_batch(a);
        let resolver = AnchorResolver {
            one: &resolve,
            many: &resolve_batch,
        };
        let eps = highlights::create_highlight_endpoints(
            &range,
            text_highlights_ref,
            semantic_ref,
            lsp_ref,
            &resolver,
        );
        Arc::from(eps)
    }

    /// Endpoint builder for [`Self::highlighted_chunks_cached`], routing through
    /// the version-keyed [`highlights::create_highlight_endpoints_cached`].
    fn build_endpoints_cached(
        &self,
        highlights: Highlights<'_>,
        range: Range<usize>,
        cache: &mut Option<CachedHighlightEndpoints>,
    ) -> Arc<[highlights::HighlightEndpoint]> {
        let buffer = self.buffer_snapshot();
        let text_highlights_ref = highlights.text_highlights.unwrap_or(&EMPTY_TEXT_HIGHLIGHTS);
        let semantic_ref = highlights.semantic_token_highlights;
        let lsp_ref = highlights.lsp_token_highlights;
        let resolve = |a: &Anchor| buffer.resolve_anchor(a);
        let resolve_batch = |a: &[Anchor]| buffer.resolve_anchors_batch(a);
        let resolver = AnchorResolver {
            one: &resolve,
            many: &resolve_batch,
        };
        highlights::create_highlight_endpoints_cached(
            buffer.version(),
            &range,
            text_highlights_ref,
            semantic_ref,
            lsp_ref,
            &resolver,
            cache,
        )
    }

    pub fn is_line_folded(&self, buffer_row: u32) -> bool {
        let inlay_point = self
            .fold_snapshot()
            .inlay_snapshot()
            .to_inlay_point(Point::new(buffer_row, 0));
        self.fold_snapshot().is_line_folded(inlay_point.row())
    }

    pub fn buffer_to_display(&self, point: Point) -> DisplayPoint {
        let block = self.block_snapshot.buffer_to_block(point);
        DisplayPoint::new(block.row, block.column)
    }

    /// Tab-space position of `point`, the halfway house
    /// [`Self::buffer_to_display`] passes through.
    ///
    /// A caller stepping along a row can take this once for the row's start and
    /// accumulate the column itself, then finish with [`Self::tab_to_display`].
    /// The alternative is re-entering at the buffer each time, and the tab leg
    /// walks the row from its start to expand tabs, so doing that per character
    /// is quadratic in the row.
    pub fn buffer_to_tab_point(&self, point: Point) -> TabPoint {
        let fold_snapshot = self.fold_snapshot();
        let inlay_point = fold_snapshot.inlay_snapshot().to_inlay_point(point);
        let fold_point = fold_snapshot.to_fold_point(inlay_point, Bias::Right);
        self.tab_snapshot().to_tab_point(fold_point)
    }

    /// Display position of a tab-space position, for a caller that already
    /// knows the tab-expanded column.
    ///
    /// See [`Self::buffer_to_tab_point`] for why a caller would hold one.
    pub fn tab_to_display(&self, tab_point: TabPoint) -> DisplayPoint {
        let wrap_point = self.wrap_snapshot().to_wrap_point(tab_point);
        let block = self.block_snapshot.wrap_to_block(wrap_point);
        DisplayPoint::new(block.row, block.column)
    }

    pub fn display_to_buffer(&self, point: DisplayPoint) -> Option<Point> {
        self.block_snapshot
            .block_to_buffer(BlockPoint::new(point.row, point.column))
    }

    /// Cells `point` sits at, counted from the start of its buffer line.
    ///
    /// A character occupies as many cells as it is drawn in, one for most, two
    /// for a wide glyph, and as many as the next tab stop for a tab. That count
    /// is what a reader means by a column, where a byte offset is only the same
    /// number while every character is one byte and one cell.
    ///
    /// Counted along the buffer line rather than along a display row, so a
    /// soft-wrapped line answers one column for its whole length instead of
    /// restarting at each wrap.
    ///
    /// See also:
    /// - [`Self::buffer_column_at_visual`] for the way back.
    pub fn visual_column(&self, point: Point) -> u32 {
        let tabs = self.tab_snapshot();
        tab_map::expand_column(
            self.line_chars(point.row),
            point.column,
            tabs.tab_size(),
            tabs.max_expansion_column(),
        )
    }

    /// Byte column on `row` that sits at `visual` cells from its start.
    ///
    /// A column landing inside a character resolves by `bias`, and one past the
    /// line's end gives the line's length, so a short line answers its own end
    /// rather than refusing.
    pub fn buffer_column_at_visual(&self, row: u32, visual: u32, bias: Bias) -> u32 {
        let tabs = self.tab_snapshot();
        tab_map::collapse_column(
            self.line_chars(row),
            visual,
            tabs.tab_size(),
            bias,
            tabs.max_expansion_column(),
        )
    }

    /// Characters of buffer `row`, stopping at its line break.
    fn line_chars(&self, row: u32) -> impl Iterator<Item = char> {
        let buffer = self.buffer_snapshot();
        let rope = buffer.rope();
        let start = rope.point_to_offset(Point::new(row, 0));
        let len = rope.line_len(row) as usize;
        rope.chars_at(start).take(len)
    }

    pub fn classify_row(&self, display_row: u32) -> BlockRowKind<'_> {
        self.block_snapshot.classify_row(display_row)
    }

    pub fn buffer_rows_above(&self, display_row: u32) -> u32 {
        self.block_snapshot.buffer_rows_above(display_row)
    }

    pub fn clip_point(&self, point: DisplayPoint, bias: Bias) -> DisplayPoint {
        let bp = self
            .block_snapshot
            .clip_point(BlockPoint::new(point.row, point.column), bias);
        let mut clipped = DisplayPoint::new(bp.row, bp.column);
        if self.clip_at_line_ends {
            clipped = self.clip_point_at_line_end(clipped);
        }
        clipped
    }

    pub fn clip_ignoring_line_ends(&self, point: DisplayPoint, bias: Bias) -> DisplayPoint {
        let bp = self
            .block_snapshot
            .clip_point(BlockPoint::new(point.row, point.column), bias);
        DisplayPoint::new(bp.row, bp.column)
    }

    fn clip_point_at_line_end(&self, point: DisplayPoint) -> DisplayPoint {
        let line_len = self.line_len(point.row);
        if line_len > 0 && point.column >= line_len {
            DisplayPoint::new(point.row, line_len.saturating_sub(1))
        } else {
            point
        }
    }

    pub fn max_point(&self) -> DisplayPoint {
        let bp = self.block_snapshot.max_point();
        DisplayPoint::new(bp.row, bp.column)
    }

    pub fn line_len(&self, display_row: u32) -> u32 {
        self.block_snapshot.line_len(display_row)
    }

    pub fn line_count(&self) -> u32 {
        self.block_snapshot.total_lines()
    }

    pub fn buffer_line_count(&self) -> u32 {
        self.block_snapshot.buffer_line_count()
    }

    pub fn text(&self) -> &str {
        self.block_snapshot.buffer_text()
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.block_snapshot.buffer_lines()
    }

    pub fn line_diff_status(&self, buffer_line: u32) -> DiffStatus {
        self.diff_map
            .as_ref()
            .map(|dm| dm.status_for_line(buffer_line))
            .unwrap_or_default()
    }

    /// Snapshot's freshly-cloned diff map. Prefer this over reaching for
    /// `buffer_snapshot().diff_map`, which is read through the inlay/fold/
    /// tab/wrap cache chain and can lag behind buffer mutations that don't
    /// bump the buffer's edit version.
    pub fn diff_map(&self) -> Option<&DiffMap> {
        self.diff_map.as_ref()
    }

    pub fn write_display_line(&self, buf: &mut String, display_row: u32) {
        self.block_snapshot.write_display_line(buf, display_row);
    }

    pub fn display_line(&self, display_row: u32) -> String {
        let mut result = String::new();
        self.write_display_line(&mut result, display_row);
        result
    }

    pub fn display_lines(&self, range: Range<u32>) -> impl Iterator<Item = String> + '_ {
        range.map(move |row| self.display_line(row))
    }

    pub fn is_wrap_continuation(&self, display_row: u32) -> bool {
        self.block_snapshot.is_wrap_continuation(display_row)
    }

    pub fn soft_wrap_indent(&self, display_row: u32) -> u32 {
        self.block_snapshot.soft_wrap_indent(display_row)
    }

    pub fn wrap_width(&self) -> Option<u32> {
        self.block_snapshot.wrap_width()
    }

    pub fn has_deletion_after(&self, buffer_line: u32) -> bool {
        self.diff_map
            .as_ref()
            .map(|dm| dm.has_deletion_after(buffer_line))
            .unwrap_or(false)
    }

    pub fn token_detail_for_line(&self, buffer_line: u32) -> Option<&TokenDetail> {
        self.diff_map.as_ref()?.token_detail_for_line(buffer_line)
    }

    pub fn buffer_chars_at(&self, point: Point) -> BufferCharsAt<'_> {
        let rope = &self.block_snapshot.buffer_snapshot().rope();
        let offset = rope.point_to_offset(point);
        BufferCharsAt {
            chars: rope.chars_at(offset),
            point,
        }
    }

    pub fn reverse_buffer_chars_at(&self, point: Point) -> ReversedBufferCharsAt<'_> {
        let rope = &self.block_snapshot.buffer_snapshot().rope();
        let offset = rope.point_to_offset(point);
        ReversedBufferCharsAt {
            chars: rope.reversed_chars_at(offset),
            point,
            rope,
        }
    }

    pub fn prev_line_boundary(&self, point: Point) -> (Point, DisplayPoint) {
        let display = self.buffer_to_display(point);
        let start = DisplayPoint::new(display.row, 0);
        let buf = self.display_to_buffer(start).unwrap_or(Point::zero());
        (buf, start)
    }

    pub fn next_line_boundary(&self, point: Point) -> (Point, DisplayPoint) {
        let display = self.buffer_to_display(point);
        let end = DisplayPoint::new(display.row, self.line_len(display.row));
        let max = self.block_snapshot.buffer_snapshot().rope().max_point();
        let buf = self.display_to_buffer(end).unwrap_or(max);
        (buf, end)
    }

    pub fn clip_at_line_end(&self, point: DisplayPoint) -> DisplayPoint {
        let clipped = self.clip_ignoring_line_ends(point, Bias::Left);
        DisplayPoint::new(clipped.row, clipped.column.min(self.line_len(clipped.row)))
    }

    pub fn diagnostics_max_severity(&self) -> Option<DiagnosticSeverity> {
        self.diagnostics_max_severity
    }
}

pub struct BufferCharsAt<'a> {
    chars: CharsAt<'a>,
    point: Point,
}

impl Iterator for BufferCharsAt<'_> {
    type Item = (char, Point);

    fn next(&mut self) -> Option<(char, Point)> {
        let ch = self.chars.next()?;
        let point = self.point;
        if ch == '\n' {
            self.point.row += 1;
            self.point.column = 0;
        } else {
            self.point.column += ch.len_utf8() as u32;
        }
        Some((ch, point))
    }
}

pub struct ReversedBufferCharsAt<'a> {
    chars: ReversedCharsAt<'a>,
    point: Point,
    rope: &'a Rope,
}

impl Iterator for ReversedBufferCharsAt<'_> {
    type Item = (char, Point);

    fn next(&mut self) -> Option<(char, Point)> {
        let ch = self.chars.next()?;
        if ch == '\n' {
            self.point.row -= 1;
            self.point.column = self.rope.line_len(self.point.row);
        } else {
            self.point.column -= ch.len_utf8() as u32;
        }
        Some((ch, self.point))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BlockPlacement, BlockProperties, BlockRowKind, BlockStyle, DisplayMap, DisplayPoint,
        DisplayRow, DisplaySnapshot, HighlightStyle, HighlightStyleInterner, InlayKind, InlayPoint,
        SemanticTokenHighlight,
    };
    use crate::{
        buffer::{BufferId, TextBuffer},
        diff_map::{DiffHunk, DiffHunkStatus, DiffMap},
        multi_buffer::MultiBuffer,
    };
    use std::{
        ops::Range,
        sync::{Arc, RwLock},
    };
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_text::Point;

    fn test_executor() -> Executor {
        Executor::new(Arc::new(TestScheduler::new()))
    }

    fn create_display_map(content: &str) -> DisplayMap {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        DisplayMap::new(multi_buffer, test_executor(), crate::test_notify())
    }

    fn create_display_map_with_diff(content: &str, diff_map: DiffMap) -> DisplayMap {
        let mut buffer = TextBuffer::with_text(BufferId::new(0), content);
        buffer.diff_map = Some(diff_map);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
        display_map.set_show_deleted_blocks(true);
        display_map
    }

    fn make_diff_with_deletion(
        after_line: u32,
        base_text: &str,
        byte_range: Range<usize>,
        _line_count: u32,
    ) -> DiffMap {
        let mut dm = DiffMap::default();
        dm.set_base_text(Arc::new(base_text.to_string()));
        dm.push_hunk(DiffHunk {
            status: DiffHunkStatus::Deleted,
            unstaged_lines: std::iter::once((after_line + 1)..(after_line + 1)).collect(),
            buffer_start_line: after_line + 1,
            buffer_line_range: (after_line + 1)..(after_line + 1),
            base_byte_range: byte_range,
            anchor_range: None,
            token_detail: None,
        });
        dm
    }

    /// Recomputing a diff stamps a fresh version whether or not anything moved,
    /// and every version bump used to drop and re-add every deleted-line block.
    /// A keystroke that leaves the hunks alone should leave the blocks alone,
    /// which the ids show directly since each insert mints a new one.
    #[test]
    fn a_diff_refresh_that_changes_no_hunk_keeps_the_same_blocks() {
        let base = "line1\ndeleted\nline2";
        let mut buffer = TextBuffer::with_text(BufferId::new(0), "line1\nline2");
        buffer.diff_map = Some(make_diff_with_deletion(0, base, 6..13, 1));
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
        display_map.set_show_deleted_blocks(true);

        display_map.snapshot();
        let first = display_map.inserted_diff_block_ids.clone();
        assert_eq!(first.len(), 1, "the deleted hunk contributes one block");

        // A re-diff after an edit elsewhere finds the same hunks and puts them
        // in a map carrying a new version.
        shared.write().expect("poisoned").diff_map =
            Some(make_diff_with_deletion(0, base, 6..13, 1));
        display_map.snapshot();

        assert_eq!(
            display_map.inserted_diff_block_ids, first,
            "a refresh finding the same hunks re-splices nothing",
        );
    }

    /// A refresh that found one new hunk should cost one block, not a whole new
    /// set. The ids show it: an untouched hunk keeps the block it already had.
    #[test]
    fn a_refresh_finding_a_new_hunk_keeps_the_other_blocks() {
        let base = "line1\ndeleted\nline2\ngone\nline3";
        let mut buffer = TextBuffer::with_text(BufferId::new(0), "line1\nline2\nline3");
        buffer.diff_map = Some(make_diff_with_deletion(0, base, 6..13, 1));
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
        display_map.set_show_deleted_blocks(true);

        display_map.snapshot();
        let first = display_map.inserted_diff_block_ids.clone();
        assert_eq!(first.len(), 1);

        // The same hunk plus one further down the file.
        let mut grown = make_diff_with_deletion(0, base, 6..13, 1);
        grown.push_hunk(DiffHunk {
            status: DiffHunkStatus::Deleted,
            unstaged_lines: std::iter::once(2..2).collect(),
            buffer_start_line: 2,
            buffer_line_range: 2..2,
            base_byte_range: 19..24,
            anchor_range: None,
            token_detail: None,
        });
        shared.write().expect("poisoned").diff_map = Some(grown);
        display_map.snapshot();

        let after = display_map.inserted_diff_block_ids.clone();
        assert_eq!(after.len(), 2, "the new hunk adds a block");
        assert_eq!(
            after[0], first[0],
            "the hunk that did not move keeps the block it had",
        );
    }

    #[test]
    fn display_snapshot_version() {
        let mut dm = create_display_map("hello");
        let v1 = dm.snapshot().version();
        let v2 = dm.snapshot().version();
        assert_eq!(v1, v2);
    }

    #[test]
    fn crease_sync_tracks_only_buffer_edits() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2\n");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut dm = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

        let range = {
            let snap = dm.multi_buffer.snapshot();
            snap.anchor_at(0, stoat_text::Bias::Right)..snap.anchor_at(5, stoat_text::Bias::Left)
        };
        dm.set_lsp_folding_ranges(BufferId::new(0), vec![(range, None)]);

        dm.snapshot();
        let synced_at = dm.last_crease_sync_version;

        dm.insert_blocks(Vec::new());
        dm.snapshot();
        assert_eq!(
            dm.last_crease_sync_version, synced_at,
            "a rebuild with an unchanged buffer version does not re-sync creases",
        );

        {
            let mut buf = shared.write().unwrap();
            buf.edit(0..0, "x");
        }
        dm.snapshot();
        assert!(
            dm.last_crease_sync_version > synced_at,
            "a buffer edit re-syncs the crease map",
        );
    }

    #[test]
    fn passthrough_coordinates() {
        let mut display_map = create_display_map("hello\nworld\n");
        let snapshot = display_map.snapshot();

        let buffer_point = Point::new(1, 3);
        let display_point = snapshot.buffer_to_display(buffer_point);
        assert_eq!(display_point, DisplayPoint::new(1, 3));

        let back = snapshot.display_to_buffer(display_point);
        assert_eq!(back, Some(buffer_point));
    }

    #[test]
    fn line_count() {
        let mut display_map = create_display_map("line1\nline2\nline3");
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.line_count(), 3);
    }

    #[test]
    fn line_count_survives_successive_mid_buffer_inserts() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "aaaa\nbbbb\ncccc\ndddd\n");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

        let before = display_map.snapshot().line_count();
        assert_eq!(
            before, 5,
            "five display rows including the trailing phantom row"
        );

        for ch in ["x", "y"] {
            {
                let mut buf = shared.write().unwrap();
                buf.edit(6..6, ch);
            }
            assert_eq!(
                display_map.snapshot().line_count(),
                before,
                "a mid-buffer insert must not drop a display row",
            );
        }
    }

    #[test]
    fn typing_mid_buffer_keeps_the_last_line_rendered() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 10);
        let path = h.write_file("edit.txt", "alpha\nbravo\ncharlie\ndelta\n");
        h.open_file(&path);

        h.type_keys("j j i");
        h.type_text("XY");

        let frame = h.snapshot();
        assert!(
            frame.content.contains("delta"),
            "the last line stays rendered while typing mid-buffer:\n{}",
            frame.content,
        );
    }

    /// The review and conflict paints resolve endpoints once per frame and
    /// chunk each row against them, so what a repaint costs turns on whether
    /// that resolve is reused. A hit hands back the same allocation, which is
    /// the only way to see the difference from outside.
    #[test]
    fn a_repaint_that_changed_nothing_reuses_its_endpoints() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "let x = 1\nlet y = 2\n");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

        let mut cache = None;
        let first = display_map
            .snapshot()
            .highlighted_endpoints_cached(0..2, &mut cache);
        let second = display_map
            .snapshot()
            .highlighted_endpoints_cached(0..2, &mut cache);
        assert!(
            Arc::ptr_eq(&first, &second),
            "an unchanged frame reuses the resolve",
        );

        // A different range describes different bytes, so it cannot answer from
        // the same set.
        let narrower = display_map
            .snapshot()
            .highlighted_endpoints_cached(0..1, &mut cache);
        assert!(!Arc::ptr_eq(&first, &narrower), "a new range rebuilds");

        shared.write().expect("poisoned").edit(0..0, "// edit\n");
        let after_edit = display_map
            .snapshot()
            .highlighted_endpoints_cached(0..1, &mut cache);
        assert!(
            !Arc::ptr_eq(&narrower, &after_edit),
            "a buffer edit rebuilds",
        );
    }

    #[test]
    fn insert_blocks_after_snapshot_grows_line_count() {
        let mut display_map = create_display_map("line1\nline2\nline3");
        // Prime the snapshot cache, then insert a one-row block. The next
        // snapshot must rebuild and reflect the added row rather than return
        // the cached snapshot taken before the insert.
        assert_eq!(display_map.snapshot().line_count(), 3);

        display_map.insert_blocks(vec![BlockProperties::from_text(
            BlockPlacement::Below(0),
            vec!["extra".to_string()],
            BlockStyle::Fixed,
        )]);

        assert_eq!(display_map.snapshot().line_count(), 4);
    }

    /// Many blocks shifted by one edit land where a from-scratch build puts them.
    ///
    /// Where a single block ends up is a weak check. A sync that leaves the
    /// transform tree stale can still get one block right. Comparing every row of
    /// an incrementally-synced map against a map built fresh over the same final
    /// text is what pins the tree itself, and it is the shape that catches a
    /// block silently dropped at a rebuild region's boundary.
    #[test]
    fn many_blocks_shifted_by_an_edit_match_a_fresh_build() {
        let text: String = (0..60).map(|i| format!("line{i}\n")).collect();

        // Anchored below row 5 and up, so the edit at the top slides every one
        // of them and collapses none. A block inside the edited range has its
        // own behaviour, covered by the sibling tests.
        let blocks_at = |shift: u32| -> Vec<BlockProperties> {
            (5..55)
                .map(|i| {
                    BlockProperties::from_text(
                        BlockPlacement::Below(i + shift),
                        vec![format!("marker{i}")],
                        BlockStyle::Fixed,
                    )
                })
                .collect()
        };

        let rows = |map: &mut DisplayMap| {
            let snapshot = map.snapshot();
            (0..snapshot.line_count())
                .map(|row| match snapshot.classify_row(row) {
                    BlockRowKind::BufferRow { buffer_row } => format!("buf{buffer_row}"),
                    BlockRowKind::Block { block, line_index } => {
                        block.get_line(line_index).to_string()
                    },
                })
                .collect::<Vec<_>>()
        };

        let fresh = |content: &str, shift: u32| {
            let shared = Arc::new(RwLock::new(TextBuffer::with_text(
                BufferId::new(0),
                content,
            )));
            let multi = MultiBuffer::singleton(BufferId::new(0), shared);
            let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());
            map.insert_blocks(blocks_at(shift));
            rows(&mut map)
        };

        let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
        display_map.insert_blocks(blocks_at(0));
        assert_eq!(rows(&mut display_map), fresh(&text, 0), "before any edit");

        shared.write().expect("poisoned").edit(0..0, "a\nb\nc\n");
        let inserted = format!("a\nb\nc\n{text}");
        assert_eq!(
            rows(&mut display_map),
            fresh(&inserted, 3),
            "three rows inserted above all 50 blocks, which carry down three rows",
        );

        shared.write().expect("poisoned").edit(0..6, "");
        assert_eq!(
            rows(&mut display_map),
            fresh(&text, 0),
            "and removed again, back to the original",
        );
    }

    /// An edit appending a row at the very end leaves the map the same length
    /// as a fresh build.
    ///
    /// The row patch each layer hands down says which rows changed and how many
    /// replaced them. A region running to the end of the buffer has to count
    /// the empty row after the final newline, which the accumulated position
    /// stops short of, so a patch built from that position alone reports the
    /// appended row as a same-size change and the layers above build one row
    /// short.
    #[test]
    fn an_insert_at_the_buffer_end_adds_a_row() {
        let text: String = (0..5).map(|i| format!("line{i}\n")).collect();
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
        let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());
        assert_eq!(
            map.snapshot().line_count(),
            6,
            "five lines and the empty one"
        );

        let len = shared.read().expect("poisoned").rope().len();
        shared.write().expect("poisoned").edit(len..len, "zz\n");

        let fresh_multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut fresh = DisplayMap::new(fresh_multi, test_executor(), crate::test_notify());
        assert_eq!(
            map.snapshot().line_count(),
            fresh.snapshot().line_count(),
            "the appended row reaches the incremental map too",
        );
    }

    /// Splicing an inlay onto a row above a fold leaves the fold collapsed.
    ///
    /// A splice reports the row it lands on as changed, and the region rebuilt
    /// for it ends where the next row begins. A fold starting on exactly that
    /// row sits outside the region, so the rebuild does not re-emit it and the
    /// old transform tree is expected to carry it. The transform the cursor
    /// rests on at that point is the fold's own placeholder, and re-emitting
    /// what follows the region as ordinary text unfolds it.
    #[test]
    fn an_inlay_spliced_above_a_fold_leaves_it_folded() {
        let text: String = (0..30).map(|i| format!("line{i}\n")).collect();
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
        let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());

        map.fold(vec![Point::new(5, 0)..Point::new(8, 0)]);
        let folded = map.snapshot().fold_snapshot().line_count();
        assert_eq!(folded, 28, "three rows collapsed out of thirty-one");

        let anchor = {
            let snap = map.multi_buffer.snapshot();
            let offset = snap.rope().point_to_offset(Point::new(4, 0));
            snap.anchor_at(offset, stoat_text::Bias::Left)
        };
        map.splice_inlays(
            Vec::new(),
            vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
        );

        assert_eq!(
            map.snapshot().fold_snapshot().line_count(),
            folded,
            "the inlay carries no newline, so no row is added or removed",
        );

        // The line count alone would pass a tree that lost the fold and gained
        // it back elsewhere, and it is the rows that reach the reader.
        let rows = |map: &mut DisplayMap| {
            let snapshot = map.snapshot();
            (0..snapshot.line_count())
                .map(|row| snapshot.display_line(row))
                .collect::<Vec<_>>()
        };

        let fresh = |shared: &Arc<RwLock<TextBuffer>>| {
            let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());
            let mut fresh = DisplayMap::new(multi, test_executor(), crate::test_notify());
            fresh.fold(vec![Point::new(5, 0)..Point::new(8, 0)]);
            fresh.splice_inlays(
                Vec::new(),
                vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
            );
            rows(&mut fresh)
        };
        assert_eq!(rows(&mut map), fresh(&shared), "against a fresh build");

        // An unfolded tree survives its own sync, so the divergence only
        // reaches the reader on the next one.
        let len = shared.read().expect("poisoned").rope().len();
        shared.write().expect("poisoned").edit(len..len, "tail\n");
        assert_eq!(rows(&mut map), fresh(&shared), "and after a later edit");
    }

    /// A fold follows its text across an edit that removes a row above it
    /// without changing the buffer's length.
    ///
    /// Replacing a newline with an ordinary character costs the same bytes it
    /// frees, so every offset after it stays exactly where it was while the row
    /// it ended disappears and each later row shifts up by one. A fold is stored
    /// by offset and resolved into rows, so this is the one edit shape that
    /// leaves the stored form untouched and the resolved form moved.
    #[test]
    fn a_fold_below_a_byte_neutral_row_merge_moves_up_a_row() {
        let text: String = (0..30).map(|i| format!("line{i} words\n")).collect();
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
        let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());

        map.fold(vec![Point::new(15, 0)..Point::new(16, 0)]);
        let before = map.snapshot();
        assert_eq!(
            before.line_count(),
            30,
            "one of thirty-one rows folded away"
        );

        // The newline ending row 2, overwritten in place.
        let at = {
            let snap = shared.read().expect("poisoned");
            snap.rope().point_to_offset(Point::new(3, 0)) - 1
        };
        let len_before = shared.read().expect("poisoned").rope().len();
        shared.write().expect("poisoned").edit(at..at + 1, "X");
        assert_eq!(
            shared.read().expect("poisoned").rope().len(),
            len_before,
            "the edit has to be byte-neutral or it proves nothing",
        );

        let rows = |map: &mut DisplayMap| {
            let snapshot = map.snapshot();
            (0..snapshot.line_count())
                .map(|row| snapshot.display_line(row))
                .collect::<Vec<_>>()
        };

        let fresh_rows = {
            let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());
            let mut fresh = DisplayMap::new(multi, test_executor(), crate::test_notify());
            fresh.fold(vec![Point::new(14, 0)..Point::new(15, 0)]);
            rows(&mut fresh)
        };
        assert_eq!(rows(&mut map), fresh_rows, "against a fresh build");
    }

    /// Two edits in one patch, the first ending where a replacement begins and
    /// the second landing inside it, leave the block standing.
    ///
    /// A region rebuild scans the blocks its rows cover and remembers where the
    /// scan reached, so the next region in the same patch starts from there
    /// rather than from the beginning. A block starting exactly at a region's
    /// end is outside it, since the test is strict, but it is the next region
    /// that owns it. Counting it as scanned leaves it owned by no region at
    /// all, and a replacement is not carried over from the old tree either,
    /// because the edit inside it consumed its transform.
    #[test]
    fn a_replacement_survives_an_edit_after_one_ending_at_its_start() {
        let text: String = (0..32).map(|i| format!("line{i} words\n")).collect();

        let block = || {
            vec![BlockProperties::from_text(
                BlockPlacement::Replace { start: 26, end: 28 },
                vec!["replacement".to_string()],
                BlockStyle::Fixed,
            )]
        };

        let rows = |map: &mut DisplayMap| {
            let snapshot = map.snapshot();
            (0..snapshot.line_count())
                .map(|row| match snapshot.classify_row(row) {
                    BlockRowKind::BufferRow { buffer_row } => format!("buf{buffer_row}"),
                    BlockRowKind::Block { block, line_index } => block.get_line(line_index),
                })
                .collect::<Vec<_>>()
        };

        let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
        let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());
        map.insert_blocks(block());
        map.snapshot();

        // Both land in one patch, so the second region's block scan starts
        // where the first region's left off. Same-size so no row moves.
        for row in [14u32, 27] {
            let at = {
                let snap = shared.read().expect("poisoned");
                snap.rope().point_to_offset(Point::new(row, 2))
            };
            shared.write().expect("poisoned").edit(at..at + 2, "QQ");
        }

        let fresh_rows = {
            let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());
            let mut fresh = DisplayMap::new(multi, test_executor(), crate::test_notify());
            fresh.insert_blocks(block());
            rows(&mut fresh)
        };
        assert!(
            fresh_rows.contains(&"replacement".to_string()),
            "the fresh build has to show the block, or this proves nothing",
        );
        assert_eq!(rows(&mut map), fresh_rows, "against a fresh build");
    }

    /// An edit strictly inside a replacement's hidden rows leaves the block
    /// standing.
    ///
    /// Such an edit moves neither endpoint, so nothing carries the block over
    /// and the region has to be rebuilt. The rebuild begins wherever the edit
    /// does, which is inside the block, so the rows it hides get re-emitted as
    /// ordinary text and the block itself is never put back.
    #[test]
    fn an_edit_inside_a_replacement_keeps_the_block() {
        let text: String = (0..10).map(|i| format!("line{i}\n")).collect();

        let block = || {
            vec![BlockProperties::from_text(
                BlockPlacement::Replace { start: 2, end: 6 },
                vec!["replacement".to_string()],
                BlockStyle::Fixed,
            )]
        };

        let rows = |map: &mut DisplayMap| {
            let snapshot = map.snapshot();
            (0..snapshot.line_count())
                .map(|row| match snapshot.classify_row(row) {
                    BlockRowKind::BufferRow { buffer_row } => format!("buf{buffer_row}"),
                    BlockRowKind::Block { block, line_index } => {
                        block.get_line(line_index).to_string()
                    },
                })
                .collect::<Vec<_>>()
        };

        let fresh = |content: &str| {
            let shared = Arc::new(RwLock::new(TextBuffer::with_text(
                BufferId::new(0),
                content,
            )));
            let multi = MultiBuffer::singleton(BufferId::new(0), shared);
            let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());
            map.insert_blocks(block());
            rows(&mut map)
        };

        let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
        display_map.insert_blocks(block());
        assert_eq!(rows(&mut display_map), fresh(&text), "before any edit");

        // Row 4 is the third of the five hidden rows, so this touches neither
        // endpoint of the replacement. Same length, so no row count changes.
        let edited = text.replace("line4", "LINE4");
        shared.write().expect("poisoned").edit(24..29, "LINE4");
        assert_eq!(
            rows(&mut display_map),
            fresh(&edited),
            "an edit between the replacement's endpoints leaves it standing",
        );
    }

    /// A block marks a row, not a row number, so an edit that moves the text
    /// under it has to move the block with it. Otherwise it stays where the
    /// row used to be and marks whatever slid into its place, and only the
    /// block's owner re-inserting it can put it back.
    #[test]
    fn blocks_follow_the_row_they_mark_across_an_edit() {
        let text: String = (0..20).map(|i| format!("line{i}\n")).collect();
        let buffer = TextBuffer::with_text(BufferId::new(0), &text);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

        display_map.insert_blocks(vec![BlockProperties::from_text(
            BlockPlacement::Below(10),
            vec!["marker".to_string()],
            BlockStyle::Fixed,
        )]);

        let block_row = |map: &mut DisplayMap| {
            let snapshot = map.snapshot();
            (0..snapshot.line_count())
                .find(|row| {
                    matches!(
                        snapshot.classify_row(*row),
                        BlockRowKind::Block { block, line_index }
                            if block.get_line(line_index) == "marker"
                    )
                })
                .expect("the marker block is rendered")
        };

        assert_eq!(
            block_row(&mut display_map),
            11,
            "below line10 to begin with"
        );

        shared.write().expect("poisoned").edit(0..0, "a\nb\nc\n");

        assert_eq!(
            block_row(&mut display_map),
            14,
            "three rows inserted above it push the block down with line10",
        );

        // "a\nb\nc\n" back off the front again.
        shared.write().expect("poisoned").edit(0..6, "");

        assert_eq!(
            block_row(&mut display_map),
            11,
            "deleting those rows pulls it back up",
        );
    }

    /// The rows a block was attached to can be replaced wholesale, leaving it
    /// nothing to point at. It collapses to the start of the replacement rather
    /// than keeping a row number that now belongs to unrelated text.
    #[test]
    fn a_block_inside_a_replaced_range_lands_at_the_replacement() {
        let text: String = (0..20).map(|i| format!("line{i}\n")).collect();
        let buffer = TextBuffer::with_text(BufferId::new(0), &text);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let rows = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

        display_map.insert_blocks(vec![BlockProperties::from_text(
            BlockPlacement::Below(10),
            vec!["marker".to_string()],
            BlockStyle::Fixed,
        )]);
        assert_eq!(display_map.snapshot().line_count(), 22);

        // Rows 8 through 12 replaced by a single line.
        let (start, end) = {
            let snapshot = rows.snapshot();
            let rope = snapshot.rope();
            (
                rope.point_to_offset(Point::new(8, 0)),
                rope.point_to_offset(Point::new(13, 0)),
            )
        };
        shared.write().expect("poisoned").edit(start..end, "only\n");

        let snapshot = display_map.snapshot();
        let block_row = (0..snapshot.line_count())
            .find(|row| {
                matches!(
                    snapshot.classify_row(*row),
                    BlockRowKind::Block { block, line_index }
                        if block.get_line(line_index) == "marker"
                )
            })
            .expect("the marker block is still rendered");

        assert_eq!(
            block_row, 9,
            "the block sits just below row 8, where its rows were replaced",
        );
    }

    #[test]
    fn max_point() {
        let mut display_map = create_display_map("short\nlonger line\nx");
        let snapshot = display_map.snapshot();

        let max = snapshot.max_point();
        assert_eq!(max.row, 2);
        assert_eq!(max.column, 1);
    }

    #[test]
    fn display_row_default() {
        let row = DisplayRow::default();
        assert_eq!(row.0, 0);
    }

    #[test]
    fn line_count_includes_deleted() {
        let base = "line1\ndeleted\nline2";
        let diff = make_diff_with_deletion(0, base, 6..13, 1);
        let mut display_map = create_display_map_with_diff("line1\nline2", diff);
        let snapshot = display_map.snapshot();

        assert_eq!(snapshot.line_count(), 3);
        assert_eq!(snapshot.buffer_line_count(), 2);
    }

    #[test]
    fn deleted_blocks_hidden_when_flag_off() {
        let base = "line1\ndeleted\nline2";
        let diff = make_diff_with_deletion(0, base, 6..13, 1);
        let mut buffer = TextBuffer::with_text(BufferId::new(0), "line1\nline2");
        buffer.diff_map = Some(diff);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
        let snapshot = display_map.snapshot();

        assert_eq!(
            snapshot.line_count(),
            snapshot.buffer_line_count(),
            "with show_deleted_blocks off, a deletion diff splices no block rows",
        );
    }

    #[test]
    fn classify_deleted_row() {
        let base = "line1\ndeleted\nline2";
        let diff = make_diff_with_deletion(0, base, 6..13, 1);
        let mut display_map = create_display_map_with_diff("line1\nline2", diff);
        let snapshot = display_map.snapshot();

        match snapshot.classify_row(1) {
            BlockRowKind::Block { block, line_index } => {
                assert_eq!(block.get_line(line_index), "deleted");
            },
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn roundtrip_with_tabs() {
        let mut display_map = create_display_map("\thello");
        let snapshot = display_map.snapshot();

        let display = snapshot.buffer_to_display(Point::new(0, 1));
        assert_eq!(display, DisplayPoint::new(0, 4));

        let back = snapshot.display_to_buffer(display).unwrap();
        assert_eq!(back, Point::new(0, 1));

        let display5 = DisplayPoint::new(0, 5);
        let back5 = snapshot.display_to_buffer(display5).unwrap();
        assert_eq!(back5, Point::new(0, 2));
    }

    #[test]
    fn roundtrip_with_folds() {
        let mut display_map = create_display_map("fn main() {\n    body;\n}");
        display_map.fold(vec![Point::new(0, 11)..Point::new(2, 0)]);
        let snapshot = display_map.snapshot();

        let display = snapshot.buffer_to_display(Point::new(2, 1));
        let back = snapshot.display_to_buffer(display).unwrap();
        assert_eq!(back, Point::new(2, 1));
    }

    #[test]
    fn line_len_display() {
        let mut display_map = create_display_map("\thello\nworld");
        let snapshot = display_map.snapshot();

        assert_eq!(snapshot.line_len(0), 9);
        assert_eq!(snapshot.line_len(1), 5);
    }

    #[test]
    fn clip_point_clamps() {
        use stoat_text::Bias;
        let mut display_map = create_display_map("hello\nhi");
        let snapshot = display_map.snapshot();

        assert_eq!(
            snapshot.clip_point(DisplayPoint::new(0, 100), Bias::Left),
            DisplayPoint::new(0, 5)
        );
        assert_eq!(
            snapshot.clip_point(DisplayPoint::new(10, 0), Bias::Left),
            DisplayPoint::new(1, 0)
        );
    }

    #[test]
    fn clip_point_moves_off_a_fold_placeholder() {
        use stoat_text::Bias;
        let mut display_map = create_display_map("fn main() {\n    body;\n}");
        display_map.toggle_fold(vec![Point::new(0, 11)..Point::new(2, 0)]);
        let snapshot = display_map.snapshot();

        assert_eq!(
            snapshot.clip_point(DisplayPoint::new(0, 12), Bias::Left),
            DisplayPoint::new(0, 11),
            "a column inside the placeholder falls back to where the fold opens",
        );
        assert_eq!(
            snapshot.clip_point(DisplayPoint::new(0, 12), Bias::Right),
            DisplayPoint::new(0, 14),
            "and forward to the character the fold closes before",
        );
    }

    #[test]
    fn clip_point_moves_off_a_tab_expansion() {
        use stoat_text::Bias;
        let mut display_map = create_display_map("\thello");
        let snapshot = display_map.snapshot();

        assert_eq!(
            snapshot.clip_point(DisplayPoint::new(0, 2), Bias::Left),
            DisplayPoint::new(0, 0),
            "a column among the tab's cells falls back to the tab",
        );
        assert_eq!(
            snapshot.clip_point(DisplayPoint::new(0, 2), Bias::Right),
            DisplayPoint::new(0, 4),
            "and forward to the stop it runs to",
        );
    }

    #[test]
    fn clip_point_moves_out_of_the_soft_wrap_indent() {
        use stoat_text::Bias;
        let mut display_map = create_display_map("    hello world example text");
        display_map.set_wrap_width(Some(12));
        let snapshot = display_map.snapshot();

        let indent = snapshot.soft_wrap_indent(1);
        assert!(indent > 0, "the continuation row carries an indent");
        assert_eq!(
            snapshot.clip_point(DisplayPoint::new(1, 0), Bias::Left),
            DisplayPoint::new(1, indent),
            "the margin holds no text, so a column in it clamps to the first cell",
        );
    }

    /// Clipping names a position the rest of the display map agrees exists.
    /// A clipped point converts to a buffer point and back to itself, and
    /// clipping it again leaves it alone.
    #[test]
    fn clip_point_settles_on_an_addressable_position() {
        use stoat_text::Bias;
        let mut display_map = create_display_map("fn main() {\n\tbody;\n}");
        display_map.toggle_fold(vec![Point::new(1, 1)..Point::new(1, 5)]);
        let snapshot = display_map.snapshot();

        for row in 0..snapshot.line_count() {
            for column in 0..=snapshot.line_len(row) + 1 {
                for bias in [Bias::Left, Bias::Right] {
                    let clipped = snapshot.clip_point(DisplayPoint::new(row, column), bias);
                    assert_eq!(
                        snapshot.clip_point(clipped, bias),
                        clipped,
                        "clipping {row}:{column} again moves it",
                    );

                    let buffer_point = snapshot
                        .display_to_buffer(clipped)
                        .unwrap_or_else(|| panic!("{row}:{column} clips off the buffer"));
                    assert_eq!(
                        snapshot.buffer_to_display(buffer_point),
                        clipped,
                        "{row}:{column} does not survive the buffer round trip",
                    );
                }
            }
        }
    }

    #[test]
    fn toggle_fold_folds_then_unfolds() {
        let mut display_map = create_display_map("fn main() {\n    body;\n}");
        let range = vec![Point::new(0, 11)..Point::new(2, 0)];

        display_map.toggle_fold(range.clone());
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.line_count(), 1);

        display_map.toggle_fold(range);
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.line_count(), 3);
    }

    /// The chunk stream is what every paint reads, so any change to how it is
    /// walked has to leave it identical. Each layer contributes its own way for
    /// a restart to differ from a continuation. A wrapped row splits mid-chunk,
    /// a fold and a hint move the offsets the layers below are addressed by, and
    /// a block interrupts the run entirely.
    #[test]
    fn the_chunk_stream_over_wraps_folds_inlays_and_blocks() {
        // Row 1 is indented, so its continuation rows carry an indent. Row 3 is
        // long enough to wrap after the fold collapses part of it.
        let text = "fn alpha() { let x = 1; }\n\
                    \x20   indented line that wraps a few times\n\
                    short\n\
                    fn gamma() { let z = 3; } and more text\n";
        let buffer = TextBuffer::with_text(BufferId::new(0), text);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

        let hint_at = {
            let snap = display_map.multi_buffer.snapshot();
            snap.anchor_at(
                snap.rope().point_to_offset(Point::new(0, 12)),
                stoat_text::Bias::Right,
            )
        };
        display_map.splice_inlays(
            Vec::new(),
            vec![(hint_at, ": u32".to_string(), InlayKind::Hint)],
        );
        display_map.fold(vec![Point::new(3, 12)..Point::new(3, 24)]);
        display_map.insert_blocks(vec![BlockProperties::from_text(
            BlockPlacement::Below(1),
            vec!["a block row".to_string(), "and another".to_string()],
            BlockStyle::Fixed,
        )]);
        display_map.set_wrap_width(Some(14));

        let snapshot = display_map.snapshot();
        let stream: Vec<String> = snapshot
            .highlighted_chunks(0..snapshot.line_count())
            .map(|chunk| {
                format!(
                    "{:?}{}{}",
                    chunk.text,
                    if chunk.is_inlay { " inlay" } else { "" },
                    if chunk.is_tab { " tab" } else { "" },
                )
            })
            .collect();

        assert_eq!(
            stream.join("\n"),
            [
                // Row 0 wraps mid-hint, so the hint arrives as two chunks
                // either side of the break.
                r#""fn alpha() {""#,
                r#"":" inlay"#,
                r#""\n""#,
                r#"" u32" inlay"#,
                r#"" let x = 1; }""#,
                r#""\n""#,
                // Row 1's first sub-row, then the block splitting it.
                r#""    indented ""#,
                r#""\n""#,
                r#""a block row""#,
                r#""\n""#,
                r#""and another""#,
                r#""\n""#,
                // Its remaining sub-rows, each opening with the carried indent.
                r#""    ""#,
                r#""line that ""#,
                r#""\n""#,
                r#""    ""#,
                r#""wraps a ""#,
                r#""\n""#,
                r#""    ""#,
                r#""few times""#,
                r#""\n""#,
                r#""short""#,
                r#""\n""#,
                // Row 3, whose fold placeholder is a chunk of its own.
                r#""fn gamma() ""#,
                r#""\n""#,
                r#""{""#,
                r#""...""#,
                r#""} and ""#,
                r#""\n""#,
                r#""more text""#,
                r#""\n""#,
            ]
            .join("\n"),
        );
    }

    #[test]
    fn wrap_width_none_by_default() {
        let mut display_map = create_display_map("hello");
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.wrap_width(), None);
    }

    #[test]
    fn wrap_width_after_set() {
        let mut display_map = create_display_map("hello");
        display_map.set_wrap_width(Some(40));
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.wrap_width(), Some(40));
    }

    /// A large edit batch hands its rewrap to the background and shows the
    /// interpolated wrapping meanwhile, so long lines render unwrapped. The
    /// settled result moves no buffer, fold, or inlay version, so it lands only
    /// if the snapshot path re-syncs while the rewrap is outstanding.
    #[test]
    fn a_large_edit_settles_its_wrapping_without_another_edit() {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let redraw = Arc::new(tokio::sync::Notify::new());

        // Lines long enough to wrap several times each, and more than
        // WRAP_SYNC_THRESHOLD of them, so the edit takes the background path.
        let line = "the quick brown fox jumps over the lazy dog ".repeat(3);
        let pasted: String = std::iter::repeat_n(line.as_str(), 150)
            .collect::<Vec<_>>()
            .join("\n");

        let buffer = TextBuffer::with_text(BufferId::new(0), "start\n");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, executor.clone(), redraw.clone());
        display_map.set_wrap_width(Some(30));
        let before = display_map.snapshot().max_point().row;

        shared
            .write()
            .expect("poisoned")
            .edit(6..6, pasted.as_str());
        let interim = display_map.snapshot().max_point().row;
        assert!(
            display_map.wrap_map.background_pending(),
            "a 150-row paste must hand its rewrap to the background",
        );
        assert!(
            interim > before,
            "the interpolated snapshot still grows by the pasted rows",
        );

        scheduler.run_until_parked();
        assert!(
            notified_now(&redraw),
            "the settled rewrap must wake the run loop, since no version change will",
        );

        // No further edit arrives, so the next snapshot alone has to pick it up.
        let settled = display_map.snapshot().max_point().row;
        assert!(
            !display_map.wrap_map.background_pending(),
            "the finished rewrap must land on the next snapshot",
        );

        let mut fresh = {
            let buffer = TextBuffer::with_text(BufferId::new(0), &format!("start\n{pasted}"));
            let shared = Arc::new(RwLock::new(buffer));
            let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
            DisplayMap::new(multi_buffer, executor, crate::test_notify())
        };
        fresh.set_wrap_width(Some(30));
        assert_eq!(
            settled,
            fresh.snapshot().max_point().row,
            "the settled wrapping must match a from-scratch build",
        );
        assert!(
            settled > interim,
            "wrapping the pasted long lines adds rows the interpolation lacked",
        );
    }

    /// A display map over `lines` copies of a line long enough to wrap, plus
    /// the scheduler driving its background work.
    fn wrappable_display_map(lines: usize) -> (Arc<TestScheduler>, DisplayMap) {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let line = "the quick brown fox jumps over the lazy dog ".repeat(3);
        let text: String = std::iter::repeat_n(line.as_str(), lines)
            .collect::<Vec<_>>()
            .join("\n");
        let buffer = TextBuffer::with_text(BufferId::new(0), &text);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        (
            scheduler,
            DisplayMap::new(multi_buffer, executor, crate::test_notify()),
        )
    }

    /// Rows the same content wraps to at `width`, built from nothing.
    fn rows_at_width(lines: usize, width: u32) -> u32 {
        let (_scheduler, mut display_map) = wrappable_display_map(lines);
        display_map.set_wrap_width(Some(width));
        display_map.snapshot().max_point().row
    }

    /// Dragging a pane edge emits a width change per resize event, and
    /// rewrapping a large file is the same O(file) walk a large edit batch
    /// already defers, so it must not run on the UI thread either.
    #[test]
    fn a_wide_files_width_change_rewraps_in_the_background() {
        const LINES: usize = 150;
        let (scheduler, mut display_map) = wrappable_display_map(LINES);
        display_map.set_wrap_width(Some(60));
        let at_60 = display_map.snapshot().max_point().row;

        display_map.set_wrap_width(Some(20));
        let interim = display_map.snapshot().max_point().row;
        assert!(
            display_map.wrap_map.background_pending(),
            "a large file's width change must not rewrap on the UI thread",
        );
        assert_eq!(
            interim, at_60,
            "the immediate snapshot still carries the previous width's wrapping",
        );

        scheduler.run_until_parked();
        let settled = display_map.snapshot().max_point().row;
        assert!(
            !display_map.wrap_map.background_pending(),
            "the finished rewrap lands on the next snapshot",
        );
        assert_eq!(
            settled,
            rows_at_width(LINES, 20),
            "the settled wrapping matches a from-scratch build at the new width",
        );
        assert!(settled > at_60, "a narrower width wraps into more rows");
    }

    /// Below the threshold the inline rebuild is cheaper than a task, so the
    /// new width has to be on screen the moment it is set.
    #[test]
    fn a_small_files_width_change_rewraps_synchronously() {
        const LINES: usize = 4;
        let (_scheduler, mut display_map) = wrappable_display_map(LINES);
        display_map.set_wrap_width(Some(60));
        display_map.snapshot();

        display_map.set_wrap_width(Some(20));
        let rows = display_map.snapshot().max_point().row;
        assert!(
            !display_map.wrap_map.background_pending(),
            "a small file rewraps inline, leaving nothing outstanding",
        );
        assert_eq!(
            rows,
            rows_at_width(LINES, 20),
            "and the new width is already in the first snapshot after the change",
        );
    }

    /// A drag emits many widths in a row. Each intermediate one may be
    /// abandoned, but the last must be what the display settles on.
    #[test]
    fn successive_width_changes_settle_on_the_last() {
        const LINES: usize = 150;
        let (scheduler, mut display_map) = wrappable_display_map(LINES);
        display_map.set_wrap_width(Some(60));
        display_map.snapshot();

        for width in [50, 40, 30, 20] {
            display_map.set_wrap_width(Some(width));
            display_map.snapshot();
        }

        // Successive rewraps chain through the pending queue, so drain and
        // re-sync until nothing is outstanding rather than assuming one pass.
        for _ in 0..10 {
            scheduler.run_until_parked();
            display_map.snapshot();
            if !display_map.wrap_map.background_pending() {
                break;
            }
        }

        assert!(
            !display_map.wrap_map.background_pending(),
            "the chain of width changes must converge",
        );
        assert_eq!(
            display_map.snapshot().max_point().row,
            rows_at_width(LINES, 20),
            "the display settles on the last width, not an abandoned one",
        );
    }

    /// Whether `notify` is holding a permit, without awaiting one.
    ///
    /// `notify_one` stores a permit when nobody is waiting, so a `notified()`
    /// future polled afterwards completes at once. Polling it a single time is
    /// how a synchronous test observes that the wake happened.
    fn notified_now(notify: &Arc<tokio::sync::Notify>) -> bool {
        let mut fut = Box::pin(notify.notified());
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        fut.as_mut().poll(&mut cx).is_ready()
    }

    #[test]
    fn a_highlight_change_keeps_the_layers_it_did_not_touch() {
        // A parse installs a token channel on every keystroke in a highlighted
        // file, so this is the difference between a typed character paying the
        // five-layer pipeline once or twice.
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "fn alpha() {}\nfn beta() {}\n",
        )));
        let mut display_map = DisplayMap::new(
            MultiBuffer::singleton(BufferId::new(0), shared.clone()),
            test_executor(),
            crate::test_notify(),
        );

        let (tokens, interner) = {
            let snap = display_map.multi_buffer.snapshot();
            let mut interner = HighlightStyleInterner::default();
            let style = interner.push(HighlightStyle::default());
            let tokens: Arc<[SemanticTokenHighlight]> = [(3usize, 8usize)]
                .iter()
                .map(|&(start, end)| SemanticTokenHighlight {
                    range: snap.anchor_at(start, stoat_text::Bias::Right)
                        ..snap.anchor_at(end, stoat_text::Bias::Left),
                    style,
                })
                .collect();
            (tokens, Arc::new(interner))
        };

        // The block snapshot derefs through an Arc, so the address of what it
        // points at says whether the layers below were rebuilt or handed over.
        let layers = |snapshot: &DisplaySnapshot| std::ptr::from_ref(&*snapshot.block_snapshot);

        let before = display_map.snapshot();
        assert!(
            display_map
                .semantic_token_highlights
                .get(&BufferId::new(0))
                .is_none(),
            "nothing is installed yet",
        );

        display_map.set_semantic_token_highlights(BufferId::new(0), tokens, interner);
        let after = display_map.snapshot();

        assert_eq!(
            layers(&before),
            layers(&after),
            "no edit happened, so the layers are the same ones",
        );
        assert!(
            after
                .semantic_token_highlights
                .get(&BufferId::new(0))
                .is_some(),
            "and the snapshot carries the highlights that were installed",
        );
    }

    #[test]
    fn an_unfold_that_removes_nothing_costs_no_wrap_rebuild() {
        // What the version bump actually buys downstream. A fold-version change
        // with no text edit alongside is WrapMap::sync's whole-file rebuild
        // condition, and the whole-range patch it emits invalidates the block
        // map and every render cache below it.
        let mut display_map = create_display_map("line0\nline1\nline2\nline3");
        let _ = display_map.snapshot();

        display_map.unfold(vec![Point::new(0, 0)..Point::new(3, 5)]);
        let (_, wrap_edits, _, _) = display_map.sync_through_wrap();
        assert!(
            wrap_edits.is_empty(),
            "no fold was there to remove: {:?}",
            wrap_edits.edits(),
        );

        // The same call after a fold that does land reports the rebuild, so the
        // emptiness above is the unfold being a no-op rather than this sync
        // never reporting anything.
        display_map.fold(vec![Point::new(2, 0)..Point::new(2, 5)]);
        let (_, wrap_edits, _, _) = display_map.sync_through_wrap();
        assert!(!wrap_edits.is_empty(), "a fold that lands rebuilds");
    }

    #[test]
    fn is_line_folded_through_display() {
        let mut display_map = create_display_map("line0\nline1\nline2\nline3");
        display_map.fold(vec![Point::new(1, 0)..Point::new(2, 5)]);
        let snapshot = display_map.snapshot();
        assert!(!snapshot.is_line_folded(0));
        assert!(snapshot.is_line_folded(1));
        assert!(snapshot.is_line_folded(2));
        assert!(!snapshot.is_line_folded(3));
    }

    #[test]
    fn buffer_chars_at_simple() {
        let mut display_map = create_display_map("hello");
        let snapshot = display_map.snapshot();
        let chars: Vec<(char, Point)> = snapshot.buffer_chars_at(Point::new(0, 0)).collect();
        assert_eq!(
            chars,
            vec![
                ('h', Point::new(0, 0)),
                ('e', Point::new(0, 1)),
                ('l', Point::new(0, 2)),
                ('l', Point::new(0, 3)),
                ('o', Point::new(0, 4)),
            ]
        );
    }

    #[test]
    fn buffer_chars_at_multiline() {
        let mut display_map = create_display_map("ab\ncd");
        let snapshot = display_map.snapshot();
        let chars: Vec<(char, Point)> = snapshot.buffer_chars_at(Point::new(0, 0)).collect();
        assert_eq!(
            chars,
            vec![
                ('a', Point::new(0, 0)),
                ('b', Point::new(0, 1)),
                ('\n', Point::new(0, 2)),
                ('c', Point::new(1, 0)),
                ('d', Point::new(1, 1)),
            ]
        );
    }

    #[test]
    fn reverse_buffer_chars_at_simple() {
        let mut display_map = create_display_map("hello");
        let snapshot = display_map.snapshot();
        let chars: Vec<(char, Point)> =
            snapshot.reverse_buffer_chars_at(Point::new(0, 5)).collect();
        assert_eq!(
            chars,
            vec![
                ('o', Point::new(0, 4)),
                ('l', Point::new(0, 3)),
                ('l', Point::new(0, 2)),
                ('e', Point::new(0, 1)),
                ('h', Point::new(0, 0)),
            ]
        );
    }

    #[test]
    fn reverse_buffer_chars_at_multiline() {
        let mut display_map = create_display_map("ab\ncd");
        let snapshot = display_map.snapshot();
        let chars: Vec<(char, Point)> =
            snapshot.reverse_buffer_chars_at(Point::new(1, 2)).collect();
        assert_eq!(
            chars,
            vec![
                ('d', Point::new(1, 1)),
                ('c', Point::new(1, 0)),
                ('\n', Point::new(0, 2)),
                ('b', Point::new(0, 1)),
                ('a', Point::new(0, 0)),
            ]
        );
    }

    #[test]
    fn prev_line_boundary_test() {
        let mut display_map = create_display_map("hello\nworld");
        let snapshot = display_map.snapshot();
        let (buf, display) = snapshot.prev_line_boundary(Point::new(1, 3));
        assert_eq!(buf, Point::new(1, 0));
        assert_eq!(display, DisplayPoint::new(1, 0));
    }

    #[test]
    fn next_line_boundary_test() {
        let mut display_map = create_display_map("hello\nworld");
        let snapshot = display_map.snapshot();
        let (buf, display) = snapshot.next_line_boundary(Point::new(0, 2));
        assert_eq!(buf, Point::new(0, 5));
        assert_eq!(display, DisplayPoint::new(0, 5));
    }

    #[test]
    fn clip_at_line_end_test() {
        let mut display_map = create_display_map("hello\nhi");
        let snapshot = display_map.snapshot();
        let clipped = snapshot.clip_at_line_end(DisplayPoint::new(0, 100));
        assert_eq!(clipped, DisplayPoint::new(0, 5));
    }

    #[test]
    fn inlay_survives_compaction() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "hello world");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

        let snap = display_map.multi_buffer.snapshot();
        let off = snap.rope().point_to_offset(Point::new(0, 5));
        let anchor = snap.anchor_at(off, stoat_text::Bias::Right);
        display_map.inlay_map.splice(
            &snap,
            Vec::new(),
            vec![(anchor, ": str".to_string(), InlayKind::Hint)],
        );

        for i in 0..10 {
            {
                let mut buf = shared.write().unwrap();
                let prefix = format!("{i}");
                buf.edit(0..0, &prefix);
            }
            let _ = display_map.snapshot();
        }

        let snapshot = display_map.snapshot();
        let inlay_snap = snapshot.inlay_snapshot();
        assert_eq!(
            inlay_snap.to_inlay_point(Point::new(0, 15)),
            InlayPoint::new(0, 20)
        );
    }

    #[test]
    fn mid_line_inlay_keeps_trailing_buffer_text() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "let x = 1\n");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

        let anchor = {
            let snap = display_map.multi_buffer.snapshot();
            let off = snap.rope().point_to_offset(Point::new(0, 5));
            snap.anchor_at(off, stoat_text::Bias::Left)
        };
        {
            let snap = display_map.multi_buffer.snapshot();
            display_map.inlay_map.splice(
                &snap,
                Vec::new(),
                vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
            );
        }

        let snapshot = display_map.snapshot();
        let chunks: Vec<_> = snapshot.highlighted_chunks(0..1).collect();
        let text: String = chunks.iter().map(|c| c.text.as_ref()).collect();
        assert_eq!(text, "let x: u32 = 1");
    }

    /// The rows a splice patches have to arrive intact at the far end of the
    /// pipeline, where the fold, wrap, and block layers each sync against the
    /// inlay patch rather than rebuilding. A hint landing deep in a file is
    /// where a mis-scoped patch would leave the wrong row painted.
    #[test]
    fn a_spliced_hint_paints_through_every_layer() {
        // A fixed-width name puts the hint's column at the same place on every
        // row, so the assertions below name one column rather than three.
        let text: String = (0..200).map(|i| format!("let x{i:03} = {i}\n")).collect();
        let buffer = TextBuffer::with_text(BufferId::new(0), &text);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
        display_map.snapshot();

        let hint_at = |display_map: &DisplayMap, row: u32| {
            let snap = display_map.multi_buffer.snapshot();
            let off = snap.rope().point_to_offset(Point::new(row, 8));
            snap.anchor_at(off, stoat_text::Bias::Left)
        };

        let row_text = |display_map: &mut DisplayMap, row: u32| -> String {
            let snapshot = display_map.snapshot();
            snapshot
                .highlighted_chunks(row..row + 1)
                .map(|chunk| chunk.text.to_string())
                .collect()
        };

        let anchor = hint_at(&display_map, 150);
        let ids = display_map.splice_inlays(
            Vec::new(),
            vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
        );
        assert_eq!(row_text(&mut display_map, 150), "let x150: u32 = 150");
        assert_eq!(row_text(&mut display_map, 149), "let x149 = 149");

        // Replacing the set is what an unchanged hint refresh looks like, and it
        // has to leave the same text behind.
        let anchor = hint_at(&display_map, 150);
        display_map.splice_inlays(ids, vec![(anchor, ": u32".to_string(), InlayKind::Hint)]);
        assert_eq!(row_text(&mut display_map, 150), "let x150: u32 = 150");
    }

    /// A splice resolves its offsets against the buffer as it stands, and the
    /// inlay layer can only place them while its own text still reads the same.
    /// An edit arriving first strands them, and the layer falls back to marking
    /// every row for rebuild, which each layer downstream then repeats.
    #[test]
    fn an_edit_after_a_splice_rebuilds_only_the_rows_it_touched() {
        let text: String = (0..200).map(|i| format!("let x{i:03} = {i}\n")).collect();
        let buffer = TextBuffer::with_text(BufferId::new(0), &text);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
        display_map.snapshot();

        let anchor = {
            let snap = display_map.multi_buffer.snapshot();
            let offset = snap.rope().point_to_offset(Point::new(150, 8));
            snap.anchor_at(offset, stoat_text::Bias::Left)
        };
        display_map.splice_inlays(
            Vec::new(),
            vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
        );

        shared.write().expect("poisoned").edit(0..0, "x");

        let (wrap_snapshot, wrap_edits, _, _) = display_map.sync_through_wrap();
        assert!(!wrap_edits.is_empty(), "the edit itself must be reported");
        assert!(
            !wrap_edits
                .edits()
                .iter()
                .any(|edit| edit.new.contains(&150)),
            "an edit on the first row must not rebuild row 150: {:?}",
            wrap_edits.edits(),
        );
        let painted: String = wrap_snapshot
            .chunks(150..151, Arc::from(Vec::new()))
            .map(|chunk| chunk.text.to_string())
            .collect();
        assert_eq!(
            painted, "let x150: u32 = 150",
            "the spliced hint survives the edit",
        );
    }

    #[test]
    fn soft_wrap_indent_exposed() {
        let mut display_map = create_display_map("    hello world foo");
        display_map.set_wrap_width(Some(8));
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.soft_wrap_indent(0), 0);
        if snapshot.line_count() > 1 {
            assert_eq!(snapshot.soft_wrap_indent(1), 4);
        }
    }

    #[test]
    fn display_lines_empty_range() {
        let mut display_map = create_display_map("hello\nworld");
        let snapshot = display_map.snapshot();
        let lines: Vec<String> = snapshot.display_lines(0..0).collect();
        assert!(lines.is_empty());
    }

    #[test]
    fn display_lines_multi_line() {
        let mut display_map = create_display_map("hello\nworld\nfoo");
        let snapshot = display_map.snapshot();
        let lines: Vec<String> = snapshot.display_lines(0..3).collect();
        assert_eq!(lines, vec!["hello", "world", "foo"]);
    }

    #[test]
    fn cjk_wide_chars_display_width() {
        let mut display_map = create_display_map("ab\u{4f60}\u{597d}cd");
        let snapshot = display_map.snapshot();
        // "ab" = 2, "你" = 2, "好" = 2, "cd" = 2 => total 8
        assert_eq!(snapshot.line_len(0), 8);
    }

    #[test]
    fn cjk_wrap_at_correct_column() {
        let mut display_map = create_display_map("ab\u{4f60}\u{597d}cd");
        display_map.set_wrap_width(Some(5));
        let snapshot = display_map.snapshot();
        // "ab你" = 4 cols, "好cd" = 4 cols -> wraps after 你
        assert_eq!(snapshot.line_count(), 2);
    }

    #[test]
    fn write_display_line_matches_display_line() {
        let base = "line1\ndeleted\nline2";
        let diff = make_diff_with_deletion(0, base, 6..13, 1);
        let mut display_map = create_display_map_with_diff("line1\nline2", diff);
        let snapshot = display_map.snapshot();
        for row in 0..snapshot.line_count() {
            let expected = snapshot.display_line(row);
            let mut buf = String::new();
            snapshot.write_display_line(&mut buf, row);
            assert_eq!(buf, expected, "mismatch at row {row}");
        }
    }

    #[test]
    fn chunks_match_display_lines() {
        let mut display_map = create_display_map("hello\nworld\nfoo bar");
        let snapshot = display_map.snapshot();
        let total = snapshot.line_count();

        let chunks: Vec<_> = snapshot.highlighted_chunks(0..total).collect();
        let from_chunks: String = chunks.iter().map(|c| c.text.as_ref()).collect();
        let from_lines: String = (0..total)
            .map(|r| snapshot.display_line(r))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(from_chunks, from_lines);
    }

    #[test]
    fn chunks_with_blocks_match_display_lines() {
        let diff = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Deleted,
                unstaged_lines: std::iter::once(2..2).collect(),
                buffer_start_line: 2,
                buffer_line_range: 2..2,
                base_byte_range: 0..7,
                anchor_range: None,
                token_detail: None,
            }],
            Some(Arc::new("deleted".to_string())),
        );
        let mut display_map = create_display_map_with_diff("aaa\nbbb\nccc", diff);
        let snapshot = display_map.snapshot();
        let total = snapshot.line_count();

        let chunks: Vec<_> = snapshot.highlighted_chunks(0..total).collect();
        let from_chunks: String = chunks.iter().map(|c| c.text.as_ref()).collect();
        let from_lines: String = (0..total)
            .map(|r| snapshot.display_line(r))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(from_chunks, from_lines);
    }

    #[test]
    fn snapshot_open_rust_file_highlights() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 6);
        let path = h.write_file("sample.rs", "fn main() {\n    let x = \"hi\";\n}\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_open_rust_file_highlights");
    }

    #[test]
    fn snapshot_open_json_file_highlights() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 6);
        let path = h.write_file("sample.json", "{\n  \"a\": 1\n}\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_open_json_file_highlights");
    }

    #[test]
    fn snapshot_open_markdown_file_highlights() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 6);
        let path = h.write_file("sample.md", "# Title\n\nbody\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_open_markdown_file_highlights");
    }

    #[test]
    fn snapshot_open_markdown_file_with_bold_inline() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 6);
        let path = h.write_file("bold.md", "# Title\n\n**bold** text\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_open_markdown_file_with_bold_inline");
    }

    #[test]
    fn snapshot_open_unknown_extension_no_highlights() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 6);
        let path = h.write_file("sample.txt", "fn main() {}\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_open_unknown_extension_no_highlights");
    }

    #[test]
    fn snapshot_open_rust_file_nested_captures() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 6);
        let path = h.write_file("nested.rs", "fn main() { \"a\\nb\"; }\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_open_rust_file_nested_captures");
    }

    #[test]
    fn snapshot_open_rust_file_then_edit_highlights() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 6);
        let path = h.write_file("edit.rs", "fn a() {}\n");

        h.open_file(&path);
        h.edit_focused(8..8, " let x = 1; ");
        h.assert_snapshot("snapshot_open_rust_file_then_edit_highlights");
    }

    #[test]
    fn snapshot_rust_doc_comment_markdown() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 6);
        let path = h.write_file(
            "doc.rs",
            "/// A **bold** word and a [link](url).\nfn a() {}\n",
        );

        h.open_file(&path);
        h.assert_snapshot("snapshot_rust_doc_comment_markdown");
    }

    #[test]
    fn snapshot_markdown_fence_highlight() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 8);
        let path = h.write_file("doc.md", "# Title\n\n```rust\nfn a() {}\n```\n");

        h.open_file(&path);
        h.assert_snapshot("snapshot_markdown_fence_highlight");
    }

    #[test]
    fn snapshot_open_rust_file_with_fold() {
        use stoat_text::Point;
        let mut h = crate::test_harness::TestHarness::with_size(40, 8);
        let path = h.write_file("folded.rs", "fn a() { 1 }\nfn b() { 2 }\nfn c() { 3 }\n");

        h.open_file(&path);
        h.fold_focused(Point::new(1, 7)..Point::new(1, 12));
        h.assert_snapshot("snapshot_open_rust_file_with_fold");
    }

    /// Swapping the interner reaches the painted chunks, not just the stored
    /// channels.
    ///
    /// Endpoints bake a resolved style and are cached across frames, so a swap
    /// that updated the channels without invalidating that cache would leave
    /// the old colors on screen. This drives the same cache across the swap.
    #[test]
    fn swapping_the_interner_repaints_through_the_endpoint_cache() {
        use crate::display_map::highlights::{
            HighlightStyle, HighlightStyleInterner, SemanticTokenHighlight,
        };
        use ratatui::style::Color;

        let interner_with = |color: Color| {
            let mut interner = HighlightStyleInterner::default();
            interner.push(HighlightStyle {
                foreground: Some(color),
                ..Default::default()
            });
            Arc::new(interner)
        };

        let mut display_map = create_display_map("let x = 1\n");
        let style_id = {
            let mut probe = HighlightStyleInterner::default();
            probe.push(HighlightStyle::default())
        };
        let token = {
            let snap = display_map.multi_buffer.snapshot();
            let start = snap.anchor_at(0, stoat_text::Bias::Right);
            let end = snap.anchor_at(3, stoat_text::Bias::Left);
            SemanticTokenHighlight {
                range: start..end,
                style: style_id,
            }
        };
        display_map.set_semantic_token_highlights(
            BufferId::new(0),
            Arc::from(vec![token]),
            interner_with(Color::Red),
        );

        let mut cache = None;
        let painted = |display_map: &mut DisplayMap, cache: &mut Option<_>| {
            let snapshot = display_map.snapshot();
            snapshot
                .highlighted_chunks_cached(0..1, cache)
                .filter_map(|chunk| chunk.highlight_style?.foreground)
                .collect::<Vec<_>>()
        };

        assert_eq!(painted(&mut display_map, &mut cache), vec![Color::Red]);

        display_map.swap_style_interner(&interner_with(Color::Blue));
        assert_eq!(
            painted(&mut display_map, &mut cache),
            vec![Color::Blue],
            "the cached endpoints are rebuilt against the new interner"
        );
    }

    #[test]
    fn clearing_an_absent_highlight_key_leaves_the_map_alone() {
        use super::highlights::{HighlightKey, HighlightLayer, HighlightStyle};
        use stoat_text::Bias;

        let mut display_map = create_display_map("fn alpha() {}\n");
        let present = HighlightKey::layer(HighlightLayer::DocumentHighlightRead);
        let absent = HighlightKey::layer(HighlightLayer::DocumentHighlightWrite);

        let range = {
            let snap = display_map.multi_buffer.snapshot();
            snap.anchor_at(3, Bias::Right)..snap.anchor_at(8, Bias::Left)
        };
        display_map.highlight_text(present, vec![range], HighlightStyle::default());

        // A live snapshot is what puts the map's refcount above one, which is
        // the condition under which a mutable borrow would deep-clone it.
        let _snapshot = display_map.snapshot();
        display_map.highlights_dirty = false;
        let before = display_map.text_highlights.clone();

        assert!(
            !display_map.clear_highlights(absent),
            "nothing was stored under the absent key",
        );
        assert!(
            Arc::ptr_eq(&before, &display_map.text_highlights),
            "clearing an absent key must not rebuild the map",
        );
        assert!(
            !display_map.highlights_dirty,
            "a clear that removed nothing leaves the highlights clean",
        );

        assert!(
            display_map.clear_highlights(present),
            "the stored key still clears",
        );
        assert!(display_map.highlights_dirty, "a real clear marks dirty");
    }

    /// A keystroke takes six to eight snapshots, and each one used to walk and
    /// reallocate the whole nested inlay-highlight map. Sharing it makes a
    /// snapshot a refcount bump, and a highlight change still takes its own
    /// copy so no snapshot already handed out sees the mutation.
    #[test]
    fn snapshots_share_the_inlay_highlight_map_until_it_changes() {
        use super::highlights::{HighlightKey, HighlightLayer, HighlightStyle, InlayHighlight};
        use stoat_text::Bias;

        let mut display_map = create_display_map("fn alpha() {}\n");
        let anchor = display_map
            .multi_buffer
            .snapshot()
            .anchor_at(3, Bias::Right);
        let ids = display_map.splice_inlays(
            Vec::new(),
            vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
        );
        let inlay = *ids.first().expect("the splice added an inlay");

        let key = HighlightKey::layer(HighlightLayer::DocumentHighlightRead);
        display_map.highlight_inlays(
            key,
            vec![InlayHighlight { inlay, range: 0..5 }],
            HighlightStyle::default(),
        );

        let first = display_map.snapshot();
        assert_eq!(first.inlay_highlights().len(), 1, "the fixture stored one");
        assert!(
            Arc::ptr_eq(
                first.inlay_highlights(),
                display_map.snapshot().inlay_highlights()
            ),
            "a snapshot served from the cache shares the map rather than copying it",
        );

        // Splicing an inlay invalidates the cache without touching a highlight,
        // so the rebuild has to carry the same map forward rather than mint one.
        display_map.splice_inlays(
            Vec::new(),
            vec![(anchor, ": u8".to_string(), InlayKind::Hint)],
        );
        assert!(
            Arc::ptr_eq(
                first.inlay_highlights(),
                display_map.snapshot().inlay_highlights()
            ),
            "a rebuild that changed no highlight shares the map too",
        );

        assert!(display_map.clear_highlights(key), "the key was stored");
        let third = display_map.snapshot();
        assert!(
            !Arc::ptr_eq(first.inlay_highlights(), third.inlay_highlights()),
            "a highlight change copies rather than mutating a shared map",
        );
        assert_eq!(
            first.inlay_highlights().len(),
            1,
            "so a snapshot taken before the clear still sees the highlight",
        );
        assert!(
            third.inlay_highlights().is_empty(),
            "and one taken after sees it gone",
        );
    }

    #[test]
    fn highlight_text_orders_ranges_by_resolved_start() {
        use super::highlights::{HighlightKey, HighlightLayer, HighlightStyle};
        use stoat_text::{Anchor, Bias};

        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "aaa\nbbb\nccc\nddd\n",
        )));
        let mut display_map = DisplayMap::new(
            MultiBuffer::singleton(BufferId::new(0), shared.clone()),
            test_executor(),
            crate::test_notify(),
        );

        let ranges: Vec<Range<Anchor>> = {
            let snap = display_map.multi_buffer.snapshot();
            [(12usize, 15usize), (0, 3), (8, 11), (4, 7)]
                .iter()
                .map(|&(start, end)| {
                    snap.anchor_at(start, Bias::Right)..snap.anchor_at(end, Bias::Left)
                })
                .collect()
        };

        // Edited after the anchors were minted, so their stored offsets are no
        // longer where they resolve and the sort has to ask the buffer.
        {
            let mut buf = shared.write().expect("poisoned");
            buf.edit(0..0, "// header\n");
        }

        let key = HighlightKey::layer(HighlightLayer::DocumentHighlightRead);
        display_map.highlight_text(key, ranges, HighlightStyle::default());

        let snapshot = display_map.multi_buffer.snapshot();
        let stored = display_map
            .text_highlights
            .get(&key)
            .expect("the ranges are stored under their key");
        let starts: Vec<usize> = stored
            .1
            .iter()
            .map(|range| snapshot.resolve_anchor(&range.start))
            .collect();

        assert_eq!(
            starts,
            vec![10, 14, 18, 22],
            "ranges are stored ascending by resolved start, whatever order they arrived in",
        );
    }

    #[test]
    fn installing_tokens_indexes_them_like_a_per_anchor_build() {
        use super::highlights::{
            BufferSemanticTokens, HighlightStyle, HighlightStyleInterner, SemanticTokenHighlight,
        };
        use stoat_text::{Anchor, Bias};

        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n",
        )));
        let mut display_map = DisplayMap::new(
            MultiBuffer::singleton(BufferId::new(0), shared.clone()),
            test_executor(),
            crate::test_notify(),
        );

        let (tokens, interner) = {
            let snap = display_map.multi_buffer.snapshot();
            let mut interner = HighlightStyleInterner::default();
            let style = interner.push(HighlightStyle::default());
            // An enclosing token ahead of the three it contains, the order
            // captures() emits an outer node and its children in. Without one
            // the ends rise monotonically, the running argmax is the identity,
            // and the index cannot distinguish a correct build from a wrong one.
            let tokens: Arc<[SemanticTokenHighlight]> =
                [(0usize, 40usize), (3, 8), (17, 21), (30, 35)]
                    .iter()
                    .map(|&(start, end)| SemanticTokenHighlight {
                        range: snap.anchor_at(start, Bias::Right)..snap.anchor_at(end, Bias::Left),
                        style,
                    })
                    .collect();
            (tokens, Arc::new(interner))
        };

        // Fragment the buffer first. On an untouched one every anchor still
        // resolves to the offset it was minted from, so any way of building the
        // index agrees and the comparison below proves nothing.
        {
            let mut buf = shared.write().expect("poisoned");
            buf.edit(0..0, "// header\n");
            buf.edit(20..20, "  ");
        }

        display_map.set_semantic_token_highlights(
            BufferId::new(0),
            tokens.clone(),
            interner.clone(),
        );

        let snapshot = display_map.multi_buffer.snapshot();
        let resolve = |a: &Anchor| snapshot.resolve_anchor(a);
        let per_anchor = BufferSemanticTokens::new(tokens, interner, resolve);
        let installed = display_map
            .semantic_token_highlights
            .get(&BufferId::new(0))
            .expect("the channel is installed under its buffer id");

        let len = snapshot.rope().len();
        for start in 0..=len {
            for end in [start, start + 1, len] {
                let end = end.min(len);
                assert_eq!(
                    installed.overlap_bounds(&(start..end), resolve),
                    per_anchor.overlap_bounds(&(start..end), resolve),
                    "bounds must agree for {start}..{end}",
                );
            }
        }
    }
}
