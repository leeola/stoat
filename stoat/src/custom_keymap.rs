//! Custom keymap implementation that supports dynamic modes from configuration.

use crate::{
    actions::EditMode,
    command::Command,
    config::{KeyBinding as ConfigKeyBinding, KeymapConfig, ModeConfig},
    input::{Key, Modifiers},
};
use std::collections::HashMap;

/// Custom keymap that supports dynamic modes and advanced features.
///
/// This keymap implementation supports:
/// - Dynamic mode definitions from configuration
/// - Mode inheritance
/// - Fallback handlers for unmapped keys
/// - Return-to mode behavior
/// - Command sequences
#[derive(Debug, Clone)]
pub struct CustomKeymap {
    /// Mode configurations including bindings and settings
    modes: HashMap<String, ProcessedMode>,
    /// Initial mode to start in
    initial_mode: String,
    /// Stack of modes for return_to behavior
    mode_stack: Vec<String>,
}

/// Processed mode with resolved inheritance and compiled bindings.
#[derive(Debug, Clone)]
struct ProcessedMode {
    /// Display name for status line
    display_name: Option<String>,
    /// Resolved key bindings (including inherited)
    bindings: HashMap<KeyBinding, ProcessedBinding>,
    /// Fallback behavior for unmapped keys
    fallback: Option<ProcessedBinding>,
    /// Mode to return to after executing a command
    return_to: Option<String>,
}

/// A processed key binding ready for execution.
#[derive(Debug, Clone)]
enum ProcessedBinding {
    /// Single command
    Command(Command),
    /// Switch to another mode
    Mode(String),
    /// Execute command then optionally switch mode
    CommandAndMode {
        command: Command,
        mode: Option<String>,
    },
    /// Execute multiple bindings in sequence
    Sequence(Vec<ProcessedBinding>),
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

impl CustomKeymap {
    /// Creates a new custom keymap from configuration.
    pub fn from_config(config: KeymapConfig) -> Self {
        let mut modes = HashMap::new();

        // Process each mode configuration
        for (mode_name, mode_config) in &config.modes {
            let processed = Self::process_mode(mode_name, mode_config, &config);
            modes.insert(mode_name.clone(), processed);
        }

        // Add default modes if not present
        if !modes.contains_key("normal") {
            modes.insert("normal".to_string(), Self::default_normal_mode());
        }
        if !modes.contains_key("insert") {
            modes.insert("insert".to_string(), Self::default_insert_mode());
        }
        if !modes.contains_key("command") {
            modes.insert("command".to_string(), Self::default_command_mode());
        }

        Self {
            modes,
            initial_mode: config.initial_mode,
            mode_stack: Vec::new(),
        }
    }

    /// Creates a default keymap with vim-like bindings.
    pub fn default() -> Self {
        let mut modes = HashMap::new();
        modes.insert("normal".to_string(), Self::default_normal_mode());
        modes.insert("insert".to_string(), Self::default_insert_mode());
        modes.insert("command".to_string(), Self::default_command_mode());

        Self {
            modes,
            initial_mode: "normal".to_string(),
            mode_stack: Vec::new(),
        }
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

        // First check explicit bindings
        if let Some(processed) = processed_mode.bindings.get(&binding) {
            return Some(self.process_binding(processed.clone(), mode_name));
        }

        // Then check fallback
        if let Some(fallback) = &processed_mode.fallback {
            return Some(self.process_binding(fallback.clone(), mode_name));
        }

        None
    }

    /// Process a binding and return the result.
    fn process_binding(&self, binding: ProcessedBinding, current_mode: &str) -> KeymapResult {
        match binding {
            ProcessedBinding::Command(cmd) => {
                // Check if this mode has return_to behavior
                if let Some(mode) = self.modes.get(current_mode) {
                    if let Some(return_to) = &mode.return_to {
                        KeymapResult::CommandAndMode {
                            command: cmd,
                            new_mode: Some(EditMode::from_name(return_to)),
                        }
                    } else {
                        KeymapResult::Command(cmd)
                    }
                } else {
                    KeymapResult::Command(cmd)
                }
            },
            ProcessedBinding::Mode(mode_name) => {
                KeymapResult::ModeChange(EditMode::from_name(&mode_name))
            },
            ProcessedBinding::CommandAndMode { command, mode } => KeymapResult::CommandAndMode {
                command,
                new_mode: mode.map(|m| EditMode::from_name(&m)),
            },
            ProcessedBinding::Sequence(bindings) => {
                let mut commands = Vec::new();
                let mut final_mode = None;

                for binding in bindings {
                    match self.process_binding(binding, current_mode) {
                        KeymapResult::Command(cmd) => commands.push(cmd),
                        KeymapResult::ModeChange(mode) => final_mode = Some(mode),
                        KeymapResult::CommandAndMode { command, new_mode } => {
                            commands.push(command);
                            if new_mode.is_some() {
                                final_mode = new_mode;
                            }
                        },
                        KeymapResult::Sequence(mut cmds, mode) => {
                            commands.append(&mut cmds);
                            if mode.is_some() {
                                final_mode = mode;
                            }
                        },
                    }
                }

                KeymapResult::Sequence(commands, final_mode)
            },
        }
    }

