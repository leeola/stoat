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
