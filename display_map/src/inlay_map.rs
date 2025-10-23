///! InlayMap: Coordinate transformation layer for inlay hints.
///!
///! This layer inserts visual-only text (type hints, parameter names, etc.) without
///! modifying the buffer, transforming [`Point`] (buffer coordinates) into [`InlayPoint`]
///! (coordinates after adding inlays).
///!
///! # Visual-Only Text
///!
///! Inlays display information that isn't in the buffer:
///!
///! ```text
///! Buffer:     let x = compute(42);
///! Display:    let x: String = compute(value: 42);
///!                   ^^^^^^^^             ^^^^^^^
///!                   inlay hints (not in buffer)
///! ```
///!
///! # Coordinate Transformation
///!
///! Inlays add columns to the display without changing buffer content:
///! - Buffer column 5 with a 7-character inlay before it becomes display column 12
///! - Cursor inside an inlay clamps to the inlay's insertion point
///!
///! # Usage Context
///!
///! InlayMap is the first layer in the DisplayMap pipeline:
///! - Input: [`Point`] from buffer
///! - Output: [`InlayPoint`] consumed by [`FoldMap`](crate::FoldMap)
///!
///! Used by:
///! - [`FoldMap`](crate::FoldMap): Applies code folding after inlays
///! - LSP integration: For displaying type hints and parameter names
///! - Editor view: For rendering inline documentation
///!
///! # Implementation Notes
///!
///! This simplified implementation uses a Vec instead of SumTree for inlay storage.
///! A production implementation would use SumTree for O(log n) queries.
///!
///! Inlays are positioned using [`Point`] directly. A full implementation would use
///! Anchors to automatically track positions through buffer edits.
///!
///! # Related
///!
///! - [`CoordinateTransform`]: Trait for bidirectional coordinate conversion
///! - [`EditableLayer`]: Trait for handling buffer edits
///! - [`InlayPoint`]: Output coordinate type
use crate::{
    buffer_stubs::{BufferEdit, BufferSnapshot, Point},
    coords::InlayPoint,
    traits::{CoordinateTransform, EditableLayer},
};
use sum_tree::{self, SumTree};

/// Represents a single inlay hint in the buffer.
///
/// An inlay displays visual-only text at a specific buffer position. The text appears
/// in the display but doesn't exist in the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inlay {
    /// Buffer position where the inlay appears (insertion point).
    ///
    /// The inlay text is displayed starting at this position, shifting subsequent
    /// buffer content to the right visually.
    pub position: Point,

    /// Text to display for this inlay.
    ///
    /// Common examples:
    /// - Type hints: ": String", ": i32"
    /// - Parameter names: "value: ", "count: "
    /// - Inline documentation
    pub text: String,
}

impl Inlay {
    /// Create a new inlay at the specified position.
    pub fn new(position: Point, text: String) -> Self {
        Self { position, text }
    }

    /// Get the display width of this inlay (number of visual columns).
    pub fn len(&self) -> u32 {
        self.text.len() as u32
    }

    /// Check if this inlay is empty (has no text).
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Check if a buffer point is at this inlay's position.
    pub fn is_at(&self, point: Point) -> bool {
        self.position == point
    }
}

/// Summary for [`Inlay`] items in the SumTree.
///
/// Aggregates metadata about inlays in a subtree for efficient queries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlaySummary {
    /// Total display columns added by all inlays in this subtree.
    pub total_len: u32,
    /// Number of inlays in this subtree.
    pub count: usize,
}

impl sum_tree::ContextLessSummary for InlaySummary {
    fn zero() -> Self {
        Self {
            total_len: 0,
            count: 0,
        }
    }

    fn add_summary(&mut self, other: &Self) {
        self.total_len += other.total_len;
        self.count += other.count;
    }
}

impl sum_tree::Item for Inlay {
    type Summary = InlaySummary;

    fn summary(&self, _: ()) -> Self::Summary {
        InlaySummary {
            total_len: self.len(),
            count: 1,
        }
    }
}

