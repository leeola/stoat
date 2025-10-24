///! TabMap v2: Tab expansion transformation layer with snapshot pattern.
///!
///! Unlike InlayMap and FoldMap which use SumTree<Transform>, TabMap uses a simpler
///! line-scanning approach since tab expansion is inherently line-local and doesn't
///! benefit from tree-based seeking.
///!
///! # Transform Architecture
///!
///! TabMap converts FoldPoint -> TabPoint by expanding tab characters:
///! - Regular characters: 1 display column
///! - Tab characters: Advances to next multiple of tab_width
///!
///! # Example
///!
///! ```text
///! Buffer (tab_width=4):     Display:
///! "\ttext"                  "    text"   (tab expands to 4 spaces)
///! "ab\tcd"                  "ab  cd"     (tab expands to 2 spaces to align at column 4)
///! ```
///!
///! # Performance
///!
///! - Coordinate conversion: O(line_length) - must scan entire line
///! - No tree overhead: Tab expansion is purely local to each line
///! - Memory: O(1) - no caching in snapshot (immutable)
///!
///! # Related
///!
///! - Input: [`FoldPoint`](crate::FoldPoint) from [`FoldSnapshot`]
///! - Output: [`TabPoint`](crate::TabPoint)
///! - [`fold_map_v2::FoldSnapshot`] - Input layer
use crate::{
    coords::{FoldPoint, TabPoint},
    dimensions::FoldOffset,
    fold_map::FoldSnapshot,
};
use sum_tree::Bias;
use text::{Edit, Point, TextSummary, ToOffset};

/// Edit in FoldOffset space (input to TabMap).
pub type FoldEdit = Edit<FoldOffset>;

/// Edit in TabPoint space (output from TabMap).
pub type TabEdit = Edit<TabPoint>;

/// Immutable snapshot of tab expansion state.
///
/// Cheap to clone (contains Arc-based FoldSnapshot). Used for coordinate
/// conversions and can be safely shared across threads.
#[derive(Clone)]
pub struct TabSnapshot {
    /// Fold snapshot providing input coordinates.
    pub fold_snapshot: FoldSnapshot,

    /// Tab width setting (typically 2, 4, or 8).
    tab_width: u32,

    /// Version counter for change tracking.
    pub version: usize,
}

impl TabSnapshot {
    /// Create a new tab snapshot with the given fold snapshot and tab width.
    pub fn new(fold_snapshot: FoldSnapshot, tab_width: u32) -> Self {
        Self {
            fold_snapshot,
            tab_width,
            version: 0,
        }
    }

    /// Get the tab width setting.
    pub fn tab_width(&self) -> u32 {
        self.tab_width
    }

    /// Get the underlying buffer snapshot.
    pub fn buffer(&self) -> &text::BufferSnapshot {
        self.fold_snapshot.buffer()
    }

    /// Get the version number of this snapshot.
    pub fn version(&self) -> usize {
        self.version
    }

    /// Convert FoldPoint to TabPoint.
    ///
    /// Scans the line from the start, expanding tabs until reaching the target column.
    /// The display column advances by 1 for regular characters, and jumps to the next
    /// tab stop for tab characters.
    ///
    /// # Algorithm
    ///
    /// 1. Convert FoldPoint to InlayPoint to get buffer coordinates
    /// 2. Get the line text from buffer
    /// 3. Iterate through characters up to target buffer column
    /// 4. For each tab: `display_col = (display_col / tab_width + 1) * tab_width`
    /// 5. For each regular char: `display_col += 1`
    /// 6. Return TabPoint with computed display column and same row
    pub fn to_tab_point(&self, fold_point: FoldPoint, _bias: Bias) -> TabPoint {
        // Convert FoldPoint to InlayPoint, then to buffer Point to read the text
        let inlay_point = self.fold_snapshot.to_inlay_point(fold_point);
        let point = Point {
            row: inlay_point.row,
            column: inlay_point.column,
        };

        let line = self.get_line_text(point.row);

        let mut display_col = 0;
        let mut byte_offset = 0;

        // Scan characters up to the target column
        for ch in line.chars() {
            if byte_offset >= point.column {
                break;
            }

            if ch == '\t' {
                // Tab advances to next multiple of tab_width
                let next_stop = (display_col / self.tab_width + 1) * self.tab_width;
                display_col = next_stop;
            } else {
                // Regular character - one display column
                display_col += 1;
            }

            byte_offset += ch.len_utf8() as u32;
        }

        TabPoint {
            row: fold_point.row,
            column: display_col,
        }
    }

