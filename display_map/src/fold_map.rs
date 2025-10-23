use crate::{
    inlay_map::InlayMap, BufferEdit, BufferSnapshot, CoordinateTransform, EditableLayer, FoldPoint,
    InlayPoint, Point,
};
use std::ops::Range;
use sum_tree::{self, SumTree};
///! FoldMap: Coordinate transformation layer for code folding.
///!
///! This layer hides folded regions from display, transforming [`InlayPoint`] (coordinates
///! after inlays) into [`FoldPoint`] (coordinates after applying folds). When a region is
///! folded, the hidden rows are collapsed into a single line with a placeholder.
///!
///! # Coordinate Transformation
///!
///! ```text
///! Buffer:                     Display (FoldPoint):
///! Row 0: fn example() {       Row 0: fn example() { <fold> }
///! Row 1:     line 1           Row 1: fn another() {
///! Row 2:     line 2
///! Row 3:     line 3
///! Row 4: }
///! Row 5: fn another() {
///! ```
///!
///! In this example, rows 1-4 are folded, so:
///! - Buffer row 0 -> FoldPoint row 0
///! - Buffer rows 1-4 -> Hidden (folded)
///! - Buffer row 5 -> FoldPoint row 1
///!
///! # Usage Context
///!
///! FoldMap is the second layer in the DisplayMap pipeline:
///! - Input: [`InlayPoint`] from [`InlayMap`](crate::InlayMap)
///! - Output: [`FoldPoint`] consumed by [`TabMap`](crate::TabMap)
///!
///! Used by:
///! - [`TabMap`](crate::TabMap): Applies tab expansion after folding
///! - Editor view: For rendering folded code regions
///! - Navigation: For skipping over folded regions when moving cursor
///!
///! # Implementation Notes
///!
///! This simplified implementation uses a Vec instead of SumTree for fold storage.
///! A production implementation would use SumTree for O(log n) queries.
///!
///! # Related
///!
///! - [`CoordinateTransform`]: Trait for bidirectional coordinate conversion
///! - [`EditableLayer`]: Trait for handling buffer edits
///! - [`FoldPoint`]: Output coordinate type

/// Represents a single folded region in the buffer.
///
/// A fold hides a contiguous range of rows, replacing them with a placeholder
/// displayed at the fold's start position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fold {
    /// The buffer range being folded (inclusive).
    ///
    /// For example, folding rows 10-20 would have:
    /// - `range.start = Point { row: 10, column: 0 }`
    /// - `range.end = Point { row: 20, column: <end of line 20> }`
    pub range: Range<Point>,

    /// Text displayed as placeholder (e.g., three dots, curly braces, etc.).
    pub placeholder: String,
}

impl Fold {
    /// Create a new fold with default placeholder (three dots).
    pub fn new(range: Range<Point>) -> Self {
        Self {
            range,
            placeholder: "...".to_string(),
        }
    }

    /// Create a fold with custom placeholder text.
    pub fn with_placeholder(range: Range<Point>, placeholder: String) -> Self {
        Self { range, placeholder }
    }

    /// Number of buffer rows hidden by this fold.
    ///
    /// A fold from row 10 to row 20 hides 10 rows (rows 11-20).
    /// The start row (10) remains visible with the placeholder.
    pub fn hidden_row_count(&self) -> u32 {
        if self.range.end.row > self.range.start.row {
            self.range.end.row - self.range.start.row
        } else {
            0
        }
    }

    /// Check if a buffer point is inside this fold.
    ///
    /// Note: The fold start point is NOT considered inside (it shows the placeholder).
    /// Only points strictly after the start and before the end are inside.
    pub fn contains(&self, point: Point) -> bool {
        point > self.range.start && point < self.range.end
    }
}

/// Summary for [`Fold`] items in the SumTree.
///
/// Aggregates metadata about folds in a subtree for efficient queries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FoldSummary {
    /// Total buffer rows hidden by all folds in this subtree.
    pub hidden_rows: u32,
    /// Total buffer rows spanned by folds in this subtree (including fold starts).
    pub buffer_rows: u32,
    /// Number of folds in this subtree.
    pub count: usize,
}

impl sum_tree::ContextLessSummary for FoldSummary {
    fn zero() -> Self {
        Self {
            hidden_rows: 0,
            buffer_rows: 0,
            count: 0,
        }
    }

