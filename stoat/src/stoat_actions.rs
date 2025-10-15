//! Action implementations for Stoat.
//!
//! These demonstrate the Context<Self> pattern - methods can spawn self-updating tasks.

use crate::{
    file_finder::{PreviewData, load_file_preview, load_text_only},
    stoat::Stoat,
};
use gpui::{AppContext, Context};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use std::{num::NonZeroU64, path::PathBuf};
use text::{Bias, Buffer, BufferId, ToPoint};
use tracing::{debug, warn};

impl Stoat {
    // ==== Editing actions ====

    /// Insert text at cursor
    pub fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        // Route to file finder input buffer if in file_finder mode
        if self.mode == "file_finder" {
            if let Some(input_buffer) = &self.file_finder_input {
                // Insert at end of input buffer
                let snapshot = input_buffer.read(cx).snapshot();
                let end_offset = snapshot.len();

                input_buffer.update(cx, |buffer, _| {
                    buffer.edit([(end_offset..end_offset, text)]);
                });

                // Re-filter files based on new query
                let query = input_buffer.read(cx).snapshot().text();
                self.filter_files(&query, cx);
            }
            return;
        }

        // Route to command palette input buffer if in command_palette mode
        if self.mode == "command_palette" {
            if let Some(input_buffer) = &self.command_palette_input {
                let snapshot = input_buffer.read(cx).snapshot();
                let end_offset = snapshot.len();

                input_buffer.update(cx, |buffer, _| {
                    buffer.edit([(end_offset..end_offset, text)]);
                });

                // Re-filter commands based on new query
                let query = input_buffer.read(cx).snapshot().text();
                self.filter_commands(&query);
            }
            return;
        }

        // Route to buffer finder input buffer if in buffer_finder mode
        if self.mode == "buffer_finder" {
            if let Some(input_buffer) = &self.buffer_finder_input {
                let snapshot = input_buffer.read(cx).snapshot();
                let end_offset = snapshot.len();

                input_buffer.update(cx, |buffer, _| {
                    buffer.edit([(end_offset..end_offset, text)]);
                });

                // Re-filter buffers based on new query
                let query = input_buffer.read(cx).snapshot().text();
                self.filter_buffers(&query, cx);
            }
            return;
        }

        // Main buffer insertion for all other modes
        let cursor = self.cursor.position();
        let buffer = self.active_buffer(cx).read(cx).buffer().clone();
        buffer.update(cx, |buffer, _| {
            let offset = buffer.point_to_offset(cursor);
            buffer.edit([(offset..offset, text)]);
        });

        // Move cursor forward
        let new_col = cursor.column + text.len() as u32;
        self.cursor.move_to(text::Point::new(cursor.row, new_col));

        // Reparse for syntax highlighting
        self.active_buffer(cx).update(cx, |item, cx| {
            let _ = item.reparse(cx);
        });

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Delete character before cursor
    pub fn delete_left(&mut self, cx: &mut Context<Self>) {
        // Route to file finder input buffer if in file_finder mode
        if self.mode == "file_finder" {
            if let Some(input_buffer) = &self.file_finder_input {
                let snapshot = input_buffer.read(cx).snapshot();
                let len = snapshot.len();

                if len > 0 {
                    // Delete last character from input buffer
                    // Find char boundary to handle multi-byte UTF-8 characters
                    let text = snapshot.text();
                    let mut char_boundary = len.saturating_sub(1);
                    while char_boundary > 0 && !text.is_char_boundary(char_boundary) {
                        char_boundary -= 1;
                    }

                    input_buffer.update(cx, |buffer, _| {
                        buffer.edit([(char_boundary..len, "")]);
                    });

                    // Re-filter files based on new query
                    let query = input_buffer.read(cx).snapshot().text();
                    self.filter_files(&query, cx);
                }
            }
            return;
        }

        // Route to command palette input buffer if in command_palette mode
        if self.mode == "command_palette" {
            if let Some(input_buffer) = &self.command_palette_input {
                let snapshot = input_buffer.read(cx).snapshot();
                let len = snapshot.len();

                if len > 0 {
                    // Delete last character from input buffer
                    // Find char boundary to handle multi-byte UTF-8 characters
                    let text = snapshot.text();
                    let mut char_boundary = len.saturating_sub(1);
                    while char_boundary > 0 && !text.is_char_boundary(char_boundary) {
                        char_boundary -= 1;
                    }

                    input_buffer.update(cx, |buffer, _| {
                        buffer.edit([(char_boundary..len, "")]);
                    });

                    // Re-filter commands based on new query
                    let query = input_buffer.read(cx).snapshot().text();
                    self.filter_commands(&query);
                }
            }
            return;
        }

        // Route to buffer finder input buffer if in buffer_finder mode
        if self.mode == "buffer_finder" {
            if let Some(input_buffer) = &self.buffer_finder_input {
                let snapshot = input_buffer.read(cx).snapshot();
                let len = snapshot.len();

                if len > 0 {
                    // Delete last character from input buffer
                    // Find char boundary to handle multi-byte UTF-8 characters
                    let text = snapshot.text();
                    let mut char_boundary = len.saturating_sub(1);
                    while char_boundary > 0 && !text.is_char_boundary(char_boundary) {
                        char_boundary -= 1;
                    }

                    input_buffer.update(cx, |buffer, _| {
                        buffer.edit([(char_boundary..len, "")]);
                    });

                    // Re-filter buffers based on new query
                    let query = input_buffer.read(cx).snapshot().text();
                    self.filter_buffers(&query, cx);
                }
            }
            return;
        }

        // Main buffer deletion for all other modes
        let cursor = self.cursor.position();
        if cursor.column == 0 {
            return; // At start of line
        }

        // Naive calculation: one position to the left
        let target_point = text::Point::new(cursor.row, cursor.column.saturating_sub(1));

        // Clip to valid character boundary
        let (clipped_point, clipped_offset, cursor_offset) = {
            let buffer = self.active_buffer(cx).read(cx).buffer();
            let buffer_read = buffer.read(cx);
            let snapshot = buffer_read.snapshot();
            let clipped = snapshot.clip_point(target_point, Bias::Left);
            let clipped_offset = buffer_read.point_to_offset(clipped);
            let cursor_offset = buffer_read.point_to_offset(cursor);
            (clipped, clipped_offset, cursor_offset)
        };

        // Perform the edit
        let buffer = self.active_buffer(cx).read(cx).buffer().clone();
        buffer.update(cx, |buffer, _| {
            buffer.edit([(clipped_offset..cursor_offset, "")]);
        });

        // Move cursor to clipped position
        self.cursor.move_to(clipped_point);

        // Reparse
        self.active_buffer(cx).update(cx, |item, cx| {
            let _ = item.reparse(cx);
        });

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Delete character at cursor position (delete right)
    pub fn delete_right(&mut self, cx: &mut Context<Self>) {
        let cursor = self.cursor.position();

        // Read buffer info in separate scope to release locks
        let (line_len, max_row) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer = buffer_item.buffer().read(cx);
            (buffer.line_len(cursor.row), buffer.max_point().row)
        };

        if cursor.column < line_len {
            // Delete character on same line
            let buffer = self.active_buffer(cx).read(cx).buffer().clone();
            buffer.update(cx, |buffer, _| {
                let start = buffer.point_to_offset(cursor);
                let end = buffer.point_to_offset(text::Point::new(cursor.row, cursor.column + 1));
                buffer.edit([(start..end, "")]);
            });

            // Cursor stays in place
        } else if cursor.row < max_row {
            // At line end: merge with next line
            let buffer = self.active_buffer(cx).read(cx).buffer().clone();
            buffer.update(cx, |buffer, _| {
                let start = buffer.point_to_offset(cursor);
                let end = buffer.point_to_offset(text::Point::new(cursor.row + 1, 0));
                buffer.edit([(start..end, "")]);
            });

            // Cursor stays in place
        }
        // Else: at buffer end, no-op

        // Reparse
        self.active_buffer(cx).update(cx, |item, cx| {
            let _ = item.reparse(cx);
        });

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Delete word (symbol) before cursor.
    ///
    /// Deletes from the start of the previous symbol to the cursor position,
    /// removing the previous word along with any intervening whitespace.
    ///
    /// # Behavior
    ///
    /// - Finds previous symbol boundary
    /// - Deletes from symbol start to cursor
    /// - Moves cursor to deletion start
    /// - If no previous symbol, does nothing
    /// - Triggers reparse for syntax highlighting
    ///
    /// # Related
    ///
    /// See also [`Self::delete_word_right`] for forward word deletion.
    pub fn delete_word_left(&mut self, cx: &mut Context<Self>) {
        use text::ToOffset;

        // Get buffer and token snapshots
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        let cursor_pos = self.cursor.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut prev_symbol_start: Option<usize> = None;

        // Iterate through tokens to find the previous symbol
        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // If we've passed the cursor, we're done
            if token_start >= cursor_offset {
                break;
            }

            // Check if this token is a symbol
            if token.kind.is_symbol() {
                // If cursor is inside or at the end of this token, delete from start to cursor
                if token_start < cursor_offset && cursor_offset <= token_end {
                    prev_symbol_start = Some(token_start);
                    break;
                }

                // Track symbols that end strictly before cursor
                if token_end < cursor_offset {
                    prev_symbol_start = Some(token_start);
                }
            }

            token_cursor.next();
        }

        // Delete from symbol start to cursor if found
        if let Some(start_offset) = prev_symbol_start {
            let delete_start = buffer_snapshot.offset_to_point(start_offset);

            // Perform deletion
            let buffer = self.active_buffer(cx).read(cx).buffer().clone();
            buffer.update(cx, |buffer, _| {
                let start = buffer.point_to_offset(delete_start);
                let end = buffer.point_to_offset(cursor_pos);
                buffer.edit([(start..end, "")]);
            });

            // Move cursor to deletion start
            self.cursor.move_to(delete_start);

            // Reparse
            self.active_buffer(cx).update(cx, |item, cx| {
                let _ = item.reparse(cx);
            });

            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        }
    }

    /// Delete word (symbol) after cursor.
    ///
    /// Deletes from the cursor position to the end of the next symbol,
    /// removing the next word along with any intervening whitespace.
    ///
    /// # Behavior
    ///
    /// - Finds next symbol boundary
    /// - Deletes from cursor to symbol end
    /// - Cursor stays at current position
    /// - If no next symbol, does nothing
    /// - Triggers reparse for syntax highlighting
    ///
    /// # Related
    ///
    /// See also [`Self::delete_word_left`] for backward word deletion.
    pub fn delete_word_right(&mut self, cx: &mut Context<Self>) {
        use text::ToOffset;

        // Get buffer and token snapshots
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        let cursor_pos = self.cursor.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        // Create a cursor to iterate through tokens
        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut found_symbol_end: Option<usize> = None;

        // Iterate through tokens to find the next symbol
        while let Some(token) = token_cursor.item() {
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // Skip tokens that are entirely before the cursor
            if token_end <= cursor_offset {
                token_cursor.next();
                continue;
            }

            // Check if this token is a symbol
            if token.kind.is_symbol() {
                // Found a symbol - delete to its end
                found_symbol_end = Some(token_end);
                break;
            }

            // Not a symbol, keep looking
            token_cursor.next();
        }

        // Delete from cursor to symbol end if found
        if let Some(end_offset) = found_symbol_end {
            let delete_end = buffer_snapshot.offset_to_point(end_offset);

            // Perform deletion
            let buffer = self.active_buffer(cx).read(cx).buffer().clone();
            buffer.update(cx, |buffer, _| {
                let start = buffer.point_to_offset(cursor_pos);
                let end = buffer.point_to_offset(delete_end);
                buffer.edit([(start..end, "")]);
            });

            // Cursor stays in place

            // Reparse
            self.active_buffer(cx).update(cx, |item, cx| {
                let _ = item.reparse(cx);
            });

            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        }
    }

