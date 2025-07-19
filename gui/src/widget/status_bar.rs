use crate::widget::theme::{Colors, Style};
use iced::{
    alignment::{Horizontal, Vertical},
    widget::{container, row, text, Row},
    Background, Border, Element, Length,
};

/// Status bar widget for displaying editor state
pub struct StatusBar {
    mode: Mode,
    cursor_position: Option<(usize, usize)>,
    project_name: Option<String>,
}

/// Editor modes with associated colors and labels
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    Command,
}

impl Mode {
    fn label(&self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => "VISUAL",
            Mode::Command => "COMMAND",
        }
    }
}

impl StatusBar {
    /// Create a status bar element directly
    pub fn create<'a, Message: 'a>(
        mode_str: &str,
        project_name: Option<String>,
    ) -> Element<'a, Message> {
        let mode = match mode_str.to_lowercase().as_str() {
            "insert" => Mode::Insert,
            "visual" => Mode::Visual,
            "command" => Mode::Command,
            _ => Mode::Normal,
        };

        Self::build_view(mode, None, project_name)
    }

    /// Create a new status bar
    pub fn new(mode_str: &str) -> Self {
        let mode = match mode_str.to_lowercase().as_str() {
            "insert" => Mode::Insert,
            "visual" => Mode::Visual,
            "command" => Mode::Command,
            _ => Mode::Normal,
        };

        Self {
            mode,
            cursor_position: None,
            project_name: None,
        }
    }

    /// Set cursor position
    pub fn cursor_position(mut self, line: usize, col: usize) -> Self {
        self.cursor_position = Some((line, col));
        self
    }

    /// Set project name
    pub fn project_name(mut self, name: String) -> Self {
        self.project_name = Some(name);
        self
    }

    /// Convert to iced Element
    pub fn view<'a, Message: 'a>(&'a self) -> Element<'a, Message> {
        Self::build_view(self.mode, self.cursor_position, self.project_name.clone())
    }

    /// Build the view without requiring self reference
    fn build_view<'a, Message: 'a>(
        mode: Mode,
        cursor_position: Option<(usize, usize)>,
        project_name: Option<String>,
    ) -> Element<'a, Message> {
        let mode_text = Self::build_mode_text(mode);
        let cursor_info = Self::build_cursor_info(cursor_position);
        let project_info = Self::build_project_info(project_name);

        let content: Row<'a, Message> = row![mode_text]
            .push_maybe(cursor_info)
            .push_maybe(project_info)
            .spacing(Style::SPACING_LARGE);

        container(content)
            .width(Length::Fill)
            .height(Length::Fixed(24.0))
            .padding([2, Style::SPACING_MEDIUM as u16])
            .style(|_theme: &iced::Theme| container::Style {
                background: Some(Background::Color(Colors::NODE_TITLE_BACKGROUND)),
                border: Border {
                    color: Colors::BORDER_DEFAULT,
                    width: 0.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            })
            .into()
    }

    fn build_mode_text<'a, Message: 'a>(mode: Mode) -> Element<'a, Message> {
        text(format!("-- {} --", mode.label()))
            .size(Style::TEXT_SIZE_SMALL)
            .color(Colors::TEXT_PRIMARY)
            .into()
    }

    fn build_cursor_info<'a, Message: 'a>(
        cursor_position: Option<(usize, usize)>,
    ) -> Option<Element<'a, Message>> {
        cursor_position.map(|(line, col)| {
            let cursor_text = text(format!("{}:{}", line + 1, col + 1))
                .size(Style::TEXT_SIZE_SMALL)
                .color(Colors::TEXT_TERTIARY);

            container(cursor_text)
                .align_x(Horizontal::Center)
                .align_y(Vertical::Center)
                .into()
        })
    }

    fn build_project_info<'a, Message: 'a>(
        project_name: Option<String>,
    ) -> Option<Element<'a, Message>> {
        project_name.map(|name| {
            let project_text = text(name)
                .size(Style::TEXT_SIZE_SMALL)
                .color(Colors::TEXT_TERTIARY);

            container(project_text)
                .align_x(Horizontal::Right)
                .align_y(Vertical::Center)
                .width(Length::Fill)
                .into()
        })
    }
}