    /// Process a mode configuration, resolving inheritance.
    fn process_mode(
        _name: &str,
        config: &ModeConfig,
        keymap_config: &KeymapConfig,
    ) -> ProcessedMode {
        let mut bindings = HashMap::new();

        // Handle inheritance
        if let Some(parent_name) = &config.inherit {
            if let Some(parent_config) = keymap_config.modes.get(parent_name) {
                // Recursively process parent mode
                let parent = Self::process_mode(parent_name, parent_config, keymap_config);
                bindings = parent.bindings;
            }
        }

        // Process this mode's bindings (overrides inherited)
        for (key_str, binding) in &config.keys {
            let key_binding = Self::parse_key_binding(key_str);
            if let Some(processed) = Self::process_config_binding(binding) {
                bindings.insert(key_binding, processed);
            }
            // Skip bindings with unknown commands
        }

        // Process fallback
        let fallback = config
            .fallback
            .as_ref()
            .and_then(|b| Self::process_config_binding(b));

        ProcessedMode {
            display_name: config.display_name.clone(),
            bindings,
            fallback,
            return_to: config.return_to.clone(),
        }
    }

    /// Process a configuration binding into a processed binding.
    fn process_config_binding(binding: &ConfigKeyBinding) -> Option<ProcessedBinding> {
        match binding {
            ConfigKeyBinding::Command(cmd_str) => {
                if let Some(cmd) = Self::parse_command(cmd_str) {
                    Some(ProcessedBinding::Command(cmd))
                } else {
                    // Unknown command, skip this binding
                    None
                }
            },
            ConfigKeyBinding::Mode { mode } => Some(ProcessedBinding::Mode(mode.clone())),
            ConfigKeyBinding::CommandAndMode { command, mode } => {
                if let Some(cmd) = Self::parse_command(command) {
                    Some(ProcessedBinding::CommandAndMode {
                        command: cmd,
                        mode: mode.clone(),
                    })
                } else {
                    // Unknown command, skip this binding
                    None
                }
            },
            ConfigKeyBinding::Sequence(seq) => {
                let processed: Result<Vec<_>, _> = seq
                    .iter()
                    .map(|b| Self::process_config_binding(b).ok_or(()))
                    .collect();
                processed.ok().map(ProcessedBinding::Sequence)
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
            // Unknown commands are not mapped
            _ => return None,
        })
    }

    /// Creates default normal mode configuration.
    fn default_normal_mode() -> ProcessedMode {
        let mut bindings = HashMap::new();

        // Movement keys
        bindings.insert(
            KeyBinding::new("h".to_string(), Modifiers::default()),
            ProcessedBinding::Command(Command::MoveCursorLeft),
        );
        bindings.insert(
            KeyBinding::new("j".to_string(), Modifiers::default()),
            ProcessedBinding::Command(Command::MoveCursorDown),
        );
        bindings.insert(
            KeyBinding::new("k".to_string(), Modifiers::default()),
            ProcessedBinding::Command(Command::MoveCursorUp),
        );
        bindings.insert(
            KeyBinding::new("l".to_string(), Modifiers::default()),
            ProcessedBinding::Command(Command::MoveCursorRight),
        );

        // Mode changes
        bindings.insert(
            KeyBinding::new("i".to_string(), Modifiers::default()),
            ProcessedBinding::Mode("insert".to_string()),
        );
        bindings.insert(
            KeyBinding::new(":".to_string(), Modifiers::default()),
            ProcessedBinding::Mode("command".to_string()),
        );

        // Other commands
        bindings.insert(
            KeyBinding::new("?".to_string(), Modifiers::default()),
            ProcessedBinding::Command(Command::ToggleCommandInfo),
        );
        bindings.insert(
            KeyBinding::new("escape".to_string(), Modifiers::default()),
            ProcessedBinding::Command(Command::Exit),
        );

        ProcessedMode {
            display_name: Some("NORMAL".to_string()),
            bindings,
            fallback: None,
            return_to: None,
        }
    }

