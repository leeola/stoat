//! Insert text action implementation and tests.
//!
//! This module implements the [`insert_text`](crate::Stoat::insert_text) action, which handles
//! text insertion at the cursor position. The action routes input to different buffers depending
//! on the current mode:
//! - In [`file_finder`](crate::Stoat::open_file_finder) mode, inserts into the file finder input
//! - In [`command_palette`](crate::Stoat::open_command_palette) mode, inserts into the palette
//!   input
//! - In [`buffer_finder`](crate::Stoat::open_buffer_finder) mode, inserts into the buffer finder
//!   input
//! - Otherwise, inserts into the main buffer at the cursor position
//!
//! After insertion, the cursor moves forward by the length of the inserted text, and the buffer
//! is reparsed for syntax highlighting.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Insert text at the cursor position.
    ///
    /// Routes text insertion to the appropriate buffer based on the current mode. In finder
    /// and palette modes, text is inserted into the respective input buffers and triggers
    /// filtering. In normal mode, text is inserted into the main buffer at the cursor position.
    ///
    /// # Parameters
    ///
    /// - `text`: The text string to insert
    /// - `cx`: The GPUI context for buffer updates
    ///
    /// # Behavior
    ///
    /// - File finder mode: Inserts at end of input buffer, triggers file filtering
    /// - Command palette mode: Inserts at end of input buffer, triggers command filtering
    /// - Buffer finder mode: Inserts at end of input buffer, triggers buffer filtering
    /// - Normal mode: Inserts at cursor position, moves cursor forward, triggers reparse
    ///
    /// # Related Actions
    ///
    /// - [`delete_left`](crate::Stoat::delete_left) - Delete character before cursor
    /// - [`new_line`](crate::Stoat::new_line) - Insert newline character
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

        // Main buffer insertion with multi-cursor support
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        // Auto-sync from cursor if single selection (backward compat)
        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<text::Point>(&snapshot);
            if newest_sel.head() != cursor_pos {
                let id = self.selections.next_id();
                self.selections.select(
                    vec![text::Selection {
                        id,
                        start: cursor_pos,
                        end: cursor_pos,
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    }],
                    &snapshot,
                );
            }
        }

        // Collect insertion points for all selections (sorted by offset ascending)
        let mut selections_with_offsets: Vec<_> = self
            .selections
            .all::<text::Point>(&snapshot)
            .into_iter()
            .map(|sel| {
                let offset = snapshot.point_to_offset(sel.head());
                (offset, sel)
            })
            .collect();
        selections_with_offsets.sort_by_key(|(offset, _)| *offset);

        // Collect all edits to apply at once
        let edits: Vec<_> = selections_with_offsets
            .iter()
            .map(|(offset, _)| (*offset..*offset, text))
            .collect();

        // Apply all insertions at once
        let buffer = buffer.clone();
        let text_byte_len = text.len();
        buffer.update(cx, |buffer, _| {
            buffer.edit(edits);
        });

        // Calculate new positions accounting for all prior insertions
        let snapshot = buffer.read(cx).snapshot();
        let mut new_selections = Vec::new();
        let id_start = self.selections.next_id();

        for (i, (old_offset, _)) in selections_with_offsets.iter().enumerate() {
            // Each insertion before this one shifts this position forward by text_byte_len
            let shift = i * text_byte_len;
            // The cursor ends up after the inserted text at this position
            let new_offset = old_offset + shift + text_byte_len;
            let new_pos = snapshot.offset_to_point(new_offset);

            new_selections.push(text::Selection {
                id: id_start + i,
                start: new_pos,
                end: new_pos,
                reversed: false,
                goal: text::SelectionGoal::None,
            });
        }

        // Update selections to new positions
        self.selections.select(new_selections.clone(), &snapshot);

        // Sync cursor to last selection
        if let Some(last) = new_selections.last() {
            self.cursor.move_to(last.head());
        }

        // Reparse for syntax highlighting
        buffer_item.update(cx, |item, cx| {
            let _ = item.reparse(cx);
        });

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn inserts_text_at_cursor(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hello");
            assert_eq!(s.cursor.position(), text::Point::new(0, 5));
        });
    }

    #[gpui::test]
    fn inserts_text_mid_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("world", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.insert_text("Hello ", cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hello world");
        });
    }

    #[gpui::test]
    fn moves_cursor_forward(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            assert_eq!(s.cursor.position(), text::Point::new(0, 0));
            s.insert_text("Hi", cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 2));
            s.insert_text("!", cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 3));
        });
    }

    #[gpui::test]
    fn inserts_at_multiple_cursors(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("line1\nline2\nline3", cx);

            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 5),
                        end: text::Point::new(0, 5),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 5),
                        end: text::Point::new(1, 5),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 2,
                        start: text::Point::new(2, 5),
                        end: text::Point::new(2, 5),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            s.insert_text("!", cx);

            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "line1!\nline2!\nline3!");
        });
    }
}
