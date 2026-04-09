use super::{
    fold_map::FoldPointCursor,
    highlights::{Chunk, Highlights},
    inlay_map::InlayPointCursor,
    wrap_map::{WrapPointCursor, WrapSnapshot},
    Companion, DisplayMapId,
};
use crate::{
    buffer::BufferId,
    diff_map::DiffHunkStatus,
    multi_buffer::{ExcerptId, ExcerptInfo, MultiBufferSnapshot},
};
use ratatui::text::Line;
use std::{
    cmp::Ordering,
    collections::HashSet,
    ops::Deref,
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Arc,
    },
};
use stoat_text::{
    patch::Patch, tree_map::TreeMap, Bias, ContextLessSummary, Dimension, Dimensions, Item, Point,
    SeekTarget, SumTree,
};

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockPoint {
    pub row: u32,
    pub column: u32,
}

impl BlockPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self { row, column }
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockRow(pub u32);

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CustomBlockId(pub usize);

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpacerId(pub usize);

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub enum BlockStyle {
    Fixed,
    Flex,
    Spacer,
    Sticky,
}

/// Render callback producing styled terminal lines for a block.
pub type RenderBlock = Arc<dyn Fn(&BlockContext<'_>) -> Vec<Line<'static>> + Send + Sync>;

pub struct BlockContext<'a> {
    pub block_id: BlockId,
    pub max_width: u32,
    pub height: u32,
    pub selected: bool,
    pub anchor_row: u32,
    pub diff_status: Option<DiffHunkStatus>,
    pub buffer_snapshot: &'a MultiBufferSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BlockId {
    Custom(CustomBlockId),
    ExcerptBoundary(ExcerptId),
    BufferHeader(ExcerptId),
    FoldedBuffer(ExcerptId),
    Spacer(SpacerId),
}

pub struct CompanionView<'a> {
    pub display_map_id: DisplayMapId,
    pub companion_wrap_snapshot: &'a WrapSnapshot,
    pub companion_wrap_edits: &'a Patch<u32>,
    pub companion: &'a Companion,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BlockPlacement<T = u32> {
    Above(T),
    Below(T),
    Near(T),
    Replace { start: T, end: T },
}

impl<T: Copy> BlockPlacement<T> {
    pub fn start(&self) -> T {
        match self {
            BlockPlacement::Above(v) | BlockPlacement::Below(v) | BlockPlacement::Near(v) => *v,
            BlockPlacement::Replace { start, .. } => *start,
        }
    }

    pub fn end(&self) -> T {
        match self {
            BlockPlacement::Above(v) | BlockPlacement::Below(v) | BlockPlacement::Near(v) => *v,
            BlockPlacement::Replace { end, .. } => *end,
        }
    }

    pub fn map<U: Copy>(&self, f: impl Fn(T) -> U) -> BlockPlacement<U> {
        match self {
            BlockPlacement::Above(v) => BlockPlacement::Above(f(*v)),
            BlockPlacement::Below(v) => BlockPlacement::Below(f(*v)),
            BlockPlacement::Near(v) => BlockPlacement::Near(f(*v)),
            BlockPlacement::Replace { start, end } => BlockPlacement::Replace {
                start: f(*start),
                end: f(*end),
            },
        }
    }
}

impl BlockPlacement<u32> {
    fn start_row(&self) -> u32 {
        self.start()
    }
}

#[derive(Copy, Clone, Debug)]
enum ResolvedPlacement {
    Above(u32),
    Below(u32),
    Near(u32),
    Replace { start: u32, end: u32 },
}

impl ResolvedPlacement {
    fn start_wrap_row(&self) -> u32 {
        match self {
            ResolvedPlacement::Above(r)
            | ResolvedPlacement::Below(r)
            | ResolvedPlacement::Near(r) => *r,
            ResolvedPlacement::Replace { start, .. } => *start,
        }
    }

    fn input_rows(&self) -> u32 {
        match self {
            ResolvedPlacement::Above(_)
            | ResolvedPlacement::Below(_)
            | ResolvedPlacement::Near(_) => 0,
            ResolvedPlacement::Replace { start, end } => end - start + 1,
        }
    }
}

#[derive(Clone)]
pub struct BlockProperties {
    pub placement: BlockPlacement,
    pub height: Option<u32>,
    pub style: BlockStyle,
    pub render: RenderBlock,
    pub diff_status: Option<DiffHunkStatus>,
    pub priority: usize,
}

impl std::fmt::Debug for BlockProperties {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockProperties")
            .field("placement", &self.placement)
            .field("height", &self.height)
            .field("style", &self.style)
            .finish()
    }
}

impl BlockProperties {
    pub fn from_text(placement: BlockPlacement, lines: Vec<String>, style: BlockStyle) -> Self {
        let height = lines.len().max(1) as u32;
        let lines = Arc::new(lines);
        Self {
            placement,
            height: Some(height),
            style,
            render: Arc::new(move |_ctx| lines.iter().map(|l| Line::raw(l.clone())).collect()),
            diff_status: None,
            priority: 0,
        }
    }

    pub fn from_lines_fn(
        placement: BlockPlacement,
        line_count: u32,
        get_line: Arc<dyn Fn(u32) -> String + Send + Sync>,
        style: BlockStyle,
    ) -> Self {
        Self {
            placement,
            height: Some(line_count),
            style,
            render: Arc::new(move |_ctx| (0..line_count).map(|i| Line::raw(get_line(i))).collect()),
            diff_status: None,
            priority: 0,
        }
    }
}

#[derive(Clone)]
pub struct CustomBlock {
    pub id: CustomBlockId,
    pub placement: BlockPlacement,
    pub height: Option<u32>,
    pub render: RenderBlock,
    pub diff_status: Option<DiffHunkStatus>,
    pub style: BlockStyle,
    pub priority: usize,
}

impl std::fmt::Debug for CustomBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomBlock")
            .field("id", &self.id)
            .field("placement", &self.placement)
            .field("height", &self.height)
            .field("style", &self.style)
            .field("priority", &self.priority)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub enum Block {
    Custom(Arc<CustomBlock>),
    FoldedBuffer {
        first_excerpt: ExcerptInfo,
        height: u32,
    },
    ExcerptBoundary {
        excerpt: ExcerptInfo,
        height: u32,
    },
    BufferHeader {
        excerpt: ExcerptInfo,
        height: u32,
    },
    Spacer {
        id: SpacerId,
        height: u32,
        is_below: bool,
    },
}

impl Block {
    pub fn height(&self) -> u32 {
        match self {
            Block::Custom(b) => b.height.unwrap_or(0),
            Block::FoldedBuffer { height, .. }
            | Block::ExcerptBoundary { height, .. }
            | Block::BufferHeader { height, .. }
            | Block::Spacer { height, .. } => *height,
        }
    }

    pub fn render_lines(&self, ctx: &BlockContext<'_>) -> Vec<Line<'static>> {
        match self {
            Block::Custom(b) => (b.render)(ctx),
            _ => vec![Line::raw(String::new()); self.height() as usize],
        }
    }

    fn default_ctx(&self) -> BlockContext<'static> {
        // Non-Custom blocks render static content and don't access the buffer.
        // Custom blocks receive a real BlockContext through the display pipeline.
        static EMPTY_SNAPSHOT: std::sync::LazyLock<MultiBufferSnapshot> =
            std::sync::LazyLock::new(MultiBufferSnapshot::empty);
        BlockContext {
            block_id: match self {
                Block::Custom(b) => BlockId::Custom(b.id),
                Block::FoldedBuffer { first_excerpt, .. } => {
                    BlockId::FoldedBuffer(first_excerpt.id)
                },
                Block::ExcerptBoundary { excerpt, .. } => BlockId::ExcerptBoundary(excerpt.id),
                Block::BufferHeader { excerpt, .. } => BlockId::BufferHeader(excerpt.id),
                Block::Spacer { id, .. } => BlockId::Spacer(*id),
            },
            max_width: 256,
            height: self.height(),
            selected: false,
            anchor_row: 0,
            diff_status: match self {
                Block::Custom(b) => b.diff_status,
                _ => None,
            },
            buffer_snapshot: &EMPTY_SNAPSHOT,
        }
    }

    pub fn get_line(&self, index: u32) -> String {
        match self {
            Block::Custom(b) => {
                let ctx = self.default_ctx();
                let lines = (b.render)(&ctx);
                lines
                    .get(index as usize)
                    .map(|l| l.to_string())
                    .unwrap_or_default()
            },
            _ => String::new(),
        }
    }