    /// Creates default insert mode configuration.
    fn default_insert_mode() -> ProcessedMode {
        let mut bindings = HashMap::new();

        bindings.insert(
            KeyBinding::new("escape".to_string(), Modifiers::default()),
            ProcessedBinding::Mode("normal".to_string()),
        );
        bindings.insert(
            KeyBinding::new("enter".to_string(), Modifiers::default()),
            ProcessedBinding::Command(Command::InsertNewline),
        );
        bindings.insert(
            KeyBinding::new("backspace".to_string(), Modifiers::default()),
            ProcessedBinding::Command(Command::DeleteChar),
        );

        ProcessedMode {
            display_name: Some("INSERT".to_string()),
            bindings,
            fallback: Some(ProcessedBinding::Command(Command::InsertChar)),
            return_to: None,
        }
    }

    /// Creates default command mode configuration.
    fn default_command_mode() -> ProcessedMode {
        let mut bindings = HashMap::new();

        bindings.insert(
            KeyBinding::new("escape".to_string(), Modifiers::default()),
            ProcessedBinding::Mode("normal".to_string()),
        );

        ProcessedMode {
            display_name: Some("COMMAND".to_string()),
            bindings,
            fallback: None,
            return_to: None,
        }
    }

    /// Gets the display name for a mode.
    pub fn get_mode_display_name(&self, mode: &EditMode) -> String {
        self.modes
            .get(mode.name())
            .and_then(|m| m.display_name.clone())
            .unwrap_or_else(|| mode.name().to_uppercase())
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
                    .filter_map(|(binding, processed)| match processed {
                        ProcessedBinding::Command(cmd) => Some((
                            format_key_binding(&binding.key, &binding.modifiers),
                            cmd.clone(),
                        )),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Result of a keymap lookup.
#[derive(Debug, Clone)]
pub enum KeymapResult {
    /// Execute a single command
    Command(Command),
    /// Change to a new mode
    ModeChange(EditMode),
    /// Execute a command then optionally change mode
    CommandAndMode {
        command: Command,
        new_mode: Option<EditMode>,
    },
    /// Execute a sequence of commands then optionally change mode
    Sequence(Vec<Command>, Option<EditMode>),
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
        let keymap = CustomKeymap::default();

        // Test normal mode binding
        let result = keymap.lookup(&"h".to_string(), &Modifiers::default(), &EditMode::Normal);
        assert!(matches!(
            result,
            Some(KeymapResult::Command(Command::MoveCursorLeft))
        ));

        // Test insert mode fallback
        let result = keymap.lookup(&"x".to_string(), &Modifiers::default(), &EditMode::Insert);
        assert!(matches!(
            result,
            Some(KeymapResult::Command(Command::InsertChar))
        ));
    }

    #[test]
    fn test_custom_mode() {
        let mut config = KeymapConfig::default();

        let mut delete_mode = ModeConfig::default();
        delete_mode.display_name = Some("DELETE".to_string());
        delete_mode.return_to = Some("normal".to_string());
        delete_mode.keys.insert(
            "d".to_string(),
            ConfigKeyBinding::Command("delete_line".to_string()),
        );
        delete_mode.keys.insert(
            "w".to_string(),
            ConfigKeyBinding::Command("delete_word".to_string()),
        );

        config.modes.insert("delete".to_string(), delete_mode);

        let keymap = CustomKeymap::from_config(config);

        // Test custom mode binding with return_to
        let result = keymap.lookup(
            &"d".to_string(),
            &Modifiers::default(),
            &EditMode::custom("delete"),
        );
        assert!(matches!(
            result,
            Some(KeymapResult::CommandAndMode {
                command: Command::DeleteLine,
                new_mode: Some(EditMode::Normal),
            })
        ));
    }

    #[test]
    fn test_mode_inheritance() {
        let mut config = KeymapConfig::default();

        // Base mode
        let mut base_mode = ModeConfig::default();
        base_mode.keys.insert(
            "x".to_string(),
            ConfigKeyBinding::Command("delete_char".to_string()),
        );
        config.modes.insert("base".to_string(), base_mode);

        // Derived mode
        let mut derived_mode = ModeConfig::default();
        derived_mode.inherit = Some("base".to_string());
        derived_mode.keys.insert(
            "y".to_string(),
            ConfigKeyBinding::Command("delete_word".to_string()),
        );
        config.modes.insert("derived".to_string(), derived_mode);

        let keymap = CustomKeymap::from_config(config);

        // Test inherited binding
        let result = keymap.lookup(
            &"x".to_string(),
            &Modifiers::default(),
            &EditMode::custom("derived"),
        );
        assert!(matches!(
            result,
            Some(KeymapResult::Command(Command::DeleteChar))
        ));

        // Test own binding
        let result = keymap.lookup(
            &"y".to_string(),
            &Modifiers::default(),
            &EditMode::custom("derived"),
        );
        assert!(matches!(
            result,
            Some(KeymapResult::Command(Command::DeleteWord))
        ));
    }

    #[test]
    fn test_command_sequence() {
        let mut config = KeymapConfig::default();

        let mut normal_mode = ModeConfig::default();
        normal_mode.keys.insert(
            "ZZ".to_string(),
            ConfigKeyBinding::Sequence(vec![
                ConfigKeyBinding::Command("toggle_command_info".to_string()),
                ConfigKeyBinding::Command("exit".to_string()),
            ]),
        );
        config.modes.insert("normal".to_string(), normal_mode);

        let keymap = CustomKeymap::from_config(config);

        let result = keymap.lookup(&"ZZ".to_string(), &Modifiers::default(), &EditMode::Normal);
        assert!(matches!(
            result,
            Some(KeymapResult::Sequence(cmds, None)) if cmds.len() == 2
        ));
    }

    #[test]
    fn test_fallback_handler() {
        let mut config = KeymapConfig::default();

        // Create a mode with fallback to insert_char
        let mut type_mode = ModeConfig::default();
        type_mode.fallback = Some(ConfigKeyBinding::Command("insert_char".to_string()));
        type_mode.keys.insert(
            "Escape".to_string(),
            ConfigKeyBinding::Mode {
                mode: "normal".to_string(),
            },
        );
        config.modes.insert("type".to_string(), type_mode);

        let keymap = CustomKeymap::from_config(config);

        // Test that unmapped keys trigger fallback
        let result = keymap.lookup(
            &"a".to_string(),
            &Modifiers::default(),
            &EditMode::custom("type"),
        );
        assert!(matches!(
            result,
            Some(KeymapResult::Command(Command::InsertChar))
        ));

        // Test that mapped keys work normally
        let result = keymap.lookup(
            &"Escape".to_string(),
            &Modifiers::default(),
            &EditMode::custom("type"),
        );
        assert!(matches!(
            result,
            Some(KeymapResult::ModeChange(EditMode::Normal))
        ));
    }

    #[test]
    fn test_command_and_mode_binding() {
        let mut config = KeymapConfig::default();

        let mut normal_mode = ModeConfig::default();
        normal_mode.keys.insert(
            "c".to_string(),
            ConfigKeyBinding::CommandAndMode {
                command: "delete_char".to_string(),
                mode: Some("insert".to_string()),
            },
        );
        config.modes.insert("normal".to_string(), normal_mode);

        let keymap = CustomKeymap::from_config(config);

        let result = keymap.lookup(&"c".to_string(), &Modifiers::default(), &EditMode::Normal);
        assert!(matches!(
            result,
            Some(KeymapResult::CommandAndMode {
                command: Command::DeleteChar,
                new_mode: Some(EditMode::Insert),
            })
        ));
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
            fallback = "insert_char"
            
            [modes.insert.keys]
            Escape = { mode = "normal" }
        "#;

        let config: KeymapConfig = toml::from_str(toml).unwrap();
        let keymap = CustomKeymap::from_config(config);

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
            Some(KeymapResult::ModeChange(EditMode::Insert))
        ));

        // Test insert mode fallback
        let result = keymap.lookup(&"x".to_string(), &Modifiers::default(), &EditMode::Insert);
        assert!(matches!(
            result,
            Some(KeymapResult::Command(Command::InsertChar))
        ));
    }
}
