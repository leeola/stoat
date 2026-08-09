use super::{
    fold_map::FoldPointCursor,
    highlights::Chunk,
    inlay_map::InlayPointCursor,
    wrap_map::{WrapPoint, WrapPointCursor, WrapSnapshot},
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
    ops::{Deref, Range},
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Arc, LazyLock, OnceLock,
    },
};
use stoat_text::{
    patch::Patch, tree_map::TreeMap, Bias, ContextLessSummary, Cursor, Dimension, Dimensions, Item,
    Point, SeekTarget, SumTree,
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

/// A block's rendered lines together with the byte length of each.
///
/// Measuring a line means writing it out, and the callers that only want the
/// width would otherwise build and drop a `String` per line on every call. The
/// lengths are taken here, while the render is already in hand, and kept beside
/// the lines they describe so the two cannot come to disagree.
struct RenderedBlock {
    lines: Vec<Line<'static>>,
    line_lens: Vec<u32>,
}

impl RenderedBlock {
    fn new(lines: Vec<Line<'static>>) -> Self {
        let line_lens = lines
            .iter()
            .map(|line| line.to_string().len() as u32)
            .collect();
        Self { lines, line_lens }
    }
}

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
    /// Memoized default-context render, filled on first line access.
    ///
    /// The per-row line accessors render with the constant
    /// [`Block::default_ctx`], and this block's closure is pure over the data it
    /// captured at construction (the diff and text blocks ignore the context),
    /// so the render is invariant and cached once instead of re-run per line.
    rendered: OnceLock<Arc<RenderedBlock>>,
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
        static EMPTY_SNAPSHOT: LazyLock<MultiBufferSnapshot> =
            LazyLock::new(MultiBufferSnapshot::empty);
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

    /// Rendered content for this block, memoized on custom blocks.
    ///
    /// Custom blocks render with the constant [`Self::default_ctx`], so the
    /// output never varies between calls. The diff and text block closures these
    /// serve are pure over the lines captured at construction and ignore the
    /// block context, so caching the first render is exact. Per-row callers
    /// (`get_line`, `line_len`, `write_line`) then reuse one render instead of
    /// re-running the closure for every line. Non-custom blocks carry no
    /// rendered content here.
    fn rendered_memo(&self) -> Arc<RenderedBlock> {
        static EMPTY: LazyLock<Arc<RenderedBlock>> =
            LazyLock::new(|| Arc::new(RenderedBlock::new(Vec::new())));
        match self {
            Block::Custom(b) => b
                .rendered
                .get_or_init(|| Arc::new(RenderedBlock::new((b.render)(&self.default_ctx()))))
                .clone(),
            _ => EMPTY.clone(),
        }
    }

    pub fn get_line(&self, index: u32) -> String {
        match self {
            Block::Custom(_) => self
                .rendered_memo()
                .lines
                .get(index as usize)
                .map(|l| l.to_string())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    pub fn line_len(&self, index: u32) -> u32 {
        self.rendered_memo()
            .line_lens
            .get(index as usize)
            .copied()
            .unwrap_or(0)
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
}

impl ContextLessSummary for TransformSummary {
    fn add_summary(&mut self, other: &Self) {
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

use super::{
    highlights::{HighlightCursor, HighlightEndpoint},
    wrap_map::WrapChunks,
};

/// Iterator over a range of block rows, emitting [`Chunk`]s that propagate
/// highlight styles from the wrap layer below.
///
/// Walks the block transform tree, emitting one unstyled chunk per line of a
/// block transform and forwarding [`WrapSnapshot::chunks`] for everything else.
///
/// A run of rows carrying no block is forwarded as one `WrapChunks`, which emits
/// the newlines between them itself. Only the boundaries a block sits on are
/// stepped here, so the layers below are opened once per run rather than once
/// per row.
pub struct BlockChunks<'a> {
    snapshot: &'a BlockSnapshot,
    endpoints: Arc<[HighlightEndpoint]>,
    /// Advanced in step with the rows, since they are visited in order. A fresh
    /// seek per row would re-descend the tree for a position it already holds.
    cursor: Cursor<'a, 'static, Transform, Dimensions<InputRow, OutputRow>>,
    current_row: u32,
    end_row: u32,
    pending_wrap_chunks: Option<WrapChunks<'a>>,
    pending_newline: bool,
    /// Taken by the first wrap range opened, which is the only one starting
    /// where the caller's replay left off.
    seed: Option<HighlightCursor>,
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
                // The row already moved to the end of the run when it opened.
                self.pending_wrap_chunks = None;
                if self.current_row < self.end_row {
                    self.pending_newline = true;
                }
                continue;
            }

            if self.current_row >= self.end_row {
                return None;
            }

            // Classify the current row. Rows are visited in order, so the cursor
            // moves forward from where it already stands.
            let target = OutputRow(self.current_row + 1);
            if self.cursor.did_seek() {
                self.cursor.seek_forward(&target, Bias::Left);
            } else {
                self.cursor.seek(&target, Bias::Left);
            }
            let Dimensions(input_start, output_start, _) = *self.cursor.start();
            let rows_into_transform = self.current_row - output_start.0;

            if let Some(transform) = self.cursor.item()
                && let Some(ref block) = transform.block
            {
                let mut line = String::new();
                block.write_line(&mut line, rows_into_transform);
                self.current_row += 1;
                if self.current_row < self.end_row {
                    self.pending_newline = true;
                }
                return Some(Chunk {
                    text: std::borrow::Cow::Owned(line),
                    ..Default::default()
                });
            }

            // Everything up to the next block belongs to one wrap-row range, and
            // the wrap layer emits the newlines within it.
            let wrap_start = input_start.0 + rows_into_transform;
            let run_end = self.run_end(self.current_row);
            let wrap_end = wrap_start + (run_end - self.current_row);

            self.current_row = run_end;
            self.pending_wrap_chunks = Some(self.snapshot.wrap_snapshot.chunks_seeded(
                wrap_start..wrap_end,
                self.endpoints.clone(),
                self.seed.take().as_ref(),
            ));
        }
    }
}

impl BlockChunks<'_> {
    /// Output row where the block-free run containing `row` ends.
    ///
    /// Walks its own cursor over the transforms rather than the rows, so a long
    /// run costs one step per transform. The scan cannot borrow the iterator's
    /// cursor, which has to stay on the row being emitted.
    fn run_end(&self, row: u32) -> u32 {
        let mut scan = self
            .snapshot
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        scan.seek(&OutputRow(row + 1), Bias::Left);

        let mut end = scan.start().1 .0 + scan.item().map_or(0, |t| t.summary.output_rows);
        while end < self.end_row {
            scan.next();
            match scan.item() {
                Some(transform) if transform.block.is_none() => {
                    end += transform.summary.output_rows;
                },
                _ => break,
            }
        }
        end.min(self.end_row)
    }
}

