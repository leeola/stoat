//! Mode-switching commands for changing editor modes.

use crate::actions::{EditMode, EditorAction};

/// Mode change commands
#[derive(Debug, Clone, PartialEq)]
pub enum ModeCommand {
    /// Enter Insert mode
    EnterInsert,
    /// Enter Normal mode
    EnterNormal,
    /// Enter Command mode
    EnterCommand,
}

impl ModeCommand {
    pub fn to_action(&self) -> EditorAction {
        let mode = match self {
            ModeCommand::EnterInsert => EditMode::Insert,
            ModeCommand::EnterNormal => EditMode::Normal,
            ModeCommand::EnterCommand => EditMode::Command,
        };
        EditorAction::SetMode { mode }
    }

    pub fn description(&self) -> &'static str {
        match self {
            ModeCommand::EnterInsert => "Enter Insert mode",
            ModeCommand::EnterNormal => "Enter Normal mode",
            ModeCommand::EnterCommand => "Enter Command mode",
        }
    }

    pub fn short_name(&self) -> &'static str {
        match self {
            ModeCommand::EnterInsert => "Insert",
            ModeCommand::EnterNormal => "Normal",
            ModeCommand::EnterCommand => "Command",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stoat;

    #[test]
    fn enter_and_exit_insert_mode() {
        Stoat::test()
            .assert_mode(EditMode::Normal)
            .type_keys("i") // Enter insert mode
            .assert_mode(EditMode::Insert)
            .type_keys("<Esc>") // Exit to normal mode
            .assert_mode(EditMode::Normal);
    }

    #[test]
    fn enter_command_mode() {
        Stoat::test()
            .assert_mode(EditMode::Normal)
            .type_keys(":") // Enter command mode
            .assert_mode(EditMode::Command)
            .type_keys("<Esc>") // Exit to normal mode
            .assert_mode(EditMode::Normal);
    }

    #[test]
    fn mode_switching_with_typing() {
        Stoat::test()
            .type_keys("i") // Enter insert mode
            .assert_mode(EditMode::Insert)
            .type_keys("Hello")
            .assert_text("Hello")
            .assert_mode(EditMode::Insert) // Still in insert mode
            .type_keys("<Esc>")
            .assert_mode(EditMode::Normal)
            .assert_text("Hello"); // Text preserved after mode switch
    }
}
