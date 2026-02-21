//! BlockMap v2: Block decoration transformation layer.
//!
//! Inserts visual blocks between lines (diagnostics, git blame, code lens, etc.).
//! This implementation starts with basic coordinate transformation and can be
//! enhanced with full block rendering support later.
//!
//! # Transform Architecture
//!
//! BlockMap uses a struct-based Transform similar to WrapMap:
//! - `block_height == 0`: Isomorphic transform (no block, 1:1 mapping)
//! - `block_height > 0`: Block transform (adds display rows)
//!
//! # Coordinate Transformation
//!
//! Blocks **add rows** to display without consuming input:
//! ```text
//! WrapPoint (input):        BlockPoint (output):
//! Row 0: "fn example()"     Row 0: "fn example()"
//!                           Row 1: [Diagnostic Block - 2 rows]
//!                           Row 2: [continued]
//! Row 1: "  let x = 42"     Row 3: "  let x = 42"
//! ```
//!
//! # Related
//!
//! - Input: [`WrapPoint`](crate::WrapPoint) from [`WrapSnapshot`](crate::wrap_map::WrapSnapshot)
//! - Output: [`BlockPoint`](crate::BlockPoint) which becomes final DisplayPoint
use crate::{
    coords::{BlockPoint, WrapPoint},
    dimensions::BlockOffset,
    wrap_map::WrapSnapshot,
};
use std::{
    cmp::Ordering,
    collections::HashMap,
    ops::{Range, RangeInclusive},
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Arc,
    },
};
use sum_tree::{Bias, Item, SumTree};
use text::{Anchor, BufferSnapshot, Edit, Point, TextSummary, ToOffset};

/// Unique identifier for a custom block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CustomBlockId(pub usize);

/// Edit in BlockOffset space (output from BlockMap).
pub type BlockEdit = Edit<BlockOffset>;

/// Style of block rendering.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Default)]
pub enum BlockStyle {
    /// Fixed-height block.
    #[default]
    Fixed,
    /// Flexible-height block that can grow/shrink.
    Flex,
    /// Sticky block that stays visible when scrolling.
    Sticky,
}

/// Placement strategy for a block relative to an anchor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockPlacement<T> {
    /// Block is placed above the anchor.
    Above(T),
    /// Block is placed below the anchor.
    Below(T),
    /// Block is placed near the anchor (smart placement based on space).
    Near(T),
    /// Block replaces the given range.
    Replace(RangeInclusive<T>),
}

impl<T> BlockPlacement<T> {
    /// Get the start position of this placement.
    pub fn start(&self) -> &T {
        match self {
            BlockPlacement::Above(position) => position,
            BlockPlacement::Below(position) => position,
            BlockPlacement::Near(position) => position,
            BlockPlacement::Replace(range) => range.start(),
        }
    }

    /// Get the end position of this placement.
    fn end(&self) -> &T {
        match self {
            BlockPlacement::Above(position) => position,
            BlockPlacement::Below(position) => position,
            BlockPlacement::Near(position) => position,
            BlockPlacement::Replace(range) => range.end(),
        }
    }

    /// Get a reference to this placement.
    pub fn as_ref(&self) -> BlockPlacement<&T> {
        match self {
            BlockPlacement::Above(position) => BlockPlacement::Above(position),
            BlockPlacement::Below(position) => BlockPlacement::Below(position),
            BlockPlacement::Near(position) => BlockPlacement::Near(position),
            BlockPlacement::Replace(range) => BlockPlacement::Replace(range.start()..=range.end()),
        }
    }

    /// Map the coordinate type using a conversion function.
    pub fn map<R>(self, mut f: impl FnMut(T) -> R) -> BlockPlacement<R> {
        match self {
            BlockPlacement::Above(position) => BlockPlacement::Above(f(position)),
            BlockPlacement::Below(position) => BlockPlacement::Below(f(position)),
            BlockPlacement::Near(position) => BlockPlacement::Near(f(position)),
            BlockPlacement::Replace(range) => {
                let (start, end) = range.into_inner();
                BlockPlacement::Replace(f(start)..=f(end))
            },
        }
    }

    /// Tiebreaker for sorting blocks at the same position.
    ///
    /// Replace blocks come first, then Above, then Near, then Below.
    fn tie_break(&self) -> u8 {
        match self {
            BlockPlacement::Replace(_) => 0,
            BlockPlacement::Above(_) => 1,
            BlockPlacement::Near(_) => 2,
            BlockPlacement::Below(_) => 3,
        }
    }
}

