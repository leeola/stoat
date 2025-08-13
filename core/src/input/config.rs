use super::{
    action::{Action, Mode},
    key::Key,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Complete modal configuration loaded from RON
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModalConfig {
    /// Map of modes to their definitions
    pub modes: HashMap<Mode, ModeDefinition>,

    /// Which mode to start in
    pub initial_mode: Mode,

    /// Global key bindings that work in all modes
    #[serde(default)]
    pub global_bindings: HashMap<Key, Action>,
}

/// Definition of a single mode
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModeDefinition {
    /// Key bindings for this mode
    pub bindings: HashMap<Key, Action>,

    /// Default action for unmapped keys (e.g., InsertChar in insert mode)
    #[serde(default)]
    pub default_action: Option<Action>,
}

impl Default for ModalConfig {
    fn default() -> Self {
        // Use the full default keymap
        super::keymap::default_keymap()
    }
}

impl ModalConfig {
    /// Load configuration from a RON string
    pub fn from_ron(ron_str: &str) -> Result<Self, ron::error::SpannedError> {
        ron::from_str(ron_str)
    }

    /// Load configuration from a file
    pub fn from_file(path: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        Ok(Self::from_ron(&contents)?)
    }

    /// Save configuration to a RON string
    pub fn to_ron(&self) -> Result<String, ron::Error> {
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{
        action::{Direction, Mode},
        key::{Key, NamedKey},
    };

    #[test]
    fn test_default_config() {
        let config = ModalConfig::default();
        assert_eq!(config.initial_mode, Mode::Normal);

        // Verify all modes exist
        assert!(config.modes.contains_key(&Mode::Normal));
        assert!(config.modes.contains_key(&Mode::Insert));
        assert!(config.modes.contains_key(&Mode::Visual));
        assert!(config.modes.contains_key(&Mode::Command));
        assert!(config.modes.contains_key(&Mode::Canvas));

        // Verify some key bindings in Normal mode
        let normal_mode = &config.modes[&Mode::Normal];
        assert_eq!(
            normal_mode.bindings.get(&Key::Named(NamedKey::Esc)),
            Some(&Action::ExitApp)
        );
        assert_eq!(
            normal_mode.bindings.get(&Key::Char('i')),
            Some(&Action::ChangeMode(Mode::Insert))
        );
        assert_eq!(
            normal_mode.bindings.get(&Key::Char('c')),
            Some(&Action::ChangeMode(Mode::Canvas))
        );

        // Verify Canvas mode has GatherNodes action
        let canvas_mode = &config.modes[&Mode::Canvas];
        assert_eq!(
            canvas_mode.bindings.get(&Key::Char('a')),
            Some(&Action::GatherNodes)
        );
    }

    #[test]
    fn test_config_serialization() {
        let config = ModalConfig::default();
        let ron_str = config.to_ron().expect("Failed to serialize config to RON");

        // Should be able to parse it back
        let parsed = ModalConfig::from_ron(&ron_str).expect("Failed to parse config from RON");
        assert_eq!(parsed.initial_mode, config.initial_mode);
    }

    #[test]
    fn test_parse_example_config() {
        let ron_str = r#"(
            modes: {
                Normal: (
                    bindings: {
                        Named(Esc): ExitApp,
                        Char('h'): Move(Left),
                        Char('j'): Move(Down),
                        Char('k'): Move(Up),
                        Char('l'): Move(Right),
                    }
                ),
            },
            initial_mode: Normal,
        )"#;

        let config = ModalConfig::from_ron(ron_str).expect("Failed to parse example config");
        assert_eq!(config.initial_mode, Mode::Normal);

        let normal_mode = &config.modes[&Mode::Normal];
        assert_eq!(
            normal_mode.bindings.get(&Key::Char('j')),
            Some(&Action::Move(Direction::Down))
        );
    }
}
