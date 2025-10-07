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
//! - [`selection`]: Text selection operations (symbol and token-based)
//! - [`movement`]: Cursor navigation and movement commands
//! - [`edit`]: Text modification and deletion commands
//! - [`modal`]: Mode transition commands (Normal, Insert, Visual)
//! - [`scroll`]: Viewport scrolling operations

mod edit;
mod modal;
mod movement;
mod scroll;
mod selection;
mod shell;

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
        /// Enter Pane mode for pane management.
        ///
        /// Transitions to Pane mode for window/pane operations like splitting and navigation.
        /// In Pane mode, simple keys trigger pane commands, then returns to Normal mode.
        EnterPaneMode,
        /// Exit the application.
        ///
        /// Closes the editor. If there are unsaved changes, a confirmation prompt may be
        /// displayed depending on configuration.
        ExitApp,
    ]
);

/// Set the editor mode dynamically.
///
/// Changes to the specified mode by name. This is the generic action for all
/// mode transitions, replacing individual [`EnterInsertMode`], [`EnterNormalMode`],
/// [`EnterVisualMode`], and [`EnterPaneMode`] actions.
///
/// # Arguments
///
/// * `0` - The name of the mode to activate (e.g., "normal", "insert", "visual", "pane", "space")
///
/// # Examples
///
/// ```ignore
/// SetMode("normal".to_string())  // Enter normal mode
/// SetMode("insert".to_string())  // Enter insert mode
/// SetMode("space".to_string())   // Enter space command mode
/// ```
///
/// # Related
///
/// See also the mode-specific actions (kept for backward compatibility):
/// - [`EnterInsertMode`] - enter insert mode
/// - [`EnterNormalMode`] - enter normal mode
/// - [`EnterVisualMode`] - enter visual mode
/// - [`EnterPaneMode`] - enter pane mode
#[derive(Clone, PartialEq, Action)]
#[action(no_json)]
pub struct SetMode(pub String);

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
        /// Open the file finder.
        ///
        /// Displays a fuzzy file finder modal to quickly navigate to files in the current
        /// directory.
        OpenFileFinder,
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
        /// Select the next symbol from the cursor position.
        ///
        /// Finds and selects the next identifier, keyword, or literal, skipping whitespace,
        /// punctuation, and operators. This enables semantic navigation through code by
        /// jumping between meaningful named entities.
        ///
        /// Implemented by [`crate::actions::selection::select_next_symbol`].
        SelectNextSymbol,
        /// Select the previous symbol from the cursor position.
        ///
        /// Finds and selects the previous identifier, keyword, or literal, skipping whitespace,
        /// punctuation, and operators. This enables semantic backward navigation through code by
        /// jumping between meaningful named entities.
        ///
        /// Implemented by [`crate::actions::selection::select_prev_symbol`].
        SelectPrevSymbol,
        /// Select the next token from the cursor position.
        ///
        /// Finds and selects the next syntactic token including punctuation, operators,
        /// and brackets. This enables low-level navigation through code structure.
        ///
        /// Implemented by [`crate::actions::selection::select_next_token`].
        SelectNextToken,
        /// Select the previous token from the cursor position.
        ///
        /// Finds and selects the previous syntactic token including punctuation, operators,
        /// and brackets. This enables low-level backward navigation through code structure.
        ///
        /// Implemented by [`crate::actions::selection::select_prev_token`].
        SelectPrevToken,
    ]
);

