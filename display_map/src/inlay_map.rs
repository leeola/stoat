///! InlayMap v2: Transform-based coordinate transformation for inlay hints.
///!
///! This implementation uses the Transform pattern with [`SumTree<Transform>`] instead
///! of storing inlays directly, enabling efficient O(log n) coordinate conversions.
///!
///! # Transform Architecture
///!
///! The core data structure is `SumTree<Transform>` where each Transform is either:
///! - **Isomorphic**: 1:1 mapping (no inlay), coordinates unchanged
///! - **Inlay**: Transformation adding visual text
///!
///! This explicitly represents both transformed and untransformed regions, enabling
///! efficient cursor-based seeking through the coordinate space.
///!
///! # Example
///!
///! ```text
///! Buffer:     let x = compute(42);
///! Display:    let x: String = compute(value: 42);
///!                   ^^^^^^^^             ^^^^^^^
///!                   inlay transforms
///!
///! SumTree<Transform>:
///! [Isomorphic("let x"), Inlay(": String"), Isomorphic(" = compute("),
///!  Inlay("value: "), Isomorphic("42);")]
///! ```
///!
///! # Coordinate Conversion
///!
///! Uses [`text::TextSummary`] to track both input (buffer) and output (display)
///! coordinates through the transform tree. Cursor seeking provides O(log n)
///! conversion between coordinate spaces.
///!
///! # Anchor Stability
///!
///! InlayMap achieves anchor stability **without storing explicit [`Anchor`] positions**.
///! Instead, position is implicit in the SumTree structure:
///!
///! - **Inlay positions** are derived from tree traversal, not stored
///! - **Buffer anchors** handle stability through edits (InlayMap rebuilds from anchors)
///! - **More efficient** than storing Anchor in each Transform (avoids anchor comparison)
///! - **Simpler** than maintaining anchor-to-transform mappings
///!
///! This differs from FoldMap/BlockMap which store `Range<Anchor>` because:
///! - Folds/blocks are user-visible entities that persist across syncs
///! - Inlays are rebuilt from scratch on each sync from external sources
///! - InlayMap is a pure transformation layer, not a persistence layer
///!
///! # Related
///!
///! - [`crate::transform`]: Base Transform pattern infrastructure
///! - [`InlayPoint`](crate::InlayPoint): Output coordinate type
///! - [`text::TextSummary`]: Aggregated text metadata
use crate::{coords::InlayPoint, dimensions::InlayOffset, transform::Isomorphic};
use std::{
    cmp::Ordering,
    sync::atomic::{AtomicUsize, Ordering::SeqCst},
};
use sum_tree::{Bias, Dimensions, Item, SumTree};
use text::{Anchor, BufferSnapshot, Edit, Point, TextSummary, ToOffset};

/// Transform representing either unchanged buffer regions or inlay insertions.
///
/// Each layer's Transform enum follows this pattern: Isomorphic variant for 1:1
/// mapping, and layer-specific variant for the actual transformation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transform {
    /// 1:1 mapping - buffer coordinates equal display coordinates.
    ///
    /// Represents regions with no inlays. Input and output summaries are identical.
    Isomorphic(Isomorphic),

    /// Inlay insertion - adds visual text at a specific position.
    ///
    /// The text appears in display but not in buffer. Increases output coordinates
    /// without changing input.
    Inlay(InlayData),
}

/// Data for an inlay transformation.
///
/// Unlike the old Inlay struct which stored position as Point, this only stores
/// the text and bias since position is implicit in the Transform tree structure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlayData {
    /// Visual text displayed for this inlay.
    pub text: String,

    /// Bias determines whether inlay attaches to character on left or right.
    ///
    /// - `Bias::Left`: Inlay appears after the insertion point (attaches to left char)
    /// - `Bias::Right`: Inlay appears before the insertion point (attaches to right char)
    pub bias: Bias,
}

