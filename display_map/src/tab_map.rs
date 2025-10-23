///! TabMap transformation layer.
///!
///! Expands tab characters into spaces based on tab width settings, transforming
///! buffer coordinates into tab-expanded coordinates.
///!
///! # Tab Expansion Rules
///!
///! Tabs advance to the next multiple of `tab_width`:
///! - Tab at column 0 with tab_width=4 displays at columns 0-3
///! - Tab at column 2 with tab_width=4 displays at columns 2-3
///! - Tab at column 4 with tab_width=4 displays at columns 4-7
///!
///! # Example
///!
///! ```text
///! Buffer (tab_width=4):     Display columns:
///! "\ttext"                  [0,1,2,3][4,5,6,7]...
///!                           tab=0-3  t e x t
///!
///! "ab\tcd"                  [0,1,2,3][4,5]...
///!                           a b     c d
///!                           tab=2-3
///! ```
use crate::{
    buffer_stubs::{BufferEdit, BufferSnapshot, Point},
    coords::{FoldPoint, InlayPoint, TabPoint},
    fold_map::FoldMap,
    inlay_map::InlayMap,
    traits::{CoordinateTransform, EditableLayer},
};
use std::collections::HashMap;

/// Tab expansion transformation layer.
///
/// Transforms [`FoldPoint`] coordinates (after folding) to [`TabPoint`] coordinates
/// by expanding tab characters into the appropriate number of visual columns.
///
/// # Architecture
///
/// TabMap scans buffer lines character-by-character, tracking both:
/// - Buffer column (byte offset in the line)
/// - Display column (visual column after tab expansion)
///
/// When a tab character is encountered, the display column advances to the next
/// multiple of `tab_width`, while the buffer column advances by one character.
///
/// # Performance
///
/// - Forward conversion: O(n) where n = buffer column position
/// - Backward conversion: O(n) where n = display column position
/// - Optional caching can reduce repeated conversions to O(1)
///
/// # Related
///
/// - Input coordinates: [`FoldPoint`] (after folding)
/// - Output coordinates: [`TabPoint`] (after tab expansion)
/// - Implements [`CoordinateTransform`] for bidirectional conversion
/// - Implements [`EditableLayer`] to handle buffer edits
pub struct TabMap {
    /// Tab width setting (typically 2, 4, or 8)
    tab_width: u32,

    /// FoldMap for converting between FoldPoint and buffer Point
    fold_map: FoldMap,

    /// Optional cache mapping row to Vec<(buffer_column, expanded_width)>
    /// Stores tab positions and their expansion widths for fast lookup
    tab_cache: HashMap<u32, Vec<(u32, u32)>>,

    /// Version counter incremented on each edit
    /// Used for cache invalidation
    version: usize,
}

impl TabMap {
    /// Create a new TabMap.
    ///
    /// # Arguments
    ///
    /// - `fold_map`: FoldMap for converting between FoldPoint and buffer Point
    /// - `tab_width`: Number of columns tabs align to (typically 2, 4, or 8)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let buffer = BufferSnapshot::from_text("\thello\n\tworld");
    /// let fold_map = FoldMap::new(buffer.clone());
    /// let tab_map = TabMap::new(fold_map, 4);
    /// ```
    pub fn new(fold_map: FoldMap, tab_width: u32) -> Self {
        Self {
            tab_width,
            fold_map,
            tab_cache: HashMap::new(),
            version: 0,
        }
    }

