//! Action definitions for stoat.
//!
//! Actions are dispatched through GPUI's action system and handled by [`crate::Stoat`].

use gpui::{actions, Action};
use std::{any::TypeId, collections::HashMap, sync::LazyLock};

// Editing actions
actions!(
    stoat,
    [
        /// Delete character before cursor
        DeleteLeft,
        /// Delete character after cursor
        DeleteRight,
        /// Delete word before cursor
        DeleteWordLeft,
        /// Delete word after cursor
        DeleteWordRight,
        /// Insert newline
        NewLine,
        /// Delete current line
        DeleteLine,
        /// Delete from cursor to end of line
        DeleteToEndOfLine,
    ]
);

// Movement actions
actions!(
    stoat,
    [
        /// Move cursor up one line
        MoveUp,
        /// Move cursor down one line
        MoveDown,
        /// Move cursor left one character
        MoveLeft,
        /// Move cursor right one character
        MoveRight,
        /// Move cursor left by one word
        MoveWordLeft,
        /// Move cursor right by one word
        MoveWordRight,
        /// Move cursor to start of line
        MoveToLineStart,
        /// Move cursor to end of line
        MoveToLineEnd,
        /// Move cursor to start of file
        MoveToFileStart,
        /// Move cursor to end of file
        MoveToFileEnd,
        /// Scroll up one page
        PageUp,
        /// Scroll down one page
        PageDown,
    ]
);

// Mode actions
actions!(
    stoat,
    [
        /// Enter insert mode
        EnterInsertMode,
        /// Enter normal mode
        EnterNormalMode,
        /// Enter visual mode
        EnterVisualMode,
        /// Enter space mode (leader key)
        EnterSpaceMode,
        /// Enter pane mode (window management)
        EnterPaneMode,
    ]
);

// File finder actions
actions!(
    stoat,
    [
        /// Open file finder
        OpenFileFinder,
        /// Move to next file in finder
        FileFinderNext,
        /// Move to previous file in finder
        FileFinderPrev,
        /// Select current file in finder
        FileFinderSelect,
        /// Dismiss file finder
        FileFinderDismiss,
    ]
);

// Command palette actions
actions!(
    stoat,
    [
        /// Open command palette
        OpenCommandPalette,
        /// Move to next command in palette
        CommandPaletteNext,
        /// Move to previous command in palette
        CommandPalettePrev,
        /// Execute selected command
        CommandPaletteExecute,
        /// Dismiss command palette
        CommandPaletteDismiss,
    ]
);

// Git status actions
actions!(
    stoat,
    [
        /// Open git status modal
        OpenGitStatus,
        /// Move to next file in git status
        GitStatusNext,
        /// Move to previous file in git status
        GitStatusPrev,
        /// Open selected file from git status
        GitStatusSelect,
        /// Dismiss git status modal
        GitStatusDismiss,
    ]
);

// Selection actions
actions!(
    stoat,
    [
        /// Select next symbol (identifier, keyword, or literal)
        SelectNextSymbol,
        /// Select previous symbol (identifier, keyword, or literal)
        SelectPrevSymbol,
        /// Select next token (including punctuation and operators)
        SelectNextToken,
        /// Select previous token (including punctuation and operators)
        SelectPrevToken,
        /// Extend selection left by one character
        SelectLeft,
        /// Extend selection right by one character
        SelectRight,
        /// Extend selection up by one line
        SelectUp,
        /// Extend selection down by one line
        SelectDown,
        /// Extend selection to start of line
        SelectToLineStart,
        /// Extend selection to end of line
        SelectToLineEnd,
    ]
);

// Pane management actions
actions!(
    stoat,
    [
        /// Split the active pane upward
        SplitUp,
        /// Split the active pane downward
        SplitDown,
        /// Split the active pane to the left
        SplitLeft,
        /// Split the active pane to the right
        SplitRight,
        /// Close the active pane
        ClosePane,
        /// Focus the pane above the current one
        FocusPaneUp,
        /// Focus the pane below the current one
        FocusPaneDown,
        /// Focus the pane to the left of the current one
        FocusPaneLeft,
        /// Focus the pane to the right of the current one
        FocusPaneRight,
    ]
);