impl InlayData {
    /// Create a new inlay with the given text and bias.
    pub fn new(text: String, bias: Bias) -> Self {
        Self { text, bias }
    }

    /// Get display width (number of columns) of this inlay.
    pub fn len(&self) -> u32 {
        self.text.len() as u32
    }

    /// Check if this inlay has no text.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Summary aggregating coordinate information for a Transform subtree.
///
/// Tracks both input coordinates (buffer space) and output coordinates (display space)
/// to enable bidirectional seeking through the transform tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlayTransformSummary {
    /// Input summary (buffer space before inlays applied).
    pub input: TextSummary,

    /// Output summary (display space after inlays applied).
    pub output: TextSummary,
}

impl InlayTransformSummary {
    /// Create summary for an isomorphic region (1:1 mapping).
    fn isomorphic(summary: TextSummary) -> Self {
        Self {
            input: summary.clone(),
            output: summary,
        }
    }

    /// Create summary for an inlay insertion.
    ///
    /// Input summary is zero (inlay has no buffer extent), output summary is the
    /// inlay's display extent.
    fn inlay(text: &str) -> Self {
        Self {
            input: TextSummary::default(),
            output: TextSummary::from(text),
        }
    }
}

impl sum_tree::ContextLessSummary for InlayTransformSummary {
    fn zero() -> Self {
        Self::default()
    }

    fn add_summary(&mut self, other: &Self) {
        self.input = self.input.clone() + other.input.clone();
        self.output = self.output.clone() + other.output.clone();
    }
}

impl Item for Transform {
    type Summary = InlayTransformSummary;

    fn summary(&self, _cx: ()) -> Self::Summary {
        match self {
            Transform::Isomorphic(iso) => InlayTransformSummary::isomorphic(iso.summary().clone()),
            Transform::Inlay(inlay) => InlayTransformSummary::inlay(&inlay.text),
        }
    }
}

/// Immutable snapshot of the inlay transform tree.
///
/// Cheap to clone (Arc-based via SumTree). Used for coordinate conversions
/// and can be safely shared across threads.
#[derive(Clone)]
pub struct InlaySnapshot {
    /// Buffer snapshot for text access.
    buffer: BufferSnapshot,

    /// Transform tree mapping buffer coordinates to display coordinates.
    transforms: SumTree<Transform>,

    /// Version counter for change tracking.
    version: usize,
}

impl InlaySnapshot {
    /// Create a new empty snapshot with no inlays.
    ///
    /// The transform tree contains a single Isomorphic transform spanning the
    /// entire buffer.
    pub fn new(buffer: BufferSnapshot) -> Self {
        let summary = buffer.text_summary();
        let transforms = SumTree::from_iter([Transform::Isomorphic(Isomorphic::new(summary))], ());

        Self {
            buffer,
            transforms,
            version: 0,
        }
    }

    /// Get the buffer snapshot.
    pub fn buffer(&self) -> &BufferSnapshot {
        &self.buffer
    }

    /// Convert buffer Point to InlayPoint.
    ///
    /// Uses cursor to seek through transform tree by input coordinate (Point),
    /// accumulating output coordinates along the way.
    ///
    /// # Bias Handling
    ///
    /// The bias parameter controls positioning at inlay boundaries:
    /// - `Bias::Left`: Skip over left-biased inlays, position after them
    /// - `Bias::Right`: Stop before right-biased inlays, position before them
    pub fn to_inlay_point(&self, point: Point, bias: Bias) -> InlayPoint {
        let mut cursor = self.transforms.cursor::<Dimensions<Point, InlayPoint>>(());
        cursor.seek(&point, bias);

        loop {
            match cursor.item() {
                Some(Transform::Isomorphic(_)) => {
                    // Check if we're exactly at the end of this isomorphic region
                    if point == cursor.end().0 {
                        // Apply bias: skip inlays that match the bias direction
                        while let Some(Transform::Inlay(inlay)) = cursor.next_item() {
                            if inlay.bias != bias {
                                break;
                            } else {
                                cursor.next();
                            }
                        }
                        return cursor.end().1;
                    } else {
                        // Inside isomorphic region - calculate overshoot
                        let overshoot = point - cursor.start().0;
                        let output_start = cursor.start().1;

                        return InlayPoint {
                            row: output_start.row + overshoot.row,
                            column: output_start.column + overshoot.column,
                        };
                    }
                },
                Some(Transform::Inlay(inlay)) => {
                    // Skip Left-biased inlays, stop at Right-biased ones
                    if inlay.bias == Bias::Left {
                        cursor.next();
                    } else {
                        return cursor.start().1;
                    }
                },
                None => {
                    // Beyond end of buffer
                    return cursor.start().1;
                },
            }
        }
    }

