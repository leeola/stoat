//! Theme system for the editor

use gpui::{hsla, rgb, Global, Hsla};

/// Editor theme configuration
#[derive(Debug, Clone)]
pub struct EditorTheme {
    pub background: Hsla,
    pub foreground: Hsla,
    pub line_number: Hsla,
    pub cursor_normal: Hsla,
    pub cursor_insert: Hsla,
    pub cursor_visual: Hsla,
    pub cursor_command: Hsla,
    pub selection: Hsla,
    pub status_bar_bg: Hsla,
    pub status_bar_fg: Hsla,
    pub comment: Hsla,
    pub keyword: Hsla,
    pub string: Hsla,
    pub number: Hsla,
    pub function: Hsla,
    pub type_color: Hsla,
    pub operator: Hsla,
}

impl Default for EditorTheme {
    fn default() -> Self {
        Self::dark()
    }
}

impl EditorTheme {
    /// Dark theme (based on One Dark)
    pub fn dark() -> Self {
        Self {
            background: hsla(220.0 / 360.0, 0.13, 0.18, 1.0), // #282c34
            foreground: hsla(220.0 / 360.0, 0.14, 0.71, 1.0), // #abb2bf
            line_number: hsla(220.0 / 360.0, 0.10, 0.40, 1.0), // #5c6370
            cursor_normal: hsla(220.0 / 360.0, 1.0, 0.66, 1.0), // #61afef
            cursor_insert: hsla(95.0 / 360.0, 0.38, 0.62, 1.0), // #98c379
            cursor_visual: hsla(286.0 / 360.0, 0.35, 0.65, 1.0), // #c678dd
            cursor_command: hsla(355.0 / 360.0, 0.65, 0.65, 1.0), // #e06c75
            selection: hsla(220.0 / 360.0, 0.13, 0.28, 0.8),  // #3e4451
            status_bar_bg: hsla(220.0 / 360.0, 0.13, 0.13, 1.0), // #21252b
            status_bar_fg: hsla(220.0 / 360.0, 0.09, 0.55, 1.0), // #828997
            comment: hsla(220.0 / 360.0, 0.10, 0.40, 1.0),    // #5c6370
            keyword: hsla(286.0 / 360.0, 0.35, 0.65, 1.0),    // #c678dd
            string: hsla(95.0 / 360.0, 0.38, 0.62, 1.0),      // #98c379
            number: hsla(29.0 / 360.0, 0.54, 0.61, 1.0),      // #d19a66
            function: hsla(207.0 / 360.0, 0.82, 0.66, 1.0),   // #61afef
            type_color: hsla(39.0 / 360.0, 0.67, 0.69, 1.0),  // #e5c07b
            operator: hsla(187.0 / 360.0, 0.47, 0.55, 1.0),   // #56b6c2
        }
    }

    /// Light theme (based on One Light)
    pub fn light() -> Self {
        Self {
            background: hsla(0.0, 0.0, 0.98, 1.0),               // #fafafa
            foreground: hsla(220.0 / 360.0, 0.09, 0.23, 1.0),    // #383a42
            line_number: hsla(220.0 / 360.0, 0.05, 0.63, 1.0),   // #9d9d9f
            cursor_normal: hsla(230.0 / 360.0, 0.97, 0.62, 1.0), // #4078f2
            cursor_insert: hsla(98.0 / 360.0, 0.35, 0.42, 1.0),  // #50a14f
            cursor_visual: hsla(286.0 / 360.0, 0.29, 0.50, 1.0), // #a626a4
            cursor_command: hsla(5.0 / 360.0, 0.74, 0.59, 1.0),  // #e45649
            selection: hsla(230.0 / 360.0, 0.20, 0.90, 0.8),     // #e5e5e6
            status_bar_bg: hsla(0.0, 0.0, 0.93, 1.0),            // #eeeeee
            status_bar_fg: hsla(220.0 / 360.0, 0.09, 0.45, 1.0), // #696c77
            comment: hsla(220.0 / 360.0, 0.05, 0.63, 1.0),       // #a0a1a7
            keyword: hsla(286.0 / 360.0, 0.29, 0.50, 1.0),       // #a626a4
            string: hsla(98.0 / 360.0, 0.35, 0.42, 1.0),         // #50a14f
            number: hsla(31.0 / 360.0, 0.61, 0.46, 1.0),         // #986801
            function: hsla(230.0 / 360.0, 0.97, 0.62, 1.0),      // #4078f2
            type_color: hsla(35.0 / 360.0, 0.82, 0.43, 1.0),     // #c18401
            operator: hsla(184.0 / 360.0, 0.91, 0.34, 1.0),      // #0184bc
        }
    }
}

/// Global theme settings
pub struct ThemeSettings {
    pub current_theme: EditorTheme,
}

impl ThemeSettings {
    pub fn new() -> Self {
        Self {
            current_theme: EditorTheme::default(),
        }
    }
}

impl Global for ThemeSettings {}
