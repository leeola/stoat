use super::{
    action::{Action, Mode},
    config::ModalConfig,
    key::{Key, NamedKey},
};
use crate::value::Value;
use std::collections::VecDeque;

/// State for command input in command mode
#[derive(Debug, Clone, Default)]
pub struct CommandInputState {
    /// The current command being typed
    pub buffer: String,
    /// Available command completions
    pub completions: Vec<String>,
    /// Current completion index
    pub completion_index: usize,
    /// Whether we're in completion mode
    pub showing_completions: bool,
}

impl CommandInputState {
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.completions.clear();
        self.completion_index = 0;
        self.showing_completions = false;
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Parse the command buffer into command name and arguments
    pub fn parse_command(&self) -> Option<(String, Vec<Value>)> {
        if self.buffer.trim().is_empty() {
            return None;
        }

        let parts: Vec<&str> = self.buffer.trim().split_whitespace().collect();
        if parts.is_empty() {
            return None;
        }

        let command_name = parts[0].to_string();
        let args: Vec<Value> = parts[1..]
            .iter()
            .map(|&arg| {
                // Try to parse as number first
                if let Ok(num) = arg.parse::<i64>() {
                    Value::I64(num)
                } else if let Ok(num) = arg.parse::<u64>() {
                    Value::U64(num)
                } else if let Ok(num) = arg.parse::<f64>() {
                    Value::Float(num.into())
                } else if arg == "true" {
                    Value::Bool(true)
                } else if arg == "false" {
                    Value::Bool(false)
                } else {
                    // Default to string, removing quotes if present
                    let cleaned = arg.trim_matches('"').trim_matches('\'');
                    Value::String(cleaned.into())
                }
            })
            .collect();

        Some((command_name, args))
    }
}

/// The modal input system that processes keys and returns actions
pub struct ModalSystem {
    /// Current configuration
    config: ModalConfig,

    /// Current active mode
    current_mode: Mode,

    /// Mode history for returning to previous modes
    mode_stack: Vec<Mode>,

    /// When in help mode, which mode's help is being shown
    help_target_mode: Option<Mode>,

    /// Whether we're currently showing action-specific help
    showing_action_help: bool,

    /// The current action being shown in action help (if any)
    current_action_help: Option<String>,

    /// Buffer for multi-key sequences
    key_buffer: VecDeque<Key>,

    /// Timeout for key sequences (in frames/ticks)
    sequence_timeout: u32,

    /// Ticks since last key
    ticks_since_key: u32,