    /// Insert newline at cursor
    pub fn new_line(&mut self, cx: &mut Context<Self>) {
        let cursor = self.cursor.position();
        let buffer = self.active_buffer(cx).read(cx).buffer().clone();
        buffer.update(cx, |buffer, _| {
            let offset = buffer.point_to_offset(cursor);
            buffer.edit([(offset..offset, "\n")]);
        });

        // Move cursor to next line, column 0
        self.cursor.move_to(text::Point::new(cursor.row + 1, 0));

        // Reparse
        self.active_buffer(cx).update(cx, |item, cx| {
            let _ = item.reparse(cx);
        });

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Delete current line
    ///
    /// Removes the entire line where the cursor is positioned, including the trailing
    /// newline (except for the last line). The cursor moves to the beginning of the line.
    ///
    /// # Behavior
    ///
    /// - For non-last lines: deletes from line start to next line start (includes newline)
    /// - For last line: deletes from line start to line end (no newline to delete)
    /// - Cursor moves to beginning of the line (or next line if not last)
    /// - Empty buffer remains empty
    ///
    /// # Related
    ///
    /// See also [`Self::delete_to_end_of_line`] for partial line deletion.
    pub fn delete_line(&mut self, cx: &mut Context<Self>) {
        let cursor = self.cursor.position();

        // Get buffer snapshot to determine line boundaries
        let (line_start, line_end) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer = buffer_item.buffer().read(cx);
            let row_count = buffer.row_count();

            let line_start = text::Point::new(cursor.row, 0);

            // Include newline if not last line
            let line_end = if cursor.row < row_count - 1 {
                text::Point::new(cursor.row + 1, 0)
            } else {
                // Last line - delete to end of line
                let line_len = buffer.line_len(cursor.row);
                text::Point::new(cursor.row, line_len)
            };

            (line_start, line_end)
        };

        debug!(row = cursor.row, from = ?line_start, to = ?line_end, "Deleting line");

        // Perform deletion
        let buffer = self.active_buffer(cx).read(cx).buffer().clone();
        buffer.update(cx, |buffer, _| {
            let start_offset = buffer.point_to_offset(line_start);
            let end_offset = buffer.point_to_offset(line_end);
            buffer.edit([(start_offset..end_offset, "")]);
        });

        // Move cursor to line start
        self.cursor.move_to(line_start);

        // Reparse
        self.active_buffer(cx).update(cx, |item, cx| {
            let _ = item.reparse(cx);
        });

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Delete from cursor to end of line
    ///
    /// Removes all text from the cursor position to the end of the current line,
    /// preserving the newline character. The cursor stays at its current position.
    ///
    /// # Behavior
    ///
    /// - Deletes from cursor to end of line (exclusive of newline)
    /// - Cursor stays at current position
    /// - If cursor is already at end of line: no effect
    /// - Empty lines remain empty
    ///
    /// # Related
    ///
    /// See also [`Self::delete_line`] for full line deletion.
    pub fn delete_to_end_of_line(&mut self, cx: &mut Context<Self>) {
        let cursor = self.cursor.position();

        // Get line length
        let line_len = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer = buffer_item.buffer().read(cx);
            buffer.line_len(cursor.row)
        };

        // Only delete if not already at end of line
        if cursor.column < line_len {
            let end = text::Point::new(cursor.row, line_len);

            debug!(from = ?cursor, to = ?end, "Delete to end of line");

            // Perform deletion
            let buffer = self.active_buffer(cx).read(cx).buffer().clone();
            buffer.update(cx, |buffer, _| {
                let start_offset = buffer.point_to_offset(cursor);
                let end_offset = buffer.point_to_offset(end);
                buffer.edit([(start_offset..end_offset, "")]);
            });

            // Reparse
            self.active_buffer(cx).update(cx, |item, cx| {
                let _ = item.reparse(cx);
            });

            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        } else {
            debug!(pos = ?cursor, "Already at end of line, nothing to delete");
        }
    }

    // ==== Movement actions ====

    /// Move cursor up
    pub fn move_up(&mut self, cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        if pos.row > 0 {
            let target_row = pos.row - 1;
            let line_len = self
                .active_buffer(cx)
                .read(cx)
                .buffer()
                .read(cx)
                .line_len(target_row);
            let target_column = self.cursor.goal_column().min(line_len);
            self.cursor
                .move_to_with_goal(text::Point::new(target_row, target_column));
            self.ensure_cursor_visible();
        }
    }

    /// Move cursor down
    pub fn move_down(&mut self, cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        let max_row = self
            .active_buffer(cx)
            .read(cx)
            .buffer()
            .read(cx)
            .max_point()
            .row;

        if pos.row < max_row {
            let target_row = pos.row + 1;
            let line_len = self
                .active_buffer(cx)
                .read(cx)
                .buffer()
                .read(cx)
                .line_len(target_row);
            let target_column = self.cursor.goal_column().min(line_len);
            self.cursor
                .move_to_with_goal(text::Point::new(target_row, target_column));
            self.ensure_cursor_visible();
        }
    }

    /// Move cursor left
    pub fn move_left(&mut self, cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        if pos.column > 0 {
            let target = text::Point::new(pos.row, pos.column - 1);
            let snapshot = self.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let clipped = snapshot.clip_point(target, Bias::Left);
            self.cursor.move_to(clipped);
        }
    }

    /// Move cursor right
    pub fn move_right(&mut self, cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        let line_len = self
            .active_buffer(cx)
            .read(cx)
            .buffer()
            .read(cx)
            .line_len(pos.row);

        if pos.column < line_len {
            let target = text::Point::new(pos.row, pos.column + 1);
            let snapshot = self.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let clipped = snapshot.clip_point(target, Bias::Right);
            self.cursor.move_to(clipped);
        }
    }

    /// Move cursor left by one word (symbol).
    ///
    /// Moves the cursor to the start of the previous symbol, skipping whitespace,
    /// punctuation, and operators. A symbol is an identifier, keyword, or number.
    ///
    /// # Behavior
    ///
    /// - Skips whitespace, newlines, punctuation, and operators
    /// - Moves to start of previous symbol
    /// - If cursor is mid-symbol, moves to start of current symbol
    /// - If no previous symbol exists, does nothing
    ///
    /// # Related
    ///
    /// See also [`Self::move_word_right`] for forward word movement.
    pub fn move_word_left(&mut self, cx: &mut Context<Self>) {
        use text::ToOffset;

        // Get buffer and token snapshots
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        let cursor_pos = self.cursor.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut prev_symbol_start: Option<usize> = None;

        // Iterate through tokens to find the previous symbol
        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // If we've passed the cursor, we're done
            if token_start >= cursor_offset {
                break;
            }

            // Check if this token is a symbol
            if token.kind.is_symbol() {
                // If cursor is inside or at the end of this token, move to start
                if token_start < cursor_offset && cursor_offset <= token_end {
                    prev_symbol_start = Some(token_start);
                    break;
                }

                // Track symbols that end strictly before cursor
                if token_end < cursor_offset {
                    prev_symbol_start = Some(token_start);
                }
            }

            token_cursor.next();
        }

