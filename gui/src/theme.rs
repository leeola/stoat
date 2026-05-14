use crate::editor::render::ratatui_color_to_hsla;
use gpui::{rgb, App, Global, Hsla};

/// Default monospace font family used by the editor pane when the
/// active [`Settings`] does not override it. Picked to resolve on
/// gpui's primary platform without requiring the user to install
/// extra fonts.
///
/// [`Settings`]: crate::settings::Settings
pub const DEFAULT_EDITOR_FONT_FAMILY: &str = "Menlo";

/// Default editor pane font size in logical pixels. Mirrors the
/// size most editors ship with so a fresh install reads correctly
/// at 1x scaling.
pub const DEFAULT_EDITOR_FONT_SIZE: f32 = 14.0;

/// Default proportional font family for chrome (status bar, tab
/// bar, modals, dock panels). `.SystemUIFont` is the platform's
/// resolved system UI face on the gpui-supported platforms.
pub const DEFAULT_UI_FONT_FAMILY: &str = ".SystemUIFont";

/// Default chrome font size in logical pixels.
pub const DEFAULT_UI_FONT_SIZE: f32 = 14.0;

/// Fallback for the inactive border / divider color when no
/// theme overrides `ui.border.inactive`. Muted enough to read as
/// a separator against the default backgrounds.
pub const DEFAULT_BORDER_INACTIVE_HEX: u32 = 0x404040;

/// Fallback for the focus-ring color when no theme overrides
/// `ui.border.focused`. Subtle accent blue so the focused pane
/// reads as the active surface without dominating the chrome.
pub const DEFAULT_BORDER_FOCUSED_HEX: u32 = 0x6090ff;

/// Resolve the inactive border color from the active [`Theme`]
/// global, falling back to [`DEFAULT_BORDER_INACTIVE_HEX`] when
/// no theme is installed or the scope is unset.
pub fn border_inactive_color(cx: &App) -> Hsla {
    theme_fg_or(
        cx,
        stoat::theme::scope::UI_BORDER_INACTIVE,
        DEFAULT_BORDER_INACTIVE_HEX,
    )
}

/// Resolve the focus-ring color from the active [`Theme`] global,
/// falling back to [`DEFAULT_BORDER_FOCUSED_HEX`] when no theme is
/// installed or the scope is unset.
pub fn border_focused_color(cx: &App) -> Hsla {
    theme_fg_or(
        cx,
        stoat::theme::scope::UI_BORDER_FOCUSED,
        DEFAULT_BORDER_FOCUSED_HEX,
    )
}

fn theme_fg_or(cx: &App, scope: &str, fallback_hex: u32) -> Hsla {
    cx.try_global::<Theme>()
        .and_then(|t| t.0.try_get(scope))
        .and_then(|style| style.fg)
        .and_then(ratatui_color_to_hsla)
        .unwrap_or_else(|| rgb(fallback_hex).into())
}

/// App-global wrapper around [`stoat::theme::Theme`]. Stored via
/// [`gpui::App::set_global`] and observed via
/// [`gpui::App::observe_global::<Theme>`]. The inner value is the
/// resolved theme struct produced by the existing
/// `stoat_config::parse` -> `stoat::theme::Theme::from_config`
/// pipeline.
pub struct Theme(pub stoat::theme::Theme);

impl Global for Theme {}

impl Theme {
    /// Construct an empty [`Theme`] -- every scope lookup returns
    /// `None`. Used as the parse-failure fallback so the GUI never
    /// refuses to render due to a bad config.
    pub fn empty() -> Self {
        Self(stoat::theme::Theme::empty())
    }

    /// Resolve the named theme block from an already-parsed
    /// [`stoat_config::Config`]. Logs a tracing error and falls
    /// back to [`Theme::empty`] when the block is absent or fails
    /// to resolve.
    pub fn from_config(config: &stoat_config::Config, name: &str) -> Self {
        match stoat::theme::Theme::from_config(config, name) {
            Ok(inner) => Self(inner),
            Err(e) => {
                tracing::error!("theme '{name}' load failed: {e}");
                Self::empty()
            },
        }
    }

    /// Parse stcfg source text and resolve the named theme block.
    /// Falls back to [`Theme::empty`] if the source fails to parse.
    pub fn load_from_source(source: &str, name: &str) -> Self {
        let (config, _errors) = stoat_config::parse(source);
        match config {
            Some(c) => Self::from_config(&c, name),
            None => Self::empty(),
        }
    }
}
