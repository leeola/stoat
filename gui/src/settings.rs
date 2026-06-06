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

/// Session-only override for the editor buffer font size, layered over
/// the configured `editor.font.size`. Absent until the user adjusts the
/// font (IncreaseFontSize / DecreaseFontSize); when present, editors
/// use this value instead of the configured base. Not persisted to
/// stcfg. Observed via [`gpui::App::observe_global::<EditorFontSize>`]
/// so a change relays out every editor.
pub struct EditorFontSize(pub f32);

impl Global for EditorFontSize {}

impl Settings {
    /// Build [`Settings`] from an already-parsed
    /// [`stoat_config::Config`]. Stores the config and the
    /// resolved settings derived from it via
    /// [`stoat_config::Settings::from_config`].
    pub fn from_config(config: stoat_config::Config) -> Self {
        let resolved = stoat_config::Settings::from_config(&config);
        Self { config, resolved }
    }

    /// Parse stcfg source text into [`Settings`]. Parse errors are
    /// discarded -- the loader silently falls back to
    /// [`Settings::default`] when parsing fails, matching the
    /// TUI's behavior.
    pub fn load_from_source(source: &str) -> Self {
        let (config, _errors) = stoat_config::parse(source);
        match config {
            Some(c) => Self::from_config(c),
            None => Self::default(),
        }
    }

    /// Overlay a user stcfg source onto these settings, with the user's
    /// values winning. Parses `user_source` and merges its resolved
    /// settings over `self.resolved` (right-hand wins per field); the
    /// parsed config AST stays `self`'s, so this round-trips resolved
    /// keys like `theme` while leaving keymap and mode blocks to the
    /// default. A `user_source` that fails to parse is logged and
    /// ignored, leaving `self` unchanged.
    pub fn layer_user_source(self, user_source: &str) -> Self {
        let (user_config, errors) = stoat_config::parse(user_source);
        if !errors.is_empty() {
            tracing::warn!(
                target: "stoat_gui::settings",
                "user config parse errors: {}",
                stoat_config::format_errors(user_source, &errors)
            );
        }
        let Some(user_config) = user_config else {
            return self;
        };
        let user_resolved = stoat_config::Settings::from_config(&user_config);
        Self {
            resolved: self.resolved.merge(user_resolved),
            config: self.config,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_user_source_lets_user_theme_win() {
        let default = Settings::load_from_source("on init { theme = default_dark; }");
        let merged = default.layer_user_source("on init { theme = solarized_light; }");
        assert_eq!(merged.resolved.theme.as_deref(), Some("solarized_light"));
    }

    #[test]
    fn layer_user_source_keeps_default_theme_when_user_omits_it() {
        let default = Settings::load_from_source("on init { theme = default_dark; }");
        let merged = default.layer_user_source("on init { text_proto_log = true; }");
        assert_eq!(merged.resolved.theme.as_deref(), Some("default_dark"));
        assert_eq!(merged.resolved.text_proto_log, Some(true));
    }

    #[test]
    fn layer_user_source_ignores_unparseable_user_config() {
        let default = Settings::load_from_source("on init { theme = default_dark; }");
        let merged = default.layer_user_source("@@@ not valid @@@");
        assert_eq!(merged.resolved.theme.as_deref(), Some("default_dark"));
    }
}
