use crate::input::{
    action::{Action, Mode},
    config::{ModalConfig, ModeDefinition},
    key::{Key, ModifiedKey, NamedKey},
};
use std::collections::HashMap;

/// Returns the default keymap configuration
pub fn default_keymap() -> ModalConfig {
    let mut modes = HashMap::new();

    // Normal mode - the default mode
    let mut normal_bindings = HashMap::new();
    normal_bindings.insert(Key::Char('c'), Action::ChangeMode(Mode::Canvas));
    normal_bindings.insert(Key::Named(NamedKey::Esc), Action::ExitApp);
    normal_bindings.insert(Key::Modified(ModifiedKey::Shift('/')), Action::ShowHelp);
    normal_bindings.insert(
        Key::Modified(ModifiedKey::Alt('x')),
        Action::ChangeMode(Mode::Command),
    );
    // Add some direct command bindings (Emacs-style)
    normal_bindings.insert(
        Key::Modified(ModifiedKey::Ctrl('s')),
        Action::ExecuteCommand("save-buffer".to_string(), vec![]),
    );
    normal_bindings.insert(
        Key::Modified(ModifiedKey::Ctrl('x')),
        Action::ExecuteCommand("next-buffer".to_string(), vec![]),
    );

    modes.insert(
        Mode::Normal,
        ModeDefinition {
            bindings: normal_bindings,
            default_action: None,
        },
    );

    // Canvas mode - for node manipulation
    let mut canvas_bindings = HashMap::new();
    canvas_bindings.insert(Key::Named(NamedKey::Esc), Action::ChangeMode(Mode::Normal));
    canvas_bindings.insert(Key::Char('a'), Action::GatherNodes);
    canvas_bindings.insert(Key::Modified(ModifiedKey::Shift('a')), Action::AlignNodes);
    canvas_bindings.insert(Key::Modified(ModifiedKey::Shift('/')), Action::ShowHelp);
    canvas_bindings.insert(
        Key::Modified(ModifiedKey::Alt('x')),
        Action::ChangeMode(Mode::Command),
    );
    // Add direct command bindings to Canvas mode too
    canvas_bindings.insert(
        Key::Modified(ModifiedKey::Ctrl('s')),
        Action::ExecuteCommand("save-buffer".to_string(), vec![]),
    );
    canvas_bindings.insert(
        Key::Modified(ModifiedKey::Ctrl('x')),
        Action::ExecuteCommand("next-buffer".to_string(), vec![]),
    );

    modes.insert(
        Mode::Canvas,
        ModeDefinition {
            bindings: canvas_bindings,
            default_action: None,
        },
    );

    // Help mode - special interactive mode
    let mut help_bindings = HashMap::new();
    help_bindings.insert(Key::Named(NamedKey::Esc), Action::ChangeMode(Mode::Normal)); // This will be overridden

    modes.insert(
        Mode::Help,
        ModeDefinition {
            bindings: help_bindings,
            default_action: None, // Help mode will be handled specially
        },
    );

    // Command mode - for entering commands
    let command_bindings = HashMap::new();
    // Command mode key handling is done specially in the modal system
    // We still need the mode definition for completeness

    modes.insert(
        Mode::Command,
        ModeDefinition {
            bindings: command_bindings,
            default_action: None, // Command mode will be handled specially
        },
    );

    ModalConfig {
        modes,
        initial_mode: Mode::Normal,
        global_bindings: HashMap::new(),
    }
}
