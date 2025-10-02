//! Action definitions for Stoat editor
//!
//! Actions are the commands that can be executed in the editor. They integrate with GPUI's
//! action system, making them keyboard-bindable, discoverable, and testable.
//!
//! Actions are organized into namespaces by functionality:
//! - [`editor_movement`]: Cursor navigation actions
//! - [`editor_edit`]: Text modification actions
//! - [`editor_modal`]: Mode transitions and modal operations
//! - [`editor_file`]: File operations
//! - [`editor_selection`]: Text selection actions
//!
//! # Submodules
//!
//! - [`selection`]: Symbol-based selection operations

mod selection;

use crate::ScrollDelta;
use gpui::{actions, Action, Pixels, Point};

/// Insert text at the current cursor position(s).
///
/// This is the primary text input action for the editor. It handles single characters,
/// multi-character input from IME systems, and paste operations. The action is typically
/// triggered during insert mode or when text is pasted in any mode.
///
/// # Context
/// This action is dispatched by the input system when text needs to be inserted. It
/// interacts with [`crate::Stoat`] to insert text at cursor positions and update the
/// buffer accordingly.
#[derive(Clone, PartialEq, Action)]
#[action(no_json)]
pub struct InsertText(pub String);

/// Handle scroll events from mouse wheel or trackpad.
///
/// This action processes scroll input and updates the viewport position. It supports both
/// discrete mouse wheel scrolling and smooth trackpad gestures. The [`fast_scroll`] flag
/// enables accelerated scrolling when modifier keys are held.
///
/// # Context
/// Dispatched by the GUI layer when scroll events occur. The action is processed by
/// [`crate::Stoat`]'s scroll management system to update the visible viewport.
#[derive(Clone, PartialEq, Action)]
#[action(no_json)]
pub struct HandleScroll {
    /// Mouse position when the scroll event occurred
    pub position: Point<Pixels>,
    /// Scroll amount and direction
    pub delta: ScrollDelta,
    /// Whether Alt key was held during scroll for fast scrolling
    pub fast_scroll: bool,
}

// Movement actions - basic cursor navigation
actions!(
    editor_movement,
    [
        /// Move cursor left by one character.
        ///
        /// Moves the primary cursor one character to the left. In normal mode, this stops at
        /// the beginning of the line. In insert mode, it can move to the previous line.
        MoveLeft,
        /// Move cursor right by one character.
        ///
        /// Moves the primary cursor one character to the right. Behavior at line endings
        /// depends on the current mode.
        MoveRight,
        /// Move cursor up by one line.
        ///
        /// Moves the primary cursor up by one line, maintaining the column position when
        /// possible. At the first line, this has no effect.
        MoveUp,
        /// Move cursor down by one line.
        ///
        /// Moves the primary cursor down by one line, maintaining the column position when
        /// possible. At the last line, this has no effect.
        MoveDown,
        /// Move cursor left by one word.
        ///
        /// Jumps the cursor to the beginning of the previous word, using language-aware word
        /// boundary detection.
        MoveWordLeft,
        /// Move cursor right by one word.
        ///
        /// Jumps the cursor to the beginning of the next word, using language-aware word
        /// boundary detection.
        MoveWordRight,
        /// Move cursor to the start of the current line.
        ///
        /// Positions the cursor at the first character of the current line.
        MoveToLineStart,
        /// Move cursor to the end of the current line.
        ///
        /// Positions the cursor after the last character of the current line.
        MoveToLineEnd,
        /// Move cursor to the start of the file.
        ///
        /// Jumps to the beginning of the buffer (line 0, column 0).
        MoveToFileStart,
        /// Move cursor to the end of the file.
        ///
        /// Jumps to the end of the buffer after the last character.
        MoveToFileEnd,
        /// Move cursor up by one page.
        ///
        /// Scrolls the viewport up by one page and moves the cursor accordingly. The page
        /// size is determined by the current viewport height.
        PageUp,
        /// Move cursor down by one page.
        ///
        /// Scrolls the viewport down by one page and moves the cursor accordingly. The page
        /// size is determined by the current viewport height.
        PageDown,
    ]
);

