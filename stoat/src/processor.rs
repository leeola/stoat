//! Core event processing engine.
//!
//! This module contains the pure functions that process events and transform
//! editor state. All business logic lives here as testable, pure functions.

use crate::{
    actions::{EditMode, EditorAction, TextPosition, TextRange},
    effects::Effect,
    events::EditorEvent,
    state::{EditorState, TextBuffer},
};
use iced::keyboard;

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
///
/// # Returns
///
/// Tuple of (new_state, effects_to_execute)
pub fn process_event(state: EditorState, event: EditorEvent) -> (EditorState, Vec<Effect>) {
    tracing::debug!("Processing event: {:?} in mode: {:?}", event, state.mode);

    let result = match event {
        EditorEvent::KeyPress { key, modifiers } => process_key_press(state, key, modifiers),

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

        // TODO: Implement remaining events
        _ => (state, vec![]),
    };

    tracing::debug!("Event processed, effects count: {}", result.1.len());
    result
}

/// Process keyboard input based on current mode.
fn process_key_press(
    state: EditorState,
    key: keyboard::Key,
    modifiers: keyboard::Modifiers,
) -> (EditorState, Vec<Effect>) {
    tracing::trace!("Processing key in {:?} mode: {:?}", state.mode, key);

    let original_mode = state.mode;
    let result = match state.mode {
        EditMode::Insert => process_insert_mode(state, key, modifiers),
        EditMode::Normal => process_normal_mode(state, key, modifiers),
        EditMode::Visual { line_mode } => process_visual_mode(state, key, modifiers, line_mode),
        EditMode::Command => process_command_mode(state, key, modifiers),
    };

    // Log mode changes
    if result.0.mode != original_mode {
        tracing::debug!("Mode changed: {:?} -> {:?}", original_mode, result.0.mode);
    }

    result
}

/// Process key press in Insert mode.
fn process_insert_mode(
    state: EditorState,
    key: keyboard::Key,
    _modifiers: keyboard::Modifiers,
) -> (EditorState, Vec<Effect>) {
    match key {
        keyboard::Key::Named(keyboard::key::Named::Escape) => {
            let action = EditorAction::SetMode {
                mode: EditMode::Normal,
            };
            let new_state = apply_action(state, action);
            (new_state, vec![])
        },

        keyboard::Key::Named(keyboard::key::Named::Enter) => {
            let action = EditorAction::InsertText {
                position: state.cursor_position(),
                text: "\n".to_string(),
            };
            let new_state = apply_action(state, action);
            (new_state, vec![])
        },

        keyboard::Key::Named(keyboard::key::Named::Backspace) => {
            let cursor_pos = state.cursor_position();
            if cursor_pos.line > 0 || cursor_pos.column > 0 {
                let delete_pos = if cursor_pos.column > 0 {
                    TextPosition::new(cursor_pos.line, cursor_pos.column - 1)
                } else {
                    // Delete line break - move to end of previous line
                    let prev_line_len = state
                        .line(cursor_pos.line - 1)
                        .map(|l| l.len())
                        .unwrap_or(0);
                    TextPosition::new(cursor_pos.line - 1, prev_line_len)
                };

                let action = EditorAction::DeleteText {
                    range: TextRange::new(delete_pos, cursor_pos),
                };
                let new_state = apply_action(state, action);
                (new_state, vec![])
            } else {
                (state, vec![])
            }
        },

        keyboard::Key::Character(smol_str) => {
            let ch = smol_str.as_str();
            let action = EditorAction::InsertText {
                position: state.cursor_position(),
                text: ch.to_string(),
            };
            let new_state = apply_action(state, action);
            (new_state, vec![])
        },

        _ => (state, vec![]),
    }
}

/// Process key press in Normal mode (simplified vim commands).
fn process_normal_mode(
    state: EditorState,
    key: keyboard::Key,
    _modifiers: keyboard::Modifiers,
) -> (EditorState, Vec<Effect>) {
    match key {
        // Exit on Escape key
        keyboard::Key::Named(keyboard::key::Named::Escape) => {
            tracing::info!("Escape pressed in Normal mode, exiting application");
            (state, vec![Effect::Exit])
        },

        // Enter insert mode
        keyboard::Key::Character(s) if s.as_str() == "i" => {
            let action = EditorAction::SetMode {
                mode: EditMode::Insert,
            };
            let new_state = apply_action(state, action);
            (new_state, vec![])
        },

        // Movement keys
        keyboard::Key::Character(s) if s.as_str() == "h" => {
            let pos = move_cursor_left(&state);
            let action = EditorAction::MoveCursor { position: pos };
            let new_state = apply_action(state, action);
            (new_state, vec![])
        },

        keyboard::Key::Character(s) if s.as_str() == "j" => {
            let pos = move_cursor_down(&state);
            let action = EditorAction::MoveCursor { position: pos };
            let new_state = apply_action(state, action);
            (new_state, vec![])
        },

        keyboard::Key::Character(s) if s.as_str() == "k" => {
            let pos = move_cursor_up(&state);
            let action = EditorAction::MoveCursor { position: pos };
            let new_state = apply_action(state, action);
            (new_state, vec![])
        },

        keyboard::Key::Character(s) if s.as_str() == "l" => {
            let pos = move_cursor_right(&state);
            let action = EditorAction::MoveCursor { position: pos };
            let new_state = apply_action(state, action);
            (new_state, vec![])
        },

        _ => (state, vec![]),
    }
}

