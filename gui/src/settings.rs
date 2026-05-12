use gpui::Global;

/// App-global wrapper around the parsed `stcfg` configuration.
/// Stored via [`gpui::App::set_global`] and observed via
/// [`gpui::App::observe_global::<Settings>`].
///
/// `config` is the raw parsed [`stoat_config::Config`], used by the
/// keymap loader to recompile the [`stoat::keymap::Keymap`] on
/// settings changes. `resolved` is the derived settings struct
/// produced by `stoat_config::Settings::from_config`, used by
/// callers that read individual setting values.
pub struct Settings {
    pub config: stoat_config::Config,
    pub resolved: stoat_config::Settings,
}

impl Global for Settings {}

impl Settings {
    /// Parse stcfg source text into [`Settings`]. Parse errors are
    /// discarded -- the loader silently falls back to an empty
    /// config when parsing fails, matching the TUI's behavior.
    pub fn load_from_source(source: &str) -> Self {
        let (config, _errors) = stoat_config::parse(source);
        let config = config.unwrap_or_else(|| stoat_config::Config {
            blocks: Vec::new(),
            themes: Vec::new(),
        });
        let resolved = stoat_config::Settings::from_config(&config);
        Self { config, resolved }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            config: stoat_config::Config {
                blocks: Vec::new(),
                themes: Vec::new(),
            },
            resolved: stoat_config::Settings::default(),
        }
    }
}