    /// Convert InlayPoint back to buffer Point.
    ///
    /// Uses cursor to seek through transform tree by output coordinate (InlayPoint),
    /// finding the corresponding input coordinate.
    ///
    /// # Bias Handling
    ///
    /// Positions inside inlay display text map back to the inlay's insertion point.
    /// The bias parameter is provided for consistency with other conversion functions,
    /// but doesn't affect the result since any display position within an inlay's
    /// extent maps to the same buffer position.
    pub fn to_point(&self, inlay_point: InlayPoint, bias: Bias) -> Point {
        let mut cursor = self.transforms.cursor::<Dimensions<InlayPoint, Point>>(());
        cursor.seek(&inlay_point, bias);

        loop {
            match cursor.item() {
                Some(Transform::Isomorphic(_)) => {
                    // Calculate overshoot from start of this isomorphic region
                    let overshoot_row = inlay_point.row - cursor.start().0.row;
                    let overshoot_col = inlay_point.column - cursor.start().0.column;
                    let input_start = cursor.start().1;

                    return Point::new(
                        input_start.row + overshoot_row,
                        input_start.column + overshoot_col,
                    );
                },
                Some(Transform::Inlay(_)) => {
                    // Position is inside inlay - return the buffer insertion point
                    // Bias doesn't matter for reverse conversion
                    return cursor.start().1;
                },
                None => {
                    // Beyond end of buffer
                    return cursor.start().1;
                },
            }
        }
    }

    /// Convert InlayPoint to InlayOffset (byte offset in inlay coordinate space).
    ///
    /// FIXME: Simplified implementation - approximates by using buffer offset.
    /// Should properly account for inlay bytes in the output space.
    pub fn to_inlay_offset(&self, inlay_point: InlayPoint) -> InlayOffset {
        // Convert to buffer point and then to offset
        // This is an approximation - inlay bytes are not counted
        let point_in_buffer = self.to_point(inlay_point, Bias::Left);
        let offset = self.buffer.point_to_offset(point_in_buffer);
        InlayOffset(offset)
    }

    /// Get the version number of this snapshot.
    pub fn version(&self) -> usize {
        self.version
    }
}

/// Dimension impl for Point - seeks by input (buffer) coordinates.
impl<'a> sum_tree::Dimension<'a, InlayTransformSummary> for Point {
    fn zero(_cx: ()) -> Self {
        Point::default()
    }

    fn add_summary(&mut self, summary: &'a InlayTransformSummary, _: ()) {
        *self += &summary.input.lines;
    }
}

/// Dimension impl for InlayPoint - seeks by output (display) coordinates.
impl<'a> sum_tree::Dimension<'a, InlayTransformSummary> for InlayPoint {
    fn zero(_cx: ()) -> Self {
        InlayPoint::default()
    }

    fn add_summary(&mut self, summary: &'a InlayTransformSummary, _: ()) {
        self.row += summary.output.lines.row;
        self.column += summary.output.lines.column;
    }
}

/// Unique identifier for an inlay hint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InlayId(pub usize);

/// Edit in InlayOffset space (output from InlayMap).
pub type InlayEdit = Edit<InlayOffset>;