        // Move cursor to symbol start if found
        if let Some(offset) = prev_symbol_start {
            let new_pos = buffer_snapshot.offset_to_point(offset);
            self.cursor.move_to(new_pos);
            cx.notify();
        }
    }

    /// Move cursor right by one word (symbol).
    ///
    /// Moves the cursor to the end of the next symbol, skipping whitespace,
    /// punctuation, and operators. A symbol is an identifier, keyword, or number.
    ///
    /// # Behavior
    ///
    /// - Skips whitespace, newlines, punctuation, and operators
    /// - Moves to end of next symbol
    /// - If cursor is mid-symbol, moves to end of current symbol
    /// - If no next symbol exists, does nothing
    ///
    /// # Related
    ///
    /// See also [`Self::move_word_left`] for backward word movement.
    pub fn move_word_right(&mut self, cx: &mut Context<Self>) {
        use text::ToOffset;

        // Get buffer and token snapshots
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        let cursor_pos = self.cursor.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        // Create a cursor to iterate through tokens
        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut found_symbol_end: Option<usize> = None;

        // Iterate through tokens to find the next symbol
        while let Some(token) = token_cursor.item() {
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // Skip tokens that are entirely before the cursor
            if token_end <= cursor_offset {
                token_cursor.next();
                continue;
            }

            // Check if this token is a symbol
            if token.kind.is_symbol() {
                // Found a symbol - move to its end
                found_symbol_end = Some(token_end);
                break;
            }

            // Not a symbol, keep looking
            token_cursor.next();
        }

        // Move cursor to symbol end if found
        if let Some(offset) = found_symbol_end {
            let new_pos = buffer_snapshot.offset_to_point(offset);
            self.cursor.move_to(new_pos);
            cx.notify();
        }
    }

    /// Move cursor to start of line (column 0)
    pub fn move_to_line_start(&mut self, _cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        self.cursor.move_to(text::Point::new(pos.row, 0));
    }

    /// Move cursor to end of line
    pub fn move_to_line_end(&mut self, cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        let line_len = self
            .active_buffer(cx)
            .read(cx)
            .buffer()
            .read(cx)
            .line_len(pos.row);
        self.cursor.move_to(text::Point::new(pos.row, line_len));
    }

    /// Handle scroll wheel/trackpad events
    pub fn handle_scroll(
        &mut self,
        delta: &crate::scroll::ScrollDelta,
        fast_scroll: bool,
        cx: &mut Context<Self>,
    ) {
        // Scroll sensitivity values
        let base_sensitivity = 1.0;
        let fast_multiplier = 3.0;

        // Line height for delta conversion
        let line_height = 20.0; // Default line height in pixels

        // Calculate new scroll position using existing infrastructure
        let new_position = self.scroll.apply_scroll_delta(
            delta,
            line_height,
            base_sensitivity,
            fast_multiplier,
            fast_scroll,
        );

        // Apply bounds checking
        let buffer_item = self.active_buffer(cx).read(cx);
        let buffer = buffer_item.buffer().read(cx);
        let max_point = buffer.max_point();
        let max_scroll_y = (max_point.row as f32).max(0.0);

        let bounded_position = gpui::point(
            new_position.x.max(0.0),
            new_position.y.max(0.0).min(max_scroll_y),
        );

        // Update scroll position
        self.scroll.scroll_to(bounded_position);
    }

    // ==== Mode actions ====

    /// Enter insert mode
    pub fn enter_insert_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "insert".to_string();
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Enter normal mode
    pub fn enter_normal_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "normal".to_string();
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Enter visual mode
    ///
    /// Transitions to Visual mode for text selection. Movement commands extend the
    /// selection rather than moving the cursor.
    ///
    /// # Behavior
    ///
    /// - Sets editor mode to Visual
    /// - Selection is anchored at current cursor position
    /// - Movement commands now extend selection
    /// - Can transition from Normal or Insert mode
    /// - Typically bound to 'v' key
    ///
    /// # Related
    ///
    /// See also [`Self::enter_normal_mode`] for returning to command mode.
    pub fn enter_visual_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "visual".to_string();
        debug!("Entering visual mode");
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Enter space mode (leader key)
    pub fn enter_space_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "space".to_string();
        tracing::info!("Entering space mode");
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Enter pane mode (window management)
    pub fn enter_pane_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "pane".to_string();
        debug!("Entering pane mode");
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    pub fn enter_git_filter_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "git_filter".to_string();
        debug!("Entering git_filter mode");
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Set the active KeyContext (action handler).
    ///
    /// Changes which UI is rendered (e.g., TextEditor, Git modal, FileFinder).
    /// The KeyContext determines the high-level "what's showing" while mode
    /// determines "how you interact with it".
    ///
    /// This is the action handler version that emits events and notifications.
    ///
    /// # Arguments
    ///
    /// * `context` - The KeyContext to activate
    ///
    /// # Example
    ///
    /// When opening git status, we use `SetKeyContext(Git)` which sets context to Git
    /// and mode to "git_status" (the default mode for Git context from keymap.toml).
    /// Within Git context, we can switch between git_status and git_filter modes without
    /// the modal disappearing.
    pub fn handle_set_key_context(
        &mut self,
        context: crate::stoat::KeyContext,
        cx: &mut Context<Self>,
    ) {
        // Set the new context
        self.set_key_context(context);

        // Look up and set the default mode for this context
        if let Some(meta) = self.get_key_context_meta(context) {
            let default_mode = meta.default_mode.clone();
            self.set_mode(&default_mode);
            debug!(context = ?context, mode = %default_mode, "Set KeyContext with default mode");
        } else {
            warn!(context = ?context, "No metadata found for KeyContext, mode unchanged");
        }

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Set the active mode within the current KeyContext.
    ///
    /// Changes which keybindings are active without changing the rendered UI.
    /// Used for transitions like git_status to git_filter within the Git context.
    ///
    /// # Arguments
    ///
    /// * `mode_name` - Name of the mode to activate
    pub fn set_mode_by_name(&mut self, mode_name: &str, cx: &mut Context<Self>) {
        self.mode = mode_name.to_string();
        debug!(mode = mode_name, "Set mode");
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    // ==== File finder actions ====

    /// Open file finder.
    ///
    /// This demonstrates Context<Self> - can create entities and scan worktree.
    pub fn open_file_finder(&mut self, cx: &mut Context<Self>) {
        debug!("Opening file finder");

        // Save current mode
        self.file_finder_previous_mode = Some(self.mode.clone());
        self.key_context = crate::stoat::KeyContext::FileFinder;
        self.mode = "file_finder".to_string();

        // Create input buffer
        let buffer_id = BufferId::from(NonZeroU64::new(2).unwrap());
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        self.file_finder_input = Some(input_buffer);

        // Scan worktree
        let entries = self.worktree.lock().snapshot().entries(false);
        debug!(file_count = entries.len(), "Loaded files from worktree");

        self.file_finder_files = entries;
        self.file_finder_filtered = self
            .file_finder_files
            .iter()
            .map(|e| PathBuf::from(e.path.as_unix_str()))
            .collect();
        self.file_finder_selected = 0;

        // Load preview for first file
        self.load_preview_for_selected(cx);

        cx.notify();
    }

    /// Move to next file in finder.
    ///
    /// Demonstrates spawning async task with Context<Self>.
    pub fn file_finder_next(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        if self.file_finder_selected + 1 < self.file_finder_filtered.len() {
            self.file_finder_selected += 1;
            debug!(selected = self.file_finder_selected, "File finder: next");

            // Load preview for newly selected file
            self.load_preview_for_selected(cx);
            cx.notify();
        }
    }

    /// Move to previous file in finder
    pub fn file_finder_prev(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        if self.file_finder_selected > 0 {
            self.file_finder_selected -= 1;
            debug!(selected = self.file_finder_selected, "File finder: prev");

            // Load preview for newly selected file
            self.load_preview_for_selected(cx);
            cx.notify();
        }
    }

    /// Select file in finder
    pub fn file_finder_select(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        if self.file_finder_selected < self.file_finder_filtered.len() {
            let relative_path = &self.file_finder_filtered[self.file_finder_selected];
            debug!(file = ?relative_path, "File finder: select");

            // Build absolute path
            let root = self.worktree.lock().snapshot().root().to_path_buf();
            let abs_path = root.join(relative_path);

            // Store file path for status bar
            self.current_file_path = Some(relative_path.clone());

            // Load file (uses load_file to ensure diff computation)
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::error!("Failed to load file {:?}: {}", abs_path, e);
            }
        }

        self.file_finder_dismiss(cx);
    }

    /// Dismiss file finder.
    ///
    /// Clears file finder state. Mode and KeyContext transitions are now handled
    /// by the [`SetKeyContext`](crate::actions::SetKeyContext) action bound to Escape.
    pub fn file_finder_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        debug!("Dismissing file finder");

        // Clear state
        self.file_finder_input = None;
        self.file_finder_files.clear();
        self.file_finder_filtered.clear();
        self.file_finder_selected = 0;
        self.file_finder_preview = None;
        self.file_finder_preview_task = None;
        self.file_finder_previous_mode = None;

        cx.notify();
    }

    /// Load preview for selected file.
    ///
    /// KEY METHOD: Demonstrates Context<Self> pattern with async tasks.
    /// Uses `cx.spawn` to get `WeakEntity<Self>` for self-updating.
    pub fn load_preview_for_selected(&mut self, cx: &mut Context<Self>) {
        // Cancel existing task
        self.file_finder_preview_task = None;

        // Get selected file path
        let relative_path = match self.file_finder_filtered.get(self.file_finder_selected) {
            Some(path) => path.clone(),
            None => {
                self.file_finder_preview = None;
                return;
            },
        };

        // Build absolute path
        let root = self.worktree.lock().snapshot().root().to_path_buf();
        let abs_path = root.join(&relative_path);
        let abs_path_for_highlight = abs_path.clone();

        // Spawn async task with WeakEntity<Self> handle
        // This is the key pattern: cx.spawn gives us self handle!
        self.file_finder_preview_task = Some(cx.spawn(async move |this, cx| {
            // Phase 1: Load plain text immediately
            if let Some(text) = load_text_only(&abs_path).await {
                // Update self through entity handle
                let _ = this.update(cx, |stoat, cx| {
                    stoat.file_finder_preview = Some(PreviewData::Plain(text));
                    cx.notify();
                });
            }

            // Phase 2: Load syntax-highlighted version
            if let Some(highlighted) = load_file_preview(&abs_path_for_highlight).await {
                let _ = this.update(cx, |stoat, cx| {
                    stoat.file_finder_preview = Some(highlighted);
                    cx.notify();
                });
            }
        }));
    }

    /// Filter files based on query
    pub fn filter_files(&mut self, query: &str, cx: &mut Context<Self>) {
        if query.is_empty() {
            // No query: show all files
            self.file_finder_filtered = self
                .file_finder_files
                .iter()
                .map(|e| PathBuf::from(e.path.as_unix_str()))
                .collect();
        } else {
            // Fuzzy match
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

            let candidates: Vec<&str> = self
                .file_finder_files
                .iter()
                .map(|e| e.path.as_unix_str())
                .collect();

            let mut matches = pattern.match_list(candidates, &mut self.file_finder_matcher);
            matches.sort_by(|a, b| b.1.cmp(&a.1));
            matches.truncate(100);

            self.file_finder_filtered = matches
                .into_iter()
                .map(|(path, _score)| PathBuf::from(path))
                .collect();
        }

        // Reset selection
        self.file_finder_selected = 0;

        // Load preview for newly selected (top) file
        self.load_preview_for_selected(cx);

        cx.notify();
    }

    // ==== File finder state accessors ====

    /// Get file finder input buffer
    pub fn file_finder_input(&self) -> Option<&gpui::Entity<Buffer>> {
        self.file_finder_input.as_ref()
    }

    /// Get filtered files
    pub fn file_finder_filtered(&self) -> &[PathBuf] {
        &self.file_finder_filtered
    }

    /// Get selected index
    pub fn file_finder_selected(&self) -> usize {
        self.file_finder_selected
    }

    /// Get preview data
    pub fn file_finder_preview(&self) -> Option<&PreviewData> {
        self.file_finder_preview.as_ref()
    }

    // ==== Selection actions ====

    /// Select the next symbol from the current cursor position.
    ///
    /// Skips whitespace, punctuation, and operators to find the next alphanumeric token
    /// (identifier, keyword, or number). The selection is created without changing editor mode.
    ///
    /// # Symbol Types
    ///
    /// Selects any of:
    /// - Identifiers: `foo`, `bar_baz`, `MyType`
    /// - Keywords: `fn`, `let`, `struct`
    /// - Numbers: `42`, `3.14`
    ///
    /// # Behavior
    ///
    /// - Skips whitespace, newlines, punctuation, and operators
    /// - Selects the entire symbol (respects token boundaries)
    /// - If cursor is mid-symbol, selects remainder of current symbol
    /// - If no next symbol exists, does nothing
    ///
    /// # Related
    ///
    /// See also [`Self::select_next_token`] for token-level selection that
    /// includes punctuation and operators.
    pub fn select_next_symbol(&mut self, cx: &mut Context<Self>) {
        use std::ops::Range;
        use text::ToOffset;

        // Get buffer and token snapshots via entity
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        // If there's already a non-empty selection with cursor on left, flip cursor to right
        let current_selection = self.cursor.selection();
        if !current_selection.is_empty() && current_selection.reversed {
            // Cursor is on the left side, flip it to the right
            let start = current_selection.start;
            let end = current_selection.end;

            // Flip cursor to the right (end) side
            let selection = crate::cursor::Selection::new(start, end);
            self.cursor.set_selection(selection);

            cx.notify();
            return;
        }

        let cursor_pos = self.cursor.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        // Create a cursor to iterate through tokens
        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut found_symbol: Option<Range<usize>> = None;

        // Track the first symbol we encounter at cursor position (fallback)
        let mut symbol_at_cursor = None;

        // Iterate through tokens to find the next symbol
        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // Skip tokens that are entirely before the cursor
            if token_end <= cursor_offset {
                token_cursor.next();
                continue;
            }

            // Check if this token is a symbol
            if token.kind.is_symbol() {
                // If we're at the start of a symbol, remember it as fallback
                // but try to find the next symbol first
                if token_start == cursor_offset && symbol_at_cursor.is_none() {
                    symbol_at_cursor = Some((token_start, token_end));
                    token_cursor.next();
                    continue;
                }

                // Found a symbol after the cursor position
                let selection_start = cursor_offset.max(token_start);
                found_symbol = Some(selection_start..token_end);
                break;
            }

            // Not a symbol, keep looking
            token_cursor.next();
        }

        // If we didn't find a symbol after the cursor, use the one at cursor (if any)
        if found_symbol.is_none() {
            if let Some((start, end)) = symbol_at_cursor {
                found_symbol = Some(start..end);
            }
        }

        // If we found a symbol, update the cursor and selection
        if let Some(ref range) = found_symbol {
            let selection_start = buffer_snapshot.offset_to_point(range.start);
            let selection_end = buffer_snapshot.offset_to_point(range.end);

            // Create the selection (cursor on right/end side by default)
            let selection = crate::cursor::Selection::new(selection_start, selection_end);
            self.cursor.set_selection(selection);

            cx.notify();
        }
    }

    /// Select the previous symbol from the current cursor position.
    ///
    /// Skips whitespace, punctuation, and operators to find the previous alphanumeric token
    /// (identifier, keyword, or number). The selection is created without changing editor mode.
    ///
    /// # Symbol Types
    ///
    /// Selects any of:
    /// - Identifiers: `foo`, `bar_baz`, `MyType`
    /// - Keywords: `fn`, `let`, `struct`
    /// - Numbers: `42`, `3.14`
    ///
    /// # Behavior
    ///
    /// - Skips whitespace, newlines, punctuation, and operators
    /// - Selects the entire symbol (respects token boundaries)
    /// - If cursor is mid-symbol, selects from start of symbol to cursor
    /// - If no previous symbol exists, does nothing
    ///
    /// # Related
    ///
    /// See also [`Self::select_next_symbol`] for forward symbol selection.
    pub fn select_prev_symbol(&mut self, cx: &mut Context<Self>) {
        use std::ops::Range;
        use text::ToOffset;

        // Get buffer and token snapshots via entity
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        // If there's already a non-empty selection with cursor on right, flip cursor to left
        let current_selection = self.cursor.selection();
        if !current_selection.is_empty() && !current_selection.reversed {
            // Cursor is on the right side, flip it to the left
            let start = current_selection.start;
            let end = current_selection.end;

            // Flip cursor to the left (start) side
            let selection = crate::cursor::Selection::new(end, start);
            self.cursor.set_selection(selection);

            cx.notify();
            return;
        }

        let cursor_pos = self.cursor.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut prev_symbol: Option<(usize, usize)> = None;

        // Iterate through tokens to find the previous symbol
        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // If we've passed the cursor, we're done
            if token_start >= cursor_offset {
                break;
            }

            // Check if this token is a symbol
            if token.kind.is_symbol() {
                // If cursor is strictly inside this token (mid-token), select from start to cursor
                if token_start < cursor_offset && cursor_offset < token_end {
                    prev_symbol = Some((token_start, cursor_offset));
                    break;
                }

                // Track symbols that end strictly before cursor
                if token_end < cursor_offset {
                    prev_symbol = Some((token_start, token_end));
                }
            }

            token_cursor.next();
        }

        let found_symbol: Option<Range<usize>> = prev_symbol.map(|(start, end)| start..end);

        // If we found a symbol, update the cursor and selection
        if let Some(ref range) = found_symbol {
            let selection_start = buffer_snapshot.offset_to_point(range.start);
            let selection_end = buffer_snapshot.offset_to_point(range.end);

            // Create reversed selection (cursor on left/start side)
            let selection = crate::cursor::Selection::new(selection_end, selection_start);
            self.cursor.set_selection(selection);

            cx.notify();
        }
    }

    /// Select the next token from the current cursor position.
    ///
    /// Selects ANY syntactic token including punctuation, operators, brackets,
    /// identifiers, and keywords. The selection is created without changing editor mode.
    ///
    /// # Token Types
    ///
    /// Selects any of:
    /// - Identifiers: `foo`, `bar_baz`, `MyType`
    /// - Keywords: `fn`, `let`, `struct`
    /// - Numbers: `42`, `3.14`
    /// - Operators: `+`, `-`, `->`, `==`
    /// - Punctuation: `.`, `,`, `;`, `:`
    /// - Brackets: `(`, `)`, `{`, `}`, `[`, `]`
    ///
    /// # Behavior
    ///
    /// - Skips only whitespace and newlines
    /// - Selects the entire token (respects token boundaries)
    /// - If cursor is mid-token, selects remainder of current token
    /// - If no next token exists, does nothing
    /// - Cursor positioned on right/end side of selection
    ///
    /// # Related
    ///
    /// See also [`Self::select_next_symbol`] for symbol-level selection
    /// that skips punctuation and operators.
    pub fn select_next_token(&mut self, cx: &mut Context<Self>) {
        use std::ops::Range;
        use text::ToOffset;

        // Get buffer and token snapshots via entity
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        // If there's already a non-empty selection with cursor on left, flip cursor to right
        let current_selection = self.cursor.selection();
        if !current_selection.is_empty() && current_selection.reversed {
            // Cursor is on the left side, flip it to the right
            let start = current_selection.start;
            let end = current_selection.end;

            // Flip cursor to the right (end) side
            let selection = crate::cursor::Selection::new(start, end);
            self.cursor.set_selection(selection);

            cx.notify();
            return;
        }

        let cursor_pos = self.cursor.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        // Create a cursor to iterate through tokens
        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut found_token: Option<Range<usize>> = None;

        // Iterate through tokens to find the next token
        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // Skip tokens that are entirely before the cursor
            if token_end <= cursor_offset {
                token_cursor.next();
                continue;
            }

            // Check if this token is a non-whitespace token
            if token.kind.is_token() {
                // Select from cursor position to end of token
                let selection_start = cursor_offset.max(token_start);
                found_token = Some(selection_start..token_end);
                break;
            }

            // Not a token (whitespace), keep looking
            token_cursor.next();
        }

        // If we found a token, update the cursor and selection
        if let Some(ref range) = found_token {
            let selection_start = buffer_snapshot.offset_to_point(range.start);
            let selection_end = buffer_snapshot.offset_to_point(range.end);

            // Create the selection (cursor on right/end side by default)
            let selection = crate::cursor::Selection::new(selection_start, selection_end);
            self.cursor.set_selection(selection);

            cx.notify();
        }
    }

    /// Select the previous token from the current cursor position.
    ///
    /// Selects ANY syntactic token including punctuation, operators, brackets,
    /// identifiers, and keywords. The selection is created without changing editor mode.
    ///
    /// # Token Types
    ///
    /// Selects any of:
    /// - Identifiers: `foo`, `bar_baz`, `MyType`
    /// - Keywords: `fn`, `let`, `struct`
    /// - Numbers: `42`, `3.14`
    /// - Operators: `+`, `-`, `->`, `==`
    /// - Punctuation: `.`, `,`, `;`, `:`
    /// - Brackets: `(`, `)`, `{`, `}`, `[`, `]`
    ///
    /// # Behavior
    ///
    /// - Skips only whitespace and newlines
    /// - Selects the entire token (respects token boundaries)
    /// - If cursor is mid-token, selects from start of token to cursor
    /// - If no previous token exists, does nothing
    /// - Cursor positioned on left/start side of selection
    ///
    /// # Related
    ///
    /// See also [`Self::select_prev_symbol`] for symbol-level selection
    /// that skips punctuation and operators.
    pub fn select_prev_token(&mut self, cx: &mut Context<Self>) {
        use std::ops::Range;
        use text::ToOffset;

        // Get buffer and token snapshots via entity
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        // If there's already a non-empty selection with cursor on right, flip cursor to left
        let current_selection = self.cursor.selection();
        if !current_selection.is_empty() && !current_selection.reversed {
            // Cursor is on the right side, flip it to the left
            let start = current_selection.start;
            let end = current_selection.end;

            // Flip cursor to the left (start) side
            let selection = crate::cursor::Selection::new(end, start);
            self.cursor.set_selection(selection);

            cx.notify();
            return;
        }

        let cursor_pos = self.cursor.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut prev_token: Option<(usize, usize)> = None;

        // Iterate through tokens to find the previous token
        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // If we've passed the cursor, we're done
            if token_start >= cursor_offset {
                break;
            }

            // Check if this token is a non-whitespace token
            if token.kind.is_token() {
                // If cursor is strictly inside this token (mid-token), select from start to cursor
                if token_start < cursor_offset && cursor_offset < token_end {
                    prev_token = Some((token_start, cursor_offset));
                    break;
                }

                // Track tokens that end at or before cursor
                if token_end <= cursor_offset {
                    prev_token = Some((token_start, token_end));
                }
            }

            token_cursor.next();
        }

        let found_token: Option<Range<usize>> = prev_token.map(|(start, end)| start..end);

        // If we found a token, update the cursor and selection
        if let Some(ref range) = found_token {
            let selection_start = buffer_snapshot.offset_to_point(range.start);
            let selection_end = buffer_snapshot.offset_to_point(range.end);

            // Create reversed selection (cursor on left/start side)
            let selection = crate::cursor::Selection::new(selection_end, selection_start);
            self.cursor.set_selection(selection);

            cx.notify();
        }
    }

    // ==== Visual mode selection actions ====

    /// Extend selection left by one character.
    ///
    /// In visual mode, moves the selection endpoint left by one character,
    /// extending or shrinking the selection based on direction.
    ///
    /// # Behavior
    ///
    /// - Extends selection left by one character
    /// - Stops at line start
    /// - Updates selection in cursor manager
    ///
    /// # Related
    ///
    /// See also [`Self::select_right`] for right selection extension.
    pub fn select_left(&mut self, cx: &mut Context<Self>) {
        let selection = self.cursor.selection().clone();
        let cursor_pos = selection.cursor_position();

        if cursor_pos.column > 0 {
            let target = text::Point::new(cursor_pos.row, cursor_pos.column - 1);
            let snapshot = self.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let new_pos = snapshot.clip_point(target, Bias::Left);

            let new_selection = if selection.is_empty() {
                // First extension - create selection
                crate::cursor::Selection::new(cursor_pos, new_pos)
            } else {
                // Extend existing selection
                let anchor = selection.anchor_position();
                crate::cursor::Selection::new(anchor, new_pos)
            };
            self.cursor.set_selection(new_selection);
        }

        cx.notify();
    }

    /// Extend selection right by one character.
    ///
    /// In visual mode, moves the selection endpoint right by one character,
    /// extending or shrinking the selection based on direction.
    ///
    /// # Behavior
    ///
    /// - Extends selection right by one character
    /// - Stops at line end
    /// - Updates selection in cursor manager
    ///
    /// # Related
    ///
    /// See also [`Self::select_left`] for left selection extension.
    pub fn select_right(&mut self, cx: &mut Context<Self>) {
        let selection = self.cursor.selection().clone();
        let cursor_pos = selection.cursor_position();

        // Get line length
        let line_len = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer = buffer_item.buffer().read(cx);
            buffer.line_len(cursor_pos.row)
        };

        if cursor_pos.column < line_len {
            let target = text::Point::new(cursor_pos.row, cursor_pos.column + 1);
            let snapshot = self.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let new_pos = snapshot.clip_point(target, Bias::Right);
            let new_selection = if selection.is_empty() {
                // First extension - create selection
                crate::cursor::Selection::new(cursor_pos, new_pos)
            } else {
                // Extend existing selection
                let anchor = selection.anchor_position();
                crate::cursor::Selection::new(anchor, new_pos)
            };
            self.cursor.set_selection(new_selection);
        }

        cx.notify();
    }

    /// Extend selection up by one line.
    ///
    /// In visual mode, moves the selection endpoint up by one line,
    /// extending or shrinking the selection based on direction.
    ///
    /// # Behavior
    ///
    /// - Extends selection up by one line
    /// - Stops at first line
    /// - Maintains goal column when possible
    /// - Updates selection in cursor manager
    ///
    /// # Related
    ///
    /// See also [`Self::select_down`] for down selection extension.
    pub fn select_up(&mut self, cx: &mut Context<Self>) {
        let selection = self.cursor.selection().clone();
        let cursor_pos = selection.cursor_position();

        if cursor_pos.row > 0 {
            let target_row = cursor_pos.row - 1;
            let line_len = {
                let buffer_item = self.active_buffer(cx).read(cx);
                let buffer = buffer_item.buffer().read(cx);
                buffer.line_len(target_row)
            };

            let target_column = self.cursor.goal_column().min(line_len);
            let new_pos = text::Point::new(target_row, target_column);

            let new_selection = if selection.is_empty() {
                // First extension - create selection
                crate::cursor::Selection::new(cursor_pos, new_pos)
            } else {
                // Extend existing selection
                let anchor = selection.anchor_position();
                crate::cursor::Selection::new(anchor, new_pos)
            };

            self.cursor.set_selection(new_selection);
        }

        cx.notify();
    }

    /// Extend selection down by one line.
    ///
    /// In visual mode, moves the selection endpoint down by one line,
    /// extending or shrinking the selection based on direction.
    ///
    /// # Behavior
    ///
    /// - Extends selection down by one line
    /// - Stops at last line
    /// - Maintains goal column when possible
    /// - Updates selection in cursor manager
    ///
    /// # Related
    ///
    /// See also [`Self::select_up`] for up selection extension.
    pub fn select_down(&mut self, cx: &mut Context<Self>) {
        let selection = self.cursor.selection().clone();
        let cursor_pos = selection.cursor_position();

        let target_column = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer = buffer_item.buffer().read(cx);
            let max_row = buffer.max_point().row;

            if cursor_pos.row < max_row {
                let target_row = cursor_pos.row + 1;
                let line_len = buffer.line_len(target_row);
                let target_column = self.cursor.goal_column().min(line_len);
                Some(target_column)
            } else {
                None
            }
        };

        if let Some(target_column) = target_column {
            let new_pos = text::Point::new(cursor_pos.row + 1, target_column);

            let new_selection = if selection.is_empty() {
                // First extension - create selection
                crate::cursor::Selection::new(cursor_pos, new_pos)
            } else {
                // Extend existing selection
                let anchor = selection.anchor_position();
                crate::cursor::Selection::new(anchor, new_pos)
            };

            self.cursor.set_selection(new_selection);
        }

        cx.notify();
    }

    /// Extend selection to start of line.
    ///
    /// In visual mode, extends the selection to column 0 of the current line.
    ///
    /// # Behavior
    ///
    /// - Extends selection to line start (column 0)
    /// - Works from any position on the line
    /// - Updates selection in cursor manager
    ///
    /// # Related
    ///
    /// See also [`Self::select_to_line_end`] for end-of-line selection.
    pub fn select_to_line_start(&mut self, cx: &mut Context<Self>) {
        let selection = self.cursor.selection().clone();
        let cursor_pos = selection.cursor_position();
        let new_pos = text::Point::new(cursor_pos.row, 0);

        let new_selection = if selection.is_empty() {
            // First extension - create selection
            crate::cursor::Selection::new(cursor_pos, new_pos)
        } else {
            // Extend existing selection
            let anchor = selection.anchor_position();
            crate::cursor::Selection::new(anchor, new_pos)
        };

        self.cursor.set_selection(new_selection);
        cx.notify();
    }

    /// Extend selection to end of line.
    ///
    /// In visual mode, extends the selection to the end of the current line.
    ///
    /// # Behavior
    ///
    /// - Extends selection to line end
    /// - Works from any position on the line
    /// - Updates selection in cursor manager
    ///
    /// # Related
    ///
    /// See also [`Self::select_to_line_start`] for start-of-line selection.
    pub fn select_to_line_end(&mut self, cx: &mut Context<Self>) {
        let selection = self.cursor.selection().clone();
        let cursor_pos = selection.cursor_position();

        let line_len = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer = buffer_item.buffer().read(cx);
            buffer.line_len(cursor_pos.row)
        };

        let new_pos = text::Point::new(cursor_pos.row, line_len);

        let new_selection = if selection.is_empty() {
            // First extension - create selection
            crate::cursor::Selection::new(cursor_pos, new_pos)
        } else {
            // Extend existing selection
            let anchor = selection.anchor_position();
            crate::cursor::Selection::new(anchor, new_pos)
        };

        self.cursor.set_selection(new_selection);
        cx.notify();
    }

    // ==== File navigation actions ====

    /// Move cursor to the start of the file.
    ///
    /// Positions the cursor at the very beginning of the buffer (row 0, column 0),
    /// regardless of current position.
    ///
    /// # Behavior
    ///
    /// - Moves cursor to (0, 0)
    /// - Resets goal column for vertical movement
    /// - Works from any position in the buffer
    /// - Triggers scroll animation to make cursor visible
    ///
    /// # Related
    ///
    /// See also [`Self::move_to_file_end`] for end-of-file movement.
    pub fn move_to_file_start(&mut self, cx: &mut Context<Self>) {
        self.cursor.move_to(text::Point::new(0, 0));
        self.ensure_cursor_visible();
        cx.notify();
    }

    /// Move cursor to the end of the file.
    ///
    /// Positions the cursor at the very end of the buffer, after the last character of
    /// the last line.
    ///
    /// # Behavior
    ///
    /// - Moves cursor to last row, end of line
    /// - Position is after the last character
    /// - Resets goal column for vertical movement
    /// - Triggers scroll animation to make cursor visible
    ///
    /// # Related
    ///
    /// See also [`Self::move_to_file_start`] for start-of-file movement.
    pub fn move_to_file_end(&mut self, cx: &mut Context<Self>) {
        // Get buffer snapshot to find last line
        let buffer_snapshot = {
            let buffer_item = self.active_buffer(cx).read(cx);
            buffer_item.buffer().read(cx).snapshot()
        };

        let last_row = buffer_snapshot.row_count().saturating_sub(1);
        let last_line_len = buffer_snapshot.line_len(last_row);
        let new_pos = text::Point::new(last_row, last_line_len);

        self.cursor.move_to(new_pos);
        self.ensure_cursor_visible();
        cx.notify();
    }

    /// Move cursor up by one page (approximately one viewport height).
    ///
    /// Moves the cursor up by the visible line count and animates the viewport to follow.
    /// The page size is determined by the current viewport dimensions.
    ///
    /// # Behavior
    ///
    /// - Moves up by `viewport_lines` rows (defaults to 30 if not set)
    /// - Maintains goal column across the movement
    /// - Clamps to line length if target line is shorter
    /// - Initiates animated scroll to keep cursor visible
    /// - No effect if already at first line
    ///
    /// # Scroll Animation
    ///
    /// The viewport animates smoothly to position the cursor approximately 3 lines from
    /// the top, providing context while avoiding the very top edge.
    ///
    /// # Related
    ///
    /// See also [`Self::page_down`] for downward page movement.
    pub fn page_up(&mut self, cx: &mut Context<Self>) {
        let lines_per_page = self.viewport_lines.unwrap_or(30.0).floor() as u32;

        if lines_per_page == 0 {
            return;
        }

        let current_pos = self.cursor.position();
        let new_row = current_pos.row.saturating_sub(lines_per_page);

        // Get buffer snapshot to clamp column
        let buffer_snapshot = {
            let buffer_item = self.active_buffer(cx).read(cx);
            buffer_item.buffer().read(cx).snapshot()
        };

        let line_len = buffer_snapshot.line_len(new_row);
        let new_column = self.cursor.goal_column().min(line_len);
        let new_pos = text::Point::new(new_row, new_column);

        self.cursor.move_to_with_goal(new_pos);

        // Start animated scroll to keep cursor visible (3 lines from top for context)
        let target_scroll_y = new_row.saturating_sub(3) as f32;
        self.scroll
            .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));

        cx.notify();
    }

    /// Move cursor down by one page (approximately one viewport height).
    ///
    /// Moves the cursor down by the visible line count and animates the viewport to follow.
    /// The page size is determined by the current viewport dimensions.
    ///
    /// # Behavior
    ///
    /// - Moves down by `viewport_lines` rows (defaults to 30 if not set)
    /// - Maintains goal column across the movement
    /// - Clamps to line length if target line is shorter
    /// - Clamps to last line of buffer
    /// - Initiates animated scroll to keep cursor visible
    /// - No effect if already at last line
    ///
    /// # Scroll Animation
    ///
    /// The viewport animates smoothly to position the cursor approximately 3 lines from
    /// the top, providing context while avoiding the very top edge.
    ///
    /// # Related
    ///
    /// See also [`Self::page_up`] for upward page movement.
    pub fn page_down(&mut self, cx: &mut Context<Self>) {
        let lines_per_page = self.viewport_lines.unwrap_or(30.0).floor() as u32;

        if lines_per_page == 0 {
            return;
        }

        // Get buffer snapshot to find max row
        let buffer_snapshot = {
            let buffer_item = self.active_buffer(cx).read(cx);
            buffer_item.buffer().read(cx).snapshot()
        };

        let max_row = buffer_snapshot.row_count().saturating_sub(1);
        let current_pos = self.cursor.position();

        if max_row == 0 {
            return;
        }

        let new_row = (current_pos.row + lines_per_page).min(max_row);
        let line_len = buffer_snapshot.line_len(new_row);
        let new_column = self.cursor.goal_column().min(line_len);
        let new_pos = text::Point::new(new_row, new_column);

        self.cursor.move_to_with_goal(new_pos);

        // Start animated scroll to keep cursor visible (3 lines from top for context)
        let target_scroll_y = new_row.saturating_sub(3) as f32;
        self.scroll
            .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));

        cx.notify();
    }

    // ==== Command palette actions ====

    /// Open the command palette modal.
    ///
    /// Builds a list of all available commands from the keymap bindings and creates
    /// an input buffer for fuzzy search. Transitions to command_palette mode.
    ///
    /// # Arguments
    ///
    /// * `keymap` - The keymap to extract commands from
    ///
    /// # Behavior
    ///
    /// - Saves current mode to restore later
    /// - Builds command list from all keymap bindings
    /// - Creates empty input buffer for search query
    /// - Initializes filtered commands list (initially all commands)
    /// - Sets mode to "command_palette"
    ///
    /// # Related
    ///
    /// See also:
    /// - [`Self::command_palette_dismiss`] - close command palette
    /// - [`Self::command_palette_next`] - navigate down
    /// - [`Self::command_palette_prev`] - navigate up
    /// - [`Self::command_palette_execute`] - execute selected command
    pub fn open_command_palette(&mut self, _keymap: &gpui::Keymap, cx: &mut Context<Self>) {
        debug!(from_mode = self.mode(), "Opening command palette");

        // Save current mode to restore later
        self.command_palette_previous_mode = Some(self.mode.clone());

        // Build command list from action metadata
        let commands = build_command_list();
        debug!(command_count = commands.len(), "Built command list");

        // Create input buffer for search query
        let buffer_id = BufferId::from(NonZeroU64::new(3).unwrap()); // Use ID 3 for command palette
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));

        // Initialize command palette state
        self.command_palette_input = Some(input_buffer);
        self.command_palette_commands = commands.clone();
        self.command_palette_filtered = commands;
        self.command_palette_selected = 0;

        // Enter command_palette mode
        self.key_context = crate::stoat::KeyContext::CommandPalette;
        self.mode = "command_palette".into();
        debug!("Entered command_palette mode");

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Filter commands based on fuzzy search query.
    ///
    /// Uses nucleo fuzzy matching to filter the command list based on the query string.
    /// Searches both command name and description for matches.
    ///
    /// # Arguments
    ///
    /// * `query` - The search query string
    ///
    /// # Behavior
    ///
    /// - If query is empty, shows all commands
    /// - Otherwise, fuzzy matches against "name description" for each command
    /// - Sorts results by match score (best matches first)
    /// - Limits to top 50 results
    /// - Resets selection to first item
    pub fn filter_commands(&mut self, query: &str) {
        tracing::info!("filter_commands called with query: '{}'", query);
        if query.is_empty() {
            // No query: show all commands
            self.command_palette_filtered = self.command_palette_commands.clone();
        } else {
            // Parse pattern for smart fuzzy matching
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

            // Create a temporary matcher for commands (uses default config, not path-specific)
            let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);

            // Build indexed search strings, including aliases
            let indexed_strings: Vec<(usize, String)> = self
                .command_palette_commands
                .iter()
                .enumerate()
                .map(|(idx, cmd)| {
                    let mut search_text = format!("{} {}", cmd.name, cmd.description);
                    // Include aliases in search text
                    for alias in &cmd.aliases {
                        search_text.push(' ');
                        search_text.push_str(alias);
                    }
                    (idx, search_text)
                })
                .collect();

            // Match and score all candidates, checking for exact alias matches
            let mut scored_commands: Vec<(usize, u32)> = indexed_strings
                .iter()
                .filter_map(|(idx, search_text)| {
                    let cmd = &self.command_palette_commands[*idx];

                    // Check for exact alias match (case-insensitive)
                    let query_lower = query.to_lowercase();
                    let has_exact_alias_match = cmd
                        .aliases
                        .iter()
                        .any(|alias| alias.to_lowercase() == query_lower);

                    if has_exact_alias_match {
                        // Perfect match - use maximum score to ensure it appears first
                        tracing::info!(
                            "Exact alias match for '{}': {} (aliases: {:?})",
                            query,
                            cmd.name,
                            cmd.aliases
                        );
                        Some((*idx, u32::MAX))
                    } else {
                        // Regular fuzzy matching
                        let candidates = vec![search_text.as_str()];
                        let matches = pattern.match_list(&candidates, &mut matcher);
                        let result = matches.first().map(|(_, score)| (*idx, *score));
                        if result.is_some() && query == ":q" {
                            tracing::info!(
                                "Fuzzy match for '{}': {} (score: {:?}, aliases: {:?})",
                                query,
                                cmd.name,
                                result.as_ref().map(|(_, s)| s),
                                cmd.aliases
                            );
                        }
                        result
                    }
                })
                .collect();

            // Sort by score (descending - higher score = better match)
            scored_commands.sort_by(|a, b| b.1.cmp(&a.1));

            // Limit to top 50 results
            scored_commands.truncate(50);

            // Convert back to CommandInfo
            self.command_palette_filtered = scored_commands
                .into_iter()
                .map(|(idx, _score)| self.command_palette_commands[idx].clone())
                .collect();
        }

        // Reset selection to first item
        self.command_palette_selected = 0;
    }

    /// Move to the next command in the command palette list.
    ///
    /// Moves the selection highlight down to the next command in the filtered list.
    /// If at the end of the list, stays at the last command.
    pub fn command_palette_next(&mut self, cx: &mut Context<Self>) {
        if self.mode() != "command_palette" {
            return;
        }

        if self.command_palette_selected + 1 < self.command_palette_filtered.len() {
            self.command_palette_selected += 1;
            debug!(
                selected = self.command_palette_selected,
                "Command palette: next"
            );
        }

        cx.notify();
    }

    /// Move to the previous command in the command palette list.
    ///
    /// Moves the selection highlight up to the previous command in the filtered list.
    /// If at the beginning of the list, stays at the first command.
    pub fn command_palette_prev(&mut self, cx: &mut Context<Self>) {
        if self.mode() != "command_palette" {
            return;
        }

        if self.command_palette_selected > 0 {
            self.command_palette_selected -= 1;
            debug!(
                selected = self.command_palette_selected,
                "Command palette: prev"
            );
        }

        cx.notify();
    }

    /// Dismiss the command palette and return to the previous mode.
    ///
    /// Closes the command palette modal, clears all state, and returns
    /// to the mode that was active before opening the palette.
    pub fn command_palette_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode() != "command_palette" {
            return;
        }

        debug!("Dismissing command palette");

        // Clear command palette state
        self.command_palette_input = None;
        self.command_palette_commands.clear();
        self.command_palette_filtered.clear();
        self.command_palette_selected = 0;
        self.command_palette_previous_mode = None;

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Get the TypeId of the currently selected command.
    ///
    /// Returns the TypeId of the selected command's action for dispatch,
    /// or None if the command palette is not open or no command is selected.
    pub fn command_palette_selected_type_id(&self) -> Option<std::any::TypeId> {
        if self.mode() != "command_palette" {
            return None;
        }

        self.command_palette_filtered
            .get(self.command_palette_selected)
            .map(|cmd| cmd.type_id)
    }

    /// Accessor for command palette input buffer (for GUI layer).
    pub fn command_palette_input(&self) -> Option<&gpui::Entity<Buffer>> {
        self.command_palette_input.as_ref()
    }

    /// Accessor for filtered commands list (for GUI layer).
    pub fn command_palette_filtered(&self) -> &[crate::stoat::CommandInfo] {
        &self.command_palette_filtered
    }

    /// Accessor for selected command index (for GUI layer).
    pub fn command_palette_selected(&self) -> usize {
        self.command_palette_selected
    }

    // ==== Git status actions ====

    /// Open git status modal.
    ///
    /// Discovers the git repository for the current file, gathers status information
    /// for all modified files, and enters git_status mode to display them.
    pub fn open_git_status(&mut self, cx: &mut Context<Self>) {
        debug!("Opening git status");

        // Save current mode to restore later
        self.git_status_previous_mode = Some(self.mode.clone());

        // Use worktree root to discover repository
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git_repository::Repository::discover(&root_path).ok() {
            Some(repo) => repo,
            None => {
                debug!("No git repository found");
                return;
            },
        };

        // Gather git status
        let entries = match crate::git_status::gather_git_status(repo.inner()) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::error!("Failed to gather git status: {}", e);
                return;
            },
        };

        let dirty_count = entries.len();
        debug!(file_count = dirty_count, "Gathered git status");

        // Gather branch info
        let branch_info = crate::git_status::gather_git_branch_info(repo.inner());
        if let Some(ref info) = branch_info {
            debug!(
                branch = %info.branch_name,
                ahead = info.ahead,
                behind = info.behind,
                "Gathered git branch info"
            );
        }

        // Initialize git status state
        self.git_status_files = entries;
        self.git_status_filter = crate::git_status::GitStatusFilter::default();
        self.git_status_selected = 0;
        self.git_status_branch_info = branch_info;
        self.git_dirty_count = dirty_count;

        // Enter Git KeyContext and git_status mode
        self.key_context = crate::stoat::KeyContext::Git;
        self.mode = "git_status".into();
        debug!("Entered Git KeyContext with git_status mode");

        // Apply initial filter and load preview
        self.filter_git_status_files(cx);

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Load git diff preview for the currently selected file.
    ///
    /// Spawns an async task to load the diff patch for the selected file.
    /// The task updates the preview state when the diff is ready.
    pub fn load_git_diff_preview(&mut self, cx: &mut Context<Self>) {
        // Cancel existing task
        self.git_status_preview_task = None;

        // Get selected file entry from filtered list
        let entry = match self.git_status_filtered.get(self.git_status_selected) {
            Some(entry) => entry.clone(),
            None => {
                self.git_status_preview = None;
                return;
            },
        };

        // Get repository root path
        let root_path = self.worktree.lock().root().to_path_buf();
        let file_path = entry.path.clone();

        // Spawn async task to load diff
        self.git_status_preview_task = Some(cx.spawn(async move |this, cx| {
            // Load git diff
            if let Some(diff) = crate::git_status::load_git_diff(&root_path, &file_path).await {
                // Update self through entity handle
                let _ = this.update(cx, |stoat, cx| {
                    stoat.git_status_preview = Some(diff);
                    cx.notify();
                });
            }
        }));
    }

    /// Move to next file in git status list.
    pub fn git_status_next(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_status" {
            return;
        }

        if self.git_status_selected + 1 < self.git_status_filtered.len() {
            self.git_status_selected += 1;
            debug!(selected = self.git_status_selected, "Git status: next");
            self.load_git_diff_preview(cx);
            cx.notify();
        }
    }

    /// Move to previous file in git status list.
    pub fn git_status_prev(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_status" {
            return;
        }

        if self.git_status_selected > 0 {
            self.git_status_selected -= 1;
            debug!(selected = self.git_status_selected, "Git status: prev");
            self.load_git_diff_preview(cx);
            cx.notify();
        }
    }

    /// Open selected file from git status.
    pub fn git_status_select(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_status" {
            return;
        }

        if self.git_status_selected < self.git_status_filtered.len() {
            let entry = &self.git_status_filtered[self.git_status_selected];
            let relative_path = &entry.path;
            debug!(file = ?relative_path, "Git status: select");

            // Build absolute path from repository root
            let root_path = self.worktree.lock().root().to_path_buf();
            if let Ok(repo) = crate::git_repository::Repository::discover(&root_path) {
                let abs_path = repo.workdir().join(relative_path);

                // Load the file
                if let Err(e) = self.load_file(&abs_path, cx) {
                    tracing::error!("Failed to load file {:?}: {}", abs_path, e);
                }
            }
        }

        self.git_status_dismiss(cx);
    }

    /// Dismiss git status modal.
    ///
    /// Clears git status state. Mode and KeyContext transitions are now handled
    /// by the [`SetKeyContext`](crate::actions::SetKeyContext) action bound to Escape.
    pub fn git_status_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_status" {
            return;
        }

        debug!("Dismissing git status");

        // Clear git status state
        self.git_status_files.clear();
        self.git_status_selected = 0;
        self.git_status_branch_info = None;
        self.git_status_previous_mode = None;

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Apply current filter to git status files.
    ///
    /// Filters the `git_status_files` list based on the current `git_status_filter` value
    /// and updates `git_status_filtered` with the results. Also resets selection to 0
    /// and loads preview for the first filtered file.
    ///
    /// This method is called:
    /// - When opening git status modal (with initial filter)
    /// - When cycling/changing the filter mode
    ///
    /// # Arguments
    ///
    /// * `cx` - GPUI context for spawning async tasks
    pub fn filter_git_status_files(&mut self, cx: &mut Context<Self>) {
        // Apply filter
        self.git_status_filtered = self
            .git_status_files
            .iter()
            .filter(|entry| self.git_status_filter.matches(entry))
            .cloned()
            .collect();

        // Reset selection to first item
        self.git_status_selected = 0;

        // Load preview for first filtered file
        self.load_git_diff_preview(cx);
    }

    /// Cycle to next git status filter.
    ///
    /// Rotates through filter modes (All, Staged, Unstaged, UnstagedWithUntracked, Untracked)
    /// and re-filters the file list.
    pub fn git_status_cycle_filter(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_status" {
            return;
        }

        self.git_status_filter = self.git_status_filter.next();
        debug!(filter = ?self.git_status_filter, "Git status: cycled filter");

        self.filter_git_status_files(cx);
        cx.notify();
    }

    /// Set git status filter to show all files.
    pub fn git_status_set_filter_all(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_filter" {
            return;
        }

        self.git_status_filter = crate::git_status::GitStatusFilter::All;
        debug!("Git status: set filter to All");

        self.filter_git_status_files(cx);
        self.set_mode("git_status");
        cx.notify();
    }

    /// Set git status filter to show only staged files.
    pub fn git_status_set_filter_staged(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_filter" {
            return;
        }

        self.git_status_filter = crate::git_status::GitStatusFilter::Staged;
        debug!("Git status: set filter to Staged");

        self.filter_git_status_files(cx);
        self.set_mode("git_status");
        cx.notify();
    }

    /// Set git status filter to show only unstaged files (excluding untracked).
    pub fn git_status_set_filter_unstaged(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_filter" {
            return;
        }

        self.git_status_filter = crate::git_status::GitStatusFilter::Unstaged;
        debug!("Git status: set filter to Unstaged");

        self.filter_git_status_files(cx);
        self.set_mode("git_status");
        cx.notify();
    }

    /// Set git status filter to show unstaged and untracked files.
    pub fn git_status_set_filter_unstaged_with_untracked(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_filter" {
            return;
        }

        self.git_status_filter = crate::git_status::GitStatusFilter::UnstagedWithUntracked;
        debug!("Git status: set filter to UnstagedWithUntracked");

        self.filter_git_status_files(cx);
        self.set_mode("git_status");
        cx.notify();
    }

    /// Set git status filter to show only untracked files.
    pub fn git_status_set_filter_untracked(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_filter" {
            return;
        }

        self.git_status_filter = crate::git_status::GitStatusFilter::Untracked;
        debug!("Git status: set filter to Untracked");

        self.filter_git_status_files(cx);
        self.set_mode("git_status");
        cx.notify();
    }

    /// Accessor for git status files (for GUI layer).
    pub fn git_status_files(&self) -> &[crate::git_status::GitStatusEntry] {
        &self.git_status_files
    }

    /// Accessor for filtered git status files (for GUI layer).
    ///
    /// Returns the list of files after the current filter has been applied.
    /// This is the list that should be displayed in the git status modal.
    pub fn git_status_filtered(&self) -> &[crate::git_status::GitStatusEntry] {
        &self.git_status_filtered
    }

    /// Accessor for current git status filter mode (for GUI layer).
    ///
    /// Returns the current filter mode being used to filter the git status files.
    pub fn git_status_filter(&self) -> crate::git_status::GitStatusFilter {
        self.git_status_filter
    }

    /// Accessor for git branch info (for GUI layer).
    pub fn git_status_branch_info(&self) -> Option<&crate::git_status::GitBranchInfo> {
        self.git_status_branch_info.as_ref()
    }

    /// Accessor for selected file index (for GUI layer).
    pub fn git_status_selected(&self) -> usize {
        self.git_status_selected
    }

    /// Accessor for git diff preview (for GUI layer).
    pub fn git_status_preview(&self) -> Option<&crate::git_status::DiffPreviewData> {
        self.git_status_preview.as_ref()
    }

    /// Accessor for git dirty count (number of modified files).
    pub fn git_dirty_count(&self) -> usize {
        self.git_dirty_count
    }

    /// Accessor for current file path (for status bar).
    pub fn current_file_path(&self) -> Option<&std::path::Path> {
        self.current_file_path.as_deref()
    }

    // ==== Help modal actions ====

    /// Open help modal.
    ///
    /// Displays full help modal with comprehensive keybinding reference.
    pub fn open_help_modal(&mut self, cx: &mut Context<Self>) {
        debug!("Opening help modal");

        // Save current mode to restore later
        self.help_modal_previous_mode = Some(self.mode.clone());
        self.key_context = crate::stoat::KeyContext::HelpModal;
        self.mode = "help_modal".to_string();

        cx.notify();
    }

    /// Dismiss help modal.
    ///
    /// Clears help modal state. Mode and KeyContext transitions are now handled
    /// by the [`SetKeyContext`](crate::actions::SetKeyContext) action bound to Escape.
    pub fn help_modal_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "help_modal" {
            return;
        }

        debug!("Dismissing help modal");

        // Clear help modal state
        self.help_modal_previous_mode = None;

        cx.notify();
    }

    // ==== Buffer finder actions ====

    /// Open buffer finder modal.
    ///
    /// Initializes buffer list with currently open buffers and creates input buffer for searching.
    ///
    /// # Arguments
    ///
    /// * `visible_buffer_ids` - Buffer IDs visible in any pane (for visibility flag)
    /// * `cx` - Context for creating entities and reading state
    pub fn open_buffer_finder(&mut self, visible_buffer_ids: &[BufferId], cx: &mut Context<Self>) {
        debug!("Opening buffer finder");

        // Save current mode
        self.buffer_finder_previous_mode = Some(self.mode.clone());
        self.key_context = crate::stoat::KeyContext::BufferFinder;
        self.mode = "buffer_finder".to_string();

        // Create input buffer
        let buffer_id = BufferId::from(NonZeroU64::new(3).unwrap());
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        self.buffer_finder_input = Some(input_buffer);

        // Get all open buffers from BufferStore with status flags
        let buffers =
            self.buffer_store
                .read(cx)
                .buffer_list(self.active_buffer_id, visible_buffer_ids, cx);

        debug!(
            buffer_count = buffers.len(),
            "Loaded buffers from BufferStore"
        );

        self.buffer_finder_buffers = buffers.clone();
        self.buffer_finder_filtered = buffers;
        self.buffer_finder_selected = 0;

        cx.notify();
    }

    /// Move to next buffer in finder.
    pub fn buffer_finder_next(&mut self, cx: &mut Context<Self>) {
        if self.mode != "buffer_finder" {
            return;
        }

        if self.buffer_finder_selected + 1 < self.buffer_finder_filtered.len() {
            self.buffer_finder_selected += 1;
            debug!(
                selected = self.buffer_finder_selected,
                "Buffer finder: next"
            );
            cx.notify();
        }
    }

    /// Move to previous buffer in finder.
    pub fn buffer_finder_prev(&mut self, cx: &mut Context<Self>) {
        if self.mode != "buffer_finder" {
            return;
        }

        if self.buffer_finder_selected > 0 {
            self.buffer_finder_selected -= 1;
            debug!(
                selected = self.buffer_finder_selected,
                "Buffer finder: prev"
            );
            cx.notify();
        }
    }

    /// Select buffer in finder.
    ///
    /// Switches to the selected buffer from BufferStore.
    pub fn buffer_finder_select(&mut self, cx: &mut Context<Self>) {
        if self.mode != "buffer_finder" {
            return;
        }

        if self.buffer_finder_selected < self.buffer_finder_filtered.len() {
            let entry = &self.buffer_finder_filtered[self.buffer_finder_selected];
            debug!(buffer = ?entry.display_name, buffer_id = ?entry.buffer_id, "Buffer finder: switching to buffer");

            // Get buffer from BufferStore by ID
            if let Some(buffer_item) = self.buffer_store.read(cx).get_buffer(entry.buffer_id) {
                let buffer_id = buffer_item.read(cx).buffer().read(cx).remote_id();

                // Update active_buffer_id
                self.active_buffer_id = Some(buffer_id);

                // Update current_file_path for status bar (None for unnamed buffers)
                self.current_file_path = entry.path.clone();

                // Update activation history
                self.buffer_store
                    .update(cx, |store, _cx| store.activate_buffer(buffer_id));

                // Reset cursor to beginning (could be improved to save/restore per-buffer cursor)
                self.cursor.move_to(text::Point::new(0, 0));

                debug!(buffer_id = ?buffer_id, "Switched to buffer");
            } else {
                tracing::error!("Buffer not found in BufferStore: {:?}", entry.buffer_id);
            }
        }

        self.buffer_finder_dismiss(cx);
    }

    /// Dismiss buffer finder.
    ///
    /// Clears buffer finder state. Mode and KeyContext transitions are now handled
    /// by the [`SetKeyContext`](crate::actions::SetKeyContext) action bound to Escape.
    pub fn buffer_finder_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "buffer_finder" {
            return;
        }

        debug!("Dismissing buffer finder");

        // Clear buffer finder state
        self.buffer_finder_input = None;
        self.buffer_finder_buffers.clear();
        self.buffer_finder_filtered.clear();
        self.buffer_finder_selected = 0;
        self.buffer_finder_previous_mode = None;

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Filter buffers based on query string.
    pub fn filter_buffers(&mut self, query: &str, cx: &mut Context<Self>) {
        if query.is_empty() {
            // No query: show all buffers
            self.buffer_finder_filtered = self.buffer_finder_buffers.clone();
        } else {
            // Fuzzy match on buffer display names
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

            let candidates: Vec<&str> = self
                .buffer_finder_buffers
                .iter()
                .map(|entry| entry.display_name.as_str())
                .collect();

            let mut matches = pattern.match_list(candidates, &mut self.file_finder_matcher);
            matches.sort_by(|a, b| b.1.cmp(&a.1));

            self.buffer_finder_filtered = matches
                .into_iter()
                .map(|(display_name, _score)| {
                    // Find the original BufferListEntry by display_name
                    self.buffer_finder_buffers
                        .iter()
                        .find(|entry| entry.display_name == display_name)
                        .cloned()
                        .expect("Matched entry should exist in buffer_finder_buffers")
                })
                .collect();
        }

        // Reset selection
        self.buffer_finder_selected = 0;

        cx.notify();
    }

    // ==== Buffer finder state accessors ====

    /// Get buffer finder input buffer.
    pub fn buffer_finder_input(&self) -> Option<&gpui::Entity<Buffer>> {
        self.buffer_finder_input.as_ref()
    }

    /// Get filtered buffer list.
    pub fn buffer_finder_filtered(&self) -> &[crate::buffer_store::BufferListEntry] {
        &self.buffer_finder_filtered
    }

    /// Get selected buffer index.
    pub fn buffer_finder_selected(&self) -> usize {
        self.buffer_finder_selected
    }

    // ==== Git Diff Actions ====

    /// Toggle expansion of diff hunk at cursor position (no-op).
    ///
    /// NOTE: This action is now a no-op. In the new phantom row design, all diff hunks
    /// are always visible with their deleted content shown inline. There is no concept
    /// of collapsed/expanded hunks anymore.
    pub fn toggle_diff_hunk(&mut self, _cx: &mut Context<Self>) {
        tracing::debug!("toggle_diff_hunk called (no-op - all hunks always visible)");
    }

    /// Jump to the next diff hunk.
    ///
    /// Moves the cursor to the start of the next git diff hunk after the current position.
    /// Wraps around to the first hunk if at the end of the file.
    pub fn goto_next_hunk(&mut self, cx: &mut Context<Self>) {
        let cursor_row = self.cursor.position().row;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        let diff = buffer_item.read(cx).diff();
        if let Some(diff) = diff {
            // Find next hunk after cursor
            let next_hunk = diff
                .hunks
                .iter()
                .find(|hunk| {
                    let hunk_start_row = hunk.buffer_range.start.to_point(&buffer_snapshot).row;
                    hunk_start_row > cursor_row
                })
                .or_else(|| diff.hunks.first()); // Wrap to first hunk

            if let Some(hunk) = next_hunk {
                let target_row = hunk.buffer_range.start.to_point(&buffer_snapshot).row;
                self.cursor.move_to(text::Point::new(target_row, 0));
                self.ensure_cursor_visible();

                tracing::debug!("Jumped to next diff hunk at row {}", target_row);
                cx.emit(crate::stoat::StoatEvent::Changed);
                cx.notify();
            }
        }
    }

    /// Jump to the previous diff hunk.
    ///
    /// Moves the cursor to the start of the previous git diff hunk before the current position.
    /// Wraps around to the last hunk if at the beginning of the file.
    pub fn goto_prev_hunk(&mut self, cx: &mut Context<Self>) {
        let cursor_row = self.cursor.position().row;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        let diff = buffer_item.read(cx).diff();
        if let Some(diff) = diff {
            // Find previous hunk before cursor
            let prev_hunk = diff
                .hunks
                .iter()
                .rev()
                .find(|hunk| {
                    let hunk_start_row = hunk.buffer_range.start.to_point(&buffer_snapshot).row;
                    hunk_start_row < cursor_row
                })
                .or_else(|| diff.hunks.last()); // Wrap to last hunk

            if let Some(hunk) = prev_hunk {
                let target_row = hunk.buffer_range.start.to_point(&buffer_snapshot).row;
                self.cursor.move_to(text::Point::new(target_row, 0));
                self.ensure_cursor_visible();

                tracing::debug!("Jumped to previous diff hunk at row {}", target_row);
                cx.emit(crate::stoat::StoatEvent::Changed);
                cx.notify();
            }
        }
    }

    // ==== Diff review actions ====

    /// Open diff review mode.
    ///
    /// Scans the repository for all modified files, pre-computes diffs for all files
    /// in the current comparison mode, and enters diff_review mode for hunk-by-hunk review.
    ///
    /// Following Zed's ProjectDiff pattern but simplified for stoat's modal architecture.
    pub fn open_diff_review(&mut self, cx: &mut Context<Self>) {
        tracing::info!("Opening diff review");
        debug!("Opening diff review");

        // Save current mode to restore later
        self.diff_review_previous_mode = Some(self.mode.clone());

        // Use worktree root to discover repository
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git_repository::Repository::discover(&root_path).ok() {
            Some(repo) => repo,
            None => {
                debug!("No git repository found");
                return;
            },
        };

        // Check if we have existing review state to restore
        if !self.diff_review_files.is_empty() {
            // Restore previous review session
            debug!(
                "Restoring review session at file {}, hunk {}",
                self.diff_review_current_file_idx, self.diff_review_current_hunk_idx
            );

            // Load the saved file
            if let Some(saved_file_path) = self
                .diff_review_files
                .get(self.diff_review_current_file_idx)
            {
                let abs_path = repo.workdir().join(saved_file_path);

                if let Err(e) = self.load_file(&abs_path, cx) {
                    tracing::error!("Failed to load saved file {:?}: {}", abs_path, e);
                    return;
                }

                // Compute diff respecting the comparison mode
                if let Some(diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
                    // Update the buffer item's diff for display
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, _| {
                        item.set_diff(Some(diff));
                    });
                }
            }

            // Enter diff_review mode
            self.key_context = crate::stoat::KeyContext::DiffReview;
            self.mode = "diff_review".to_string();

            // Jump to saved hunk
            self.jump_to_current_hunk(cx);

            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
            return;
        }

        // No existing state - start fresh review session
        // Scan git status to get list of modified files
        let entries = match crate::git_status::gather_git_status(repo.inner()) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::error!("Failed to gather git status: {}", e);
                return;
            },
        };

        if entries.is_empty() {
            debug!("No modified files to review");
            return;
        }

        // Deduplicate files and store paths
        let mut seen = std::collections::HashSet::new();
        let file_paths: Vec<std::path::PathBuf> = entries
            .into_iter()
            .filter(|e| seen.insert(e.path.clone()))
            .map(|e| e.path)
            .collect();

        if file_paths.is_empty() {
            debug!("No unique files to review");
            return;
        }

        // Store file list
        self.diff_review_files = file_paths.clone();

        // Find first file with hunks by loading and checking on-demand
        let mut first_file_idx = None;
        for (idx, file_path) in file_paths.iter().enumerate() {
            let abs_path = repo.workdir().join(file_path);

            // Load file
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                continue;
            }

            // Compute diff
            if let Some(diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
                if !diff.hunks.is_empty() {
                    // Found first file with hunks
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, _| {
                        item.set_diff(Some(diff.clone()));
                    });

                    first_file_idx = Some(idx);
                    tracing::info!(
                        "Diff review: found first file with {} hunks in {} mode",
                        diff.hunks.len(),
                        self.diff_review_comparison_mode.display_name()
                    );
                    break;
                }
            }
        }

        let first_idx = match first_file_idx {
            Some(idx) => idx,
            None => {
                debug!("No files with hunks in current comparison mode");
                self.diff_review_files.clear();
                return;
            },
        };

        // Initialize state to start at first file with hunks
        self.diff_review_current_file_idx = first_idx;
        self.diff_review_current_hunk_idx = 0;
        self.diff_review_approved_hunks.clear();

        // Enter diff_review mode
        self.key_context = crate::stoat::KeyContext::DiffReview;
        self.mode = "diff_review".to_string();

        // Jump to first hunk
        self.jump_to_current_hunk(cx);

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Jump to next hunk in diff review mode.
    ///
    /// Navigates to the next hunk (whether reviewed or not).
    /// Automatically loads the next file if at the last hunk of current file.
    /// Following Zed's hunk navigation pattern with cross-file support.
    pub fn diff_review_next_hunk(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        // Get hunk count from buffer diff
        let hunk_count = {
            let buffer_item = self.active_buffer(cx);
            let item = buffer_item.read(cx);
            match item.diff() {
                Some(diff) => diff.hunks.len(),
                None => return,
            }
        };

        tracing::debug!(
            "diff_review_next_hunk: file_idx={}, hunk_idx={}, hunk_count={}",
            self.diff_review_current_file_idx,
            self.diff_review_current_hunk_idx,
            hunk_count
        );

        // Try to move to next hunk in current file
        if self.diff_review_current_hunk_idx + 1 < hunk_count {
            // Move to next hunk in current file
            self.diff_review_current_hunk_idx += 1;
            tracing::debug!("Moving to next hunk: {}", self.diff_review_current_hunk_idx);
            self.jump_to_current_hunk(cx);
        } else {
            // At last hunk, try next file
            tracing::debug!("At last hunk, loading next file");
            self.load_next_file(cx);
        }

        cx.notify();
    }

    /// Jump to previous hunk in diff review mode.
    ///
    /// Navigates to the previous hunk (whether reviewed or not).
    /// Automatically loads the previous file if at the first hunk of current file.
    pub fn diff_review_prev_hunk(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        if self.diff_review_files.is_empty() {
            return;
        }

        if self.diff_review_current_hunk_idx > 0 {
            // Go to previous hunk in current file
            self.diff_review_current_hunk_idx -= 1;
            self.jump_to_current_hunk(cx);
        } else {
            // Go to previous file's last hunk
            self.load_prev_file(cx);
        }

        cx.notify();
    }

    /// Approve current hunk and jump to next unreviewed hunk.
    ///
    /// Marks the current hunk as reviewed and automatically navigates to the next
    /// unreviewed hunk. Combines marking + navigation for efficient review workflow.
    pub fn diff_review_approve_hunk(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        // Get current file path
        let current_file_path = match self
            .diff_review_files
            .get(self.diff_review_current_file_idx)
        {
            Some(path) => path.clone(),
            None => return,
        };

        // Mark current hunk as approved
        self.diff_review_approved_hunks
            .entry(current_file_path.clone())
            .or_default()
            .insert(self.diff_review_current_hunk_idx);

        debug!(
            file = ?current_file_path,
            hunk = self.diff_review_current_hunk_idx,
            "Approved hunk"
        );

        // Move to next unreviewed hunk
        self.diff_review_next_hunk(cx);
    }

    /// Exit diff review mode.
    ///
    /// Clears the previous mode reference. Mode and KeyContext transitions are now
    /// handled by the [`SetKeyContext`](crate::actions::SetKeyContext) action bound to Escape.
    ///
    /// State persists for next review session (files, hunks, approved state).
    /// To fully reset review progress, use
    /// [`DiffReviewResetProgress`](crate::actions::DiffReviewResetProgress).
    pub fn diff_review_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        debug!("Dismissing diff review");

        // Clear previous mode reference
        self.diff_review_previous_mode = None;

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Toggle approval status of current hunk.
    ///
    /// Toggles the current hunk between reviewed and not reviewed. Stays on the
    /// current hunk (doesn't advance). Useful for marking things you've already seen.
    pub fn diff_review_toggle_approval(&mut self, _cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        // Get current file path
        let current_file_path = match self
            .diff_review_files
            .get(self.diff_review_current_file_idx)
        {
            Some(path) => path.clone(),
            None => return,
        };

        let approved_hunks = self
            .diff_review_approved_hunks
            .entry(current_file_path.clone())
            .or_default();

        if approved_hunks.contains(&self.diff_review_current_hunk_idx) {
            // Currently approved - unapprove it
            approved_hunks.remove(&self.diff_review_current_hunk_idx);
            debug!(
                file = ?current_file_path,
                hunk = self.diff_review_current_hunk_idx,
                "Unapproved hunk"
            );
        } else {
            // Not approved - approve it
            approved_hunks.insert(self.diff_review_current_hunk_idx);
            debug!(
                file = ?current_file_path,
                hunk = self.diff_review_current_hunk_idx,
                "Approved hunk"
            );
        }
    }

    /// Jump to next unreviewed hunk across all files.
    ///
    /// Searches files on-demand for the next unreviewed hunk. Loads each file,
    /// computes diff, and checks for unreviewed hunks. Wraps around to the beginning
    /// if needed. Exits review mode if all hunks reviewed.
    pub fn diff_review_next_unreviewed_hunk(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        if self.diff_review_files.is_empty() {
            return;
        }

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git_repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };

        let start_file = self.diff_review_current_file_idx;
        let start_hunk = self.diff_review_current_hunk_idx + 1; // Start from next hunk
        let file_count = self.diff_review_files.len();

        let empty_set = std::collections::HashSet::new();

        // Helper to check if a file has an unreviewed hunk at/after start_hunk_idx
        let find_unreviewed_in_file =
            |file_path: &std::path::PathBuf, start_hunk_idx: usize| -> Option<usize> {
                let approved_hunks = self
                    .diff_review_approved_hunks
                    .get(file_path)
                    .unwrap_or(&empty_set);

                // Get hunk count from current buffer diff if this is the current file
                let hunk_count = {
                    let buffer_item = self.active_buffer(cx);
                    let item = buffer_item.read(cx);
                    item.diff().map(|d| d.hunks.len()).unwrap_or(0)
                };

                (start_hunk_idx..hunk_count).find(|idx| !approved_hunks.contains(idx))
            };

        // Search in current file first (from start_hunk onward)
        if let Some(current_file_path) = self.diff_review_files.get(start_file) {
            if let Some(hunk_idx) = find_unreviewed_in_file(current_file_path, start_hunk) {
                self.diff_review_current_hunk_idx = hunk_idx;
                self.jump_to_current_hunk(cx);
                cx.notify();
                return;
            }
        }

        // Search remaining files (load each on-demand)
        for offset in 1..file_count {
            let file_idx = (start_file + offset) % file_count;
            if file_idx == start_file {
                break; // Back to start - handle this case separately
            }

            // Clone file path to avoid borrow conflicts
            let file_path = self.diff_review_files[file_idx].clone();
            let abs_path = repo.workdir().join(&file_path);

            // Load file
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                continue;
            }

            // Compute diff
            if let Some(diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
                let buffer_item = self.active_buffer(cx);
                buffer_item.update(cx, |item, _| {
                    item.set_diff(Some(diff.clone()));
                });

                // Check for unreviewed hunks in this file
                let approved_hunks = self
                    .diff_review_approved_hunks
                    .get(&file_path)
                    .unwrap_or(&empty_set);

                if let Some(hunk_idx) =
                    (0..diff.hunks.len()).find(|idx| !approved_hunks.contains(idx))
                {
                    self.diff_review_current_file_idx = file_idx;
                    self.diff_review_current_hunk_idx = hunk_idx;
                    self.jump_to_current_hunk(cx);
                    cx.notify();
                    return;
                }
            }
        }

        // Search current file from beginning up to start_hunk
        if let Some(current_file_path) = self.diff_review_files.get(start_file) {
            let approved_hunks = self
                .diff_review_approved_hunks
                .get(current_file_path)
                .unwrap_or(&empty_set);

            if let Some(hunk_idx) = (0..start_hunk).find(|idx| !approved_hunks.contains(idx)) {
                self.diff_review_current_hunk_idx = hunk_idx;
                self.jump_to_current_hunk(cx);
                cx.notify();
                return;
            }
        }

        // No unreviewed hunks found - all review complete
        debug!("All hunks reviewed");
        self.diff_review_dismiss(cx);
    }

    /// Reset all review progress and start from beginning.
    ///
    /// Clears all approved hunks and resets to file 0, hunk 0.
    /// Use this to start a fresh review pass.
    pub fn diff_review_reset_progress(&mut self, cx: &mut Context<Self>) {
        debug!("Resetting diff review progress");

        // Clear all approved hunks
        self.diff_review_approved_hunks.clear();

        // If in review mode, load first file and jump to first hunk
        if self.mode == "diff_review" && !self.diff_review_files.is_empty() {
            let root_path = self.worktree.lock().root().to_path_buf();
            if let Ok(repo) = crate::git_repository::Repository::discover(&root_path) {
                // Clone file list to avoid borrow conflicts
                let files = self.diff_review_files.clone();
                // Find first file with hunks by loading files on-demand
                for (idx, file_path) in files.iter().enumerate() {
                    let abs_path = repo.workdir().join(file_path);

                    // Load file
                    if let Err(e) = self.load_file(&abs_path, cx) {
                        tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                        continue;
                    }

                    // Compute diff
                    if let Some(diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
                        if !diff.hunks.is_empty() {
                            // Found first file with hunks
                            let buffer_item = self.active_buffer(cx);
                            buffer_item.update(cx, |item, _| {
                                item.set_diff(Some(diff.clone()));
                            });

                            // Reset to start
                            self.diff_review_current_file_idx = idx;
                            self.diff_review_current_hunk_idx = 0;

                            self.jump_to_current_hunk(cx);
                            cx.notify();
                            return;
                        }
                    }
                }
            }
        }

        cx.notify();
    }

    /// Cycle through diff comparison modes in diff review.
    ///
    /// Rotates through WorkingVsHead -> WorkingVsIndex -> IndexVsHead -> WorkingVsHead.
    /// Recomputes the diff for the new comparison mode using the centralized hotpath.
    ///
    /// # Related
    ///
    /// - [`Stoat::cycle_diff_comparison_mode`] - Cycles the mode setting
    /// - [`Stoat::compute_diff_for_review_mode`] - Centralized diff computation hotpath
    pub fn diff_review_cycle_comparison_mode(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        debug!("Cycling diff comparison mode");

        // Cycle to next mode
        self.cycle_diff_comparison_mode();
        let new_mode = self.diff_comparison_mode();
        debug!("New comparison mode: {:?}", new_mode);

        // Get current file path
        let current_file_path = match self
            .diff_review_files
            .get(self.diff_review_current_file_idx)
        {
            Some(path) => path.clone(),
            None => return,
        };

        // Get absolute path
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git_repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };
        let abs_path = repo.workdir().join(&current_file_path);

        // Recompute diff for new comparison mode
        if let Some(new_diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
            // Update the buffer item diff
            let buffer_item = self.active_buffer(cx);
            buffer_item.update(cx, |item, _cx| {
                item.set_diff(Some(new_diff.clone()));
            });

            // Reset hunk index if it's now out of range
            let hunk_count = new_diff.hunks.len();
            if self.diff_review_current_hunk_idx >= hunk_count {
                self.diff_review_current_hunk_idx = if hunk_count > 0 { 0 } else { 0 };
            }

            // Jump to current hunk (or first hunk if current is out of range)
            self.jump_to_current_hunk(cx);
        }

        cx.emit(crate::stoat::StoatEvent::Changed);
    }

    // Helper methods for diff review

    /// Jump cursor to the start of the current hunk.
    ///
    /// Uses the current file and hunk indices to position the cursor and scroll
    /// the view to show the hunk. Following Zed's go_to_hunk pattern.
    ///
    /// Implements smart scrolling:
    /// - If hunk fits in viewport: centers the hunk
    /// - If hunk is too large: positions top of hunk at 1/3 from viewport top
    fn jump_to_current_hunk(&mut self, cx: &mut Context<Self>) {
        // Get the diff from the buffer item (has fresh anchors) instead of GitIndex (has stale
        // anchors)
        let buffer_item = self.active_buffer(cx);
        let (diff, buffer_snapshot) = {
            let item = buffer_item.read(cx);
            let diff = match item.diff() {
                Some(d) => d.clone(),
                None => return,
            };
            let buffer_snapshot = item.buffer().read(cx).snapshot();
            (diff, buffer_snapshot)
        };

        if self.diff_review_current_hunk_idx >= diff.hunks.len() {
            return;
        }

        let hunk = &diff.hunks[self.diff_review_current_hunk_idx];

        // Convert hunk anchors to points
        let hunk_start = hunk.buffer_range.start.to_point(&buffer_snapshot);
        let hunk_end = hunk.buffer_range.end.to_point(&buffer_snapshot);

        let hunk_idx = self.diff_review_current_hunk_idx;
        let start_row = hunk_start.row;

        // Move cursor to hunk start
        self.cursor.move_to(hunk_start);

        // Smart scrolling based on hunk size
        if let Some(viewport_lines) = self.viewport_lines {
            let hunk_height = (hunk_end.row - hunk_start.row) as f32;

            // Only center small hunks (less than ~40% of viewport)
            // Larger hunks get positioned near top with padding
            let target_scroll_y = if hunk_height < viewport_lines * 0.4 {
                // Small hunk - center it
                let hunk_middle = hunk_start.row as f32 + (hunk_height / 2.0);
                (hunk_middle - (viewport_lines / 2.0)).max(0.0)
            } else {
                // Larger hunk - position near top with padding (like normal cursor)
                const TOP_PADDING: f32 = 3.0;
                (hunk_start.row as f32 - TOP_PADDING).max(0.0)
            };

            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        } else {
            // No viewport info - fall back to basic visibility check
            self.ensure_cursor_visible();
        }

        debug!(hunk = hunk_idx, line = start_row, "Jumped to hunk");
    }

    /// Load next file in diff review.
    ///
    /// Uses pre-computed indices from GitIndex for O(1) navigation to the next file with hunks.
    /// Wraps to first file if at the end.
    fn load_next_file(&mut self, cx: &mut Context<Self>) {
        if self.diff_review_files.is_empty() {
            return;
        }

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git_repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };

        let file_count = self.diff_review_files.len();
        let current_idx = self.diff_review_current_file_idx;

        // Loop through files starting from next one, looking for one with hunks
        for offset in 1..=file_count {
            let next_idx = (current_idx + offset) % file_count;
            let file_path = &self.diff_review_files[next_idx];
            let abs_path = repo.workdir().join(file_path);

            // Load file
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                continue;
            }

            // Compute diff and check if it has hunks
            if let Some(diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
                if !diff.hunks.is_empty() {
                    // Found file with hunks - set it and jump to first hunk
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, _| {
                        item.set_diff(Some(diff.clone()));
                    });

                    debug!(
                        "Loaded next file with {} hunks at idx={}",
                        diff.hunks.len(),
                        next_idx
                    );

                    self.diff_review_current_file_idx = next_idx;
                    self.diff_review_current_hunk_idx = 0;
                    self.jump_to_current_hunk(cx);
                    return;
                }
            }
        }

        debug!("No more files with hunks in current comparison mode");
    }

    /// Load previous file in diff review.
    ///
    /// Uses pre-computed indices from GitIndex for O(1) navigation to the previous file with hunks.
    /// Wraps to last file if at the beginning.
    fn load_prev_file(&mut self, cx: &mut Context<Self>) {
        if self.diff_review_files.is_empty() {
            return;
        }

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git_repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };

        let file_count = self.diff_review_files.len();
        let current_idx = self.diff_review_current_file_idx;

        // Loop through files backwards starting from previous one, looking for one with hunks
        for offset in 1..=file_count {
            let prev_idx = if current_idx >= offset {
                current_idx - offset
            } else {
                file_count - (offset - current_idx)
            };

            let file_path = &self.diff_review_files[prev_idx];
            let abs_path = repo.workdir().join(file_path);

            // Load file
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                continue;
            }

            // Compute diff and check if it has hunks
            if let Some(diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
                if !diff.hunks.is_empty() {
                    // Found file with hunks - set it and jump to last hunk
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, _| {
                        item.set_diff(Some(diff.clone()));
                    });

                    debug!(
                        "Loaded prev file with {} hunks at idx={}",
                        diff.hunks.len(),
                        prev_idx
                    );

                    // Jump to last hunk in previous file
                    let last_hunk_idx = diff.hunks.len().saturating_sub(1);
                    self.diff_review_current_file_idx = prev_idx;
                    self.diff_review_current_hunk_idx = last_hunk_idx;
                    self.jump_to_current_hunk(cx);
                    return;
                }
            }
        }

        debug!("No more files with hunks in current comparison mode");
    }
}

/// Build the list of all available commands from action metadata.
///
/// Iterates through all registered actions with metadata and builds command information
/// including name, description, aliases, and TypeId for dispatch. This includes all
/// actions with metadata, regardless of whether they have keybindings.
///
/// # Returns
///
/// A vector of [`CommandInfo`] structs representing all available commands
fn build_command_list() -> Vec<crate::stoat::CommandInfo> {
    let mut commands = Vec::new();

    // Iterate through all actions with metadata
    for (type_id, name) in crate::actions::ACTION_NAMES.iter() {
        // Get description - skip if not available
        let Some(description) = crate::actions::DESCRIPTIONS.get(type_id) else {
            continue;
        };

        // Get aliases (empty slice if none)
        let aliases = crate::actions::ALIASES
            .get(type_id)
            .copied()
            .unwrap_or(&[])
            .to_vec();

        if !aliases.is_empty() {
            tracing::info!("Command {} has aliases: {:?}", name, aliases);
        }

        commands.push(crate::stoat::CommandInfo {
            name: name.to_string(),
            description: description.to_string(),
            aliases,
            type_id: *type_id,
        });
    }

    // Sort alphabetically by name
    commands.sort_by(|a, b| a.name.cmp(&b.name));

    commands
}
