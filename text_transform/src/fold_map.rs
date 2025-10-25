//! FoldMap v2: Transform-based coordinate transformation for code folding.
//!
//! This implementation uses the Transform pattern with dual `SumTree` architecture:
//! - `transforms: SumTree<Transform>` - For coordinate conversion (InlayPoint -> FoldPoint)
//! - `folds: SumTree<Fold>` - For fold metadata and rendering
//!
//! # Transform Architecture
//!
//! Unlike InlayMap which uses an enum, FoldMap uses a struct with optional placeholder:
//! ```rust
//! struct Transform {
//!     summary: TransformSummary,
//!     placeholder: Option<TransformPlaceholder>,  // None = isomorphic, Some = fold
//! }
//! ```
//!
//! This design allows efficient merging of adjacent isomorphic regions while
//! maintaining fold-specific data separately.
//!
//! # Coordinate Transformation
//!
//! Folds **hide rows** from display:
//! ```text
//! InlayPoint (input):    FoldPoint (output):
//! Row 0: fn example() {  Row 0: fn example() { ... }
//! Row 1:     line 1      (hidden)
//! Row 2:     line 2      (hidden)
//! Row 3: }               (hidden)
//! Row 4: fn another()    Row 1: fn another()
//! ```
//!
//! # Dual SumTree Pattern
//!
//! - **transforms**: Efficient coordinate conversion via cursor seeking
//! - **folds**: Query folds by Anchor range, ID lookup via TreeMap
//!
//! # Related
//!
//! - [`crate::transform`]: Base Transform pattern infrastructure
//! - [`FoldPoint`](crate::FoldPoint): Output coordinate type
//! - [`text::Anchor`]: Stable positioning through edits

use crate::{
    coords::{FoldPoint, InlayPoint},
    dimensions::{FoldOffset, InlayOffset},
    inlay_map::InlaySnapshot,
};
use std::{
    any::TypeId,
    cmp,
    ops::{Deref, DerefMut, Range},
    sync::Arc,
};
use sum_tree::{Bias, Dimensions, Item, SumTree, TreeMap};
use text::{Anchor, Edit, Point, TextSummary, ToOffset};

/// Unique identifier for a fold.
///
/// FoldIds are monotonically increasing and assigned when folds are created.
/// Used for O(1) lookup in the metadata TreeMap.
#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq, Hash)]
pub struct FoldId(pub usize);

/// Anchor-based range for a fold.
///
/// Using Anchors instead of Points ensures folds remain stable through buffer edits.
/// When text is inserted before/inside a fold, the Anchors automatically adjust.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FoldRange(pub Range<Anchor>);

impl Deref for FoldRange {
    type Target = Range<Anchor>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for FoldRange {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Default for FoldRange {
    fn default() -> Self {
        // FIXME: Default range uses MIN/MAX anchors but can't construct without buffer
        // This will be unused in practice as Folds are created from actual ranges
        Self(Anchor::MIN..Anchor::MAX)
    }
}

/// Placeholder configuration for rendering a fold.
///
/// FoldPlaceholder defines how a folded region appears in the editor,
/// including custom rendering callbacks and merge behavior.
#[derive(Clone)]
pub struct FoldPlaceholder {
    /// Rendering callback producing the visual element for this fold.
    ///
    /// Takes the fold ID, Anchor range, and app context to generate the displayed element.
    /// For example, this might render "..." for code folds or "[+]" for collapsed sections.
    ///
    /// NOTE: Using a simple function signature for now. Full GPUI integration requires
    /// `AnyElement` and `App` types which we'll integrate later.
    pub render: Arc<dyn Send + Sync + Fn(FoldId, Range<Anchor>) -> String>,

    /// If true, constrain the rendered element to a fixed width (typically ellipsis width).
    pub constrain_width: bool,

    /// If true, adjacent folds of the same type are merged into a single fold.
    ///
    /// Useful for combining multiple consecutive folded imports or similar constructs.
    pub merge_adjacent: bool,

    /// Optional type tag for categorizing folds.
    ///
    /// Allows selective removal of folds by category (e.g., remove all "imports" folds
    /// but keep "function body" folds).
    pub type_tag: Option<TypeId>,
}

impl Default for FoldPlaceholder {
    fn default() -> Self {
        Self {
            render: Arc::new(|_, _| "...".to_string()),
            constrain_width: true,
            merge_adjacent: true,
            type_tag: None,
        }
    }
}

impl FoldPlaceholder {
    /// Create a test placeholder with default empty rendering.
    #[cfg(test)]
    pub fn test() -> Self {
        Self {
            render: Arc::new(|_, _| "...".to_string()),
            constrain_width: true,
            merge_adjacent: true,
            type_tag: None,
        }
    }
}

impl std::fmt::Debug for FoldPlaceholder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FoldPlaceholder")
            .field("constrain_width", &self.constrain_width)
            .field("merge_adjacent", &self.merge_adjacent)
            .field("type_tag", &self.type_tag)
            .finish()
    }
}