/// Transformation layer for inlay hints.
///
/// Maintains a collection of [`Inlay`]s and transforms buffer coordinates to
/// inlay coordinates by accounting for inserted visual text.
///
/// # Example
///!
///! ```ignore
///! use stoat_display_map::{InlayMap, Point, InlayPoint, BufferSnapshot};
///!
///! let buffer = BufferSnapshot::from_text("let x = 42;");
///! let mut inlay_map = InlayMap::new(buffer);
///!
///! // Add type hint at position after 'x'
///! inlay_map.insert(Point { row: 0, column: 5 }, ": i32".to_string());
///!
///! // Column 6 in buffer appears at column 11 in display (5 chars added)
///! let inlay_point = inlay_map.to_inlay_point(Point { row: 0, column: 6 });
///! assert_eq!(inlay_point.column, 11);
///! ```
pub struct InlayMap {
    /// Buffer snapshot for text access.
    buffer: BufferSnapshot,

    /// Ordered collection of inlays (sorted by position).
    ///
    /// Uses SumTree for O(log n) coordinate queries.
    inlays: SumTree<Inlay>,

    /// Version counter for change tracking.
    version: usize,
}

impl InlayMap {
    /// Create a new InlayMap with no inlays.
    pub fn new(buffer: BufferSnapshot) -> Self {
        Self {
            buffer,
            inlays: SumTree::new(()),
            version: 0,
        }
    }

    /// Convert buffer Point to InlayPoint.
    ///
    /// Accounts for all inlays before the given point by adding their display widths.
    ///
    /// # Algorithm
    ///
    /// 1. Find all inlays on the same row before this column
    /// 2. Sum their display widths
    /// 3. Add to the buffer column
    ///
    /// # Example
    ///
    /// ```ignore
    /// // With inlay ": String" at column 5 (8 chars):
    /// Point { row: 0, column: 10 } -> InlayPoint { row: 0, column: 18 }
    /// //                                          (10 + 8 added = 18)
    /// ```
    pub fn to_inlay_point(&self, point: Point) -> InlayPoint {
        let mut added_columns = 0;

        // Iterate through inlays using cursor
        let mut cursor = self.inlays.cursor::<()>(());
        cursor.next();

        while let Some(inlay) = cursor.item() {
            if inlay.position.row != point.row {
                cursor.next();
                continue;
            }

            if inlay.position.column < point.column {
                added_columns += inlay.len();
            } else {
                break;
            }

            cursor.next();
        }

        InlayPoint {
            row: point.row,
            column: point.column + added_columns,
        }
    }

    /// Convert InlayPoint back to buffer Point.
    ///
    /// Subtracts the display widths of inlays to map back to buffer coordinates.
    ///
    /// # Clamping
    ///
    /// If the inlay point is inside an inlay's display range, it clamps to the
    /// inlay's buffer position (insertion point).
    ///
    /// # Example
    ///
    /// ```ignore
    /// // With inlay ": String" at column 5 (8 chars):
    /// InlayPoint { row: 0, column: 18 } -> Point { row: 0, column: 10 }
    /// //                                          (18 - 8 subtracted = 10)
    ///
    /// // Inside inlay (columns 5-12 map to column 5):
    /// InlayPoint { row: 0, column: 7 } -> Point { row: 0, column: 5 }
    /// ```
    pub fn to_point(&self, inlay_point: InlayPoint) -> Point {
        let mut buffer_column = inlay_point.column;
        let mut subtracted_columns = 0;

        // Iterate through inlays using cursor
        let mut cursor = self.inlays.cursor::<()>(());
        cursor.next();

        while let Some(inlay) = cursor.item() {
            if inlay.position.row != inlay_point.row {
                cursor.next();
                continue;
            }

            let inlay_display_start = inlay.position.column + subtracted_columns;
            let inlay_display_end = inlay_display_start + inlay.len();

            if inlay_display_start >= buffer_column {
                // Inlay is after the current position, stop
                break;
            }

            if buffer_column < inlay_display_end {
                // Cursor is inside this inlay - clamp to inlay start
                buffer_column = inlay.position.column;
                break;
            }

            // Inlay is entirely before the cursor
            subtracted_columns += inlay.len();
            buffer_column -= inlay.len();

            cursor.next();
        }

        Point {
            row: inlay_point.row,
            column: buffer_column,
        }
    }

