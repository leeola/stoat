//! Fullscreen text editor widget that integrates with stoat_text buffers
//!
//! This module provides [`EditorWidget`], a fullscreen text editing component that integrates
//! directly with [`stoat_text::Buffer`] for efficient text editing. Unlike the simple text input
//! widgets, this provides a full-featured editor with syntax highlighting, line numbers, and
//! modal input support.

use crate::widget::theme::{Colors, Style};
use iced::{
    widget::{container, scrollable, text, Column, Row},
    Background, Border, Element, Length, Padding, Theme,
};
use stoat_core::buffer_manager::{BufferId, BufferManager};
use stoat_text::buffer::Buffer;

/// Configuration for the editor widget
#[derive(Debug, Clone)]
pub struct EditorConfig {
    /// Whether to show line numbers
    pub show_line_numbers: bool,
    /// Number of spaces per tab
    pub tab_size: usize,
    /// Whether to wrap long lines
    pub word_wrap: bool,
    /// Show syntax highlighting
    pub syntax_highlighting: bool,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            show_line_numbers: true,
            tab_size: 4,
            word_wrap: false,
            syntax_highlighting: true,
        }
    }
}

/// Messages for editor interaction
#[derive(Debug, Clone)]
pub enum EditorMessage {
    /// Character was inserted
    CharacterInserted(char),
    /// Key was pressed (for movement, deletion, etc.)
    KeyPressed(EditorKey),
    /// Buffer content changed
    BufferChanged(BufferId),
    /// Focus changed
    FocusChanged(bool),
    /// Scroll position changed
    ScrollChanged(f32, f32),
}

/// Editor-specific key commands
#[derive(Debug, Clone)]
pub enum EditorKey {
    /// Arrow keys for cursor movement
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    /// Text editing keys
    Backspace,
    Delete,
    Enter,
    Tab,
    /// Page navigation
    PageUp,
    PageDown,
    Home,
    End,
}

/// State for the editor widget
#[derive(Debug, Clone)]
pub struct EditorState {
    /// Currently active buffer
    pub active_buffer: Option<BufferId>,
    /// Editor configuration
    pub config: EditorConfig,
    /// Whether the editor has focus
    pub focused: bool,
    /// Current scroll position (x, y)
    pub scroll_position: (f32, f32),
    /// Cursor position for display (line, column)
    pub cursor_position: (usize, usize),
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            active_buffer: None,
            config: EditorConfig::default(),
            focused: false,
            scroll_position: (0.0, 0.0),
            cursor_position: (0, 0),
        }
    }
}

impl EditorState {
    /// Create a new editor state with a buffer
    pub fn with_buffer(buffer_id: BufferId) -> Self {
        Self {
            active_buffer: Some(buffer_id),
            ..Default::default()
        }
    }

    /// Set the active buffer
    pub fn set_active_buffer(&mut self, buffer_id: Option<BufferId>) {
        self.active_buffer = buffer_id;
    }

    /// Update cursor position
    pub fn set_cursor_position(&mut self, line: usize, column: usize) {
        self.cursor_position = (line, column);
    }

    /// Set focus state
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

/// Create a fullscreen editor widget
pub fn create_editor<'a, Message>(
    state: &EditorState,
    buffers: &BufferManager,
    _on_message: impl Fn(EditorMessage) -> Message + 'a,
) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    let content = if let Some(buffer_id) = state.active_buffer {
        if let Some(buffer) = buffers.get(buffer_id) {
            create_buffer_view(buffer, state)
        } else {
            create_empty_view("Buffer not found")
        }
    } else {
        create_empty_view("No buffer active")
    };

    // Wrap in a container with editor styling
    let border = if state.focused {
        Border {
            color: Colors::BORDER_FOCUSED,
            width: Style::BORDER_WIDTH,
            radius: Style::BORDER_RADIUS.into(),
        }
    } else {
        Border {
            color: Colors::BORDER_DEFAULT,
            width: Style::BORDER_WIDTH,
            radius: Style::BORDER_RADIUS.into(),
        }
    };

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding::new(Style::NODE_PADDING))
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(Colors::NODE_BACKGROUND)),
            border,
            ..Default::default()
        })
        .into()
}

