//! Action definitions for stoat_v4.
//!
//! Actions are dispatched through GPUI's action system and handled by [`crate::Stoat`].

use gpui::{actions, Action};

// Editing actions
actions!(
    stoat_v4,
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
    stoat_v4,
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
    stoat_v4,
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
    stoat_v4,
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
    stoat_v4,
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

// Selection actions
actions!(
    stoat_v4,
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

// Application actions
actions!(
    stoat_v4,
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