    pub fn line_len(&self, index: u32) -> u32 {
        self.get_line(index).len() as u32
    }

    pub fn write_line(&self, buf: &mut String, index: u32) {
        buf.push_str(&self.get_line(index));
    }

    fn placement(&self) -> BlockPlacement {
        match self {
            Block::Custom(b) => b.placement,
            Block::FoldedBuffer { .. } => BlockPlacement::Replace { start: 0, end: 0 },
            Block::ExcerptBoundary { .. } | Block::BufferHeader { .. } => BlockPlacement::Above(0),
            Block::Spacer { is_below, .. } => {
                if *is_below {
                    BlockPlacement::Below(0)
                } else {
                    BlockPlacement::Above(0)
                }
            },
        }
    }

    fn is_replacement(&self) -> bool {
        match self {
            Block::Custom(b) => matches!(b.placement, BlockPlacement::Replace { .. }),
            Block::FoldedBuffer { .. } => true,
            _ => false,
        }
    }

    fn place_below(&self) -> bool {
        match self {
            Block::Custom(b) => matches!(
                b.placement,
                BlockPlacement::Below(_) | BlockPlacement::Near(_)
            ),
            Block::Spacer { is_below, .. } => *is_below,
            _ => false,
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct TransformSummary {
    pub input_rows: u32,
    pub output_rows: u32,
    pub longest_row: u32,
    pub longest_row_chars: u32,
}

impl ContextLessSummary for TransformSummary {
    fn add_summary(&mut self, other: &Self) {
        if other.longest_row_chars > self.longest_row_chars {
            self.longest_row = self.output_rows + other.longest_row;
            self.longest_row_chars = other.longest_row_chars;
        }
        self.input_rows += other.input_rows;
        self.output_rows += other.output_rows;
    }
}

#[derive(Clone, Debug)]
pub struct Transform {
    pub summary: TransformSummary,
    pub block: Option<Block>,
}

impl Item for Transform {
    type Summary = TransformSummary;
    fn summary(&self, _cx: ()) -> TransformSummary {
        self.summary.clone()
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InputRow(pub u32);

impl<'a> Dimension<'a, TransformSummary> for InputRow {
    fn zero(_cx: ()) -> Self {
        InputRow(0)
    }
    fn add_summary(&mut self, summary: &'a TransformSummary, _cx: ()) {
        self.0 += summary.input_rows;
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OutputRow(pub u32);

impl<'a> Dimension<'a, TransformSummary> for OutputRow {
    fn zero(_cx: ()) -> Self {
        OutputRow(0)
    }
    fn add_summary(&mut self, summary: &'a TransformSummary, _cx: ()) {
        self.0 += summary.output_rows;
    }
}

impl<'a> SeekTarget<'a, TransformSummary, Dimensions<InputRow, OutputRow>> for OutputRow {
    fn cmp(&self, cursor_location: &Dimensions<InputRow, OutputRow>, _cx: ()) -> Ordering {
        Ord::cmp(&self.0, &cursor_location.1 .0)
    }
}

pub enum BlockRowKind<'a> {
    BufferRow { buffer_row: u32 },
    Block { block: &'a Block, line_index: u32 },
}

use super::{highlights::HighlightEndpoint, wrap_map::WrapChunks};

/// Iterator over a range of block rows, emitting [`Chunk`]s that propagate
/// highlight styles from the wrap layer below.
///
/// Walks the block transform tree row-by-row. For block transforms, emits one
/// unstyled chunk per block line. For regular transforms, forwards chunks from
/// [`WrapSnapshot::chunks`] for the matching wrap row. Newline separators are
/// inserted between rows.
pub struct BlockChunks<'a> {
    snapshot: &'a BlockSnapshot,
    endpoints: Arc<[HighlightEndpoint]>,
    current_row: u32,
    end_row: u32,
    pending_wrap_chunks: Option<WrapChunks<'a>>,
    pending_newline: bool,
}

impl<'a> Iterator for BlockChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Chunk<'a>> {
        loop {
            if self.pending_newline {
                self.pending_newline = false;
                return Some(Chunk {
                    text: std::borrow::Cow::Borrowed("\n"),
                    ..Default::default()
                });
            }

            if let Some(wc) = self.pending_wrap_chunks.as_mut() {
                if let Some(chunk) = wc.next() {
                    return Some(chunk);
                }
                self.pending_wrap_chunks = None;
                self.current_row += 1;
                if self.current_row < self.end_row {
                    self.pending_newline = true;
                }
                continue;
            }

            if self.current_row >= self.end_row {
                return None;
            }

            // Classify the current row via the block transform cursor.
            let target = OutputRow(self.current_row + 1);
            let mut cursor = self
                .snapshot
                .transforms
                .cursor::<Dimensions<InputRow, OutputRow>>(());
            cursor.seek(&target, Bias::Left);
            let Dimensions(input_start, output_start, _) = *cursor.start();
            let rows_into_transform = self.current_row - output_start.0;

            let is_block = cursor.item().and_then(|t| t.block.as_ref()).is_some();

            if is_block {
                let mut line = String::new();
                if let Some(transform) = cursor.item() {
                    if let Some(ref block) = transform.block {
                        block.write_line(&mut line, rows_into_transform);
                    }
                }
                self.current_row += 1;
                if self.current_row < self.end_row {
                    self.pending_newline = true;
                }
                return Some(Chunk {
                    text: std::borrow::Cow::Owned(line),
                    ..Default::default()
                });
            }

            // Regular transform: pull chunks from the wrap layer for exactly
            // one wrap row.
            let wrap_row = input_start.0 + rows_into_transform;
            self.pending_wrap_chunks = Some(
                self.snapshot
                    .wrap_snapshot
                    .chunks(wrap_row..wrap_row + 1, self.endpoints.clone()),
            );
        }
    }
}

pub struct BlockMap {
    next_block_id: AtomicUsize,
    next_spacer_id: AtomicUsize,
    custom_blocks: Vec<Arc<CustomBlock>>,
    custom_blocks_by_id: TreeMap<CustomBlockId, Arc<CustomBlock>>,
    transforms: Option<SumTree<Transform>>,
    total_rows: u32,
    blocks_dirty: bool,
    deferred_edits: Patch<u32>,
    buffer_header_height: u32,
    excerpt_header_height: u32,
    folded_buffers: HashSet<BufferId>,
    buffers_with_disabled_headers: HashSet<BufferId>,
}

impl Default for BlockMap {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockMap {
    pub fn new() -> Self {
        Self {
            next_block_id: AtomicUsize::new(0),
            next_spacer_id: AtomicUsize::new(0),
            custom_blocks: Vec::new(),
            custom_blocks_by_id: TreeMap::default(),
            transforms: None,
            total_rows: 0,
            blocks_dirty: true,
            deferred_edits: Patch::empty(),
            buffer_header_height: 1,
            excerpt_header_height: 1,
            folded_buffers: HashSet::new(),
            buffers_with_disabled_headers: HashSet::new(),
        }
    }

    pub fn mark_dirty(&mut self) {
        self.blocks_dirty = true;
    }

    pub fn insert(&mut self, blocks: Vec<BlockProperties>) -> Vec<CustomBlockId> {
        let mut ids = Vec::with_capacity(blocks.len());
        for props in blocks {
            let id = CustomBlockId(self.next_block_id.fetch_add(1, SeqCst));
            let block = Arc::new(CustomBlock {
                id,
                placement: props.placement,
                height: props.height,
                render: props.render,
                diff_status: props.diff_status,
                style: props.style,
                priority: props.priority,
            });
            let ix = self
                .custom_blocks
                .partition_point(|b| b.placement.start_row() <= props.placement.start_row());
            self.custom_blocks.insert(ix, block.clone());
            self.custom_blocks_by_id.insert(id, block);
            ids.push(id);
        }
        self.blocks_dirty = true;
        ids
    }

    pub fn remove(&mut self, ids: &HashSet<CustomBlockId>) {
        if ids.is_empty() {
            return;
        }
        self.custom_blocks.retain(|b| !ids.contains(&b.id));
        for id in ids {
            self.custom_blocks_by_id.remove(id);
        }
        self.blocks_dirty = true;
    }

    pub fn folded_buffers(&self) -> &HashSet<BufferId> {
        &self.folded_buffers
    }

    pub fn fold_buffer(&mut self, buffer_id: BufferId) {
        self.folded_buffers.insert(buffer_id);
        self.blocks_dirty = true;
    }

    pub fn unfold_buffer(&mut self, buffer_id: BufferId) {
        self.folded_buffers.remove(&buffer_id);
        self.blocks_dirty = true;
    }

    pub fn disable_header_for_buffer(&mut self, buffer_id: BufferId) {
        self.buffers_with_disabled_headers.insert(buffer_id);
        self.blocks_dirty = true;
    }

    pub fn sync(
        &mut self,
        wrap_snapshot: Arc<WrapSnapshot>,
        wrap_edits: &Patch<u32>,
        companion_view: Option<CompanionView<'_>>,
    ) -> BlockSnapshot {
        let mut edits = if self.deferred_edits.is_empty() {
            wrap_edits.clone()
        } else {
            let deferred = std::mem::replace(&mut self.deferred_edits, Patch::empty());
            deferred.compose(wrap_edits.edits().iter().cloned())
        };

        // Pull in companion edits: when the companion changes, we need to
        // recompute spacer blocks in the affected region.
        if let Some(ref cv) = companion_view {
            if !cv.companion_wrap_edits.is_empty() {
                let our_buffer = wrap_snapshot
                    .tab_snapshot()
                    .fold_snapshot()
                    .inlay_snapshot()
                    .buffer_snapshot();
                let their_buffer = cv
                    .companion_wrap_snapshot
                    .tab_snapshot()
                    .fold_snapshot()
                    .inlay_snapshot()
                    .buffer_snapshot();

                let mut merged = Patch::empty();
                for edit in cv.companion_wrap_edits.edits() {
                    let companion_row =
                        wrap_row_to_buffer_row(edit.new.start, cv.companion_wrap_snapshot);
                    let our_range = cv.companion.convert_point_from_companion(
                        cv.display_map_id,
                        our_buffer,
                        their_buffer,
                        Point::new(companion_row, 0),
                    );
                    let our_wrap_start =
                        buffer_row_to_wrap_row(our_range.start.row, &wrap_snapshot);
                    let our_wrap_end = buffer_row_to_wrap_row(our_range.end.row, &wrap_snapshot)
                        .max(our_wrap_start + 1);
                    merged.push(stoat_text::patch::Edit {
                        old: our_wrap_start..our_wrap_end,
                        new: our_wrap_start..our_wrap_end,
                    });
                }
                if !merged.is_empty() {
                    edits = edits.compose(merged.into_inner());
                }
            }
        }

        if edits.is_empty() && !self.blocks_dirty {
            if let Some(ref transforms) = self.transforms {
                return BlockSnapshot {
                    wrap_snapshot,
                    transforms: transforms.clone(),
                    total_rows: self.total_rows,
                };
            }
        }

        let wrap_line_count = wrap_snapshot.line_count();

        let buffer_snapshot = wrap_snapshot
            .tab_snapshot()
            .fold_snapshot()
            .inlay_snapshot()
            .buffer_snapshot();
        let mut blocks: Vec<Block> = self
            .custom_blocks
            .iter()
            .map(|b| Block::Custom(b.clone()))
            .collect();
        blocks.extend(
            self.header_and_footer_blocks(buffer_snapshot)
                .into_iter()
                .map(|(_placement, block)| block),
        );
        if let Some(ref companion_view) = companion_view {
            blocks.extend(
                self.spacer_blocks(&wrap_snapshot, companion_view)
                    .into_iter()
                    .map(|(_placement, block)| block),
            );
        }

        let can_incremental = !self.blocks_dirty && !edits.is_empty() && self.transforms.is_some();

        let transforms = if can_incremental {
            sync_incremental(
                self.transforms
                    .as_ref()
                    .expect("guarded by can_incremental"),
                wrap_line_count,
                &blocks,
                &wrap_snapshot,
                &edits,
            )
        } else {
            build_transforms(wrap_line_count, &blocks, &wrap_snapshot)
        };

        let total_rows: OutputRow = transforms.extent(());

        self.transforms = Some(transforms.clone());
        self.total_rows = total_rows.0;
        self.blocks_dirty = false;

        BlockSnapshot {
            wrap_snapshot,
            transforms,
            total_rows: total_rows.0,
        }
    }

    fn header_and_footer_blocks(
        &self,
        buffer: &MultiBufferSnapshot,
    ) -> Vec<(BlockPlacement, Block)> {
        if !buffer.show_headers() {
            return Vec::new();
        }

        let mut results = Vec::new();
        for boundary in buffer.excerpt_boundaries_in_range(0..buffer.line_count()) {
            if self
                .buffers_with_disabled_headers
                .contains(&boundary.next.buffer_id)
            {
                continue;
            }

            if boundary.starts_new_buffer() {
                if self.folded_buffers.contains(&boundary.next.buffer_id) {
                    results.push((
                        BlockPlacement::Replace {
                            start: boundary.row,
                            end: boundary.row,
                        },
                        Block::FoldedBuffer {
                            first_excerpt: boundary.next.clone(),
                            height: self.buffer_header_height,
                        },
                    ));
                } else {
                    results.push((
                        BlockPlacement::Above(boundary.row),
                        Block::BufferHeader {
                            excerpt: boundary.next,
                            height: self.buffer_header_height,
                        },
                    ));
                }
            } else if boundary.prev.is_some() {
                results.push((
                    BlockPlacement::Above(boundary.row),
                    Block::ExcerptBoundary {
                        excerpt: boundary.next,
                        height: self.excerpt_header_height,
                    },
                ));
            }
        }

        results
    }

    fn spacer_blocks(
        &self,
        wrap_snapshot: &WrapSnapshot,
        companion_view: &CompanionView<'_>,
    ) -> Vec<(BlockPlacement, Block)> {
        let companion = companion_view.companion;
        let our_snapshot = wrap_snapshot
            .tab_snapshot()
            .fold_snapshot()
            .inlay_snapshot()
            .buffer_snapshot();
        let companion_snapshot = companion_view
            .companion_wrap_snapshot
            .tab_snapshot()
            .fold_snapshot()
            .inlay_snapshot()
            .buffer_snapshot();

        let convert_fn = companion.rows_to_companion(companion_view.display_map_id);
        let excerpt_map = companion.excerpt_map(companion_view.display_map_id);
        let patches = convert_fn(
            excerpt_map,
            companion_snapshot,
            our_snapshot,
            (std::ops::Bound::Unbounded, std::ops::Bound::Unbounded),
        );

        let mut spacers = Vec::new();
        for patch in &patches {
            let mut delta: i64 = 0;

            for edit in patch.patch.edits() {
                let our_start_wrap =
                    buffer_row_to_wrap_row(edit.new.start.row, wrap_snapshot) as i64;
                let our_end_wrap = buffer_row_to_wrap_row(edit.new.end.row, wrap_snapshot) as i64;
                let companion_start_wrap = buffer_row_to_wrap_row(
                    edit.old.start.row,
                    companion_view.companion_wrap_snapshot,
                ) as i64;
                let companion_end_wrap = buffer_row_to_wrap_row(
                    edit.old.end.row,
                    companion_view.companion_wrap_snapshot,
                ) as i64;

                let our_rows = our_end_wrap - our_start_wrap;
                let companion_rows = companion_end_wrap - companion_start_wrap;
                let new_delta = delta + (companion_rows - our_rows);

                if new_delta > delta {
                    let height = (new_delta - delta) as u32;
                    let spacer_id = SpacerId(
                        self.next_spacer_id
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    );
                    spacers.push((
                        BlockPlacement::Above(edit.new.start.row),
                        Block::Spacer {
                            id: spacer_id,
                            height,
                            is_below: false,
                        },
                    ));
                }

                delta = new_delta;
            }

            if delta > 0 {
                if let Some(last_edit) = patch.patch.edits().last() {
                    let spacer_id = SpacerId(
                        self.next_spacer_id
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    );
                    spacers.push((
                        BlockPlacement::Below(last_edit.new.end.row),
                        Block::Spacer {
                            id: spacer_id,
                            height: delta as u32,
                            is_below: true,
                        },
                    ));
                }
            }
        }
        spacers
    }
}

#[derive(Clone)]
pub struct BlockSnapshot {
    wrap_snapshot: Arc<WrapSnapshot>,
    transforms: SumTree<Transform>,
    total_rows: u32,
}

impl Deref for BlockSnapshot {
    type Target = WrapSnapshot;
    fn deref(&self) -> &WrapSnapshot {
        &self.wrap_snapshot
    }
}

impl BlockSnapshot {
    pub fn buffer_to_block(&self, point: Point) -> BlockPoint {
        let inlay_point = self
            .wrap_snapshot
            .tab_snapshot()
            .fold_snapshot()
            .inlay_snapshot()
            .to_inlay_point(point);
        let fold_point = self
            .wrap_snapshot
            .tab_snapshot()
            .fold_snapshot()
            .to_fold_point(inlay_point, Bias::Right);
        let tab_point = self.wrap_snapshot.tab_snapshot().to_tab_point(fold_point);
        let wrap_point = self.wrap_snapshot.to_wrap_point(tab_point);
        let wrap_row = wrap_point.row();

        let target = InputRow(wrap_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input, output, _) = cursor.start();
        let rows_into_transform = wrap_row.saturating_sub(input.0);
        let block_row = output.0 + rows_into_transform;

        BlockPoint {
            row: block_row,
            column: wrap_point.column(),
        }
    }

    pub fn block_to_buffer(&self, point: BlockPoint) -> Option<Point> {
        let target = OutputRow(point.row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = point.row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if transform.block.is_some() {
                return None;
            }
        }

        let wrap_row = input_start.0 + rows_into_transform;
        let wrap_point = super::wrap_map::WrapPoint::new(wrap_row, point.column);
        let tab_point = self.wrap_snapshot.to_tab_point(wrap_point);
        let fold_point = self
            .wrap_snapshot
            .tab_snapshot()
            .to_fold_point(tab_point, Bias::Left);
        let inlay_point = self
            .wrap_snapshot
            .tab_snapshot()
            .fold_snapshot()
            .to_inlay_point(fold_point);
        let buf = self
            .wrap_snapshot
            .tab_snapshot()
            .fold_snapshot()
            .inlay_snapshot()
            .to_buffer_point(inlay_point);
        Some(buf)
    }

    pub fn classify_row(&self, block_row: u32) -> BlockRowKind<'_> {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = block_row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if let Some(ref block) = transform.block {
                return BlockRowKind::Block {
                    block,
                    line_index: rows_into_transform,
                };
            }
        }

        let wrap_row = input_start.0 + rows_into_transform;
        let tab_point = self
            .wrap_snapshot
            .to_tab_point(super::wrap_map::WrapPoint::new(wrap_row, 0));
        let inlay_point = self
            .wrap_snapshot
            .fold_snapshot()
            .to_inlay_point(super::fold_map::FoldPoint::new(tab_point.row(), 0));
        let buffer_point = self
            .wrap_snapshot
            .fold_snapshot()
            .inlay_snapshot()
            .to_buffer_point(inlay_point);

        BlockRowKind::BufferRow {
            buffer_row: buffer_point.row,
        }
    }

    pub fn clip_point(&self, point: BlockPoint, bias: Bias) -> BlockPoint {
        let row = point.row.min(self.total_rows.saturating_sub(1));
        match self.classify_row(row) {
            BlockRowKind::BufferRow { .. } => {
                let col = point.column.min(self.line_len(row));
                BlockPoint::new(row, col)
            },
            BlockRowKind::Block { .. } => {
                let target = OutputRow(row + 1);
                let mut cursor = self
                    .transforms
                    .cursor::<Dimensions<InputRow, OutputRow>>(());
                cursor.seek(&target, Bias::Left);

                if bias == Bias::Left {
                    cursor.prev();
                    while let Some(t) = cursor.item() {
                        if t.block.is_none() {
                            let end = cursor.end();
                            let last_buf_row = end.1 .0.saturating_sub(1);
                            return BlockPoint::new(last_buf_row, self.line_len(last_buf_row));
                        }
                        cursor.prev();
                    }
                    BlockPoint::new(0, 0)
                } else {
                    cursor.next();
                    while let Some(t) = cursor.item() {
                        if t.block.is_none() {
                            let start_row = cursor.start().1 .0;
                            return BlockPoint::new(start_row, 0);
                        }
                        cursor.next();
                    }
                    self.max_point()
                }
            },
        }
    }

    pub fn line_len(&self, block_row: u32) -> u32 {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = block_row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if let Some(ref block) = transform.block {
                return block.line_len(rows_into_transform);
            }
        }

        let wrap_row = input_start.0 + rows_into_transform;
        self.wrap_snapshot.line_len(wrap_row)
    }

    pub fn max_point(&self) -> BlockPoint {
        let last_row = self.total_rows.saturating_sub(1);
        BlockPoint::new(last_row, self.line_len(last_row))
    }

    pub fn total_lines(&self) -> u32 {
        self.total_rows
    }

    pub fn buffer_line_count(&self) -> u32 {
        self.wrap_snapshot
            .fold_snapshot()
            .inlay_snapshot()
            .buffer_snapshot()
            .line_count()
    }

    pub fn buffer_text(&self) -> &str {
        self.wrap_snapshot
            .fold_snapshot()
            .inlay_snapshot()
            .buffer_snapshot()
            .text()
    }

    pub fn buffer_lines(&self) -> impl Iterator<Item = &str> {
        self.wrap_snapshot
            .fold_snapshot()
            .inlay_snapshot()
            .buffer_snapshot()
            .lines()
    }

    pub fn buffer_snapshot(&self) -> &MultiBufferSnapshot {
        self.wrap_snapshot
            .fold_snapshot()
            .inlay_snapshot()
            .buffer_snapshot()
    }

    pub fn longest_row(&self) -> (u32, u32) {
        let s = self.transforms.summary();
        (s.longest_row, s.longest_row_chars)
    }

    pub fn wrap_snapshot(&self) -> &WrapSnapshot {
        &self.wrap_snapshot
    }

    pub fn write_display_line(&self, buf: &mut String, block_row: u32) {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = block_row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if let Some(ref block) = transform.block {
                block.write_line(buf, rows_into_transform);
                return;
            }
        }

        let wrap_row = input_start.0 + rows_into_transform;
        self.wrap_snapshot.write_display_line(buf, wrap_row);
    }

    pub fn display_line(&self, block_row: u32) -> String {
        let mut result = String::new();
        self.write_display_line(&mut result, block_row);
        result
    }

    pub fn chunks(
        &self,
        rows: std::ops::Range<u32>,
        endpoints: Arc<[HighlightEndpoint]>,
    ) -> BlockChunks<'_> {
        BlockChunks {
            snapshot: self,
            endpoints,
            current_row: rows.start,
            end_row: rows.end,
            pending_wrap_chunks: None,
            pending_newline: false,
        }
    }

