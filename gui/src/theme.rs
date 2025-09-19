//! Theme system for the editor

use gpui::{hsla, Global, Hsla};

/// Editor theme configuration
#[derive(Debug, Clone)]
pub struct EditorTheme {
    pub background: Hsla,
    pub foreground: Hsla,
    pub line_number: Hsla,
    pub status_bar_bg: Hsla,
    pub status_bar_fg: Hsla,
    pub comment: Hsla,
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
            status_bar_bg: hsla(220.0 / 360.0, 0.13, 0.13, 1.0), // #21252b
            status_bar_fg: hsla(220.0 / 360.0, 0.09, 0.55, 1.0), // #828997
            comment: hsla(220.0 / 360.0, 0.10, 0.40, 1.0),    // #5c6370
        }
    }
}

/// Global theme settings
pub struct ThemeSettings {}

impl ThemeSettings {
    pub fn new() -> Self {
        Self {}
    }
}

impl Global for ThemeSettings {}