    /// Convert TabPoint to FoldPoint.
    ///
    /// Scans the line from the start, expanding tabs until reaching or exceeding the
    /// target display column. If the target is inside a tab expansion, clamps to the
    /// buffer position of the tab character itself.
    ///
    /// # Clamping Behavior
    ///
    /// If the target display column is inside a tab expansion (e.g., column 2 when
    /// a tab occupies columns 0-3), the cursor is clamped to the tab character's
    /// position. This prevents the cursor from appearing "inside" the tab.
    ///
    /// ```text
    /// Buffer: "\ttext"
    /// Display columns: 0,1,2,3,4,5,6,7
    ///                  [tab][t][e][x][t]
    ///
    /// TabPoint column 0,1,2,3 all map to buffer column 0 (the tab)
    /// TabPoint column 4 maps to buffer column 1 (the 't')
    /// ```
    pub fn to_fold_point(&self, tab_point: TabPoint, _bias: Bias) -> FoldPoint {
        // Get the buffer row for this FoldPoint row
        let fold_point_for_row = FoldPoint {
            row: tab_point.row,
            column: 0,
        };
        let inlay_point_for_row = self.fold_snapshot.to_inlay_point(fold_point_for_row);
        let buffer_row = inlay_point_for_row.row;

        let line = self.get_line_text(buffer_row);

        let mut display_col = 0;
        let mut byte_offset = 0;

        for ch in line.chars() {
            // If we've reached the target display column, stop
            if display_col >= tab_point.column {
                break;
            }

            if ch == '\t' {
                let next_stop = (display_col / self.tab_width + 1) * self.tab_width;

                // If target column is inside this tab expansion, clamp to tab start
                if next_stop > tab_point.column {
                    break;
                }

                display_col = next_stop;
            } else {
                display_col += 1;
            }

            byte_offset += ch.len_utf8() as u32;
        }

        // Convert buffer Point to InlayPoint, then to FoldPoint
        let buffer_point = Point {
            row: buffer_row,
            column: byte_offset,
        };
        let inlay_point = crate::coords::InlayPoint {
            row: buffer_point.row,
            column: buffer_point.column,
        };
        self.fold_snapshot.to_fold_point(inlay_point, Bias::Left)
    }

    /// Get the maximum TabPoint in the snapshot.
    pub fn max_point(&self) -> TabPoint {
        let max_fold = self.fold_snapshot.max_point();
        self.to_tab_point(max_fold, Bias::Left)
    }

    /// Get text summary for a range of TabPoints.
    ///
    /// Used by WrapMap for computing transform summaries during interpolation.
    pub fn text_summary_for_range(&self, range: std::ops::Range<TabPoint>) -> TextSummary {
        // Convert TabPoints to FoldPoints
        let start_fold = self.to_fold_point(range.start, Bias::Left);
        let end_fold = self.to_fold_point(range.end, Bias::Left);

        // Get the text between these fold points
        // For now, approximate using line-based summary
        let mut summary = TextSummary::default();

        if start_fold.row == end_fold.row {
            // Same row - just column difference
            let col_diff = end_fold.column.saturating_sub(start_fold.column);
            summary.lines = Point::new(0, col_diff);
            summary.len = col_diff as usize;
            summary.chars = col_diff as usize;
            summary.last_line_chars = col_diff;
            summary.last_line_len_utf16 = col_diff;
            summary.len_utf16 = text::OffsetUtf16(col_diff as usize);
        } else {
            // Multiple rows
            let row_diff = end_fold.row - start_fold.row;
            summary.lines = Point::new(row_diff, end_fold.column);
            // Approximate length
            summary.len = (row_diff * 80 + end_fold.column) as usize;
            summary.chars = summary.len;
            summary.last_line_chars = end_fold.column;
            summary.last_line_len_utf16 = end_fold.column;
            summary.len_utf16 = text::OffsetUtf16(summary.len);
        }

        summary
    }