    /// Conservatively bound the rope byte range covering `rows`.
    ///
    /// Walks forward from `rows.start` (and backward from `rows.end - 1`) to
    /// find the first display rows that map to a buffer point. Display rows
    /// inside custom blocks have no buffer mapping and are skipped. The end
    /// is taken at the start of the buffer line *after* the last visible row
    /// so its full content is included.
    ///
    /// Used by [`crate::display_map::DisplayMap::build_endpoints`] to bound
    /// highlight endpoint construction to the viewport instead of the whole
    /// rope.
    pub fn row_range_to_buffer_byte_range(
        &self,
        rows: std::ops::Range<u32>,
    ) -> std::ops::Range<usize> {
        let buffer = self.buffer_snapshot();
        let rope = buffer.rope();
        let total = rope.len();
        if rows.start >= rows.end || total == 0 {
            return 0..0;
        }

        let max_row = self.total_rows;
        let start_row = rows.start.min(max_row);
        let end_row = rows.end.min(max_row);

        let start_offset = (start_row..end_row)
            .find_map(|r| self.block_to_buffer(BlockPoint::new(r, 0)))
            .map(|p| rope.point_to_offset(p))
            .unwrap_or(total);

        let end_offset = (start_row..end_row)
            .rev()
            .find_map(|r| self.block_to_buffer(BlockPoint::new(r, 0)))
            .map(|p| {
                // Take through the start of the next buffer line so the
                // entire visible row's content (incl. any trailing newline)
                // is covered. point_to_offset clamps past-the-end points.
                rope.point_to_offset(Point::new(p.row + 1, 0)).min(total)
            })
            .unwrap_or(start_offset);

        start_offset.min(end_offset)..end_offset.max(start_offset)
    }