impl BlockPlacement<Anchor> {
    /// Compare two placements using buffer positions.
    fn cmp(&self, other: &Self, buffer: &BufferSnapshot) -> Ordering {
        let self_start_offset = self.start().to_offset(buffer);
        let other_start_offset = other.start().to_offset(buffer);

        self_start_offset
            .cmp(&other_start_offset)
            .then_with(|| {
                let self_end_offset = self.end().to_offset(buffer);
                let other_end_offset = other.end().to_offset(buffer);
                other_end_offset.cmp(&self_end_offset)
            })
            .then_with(|| self.tie_break().cmp(&other.tie_break()))
    }
}

/// Properties for creating a custom block.
#[derive(Clone)]
pub struct BlockProperties<P> {
    /// Placement relative to anchor.
    pub placement: BlockPlacement<P>,
    /// Height in rows. None for zero-height (e.g., horizontal line).
    pub height: Option<u32>,
    /// Rendering style.
    pub style: BlockStyle,
    /// Rendering priority (higher = rendered later).
    pub priority: usize,
}

/// A custom block decoration.
pub struct CustomBlock {
    /// Unique identifier.
    pub id: CustomBlockId,
    /// Placement in the buffer.
    pub placement: BlockPlacement<Anchor>,
    /// Height in display rows.
    pub height: Option<u32>,
    /// Rendering style.
    pub style: BlockStyle,
    /// Rendering priority.
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

/// Transform representing either an isomorphic region or a block.
///
/// This is a **struct** where `block_height` determines the type:
/// - `block_height == 0`: Isomorphic transform (no block, 1:1 mapping)
/// - `block_height > 0`: Block transform (adds N display rows)
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Transform {
    /// Aggregated summary of input/output coordinates.
    summary: TransformSummary,

    /// Number of display rows this block occupies.
    /// Zero indicates isomorphic region.
    block_height: u32,
}

impl Transform {
    /// Create an isomorphic transform with 1:1 mapping.
    ///
    /// Input and output summaries are identical since no blocks are inserted.
    fn isomorphic(summary: TextSummary) -> Self {
        #[cfg(test)]
        assert!(
            !summary.lines.is_zero(),
            "Isomorphic transform must have content"
        );

        Self {
            summary: TransformSummary {
                input: summary,
                output: summary,
            },
            block_height: 0,
        }
    }

    /// Create a block transform with the given height.
    ///
    /// The block has:
    /// - Zero input (doesn't consume wrap space)
    /// - `block_height` output rows with zero columns
    fn block(height: u32) -> Self {
        Self {
            summary: TransformSummary {
                input: TextSummary::default(),
                output: TextSummary {
                    lines: Point::new(height, 0),
                    first_line_chars: 0,
                    last_line_chars: 0,
                    longest_row: 0,
                    longest_row_chars: 0,
                    len: 0,
                    chars: 0,
                    last_line_len_utf16: 0,
                    len_utf16: text::OffsetUtf16(0),
                },
            },
            block_height: height,
        }
    }

    /// Check if this transform is isomorphic (no block).
    fn is_isomorphic(&self) -> bool {
        self.block_height == 0
    }
}

/// Summary aggregating coordinate information for a Transform subtree.
///
/// Tracks both input (WrapPoint) and output (BlockPoint) coordinate spaces.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TransformSummary {
    /// Input summary (WrapPoint space before blocks).
    input: TextSummary,

    /// Output summary (BlockPoint space after blocks).
    output: TextSummary,
}

impl sum_tree::ContextLessSummary for TransformSummary {
    fn zero() -> Self {
        Self::default()
    }

    fn add_summary(&mut self, other: &Self) {
        self.input += other.input;
        self.output += other.output;
    }
}

impl Item for Transform {
    type Summary = TransformSummary;

    fn summary(&self, _cx: ()) -> Self::Summary {
        self.summary.clone()
    }
}

/// Push an isomorphic transform, merging with the last transform if it's also isomorphic.
fn push_isomorphic(transforms: &mut Vec<Transform>, summary: TextSummary) {
    if summary.lines.is_zero() {
        return;
    }

    if let Some(last) = transforms.last_mut() {
        if last.is_isomorphic() {
            last.summary.input += &summary;
            last.summary.output += &summary;
            return;
        }
    }
    transforms.push(Transform::isomorphic(summary));
}

// Dimension trait implementations for coordinate seeking

