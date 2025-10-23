use crate::{
    tab_map::TabMap, BufferEdit, BufferSnapshot, CoordinateTransform, EditableLayer, TabPoint,
    WrapPoint,
};
use sum_tree::{self, SumTree};

/// Transformation layer for soft-wrapping long lines.
///
/// WrapMap breaks lines that exceed a character width limit into multiple display rows,
/// transforming [`TabPoint`] into [`WrapPoint`]. Unlike hard line breaks, wrapping is
/// visual-only and doesn't modify the buffer.
///
/// # Coordinate Transformation
///
/// ```text
/// TabMap (one line):              WrapMap (wrap_width=20):
/// Row 0, Col 0-50:                Row 0: columns 0-19
/// "A very long line..."           Row 1: columns 20-39
///                                 Row 2: columns 40-50
/// ```
///
/// A single TabPoint row can become multiple WrapPoint rows when the line exceeds
/// the wrap width.
///
/// # Usage Context
///
/// WrapMap is the fourth layer in the DisplayMap pipeline:
/// - Input: [`TabPoint`] from [`TabMap`](crate::TabMap)
/// - Output: [`WrapPoint`] consumed by [`BlockMap`](crate::BlockMap)
///
/// Used by:
/// - [`BlockMap`](crate::BlockMap): Adds block decorations after wrapping
/// - Editor view: For rendering long lines that fit the viewport width
/// - Navigation: For moving cursor across wrapped line segments
///
/// # Implementation Notes
///
/// This simplified implementation uses character-count-based hard wrapping:
/// - No font metrics (assumes monospace)
/// - No word-boundary wrapping
/// - No wrap indent
/// - Rebuild strategy for edits
///
/// Future enhancements can add word wrapping, font metrics, and lazy calculation.
///
/// # Related
///
/// - [`CoordinateTransform`]: Trait for bidirectional coordinate conversion
/// - [`EditableLayer`]: Trait for handling buffer edits
/// - [`WrapPoint`]: Output coordinate type

/// Represents a wrap point where a line breaks for display.
///
/// Each Wrap indicates a position in a TabPoint row where the line wraps to the next
/// display row due to exceeding the wrap width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrap {
    /// The TabPoint row this wrap belongs to.
    pub tab_row: u32,

    /// The TabPoint column where this wrap occurs.
    ///
    /// Characters at and after this column appear on the next WrapPoint row.
    pub column: u32,
}

impl Wrap {
    pub fn new(tab_row: u32, column: u32) -> Self {
        Self { tab_row, column }
    }
}

impl PartialOrd for Wrap {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Wrap {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.tab_row, self.column).cmp(&(other.tab_row, other.column))
    }
}

/// Summary for [`Wrap`] items in the SumTree.
///
/// Aggregates metadata about wraps in a subtree for efficient queries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WrapSummary {
    /// Number of TabPoint rows covered by wraps in this subtree.
    pub tab_rows: u32,

    /// Total number of wraps in this subtree.
    pub wrap_count: usize,
}

impl sum_tree::ContextLessSummary for WrapSummary {
    fn zero() -> Self {
        Self {
            tab_rows: 0,
            wrap_count: 0,
        }
    }

    fn add_summary(&mut self, other: &Self) {
        self.tab_rows = self.tab_rows.max(other.tab_rows);
        self.wrap_count += other.wrap_count;
    }
}

impl sum_tree::Item for Wrap {
    type Summary = WrapSummary;

    fn summary(&self, _: ()) -> Self::Summary {
        WrapSummary {
            tab_rows: self.tab_row,
            wrap_count: 1,
        }
    }
}

/// Transformation layer for soft-wrapping long lines.
///
/// Maintains a collection of [`Wrap`]s and transforms [`TabPoint`] coordinates to
/// [`WrapPoint`] coordinates by accounting for line wrapping.
pub struct WrapMap {
    /// Input layer providing TabPoint coordinates.
    tab_map: TabMap,

    /// Tree of wrap points stored in a SumTree for O(log n) queries.
    wraps: SumTree<Wrap>,

    /// Character width limit - lines longer than this wrap to next row.
    wrap_width: u32,

    /// Version counter for tracking changes.
    version: usize,
}

impl WrapMap {
    /// Create a new WrapMap with the specified wrap width.
    ///
    /// # Arguments
    ///
    /// * `tab_map` - Input layer providing TabPoint coordinates
    /// * `wrap_width` - Maximum characters per line before wrapping
    pub fn new(tab_map: TabMap, wrap_width: u32) -> Self {
        let mut wrap_map = Self {
            tab_map,
            wraps: SumTree::new(()),
            wrap_width,
            version: 0,
        };
        wrap_map.recalculate_all_wraps();
        wrap_map
    }