    pub fn soft_wrap_indent(&self, block_row: u32) -> u32 {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = block_row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if transform.block.is_some() {
                return 0;
            }
        }

        let wrap_row = input_start.0 + rows_into_transform;
        self.wrap_snapshot.soft_wrap_indent(wrap_row)
    }

    pub fn wrap_width(&self) -> Option<u32> {
        self.wrap_snapshot.wrap_width()
    }

    pub fn is_wrap_continuation(&self, block_row: u32) -> bool {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = block_row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if transform.block.is_some() {
                return false;
            }
        }

        let wrap_row = input_start.0 + rows_into_transform;
        self.wrap_snapshot.classify_row(wrap_row) == super::wrap_map::WrapRowKind::Continuation
    }
}

fn sort_and_dedup_blocks(blocks: &mut Vec<(ResolvedPlacement, &Block)>) {
    blocks.sort_unstable_by(|(a, _), (b, _)| {
        a.start_wrap_row()
            .cmp(&b.start_wrap_row())
            .then_with(|| {
                let a_end = match a {
                    ResolvedPlacement::Replace { end, .. } => *end,
                    _ => a.start_wrap_row(),
                };
                let b_end = match b {
                    ResolvedPlacement::Replace { end, .. } => *end,
                    _ => b.start_wrap_row(),
                };
                b_end.cmp(&a_end)
            })
            .then_with(|| {
                fn tie(p: &ResolvedPlacement) -> u8 {
                    match p {
                        ResolvedPlacement::Replace { .. } => 0,
                        ResolvedPlacement::Above(_) => 1,
                        ResolvedPlacement::Near(_) => 2,
                        ResolvedPlacement::Below(_) => 3,
                    }
                }
                tie(a).cmp(&tie(b))
            })
    });

    blocks.dedup_by(|right, left| match (&mut left.0, &right.0) {
        (
            ResolvedPlacement::Replace {
                start: left_start,
                end: left_end,
            },
            ResolvedPlacement::Above(row)
            | ResolvedPlacement::Below(row)
            | ResolvedPlacement::Near(row),
        ) => *row >= *left_start && *row <= *left_end,
        (
            ResolvedPlacement::Replace { end: left_end, .. },
            ResolvedPlacement::Replace {
                start: right_start,
                end: right_end,
            },
        ) => {
            if *right_start <= *left_end {
                *left_end = (*left_end).max(*right_end);
                true
            } else {
                false
            }
        },
        _ => false,
    });
}