impl<'a> sum_tree::Dimension<'a, TransformSummary> for WrapPoint {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
        let lines = &summary.input.lines;
        if lines.row > 0 {
            self.row += lines.row;
            self.column = lines.column;
        } else {
            self.column += lines.column;
        }
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for BlockPoint {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
        let lines = &summary.output.lines;
        if lines.row > 0 {
            self.row += lines.row;
            self.column = lines.column;
        } else {
            self.column += lines.column;
        }
    }
}

#[cfg(test)]
mod tests_transform {
    use super::*;

    #[test]
    fn transform_isomorphic() {
        let summary = TextSummary::from("hello world");
        let transform = Transform::isomorphic(summary);

        assert!(transform.is_isomorphic());
        assert_eq!(transform.summary.input, summary);
        assert_eq!(transform.summary.output, summary);
        assert_eq!(transform.block_height, 0);
    }

    #[test]
    fn transform_block() {
        let transform = Transform::block(3);

        assert!(!transform.is_isomorphic());
        assert_eq!(transform.summary.input, TextSummary::default());
        assert_eq!(transform.summary.output.lines, Point::new(3, 0));
        assert_eq!(transform.block_height, 3);
    }

    #[test]
    fn push_isomorphic_merges() {
        let mut transforms = Vec::new();

        let summary1 = TextSummary::from("hello ");
        let summary2 = TextSummary::from("world");

        push_isomorphic(&mut transforms, summary1);
        push_isomorphic(&mut transforms, summary2);

        // Should have merged into one transform
        assert_eq!(transforms.len(), 1);
        assert_eq!(transforms[0].summary.input, summary1 + summary2);
    }

    #[test]
    fn push_isomorphic_doesnt_merge_with_block() {
        let mut transforms = Vec::new();

        transforms.push(Transform::block(2));
        push_isomorphic(&mut transforms, TextSummary::from("hello"));

        // Should have 2 transforms (block doesn't merge with isomorphic)
        assert_eq!(transforms.len(), 2);
    }
}

/// Mutable block map managing custom block decorations.
///
/// Maintains a sorted list of custom blocks and rebuilds transforms when
/// blocks are inserted, removed, or resized.
pub struct BlockMap {
    /// Next block ID to allocate.
    next_block_id: AtomicUsize,
    /// Current wrap snapshot.
    wrap_snapshot: WrapSnapshot,
    /// All custom blocks, sorted by placement.
    custom_blocks: Vec<Arc<CustomBlock>>,
    /// Fast lookup by block ID.
    custom_blocks_by_id: HashMap<CustomBlockId, Arc<CustomBlock>>,
    /// Current transform tree.
    transforms: SumTree<Transform>,
}

impl BlockMap {
    /// Create a new BlockMap with no blocks.
    pub fn new(wrap_snapshot: WrapSnapshot) -> Self {
        // Initialize with a single isomorphic transform covering the entire file
        let max_point = wrap_snapshot.max_point();
        let summary = wrap_snapshot.text_summary_for_range(0..max_point.row + 1);

        let mut transforms = SumTree::new(());
        if !summary.lines.is_zero() {
            transforms = SumTree::from_iter([Transform::isomorphic(summary)], ());
        }

        Self {
            next_block_id: AtomicUsize::new(0),
            wrap_snapshot,
            custom_blocks: Vec::new(),
            custom_blocks_by_id: HashMap::new(),
            transforms,
        }
    }

    /// Insert custom blocks and return their IDs.
    pub fn insert(
        &mut self,
        blocks: impl IntoIterator<Item = BlockProperties<Anchor>>,
    ) -> Vec<CustomBlockId> {
        let mut ids = Vec::new();
        let buffer = self
            .wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .buffer();

        for block_props in blocks {
            let id = CustomBlockId(self.next_block_id.fetch_add(1, SeqCst));
            ids.push(id);

            let block = Arc::new(CustomBlock {
                id,
                placement: block_props.placement,
                height: block_props.height,
                style: block_props.style,
                priority: block_props.priority,
            });

            // Insert in sorted order
            let insert_index = self
                .custom_blocks
                .binary_search_by(|probe| probe.placement.cmp(&block.placement, buffer))
                .unwrap_or_else(|i| i);

            self.custom_blocks.insert(insert_index, block.clone());
            self.custom_blocks_by_id.insert(id, block);
        }

        // Rebuild all transforms after insertion
        self.rebuild_transforms();
        ids
    }

    /// Remove blocks by their IDs.
    pub fn remove(&mut self, ids: &[CustomBlockId]) {
        if ids.is_empty() {
            return;
        }

        // Remove from both collections
        for id in ids {
            self.custom_blocks_by_id.remove(id);
        }
        self.custom_blocks.retain(|block| !ids.contains(&block.id));

        // Rebuild all transforms after removal
        self.rebuild_transforms();
    }