pub struct BlockMap {
    next_block_id: AtomicUsize,
    next_spacer_id: AtomicUsize,
    /// Every custom block, ordered by start row, at the placement the edits
    /// have carried it to.
    ///
    /// This is the authoritative position. A block reached through the
    /// transform tree is the [`Arc`] that was cloned in when the block was
    /// inserted, and carrying replaces the entry here without rewriting the
    /// tree's copies, so a placement read back off a snapshot names the row
    /// the block went in at rather than the row it now occupies. The sync
    /// resolves against [`Self::custom_blocks_by_id`] wherever it needs the
    /// current one.
    pub(super) custom_blocks: Vec<Arc<CustomBlock>>,
    custom_blocks_by_id: TreeMap<CustomBlockId, Arc<CustomBlock>>,
    transforms: Option<SumTree<Transform>>,
    total_rows: u32,
    blocks_dirty: bool,
    /// Placements whose block changed since the last sync.
    ///
    /// Adding, removing, or moving a block only changes the transforms where
    /// that block sits, so `sync` turns these into identity wrap-row edits and
    /// patches those rows rather than rebuilding the file. Whole placements are
    /// kept rather than bare rows because a `Below` block anchors a row past
    /// the one it names, and the wrap snapshot that settles which rows those
    /// are does not exist until `sync`.
    touched_placements: Vec<BlockPlacement>,
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
            touched_placements: Vec::new(),
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
        if blocks.is_empty() {
            return Vec::new();
        }