fn resolve_block_placement(
    block: &Block,
    inlay_cursor: &mut InlayPointCursor<'_>,
    fold_cursor: &mut FoldPointCursor<'_>,
    wrap_cursor: &mut WrapPointCursor<'_>,
) -> ResolvedPlacement {
    let map_row = |buffer_row: u32,
                   inlay_cursor: &mut InlayPointCursor<'_>,
                   fold_cursor: &mut FoldPointCursor<'_>,
                   wrap_cursor: &mut WrapPointCursor<'_>|
     -> u32 {
        let inlay_point = inlay_cursor.map(Point::new(buffer_row, 0));
        let fold_point = fold_cursor.map(inlay_point, Bias::Right);
        let tab_point = super::tab_map::TabPoint::new(fold_point.row(), fold_point.column());
        wrap_cursor.map(tab_point).row()
    };

    let placement = block.placement();
    match placement {
        BlockPlacement::Above(row) => {
            ResolvedPlacement::Above(map_row(row, inlay_cursor, fold_cursor, wrap_cursor))
        },
        BlockPlacement::Below(row) => {
            ResolvedPlacement::Below(map_row(row, inlay_cursor, fold_cursor, wrap_cursor) + 1)
        },
        BlockPlacement::Near(row) => {
            ResolvedPlacement::Near(map_row(row, inlay_cursor, fold_cursor, wrap_cursor) + 1)
        },
        BlockPlacement::Replace { start, end } => {
            let start_wrap = map_row(start, inlay_cursor, fold_cursor, wrap_cursor);
            let end_wrap = map_row(end, inlay_cursor, fold_cursor, wrap_cursor);
            ResolvedPlacement::Replace {
                start: start_wrap,
                end: end_wrap.max(start_wrap),
            }
        },
    }
}

fn sync_incremental(
    old_transforms: &SumTree<Transform>,
    wrap_line_count: u32,
    blocks: &[Block],
    wrap_snapshot: &WrapSnapshot,
    wrap_edits: &Patch<u32>,
) -> SumTree<Transform> {
    debug_assert!(
        blocks
            .windows(2)
            .all(|w| block_buffer_row(&w[0]) <= block_buffer_row(&w[1])),
        "blocks must be sorted by buffer row"
    );

    let mut new_transforms = SumTree::new(());
    let mut cursor = old_transforms.cursor::<InputRow>(());
    let mut last_block_idx: usize = 0;

    let mut inlay_cursor = wrap_snapshot
        .fold_snapshot()
        .inlay_snapshot()
        .inlay_point_cursor();
    let mut fold_cursor = wrap_snapshot.fold_snapshot().fold_point_cursor();
    let mut wrap_cursor = wrap_snapshot.wrap_point_cursor();
    let mut blocks_in_range: Vec<(ResolvedPlacement, &Block)> = Vec::new();
    let mut edits = wrap_edits.edits().iter().peekable();

    while let Some(edit) = edits.next() {
        new_transforms.append(cursor.slice(&InputRow(edit.old.start), Bias::Left), ());

        // Preserve transforms ending exactly at edit start (matching Zed lines 902-920)
        if let Some(item) = cursor.item() {
            let item_end = cursor.start().0 + item.summary.input_rows;
            if item.summary.input_rows > 0
                && item_end == edit.old.start
                && !item.block.as_ref().is_some_and(|b| b.is_replacement())
            {
                new_transforms.push(item.clone(), ());
                cursor.next();

                while let Some(item) = cursor.item() {
                    if item.block.as_ref().is_some_and(|b| b.place_below()) {
                        new_transforms.push(item.clone(), ());
                        cursor.next();
                    } else {
                        break;
                    }
                }
            }
        }

        // Handle isomorphic prefix if edit starts within a transform
        if let Some(item) = cursor.item() {
            if item.block.is_none() {
                let transform_rows_before_edit = edit.old.start - cursor.start().0;
                if transform_rows_before_edit > 0 {
                    push_isomorphic(
                        &mut new_transforms,
                        transform_rows_before_edit,
                        cursor.start().0,
                        wrap_snapshot,
                    );
                }
            }
        }

        let mut old_end = edit.old.end;
        let mut new_end = edit.new.end;
        loop {
            cursor.seek(&InputRow(old_end), Bias::Left);
            cursor.next();

            let transform_boundary = cursor.start().0;
            let extension = transform_boundary - old_end;
            old_end += extension;
            new_end += extension;

            while let Some(next_edit) = edits.peek() {
                if next_edit.old.start <= cursor.start().0 {
                    old_end = next_edit.old.end;
                    new_end = next_edit.new.end;
                    cursor.seek(&InputRow(old_end), Bias::Left);
                    cursor.next();
                    edits.next();
                } else {
                    break;
                }
            }

            if cursor.start().0 == old_end {
                break;
            }
        }

        // Discard zero-width block transforms at edit end (matching Zed lines 980-991)
        while let Some(item) = cursor.item() {
            if item.summary.input_rows == 0 && item.block.is_some() {
                cursor.next();
            } else {
                break;
            }
        }

        let current_rows: InputRow = new_transforms.extent(());
        if edit.new.start > current_rows.0 {
            let gap = edit.new.start - current_rows.0;
            push_isomorphic(&mut new_transforms, gap, current_rows.0, wrap_snapshot);
        }

        let edit_end = new_end.min(wrap_line_count);

        let edit_start_buf = wrap_row_to_buffer_row(edit.new.start, wrap_snapshot);
        let edit_end_buf = if edit_end >= wrap_line_count {
            u32::MAX
        } else {
            wrap_row_to_buffer_row(edit_end, wrap_snapshot)
        };

        let search_start_buf = edit_start_buf.saturating_sub(1);
        let start_block_idx = last_block_idx
            + blocks[last_block_idx..].partition_point(|b| block_buffer_row(b) < search_start_buf);
        let end_block_idx = if edit_end_buf == u32::MAX {
            blocks.len()
        } else {
            start_block_idx
                + blocks[start_block_idx..].partition_point(|b| block_buffer_row(b) <= edit_end_buf)
        };

        blocks_in_range.clear();
        blocks_in_range.extend(
            blocks[start_block_idx..end_block_idx]
                .iter()
                .filter_map(|b| {
                    let placement = resolve_block_placement(
                        b,
                        &mut inlay_cursor,
                        &mut fold_cursor,
                        &mut wrap_cursor,
                    );
                    let block_start = placement.start_wrap_row();
                    let block_end = match placement {
                        ResolvedPlacement::Replace { end, .. } => end,
                        _ => block_start,
                    };
                    if block_start < edit_end && block_end >= edit.new.start {
                        Some((placement, b))
                    } else {
                        None
                    }
                }),
        );
        sort_and_dedup_blocks(&mut blocks_in_range);

        let mut row = new_transforms.extent::<InputRow>(()).0;
        for &(placement, block) in &blocks_in_range {
            let anchor = placement.start_wrap_row();
            if anchor > row {
                push_isomorphic(&mut new_transforms, anchor - row, row, wrap_snapshot);
                row = anchor;
            }

            let input_rows = placement.input_rows();
            let (blk_longest_row, blk_longest_chars) = longest_block_line(block);
            new_transforms.push(
                Transform {
                    summary: TransformSummary {
                        input_rows,
                        output_rows: block.height(),
                        longest_row: blk_longest_row,
                        longest_row_chars: blk_longest_chars,
                    },
                    block: Some(block.clone()),
                },
                (),
            );
            row += input_rows;
        }

        if edit_end > row {
            push_isomorphic(&mut new_transforms, edit_end - row, row, wrap_snapshot);
        }

        last_block_idx = end_block_idx;
    }

    new_transforms.append(cursor.suffix(), ());

    if new_transforms.is_empty() && wrap_line_count > 0 {
        let (longest_row, longest_row_chars) = wrap_snapshot.longest_line();
        new_transforms.push(
            Transform {
                summary: TransformSummary {
                    input_rows: wrap_line_count,
                    output_rows: wrap_line_count,
                    longest_row,
                    longest_row_chars,
                },
                block: None,
            },
            (),
        );
    }

    debug_assert_eq!(
        new_transforms.extent::<InputRow>(()).0,
        wrap_line_count,
        "transform input_rows must equal wrap line count"
    );

    new_transforms
}