    /// Resize blocks to new heights.
    pub fn resize(&mut self, heights: HashMap<CustomBlockId, u32>) {
        if heights.is_empty() {
            return;
        }

        let mut changed = false;

        for block in &mut self.custom_blocks {
            if let Some(&new_height) = heights.get(&block.id) {
                if block.height != Some(new_height) {
                    let new_block = Arc::new(CustomBlock {
                        id: block.id,
                        placement: block.placement.clone(),
                        height: Some(new_height),
                        style: block.style,
                        priority: block.priority,
                    });

                    *block = new_block.clone();
                    self.custom_blocks_by_id.insert(block.id, new_block);
                    changed = true;
                }
            }
        }

        if changed {
            self.rebuild_transforms();
        }
    }

    /// Get an immutable snapshot.
    pub fn snapshot(&self) -> BlockSnapshot {
        BlockSnapshot {
            wrap_snapshot: self.wrap_snapshot.clone(),
            transforms: self.transforms.clone(),
        }
    }

    /// Update the wrap snapshot and rebuild transforms if needed.
    ///
    /// Returns the new snapshot and edits in block space.
    /// For now, returns empty edits (full rebuild on every sync).
    /// Sync with new wrap snapshot and transform wrap edits to block edits.
    ///
    /// Simplified implementation: converts wrap edits to block coordinates
    /// by assuming wrap points map directly to block points (no additional transforms).
    pub fn sync(
        &mut self,
        wrap_snapshot: WrapSnapshot,
        wrap_edits: Vec<Edit<u32>>,
    ) -> (BlockSnapshot, Vec<BlockEdit>) {
        tracing::trace!(
            "BlockMap.sync: wrap_max_point=({}, {})",
            wrap_snapshot.max_point().row,
            wrap_snapshot.max_point().column
        );
        self.wrap_snapshot = wrap_snapshot;
        self.rebuild_transforms();

        // Transform wrap edits (u32 offsets) to block edits (BlockOffset)
        // FIXME: Simplified - should properly account for block insertions
        let block_edits = wrap_edits
            .into_iter()
            .map(|edit| BlockEdit {
                old: BlockOffset(edit.old.start as usize)..BlockOffset(edit.old.end as usize),
                new: BlockOffset(edit.new.start as usize)..BlockOffset(edit.new.end as usize),
            })
            .collect();

        (self.snapshot(), block_edits)
    }

    /// Rebuild the entire transform tree from scratch.
    fn rebuild_transforms(&mut self) {
        let mut transforms = Vec::new();
        let buffer = self
            .wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .buffer();
        let max_wrap_row = self.wrap_snapshot.max_point().row;

        if self.custom_blocks.is_empty() {
            // No blocks - single isomorphic transform
            let summary = self
                .wrap_snapshot
                .text_summary_for_range(0..max_wrap_row + 1);
            if !summary.lines.is_zero() {
                transforms.push(Transform::isomorphic(summary));
            }
        } else {
            let mut current_row = 0u32;

            for block in &self.custom_blocks {
                // Get the wrap row for this block
                let block_start = block.placement.start();
                let block_point = buffer.offset_to_point(block_start.to_offset(buffer));

                // Convert buffer point through layers to WrapPoint
                let inlay_point = self
                    .wrap_snapshot
                    .tab_snapshot
                    .fold_snapshot
                    .inlay_snapshot
                    .to_inlay_point(block_point, Bias::Left);
                let fold_point = self
                    .wrap_snapshot
                    .tab_snapshot
                    .fold_snapshot
                    .to_fold_point(inlay_point, Bias::Right);
                let tab_point = self
                    .wrap_snapshot
                    .tab_snapshot
                    .to_tab_point(fold_point, Bias::Right);
                let wrap_point = self.wrap_snapshot.tab_point_to_wrap_point(tab_point);

                // Add isomorphic transform before block
                if wrap_point.row > current_row {
                    let summary = self
                        .wrap_snapshot
                        .text_summary_for_range(current_row..wrap_point.row);
                    push_isomorphic(&mut transforms, summary);
                    current_row = wrap_point.row;
                }

                // Add block transform
                if let Some(height) = block.height {
                    transforms.push(Transform::block(height));
                }
            }

            // Add final isomorphic transform
            if current_row <= max_wrap_row {
                let summary = self
                    .wrap_snapshot
                    .text_summary_for_range(current_row..max_wrap_row + 1);
                if !summary.lines.is_zero() {
                    push_isomorphic(&mut transforms, summary);
                }
            }
        }

        self.transforms = SumTree::from_iter(transforms, ());
    }
}

