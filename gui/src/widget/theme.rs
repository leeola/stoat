use iced::{Background, Border, Color, Shadow, Vector};

/// Shared visual constants for consistent widget styling
pub struct Style;

impl Style {
    /// Border constants
    pub const BORDER_WIDTH: f32 = 1.0;
    pub const BORDER_RADIUS: f32 = 4.0;
    pub const BORDER_RADIUS_LARGE: f32 = 8.0;

    /// Spacing and padding
    pub const SPACING_SMALL: f32 = 4.0;
    pub const SPACING_MEDIUM: f32 = 8.0;
    pub const SPACING_LARGE: f32 = 16.0;
    pub const SPACING_XLARGE: f32 = 24.0;

    /// Node-specific dimensions
    pub const NODE_MIN_WIDTH: f32 = 240.0;
    pub const NODE_MIN_HEIGHT: f32 = 120.0;
    pub const NODE_TITLE_HEIGHT: f32 = 32.0;
    pub const NODE_PADDING: f32 = 12.0;
    pub const NODE_TITLE_PADDING: f32 = 10.0;

    /// Typography
    pub const TEXT_SIZE_SMALL: f32 = 11.0;
    pub const TEXT_SIZE_REGULAR: f32 = 13.0;
    pub const TEXT_SIZE_TITLE: f32 = 14.0;
    pub const TEXT_SIZE_LARGE: f32 = 16.0;
    pub const LINE_HEIGHT: f32 = 20.0;

    /// Animation durations (for future use)
    pub const TRANSITION_FAST: f32 = 0.15;
    pub const TRANSITION_MEDIUM: f32 = 0.25;
    pub const TRANSITION_SLOW: f32 = 0.35;
}

/// Dark theme colors inspired by modern editor themes
pub struct Colors;

impl Colors {
    /// Background colors
    pub const CANVAS_BACKGROUND: Color = Color::from_rgb(0.11, 0.11, 0.12); // #1c1c1e
    pub const NODE_BACKGROUND: Color = Color::from_rgb(0.14, 0.14, 0.15); // #242427
    pub const NODE_BACKGROUND_HOVER: Color = Color::from_rgb(0.16, 0.16, 0.18); // #292a2d
    pub const NODE_TITLE_BACKGROUND: Color = Color::from_rgb(0.13, 0.13, 0.14); // #212124

    /// Border colors
    pub const BORDER_DEFAULT: Color = Color::from_rgb(0.22, 0.22, 0.24); // #383840
    pub const BORDER_HOVER: Color = Color::from_rgb(0.30, 0.30, 0.33); // #4d4d54
    pub const BORDER_FOCUSED: Color = Color::from_rgb(0.38, 0.56, 0.86); // #6190db
    pub const BORDER_SELECTED: Color = Color::from_rgb(0.45, 0.65, 0.95); // #73a6f3

    /// Text colors
    pub const TEXT_PRIMARY: Color = Color::from_rgb(0.92, 0.92, 0.93); // #ebebed
    pub const TEXT_SECONDARY: Color = Color::from_rgb(0.65, 0.65, 0.68); // #a6a6ad
    pub const TEXT_TERTIARY: Color = Color::from_rgb(0.45, 0.45, 0.48); // #73737a
    pub const TEXT_DISABLED: Color = Color::from_rgb(0.35, 0.35, 0.38); // #595961

    /// Accent colors
    pub const ACCENT_PRIMARY: Color = Color::from_rgb(0.38, 0.56, 0.86); // #6190db
    pub const ACCENT_SUCCESS: Color = Color::from_rgb(0.45, 0.70, 0.45); // #73b373
    pub const ACCENT_WARNING: Color = Color::from_rgb(0.90, 0.70, 0.40); // #e6b366
    pub const ACCENT_ERROR: Color = Color::from_rgb(0.85, 0.45, 0.45); // #d97373

    /// Socket colors (for node connections)
    pub const SOCKET_INPUT: Color = Color::from_rgb(0.40, 0.65, 0.40); // #66a666
    pub const SOCKET_OUTPUT: Color = Color::from_rgb(0.65, 0.40, 0.40); // #a66666
    pub const SOCKET_ACTIVE: Color = Color::from_rgb(0.45, 0.70, 0.95); // #73b3f3

    /// Selection and highlights
    pub const SELECTION_BACKGROUND: Color = Color::from_rgba(0.38, 0.56, 0.86, 0.20); // #6190db33
    pub const HIGHLIGHT_BACKGROUND: Color = Color::from_rgba(0.90, 0.70, 0.40, 0.15); // #e6b36626

    /// Shadows
    pub const SHADOW_COLOR: Color = Color::from_rgba(0.0, 0.0, 0.0, 0.25);
    pub const SHADOW_COLOR_LIGHT: Color = Color::from_rgba(0.0, 0.0, 0.0, 0.15);
}

/// Pre-configured widget styles
pub struct Styles;

impl Styles {
    /// Standard border style
    pub fn border_default() -> Border {
        Border {
            color: Colors::BORDER_DEFAULT,
            width: Style::BORDER_WIDTH,
            radius: Style::BORDER_RADIUS.into(),
        }
    }

    /// Focused border style
    pub fn border_focused() -> Border {
        Border {
            color: Colors::BORDER_FOCUSED,
            width: Style::BORDER_WIDTH,
            radius: Style::BORDER_RADIUS.into(),
        }
    }

    /// Selected border style
    pub fn border_selected() -> Border {
        Border {
            color: Colors::BORDER_SELECTED,
            width: Style::BORDER_WIDTH * 1.5,
            radius: Style::BORDER_RADIUS.into(),
        }
    }

    /// Standard shadow
    pub fn shadow_default() -> Shadow {
        Shadow {
            color: Colors::SHADOW_COLOR,
            offset: Vector::new(0.0, 2.0),
            blur_radius: 4.0,
        }
    }

    /// Elevated shadow (for hovering elements)
    pub fn shadow_elevated() -> Shadow {
        Shadow {
            color: Colors::SHADOW_COLOR,
            offset: Vector::new(0.0, 4.0),
            blur_radius: 8.0,
        }
    }

    /// Node background
    pub fn node_background() -> Background {
        Background::Color(Colors::NODE_BACKGROUND)
    }

    /// Node title background
    pub fn node_title_background() -> Background {
        Background::Color(Colors::NODE_TITLE_BACKGROUND)
    }
}
