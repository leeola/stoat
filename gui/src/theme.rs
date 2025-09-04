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
    /// Small font size for UI elements
    pub small_font_size: f32,
    /// Command info panel background color
    pub command_info_bg_color: Color,
    /// Command info panel border color  
    pub command_info_border_color: Color,
    /// Command info panel text color
    pub command_info_text_color: Color,
    /// Command info panel title color
    pub command_info_title_color: Color,
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
            small_font_size: 10.0,
            command_info_bg_color: Color::from_rgba(0.0, 0.0, 0.0, 0.8),
            command_info_border_color: Color::from_rgb(0.3, 0.3, 0.3),
            command_info_text_color: Color::from_rgb(0.9, 0.9, 0.9),
            command_info_title_color: Color::from_rgb(1.0, 1.0, 0.8),
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
            small_font_size: 10.0,
            command_info_bg_color: Color::from_rgba(1.0, 1.0, 1.0, 0.9),
            command_info_border_color: Color::from_rgb(0.7, 0.7, 0.7),
            command_info_text_color: Color::from_rgb(0.1, 0.1, 0.1),
            command_info_title_color: Color::from_rgb(0.2, 0.2, 0.8),
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

    /// Get small font size for UI elements
    pub fn small_font_size(&self) -> f32 {
        self.small_font_size
    }

    /// Get command info panel background color
    pub fn command_info_bg_color(&self) -> Color {
        self.command_info_bg_color
    }

    /// Get command info panel border color
    pub fn command_info_border_color(&self) -> Color {
        self.command_info_border_color
    }

    /// Get command info panel text color
    pub fn command_info_text_color(&self) -> Color {
        self.command_info_text_color
    }

    /// Get command info panel title color
    pub fn command_info_title_color(&self) -> Color {
        self.command_info_title_color
    }
}
