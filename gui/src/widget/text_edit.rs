//! Interactive text editing widget for GUI text input
//!
//! This module provides [`TextEditWidget`], an interactive text editing component that supports
//! multi-line editing, cursor management, and real-time text modification. Unlike the canvas-based
//! text viewing widgets, this provides actual text input capabilities.

use crate::widget::theme::{Colors, Style};
use iced::{
    widget::{container, text_editor, TextEditor},
    Background, Border, Element, Length, Padding, Theme,
};

/// Simple function to create a styled text editor widget  
pub fn create_text_editor<'a, Message>(
    content: &'a text_editor::Content,
    placeholder: &'a str,
    focused: bool,
) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    let text_editor = TextEditor::new(content)
        .placeholder(placeholder)
        .font(iced::Font::MONOSPACE)
        .size(Style::TEXT_SIZE_REGULAR);

    // Style the container based on focus state
    let border = if focused {
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

    let background = Background::Color(Colors::NODE_BACKGROUND);

    container(text_editor)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding::new(Style::NODE_PADDING))
        .style(move |_theme: &Theme| container::Style {
            background: Some(background),
            border,
            ..Default::default()
        })
        .into()
}

/// Text editing messages for handling user input
#[derive(Debug, Clone)]
pub enum TextEditMessage {
    /// Content changed via user input
    ContentChanged(text_editor::Action),
    /// Focus state changed
    FocusChanged(bool),
    /// Cursor position changed
    CursorMoved(usize),
}
