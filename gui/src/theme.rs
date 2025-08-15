//! Visual theming for the editor application.

use iced::{Color, Font};

/// Visual styling configuration for the editor.
#[derive(Debug, Clone)]
pub struct EditorTheme {
    /// Font to use for text rendering
    pub font: Font,
    /// Font size in points
    pub font_size: f32,
    /// Line height multiplier
    pub line_height: f32,
    /// Text color
    pub text_color: Color,
    /// Background color
    pub background_color: Color,
    /// Cursor color
    pub cursor_color: Color,
    /// Text selection highlight color
    pub selection_color: Color,
    /// Line number color
    pub line_number_color: Color,
    /// Whether to show line numbers
    pub show_line_numbers: bool,
    /// Status bar background color
    pub status_bg_color: Color,
    /// Status bar text color
    pub status_text_color: Color,
}

impl Default for EditorTheme {
    fn default() -> Self {
        Self::dark()
    }
}

impl EditorTheme {
    /// Dark theme (default)
    pub fn dark() -> Self {
        Self {
            font: Font::MONOSPACE,
            font_size: 14.0,
            line_height: 1.4,
            text_color: Color::from_rgb(0.9, 0.9, 0.9),
            background_color: Color::from_rgb(0.1, 0.1, 0.1),
            cursor_color: Color::WHITE,
            selection_color: Color::from_rgba(0.2, 0.4, 0.8, 0.3),
            line_number_color: Color::from_rgb(0.5, 0.5, 0.5),
            show_line_numbers: true,
            status_bg_color: Color::from_rgb(0.15, 0.15, 0.15),
            status_text_color: Color::from_rgb(0.8, 0.8, 0.8),
        }
    }

    /// Light theme
    pub fn light() -> Self {
        Self {
            font: Font::MONOSPACE,
            font_size: 14.0,
            line_height: 1.4,
            text_color: Color::BLACK,
            background_color: Color::WHITE,
            cursor_color: Color::BLACK,
            selection_color: Color::from_rgba(0.0, 0.5, 1.0, 0.3),
            line_number_color: Color::from_rgb(0.5, 0.5, 0.5),
            show_line_numbers: true,
            status_bg_color: Color::from_rgb(0.9, 0.9, 0.9),
            status_text_color: Color::from_rgb(0.2, 0.2, 0.2),
        }
    }

    /// Calculate character width for monospace font
    pub fn char_width(&self) -> f32 {
        // Approximation for monospace fonts
        self.font_size * 0.6
    }

    /// Calculate line height in pixels
    pub fn line_height_px(&self) -> f32 {
        self.font_size * self.line_height
    }
}