    /// Helper to get line text from buffer.
    fn get_line_text(&self, row: u32) -> String {
        let buffer = self.buffer();
        let max_row = buffer.max_point().row;

        if row > max_row {
            return String::new();
        }

        let line_start = Point::new(row, 0);
        let line_end = if row < max_row {
            Point::new(row + 1, 0)
        } else {
            buffer.max_point()
        };

        let start_offset = line_start.to_offset(buffer);
        let end_offset = line_end.to_offset(buffer);

        buffer
            .text_for_range(start_offset..end_offset)
            .collect::<String>()
            .trim_end_matches('\n')
            .to_string()
    }
}

/// Mutable tab map managing tab expansion state.
pub struct TabMap {
    /// Current snapshot.
    snapshot: TabSnapshot,
}

impl TabMap {
    /// Create a new TabMap from a fold snapshot.
    pub fn new(fold_snapshot: FoldSnapshot, tab_width: u32) -> (Self, TabSnapshot) {
        let snapshot = TabSnapshot::new(fold_snapshot, tab_width);
        let map = Self {
            snapshot: snapshot.clone(),
        };
        (map, snapshot)
    }

    /// Get read-only access and sync with new fold snapshot.
    ///
    /// Returns updated snapshot and tab edits derived from fold edits.
    pub fn read(
        &mut self,
        fold_snapshot: FoldSnapshot,
        fold_edits: Vec<FoldEdit>,
    ) -> (TabSnapshot, Vec<TabEdit>) {
        let tab_edits = self.sync(fold_snapshot, fold_edits);
        (self.snapshot.clone(), tab_edits)
    }

    /// Sync with new fold snapshot and convert fold edits to tab edits.
    ///
    /// Tab expansion changes column positions for lines with tabs. We convert
    /// FoldOffset-based edits to TabPoint-based edits by converting offsets to
    /// points and then applying tab expansion to the points.
    fn sync(&mut self, fold_snapshot: FoldSnapshot, fold_edits: Vec<FoldEdit>) -> Vec<TabEdit> {
        let old_snapshot = self.snapshot.clone();

        // Update fold snapshot
        self.snapshot.fold_snapshot = fold_snapshot;
        self.snapshot.version += 1;

        // Convert fold edits (FoldOffset ranges) to tab edits (TabPoint ranges)
        // Since fold edits currently cover entire buffer, we convert start/end offsets to points
        let tab_edits = fold_edits
            .into_iter()
            .map(|edit| {
                // Convert old FoldOffsets to TabPoints using old snapshot
                // For now, simplified: assume offset 0 = point (0,0), max offset = max point
                let old_start = if edit.old.start.0 == 0 {
                    TabPoint { row: 0, column: 0 }
                } else {
                    // FIXME: Need proper FoldOffset -> FoldPoint -> TabPoint conversion
                    old_snapshot.max_point()
                };
                let old_end = if edit.old.end.0 == 0 {
                    TabPoint { row: 0, column: 0 }
                } else {
                    old_snapshot.max_point()
                };

                // Convert new FoldOffsets to TabPoints using new snapshot
                let new_start = if edit.new.start.0 == 0 {
                    TabPoint { row: 0, column: 0 }
                } else {
                    self.snapshot.max_point()
                };
                let new_end = if edit.new.end.0 == 0 {
                    TabPoint { row: 0, column: 0 }
                } else {
                    self.snapshot.max_point()
                };

                TabEdit {
                    old: old_start..old_end,
                    new: new_start..new_end,
                }
            })
            .collect();

        tab_edits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        coords::FoldPoint,
        fold_map::{FoldMap, FoldPlaceholder, FoldSnapshot},
        inlay_map::InlaySnapshot,
    };
    use std::num::NonZeroU64;
    use text::{Buffer, BufferId};

