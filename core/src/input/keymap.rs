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
    canvas_bindings.insert(Key::Modified(ModifiedKey::Shift('/')), Action::ShowHelp);

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

    ModalConfig {
        modes,
        initial_mode: Mode::Normal,
        global_bindings: HashMap::new(),
    }
}