    /// Recalculate wraps for all rows in the buffer.
    fn recalculate_all_wraps(&mut self) {
        let buffer = self.tab_map.buffer();
        let max_point = buffer.max_point();

        let mut all_wraps = Vec::new();
        for row in 0..=max_point.row {
            let wraps = self.calculate_wraps_for_row(row);
            all_wraps.extend(wraps);
        }

        self.wraps = SumTree::from_iter(all_wraps, ());
        self.version += 1;
    }

    /// Calculate wrap points for a specific TabPoint row.
    ///
    /// Returns wrap points for each position where the line exceeds wrap_width.
    fn calculate_wraps_for_row(&self, tab_row: u32) -> Vec<Wrap> {
        // Convert tab_row to buffer coordinates and get the line
        let tab_point = TabPoint {
            row: tab_row,
            column: 0,
        };
        let buffer_point = self.tab_map.from_coords(tab_point);
        let line = self.tab_map.buffer().line(buffer_point.row);

        // Calculate how many characters the line occupies after tab expansion
        let mut tab_column = 0;
        for ch in line.chars() {
            if ch == '\t' {
                let tab_width = self.tab_map.tab_width();
                tab_column += tab_width - (tab_column % tab_width);
            } else {
                tab_column += 1;
            }
        }

        let line_length = tab_column;

        // Calculate wrap points
        let mut wraps = Vec::new();
        if line_length > self.wrap_width {
            let mut current_column = self.wrap_width;
            while current_column < line_length {
                wraps.push(Wrap::new(tab_row, current_column));
                current_column += self.wrap_width;
            }
        }

        wraps
    }

    /// Convert TabPoint to WrapPoint.
    pub fn to_wrap_point(&self, tab_point: TabPoint) -> WrapPoint {
        let mut wrap_row = tab_point.row;
        let mut wrap_column = tab_point.column;

        // Count wraps before this point
        let mut cursor = self.wraps.cursor::<()>(());
        cursor.next();

        while let Some(wrap) = cursor.item() {
            if wrap.tab_row < tab_point.row {
                // Wrap is on an earlier row, adds one display row
                wrap_row += 1;
            } else if wrap.tab_row == tab_point.row && wrap.column <= tab_point.column {
                // Wrap is on the same row, before or at our column
                wrap_row += 1;
                wrap_column = tab_point.column - wrap.column;
            } else {
                // Past our position
                break;
            }
            cursor.next();
        }

        WrapPoint {
            row: wrap_row,
            column: wrap_column,
        }
    }

    /// Convert WrapPoint to TabPoint.
    pub fn to_tab_point(&self, wrap_point: WrapPoint) -> TabPoint {
        // Find which TabPoint row this WrapPoint row belongs to
        let mut tab_row = wrap_point.row;
        let mut wraps_before = 0;

        let mut cursor = self.wraps.cursor::<()>(());
        cursor.next();

        while let Some(wrap) = cursor.item() {
            // Count how many wraps we've seen
            wraps_before += 1;

            if wrap.tab_row + wraps_before > wrap_point.row {
                // We've gone past the target row
                wraps_before -= 1;
                break;
            }
            cursor.next();
        }

        tab_row -= wraps_before as u32;

        // Find the wrap column offset for this row
        let mut wrap_column_offset = 0;
        cursor = self.wraps.cursor::<()>(());
        cursor.next();

        let mut wrap_index_on_row = 0;
        while let Some(wrap) = cursor.item() {
            if wrap.tab_row == tab_row {
                wrap_index_on_row += 1;
                let display_row_for_wrap = tab_row + wrap_index_on_row;
                if display_row_for_wrap == wrap_point.row {
                    wrap_column_offset = wrap.column;
                    break;
                }
            } else if wrap.tab_row > tab_row {
                break;
            }
            cursor.next();
        }

        TabPoint {
            row: tab_row,
            column: wrap_point.column + wrap_column_offset,
        }
    }

    /// Get reference to the underlying TabMap.
    pub fn tab_map(&self) -> &TabMap {
        &self.tab_map
    }

    /// Get the buffer snapshot.
    pub fn buffer(&self) -> &BufferSnapshot {
        self.tab_map.buffer()
    }

