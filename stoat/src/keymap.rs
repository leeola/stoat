//! Keymap implementation that supports dynamic modes from configuration.

mod default;

use crate::{
    actions::EditMode,
    command::Command,
    config::keymap::{KeyBinding as ConfigKeyBinding, KeymapConfig, ModeConfig},
    input::{Key, Modifiers},
};
use compact_str::CompactString;
pub use default::default_config;
use std::collections::HashMap;

/// Keymap that supports dynamic modes.
///
/// This keymap implementation supports dynamic mode definitions from configuration
/// with simple key-to-command mappings.
#[derive(Debug, Clone)]
pub struct Keymap {
    /// Mode configurations including bindings and settings
    modes: HashMap<String, ProcessedMode>,
}

/// Processed mode with compiled bindings.
#[derive(Debug, Clone)]
struct ProcessedMode {
    /// Display name for status line
    display_name: Option<String>,
    /// Resolved key bindings
    bindings: HashMap<KeyBinding, Command>,
}

/// Key binding with normalized key and modifiers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct KeyBinding {
    key: Key,
    modifiers: Modifiers,
}

impl KeyBinding {
    fn new(key: Key, modifiers: Modifiers) -> Self {
        // Normalize the key to lowercase for consistent matching
        let key = key.to_lowercase();
        Self { key, modifiers }
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self::from_config(default_config())
    }
}

impl Keymap {
    /// Creates a new custom keymap from configuration.
    pub fn from_config(config: KeymapConfig) -> Self {
        let mut modes = HashMap::new();

        // Process each mode configuration
        for (mode_name, mode_config) in &config.modes {
            let processed = Self::process_mode(mode_config);
            modes.insert(mode_name.clone(), processed);
        }

        Self { modes }
    }

    /// Looks up a binding for the given key and mode.
    pub fn lookup(
        &self,
        key: &Key,
        modifiers: &Modifiers,
        mode: &EditMode,
    ) -> Option<KeymapResult> {
        let mode_name = mode.name();
        let processed_mode = self.modes.get(mode_name)?;
        let binding = KeyBinding::new(key.clone(), *modifiers);

        // Look up the command for this key binding
        processed_mode
            .bindings
            .get(&binding)
            .map(|cmd| KeymapResult::Command(cmd.clone()))
    }

    /// Process a mode configuration.
    fn process_mode(config: &ModeConfig) -> ProcessedMode {
        let mut bindings = HashMap::new();

        // Process this mode's bindings
        for (key_str, binding) in &config.keys {
            let key_binding = Self::parse_key_binding(key_str);
            if let Some(command) = Self::process_config_binding(binding) {
                bindings.insert(key_binding, command);
            }
            // Skip bindings with unknown commands
        }

        ProcessedMode {
            display_name: config.display_name.clone(),
            bindings,
        }
    }

    /// Process a configuration binding into a command.
    fn process_config_binding(binding: &ConfigKeyBinding) -> Option<Command> {
        match binding {
            ConfigKeyBinding::Command(cmd_str) => Self::parse_command(cmd_str),
            ConfigKeyBinding::Mode { mode } => {
                // Convert mode change to a command
                match mode.as_str() {
                    "normal" => Some(Command::EnterNormalMode),
                    "insert" => Some(Command::EnterInsertMode),
                    "command" => Some(Command::EnterCommandMode),
                    custom => Some(Command::EnterMode(CompactString::new(custom))),
                }
            },
        }
    }

    /// Parse a key binding string into a KeyBinding.
    fn parse_key_binding(key_str: &str) -> KeyBinding {
        // Handle modifier syntax (e.g., "C-x", "M-x", "C-M-x")
        let mut modifiers = Modifiers::default();
        let mut remaining = key_str;

        while let Some(dash_pos) = remaining.find('-') {
            if dash_pos > 0 {
                let modifier = &remaining[..dash_pos];
                match modifier {
                    "C" | "Ctrl" => modifiers.control = true,
                    "M" | "Alt" | "Meta" => modifiers.alt = true,
                    "S" | "Shift" => modifiers.shift = true,
                    _ => break, // Not a modifier, treat as part of key
                }
                remaining = &remaining[dash_pos + 1..];
            } else {
                break;
            }
        }

        KeyBinding::new(remaining.to_string(), modifiers)
    }

    /// Parse a command string into a Command.
    fn parse_command(cmd_str: &str) -> Option<Command> {
        // Map command strings to Command enum variants
        Some(match cmd_str {
            "move_left" | "move_cursor_left" => Command::MoveCursorLeft,
            "move_right" | "move_cursor_right" => Command::MoveCursorRight,
            "move_up" | "move_cursor_up" => Command::MoveCursorUp,
            "move_down" | "move_cursor_down" => Command::MoveCursorDown,
            "enter_insert_mode" => Command::EnterInsertMode,
            "enter_normal_mode" => Command::EnterNormalMode,
            "enter_command_mode" => Command::EnterCommandMode,
            "delete_char" => Command::DeleteChar,
            "delete_line" => Command::DeleteLine,
            "delete_word" => Command::DeleteWord,
            "insert_newline" => Command::InsertNewline,
            "exit" => Command::Exit,
            "toggle_command_info" => Command::ToggleCommandInfo,
            "insert_char" => Command::InsertChar,
            "help" => Command::Help,
            "next_paragraph" => Command::NextParagraph,
            "previous_paragraph" => Command::PreviousParagraph,
            // Unknown commands are not mapped
            _ => return None,
        })
    }

    /// Gets the display name for a mode.
    pub fn get_mode_display_name(&self, mode: &EditMode) -> String {
        self.modes
            .get(mode.name())
            .and_then(|m| m.display_name.clone())
            .unwrap_or_else(|| mode.name().to_uppercase())
    }

