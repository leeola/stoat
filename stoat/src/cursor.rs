//! Cursor and selection management for the editor
//!
//! This module provides efficient cursor positioning and text selection
//! functionality, following patterns from Zed for optimal performance.
//! Cursors track positions in the buffer and manage selection ranges,
//! enabling text editing operations and visual feedback.

use std::cmp::{max, min};
use text::Point;

/// A single cursor position in the buffer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Current position in the buffer
    pub position: Point,
    /// Goal column for vertical movement - maintains horizontal position when possible
    pub goal_column: Option<u32>,
}

impl Cursor {
    /// Create a new cursor at the given position
    pub fn new(position: Point) -> Self {
        Self {
            position,
            goal_column: None,
        }
    }

    /// Create a cursor at the origin (0, 0)
    pub fn at_origin() -> Self {
        Self::new(Point::new(0, 0))
    }

    /// Update the cursor position, preserving goal column for vertical moves
    pub fn move_to(&mut self, position: Point, preserve_goal_column: bool) {
        self.position = position;
        if !preserve_goal_column {
            self.goal_column = Some(position.column);
        }
    }

    /// Set the goal column explicitly (used for vertical movement)
    pub fn set_goal_column(&mut self, column: u32) {
        self.goal_column = Some(column);
    }

    /// Get the goal column, falling back to current column
    pub fn goal_column(&self) -> u32 {
        self.goal_column.unwrap_or(self.position.column)
    }
}

/// A text selection range with directional information
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Selection start point
    pub start: Point,
    /// Selection end point (cursor position)
    pub end: Point,
    /// Whether the selection was made backwards (end < start visually)
    pub reversed: bool,
}

impl Selection {
    /// Create a new selection from start to end
    pub fn new(start: Point, end: Point) -> Self {
        let reversed = end < start;
        Self {
            start: min(start, end),
            end: max(start, end),
            reversed,
        }
    }

    /// Create a collapsed selection (cursor) at the given position
    pub fn cursor(position: Point) -> Self {
        Self {
            start: position,
            end: position,
            reversed: false,
        }
    }

    /// Check if the selection is collapsed (cursor only)
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Get the cursor position (active end of selection)
    pub fn cursor_position(&self) -> Point {
        if self.reversed { self.start } else { self.end }
    }

    /// Get the anchor position (inactive end of selection)
    pub fn anchor_position(&self) -> Point {
        if self.reversed { self.end } else { self.start }
    }

    /// Get the selection range as start..end
    pub fn range(&self) -> std::ops::Range<Point> {
        self.start..self.end
    }

    /// Extend the selection to a new position
    pub fn extend_to(&mut self, position: Point) {
        if self.is_empty() {
            // First extension - determine direction
            self.reversed = position < self.start;
        }

        if self.reversed {
            self.start = min(self.start, position);
        } else {
            self.end = max(self.end, position);
        }
    }

    /// Collapse the selection to cursor position
    pub fn collapse(&mut self) {
        let cursor_pos = self.cursor_position();
        self.start = cursor_pos;
        self.end = cursor_pos;
        self.reversed = false;
    }

    /// Move the entire selection by an offset
    pub fn move_by(&mut self, offset: Point) {
        self.start = Point::new(
            self.start.row + offset.row,
            self.start.column + offset.column,
        );
        self.end = Point::new(self.end.row + offset.row, self.end.column + offset.column);
    }
}

/// Manages cursor and selection state for the editor
#[derive(Debug, Clone)]
pub struct CursorManager {
    /// Current cursor position
    cursor: Cursor,
    /// Current selection (may be collapsed to cursor)
    selection: Selection,
    /// Whether we're in selection mode
    selecting: bool,
}

impl CursorManager {
    /// Create a new cursor manager at origin
    pub fn new() -> Self {
        let cursor = Cursor::at_origin();
        let selection = Selection::cursor(cursor.position);

        Self {
            cursor,
            selection,
            selecting: false,
        }
    }

    /// Get the current cursor
    pub fn cursor(&self) -> &Cursor {
        &self.cursor
    }

    /// Get the current selection
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// Check if actively selecting text
    pub fn is_selecting(&self) -> bool {
        self.selecting
    }

    /// Move cursor to a new position
    pub fn move_to(&mut self, position: Point) {
        self.cursor.move_to(position, false);

        if self.selecting {
            self.selection.extend_to(position);
        } else {
            self.selection = Selection::cursor(position);
        }
    }

    /// Move cursor preserving goal column (for vertical movement)
    pub fn move_to_with_goal(&mut self, position: Point) {
        self.cursor.move_to(position, true);

        if self.selecting {
            self.selection.extend_to(position);
        } else {
            self.selection = Selection::cursor(position);
        }
    }

    /// Start selection mode at current position
    pub fn start_selection(&mut self) {
        self.selecting = true;
        self.selection = Selection::cursor(self.cursor.position);
    }

    /// End selection mode
    pub fn end_selection(&mut self) {
        self.selecting = false;
    }

    /// Clear selection and move cursor to position
    pub fn clear_selection_and_move_to(&mut self, position: Point) {
        self.selecting = false;
        self.cursor.move_to(position, false);
        self.selection = Selection::cursor(position);
    }

    /// Get cursor position
    pub fn position(&self) -> Point {
        self.cursor.position
    }

    /// Get goal column for vertical movement
    pub fn goal_column(&self) -> u32 {
        self.cursor.goal_column()
    }

    /// Set goal column explicitly
    pub fn set_goal_column(&mut self, column: u32) {
        self.cursor.set_goal_column(column);
    }

    /// Set selection directly (used for testing)
    pub fn set_selection(&mut self, selection: Selection) {
        let cursor_pos = selection.cursor_position();
        self.selection = selection;
        self.cursor.move_to(cursor_pos, false);
    }
}

impl Default for CursorManager {
    fn default() -> Self {
        Self::new()
    }
}