/// Immutable snapshot of block state.
///
/// Cheap to clone (Arc-based WrapSnapshot). For now, this is a passthrough
/// implementation that maps WrapPoint 1:1 to BlockPoint.
///
/// Future enhancements will add actual block insertion and rendering.
#[derive(Clone)]
pub struct BlockSnapshot {
    /// Wrap snapshot providing input coordinates.
    pub wrap_snapshot: WrapSnapshot,

    /// Transform tree representing blocks and isomorphic regions.
    /// For now, starts empty (all isomorphic mapping).
    transforms: SumTree<Transform>,
}

impl BlockSnapshot {
    /// Create a new block snapshot with no blocks.
    ///
    /// Initially, this creates an empty transform tree, making all
    /// coordinate conversions passthrough (WrapPoint == BlockPoint).
    pub fn new(wrap_snapshot: WrapSnapshot) -> Self {
        Self {
            wrap_snapshot,
            transforms: SumTree::new(()),
        }
    }

    /// Get the maximum BlockPoint in this snapshot.
    pub fn max_point(&self) -> BlockPoint {
        let lines = &self.transforms.summary().output.lines;
        BlockPoint {
            row: lines.row,
            column: lines.column,
        }
    }

    /// Convert WrapPoint to BlockPoint.
    ///
    /// With empty transforms, this is currently a passthrough conversion.
    pub fn wrap_point_to_block_point(&self, wrap_point: WrapPoint) -> BlockPoint {
        if self.transforms.is_empty() {
            return BlockPoint {
                row: wrap_point.row,
                column: wrap_point.column,
            };
        }

        let mut cursor = self
            .transforms
            .cursor::<sum_tree::Dimensions<WrapPoint, BlockPoint>>(());
        cursor.seek(&wrap_point, Bias::Right);

        let wrap_start = cursor.start().0;
        let block_start = cursor.start().1;

        // Calculate row/column offset
        if wrap_point.row > wrap_start.row {
            BlockPoint {
                row: block_start.row + (wrap_point.row - wrap_start.row),
                column: wrap_point.column,
            }
        } else {
            BlockPoint {
                row: block_start.row,
                column: block_start.column + (wrap_point.column - wrap_start.column),
            }
        }
    }

    /// Convert BlockPoint to WrapPoint.
    ///
    /// With empty transforms, this is currently a passthrough conversion.
    /// If the block point is inside a block transform, clamps to the
    /// position before the block.
    pub fn to_wrap_point(&self, block_point: BlockPoint) -> WrapPoint {
        if self.transforms.is_empty() {
            return WrapPoint {
                row: block_point.row,
                column: block_point.column,
            };
        }

        let mut cursor = self
            .transforms
            .cursor::<sum_tree::Dimensions<BlockPoint, WrapPoint>>(());
        cursor.seek(&block_point, Bias::Right);

        let block_start = cursor.start().0;
        let wrap_start = cursor.start().1;

        if cursor.item().is_some_and(|t| t.is_isomorphic()) {
            // Isomorphic - calculate offset
            if block_point.row > block_start.row {
                WrapPoint {
                    row: wrap_start.row + (block_point.row - block_start.row),
                    column: block_point.column,
                }
            } else {
                WrapPoint {
                    row: wrap_start.row,
                    column: wrap_start.column + (block_point.column - block_start.column),
                }
            }
        } else {
            // Block transform - return start position
            wrap_start
        }
    }

    /// Get text summary for a range of block rows.
    pub fn text_summary_for_range(&self, rows: Range<u32>) -> TextSummary {
        let mut summary = TextSummary::default();

        let start = BlockPoint {
            row: rows.start,
            column: 0,
        };
        let end = BlockPoint {
            row: rows.end,
            column: 0,
        };

        let mut cursor = self
            .transforms
            .cursor::<sum_tree::Dimensions<BlockPoint, WrapPoint>>(());
        cursor.seek(&start, Bias::Right);

        // Accumulate transforms in range
        while let Some(transform) = cursor.item() {
            let block_pos = cursor.end().0;

            if block_pos.row >= end.row {
                break;
            }

            summary += &transform.summary.output;
            cursor.next();
        }

        summary
    }

