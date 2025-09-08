//! Core event processing engine.
//!
//! This module contains the pure functions that process events and transform
//! editor state. All business logic lives here as testable, pure functions.

use crate::{
    actions::{EditMode, EditorAction, TextPosition, TextRange},
    command::Command,
    effects::Effect,
    events::EditorEvent,
    input::{Key, Modifiers},
    keymap::Keymap,
    state::{EditorState, TextBuffer},
};

/// Process a single event and return new state plus effects.
///
/// This is the core function of the editor - it takes the current state and
/// an event, and returns the new state along with any side effects that
/// should be executed.
///
/// # Arguments
///
/// * `state` - Current editor state
/// * `event` - Event to process
/// * `keymap` - Keymap for resolving key presses to commands
///
/// # Returns
///
/// Tuple of (new_state, effects_to_execute)
pub fn process_event(
    state: EditorState,
    event: EditorEvent,
    keymap: &Keymap,
) -> (EditorState, Vec<Effect>) {
    tracing::debug!("Processing event: {:?} in mode: {:?}", event, state.mode);

    let result = match event {
        EditorEvent::KeyPress { key, modifiers } => {
            process_key_press(state, key, modifiers, keymap)
        },

        EditorEvent::TextPasted { content } => {
            let action = EditorAction::InsertText {
                position: state.cursor_position(),
                text: content,
            };
            let new_state = apply_action(state, action);
            (new_state, vec![])
        },

        EditorEvent::MouseClick {
            position,
            button: _,
        } => {
            // Convert pixel position to text position (simplified for now)
            let text_pos = pixel_to_text_position(&state, position);
            let action = EditorAction::MoveCursor { position: text_pos };
            let new_state = apply_action(state, action);
            (new_state, vec![])
        },

        EditorEvent::NewFile => {
            let mut new_state = EditorState::new();
            new_state = apply_action(new_state, EditorAction::SetMode { mode: state.mode });
            (new_state, vec![])
        },

        EditorEvent::Exit => {
            if state.is_dirty {
                let effect = Effect::ShowInfo {
                    message: "File has unsaved changes".to_string(),
                };
                (state, vec![effect])
            } else {
                (state, vec![Effect::Exit])
            }
        },

        EditorEvent::Resize { width, height } => {
            let action = EditorAction::SetViewportSize { width, height };
            let new_state = apply_action(state, action);
            (new_state, vec![])
        },

        EditorEvent::Scroll { delta_x, delta_y } => {
            let action = EditorAction::ScrollViewport { delta_x, delta_y };
            let new_state = apply_action(state, action);
            (new_state, vec![])
        },

        EditorEvent::Undo => {
            // TODO: Implement proper undo with history stack
            // For now, just return state unchanged to avoid crashes
            tracing::debug!("Undo requested (not implemented)");
            (state, vec![])
        },

        EditorEvent::Redo => {
            // TODO: Implement proper redo with history stack
            // For now, just return state unchanged to avoid crashes
            tracing::debug!("Redo requested (not implemented)");
            (state, vec![])
        },

        EditorEvent::MouseMove { position } => {
            // TODO: Implement proper mouse move handling for selection extension
            // For now, just return state unchanged to avoid crashes
            tracing::trace!("Mouse moved to: {:?} (not implemented)", position);
            (state, vec![])
        },
    };

    tracing::debug!("Event processed, effects count: {}", result.1.len());
    result
}

/// Process keyboard input using the command system.
fn process_key_press(
    state: EditorState,
    key: Key,
    modifiers: Modifiers,
    keymap: &Keymap,
) -> (EditorState, Vec<Effect>) {
    tracing::trace!("Processing key in {:?} mode: {:?}", state.mode, key);

    let original_mode = state.mode;

    // First try to look up a command from the keymap
    if let Some(command) = keymap.lookup(&key, &modifiers, state.mode) {
        tracing::debug!("Found command for key: {:?}", command);
        let result = process_command(state, command);

        // Log mode changes
        if result.0.mode != original_mode {
            tracing::debug!("Mode changed: {:?} -> {:?}", original_mode, result.0.mode);
        }

        return result;
    }

    // Handle special cases for insert mode character insertion
    if state.mode == EditMode::Insert {
        // Check what kind of key we have
        let text_to_insert = match key.as_str() {
            // Special keys that insert text
            "space" => Some(" ".to_string()),
            "tab" => Some("\t".to_string()),
            // Single characters (but not control characters)
            s if s.len() == 1 => {
                let ch = s.chars().next().unwrap();
                if !ch.is_control() {
                    Some(s.to_string())
                } else {
                    None
                }
            },
            _ => None,
        };

        if let Some(text) = text_to_insert {
            let command = Command::InsertStr(text.into());
            tracing::debug!("Insert mode text: {:?}", command);
            let result = process_command(state, command);

            if result.0.mode != original_mode {
                tracing::debug!("Mode changed: {:?} -> {:?}", original_mode, result.0.mode);
            }

            return result;
        }
    }

    // If no command found, return state unchanged
    tracing::trace!(
        "No command found for key: {:?} in mode: {:?}",
        key,
        state.mode
    );
    (state, vec![])
}