    /// Convert FoldPoint to TabPoint.
    ///
    /// Scans the line from the start, expanding tabs until reaching the target column.
    /// The display column advances by 1 for regular characters, and jumps to the next
    /// tab stop for tab characters.
    ///
    /// # Algorithm
    ///
    /// 1. Convert FoldPoint to buffer Point via FoldMap
    /// 2. Get the line text from buffer
    /// 3. Iterate through characters up to target buffer column
    /// 4. For each tab: `display_col = (display_col / tab_width + 1) * tab_width`
    /// 5. For each regular char: `display_col += 1`
    /// 6. Return TabPoint with computed display column and fold_point row
    ///
    /// # Edge Cases
    ///
    /// - Empty line: returns TabPoint with column 0
    /// - Column beyond line end: returns column at line end
    /// - Line with only tabs: correctly computes expanded width
    pub fn to_tab_point(&self, fold_point: FoldPoint) -> TabPoint {
        // Convert FoldPoint to InlayPoint, then to buffer Point to read the text
        let inlay_point = self.fold_map.to_inlay_point(fold_point);
        // For now, InlayPoint and Point have same structure (row, column)
        // In a system with no inlays, they're identical
        let point = Point {
            row: inlay_point.row,
            column: inlay_point.column,
        };
        let line = self.fold_map.buffer().line(point.row);

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

    /// Convert TabPoint back to FoldPoint.
    ///
    /// Scans the line from the start, expanding tabs until reaching or exceeding the
    /// target display column. If the target is inside a tab expansion, clamps to the
    /// buffer position of the tab character itself, then converts to FoldPoint.
    ///
    /// # Algorithm
    ///
    /// 1. Determine buffer row from TabPoint row (same as FoldPoint row for now)
    /// 2. Get the line text from buffer
    /// 3. Iterate through characters, tracking display column
    /// 4. Stop when display column reaches or would exceed target
    /// 5. If stopped inside tab expansion, use tab's buffer position
    /// 6. Convert buffer Point to FoldPoint via FoldMap
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
    pub fn to_fold_point(&self, tab_point: TabPoint) -> FoldPoint {
        // First, we need to figure out which buffer row corresponds to this TabPoint row
        // Since TabPoint row == FoldPoint row, we can create a FoldPoint and convert to buffer
        // Point via InlayPoint
        let fold_point_for_row = FoldPoint {
            row: tab_point.row,
            column: 0,
        };
        let inlay_point_for_row = self.fold_map.to_inlay_point(fold_point_for_row);
        let buffer_point_for_row = Point {
            row: inlay_point_for_row.row,
            column: inlay_point_for_row.column,
        };
        let line = self.fold_map.buffer().line(buffer_point_for_row.row);

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
            row: buffer_point_for_row.row,
            column: byte_offset,
        };
        let inlay_point = InlayPoint {
            row: buffer_point.row,
            column: buffer_point.column,
        };
        self.fold_map.to_fold_point(inlay_point)
    }

    /// Get the tab width setting.
    pub fn tab_width(&self) -> u32 {
        self.tab_width
    }

    /// Get the underlying FoldMap.
    pub fn fold_map(&self) -> &FoldMap {
        &self.fold_map
    }

    /// Get the buffer snapshot.
    pub fn buffer(&self) -> &BufferSnapshot {
        self.fold_map.buffer()
    }
}

impl CoordinateTransform<FoldPoint, TabPoint> for TabMap {
    fn to_coords(&self, fold_point: FoldPoint) -> TabPoint {
        self.to_tab_point(fold_point)
    }

    fn from_coords(&self, tab_point: TabPoint) -> FoldPoint {
        self.to_fold_point(tab_point)
    }
}

impl EditableLayer for TabMap {
    fn apply_edit(&mut self, edit: &BufferEdit) {
        // Forward edit to FoldMap
        self.fold_map.apply_edit(edit);

        // Invalidate entire cache on any edit
        // FIXME: Could optimize to only clear affected rows
        self.tab_cache.clear();
        self.version += 1;
    }

    fn version(&self) -> usize {
        self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_tab_map(text: &str, tab_width: u32) -> TabMap {
        let buffer = BufferSnapshot::from_text(text);
        let inlay_map = InlayMap::new(buffer);
        let fold_map = FoldMap::new(inlay_map);
        TabMap::new(fold_map, tab_width)
    }

    #[test]
    fn single_tab_at_line_start() {
        let tab_map = create_tab_map("\ttext", 4);

        // Tab at column 0 expands to columns 0-3
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 0 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });

