use gpui::{px, rgb, Hsla, Pixels};

/// Style configuration for the editor
pub struct EditorStyle {
    pub text_color: Hsla,
    pub background: Hsla,
    pub line_height: Pixels,
    pub font_size: Pixels,
    pub padding: Pixels,
    /// Whether to show line numbers in the gutter
    pub show_line_numbers: bool,
    /// Whether to show diff indicators in the gutter
    pub show_diff_indicators: bool,
    /// Background color for the gutter area
    pub gutter_background_color: Hsla,
    /// Color for added line indicators (green)
    pub diff_added_color: Hsla,
    /// Color for modified line indicators (blue)
    pub diff_modified_color: Hsla,
    /// Color for deleted line indicators (red)
    pub diff_deleted_color: Hsla,
}

impl Default for EditorStyle {
    fn default() -> Self {
        Self {
            text_color: rgb(0xcccccc).into(),
            background: rgb(0x1e1e1e).into(),
            line_height: px(20.0),
            font_size: px(14.0),
            padding: px(8.0),
            show_line_numbers: false,
            show_diff_indicators: true,
            gutter_background_color: rgb(0x1c1c1c).into(), // Slightly darker than editor bg
            diff_added_color: rgb(0x4ec9b0).into(),        // Green (VS Code green)
            diff_modified_color: rgb(0x569cd6).into(),     // Blue (VS Code blue)
            diff_deleted_color: rgb(0xf44747).into(),      // Red (VS Code red)
        }
    }
}