/// Process a command and return new state plus effects.
fn process_command(state: EditorState, command: Command) -> (EditorState, Vec<Effect>) {
    tracing::debug!("Processing command: {:?}", command);

    // Handle commands that produce effects but no state changes
    if command == Command::Exit {
        return (state, vec![Effect::Exit]);
    }

    // Convert command to action and apply it
    if let Some(action) = command.to_action(&state) {
        let new_state = apply_action(state, action);
        (new_state, vec![])
    } else {
        // Command didn't produce an action (like Exit or invalid operations)
        (state, vec![])
    }
}

/// Apply an action to the state, returning new state.
fn apply_action(mut state: EditorState, action: EditorAction) -> EditorState {
    match action {
        EditorAction::InsertText { position, text } => {
            insert_text_at_position(&mut state, position, text);
        },

        EditorAction::DeleteText { range } => {
            delete_text_in_range(&mut state, range);
        },

        EditorAction::ReplaceText { range, new_text } => {
            replace_text_in_range(&mut state, range, new_text);
        },

        EditorAction::MoveCursor { position } => {
            state.cursor.position = position;
            // Update desired_column to be the visual column for proper tab handling
            if let Some(line) = state.line(position.line) {
                // Always calculate visual column when moving cursor
                let byte_pos = if position.byte_offset > 0 {
                    position.byte_offset
                } else {
                    char_to_byte(&line, position.column)
                };
                state.cursor.desired_column = byte_to_visual(&line, byte_pos);
            } else {
                state.cursor.desired_column = position.column;
            }
        },

        EditorAction::SetSelection { range } => {
            state.cursor.selection = range;
        },

        EditorAction::SetMode { mode } => {
            state.mode = mode;
        },

        EditorAction::SetViewportSize { width, height } => {
            state.viewport.width = width;
            state.viewport.height = height;
        },

        EditorAction::ScrollViewport { delta_x, delta_y } => {
            state.viewport.scroll_x = (state.viewport.scroll_x + delta_x).max(0.0);
            state.viewport.scroll_y = (state.viewport.scroll_y + delta_y).max(0.0);
        },

        EditorAction::SetContent { content } => {
            state.buffer = TextBuffer::with_text(&content);
            state.cursor.position = TextPosition::start();
            state.cursor.desired_column = 0;
        },

        EditorAction::SetFilePath { path } => {
            if let Some(path) = path {
                state.file = crate::state::FileInfo::with_path(path);
            } else {
                state.file = crate::state::FileInfo::new();
            }
        },

        EditorAction::SetDirty { dirty } => {
            state.is_dirty = dirty;
        },

        EditorAction::ToggleCommandInfo => {
            state.show_command_info = !state.show_command_info;
        },
    }

    state
}

// Helper functions for text manipulation and cursor movement

/// Convert a visual column position back to a character/byte position
/// Uses a fixed tab width of 8 (Iced's default)
pub fn visual_to_char_column(line: &str, visual_target: usize) -> (usize, usize) {
    const TAB_WIDTH: usize = 8;
    let mut visual_col = 0;
    let mut char_col = 0;
    let mut byte_col = 0;

    for ch in line.chars() {
        let char_width = if ch == '\t' {
            // Tab aligns to next tab stop
            (visual_col / TAB_WIDTH + 1) * TAB_WIDTH - visual_col
        } else {
            1
        };

        if visual_col + char_width > visual_target {
            // We're in the middle of this character
            // Return both the char position and actual byte position
            return (char_col, byte_col);
        }

        visual_col += char_width;
        char_col += 1;
        byte_col += ch.len_utf8();

        if visual_col >= visual_target {
            return (char_col, byte_col);
        }
    }

    (char_col, byte_col)
}

