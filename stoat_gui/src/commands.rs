//! Command system for Stoat editor
//!
//! This module defines all commands that can be executed in the editor.
//! Commands are implemented as GPUI actions, making them discoverable,
//! bindable to keys, and testable in isolation.

use gpui::{actions, Action, Pixels, Point};
use stoat::ScrollDelta;

/// Insert text at the current cursor position(s).
///
/// This is the primary text input command, similar to Zed's [`HandleInput`].
/// It handles single characters, multi-character input from IME, and paste operations.
#[derive(Clone, PartialEq, Action)]
#[action(no_json)]
pub struct InsertText(pub String);

/// Handle scroll events from mouse wheel or trackpad
///
/// This command processes scroll input and updates the viewport position.
/// It supports both discrete wheel scrolling and smooth trackpad gestures.
#[derive(Clone, PartialEq, Action)]
#[action(no_json)]
pub struct HandleScroll {
    /// Mouse position when scroll occurred
    pub position: Point<Pixels>,
    /// Scroll amount and direction
    pub delta: ScrollDelta,
    /// Whether Alt key was held (for fast scrolling)
    pub fast_scroll: bool,
}

// Movement commands - basic cursor navigation
actions!(
    editor_movement,
    [
        /// Move cursor left by one character
        MoveLeft,
        /// Move cursor right by one character
        MoveRight,
        /// Move cursor up by one line
        MoveUp,
        /// Move cursor down by one line
        MoveDown,
        /// Move cursor left by one word
        MoveWordLeft,
        /// Move cursor right by one word
        MoveWordRight,
        /// Move cursor to the start of the current line
        MoveToLineStart,
        /// Move cursor to the end of the current line
        MoveToLineEnd,
        /// Move cursor to the start of the file
        MoveToFileStart,
        /// Move cursor to the end of the file
        MoveToFileEnd,
        /// Move cursor up by one page
        PageUp,
        /// Move cursor down by one page
        PageDown,
    ]
);

// Editing commands - text modification operations
actions!(
    editor_edit,
    [
        /// Delete the character to the left of the cursor (backspace)
        DeleteLeft,
        /// Delete the character to the right of the cursor (delete)
        DeleteRight,
        /// Delete the word to the left of the cursor
        DeleteWordLeft,
        /// Delete the word to the right of the cursor
        DeleteWordRight,
        /// Delete the current line
        DeleteLine,
        /// Delete from cursor to end of line
        DeleteToEndOfLine,
        /// Insert a newline character
        NewLine,
        /// Undo the last change
        Undo,
        /// Redo the last undone change
        Redo,
        /// Copy selected text to clipboard
        Copy,
        /// Cut selected text to clipboard
        Cut,
        /// Paste text from clipboard
        Paste,
        /// Indent the current line or selection
        Indent,
        /// Outdent the current line or selection
        Outdent,
    ]
);

// Modal commands - mode transitions and modal-specific operations
actions!(
    editor_modal,
    [
        /// Enter Insert mode for text input
        EnterInsertMode,
        /// Enter Normal mode for command input
        EnterNormalMode,
        /// Enter Visual mode for text selection
        EnterVisualMode,
        /// Exit the application
        ExitApp,
    ]
);

// File commands - file operations
actions!(
    editor_file,
    [
        /// Save the current file
        Save,
        /// Save the current file with a new name
        SaveAs,
        /// Open a file
        Open,
        /// Quit the editor (with save prompt if needed)
        Quit,
        /// Force quit without saving
        ForceQuit,
    ]
);

// Selection commands - text selection operations
actions!(
    editor_selection,
    [
        /// Select all text in the buffer
        SelectAll,
        /// Clear the current selection
        ClearSelection,
        /// Select the current line
        SelectLine,
        /// Extend selection left by one character
        SelectLeft,
        /// Extend selection right by one character
        SelectRight,
        /// Extend selection up by one line
        SelectUp,
        /// Extend selection down by one line
        SelectDown,
        /// Extend selection left by one word
        SelectWordLeft,
        /// Extend selection right by one word
        SelectWordRight,
        /// Extend selection to start of line
        SelectToLineStart,
        /// Extend selection to end of line
        SelectToLineEnd,
    ]
);

/// A command that can be executed by the editor.
///
/// This trait allows different types of commands to be handled uniformly
/// by the command execution system.
pub trait EditorCommand {
    /// Execute this command in the given context
    fn execute(
        &self,
        editor: &mut crate::editor::view::EditorView,
        cx: &mut gpui::Context<'_, crate::editor::view::EditorView>,
    );
}

// Implement EditorCommand for the InsertText action
impl EditorCommand for InsertText {
    fn execute(
        &self,
        _editor: &mut crate::editor::view::EditorView,
        cx: &mut gpui::Context<'_, crate::editor::view::EditorView>,
    ) {
        // Implementation will be added when we update EditorView
        tracing::info!("Executing InsertText command: '{}'", self.0);
        cx.notify();
    }
}
