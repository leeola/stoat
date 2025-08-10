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

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::ExitApp => write!(f, "Exit app"),
            Action::ChangeMode(mode) => write!(f, "{} mode", mode.as_str()),
            Action::Move(dir) => write!(f, "Move {}", dir),
            Action::Jump(target) => write!(f, "Jump {}", target),
            Action::InsertChar => write!(f, "Insert"),
            Action::Delete => write!(f, "Delete"),
            Action::DeleteLine => write!(f, "Delete line"),
            Action::Yank => write!(f, "Yank"),
            Action::YankLine => write!(f, "Yank line"),
            Action::Paste => write!(f, "Paste"),
            Action::CommandInput => write!(f, "Command input"),
            Action::ExecuteCommand => write!(f, "Execute"),
            Action::ShowActionList => write!(f, "Show actions"),
            Action::ShowCommandPalette => write!(f, "Command palette"),
        }
    }
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Direction::Up => write!(f, "up"),
            Direction::Down => write!(f, "down"),
            Direction::Left => write!(f, "left"),
            Direction::Right => write!(f, "right"),
        }
    }
}

impl std::fmt::Display for JumpTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JumpTarget::LineStart => write!(f, "line start"),
            JumpTarget::LineEnd => write!(f, "line end"),
            JumpTarget::FileStart => write!(f, "file start"),
            JumpTarget::FileEnd => write!(f, "file end"),
            JumpTarget::WordForward => write!(f, "word forward"),
            JumpTarget::WordBackward => write!(f, "word backward"),
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