// Shell actions - pane management and file finder
actions!(
    shell,
    [
        /// Split the active pane upward.
        ///
        /// Creates a new pane above the active pane with a vertical layout (tall panes
        /// stacked vertically). The new pane becomes active.
        SplitUp,
        /// Split the active pane downward.
        ///
        /// Creates a new pane below the active pane with a vertical layout (tall panes
        /// stacked vertically). The new pane becomes active.
        SplitDown,
        /// Split the active pane to the left.
        ///
        /// Creates a new pane to the left of the active pane with a horizontal layout
        /// (wide panes side-by-side). The new pane becomes active.
        SplitLeft,
        /// Split the active pane to the right.
        ///
        /// Creates a new pane to the right of the active pane with a horizontal layout
        /// (wide panes side-by-side). The new pane becomes active.
        SplitRight,
        /// Close the active pane.
        ///
        /// Removes the active pane from the layout. If this is the last remaining pane,
        /// this action has no effect. After closing, focus moves to another pane.
        ClosePane,
        /// Focus the pane above the current one.
        ///
        /// Moves focus to the pane directly above the active pane. Has no effect if there
        /// is no pane above.
        FocusPaneUp,
        /// Focus the pane below the current one.
        ///
        /// Moves focus to the pane directly below the active pane. Has no effect if there
        /// is no pane below.
        FocusPaneDown,
        /// Focus the pane to the left of the current one.
        ///
        /// Moves focus to the pane directly to the left of the active pane. Has no effect
        /// if there is no pane to the left.
        FocusPaneLeft,
        /// Focus the pane to the right of the current one.
        ///
        /// Moves focus to the pane directly to the right of the active pane. Has no effect
        /// if there is no pane to the right.
        FocusPaneRight,
        /// Move to the next file in the file finder list.
        ///
        /// In file finder mode, moves the selection highlight down to the next file in the
        /// filtered list. Wraps to the first file if at the end.
        FileFinderNext,
        /// Move to the previous file in the file finder list.
        ///
        /// In file finder mode, moves the selection highlight up to the previous file in the
        /// filtered list. Wraps to the last file if at the beginning.
        FileFinderPrev,
        /// Dismiss the file finder and return to the previous mode.
        ///
        /// Closes the file finder modal and restores the mode that was active before
        /// opening the file finder (typically Normal mode).
        FileFinderDismiss,
        /// Select the currently highlighted file in the file finder.
        ///
        /// Opens the selected file in the editor. This action is typically bound to Enter
        /// in file finder mode.
        FileFinderSelect,
        /// Open the command palette.
        ///
        /// Opens the command palette modal for fuzzy-searching and executing commands.
        /// The palette displays all available actions with their keybindings and descriptions.
        OpenCommandPalette,
        /// Move to the next command in the command palette list.
        ///
        /// In command palette mode, moves the selection highlight down to the next command
        /// in the filtered list.
        CommandPaletteNext,
        /// Move to the previous command in the command palette list.
        ///
        /// In command palette mode, moves the selection highlight up to the previous command
        /// in the filtered list.
        CommandPalettePrev,
        /// Dismiss the command palette and return to the previous mode.
        ///
        /// Closes the command palette modal and restores the mode that was active before
        /// opening the palette (typically Normal mode).
        CommandPaletteDismiss,
        /// Execute the currently highlighted command in the command palette.
        ///
        /// Dispatches the selected command's action. This action is typically bound to Enter
        /// in command palette mode.
        CommandPaletteExecute,
    ]
);

use once_cell::sync::Lazy;
use std::{any::TypeId, collections::HashMap};

/// Metadata for actions, providing display names and descriptions.
///
/// This trait is implemented for all action types to provide consistent metadata
/// for UI display, command palette integration, and help systems. It supports
/// a three-tiered description system:
/// - **help_text**: Compact 1-2 words for the bottom help modal
/// - **description**: Detailed 1-2 sentences for the command palette
/// - **documentation**: (Future) Full paragraphs for manual pages
pub trait ActionMetadata {
    /// The canonical name of the action (e.g., "MoveLeft").
    fn action_name() -> &'static str;

    /// Compact help text for the bottom help modal (e.g., "move left").
    ///
    /// This is a very brief 1-2 word description displayed in the help overlay
    /// at the bottom of the screen where horizontal space is limited.
    fn help_text() -> &'static str;

    /// Detailed description for the command palette.
    ///
    /// This is a 1-2 sentence description that provides context and explains
    /// the action's behavior. Used in the command palette where users are
    /// searching for and learning about commands.
    fn description() -> &'static str;
}

/// Helper macro to implement [`ActionMetadata`] for an action type.
///
/// This macro reduces boilerplate by implementing the ActionMetadata trait with
/// help text and description.
///
/// # Usage
/// ```ignore
/// action_metadata!(
///     MoveLeft,
///     "move left",
///     "Move the cursor one character to the left. In normal mode, stops at the beginning of the line."
/// );
/// ```
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
    "Move the cursor one character to the left. In normal mode, stops at the beginning of the line."
);
action_metadata!(
    MoveRight,
    "move right",
    "Move the cursor one character to the right. In normal mode, stops at the end of the line."
);
action_metadata!(
    MoveUp,
    "move up",
    "Move the cursor up one line, maintaining the column position when possible. Stops at the first line."
);
action_metadata!(
    MoveDown,
    "move down",
    "Move the cursor down one line, maintaining the column position when possible. Stops at the last line."
);
action_metadata!(
    MoveToLineStart,
    "line start",
    "Move the cursor to the beginning of the current line, before any indentation or content."
);
action_metadata!(
    MoveToLineEnd,
    "line end",
    "Move the cursor to the end of the current line, after all content."
);
action_metadata!(
    MoveToFileStart,
    "file start",
    "Jump to the very beginning of the file, moving the cursor to line 1, column 1."
);
action_metadata!(
    MoveToFileEnd,
    "file end",
    "Jump to the very end of the file, moving the cursor to the last line."
);
action_metadata!(
    PageUp,
    "page up",
    "Scroll up one page (viewport height) and move the cursor to maintain visibility."
);
action_metadata!(
    PageDown,
    "page down",
    "Scroll down one page (viewport height) and move the cursor to maintain visibility."
);