impl Eq for FoldPlaceholder {}

impl PartialEq for FoldPlaceholder {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.render, &other.render)
            && self.constrain_width == other.constrain_width
            && self.merge_adjacent == other.merge_adjacent
            && self.type_tag == other.type_tag
    }
}

/// A fold region in the buffer.
///
/// Folds hide a contiguous range of text, replacing it with a placeholder element.
/// The range is specified using stable Anchors that track through buffer edits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fold {
    /// Unique identifier for this fold.
    pub id: FoldId,

    /// Anchor-based range being folded.
    ///
    /// The range is inclusive of start and exclusive of end, following Rust conventions.
    pub range: FoldRange,

    /// Placeholder configuration for rendering this fold.
    pub placeholder: FoldPlaceholder,
}

impl Fold {
    /// Create a new fold with the given ID, range, and placeholder.
    pub fn new(id: FoldId, range: Range<Anchor>, placeholder: FoldPlaceholder) -> Self {
        Self {
            id,
            range: FoldRange(range),
            placeholder,
        }
    }
}

/// Summary for a fold subtree.
///
/// Tracks the range spanned by folds in a SumTree node for efficient querying.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FoldSummary {
    /// Start anchor of the first fold in this subtree.
    pub start: Anchor,

    /// End anchor of the last fold in this subtree.
    pub end: Anchor,

    /// Minimum start anchor across all folds in subtree (for range queries).
    pub min_start: Anchor,

    /// Maximum end anchor across all folds in subtree (for range queries).
    pub max_end: Anchor,

    /// Number of folds in this subtree.
    pub count: usize,
}

impl sum_tree::Summary for FoldSummary {
    type Context<'a> = &'a text::BufferSnapshot;

    fn zero(_cx: &text::BufferSnapshot) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, other: &Self, buffer: &text::BufferSnapshot) {
        if other.count > 0 {
            if self.count == 0 {
                *self = other.clone();
            } else {
                self.end = other.end;
                self.min_start = self.min_start.min(&other.min_start, buffer);
                self.max_end = self.max_end.max(&other.max_end, buffer);
                self.count += other.count;
            }
        }
    }
}

impl Item for Fold {
    type Summary = FoldSummary;

    fn summary(&self, _cx: &text::BufferSnapshot) -> Self::Summary {
        FoldSummary {
            start: self.range.start,
            end: self.range.end,
            min_start: self.range.start,
            max_end: self.range.end,
            count: 1,
        }
    }
}

impl<'a> sum_tree::Dimension<'a, FoldSummary> for FoldRange {
    fn zero(_cx: &text::BufferSnapshot) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a FoldSummary, _: &text::BufferSnapshot) {
        self.0.start = summary.start;
        self.0.end = summary.end;
    }
}

/// Transform representing either an isomorphic region or a fold.
///
/// This is a **struct** (not enum) where the presence of `placeholder` determines type:
/// - `placeholder == None`: Isomorphic transform (1:1 mapping)
/// - `placeholder == Some(...)`: Fold transform (hides rows)
#[derive(Clone, Debug)]
struct Transform {
    /// Aggregated summary of input/output coordinates for this transform.
    summary: TransformSummary,

    /// If present, this transform represents a fold with the given placeholder.
    /// If None, this is an isomorphic transform.
    placeholder: Option<TransformPlaceholder>,
}

