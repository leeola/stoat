use gpui::{Hsla, Pixels, px, rgb};

/// Style configuration for the editor
pub struct EditorStyle {
    pub text_color: Hsla,
    pub background: Hsla,
    pub line_height: Pixels,
    pub font_size: Pixels,
    pub padding: Pixels,
}

impl Default for EditorStyle {
    fn default() -> Self {
        Self {
            text_color: rgb(0xcccccc).into(),
            background: rgb(0x1e1e1e).into(),
            line_height: px(20.0),
            font_size: px(14.0),
            padding: px(20.0),
        }
    }
}