// Edit actions
action_metadata!(
    DeleteLeft,
    "delete left",
    "Delete the character to the left of the cursor (backspace). In insert mode, may merge lines if at line start."
);
action_metadata!(
    DeleteRight,
    "delete right",
    "Delete the character to the right of the cursor. In normal mode, does not advance the cursor position."
);
action_metadata!(
    DeleteLine,
    "delete line",
    "Delete the entire current line including its content and the line break, moving subsequent lines up."
);
action_metadata!(
    DeleteToEndOfLine,
    "delete to end",
    "Delete all text from the cursor position to the end of the current line, preserving the line break."
);

// Mode transition actions
action_metadata!(
    EnterInsertMode,
    "insert mode",
    "Switch to insert mode where you can type and edit text freely. Press Escape to return to normal mode."
);
action_metadata!(
    EnterNormalMode,
    "normal mode",
    "Return to normal mode for navigation, commands, and modal editing. This is the default mode."
);
action_metadata!(
    EnterVisualMode,
    "visual mode",
    "Enter visual mode to select text by moving the cursor. Selection anchors at the current position."
);
action_metadata!(
    EnterPaneMode,
    "pane mode",
    "Enter pane mode for creating, closing, and navigating between split panes. Press Escape to exit."
);

// Selection actions
action_metadata!(
    SelectNextSymbol,
    "next symbol",
    "Extend the current selection forward to the next symbol boundary (word, punctuation, or whitespace transition)."
);
action_metadata!(
    SelectPrevSymbol,
    "prev symbol",
    "Extend the current selection backward to the previous symbol boundary (word, punctuation, or whitespace transition)."
);
action_metadata!(
    SelectNextToken,
    "next token",
    "Extend the current selection forward to include the next complete token in the document."
);
action_metadata!(
    SelectPrevToken,
    "prev token",
    "Extend the current selection backward to include the previous complete token in the document."
);

// Pane management actions
action_metadata!(
    SplitRight,
    "split right",
    "Split the current pane vertically, creating a new empty pane to the right. Both panes share the available width."
);
action_metadata!(
    SplitDown,
    "split down",
    "Split the current pane horizontally, creating a new empty pane below. Both panes share the available height."
);
action_metadata!(
    SplitLeft,
    "split left",
    "Split the current pane vertically, creating a new empty pane to the left. Both panes share the available width."
);
action_metadata!(
    SplitUp,
    "split up",
    "Split the current pane horizontally, creating a new empty pane above. Both panes share the available height."
);
action_metadata!(
    ClosePane,
    "close pane",
    "Close the currently focused pane and remove it from the layout. Cannot close the last remaining pane."
);
action_metadata!(
    FocusPaneLeft,
    "focus left",
    "Move keyboard focus to the pane immediately to the left of the current pane, if one exists."
);
action_metadata!(
    FocusPaneRight,
    "focus right",
    "Move keyboard focus to the pane immediately to the right of the current pane, if one exists."
);
action_metadata!(
    FocusPaneUp,
    "focus up",
    "Move keyboard focus to the pane immediately above the current pane, if one exists."
);
action_metadata!(
    FocusPaneDown,
    "focus down",
    "Move keyboard focus to the pane immediately below the current pane, if one exists."
);

// File operations
action_metadata!(
    Save,
    "save file",
    "Save the current file's contents to disk, writing all unsaved changes. Prompts for path if file is new."
);
action_metadata!(
    Open,
    "open file",
    "Open the file picker to select a file for editing in the current pane."
);
action_metadata!(
    Quit,
    "quit",
    "Close the current pane's file. If it's the last pane, exits the application. Warns if there are unsaved changes."
);
action_metadata!(
    ExitApp,
    "exit app",
    "Immediately exit the application, closing all panes and files. Warns if there are unsaved changes in any file."
);