    /// Get the wrap width setting.
    pub fn wrap_width(&self) -> u32 {
        self.wrap_width
    }

    /// Get all wraps as a Vec for inspection.
    pub fn wraps(&self) -> Vec<Wrap> {
        self.wraps.iter().cloned().collect()
    }
}

impl CoordinateTransform<TabPoint, WrapPoint> for WrapMap {
    fn to_coords(&self, point: TabPoint) -> WrapPoint {
        self.to_wrap_point(point)
    }

    fn from_coords(&self, point: WrapPoint) -> TabPoint {
        self.to_tab_point(point)
    }
}

impl EditableLayer for WrapMap {
    fn apply_edit(&mut self, edit: &BufferEdit) {
        // Delegate edit to TabMap
        self.tab_map.apply_edit(edit);

        // Recalculate wraps for affected rows
        // For simplicity, recalculate all wraps for now
        // Future optimization: only recalculate affected rows
        self.recalculate_all_wraps();
    }

    fn version(&self) -> usize {
        self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FoldMap, InlayMap, Point};

    fn create_test_buffer(text: &str) -> BufferSnapshot {
        BufferSnapshot::from_text(text)
    }

    fn create_wrap_map(text: &str, wrap_width: u32) -> WrapMap {
        let buffer = create_test_buffer(text);
        let inlay_map = InlayMap::new(buffer);
        let fold_map = FoldMap::new(inlay_map);
        let tab_map = TabMap::new(fold_map, 4);
        WrapMap::new(tab_map, wrap_width)
    }

    #[test]
    fn no_wrap_short_line() {
        let wrap_map = create_wrap_map("Hello", 20);

        assert_eq!(wrap_map.wraps().len(), 0);

        let tab = TabPoint { row: 0, column: 0 };
        let wrap = wrap_map.to_coords(tab);
        assert_eq!(wrap, WrapPoint { row: 0, column: 0 });
    }

    #[test]
    fn single_wrap() {
        let wrap_map = create_wrap_map("This is a line that is longer than twenty characters", 20);

        let wraps = wrap_map.wraps();
        assert!(wraps.len() > 0);

        // Column 0 should be on row 0
        let tab = TabPoint { row: 0, column: 0 };
        let wrap = wrap_map.to_coords(tab);
        assert_eq!(wrap.row, 0);

        // Column 30 should be on a later row
        let tab = TabPoint { row: 0, column: 30 };
        let wrap = wrap_map.to_coords(tab);
        assert!(wrap.row > 0);
    }

    #[test]
    fn multiple_wraps() {
        let text = "A".repeat(100);
        let wrap_map = create_wrap_map(&text, 20);

        let wraps = wrap_map.wraps();
        // 100 chars with wrap_width=20 should create 4 wraps (at 20, 40, 60, 80)
        assert_eq!(wraps.len(), 4);

        // Last character should be on row 4
        let tab = TabPoint { row: 0, column: 99 };
        let wrap = wrap_map.to_coords(tab);
        assert_eq!(wrap.row, 4);
    }

    #[test]
    fn wrap_at_exact_boundary() {
        let text = "A".repeat(40);
        let wrap_map = create_wrap_map(&text, 20);

        let wraps = wrap_map.wraps();
        assert_eq!(wraps.len(), 1); // One wrap at column 20

        // Character at column 20 should be on row 1
        let tab = TabPoint { row: 0, column: 20 };
        let wrap = wrap_map.to_coords(tab);
        assert_eq!(wrap.row, 1);
        assert_eq!(wrap.column, 0);
    }

    #[test]
    fn empty_line() {
        let wrap_map = create_wrap_map("", 20);

        assert_eq!(wrap_map.wraps().len(), 0);

        let tab = TabPoint { row: 0, column: 0 };
        let wrap = wrap_map.to_coords(tab);
        assert_eq!(wrap, WrapPoint { row: 0, column: 0 });
    }

    #[test]
    fn multiple_lines_mixed_lengths() {
        let text = "Short\nThis is a very long line that will wrap multiple times\nShort again";
        let wrap_map = create_wrap_map(text, 20);

        // First line (short) should not wrap
        let tab = TabPoint { row: 0, column: 0 };
        let wrap = wrap_map.to_coords(tab);
        assert_eq!(wrap.row, 0);

        // Second line (long) should wrap
        let wraps = wrap_map.wraps();
        let row1_wraps: Vec<_> = wraps.iter().filter(|w| w.tab_row == 1).collect();
        assert!(row1_wraps.len() > 0);
    }

