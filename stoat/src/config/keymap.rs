//! Keymap configuration structures for serialization/deserialization.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Root keymap configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeymapConfig {
    /// Mode definitions
    pub modes: HashMap<String, ModeConfig>,

    /// Which mode to start in
    #[serde(default = "default_initial_mode")]
    pub initial_mode: String,
}

/// Configuration for a single mode
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModeConfig {
    /// Display name for status line
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,

    /// Key bindings for this mode
    #[serde(default)]
    pub keys: HashMap<String, KeyBinding>,
}

/// A key binding - what happens when a key is pressed
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KeyBinding {
    /// Simple command string
    Command(String),

    /// Enter another mode (simple form)
    Mode { mode: String },
}

fn default_initial_mode() -> String {
    "normal".to_string()
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            modes: HashMap::new(),
            initial_mode: default_initial_mode(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_simple_config() {
        let toml = r#"
            initial_mode = "normal"
            
            [modes.normal]
            display_name = "NORMAL"
            
            [modes.normal.keys]
            h = "move_left"
            j = "move_down"
            i = { mode = "insert" }
        "#;

        let config: KeymapConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.initial_mode, "normal");
        assert!(config.modes.contains_key("normal"));

        let normal = &config.modes["normal"];
        assert_eq!(normal.display_name, Some("NORMAL".to_string()));
        assert_eq!(normal.keys.len(), 3);

        match &normal.keys["h"] {
            KeyBinding::Command(cmd) => assert_eq!(cmd, "move_left"),
            _ => panic!("Expected command binding"),
        }

        match &normal.keys["i"] {
            KeyBinding::Mode { mode } => assert_eq!(mode, "insert"),
            _ => panic!("Expected mode binding"),
        }
    }

    #[test]
    fn deserialize_delete_mode() {
        let toml = r#"
            [modes.delete]
            display_name = "DELETE"
            
            [modes.delete.keys]
            d = "delete_line"
            w = "delete_word"
        "#;

        let config: KeymapConfig = toml::from_str(toml).unwrap();
        let delete = &config.modes["delete"];

        assert_eq!(delete.display_name, Some("DELETE".to_string()));
        assert_eq!(delete.keys.len(), 2);
    }

    #[test]
    fn round_trip_serialization() {
        let mut config = KeymapConfig::default();

        let mut normal = ModeConfig::default();
        normal.display_name = Some("NORMAL".to_string());
        normal.keys.insert(
            "h".to_string(),
            KeyBinding::Command("move_left".to_string()),
        );
        normal.keys.insert(
            "i".to_string(),
            KeyBinding::Mode {
                mode: "insert".to_string(),
            },
        );

        config.modes.insert("normal".to_string(), normal);

        // Serialize to JSON
        let json = serde_json::to_string(&config).unwrap();

        // Deserialize back
        let config2: KeymapConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.initial_mode, config2.initial_mode);
        assert_eq!(config.modes.len(), config2.modes.len());
        assert_eq!(
            config.modes["normal"].display_name,
            config2.modes["normal"].display_name
        );
    }
}
