use gpui::Global;

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

/// App-global wrapper around [`stoat::theme::Theme`]. Stored via
/// [`gpui::App::set_global`] and observed via
/// [`gpui::App::observe_global::<Theme>`]. The inner value is the
/// resolved theme struct produced by the existing
/// `stoat_config::parse` -> `stoat::theme::Theme::from_config`
/// pipeline.
pub struct Theme(pub stoat::theme::Theme);

impl Global for Theme {}

impl Theme {
    /// Parse stcfg source text and resolve the named theme block.
    /// Falls back to [`stoat::theme::Theme::empty`] if the source
    /// fails to parse or the named block is absent, so the GUI
    /// never refuses to render due to a bad config.
    pub fn load_from_source(source: &str, name: &str) -> Self {
        let (config, _errors) = stoat_config::parse(source);
        let inner = config
            .and_then(|c| stoat::theme::Theme::from_config(&c, name).ok())
            .unwrap_or_else(stoat::theme::Theme::empty);
        Self(inner)
    }
}
