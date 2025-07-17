use iced::Color;

/// Dark theme colors optimized for text editing
pub struct Theme;

impl Theme {
    /// Background color of the canvas
    pub const CANVAS_BACKGROUND: Color = Color::from_rgb(0.12, 0.12, 0.14);

    /// Node colors
    pub const NODE_BACKGROUND: Color = Color::from_rgb(0.16, 0.16, 0.18);
    pub const NODE_BORDER: Color = Color::from_rgb(0.3, 0.3, 0.35);
    pub const NODE_BORDER_FOCUSED: Color = Color::from_rgb(0.4, 0.6, 0.9);
    pub const NODE_BORDER_SELECTED: Color = Color::from_rgb(0.5, 0.7, 1.0);

    /// Node title bar
    pub const NODE_TITLE_BACKGROUND: Color = Color::from_rgb(0.2, 0.2, 0.22);
    pub const NODE_TITLE_TEXT: Color = Color::from_rgb(0.9, 0.9, 0.9);

    /// Text colors
    pub const TEXT_PRIMARY: Color = Color::from_rgb(0.85, 0.85, 0.87);
    pub const TEXT_SECONDARY: Color = Color::from_rgb(0.6, 0.6, 0.65);
    pub const TEXT_SELECTION: Color = Color::from_rgba(0.3, 0.5, 0.8, 0.3);

    /// Cursor
    pub const CURSOR_COLOR: Color = Color::from_rgb(0.9, 0.9, 0.9);

    /// Socket colors (for future use)
    pub const SOCKET_INPUT: Color = Color::from_rgb(0.4, 0.7, 0.4);
    pub const SOCKET_OUTPUT: Color = Color::from_rgb(0.7, 0.4, 0.4);
}

/// Layout constants
pub struct Layout;

impl Layout {
    /// Node dimensions
    pub const NODE_MIN_WIDTH: f32 = 200.0;
    pub const NODE_MIN_HEIGHT: f32 = 100.0;
    pub const NODE_BORDER_WIDTH: f32 = 2.0;
    pub const NODE_BORDER_RADIUS: f32 = 8.0;

    /// Node title bar
    pub const NODE_TITLE_HEIGHT: f32 = 28.0;
    pub const NODE_TITLE_PADDING: f32 = 8.0;

    /// Content area
    pub const NODE_CONTENT_PADDING: f32 = 12.0;

    /// Text rendering
    pub const TEXT_SIZE: f32 = 14.0;
    pub const LINE_HEIGHT: f32 = 20.0;
    pub const CURSOR_WIDTH: f32 = 2.0;

    /// Socket dimensions (for future use)
    pub const SOCKET_RADIUS: f32 = 6.0;
    pub const SOCKET_SPACING: f32 = 24.0;
}

/// Font settings
pub struct Fonts;

impl Fonts {
    /// Default monospace font family
    pub const MONO_FAMILY: &'static str = "monospace";
    /// UI font family
    pub const UI_FAMILY: &'static str = "sans-serif";
}
