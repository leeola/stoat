use serde::{Deserialize, Serialize};

/// Actions that can be performed in the editor
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub enum Action {
    /// Exit the application
    ExitApp,

    /// Change to a different mode
    ChangeMode(Mode),

    /// Movement actions
    Move(Direction),

    /// Jump to specific locations
    Jump(JumpTarget),

    /// Text editing actions
    InsertChar,
    Delete,
    DeleteLine,
    Yank,
    YankLine,
    Paste,

    /// Command mode actions
    CommandInput,
    ExecuteCommand,

    /// UI actions
    ShowActionList,
    ShowCommandPalette,
}

/// Editor modes
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    Command,

    /// Custom mode with a name
    Custom(String),
}

impl Mode {
    pub fn as_str(&self) -> &str {
        match self {
            Mode::Normal => "normal",
            Mode::Insert => "insert",
            Mode::Visual => "visual",
            Mode::Command => "command",
            Mode::Custom(name) => name,
        }
    }
}

/// Movement directions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// Jump targets for navigation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum JumpTarget {
    LineStart,
    LineEnd,
    FileStart,
    FileEnd,
    WordForward,
    WordBackward,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_serialization() {
        // Test that actions serialize nicely for RON
        let action = Action::ExitApp;
        let serialized = ron::to_string(&action).expect("Failed to serialize ExitApp action");
        assert_eq!(serialized, "ExitApp");

        let action = Action::Move(Direction::Down);
        let serialized = ron::to_string(&action).expect("Failed to serialize Move action");
        assert_eq!(serialized, "Move(Down)");

        let action = Action::ChangeMode(Mode::Insert);
        let serialized = ron::to_string(&action).expect("Failed to serialize ChangeMode action");
        assert_eq!(serialized, "ChangeMode(Insert)");
    }

    #[test]
    fn test_mode_serialization() {
        let mode = Mode::Normal;
        let serialized = ron::to_string(&mode).expect("Failed to serialize Normal mode");
        assert_eq!(serialized, "Normal");

        // Custom modes serialize with the Custom tag
        let mode = Mode::Custom("my_mode".to_string());
        let serialized = ron::to_string(&mode).expect("Failed to serialize Custom mode");
        assert_eq!(serialized, "Custom(\"my_mode\")");
    }
}