    /// Command input state
    command_input: CommandInputState,
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
            help_target_mode: None,
            showing_action_help: false,
            current_action_help: None,
            key_buffer: VecDeque::new(),
            sequence_timeout: 30, // About 0.5 seconds at 60fps
            ticks_since_key: 0,
            command_input: CommandInputState::default(),
        }
    }

    /// Get the current mode
    pub fn current_mode(&self) -> &Mode {
        &self.current_mode
    }

    /// Get the command input state
    pub fn command_input(&self) -> &CommandInputState {
        &self.command_input
    }

    /// Get mutable command input state
    pub fn command_input_mut(&mut self) -> &mut CommandInputState {
        &mut self.command_input
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

        // Special handling for Command mode
        if self.current_mode == Mode::Command {
            return self.handle_command_mode_key(key);
        }

        // Special handling for Help mode
        if self.current_mode == Mode::Help {
            // Handle Esc when showing action help - return to mode help
            if matches!(key, Key::Named(NamedKey::Esc)) && self.showing_action_help {
                self.key_buffer.clear();
                self.showing_action_help = false;
                self.current_action_help = None;
                // Return to showing the current help target mode
                if let Some(target_mode) = self.help_target_mode.clone() {
                    return Some(Action::ShowModeHelp(target_mode));
                } else {
                    // Fallback to previous mode if no target set
                    if let Some(previous_mode) = self.mode_stack.last() {
                        return Some(Action::ShowModeHelp(previous_mode.clone()));
                    } else {
                        return Some(Action::ShowModeHelp(Mode::Normal));
                    }
                }
            }

            // Handle Esc to return to previous mode (only if it wouldn't trigger help navigation)
            if matches!(key, Key::Named(NamedKey::Esc)) {
                // Check if Esc would navigate to a mode we're already showing help for
                let mut target_mode_for_esc = None;
                for (mode, mode_def) in &self.config.modes {
                    if *mode != Mode::Help {
                        if let Some(Action::ChangeMode(target_mode)) =
                            mode_def.bindings.get(&Key::Named(NamedKey::Esc))
                        {
                            target_mode_for_esc = Some(target_mode);
                            break;
                        }
                    }
                }

                // If we're already showing help for the target mode, exit help mode
                if let Some(target_mode) = target_mode_for_esc {
                    if self.help_target_mode == Some(target_mode.clone()) {
                        self.key_buffer.clear();
                        self.help_target_mode = None;
                        self.showing_action_help = false;
                        self.current_action_help = None;
                        if let Some(previous_mode) = self.mode_stack.pop() {
                            self.current_mode = previous_mode.clone();
                            return Some(Action::ChangeMode(previous_mode));
                        } else {
                            self.current_mode = Mode::Normal;
                            return Some(Action::ChangeMode(Mode::Normal));
                        }
                    }
                }
            }

            // First priority: Check if this key would trigger a mode change from any mode
            // This allows navigation to any mode from help mode
            // Exclude Help mode itself to avoid conflicts with Help mode's own bindings
            for (mode, mode_def) in &self.config.modes {
                if *mode != Mode::Help {
                    if let Some(Action::ChangeMode(target_mode)) =
                        self.check_sequence_match(&mode_def.bindings)
                    {
                        self.key_buffer.clear();
                        self.help_target_mode = Some(target_mode.clone());
                        self.showing_action_help = false;
                        self.current_action_help = None;
                        return Some(Action::ShowModeHelp(target_mode));
                    }
                }
            }

            // If no mode change found, check for actions in the currently displayed mode
            let display_mode = self
                .help_target_mode
                .as_ref()
                .or_else(|| self.mode_stack.last())
                .unwrap_or(&Mode::Normal);

            if let Some(mode_def) = self.config.modes.get(display_mode) {
                if let Some(_action) = self.check_sequence_match(&mode_def.bindings) {
                    self.key_buffer.clear();
                    self.showing_action_help = true;
                    let key_str = key.to_string();
                    self.current_action_help = Some(key_str.clone());
                    return Some(Action::ShowActionHelp(key_str));
                }
            }

            // Clear buffer and ignore unrecognized keys in help mode
            self.key_buffer.clear();
            return None;
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

    /// Handle key input in command mode
    fn handle_command_mode_key(&mut self, key: Key) -> Option<Action> {
        self.key_buffer.clear(); // Don't accumulate keys in command mode

        match key {
            Key::Named(NamedKey::Esc) => {
                // Cancel command mode and return to previous mode
                self.command_input.clear();
                if let Some(previous_mode) = self.mode_stack.pop() {
                    self.current_mode = previous_mode.clone();
                    Some(Action::ChangeMode(previous_mode))
                } else {
                    self.current_mode = Mode::Normal;
                    Some(Action::ChangeMode(Mode::Normal))
                }
            },
            Key::Named(NamedKey::Enter) => {
                // Execute command
                if let Some((command_name, args)) = self.command_input.parse_command() {
                    self.command_input.clear();
                    // Exit command mode
                    if let Some(previous_mode) = self.mode_stack.pop() {
                        self.current_mode = previous_mode;
                    } else {
                        self.current_mode = Mode::Normal;
                    }
                    Some(Action::ExecuteCommand(command_name, args))
                } else {
                    None // Empty command, do nothing
                }
            },
            Key::Named(NamedKey::Tab) => {
                // TODO: Implement command completion
                // For now, just ignore Tab
                None
            },
            Key::Named(NamedKey::Backspace) => {
                // Remove last character
                if !self.command_input.buffer.is_empty() {
                    self.command_input.buffer.pop();
                }
                None
            },
            Key::Char(c) => {
                // Add character to command buffer
                self.command_input.buffer.push(c);
                None
            },
            _ => {
                // Ignore other keys in command mode
                None
            },
        }
    }

    /// Handle mode changes and return the action
    fn handle_action(&mut self, action: Action) -> Action {
        match &action {
            Action::ChangeMode(new_mode) => {
                self.mode_stack.push(self.current_mode.clone());
                self.current_mode = new_mode.clone();
                // Reset action help when changing modes
                if *new_mode != Mode::Help {
                    self.showing_action_help = false;
                    self.current_action_help = None;
                }
                // Clear command input when entering command mode
                if *new_mode == Mode::Command {
                    self.command_input.clear();
                }
            },
            Action::ShowHelp => {
                // Convert ShowHelp to ChangeMode(Help)
                self.mode_stack.push(self.current_mode.clone());
                let previous_mode = self.current_mode.clone();
                self.current_mode = Mode::Help;
                self.help_target_mode = Some(previous_mode); // Set to previous mode so first Esc can exit
                self.showing_action_help = false; // Reset action help flag when entering help
                self.current_action_help = None;
                return Action::ChangeMode(Mode::Help);
            },
            _ => {},
        }
        action
    }

    /// Get all available actions in the current mode
    pub fn available_actions(&self) -> Vec<(&Key, &Action)> {
        // When in help mode, show actions for the help target mode or previous mode
        if self.current_mode == Mode::Help {
            let target_mode = self
                .help_target_mode
                .as_ref()
                .or_else(|| self.mode_stack.last())
                .unwrap_or(&Mode::Normal);
            self.available_actions_for_mode(target_mode)
        } else {
            self.available_actions_for_mode(&self.current_mode)
        }
    }

    /// Get all available actions for a specific mode
    pub fn available_actions_for_mode(&self, mode: &Mode) -> Vec<(&Key, &Action)> {
        let mut actions = Vec::new();

        // Add global actions
        for (key, action) in &self.config.global_bindings {
            actions.push((key, action));
        }

        // Add mode-specific actions
        if let Some(mode_def) = self.config.modes.get(mode) {
            for (key, action) in &mode_def.bindings {
                actions.push((key, action));
            }
        }

        actions
    }

    /// Get the previous mode from the stack (for help mode)
    pub fn previous_mode(&self) -> Option<&Mode> {
        self.mode_stack.last()
    }

    /// Get the help target mode (which mode's help is being shown)
    pub fn help_target_mode(&self) -> Option<&Mode> {
        self.help_target_mode.as_ref()
    }

    /// Check if currently showing action-specific help
    pub fn showing_action_help(&self) -> bool {
        self.showing_action_help
    }

    /// Get the current action being shown in help (if any)
    pub fn current_action_help(&self) -> Option<&str> {
        self.current_action_help.as_deref()
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
                        Char('c'): ChangeMode(Canvas),
                    }
                ),
                Canvas: (
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

        // Press 'c' to enter canvas mode
        let action = system.process_key(Key::Char('c'));
        assert_eq!(action, Some(Action::ChangeMode(Mode::Canvas)));
        assert_eq!(system.current_mode(), &Mode::Canvas);

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
                        Sequence("gg"): GatherNodes,
                        Char('g'): ShowHelp,
                    }
                ),
            },
            initial_mode: Normal,
        )"#;

        let config =
            ModalConfig::from_ron(ron_str).expect("Failed to parse modal config for sequence test");
        let mut system = ModalSystem::with_config(config);

        // First 'g' should wait
        let action = system.process_key(Key::Char('g'));
        assert_eq!(action, None);

        // Second 'g' should trigger GatherNodes
        let action = system.process_key(Key::Char('g'));
        assert_eq!(action, Some(Action::GatherNodes));
    }
}
