use gpui::Global;

/// App-global wrapper around [`stoat_config::Settings`]. Stored via
/// [`gpui::App::set_global`] and observed via
/// [`gpui::App::observe_global::<Settings>`]. The inner value is the
/// resolved settings struct produced by the existing
/// `stoat_config::parse` -> `Settings::from_config` pipeline.
pub struct Settings(pub stoat_config::Settings);

impl Global for Settings {}

impl Settings {
    /// Parse stcfg source text into [`Settings`]. Parse errors are
    /// discarded -- the existing pipeline silently falls back to a
    /// default config when parsing fails, matching the behavior of
    /// the TUI's loader.
    pub fn load_from_source(source: &str) -> Self {
        let (config, _errors) = stoat_config::parse(source);
        let inner = config
            .map(|c| stoat_config::Settings::from_config(&c))
            .unwrap_or_default();
        Self(inner)
    }
}
