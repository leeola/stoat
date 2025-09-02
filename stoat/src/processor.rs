//! Core event processing engine.
//!
//! This module contains the pure functions that process events and transform
//! editor state. All business logic lives here as testable, pure functions.

use crate::{
    actions::{EditMode, EditorAction, TextPosition},
    command::Command,
    effects::Effect,
    events::EditorEvent,
    keymap::Keymap,
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

        // TODO: Implement remaining events
        _ => (state, vec![]),
    };

    tracing::debug!("Event processed, effects count: {}", result.1.len());
    result
}

/// Process keyboard input using the command system.
fn process_key_press(
    state: EditorState,
    key: keyboard::Key,
    modifiers: keyboard::Modifiers,
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
        if let keyboard::Key::Character(smol_str) = &key {
            let ch = smol_str.chars().next().unwrap_or('\0');
            let command = Command::InsertChar(ch);
            tracing::debug!("Insert mode char: {:?}", command);
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
    match command {
        Command::Exit => return (state, vec![Effect::Exit]),
        _ => {},
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
            // TODO: Implement direct rope text insertion
            // Need: position conversion, token creation, rope edit operations
            todo!(
                "Direct rope text insertion at TextPosition {:?} with text {:?}",
                position,
                text
            );
        },

        EditorAction::DeleteText { range } => {
            // TODO: Implement direct rope text deletion
            // Need: range conversion, rope delete operations
            todo!("Direct rope text deletion for range {:?}", range);
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

        EditorAction::ToggleCommandInfo => {
            state.show_command_info = !state.show_command_info;
        },

        // TODO: Implement remaining actions
        _ => {},
    }

    state
}

// Helper functions for text manipulation and cursor movement

fn pixel_to_text_position(state: &EditorState, position: iced::Point) -> TextPosition {
    let line = ((position.y - state.viewport.scroll_y) / state.viewport.line_height) as usize;
    let column = ((position.x - state.viewport.scroll_x) / state.viewport.char_width) as usize;

    // Clamp to valid text boundaries
    let line = line.min(state.line_count().saturating_sub(1));
    let max_column = state.line(line).map(|l| l.len()).unwrap_or(0);
    let column = column.min(max_column);

    TextPosition::new(line, column)
}