// File finder actions
action_metadata!(
    OpenFileFinder,
    "file finder",
    "Open the fuzzy file finder modal to quickly search for and open files by name from the current directory."
);
action_metadata!(
    FileFinderNext,
    "next file",
    "Move the selection highlight down to the next matching file in the file finder results list."
);
action_metadata!(
    FileFinderPrev,
    "prev file",
    "Move the selection highlight up to the previous matching file in the file finder results list."
);
action_metadata!(
    FileFinderDismiss,
    "dismiss",
    "Close the file finder modal without opening any file and return to the editor."
);
action_metadata!(
    FileFinderSelect,
    "select file",
    "Open the currently highlighted file from the finder in the active pane and close the finder modal."
);

// Command palette actions
action_metadata!(
    OpenCommandPalette,
    "command palette",
    "Open the command palette to fuzzy search all available commands and see their keybindings and descriptions."
);
action_metadata!(
    CommandPaletteNext,
    "next command",
    "Move the selection highlight down to the next matching command in the command palette results list."
);
action_metadata!(
    CommandPalettePrev,
    "prev command",
    "Move the selection highlight up to the previous matching command in the command palette results list."
);
action_metadata!(
    CommandPaletteDismiss,
    "dismiss",
    "Close the command palette modal without executing any command and return to the previous mode."
);
action_metadata!(
    CommandPaletteExecute,
    "execute",
    "Execute the currently highlighted command from the palette, close the palette, and return to the previous mode."
);

/// Action names mapped by TypeId for runtime lookup.
///
/// Populated from [`ActionMetadata`] implementations for all registered action types.
pub static ACTION_NAMES: Lazy<HashMap<TypeId, &'static str>> = Lazy::new(|| {
    let mut names = HashMap::new();

    // Register all actions using their ActionMetadata implementations
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
    names.insert(TypeId::of::<PageUp>(), PageUp::action_name());
    names.insert(TypeId::of::<PageDown>(), PageDown::action_name());
    names.insert(TypeId::of::<DeleteLeft>(), DeleteLeft::action_name());
    names.insert(TypeId::of::<DeleteRight>(), DeleteRight::action_name());
    names.insert(TypeId::of::<DeleteLine>(), DeleteLine::action_name());
    names.insert(
        TypeId::of::<DeleteToEndOfLine>(),
        DeleteToEndOfLine::action_name(),
    );
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
    names.insert(TypeId::of::<EnterPaneMode>(), EnterPaneMode::action_name());
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
    names.insert(TypeId::of::<SplitRight>(), SplitRight::action_name());
    names.insert(TypeId::of::<SplitDown>(), SplitDown::action_name());
    names.insert(TypeId::of::<SplitLeft>(), SplitLeft::action_name());
    names.insert(TypeId::of::<SplitUp>(), SplitUp::action_name());
    names.insert(TypeId::of::<ClosePane>(), ClosePane::action_name());
    names.insert(TypeId::of::<FocusPaneLeft>(), FocusPaneLeft::action_name());
    names.insert(
        TypeId::of::<FocusPaneRight>(),
        FocusPaneRight::action_name(),
    );
    names.insert(TypeId::of::<FocusPaneUp>(), FocusPaneUp::action_name());
    names.insert(TypeId::of::<FocusPaneDown>(), FocusPaneDown::action_name());
    names.insert(TypeId::of::<Save>(), Save::action_name());
    names.insert(TypeId::of::<Open>(), Open::action_name());
    names.insert(TypeId::of::<Quit>(), Quit::action_name());
    names.insert(TypeId::of::<ExitApp>(), ExitApp::action_name());
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
        TypeId::of::<FileFinderDismiss>(),
        FileFinderDismiss::action_name(),
    );
    names.insert(
        TypeId::of::<FileFinderSelect>(),
        FileFinderSelect::action_name(),
    );
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
        TypeId::of::<CommandPaletteDismiss>(),
        CommandPaletteDismiss::action_name(),
    );
    names.insert(
        TypeId::of::<CommandPaletteExecute>(),
        CommandPaletteExecute::action_name(),
    );

    names
});