impl Transform {
    /// Check if this transform represents a fold.
    fn is_fold(&self) -> bool {
        self.placeholder.is_some()
    }

    /// Create an isomorphic transform with 1:1 mapping.
    fn isomorphic(summary: TextSummary) -> Self {
        Self {
            summary: TransformSummary {
                input: summary,
                output: summary,
            },
            placeholder: None,
        }
    }
}

/// Placeholder data for a fold transform.
///
/// Stores the text and character boundaries for the fold's display representation.
///
/// FIXME: These fields are defined and populated but never used for rendering.
/// Fold placeholder text rendering is not yet implemented. When implemented,
/// these will be used to display "..." or other placeholder text for folded regions.
#[derive(Clone, Debug)]
struct TransformPlaceholder {
    /// Static text displayed for this fold (e.g., "...", "{...}", etc.).
    #[allow(dead_code)]
    text: &'static str,

    /// Bitmap representing valid character boundaries within the placeholder text.
    ///
    /// Used for cursor positioning within the fold placeholder. A bit is set if
    /// that byte offset is a valid char boundary.
    #[allow(dead_code)]
    chars: u128,
}

/// Summary aggregating coordinate information for a Transform subtree.
///
/// Tracks both input (InlayPoint) and output (FoldPoint) coordinate spaces.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TransformSummary {
    /// Input summary (InlayPoint space before folding).
    input: TextSummary,

    /// Output summary (FoldPoint space after folding).
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

/// Metadata for a fold stored separately from the transform tree.
///
/// Enables O(1) lookup of fold information by FoldId.
#[derive(Clone, Debug)]
/// FIXME: FoldMetadata is stored in fold_metadata_by_id for O(1) lookup,
/// but the range field is never queried. This suggests incomplete implementation
/// of fold range queries. Either implement the queries or remove the metadata entirely.
struct FoldMetadata {
    /// Anchor range of the fold.
    #[allow(dead_code)]
    range: FoldRange,
}

/// Immutable snapshot of the fold state.
///
/// Uses dual SumTree pattern:
/// - `transforms` for efficient coordinate conversion
/// - `folds` for querying by Anchor range
#[derive(Clone)]
pub struct FoldSnapshot {
    /// Transform tree for coordinate conversion (InlayPoint -> FoldPoint).
    transforms: SumTree<Transform>,

    /// Fold tree for querying folds by range.
    ///
    /// NOTE: Item impl requires buffer context - will implement once we have
    /// BufferSnapshot integration.
    folds: SumTree<Fold>,

    /// O(1) lookup of fold metadata by ID.
    fold_metadata_by_id: TreeMap<FoldId, FoldMetadata>,

    /// Underlying inlay snapshot providing input coordinates.
    pub inlay_snapshot: InlaySnapshot,

    /// Version counter for change tracking.
    pub version: usize,
}

impl FoldSnapshot {
    /// Create a new empty fold snapshot with no folds.
    pub fn new(inlay_snapshot: InlaySnapshot) -> Self {
        // Create initial isomorphic transform spanning entire inlay space
        let summary = inlay_snapshot.buffer().text_summary();
        let transforms = SumTree::from_iter([Transform::isomorphic(summary)], ());

        // Create empty folds tree with buffer context
        let buffer = inlay_snapshot.buffer();
        let folds = SumTree::new(buffer);

        Self {
            transforms,
            folds,
            fold_metadata_by_id: TreeMap::default(),
            inlay_snapshot,
            version: 0,
        }
    }

    /// Get the underlying buffer snapshot.
    pub fn buffer(&self) -> &text::BufferSnapshot {
        self.inlay_snapshot.buffer()
    }

    /// Get the version number of this snapshot.
    pub fn version(&self) -> usize {
        self.version
    }