// Application actions
actions!(
    stoat,
    [
        /// Exit the application
        ExitApp,
    ]
);

// Scroll actions - Scroll has data so defined below with #[derive(Action)]

/// Insert text action data
#[derive(Clone, PartialEq, Action)]
#[action(no_json)]
pub struct InsertText(pub String);

/// Scroll action data
#[derive(Clone, PartialEq, Action)]
#[action(no_json)]
pub struct Scroll {
    /// Scroll delta (pixels)
    pub delta: gpui::Point<f32>,
    /// Whether this is fast scroll (e.g., from trackpad)
    pub fast_scroll: bool,
}

// ==== Action Metadata System ====

/// Metadata for actions used in command palette and help display.
pub trait ActionMetadata {
    /// The canonical name of the action (e.g., "Move Left").
    fn action_name() -> &'static str;

    /// Compact help text for bottom help modal (e.g., "move left").
    fn help_text() -> &'static str;

    /// Detailed description for command palette (1-2 sentences).
    fn description() -> &'static str;
}

/// Helper macro to implement [`ActionMetadata`] for an action type.
macro_rules! action_metadata {
    ($type:ty, $help:expr, $desc:expr) => {
        impl ActionMetadata for $type {
            fn action_name() -> &'static str {
                stringify!($type)
            }

            fn help_text() -> &'static str {
                $help
            }

            fn description() -> &'static str {
                $desc
            }
        }
    };
}

// Implement ActionMetadata for all actions

// Movement actions
action_metadata!(
    MoveLeft,
    "move left",
    "Move the cursor one character to the left"
);
action_metadata!(
    MoveRight,
    "move right",
    "Move the cursor one character to the right"
);
action_metadata!(MoveUp, "move up", "Move the cursor up one line");
action_metadata!(MoveDown, "move down", "Move the cursor down one line");
action_metadata!(
    MoveToLineStart,
    "line start",
    "Move the cursor to the beginning of the current line"
);
action_metadata!(
    MoveToLineEnd,
    "line end",
    "Move the cursor to the end of the current line"
);
action_metadata!(
    MoveToFileStart,
    "file start",
    "Move the cursor to the beginning of the file"
);
action_metadata!(
    MoveToFileEnd,
    "file end",
    "Move the cursor to the end of the file"
);
action_metadata!(
    MoveWordLeft,
    "word left",
    "Move the cursor left by one word"
);
action_metadata!(
    MoveWordRight,
    "word right",
    "Move the cursor right by one word"
);
action_metadata!(PageUp, "page up", "Scroll up one page");
action_metadata!(PageDown, "page down", "Scroll down one page");

// Editing actions
action_metadata!(
    DeleteLeft,
    "delete left",
    "Delete the character before the cursor"
);
action_metadata!(
    DeleteRight,
    "delete right",
    "Delete the character after the cursor"
);
action_metadata!(
    DeleteWordLeft,
    "delete word left",
    "Delete the word before the cursor"
);
action_metadata!(
    DeleteWordRight,
    "delete word right",
    "Delete the word after the cursor"
);
action_metadata!(
    NewLine,
    "new line",
    "Insert a newline at the cursor position"
);
action_metadata!(DeleteLine, "delete line", "Delete the current line");
action_metadata!(
    DeleteToEndOfLine,
    "delete to end",
    "Delete from cursor to the end of the line"
);

// Mode actions
action_metadata!(
    EnterInsertMode,
    "insert mode",
    "Enter insert mode for editing text"
);
action_metadata!(
    EnterNormalMode,
    "normal mode",
    "Enter normal mode for navigation"
);
action_metadata!(
    EnterVisualMode,
    "visual mode",
    "Enter visual mode for selection"
);
action_metadata!(
    EnterSpaceMode,
    "space mode",
    "Enter space mode (leader key for commands)"
);
action_metadata!(
    EnterPaneMode,
    "pane mode",
    "Enter pane mode for window management"
);

