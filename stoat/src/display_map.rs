mod block_map;
mod crease_map;
mod fold_map;
pub mod highlights;
pub mod inlay_map;
pub mod invisibles;
#[cfg(test)]
mod sampler;
pub mod syntax_theme;
pub mod tab_map;
mod wrap_map;

use crate::{
    buffer::BufferId,
    diff_map::{DiffHunkStatus, DiffMap, TokenDetail},
    host::DiffStatus,
    multi_buffer::{MultiBuffer, MultiBufferSnapshot},
};
pub use block_map::{
    Block, BlockChunks, BlockContext, BlockId, BlockMap, BlockPlacement, BlockPoint,
    BlockProperties, BlockRow, BlockRowKind, BlockSnapshot, BlockStyle, CustomBlock, CustomBlockId,
    RenderBlock,
};
pub use crease_map::{
    Crease, CreaseId, CreaseMap, CreaseMetadata, CreaseSnapshot, RenderToggleFn, RenderTrailerFn,
};
pub use fold_map::{FoldMap, FoldMetadata, FoldOffset, FoldPlaceholder, FoldPoint, FoldSnapshot};
use highlights::{AnchorResolver, HighlightVersions};
pub use highlights::{
    BufferSemanticTokens, CachedHighlightEndpoints, Chunk, ChunkRenderer, ChunkRendererId,
    ChunkReplacement, HighlightKey, HighlightLayer, HighlightStyle, HighlightStyleId,
    HighlightStyleInterner, HighlightedChunk, Highlights, InlayHighlight, InlayHighlights,
    RowHighlightCursor, SemanticTokenHighlight, SemanticTokenSpans, SemanticTokensHighlights,
    TextHighlights,
};
pub use inlay_map::{InlayId, InlayKind, InlayMap, InlayOffset, InlayPoint, InlaySnapshot};
use std::{
    collections::{BTreeMap, HashMap},
    mem,
    ops::Range,
    sync::{Arc, LazyLock},
};
use stoat_scheduler::Executor;
use stoat_text::{patch::Patch, Anchor, Bias, CharsAt, Point, ReversedCharsAt, Rope};
pub use tab_map::{TabMap, TabPoint, TabRow, TabSnapshot};
use tokio::sync::Notify;
pub use wrap_map::{WrapMap, WrapPoint, WrapSnapshot};

/// Shared empty text-highlight map, used as the `unwrap_or` fallback when an
/// endpoint build carries no text highlights. Every live caller passes its own
/// highlights, so this only spares the per-frame chunk path a throwaway
/// `Arc<HashMap>` allocation on the rare `None` case.
static EMPTY_TEXT_HIGHLIGHTS: LazyLock<TextHighlights> = LazyLock::new(|| Arc::new(HashMap::new()));

pub(crate) use stoat_text::display_width;

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

/// One sync of the display layers through wrapping, and the by-products a
/// caller with more to do against the same text would otherwise rebuild.
pub struct WrapSync {
    pub snapshot: Arc<WrapSnapshot>,
    /// The wrap rows the sync changed.
    pub wrap_edits: Patch<u32>,
    /// The same edits restated in buffer rows, which is what the block map
    /// needs. Block placements are buffer rows, so a wrap-row patch cannot tell
    /// it how far an edit moved them, and by the time the wrap layer has run the
    /// buffer patch is gone.
    pub buffer_row_edits: Patch<u32>,
    /// The buffer the whole sync ran against. Building another would read the
    /// live buffer for the same answer, this sync leaving it untouched.
    pub buffer_snapshot: MultiBufferSnapshot,
    /// The buffer edits the sync computed, spanning from [`Self::since_version`]
    /// to the buffer's version now.
    ///
    /// For a layer synced outside this call, which would otherwise ask the
    /// buffer for the same window again. It has to be paired with the version it
    /// opened at, since such a layer can be at a different one and a patch
    /// spanning the wrong window carries to the wrong places.
    pub buffer_edits: Patch<usize>,
    pub since_version: u64,
}

/// One foldable region per entry, as a server named them, with the text a
/// collapsed region shows in place of its body.
///
/// In anchors so the regions follow the text through the edits that arrive
/// before the next answer does.
pub type FoldingRanges = Vec<(Range<Anchor>, Option<String>)>;