    /// Convert InlayPoint to FoldPoint.
    ///
    /// When the point lands inside a fold, Bias determines the result:
    /// - `Bias::Left`: Returns the start of the fold (before the fold)
    /// - `Bias::Right`: Returns the end of the fold (after the fold)
    ///
    /// For points not inside folds, coordinates map 1:1 (identity transform).
    pub fn to_fold_point(&self, point: InlayPoint, bias: Bias) -> FoldPoint {
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InlayPoint, FoldPoint>>(());
        cursor.seek(&point, Bias::Right);

        if cursor.item().is_some_and(|t| t.is_fold()) {
            // Point landed inside a fold
            if bias == Bias::Left || point == cursor.start().0 {
                cursor.start().1
            } else {
                cursor.end().1
            }
        } else {
            // Point is in isomorphic region - compute overshoot from cursor start
            // Use Point arithmetic to handle row/column correctly.
            //
            // SAFETY: Point arithmetic is safe here because:
            // 1. This calculation is local to a single Transform within the SumTree
            // 2. The entire tree is rebuilt from stable Anchors on each sync
            // 3. We're computing relative position (overshoot) within one transform
            // 4. Result is converted to FoldPoint (ephemeral coordinate type)
            let start_inlay = Point::new(cursor.start().0.row, cursor.start().0.column);
            let target = Point::new(point.row, point.column);
            let overshoot = target - start_inlay;

            let start_fold = Point::new(cursor.start().1.row, cursor.start().1.column);
            let end_fold = Point::new(cursor.end().1.row, cursor.end().1.column);
            let result = start_fold + overshoot;

            // Clamp to transform end
            let clamped = if result > end_fold { end_fold } else { result };

            FoldPoint {
                row: clamped.row,
                column: clamped.column,
            }
        }
    }

    /// Convert FoldPoint to InlayPoint.
    ///
    /// This is the inverse of `to_fold_point`. Since folds hide regions,
    /// a FoldPoint maps to the start of the folded range in InlayPoint space.
    pub fn to_inlay_point(&self, point: FoldPoint) -> InlayPoint {
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<FoldPoint, InlayPoint>>(());
        cursor.seek(&point, Bias::Right);

        // Use Point arithmetic to handle row/column correctly.
        //
        // SAFETY: Point arithmetic is safe here because:
        // 1. Transform tree is rebuilt from Anchors on each sync (see sync() method)
        // 2. We're calculating relative offset within a single Transform
        // 3. This is reverse conversion - InlayPoint is ephemeral coordinate type
        // 4. Anchor stability maintained through fold.range: Range<Anchor>
        let start_fold = Point::new(cursor.start().0.row, cursor.start().0.column);
        let target = Point::new(point.row, point.column);
        let overshoot = target - start_fold;

        let start_inlay = Point::new(cursor.start().1.row, cursor.start().1.column);
        let result = start_inlay + overshoot;

        InlayPoint {
            row: result.row,
            column: result.column,
        }
    }

    /// Get the total length in FoldOffset (byte offset after folding).
    pub fn len(&self) -> FoldOffset {
        FoldOffset(self.transforms.summary().output.len)
    }

    /// Get the maximum FoldPoint in the snapshot.
    pub fn max_point(&self) -> FoldPoint {
        let lines = self.transforms.summary().output.lines;
        FoldPoint {
            row: lines.row,
            column: lines.column,
        }
    }

    /// Convert FoldOffset (byte offset in fold coordinate space) to FoldPoint.
    ///
    /// Traverses the transform tree to find the position corresponding to the given byte offset,
    /// accounting for both isomorphic regions and folds.
    pub fn offset_to_fold_point(&self, offset: FoldOffset) -> FoldPoint {
        let target_bytes = offset.0;
        let mut cursor = self.transforms.cursor::<FoldPoint>(());
        cursor.seek(&FoldPoint::default(), Bias::Left);

        let mut accumulated_bytes = 0usize;
        let mut result_point = FoldPoint::default();

        // Seek through transforms until we reach or exceed target
        while let Some(transform) = cursor.item() {
            let summary = transform.summary(());
            let transform_bytes = summary.output.len;

            if accumulated_bytes + transform_bytes > target_bytes {
                // Target is within this transform
                let bytes_into_transform = target_bytes - accumulated_bytes;

                if transform.is_fold() {
                    // Within a fold - the entire fold maps to a single point
                    result_point = *cursor.start();
                } else {
                    // Isomorphic region - calculate point from bytes
                    // Use the inlay snapshot to convert bytes to points
                    let inlay_point = self.to_inlay_point(*cursor.start());
                    let inlay_offset = self.inlay_snapshot.to_inlay_offset(inlay_point);
                    let target_inlay_offset = InlayOffset(inlay_offset.0 + bytes_into_transform);
                    let target_inlay_point = self
                        .inlay_snapshot
                        .offset_to_inlay_point(target_inlay_offset);
                    result_point = self.to_fold_point(target_inlay_point, Bias::Left);
                }
                break;
            }

            accumulated_bytes += transform_bytes;
            result_point = *cursor.start();
            result_point.row += summary.output.lines.row;
            if summary.output.lines.row > 0 {
                result_point.column = summary.output.lines.column;
            } else {
                result_point.column += summary.output.lines.column;
            }
            cursor.next();
        }

        result_point
    }
}