// Selection actions
action_metadata!(
    SelectNextSymbol,
    "select next symbol",
    "Select the next symbol (identifier, keyword, or literal)"
);
action_metadata!(
    SelectPrevSymbol,
    "select prev symbol",
    "Select the previous symbol (identifier, keyword, or literal)"
);
action_metadata!(
    SelectNextToken,
    "select next token",
    "Select the next token (including punctuation and operators)"
);
action_metadata!(
    SelectPrevToken,
    "select prev token",
    "Select the previous token (including punctuation and operators)"
);
action_metadata!(
    SelectLeft,
    "select left",
    "Extend selection left by one character"
);
action_metadata!(
    SelectRight,
    "select right",
    "Extend selection right by one character"
);
action_metadata!(SelectUp, "select up", "Extend selection up by one line");
action_metadata!(
    SelectDown,
    "select down",
    "Extend selection down by one line"
);
action_metadata!(
    SelectToLineStart,
    "select to line start",
    "Extend selection to the beginning of the line"
);
action_metadata!(
    SelectToLineEnd,
    "select to line end",
    "Extend selection to the end of the line"
);

// File finder actions
action_metadata!(
    OpenFileFinder,
    "file finder",
    "Open the file finder to quickly navigate to files"
);
action_metadata!(
    FileFinderNext,
    "next file",
    "Move to the next file in the file finder list"
);
action_metadata!(
    FileFinderPrev,
    "prev file",
    "Move to the previous file in the file finder list"
);
action_metadata!(
    FileFinderSelect,
    "select file",
    "Open the currently selected file from the file finder"
);
action_metadata!(
    FileFinderDismiss,
    "dismiss finder",
    "Close the file finder without opening a file"
);

// Command palette actions
action_metadata!(
    OpenCommandPalette,
    "command palette",
    "Open the command palette to search for commands"
);
action_metadata!(
    CommandPaletteNext,
    "next command",
    "Move to the next command in the command palette"
);
action_metadata!(
    CommandPalettePrev,
    "prev command",
    "Move to the previous command in the command palette"
);
action_metadata!(
    CommandPaletteExecute,
    "execute command",
    "Execute the currently selected command from the palette"
);
action_metadata!(
    CommandPaletteDismiss,
    "dismiss palette",
    "Close the command palette without executing a command"
);

// Git status actions
action_metadata!(
    OpenGitStatus,
    "git status",
    "Open git status modal to view modified files"
);
action_metadata!(
    GitStatusNext,
    "next file",
    "Move to the next file in the git status list"
);
action_metadata!(
    GitStatusPrev,
    "prev file",
    "Move to the previous file in the git status list"
);
action_metadata!(
    GitStatusSelect,
    "select file",
    "Open the currently selected file from git status"
);
action_metadata!(
    GitStatusDismiss,
    "dismiss status",
    "Close the git status modal without opening a file"
);

// Pane management actions
action_metadata!(
    SplitRight,
    "split right",
    "Split the current pane vertically, creating a new empty pane to the right"
);
action_metadata!(
    SplitDown,
    "split down",
    "Split the current pane horizontally, creating a new empty pane below"
);
action_metadata!(
    SplitLeft,
    "split left",
    "Split the current pane vertically, creating a new empty pane to the left"
);
action_metadata!(
    SplitUp,
    "split up",
    "Split the current pane horizontally, creating a new empty pane above"
);
action_metadata!(
    ClosePane,
    "close pane",
    "Close the currently focused pane and remove it from the layout"
);
action_metadata!(
    FocusPaneLeft,
    "focus left",
    "Move keyboard focus to the pane immediately to the left of the current pane"
);
action_metadata!(
    FocusPaneRight,
    "focus right",
    "Move keyboard focus to the pane immediately to the right of the current pane"
);
action_metadata!(
    FocusPaneUp,
    "focus up",
    "Move keyboard focus to the pane immediately above the current pane"
);
action_metadata!(
    FocusPaneDown,
    "focus down",
    "Move keyboard focus to the pane immediately below the current pane"
);