    fn add_summary(&mut self, other: &Self) {
        self.hidden_rows += other.hidden_rows;
        self.buffer_rows += other.buffer_rows;
        self.count += other.count;
    }
}

impl sum_tree::Item for Fold {
    type Summary = FoldSummary;

    fn summary(&self, _: ()) -> Self::Summary {
        FoldSummary {
            hidden_rows: self.hidden_row_count(),
            buffer_rows: self.range.end.row - self.range.start.row + 1,
            count: 1,
        }
    }
}

/// Transformation layer for code folding.
///
/// Maintains a collection of [`Fold`]s and transforms buffer coordinates to
/// fold coordinates by accounting for hidden rows.
///
/// # Example
///
/// ```ignore
/// use stoat_display_map::{FoldMap, Point, FoldPoint, BufferSnapshot};
///
/// let buffer = BufferSnapshot::from_text("line 0\nline 1\nline 2\nline 3\n");
/// let mut fold_map = FoldMap::new(buffer);
///
/// // Fold rows 1-2
/// fold_map.fold(Point { row: 1, column: 0 }..Point { row: 2, column: 6 });
///
/// // Row 3 in buffer appears at row 2 in fold space (1 row hidden)
/// let fold_point = fold_map.to_fold_point(Point { row: 3, column: 0 });
/// assert_eq!(fold_point.row, 2);
/// ```
pub struct FoldMap {
    /// InlayMap for converting between InlayPoint and buffer Point.
    inlay_map: InlayMap,

    /// Ordered collection of folds.
    ///
    /// Uses SumTree for O(log n) coordinate queries.
    folds: SumTree<Fold>,

    /// Version counter for change tracking.
    version: usize,
}

impl FoldMap {
    /// Create a new FoldMap with no folds.
    pub fn new(inlay_map: InlayMap) -> Self {
        Self {
            inlay_map,
            folds: SumTree::new(()),
            version: 0,
        }
    }

    /// Convert InlayPoint to FoldPoint.
    ///
    /// Accounts for all folds before the given point by subtracting their hidden rows.
    ///
    /// # Clamping
    ///
    /// If the point is inside a folded region, it clamps to the fold's start position.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // With fold on rows 10-20 (10 hidden rows):
    /// InlayPoint { row: 25, column: 5 } -> FoldPoint { row: 15, column: 5 }
    /// //                                              (25 - 10 hidden = 15)
    /// ```
    pub fn to_fold_point(&self, inlay_point: InlayPoint) -> FoldPoint {
        // Convert InlayPoint to buffer Point to check fold positions
        let point = self.inlay_map.to_point(inlay_point);

        let mut fold_row = point.row;
        let mut fold_column = point.column;

        // Iterate through folds using cursor
        let mut cursor = self.folds.cursor::<()>(());
        cursor.next();

        // Check if point is inside a fold - if so, clamp to fold start
        while let Some(fold) = cursor.item() {
            if fold.contains(point) {
                // Point is inside this fold - clamp to fold start
                fold_row = fold.range.start.row;
                fold_column = fold.range.start.column;
                break;
            }
            cursor.next();
        }

        // Subtract hidden rows from all folds before this point
        let mut hidden_rows = 0;
        let mut cursor = self.folds.cursor::<()>(());
        cursor.next();

        while let Some(fold) = cursor.item() {
            if fold.range.start.row < fold_row {
                hidden_rows += fold.hidden_row_count();
            } else {
                break;
            }
            cursor.next();
        }

        FoldPoint {
            row: fold_row.saturating_sub(hidden_rows),
            column: fold_column,
        }
    }

    /// Convert FoldPoint back to InlayPoint.
    ///
    /// Adds back the hidden rows from folds to map to the correct inlay position.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // With fold on rows 10-20 (10 hidden rows):
    /// FoldPoint { row: 15, column: 5 } -> InlayPoint { row: 25, column: 5 }
    /// //                                              (15 + 10 hidden = 25)
    /// ```
    pub fn to_inlay_point(&self, fold_point: FoldPoint) -> InlayPoint {
        let mut inlay_row = fold_point.row;

        // Add back hidden rows from folds that are entirely before this fold point
        let mut added_rows = 0;
        let mut cursor = self.folds.cursor::<()>(());
        cursor.next();

        while let Some(fold) = cursor.item() {
            // Adjust fold start by previously added rows
            let adjusted_fold_start = fold.range.start.row - added_rows;

            if adjusted_fold_start < inlay_row {
                let hidden = fold.hidden_row_count();
                inlay_row += hidden;
                added_rows += hidden;
            } else {
                break;
            }
            cursor.next();
        }

        // Convert buffer Point to InlayPoint
        let buffer_point = Point {
            row: inlay_row,
            column: fold_point.column,
        };
        self.inlay_map.to_inlay_point(buffer_point)
    }