/// Edit in InlayOffset space (input to FoldMap).
pub type InlayEdit = Edit<InlayOffset>;

/// Edit in FoldOffset space (output from FoldMap).
pub type FoldEdit = Edit<FoldOffset>;

/// Mutable fold map managing fold state.
pub struct FoldMap {
    /// Current snapshot.
    snapshot: FoldSnapshot,

    /// Next fold ID to assign.
    next_fold_id: FoldId,
}

impl FoldMap {
    /// Create a new FoldMap from an inlay snapshot.
    pub fn new(inlay_snapshot: InlaySnapshot) -> (Self, FoldSnapshot) {
        let snapshot = FoldSnapshot::new(inlay_snapshot);
        let map = Self {
            snapshot: snapshot.clone(),
            next_fold_id: FoldId(0),
        };
        (map, snapshot)
    }

    /// Get read-only access and sync with new inlay snapshot.
    pub fn read(
        &mut self,
        inlay_snapshot: InlaySnapshot,
        edits: Vec<InlayEdit>,
    ) -> (FoldSnapshot, Vec<FoldEdit>) {
        let inlay_max = inlay_snapshot.buffer().max_point();
        let edits = self.sync(inlay_snapshot, edits);
        let fold_max = self.snapshot.max_point();
        tracing::trace!(
            "FoldMap.read: inlay_max=({}, {}) -> fold_max=({}, {})",
            inlay_max.row,
            inlay_max.column,
            fold_max.row,
            fold_max.column
        );
        (self.snapshot.clone(), edits)
    }

    /// Get mutable access via FoldMapWriter.
    pub fn write(
        &mut self,
        inlay_snapshot: InlaySnapshot,
        edits: Vec<InlayEdit>,
    ) -> (FoldMapWriter<'_>, FoldSnapshot, Vec<FoldEdit>) {
        let (snapshot, edits) = self.read(inlay_snapshot, edits);
        (FoldMapWriter(self), snapshot, edits)
    }

    /// Rebuild transform tree after inlay edits.
    ///
    /// This rebuilds the entire transform tree from scratch based on current folds.
    /// Simplified implementation that assumes InlayOffset == BufferOffset (no inlays).
    fn sync(
        &mut self,
        inlay_snapshot: InlaySnapshot,
        inlay_edits: Vec<InlayEdit>,
    ) -> Vec<FoldEdit> {
        // Update inlay snapshot first
        self.snapshot.inlay_snapshot = inlay_snapshot;

        // Rebuild entire transform tree from current folds
        let buffer = self.snapshot.buffer();
        let mut new_transforms = SumTree::default();
        let mut cursor = self.snapshot.folds.cursor::<()>(buffer);
        cursor.next(); // Position cursor at first item
        let mut position = InlayOffset(0);

        while let Some(fold) = cursor.item() {
            // Get fold range as offsets (simplified: assuming InlayOffset == BufferOffset)
            let fold_start = InlayOffset(fold.range.start.to_offset(buffer));
            let fold_end = InlayOffset(fold.range.end.to_offset(buffer));

            // Add isomorphic region before this fold
            if fold_start > position {
                let summary =
                    buffer.text_summary_for_range::<TextSummary, usize>(position.0..fold_start.0);
                push_isomorphic(&mut new_transforms, summary);
            }

            // Add fold transform (hides the folded text)
            if fold_end > fold_start {
                let input_summary =
                    buffer.text_summary_for_range::<TextSummary, usize>(fold_start.0..fold_end.0);
                new_transforms.push(
                    Transform {
                        summary: TransformSummary {
                            input: input_summary,
                            output: TextSummary::default(), // Folds have zero output
                        },
                        placeholder: Some(TransformPlaceholder {
                            text: "...",
                            chars: 1,
                        }),
                    },
                    (),
                );
            }

            position = fold_end;
            cursor.next();
        }

        // Add final isomorphic region
        let total_len = buffer.len();
        if position.0 < total_len {
            let summary =
                buffer.text_summary_for_range::<TextSummary, usize>(position.0..total_len);
            push_isomorphic(&mut new_transforms, summary);
        }

        // Ensure at least one transform
        if new_transforms.is_empty() {
            push_isomorphic(&mut new_transforms, buffer.text_summary());
        }

        // Generate fold edits (simplified: just mark entire buffer as changed if we had edits)
        let fold_edits = if !inlay_edits.is_empty() {
            vec![FoldEdit {
                old: FoldOffset(0)..FoldOffset(self.snapshot.transforms.summary().output.len),
                new: FoldOffset(0)..FoldOffset(new_transforms.summary().output.len),
            }]
        } else {
            Vec::new()
        };

        // Update snapshot
        self.snapshot.transforms = new_transforms;
        self.snapshot.version += 1;

        fold_edits
    }
}

