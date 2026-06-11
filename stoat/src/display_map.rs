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
    diff_map::{DiffBlockKey, DiffMap, TokenDetail},
    host::DiffStatus,
    multi_buffer::{ExcerptId, MultiBuffer, MultiBufferSnapshot},
};
pub use block_map::{
    balancing_block, Block, BlockContext, BlockId, BlockMap, BlockPlacement, BlockPoint,
    BlockProperties, BlockRow, BlockRowKind, BlockSnapshot, BlockStyle, CompanionView, CustomBlock,
    CustomBlockId, RenderBlock, RowInfo,
};
pub use crease_map::{
    Crease, CreaseId, CreaseMap, CreaseMetadata, CreaseSnapshot, RenderToggleFn, RenderTrailerFn,
};
pub use fold_map::{FoldMap, FoldMetadata, FoldOffset, FoldPlaceholder, FoldPoint, FoldSnapshot};
pub use highlights::{
    CachedHighlightEndpoints, Chunk, ChunkRenderer, ChunkRendererId, ChunkReplacement,
    DecorationHighlight, DecorationHighlights, HighlightKey, HighlightLayer, HighlightStyle,
    HighlightStyleId, HighlightStyleInterner, HighlightedChunk, Highlights, SemanticTokenHighlight,
    SemanticTokensHighlights, TextHighlights,
};
pub use inlay_map::{InlayId, InlayKind, InlayMap, InlayOffset, InlayPoint, InlaySnapshot};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        Arc, Mutex,
    },
};
use stoat_scheduler::Executor;
use stoat_text::{patch::Patch, Anchor, Bias, CharsAt, Point, ReversedCharsAt, Rope};
pub use tab_map::{TabMap, TabPoint, TabRow, TabSnapshot};
use unicode_width::UnicodeWidthChar;
pub use wrap_map::{WrapMap, WrapPoint, WrapSnapshot};

pub(crate) fn display_width(ch: char) -> u32 {
    ch.width().unwrap_or(0) as u32
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
    pub edited_range: std::ops::Range<Point>,
    pub source_excerpt_range: std::ops::Range<Point>,
    pub target_excerpt_range: std::ops::Range<Point>,
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
    ) -> std::ops::Range<Point> {
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
    decoration_highlights: DecorationHighlights,
    companion: Option<Companion>,
    lsp_folding_crease_ids: HashMap<BufferId, Vec<CreaseId>>,
    masked: bool,
    clip_at_line_ends: bool,
    diagnostics_max_severity: Option<DiagnosticSeverity>,
    last_buffer_version: u64,
    inserted_diff_blocks: HashMap<DiffBlockKey, CustomBlockId>,
    last_diff_version: usize,
    cached_snapshot: Option<DisplaySnapshot>,
    /// Set when any highlight collection is mutated. Checked inside
    /// [`DisplayMap::snapshot_with_companion`] so a single rebuild
    /// covers any number of highlight setters fired in the same frame.
    highlights_dirty: bool,
}

impl DisplayMap {
    pub(crate) fn multi_buffer(&self) -> &MultiBuffer {
        &self.multi_buffer
    }

