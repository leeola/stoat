//! Command types representing high-level editor operations.
//!
//! Commands are the intermediate layer between user input (keys, mouse, etc.)
//! and the low-level actions that transform editor state. They represent
//! semantic operations that users want to perform.

mod app;
mod edit;
mod mode;
mod movement;

pub use app::AppCommand;
use compact_str::CompactString;
pub use edit::EditCommand;
pub use mode::ModeCommand;
pub use movement::MovementCommand;
use smol_str::SmolStr;

/// High-level editor commands.
///
/// Commands represent the user's intent - what operation they want to perform.
/// These are mapped from keys via the keymap system and then converted to
/// [`EditorAction`]s that actually transform the state.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    // Movement commands
    /// Move cursor left one character
    MoveCursorLeft,
    /// Move cursor right one character
    MoveCursorRight,
    /// Move cursor up one line
    MoveCursorUp,
    /// Move cursor down one line
    MoveCursorDown,
    /// Move to next paragraph
    NextParagraph,
    /// Move to previous paragraph
    PreviousParagraph,

    // Mode change commands
    /// Enter Insert mode
    EnterInsertMode,
    /// Enter Normal mode
    EnterNormalMode,
    /// Enter Command mode
    EnterCommandMode,
    /// Enter a specific mode by name
    EnterMode(CompactString),

    // Text manipulation commands
    /// Insert a string at cursor position
    InsertStr(SmolStr),
    /// Insert a newline at cursor position
    InsertNewline,
    /// Delete character before cursor (backspace)
    DeleteChar,

    // Application commands
    /// Exit the application
    Exit,

    /// Toggle command info display
    ToggleCommandInfo,

    /// Delete a line
    DeleteLine,

    /// Delete a word
    DeleteWord,

    /// Insert a single character (used for fallback)
    InsertChar,

    /// Show help information
    Help,
}

impl Command {
    /// Returns a human-readable description of the command.
    pub fn description(&self) -> &'static str {
        match self {
            Command::MoveCursorLeft => MovementCommand::Left.description(),
            Command::MoveCursorRight => MovementCommand::Right.description(),
            Command::MoveCursorUp => MovementCommand::Up.description(),
            Command::MoveCursorDown => MovementCommand::Down.description(),
            Command::NextParagraph => MovementCommand::NextParagraph.description(),
            Command::PreviousParagraph => MovementCommand::PreviousParagraph.description(),
            Command::EnterInsertMode => ModeCommand::EnterInsert.description(),
            Command::EnterNormalMode => ModeCommand::EnterNormal.description(),
            Command::EnterCommandMode => ModeCommand::EnterCommand.description(),
            Command::EnterMode(_) => "Enter custom mode",
            Command::InsertStr(_) => "Insert text",
            Command::InsertNewline => EditCommand::InsertNewline.description(),
            Command::DeleteChar => EditCommand::DeleteChar.description(),
            Command::Exit => AppCommand::Exit.description(),
            Command::ToggleCommandInfo => AppCommand::ToggleCommandInfo.description(),
            Command::DeleteLine => "Delete line",
            Command::DeleteWord => "Delete word",
            Command::InsertChar => "Insert character",
            Command::Help => "Show help",
        }
    }

    /// Returns a short, concise name for display in UI.
    pub fn short_name(&self) -> &'static str {
        match self {
            Command::MoveCursorLeft => MovementCommand::Left.short_name(),
            Command::MoveCursorRight => MovementCommand::Right.short_name(),
            Command::MoveCursorUp => MovementCommand::Up.short_name(),
            Command::MoveCursorDown => MovementCommand::Down.short_name(),
            Command::NextParagraph => MovementCommand::NextParagraph.short_name(),
            Command::PreviousParagraph => MovementCommand::PreviousParagraph.short_name(),
            Command::EnterInsertMode => ModeCommand::EnterInsert.short_name(),
            Command::EnterNormalMode => ModeCommand::EnterNormal.short_name(),
            Command::EnterCommandMode => ModeCommand::EnterCommand.short_name(),
            Command::EnterMode(_) => "Mode",
            Command::InsertStr(_) => "Insert",
            Command::InsertNewline => EditCommand::InsertNewline.short_name(),
            Command::DeleteChar => EditCommand::DeleteChar.short_name(),
            Command::Exit => AppCommand::Exit.short_name(),
            Command::ToggleCommandInfo => AppCommand::ToggleCommandInfo.short_name(),
            Command::DeleteLine => "DelLine",
            Command::DeleteWord => "DelWord",
            Command::InsertChar => "InsChar",
            Command::Help => "Help",
        }
    }

    /// Converts a command to the corresponding editor action(s).
    ///
    /// Some commands map directly to actions, while others might require
    /// context from the editor state to determine the appropriate action.
    pub fn to_action(
        &self,
        state: &crate::state::EditorState,
    ) -> Option<crate::actions::EditorAction> {
        match self {
            Command::MoveCursorLeft => Some(MovementCommand::Left.to_action(state)),
            Command::MoveCursorRight => Some(MovementCommand::Right.to_action(state)),
            Command::MoveCursorUp => Some(MovementCommand::Up.to_action(state)),
            Command::MoveCursorDown => Some(MovementCommand::Down.to_action(state)),
            Command::NextParagraph => Some(MovementCommand::NextParagraph.to_action(state)),
            Command::PreviousParagraph => Some(MovementCommand::PreviousParagraph.to_action(state)),
            Command::EnterInsertMode => Some(ModeCommand::EnterInsert.to_action()),
            Command::EnterNormalMode => Some(ModeCommand::EnterNormal.to_action()),
            Command::EnterCommandMode => Some(ModeCommand::EnterCommand.to_action()),
            Command::EnterMode(mode_name) => Some(crate::actions::EditorAction::SetMode {
                mode: crate::actions::EditMode::from_name(mode_name),
            }),
            Command::InsertStr(text) => EditCommand::InsertStr(text.clone()).to_action(state),
            Command::InsertNewline => EditCommand::InsertNewline.to_action(state),
            Command::DeleteChar => EditCommand::DeleteChar.to_action(state),
            Command::Exit => AppCommand::Exit.to_action(),
            Command::ToggleCommandInfo => AppCommand::ToggleCommandInfo.to_action(),
            Command::DeleteLine => {
                // FIXME: Implement delete line action
                None
            },
            Command::DeleteWord => {
                // FIXME: Implement delete word action
                None
            },
            Command::InsertChar => {
                // InsertChar is handled specially in processor for fallback
                None
            },
            Command::Help => {
                // FIXME: Implement help action
                None
            },
        }
    }
}