/// Mutable wrapper for adding/removing folds.
pub struct FoldMapWriter<'a>(&'a mut FoldMap);

impl FoldMapWriter<'_> {
    /// Add folds for the given ranges with placeholders.
    ///
    /// Returns the updated snapshot and fold edits describing the changes.
    pub fn fold(
        &mut self,
        ranges: impl IntoIterator<Item = (Range<Anchor>, FoldPlaceholder)>,
    ) -> (FoldSnapshot, Vec<FoldEdit>) {
        let mut folds = Vec::new();

        for (range, placeholder) in ranges {
            // Skip empty ranges
            {
                let buffer = self.0.snapshot.buffer();
                if range.start.cmp(&range.end, buffer) == cmp::Ordering::Equal {
                    continue;
                }
            }

            // Create fold with new ID
            let id = FoldId(self.0.next_fold_id.0);
            self.0.next_fold_id.0 += 1;

            folds.push(Fold::new(id, range, placeholder));
        }

        // Sort folds by range
        {
            let buffer = self.0.snapshot.buffer();
            folds.sort_unstable_by(|a, b| {
                let cmp_start = a.range.start.cmp(&b.range.start, buffer);
                if cmp_start == cmp::Ordering::Equal {
                    a.range.end.cmp(&b.range.end, buffer)
                } else {
                    cmp_start
                }
            });
        }

        // Insert metadata first
        for fold in &folds {
            self.0.snapshot.fold_metadata_by_id.insert(
                fold.id,
                FoldMetadata {
                    range: fold.range.clone(),
                },
            );
        }

        // Insert folds into tree by merging with existing folds
        self.0.snapshot.folds = {
            let buffer = self.0.snapshot.buffer();
            let mut new_tree = SumTree::new(buffer);
            let mut old_cursor = self.0.snapshot.folds.cursor::<()>(buffer);
            old_cursor.next(); // Position cursor at first item
            let mut folds_iter = folds.into_iter().peekable();

            while let Some(fold) = folds_iter.peek() {
                // Append any old folds that come before this new fold
                while let Some(old_fold) = old_cursor.item() {
                    if old_fold.range.start.cmp(&fold.range.start, buffer).is_lt() {
                        new_tree.push(old_fold.clone(), buffer);
                        old_cursor.next();
                    } else {
                        break;
                    }
                }

                // Insert the new fold (safe: we just peeked successfully)
                new_tree.push(
                    folds_iter.next().expect("iterator has item (just peeked)"),
                    buffer,
                );
            }

            // Append remaining old folds
            while let Some(old_fold) = old_cursor.item() {
                new_tree.push(old_fold.clone(), buffer);
                old_cursor.next();
            }

            new_tree
        };

        // Rebuild transforms via sync()
        let edits = self
            .0
            .sync(self.0.snapshot.inlay_snapshot.clone(), Vec::new());
        (self.0.snapshot.clone(), edits)
    }

    /// Remove folds that intersect the given ranges.
    pub fn unfold_intersecting(
        &mut self,
        ranges: impl IntoIterator<Item = Range<Anchor>>,
    ) -> (FoldSnapshot, Vec<FoldEdit>) {
        let mut fold_ids_to_remove = Vec::new();

        // Find folds to remove by iterating through all folds
        {
            let buffer = self.0.snapshot.buffer();
            let ranges_vec: Vec<_> = ranges.into_iter().collect();

            let mut cursor = self.0.snapshot.folds.cursor::<()>(buffer);
            while let Some(fold) = cursor.item() {
                // Check if fold intersects any of the ranges
                for range in &ranges_vec {
                    let fold_before_range = fold.range.end.cmp(&range.start, buffer).is_le();
                    let fold_after_range = fold.range.start.cmp(&range.end, buffer).is_ge();

                    if !fold_before_range && !fold_after_range {
                        // Fold intersects this range
                        fold_ids_to_remove.push(fold.id);
                        break;
                    }
                }
                cursor.next();
            }
        }

        // Remove duplicates
        fold_ids_to_remove.sort_unstable();
        fold_ids_to_remove.dedup();

        // Remove metadata
        for fold_id in &fold_ids_to_remove {
            self.0.snapshot.fold_metadata_by_id.remove(fold_id);
        }

        // Rebuild folds tree without removed folds
        self.0.snapshot.folds = {
            let buffer = self.0.snapshot.buffer();
            let mut new_tree = SumTree::new(buffer);
            let mut cursor = self.0.snapshot.folds.cursor::<()>(buffer);

            while let Some(fold) = cursor.item() {
                if !fold_ids_to_remove.contains(&fold.id) {
                    new_tree.push(fold.clone(), buffer);
                }
                cursor.next();
            }
            new_tree
        };

        // Rebuild transforms via sync()
        let edits = self
            .0
            .sync(self.0.snapshot.inlay_snapshot.clone(), Vec::new());
        (self.0.snapshot.clone(), edits)
    }
}