    /// Iterate through text chunks for a range of display rows with highlight information.
    ///
    /// This is a simplified reference implementation that demonstrates text iteration
    /// and highlight merging. Production implementation will optimize for performance.
    pub fn chunks<'a>(
        &'a self,
        rows: Range<u32>,
        _highlights: crate::display_map::Highlights<'a>,
    ) -> BlockChunks<'a> {
        // Convert block row range to buffer coordinates
        let start_wrap_point = self.to_wrap_point(BlockPoint {
            row: rows.start,
            column: 0,
        });
        let end_wrap_point = self.to_wrap_point(BlockPoint {
            row: rows.end,
            column: 0,
        });

        // Convert wrap points to buffer points
        let buffer = self
            .wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .buffer();

        let start_tab_point = self.wrap_snapshot.to_tab_point(start_wrap_point);
        let start_fold_point = self
            .wrap_snapshot
            .tab_snapshot
            .to_fold_point(start_tab_point, Bias::Left);
        let start_inlay_point = self
            .wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .to_inlay_point(start_fold_point);
        let start_buffer_point = self
            .wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .to_point(start_inlay_point, Bias::Left);

        let end_tab_point = self.wrap_snapshot.to_tab_point(end_wrap_point);
        let end_fold_point = self
            .wrap_snapshot
            .tab_snapshot
            .to_fold_point(end_tab_point, Bias::Right);
        let end_inlay_point = self
            .wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .to_inlay_point(end_fold_point);
        let end_buffer_point = self
            .wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .to_point(end_inlay_point, Bias::Right);

        let start_offset = buffer.point_to_offset(start_buffer_point);
        let end_offset = buffer.point_to_offset(end_buffer_point);

        BlockChunks {
            snapshot: self,
            start_offset,
            end_offset,
            yielded: false,
        }
    }
}

/// Iterator over text chunks with highlight information.
///
/// This is a simplified reference implementation. Production version will be
/// more sophisticated with incremental chunk yielding and proper highlight merging.
pub struct BlockChunks<'a> {
    snapshot: &'a BlockSnapshot,
    start_offset: usize,
    end_offset: usize,
    yielded: bool,
}

impl<'a> Iterator for BlockChunks<'a> {
    type Item = crate::display_map::Chunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.yielded {
            return None;
        }

        self.yielded = true;

        let buffer = self
            .snapshot
            .wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .buffer();

        // Collect text chunks into a single string
        // FIXME: This is inefficient - production should yield chunks incrementally
        let text: String = buffer
            .text_for_range(self.start_offset..self.end_offset)
            .collect();

        // Leak the string to get a 'static reference, then cast to 'a
        // FIXME: This leaks memory - production should use proper arena allocation
        let leaked: &'static str = Box::leak(text.into_boxed_str());
        let text_ref: &'a str = unsafe { std::mem::transmute(leaked) };

        Some(crate::display_map::Chunk {
            text: text_ref,
            highlight_style: None,
            syntax_highlight_id: None,
            diagnostic_severity: None,
            is_tab: false,
            is_inlay: false,
            is_unnecessary: false,
            underline: false,
        })
    }
}

#[cfg(test)]
mod tests_block_snapshot {
    use super::*;
    use crate::{fold_map::FoldSnapshot, inlay_map::InlaySnapshot, tab_map::TabSnapshot};
    use std::num::NonZeroU64;
    use text::{Buffer, BufferId};