// Application actions
action_metadata!(ExitApp, "exit", "Exit the application");

// Static maps for looking up action metadata by TypeId

/// Map from TypeId to action name
pub static ACTION_NAMES: LazyLock<HashMap<TypeId, &'static str>> = LazyLock::new(|| {
    let mut names = HashMap::new();

    // Movement actions
    names.insert(TypeId::of::<MoveLeft>(), MoveLeft::action_name());
    names.insert(TypeId::of::<MoveRight>(), MoveRight::action_name());
    names.insert(TypeId::of::<MoveUp>(), MoveUp::action_name());
    names.insert(TypeId::of::<MoveDown>(), MoveDown::action_name());
    names.insert(
        TypeId::of::<MoveToLineStart>(),
        MoveToLineStart::action_name(),
    );
    names.insert(TypeId::of::<MoveToLineEnd>(), MoveToLineEnd::action_name());
    names.insert(
        TypeId::of::<MoveToFileStart>(),
        MoveToFileStart::action_name(),
    );
    names.insert(TypeId::of::<MoveToFileEnd>(), MoveToFileEnd::action_name());
    names.insert(TypeId::of::<MoveWordLeft>(), MoveWordLeft::action_name());
    names.insert(TypeId::of::<MoveWordRight>(), MoveWordRight::action_name());
    names.insert(TypeId::of::<PageUp>(), PageUp::action_name());
    names.insert(TypeId::of::<PageDown>(), PageDown::action_name());

    // Editing actions
    names.insert(TypeId::of::<DeleteLeft>(), DeleteLeft::action_name());
    names.insert(TypeId::of::<DeleteRight>(), DeleteRight::action_name());
    names.insert(
        TypeId::of::<DeleteWordLeft>(),
        DeleteWordLeft::action_name(),
    );
    names.insert(
        TypeId::of::<DeleteWordRight>(),
        DeleteWordRight::action_name(),
    );
    names.insert(TypeId::of::<NewLine>(), NewLine::action_name());
    names.insert(TypeId::of::<DeleteLine>(), DeleteLine::action_name());
    names.insert(
        TypeId::of::<DeleteToEndOfLine>(),
        DeleteToEndOfLine::action_name(),
    );

    // Mode actions
    names.insert(
        TypeId::of::<EnterInsertMode>(),
        EnterInsertMode::action_name(),
    );
    names.insert(
        TypeId::of::<EnterNormalMode>(),
        EnterNormalMode::action_name(),
    );
    names.insert(
        TypeId::of::<EnterVisualMode>(),
        EnterVisualMode::action_name(),
    );
    names.insert(
        TypeId::of::<EnterSpaceMode>(),
        EnterSpaceMode::action_name(),
    );
    names.insert(TypeId::of::<EnterPaneMode>(), EnterPaneMode::action_name());

    // Selection actions
    names.insert(
        TypeId::of::<SelectNextSymbol>(),
        SelectNextSymbol::action_name(),
    );
    names.insert(
        TypeId::of::<SelectPrevSymbol>(),
        SelectPrevSymbol::action_name(),
    );
    names.insert(
        TypeId::of::<SelectNextToken>(),
        SelectNextToken::action_name(),
    );
    names.insert(
        TypeId::of::<SelectPrevToken>(),
        SelectPrevToken::action_name(),
    );
    names.insert(TypeId::of::<SelectLeft>(), SelectLeft::action_name());
    names.insert(TypeId::of::<SelectRight>(), SelectRight::action_name());
    names.insert(TypeId::of::<SelectUp>(), SelectUp::action_name());
    names.insert(TypeId::of::<SelectDown>(), SelectDown::action_name());
    names.insert(
        TypeId::of::<SelectToLineStart>(),
        SelectToLineStart::action_name(),
    );
    names.insert(
        TypeId::of::<SelectToLineEnd>(),
        SelectToLineEnd::action_name(),
    );

    // File finder actions
    names.insert(
        TypeId::of::<OpenFileFinder>(),
        OpenFileFinder::action_name(),
    );
    names.insert(
        TypeId::of::<FileFinderNext>(),
        FileFinderNext::action_name(),
    );
    names.insert(
        TypeId::of::<FileFinderPrev>(),
        FileFinderPrev::action_name(),
    );
    names.insert(
        TypeId::of::<FileFinderSelect>(),
        FileFinderSelect::action_name(),
    );
    names.insert(
        TypeId::of::<FileFinderDismiss>(),
        FileFinderDismiss::action_name(),
    );

    // Command palette actions
    names.insert(
        TypeId::of::<OpenCommandPalette>(),
        OpenCommandPalette::action_name(),
    );
    names.insert(
        TypeId::of::<CommandPaletteNext>(),
        CommandPaletteNext::action_name(),
    );
    names.insert(
        TypeId::of::<CommandPalettePrev>(),
        CommandPalettePrev::action_name(),
    );
    names.insert(
        TypeId::of::<CommandPaletteExecute>(),
        CommandPaletteExecute::action_name(),
    );
    names.insert(
        TypeId::of::<CommandPaletteDismiss>(),
        CommandPaletteDismiss::action_name(),
    );

    // Git status actions
    names.insert(TypeId::of::<OpenGitStatus>(), OpenGitStatus::action_name());
    names.insert(TypeId::of::<GitStatusNext>(), GitStatusNext::action_name());
    names.insert(TypeId::of::<GitStatusPrev>(), GitStatusPrev::action_name());
    names.insert(
        TypeId::of::<GitStatusSelect>(),
        GitStatusSelect::action_name(),
    );
    names.insert(
        TypeId::of::<GitStatusDismiss>(),
        GitStatusDismiss::action_name(),
    );

    // Pane management actions
    names.insert(TypeId::of::<SplitUp>(), SplitUp::action_name());
    names.insert(TypeId::of::<SplitDown>(), SplitDown::action_name());
    names.insert(TypeId::of::<SplitLeft>(), SplitLeft::action_name());
    names.insert(TypeId::of::<SplitRight>(), SplitRight::action_name());
    names.insert(TypeId::of::<ClosePane>(), ClosePane::action_name());
    names.insert(TypeId::of::<FocusPaneUp>(), FocusPaneUp::action_name());
    names.insert(TypeId::of::<FocusPaneDown>(), FocusPaneDown::action_name());
    names.insert(TypeId::of::<FocusPaneLeft>(), FocusPaneLeft::action_name());
    names.insert(
        TypeId::of::<FocusPaneRight>(),
        FocusPaneRight::action_name(),
    );

    // Application actions
    names.insert(TypeId::of::<ExitApp>(), ExitApp::action_name());

    names
});

