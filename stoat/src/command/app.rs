//! Application-level commands for editor operations.

use crate::actions::EditorAction;

/// Application commands
#[derive(Debug, Clone, PartialEq)]
pub enum AppCommand {
    /// Exit the application
    Exit,
    /// Toggle command info display
    ToggleCommandInfo,
}

impl AppCommand {
    pub fn to_action(&self) -> Option<EditorAction> {
        match self {
            AppCommand::Exit => None, // Exit is handled as an effect, not an action
            AppCommand::ToggleCommandInfo => Some(EditorAction::ToggleCommandInfo),
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            AppCommand::Exit => "Exit application",
            AppCommand::ToggleCommandInfo => "Toggle command help",
        }
    }

    pub fn short_name(&self) -> &'static str {
        match self {
            AppCommand::Exit => "Exit",
            AppCommand::ToggleCommandInfo => "Help",
        }
    }
}