    pub fn new(multi_buffer: MultiBuffer, executor: Executor) -> Self {
        let buffer_snapshot = multi_buffer.snapshot();
        let version = buffer_snapshot.version();
        let (inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (fold_map, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).expect("non-zero literal"));
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (wrap_map, _wrap_snapshot) = WrapMap::new(tab_snapshot, None, executor);
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
            decoration_highlights: Arc::new(HashMap::new()),
            companion: None,
            lsp_folding_crease_ids: HashMap::new(),
            masked: false,
            clip_at_line_ends: false,
            diagnostics_max_severity: None,
            last_buffer_version: version,
            inserted_diff_blocks: HashMap::new(),
            last_diff_version: 0,
            cached_snapshot: None,
            highlights_dirty: false,
        }
    }

    pub fn id(&self) -> DisplayMapId {
        self.id
    }

    pub fn folded_buffers(&self) -> &HashSet<BufferId> {
        self.block_map.folded_buffers()
    }

    pub fn set_companion(&mut self, companion: Option<Companion>) {
        if companion.is_none() {
            if let Some(old) = self.companion.take() {
                let ids: HashSet<CustomBlockId> = old
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

    pub fn set_clip_at_line_ends(&mut self, clip: bool) {
        self.clip_at_line_ends = clip;
    }

    pub fn set_diagnostics_max_severity(&mut self, severity: Option<DiagnosticSeverity>) {
        self.diagnostics_max_severity = severity;
    }

    pub fn insert_blocks(&mut self, blocks: Vec<BlockProperties>) -> Vec<CustomBlockId> {
        self.block_map.insert(blocks)
    }

    pub fn remove_blocks(&mut self, ids: &HashSet<CustomBlockId>) {
        self.block_map.remove(ids);
    }

    pub fn fold(&mut self, ranges: Vec<std::ops::Range<Point>>) {
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

    pub fn unfold(&mut self, ranges: Vec<std::ops::Range<Point>>) {
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

    pub fn toggle_fold(&mut self, ranges: Vec<std::ops::Range<Point>>) {
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

    /// Buffer-`Point` range of every active fold, resolved against the
    /// current buffer. The inverse of [`Self::fold`]'s input; used to
    /// enumerate folds for persistence.
    pub fn fold_point_ranges(&self) -> Vec<std::ops::Range<Point>> {
        let buffer_snapshot = self.multi_buffer.snapshot();
        let rope = buffer_snapshot.rope();
        self.fold_map
            .fold_anchor_ranges()
            .into_iter()
            .map(|range| {
                let start = rope.offset_to_point(buffer_snapshot.resolve_anchor(&range.start));
                let end = rope.offset_to_point(buffer_snapshot.resolve_anchor(&range.end));
                start..end
            })
            .collect()
    }

    pub fn set_wrap_width(&mut self, width: Option<u32>) {
        self.wrap_map.set_wrap_width(width);
    }

    pub fn highlight_text(
        &mut self,
        key: HighlightKey,
        ranges: Vec<std::ops::Range<Anchor>>,
        style: HighlightStyle,
    ) {
        let buffer_snapshot = self.multi_buffer.snapshot();
        let mut sorted_ranges = ranges;
        sorted_ranges.sort_by(|a, b| {
            buffer_snapshot
                .resolve_anchor(&a.start)
                .cmp(&buffer_snapshot.resolve_anchor(&b.start))
        });
        Arc::make_mut(&mut self.text_highlights).insert(key, Arc::new((style, sorted_ranges)));
        self.highlights_dirty = true;
    }

    pub fn clear_highlights(&mut self, key: HighlightKey) -> bool {
        let cleared = Arc::make_mut(&mut self.text_highlights)
            .remove(&key)
            .is_some();
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
        Arc::make_mut(&mut self.semantic_token_highlights).insert(buffer_id, (tokens, interner));
        self.highlights_dirty = true;
    }

    pub fn invalidate_semantic_highlights(&mut self, buffer_id: BufferId) {
        Arc::make_mut(&mut self.semantic_token_highlights).remove(&buffer_id);
        self.highlights_dirty = true;
    }

    /// Replace the non-LSP decoration highlight set for `buffer_id`.
    /// Stored in a parallel slot to the semantic-token highlights so
    /// the two layers co-exist without clobbering each other.
    pub fn set_decoration_highlights(
        &mut self,
        buffer_id: BufferId,
        decorations: Arc<[DecorationHighlight]>,
    ) {
        Arc::make_mut(&mut self.decoration_highlights).insert(buffer_id, decorations);
        self.highlights_dirty = true;
    }

    /// Drop the decoration-highlight set for `buffer_id`. Used by
    /// hosts to clear overlays when the underlying buffer state no
    /// longer warrants them (e.g. all conflicts resolved).
    pub fn clear_decoration_highlights(&mut self, buffer_id: BufferId) {
        Arc::make_mut(&mut self.decoration_highlights).remove(&buffer_id);
        self.highlights_dirty = true;
    }

    /// Remove the inlays identified by `remove` and insert new
    /// anchored inlays for each `(Anchor, String, InlayKind)` triple
    /// in `insert`. Returns the freshly-allocated [`InlayId`]s in the
    /// same order as `insert`, so callers can track the live set for
    /// the next splice. Delegates to [`InlayMap::splice`].
    pub fn splice_inlays(
        &mut self,
        remove: Vec<InlayId>,
        insert: Vec<(Anchor, String, InlayKind)>,
    ) -> Vec<InlayId> {
        self.inlay_map.splice(remove, insert)
    }

    pub fn insert_creases(
        &mut self,
        creases: impl IntoIterator<Item = Crease<Anchor>>,
    ) -> Vec<CreaseId> {
        let buffer_snapshot = self.multi_buffer.snapshot();
        self.crease_map.insert(creases, &buffer_snapshot)
    }

    pub fn remove_creases(&mut self, ids: impl IntoIterator<Item = CreaseId>) {
        let buffer_snapshot = self.multi_buffer.snapshot();
        self.crease_map.remove(ids, &buffer_snapshot);
    }

    pub fn set_lsp_folding_ranges(
        &mut self,
        buffer_id: BufferId,
        ranges: Vec<(std::ops::Range<Anchor>, Option<String>)>,
    ) {
        if let Some(old_ids) = self.lsp_folding_crease_ids.remove(&buffer_id) {
            let buffer_snapshot = self.multi_buffer.snapshot();
            self.crease_map.remove(old_ids, &buffer_snapshot);
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

    pub fn sync_through_wrap(&mut self) -> (Arc<WrapSnapshot>, Patch<u32>) {
        let buffer_snapshot = self.multi_buffer.snapshot();
        let buffer_edits = buffer_snapshot.edits_since(self.last_buffer_version);
        self.last_buffer_version = buffer_snapshot.version();
        let (inlay_snapshot, inlay_edits) = self.inlay_map.sync(buffer_snapshot, &buffer_edits);
        let (fold_snapshot, fold_edits) = self.fold_map.sync(inlay_snapshot, &inlay_edits);
        let (tab_snapshot, tab_edits) = self.tab_map.sync(fold_snapshot, fold_edits);
        self.wrap_map.sync(tab_snapshot, &tab_edits)
    }

    pub fn snapshot(&mut self) -> DisplaySnapshot {
        self.snapshot_with_companion(None)
    }

    pub fn snapshot_with_companion(
        &mut self,
        companion_wrap_data: Option<(&WrapSnapshot, &Patch<u32>)>,
    ) -> DisplaySnapshot {
        if self.highlights_dirty {
            self.cached_snapshot = None;
            self.highlights_dirty = false;
        }
        let buffer_version = self.multi_buffer.buffer_version();
        let diff_version_now = self.multi_buffer.diff_version();
        if buffer_version == self.last_buffer_version
            && diff_version_now == self.last_diff_version
            && self.fold_map.version_unchanged()
            && self.inlay_map.version_unchanged()
            && !self.block_map.is_dirty()
            && companion_wrap_data.is_none()
        {
            if let Some(ref cached) = self.cached_snapshot {
                return cached.clone();
            }
        }

        let (wrap_snapshot, wrap_edits) = self.sync_through_wrap();
        let diff_map = self.multi_buffer.snapshot().diff_map.clone();
        let diff_version = diff_map.as_ref().map(|dm| dm.version()).unwrap_or(0);
        if diff_version != self.last_diff_version {
            let existing: HashSet<DiffBlockKey> =
                self.inserted_diff_blocks.keys().cloned().collect();
            let (current_keys, new_blocks) = diff_map
                .as_ref()
                .map(|dm| dm.deleted_block_specs(&existing))
                .unwrap_or_default();

            let stale: HashSet<CustomBlockId> = self
                .inserted_diff_blocks
                .iter()
                .filter(|(key, _)| !current_keys.contains(key))
                .map(|(_, id)| *id)
                .collect();
            if !stale.is_empty() {
                self.block_map.remove(&stale);
                self.inserted_diff_blocks
                    .retain(|key, _| current_keys.contains(key));
            }

            if !new_blocks.is_empty() {
                let (keys, props): (Vec<_>, Vec<_>) = new_blocks.into_iter().unzip();
                let ids = self.block_map.insert(props);
                self.inserted_diff_blocks.extend(keys.into_iter().zip(ids));
            }

            self.last_diff_version = diff_version;
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
        let block_snapshot = self
            .block_map
            .sync(wrap_snapshot, &wrap_edits, companion_view);

        let snapshot = DisplaySnapshot {
            companion_display_snapshot: None,
            block_snapshot,
            diff_map,
            text_highlights: self.text_highlights.clone(),
            semantic_token_highlights: self.semantic_token_highlights.clone(),
            decoration_highlights: self.decoration_highlights.clone(),
            crease_snapshot: self.crease_map.snapshot(),
            fold_placeholder: FoldPlaceholder::default(),
            masked: self.masked,
            clip_at_line_ends: self.clip_at_line_ends,
            diagnostics_max_severity: self.diagnostics_max_severity,
            highlight_endpoint_cache: Arc::new(Mutex::new(None)),
        };
        self.cached_snapshot = Some(snapshot.clone());
        snapshot
    }
}

#[derive(Clone)]
pub struct DisplaySnapshot {
    companion_display_snapshot: Option<Arc<DisplaySnapshot>>,
    block_snapshot: BlockSnapshot,
    diff_map: Option<DiffMap>,
    text_highlights: TextHighlights,
    semantic_token_highlights: SemanticTokensHighlights,
    decoration_highlights: DecorationHighlights,
    crease_snapshot: CreaseSnapshot,
    fold_placeholder: FoldPlaceholder,
    masked: bool,
    clip_at_line_ends: bool,
    diagnostics_max_severity: Option<DiagnosticSeverity>,
    /// Per-snapshot cache of resolved highlight endpoints, shared across this
    /// snapshot's clones. Reused across the per-frame render passes (and idle
    /// frames while the coordinator keeps reusing this snapshot) so the
    /// visible range's anchors resolve at most once until a highlight set or
    /// the range changes; a rebuilt snapshot starts with an empty cache.
    highlight_endpoint_cache: Arc<Mutex<Option<CachedHighlightEndpoints>>>,
}

/// Leading-whitespace measurement of a display row. `spaces` counts the
/// leading space columns (display text has tabs expanded to spaces);
/// `blank` is true when the row holds no non-space content. Produced by
/// [`DisplaySnapshot::line_indent`] without materializing the line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineIndent {
    pub spaces: u32,
    pub blank: bool,
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

    pub fn decoration_highlights(&self) -> &DecorationHighlights {
        &self.decoration_highlights
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

    pub fn longest_row(&self) -> (u32, u32) {
        self.block_snapshot.longest_row()
    }

    pub fn chunks(
        &self,
        display_rows: std::ops::Range<u32>,
        highlights: Highlights<'_>,
    ) -> block_map::BlockChunks<'_> {
        let byte_range = self
            .block_snapshot
            .row_range_to_buffer_byte_range(display_rows.clone());
        let endpoints = self.build_endpoints(highlights, byte_range);
        self.block_snapshot.chunks(display_rows, endpoints)
    }

    pub fn highlighted_chunks(
        &self,
        display_rows: std::ops::Range<u32>,
    ) -> block_map::BlockChunks<'_> {
        let highlights = Highlights {
            text_highlights: Some(&self.text_highlights),
            semantic_token_highlights: Some(&self.semantic_token_highlights),
            decoration_highlights: Some(&self.decoration_highlights),
        };
        let byte_range = self
            .block_snapshot
            .row_range_to_buffer_byte_range(display_rows.clone());
        let endpoints = self.build_endpoints(highlights, byte_range);
        self.block_snapshot.chunks(display_rows, endpoints)
    }

    fn build_endpoints(
        &self,
        highlights: Highlights<'_>,
        range: std::ops::Range<usize>,
    ) -> Arc<[highlights::HighlightEndpoint]> {
        let buffer = self.buffer_snapshot();
        let empty: TextHighlights = Arc::new(HashMap::new());
        let text_highlights_ref = highlights.text_highlights.unwrap_or(&empty);
        let semantic_ref = highlights.semantic_token_highlights;
        let decoration_ref = highlights.decoration_highlights;
        let resolve = |a: &Anchor| buffer.resolve_anchor(a);
        let mut cache = self
            .highlight_endpoint_cache
            .lock()
            .expect("highlight endpoint cache mutex poisoned");
        highlights::create_highlight_endpoints_cached(
            &range,
            text_highlights_ref,
            semantic_ref,
            decoration_ref,
            &resolve,
            &mut cache,
        )
    }

    pub fn is_line_folded(&self, buffer_row: u32) -> bool {
        let inlay_point = self
            .fold_snapshot()
            .inlay_snapshot()
            .to_inlay_point(Point::new(buffer_row, 0));
        self.fold_snapshot().is_line_folded(inlay_point.row())
    }

    pub fn buffer_to_display(&self, point: Point, bias: Bias) -> DisplayPoint {
        let block = self.block_snapshot.buffer_to_block(point, bias);
        DisplayPoint::new(block.row, block.column)
    }

    pub fn display_to_buffer(&self, point: DisplayPoint, bias: Bias) -> Option<Point> {
        self.block_snapshot
            .block_to_buffer(BlockPoint::new(point.row, point.column), bias)
    }

    pub fn classify_row(&self, display_row: u32) -> BlockRowKind<'_> {
        self.block_snapshot.classify_row(display_row)
    }

    pub fn row_infos(&self, rows: std::ops::Range<u32>) -> Vec<RowInfo> {
        self.block_snapshot.row_infos(rows)
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

    pub fn display_lines(&self, range: std::ops::Range<u32>) -> impl Iterator<Item = String> + '_ {
        range.map(move |row| self.display_line(row))
    }

    /// Leading whitespace of `display_row`, read in a single forward pass
    /// over the row's display chunks without allocating the line. Block
    /// rows -- whose chunk text differs from their rendered line -- fall
    /// back to the materialized line so the result matches
    /// [`display_line`](Self::display_line).
    pub fn line_indent(&self, display_row: u32) -> LineIndent {
        if matches!(self.classify_row(display_row), BlockRowKind::Block { .. }) {
            let line = self.display_line(display_row);
            let spaces = line.chars().take_while(|c| *c == ' ').count() as u32;
            return LineIndent {
                spaces,
                blank: line.trim().is_empty(),
            };
        }
        let mut spaces = 0u32;
        let mut blank = true;
        'scan: for chunk in self.chunks(display_row..display_row + 1, Highlights::default()) {
            for ch in chunk.text.chars() {
                match ch {
                    '\n' => break 'scan,
                    ' ' => spaces += 1,
                    _ => {
                        blank = false;
                        break 'scan;
                    },
                }
            }
        }
        LineIndent { spaces, blank }
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
        let display = self.buffer_to_display(point, Bias::Left);
        let start = DisplayPoint::new(display.row, 0);
        let buf = self
            .display_to_buffer(start, Bias::Left)
            .unwrap_or(Point::zero());
        (buf, start)
    }

    pub fn next_line_boundary(&self, point: Point) -> (Point, DisplayPoint) {
        let display = self.buffer_to_display(point, Bias::Left);
        let end = DisplayPoint::new(display.row, self.line_len(display.row));
        let max = self.block_snapshot.buffer_snapshot().rope().max_point();
        let buf = self.display_to_buffer(end, Bias::Left).unwrap_or(max);
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
        BlockRowKind, DisplayMap, DisplayPoint, DisplayRow, HighlightKey, HighlightLayer,
        HighlightStyle, InlayKind, InlayPoint, LineIndent,
    };
    use crate::{
        buffer::{BufferId, TextBuffer},
        diff_map::{DiffHunk, DiffHunkStatus, DiffMap},
        multi_buffer::MultiBuffer,
    };
    use ratatui::style::Color;
    use std::{
        ops::Range,
        sync::{Arc, RwLock},
    };
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_text::{Bias, Point};

    fn test_executor() -> Executor {
        Executor::new(Arc::new(TestScheduler::new()))
    }

    fn create_display_map(content: &str) -> DisplayMap {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        DisplayMap::new(multi_buffer, test_executor())
    }

    fn create_display_map_with_diff(content: &str, diff_map: DiffMap) -> DisplayMap {
        let mut buffer = TextBuffer::with_text(BufferId::new(0), content);
        buffer.diff_map = Some(diff_map);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        DisplayMap::new(multi_buffer, test_executor())
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
            staged: false,
            buffer_start_line: after_line + 1,
            buffer_line_range: (after_line + 1)..(after_line + 1),
            base_byte_range: byte_range,
            anchor_range: None,
            token_detail: None,
        });
        dm
    }

    #[test]
    fn diff_block_reused_across_identical_versions_and_swapped_on_change() {
        let base = "deleted base\n";
        let mut buffer = TextBuffer::with_text(BufferId::new(0), "alpha\nbeta\ngamma\ndelta");
        buffer.diff_map = Some(make_diff_with_deletion(0, base, 0..12, 1));
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut dm = DisplayMap::new(multi_buffer, test_executor());

        let _ = dm.snapshot();
        assert_eq!(dm.inserted_diff_blocks.len(), 1);
        let id1 = *dm.inserted_diff_blocks.values().next().unwrap();

        // A fresh diff version carrying the same hunk keeps the existing block.
        shared.write().unwrap().diff_map = Some(make_diff_with_deletion(0, base, 0..12, 1));
        let _ = dm.snapshot();
        let id2 = *dm.inserted_diff_blocks.values().next().unwrap();
        assert_eq!(id1, id2, "identical hunk must keep its block id");

        // A hunk at a different line is a different key, so the block is swapped.
        shared.write().unwrap().diff_map = Some(make_diff_with_deletion(2, base, 0..12, 1));
        let _ = dm.snapshot();
        assert_eq!(dm.inserted_diff_blocks.len(), 1);
        let id3 = *dm.inserted_diff_blocks.values().next().unwrap();
        assert_ne!(id1, id3, "a hunk at a new line must get a new block id");
    }

    #[test]
    fn display_snapshot_version() {
        let mut dm = create_display_map("hello");
        let v1 = dm.snapshot().version();
        let v2 = dm.snapshot().version();
        assert_eq!(v1, v2);
    }

    #[test]
    fn snapshot_reflects_block_insert_and_remove_without_buffer_edit() {
        use super::{BlockPlacement, BlockProperties, BlockStyle};
        let mut dm = create_display_map("alpha\nbeta");
        assert_eq!(dm.snapshot().max_point().row, 1);

        let ids = dm.insert_blocks(vec![BlockProperties::from_text(
            BlockPlacement::Above(0),
            vec!["lens".into()],
            BlockStyle::Fixed,
        )]);
        assert_eq!(dm.snapshot().max_point().row, 2);

        dm.remove_blocks(&ids.into_iter().collect());
        assert_eq!(dm.snapshot().max_point().row, 1);
    }

    #[test]
    fn passthrough_coordinates() {
        let mut display_map = create_display_map("hello\nworld\n");
        let snapshot = display_map.snapshot();

        let buffer_point = Point::new(1, 3);
        let display_point = snapshot.buffer_to_display(buffer_point, Bias::Left);
        assert_eq!(display_point, DisplayPoint::new(1, 3));

        let back = snapshot.display_to_buffer(display_point, Bias::Left);
        assert_eq!(back, Some(buffer_point));
    }

    #[test]
    fn line_count() {
        let mut display_map = create_display_map("line1\nline2\nline3");
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.line_count(), 3);
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

        let display = snapshot.buffer_to_display(Point::new(0, 1), Bias::Left);
        assert_eq!(display, DisplayPoint::new(0, 4));

        let back = snapshot.display_to_buffer(display, Bias::Left).unwrap();
        assert_eq!(back, Point::new(0, 1));

        let display5 = DisplayPoint::new(0, 5);
        let back5 = snapshot.display_to_buffer(display5, Bias::Left).unwrap();
        assert_eq!(back5, Point::new(0, 2));
    }

    #[test]
    fn roundtrip_with_folds() {
        let mut display_map = create_display_map("fn main() {\n    body;\n}");
        display_map.fold(vec![Point::new(0, 11)..Point::new(2, 0)]);
        let snapshot = display_map.snapshot();

        let display = snapshot.buffer_to_display(Point::new(2, 1), Bias::Left);
        let back = snapshot.display_to_buffer(display, Bias::Left).unwrap();
        assert_eq!(back, Point::new(2, 1));
    }

    #[test]
    fn fold_point_ranges_returns_folded_ranges() {
        let mut display_map = create_display_map("fn main() {\n    body;\n}");
        assert!(display_map.fold_point_ranges().is_empty());

        display_map.fold(vec![Point::new(0, 11)..Point::new(2, 0)]);
        assert_eq!(
            display_map.fold_point_ranges(),
            vec![Point::new(0, 11)..Point::new(2, 0)]
        );
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

    #[test]
    fn longest_row_no_blocks() {
        let mut display_map = create_display_map("short\nlonger line\nx");
        let snapshot = display_map.snapshot();
        let (row, chars) = snapshot.longest_row();
        assert_eq!(chars, 11);
        assert_eq!(row, 1);
    }

    #[test]
    fn longest_row_with_blocks() {
        let base = "line1\ndeleted long line here\nline2";
        let diff = make_diff_with_deletion(0, base, 6..28, 1);
        let mut display_map = create_display_map_with_diff("line1\nline2", diff);
        let snapshot = display_map.snapshot();
        let (_, chars) = snapshot.longest_row();
        assert!(chars >= 5);
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
    fn splice_inlays_inserts_and_removes_through_public_api() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "hello world");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let mut display_map = DisplayMap::new(multi_buffer, test_executor());

        let anchor = {
            let snap = display_map.multi_buffer.snapshot();
            let off = snap.rope().point_to_offset(Point::new(0, 5));
            snap.anchor_at(off, Bias::Right)
        };

        let inserted = display_map.splice_inlays(
            Vec::new(),
            vec![(anchor, ": str".to_string(), InlayKind::Hint)],
        );
        assert_eq!(inserted.len(), 1);
        assert_eq!(
            display_map.snapshot().inlay_snapshot().inlay_text(),
            "hello: str world"
        );

        let removed = display_map.splice_inlays(inserted, Vec::new());
        assert!(removed.is_empty());
        assert_eq!(
            display_map.snapshot().inlay_snapshot().inlay_text(),
            "hello world"
        );
    }

    #[test]
    fn inlay_survives_compaction() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "hello world");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer, test_executor());

        let snap = display_map.multi_buffer.snapshot();
        let off = snap.rope().point_to_offset(Point::new(0, 5));
        let anchor = snap.anchor_at(off, Bias::Right);
        display_map.inlay_map.splice(
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
    fn line_indent_reads_leading_whitespace() {
        let mut display_map = create_display_map("code\n    code\n        x\n   \ny");
        let snapshot = display_map.snapshot();
        let indents: Vec<LineIndent> = (0..5).map(|r| snapshot.line_indent(r)).collect();
        assert_eq!(
            indents,
            vec![
                LineIndent {
                    spaces: 0,
                    blank: false
                },
                LineIndent {
                    spaces: 4,
                    blank: false
                },
                LineIndent {
                    spaces: 8,
                    blank: false
                },
                LineIndent {
                    spaces: 3,
                    blank: true
                },
                LineIndent {
                    spaces: 0,
                    blank: false
                },
            ]
        );
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
                staged: false,
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
    fn fold_tail_highlight_endpoints_survive() {
        let mut display_map = create_display_map("fn foo() {\n    body\n}  TAIL");
        display_map.fold(vec![Point::new(0, 10)..Point::new(2, 1)]);

        let range = {
            let snap = display_map.multi_buffer.snapshot();
            let rope = snap.rope();
            let start = rope.point_to_offset(Point::new(2, 3));
            let end = rope.point_to_offset(Point::new(2, 7));
            snap.anchor_at(start, Bias::Right)..snap.anchor_at(end, Bias::Left)
        };
        display_map.highlight_text(
            HighlightKey::layer(HighlightLayer::SearchHighlight),
            vec![range],
            HighlightStyle {
                foreground: Some(Color::Red),
                ..Default::default()
            },
        );

        let snapshot = display_map.snapshot();
        assert_eq!(
            snapshot.line_count(),
            1,
            "fold collapses to one display row"
        );

        let styled: String = snapshot
            .highlighted_chunks(0..1)
            .filter(|c| c.highlight_style.is_some())
            .map(|c| c.text.into_owned())
            .collect();
        assert_eq!(styled, "TAIL");
    }

    #[test]
    fn highlight_on_later_row_survives_multi_row_streaming() {
        let mut display_map = create_display_map("alpha\nbeta\ngamma");
        let range = {
            let snap = display_map.multi_buffer.snapshot();
            let rope = snap.rope();
            let start = rope.point_to_offset(Point::new(2, 0));
            let end = rope.point_to_offset(Point::new(2, 5));
            snap.anchor_at(start, Bias::Right)..snap.anchor_at(end, Bias::Left)
        };
        display_map.highlight_text(
            HighlightKey::layer(HighlightLayer::SearchHighlight),
            vec![range],
            HighlightStyle {
                foreground: Some(Color::Red),
                ..Default::default()
            },
        );

        let snapshot = display_map.snapshot();
        let total = snapshot.line_count();
        let styled: String = snapshot
            .highlighted_chunks(0..total)
            .filter(|c| c.highlight_style.is_some())
            .map(|c| c.text.into_owned())
            .collect();
        assert_eq!(styled, "gamma");
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
    fn snapshot_open_rust_file_with_fold() {
        use stoat_text::Point;
        let mut h = crate::test_harness::TestHarness::with_size(40, 8);
        let path = h.write_file("folded.rs", "fn a() { 1 }\nfn b() { 2 }\nfn c() { 3 }\n");

        h.open_file(&path);
        h.fold_focused(Point::new(1, 7)..Point::new(1, 12));
        h.assert_snapshot("snapshot_open_rust_file_with_fold");
    }
}