/// Map from TypeId to action description
pub static DESCRIPTIONS: LazyLock<HashMap<TypeId, &'static str>> = LazyLock::new(|| {
    let mut descriptions = HashMap::new();

    // Movement actions
    descriptions.insert(TypeId::of::<MoveLeft>(), MoveLeft::description());
    descriptions.insert(TypeId::of::<MoveRight>(), MoveRight::description());
    descriptions.insert(TypeId::of::<MoveUp>(), MoveUp::description());
    descriptions.insert(TypeId::of::<MoveDown>(), MoveDown::description());
    descriptions.insert(
        TypeId::of::<MoveToLineStart>(),
        MoveToLineStart::description(),
    );
    descriptions.insert(TypeId::of::<MoveToLineEnd>(), MoveToLineEnd::description());
    descriptions.insert(
        TypeId::of::<MoveToFileStart>(),
        MoveToFileStart::description(),
    );
    descriptions.insert(TypeId::of::<MoveToFileEnd>(), MoveToFileEnd::description());
    descriptions.insert(TypeId::of::<MoveWordLeft>(), MoveWordLeft::description());
    descriptions.insert(TypeId::of::<MoveWordRight>(), MoveWordRight::description());
    descriptions.insert(TypeId::of::<PageUp>(), PageUp::description());
    descriptions.insert(TypeId::of::<PageDown>(), PageDown::description());

    // Editing actions
    descriptions.insert(TypeId::of::<DeleteLeft>(), DeleteLeft::description());
    descriptions.insert(TypeId::of::<DeleteRight>(), DeleteRight::description());
    descriptions.insert(
        TypeId::of::<DeleteWordLeft>(),
        DeleteWordLeft::description(),
    );
    descriptions.insert(
        TypeId::of::<DeleteWordRight>(),
        DeleteWordRight::description(),
    );
    descriptions.insert(TypeId::of::<NewLine>(), NewLine::description());
    descriptions.insert(TypeId::of::<DeleteLine>(), DeleteLine::description());
    descriptions.insert(
        TypeId::of::<DeleteToEndOfLine>(),
        DeleteToEndOfLine::description(),
    );

    // Mode actions
    descriptions.insert(
        TypeId::of::<EnterInsertMode>(),
        EnterInsertMode::description(),
    );
    descriptions.insert(
        TypeId::of::<EnterNormalMode>(),
        EnterNormalMode::description(),
    );
    descriptions.insert(
        TypeId::of::<EnterVisualMode>(),
        EnterVisualMode::description(),
    );
    descriptions.insert(
        TypeId::of::<EnterSpaceMode>(),
        EnterSpaceMode::description(),
    );
    descriptions.insert(TypeId::of::<EnterPaneMode>(), EnterPaneMode::description());

    // Selection actions
    descriptions.insert(
        TypeId::of::<SelectNextSymbol>(),
        SelectNextSymbol::description(),
    );
    descriptions.insert(
        TypeId::of::<SelectPrevSymbol>(),
        SelectPrevSymbol::description(),
    );
    descriptions.insert(
        TypeId::of::<SelectNextToken>(),
        SelectNextToken::description(),
    );
    descriptions.insert(
        TypeId::of::<SelectPrevToken>(),
        SelectPrevToken::description(),
    );
    descriptions.insert(TypeId::of::<SelectLeft>(), SelectLeft::description());
    descriptions.insert(TypeId::of::<SelectRight>(), SelectRight::description());
    descriptions.insert(TypeId::of::<SelectUp>(), SelectUp::description());
    descriptions.insert(TypeId::of::<SelectDown>(), SelectDown::description());
    descriptions.insert(
        TypeId::of::<SelectToLineStart>(),
        SelectToLineStart::description(),
    );
    descriptions.insert(
        TypeId::of::<SelectToLineEnd>(),
        SelectToLineEnd::description(),
    );

    // File finder actions
    descriptions.insert(
        TypeId::of::<OpenFileFinder>(),
        OpenFileFinder::description(),
    );
    descriptions.insert(
        TypeId::of::<FileFinderNext>(),
        FileFinderNext::description(),
    );
    descriptions.insert(
        TypeId::of::<FileFinderPrev>(),
        FileFinderPrev::description(),
    );
    descriptions.insert(
        TypeId::of::<FileFinderSelect>(),
        FileFinderSelect::description(),
    );
    descriptions.insert(
        TypeId::of::<FileFinderDismiss>(),
        FileFinderDismiss::description(),
    );

    // Command palette actions
    descriptions.insert(
        TypeId::of::<OpenCommandPalette>(),
        OpenCommandPalette::description(),
    );
    descriptions.insert(
        TypeId::of::<CommandPaletteNext>(),
        CommandPaletteNext::description(),
    );
    descriptions.insert(
        TypeId::of::<CommandPalettePrev>(),
        CommandPalettePrev::description(),
    );
    descriptions.insert(
        TypeId::of::<CommandPaletteExecute>(),
        CommandPaletteExecute::description(),
    );
    descriptions.insert(
        TypeId::of::<CommandPaletteDismiss>(),
        CommandPaletteDismiss::description(),
    );

    // Git status actions
    descriptions.insert(TypeId::of::<OpenGitStatus>(), OpenGitStatus::description());
    descriptions.insert(TypeId::of::<GitStatusNext>(), GitStatusNext::description());
    descriptions.insert(TypeId::of::<GitStatusPrev>(), GitStatusPrev::description());
    descriptions.insert(
        TypeId::of::<GitStatusSelect>(),
        GitStatusSelect::description(),
    );
    descriptions.insert(
        TypeId::of::<GitStatusDismiss>(),
        GitStatusDismiss::description(),
    );

    // Pane management actions
    descriptions.insert(TypeId::of::<SplitUp>(), SplitUp::description());
    descriptions.insert(TypeId::of::<SplitDown>(), SplitDown::description());
    descriptions.insert(TypeId::of::<SplitLeft>(), SplitLeft::description());
    descriptions.insert(TypeId::of::<SplitRight>(), SplitRight::description());
    descriptions.insert(TypeId::of::<ClosePane>(), ClosePane::description());
    descriptions.insert(TypeId::of::<FocusPaneUp>(), FocusPaneUp::description());
    descriptions.insert(TypeId::of::<FocusPaneDown>(), FocusPaneDown::description());
    descriptions.insert(TypeId::of::<FocusPaneLeft>(), FocusPaneLeft::description());
    descriptions.insert(
        TypeId::of::<FocusPaneRight>(),
        FocusPaneRight::description(),
    );

    // Application actions
    descriptions.insert(TypeId::of::<ExitApp>(), ExitApp::description());

    descriptions
});

