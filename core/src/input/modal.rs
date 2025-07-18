use super::{
    action::{Action, Mode},
    config::ModalConfig,
    key::Key,
};
use std::collections::VecDeque;

/// The modal input system that processes keys and returns actions
pub struct ModalSystem {
    /// Current configuration
    config: ModalConfig,

    /// Current active mode
    current_mode: Mode,

    /// Mode history for returning to previous modes
    mode_stack: Vec<Mode>,

    /// Buffer for multi-key sequences
    key_buffer: VecDeque<Key>,

    /// Timeout for key sequences (in frames/ticks)
    sequence_timeout: u32,

    /// Ticks since last key
    ticks_since_key: u32,
}

impl ModalSystem {
    /// Create a new modal system with default configuration
    pub fn new() -> Self {
        Self::with_config(ModalConfig::default())
    }

    /// Create a new modal system with provided configuration
    pub fn with_config(config: ModalConfig) -> Self {
        let initial_mode = config.initial_mode.clone();
        Self {
            config,
            current_mode: initial_mode,
            mode_stack: Vec::new(),
            key_buffer: VecDeque::new(),
            sequence_timeout: 30, // About 0.5 seconds at 60fps
            ticks_since_key: 0,
        }
    }

    /// Get the current mode
    pub fn current_mode(&self) -> &Mode {
        &self.current_mode
    }

    /// Process a key input and return an action if one is triggered
    pub fn process_key(&mut self, key: Key) -> Option<Action> {
        // Reset timeout
        self.ticks_since_key = 0;

        // Add key to buffer
        self.key_buffer.push_back(key.clone());

        // Check global bindings first
        if let Some(action) = self.check_global_binding() {
            self.key_buffer.clear();
            return Some(self.handle_action(action));
        }

        // Check current mode bindings
        if let Some(mode_def) = self.config.modes.get(&self.current_mode) {
            // First check if buffer could be a prefix of any sequence
            if self.could_be_sequence_prefix(&mode_def.bindings) {
                // Wait for more keys
                return None;
            }

            // Try to match the buffer as a complete binding
            if let Some(action) = self.check_sequence_match(&mode_def.bindings) {
                self.key_buffer.clear();
                return Some(self.handle_action(action));
            }

            // Check default action
            if let Some(default_action) = &mode_def.default_action {
                self.key_buffer.clear();
                return Some(default_action.clone());
            }
        }

        // No match found
        self.key_buffer.clear();
        None
    }

    /// Update the system (call this on each frame/tick)
    pub fn tick(&mut self) {
        self.ticks_since_key += 1;

        // Clear buffer if timeout exceeded
        if self.ticks_since_key > self.sequence_timeout && !self.key_buffer.is_empty() {
            self.key_buffer.clear();
        }
    }

    /// Check if the current buffer matches any global binding
    fn check_global_binding(&self) -> Option<Action> {
        let buffer_as_key = self.buffer_to_key()?;
        self.config.global_bindings.get(&buffer_as_key).cloned()
    }

    /// Check if the current buffer matches any sequence in the bindings
    fn check_sequence_match(
        &self,
        bindings: &std::collections::HashMap<Key, Action>,
    ) -> Option<Action> {
        let buffer_as_key = self.buffer_to_key()?;
        bindings.get(&buffer_as_key).cloned()
    }

    /// Check if the current buffer could be a prefix of any binding
    fn could_be_sequence_prefix(&self, bindings: &std::collections::HashMap<Key, Action>) -> bool {
        let buffer_str = self.buffer_to_string();

        bindings.keys().any(|key| match key {
            Key::Sequence(seq) => seq.starts_with(&buffer_str) && seq != &buffer_str,
            _ => false,
        })
    }

    /// Convert the current buffer to a Key
    fn buffer_to_key(&self) -> Option<Key> {
        match self.key_buffer.len() {
            0 => None,
            1 => self.key_buffer.front().cloned(),
            _ => Some(Key::Sequence(self.buffer_to_string())),
        }
    }

    /// Convert the buffer to a string for sequence matching
    fn buffer_to_string(&self) -> String {
        self.key_buffer
            .iter()
            .map(|k| match k {
                Key::Char(ch) => ch.to_string(),
                Key::Named(named) => format!("<{named:?}>"),
                Key::Modified(modified) => format!("<{modified:?}>"),
                Key::Sequence(seq) => seq.clone(),
            })
            .collect()
    }

    /// Handle mode changes and return the action
    fn handle_action(&mut self, action: Action) -> Action {
        if let Action::ChangeMode(new_mode) = &action {
            self.mode_stack.push(self.current_mode.clone());
            self.current_mode = new_mode.clone();
        }
        action
    }

    /// Get all available actions in the current mode
    pub fn available_actions(&self) -> Vec<(&Key, &Action)> {
        let mut actions = Vec::new();

        // Add global actions
        for (key, action) in &self.config.global_bindings {
            actions.push((key, action));
        }

        // Add mode-specific actions
        if let Some(mode_def) = self.config.modes.get(&self.current_mode) {
            for (key, action) in &mode_def.bindings {
                actions.push((key, action));
            }
        }

        actions
    }
}

impl Default for ModalSystem {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{action::Mode, key::NamedKey};

    #[test]
    fn test_basic_key_processing() {
        let mut system = ModalSystem::new();

        // Escape should exit
        let action = system.process_key(Key::Named(NamedKey::Esc));
        assert_eq!(action, Some(Action::ExitApp));
    }

    #[test]
    fn test_mode_change() {
        let ron_str = r#"(
            modes: {
                Normal: (
                    bindings: {
                        Char('i'): ChangeMode(Insert),
                    }
                ),
                Insert: (
                    bindings: {
                        Named(Esc): ChangeMode(Normal),
                    }
                ),
            },
            initial_mode: Normal,
        )"#;

        let config = ModalConfig::from_ron(ron_str)
            .expect("Failed to parse modal config for mode change test");
        let mut system = ModalSystem::with_config(config);

        assert_eq!(system.current_mode(), &Mode::Normal);

        // Press 'i' to enter insert mode
        let action = system.process_key(Key::Char('i'));
        assert_eq!(action, Some(Action::ChangeMode(Mode::Insert)));
        assert_eq!(system.current_mode(), &Mode::Insert);

        // Press Esc to return to normal mode
        let action = system.process_key(Key::Named(NamedKey::Esc));
        assert_eq!(action, Some(Action::ChangeMode(Mode::Normal)));
        assert_eq!(system.current_mode(), &Mode::Normal);
    }

    #[test]
    fn test_sequence_handling() {
        let ron_str = r#"(
            modes: {
                Normal: (
                    bindings: {
                        Sequence("dd"): DeleteLine,
                        Char('d'): Delete,
                    }
                ),
            },
            initial_mode: Normal,
        )"#;

        let config =
            ModalConfig::from_ron(ron_str).expect("Failed to parse modal config for sequence test");
        let mut system = ModalSystem::with_config(config);

        // First 'd' should wait
        let action = system.process_key(Key::Char('d'));
        assert_eq!(action, None);

        // Second 'd' should trigger DeleteLine
        let action = system.process_key(Key::Char('d'));
        assert_eq!(action, Some(Action::DeleteLine));
    }
}
