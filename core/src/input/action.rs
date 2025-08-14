use serde::{Deserialize, Serialize};

/// Actions that can be performed in the editor
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub enum Action {
    /// Exit the application
    ExitApp,

    /// Change to a different mode
    ChangeMode(Mode),

    /// Canvas mode actions
    GatherNodes,

    /// Help actions
    ShowHelp,
    ShowActionHelp(String),
    ShowModeHelp(Mode),
}

/// Help system state for GUI rendering
#[derive(Debug, Clone, PartialEq)]
pub struct HelpDisplayState {
    /// Whether help should be visible
    pub visible: bool,
    /// Type of help being displayed
    pub help_type: HelpType,
    /// Name of the mode or action being shown
    pub title: String,
    /// List of commands to display (key, action, description)
    pub commands: Vec<(String, String, String)>,
    /// Extended help text for action-specific help
    pub extended_help: Option<String>,
}

/// Type of help being displayed
#[derive(Debug, Clone, PartialEq)]
pub enum HelpType {
    /// Basic mode help showing key bindings
    Mode,
    /// Extended help with detailed descriptions
    ExtendedMode,
    /// Help for a specific action
    Action,
}

impl Default for HelpDisplayState {
    fn default() -> Self {
        Self {
            visible: false,
            help_type: HelpType::Mode,
            title: String::new(),
            commands: Vec::new(),
            extended_help: None,
        }
    }
}

/// Command info display state for GUI rendering
#[derive(Debug, Clone, PartialEq)]
pub struct CommandInfoState {
    /// Whether command info should be visible
    pub visible: bool,
    /// Current mode name
    pub mode_name: String,
    /// List of key bindings to display (key, description)
    pub commands: Vec<(String, String)>,
}

impl Default for CommandInfoState {
    fn default() -> Self {
        Self {
            visible: true,
            mode_name: String::new(),
            commands: Vec::new(),
        }
    }
}

/// Editor modes
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum Mode {
    Normal,
    Canvas,
    Help,
}

impl Mode {
    pub fn as_str(&self) -> &str {
        match self {
            Mode::Normal => "normal",
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
            Action::GatherNodes => write!(f, "Gather nodes"),
            Action::ShowHelp => write!(f, "Show help"),
            Action::ShowActionHelp(action_name) => write!(f, "Help for {action_name}"),
            Action::ShowModeHelp(mode) => write!(f, "Show {} help", mode.as_str()),
        }
    }
}

impl Action {
    /// Get a brief description of this action
    pub fn description(&self) -> &str {
        match self {
            Action::ExitApp => "Exit the application",
            Action::ChangeMode(Mode::Normal) => "Return to Normal mode for navigation",
            Action::ChangeMode(Mode::Canvas) => "Enter Canvas mode for node manipulation",
            Action::ChangeMode(Mode::Help) => "Enter Help mode for interactive documentation",
            Action::GatherNodes => "Gather selected nodes into viewport",
            Action::ShowHelp => "Display help for current mode",
            Action::ShowActionHelp(_) => "Display detailed help information",
            Action::ShowModeHelp(_) => "Display help for specific mode",
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
                        - Navigate the canvas\n\
                        - Switch to Canvas mode\n\
                        - Access help\n\n\
                        Key: Esc (from any mode)"
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
            Action::ShowModeHelp(mode) => {
                format!(
                    "Display help for {} mode.\n\n\
                    Shows all available commands and keybindings\n\
                    for {} mode without leaving help mode.\n\
                    This allows exploring different modes' commands\n\
                    from within the interactive help system.\n\n\
                    Navigate modes by pressing their activation keys\n\
                    while in help mode (e.g., 'c' for Canvas mode)",
                    mode.as_str(),
                    mode.as_str()
                )
            },
        }
    }
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

        let action = Action::GatherNodes;
        let serialized = ron::to_string(&action).expect("Failed to serialize GatherNodes action");
        assert_eq!(serialized, "GatherNodes");

        let action = Action::ChangeMode(Mode::Canvas);
        let serialized = ron::to_string(&action).expect("Failed to serialize ChangeMode action");
        assert_eq!(serialized, "ChangeMode(Canvas)");
    }

    #[test]
    fn test_mode_serialization() {
        let mode = Mode::Normal;
        let serialized = ron::to_string(&mode).expect("Failed to serialize Normal mode");
        assert_eq!(serialized, "Normal");

        let mode = Mode::Canvas;
        let serialized = ron::to_string(&mode).expect("Failed to serialize Canvas mode");
        assert_eq!(serialized, "Canvas");

        let mode = Mode::Help;
        let serialized = ron::to_string(&mode).expect("Failed to serialize Help mode");
        assert_eq!(serialized, "Help");
    }
}