/// Get the action name for a given action.
pub fn action_name(action: &dyn Action) -> Option<&'static str> {
    ACTION_NAMES.get(&action.type_id()).copied()
}

/// Get the description for a given action.
pub fn description(action: &dyn Action) -> Option<&'static str> {
    DESCRIPTIONS.get(&action.type_id()).copied()
}

/// Map from TypeId to action help text
pub static HELP_TEXT: LazyLock<HashMap<TypeId, &'static str>> = LazyLock::new(|| {
    let mut help = HashMap::new();

    // Movement actions
    help.insert(TypeId::of::<MoveLeft>(), MoveLeft::help_text());
    help.insert(TypeId::of::<MoveRight>(), MoveRight::help_text());
    help.insert(TypeId::of::<MoveUp>(), MoveUp::help_text());
    help.insert(TypeId::of::<MoveDown>(), MoveDown::help_text());
    help.insert(
        TypeId::of::<MoveToLineStart>(),
        MoveToLineStart::help_text(),
    );
    help.insert(TypeId::of::<MoveToLineEnd>(), MoveToLineEnd::help_text());
    help.insert(
        TypeId::of::<MoveToFileStart>(),
        MoveToFileStart::help_text(),
    );
    help.insert(TypeId::of::<MoveToFileEnd>(), MoveToFileEnd::help_text());
    help.insert(TypeId::of::<MoveWordLeft>(), MoveWordLeft::help_text());
    help.insert(TypeId::of::<MoveWordRight>(), MoveWordRight::help_text());
    help.insert(TypeId::of::<PageUp>(), PageUp::help_text());
    help.insert(TypeId::of::<PageDown>(), PageDown::help_text());

    // Editing actions
    help.insert(TypeId::of::<DeleteLeft>(), DeleteLeft::help_text());
    help.insert(TypeId::of::<DeleteRight>(), DeleteRight::help_text());
    help.insert(TypeId::of::<DeleteWordLeft>(), DeleteWordLeft::help_text());
    help.insert(
        TypeId::of::<DeleteWordRight>(),
        DeleteWordRight::help_text(),
    );
    help.insert(TypeId::of::<NewLine>(), NewLine::help_text());
    help.insert(TypeId::of::<DeleteLine>(), DeleteLine::help_text());
    help.insert(
        TypeId::of::<DeleteToEndOfLine>(),
        DeleteToEndOfLine::help_text(),
    );

    // Mode actions
    help.insert(
        TypeId::of::<EnterInsertMode>(),
        EnterInsertMode::help_text(),
    );
    help.insert(
        TypeId::of::<EnterNormalMode>(),
        EnterNormalMode::help_text(),
    );
    help.insert(
        TypeId::of::<EnterVisualMode>(),
        EnterVisualMode::help_text(),
    );
    help.insert(TypeId::of::<EnterSpaceMode>(), EnterSpaceMode::help_text());
    help.insert(TypeId::of::<EnterPaneMode>(), EnterPaneMode::help_text());

    // Selection actions
    help.insert(
        TypeId::of::<SelectNextSymbol>(),
        SelectNextSymbol::help_text(),
    );
    help.insert(
        TypeId::of::<SelectPrevSymbol>(),
        SelectPrevSymbol::help_text(),
    );
    help.insert(
        TypeId::of::<SelectNextToken>(),
        SelectNextToken::help_text(),
    );
    help.insert(
        TypeId::of::<SelectPrevToken>(),
        SelectPrevToken::help_text(),
    );
    help.insert(TypeId::of::<SelectLeft>(), SelectLeft::help_text());
    help.insert(TypeId::of::<SelectRight>(), SelectRight::help_text());
    help.insert(TypeId::of::<SelectUp>(), SelectUp::help_text());
    help.insert(TypeId::of::<SelectDown>(), SelectDown::help_text());
    help.insert(
        TypeId::of::<SelectToLineStart>(),
        SelectToLineStart::help_text(),
    );
    help.insert(
        TypeId::of::<SelectToLineEnd>(),
        SelectToLineEnd::help_text(),
    );

    // File finder actions
    help.insert(TypeId::of::<OpenFileFinder>(), OpenFileFinder::help_text());
    help.insert(TypeId::of::<FileFinderNext>(), FileFinderNext::help_text());
    help.insert(TypeId::of::<FileFinderPrev>(), FileFinderPrev::help_text());
    help.insert(
        TypeId::of::<FileFinderSelect>(),
        FileFinderSelect::help_text(),
    );
    help.insert(
        TypeId::of::<FileFinderDismiss>(),
        FileFinderDismiss::help_text(),
    );

    // Command palette actions
    help.insert(
        TypeId::of::<OpenCommandPalette>(),
        OpenCommandPalette::help_text(),
    );
    help.insert(
        TypeId::of::<CommandPaletteNext>(),
        CommandPaletteNext::help_text(),
    );
    help.insert(
        TypeId::of::<CommandPalettePrev>(),
        CommandPalettePrev::help_text(),
    );
    help.insert(
        TypeId::of::<CommandPaletteExecute>(),
        CommandPaletteExecute::help_text(),
    );
    help.insert(
        TypeId::of::<CommandPaletteDismiss>(),
        CommandPaletteDismiss::help_text(),
    );

    // Git status actions
    help.insert(TypeId::of::<OpenGitStatus>(), OpenGitStatus::help_text());
    help.insert(TypeId::of::<GitStatusNext>(), GitStatusNext::help_text());
    help.insert(TypeId::of::<GitStatusPrev>(), GitStatusPrev::help_text());
    help.insert(
        TypeId::of::<GitStatusSelect>(),
        GitStatusSelect::help_text(),
    );
    help.insert(
        TypeId::of::<GitStatusDismiss>(),
        GitStatusDismiss::help_text(),
    );

    // Pane management actions
    help.insert(TypeId::of::<SplitUp>(), SplitUp::help_text());
    help.insert(TypeId::of::<SplitDown>(), SplitDown::help_text());
    help.insert(TypeId::of::<SplitLeft>(), SplitLeft::help_text());
    help.insert(TypeId::of::<SplitRight>(), SplitRight::help_text());
    help.insert(TypeId::of::<ClosePane>(), ClosePane::help_text());
    help.insert(TypeId::of::<FocusPaneUp>(), FocusPaneUp::help_text());
    help.insert(TypeId::of::<FocusPaneDown>(), FocusPaneDown::help_text());
    help.insert(TypeId::of::<FocusPaneLeft>(), FocusPaneLeft::help_text());
    help.insert(TypeId::of::<FocusPaneRight>(), FocusPaneRight::help_text());

    // Application actions
    help.insert(TypeId::of::<ExitApp>(), ExitApp::help_text());

    help
});

/// Get the help text for a given action.
pub fn help_text(action: &dyn Action) -> Option<&'static str> {
    HELP_TEXT.get(&action.type_id()).copied()
}