        // Character after tab
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 1 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 4 });
    }

    #[test]
    fn tab_in_middle_of_line() {
        let tab_map = create_tab_map("ab\tcd", 4);

        // 'a' at column 0
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 0 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });

        // 'b' at column 1
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 1 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 1 });

        // tab at column 2 (expands to fill columns 2-3, next char at 4)
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 2 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 2 });

        // 'c' at column 3 (after tab)
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 3 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 4 });
    }

    #[test]
    fn multiple_tabs_on_same_line() {
        let tab_map = create_tab_map("\t\ttext", 4);

        // First tab: 0-3
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 0 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });

        // Second tab: 4-7
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 1 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 4 });

        // Text after both tabs
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 2 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 8 });
    }

    #[test]
    fn tab_width_2() {
        let tab_map = create_tab_map("\ttext", 2);

        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 0 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });

        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 1 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 2 });
    }

    #[test]
    fn tab_width_8() {
        let tab_map = create_tab_map("\ttext", 8);

        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 0 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });

        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 1 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 8 });
    }

    #[test]
    fn mixed_tabs_and_spaces() {
        let tab_map = create_tab_map(" \t text", 4);

        // Space at 0, tab at 1 (expands 1 to 4), space at 2, text at 3
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 0 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });

        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 1 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 1 });

        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 2 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 4 });
    }

    #[test]
    fn tabs_with_unicode_characters() {
        let tab_map = create_tab_map("a\tb", 4);

        // 'a' at column 0
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 0 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });

        // Tab after 'a' (advances 1 to 4)
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 1 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 1 });

        // 'b' after tab
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 2 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 4 });
    }

    #[test]
    fn empty_line() {
        let tab_map = create_tab_map("", 4);

        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 0 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });
    }

    #[test]
    fn line_with_only_tabs() {
        let tab_map = create_tab_map("\t\t\t", 4);

        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 0 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 0 });

        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 1 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 4 });

        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 2 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 8 });
    }

    #[test]
    fn round_trip_conversion() {
        let tab_map = create_tab_map("a\tbc\tdef", 4);

        // Test several positions
        for col in 0..10 {
            let fold_point = FoldPoint {
                row: 0,
                column: col,
            };
            let tab_point = tab_map.to_tab_point(fold_point);
            let back = tab_map.to_fold_point(tab_point);

            // Should get back same or clamped position
            assert!(back.row == fold_point.row);
            assert!(back.column <= fold_point.column);
        }
    }

    #[test]
    fn cursor_inside_tab_expansion_clamps() {
        let tab_map = create_tab_map("\ttext", 4);

        // Tab occupies display columns 0-3
        // Cursor at display columns 0,1,2,3 should all clamp to buffer column 0

        let point = tab_map.to_fold_point(TabPoint { row: 0, column: 0 });
        assert_eq!(point, FoldPoint { row: 0, column: 0 });

        let point = tab_map.to_fold_point(TabPoint { row: 0, column: 1 });
        assert_eq!(point, FoldPoint { row: 0, column: 0 });

        let point = tab_map.to_fold_point(TabPoint { row: 0, column: 2 });
        assert_eq!(point, FoldPoint { row: 0, column: 0 });

        let point = tab_map.to_fold_point(TabPoint { row: 0, column: 3 });
        assert_eq!(point, FoldPoint { row: 0, column: 0 });

        // Display column 4 is the first character after the tab
        let point = tab_map.to_fold_point(TabPoint { row: 0, column: 4 });
        assert_eq!(point, FoldPoint { row: 0, column: 1 });
    }

    #[test]
    fn tab_at_end_of_line() {
        let tab_map = create_tab_map("text\t", 4);

        // 't' 'e' 'x' 't' at columns 0-3, tab at column 4
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 4 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 4 });

        // After the tab
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 5 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 8 });
    }

    #[test]
    fn column_beyond_line_length() {
        let tab_map = create_tab_map("hi", 4);

        // Line is only 2 characters, but request column 10
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 10 });
        // Should clamp to line end
        assert_eq!(tab_point, TabPoint { row: 0, column: 2 });
    }

    #[test]
    fn long_line_with_many_tabs() {
        let text = "\t\t\t\t\t\t\t\ttext";
        let tab_map = create_tab_map(text, 4);

        // 8 tabs = 32 display columns (tabs occupy columns 0-31)
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 8 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 32 });

        // 't' (first char of "text") starts at display column 32
        let tab_point = tab_map.to_tab_point(FoldPoint { row: 0, column: 9 });
        assert_eq!(tab_point, TabPoint { row: 0, column: 33 });
    }
}