    /// Create a new fold for the given buffer range.
    ///
    /// # Arguments
    ///
    /// - `range`: Buffer range to fold (must be valid and non-empty)
    ///
    /// # Returns
    ///
    /// The index of the newly created fold, or `None` if the fold is invalid.
    ///
    /// # Invariants
    ///
    /// - Folds are kept sorted by start position
    /// - Overlapping folds are not allowed (later fold operations may auto-unfold)
    pub fn fold(&mut self, range: Range<Point>) -> Option<usize> {
        if range.start >= range.end {
            return None; // Invalid range
        }

        let fold = Fold::new(range);

        // Extract to Vec, insert, rebuild
        let mut items: Vec<Fold> = self.folds.iter().cloned().collect();

        // Find insertion position to keep folds sorted
        let insert_pos = items
            .binary_search_by(|f| f.range.start.cmp(&fold.range.start))
            .unwrap_or_else(|pos| pos);

        items.insert(insert_pos, fold);

        // Rebuild SumTree
        self.folds = SumTree::from_iter(items, ());
        self.version += 1;

        Some(insert_pos)
    }

    /// Remove a fold at the given index.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds.
    pub fn unfold(&mut self, index: usize) {
        // Extract to Vec, remove, rebuild
        let mut items: Vec<Fold> = self.folds.iter().cloned().collect();
        items.remove(index);

        self.folds = SumTree::from_iter(items, ());
        self.version += 1;
    }

    /// Remove all folds overlapping with the given range.
    ///
    /// This is typically called when an edit spans a fold boundary.
    pub fn unfold_range(&mut self, range: &Range<Point>) {
        // Extract to Vec, filter, rebuild
        let items: Vec<Fold> = self
            .folds
            .iter()
            .filter(|fold| {
                // Keep folds that don't overlap with the range
                !(fold.range.start < range.end && fold.range.end > range.start)
            })
            .cloned()
            .collect();

        self.folds = SumTree::from_iter(items, ());
        self.version += 1;
    }

    /// Check if a point is inside any fold.
    pub fn is_folded(&self, point: Point) -> bool {
        let mut cursor = self.folds.cursor::<()>(());
        cursor.next();

        while let Some(fold) = cursor.item() {
            if fold.contains(point) {
                return true;
            }
            cursor.next();
        }
        false
    }

    /// Get all current folds (for testing/debugging).
    pub fn folds(&self) -> Vec<Fold> {
        self.folds.iter().cloned().collect()
    }

    /// Get the buffer snapshot.
    pub fn buffer(&self) -> &BufferSnapshot {
        self.inlay_map.buffer()
    }
}

impl CoordinateTransform<InlayPoint, FoldPoint> for FoldMap {
    fn to_coords(&self, inlay_point: InlayPoint) -> FoldPoint {
        self.to_fold_point(inlay_point)
    }

    fn from_coords(&self, fold_point: FoldPoint) -> InlayPoint {
        self.to_inlay_point(fold_point)
    }
}

impl EditableLayer for FoldMap {
    fn apply_edit(&mut self, edit: &BufferEdit) {
        // Forward edit to InlayMap
        self.inlay_map.apply_edit(edit);

        // Auto-unfold any folds that are affected by the edit
        // This is a simple strategy: unfold anything overlapping the edit
        self.unfold_range(&edit.old_range);

        // FIXME: In a full implementation, we would:
        // 1. Use Anchors instead of Points for fold positions
        // 2. Automatically adjust fold positions based on the edit
        // 3. Only unfold if the edit truly disrupts the fold structure

        self.version += 1;
    }