fn build_transforms(
    wrap_line_count: u32,
    blocks: &[Block],
    wrap_snapshot: &WrapSnapshot,
) -> SumTree<Transform> {
    debug_assert!(
        blocks
            .windows(2)
            .all(|w| block_buffer_row(&w[0]) <= block_buffer_row(&w[1])),
        "blocks must be sorted by buffer row"
    );

    let mut transforms = SumTree::new(());

    if blocks.is_empty() {
        if wrap_line_count > 0 {
            let (longest_row, longest_row_chars) = wrap_snapshot.longest_line();
            transforms.push(
                Transform {
                    summary: TransformSummary {
                        input_rows: wrap_line_count,
                        output_rows: wrap_line_count,
                        longest_row,
                        longest_row_chars,
                    },
                    block: None,
                },
                (),
            );
        }
        return transforms;
    }

    let mut inlay_cursor = wrap_snapshot
        .fold_snapshot()
        .inlay_snapshot()
        .inlay_point_cursor();
    let mut fold_cursor = wrap_snapshot.fold_snapshot().fold_point_cursor();
    let mut wrap_cursor = wrap_snapshot.wrap_point_cursor();

    let mut keyed_blocks: Vec<(ResolvedPlacement, &Block)> = Vec::with_capacity(blocks.len());
    for b in blocks {
        keyed_blocks.push((
            resolve_block_placement(b, &mut inlay_cursor, &mut fold_cursor, &mut wrap_cursor),
            b,
        ));
    }
    sort_and_dedup_blocks(&mut keyed_blocks);

    let mut current_wrap_row = 0u32;

    for &(placement, block) in &keyed_blocks {
        let anchor = placement.start_wrap_row();
        if anchor > current_wrap_row {
            push_isomorphic(
                &mut transforms,
                anchor - current_wrap_row,
                current_wrap_row,
                wrap_snapshot,
            );
            current_wrap_row = anchor;
        }

        let input_rows = placement.input_rows();
        let (blk_longest_row, blk_longest_chars) = longest_block_line(block);
        transforms.push(
            Transform {
                summary: TransformSummary {
                    input_rows,
                    output_rows: block.height(),
                    longest_row: blk_longest_row,
                    longest_row_chars: blk_longest_chars,
                },
                block: Some(block.clone()),
            },
            (),
        );
        current_wrap_row += input_rows;
    }

    if current_wrap_row < wrap_line_count {
        let rows = wrap_line_count - current_wrap_row;
        push_isomorphic(&mut transforms, rows, current_wrap_row, wrap_snapshot);
    }

    debug_assert_eq!(
        transforms.extent::<InputRow>(()).0,
        wrap_line_count,
        "transform input_rows must equal wrap line count"
    );

    transforms
}

fn block_buffer_row(block: &Block) -> u32 {
    block.placement().start_row()
}

fn wrap_row_to_buffer_row(wrap_row: u32, wrap_snapshot: &WrapSnapshot) -> u32 {
    let tab_point = wrap_snapshot.to_tab_point(super::wrap_map::WrapPoint::new(wrap_row, 0));
    let inlay_point = wrap_snapshot
        .fold_snapshot()
        .to_inlay_point(super::fold_map::FoldPoint::new(tab_point.row(), 0));
    wrap_snapshot
        .fold_snapshot()
        .inlay_snapshot()
        .to_buffer_point(inlay_point)
        .row
}

fn buffer_row_to_wrap_row(buffer_row: u32, wrap_snapshot: &WrapSnapshot) -> u32 {
    let inlay_point = wrap_snapshot
        .fold_snapshot()
        .inlay_snapshot()
        .to_inlay_point(Point::new(buffer_row, 0));
    let fold_point = wrap_snapshot
        .fold_snapshot()
        .to_fold_point(inlay_point, Bias::Left);
    let tab_point = wrap_snapshot.tab_snapshot().to_tab_point(fold_point);
    wrap_snapshot.to_wrap_point(tab_point).row()
}

pub fn balancing_block(
    block: &CustomBlock,
    our_snapshot: &MultiBufferSnapshot,
    companion_snapshot: &MultiBufferSnapshot,
    companion: &Companion,
    display_map_id: DisplayMapId,
) -> Option<BlockProperties> {
    let our_row = block.placement.start_row();
    let our_point = Point::new(our_row, 0);
    let their_range = companion.convert_point_from_companion(
        display_map_id,
        our_snapshot,
        companion_snapshot,
        our_point,
    );
    let placement = match block.placement {
        BlockPlacement::Above(_) => BlockPlacement::Above(their_range.start.row),
        BlockPlacement::Below(_) => {
            if their_range.start == their_range.end {
                BlockPlacement::Above(their_range.start.row)
            } else {
                BlockPlacement::Below(their_range.start.row)
            }
        },
        BlockPlacement::Near(_) | BlockPlacement::Replace { .. } => return None,
    };
    let height = block.height;
    Some(BlockProperties {
        placement,
        height,
        style: BlockStyle::Spacer,
        render: Arc::new(move |_ctx| {
            let h = height.unwrap_or(0) as usize;
            vec![Line::raw(String::new()); h]
        }),
        diff_status: None,
        priority: block.priority,
    })
}

fn longest_block_line(block: &Block) -> (u32, u32) {
    let mut best_row = 0u32;
    let mut best_chars = 0u32;
    for i in 0..block.height() {
        let len = block.line_len(i);
        if len > best_chars {
            best_row = i;
            best_chars = len;
        }
    }
    (best_row, best_chars)
}

