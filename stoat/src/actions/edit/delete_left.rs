//! Delete left action implementation and tests.
//!
//! This module implements the [`delete_left`](crate::Stoat::delete_left) action, which deletes
//! the character before the cursor (backspace). Like [`insert_text`](crate::Stoat::insert_text),
//! this action routes to different buffers based on the current mode, handling input deletion
//! for finders and palettes.
//!
//! The implementation carefully handles UTF-8 character boundaries to ensure multi-byte characters
//! are deleted correctly without breaking the string encoding.

use crate::stoat::Stoat;
use gpui::Context;
use text::Bias;

impl Stoat {
    /// Delete character before cursor.
    ///
    /// Routes deletion to the appropriate buffer based on the current mode. Handles UTF-8
    /// character boundaries correctly to avoid breaking multi-byte characters. In finder and
    /// palette modes, triggers re-filtering after deletion.
    ///
    /// # Behavior
    ///
    /// - File finder mode: Deletes last character from input, triggers file filtering
    /// - Command palette mode: Deletes last character from input, triggers command filtering
    /// - Buffer finder mode: Deletes last character from input, triggers buffer filtering
    /// - Normal mode: Deletes character before cursor, clips to valid UTF-8 boundary
    /// - At start of line: Does nothing (no-op)
    ///
    /// # Implementation
    ///
    /// Uses [`BufferSnapshot::clip_point`] with [`Bias::Left`] to find the correct character
    /// boundary, ensuring we never break a multi-byte UTF-8 character.
    ///
    /// # Related Actions
    ///
    /// - [`delete_right`](crate::Stoat::delete_right) - Delete character after cursor
    /// - [`delete_word_left`](crate::Stoat::delete_word_left) - Delete previous word
    pub fn delete_left(&mut self, cx: &mut Context<Self>) {
        // Route to file finder input buffer if in file_finder mode
        if self.mode == "file_finder" {
            if let Some(input_buffer) = &self.file_finder_input_ref {
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

                    // FIXME: Filtering moved to PaneGroupView in Step 4
                    // (will be triggered by PaneGroupView observing buffer changes)
                }
            }
            return;
        }

        // Route to command palette input buffer if in command_palette mode
        if self.mode == "command_palette" {
            if let Some(input_buffer) = &self.command_palette_input_ref {
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

                    // FIXME: Filtering moved to PaneGroupView
                    // (will be triggered by PaneGroupView observing buffer changes or through
                    // event)
                }
            }
            return;
        }

        // Route to buffer finder input buffer if in buffer_finder mode
        if self.mode == "buffer_finder" {
            if let Some(input_buffer) = &self.buffer_finder_input_ref {
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

                    // FIXME: Filtering moved to PaneGroupView
                    // (will be triggered by PaneGroupView observing buffer changes or through
                    // event)
                }
            }
            return;
        }

        // Main buffer deletion with multi-cursor support
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().clone();

        let before_selections = self.selections.disjoint_anchors_arc();
        buffer.update(cx, |buf, _| {
            buf.start_transaction();
        });
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

        // Collect deletion ranges for all selections
        tracing::debug!("delete_left: About to call selections.all()");
        let selections = self.selections.all::<text::Point>(&snapshot);
        tracing::debug!(
            "delete_left: selections.all() succeeded, got {} selections",
            selections.len()
        );
        let mut edits = Vec::new();
        let mut new_cursor_pos = None;

        for selection in &selections {
            let pos = selection.head();
            if pos.column > 0 {
                // Naive calculation: one position to the left
                let target_point = text::Point::new(pos.row, pos.column.saturating_sub(1));

                // Clip to valid character boundary
                let clipped = snapshot.clip_point(target_point, Bias::Left);
                let clipped_offset = snapshot.point_to_offset(clipped);
                let pos_offset = snapshot.point_to_offset(pos);

                edits.push((clipped_offset..pos_offset, ""));

                // Remember the clipped position for cursor update
                new_cursor_pos = Some(clipped);
            }
        }

        // Apply all deletions at once
        if !edits.is_empty() {
            tracing::debug!(
                "delete_left: About to call buffer.edit() with {} edits",
                edits.len()
            );
            buffer.update(cx, |buffer, _| {
                tracing::debug!("delete_left: Inside buffer.update, calling buffer.edit()");
                buffer.edit(edits);
                tracing::debug!("delete_left: buffer.edit() completed successfully");
            });

            // Recreate SelectionsCollection with new snapshot to refresh locators
            // This is necessary because buffer edits invalidate the internal locators
            // used for efficient anchor resolution.
            let new_snapshot = buffer.read(cx).snapshot();
            self.selections = crate::selections::SelectionsCollection::new(&new_snapshot);

            // Sync cursor to the position where deletion occurred
            if let Some(pos) = new_cursor_pos {
                self.cursor.move_to(pos);

                // Create a selection at the cursor position
                let id = self.selections.next_id();
                self.selections.select(
                    vec![text::Selection {
                        id,
                        start: pos,
                        end: pos,
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    }],
                    &new_snapshot,
                );
            }

            let tx = buffer.update(cx, |buf, _| buf.end_transaction());
            if let Some((tx_id, _)) = tx {
                self.selection_history
                    .insert_transaction(tx_id, before_selections);
                self.selection_history
                    .set_after_selections(tx_id, self.selections.disjoint_anchors_arc());
            }

            // Reparse
            buffer_item.update(cx, |item, cx| {
                let _ = item.reparse(cx);
            });

            // Notify LSP servers of the change
            self.send_did_change_notification(cx);

            cx.emit(crate::stoat::StoatEvent::Changed);
        } else {
            buffer.update(cx, |buf, _| {
                buf.end_transaction();
            });
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn deletes_character_before_cursor(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.delete_left(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hell");
            assert_eq!(s.cursor.position(), text::Point::new(0, 4));
        });
    }

    #[gpui::test]
    fn no_op_at_start_of_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hi", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.delete_left(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hi");
        });
    }

    #[gpui::test]
    fn handles_multi_byte_utf8(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello 世界", cx);
            s.delete_left(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hello 世");
        });
    }

    #[gpui::test]
    fn deletes_at_multiple_cursors(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("abc\ndef\nghi", cx);

            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 3),
                        end: text::Point::new(0, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 3),
                        end: text::Point::new(1, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 2,
                        start: text::Point::new(2, 3),
                        end: text::Point::new(2, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            s.delete_left(cx);

            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "ab\nde\ngh");
        });
    }
}