    /// Returns all available commands for the given mode.
    pub fn available_commands(&self, mode: &EditMode) -> Vec<&Command> {
        let mode_name = mode.name();

        self.modes
            .get(mode_name)
            .map(|processed_mode| processed_mode.bindings.values().collect())
            .unwrap_or_default()
    }

    /// Gets all bindings for a mode as (key_display, command) pairs.
    pub fn get_bindings_for_mode(&self, mode: &EditMode) -> Vec<(String, Command)> {
        let mode_name = mode.name();

        self.modes
            .get(mode_name)
            .map(|processed_mode| {
                processed_mode
                    .bindings
                    .iter()
                    .map(|(binding, cmd)| {
                        (
                            format_key_binding(&binding.key, &binding.modifiers),
                            cmd.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Result of a keymap lookup - simplified to just commands.
#[derive(Debug, Clone)]
pub enum KeymapResult {
    /// Execute a command
    Command(Command),
}

/// Format a key binding for display.
fn format_key_binding(key: &Key, modifiers: &Modifiers) -> String {
    let mut result = String::new();

    if modifiers.control {
        result.push_str("C-");
    }
    if modifiers.alt {
        result.push_str("M-");
    }
    if modifiers.shift {
        result.push_str("S-");
    }

    match key.as_str() {
        "escape" | "esc" => result.push_str("Esc"),
        "enter" | "return" => result.push_str("Enter"),
        "backspace" => result.push_str("Backsp"),
        "tab" => result.push_str("Tab"),
        "space" => result.push_str("Space"),
        "left" => result.push_str("Left"),
        "right" => result.push_str("Right"),
        "up" => result.push_str("Up"),
        "down" => result.push_str("Down"),
        s if s.len() == 1 => result.push_str(s),
        s => {
            let mut chars = s.chars();
            if let Some(first) = chars.next() {
                result.push(first.to_ascii_uppercase());
                result.extend(chars);
            }
        },
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{KeyBinding as ConfigKeyBinding, KeymapConfig, ModeConfig};

    #[test]
    fn test_default_keymap() {
        let keymap = Keymap::default();

        // Test normal mode binding
        let result = keymap.lookup(&"h".to_string(), &Modifiers::default(), &EditMode::Normal);
        assert!(matches!(
            result,
            Some(KeymapResult::Command(Command::MoveCursorLeft))
        ));

        // Test insert mode has no default character insertion
        let result = keymap.lookup(&"x".to_string(), &Modifiers::default(), &EditMode::Insert);
        assert!(result.is_none());
    }

    #[test]
    fn test_custom_mode() {
        let mut config = KeymapConfig::default();

        let mut delete_mode = ModeConfig::default();
        delete_mode.display_name = Some("DELETE".to_string());
        delete_mode.keys.insert(
            "d".to_string(),
            ConfigKeyBinding::Command("delete_line".to_string()),
        );
        delete_mode.keys.insert(
            "w".to_string(),
            ConfigKeyBinding::Command("delete_word".to_string()),
        );

        config.modes.insert("delete".to_string(), delete_mode);

        let keymap = Keymap::from_config(config);

        // Test custom mode binding
        let result = keymap.lookup(
            &"d".to_string(),
            &Modifiers::default(),
            &EditMode::custom("delete"),
        );
        assert!(matches!(
            result,
            Some(KeymapResult::Command(Command::DeleteLine))
        ));
    }

    #[test]
    fn test_mode_change_as_command() {
        let mut config = KeymapConfig::default();

        let mut normal_mode = ModeConfig::default();
        normal_mode.keys.insert(
            "i".to_string(),
            ConfigKeyBinding::Mode {
                mode: "insert".to_string(),
            },
        );
        normal_mode.keys.insert(
            "d".to_string(),
            ConfigKeyBinding::Mode {
                mode: "delete".to_string(),
            },
        );

        config.modes.insert("normal".to_string(), normal_mode);

        let keymap = Keymap::from_config(config);

        // Test mode change to built-in mode
        let result = keymap.lookup(&"i".to_string(), &Modifiers::default(), &EditMode::Normal);
        assert!(matches!(
            result,
            Some(KeymapResult::Command(Command::EnterInsertMode))
        ));

        // Test mode change to custom mode
        let result = keymap.lookup(&"d".to_string(), &Modifiers::default(), &EditMode::Normal);
        match result {
            Some(KeymapResult::Command(Command::EnterMode(mode))) => {
                assert_eq!(mode.as_str(), "delete");
            },
            _ => panic!("Expected EnterMode command"),
        }
    }

    #[test]
    fn test_keymap_from_toml() {
        let toml = r#"
            initial_mode = "normal"
            
            [modes.normal]
            display_name = "NORMAL"
            
            [modes.normal.keys]
            h = "move_left"
            j = "move_down"
            k = "move_up"
            l = "move_right"
            i = { mode = "insert" }
            
            [modes.insert]
            display_name = "INSERT"
            
            [modes.insert.keys]
            Escape = { mode = "normal" }
        "#;

        let config: KeymapConfig = toml::from_str(toml).unwrap();
        let keymap = Keymap::from_config(config);

        // Test normal mode movement
        let result = keymap.lookup(&"h".to_string(), &Modifiers::default(), &EditMode::Normal);
        assert!(matches!(
            result,
            Some(KeymapResult::Command(Command::MoveCursorLeft))
        ));

        // Test mode switching
        let result = keymap.lookup(&"i".to_string(), &Modifiers::default(), &EditMode::Normal);
        assert!(matches!(
            result,
            Some(KeymapResult::Command(Command::EnterInsertMode))
        ));
    }

    // Integration tests using Stoat have been moved to tests/keymap.rs
}
