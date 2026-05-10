use gpui::Global;

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
