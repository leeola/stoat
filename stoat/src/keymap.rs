//! Keymap system for mapping keys to commands based on editor mode.
//!
//! The keymap provides mode-dependent key bindings, allowing the same key
//! to perform different operations depending on the current editor mode.

use crate::{
    actions::EditMode,
    command::Command,
    input::{Key, Modifiers, keys},
};
use std::collections::HashMap;

/// Mode-dependent keymap for converting key presses to commands.
///
/// Each editing mode has its own set of key bindings. When a key is pressed,
/// the keymap looks up the appropriate command based on the current mode.
#[derive(Debug, Clone)]
pub struct Keymap {
    normal: HashMap<KeyBinding, Command>,
    insert: HashMap<KeyBinding, Command>,
    command: HashMap<KeyBinding, Command>,
}

impl Keymap {
    /// Creates a new keymap with default vim-like bindings.
    pub fn new() -> Self {
        let mut keymap = Self {
            normal: HashMap::new(),
            insert: HashMap::new(),
            command: HashMap::new(),
        };

        keymap.setup_default_bindings();
        keymap
    }

    /// Looks up a command for the given key and mode.
    pub fn lookup(&self, key: &Key, modifiers: &Modifiers, mode: EditMode) -> Option<Command> {
        let binding = KeyBinding::new(key.clone(), *modifiers);

        match mode {
            EditMode::Normal => self.normal.get(&binding),
            EditMode::Insert => self.insert.get(&binding),
            EditMode::Command => self.command.get(&binding),
        }
        .cloned()
    }

    /// Sets up the default vim-like key bindings.
    fn setup_default_bindings(&mut self) {
        // Normal mode bindings
        self.bind_normal("h", Command::MoveCursorLeft);
        self.bind_normal("j", Command::MoveCursorDown);
        self.bind_normal("k", Command::MoveCursorUp);
        self.bind_normal("l", Command::MoveCursorRight);
        self.bind_normal("i", Command::EnterInsertMode);
        self.bind_normal(":", Command::EnterCommandMode);
        self.bind_normal("?", Command::ToggleCommandInfo);

        // Escape in normal mode exits
        self.bind_normal_key(keys::ESCAPE.to_string(), Command::Exit);

        // Insert mode bindings
        self.bind_insert_key(keys::ESCAPE.to_string(), Command::EnterNormalMode);
        self.bind_insert_key(keys::ENTER.to_string(), Command::InsertNewline);
        self.bind_insert_key(keys::BACKSPACE.to_string(), Command::DeleteChar);

        // Command mode bindings
        self.bind_command_key(keys::ESCAPE.to_string(), Command::EnterNormalMode);
    }

    /// Convenience method for binding character keys in normal mode.
    fn bind_normal(&mut self, ch: &str, command: Command) {
        self.bind_normal_key(ch.to_string(), command);
    }

    /// Bind a key to a command in normal mode.
    fn bind_normal_key(&mut self, key: Key, command: Command) {
        let binding = KeyBinding::new(key, Modifiers::default());
        self.normal.insert(binding, command);
    }

    /// Bind a key to a command in insert mode.
    fn bind_insert_key(&mut self, key: Key, command: Command) {
        let binding = KeyBinding::new(key, Modifiers::default());
        self.insert.insert(binding, command);
    }

    /// Bind a key to a command in command mode.
    fn bind_command_key(&mut self, key: Key, command: Command) {
        let binding = KeyBinding::new(key, Modifiers::default());
        self.command.insert(binding, command);
    }

    /// Returns all available commands for the given mode.
    pub fn available_commands(&self, mode: EditMode) -> Vec<&Command> {
        match mode {
            EditMode::Normal => self.normal.values().collect(),
            EditMode::Insert => self.insert.values().collect(),
            EditMode::Command => self.command.values().collect(),
        }
    }

    /// Returns key bindings for the given mode as (key_display, command) pairs.
    pub fn get_bindings_for_mode(&self, mode: EditMode) -> Vec<(String, Command)> {
        let map = match mode {
            EditMode::Normal => &self.normal,
            EditMode::Insert => &self.insert,
            EditMode::Command => &self.command,
        };

        map.iter()
            .map(|(binding, command)| (format_key_binding(&binding.key), command.clone()))
            .collect()
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self::new()
    }
}

/// A key binding consisting of a key and modifiers.
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

/// Formats a key for display in UI
fn format_key_binding(key: &Key) -> String {
    match key.as_str() {
        "escape" | "esc" => "Esc".to_string(),
        "enter" | "return" => "Enter".to_string(),
        "backspace" => "Backsp".to_string(),
        "tab" => "Tab".to_string(),
        "space" => "Space".to_string(),
        "left" => "Left".to_string(),
        "right" => "Right".to_string(),
        "up" => "Up".to_string(),
        "down" => "Down".to_string(),
        // Single characters stay as-is
        s if s.len() == 1 => s.to_string(),
        // Everything else gets capitalized first letter
        s => {
            let mut chars = s.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().chain(chars).collect(),
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keymap_has_normal_mode_bindings() {
        let keymap = Keymap::new();
        let command = keymap.lookup(&"h".to_string(), &Modifiers::default(), EditMode::Normal);
        assert_eq!(command, Some(Command::MoveCursorLeft));
    }

    #[test]
    fn insert_mode_escape_returns_to_normal() {
        let keymap = Keymap::new();
        let command = keymap.lookup(
            &keys::ESCAPE.to_string(),
            &Modifiers::default(),
            EditMode::Insert,
        );
        assert_eq!(command, Some(Command::EnterNormalMode));
    }

    #[test]
    fn unknown_key_returns_none() {
        let keymap = Keymap::new();
        let command = keymap.lookup(&"z".to_string(), &Modifiers::default(), EditMode::Normal);
        assert_eq!(command, None);
    }

    #[test]
    fn available_commands_returns_mode_specific_commands() {
        let keymap = Keymap::new();
        let normal_commands = keymap.available_commands(EditMode::Normal);
        assert!(!normal_commands.is_empty());
        assert!(normal_commands.contains(&&Command::MoveCursorLeft));
    }

    #[test]
    fn key_normalization_works() {
        let mut keymap = Keymap::new();
        keymap.bind_normal_key("H".to_string(), Command::MoveCursorLeft);

        // Should find it with lowercase
        let command = keymap.lookup(&"h".to_string(), &Modifiers::default(), EditMode::Normal);
        assert_eq!(command, Some(Command::MoveCursorLeft));

        // Should also find it with uppercase
        let command = keymap.lookup(&"H".to_string(), &Modifiers::default(), EditMode::Normal);
        assert_eq!(command, Some(Command::MoveCursorLeft));
    }
}