/// Convert character index to byte offset in a string
pub fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(text.len())
}

/// Convert byte offset to character index
pub fn byte_to_char(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset.min(text.len())].chars().count()
}

/// Calculate visual column from byte offset (accounting for tabs)
/// Uses a fixed tab width of 8 (Iced's default)
pub fn byte_to_visual(text: &str, byte_offset: usize) -> usize {
    const TAB_WIDTH: usize = 8;
    let mut visual_col = 0;
    let mut current_byte = 0;

    for ch in text.chars() {
        if current_byte >= byte_offset {
            break;
        }

        visual_col += match ch {
            '\t' => TAB_WIDTH - (visual_col % TAB_WIDTH), // Align to next tab stop
            _ => 1,                                       /* For now, assume other chars are
                                                            * width 1 */
        };

        current_byte += ch.len_utf8();
    }

    visual_col
}

/// Find byte offset for a target visual column
/// Uses a fixed tab width of 8 (Iced's default)
pub fn visual_to_byte(text: &str, target_visual: usize) -> (usize, usize) {
    const TAB_WIDTH: usize = 8;
    let mut visual_col = 0;
    let mut byte_offset = 0;

    for ch in text.chars() {
        let char_width = match ch {
            '\t' => TAB_WIDTH - (visual_col % TAB_WIDTH),
            _ => 1,
        };

        if visual_col + char_width > target_visual {
            // Target is in the middle of this character
            return (byte_offset, visual_col);
        }

        visual_col += char_width;
        byte_offset += ch.len_utf8();

        if visual_col >= target_visual {
            return (byte_offset, visual_col);
        }
    }

    (byte_offset, visual_col)
}

/// Insert text at a specific TextPosition
fn insert_text_at_position(state: &mut EditorState, position: TextPosition, text: String) {
    // For now, let's do a simple string-based insertion to get tests passing
    // TODO: Implement proper rope-based insertion later

    let current_text = state.text();
    let lines: Vec<&str> = current_text.lines().collect();

    if position.line < lines.len() || (position.line == lines.len() && current_text.is_empty()) {
        // Split text into before and after the insertion point
        let mut new_text = String::new();

        // Add lines before the target line
        for (i, line) in lines.iter().enumerate() {
            if i < position.line {
                new_text.push_str(line);
                new_text.push('\n');
            } else if i == position.line {
                // Use byte offset for insertion if available, otherwise convert from char position
                let byte_pos = if position.byte_offset > 0 || position.column == 0 {
                    position.byte_offset.min(line.len())
                } else {
                    // Fallback: convert char position to byte offset
                    char_to_byte(line, position.column)
                };

                // Insert at byte position
                new_text.push_str(&line[..byte_pos]);
                new_text.push_str(&text);
                new_text.push_str(&line[byte_pos..]);

                // Add newline if not the last line
                if i < lines.len() - 1 {
                    new_text.push('\n');
                }
            } else {
                // Add remaining lines
                new_text.push_str(line);
                if i < lines.len() - 1 {
                    new_text.push('\n');
                }
            }
        }

        // Handle the case where we're inserting in an empty buffer
        if lines.is_empty() && position.line == 0 && position.column == 0 {
            new_text = text.clone();
        }

        // Calculate cursor position BEFORE updating the buffer
        let new_cursor_position;
        let new_desired_column;

        if text.contains('\n') {
            // Handle newlines - cursor goes to beginning of next line
            let newline_count = text.chars().filter(|&c| c == '\n').count();
            let after_last_newline = text.rsplit('\n').next().unwrap_or("");
            let new_column = after_last_newline.chars().count();
            let new_byte_offset = after_last_newline.len();
            let new_visual_column = byte_to_visual(after_last_newline, new_byte_offset);

            new_cursor_position = TextPosition::new_with_byte_offset(
                position.line + newline_count,
                new_column,
                new_byte_offset,
                new_visual_column,
            );
            new_desired_column = new_visual_column;
        } else {
            // Calculate new position after insertion (including tabs)
            let line_text = if position.line < lines.len() {
                lines[position.line]
            } else {
                ""
            };

            // Get byte position where we inserted
            let insert_byte_pos = if position.byte_offset > 0 || position.column == 0 {
                position.byte_offset.min(line_text.len())
            } else {
                char_to_byte(line_text, position.column)
            };

            // Calculate new cursor position
            let inserted_chars = text.chars().count();
            let inserted_bytes = text.len();

            let new_column = position.column + inserted_chars;
            let new_byte_offset = insert_byte_pos + inserted_bytes;

            // Build the line after insertion to calculate visual column
            let line_after_insertion = format!(
                "{}{}{}",
                &line_text[..insert_byte_pos],
                text,
                &line_text[insert_byte_pos..]
            );

            let new_visual_column = byte_to_visual(&line_after_insertion, new_byte_offset);

            new_cursor_position = TextPosition::new_with_byte_offset(
                position.line,
                new_column,
                new_byte_offset,
                new_visual_column,
            );
            new_desired_column = new_visual_column;
        }

        // Update the buffer with new content
        state.buffer = TextBuffer::with_text(&new_text);

        // Apply the pre-calculated cursor position
        state.cursor.position = new_cursor_position;
        state.cursor.desired_column = new_desired_column;
    }
}