// Helper functions for sync()

/// Push an isomorphic transform, merging with the previous one if possible.
fn push_isomorphic(transforms: &mut SumTree<Transform>, summary: TextSummary) {
    let mut did_merge = false;
    transforms.update_last(
        |last| {
            if !last.is_fold() {
                last.summary.input += summary;
                last.summary.output += summary;
                did_merge = true;
            }
        },
        (),
    );
    if !did_merge {
        transforms.push(Transform::isomorphic(summary), ());
    }
}

// NOTE: Zed has consolidate_inlay_edits() and consolidate_fold_edits() functions
// to merge overlapping edits. We don't need them because our simplified implementation
// generates a single consolidated edit directly in compute_edits() (see line 633).
// Zed's multi-layer architecture can produce overlapping edits from different sources,
// but our architecture pre-consolidates edits at generation time.

// Dimension trait implementations for coordinate seeking

impl<'a> sum_tree::Dimension<'a, TransformSummary> for InlayPoint {
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

impl<'a> sum_tree::Dimension<'a, TransformSummary> for FoldPoint {
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

impl<'a> sum_tree::Dimension<'a, TransformSummary> for InlayOffset {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
        self.0 += summary.input.len;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for FoldOffset {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
        self.0 += summary.output.len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use text::{Buffer, BufferId};

    fn create_buffer(text: &str) -> text::BufferSnapshot {
        let buffer = Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text);
        buffer.snapshot()
    }