/// Create a view for a specific buffer
fn create_buffer_view<'a, Message>(buffer: &Buffer, state: &EditorState) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    let content_text = buffer.rope().to_string();
    let lines: Vec<String> = content_text.lines().map(|s| s.to_string()).collect();

    let mut rows = Vec::new();

    // Create line content with optional line numbers
    for (line_idx, line_content) in lines.iter().enumerate() {
        let line_number = line_idx + 1;

        let line_row = if state.config.show_line_numbers {
            // Line number column
            let line_num_text = text(format!("{line_number:4}"))
                .font(iced::Font::MONOSPACE)
                .size(Style::TEXT_SIZE_SMALL)
                .color(Colors::TEXT_SECONDARY);

            // Line content
            let content_text = text(line_content.clone())
                .font(iced::Font::MONOSPACE)
                .size(Style::TEXT_SIZE_REGULAR)
                .color(Colors::TEXT_PRIMARY);

            Row::new()
                .push(
                    container(line_num_text)
                        .width(Length::Fixed(50.0))
                        .padding(Padding::new(8.0)),
                )
                .push(content_text)
                .into()
        } else {
            // Just line content
            text(line_content.clone())
                .font(iced::Font::MONOSPACE)
                .size(Style::TEXT_SIZE_REGULAR)
                .color(Colors::TEXT_PRIMARY)
                .into()
        };

        rows.push(line_row);
    }

    // Add cursor indicator if this line matches cursor position
    if state.focused {
        // FIXME: Add visual cursor indicator at cursor_position
        // This would require more sophisticated rendering
    }

    let column = Column::with_children(rows).spacing(2);

    // Wrap in scrollable container
    scrollable(column)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Create an empty view with a message
fn create_empty_view<'a, Message>(message: &'a str) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    container(
        text(message)
            .size(Style::TEXT_SIZE_REGULAR)
            .color(Colors::TEXT_SECONDARY),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .into()
}

/// Update editor state based on a message
pub fn update_editor_state(
    state: &mut EditorState,
    message: EditorMessage,
    _buffers: &mut BufferManager,
) -> bool {
    match message {
        EditorMessage::CharacterInserted(_ch) => {
            // FIXME: This would need to integrate with the buffer's text editing
            // For now, just mark as changed
            true
        },
        EditorMessage::KeyPressed(key) => {
            match key {
                EditorKey::ArrowUp => {
                    if state.cursor_position.0 > 0 {
                        state.cursor_position.0 -= 1;
                    }
                },
                EditorKey::ArrowDown => {
                    state.cursor_position.0 += 1;
                },
                EditorKey::ArrowLeft => {
                    if state.cursor_position.1 > 0 {
                        state.cursor_position.1 -= 1;
                    }
                },
                EditorKey::ArrowRight => {
                    state.cursor_position.1 += 1;
                },
                EditorKey::Home => {
                    state.cursor_position.1 = 0;
                },
                EditorKey::End => {
                    // FIXME: Should move to end of current line
                },
                _ => {
                    // FIXME: Implement other key handlers
                },
            }
            true
        },
        EditorMessage::BufferChanged(buffer_id) => {
            state.active_buffer = Some(buffer_id);
            state.cursor_position = (0, 0); // Reset cursor
            true
        },
        EditorMessage::FocusChanged(focused) => {
            state.focused = focused;
            true
        },
        EditorMessage::ScrollChanged(x, y) => {
            state.scroll_position = (x, y);
            true
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_core::buffer_manager::BufferManager;

    #[test]
    fn test_editor_state_default() {
        let state = EditorState::default();
        assert!(state.active_buffer.is_none());
        assert!(!state.focused);
        assert_eq!(state.cursor_position, (0, 0));
    }

    #[test]
    fn test_editor_state_with_buffer() {
        let buffer_id = BufferId(1);
        let state = EditorState::with_buffer(buffer_id);
        assert_eq!(state.active_buffer, Some(buffer_id));
    }

    #[test]
    fn test_editor_config_default() {
        let config = EditorConfig::default();
        assert!(config.show_line_numbers);
        assert_eq!(config.tab_size, 4);
        assert!(!config.word_wrap);
        assert!(config.syntax_highlighting);
    }

    #[test]
    fn test_update_cursor_movement() {
        let mut state = EditorState::default();
        let mut buffers = BufferManager::new();

        // Test arrow key movements
        update_editor_state(
            &mut state,
            EditorMessage::KeyPressed(EditorKey::ArrowDown),
            &mut buffers,
        );
        assert_eq!(state.cursor_position, (1, 0));

        update_editor_state(
            &mut state,
            EditorMessage::KeyPressed(EditorKey::ArrowRight),
            &mut buffers,
        );
        assert_eq!(state.cursor_position, (1, 1));

        update_editor_state(
            &mut state,
            EditorMessage::KeyPressed(EditorKey::ArrowUp),
            &mut buffers,
        );
        assert_eq!(state.cursor_position, (0, 1));

        update_editor_state(
            &mut state,
            EditorMessage::KeyPressed(EditorKey::ArrowLeft),
            &mut buffers,
        );
        assert_eq!(state.cursor_position, (0, 0));
    }

    #[test]
    fn test_update_focus_change() {
        let mut state = EditorState::default();
        let mut buffers = BufferManager::new();

        assert!(!state.focused);

        update_editor_state(&mut state, EditorMessage::FocusChanged(true), &mut buffers);
        assert!(state.focused);

        update_editor_state(&mut state, EditorMessage::FocusChanged(false), &mut buffers);
        assert!(!state.focused);
    }
}