pub struct DisplayMap {
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
    lsp_folding_crease_ids: HashMap<BufferId, Vec<CreaseId>>,
    /// The ranges each buffer's installed folding creases were built from.
    ///
    /// Compared against a fresh response so an answer that names the same
    /// regions installs nothing. In anchors rather than offsets, matching what
    /// the creases hold, so an edit that moves a region without changing it
    /// still compares equal.
    lsp_folding_signature: HashMap<BufferId, FoldingRanges>,
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
    /// The crease sync in [`Self::snapshot`] is skipped while
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
    inserted_diff_block_signature: Vec<(DiffHunkStatus, u32, u32, Range<usize>)>,
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
    /// Whether a modified hunk's base rows pair with its live rows rather than
    /// blocking above them.
    ///
    /// The two-column diff view pairs, putting each base row in the left column
    /// of the live row it replaced, so a block holds only the base rows that
    /// have no live row to sit beside. The unified layout has one column and so
    /// nowhere to pair into, and keeps every base row as a block row of its own.
    ///
    /// Set from the painted width, since that is what decides which layout the
    /// view is in. Off by default, which is the stacked form.
    pair_modified_hunks: bool,
    /// The `pair_modified_hunks` value applied at the last block re-splice, for
    /// the same reason as [`Self::last_show_deleted_blocks`].
    last_pair_modified_hunks: bool,
    /// The `show_deleted_blocks` value applied at the last block re-splice, so a
    /// mid-session toggle re-splices even when the diff version is unchanged.
    last_show_deleted_blocks: bool,
    cached_snapshot: Option<DisplaySnapshot>,
    /// Set when any highlight collection is mutated. Checked inside
    /// [`DisplayMap::snapshot`] so a single rebuild
    /// covers any number of highlight setters fired in the same frame.
    highlights_dirty: bool,
    /// Counts the setter calls that changed what this map paints.
    ///
    /// None of the layer versions can see these. A mask, a diagnostic
    /// threshold, or a token channel changes the painted output while every
    /// buffer, fold, inlay, wrap, and block version stands still, which is what
    /// makes a cache keyed on those alone unsound.
    ///
    /// Setters whose value compares cheaply move it only on a real change,
    /// because callers are invited to call some of them every frame. The rest
    /// carry values with no equality to lean on, a token channel or a
    /// folding-range set, and move it whenever the call does work. So
    /// installing the same tokens twice counts twice, which costs a repaint and
    /// never a wrong frame.
    settings_generation: u64,
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
            lsp_folding_crease_ids: HashMap::new(),
            lsp_folding_signature: HashMap::new(),
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
            pair_modified_hunks: false,
            last_pair_modified_hunks: false,
            last_show_deleted_blocks: false,
            cached_snapshot: None,
            highlights_dirty: false,
            settings_generation: 0,
        }
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
        if self.show_deleted_blocks == show {
            return;
        }
        self.show_deleted_blocks = show;
        self.cached_snapshot = None;
        self.settings_generation += 1;
    }

    /// Choose whether a modified hunk's base rows pair with its live rows. See
    /// [`Self::pair_modified_hunks`]. Nulls the snapshot cache so the next
    /// snapshot re-splices the blocks under the new answer.
    pub(crate) fn set_pair_modified_hunks(&mut self, pair: bool) {
        if self.pair_modified_hunks == pair {
            return;
        }
        self.pair_modified_hunks = pair;
        self.cached_snapshot = None;
        self.settings_generation += 1;
    }

    pub fn set_masked(&mut self, masked: bool) {
        if self.masked == masked {
            return;
        }
        self.masked = masked;
        self.settings_generation += 1;
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
            self.settings_generation += 1;
        }
    }

    pub fn set_clip_at_line_ends(&mut self, clip: bool) {
        if self.clip_at_line_ends == clip {
            return;
        }
        self.clip_at_line_ends = clip;
        self.settings_generation += 1;
    }

    pub fn set_diagnostics_max_severity(&mut self, severity: Option<DiagnosticSeverity>) {
        if self.diagnostics_max_severity == severity {
            return;
        }
        self.diagnostics_max_severity = severity;
        self.settings_generation += 1;
    }

    pub fn insert_blocks(&mut self, blocks: Vec<BlockProperties>) {
        self.block_map.insert(blocks);
        // A block insert marks the block map dirty but touches no buffer, fold,
        // or inlay version, so the cached snapshot must be dropped explicitly or
        // snapshot short-circuits to it and the new blocks stay
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
        let had_none = stale.is_empty() && blocks.is_empty();
        self.block_map.remove(&stale);
        self.conflict_padding_block_ids = self.block_map.insert(blocks);
        self.cached_snapshot = None;
        if !had_none {
            self.settings_generation += 1;
        }
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
        if self.wrap_map.wrap_width() != width {
            self.settings_generation += 1;
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
        self.settings_generation += 1;
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
            self.settings_generation += 1;
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
        let resolved = snapshot.resolve_anchors_batch(&ends);

        BufferSemanticTokens::with_resolved_ends(tokens, interner, &resolved)
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
        self.settings_generation += 1;
    }

    pub fn invalidate_semantic_highlights(&mut self, buffer_id: BufferId) {
        Arc::make_mut(&mut self.semantic_token_highlights).remove(&buffer_id);
        self.highlights_dirty = true;
        self.settings_generation += 1;
    }

    /// Install LSP semantic tokens for `buffer_id`, as the channel the caller
    /// already built. They render on a higher layer than the tree-sitter tokens
    /// set by [`Self::set_semantic_token_highlights`], so their styles merge
    /// over the syntactic baseline.
    ///
    /// The semantic-tokens pump builds one channel per reply and installs that
    /// same value into every editor viewing the buffer, the way the parse
    /// pipeline does for its own channel. Building it resolves every token end,
    /// so a rebuild per editor is milliseconds of the turn that paints.
    pub fn set_lsp_token_channel(&mut self, buffer_id: BufferId, channel: BufferSemanticTokens) {
        Arc::make_mut(&mut self.lsp_token_highlights).insert(buffer_id, channel);
        self.highlights_dirty = true;
        self.settings_generation += 1;
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

    pub fn set_lsp_folding_ranges(&mut self, buffer_id: BufferId, ranges: FoldingRanges) {
        // A server re-answers on every settled edit, and its answer for a file
        // nobody restructured is the one already installed. Reinstalling it
        // rebuilds the crease tree twice, hands every crease a fresh id, and
        // bumps settings_generation, which throws away every highlight
        // endpoint cache with it.
        if self.lsp_folding_signature.get(&buffer_id) == Some(&ranges) {
            return;
        }

        let old_ids = self.lsp_folding_crease_ids.remove(&buffer_id);
        let had_none = old_ids.as_ref().is_none_or(Vec::is_empty) && ranges.is_empty();
        if let Some(old_ids) = old_ids {
            self.crease_map.remove(old_ids);
        }
        if !had_none {
            self.settings_generation += 1;
        }
        let signature = ranges.clone();
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
        self.lsp_folding_signature.insert(buffer_id, signature);
    }

    /// Bring the installed deleted-line blocks in line with `signature`,
    /// keeping the block already standing for each hunk that survived.
    ///
    /// Most refreshes change one hunk out of many, and replacing the whole set
    /// would mark every one of their rows for rebuild and hand back fresh ids
    /// for blocks that never moved.
    fn resplice_diff_blocks(
        &mut self,
        signature: Vec<(DiffHunkStatus, u32, u32, Range<usize>)>,
        diff_map: Option<&DiffMap>,
    ) {
        let mut standing: HashMap<(DiffHunkStatus, u32, u32, Range<usize>), CustomBlockId> = self
            .inserted_diff_block_signature
            .drain(..)
            .zip(self.inserted_diff_block_ids.drain(..))
            .collect();

        // Built in the same order as the signature, since both walk one filtered
        // pass over the hunks.
        let props = match diff_map.filter(|_| self.show_deleted_blocks) {
            Some(dm) => dm.deleted_blocks(self.pair_modified_hunks),
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

    /// Sync the layers up to wrapping, and everything that sync produced which
    /// a caller with more to do against the same text would otherwise rebuild.
    pub fn sync_through_wrap(&mut self) -> WrapSync {
        let buffer_snapshot = self.multi_buffer.snapshot();
        let since_version = self.last_buffer_version;
        let buffer_edits = buffer_snapshot.edits_since(since_version);
        let buffer_row_edits = match self.last_buffer_snapshot.take() {
            Some(previous) => buffer_row_patch(&buffer_edits, &previous, &buffer_snapshot),
            None => Patch::empty(),
        };

        self.last_buffer_version = buffer_snapshot.version();
        self.last_buffer_snapshot = Some(buffer_snapshot.clone());

        let (inlay_snapshot, inlay_edits) =
            self.inlay_map.sync(buffer_snapshot.clone(), &buffer_edits);
        let (fold_snapshot, fold_edits) = self.fold_map.sync(
            inlay_snapshot,
            &inlay_edits,
            Some((&buffer_edits, since_version)),
        );
        let (tab_snapshot, tab_edits) = self.tab_map.sync(fold_snapshot, fold_edits);
        let (wrap_snapshot, wrap_edits) = self.wrap_map.sync(tab_snapshot, &tab_edits);

        WrapSync {
            snapshot: wrap_snapshot,
            wrap_edits,
            buffer_row_edits,
            buffer_snapshot,
            buffer_edits,
            since_version,
        }
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

    pub fn snapshot(&mut self) -> DisplaySnapshot {
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

        let WrapSync {
            snapshot: wrap_snapshot,
            wrap_edits,
            buffer_row_edits,
            buffer_snapshot,
            buffer_edits,
            since_version,
        } = self.sync_through_wrap();
        let diff_map = buffer_snapshot.diff_map.clone();
        let diff_version = diff_map.as_ref().map(|dm| dm.version()).unwrap_or(0);
        if diff_version != self.last_diff_version
            || self.show_deleted_blocks != self.last_show_deleted_blocks
            || self.pair_modified_hunks != self.last_pair_modified_hunks
        {
            let signature = if self.show_deleted_blocks {
                diff_map
                    .as_ref()
                    .map(|dm| dm.deleted_block_signature(self.pair_modified_hunks))
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
            self.last_pair_modified_hunks = self.pair_modified_hunks;
        }
        let block_snapshot = self
            .block_map
            .sync(wrap_snapshot, &wrap_edits, &buffer_row_edits);

        if buffer_version != self.last_crease_sync_version {
            self.crease_map
                .sync(&buffer_snapshot, Some((&buffer_edits, since_version)));
            self.last_crease_sync_version = buffer_version;
        }

        let snapshot = DisplaySnapshot {
            block_snapshot,
            diff_map,
            pairs_modified_hunks: self.pair_modified_hunks,
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
            settings_generation: self.settings_generation,
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
        snapshot.settings_generation = self.settings_generation;
    }
}

#[derive(Clone)]
pub struct DisplaySnapshot {
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
    settings_generation: u64,
    /// What [`DisplayMap::pair_modified_hunks`] said when these blocks were
    /// spliced, so the paint reads the same answer the block set was built
    /// from rather than re-deriving it.
    pairs_modified_hunks: bool,
}

/// Everything that decides a snapshot's painted text, gathered so two frames
/// can be compared in one step.
///
/// Only equality is defined, along with the hash that agrees with it for a
/// cache keyed on a single scalar. There is no sense in which one of these is
/// later than another, since the parts count different things, so ordering is
/// left out rather than left to be misread.
/// The [`Default`] is what a freshly built map answers with, before any layer
/// has synced or any setter has fired.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PaintVersion {
    fold: usize,
    inlay: usize,
    wrap: u64,
    block: u64,
    settings: u64,
}

impl DisplaySnapshot {
    pub fn version(&self) -> usize {
        self.fold_snapshot().version()
    }

    /// What a pane cache compares to decide whether it may reuse its cells.
    ///
    /// Two snapshots of the same buffer version sharing this paint the same
    /// text. That is the direction a cache needs, and it holds because every
    /// layer between the buffer and the screen contributes its own count, along
    /// with the settings no layer sees.
    ///
    /// The converse is weaker. Several of the parts move on work that changed
    /// no visible row, so an unequal pair may still paint the same, costing a
    /// repaint rather than a wrong frame.
    ///
    /// The buffer version is not folded in, because a caller comparing frames
    /// already holds it, and burying it here would hide which half of the
    /// condition failed.
    pub fn paint_version(&self) -> PaintVersion {
        PaintVersion {
            fold: self.fold_snapshot().version(),
            inlay: self.inlay_snapshot().inlay_version,
            wrap: self.block_snapshot.wrap_snapshot().version(),
            block: self.block_snapshot.version(),
            settings: self.settings_generation,
        }
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

    /// Whether this snapshot's editor paints syntax color.
    ///
    /// The snapshot already withholds its own token highlights when this is
    /// off. A caller reads the flag itself only for coloring that comes from
    /// somewhere else, as the diff view's base column does.
    pub fn syntax_highlighting(&self) -> bool {
        self.syntax_highlighting
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

    pub fn chunks(&self, display_rows: Range<u32>, highlights: Highlights<'_>) -> BlockChunks<'_> {
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

    pub fn highlighted_chunks(&self, display_rows: Range<u32>) -> BlockChunks<'_> {
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
    ) -> BlockChunks<'_> {
        self.block_snapshot.chunks(display_rows, endpoints)
    }

    /// The buffer byte offset display row `row` starts at, when it has one.
    ///
    /// `None` for a row belonging to a block, which shows text the buffer does
    /// not contain and so has no offset of its own.
    fn row_start_offset(&self, row: u32) -> Option<usize> {
        let point = self
            .block_snapshot
            .block_to_buffer(BlockPoint::new(row, 0))?;
        Some(self.buffer_snapshot().rope().point_to_offset(point))
    }

    /// Whether display offsets below the block layer are buffer offsets.
    ///
    /// A fold or an inlay makes the layers between renumber, and an endpoint
    /// replay measured in buffer offsets means nothing against those. Asked once
    /// by [`RowHighlightCursor`] rather than re-derived per layer per row.
    fn offsets_are_buffer_offsets(&self) -> bool {
        let fold = self
            .block_snapshot
            .wrap_snapshot()
            .tab_snapshot()
            .fold_snapshot();
        fold.fold_count() == 0 && !fold.inlay_snapshot().has_inlays()
    }

    /// Chunks for one row, resuming `cursor` rather than replaying the
    /// endpoints below the row.
    ///
    /// For a painter that walks the viewport a row at a time and cannot hold one
    /// stream open across them, because it paints other things between rows.
    /// Rows must be asked for in ascending order, which is the order such a
    /// painter visits them.
    pub fn row_chunks(&self, row: u32, cursor: &mut RowHighlightCursor) -> BlockChunks<'_> {
        let endpoints = cursor.endpoints.clone();
        let seed = cursor.seed_at(self.row_start_offset(row));
        self.block_snapshot
            .chunks_seeded(row..row + 1, endpoints, seed)
    }

    /// A replay over `endpoints` for painting this snapshot row by row.
    pub fn row_highlight_cursor(
        &self,
        endpoints: Arc<[highlights::HighlightEndpoint]>,
    ) -> RowHighlightCursor {
        RowHighlightCursor::new(endpoints, self.offsets_are_buffer_offsets())
    }

    /// Like [`Self::highlighted_chunks`] but memoizes the resolved endpoints in
    /// `cache`, recomputing only when the buffer version, highlight identity, or
    /// visible byte range changes.
    pub fn highlighted_chunks_cached(
        &self,
        display_rows: Range<u32>,
        cache: &mut Option<CachedHighlightEndpoints>,
    ) -> BlockChunks<'_> {
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
            HighlightVersions {
                buffer: buffer.version(),
                settings: self.settings_generation,
            },
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
        tab_map::expand_column_runs(
            self.buffer_snapshot()
                .rope()
                .measured_chunks_in_line(point.row),
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
        tab_map::collapse_column_runs(
            self.buffer_snapshot().rope().measured_chunks_in_line(row),
            visual,
            tabs.tab_size(),
            bias,
            tabs.max_expansion_column(),
        )
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
            .map(|dm| dm.has_deletion_after(buffer_line, self.pairs_modified_hunks))
            .unwrap_or(false)
    }

    /// Whether this snapshot's blocks were spliced with a modified hunk's base
    /// rows paired against its live rows. See
    /// [`DisplayMap::pair_modified_hunks`].
    pub(crate) fn pairs_modified_hunks(&self) -> bool {
        self.pairs_modified_hunks
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
mod tests;