        let mut ids = Vec::with_capacity(blocks.len());
        let mut added: Vec<Arc<CustomBlock>> = Vec::with_capacity(blocks.len());
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
                rendered: OnceLock::new(),
            });
            self.touched_placements.push(block.placement);
            self.custom_blocks_by_id.insert(id, block.clone());
            added.push(block);
            ids.push(id);
        }

        // Merged in one pass. Placing each block with its own `Vec::insert`
        // reshuffles the tail once per block, which a diff view splicing every
        // hunk at once pays quadratically.
        added.sort_by_key(|b| b.placement.start_row());
        let merged = merge_by_start_row(std::mem::take(&mut self.custom_blocks), added);
        self.custom_blocks = merged;

        ids
    }

    pub fn remove(&mut self, ids: &HashSet<CustomBlockId>) {
        if ids.is_empty() {
            return;
        }
        for block in self.custom_blocks.iter().filter(|b| ids.contains(&b.id)) {
            self.touched_placements.push(block.placement);
        }
        self.custom_blocks.retain(|b| !ids.contains(&b.id));
        for id in ids {
            self.custom_blocks_by_id.remove(id);
        }
    }

    /// Move every custom block's placement across `buffer_row_edits`, so a
    /// block goes on marking the row it was attached to.
    ///
    /// A placement is a plain buffer row. Without this an edit above a block
    /// leaves it pointing at whatever text slid into that row, and only the
    /// block's owner removing and re-inserting it can put it back. That
    /// re-splice is what the diff view currently pays on every refresh.
    ///
    /// A block whose rows were replaced outright has nothing left to point at,
    /// so it collapses to the start of the replacement.
    fn carry_block_placements(&mut self, buffer_row_edits: &Patch<u32>) {
        if buffer_row_edits.is_empty() || self.custom_blocks.is_empty() {
            return;
        }

        // Blocks are ordered by start row, but a Replace block's end can sit
        // past the next block's start, so the rows are visited in sorted order
        // rather than read straight off the list.
        let mut rows: Vec<u32> = self
            .custom_blocks
            .iter()
            .flat_map(|b| [b.placement.start(), b.placement.end()])
            .collect();
        let mut order: Vec<usize> = (0..rows.len()).collect();
        order.sort_unstable_by_key(|&i| rows[i]);

        let mut ascending: Vec<u32> = order.iter().map(|&i| rows[i]).collect();
        let mut ascending_touch = vec![false; ascending.len()];
        carry_rows(&mut ascending, &mut ascending_touch, buffer_row_edits);

        let mut needs_touch = vec![false; rows.len()];
        for (slot, &i) in order.iter().enumerate() {
            rows[i] = ascending[slot];
            needs_touch[i] = ascending_touch[slot];
        }

        for (i, block) in self.custom_blocks.iter_mut().enumerate() {
            let (start, end) = (rows[i * 2], rows[i * 2 + 1].max(rows[i * 2]));
            let placement = match block.placement {
                BlockPlacement::Above(_) => BlockPlacement::Above(start),
                BlockPlacement::Below(_) => BlockPlacement::Below(start),
                BlockPlacement::Near(_) => BlockPlacement::Near(start),
                BlockPlacement::Replace { .. } => BlockPlacement::Replace { start, end },
            };
            if placement == block.placement {
                continue;
            }

            // A block that only shifted is carried by the transform trees'
            // own prefix and suffix relocation, so it needs no rebuild region.
            // Only a collapsed endpoint changed position relative to the text.
            if needs_touch[i * 2] || needs_touch[i * 2 + 1] {
                self.touched_placements.push(placement);
            }

            let moved = Arc::new(CustomBlock {
                id: block.id,
                placement,
                height: block.height,
                render: block.render.clone(),
                diff_status: block.diff_status,
                style: block.style,
                priority: block.priority,
                // The closures behind this memo are pure over what they captured
                // at construction, so moving the block does not stale it.
                rendered: block.rendered.clone(),
            });
            *block = Arc::clone(&moved);
            self.custom_blocks_by_id.insert(moved.id, moved);
        }

        debug_assert!(
            self.custom_blocks
                .windows(2)
                .all(|pair| pair[0].placement.start_row() <= pair[1].placement.start_row()),
            "carrying rows is monotone, so the stored order must survive it",
        );
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

    /// `buffer_row_edits` restates the same change as `wrap_edits` in buffer
    /// rows, which is the space custom block placements live in.
    pub fn sync(
        &mut self,
        wrap_snapshot: Arc<WrapSnapshot>,
        wrap_edits: &Patch<u32>,
        buffer_row_edits: &Patch<u32>,
        companion_view: Option<CompanionView<'_>>,
    ) -> BlockSnapshot {
        self.carry_block_placements(buffer_row_edits);

        let mut edits = if self.deferred_edits.is_empty() {
            wrap_edits.clone()
        } else {
            let deferred = std::mem::replace(&mut self.deferred_edits, Patch::empty());
            deferred.compose(wrap_edits.edits().iter().cloned())
        };

        // Pull in companion edits: when the companion changes, we need to
        // recompute spacer blocks in the affected region.
        if let Some(ref cv) = companion_view
            && !cv.companion_wrap_edits.is_empty()
        {
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
                let our_wrap_start = buffer_row_to_wrap_row(our_range.start.row, &wrap_snapshot);
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

        // Rows whose block set changed, marked for rebuild the same way. These
        // placements name the post-edit buffer, so they compose onto the wrap
        // patch rather than into it.
        if !self.touched_placements.is_empty() {
            let mut ranges: Vec<Range<u32>> = std::mem::take(&mut self.touched_placements)
                .iter()
                .map(|placement| {
                    // Widened by a row on each side. A block transform consumes
                    // no input rows, so one whose row merely bounds the edit
                    // gets kept by the unchanged prefix instead of rebuilt, and
                    // the stale block survives beside its replacement. The end
                    // stays inside the wrap text, which the sync walks by
                    // seeking and would otherwise run off.
                    let rows = placement_wrap_rows(placement, &wrap_snapshot);
                    let line_count = wrap_snapshot.line_count();
                    rows.start.min(line_count).saturating_sub(1)..(rows.end + 1).min(line_count)
                })
                .filter(|rows| rows.start < rows.end)
                .collect();
            ranges.sort_unstable_by_key(|r| r.start);
            ranges.dedup();

            let mut touched = Patch::empty();
            for range in ranges {
                touched.push(stoat_text::patch::Edit {
                    old: range.clone(),
                    new: range,
                });
            }
            edits = edits.compose(touched.into_inner());
        }

        if edits.is_empty()
            && !self.blocks_dirty
            && let Some(ref transforms) = self.transforms
        {
            return BlockSnapshot {
                wrap_snapshot,
                transforms: transforms.clone(),
                total_rows: self.total_rows,
            };
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
                &self.custom_blocks_by_id,
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

            if delta > 0
                && let Some(last_edit) = patch.patch.edits().last()
            {
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
    /// Display position showing buffer `point`.
    ///
    /// A row a block replaces is not shown, so it has no display row of its
    /// own. Those rows answer the row the replacement starts at, at column 0,
    /// since that row's columns belong to the replacement's text rather than to
    /// the buffer.
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
        self.wrap_to_block(wrap_point)
    }

    /// Block position of `wrap_point`, the last leg of what
    /// [`Self::buffer_to_block`] walks.
    ///
    /// Separate so a caller that already holds a wrap position can join the
    /// chain here. The legs before it expand tabs by walking the row from its
    /// start, which a caller stepping along that row has already done.
    pub fn wrap_to_block(&self, wrap_point: WrapPoint) -> BlockPoint {
        let wrap_row = wrap_point.row();

        let target = InputRow(wrap_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input, output, _) = cursor.start();

        if let Some(transform) = cursor.item()
            && transform.block.is_some()
        {
            return BlockPoint::new(output.0, 0);
        }

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

        if let Some(transform) = cursor.item()
            && transform.block.is_some()
        {
            return None;
        }

        let wrap_row = input_start.0 + rows_into_transform;
        let wrap_point = WrapPoint::new(wrap_row, point.column);
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

        if let Some(transform) = cursor.item()
            && let Some(ref block) = transform.block
        {
            return BlockRowKind::Block {
                block,
                line_index: rows_into_transform,
            };
        }

        let wrap_row = input_start.0 + rows_into_transform;
        let tab_point = self.wrap_snapshot.to_tab_point(WrapPoint::new(wrap_row, 0));
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

    /// Input (buffer) rows above display row `display_row`.
    ///
    /// Counts the input rows the transforms consume before `display_row`. A
    /// block transform consumes no input, so a display row inside a block
    /// returns the input rows before the block. With no soft-wrap or folds
    /// active (the diff view's case) input rows equal buffer rows. Mirrors
    /// [`Self::classify_row`]'s seek.
    pub fn buffer_rows_above(&self, display_row: u32) -> u32 {
        let target = OutputRow(display_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = display_row.saturating_sub(output_start.0);

        match cursor.item() {
            Some(transform) if transform.block.is_some() => input_start.0,
            _ => input_start.0 + rows_into_transform,
        }
    }

    /// Wrap row that block row `block_row` shows.
    ///
    /// Meaningful only for a row [`Self::classify_row`] reports as
    /// [`BlockRowKind::BufferRow`]. A block's rows consume no input, so one of
    /// those answers the wrap row its transform begins at, which belongs to the
    /// following buffer row instead.
    fn wrap_row_at(&self, block_row: u32) -> u32 {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        input_start.0 + block_row.saturating_sub(output_start.0)
    }

    /// Move `point` to the nearest position a caret can occupy, preferring the
    /// side `bias` names.
    ///
    /// A block's own rows hold no buffer text, so a point on one moves off the
    /// block entirely. Every other point clips in wrap space, which clears the
    /// soft-wrap indent margin, tab expansions, fold placeholders, and inlays
    /// in turn.
    ///
    /// Wrap-space clipping can answer on a different row than it was asked
    /// about, because a fold placeholder straddling a wrap break clips to the
    /// whole placeholder. The result is read back through the wrap-to-block
    /// mapping rather than pinned to the row the point started on, so it names
    /// the position the wrap layer chose.
    ///
    /// A block at a document edge has buffer text on one side only, so the
    /// direction `bias` names can run out. The search then turns around, since
    /// the far side of the block is the only text left to land on.
    pub fn clip_point(&self, point: BlockPoint, bias: Bias) -> BlockPoint {
        let row = point.row.min(self.total_rows.saturating_sub(1));
        match self.classify_row(row) {
            BlockRowKind::BufferRow { .. } => self.clip_on_buffer_row(row, point.column, bias),
            BlockRowKind::Block { .. } => self.off_block_row(row, bias),
        }
    }

    /// Clip `column` on `row`, which must hold buffer text.
    ///
    /// The whole clip happens in wrap space, which clears the soft-wrap indent
    /// margin, tab expansions, fold placeholders and inlays in turn. The result
    /// is read back through the wrap-to-block mapping rather than pinned to
    /// `row`, since wrap-space clipping does not preserve the row.
    fn clip_on_buffer_row(&self, row: u32, column: u32, bias: Bias) -> BlockPoint {
        let clipped = self.wrap_clip(row, column, bias);
        match self.classify_row(clipped.row) {
            BlockRowKind::BufferRow { .. } => clipped,
            // The wrap layer carried the position onto a row a `Replace` block
            // hides. The search resumes in whichever direction it was carried
            // rather than the one `bias` names, so the walk only ever moves
            // away from the row it started on and cannot come back to it.
            BlockRowKind::Block { .. } => {
                let onward = if clipped.row > row {
                    Bias::Right
                } else {
                    Bias::Left
                };
                self.off_block_row(clipped.row, onward)
            },
        }
    }

    /// Clip `column` on `row` through the wrap layer, without checking whether
    /// the row it lands on holds buffer text.
    ///
    /// Separate from [`Self::clip_on_buffer_row`] so the searches that feed it
    /// candidate positions cannot re-enter that check and cycle between two
    /// rows that each carry onto the other.
    fn wrap_clip(&self, row: u32, column: u32, bias: Bias) -> BlockPoint {
        let wrap_point = self
            .wrap_snapshot
            .clip_point(WrapPoint::new(self.wrap_row_at(row), column), bias);
        self.wrap_to_block(wrap_point)
    }

    /// Nearest position holding buffer text to block row `row`, preferring the
    /// side `bias` names.
    fn off_block_row(&self, row: u32, bias: Bias) -> BlockPoint {
        let found = if bias == Bias::Left {
            self.scan_up(row).or_else(|| self.scan_down(row))
        } else {
            self.scan_down(row).or_else(|| self.scan_up(row))
        };

        // A `Replace` block spanning every wrap row leaves the tree holding no
        // buffer text at all, so neither scan can answer. The block's own row
        // keeps clipping total and idempotent, which is the most a document
        // with nowhere to put a caret allows.
        found.unwrap_or_else(|| BlockPoint::new(row, 0))
    }

    /// Last position holding buffer text above block row `row`.
    ///
    /// A candidate row can hold no position the scan will take, so it carries on
    /// upward. Only moving further from `row` each step is what ends the walk,
    /// since a candidate that turned around could carry back onto the one
    /// before it.
    fn scan_up(&self, row: u32) -> Option<BlockPoint> {
        let mut candidate = row;
        loop {
            candidate = candidate.checked_sub(1)?;
            if !matches!(self.classify_row(candidate), BlockRowKind::BufferRow { .. }) {
                continue;
            }

            let end = self.line_len(candidate);
            let found = self.probe(
                candidate,
                [(end, Bias::Left), (end.saturating_sub(1), Bias::Left)],
            );
            if found.is_some() {
                return found;
            }
        }
    }

    /// First position holding buffer text below block row `row`.
    fn scan_down(&self, row: u32) -> Option<BlockPoint> {
        let mut candidate = row;
        loop {
            candidate += 1;
            if candidate >= self.total_rows {
                return None;
            }
            if !matches!(self.classify_row(candidate), BlockRowKind::BufferRow { .. }) {
                continue;
            }

            let found = self.probe(candidate, [(0, Bias::Right), (0, Bias::Left)]);
            if found.is_some() {
                return found;
            }
        }
    }

    /// A position on `row` a caret can occupy, clipping at `probes` in order
    /// and taking the first that lands on a row holding buffer text.
    ///
    /// The first probe is the end of the row the scan wants. It needs a second
    /// because a row's own end can belong to the row after it: under soft wrap
    /// the end of a wrapped row and the start of its continuation are one
    /// position, and the wrap layer answers with the continuation. A `Replace`
    /// block hiding that continuation leaves the end naming nothing, and the
    /// probe held one cell inside the row stands in for it.
    fn probe(&self, row: u32, probes: [(u32, Bias); 2]) -> Option<BlockPoint> {
        probes.into_iter().find_map(|(column, bias)| {
            let clipped = self.wrap_clip(row, column, bias);
            matches!(
                self.classify_row(clipped.row),
                BlockRowKind::BufferRow { .. }
            )
            .then_some(clipped)
        })
    }

    pub fn line_len(&self, block_row: u32) -> u32 {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = block_row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item()
            && let Some(ref block) = transform.block
        {
            return block.line_len(rows_into_transform);
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

        if let Some(transform) = cursor.item()
            && let Some(ref block) = transform.block
        {
            block.write_line(buf, rows_into_transform);
            return;
        }

        let wrap_row = input_start.0 + rows_into_transform;
        self.wrap_snapshot.write_display_line(buf, wrap_row);
    }

    pub fn display_line(&self, block_row: u32) -> String {
        let mut result = String::new();
        self.write_display_line(&mut result, block_row);
        result
    }

    pub fn chunks(&self, rows: Range<u32>, endpoints: Arc<[HighlightEndpoint]>) -> BlockChunks<'_> {
        self.chunks_seeded(rows, endpoints, None)
    }

    /// Like [`Self::chunks`], resuming an endpoint replay from `seed`.
    ///
    /// The seed is offered to the first wrap range this opens and to no other,
    /// since only the first begins where the caller's replay stopped. A stream
    /// covering several rows therefore gains nothing from it, and one covering a
    /// single row gains everything, which is the shape of a caller painting row
    /// by row.
    pub fn chunks_seeded(
        &self,
        rows: Range<u32>,
        endpoints: Arc<[HighlightEndpoint]>,
        seed: Option<&HighlightCursor>,
    ) -> BlockChunks<'_> {
        BlockChunks {
            snapshot: self,
            endpoints,
            cursor: self
                .transforms
                .cursor::<Dimensions<InputRow, OutputRow>>(()),
            current_row: rows.start,
            end_row: rows.end,
            pending_wrap_chunks: None,
            pending_newline: false,
            seed: seed.cloned(),
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
    pub fn row_range_to_buffer_byte_range(&self, rows: Range<u32>) -> Range<usize> {
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

        if let Some(transform) = cursor.item()
            && transform.block.is_some()
        {
            return 0;
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

        if let Some(transform) = cursor.item()
            && transform.block.is_some()
        {
            return false;
        }

        let wrap_row = input_start.0 + rows_into_transform;
        self.wrap_snapshot.classify_row(wrap_row) == super::wrap_map::WrapRowKind::Continuation
    }
}

fn sort_and_dedup_blocks(blocks: &mut Vec<(ResolvedPlacement, &Block)>) {
    blocks.sort_unstable_by(|(a, block_a), (b, block_b)| {
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
            // Everything above can tie, and `sort_unstable_by` is free to order
            // ties either way, so the same set of blocks could render in a
            // different order on any rebuild. What is left to separate two
            // blocks is which block each one is.
            .then_with(|| kind_rank(block_a).cmp(&kind_rank(block_b)))
            .then_with(|| match (block_a, block_b) {
                // Ids are minted in insertion order, so blocks left equal by
                // priority render in the order they were added.
                (Block::Custom(a), Block::Custom(b)) => {
                    Ord::cmp(&a.priority, &b.priority).then_with(|| Ord::cmp(&a.id, &b.id))
                },
                (Block::Spacer { id: a, .. }, Block::Spacer { id: b, .. }) => Ord::cmp(a, b),
                (
                    Block::ExcerptBoundary { excerpt: a, .. }
                    | Block::BufferHeader { excerpt: a, .. }
                    | Block::FoldedBuffer {
                        first_excerpt: a, ..
                    },
                    Block::ExcerptBoundary { excerpt: b, .. }
                    | Block::BufferHeader { excerpt: b, .. }
                    | Block::FoldedBuffer {
                        first_excerpt: b, ..
                    },
                ) => Ord::cmp(&a.id, &b.id),
                // Blocks of different kinds were already separated by rank.
                _ => Ordering::Equal,
            })
    });

    blocks.dedup_by(|right, left| match (&mut left.0, &right.0) {
        // Near is left out. It attaches beside a row rather than occupying one,
        // and a replaced range stands for the rows it replaces.
        (
            ResolvedPlacement::Replace {
                start: left_start,
                end: left_end,
            },
            ResolvedPlacement::Above(row) | ResolvedPlacement::Below(row),
        ) => *row >= *left_start && *row <= *left_end,
        (
            ResolvedPlacement::Replace { end: left_end, .. },
            ResolvedPlacement::Replace {
                start: right_start,
                end: right_end,
            },
        ) if *right_start <= *left_end => {
            *left_end = (*left_end).max(*right_end);
            true
        },
        _ => false,
    });
}

/// Which kind of block goes first when two land on the same row with the same
/// placement.
///
/// Structural blocks rank ahead of decorative ones, so a header stays at the top
/// of its excerpt whatever else was spliced onto the row.
fn kind_rank(block: &Block) -> u8 {
    match block {
        Block::FoldedBuffer { .. } => 0,
        Block::BufferHeader { .. } => 1,
        Block::ExcerptBoundary { .. } => 2,
        Block::Spacer { .. } => 3,
        Block::Custom(_) => 4,
    }
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

/// The wrap rows a placement's block occupies, matching where
/// [`resolve_block_placement`] puts it.
///
/// A `Below` or `Near` block anchors the row after the one it names, so an edit
/// marking the named row alone would leave the block out of the region it
/// rebuilds and the block would be dropped rather than replaced.
fn placement_wrap_rows(placement: &BlockPlacement, wrap_snapshot: &WrapSnapshot) -> Range<u32> {
    match placement {
        BlockPlacement::Above(row) => {
            let wrap_row = buffer_row_to_wrap_row(*row, wrap_snapshot);
            wrap_row..wrap_row + 1
        },
        BlockPlacement::Below(row) | BlockPlacement::Near(row) => {
            let wrap_row = buffer_row_to_wrap_row(*row, wrap_snapshot) + 1;
            wrap_row..wrap_row + 1
        },
        BlockPlacement::Replace { start, end } => {
            let start_wrap = buffer_row_to_wrap_row(*start, wrap_snapshot);
            let end_wrap = buffer_row_to_wrap_row(*end, wrap_snapshot);
            start_wrap..end_wrap.max(start_wrap) + 1
        },
    }
}

/// Merge two lists already ordered by placement start row into one.
///
/// Ties keep `existing` ahead of `added`, matching where the old per-block
/// `partition_point` placed a new block among equal start rows.
fn merge_by_start_row(
    existing: Vec<Arc<CustomBlock>>,
    added: Vec<Arc<CustomBlock>>,
) -> Vec<Arc<CustomBlock>> {
    let mut merged = Vec::with_capacity(existing.len() + added.len());
    let mut existing = existing.into_iter().peekable();
    let mut added = added.into_iter().peekable();

    loop {
        let take_added = match (existing.peek(), added.peek()) {
            (Some(a), Some(b)) => b.placement.start_row() < a.placement.start_row(),
            (None, Some(_)) => true,
            (_, None) => false,
        };
        let next = if take_added {
            added.next()
        } else {
            existing.next()
        };
        match next {
            Some(block) => merged.push(block),
            None => return merged,
        }
    }
}

/// Carry each row in `rows` across `edits`, collapsing a row inside a replaced
/// range onto the start of what replaced it.
///
/// One forward pass over both ascending sequences, rather than re-shifting the
/// whole trailing slice once per edit. A row ends up carrying the summed delta
/// of every edit that ends at or before it.
///
/// `rows` must be ascending, and stays ascending: rows outside an edit all move
/// by the same delta, and rows inside one collapse to a single value.
///
/// `needs_touch` is set for exactly the collapsed rows. A row that only shifted
/// keeps its position relative to the text around it, which the transform trees
/// below reproduce on their own by relocating whole prefixes and suffixes. A
/// collapsed row does not. Where it used to sit no longer exists, so whatever
/// caches its position has to be told.
fn carry_rows(rows: &mut [u32], needs_touch: &mut [bool], edits: &Patch<u32>) {
    let mut delta: i64 = 0;
    let mut i = 0;

    for edit in edits {
        while i < rows.len() && rows[i] < edit.old.start {
            rows[i] = (rows[i] as i64 + delta).max(0) as u32;
            i += 1;
        }
        while i < rows.len() && rows[i] < edit.old.end {
            rows[i] = edit.new.start;
            needs_touch[i] = true;
            i += 1;
        }
        // Assigned, not accumulated. A patch states new ranges in
        // post-all-edits coordinates, so this difference is the running total
        // already and adding them counts every earlier edit again.
        delta = edit.new.end as i64 - edit.old.end as i64;
    }

    for row in &mut rows[i..] {
        *row = (*row as i64 + delta).max(0) as u32;
    }
}

fn sync_incremental(
    old_transforms: &SumTree<Transform>,
    wrap_line_count: u32,
    blocks: &[Block],
    wrap_snapshot: &WrapSnapshot,
    wrap_edits: &Patch<u32>,
    custom_blocks_by_id: &TreeMap<CustomBlockId, Arc<CustomBlock>>,
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
        // The rebuild region can start earlier than the edit does, when the edit
        // lands inside a replacement. Both coordinates move together below.
        let old_start = edit.old.start;
        let mut new_start = edit.new.start;

        new_transforms.append(cursor.slice(&InputRow(old_start), Bias::Left), ());

        // Preserve transforms ending exactly at edit start (matching Zed lines 902-920)
        let mut kept_below_blocks_at_start = false;
        if let Some(item) = cursor.item() {
            let item_end = cursor.start().0 + item.summary.input_rows;
            if item.summary.input_rows > 0
                && item_end == old_start
                && !item.block.as_ref().is_some_and(|b| b.is_replacement())
            {
                new_transforms.push(item.clone(), ());
                cursor.next();

                while let Some(item) = cursor.item() {
                    if item.block.as_ref().is_some_and(|b| b.place_below()) {
                        new_transforms.push(item.clone(), ());
                        cursor.next();
                        kept_below_blocks_at_start = true;
                    } else {
                        break;
                    }
                }
            }
        }

        // The region has to begin on a transform boundary, and the edit may
        // start partway into one.
        if let Some(item) = cursor.item() {
            let transform_rows_before_edit = old_start - cursor.start().0;
            if transform_rows_before_edit > 0 {
                if item.block.is_none() {
                    push_isomorphic(&mut new_transforms, transform_rows_before_edit);
                } else {
                    // A block hiding rows cannot be cut in half, so an edit
                    // landing among them pulls the region back to where the
                    // replacement starts. Splitting it instead would leave the
                    // rows before the edit emitted as ordinary text, and the
                    // block itself outside the region that re-emits it.
                    debug_assert!(item.summary.input_rows > 0);
                    new_start -= transform_rows_before_edit;
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

        let edit_end = new_end.min(wrap_line_count);

        // Discard the zero-width block transforms at the region's end, whose
        // blocks are re-emitted from `blocks_in_range` below.
        //
        // A custom block the edit only shifted is the exception. The extension
        // loop above runs the region out to the leading isomorphic transform's
        // far boundary, and a shifted block lands exactly there, because the
        // region's new end takes the same extension the block's rows took. The
        // re-emission filter is strict (`block_start < edit_end`), so a block at
        // that boundary would be dropped and never put back. Keeping its
        // transform leaves it to the suffix, which relocates it by row
        // arithmetic. Blocks the edit collapsed, inserted, or removed resolve
        // inside the region and re-emit normally.
        while let Some(item) = cursor.item() {
            if item.summary.input_rows != 0 || item.block.is_none() {
                break;
            }

            if let Some(Block::Custom(stale)) = item.block.as_ref()
                && let Some(current) = custom_blocks_by_id.get(&stale.id)
            {
                let start = placement_wrap_rows(&current.placement, wrap_snapshot).start;
                if start >= edit_end {
                    debug_assert_eq!(
                        start, edit_end,
                        "a kept boundary block must land exactly at the region end",
                    );
                    break;
                }
            }

            cursor.next();
        }

        let current_rows: InputRow = new_transforms.extent(());
        if new_start > current_rows.0 {
            let gap = new_start - current_rows.0;
            push_isomorphic(&mut new_transforms, gap);
        }

        let edit_start_buf = wrap_row_to_buffer_row(new_start, wrap_snapshot);
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

        // The first block this region declines because it begins at or past the
        // region's end. Such a block has not been dealt with, it simply belongs
        // to a later region, so the scan for the next one must not start past
        // it. Blocks are ordered by row and the test is on their start, so
        // everything from here on is declined for the same reason.
        let mut deferred_block_idx = end_block_idx;

        blocks_in_range.clear();
        blocks_in_range.extend(
            blocks[start_block_idx..end_block_idx]
                .iter()
                .enumerate()
                .filter_map(|(offset, b)| {
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
                    // A below block sitting on the edit's first row was already
                    // carried over from the old tree above. The search deliberately
                    // reaches a buffer row back to find below blocks anchored
                    // outside the edit, so without this it finds that one too and
                    // emits it a second time.
                    let first_row = if kept_below_blocks_at_start {
                        new_start + 1
                    } else {
                        new_start
                    };
                    if block_start < edit_end && block_end >= first_row {
                        Some((placement, b))
                    } else {
                        if block_start >= edit_end {
                            deferred_block_idx = deferred_block_idx.min(start_block_idx + offset);
                        }
                        None
                    }
                }),
        );
        sort_and_dedup_blocks(&mut blocks_in_range);

        let mut row = new_transforms.extent::<InputRow>(()).0;
        for &(placement, block) in &blocks_in_range {
            let anchor = placement.start_wrap_row();
            if anchor > row {
                push_isomorphic(&mut new_transforms, anchor - row);
                row = anchor;
            }

            let input_rows = placement.input_rows();
            new_transforms.push(
                Transform {
                    summary: TransformSummary {
                        input_rows,
                        output_rows: block.height(),
                    },
                    block: Some(block.clone()),
                },
                (),
            );
            row += input_rows;
        }

        if edit_end > row {
            push_isomorphic(&mut new_transforms, edit_end - row);
        }

        last_block_idx = deferred_block_idx;
    }

    new_transforms.append(cursor.suffix(), ());

    if new_transforms.is_empty() && wrap_line_count > 0 {
        new_transforms.push(
            Transform {
                summary: TransformSummary {
                    input_rows: wrap_line_count,
                    output_rows: wrap_line_count,
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
            transforms.push(
                Transform {
                    summary: TransformSummary {
                        input_rows: wrap_line_count,
                        output_rows: wrap_line_count,
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
            push_isomorphic(&mut transforms, anchor - current_wrap_row);
            current_wrap_row = anchor;
        }

        let input_rows = placement.input_rows();
        transforms.push(
            Transform {
                summary: TransformSummary {
                    input_rows,
                    output_rows: block.height(),
                },
                block: Some(block.clone()),
            },
            (),
        );
        current_wrap_row += input_rows;
    }

    if current_wrap_row < wrap_line_count {
        let rows = wrap_line_count - current_wrap_row;
        push_isomorphic(&mut transforms, rows);
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
    let tab_point = wrap_snapshot.to_tab_point(WrapPoint::new(wrap_row, 0));
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

fn push_isomorphic(transforms: &mut SumTree<Transform>, rows: u32) {
    if rows == 0 {
        return;
    }

    let mut merged = false;
    transforms.update_last(
        |last| {
            if last.block.is_none() {
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

    #[test]
    fn carrying_rows_across_a_multi_edit_patch() {
        // Two edits each adding a row. The second's new range is stated with
        // the first's row already added, so its own ends give +2 as the running
        // total and not as a further step, and a row below both lands at +2
        // rather than at +3.
        let edits = Patch::new(vec![
            stoat_text::patch::Edit {
                old: 2..2,
                new: 2..3,
            },
            stoat_text::patch::Edit {
                old: 10..10,
                new: 11..12,
            },
        ]);
        let mut rows = [0, 1, 5, 20];
        let mut touch = [false; 4];
        super::carry_rows(&mut rows, &mut touch, &edits);

        assert_eq!(rows, [0, 1, 6, 22]);
        assert_eq!(
            touch, [false; 4],
            "both edits are pure insertions, so every row only shifted",
        );
    }

    #[test]
    fn carrying_rows_through_a_replaced_range() {
        // A row the edit covers has nowhere of its own to go, so it collapses
        // onto where the replacement starts.
        let edits = Patch::new(vec![stoat_text::patch::Edit {
            old: 3..6,
            new: 3..4,
        }]);
        let mut rows = [1, 4, 5, 9];
        let mut touch = [false; 4];
        super::carry_rows(&mut rows, &mut touch, &edits);

        assert_eq!(rows, [1, 3, 3, 7]);
        assert_eq!(
            touch,
            [false, true, true, false],
            "only the two rows inside the replaced range collapsed",
        );
    }

    use crate::{
        buffer::{BufferId, TextBuffer},
        display_map::{
            fold_map::{FoldMap, FoldPlaceholder},
            inlay_map::InlayMap,
            sampler,
            tab_map::TabMap,
            wrap_map::WrapMap,
        },
        multi_buffer::MultiBuffer,
    };
    use ratatui::text::Line;
    use std::{
        ops::Range,
        sync::{Arc, OnceLock, RwLock},
    };
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
        let (_, wrap_snapshot) =
            WrapMap::new(tab_snapshot, None, test_executor(), crate::test_notify());
        let mut block_map = BlockMap::new();
        block_map.insert(props.to_vec());
        block_map.sync(wrap_snapshot, &Patch::empty(), &Patch::empty(), None)
    }

    /// [`create_block_snapshot`] with soft wrap on and `fold` (given as buffer
    /// `(row, column)` ends) folded behind the default ellipsis.
    fn wrapped_block_snapshot(
        content: &str,
        wrap_width: u32,
        fold: Range<(u32, u32)>,
    ) -> super::BlockSnapshot {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let offset = |(row, column): (u32, u32)| {
            buffer_snapshot
                .rope()
                .point_to_offset(Point::new(row, column))
        };
        fold_map.fold(
            vec![
                buffer_snapshot.anchor_at(offset(fold.start), Bias::Right)
                    ..buffer_snapshot.anchor_at(offset(fold.end), Bias::Left),
            ],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        let (fold_snapshot, _) = fold_map.sync(inlay_snapshot, &Patch::empty(), None);

        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).unwrap());
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (_, wrap_snapshot) = WrapMap::new(
            tab_snapshot,
            Some(wrap_width),
            test_executor(),
            crate::test_notify(),
        );
        BlockMap::new().sync(wrap_snapshot, &Patch::empty(), &Patch::empty(), None)
    }

    /// Wrap-space clipping answers on whichever row the position it chose lives
    /// on, and a fold placeholder straddling a wrap break makes that a
    /// different row than the one asked about. Keeping the row asked about and
    /// taking only the column reads that column against the wrong row, which
    /// shows up as the rightward bias landing before the leftward one.
    #[test]
    fn clip_point_follows_a_fold_across_a_wrap_break() {
        let snapshot = wrapped_block_snapshot("abcdefghij\nklm\n", 5, (0, 3)..(0, 7));

        for row in 0..snapshot.total_lines() {
            for column in 0..=snapshot.line_len(row) + 1 {
                let point = BlockPoint::new(row, column);
                let left = snapshot.clip_point(point, Bias::Left);
                let right = snapshot.clip_point(point, Bias::Right);

                assert!(
                    left <= right,
                    "{point:?} clips to {left:?} left of {right:?} right",
                );
                for (clipped, bias) in [(left, Bias::Left), (right, Bias::Right)] {
                    assert_eq!(
                        snapshot.clip_point(clipped, bias),
                        clipped,
                        "{clipped:?} moves when clipped {bias:?} again",
                    );
                    assert!(
                        snapshot.block_to_buffer(clipped).is_some(),
                        "{point:?} clips to {clipped:?}, which names no buffer position",
                    );
                }
            }
        }
    }

    /// A touch is what turns a block into a rebuilt region, so an edit that only
    /// slides blocks must produce none. Only a block whose anchor row the edit
    /// replaced has moved relative to the text, and that one still touches.
    #[test]
    fn only_a_collapsed_block_is_touched() {
        let shift = Patch::new(vec![stoat_text::patch::Edit {
            old: 2..2,
            new: 2..5,
        }]);
        let collapse = Patch::new(vec![stoat_text::patch::Edit {
            old: 8..12,
            new: 8..9,
        }]);

        let touched_by = |edits: &Patch<u32>| {
            let mut block_map = BlockMap::new();
            block_map.insert(vec![
                text_block(BlockPlacement::Below(10), "a"),
                text_block(BlockPlacement::Below(15), "b"),
            ]);
            block_map.touched_placements.clear();
            block_map.carry_block_placements(edits);
            block_map.touched_placements.len()
        };

        assert_eq!(
            (touched_by(&shift), touched_by(&collapse)),
            (0, 1),
            "three rows inserted above both blocks touches neither; \
             replacing rows 8..12 collapses the block anchored at 10",
        );
    }

    fn text_block(placement: BlockPlacement, content: &str) -> BlockProperties {
        BlockProperties::from_text(
            placement,
            content.lines().map(String::from).collect(),
            BlockStyle::Fixed,
        )
    }

    #[test]
    fn custom_block_render_memoized() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

        let calls = Arc::new(AtomicUsize::new(0));
        let props = BlockProperties::from_lines_fn(
            BlockPlacement::Below(0),
            3,
            {
                let calls = calls.clone();
                Arc::new(move |i| {
                    calls.fetch_add(1, SeqCst);
                    format!("line {i}")
                })
            },
            BlockStyle::Fixed,
        );
        let block = super::Block::Custom(Arc::new(super::CustomBlock {
            id: super::CustomBlockId(0),
            placement: props.placement,
            height: props.height,
            render: props.render,
            diff_status: props.diff_status,
            style: props.style,
            priority: props.priority,
            rendered: OnceLock::new(),
        }));

        for _ in 0..3 {
            assert_eq!(block.get_line(0), "line 0");
            assert_eq!(block.get_line(2), "line 2");
            assert_eq!(block.line_len(1), "line 1".len() as u32);
        }

        assert_eq!(
            calls.load(SeqCst),
            3,
            "the render closure must run once, not per line access"
        );
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

    /// The lengths are taken once and kept beside the lines, so nothing forces
    /// them to keep describing the text `get_line` hands back. A block whose
    /// lines differ in width is what would show the two coming apart.
    #[test]
    fn a_blocks_reported_lengths_describe_the_lines_it_renders() {
        let block = super::Block::Custom(Arc::new(super::CustomBlock {
            id: super::CustomBlockId(0),
            placement: BlockPlacement::Below(0),
            height: Some(4),
            render: {
                let lines = ["", "one", "a much wider line", "mid"];
                Arc::new(move |_: &super::BlockContext<'_>| {
                    lines.iter().map(|l| Line::from(l.to_string())).collect()
                })
            },
            diff_status: None,
            style: BlockStyle::Fixed,
            priority: 0,
            rendered: OnceLock::new(),
        }));

        let lens: Vec<u32> = (0..5).map(|i| block.line_len(i)).collect();
        assert_eq!(
            lens,
            vec![0, 3, 17, 3, 0],
            "a row past the end measures zero"
        );

        for row in 0..4 {
            assert_eq!(
                block.line_len(row) as usize,
                block.get_line(row).len(),
                "row {row}",
            );
        }
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

    /// A block at a document edge has buffer text on one side only, so the scan
    /// the bias asks for runs out. Clipping still has to name a row the buffer
    /// holds, which means falling back to the other direction rather than to the
    /// edge of the document, since that edge is the block itself.
    #[test]
    fn clip_point_leaves_a_block_at_either_edge() {
        let blocks = vec![
            text_block(BlockPlacement::Above(0), "deleted above"),
            text_block(BlockPlacement::Below(1), "deleted below"),
        ];
        let snapshot = create_block_snapshot("hello\nworld", &blocks);

        let block_rows: Vec<u32> = (0..snapshot.total_lines())
            .filter(|&row| matches!(snapshot.classify_row(row), BlockRowKind::Block { .. }))
            .collect();
        assert_eq!(block_rows, [0, 3], "a block sits at each edge");

        for (row, bias, clipped, buffer) in [
            (0, Bias::Left, BlockPoint::new(1, 0), Point::new(0, 0)),
            (0, Bias::Right, BlockPoint::new(1, 0), Point::new(0, 0)),
            (3, Bias::Left, BlockPoint::new(2, 5), Point::new(1, 5)),
            (3, Bias::Right, BlockPoint::new(2, 5), Point::new(1, 5)),
        ] {
            let actual = snapshot.clip_point(BlockPoint::new(row, 0), bias);
            assert_eq!(actual, clipped, "clipping block row {row} {bias:?}");
            assert_eq!(
                snapshot.block_to_buffer(actual),
                Some(buffer),
                "block row {row} {bias:?} clips where the buffer cannot be addressed",
            );
        }
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
        let (_, wrap_snapshot) =
            WrapMap::new(tab_snapshot, None, test_executor(), crate::test_notify());
        let mut block_map = BlockMap::new();
        let snapshot = block_map.sync(wrap_snapshot, &Patch::empty(), &Patch::empty(), None);

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
        let (_, wrap_snapshot) =
            WrapMap::new(tab_snapshot, None, test_executor(), crate::test_notify());
        wrap_snapshot
    }

    #[test]
    fn cache_reused_when_nothing_changes() {
        let wrap_snapshot = create_wrap_snapshot("hello\nworld");
        let mut block_map = BlockMap::new();
        block_map.insert(vec![text_block(BlockPlacement::Below(0), "deleted")]);

        let snap1 = block_map.sync(
            Arc::clone(&wrap_snapshot),
            &Patch::empty(),
            &Patch::empty(),
            None,
        );
        let snap2 = block_map.sync(wrap_snapshot, &Patch::empty(), &Patch::empty(), None);

        assert_eq!(snap1.total_lines(), snap2.total_lines());
    }

    #[test]
    fn cache_invalidated_on_block_change() {
        let wrap_snapshot = create_wrap_snapshot("hello\nworld");
        let mut block_map = BlockMap::new();
        let ids = block_map.insert(vec![text_block(BlockPlacement::Below(0), "deleted")]);

        let snap1 = block_map.sync(
            Arc::clone(&wrap_snapshot),
            &Patch::empty(),
            &Patch::empty(),
            None,
        );
        assert_eq!(snap1.total_lines(), 3);

        block_map.remove(&ids.into_iter().collect());
        block_map.insert(vec![text_block(
            BlockPlacement::Below(0),
            "deleted\nextra line",
        )]);

        let snap2 = block_map.sync(wrap_snapshot, &Patch::empty(), &Patch::empty(), None);
        assert_eq!(snap2.total_lines(), 4);
    }

    /// Splicing a block no longer rebuilds the file, so the patched tree has to
    /// be held against the one a rebuild would have produced. The ways the two
    /// come apart are quiet ones. A stale block left beside its replacement and
    /// a new block dropped both read as a plausible number of rows.
    #[test]
    fn an_incremental_splice_agrees_with_a_full_rebuild() {
        let content: String = (0..12).map(|i| format!("line{i}\n")).collect();
        let wrap_snapshot = create_wrap_snapshot(&content);
        let sync = |map: &mut BlockMap| {
            map.sync(
                Arc::clone(&wrap_snapshot),
                &Patch::empty(),
                &Patch::empty(),
                None,
            )
        };

        let mut block_map = BlockMap::new();
        let ids = block_map.insert(vec![
            text_block(BlockPlacement::Below(2), "below two"),
            text_block(BlockPlacement::Above(7), "above seven"),
            text_block(BlockPlacement::Below(9), "below nine\nsecond"),
        ]);
        sync(&mut block_map);

        block_map.remove(&[ids[1]].into_iter().collect());
        block_map.insert(vec![
            text_block(BlockPlacement::Above(4), "above four"),
            text_block(BlockPlacement::Below(9), "another below nine"),
        ]);
        let patched = sync(&mut block_map);
        let patched_rows: Vec<String> = (0..patched.total_lines())
            .map(|row| patched.display_line(row))
            .collect();

        block_map.mark_dirty();
        let rebuilt = sync(&mut block_map);
        let rebuilt_rows: Vec<String> = (0..rebuilt.total_lines())
            .map(|row| rebuilt.display_line(row))
            .collect();

        assert_eq!(patched_rows, rebuilt_rows);
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

    /// The replaced rows are the case the other replace tests skip. They read
    /// the display rows around a replacement, never the buffer rows under it.
    #[test]
    fn buffer_to_block_inside_a_replaced_range_lands_on_the_replacement() {
        let blocks = vec![text_block(
            BlockPlacement::Replace { start: 1, end: 3 },
            "rep0\nrep1",
        )];
        let snapshot = create_block_snapshot("r0\nr1\nr2\nr3\nr4", &blocks);

        for row in 1..=3 {
            assert_eq!(
                snapshot.buffer_to_block(Point::new(row, 1)),
                BlockPoint::new(1, 0),
                "buffer row {row} is replaced",
            );
        }

        assert_eq!(
            snapshot.buffer_to_block(Point::new(0, 1)),
            BlockPoint::new(0, 1),
            "the row above the replacement is untouched",
        );
        assert_eq!(
            snapshot.buffer_to_block(Point::new(4, 1)),
            BlockPoint::new(3, 1),
            "the row below it shifts by the rows the replacement swallowed",
        );
    }

    /// The sort is unstable, so blocks landing on one row with the same
    /// placement render in whatever order it happened to leave them unless the
    /// comparator separates them itself.
    #[test]
    fn blocks_sharing_a_row_render_in_one_settled_order() {
        let rendered = |props: &[BlockProperties]| -> Vec<String> {
            let snapshot = create_block_snapshot("line0\nline1", props);
            (0..snapshot.total_lines())
                .map(|row| snapshot.display_line(row))
                .collect()
        };

        let mut props: Vec<BlockProperties> = [("first", 5), ("second", 3), ("third", 1)]
            .iter()
            .map(|(text, priority)| {
                let mut block = text_block(BlockPlacement::Below(0), text);
                block.priority = *priority;
                block
            })
            .collect();

        let by_priority = vec!["line0", "third", "second", "first", "line1"];
        assert_eq!(rendered(&props), by_priority);

        // The same blocks handed over in the opposite order land the same way,
        // which holds only if the comparator settled it rather than the sort.
        props.reverse();
        assert_eq!(rendered(&props), by_priority, "priority outranks insertion");

        // Blocks left equal by priority fall back to the order they arrived in,
        // since ids are minted as they are inserted.
        let tied: Vec<BlockProperties> = ["alpha", "beta"]
            .iter()
            .map(|text| text_block(BlockPlacement::Below(0), text))
            .collect();
        assert_eq!(
            rendered(&tied),
            vec!["line0", "alpha", "beta", "line1"],
            "an equal priority leaves insertion order deciding",
        );
    }

    /// A replaced range stands for the rows it replaces. A `Near` block is
    /// attached beside a row rather than occupying one, so it outlives the
    /// replacement the way it outlives the row itself.
    #[test]
    fn a_near_block_survives_a_replacement_over_its_row() {
        // Near resolves a row past the one it names, so this lands inside the
        // replaced span rather than beyond it.
        let blocks = vec![
            text_block(BlockPlacement::Replace { start: 1, end: 3 }, "replacement"),
            text_block(BlockPlacement::Near(1), "near-block"),
        ];
        let snapshot = create_block_snapshot("line0\nline1\nline2\nline3\nline4", &blocks);

        let rows: Vec<String> = (0..snapshot.total_lines())
            .map(|row| snapshot.display_line(row))
            .collect();
        assert_eq!(rows, vec!["line0", "replacement", "near-block", "line4"]);
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

        let snap = block_map.sync(
            Arc::clone(&wrap_snapshot),
            &Patch::empty(),
            &Patch::empty(),
            None,
        );
        assert_eq!(snap.total_lines(), 5);

        block_map.remove(&[ids[0]].into_iter().collect());
        let snap = block_map.sync(wrap_snapshot, &Patch::empty(), &Patch::empty(), None);
        assert_eq!(snap.total_lines(), 4);
    }

    /// A generated stack under a generated block set.
    ///
    /// The placements draw from all four kinds. `Replace` is what makes rows
    /// disappear rather than only shift, and it is the one that can leave a
    /// document holding no buffer text at all.
    fn random_block_snapshot(seed: u64) -> super::BlockSnapshot {
        let (mut sampler, wrap_snapshot) = sampler::random_wrap_stack(seed);
        let buffer_rows = wrap_snapshot
            .tab_snapshot()
            .fold_snapshot()
            .inlay_snapshot()
            .buffer_snapshot()
            .line_count();

        // One block per row, ascending, with every `Replace` range stopping
        // short of the next block's row. `build_transforms` resolves placements
        // through forward-only cursors, so the rows it maps have to ascend, and
        // a `Replace` reaching the row after it would send the next block's
        // lookup backwards. Nothing in the editor builds a `Replace` spanning
        // more than the row it names, so this is the shape the layer is asked
        // for rather than a restriction on the sweep.
        let mut rows: Vec<u32> = (0..sampler.below(4))
            .map(|_| sampler.below(buffer_rows))
            .collect();
        rows.sort_unstable();
        rows.dedup();

        let blocks = rows
            .iter()
            .enumerate()
            .map(|(index, &row)| {
                let next = rows.get(index + 1).copied().unwrap_or(buffer_rows);
                let placement = match sampler.below(4) {
                    0 => BlockPlacement::Above(row),
                    1 => BlockPlacement::Below(row),
                    2 => BlockPlacement::Near(row),
                    _ => BlockPlacement::Replace {
                        start: row,
                        end: row + sampler.below(2).min(next - row - 1),
                    },
                };
                let lines = (0..1 + sampler.below(2))
                    .map(|line| format!("block{row}-{line}"))
                    .collect();
                BlockProperties::from_text(placement, lines, BlockStyle::Fixed)
            })
            .collect();

        let mut block_map = BlockMap::new();
        block_map.insert(blocks);
        block_map.sync(wrap_snapshot, &Patch::empty(), &Patch::empty(), None)
    }

    /// A block's rows hold no buffer text, so clipping has to move off them and
    /// onto a row the buffer can name. Clipping also has to settle, keep the
    /// two biases in the order they name, and land inside the row it answers
    /// with.
    ///
    /// The addressability check is skipped for a document whose every row a
    /// `Replace` block swallowed. There is no position to answer with there, so
    /// requiring one would assert something false rather than find a bug.
    #[test]
    fn clipping_a_block_point_settles_on_buffer_text() {
        for seed in 0..256 {
            let snap = random_block_snapshot(seed);
            let addressable = (0..snap.total_lines())
                .any(|row| matches!(snap.classify_row(row), BlockRowKind::BufferRow { .. }));

            for row in 0..snap.total_lines() {
                for column in 0..=snap.line_len(row) + 1 {
                    let point = BlockPoint::new(row, column);
                    let left = snap.clip_point(point, Bias::Left);
                    let right = snap.clip_point(point, Bias::Right);

                    assert!(
                        left <= right,
                        "seed {seed}: {point:?} clips to {left:?} left of {right:?} right",
                    );
                    for (clipped, bias) in [(left, Bias::Left), (right, Bias::Right)] {
                        assert_eq!(
                            snap.clip_point(clipped, bias),
                            clipped,
                            "seed {seed}: {point:?} clipped {bias:?} to {clipped:?}, which moves \
                             again",
                        );
                        assert!(
                            clipped.column <= snap.line_len(clipped.row),
                            "seed {seed}: {clipped:?} runs past its row",
                        );

                        if addressable {
                            let buffer = snap.block_to_buffer(clipped);
                            assert!(
                                buffer.is_some(),
                                "seed {seed}: {point:?} clipped {bias:?} to {clipped:?}, which \
                                 names no buffer position",
                            );
                        }
                    }
                }
            }
        }
    }
}
