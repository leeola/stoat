use crate::input::{
    action::{Action, Mode},
    config::{ModalConfig, ModeDefinition},
    key::{Key, NamedKey},
};
use std::collections::HashMap;

/// Returns the default keymap configuration
pub fn default_keymap() -> ModalConfig {
    let mut modes = HashMap::new();

    // Normal mode - the default mode
    let mut normal_bindings = HashMap::new();
    normal_bindings.insert(Key::Char('i'), Action::ChangeMode(Mode::Insert));
    normal_bindings.insert(Key::Char('v'), Action::ChangeMode(Mode::Visual));
    normal_bindings.insert(Key::Char(':'), Action::ChangeMode(Mode::Command));
    normal_bindings.insert(Key::Char('c'), Action::ChangeMode(Mode::Canvas));
    normal_bindings.insert(Key::Named(NamedKey::Esc), Action::ExitApp);
    normal_bindings.insert(Key::Sequence("dd".to_string()), Action::DeleteLine);

    modes.insert(
        Mode::Normal,
        ModeDefinition {
            bindings: normal_bindings,
            default_action: None,
        },
    );

    // Insert mode - for text entry
    let mut insert_bindings = HashMap::new();
    insert_bindings.insert(Key::Named(NamedKey::Esc), Action::ChangeMode(Mode::Normal));

    modes.insert(
        Mode::Insert,
        ModeDefinition {
            bindings: insert_bindings,
            default_action: Some(Action::InsertChar),
        },
    );

    // Visual mode - for selection
    let mut visual_bindings = HashMap::new();
    visual_bindings.insert(Key::Named(NamedKey::Esc), Action::ChangeMode(Mode::Normal));
    visual_bindings.insert(Key::Char('d'), Action::Delete);

    modes.insert(
        Mode::Visual,
        ModeDefinition {
            bindings: visual_bindings,
            default_action: None,
        },
    );

    // Command mode - for commands
    let mut command_bindings = HashMap::new();
    command_bindings.insert(Key::Named(NamedKey::Esc), Action::ChangeMode(Mode::Normal));
    command_bindings.insert(Key::Named(NamedKey::Enter), Action::ExecuteCommand);

    modes.insert(
        Mode::Command,
        ModeDefinition {
            bindings: command_bindings,
            default_action: Some(Action::CommandInput),
        },
    );

    // Canvas mode - for node manipulation
    let mut canvas_bindings = HashMap::new();
    canvas_bindings.insert(Key::Named(NamedKey::Esc), Action::ChangeMode(Mode::Normal));
    canvas_bindings.insert(Key::Char('a'), Action::GatherNodes);

    modes.insert(
        Mode::Canvas,
        ModeDefinition {
            bindings: canvas_bindings,
            default_action: None,
        },
    );

    ModalConfig {
        modes,
        initial_mode: Mode::Normal,
        global_bindings: HashMap::new(),
    }
}
