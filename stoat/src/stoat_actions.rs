//! Action implementations for Stoat.
//!
//! These demonstrate the Context<Self> pattern - methods can spawn self-updating tasks.

use crate::{
    file_finder::{load_file_preview, load_text_only, PreviewData},
    stoat::Stoat,
};
use gpui::{AppContext, Context};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use std::{num::NonZeroU64, path::PathBuf};
use text::{Bias, Buffer, BufferId};
use tracing::debug;

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
            let token_start = token.range.start.to_offset(&buffer_snapshot);
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
                .buffer_item
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
                .buffer_item
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
            .buffer_item
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
            let token_start = token.range.start.to_offset(&buffer_snapshot);
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
            .buffer_item
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
        debug!("Entering space mode");
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

    // ==== File finder actions ====

    /// Open file finder.
    ///
    /// This demonstrates Context<Self> - can create entities and scan worktree.
    pub fn open_file_finder(&mut self, cx: &mut Context<Self>) {
        debug!("Opening file finder");

        // Save current mode
        self.file_finder_previous_mode = Some(self.mode.clone());
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

    /// Dismiss file finder
    pub fn file_finder_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        debug!("Dismissing file finder");

        // Restore previous mode - check if mode has configured previous override
        self.mode = if let Some(mode_def) = self.modes.get("file_finder") {
            if let Some(previous) = &mode_def.previous {
                previous.clone()
            } else {
                self.file_finder_previous_mode
                    .take()
                    .unwrap_or_else(|| "normal".to_string())
            }
        } else {
            self.file_finder_previous_mode
                .take()
                .unwrap_or_else(|| "normal".to_string())
        };

        // Clear state
        self.file_finder_input = None;
        self.file_finder_files.clear();
        self.file_finder_filtered.clear();
        self.file_finder_selected = 0;
        self.file_finder_preview = None;
        self.file_finder_preview_task = None;

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
    pub fn open_command_palette(&mut self, keymap: &gpui::Keymap, cx: &mut Context<Self>) {
        debug!(from_mode = self.mode(), "Opening command palette");

        // Save current mode to restore later
        self.command_palette_previous_mode = Some(self.mode.clone());

        // Build command list from keymap
        let commands = build_command_list(keymap);
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
        if query.is_empty() {
            // No query: show all commands
            self.command_palette_filtered = self.command_palette_commands.clone();
        } else {
            // Parse pattern for smart fuzzy matching
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

            // Create a temporary matcher for commands (uses default config, not path-specific)
            let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);

            // Build indexed search strings
            let indexed_strings: Vec<(usize, String)> = self
                .command_palette_commands
                .iter()
                .enumerate()
                .map(|(idx, cmd)| (idx, format!("{} {}", cmd.name, cmd.description)))
                .collect();

            // Match and score all candidates, keeping track of indices
            let mut scored_commands: Vec<(usize, u32)> = indexed_strings
                .iter()
                .filter_map(|(idx, search_text)| {
                    // Create references for matching
                    let candidates = vec![search_text.as_str()];
                    let matches = pattern.match_list(&candidates, &mut matcher);
                    matches.first().map(|(_, score)| (*idx, *score))
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

        // Restore previous mode - check if mode has configured previous override
        self.mode = if let Some(mode_def) = self.modes.get("command_palette") {
            if let Some(previous) = &mode_def.previous {
                previous.clone()
            } else {
                self.command_palette_previous_mode
                    .take()
                    .unwrap_or_else(|| "normal".to_string())
            }
        } else {
            self.command_palette_previous_mode
                .take()
                .unwrap_or_else(|| "normal".to_string())
        };

        // Clear command palette state
        self.command_palette_input = None;
        self.command_palette_commands.clear();
        self.command_palette_filtered.clear();
        self.command_palette_selected = 0;

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
        self.git_status_selected = 0;
        self.git_status_branch_info = branch_info;
        self.git_dirty_count = dirty_count;

        // Enter git_status mode
        self.mode = "git_status".into();
        debug!("Entered git_status mode");

        // Load preview for first file
        self.load_git_diff_preview(cx);

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

        // Get selected file entry
        let entry = match self.git_status_files.get(self.git_status_selected) {
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

        if self.git_status_selected + 1 < self.git_status_files.len() {
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

        if self.git_status_selected < self.git_status_files.len() {
            let entry = &self.git_status_files[self.git_status_selected];
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
    pub fn git_status_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_status" {
            return;
        }

        debug!("Dismissing git status");

        // Restore previous mode - check if mode has configured previous override
        self.mode = if let Some(mode_def) = self.modes.get("git_status") {
            if let Some(previous) = &mode_def.previous {
                previous.clone()
            } else {
                self.git_status_previous_mode
                    .take()
                    .unwrap_or_else(|| "normal".to_string())
            }
        } else {
            self.git_status_previous_mode
                .take()
                .unwrap_or_else(|| "normal".to_string())
        };

        // Clear git status state
        self.git_status_files.clear();
        self.git_status_selected = 0;
        self.git_status_branch_info = None;

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Accessor for git status files (for GUI layer).
    pub fn git_status_files(&self) -> &[crate::git_status::GitStatusEntry] {
        &self.git_status_files
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

    // ==== Buffer finder actions ====

    /// Open buffer finder modal.
    ///
    /// Initializes buffer list with currently open buffers and creates input buffer for searching.
    pub fn open_buffer_finder(&mut self, cx: &mut Context<Self>) {
        debug!("Opening buffer finder");

        // Save current mode
        self.buffer_finder_previous_mode = Some(self.mode.clone());
        self.mode = "buffer_finder".to_string();

        // Create input buffer
        let buffer_id = BufferId::from(NonZeroU64::new(3).unwrap());
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        self.buffer_finder_input = Some(input_buffer);

        // Get all open buffers from BufferStore
        let buffers = self.buffer_store.read(cx).buffer_paths();

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
            let path = &self.buffer_finder_filtered[self.buffer_finder_selected];
            debug!(buffer = ?path, "Buffer finder: switching to buffer");

            // Get buffer from BufferStore and switch to it
            if let Some(buffer_item) = self.buffer_store.read(cx).get_buffer_by_path(path) {
                let buffer_id = buffer_item.read(cx).buffer().read(cx).remote_id();

                // Update legacy buffer_item
                self.buffer_item = buffer_item.clone();

                // Update active_buffer_id
                self.active_buffer_id = Some(buffer_id);

                // Update current_file_path for status bar
                self.current_file_path = Some(path.clone());

                // Update activation history
                self.buffer_store
                    .update(cx, |store, _cx| store.activate_buffer(buffer_id));

                // Reset cursor to beginning (could be improved to save/restore per-buffer cursor)
                self.cursor.move_to(text::Point::new(0, 0));

                debug!(buffer_id = ?buffer_id, "Switched to buffer");
            } else {
                tracing::error!("Buffer not found in BufferStore: {:?}", path);
            }
        }

        self.buffer_finder_dismiss(cx);
    }

    /// Dismiss buffer finder.
    pub fn buffer_finder_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "buffer_finder" {
            return;
        }

        debug!("Dismissing buffer finder");

        // Restore previous mode
        self.mode = if let Some(mode_def) = self.modes.get("buffer_finder") {
            if let Some(previous) = &mode_def.previous {
                previous.clone()
            } else {
                self.buffer_finder_previous_mode
                    .take()
                    .unwrap_or_else(|| "normal".to_string())
            }
        } else {
            self.buffer_finder_previous_mode
                .take()
                .unwrap_or_else(|| "normal".to_string())
        };

        // Clear buffer finder state
        self.buffer_finder_input = None;
        self.buffer_finder_buffers.clear();
        self.buffer_finder_filtered.clear();
        self.buffer_finder_selected = 0;

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Filter buffers based on query string.
    pub fn filter_buffers(&mut self, query: &str, cx: &mut Context<Self>) {
        if query.is_empty() {
            // No query: show all buffers
            self.buffer_finder_filtered = self.buffer_finder_buffers.clone();
        } else {
            // Fuzzy match on buffer paths
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

            let candidates: Vec<&str> = self
                .buffer_finder_buffers
                .iter()
                .map(|p| p.to_str().unwrap_or(""))
                .collect();

            let mut matches = pattern.match_list(candidates, &mut self.file_finder_matcher);
            matches.sort_by(|a, b| b.1.cmp(&a.1));

            self.buffer_finder_filtered = matches
                .into_iter()
                .map(|(path, _score)| PathBuf::from(path))
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
    pub fn buffer_finder_filtered(&self) -> &[PathBuf] {
        &self.buffer_finder_filtered
    }

    /// Get selected buffer index.
    pub fn buffer_finder_selected(&self) -> usize {
        self.buffer_finder_selected
    }
}

/// Build the list of all available commands from the keymap.
///
/// Iterates through all bindings in the keymap and extracts command information
/// including name, description, and TypeId for dispatch.
///
/// # Arguments
///
/// * `keymap` - The keymap to extract commands from
///
/// # Returns
///
/// A vector of [`CommandInfo`] structs representing all available commands
fn build_command_list(keymap: &gpui::Keymap) -> Vec<crate::stoat::CommandInfo> {
    use std::collections::HashMap;

    let mut commands_by_type_id: HashMap<std::any::TypeId, crate::stoat::CommandInfo> =
        HashMap::new();

    // Iterate through all bindings
    for binding in keymap.bindings() {
        let action = binding.action();
        let type_id = action.type_id();

        // Skip if we've already seen this action type
        if commands_by_type_id.contains_key(&type_id) {
            continue;
        }

        // Get action name and description, skip if either unavailable
        let Some(name) = crate::actions::action_name(action) else {
            continue;
        };
        let Some(description) = crate::actions::description(action) else {
            continue;
        };

        commands_by_type_id.insert(
            type_id,
            crate::stoat::CommandInfo {
                name: name.to_string(),
                description: description.to_string(),
                type_id,
            },
        );
    }

    // Convert to sorted vector
    let mut commands: Vec<crate::stoat::CommandInfo> = commands_by_type_id.into_values().collect();
    commands.sort_by(|a, b| a.name.cmp(&b.name));

    commands
}