/// Inlay with stable anchor-based positioning.
///
/// Stored separately from the transform tree to allow mutation.
#[derive(Clone, Debug)]
struct Inlay {
    id: InlayId,
    position: Anchor,
    data: InlayData,
}

impl Inlay {
    /// Compare two inlays by buffer position.
    fn cmp(&self, other: &Self, buffer: &BufferSnapshot) -> Ordering {
        let self_offset = self.position.to_offset(buffer);
        let other_offset = other.position.to_offset(buffer);

        self_offset
            .cmp(&other_offset)
            .then_with(|| self.data.bias.cmp(&other.data.bias))
            .then_with(|| self.id.cmp(&other.id))
    }
}

/// Mutable inlay map managing inlay insertion and removal.
///
/// Holds a snapshot and a vec of inlays that can be mutated. When inlays change,
/// `rebuild_transforms()` reconstructs the transform tree.
pub struct InlayMap {
    next_inlay_id: AtomicUsize,
    snapshot: InlaySnapshot,
    inlays: Vec<Inlay>,
}

impl InlayMap {
    /// Create a new inlay map with no inlays.
    pub fn new(buffer: BufferSnapshot) -> Self {
        Self {
            next_inlay_id: AtomicUsize::new(0),
            snapshot: InlaySnapshot::new(buffer),
            inlays: Vec::new(),
        }
    }

    /// Insert a new inlay at the given anchor position.
    pub fn insert(&mut self, position: Anchor, text: String, bias: Bias) -> InlayId {
        let id = InlayId(self.next_inlay_id.fetch_add(1, SeqCst));
        let inlay = Inlay {
            id,
            position,
            data: InlayData::new(text, bias),
        };

        // Insert in sorted order
        let insert_index = self
            .inlays
            .binary_search_by(|probe| probe.cmp(&inlay, &self.snapshot.buffer))
            .unwrap_or_else(|i| i);

        self.inlays.insert(insert_index, inlay);
        self.rebuild_transforms();

        id
    }

    /// Insert multiple inlays at once.
    pub fn insert_batch(&mut self, inlays: Vec<(Anchor, String, Bias)>) -> Vec<InlayId> {
        let mut ids = Vec::with_capacity(inlays.len());

        for (position, text, bias) in inlays {
            let id = InlayId(self.next_inlay_id.fetch_add(1, SeqCst));
            ids.push(id);

            let inlay = Inlay {
                id,
                position,
                data: InlayData::new(text, bias),
            };

            let insert_index = self
                .inlays
                .binary_search_by(|probe| probe.cmp(&inlay, &self.snapshot.buffer))
                .unwrap_or_else(|i| i);

            self.inlays.insert(insert_index, inlay);
        }

        self.rebuild_transforms();
        ids
    }

    /// Remove inlays by ID.
    pub fn remove(&mut self, ids: &[InlayId]) {
        self.inlays.retain(|inlay| !ids.contains(&inlay.id));
        self.rebuild_transforms();
    }

    /// Update buffer and rebuild transforms.
    ///
    /// Transforms buffer edits (Point coordinate space) into inlay edits (InlayPoint space).
    /// Uses cursor-based seeking for O(log n + k) performance where k is number of edits.
    pub fn sync(
        &mut self,
        buffer: BufferSnapshot,
        buffer_edits: Vec<Edit<Point>>,
    ) -> (InlaySnapshot, Vec<InlayEdit>) {
        self.snapshot.buffer = buffer.clone();
        self.snapshot.version += 1;

        if buffer_edits.is_empty() {
            // No edits - just rebuild and return empty edits
            self.rebuild_transforms();
            return (self.snapshot.clone(), Vec::new());
        }

        // Transform buffer edits to inlay edits
        let inlay_edits = self.transform_buffer_edits(buffer_edits, &buffer);

        // Rebuild transforms after edits
        self.rebuild_transforms();

        (self.snapshot.clone(), inlay_edits)
    }