/// Delete text in a specific TextRange
fn delete_text_in_range(state: &mut EditorState, range: TextRange) {
    // For now, let's do a simple string-based deletion to get tests passing
    // TODO: Implement proper rope-based deletion later

    let current_text = state.text();
    let lines: Vec<&str> = current_text.lines().collect();

    // Validate range
    if range.start.line >= lines.len() || range.end.line >= lines.len() {
        return; // Invalid range
    }

    let mut new_text = String::new();

    for (line_idx, line) in lines.iter().enumerate() {
        if line_idx < range.start.line {
            // Lines before the deletion range - keep as is
            new_text.push_str(line);
            new_text.push('\n');
        } else if line_idx > range.end.line {
            // Lines after the deletion range - keep as is
            new_text.push_str(line);
            if line_idx < lines.len() - 1 {
                new_text.push('\n');
            }
        } else if line_idx == range.start.line && line_idx == range.end.line {
            // Deletion within a single line
            let chars: Vec<char> = line.chars().collect();
            let start_col = range.start.column.min(chars.len());
            let end_col = range.end.column.min(chars.len());

            // Add characters before deletion start
            for &ch in chars.iter().take(start_col) {
                new_text.push(ch);
            }

            // Skip characters in deletion range
            // Add characters after deletion end
            for &ch in chars.iter().skip(end_col) {
                new_text.push(ch);
            }

            // Add newline if not the last line
            if line_idx < lines.len() - 1 {
                new_text.push('\n');
            }
        } else if line_idx == range.start.line {
            // Start line of multi-line deletion
            let chars: Vec<char> = line.chars().collect();
            let start_col = range.start.column.min(chars.len());

            // Add characters before deletion start
            for &ch in chars.iter().take(start_col) {
                new_text.push(ch);
            }
            // Don't add newline - we're deleting across lines
        } else if line_idx == range.end.line {
            // End line of multi-line deletion
            let chars: Vec<char> = line.chars().collect();
            let end_col = range.end.column.min(chars.len());

            // Add characters after deletion end
            for &ch in chars.iter().skip(end_col) {
                new_text.push(ch);
            }

            // Add newline if not the last line
            if line_idx < lines.len() - 1 {
                new_text.push('\n');
            }
        }
        // Lines between start and end are completely deleted (skip them)
    }

    // Remove trailing newline if we added one unnecessarily
    if new_text.ends_with('\n') && !current_text.ends_with('\n') {
        new_text.pop();
    }

    // Update the buffer with new content
    state.buffer = TextBuffer::with_text(&new_text);

    // Update cursor position to the start of the deletion range
    // Need to recalculate visual column for proper tab handling
    state.cursor.position = range.start;

    // Get the line after deletion to calculate proper visual column
    if let Some(line) = state.line(range.start.line) {
        // If we have a valid byte offset, use it; otherwise calculate from column
        let byte_pos = if range.start.byte_offset > 0 {
            range.start.byte_offset.min(line.len())
        } else {
            char_to_byte(&line, range.start.column)
        };

        let visual_col = byte_to_visual(&line, byte_pos);
        state.cursor.position.visual_column = visual_col;
        state.cursor.desired_column = visual_col;
    } else {
        // Fallback if line not found
        state.cursor.desired_column = range.start.column;
    }
}

