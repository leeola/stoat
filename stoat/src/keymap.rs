//! Keymap system for mapping keys to commands based on editor mode.
//!
//! The keymap provides mode-dependent key bindings, allowing the same key
//! to perform different operations depending on the current editor mode.

use crate::{actions::EditMode, command::Command};
use iced::keyboard;
use std::collections::HashMap;

/// Mode-dependent keymap for converting key presses to commands.
///
/// Each editing mode has its own set of key bindings. When a key is pressed,
/// the keymap looks up the appropriate command based on the current mode.
#[derive(Debug, Clone)]
pub struct Keymap {
    normal: HashMap<KeyBinding, Command>,
    insert: HashMap<KeyBinding, Command>,
    visual: HashMap<KeyBinding, Command>,
    command: HashMap<KeyBinding, Command>,
}

impl Keymap {
    /// Creates a new keymap with default vim-like bindings.
    pub fn new() -> Self {
        let mut keymap = Self {
            normal: HashMap::new(),
            insert: HashMap::new(),
            visual: HashMap::new(),
            command: HashMap::new(),
        };

        keymap.setup_default_bindings();
        keymap
    }

    /// Looks up a command for the given key and mode.
    pub fn lookup(
        &self,
        key: &keyboard::Key,
        modifiers: &keyboard::Modifiers,
        mode: EditMode,
    ) -> Option<Command> {
        let binding = KeyBinding::new(key.clone(), *modifiers);

        match mode {
            EditMode::Normal => self.normal.get(&binding),
            EditMode::Insert => self.insert.get(&binding),
            EditMode::Visual { .. } => self.visual.get(&binding),
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
        self.bind_normal("v", Command::EnterVisualMode);
        self.bind_normal(":", Command::EnterCommandMode);
        self.bind_normal("?", Command::ToggleCommandInfo);

        // Escape in normal mode exits
        self.bind_normal_key(
            keyboard::Key::Named(keyboard::key::Named::Escape),
            Command::Exit,
        );

        // Insert mode bindings
        self.bind_insert_key(
            keyboard::Key::Named(keyboard::key::Named::Escape),
            Command::EnterNormalMode,
        );
        self.bind_insert_key(
            keyboard::Key::Named(keyboard::key::Named::Enter),
            Command::InsertNewline,
        );
        self.bind_insert_key(
            keyboard::Key::Named(keyboard::key::Named::Backspace),
            Command::DeleteChar,
        );

        // Visual mode bindings
        self.bind_visual_key(
            keyboard::Key::Named(keyboard::key::Named::Escape),
            Command::EnterNormalMode,
        );

        // Command mode bindings
        self.bind_command_key(
            keyboard::Key::Named(keyboard::key::Named::Escape),
            Command::EnterNormalMode,
        );
    }

    /// Convenience method for binding character keys in normal mode.
    fn bind_normal(&mut self, ch: &str, command: Command) {
        let key = keyboard::Key::Character(ch.to_string().into());
        self.bind_normal_key(key, command);
    }

    /// Bind a key to a command in normal mode.
    fn bind_normal_key(&mut self, key: keyboard::Key, command: Command) {
        let binding = KeyBinding::new(key, keyboard::Modifiers::default());
        self.normal.insert(binding, command);
    }

    /// Bind a key to a command in insert mode.
    fn bind_insert_key(&mut self, key: keyboard::Key, command: Command) {
        let binding = KeyBinding::new(key, keyboard::Modifiers::default());
        self.insert.insert(binding, command);
    }

    /// Bind a key to a command in visual mode.
    fn bind_visual_key(&mut self, key: keyboard::Key, command: Command) {
        let binding = KeyBinding::new(key, keyboard::Modifiers::default());
        self.visual.insert(binding, command);
    }

    /// Bind a key to a command in command mode.
    fn bind_command_key(&mut self, key: keyboard::Key, command: Command) {
        let binding = KeyBinding::new(key, keyboard::Modifiers::default());
        self.command.insert(binding, command);
    }

    /// Returns all available commands for the given mode.
    pub fn available_commands(&self, mode: EditMode) -> Vec<&Command> {
        match mode {
            EditMode::Normal => self.normal.values().collect(),
            EditMode::Insert => self.insert.values().collect(),
            EditMode::Visual { .. } => self.visual.values().collect(),
            EditMode::Command => self.command.values().collect(),
        }
    }

    /// Returns key bindings for the given mode as (key_display, command) pairs.
    pub fn get_bindings_for_mode(&self, mode: EditMode) -> Vec<(String, Command)> {
        let map = match mode {
            EditMode::Normal => &self.normal,
            EditMode::Insert => &self.insert,
            EditMode::Visual { .. } => &self.visual,
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
    key: keyboard::Key,
    modifiers: keyboard::Modifiers,
}

impl KeyBinding {
    fn new(key: keyboard::Key, modifiers: keyboard::Modifiers) -> Self {
        Self { key, modifiers }
    }
}

/// Formats a keyboard key for display in UI
fn format_key_binding(key: &keyboard::Key) -> String {
    match key {
        keyboard::Key::Character(s) => s.to_string(),
        keyboard::Key::Named(named) => match named {
            keyboard::key::Named::Escape => "Esc".to_string(),
            keyboard::key::Named::Enter => "Enter".to_string(),
            keyboard::key::Named::Backspace => "Backsp".to_string(),
            keyboard::key::Named::Tab => "Tab".to_string(),
            keyboard::key::Named::Space => "Space".to_string(),
            keyboard::key::Named::ArrowLeft => "Left".to_string(),
            keyboard::key::Named::ArrowRight => "Right".to_string(),
            keyboard::key::Named::ArrowUp => "Up".to_string(),
            keyboard::key::Named::ArrowDown => "Down".to_string(),
            _ => format!("{named:?}"),
        },
        _ => format!("{key:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keymap_has_normal_mode_bindings() {
        let keymap = Keymap::new();
        let h_key = keyboard::Key::Character("h".to_string().into());
        let command = keymap.lookup(&h_key, &keyboard::Modifiers::default(), EditMode::Normal);
        assert_eq!(command, Some(Command::MoveCursorLeft));
    }

    #[test]
    fn insert_mode_escape_returns_to_normal() {
        let keymap = Keymap::new();
        let escape_key = keyboard::Key::Named(keyboard::key::Named::Escape);
        let command = keymap.lookup(
            &escape_key,
            &keyboard::Modifiers::default(),
            EditMode::Insert,
        );
        assert_eq!(command, Some(Command::EnterNormalMode));
    }

    #[test]
    fn unknown_key_returns_none() {
        let keymap = Keymap::new();
        let unknown_key = keyboard::Key::Character("z".to_string().into());
        let command = keymap.lookup(
            &unknown_key,
            &keyboard::Modifiers::default(),
            EditMode::Normal,
        );
        assert_eq!(command, None);
    }

    #[test]
    fn available_commands_returns_mode_specific_commands() {
        let keymap = Keymap::new();
        let normal_commands = keymap.available_commands(EditMode::Normal);
        assert!(!normal_commands.is_empty());
        assert!(normal_commands.contains(&&Command::MoveCursorLeft));
    }
}