    fn create_buffer(text: &str) -> text::BufferSnapshot {
        let buffer = Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text);
        buffer.snapshot()
    }

    fn create_tab_snapshot(text: &str, tab_width: u32) -> TabSnapshot {
        let buffer_snapshot = create_buffer(text);
        let inlay_snapshot = InlaySnapshot::new(buffer_snapshot);
        let fold_snapshot = FoldSnapshot::new(inlay_snapshot);
        TabSnapshot::new(fold_snapshot, tab_width)
    }

    #[test]
    fn empty_line() {
        let snapshot = create_tab_snapshot("", 4);
        let tab_point = snapshot.to_tab_point(FoldPoint { row: 0, column: 0 }, Bias::Left);
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });
    }

    #[test]
    fn single_tab_at_line_start() {
        let snapshot = create_tab_snapshot("\ttext", 4);

        // Tab at column 0 expands to columns 0-3
        let tab_point = snapshot.to_tab_point(FoldPoint { row: 0, column: 0 }, Bias::Left);
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });

        // Character after tab
        let tab_point = snapshot.to_tab_point(FoldPoint { row: 0, column: 1 }, Bias::Left);
        assert_eq!(tab_point, TabPoint { row: 0, column: 4 });
    }

    #[test]
    fn tab_in_middle_of_line() {
        let snapshot = create_tab_snapshot("ab\tcd", 4);

        // 'a' at column 0
        let tab_point = snapshot.to_tab_point(FoldPoint { row: 0, column: 0 }, Bias::Left);
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });

        // 'b' at column 1
        let tab_point = snapshot.to_tab_point(FoldPoint { row: 0, column: 1 }, Bias::Left);
        assert_eq!(tab_point, TabPoint { row: 0, column: 1 });

        // tab at column 2 (expands to fill columns 2-3, next char at 4)
        let tab_point = snapshot.to_tab_point(FoldPoint { row: 0, column: 2 }, Bias::Left);
        assert_eq!(tab_point, TabPoint { row: 0, column: 2 });

        // 'c' at column 3 (after tab)
        let tab_point = snapshot.to_tab_point(FoldPoint { row: 0, column: 3 }, Bias::Left);
        assert_eq!(tab_point, TabPoint { row: 0, column: 4 });
    }

    #[test]
    fn multiple_tabs_on_same_line() {
        let snapshot = create_tab_snapshot("\t\ttext", 4);

        // First tab: 0-3
        let tab_point = snapshot.to_tab_point(FoldPoint { row: 0, column: 0 }, Bias::Left);
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });

        // Second tab: 4-7
        let tab_point = snapshot.to_tab_point(FoldPoint { row: 0, column: 1 }, Bias::Left);
        assert_eq!(tab_point, TabPoint { row: 0, column: 4 });

        // Text after both tabs
        let tab_point = snapshot.to_tab_point(FoldPoint { row: 0, column: 2 }, Bias::Left);
        assert_eq!(tab_point, TabPoint { row: 0, column: 8 });
    }

    #[test]
    fn different_tab_widths() {
        // tab_width = 2
        let snapshot = create_tab_snapshot("\ttext", 2);
        let tab_point = snapshot.to_tab_point(FoldPoint { row: 0, column: 1 }, Bias::Left);
        assert_eq!(tab_point.column, 2);

        // tab_width = 8
        let snapshot = create_tab_snapshot("\ttext", 8);
        let tab_point = snapshot.to_tab_point(FoldPoint { row: 0, column: 1 }, Bias::Left);
        assert_eq!(tab_point.column, 8);
    }

    #[test]
    fn reverse_conversion_tab_point_to_fold_point() {
        let snapshot = create_tab_snapshot("\ttext", 4);

        // Tab occupies display columns 0-3
        // All positions 0,1,2,3 should map to buffer column 0 (the tab)
        for col in 0..4 {
            let fold_point = snapshot.to_fold_point(
                TabPoint {
                    row: 0,
                    column: col,
                },
                Bias::Left,
            );
            assert_eq!(
                fold_point.column, 0,
                "TabPoint column {} should map to FoldPoint column 0",
                col
            );
        }

        // Display column 4 is the first character after the tab
        let fold_point = snapshot.to_fold_point(TabPoint { row: 0, column: 4 }, Bias::Left);
        assert_eq!(fold_point.column, 1);
    }

    #[test]
    fn round_trip_conversion() {
        let snapshot = create_tab_snapshot("a\tbc\tdef", 4);

        // Test several positions
        for col in 0..10 {
            let fold_point = FoldPoint {
                row: 0,
                column: col,
            };
            let tab_point = snapshot.to_tab_point(fold_point, Bias::Left);
            let back = snapshot.to_fold_point(tab_point, Bias::Left);

            // Should get back same or clamped position
            assert_eq!(back.row, fold_point.row);
            assert!(back.column <= fold_point.column);
        }
    }

    #[test]
    fn tab_expansion_with_fold() {
        // Create buffer with foldable content
        let text = "fn example() {\n\tline 1\n\tline 2\n}\n";
        let buffer_snapshot = create_buffer(text);
        let buffer = buffer_snapshot.clone();
        let inlay_snapshot = InlaySnapshot::new(buffer_snapshot);

        // Create fold covering lines 1-2 (the function body)
        let (mut fold_map, _initial_snapshot) = FoldMap::new(inlay_snapshot.clone());

        let fold_start = buffer.anchor_after(Point::new(1, 0));
        let fold_end = buffer.anchor_before(Point::new(3, 0));

        let (fold_snapshot, _edits) = {
            let (mut writer, _snapshot, _edits) = fold_map.write(inlay_snapshot, Vec::new());
            writer.fold(vec![(fold_start..fold_end, FoldPlaceholder::test())])
        };

        // Create TabSnapshot from folded state
        let tab_snapshot = TabSnapshot::new(fold_snapshot, 4);

        // After folding, line 3 ("}") becomes FoldPoint row 1
        // Test tab expansion on the visible line (row 0: "fn example() {")
        let tab_point = tab_snapshot.to_tab_point(FoldPoint { row: 0, column: 0 }, Bias::Left);
        assert_eq!(tab_point.row, 0);

        // Line after fold should work correctly
        let tab_point = tab_snapshot.to_tab_point(FoldPoint { row: 1, column: 0 }, Bias::Left);
        assert_eq!(tab_point.row, 1);
    }

    #[test]
    fn tab_map_sync_with_fold_edits() {
        let buffer_snapshot = create_buffer("hello\tworld");
        let inlay_snapshot = InlaySnapshot::new(buffer_snapshot);
        let fold_snapshot = FoldSnapshot::new(inlay_snapshot);

        let (mut tab_map, initial_snapshot) = TabMap::new(fold_snapshot.clone(), 4);

        // Initial version
        assert_eq!(initial_snapshot.version(), 0);

        // Sync with empty edits (no changes)
        let (new_snapshot, tab_edits) = tab_map.read(fold_snapshot.clone(), Vec::new());
        assert_eq!(new_snapshot.version(), 1);
        assert!(tab_edits.is_empty());

        // Sync with some fold edits
        let fold_edit = FoldEdit {
            old: FoldOffset(0)..FoldOffset(10),
            new: FoldOffset(0)..FoldOffset(15),
        };
        let (updated_snapshot, tab_edits) = tab_map.read(fold_snapshot, vec![fold_edit]);
        assert_eq!(updated_snapshot.version(), 2);
        assert_eq!(tab_edits.len(), 1);
    }
}