/// Replace text in a specific TextRange with new text
fn replace_text_in_range(state: &mut EditorState, range: TextRange, new_text: String) {
    // Replace is essentially delete + insert
    // First delete the range, then insert the new text at the start position
    delete_text_in_range(state, range);
    insert_text_at_position(state, range.start, new_text);
}

fn pixel_to_text_position(state: &EditorState, position: crate::input::Point) -> TextPosition {
    let line = ((position.y - state.viewport.scroll_y) / state.viewport.line_height) as usize;
    let visual_column =
        ((position.x - state.viewport.scroll_x) / state.viewport.char_width) as usize;

    // Clamp line to valid boundaries
    let line = line.min(state.line_count().saturating_sub(1));

    // Convert visual column to character column
    let column = if let Some(line_text) = state.line(line) {
        let (char_col, _byte_col) = visual_to_char_column(&line_text, visual_column);
        char_col.min(line_text.len())
    } else {
        0
    };

    TextPosition::new(line, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        actions::{EditMode, EditorAction, TextPosition, TextRange},
        events::EditorEvent,
        keymap::Keymap,
        state::EditorState,
    };

    #[test]
    fn test_text_insertion_crash() {
        // Test that reproduces the insertion crash
        let mut state = EditorState::with_text("Hello World");
        state.mode = EditMode::Insert;

        // Try to insert a character at position (0, 0)
        let action = EditorAction::InsertText {
            position: TextPosition::new(0, 0),
            text: "X".to_string(),
        };

        // This should not crash
        let new_state = apply_action(state, action);
        assert_eq!(new_state.text(), "XHello World");
    }

    #[test]
    fn test_text_insertion_middle() {
        // Test inserting in the middle of text
        let mut state = EditorState::with_text("Hello World");
        state.mode = EditMode::Insert;

        // Insert at position (0, 5) - between "Hello" and " World"
        let action = EditorAction::InsertText {
            position: TextPosition::new(0, 5),
            text: ",".to_string(),
        };

        let new_state = apply_action(state, action);
        assert_eq!(new_state.text(), "Hello, World");
    }

    #[test]
    fn test_text_insertion_end() {
        // Test inserting at the end of text
        let mut state = EditorState::with_text("Hello");
        state.mode = EditMode::Insert;

        // Insert at end
        let action = EditorAction::InsertText {
            position: TextPosition::new(0, 5),
            text: "!".to_string(),
        };

        let new_state = apply_action(state, action);
        assert_eq!(new_state.text(), "Hello!");
    }

    #[test]
    fn test_text_deletion_crash() {
        // Test that reproduces the deletion crash
        let state = EditorState::with_text("Hello World");

        // Try to delete a character (simulate backspace at position 5)
        let action = EditorAction::DeleteText {
            range: TextRange::new(
                TextPosition::new(0, 4), // delete from position 4
                TextPosition::new(0, 5), // to position 5 (deleting 'o')
            ),
        };

        // This should not crash
        let new_state = apply_action(state, action);
        assert_eq!(new_state.text(), "Hell World");
    }

    #[test]
    fn test_text_deletion_beginning() {
        // Test deleting at the beginning of text
        let state = EditorState::with_text("Hello");

        let action = EditorAction::DeleteText {
            range: TextRange::new(
                TextPosition::new(0, 0),
                TextPosition::new(0, 1), // Delete first character
            ),
        };

        let new_state = apply_action(state, action);
        assert_eq!(new_state.text(), "ello");
    }

    #[test]
    fn test_text_deletion_end() {
        // Test deleting at the end of text
        let state = EditorState::with_text("Hello");

        let action = EditorAction::DeleteText {
            range: TextRange::new(
                TextPosition::new(0, 4),
                TextPosition::new(0, 5), // Delete last character
            ),
        };

        let new_state = apply_action(state, action);
        assert_eq!(new_state.text(), "Hell");
    }

    #[test]
    fn test_text_replace() {
        // Test replacing text within a line
        let state = EditorState::with_text("Hello World");

        let action = EditorAction::ReplaceText {
            range: TextRange::new(
                TextPosition::new(0, 6),  // Start at "World"
                TextPosition::new(0, 11), // End at end of "World"
            ),
            new_text: "Claude".to_string(),
        };

        let new_state = apply_action(state, action);
        assert_eq!(new_state.text(), "Hello Claude");
    }

    #[test]
    fn test_text_replace_single_char() {
        // Test replacing a single character
        let state = EditorState::with_text("Hello");

        let action = EditorAction::ReplaceText {
            range: TextRange::new(
                TextPosition::new(0, 1), // Replace 'e'
                TextPosition::new(0, 2),
            ),
            new_text: "a".to_string(),
        };

        let new_state = apply_action(state, action);
        assert_eq!(new_state.text(), "Hallo");
    }

    #[test]
    fn test_text_replace_with_longer_text() {
        // Test replacing with longer text
        let state = EditorState::with_text("Hi");

        let action = EditorAction::ReplaceText {
            range: TextRange::new(
                TextPosition::new(0, 0), // Replace entire text
                TextPosition::new(0, 2),
            ),
            new_text: "Hello World".to_string(),
        };

        let new_state = apply_action(state, action);
        assert_eq!(new_state.text(), "Hello World");
    }

    #[test]
    fn test_undo_event_does_not_crash() {
        // Test that Undo event doesn't crash
        let state = EditorState::with_text("Hello World");
        let keymap = Keymap::default();

        let (new_state, effects) = process_event(state.clone(), EditorEvent::Undo, &keymap);

        // Should return unchanged state (for now)
        assert_eq!(new_state.text(), state.text());
        assert!(effects.is_empty());
    }

    #[test]
    fn test_redo_event_does_not_crash() {
        // Test that Redo event doesn't crash
        let state = EditorState::with_text("Hello World");
        let keymap = Keymap::default();

        let (new_state, effects) = process_event(state.clone(), EditorEvent::Redo, &keymap);

        // Should return unchanged state (for now)
        assert_eq!(new_state.text(), state.text());
        assert!(effects.is_empty());
    }

    #[test]
    fn test_set_selection() {
        // Test setting a text selection
        let state = EditorState::with_text("Hello World");

        let action = EditorAction::SetSelection {
            range: Some(TextRange::new(
                TextPosition::new(0, 0),
                TextPosition::new(0, 5), // Select "Hello"
            )),
        };

        let new_state = apply_action(state, action);
        assert!(new_state.cursor.selection.is_some());
        let selection = new_state.cursor.selection.expect("selection should be set");
        assert_eq!(selection.start, TextPosition::new(0, 0));
        assert_eq!(selection.end, TextPosition::new(0, 5));
    }

    #[test]
    fn test_clear_selection() {
        // Test clearing a text selection
        let mut state = EditorState::with_text("Hello World");
        // Set initial selection
        state.cursor.selection = Some(TextRange::new(
            TextPosition::new(0, 0),
            TextPosition::new(0, 5),
        ));

        let action = EditorAction::SetSelection { range: None };

        let new_state = apply_action(state, action);
        assert!(new_state.cursor.selection.is_none());
    }

    #[test]
    fn test_mouse_move_event_does_not_crash() {
        // Test that MouseMove event doesn't crash
        let state = EditorState::with_text("Hello World");
        let keymap = Keymap::default();

        let (new_state, effects) = process_event(
            state.clone(),
            EditorEvent::MouseMove {
                position: crate::input::Point::new(10.0, 20.0),
            },
            &keymap,
        );

        // Should return unchanged state (for now)
        assert_eq!(new_state.text(), state.text());
        assert!(effects.is_empty());
    }

    #[test]
    fn test_visual_to_char_column_with_tabs() {
        // Test converting visual columns to character positions with tabs
        // Now using fixed tab width of 8
        let line = "\tHello\tWorld";

        // Visual column 0 -> char 0 (before first tab)
        assert_eq!(visual_to_char_column(line, 0), (0, 0));

        // Visual columns 1-7 -> char 0 (within first tab)
        assert_eq!(visual_to_char_column(line, 1), (0, 0));
        assert_eq!(visual_to_char_column(line, 7), (0, 0));

        // Visual column 8 -> char 1 (H in Hello)
        assert_eq!(visual_to_char_column(line, 8), (1, 1));

        // Visual column 13 -> char 6 (after Hello)
        assert_eq!(visual_to_char_column(line, 13), (6, 6));

        // Visual column 16 -> char 7 (W in World, after tab)
        assert_eq!(visual_to_char_column(line, 16), (7, 7));
    }

    /// Calculate the display column for a position in a line, accounting for tabs
    /// Uses a fixed tab width of 8 (Iced's default)
    fn calculate_display_column(line: &str, byte_position: usize) -> usize {
        const TAB_WIDTH: usize = 8;
        let mut display_col = 0;
        let mut byte_col = 0;

        for ch in line.chars() {
            if byte_col >= byte_position {
                break;
            }

            if ch == '\t' {
                // Tab advances to next tab stop
                display_col = (display_col / TAB_WIDTH + 1) * TAB_WIDTH;
            } else {
                display_col += 1;
            }
            byte_col += ch.len_utf8();
        }

        display_col
    }

    #[test]
    fn test_calculate_display_column_with_tabs() {
        // Test calculating display columns with tabs
        let line = "\tHello\tWorld";

        // Byte position 0 (before first tab) -> visual column 0
        assert_eq!(calculate_display_column(line, 0), 0);

        // Byte position 1 (after first tab, at H) -> visual column 8 (with tab width 8)
        assert_eq!(calculate_display_column(line, 1), 8);

        // Byte position 6 (at second tab) -> visual column 13
        assert_eq!(calculate_display_column(line, 6), 13);

        // Byte position 7 (at W after second tab) -> visual column 16
        assert_eq!(calculate_display_column(line, 7), 16);
    }

    #[test]
    fn test_tab_alignment_at_different_positions() {
        // Test that tabs align to tab stops correctly (tab width 8)

        // Tab at position 0 should take full width (8 columns)
        assert_eq!(calculate_display_column("\tabc", 1), 8);

        // Tab at position 3 should take 5 columns to reach position 8
        assert_eq!(calculate_display_column("abc\tdef", 4), 8);

        // Tab at position 5 should take 3 columns to reach position 8
        assert_eq!(calculate_display_column("12345\tdef", 6), 8);
    }

    #[test]
    fn test_movecursor_desired_column_calculation() {
        // Simpler test to debug the MoveCursor desired_column calculation
        let mut state = EditorState::with_text("\tghi");

        // Move to character position 1 (g)
        let action = EditorAction::MoveCursor {
            position: TextPosition::new(0, 1),
        };
        state = apply_action(state, action);

        // Character position 1 (g) should have visual column 8
        // because the tab before it expands to 8 spaces
        assert_eq!(state.cursor.desired_column, 8);
    }

    #[test]
    fn test_cursor_movement_with_tabs_vertical() {
        // Test that MoveCursor action properly updates desired_column to visual column
        let mut state = EditorState::with_text("abc\tdef\n\tghi\njklmn");

        // Move cursor to position 4 on first line (d in def, after tab)
        // "abc\tdef" - character position 4 is 'd'
        // Visual: "abc " (tab expands to 1 space to reach column 4) "def"
        // So 'd' is at visual column 4
        let action = EditorAction::MoveCursor {
            position: TextPosition::new(0, 4),
        };
        state = apply_action(state, action);

        // The desired_column should be set to the visual column (8)
        // 'd' at column 4 has visual column 8 because "abc\t" -> abc then tab aligns to 8
        assert_eq!(state.cursor.desired_column, 8);

        // Now test moving to a line with a tab at the beginning
        // Move to position 1 on second line (g in ghi, after tab)
        // Character position 1 is at visual column 8 (\t expands to 8 spaces)
        let action2 = EditorAction::MoveCursor {
            position: TextPosition::new(1, 1),
        };

        // Debug: Check line count
        let line_count = state.line_count();
        assert!(
            line_count > 1,
            "Expected more than 1 line, got {line_count}"
        );

        // Debug: Check what line content we get
        let line1_content = state.line(1).expect("Line 1 should exist");
        assert_eq!(line1_content, "\tghi");

        // Debug: Check byte position calculation
        let byte_pos = line1_content
            .char_indices()
            .nth(1)
            .map(|(idx, _)| idx)
            .unwrap_or(line1_content.len());
        assert_eq!(byte_pos, 1); // Tab is 1 byte, so 'g' starts at byte 1

        // Debug: Check display column calculation
        let visual_col = calculate_display_column(&line1_content, byte_pos);
        assert_eq!(visual_col, 8); // Should be 8 with tab width of 8

        state = apply_action(state, action2);

        // The desired_column should be updated to visual column 8
        assert_eq!(state.cursor.position.line, 1);
        assert_eq!(state.cursor.position.column, 1);
        assert_eq!(state.cursor.desired_column, 8);
    }
}
