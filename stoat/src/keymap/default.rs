//! Default keymap configuration providing vim-like bindings.

use crate::config::keymap::{KeyBinding, KeymapConfig, ModeConfig};
use std::collections::HashMap;

/// Creates the default keymap configuration with vim-like bindings.
pub fn default_config() -> KeymapConfig {
    let mut modes = HashMap::new();

    // Normal mode
    modes.insert("normal".to_string(), default_normal_mode());

    // Insert mode
    modes.insert("insert".to_string(), default_insert_mode());

    // Command mode
    modes.insert("command".to_string(), default_command_mode());

    KeymapConfig {
        modes,
        initial_mode: "normal".to_string(),
    }
}

/// Creates the default normal mode configuration.
fn default_normal_mode() -> ModeConfig {
    let mut keys = HashMap::new();

    // Movement keys
    keys.insert(
        "h".to_string(),
        KeyBinding::Command("move_left".to_string()),
    );
    keys.insert(
        "j".to_string(),
        KeyBinding::Command("move_down".to_string()),
    );
    keys.insert("k".to_string(), KeyBinding::Command("move_up".to_string()));
    keys.insert(
        "l".to_string(),
        KeyBinding::Command("move_right".to_string()),
    );

    // Paragraph movement (vim-style)
    keys.insert(
        "}".to_string(),
        KeyBinding::Command("next_paragraph".to_string()),
    );
    keys.insert(
        "{".to_string(),
        KeyBinding::Command("previous_paragraph".to_string()),
    );

    // Mode changes
    keys.insert(
        "i".to_string(),
        KeyBinding::Command("enter_insert_mode".to_string()),
    );
    keys.insert(
        ":".to_string(),
        KeyBinding::Command("enter_command_mode".to_string()),
    );

    // Other commands
    keys.insert(
        "?".to_string(),
        KeyBinding::Command("toggle_command_info".to_string()),
    );
    keys.insert(
        "escape".to_string(),
        KeyBinding::Command("exit".to_string()),
    );

    ModeConfig {
        display_name: Some("NORMAL".to_string()),
        keys,
    }
}

/// Creates the default insert mode configuration.
fn default_insert_mode() -> ModeConfig {
    let mut keys = HashMap::new();

    keys.insert(
        "escape".to_string(),
        KeyBinding::Command("enter_normal_mode".to_string()),
    );
    keys.insert(
        "enter".to_string(),
        KeyBinding::Command("insert_newline".to_string()),
    );
    keys.insert(
        "backspace".to_string(),
        KeyBinding::Command("delete_char".to_string()),
    );

    ModeConfig {
        display_name: Some("INSERT".to_string()),
        keys,
    }
}

/// Creates the default command mode configuration.
fn default_command_mode() -> ModeConfig {
    let mut keys = HashMap::new();

    keys.insert(
        "escape".to_string(),
        KeyBinding::Command("enter_normal_mode".to_string()),
    );

    ModeConfig {
        display_name: Some("COMMAND".to_string()),
        keys,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_all_modes() {
        let config = default_config();

        assert!(config.modes.contains_key("normal"));
        assert!(config.modes.contains_key("insert"));
        assert!(config.modes.contains_key("command"));
        assert_eq!(config.initial_mode, "normal");
    }

    #[test]
    fn normal_mode_has_vim_bindings() {
        let config = default_config();
        let normal = &config.modes["normal"];

        // Check movement keys
        assert!(matches!(&normal.keys["h"], KeyBinding::Command(cmd) if cmd == "move_left"));
        assert!(matches!(&normal.keys["j"], KeyBinding::Command(cmd) if cmd == "move_down"));
        assert!(matches!(&normal.keys["k"], KeyBinding::Command(cmd) if cmd == "move_up"));
        assert!(matches!(&normal.keys["l"], KeyBinding::Command(cmd) if cmd == "move_right"));

        // Check mode switches
        assert!(
            matches!(&normal.keys["i"], KeyBinding::Command(cmd) if cmd == "enter_insert_mode")
        );
        assert!(
            matches!(&normal.keys[":"], KeyBinding::Command(cmd) if cmd == "enter_command_mode")
        );
    }

    #[test]
    fn insert_mode_has_escape_binding() {
        let config = default_config();
        let insert = &config.modes["insert"];

        assert!(
            matches!(&insert.keys["escape"], KeyBinding::Command(cmd) if cmd == "enter_normal_mode")
        );
        assert!(
            matches!(&insert.keys["enter"], KeyBinding::Command(cmd) if cmd == "insert_newline")
        );
        assert!(
            matches!(&insert.keys["backspace"], KeyBinding::Command(cmd) if cmd == "delete_char")
        );
    }
}