// Editing actions - text modification operations
actions!(
    editor_edit,
    [
        /// Delete the character to the left of the cursor.
        ///
        /// Standard backspace operation. Deletes the character immediately before the cursor,
        /// or deletes the selected text if there is a selection.
        DeleteLeft,
        /// Delete the character to the right of the cursor.
        ///
        /// Standard delete operation. Deletes the character immediately after the cursor,
        /// or deletes the selected text if there is a selection.
        DeleteRight,
        /// Delete the word to the left of the cursor.
        ///
        /// Deletes from the cursor position back to the beginning of the current word.
        DeleteWordLeft,
        /// Delete the word to the right of the cursor.
        ///
        /// Deletes from the cursor position forward to the end of the current word.
        DeleteWordRight,
        /// Delete the current line.
        ///
        /// Removes the entire line where the cursor is positioned, including the newline.
        DeleteLine,
        /// Delete from cursor to end of line.
        ///
        /// Removes all text from the cursor position to the end of the current line,
        /// preserving the newline.
        DeleteToEndOfLine,
        /// Insert a newline character.
        ///
        /// Creates a new line at the cursor position. Behavior may include auto-indentation
        /// based on the current language and context.
        NewLine,
        /// Undo the last change.
        ///
        /// Reverts the most recent modification to the buffer. Multiple undo operations
        /// walk back through the edit history.
        Undo,
        /// Redo the last undone change.
        ///
        /// Re-applies a change that was previously undone. Only available when there are
        /// undone operations in the history.
        Redo,
        /// Copy selected text to clipboard.
        ///
        /// Copies the current selection to the system clipboard. Has no effect if there
        /// is no selection.
        Copy,
        /// Cut selected text to clipboard.
        ///
        /// Copies the current selection to the system clipboard and removes it from the
        /// buffer. Has no effect if there is no selection.
        Cut,
        /// Paste text from clipboard.
        ///
        /// Inserts text from the system clipboard at the cursor position, or replaces the
        /// current selection.
        Paste,
        /// Indent the current line or selection.
        ///
        /// Increases indentation of the current line or all lines in the selection by one
        /// level. Indentation size depends on language configuration.
        Indent,
        /// Outdent the current line or selection.
        ///
        /// Decreases indentation of the current line or all lines in the selection by one
        /// level. Has no effect if already at zero indentation.
        Outdent,
    ]
);

// Modal actions - mode transitions and modal-specific operations
actions!(
    editor_modal,
    [
        /// Enter Insert mode for text input.
        ///
        /// Transitions from Normal or Visual mode to Insert mode, allowing direct text entry.
        /// In Insert mode, most keypresses insert characters rather than triggering commands.
        EnterInsertMode,
        /// Enter Normal mode for command input.
        ///
        /// Transitions to Normal mode, the default mode for navigation and commands. In
        /// Normal mode, key presses trigger actions rather than inserting text.
        EnterNormalMode,
        /// Enter Visual mode for text selection.
        ///
        /// Transitions to Visual mode for selecting text. Movement commands extend the
        /// selection rather than moving the cursor.
        EnterVisualMode,
        /// Exit the application.
        ///
        /// Closes the editor. If there are unsaved changes, a confirmation prompt may be
        /// displayed depending on configuration.
        ExitApp,
    ]
);

// File actions - file operations
actions!(
    editor_file,
    [
        /// Save the current file.
        ///
        /// Writes the buffer contents to disk. If the buffer has no associated file path,
        /// this behaves like [`SaveAs`].
        Save,
        /// Save the current file with a new name.
        ///
        /// Prompts for a file path and writes the buffer contents to the specified location.
        SaveAs,
        /// Open a file.
        ///
        /// Displays a file picker dialog to select a file to open in the editor.
        Open,
        /// Quit the editor.
        ///
        /// Closes the editor with save prompts for modified buffers.
        Quit,
        /// Force quit without saving.
        ///
        /// Immediately closes the editor, discarding any unsaved changes.
        ForceQuit,
    ]
);

// Selection actions - text selection operations
actions!(
    editor_selection,
    [
        /// Select all text in the buffer.
        ///
        /// Creates a selection spanning the entire buffer from start to end.
        SelectAll,
        /// Clear the current selection.
        ///
        /// Removes the active selection, leaving only the cursor.
        ClearSelection,
        /// Select the current line.
        ///
        /// Creates a selection spanning the entire line where the cursor is positioned.
        SelectLine,
        /// Extend selection left by one character.
        ///
        /// Moves the selection end point left by one character, extending or shrinking the
        /// selection.
        SelectLeft,
        /// Extend selection right by one character.
        ///
        /// Moves the selection end point right by one character, extending or shrinking the
        /// selection.
        SelectRight,
        /// Extend selection up by one line.
        ///
        /// Moves the selection end point up by one line, extending or shrinking the selection.
        SelectUp,
        /// Extend selection down by one line.
        ///
        /// Moves the selection end point down by one line, extending or shrinking the
        /// selection.
        SelectDown,
        /// Extend selection left by one word.
        ///
        /// Moves the selection end point to the beginning of the previous word.
        SelectWordLeft,
        /// Extend selection right by one word.
        ///
        /// Moves the selection end point to the beginning of the next word.
        SelectWordRight,
        /// Extend selection to start of line.
        ///
        /// Extends the selection to the beginning of the current line.
        SelectToLineStart,
        /// Extend selection to end of line.
        ///
        /// Extends the selection to the end of the current line.
        SelectToLineEnd,
    ]
);