    fn version(&self) -> usize {
        self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_buffer() -> BufferSnapshot {
        BufferSnapshot::from_text(
            "line 0\n\
             line 1\n\
             line 2\n\
             line 3\n\
             line 4\n\
             line 5\n\
             line 6\n\
             line 7\n\
             line 8\n\
             line 9\n",
        )
    }

    #[test]
    fn no_folds_identity_mapping() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let fold_map = FoldMap::new(inlay_map);

        let inlay_point = InlayPoint { row: 5, column: 3 };
        let fold_point = fold_map.to_fold_point(inlay_point);

        assert_eq!(fold_point.row, 5);
        assert_eq!(fold_point.column, 3);

        let back = fold_map.to_inlay_point(fold_point);
        assert_eq!(back, inlay_point);
    }

    #[test]
    fn single_fold_hides_rows() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        // Fold rows 2-4 (hides 2 rows: 3 and 4)
        fold_map.fold(Point { row: 2, column: 0 }..Point { row: 4, column: 6 });

        // Row 5 in buffer appears at row 3 in fold space (2 rows hidden)
        let fold_point = fold_map.to_fold_point(InlayPoint { row: 5, column: 0 });
        assert_eq!(fold_point.row, 3);

        // Reverse: row 3 in fold space -> row 5 in buffer (inlay)
        let back = fold_map.to_inlay_point(FoldPoint { row: 3, column: 0 });
        assert_eq!(back.row, 5);
    }

    #[test]
    fn multiple_folds_accumulate_hidden_rows() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        // First fold: rows 1-2 (hides 1 row)
        fold_map.fold(Point { row: 1, column: 0 }..Point { row: 2, column: 6 });

        // Second fold: rows 4-6 (hides 2 rows)
        fold_map.fold(Point { row: 4, column: 0 }..Point { row: 6, column: 6 });

        // Row 7 in buffer: 1 row hidden from first fold + 2 rows from second = 3 hidden
        let fold_point = fold_map.to_fold_point(InlayPoint { row: 7, column: 0 });
        assert_eq!(fold_point.row, 4); // 7 - 3 = 4

        // Row 3 in buffer: only 1 row hidden from first fold
        let fold_point = fold_map.to_fold_point(InlayPoint { row: 3, column: 0 });
        assert_eq!(fold_point.row, 2); // 3 - 1 = 2
    }

    #[test]
    fn point_inside_fold_clamps_to_start() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        // Fold rows 3-5
        fold_map.fold(Point { row: 3, column: 0 }..Point { row: 5, column: 6 });

        // Point inside fold (row 4) should clamp to fold start (row 3)
        let fold_point = fold_map.to_fold_point(InlayPoint { row: 4, column: 2 });
        assert_eq!(fold_point.row, 3);
        assert_eq!(fold_point.column, 0); // Clamped to fold start column
    }

    #[test]
    fn unfold_removes_fold() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        // Fold rows 2-4
        let fold_idx = fold_map
            .fold(Point { row: 2, column: 0 }..Point { row: 4, column: 6 })
            .unwrap();

        // Row 5 appears at row 3 (2 rows hidden)
        let fold_point = fold_map.to_fold_point(InlayPoint { row: 5, column: 0 });
        assert_eq!(fold_point.row, 3);

        // Unfold
        fold_map.unfold(fold_idx);

        // Now row 5 appears at row 5 (no hidden rows)
        let fold_point = fold_map.to_fold_point(InlayPoint { row: 5, column: 0 });
        assert_eq!(fold_point.row, 5);
    }

    #[test]
    fn is_folded_detects_points_in_fold() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        fold_map.fold(Point { row: 2, column: 0 }..Point { row: 4, column: 6 });

        assert!(!fold_map.is_folded(Point { row: 1, column: 0 }));
        assert!(!fold_map.is_folded(Point { row: 2, column: 0 })); // Start is not inside
        assert!(fold_map.is_folded(Point { row: 3, column: 0 })); // Inside
        assert!(fold_map.is_folded(Point { row: 4, column: 0 })); // Inside (end-exclusive but row 4 is still hidden)
        assert!(!fold_map.is_folded(Point { row: 5, column: 0 }));
    }

    #[test]
    fn round_trip_conversion() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        fold_map.fold(Point { row: 1, column: 0 }..Point { row: 3, column: 6 });

        // Test round-trip for point before fold
        let p1 = InlayPoint { row: 0, column: 5 };
        assert_eq!(fold_map.to_inlay_point(fold_map.to_fold_point(p1)), p1);

        // Test round-trip for point after fold
        let p2 = InlayPoint { row: 5, column: 2 };
        assert_eq!(fold_map.to_inlay_point(fold_map.to_fold_point(p2)), p2);

        // Point inside fold clamps, so round-trip returns fold start
        let p3 = InlayPoint { row: 2, column: 3 };
        let fold_p3 = fold_map.to_fold_point(p3);
        let back_p3 = fold_map.to_inlay_point(fold_p3);
        assert_eq!(back_p3.row, 1); // Clamped to fold start
    }

    #[test]
    fn edit_unfolds_affected_range() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        fold_map.fold(Point { row: 2, column: 0 }..Point { row: 4, column: 6 });

        assert_eq!(fold_map.folds().len(), 1);

        // Edit overlapping the fold
        let edit = BufferEdit {
            old_range: Point { row: 3, column: 0 }..Point { row: 3, column: 5 },
            new_range: Point { row: 3, column: 0 }..Point { row: 3, column: 10 },
        };

        fold_map.apply_edit(&edit);

        // Fold should be removed
        assert_eq!(fold_map.folds().len(), 0);
    }

    #[test]
    fn fold_at_end_of_buffer() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        // Fold last two rows
        fold_map.fold(Point { row: 8, column: 0 }..Point { row: 9, column: 6 });

        let fold_point = fold_map.to_fold_point(InlayPoint { row: 9, column: 0 });
        assert_eq!(fold_point.row, 8); // Row 9 is hidden
    }

    #[test]
    fn empty_fold_rejected() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        // Empty range (start == end)
        let result = fold_map.fold(Point { row: 3, column: 0 }..Point { row: 3, column: 0 });

        assert!(result.is_none());
        assert_eq!(fold_map.folds().len(), 0);
    }

    #[test]
    fn coordinate_transform_trait() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        fold_map.fold(Point { row: 1, column: 0 }..Point { row: 2, column: 6 });

        // Use trait methods
        let inlay_point = InlayPoint { row: 3, column: 0 };
        let fold_point = fold_map.to_coords(inlay_point);
        let back = fold_map.from_coords(fold_point);

        assert_eq!(fold_point.row, 2); // 1 row hidden
        assert_eq!(back, inlay_point);
    }

    #[test]
    fn editable_layer_trait() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        let v1 = fold_map.version();

        fold_map.fold(Point { row: 1, column: 0 }..Point { row: 2, column: 6 });

        let v2 = fold_map.version();
        assert!(v2 > v1);

        let edit = BufferEdit {
            old_range: Point { row: 1, column: 0 }..Point { row: 1, column: 5 },
            new_range: Point { row: 1, column: 0 }..Point { row: 1, column: 10 },
        };

        fold_map.apply_edit(&edit);

        let v3 = fold_map.version();
        assert!(v3 > v2);
    }

    #[test]
    fn large_fold_many_hidden_rows() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        // Fold rows 1-8 (hides 7 rows)
        fold_map.fold(Point { row: 1, column: 0 }..Point { row: 8, column: 6 });

        // Row 9 in buffer appears at row 2 in fold space (7 rows hidden)
        let fold_point = fold_map.to_fold_point(InlayPoint { row: 9, column: 0 });
        assert_eq!(fold_point.row, 2); // 9 - 7 = 2

        // Reverse mapping
        let back = fold_map.to_inlay_point(FoldPoint { row: 2, column: 0 });
        assert_eq!(back.row, 9);
    }

    #[test]
    fn fold_with_custom_placeholder() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);
        let mut fold_map = FoldMap::new(inlay_map);

        let fold = Fold::with_placeholder(
            Point { row: 2, column: 0 }..Point { row: 4, column: 6 },
            "{fold}".to_string(),
        );

        // Rebuild SumTree with the fold
        fold_map.folds = SumTree::from_iter(vec![fold.clone()], ());
        fold_map.version += 1;

        assert_eq!(fold_map.folds()[0].placeholder, "{fold}");
    }

    #[test]
    fn fold_hidden_row_count() {
        let fold = Fold::new(Point { row: 10, column: 0 }..Point { row: 20, column: 0 });

        // Rows 11-20 are hidden (10 rows)
        assert_eq!(fold.hidden_row_count(), 10);

        let single_row_fold = Fold::new(Point { row: 5, column: 0 }..Point { row: 5, column: 10 });

        // Same row fold - no rows hidden
        assert_eq!(single_row_fold.hidden_row_count(), 0);
    }
}
