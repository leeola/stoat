use gpui::{px, rgb, Hsla, Pixels};

/// Style configuration for the editor
pub struct EditorStyle {
    pub text_color: Hsla,
    pub background: Hsla,
    pub line_height: Pixels,
    pub font_size: Pixels,
    pub padding: Pixels,
    /// Width of the gutter (left margin) in pixels
    pub gutter_width: Pixels,
    /// Padding inside the gutter area
    pub gutter_padding: Pixels,
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
            padding: px(20.0),
            gutter_width: px(40.0),
            gutter_padding: px(4.0),
            diff_added_color: rgb(0x4ec9b0).into(), // Green (VS Code green)
            diff_modified_color: rgb(0x569cd6).into(), // Blue (VS Code blue)
            diff_deleted_color: rgb(0xf44747).into(), // Red (VS Code red)
        }
    }
}