    #[test]
    fn round_trip_conversion() {
        let wrap_map = create_wrap_map("A".repeat(100).as_str(), 20);

        // Test several points
        for col in [0, 10, 20, 30, 50, 75, 99] {
            let tab = TabPoint {
                row: 0,
                column: col,
            };
            let wrap = wrap_map.to_coords(tab);
            let back = wrap_map.from_coords(wrap);
            assert_eq!(tab, back, "Round trip failed for column {}", col);
        }
    }

    #[test]
    fn different_wrap_widths() {
        let text = "A".repeat(100);

        let wrap_map_10 = create_wrap_map(&text, 10);
        let wrap_map_25 = create_wrap_map(&text, 25);
        let wrap_map_50 = create_wrap_map(&text, 50);

        assert_eq!(wrap_map_10.wraps().len(), 9); // 10, 20, 30, ... 90
        assert_eq!(wrap_map_25.wraps().len(), 3); // 25, 50, 75
        assert_eq!(wrap_map_50.wraps().len(), 1); // 50
    }

    #[test]
    fn coordinate_transform_trait() {
        let wrap_map = create_wrap_map("A".repeat(60).as_str(), 20);

        let tab = TabPoint { row: 0, column: 25 };
        let wrap: WrapPoint = wrap_map.to_coords(tab);
        let back: TabPoint = wrap_map.from_coords(wrap);

        assert_eq!(tab, back);
    }

    #[test]
    fn editable_layer_trait() {
        let buffer = create_test_buffer("Hello\nWorld");
        let inlay_map = InlayMap::new(buffer);
        let fold_map = FoldMap::new(inlay_map);
        let tab_map = TabMap::new(fold_map, 4);
        let mut wrap_map = WrapMap::new(tab_map, 20);

        let initial_version = wrap_map.version();

        let edit = BufferEdit {
            old_range: Point { row: 0, column: 0 }..Point { row: 0, column: 5 },
            new_range: Point { row: 0, column: 0 }..Point { row: 0, column: 10 },
        };

        wrap_map.apply_edit(&edit);

        assert!(wrap_map.version() > initial_version);
    }

    #[test]
    fn tab_expansion_affects_wrapping() {
        let text = "A\tB\tC"; // Tabs expand to tab_width
        let wrap_map = create_wrap_map(text, 10);

        // With tab_width=4, this becomes "A   B   C" (4+4=8 chars)
        // Should not wrap at width=10
        assert_eq!(wrap_map.wraps().len(), 0);

        let wrap_map_narrow = create_wrap_map(text, 5);
        // At width=5, "A   B   C" (9 chars) should wrap
        assert!(wrap_map_narrow.wraps().len() > 0);
    }

    #[test]
    fn wrap_column_calculation() {
        let text = "A".repeat(50);
        let wrap_map = create_wrap_map(&text, 20);

        // Column 0 should be at wrap column 0
        let tab = TabPoint { row: 0, column: 0 };
        let wrap = wrap_map.to_coords(tab);
        assert_eq!(wrap.column, 0);

        // Column 20 wraps to next row, column 0
        let tab = TabPoint { row: 0, column: 20 };
        let wrap = wrap_map.to_coords(tab);
        assert_eq!(wrap.column, 0);

        // Column 25 is 5 chars into second wrap row
        let tab = TabPoint { row: 0, column: 25 };
        let wrap = wrap_map.to_coords(tab);
        assert_eq!(wrap.column, 5);
    }

    #[test]
    fn wrap_preserves_tab_row_order() {
        let text = "Short\n".to_string() + &"A".repeat(60) + "\nShort";
        let wrap_map = create_wrap_map(&text, 20);

        let wraps = wrap_map.wraps();
        let mut prev_tab_row = None;
        for wrap in wraps {
            if let Some(prev) = prev_tab_row {
                assert!(wrap.tab_row >= prev, "Wraps should be ordered by tab_row");
            }
            prev_tab_row = Some(wrap.tab_row);
        }
    }

    #[test]
    fn very_long_line() {
        let text = "A".repeat(1000);
        let wrap_map = create_wrap_map(&text, 20);

        // 1000 chars with wrap_width=20 should create 49 wraps
        let wraps = wrap_map.wraps();
        assert_eq!(wraps.len(), 49);

        // Last char should be on row 49
        let tab = TabPoint {
            row: 0,
            column: 999,
        };
        let wrap = wrap_map.to_coords(tab);
        assert_eq!(wrap.row, 49);
    }
}