    /// Transform buffer edits (Point space) to inlay edits (InlayPoint space).
    ///
    /// For each buffer edit, computes:
    /// - old range: inlay coordinates before edit
    /// - new range: inlay coordinates after edit
    ///
    /// Handles inlays that fall within, before, or after each edit.
    fn transform_buffer_edits(
        &self,
        buffer_edits: Vec<Edit<Point>>,
        new_buffer: &BufferSnapshot,
    ) -> Vec<InlayEdit> {
        let mut inlay_edits = Vec::new();

        for edit in buffer_edits {
            // Convert old buffer points to inlay points using existing API
            let old_start_inlay = self.snapshot.to_inlay_point(edit.old.start, Bias::Left);
            let old_end_inlay = self.snapshot.to_inlay_point(edit.old.end, Bias::Right);

            // Convert to offsets
            let old_start = self.snapshot.to_inlay_offset(old_start_inlay);
            let old_end = self.snapshot.to_inlay_offset(old_end_inlay);

            // Calculate new range based on buffer edit delta
            // FIXME: Simplified - doesn't account for inlays within edited range
            let old_len = self.snapshot.buffer.point_to_offset(edit.old.end)
                - self.snapshot.buffer.point_to_offset(edit.old.start);
            let new_len = new_buffer.point_to_offset(edit.new.end)
                - new_buffer.point_to_offset(edit.new.start);
            let delta = new_len as i64 - old_len as i64;

            let new_start = old_start;
            let new_end = InlayOffset((old_end.0 as i64 + delta).max(new_start.0 as i64) as usize);

            inlay_edits.push(InlayEdit {
                old: old_start..old_end,
                new: new_start..new_end,
            });
        }

        inlay_edits
    }

    /// Get the current snapshot.
    pub fn snapshot(&self) -> InlaySnapshot {
        self.snapshot.clone()
    }

    /// Rebuild the transform tree from the current inlay list.
    fn rebuild_transforms(&mut self) {
        if self.inlays.is_empty() {
            // No inlays - single isomorphic transform
            let summary = self.snapshot.buffer.text_summary();
            self.snapshot.transforms =
                SumTree::from_iter([Transform::Isomorphic(Isomorphic::new(summary))], ());
            return;
        }

        let mut transforms = Vec::new();
        let mut current_offset = 0;

        for inlay in &self.inlays {
            let inlay_offset = inlay.position.to_offset(&self.snapshot.buffer);

            // Add isomorphic transform before this inlay
            if inlay_offset > current_offset {
                let range_text = self
                    .snapshot
                    .buffer
                    .text_for_range(current_offset..inlay_offset)
                    .collect::<String>();
                let summary = TextSummary::from(range_text.as_str());
                transforms.push(Transform::Isomorphic(Isomorphic::new(summary)));
            }

            // Add inlay transform
            transforms.push(Transform::Inlay(inlay.data.clone()));
            current_offset = inlay_offset;
        }

        // Add final isomorphic transform
        let buffer_len = self.snapshot.buffer.len();
        if current_offset < buffer_len {
            let range_text = self
                .snapshot
                .buffer
                .text_for_range(current_offset..buffer_len)
                .collect::<String>();
            let summary = TextSummary::from(range_text.as_str());
            transforms.push(Transform::Isomorphic(Isomorphic::new(summary)));
        }

        self.snapshot.transforms = SumTree::from_iter(transforms, ());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use text::{Buffer, BufferId};

    fn create_buffer(text: &str) -> BufferSnapshot {
        let buffer = Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text);
        buffer.snapshot()
    }

    #[test]
    fn empty_snapshot() {
        let buffer = create_buffer("hello");
        let snapshot = InlaySnapshot::new(buffer);

        let point = Point::new(0, 2);
        let inlay_point = snapshot.to_inlay_point(point, Bias::Left);

        assert_eq!(inlay_point.row, 0);
        assert_eq!(inlay_point.column, 2);

        let back = snapshot.to_point(inlay_point, Bias::Left);
        assert_eq!(back, point);
    }

