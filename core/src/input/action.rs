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

    /// Canvas mode actions
    GatherNodes,

    /// Help actions
    ShowHelp,
    ShowActionHelp(String),
}

/// Editor modes
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    Command,
    Canvas,
    Help,
}

impl Mode {
    pub fn as_str(&self) -> &str {
        match self {
            Mode::Normal => "normal",
            Mode::Insert => "insert",
            Mode::Visual => "visual",
            Mode::Command => "command",
            Mode::Canvas => "canvas",
            Mode::Help => "help",
        }
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::ExitApp => write!(f, "Exit app"),
            Action::ChangeMode(mode) => write!(f, "{} mode", mode.as_str()),
            Action::Move(dir) => write!(f, "Move {dir}"),
            Action::Jump(target) => write!(f, "Jump {target}"),
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
            Action::GatherNodes => write!(f, "Gather nodes"),
            Action::ShowHelp => write!(f, "Show help"),
            Action::ShowActionHelp(action_name) => write!(f, "Help for {action_name}"),
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

impl Action {
    /// Get a brief description of this action
    pub fn description(&self) -> &str {
        match self {
            Action::ExitApp => "Exit the application",
            Action::ChangeMode(Mode::Normal) => "Return to Normal mode for navigation",
            Action::ChangeMode(Mode::Insert) => "Enter Insert mode for text editing",
            Action::ChangeMode(Mode::Visual) => "Enter Visual mode for selection",
            Action::ChangeMode(Mode::Command) => "Enter Command mode to execute commands",
            Action::ChangeMode(Mode::Canvas) => "Enter Canvas mode for node manipulation",
            Action::ChangeMode(Mode::Help) => "Enter Help mode for interactive documentation",
            Action::Move(_) => "Move cursor in the specified direction",
            Action::Jump(_) => "Jump to a specific location",
            Action::InsertChar => "Insert a character at cursor position",
            Action::Delete => "Delete character or selection",
            Action::DeleteLine => "Delete the entire current line",
            Action::Yank => "Copy selection to clipboard",
            Action::YankLine => "Copy entire line to clipboard",
            Action::Paste => "Paste from clipboard",
            Action::CommandInput => "Input command text",
            Action::ExecuteCommand => "Execute the entered command",
            Action::ShowActionList => "Display available actions",
            Action::ShowCommandPalette => "Open command palette",
            Action::GatherNodes => "Gather selected nodes into viewport",
            Action::ShowHelp => "Display help for current mode",
            Action::ShowActionHelp(_) => "Display detailed help information",
        }
    }

    /// Get extended description with examples and details
    pub fn extended_description(&self) -> String {
        match self {
            Action::ExitApp => "Exit the application immediately.\n\n\
                This will close the editor without saving any unsaved changes.\n\
                Key: Esc (in Normal mode)"
                .to_string(),
            Action::ChangeMode(mode) => match mode {
                Mode::Normal => "Normal mode is the default navigation mode.\n\n\
                        In this mode you can:\n\
                        - Navigate through the document\n\
                        - Execute commands\n\
                        - Switch to other modes\n\
                        - Perform quick edits\n\n\
                        Key: Esc (from any mode)"
                    .to_string(),
                Mode::Insert => "Insert mode allows text editing.\n\n\
                        In this mode:\n\
                        - Type to insert text at cursor\n\
                        - Use arrow keys to navigate\n\
                        - Press Esc to return to Normal mode\n\n\
                        Key: i (from Normal mode)"
                    .to_string(),
                Mode::Visual => "Visual mode for selecting text.\n\n\
                        In this mode:\n\
                        - Move cursor to extend selection\n\
                        - Perform operations on selected text\n\
                        - Copy, cut, or delete selections\n\n\
                        Key: v (from Normal mode)"
                    .to_string(),
                Mode::Command => "Command mode for executing commands.\n\n\
                        In this mode:\n\
                        - Type commands to execute\n\
                        - Press Enter to run command\n\
                        - Press Esc to cancel\n\n\
                        Key: : (from Normal mode)"
                    .to_string(),
                Mode::Canvas => "Canvas mode for node manipulation.\n\n\
                        In this mode:\n\
                        - Navigate between nodes\n\
                        - Reposition nodes on canvas\n\
                        - Create connections between nodes\n\
                        - Gather nodes into viewport\n\n\
                        Key: c (from Normal mode)"
                    .to_string(),
                Mode::Help => "Help mode for interactive command documentation.\n\n\
                        In this mode:\n\
                        - Press any key to see detailed help for that command\n\
                        - Navigate through help information\n\
                        - Return to previous mode with Esc\n\n\
                        Key: Shift+/ (? key) from any mode"
                    .to_string(),
            },
            Action::Move(dir) => {
                format!(
                    "Move cursor {dir}.\n\n\
                    Navigation keys:\n\
                    - h: Move left\n\
                    - j: Move down\n\
                    - k: Move up\n\
                    - l: Move right\n\
                    - Arrow keys also work\n\n\
                    Current direction: {dir}"
                )
            },
            Action::Jump(target) => {
                format!(
                    "Jump to {target}.\n\n\
                    Jump commands allow quick navigation:\n\
                    - gg: Jump to file start\n\
                    - G: Jump to file end\n\
                    - 0: Jump to line start\n\
                    - $: Jump to line end\n\
                    - w: Jump word forward\n\
                    - b: Jump word backward\n\n\
                    Current target: {target}"
                )
            },
            Action::InsertChar => "Insert character at cursor position.\n\n\
                This is the default action in Insert mode.\n\
                Any printable character typed will be inserted\n\
                at the current cursor position."
                .to_string(),
            Action::Delete => "Delete character or selection.\n\n\
                In Normal mode: Deletes character under cursor\n\
                In Visual mode: Deletes selected text\n\n\
                Key: d (in Normal or Visual mode)"
                .to_string(),
            Action::DeleteLine => "Delete entire current line.\n\n\
                Removes the entire line where the cursor is positioned.\n\
                The deleted line is stored and can be pasted.\n\n\
                Key: dd (in Normal mode)"
                .to_string(),
            Action::Yank => "Copy (yank) selection to clipboard.\n\n\
                In Visual mode: Copies selected text\n\
                The yanked text can be pasted elsewhere.\n\n\
                Key: y (in Visual mode)"
                .to_string(),
            Action::YankLine => "Copy entire line to clipboard.\n\n\
                Copies the entire current line without deleting it.\n\
                The line can then be pasted elsewhere.\n\n\
                Key: yy (in Normal mode)"
                .to_string(),
            Action::Paste => "Paste from clipboard.\n\n\
                Inserts previously yanked or deleted text\n\
                at the current cursor position.\n\n\
                Key: p (in Normal mode)"
                .to_string(),
            Action::CommandInput => "Input command text.\n\n\
                Default action in Command mode.\n\
                Allows typing command text that will be\n\
                executed when Enter is pressed."
                .to_string(),
            Action::ExecuteCommand => "Execute entered command.\n\n\
                Runs the command typed in Command mode.\n\
                Commands can perform various operations like\n\
                saving, opening files, or editor configuration.\n\n\
                Key: Enter (in Command mode)"
                .to_string(),
            Action::ShowActionList => "Display all available actions.\n\n\
                Shows a list of all actions that can be\n\
                performed in the current mode with their\n\
                associated key bindings."
                .to_string(),
            Action::ShowCommandPalette => "Open command palette.\n\n\
                Opens a searchable list of all available\n\
                commands. Type to filter and select commands\n\
                to execute them quickly."
                .to_string(),
            Action::GatherNodes => "Gather selected nodes into viewport.\n\n\
                Centers the viewport on selected nodes,\n\
                adjusting zoom if necessary to fit all\n\
                selected nodes within the visible area.\n\n\
                Key: a (in Canvas mode)"
                .to_string(),
            Action::ShowHelp => "Enter interactive help mode.\n\n\
                Opens help for the current mode. In help mode,\n\
                press any key to see detailed documentation\n\
                for that key's action.\n\n\
                Key: Shift+/ (in any mode)\n\
                Press Esc to exit help mode"
                .to_string(),
            Action::ShowActionHelp(action_name) => {
                format!(
                    "Display detailed help for '{action_name}'.\n\n\
                    Shows comprehensive information about\n\
                    this specific action including examples\n\
                    and usage details.\n\n\
                    Accessed via Help Mode (? then key)"
                )
            },
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

        let mode = Mode::Canvas;
        let serialized = ron::to_string(&mode).expect("Failed to serialize Canvas mode");
        assert_eq!(serialized, "Canvas");
    }
}