/// Compact help text for actions, displayed in UI overlays and tooltips.
///
/// Populated from [`ActionMetadata`] implementations for all registered action types.
/// These are 1-2 word descriptions optimized for space-constrained displays.
pub static HELP_TEXT: Lazy<HashMap<TypeId, &'static str>> = Lazy::new(|| {
    let mut help = HashMap::new();

    // Register all actions using their ActionMetadata implementations
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
    help.insert(TypeId::of::<PageUp>(), PageUp::help_text());
    help.insert(TypeId::of::<PageDown>(), PageDown::help_text());
    help.insert(TypeId::of::<DeleteLeft>(), DeleteLeft::help_text());
    help.insert(TypeId::of::<DeleteRight>(), DeleteRight::help_text());
    help.insert(TypeId::of::<DeleteLine>(), DeleteLine::help_text());
    help.insert(
        TypeId::of::<DeleteToEndOfLine>(),
        DeleteToEndOfLine::help_text(),
    );
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
    help.insert(TypeId::of::<EnterPaneMode>(), EnterPaneMode::help_text());
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
    help.insert(TypeId::of::<SplitRight>(), SplitRight::help_text());
    help.insert(TypeId::of::<SplitDown>(), SplitDown::help_text());
    help.insert(TypeId::of::<SplitLeft>(), SplitLeft::help_text());
    help.insert(TypeId::of::<SplitUp>(), SplitUp::help_text());
    help.insert(TypeId::of::<ClosePane>(), ClosePane::help_text());
    help.insert(TypeId::of::<FocusPaneLeft>(), FocusPaneLeft::help_text());
    help.insert(TypeId::of::<FocusPaneRight>(), FocusPaneRight::help_text());
    help.insert(TypeId::of::<FocusPaneUp>(), FocusPaneUp::help_text());
    help.insert(TypeId::of::<FocusPaneDown>(), FocusPaneDown::help_text());
    help.insert(TypeId::of::<Save>(), Save::help_text());
    help.insert(TypeId::of::<Open>(), Open::help_text());
    help.insert(TypeId::of::<Quit>(), Quit::help_text());
    help.insert(TypeId::of::<ExitApp>(), ExitApp::help_text());
    help.insert(TypeId::of::<OpenFileFinder>(), OpenFileFinder::help_text());
    help.insert(TypeId::of::<FileFinderNext>(), FileFinderNext::help_text());
    help.insert(TypeId::of::<FileFinderPrev>(), FileFinderPrev::help_text());
    help.insert(
        TypeId::of::<FileFinderDismiss>(),
        FileFinderDismiss::help_text(),
    );
    help.insert(
        TypeId::of::<FileFinderSelect>(),
        FileFinderSelect::help_text(),
    );
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
        TypeId::of::<CommandPaletteDismiss>(),
        CommandPaletteDismiss::help_text(),
    );
    help.insert(
        TypeId::of::<CommandPaletteExecute>(),
        CommandPaletteExecute::help_text(),
    );

    help
});

/// Get the canonical name for an action.
///
/// Returns the action's name string (e.g., "MoveLeft") for the given action,
/// or [`None`] if the action type is not registered.
///
/// # Example
/// ```ignore
/// let name = action_name(&MoveLeft);
/// assert_eq!(name, Some("MoveLeft"));
/// ```
pub fn action_name(action: &dyn Action) -> Option<&'static str> {
    ACTION_NAMES.get(&action.type_id()).copied()
}

/// Get short description for an action.
///
/// Returns the short description for the given action, or [`None`] if no
/// description has been registered for that action type.
///
/// Used in the help modal at the bottom of the screen for compact display.
///
/// # Example
/// ```ignore
/// let desc = help_text(&MoveLeft);
/// assert_eq!(desc, Some("move left"));
/// ```
pub fn help_text(action: &dyn Action) -> Option<&'static str> {
    HELP_TEXT.get(&action.type_id()).copied()
}