    fn create_buffer(text: &str) -> text::BufferSnapshot {
        let buffer = Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text);
        buffer.snapshot()
    }

    fn build_block_snapshot(text: &str, tab_width: u32) -> BlockSnapshot {
        let buffer = create_buffer(text);
        let inlay_snapshot = InlaySnapshot::new(buffer);
        let fold_snapshot = FoldSnapshot::new(inlay_snapshot);
        let tab_snapshot = TabSnapshot::new(fold_snapshot, tab_width);
        let wrap_snapshot = WrapSnapshot::new(tab_snapshot);
        BlockSnapshot::new(wrap_snapshot)
    }

    #[test]
    fn empty_block_snapshot() {
        let snapshot = build_block_snapshot("", 4);

        assert_eq!(snapshot.transforms.summary().input, TextSummary::default());
        assert_eq!(snapshot.transforms.summary().output, TextSummary::default());
    }

    #[test]
    fn block_snapshot_passthrough() {
        let snapshot = build_block_snapshot("hello world", 4);

        // Empty transforms means passthrough (1:1 mapping)
        let wrap_point = WrapPoint { row: 0, column: 5 };
        let block_point = snapshot.wrap_point_to_block_point(wrap_point);

        assert_eq!(block_point, BlockPoint { row: 0, column: 5 });
    }

    #[test]
    fn block_snapshot_roundtrip() {
        let snapshot = build_block_snapshot("hello\nworld", 4);

        // Test roundtrip: WrapPoint -> BlockPoint -> WrapPoint
        let original = WrapPoint { row: 1, column: 3 };
        let block_point = snapshot.wrap_point_to_block_point(original);
        let roundtrip = snapshot.to_wrap_point(block_point);

        assert_eq!(roundtrip, original);
    }

    #[test]
    fn max_point_empty() {
        let snapshot = build_block_snapshot("", 4);
        let max = snapshot.max_point();

        assert_eq!(max, BlockPoint { row: 0, column: 0 });
    }

    #[test]
    fn text_summary_for_range_empty() {
        let snapshot = build_block_snapshot("line 1\nline 2\nline 3", 4);
        let summary = snapshot.text_summary_for_range(0..0);

        assert_eq!(summary, TextSummary::default());
    }

    #[test]
    fn chunks_basic() {
        let snapshot = build_block_snapshot("hello\nworld", 4);
        let highlights = crate::display_map::Highlights::default();

        let chunks: Vec<_> = snapshot.chunks(0..2, highlights).collect();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello\nworld");
        assert!(!chunks[0].is_tab);
        assert!(!chunks[0].is_inlay);
    }

    #[test]
    fn chunks_single_line() {
        let snapshot = build_block_snapshot("hello world", 4);
        let highlights = crate::display_map::Highlights::default();

        let chunks: Vec<_> = snapshot.chunks(0..1, highlights).collect();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
    }

    #[test]
    fn chunks_empty_range() {
        let snapshot = build_block_snapshot("hello\nworld", 4);
        let highlights = crate::display_map::Highlights::default();

        let chunks: Vec<_> = snapshot.chunks(0..0, highlights).collect();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "");
    }

    #[test]
    fn chunks_multi_line() {
        let snapshot = build_block_snapshot("line 1\nline 2\nline 3\nline 4", 4);
        let highlights = crate::display_map::Highlights::default();

        let chunks: Vec<_> = snapshot.chunks(0..4, highlights).collect();

        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("line 1"));
        assert!(chunks[0].text.contains("line 2"));
        assert!(chunks[0].text.contains("line 3"));
        assert!(chunks[0].text.contains("line 4"));
    }
}

#[cfg(test)]
mod tests_block_map {
    use super::*;
    use crate::{
        fold_map::FoldSnapshot, inlay_map::InlaySnapshot, tab_map::TabSnapshot,
        wrap_map::WrapSnapshot,
    };
    use std::num::NonZeroU64;
    use text::{Buffer, BufferId};