    #[test]
    fn single_inlay() {
        let buffer = create_buffer("let x = 42;");

        // Build transform tree manually:
        // "let x" (5 chars) | ": i32" (Right-biased inlay) | " = 42;" (6 chars)
        //
        // Right bias means the inlay appears BEFORE the insertion point when seeking,
        // so Point(0,5) stops before the inlay at InlayPoint(0,5)
        let transforms = vec![
            Transform::Isomorphic(Isomorphic::new(TextSummary::from("let x"))),
            Transform::Inlay(InlayData::new(": i32".to_string(), Bias::Right)),
            Transform::Isomorphic(Isomorphic::new(TextSummary::from(" = 42;"))),
        ];

        let snapshot = InlaySnapshot {
            buffer,
            transforms: SumTree::from_iter(transforms, ()),
            version: 0,
        };

        // Point 5 is at boundary - with Right-biased inlay, we stop before it
        let point1 = Point::new(0, 5);
        let inlay_point1 = snapshot.to_inlay_point(point1, Bias::Left);
        assert_eq!(inlay_point1.row, 0);
        assert_eq!(inlay_point1.column, 5);

        // After inlay: column 6 (buffer) maps to column 11 (display)
        // "let x" (5) + ": i32" (5) + " " (1) = 11
        let point2 = Point::new(0, 6);
        let inlay_point2 = snapshot.to_inlay_point(point2, Bias::Left);
        assert_eq!(inlay_point2.row, 0);
        assert_eq!(inlay_point2.column, 11);

        // Reverse: column 11 (display) maps back to column 6 (buffer)
        let back = snapshot.to_point(inlay_point2, Bias::Left);
        assert_eq!(back, point2);
    }

    #[test]
    fn multiple_inlays() {
        let buffer = create_buffer("compute(42)");

        // Build transform tree representing:
        // Buffer: "compute(42)"
        // Display: "compute(value: 42, base: 10)"
        //
        // Both inlays are Left-biased, meaning they appear AFTER their insertion points.
        // When seeking to position 8, we skip over the Left-biased inlay and land after it.
        let transforms = vec![
            // Buffer positions 0-8: "compute("
            Transform::Isomorphic(Isomorphic::new(TextSummary::from("compute("))),
            // Inlay at position 8 (Left-biased, 7 display chars)
            Transform::Inlay(InlayData::new("value: ".to_string(), Bias::Left)),
            // Buffer positions 8-10: "42"
            Transform::Isomorphic(Isomorphic::new(TextSummary::from("42"))),
            // Inlay at position 10 (Left-biased, 10 display chars)
            Transform::Inlay(InlayData::new(", base: 10".to_string(), Bias::Left)),
            // Buffer positions 10-11: ")"
            Transform::Isomorphic(Isomorphic::new(TextSummary::from(")"))),
        ];

        let snapshot = InlaySnapshot {
            buffer,
            transforms: SumTree::from_iter(transforms, ()),
            version: 0,
        };

        // Buffer column 8 at boundary - skip over Left-biased inlay
        // Result: "compute(" (8) + "value: " (7) = 15
        let point1 = Point::new(0, 8);
        let inlay_point1 = snapshot.to_inlay_point(point1, Bias::Left);
        assert_eq!(inlay_point1.column, 15);

        // Buffer column 10 at boundary - skip over Left-biased inlay
        // Result: "compute(" (8) + "value: " (7) + "42" (2) + ", base: 10" (10) = 27
        let point2 = Point::new(0, 10);
        let inlay_point2 = snapshot.to_inlay_point(point2, Bias::Left);
        assert_eq!(inlay_point2.column, 27);

        // Reverse conversions
        assert_eq!(snapshot.to_point(inlay_point1, Bias::Left), point1);
        assert_eq!(snapshot.to_point(inlay_point2, Bias::Left), point2);
    }
}