fn push_isomorphic(
    transforms: &mut SumTree<Transform>,
    rows: u32,
    start_wrap_row: u32,
    wrap_snapshot: &WrapSnapshot,
) {
    if rows == 0 {
        return;
    }

    let (longest_row, longest_row_chars) =
        wrap_snapshot.longest_in_output_range(start_wrap_row, rows);

    let mut merged = false;
    transforms.update_last(
        |last| {
            if last.block.is_none() {
                if longest_row_chars > last.summary.longest_row_chars {
                    last.summary.longest_row = last.summary.output_rows + longest_row;
                    last.summary.longest_row_chars = longest_row_chars;
                }
                last.summary.input_rows += rows;
                last.summary.output_rows += rows;
                merged = true;
            }
        },
        (),
    );

    if !merged {
        transforms.push(
            Transform {
                summary: TransformSummary {
                    input_rows: rows,
                    output_rows: rows,
                    longest_row,
                    longest_row_chars,
                },
                block: None,
            },
            (),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockMap, BlockPlacement, BlockPoint, BlockProperties, BlockRowKind, BlockStyle};
    use crate::{
        buffer::{BufferId, TextBuffer},
        display_map::{fold_map::FoldMap, inlay_map::InlayMap, tab_map::TabMap, wrap_map::WrapMap},
        multi_buffer::MultiBuffer,
    };
    use std::sync::{Arc, RwLock};
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_text::{patch::Patch, Bias, Point};

    fn test_executor() -> Executor {
        Executor::new(Arc::new(TestScheduler::new()))
    }

    fn create_block_snapshot(content: &str, props: &[BlockProperties]) -> super::BlockSnapshot {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).unwrap());
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (_, wrap_snapshot) = WrapMap::new(tab_snapshot, None, test_executor());
        let mut block_map = BlockMap::new();
        block_map.insert(props.to_vec());
        block_map.sync(wrap_snapshot, &Patch::empty(), None)
    }

    fn text_block(placement: BlockPlacement, content: &str) -> BlockProperties {
        BlockProperties::from_text(
            placement,
            content.lines().map(String::from).collect(),
            BlockStyle::Fixed,
        )
    }

    #[test]
    fn no_blocks_passthrough() {
        let snapshot = create_block_snapshot("line1\nline2\nline3", &[]);

        assert_eq!(snapshot.total_lines(), 3);

        let block = snapshot.buffer_to_block(Point::new(1, 2));
        assert_eq!(block, BlockPoint::new(1, 2));

        let buffer = snapshot.block_to_buffer(BlockPoint::new(1, 2));
        assert_eq!(buffer, Some(Point::new(1, 2)));
    }

    #[test]
    fn classify_buffer_row_no_blocks() {
        let snapshot = create_block_snapshot("line1\nline2\nline3", &[]);

        match snapshot.classify_row(1) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 1),
            BlockRowKind::Block { .. } => panic!("expected buffer row"),
        }
    }

    #[test]
    fn block_below_first_line() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let snapshot = create_block_snapshot("line1\nline2", &blocks);

        assert_eq!(snapshot.total_lines(), 3);

        match snapshot.classify_row(0) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 0),
            _ => panic!("expected buffer row"),
        }

        match snapshot.classify_row(1) {
            BlockRowKind::Block { block, line_index } => {
                assert_eq!(line_index, 0);
                assert_eq!(block.get_line(0), "deleted");
            },
            _ => panic!("expected block"),
        }

        match snapshot.classify_row(2) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 1),
            _ => panic!("expected buffer row"),
        }
    }

    #[test]
    fn buffer_to_block_with_block() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let snapshot = create_block_snapshot("line1\nline2", &blocks);

        let block = snapshot.buffer_to_block(Point::new(0, 0));
        assert_eq!(block, BlockPoint::new(0, 0));

        let block = snapshot.buffer_to_block(Point::new(1, 0));
        assert_eq!(block, BlockPoint::new(2, 0));
    }

    #[test]
    fn block_to_buffer_returns_none_for_block() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let snapshot = create_block_snapshot("line1\nline2", &blocks);

        assert!(snapshot.block_to_buffer(BlockPoint::new(1, 0)).is_none());
        assert_eq!(
            snapshot.block_to_buffer(BlockPoint::new(2, 0)),
            Some(Point::new(1, 0))
        );
    }

    #[test]
    fn multiline_block() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "del1\ndel2\ndel3")];
        let snapshot = create_block_snapshot("line1\nline2", &blocks);

        assert_eq!(snapshot.total_lines(), 5);

        for (row, expected) in [(1, "del1"), (2, "del2"), (3, "del3")] {
            match snapshot.classify_row(row) {
                BlockRowKind::Block { block, line_index } => {
                    assert_eq!(block.get_line(line_index), expected);
                },
                _ => panic!("expected block at row {}", row),
            }
        }

        match snapshot.classify_row(4) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 1),
            _ => panic!("expected buffer row"),
        }
    }

    #[test]
    fn block_above() {
        let blocks = vec![text_block(BlockPlacement::Above(1), "inserted")];
        let snapshot = create_block_snapshot("line1\nline2", &blocks);

        assert_eq!(snapshot.total_lines(), 3);

        match snapshot.classify_row(0) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 0),
            _ => panic!("expected buffer row"),
        }

        match snapshot.classify_row(1) {
            BlockRowKind::Block { block, .. } => {
                assert_eq!(block.get_line(0), "inserted");
            },
            _ => panic!("expected block"),
        }

        match snapshot.classify_row(2) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 1),
            _ => panic!("expected buffer row"),
        }
    }

    #[test]
    fn multiple_blocks() {
        let blocks = vec![
            text_block(BlockPlacement::Below(0), "after0"),
            text_block(BlockPlacement::Below(1), "after1"),
        ];
        let snapshot = create_block_snapshot("line1\nline2\nline3", &blocks);

        assert_eq!(snapshot.total_lines(), 5);

        let classifications: Vec<_> = (0..5)
            .map(|row| match snapshot.classify_row(row) {
                BlockRowKind::BufferRow { buffer_row } => format!("buf{}", buffer_row),
                BlockRowKind::Block { block, .. } => format!("blk:{}", block.get_line(0)),
            })
            .collect();

        assert_eq!(
            classifications,
            vec!["buf0", "blk:after0", "buf1", "blk:after1", "buf2"]
        );
    }

    #[test]
    fn line_len_no_blocks() {
        let snapshot = create_block_snapshot("hello\nhi", &[]);
        assert_eq!(snapshot.line_len(0), 5);
        assert_eq!(snapshot.line_len(1), 2);
    }

    #[test]
    fn line_len_with_block() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted line")];
        let snapshot = create_block_snapshot("hello\nhi", &blocks);
        assert_eq!(snapshot.line_len(0), 5);
        assert_eq!(snapshot.line_len(1), 12);
        assert_eq!(snapshot.line_len(2), 2);
    }

    #[test]
    fn max_point_no_blocks() {
        let snapshot = create_block_snapshot("hello\nhi", &[]);
        assert_eq!(snapshot.max_point(), BlockPoint::new(1, 2));
    }

    #[test]
    fn clip_point_clamps_column() {
        let snapshot = create_block_snapshot("hello\nhi", &[]);
        assert_eq!(
            snapshot.clip_point(BlockPoint::new(0, 100), Bias::Left),
            BlockPoint::new(0, 5)
        );
        assert_eq!(
            snapshot.clip_point(BlockPoint::new(5, 0), Bias::Left),
            BlockPoint::new(1, 0)
        );
    }

    #[test]
    fn clip_point_snaps_off_block_row() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let snapshot = create_block_snapshot("hello\nworld", &blocks);
        let clipped_left = snapshot.clip_point(BlockPoint::new(1, 0), Bias::Left);
        assert_eq!(clipped_left, BlockPoint::new(0, 5));

        let clipped_right = snapshot.clip_point(BlockPoint::new(1, 0), Bias::Right);
        assert_eq!(clipped_right, BlockPoint::new(2, 0));
    }

    #[test]
    fn block_to_buffer_reverses_tabs() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "\thello");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).unwrap());
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (_, wrap_snapshot) = WrapMap::new(tab_snapshot, None, test_executor());
        let mut block_map = BlockMap::new();
        let snapshot = block_map.sync(wrap_snapshot, &Patch::empty(), None);

        let buf = snapshot.block_to_buffer(BlockPoint::new(0, 5)).unwrap();
        assert_eq!(buf, Point::new(0, 2));
    }

    #[test]
    fn block_line_len_matches_get_line() {
        let props = text_block(BlockPlacement::Below(0), "short\nlonger line\nx");
        let mut block_map = BlockMap::new();
        block_map.insert(vec![props]);
        let block = super::Block::Custom(block_map.custom_blocks[0].clone());
        for i in 0..block.height() {
            assert_eq!(
                block.line_len(i),
                block.get_line(i).len() as u32,
                "mismatch at line {i}"
            );
        }
    }

    #[test]
    fn from_text_and_from_lines_fn_match() {
        let text_props = BlockProperties::from_text(
            BlockPlacement::Below(0),
            "first\nsecond line\nthird"
                .lines()
                .map(String::from)
                .collect(),
            BlockStyle::Fixed,
        );
        let lines_props = BlockProperties::from_lines_fn(
            BlockPlacement::Below(0),
            3,
            Arc::new(|i| ["first", "second line", "third"][i as usize].to_string()),
            BlockStyle::Fixed,
        );

        assert_eq!(text_props.height, lines_props.height);
        let height = text_props.height.unwrap_or(0);
        let text_ctx = super::BlockContext {
            block_id: super::BlockId::Custom(super::CustomBlockId(0)),
            max_width: 80,
            height,
            selected: false,
            anchor_row: 0,
            diff_status: None,
            buffer_snapshot: &super::MultiBufferSnapshot::empty(),
        };
        let lines_ctx = super::BlockContext {
            block_id: super::BlockId::Custom(super::CustomBlockId(1)),
            max_width: 80,
            height,
            selected: false,
            anchor_row: 0,
            diff_status: None,
            buffer_snapshot: &super::MultiBufferSnapshot::empty(),
        };
        let text_lines = (text_props.render)(&text_ctx);
        let lines_lines = (lines_props.render)(&lines_ctx);
        for i in 0..height as usize {
            assert_eq!(
                text_lines[i].to_string(),
                lines_lines[i].to_string(),
                "get_line mismatch at {i}"
            );
        }
    }

    #[test]
    fn write_display_line_matches_display_line() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted\nlines")];
        let snapshot = create_block_snapshot("hello\nworld\nfoo", &blocks);
        for row in 0..snapshot.total_lines() {
            let expected = snapshot.display_line(row);
            let mut buf = String::new();
            snapshot.write_display_line(&mut buf, row);
            assert_eq!(buf, expected, "mismatch at row {row}");
        }
    }

    fn create_wrap_snapshot(content: &str) -> Arc<super::WrapSnapshot> {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).unwrap());
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (_, wrap_snapshot) = WrapMap::new(tab_snapshot, None, test_executor());
        wrap_snapshot
    }

    #[test]
    fn cache_reused_when_nothing_changes() {
        let wrap_snapshot = create_wrap_snapshot("hello\nworld");
        let mut block_map = BlockMap::new();
        block_map.insert(vec![text_block(BlockPlacement::Below(0), "deleted")]);

        let snap1 = block_map.sync(Arc::clone(&wrap_snapshot), &Patch::empty(), None);
        let snap2 = block_map.sync(wrap_snapshot, &Patch::empty(), None);

        assert_eq!(snap1.total_lines(), snap2.total_lines());
        assert_eq!(snap1.longest_row(), snap2.longest_row());
    }

    #[test]
    fn cache_invalidated_on_block_change() {
        let wrap_snapshot = create_wrap_snapshot("hello\nworld");
        let mut block_map = BlockMap::new();
        let ids = block_map.insert(vec![text_block(BlockPlacement::Below(0), "deleted")]);

        let snap1 = block_map.sync(Arc::clone(&wrap_snapshot), &Patch::empty(), None);
        assert_eq!(snap1.total_lines(), 3);

        block_map.remove(&ids.into_iter().collect());
        block_map.insert(vec![text_block(
            BlockPlacement::Below(0),
            "deleted\nextra line",
        )]);

        let snap2 = block_map.sync(wrap_snapshot, &Patch::empty(), None);
        assert_eq!(snap2.total_lines(), 4);
    }

    #[test]
    fn replace_single_row() {
        let blocks = vec![text_block(
            BlockPlacement::Replace { start: 1, end: 1 },
            "replacement",
        )];
        let snapshot = create_block_snapshot("line0\nline1\nline2", &blocks);

        assert_eq!(snapshot.total_lines(), 3);

        match snapshot.classify_row(0) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 0),
            _ => panic!("expected buffer row"),
        }
        match snapshot.classify_row(1) {
            BlockRowKind::Block { block, line_index } => {
                assert_eq!(line_index, 0);
                assert_eq!(block.get_line(0), "replacement");
            },
            _ => panic!("expected block"),
        }
        match snapshot.classify_row(2) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 2),
            _ => panic!("expected buffer row"),
        }

        assert!(snapshot.block_to_buffer(BlockPoint::new(1, 0)).is_none());
        assert_eq!(
            snapshot.block_to_buffer(BlockPoint::new(0, 0)),
            Some(Point::new(0, 0))
        );
        assert_eq!(
            snapshot.block_to_buffer(BlockPoint::new(2, 0)),
            Some(Point::new(2, 0))
        );
    }

    #[test]
    fn replace_multi_row() {
        let blocks = vec![text_block(
            BlockPlacement::Replace { start: 1, end: 3 },
            "rep0\nrep1",
        )];
        let snapshot = create_block_snapshot("r0\nr1\nr2\nr3\nr4", &blocks);

        assert_eq!(snapshot.total_lines(), 4);

        match snapshot.classify_row(0) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 0),
            _ => panic!("expected buffer row"),
        }
        match snapshot.classify_row(1) {
            BlockRowKind::Block { block, line_index } => {
                assert_eq!(line_index, 0);
                assert_eq!(block.get_line(0), "rep0");
            },
            _ => panic!("expected block at row 1"),
        }
        match snapshot.classify_row(2) {
            BlockRowKind::Block { block, line_index } => {
                assert_eq!(line_index, 1);
                assert_eq!(block.get_line(1), "rep1");
            },
            _ => panic!("expected block at row 2"),
        }
        match snapshot.classify_row(3) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 4),
            _ => panic!("expected buffer row"),
        }
    }

    #[test]
    fn near_placement() {
        let blocks = vec![text_block(BlockPlacement::Near(0), "near-block")];
        let snapshot = create_block_snapshot("line0\nline1", &blocks);

        assert_eq!(snapshot.total_lines(), 3);

        match snapshot.classify_row(0) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 0),
            _ => panic!("expected buffer row"),
        }
        match snapshot.classify_row(1) {
            BlockRowKind::Block { block, .. } => {
                assert_eq!(block.get_line(0), "near-block");
            },
            _ => panic!("expected block"),
        }
        match snapshot.classify_row(2) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 1),
            _ => panic!("expected buffer row"),
        }
    }

    #[test]
    fn mixed_placements() {
        let blocks = vec![
            text_block(BlockPlacement::Above(1), "above"),
            text_block(BlockPlacement::Below(1), "below"),
            text_block(BlockPlacement::Replace { start: 3, end: 3 }, "replaced"),
        ];
        let snapshot = create_block_snapshot("r0\nr1\nr2\nr3\nr4", &blocks);

        assert_eq!(snapshot.total_lines(), 7);

        let classifications: Vec<_> = (0..7)
            .map(|row| match snapshot.classify_row(row) {
                BlockRowKind::BufferRow { buffer_row } => format!("buf{}", buffer_row),
                BlockRowKind::Block { block, .. } => format!("blk:{}", block.get_line(0)),
            })
            .collect();

        assert_eq!(
            classifications,
            vec![
                "buf0",
                "blk:above",
                "buf1",
                "blk:below",
                "buf2",
                "blk:replaced",
                "buf4"
            ]
        );
    }

    #[test]
    fn replace_at_beginning() {
        let blocks = vec![text_block(
            BlockPlacement::Replace { start: 0, end: 0 },
            "new-first",
        )];
        let snapshot = create_block_snapshot("old-first\nline1", &blocks);

        assert_eq!(snapshot.total_lines(), 2);
        match snapshot.classify_row(0) {
            BlockRowKind::Block { block, .. } => assert_eq!(block.get_line(0), "new-first"),
            _ => panic!("expected block"),
        }
        match snapshot.classify_row(1) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 1),
            _ => panic!("expected buffer row"),
        }
    }

    #[test]
    fn replace_at_end() {
        let blocks = vec![text_block(
            BlockPlacement::Replace { start: 2, end: 2 },
            "new-last",
        )];
        let snapshot = create_block_snapshot("line0\nline1\nold-last", &blocks);

        assert_eq!(snapshot.total_lines(), 3);
        match snapshot.classify_row(2) {
            BlockRowKind::Block { block, .. } => assert_eq!(block.get_line(0), "new-last"),
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn insert_and_remove_blocks() {
        let wrap_snapshot = create_wrap_snapshot("hello\nworld\nfoo");
        let mut block_map = BlockMap::new();

        let ids = block_map.insert(vec![
            text_block(BlockPlacement::Below(0), "blk1"),
            text_block(BlockPlacement::Below(1), "blk2"),
        ]);
        assert_eq!(ids.len(), 2);

        let snap = block_map.sync(Arc::clone(&wrap_snapshot), &Patch::empty(), None);
        assert_eq!(snap.total_lines(), 5);

        block_map.remove(&[ids[0]].into_iter().collect());
        let snap = block_map.sync(wrap_snapshot, &Patch::empty(), None);
        assert_eq!(snap.total_lines(), 4);
    }
}