    fn create_buffer(text: &str) -> text::BufferSnapshot {
        let buffer = Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text);
        buffer.snapshot()
    }

    fn build_wrap_snapshot(text: &str, tab_width: u32) -> WrapSnapshot {
        let buffer = create_buffer(text);
        let inlay_snapshot = InlaySnapshot::new(buffer);
        let fold_snapshot = FoldSnapshot::new(inlay_snapshot);
        let tab_snapshot = TabSnapshot::new(fold_snapshot, tab_width);
        WrapSnapshot::new(tab_snapshot)
    }

    #[test]
    fn empty_block_map() {
        let wrap_snapshot = build_wrap_snapshot("hello world", 4);
        let block_map = BlockMap::new(wrap_snapshot);

        assert_eq!(block_map.custom_blocks.len(), 0);
        assert_eq!(block_map.custom_blocks_by_id.len(), 0);
    }

    #[test]
    fn insert_single_block() {
        let wrap_snapshot = build_wrap_snapshot("line 1\nline 2\nline 3", 4);
        let buffer = wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .buffer()
            .clone();
        let mut block_map = BlockMap::new(wrap_snapshot);

        // Insert a block above line 2
        let anchor = buffer.anchor_before(7); // Start of line 2
        let block = BlockProperties {
            placement: BlockPlacement::Above(anchor),
            height: Some(2),
            style: BlockStyle::Fixed,
            priority: 0,
        };

        let ids = block_map.insert([block]);

        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], CustomBlockId(0));
        assert_eq!(block_map.custom_blocks.len(), 1);
        assert_eq!(block_map.custom_blocks_by_id.len(), 1);
    }

    #[test]
    fn insert_multiple_blocks() {
        let wrap_snapshot = build_wrap_snapshot("line 1\nline 2\nline 3", 4);
        let buffer = wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .buffer()
            .clone();
        let mut block_map = BlockMap::new(wrap_snapshot);

        let anchor1 = buffer.anchor_before(0);
        let anchor2 = buffer.anchor_before(7);

        let blocks = vec![
            BlockProperties {
                placement: BlockPlacement::Above(anchor1),
                height: Some(1),
                style: BlockStyle::Fixed,
                priority: 0,
            },
            BlockProperties {
                placement: BlockPlacement::Below(anchor2),
                height: Some(3),
                style: BlockStyle::Sticky,
                priority: 1,
            },
        ];

        let ids = block_map.insert(blocks);

        assert_eq!(ids.len(), 2);
        assert_eq!(block_map.custom_blocks.len(), 2);
    }

    #[test]
    fn remove_block() {
        let wrap_snapshot = build_wrap_snapshot("line 1\nline 2", 4);
        let buffer = wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .buffer()
            .clone();
        let mut block_map = BlockMap::new(wrap_snapshot);

        let anchor = buffer.anchor_before(0);
        let block = BlockProperties {
            placement: BlockPlacement::Above(anchor),
            height: Some(2),
            style: BlockStyle::Fixed,
            priority: 0,
        };

        let ids = block_map.insert([block]);
        assert_eq!(block_map.custom_blocks.len(), 1);

        block_map.remove(&ids);
        assert_eq!(block_map.custom_blocks.len(), 0);
        assert_eq!(block_map.custom_blocks_by_id.len(), 0);
    }

    #[test]
    fn resize_block() {
        let wrap_snapshot = build_wrap_snapshot("line 1\nline 2", 4);
        let buffer = wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .buffer()
            .clone();
        let mut block_map = BlockMap::new(wrap_snapshot);

        let anchor = buffer.anchor_before(0);
        let block = BlockProperties {
            placement: BlockPlacement::Above(anchor),
            height: Some(2),
            style: BlockStyle::Fixed,
            priority: 0,
        };

        let ids = block_map.insert([block]);
        let block_id = ids[0];

        // Check original height
        assert_eq!(block_map.custom_blocks[0].height, Some(2));

        // Resize
        let mut heights = HashMap::new();
        heights.insert(block_id, 5);
        block_map.resize(heights);

        // Check new height
        assert_eq!(block_map.custom_blocks[0].height, Some(5));
    }

    #[test]
    fn snapshot_after_insert() {
        let wrap_snapshot = build_wrap_snapshot("line 1\nline 2", 4);
        let buffer = wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .buffer()
            .clone();
        let mut block_map = BlockMap::new(wrap_snapshot);

        let anchor = buffer.anchor_before(0);
        let block = BlockProperties {
            placement: BlockPlacement::Above(anchor),
            height: Some(3),
            style: BlockStyle::Fixed,
            priority: 0,
        };

        block_map.insert([block]);
        let snapshot = block_map.snapshot();

        // Snapshot should have non-empty transforms
        assert!(!snapshot.transforms.is_empty());
    }

    #[test]
    fn block_placement_ordering() {
        // Test that blocks are sorted correctly by placement
        let wrap_snapshot = build_wrap_snapshot("line 1\nline 2\nline 3", 4);
        let buffer = wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .buffer()
            .clone();
        let mut block_map = BlockMap::new(wrap_snapshot);

        let anchor1 = buffer.anchor_before(0); // line 1
        let anchor2 = buffer.anchor_before(7); // line 2
        let anchor3 = buffer.anchor_before(14); // line 3

        // Insert in reverse order
        let blocks = vec![
            BlockProperties {
                placement: BlockPlacement::Above(anchor3),
                height: Some(1),
                style: BlockStyle::Fixed,
                priority: 0,
            },
            BlockProperties {
                placement: BlockPlacement::Above(anchor1),
                height: Some(1),
                style: BlockStyle::Fixed,
                priority: 0,
            },
            BlockProperties {
                placement: BlockPlacement::Above(anchor2),
                height: Some(1),
                style: BlockStyle::Fixed,
                priority: 0,
            },
        ];

        block_map.insert(blocks);

        // Blocks should be sorted by buffer position
        let first_offset = block_map.custom_blocks[0]
            .placement
            .start()
            .to_offset(&buffer);
        let second_offset = block_map.custom_blocks[1]
            .placement
            .start()
            .to_offset(&buffer);
        let third_offset = block_map.custom_blocks[2]
            .placement
            .start()
            .to_offset(&buffer);

        assert!(first_offset <= second_offset);
        assert!(second_offset <= third_offset);
    }
}