    #[test]
    fn empty_snapshot_identity_mapping() {
        let buffer_snapshot = create_buffer("Hello, world!\nThis is a test.\n");
        let inlay_snapshot = InlaySnapshot::new(buffer_snapshot);
        let fold_snapshot = FoldSnapshot::new(inlay_snapshot);

        // Test coordinate conversion at various points
        let test_points = vec![
            (0, 0),  // Start of file
            (0, 5),  // Middle of first line
            (0, 13), // End of first line
            (1, 0),  // Start of second line
            (1, 10), // Middle of second line
            (1, 16), // End of second line
        ];

        for (row, column) in test_points {
            let inlay_point = InlayPoint { row, column };
            let fold_point = fold_snapshot.to_fold_point(inlay_point, Bias::Left);
            let back = fold_snapshot.to_inlay_point(fold_point);

            // With no folds, coordinates should map 1:1
            assert_eq!(
                fold_point.row, row,
                "FoldPoint row should equal InlayPoint row"
            );
            assert_eq!(
                fold_point.column, column,
                "FoldPoint column should equal InlayPoint column"
            );
            assert_eq!(
                back, inlay_point,
                "Round-trip conversion should be identity"
            );
        }
    }

    #[test]
    fn empty_snapshot_max_point() {
        let buffer_snapshot = create_buffer("Line 1\nLine 2\nLine 3\n");
        let inlay_snapshot = InlaySnapshot::new(buffer_snapshot);
        let fold_snapshot = FoldSnapshot::new(inlay_snapshot);

        let max_point = fold_snapshot.max_point();

        // Max point should be at row 3, column 0 (after newline on line 3)
        assert_eq!(max_point.row, 3);
        assert_eq!(max_point.column, 0);
    }

    #[test]
    fn empty_snapshot_len() {
        let text = "Hello, world!";
        let buffer_snapshot = create_buffer(text);
        let inlay_snapshot = InlaySnapshot::new(buffer_snapshot);
        let fold_snapshot = FoldSnapshot::new(inlay_snapshot);

        let len = fold_snapshot.len();

        // Length should match text length (no folds = no reduction)
        assert_eq!(len.0, text.len());
    }

    #[test]
    fn single_fold_hides_lines() {
        // Create a buffer with multiple lines
        let text = "fn example() {\n    line 1\n    line 2\n}\n";
        let buffer_snapshot = create_buffer(text);
        let buffer = buffer_snapshot.clone();
        let inlay_snapshot = InlaySnapshot::new(buffer_snapshot);

        // Create FoldMap and add a fold covering lines 1-3 (the function body)
        let (mut fold_map, _initial_snapshot) = FoldMap::new(inlay_snapshot.clone());

        // Create anchors for the fold range
        // Fold lines 1 and 2 completely (leave line 0 and 3 visible)
        // Line 0: "fn example() {\n"
        // Lines 1-2: "    line 1\n    line 2\n" <- fold these
        // Line 3: "}\n"
        let fold_start = buffer.anchor_after(Point::new(1, 0)); // Start of line 1
        let fold_end = buffer.anchor_before(Point::new(3, 0)); // Before line 3

        let (snapshot, _edits) = {
            let (mut writer, _snapshot, _edits) = fold_map.write(inlay_snapshot, Vec::new());
            writer.fold(vec![(fold_start..fold_end, FoldPlaceholder::test())])
        };

        // Point before the fold (on line 0) should map 1:1
        let before_fold = InlayPoint { row: 0, column: 10 };
        let fold_point_before = snapshot.to_fold_point(before_fold, Bias::Left);
        assert_eq!(fold_point_before.row, 0);
        assert_eq!(fold_point_before.column, 10);

        // Point at start of line 1 (which is folded) should map to fold start
        let in_fold = InlayPoint { row: 1, column: 5 };
        let fold_point_in = snapshot.to_fold_point(in_fold, Bias::Left);
        // Should map to end of line 0 (where fold starts)
        assert_eq!(
            fold_point_in.row, 1,
            "Point in fold should map to fold position"
        );

        // Point after the fold (line 3) should have reduced row number
        // Original row 3 should become row 1 after folding rows 1-2
        // (row 0 stays, rows 1-2 folded, row 3 becomes row 1)
        let after_fold = InlayPoint { row: 3, column: 0 };
        let fold_point_after = snapshot.to_fold_point(after_fold, Bias::Left);
        assert_eq!(
            fold_point_after.row, 1,
            "Row 3 should become row 1 after folding rows 1-2"
        );
        assert_eq!(fold_point_after.column, 0);

        // Round-trip should work
        let back = snapshot.to_inlay_point(fold_point_after);
        assert_eq!(back, after_fold);
    }
}