    /// Insert an inlay at the specified buffer position.
    ///
    /// # Arguments
    ///
    /// - `position`: Buffer position where the inlay should appear
    /// - `text`: Text to display for the inlay
    ///
    /// # Returns
    ///
    /// The index of the newly inserted inlay.
    ///
    /// # Invariants
    ///
    /// Inlays are kept sorted by position for efficient coordinate conversion.
    pub fn insert(&mut self, position: Point, text: String) -> usize {
        let inlay = Inlay::new(position, text);

        // Extract current inlays to Vec, insert, rebuild SumTree
        let mut items: Vec<Inlay> = self.inlays.iter().cloned().collect();

        // Find insertion position to keep inlays sorted
        let insert_pos = items
            .binary_search_by(|i| i.position.cmp(&inlay.position))
            .unwrap_or_else(|pos| pos);

        items.insert(insert_pos, inlay);

        // Rebuild SumTree from sorted Vec
        self.inlays = SumTree::from_iter(items, ());
        self.version += 1;

        insert_pos
    }

    /// Remove an inlay at the given index.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds.
    pub fn remove(&mut self, index: usize) {
        // Extract to Vec, remove, rebuild
        let mut items: Vec<Inlay> = self.inlays.iter().cloned().collect();
        items.remove(index);

        self.inlays = SumTree::from_iter(items, ());
        self.version += 1;
    }

    /// Remove all inlays in the given buffer range.
    ///
    /// This is typically called when an edit affects inlay positions.
    pub fn remove_range(&mut self, range: &std::ops::Range<Point>) {
        // Extract to Vec, filter, rebuild
        let items: Vec<Inlay> = self
            .inlays
            .iter()
            .filter(|inlay| {
                // Keep inlays that are outside the range
                inlay.position < range.start || inlay.position >= range.end
            })
            .cloned()
            .collect();

        self.inlays = SumTree::from_iter(items, ());
        self.version += 1;
    }

    /// Get all current inlays (for testing/debugging).
    pub fn inlays(&self) -> Vec<Inlay> {
        self.inlays.iter().cloned().collect()
    }

    /// Get the buffer snapshot.
    pub fn buffer(&self) -> &BufferSnapshot {
        &self.buffer
    }
}

impl CoordinateTransform<Point, InlayPoint> for InlayMap {
    fn to_coords(&self, point: Point) -> InlayPoint {
        self.to_inlay_point(point)
    }

    fn from_coords(&self, inlay_point: InlayPoint) -> Point {
        self.to_point(inlay_point)
    }
}