/// Detailed descriptions for actions, displayed in the command palette.
///
/// Populated from [`ActionMetadata`] implementations for all registered action types.
/// These are 1-2 sentence descriptions that provide context and behavioral details.
pub static DESCRIPTION: Lazy<HashMap<TypeId, &'static str>> = Lazy::new(|| {
    let mut long = HashMap::new();

    // Register all actions using their ActionMetadata implementations
    long.insert(TypeId::of::<MoveLeft>(), MoveLeft::description());
    long.insert(TypeId::of::<MoveRight>(), MoveRight::description());
    long.insert(TypeId::of::<MoveUp>(), MoveUp::description());
    long.insert(TypeId::of::<MoveDown>(), MoveDown::description());
    long.insert(
        TypeId::of::<MoveToLineStart>(),
        MoveToLineStart::description(),
    );
    long.insert(TypeId::of::<MoveToLineEnd>(), MoveToLineEnd::description());
    long.insert(
        TypeId::of::<MoveToFileStart>(),
        MoveToFileStart::description(),
    );
    long.insert(TypeId::of::<MoveToFileEnd>(), MoveToFileEnd::description());
    long.insert(TypeId::of::<PageUp>(), PageUp::description());
    long.insert(TypeId::of::<PageDown>(), PageDown::description());
    long.insert(TypeId::of::<DeleteLeft>(), DeleteLeft::description());
    long.insert(TypeId::of::<DeleteRight>(), DeleteRight::description());
    long.insert(TypeId::of::<DeleteLine>(), DeleteLine::description());
    long.insert(
        TypeId::of::<DeleteToEndOfLine>(),
        DeleteToEndOfLine::description(),
    );
    long.insert(
        TypeId::of::<EnterInsertMode>(),
        EnterInsertMode::description(),
    );
    long.insert(
        TypeId::of::<EnterNormalMode>(),
        EnterNormalMode::description(),
    );
    long.insert(
        TypeId::of::<EnterVisualMode>(),
        EnterVisualMode::description(),
    );
    long.insert(TypeId::of::<EnterPaneMode>(), EnterPaneMode::description());
    long.insert(
        TypeId::of::<SelectNextSymbol>(),
        SelectNextSymbol::description(),
    );
    long.insert(
        TypeId::of::<SelectPrevSymbol>(),
        SelectPrevSymbol::description(),
    );
    long.insert(
        TypeId::of::<SelectNextToken>(),
        SelectNextToken::description(),
    );
    long.insert(
        TypeId::of::<SelectPrevToken>(),
        SelectPrevToken::description(),
    );
    long.insert(TypeId::of::<SplitRight>(), SplitRight::description());
    long.insert(TypeId::of::<SplitDown>(), SplitDown::description());
    long.insert(TypeId::of::<SplitLeft>(), SplitLeft::description());
    long.insert(TypeId::of::<SplitUp>(), SplitUp::description());
    long.insert(TypeId::of::<ClosePane>(), ClosePane::description());
    long.insert(TypeId::of::<FocusPaneLeft>(), FocusPaneLeft::description());
    long.insert(
        TypeId::of::<FocusPaneRight>(),
        FocusPaneRight::description(),
    );
    long.insert(TypeId::of::<FocusPaneUp>(), FocusPaneUp::description());
    long.insert(TypeId::of::<FocusPaneDown>(), FocusPaneDown::description());
    long.insert(TypeId::of::<Save>(), Save::description());
    long.insert(TypeId::of::<Open>(), Open::description());
    long.insert(TypeId::of::<Quit>(), Quit::description());
    long.insert(TypeId::of::<ExitApp>(), ExitApp::description());
    long.insert(
        TypeId::of::<OpenFileFinder>(),
        OpenFileFinder::description(),
    );
    long.insert(
        TypeId::of::<FileFinderNext>(),
        FileFinderNext::description(),
    );
    long.insert(
        TypeId::of::<FileFinderPrev>(),
        FileFinderPrev::description(),
    );
    long.insert(
        TypeId::of::<FileFinderDismiss>(),
        FileFinderDismiss::description(),
    );
    long.insert(
        TypeId::of::<FileFinderSelect>(),
        FileFinderSelect::description(),
    );
    long.insert(
        TypeId::of::<OpenCommandPalette>(),
        OpenCommandPalette::description(),
    );
    long.insert(
        TypeId::of::<CommandPaletteNext>(),
        CommandPaletteNext::description(),
    );
    long.insert(
        TypeId::of::<CommandPalettePrev>(),
        CommandPalettePrev::description(),
    );
    long.insert(
        TypeId::of::<CommandPaletteDismiss>(),
        CommandPaletteDismiss::description(),
    );
    long.insert(
        TypeId::of::<CommandPaletteExecute>(),
        CommandPaletteExecute::description(),
    );

    long
});

/// Get long description for an action.
///
/// Returns the full sentence description for the given action, or [`None`] if no
/// description has been registered for that action type.
///
/// Used in the command palette for detailed action descriptions.
///
/// # Example
/// ```ignore
/// let desc = description(&MoveLeft);
/// assert_eq!(desc, Some("Move the cursor one character to the left. In normal mode, stops at the beginning of the line."));
/// ```
pub fn description(action: &dyn Action) -> Option<&'static str> {
    DESCRIPTION.get(&action.type_id()).copied()
}