/// Process key press in Visual mode (placeholder).
fn process_visual_mode(
    state: EditorState,
    _key: keyboard::Key,
    _modifiers: keyboard::Modifiers,
    _line_mode: bool,
) -> (EditorState, Vec<Effect>) {
    // TODO: Implement visual mode
    (state, vec![])
}

/// Process key press in Command mode (placeholder).
fn process_command_mode(
    state: EditorState,
    _key: keyboard::Key,
    _modifiers: keyboard::Modifiers,
) -> (EditorState, Vec<Effect>) {
    // TODO: Implement command mode
    (state, vec![])
}

/// Apply an action to the state, returning new state.
fn apply_action(mut state: EditorState, action: EditorAction) -> EditorState {
    match action {
        EditorAction::InsertText { position, text } => {
            // Insert text and update cursor position
            let new_content = insert_text_at_position(&state.buffer.text(), position, &text);
            state.buffer = TextBuffer::with_text(&new_content);

            // Move cursor to end of inserted text
            let new_cursor_pos = advance_position_by_text(position, &text);
            state.cursor.position = new_cursor_pos;
            state.cursor.desired_column = new_cursor_pos.column;
            state.is_dirty = true;
        },

        EditorAction::DeleteText { range } => {
            let new_content = delete_text_in_range(&state.buffer.text(), range.clone());
            state.buffer = TextBuffer::with_text(&new_content);
            state.cursor.position = range.start;
            state.cursor.desired_column = range.start.column;
            state.is_dirty = true;
        },

        EditorAction::MoveCursor { position } => {
            state.cursor.position = position;
            state.cursor.desired_column = position.column;
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

        // TODO: Implement remaining actions
        _ => {},
    }

    state
}

// Helper functions for text manipulation and cursor movement

fn insert_text_at_position(text: &str, position: TextPosition, insert: &str) -> String {
    let mut result = String::new();
    let mut current_line = 0;
    let mut current_col = 0;

    for ch in text.chars() {
        if current_line == position.line && current_col == position.column {
            result.push_str(insert);
        }

        result.push(ch);

        if ch == '\n' {
            current_line += 1;
            current_col = 0;
        } else {
            current_col += 1;
        }
    }

    // Handle insertion at end of file
    if current_line == position.line && current_col == position.column {
        result.push_str(insert);
    }

    result
}

fn delete_text_in_range(text: &str, range: TextRange) -> String {
    let mut result = String::new();
    let mut current_line = 0;
    let mut current_col = 0;
    let mut in_delete_range = false;

    for ch in text.chars() {
        let pos = TextPosition::new(current_line, current_col);

        if pos == range.start {
            in_delete_range = true;
        }
        if pos == range.end {
            in_delete_range = false;
        }

        if !in_delete_range {
            result.push(ch);
        }

        if ch == '\n' {
            current_line += 1;
            current_col = 0;
        } else {
            current_col += 1;
        }
    }

    result
}

fn advance_position_by_text(start: TextPosition, text: &str) -> TextPosition {
    let mut line = start.line;
    let mut column = start.column;

    for ch in text.chars() {
        if ch == '\n' {
            line += 1;
            column = 0;
        } else {
            column += 1;
        }
    }

    TextPosition::new(line, column)
}

fn move_cursor_left(state: &EditorState) -> TextPosition {
    let pos = state.cursor_position();
    if pos.column > 0 {
        TextPosition::new(pos.line, pos.column - 1)
    } else if pos.line > 0 {
        let prev_line_len = state.line(pos.line - 1).map(|l| l.len()).unwrap_or(0);
        TextPosition::new(pos.line - 1, prev_line_len)
    } else {
        pos
    }
}

fn move_cursor_right(state: &EditorState) -> TextPosition {
    let pos = state.cursor_position();
    let current_line_len = state.line(pos.line).map(|l| l.len()).unwrap_or(0);

    if pos.column < current_line_len {
        TextPosition::new(pos.line, pos.column + 1)
    } else if pos.line < state.line_count().saturating_sub(1) {
        TextPosition::new(pos.line + 1, 0)
    } else {
        pos
    }
}

fn move_cursor_up(state: &EditorState) -> TextPosition {
    let pos = state.cursor_position();
    if pos.line > 0 {
        let target_column = state.cursor.desired_column;
        let prev_line_len = state.line(pos.line - 1).map(|l| l.len()).unwrap_or(0);
        let new_column = target_column.min(prev_line_len);
        TextPosition::new(pos.line - 1, new_column)
    } else {
        pos
    }
}

fn move_cursor_down(state: &EditorState) -> TextPosition {
    let pos = state.cursor_position();
    if pos.line < state.line_count().saturating_sub(1) {
        let target_column = state.cursor.desired_column;
        let next_line_len = state.line(pos.line + 1).map(|l| l.len()).unwrap_or(0);
        let new_column = target_column.min(next_line_len);
        TextPosition::new(pos.line + 1, new_column)
    } else {
        pos
    }
}

fn pixel_to_text_position(state: &EditorState, position: iced::Point) -> TextPosition {
    let line = ((position.y - state.viewport.scroll_y) / state.viewport.line_height) as usize;
    let column = ((position.x - state.viewport.scroll_x) / state.viewport.char_width) as usize;

    // Clamp to valid text boundaries
    let line = line.min(state.line_count().saturating_sub(1));
    let max_column = state.line(line).map(|l| l.len()).unwrap_or(0);
    let column = column.min(max_column);

    TextPosition::new(line, column)
}