impl EditableLayer for InlayMap {
    fn apply_edit(&mut self, edit: &BufferEdit) {
        // Remove inlays in the edited range
        // FIXME: In a full implementation with Anchors, inlays would automatically
        // adjust their positions. Here we use a simple strategy: remove affected inlays.
        self.remove_range(&edit.old_range);

        // FIXME: Should also adjust positions of inlays after the edit
        // For now, this simple implementation just removes overlapping inlays

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
            "let x = 42;\n\
             let y = 100;\n\
             let z = 200;\n",
        )
    }

    #[test]
    fn no_inlays_identity_mapping() {
        let buffer = create_test_buffer();
        let inlay_map = InlayMap::new(buffer);

        let point = Point { row: 0, column: 5 };
        let inlay_point = inlay_map.to_inlay_point(point);

        assert_eq!(inlay_point.row, 0);
        assert_eq!(inlay_point.column, 5);

        let back = inlay_map.to_point(inlay_point);
        assert_eq!(back, point);
    }

    #[test]
    fn single_inlay_adds_columns() {
        let buffer = create_test_buffer();
        let mut inlay_map = InlayMap::new(buffer);

        // Insert ": i32" after "x" at column 5
        inlay_map.insert(Point { row: 0, column: 5 }, ": i32".to_string());

        // Column 6 appears at column 11 (5 chars added)
        let inlay_point = inlay_map.to_inlay_point(Point { row: 0, column: 6 });
        assert_eq!(inlay_point.column, 11);

        // Column 5 (inlay position) is unchanged
        let inlay_point = inlay_map.to_inlay_point(Point { row: 0, column: 5 });
        assert_eq!(inlay_point.column, 5);
    }

    #[test]
    fn multiple_inlays_accumulate() {
        let buffer = create_test_buffer();
        let mut inlay_map = InlayMap::new(buffer);

        // Insert ": i32" at column 5 (5 chars)
        inlay_map.insert(Point { row: 0, column: 5 }, ": i32".to_string());
        // Insert " /* type */" at column 8 (11 chars)
        inlay_map.insert(Point { row: 0, column: 8 }, " /* type */".to_string());

        // Column 9: after both inlays (5 + 11 = 16 added)
        let inlay_point = inlay_map.to_inlay_point(Point { row: 0, column: 9 });
        assert_eq!(inlay_point.column, 25); // 9 + 16 = 25
    }

    #[test]
    fn inlay_point_inside_inlay_clamps_to_start() {
        let buffer = create_test_buffer();
        let mut inlay_map = InlayMap::new(buffer);

        // Insert ": i32" at column 5 (occupies display columns 5-9)
        inlay_map.insert(Point { row: 0, column: 5 }, ": i32".to_string());

        // Display column 7 is inside the inlay, should clamp to buffer column 5
        let point = inlay_map.to_point(InlayPoint { row: 0, column: 7 });
        assert_eq!(point.column, 5);
    }

    #[test]
    fn reverse_conversion_after_inlay() {
        let buffer = create_test_buffer();
        let mut inlay_map = InlayMap::new(buffer);

        // Insert ": i32" at column 5
        inlay_map.insert(Point { row: 0, column: 5 }, ": i32".to_string());

        // Display column 10 -> buffer column 5 (10 - 5 = 5)
        let point = inlay_map.to_point(InlayPoint { row: 0, column: 10 });
        assert_eq!(point.column, 5);

        // Display column 11 -> buffer column 6 (11 - 5 = 6)
        let point = inlay_map.to_point(InlayPoint { row: 0, column: 11 });
        assert_eq!(point.column, 6);
    }

    #[test]
    fn round_trip_conversion() {
        let buffer = create_test_buffer();
        let mut inlay_map = InlayMap::new(buffer);

        inlay_map.insert(Point { row: 0, column: 5 }, ": i32".to_string());

        // Test round-trip for point before inlay
        let p1 = Point { row: 0, column: 3 };
        assert_eq!(inlay_map.to_point(inlay_map.to_inlay_point(p1)), p1);

        // Test round-trip for point after inlay
        let p2 = Point { row: 0, column: 8 };
        assert_eq!(inlay_map.to_point(inlay_map.to_inlay_point(p2)), p2);

        // Point at inlay position stays the same
        let p3 = Point { row: 0, column: 5 };
        assert_eq!(inlay_map.to_point(inlay_map.to_inlay_point(p3)), p3);
    }

    #[test]
    fn inlays_on_different_rows() {
        let buffer = create_test_buffer();
        let mut inlay_map = InlayMap::new(buffer);

        inlay_map.insert(Point { row: 0, column: 5 }, ": i32".to_string());
        inlay_map.insert(Point { row: 1, column: 5 }, ": i32".to_string());

        // Row 0 affected by its inlay
        let inlay_point = inlay_map.to_inlay_point(Point { row: 0, column: 6 });
        assert_eq!(inlay_point.column, 11);

        // Row 1 affected by its own inlay
        let inlay_point = inlay_map.to_inlay_point(Point { row: 1, column: 6 });
        assert_eq!(inlay_point.column, 11);

        // Row 2 has no inlays
        let inlay_point = inlay_map.to_inlay_point(Point { row: 2, column: 6 });
        assert_eq!(inlay_point.column, 6);
    }

    #[test]
    fn remove_inlay() {
        let buffer = create_test_buffer();
        let mut inlay_map = InlayMap::new(buffer);

        let idx = inlay_map.insert(Point { row: 0, column: 5 }, ": i32".to_string());

        // Inlay affects column 6
        let inlay_point = inlay_map.to_inlay_point(Point { row: 0, column: 6 });
        assert_eq!(inlay_point.column, 11);

        // Remove inlay
        inlay_map.remove(idx);

        // Now column 6 is unaffected
        let inlay_point = inlay_map.to_inlay_point(Point { row: 0, column: 6 });
        assert_eq!(inlay_point.column, 6);
    }

    #[test]
    fn edit_removes_overlapping_inlays() {
        let buffer = create_test_buffer();
        let mut inlay_map = InlayMap::new(buffer);

        inlay_map.insert(Point { row: 0, column: 5 }, ": i32".to_string());
        assert_eq!(inlay_map.inlays().len(), 1);

        // Edit overlapping the inlay position
        let edit = BufferEdit {
            old_range: Point { row: 0, column: 4 }..Point { row: 0, column: 6 },
            new_range: Point { row: 0, column: 4 }..Point { row: 0, column: 8 },
        };

        inlay_map.apply_edit(&edit);

        // Inlay should be removed
        assert_eq!(inlay_map.inlays().len(), 0);
    }

    #[test]
    fn coordinate_transform_trait() {
        let buffer = create_test_buffer();
        let mut inlay_map = InlayMap::new(buffer);

        inlay_map.insert(Point { row: 0, column: 5 }, ": i32".to_string());

        // Use trait methods
        let point = Point { row: 0, column: 8 };
        let inlay_point = inlay_map.to_coords(point);
        let back = inlay_map.from_coords(inlay_point);

        assert_eq!(inlay_point.column, 13); // 8 + 5 = 13
        assert_eq!(back, point);
    }

    #[test]
    fn editable_layer_trait() {
        let buffer = create_test_buffer();
        let mut inlay_map = InlayMap::new(buffer);

        let v1 = inlay_map.version();

        inlay_map.insert(Point { row: 0, column: 5 }, ": i32".to_string());

        let v2 = inlay_map.version();
        assert!(v2 > v1);

        let edit = BufferEdit {
            old_range: Point { row: 0, column: 0 }..Point { row: 0, column: 5 },
            new_range: Point { row: 0, column: 0 }..Point { row: 0, column: 10 },
        };

        inlay_map.apply_edit(&edit);

        let v3 = inlay_map.version();
        assert!(v3 > v2);
    }

    #[test]
    fn inlay_at_line_start() {
        let buffer = create_test_buffer();
        let mut inlay_map = InlayMap::new(buffer);

        // Inlay at column 0
        inlay_map.insert(Point { row: 0, column: 0 }, "/* hint */ ".to_string());

        // Column 0 unchanged
        let inlay_point = inlay_map.to_inlay_point(Point { row: 0, column: 0 });
        assert_eq!(inlay_point.column, 0);

        // Column 1 shifted by 11
        let inlay_point = inlay_map.to_inlay_point(Point { row: 0, column: 1 });
        assert_eq!(inlay_point.column, 12);
    }

    #[test]
    fn multiple_inlays_at_same_position() {
        let buffer = create_test_buffer();
        let mut inlay_map = InlayMap::new(buffer);

        // Two inlays at the same position
        inlay_map.insert(Point { row: 0, column: 5 }, ": i32".to_string());
        inlay_map.insert(Point { row: 0, column: 5 }, " /* note */".to_string());

        // Both inlays contribute to the offset (5 + 11 = 16)
        let inlay_point = inlay_map.to_inlay_point(Point { row: 0, column: 6 });
        assert_eq!(inlay_point.column, 22); // 6 + 16 = 22
    }

    #[test]
    fn inlay_len() {
        let inlay = Inlay::new(Point { row: 0, column: 5 }, ": String".to_string());
        assert_eq!(inlay.len(), 8);
        assert!(!inlay.is_empty());

        let empty_inlay = Inlay::new(Point { row: 0, column: 0 }, String::new());
        assert_eq!(empty_inlay.len(), 0);
        assert!(empty_inlay.is_empty());
    }
}
