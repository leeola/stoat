//! Action definitions for stoat.
//!
//! Actions are dispatched through GPUI's action system and handled by [`crate::Stoat`].
//!
//! # Metadata Architecture
//!
//! This module provides action metadata through two complementary systems:
//!
//! 1. **GPUI's idiomatic approach** (preferred for new code):
//!    - Doc comments on actions (e.g., `/// Move cursor up`)
//!    - [`Action::documentation()`] auto-extracts these comments
//!    - [`crate::action_metadata::ActionMetadataRegistry`] provides TypeId-based lookup
//!
//! 2. **ActionMetadata trait** (retained for backward compatibility):
//!    - [`ActionMetadata`] trait defines `action_name()`, `description()`, `help_text()`,
//!      `aliases()`
//!    - Implemented via `action_metadata!` macro for all 99 actions
//!    - Static HashMaps ([`ACTION_NAMES`], [`DESCRIPTIONS`], [`HELP_TEXT`], [`ALIASES`]) provide
//!      TypeId lookups
//!    - Used by command palette and help modal that need additional metadata beyond doc comments
//!
//! Both systems are kept in sync: doc comments are the single source of truth, and
//! [`ActionMetadata::description()`] implementations typically match the doc comment text.
//!
//! # Migration Complete
//!
//! All 99 actions have been migrated to use GPUI's idiomatic [`Action::documentation()`].
//! The old `generate_metadata_maps!` macro that generated HashMaps has been removed.
//! Manual HashMap entries are retained for command palette/help modal TypeId lookups.

use crate::stoat::KeyContext;
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
        /// Open new line below and enter insert mode
        OpenLineBelow,
        /// Open new line above and enter insert mode
        OpenLineAbove,
        /// Move right one character and enter insert mode
        Append,
        /// Move to end of line and enter insert mode
        AppendAtLineEnd,
        /// Move to first non-whitespace character and enter insert mode
        InsertAtLineStart,
        /// Delete selected text
        DeleteSelection,
        /// Delete selected text and enter insert mode
        ChangeSelection,
        /// Yank (copy) selected text to clipboard
        Yank,
        /// Paste clipboard contents after cursor
        PasteAfter,
        /// Paste clipboard contents before cursor
        PasteBefore,
        /// Join current line with the next line
        JoinLines,
        /// Indent selected lines
        Indent,
        /// Outdent selected lines
        Outdent,
        /// Convert selection to lowercase
        Lowercase,
        /// Convert selection to uppercase
        Uppercase,
        /// Toggle case of each character in selection
        SwapCase,
        /// Replace character under cursor with next typed character
        ReplaceChar,
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
        /// Move cursor to end of current/next word
        MoveNextWordEnd,
        /// Move cursor to end of current/next WORD (whitespace-delimited)
        MoveNextLongWordEnd,
        /// Find character forward on current line
        FindCharForward,
        /// Find character backward on current line
        FindCharBackward,
        /// Move till character forward on current line
        TillCharForward,
        /// Move till character backward on current line
        TillCharBackward,
        /// Move to first non-whitespace character on line
        MoveToFirstNonWhitespace,
        /// Scroll up half a page
        HalfPageUp,
        /// Scroll down half a page
        HalfPageDown,
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
        /// Enter git filter mode
        EnterGitFilterMode,
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

// Buffer finder actions
actions!(
    stoat,
    [
        /// Open buffer finder
        OpenBufferFinder,
        /// Move to next buffer in finder
        BufferFinderNext,
        /// Move to previous buffer in finder
        BufferFinderPrev,
        /// Select current buffer in finder
        BufferFinderSelect,
        /// Dismiss buffer finder
        BufferFinderDismiss,
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
        /// Toggle showing hidden commands in palette
        ToggleCommandPaletteHidden,
    ]
);

// CommandPaletteV2 actions (new entity-based palette)
actions!(
    stoat,
    [
        /// Open command palette V2
        OpenCommandPaletteV2,
        /// Dismiss command palette V2
        DismissCommandPaletteV2,
        /// Execute selected command in palette V2
        AcceptCommandPaletteV2,
        /// Move to next command in palette V2
        SelectNextCommandV2,
        /// Move to previous command in palette V2
        SelectPrevCommandV2,
    ]
);

// Command line actions
actions!(
    stoat,
    [
        /// Open command line prompt for vim-style commands
        ShowCommandLine,
        /// Dismiss command line prompt
        CommandLineDismiss,
        /// Print the current working directory
        PrintWorkingDirectory,
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
        /// Cycle through git status filter modes
        GitStatusCycleFilter,
        /// Set filter to show all files
        GitStatusSetFilterAll,
        /// Set filter to show only staged files
        GitStatusSetFilterStaged,
        /// Set filter to show only unstaged files
        GitStatusSetFilterUnstaged,
        /// Set filter to show unstaged and untracked files
        GitStatusSetFilterUnstagedWithUntracked,
        /// Set filter to show only untracked files
        GitStatusSetFilterUntracked,
    ]
);

// Git diff hunk actions
actions!(
    stoat,
    [
        /// Toggle inline diff view at cursor
        ToggleDiffHunk,
        /// Jump to next git diff hunk
        GotoNextHunk,
        /// Jump to previous git diff hunk
        GotoPrevHunk,
    ]
);

// Diff review actions
actions!(
    stoat,
    [
        /// Open git diff review mode
        OpenDiffReview,
        /// Jump to next unreviewed hunk in diff review
        DiffReviewNextHunk,
        /// Jump to previous hunk in diff review
        DiffReviewPrevHunk,
        /// Approve current hunk and move to next
        DiffReviewApproveHunk,
        /// Toggle current hunk approval status
        DiffReviewToggleApproval,
        /// Jump to next unreviewed hunk
        DiffReviewNextUnreviewedHunk,
        /// Reset review progress and start over
        DiffReviewResetProgress,
        /// Exit diff review mode
        DiffReviewDismiss,
        /// Cycle view filter (All/Unstaged/Staged) within WorkingTree scope
        DiffReviewCycleComparisonMode,
        /// Toggle between WorkingTree and Commit scope
        DiffReviewPreviousCommit,
        /// Revert current hunk in Commit scope
        DiffReviewRevertHunk,
        /// Toggle live follow mode in diff review
        DiffReviewToggleFollow,
    ]
);

// Conflict review actions
actions!(
    stoat,
    [
        /// Open conflict review mode
        OpenConflictReview,
        /// Exit conflict review mode
        ConflictReviewDismiss,
        /// Accept our side of the conflict
        ConflictAcceptOurs,
        /// Accept their side of the conflict
        ConflictAcceptTheirs,
        /// Accept both sides of the conflict
        ConflictAcceptBoth,
        /// Jump to next conflict
        ConflictNextConflict,
        /// Jump to previous conflict
        ConflictPrevConflict,
    ]
);

// Git repository actions
actions!(
    stoat,
    [
        /// Stage file changes for commit
        GitStageFile,
        /// Stage all changes for commit
        GitStageAll,
        /// Unstage file changes
        GitUnstageFile,
        /// Unstage all changes
        GitUnstageAll,
        /// Stage the current hunk
        GitStageHunk,
        /// Unstage the current hunk
        GitUnstageHunk,
        /// Toggle stage/unstage for the current hunk
        GitToggleStageHunk,
        /// Toggle stage/unstage for the current line
        GitToggleStageLine,
    ]
);

// Line selection actions (partial hunk staging)
actions!(
    stoat,
    [
        /// Enter line selection mode for partial hunk staging
        DiffReviewEnterLineSelect,
        /// Toggle selection of current line
        DiffReviewLineSelectToggle,
        /// Select all changeable lines
        DiffReviewLineSelectAll,
        /// Deselect all changeable lines
        DiffReviewLineSelectNone,
        /// Stage selected lines
        DiffReviewLineSelectStage,
        /// Unstage selected lines
        DiffReviewLineSelectUnstage,
        /// Cancel line selection and return to diff review
        DiffReviewLineSelectCancel,
        /// Move cursor down in line selection
        DiffReviewLineSelectDown,
        /// Move cursor up in line selection
        DiffReviewLineSelectUp,
    ]
);

// Selection actions
actions!(
    stoat,
    [
        /// Move cursor to the start of the next word
        MoveNextWordStart,
        /// Move cursor to the start of the previous word
        MovePrevWordStart,
        /// Move cursor to the start of the next WORD (whitespace-delimited)
        MoveNextLongWordStart,
        /// Move cursor to the start of the previous WORD (whitespace-delimited)
        MovePrevLongWordStart,
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
        /// Split multi-line selection into one cursor per line
        SplitSelectionIntoLines,
        /// Select next occurrence of current selection
        SelectNext,
        /// Select previous occurrence of current selection
        SelectPrevious,
        /// Select all occurrences of current selection
        SelectAllMatches,
        /// Add cursor on line above with same column
        AddSelectionAbove,
        /// Add cursor on line below with same column
        AddSelectionBelow,
        /// Select entire buffer contents
        SelectAll,
        /// Collapse all selections to cursor position
        CollapseSelection,
        /// Remove all selections except the primary
        KeepPrimarySelection,
        /// Swap anchor and head of all selections
        FlipSelection,
        /// Extend selection to end of current/next word
        ExtendNextWordEnd,
        /// Extend selection to end of current/next WORD (whitespace-delimited)
        ExtendNextLongWordEnd,
        /// Select current line, or extend existing line selection downward
        SelectLine,
        /// Open regex prompt for sub-selecting within current selections
        SelectRegex,
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
        /// Quit the current view (close pane, or quit app if last)
        Quit,
        /// Focus the pane above the current one
        FocusPaneUp,
        /// Focus the pane below the current one
        FocusPaneDown,
        /// Focus the pane to the left of the current one
        FocusPaneLeft,
        /// Focus the pane to the right of the current one
        FocusPaneRight,
        /// Close the active buffer and switch to the previous one
        CloseBuffer,
        /// Close all panes except the focused one
        CloseOtherPanes,
    ]
);

// Application actions
actions!(
    stoat,
    [
        /// Quit the application immediately
        QuitAll,
        /// Write current buffer to disk
        WriteFile,
        /// Write all modified buffers to disk
        WriteAll,
    ]
);

// View actions
actions!(
    stoat,
    [
        /// Toggle minimap visibility
        ToggleMinimap,
        /// Show minimap on scroll
        ShowMinimapOnScroll,
    ]
);

// Help actions
actions!(
    stoat,
    [
        /// Open help overlay
        OpenHelpOverlay,
        /// Open help modal
        OpenHelpModal,
        /// Dismiss help modal
        HelpModalDismiss,
        /// Open about modal
        OpenAboutModal,
        /// Dismiss about modal
        AboutModalDismiss,
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

/// Set the active KeyContext
#[derive(Clone, PartialEq, Action)]
#[action(no_json)]
pub struct SetKeyContext(pub KeyContext);

/// Set the active mode within the current KeyContext
#[derive(Clone, PartialEq, Action)]
#[action(no_json)]
pub struct SetMode(pub String);

/// Change the current working directory
#[derive(Clone, PartialEq, Action)]
#[action(no_json)]
pub struct ChangeDirectory {
    /// Target directory path
    pub path: std::path::PathBuf,
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

    /// Command aliases for the action (e.g., ["q", "quit"] for QuitApp).
    ///
    /// Aliases provide alternative ways to invoke the action in the command palette.
    /// They are matched both exactly (for perfect matches) and via fuzzy matching.
    fn aliases() -> &'static [&'static str] {
        &[]
    }

    /// Whether this command should be hidden from the command palette by default.
    ///
    /// Hidden commands are typically context-specific actions that cannot be executed
    /// from the command palette (e.g., dismiss actions for modals).
    fn hidden() -> bool {
        false
    }
}

/// Helper macro to implement [`ActionMetadata`] for an action type.
macro_rules! action_metadata {
    // Hidden with aliases
    ($type:ty, $help:expr, $desc:expr, [$($alias:expr),* $(,)?], hidden) => {
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

            fn aliases() -> &'static [&'static str] {
                &[$($alias),*]
            }

            fn hidden() -> bool {
                true
            }
        }
    };
    // Hidden without aliases
    ($type:ty, $help:expr, $desc:expr, hidden) => {
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

            fn hidden() -> bool {
                true
            }
        }
    };
    // With aliases (not hidden)
    ($type:ty, $help:expr, $desc:expr, [$($alias:expr),* $(,)?]) => {
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

            fn aliases() -> &'static [&'static str] {
                &[$($alias),*]
            }
        }
    };
    // Without aliases (backward compatible, not hidden)
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
action_metadata!(
    MoveNextWordEnd,
    "word end",
    "Move the cursor to the end of the current/next word"
);
action_metadata!(
    MoveNextLongWordEnd,
    "WORD end",
    "Move the cursor to the end of the current/next WORD (whitespace-delimited)"
);
action_metadata!(
    FindCharForward,
    "find char forward",
    "Find character forward on current line"
);
action_metadata!(
    FindCharBackward,
    "find char backward",
    "Find character backward on current line"
);
action_metadata!(
    TillCharForward,
    "till char forward",
    "Move till character forward on current line"
);
action_metadata!(
    TillCharBackward,
    "till char backward",
    "Move till character backward on current line"
);
action_metadata!(
    MoveToFirstNonWhitespace,
    "first non-whitespace",
    "Move to the first non-whitespace character on the line"
);
action_metadata!(HalfPageUp, "half page up", "Scroll up half a page");
action_metadata!(HalfPageDown, "half page down", "Scroll down half a page");

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
action_metadata!(
    OpenLineBelow,
    "open below",
    "Open new line below and enter insert mode"
);
action_metadata!(
    OpenLineAbove,
    "open above",
    "Open new line above and enter insert mode"
);
action_metadata!(Append, "append", "Move right and enter insert mode");
action_metadata!(
    AppendAtLineEnd,
    "append at end",
    "Move to end of line and enter insert mode"
);
action_metadata!(
    InsertAtLineStart,
    "insert at start",
    "Move to first non-whitespace and enter insert mode"
);
action_metadata!(
    DeleteSelection,
    "delete selection",
    "Delete selected text and enter normal mode"
);
action_metadata!(
    ChangeSelection,
    "change selection",
    "Delete selected text and enter insert mode"
);
action_metadata!(SelectAll, "select all", "Select the entire buffer contents");
action_metadata!(Yank, "yank", "Copy selected text to clipboard");
action_metadata!(
    PasteAfter,
    "paste after",
    "Paste clipboard contents after cursor"
);
action_metadata!(
    PasteBefore,
    "paste before",
    "Paste clipboard contents before cursor"
);
action_metadata!(JoinLines, "join lines", "Join current line with the next");
action_metadata!(Indent, "indent", "Indent selected lines");
action_metadata!(Outdent, "outdent", "Remove one level of indentation");
action_metadata!(Lowercase, "lowercase", "Convert selection to lowercase");
action_metadata!(Uppercase, "uppercase", "Convert selection to uppercase");
action_metadata!(SwapCase, "swap case", "Toggle case of each character");
action_metadata!(
    ReplaceChar,
    "replace char",
    "Replace character under cursor with next typed character"
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
action_metadata!(
    EnterGitFilterMode,
    "git filter mode",
    "Enter git filter mode for selecting filter type"
);

// Selection actions
action_metadata!(
    MoveNextWordStart,
    "next word start",
    "Move cursor to the start of the next word"
);
action_metadata!(
    MovePrevWordStart,
    "prev word start",
    "Move cursor to the start of the previous word"
);
action_metadata!(
    MoveNextLongWordStart,
    "next WORD start",
    "Move cursor to the start of the next WORD (whitespace-delimited)"
);
action_metadata!(
    MovePrevLongWordStart,
    "prev WORD start",
    "Move cursor to the start of the previous WORD (whitespace-delimited)"
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
action_metadata!(
    SplitSelectionIntoLines,
    "split selection into lines",
    "Split multi-line selection into one cursor per line"
);
action_metadata!(
    SelectNext,
    "select next occurrence",
    "Add selection at next occurrence of current selection"
);
action_metadata!(
    SelectPrevious,
    "select previous occurrence",
    "Add selection at previous occurrence of current selection"
);
action_metadata!(
    SelectAllMatches,
    "select all occurrences",
    "Select all occurrences of current selection"
);
action_metadata!(
    AddSelectionAbove,
    "add selection above",
    "Add cursor on line above at same column position"
);
action_metadata!(
    AddSelectionBelow,
    "add selection below",
    "Add cursor on line below at same column position"
);
action_metadata!(
    CollapseSelection,
    "collapse selection",
    "Collapse all selections to their cursor position"
);
action_metadata!(
    KeepPrimarySelection,
    "keep primary",
    "Remove all selections except the primary"
);
action_metadata!(
    FlipSelection,
    "flip selection",
    "Swap anchor and head of all selections"
);
action_metadata!(
    ExtendNextWordEnd,
    "extend word end",
    "Extend selection to the end of the current/next word"
);
action_metadata!(
    ExtendNextLongWordEnd,
    "extend WORD end",
    "Extend selection to the end of the current/next WORD (whitespace-delimited)"
);
action_metadata!(
    SelectLine,
    "select line",
    "Select the current line, or extend an existing line selection downward"
);
action_metadata!(
    SelectRegex,
    "select regex",
    "Open regex prompt to sub-select within current selections"
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
    "Close the file finder without opening a file",
    hidden
);

// Buffer finder actions
action_metadata!(
    OpenBufferFinder,
    "buffer finder",
    "Open the buffer finder to quickly switch between open buffers"
);
action_metadata!(
    BufferFinderNext,
    "next buffer",
    "Move to the next buffer in the buffer finder list"
);
action_metadata!(
    BufferFinderPrev,
    "prev buffer",
    "Move to the previous buffer in the buffer finder list"
);
action_metadata!(
    BufferFinderSelect,
    "select buffer",
    "Switch to the currently selected buffer from the buffer finder"
);
action_metadata!(
    BufferFinderDismiss,
    "dismiss finder",
    "Close the buffer finder without switching buffers",
    hidden
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
    "Close the command palette without executing a command",
    hidden
);
action_metadata!(
    ToggleCommandPaletteHidden,
    "toggle hidden",
    "Toggle showing hidden commands in the command palette"
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
    "Close the git status modal without opening a file",
    hidden
);
action_metadata!(
    GitStatusCycleFilter,
    "cycle filter",
    "Cycle through git status filter modes (All, Staged, Unstaged, Unstaged+Untracked, Untracked)"
);
action_metadata!(
    GitStatusSetFilterAll,
    "show all",
    "Show all files in git status"
);
action_metadata!(
    GitStatusSetFilterStaged,
    "show staged",
    "Show only staged files in git status"
);
action_metadata!(
    GitStatusSetFilterUnstaged,
    "show unstaged",
    "Show only unstaged files in git status (excluding untracked)"
);
action_metadata!(
    GitStatusSetFilterUnstagedWithUntracked,
    "unstaged+untracked",
    "Show all unstaged files including untracked files"
);
action_metadata!(
    GitStatusSetFilterUntracked,
    "show untracked",
    "Show only untracked files in git status"
);

// Git diff hunk actions
action_metadata!(
    ToggleDiffHunk,
    "toggle diff",
    "Toggle inline diff view at cursor position"
);
action_metadata!(GotoNextHunk, "next hunk", "Jump to the next git diff hunk");
action_metadata!(
    GotoPrevHunk,
    "prev hunk",
    "Jump to the previous git diff hunk"
);

// Diff review actions
action_metadata!(
    OpenDiffReview,
    "diff review",
    "Open git diff review mode to review all modified files hunk by hunk"
);
action_metadata!(
    DiffReviewNextHunk,
    "next hunk",
    "Jump to the next unreviewed hunk in diff review mode"
);
action_metadata!(
    DiffReviewPrevHunk,
    "prev hunk",
    "Jump to the previous hunk in diff review mode"
);
action_metadata!(
    DiffReviewApproveHunk,
    "approve hunk",
    "Mark the current hunk as reviewed and move to the next unreviewed hunk"
);
action_metadata!(
    DiffReviewToggleApproval,
    "toggle approval",
    "Toggle the current hunk between reviewed and not reviewed status"
);
action_metadata!(
    DiffReviewNextUnreviewedHunk,
    "next unreviewed",
    "Jump to the next unreviewed hunk across all files in diff review"
);
action_metadata!(
    DiffReviewResetProgress,
    "reset progress",
    "Clear all review progress and start diff review from the beginning"
);
action_metadata!(
    DiffReviewDismiss,
    "dismiss review",
    "Exit diff review mode and return to the previous mode",
    hidden
);
action_metadata!(
    DiffReviewCycleComparisonMode,
    "cycle filter",
    "Cycle view filter: All Changes, Unstaged, Staged (WorkingTree scope only)"
);
action_metadata!(
    DiffReviewPreviousCommit,
    "toggle commit",
    "Toggle between WorkingTree and Commit scope (saves/restores position)"
);
action_metadata!(
    DiffReviewRevertHunk,
    "revert hunk",
    "Revert the current hunk to the working tree (Commit scope only)"
);
action_metadata!(
    DiffReviewToggleFollow,
    "toggle follow",
    "Toggle live follow mode that auto-navigates to new hunks on file changes"
);

// Conflict review actions
action_metadata!(
    OpenConflictReview,
    "conflict review",
    "Open conflict review mode to resolve merge conflicts"
);
action_metadata!(
    ConflictReviewDismiss,
    "dismiss conflict",
    "Exit conflict review mode",
    hidden
);
action_metadata!(
    ConflictAcceptOurs,
    "accept ours",
    "Resolve the current conflict by accepting our changes"
);
action_metadata!(
    ConflictAcceptTheirs,
    "accept theirs",
    "Resolve the current conflict by accepting their changes"
);
action_metadata!(
    ConflictAcceptBoth,
    "accept both",
    "Resolve the current conflict by keeping both sides"
);
action_metadata!(
    ConflictNextConflict,
    "next conflict",
    "Jump to the next conflict marker",
    hidden
);
action_metadata!(
    ConflictPrevConflict,
    "prev conflict",
    "Jump to the previous conflict marker",
    hidden
);

// Git repository actions
action_metadata!(
    GitStageFile,
    "stage file",
    "Stage the current file's changes for commit",
    ["stage", "add"]
);
action_metadata!(
    GitStageAll,
    "stage all",
    "Stage all changes in the repository for commit",
    ["stage-all", "add-all"]
);
action_metadata!(
    GitUnstageFile,
    "unstage file",
    "Unstage the current file's changes",
    ["unstage", "reset"]
);
action_metadata!(
    GitUnstageAll,
    "unstage all",
    "Unstage all changes in the repository",
    ["unstage-all", "reset-all"]
);
action_metadata!(
    GitStageHunk,
    "stage hunk",
    "Stage the current hunk for commit",
    ["stage-hunk", "add-hunk"]
);
action_metadata!(
    GitUnstageHunk,
    "unstage hunk",
    "Unstage the current hunk",
    ["unstage-hunk", "reset-hunk"]
);
action_metadata!(
    GitToggleStageHunk,
    "toggle stage hunk",
    "Toggle stage/unstage for the current hunk",
    ["toggle-stage-hunk"]
);
action_metadata!(
    GitToggleStageLine,
    "toggle stage line",
    "Toggle stage/unstage for the current line",
    ["toggle-stage-line"]
);

// Line selection actions
action_metadata!(
    DiffReviewEnterLineSelect,
    "line select",
    "Enter line selection mode for partial hunk staging",
    hidden
);
action_metadata!(
    DiffReviewLineSelectToggle,
    "toggle line",
    "Toggle selection of current line in line selection mode",
    hidden
);
action_metadata!(
    DiffReviewLineSelectAll,
    "select all lines",
    "Select all changeable lines in line selection mode",
    hidden
);
action_metadata!(
    DiffReviewLineSelectNone,
    "deselect all lines",
    "Deselect all changeable lines in line selection mode",
    hidden
);
action_metadata!(
    DiffReviewLineSelectStage,
    "stage lines",
    "Stage selected lines from line selection",
    hidden
);
action_metadata!(
    DiffReviewLineSelectUnstage,
    "unstage lines",
    "Unstage selected lines from line selection",
    hidden
);
action_metadata!(
    DiffReviewLineSelectCancel,
    "cancel line select",
    "Cancel line selection and return to diff review",
    hidden
);
action_metadata!(
    DiffReviewLineSelectDown,
    "line select down",
    "Move cursor down in line selection mode",
    hidden
);
action_metadata!(
    DiffReviewLineSelectUp,
    "line select up",
    "Move cursor up in line selection mode",
    hidden
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
    Quit,
    "quit",
    "Close the current view, or quit the application if it's the last view",
    ["q", "quit"]
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
action_metadata!(
    CloseBuffer,
    "close buffer",
    "Close the active buffer and switch to the previous one",
    ["bd", "buffer-close"]
);
action_metadata!(
    CloseOtherPanes,
    "close other panes",
    "Close all panes except the focused one",
    ["only", "wonly"]
);

// Application actions
action_metadata!(
    QuitAll,
    "quit all",
    "Quit the application immediately by closing all views",
    ["qa", "quitall"]
);
action_metadata!(
    WriteFile,
    "write",
    "Write the current buffer to disk",
    ["w", "write"]
);
action_metadata!(
    WriteAll,
    "write all",
    "Write all modified buffers to disk",
    ["wa", "wall"]
);

// View actions
action_metadata!(
    ToggleMinimap,
    "toggle minimap",
    "Toggle minimap visibility between always visible and always hidden",
    ["minimap"]
);
action_metadata!(
    ShowMinimapOnScroll,
    "minimap on scroll",
    "Show minimap temporarily when scrolling more than 5 lines"
);

// Help actions
action_metadata!(
    OpenHelpOverlay,
    "help",
    "Show help overlay with basic keybinding hints",
    ["help", "?"]
);
action_metadata!(
    OpenHelpModal,
    "full help",
    "Open full help modal with comprehensive keybinding reference"
);
action_metadata!(
    HelpModalDismiss,
    "dismiss help",
    "Close the help modal and return to the previous mode",
    hidden
);
action_metadata!(
    OpenAboutModal,
    "about",
    "Show information about Stoat including version and build details",
    ["about"]
);
action_metadata!(
    AboutModalDismiss,
    "dismiss about",
    "Close the about modal and return to the previous mode",
    hidden
);

// Command line actions
action_metadata!(
    PrintWorkingDirectory,
    "print working directory",
    "Print the current working directory",
    ["cwd", "pwd"]
);

// KeyContext and Mode actions
action_metadata!(
    SetKeyContext,
    "set context",
    "Set the active KeyContext (controls which UI is rendered)"
);
action_metadata!(
    SetMode,
    "set mode",
    "Set the active mode within the current KeyContext"
);

// Static maps for looking up action metadata by TypeId

/// Map from TypeId to action name
pub static ACTION_NAMES: LazyLock<HashMap<TypeId, &'static str>> = LazyLock::new(|| {
    let mut names = HashMap::new();

    // Movement actions
    names.insert(TypeId::of::<MoveUp>(), MoveUp::action_name());
    names.insert(TypeId::of::<MoveDown>(), MoveDown::action_name());
    names.insert(TypeId::of::<MoveLeft>(), MoveLeft::action_name());
    names.insert(TypeId::of::<MoveRight>(), MoveRight::action_name());
    names.insert(TypeId::of::<MoveWordLeft>(), MoveWordLeft::action_name());
    names.insert(TypeId::of::<MoveWordRight>(), MoveWordRight::action_name());
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
    names.insert(
        TypeId::of::<MoveNextWordEnd>(),
        MoveNextWordEnd::action_name(),
    );
    names.insert(
        TypeId::of::<MoveNextLongWordEnd>(),
        MoveNextLongWordEnd::action_name(),
    );
    names.insert(
        TypeId::of::<FindCharForward>(),
        FindCharForward::action_name(),
    );
    names.insert(
        TypeId::of::<FindCharBackward>(),
        FindCharBackward::action_name(),
    );
    names.insert(
        TypeId::of::<TillCharForward>(),
        TillCharForward::action_name(),
    );
    names.insert(
        TypeId::of::<TillCharBackward>(),
        TillCharBackward::action_name(),
    );
    names.insert(
        TypeId::of::<MoveToFirstNonWhitespace>(),
        MoveToFirstNonWhitespace::action_name(),
    );
    names.insert(TypeId::of::<HalfPageUp>(), HalfPageUp::action_name());
    names.insert(TypeId::of::<HalfPageDown>(), HalfPageDown::action_name());

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
    names.insert(TypeId::of::<OpenLineBelow>(), OpenLineBelow::action_name());
    names.insert(TypeId::of::<OpenLineAbove>(), OpenLineAbove::action_name());
    names.insert(TypeId::of::<Append>(), Append::action_name());
    names.insert(
        TypeId::of::<AppendAtLineEnd>(),
        AppendAtLineEnd::action_name(),
    );
    names.insert(
        TypeId::of::<InsertAtLineStart>(),
        InsertAtLineStart::action_name(),
    );
    names.insert(
        TypeId::of::<DeleteSelection>(),
        DeleteSelection::action_name(),
    );
    names.insert(
        TypeId::of::<ChangeSelection>(),
        ChangeSelection::action_name(),
    );
    names.insert(TypeId::of::<SelectAll>(), SelectAll::action_name());
    names.insert(TypeId::of::<Yank>(), Yank::action_name());
    names.insert(TypeId::of::<PasteAfter>(), PasteAfter::action_name());
    names.insert(TypeId::of::<PasteBefore>(), PasteBefore::action_name());
    names.insert(TypeId::of::<JoinLines>(), JoinLines::action_name());
    names.insert(TypeId::of::<Indent>(), Indent::action_name());
    names.insert(TypeId::of::<Outdent>(), Outdent::action_name());
    names.insert(TypeId::of::<Lowercase>(), Lowercase::action_name());
    names.insert(TypeId::of::<Uppercase>(), Uppercase::action_name());
    names.insert(TypeId::of::<SwapCase>(), SwapCase::action_name());
    names.insert(TypeId::of::<ReplaceChar>(), ReplaceChar::action_name());

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
    names.insert(
        TypeId::of::<EnterGitFilterMode>(),
        EnterGitFilterMode::action_name(),
    );

    // Selection actions
    names.insert(
        TypeId::of::<MoveNextWordStart>(),
        MoveNextWordStart::action_name(),
    );
    names.insert(
        TypeId::of::<MovePrevWordStart>(),
        MovePrevWordStart::action_name(),
    );
    names.insert(
        TypeId::of::<MoveNextLongWordStart>(),
        MoveNextLongWordStart::action_name(),
    );
    names.insert(
        TypeId::of::<MovePrevLongWordStart>(),
        MovePrevLongWordStart::action_name(),
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
    names.insert(
        TypeId::of::<SplitSelectionIntoLines>(),
        SplitSelectionIntoLines::action_name(),
    );
    names.insert(TypeId::of::<SelectNext>(), SelectNext::action_name());
    names.insert(
        TypeId::of::<SelectPrevious>(),
        SelectPrevious::action_name(),
    );
    names.insert(
        TypeId::of::<SelectAllMatches>(),
        SelectAllMatches::action_name(),
    );
    names.insert(
        TypeId::of::<AddSelectionAbove>(),
        AddSelectionAbove::action_name(),
    );
    names.insert(
        TypeId::of::<AddSelectionBelow>(),
        AddSelectionBelow::action_name(),
    );
    names.insert(
        TypeId::of::<CollapseSelection>(),
        CollapseSelection::action_name(),
    );
    names.insert(
        TypeId::of::<KeepPrimarySelection>(),
        KeepPrimarySelection::action_name(),
    );
    names.insert(TypeId::of::<FlipSelection>(), FlipSelection::action_name());
    names.insert(
        TypeId::of::<ExtendNextWordEnd>(),
        ExtendNextWordEnd::action_name(),
    );
    names.insert(
        TypeId::of::<ExtendNextLongWordEnd>(),
        ExtendNextLongWordEnd::action_name(),
    );
    names.insert(TypeId::of::<SelectLine>(), SelectLine::action_name());
    names.insert(TypeId::of::<SelectRegex>(), SelectRegex::action_name());

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
    names.insert(
        TypeId::of::<ToggleCommandPaletteHidden>(),
        ToggleCommandPaletteHidden::action_name(),
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
    names.insert(
        TypeId::of::<GitStatusCycleFilter>(),
        GitStatusCycleFilter::action_name(),
    );
    names.insert(
        TypeId::of::<GitStatusSetFilterAll>(),
        GitStatusSetFilterAll::action_name(),
    );
    names.insert(
        TypeId::of::<GitStatusSetFilterStaged>(),
        GitStatusSetFilterStaged::action_name(),
    );
    names.insert(
        TypeId::of::<GitStatusSetFilterUnstaged>(),
        GitStatusSetFilterUnstaged::action_name(),
    );
    names.insert(
        TypeId::of::<GitStatusSetFilterUnstagedWithUntracked>(),
        GitStatusSetFilterUnstagedWithUntracked::action_name(),
    );
    names.insert(
        TypeId::of::<GitStatusSetFilterUntracked>(),
        GitStatusSetFilterUntracked::action_name(),
    );

    // Git diff hunk actions
    names.insert(
        TypeId::of::<ToggleDiffHunk>(),
        ToggleDiffHunk::action_name(),
    );
    names.insert(TypeId::of::<GotoNextHunk>(), GotoNextHunk::action_name());
    names.insert(TypeId::of::<GotoPrevHunk>(), GotoPrevHunk::action_name());

    // Diff review actions
    names.insert(
        TypeId::of::<OpenDiffReview>(),
        OpenDiffReview::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewNextHunk>(),
        DiffReviewNextHunk::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewPrevHunk>(),
        DiffReviewPrevHunk::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewApproveHunk>(),
        DiffReviewApproveHunk::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewToggleApproval>(),
        DiffReviewToggleApproval::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewNextUnreviewedHunk>(),
        DiffReviewNextUnreviewedHunk::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewResetProgress>(),
        DiffReviewResetProgress::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewDismiss>(),
        DiffReviewDismiss::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewCycleComparisonMode>(),
        DiffReviewCycleComparisonMode::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewPreviousCommit>(),
        DiffReviewPreviousCommit::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewRevertHunk>(),
        DiffReviewRevertHunk::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewToggleFollow>(),
        DiffReviewToggleFollow::action_name(),
    );

    // Conflict review actions
    names.insert(
        TypeId::of::<OpenConflictReview>(),
        OpenConflictReview::action_name(),
    );
    names.insert(
        TypeId::of::<ConflictReviewDismiss>(),
        ConflictReviewDismiss::action_name(),
    );
    names.insert(
        TypeId::of::<ConflictAcceptOurs>(),
        ConflictAcceptOurs::action_name(),
    );
    names.insert(
        TypeId::of::<ConflictAcceptTheirs>(),
        ConflictAcceptTheirs::action_name(),
    );
    names.insert(
        TypeId::of::<ConflictAcceptBoth>(),
        ConflictAcceptBoth::action_name(),
    );
    names.insert(
        TypeId::of::<ConflictNextConflict>(),
        ConflictNextConflict::action_name(),
    );
    names.insert(
        TypeId::of::<ConflictPrevConflict>(),
        ConflictPrevConflict::action_name(),
    );

    // Git repository actions
    names.insert(TypeId::of::<GitStageFile>(), GitStageFile::action_name());
    names.insert(TypeId::of::<GitStageAll>(), GitStageAll::action_name());
    names.insert(
        TypeId::of::<GitUnstageFile>(),
        GitUnstageFile::action_name(),
    );
    names.insert(TypeId::of::<GitUnstageAll>(), GitUnstageAll::action_name());
    names.insert(TypeId::of::<GitStageHunk>(), GitStageHunk::action_name());
    names.insert(
        TypeId::of::<GitUnstageHunk>(),
        GitUnstageHunk::action_name(),
    );
    names.insert(
        TypeId::of::<GitToggleStageHunk>(),
        GitToggleStageHunk::action_name(),
    );
    names.insert(
        TypeId::of::<GitToggleStageLine>(),
        GitToggleStageLine::action_name(),
    );

    // Line selection actions
    names.insert(
        TypeId::of::<DiffReviewEnterLineSelect>(),
        DiffReviewEnterLineSelect::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewLineSelectToggle>(),
        DiffReviewLineSelectToggle::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewLineSelectAll>(),
        DiffReviewLineSelectAll::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewLineSelectNone>(),
        DiffReviewLineSelectNone::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewLineSelectStage>(),
        DiffReviewLineSelectStage::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewLineSelectUnstage>(),
        DiffReviewLineSelectUnstage::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewLineSelectCancel>(),
        DiffReviewLineSelectCancel::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewLineSelectDown>(),
        DiffReviewLineSelectDown::action_name(),
    );
    names.insert(
        TypeId::of::<DiffReviewLineSelectUp>(),
        DiffReviewLineSelectUp::action_name(),
    );

    // Buffer finder actions
    names.insert(
        TypeId::of::<OpenBufferFinder>(),
        OpenBufferFinder::action_name(),
    );
    names.insert(
        TypeId::of::<BufferFinderNext>(),
        BufferFinderNext::action_name(),
    );
    names.insert(
        TypeId::of::<BufferFinderPrev>(),
        BufferFinderPrev::action_name(),
    );
    names.insert(
        TypeId::of::<BufferFinderSelect>(),
        BufferFinderSelect::action_name(),
    );
    names.insert(
        TypeId::of::<BufferFinderDismiss>(),
        BufferFinderDismiss::action_name(),
    );

    // Pane management actions
    names.insert(TypeId::of::<SplitUp>(), SplitUp::action_name());
    names.insert(TypeId::of::<SplitDown>(), SplitDown::action_name());
    names.insert(TypeId::of::<SplitLeft>(), SplitLeft::action_name());
    names.insert(TypeId::of::<SplitRight>(), SplitRight::action_name());
    names.insert(TypeId::of::<Quit>(), Quit::action_name());
    names.insert(TypeId::of::<FocusPaneUp>(), FocusPaneUp::action_name());
    names.insert(TypeId::of::<FocusPaneDown>(), FocusPaneDown::action_name());
    names.insert(TypeId::of::<FocusPaneLeft>(), FocusPaneLeft::action_name());
    names.insert(
        TypeId::of::<FocusPaneRight>(),
        FocusPaneRight::action_name(),
    );
    names.insert(TypeId::of::<CloseBuffer>(), CloseBuffer::action_name());
    names.insert(
        TypeId::of::<CloseOtherPanes>(),
        CloseOtherPanes::action_name(),
    );

    // Application actions
    names.insert(TypeId::of::<QuitAll>(), QuitAll::action_name());
    names.insert(TypeId::of::<WriteFile>(), WriteFile::action_name());
    names.insert(TypeId::of::<WriteAll>(), WriteAll::action_name());

    // View actions
    names.insert(TypeId::of::<ToggleMinimap>(), ToggleMinimap::action_name());
    names.insert(
        TypeId::of::<ShowMinimapOnScroll>(),
        ShowMinimapOnScroll::action_name(),
    );

    // Help actions
    names.insert(
        TypeId::of::<OpenHelpOverlay>(),
        OpenHelpOverlay::action_name(),
    );
    names.insert(TypeId::of::<OpenHelpModal>(), OpenHelpModal::action_name());
    names.insert(
        TypeId::of::<HelpModalDismiss>(),
        HelpModalDismiss::action_name(),
    );
    names.insert(
        TypeId::of::<OpenAboutModal>(),
        OpenAboutModal::action_name(),
    );
    names.insert(
        TypeId::of::<AboutModalDismiss>(),
        AboutModalDismiss::action_name(),
    );

    // Command line actions
    names.insert(
        TypeId::of::<PrintWorkingDirectory>(),
        PrintWorkingDirectory::action_name(),
    );

    // KeyContext and Mode actions
    names.insert(TypeId::of::<SetKeyContext>(), SetKeyContext::action_name());
    names.insert(TypeId::of::<SetMode>(), SetMode::action_name());

    names
});

/// Map from TypeId to action description
pub static DESCRIPTIONS: LazyLock<HashMap<TypeId, &'static str>> = LazyLock::new(|| {
    let mut descriptions = HashMap::new();

    // Movement actions
    descriptions.insert(TypeId::of::<MoveUp>(), MoveUp::description());
    descriptions.insert(TypeId::of::<MoveDown>(), MoveDown::description());
    descriptions.insert(TypeId::of::<MoveLeft>(), MoveLeft::description());
    descriptions.insert(TypeId::of::<MoveRight>(), MoveRight::description());
    descriptions.insert(TypeId::of::<MoveWordLeft>(), MoveWordLeft::description());
    descriptions.insert(TypeId::of::<MoveWordRight>(), MoveWordRight::description());
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
    descriptions.insert(TypeId::of::<PageUp>(), PageUp::description());
    descriptions.insert(TypeId::of::<PageDown>(), PageDown::description());
    descriptions.insert(
        TypeId::of::<MoveNextWordEnd>(),
        MoveNextWordEnd::description(),
    );
    descriptions.insert(
        TypeId::of::<MoveNextLongWordEnd>(),
        MoveNextLongWordEnd::description(),
    );
    descriptions.insert(
        TypeId::of::<FindCharForward>(),
        FindCharForward::description(),
    );
    descriptions.insert(
        TypeId::of::<FindCharBackward>(),
        FindCharBackward::description(),
    );
    descriptions.insert(
        TypeId::of::<TillCharForward>(),
        TillCharForward::description(),
    );
    descriptions.insert(
        TypeId::of::<TillCharBackward>(),
        TillCharBackward::description(),
    );
    descriptions.insert(
        TypeId::of::<MoveToFirstNonWhitespace>(),
        MoveToFirstNonWhitespace::description(),
    );
    descriptions.insert(TypeId::of::<HalfPageUp>(), HalfPageUp::description());
    descriptions.insert(TypeId::of::<HalfPageDown>(), HalfPageDown::description());

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
    descriptions.insert(TypeId::of::<OpenLineBelow>(), OpenLineBelow::description());
    descriptions.insert(TypeId::of::<OpenLineAbove>(), OpenLineAbove::description());
    descriptions.insert(TypeId::of::<Append>(), Append::description());
    descriptions.insert(
        TypeId::of::<AppendAtLineEnd>(),
        AppendAtLineEnd::description(),
    );
    descriptions.insert(
        TypeId::of::<InsertAtLineStart>(),
        InsertAtLineStart::description(),
    );
    descriptions.insert(
        TypeId::of::<DeleteSelection>(),
        DeleteSelection::description(),
    );
    descriptions.insert(
        TypeId::of::<ChangeSelection>(),
        ChangeSelection::description(),
    );
    descriptions.insert(TypeId::of::<SelectAll>(), SelectAll::description());
    descriptions.insert(TypeId::of::<Yank>(), Yank::description());
    descriptions.insert(TypeId::of::<PasteAfter>(), PasteAfter::description());
    descriptions.insert(TypeId::of::<PasteBefore>(), PasteBefore::description());
    descriptions.insert(TypeId::of::<JoinLines>(), JoinLines::description());
    descriptions.insert(TypeId::of::<Indent>(), Indent::description());
    descriptions.insert(TypeId::of::<Outdent>(), Outdent::description());
    descriptions.insert(TypeId::of::<Lowercase>(), Lowercase::description());
    descriptions.insert(TypeId::of::<Uppercase>(), Uppercase::description());
    descriptions.insert(TypeId::of::<SwapCase>(), SwapCase::description());
    descriptions.insert(TypeId::of::<ReplaceChar>(), ReplaceChar::description());

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
    descriptions.insert(
        TypeId::of::<EnterGitFilterMode>(),
        EnterGitFilterMode::description(),
    );

    // Selection actions
    descriptions.insert(
        TypeId::of::<MoveNextWordStart>(),
        MoveNextWordStart::description(),
    );
    descriptions.insert(
        TypeId::of::<MovePrevWordStart>(),
        MovePrevWordStart::description(),
    );
    descriptions.insert(
        TypeId::of::<MoveNextLongWordStart>(),
        MoveNextLongWordStart::description(),
    );
    descriptions.insert(
        TypeId::of::<MovePrevLongWordStart>(),
        MovePrevLongWordStart::description(),
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
    descriptions.insert(
        TypeId::of::<SplitSelectionIntoLines>(),
        SplitSelectionIntoLines::description(),
    );
    descriptions.insert(TypeId::of::<SelectNext>(), SelectNext::description());
    descriptions.insert(
        TypeId::of::<SelectPrevious>(),
        SelectPrevious::description(),
    );
    descriptions.insert(
        TypeId::of::<SelectAllMatches>(),
        SelectAllMatches::description(),
    );
    descriptions.insert(
        TypeId::of::<AddSelectionAbove>(),
        AddSelectionAbove::description(),
    );
    descriptions.insert(
        TypeId::of::<AddSelectionBelow>(),
        AddSelectionBelow::description(),
    );
    descriptions.insert(
        TypeId::of::<CollapseSelection>(),
        CollapseSelection::description(),
    );
    descriptions.insert(
        TypeId::of::<KeepPrimarySelection>(),
        KeepPrimarySelection::description(),
    );
    descriptions.insert(TypeId::of::<FlipSelection>(), FlipSelection::description());
    descriptions.insert(
        TypeId::of::<ExtendNextWordEnd>(),
        ExtendNextWordEnd::description(),
    );
    descriptions.insert(
        TypeId::of::<ExtendNextLongWordEnd>(),
        ExtendNextLongWordEnd::description(),
    );
    descriptions.insert(TypeId::of::<SelectLine>(), SelectLine::description());
    descriptions.insert(TypeId::of::<SelectRegex>(), SelectRegex::description());

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
    descriptions.insert(
        TypeId::of::<ToggleCommandPaletteHidden>(),
        ToggleCommandPaletteHidden::description(),
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
    descriptions.insert(
        TypeId::of::<GitStatusCycleFilter>(),
        GitStatusCycleFilter::description(),
    );
    descriptions.insert(
        TypeId::of::<GitStatusSetFilterAll>(),
        GitStatusSetFilterAll::description(),
    );
    descriptions.insert(
        TypeId::of::<GitStatusSetFilterStaged>(),
        GitStatusSetFilterStaged::description(),
    );
    descriptions.insert(
        TypeId::of::<GitStatusSetFilterUnstaged>(),
        GitStatusSetFilterUnstaged::description(),
    );
    descriptions.insert(
        TypeId::of::<GitStatusSetFilterUnstagedWithUntracked>(),
        GitStatusSetFilterUnstagedWithUntracked::description(),
    );
    descriptions.insert(
        TypeId::of::<GitStatusSetFilterUntracked>(),
        GitStatusSetFilterUntracked::description(),
    );

    // Git diff hunk actions
    descriptions.insert(
        TypeId::of::<ToggleDiffHunk>(),
        ToggleDiffHunk::description(),
    );
    descriptions.insert(TypeId::of::<GotoNextHunk>(), GotoNextHunk::description());
    descriptions.insert(TypeId::of::<GotoPrevHunk>(), GotoPrevHunk::description());

    // Diff review actions
    descriptions.insert(
        TypeId::of::<OpenDiffReview>(),
        OpenDiffReview::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewNextHunk>(),
        DiffReviewNextHunk::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewPrevHunk>(),
        DiffReviewPrevHunk::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewApproveHunk>(),
        DiffReviewApproveHunk::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewToggleApproval>(),
        DiffReviewToggleApproval::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewNextUnreviewedHunk>(),
        DiffReviewNextUnreviewedHunk::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewResetProgress>(),
        DiffReviewResetProgress::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewDismiss>(),
        DiffReviewDismiss::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewCycleComparisonMode>(),
        DiffReviewCycleComparisonMode::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewPreviousCommit>(),
        DiffReviewPreviousCommit::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewRevertHunk>(),
        DiffReviewRevertHunk::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewToggleFollow>(),
        DiffReviewToggleFollow::description(),
    );

    // Conflict review actions
    descriptions.insert(
        TypeId::of::<OpenConflictReview>(),
        OpenConflictReview::description(),
    );
    descriptions.insert(
        TypeId::of::<ConflictReviewDismiss>(),
        ConflictReviewDismiss::description(),
    );
    descriptions.insert(
        TypeId::of::<ConflictAcceptOurs>(),
        ConflictAcceptOurs::description(),
    );
    descriptions.insert(
        TypeId::of::<ConflictAcceptTheirs>(),
        ConflictAcceptTheirs::description(),
    );
    descriptions.insert(
        TypeId::of::<ConflictAcceptBoth>(),
        ConflictAcceptBoth::description(),
    );
    descriptions.insert(
        TypeId::of::<ConflictNextConflict>(),
        ConflictNextConflict::description(),
    );
    descriptions.insert(
        TypeId::of::<ConflictPrevConflict>(),
        ConflictPrevConflict::description(),
    );

    // Git repository actions
    descriptions.insert(TypeId::of::<GitStageFile>(), GitStageFile::description());
    descriptions.insert(TypeId::of::<GitStageAll>(), GitStageAll::description());
    descriptions.insert(
        TypeId::of::<GitUnstageFile>(),
        GitUnstageFile::description(),
    );
    descriptions.insert(TypeId::of::<GitUnstageAll>(), GitUnstageAll::description());
    descriptions.insert(TypeId::of::<GitStageHunk>(), GitStageHunk::description());
    descriptions.insert(
        TypeId::of::<GitUnstageHunk>(),
        GitUnstageHunk::description(),
    );
    descriptions.insert(
        TypeId::of::<GitToggleStageHunk>(),
        GitToggleStageHunk::description(),
    );
    descriptions.insert(
        TypeId::of::<GitToggleStageLine>(),
        GitToggleStageLine::description(),
    );

    // Line selection actions
    descriptions.insert(
        TypeId::of::<DiffReviewEnterLineSelect>(),
        DiffReviewEnterLineSelect::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewLineSelectToggle>(),
        DiffReviewLineSelectToggle::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewLineSelectAll>(),
        DiffReviewLineSelectAll::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewLineSelectNone>(),
        DiffReviewLineSelectNone::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewLineSelectStage>(),
        DiffReviewLineSelectStage::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewLineSelectUnstage>(),
        DiffReviewLineSelectUnstage::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewLineSelectCancel>(),
        DiffReviewLineSelectCancel::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewLineSelectDown>(),
        DiffReviewLineSelectDown::description(),
    );
    descriptions.insert(
        TypeId::of::<DiffReviewLineSelectUp>(),
        DiffReviewLineSelectUp::description(),
    );

    // Buffer finder actions
    descriptions.insert(
        TypeId::of::<OpenBufferFinder>(),
        OpenBufferFinder::description(),
    );
    descriptions.insert(
        TypeId::of::<BufferFinderNext>(),
        BufferFinderNext::description(),
    );
    descriptions.insert(
        TypeId::of::<BufferFinderPrev>(),
        BufferFinderPrev::description(),
    );
    descriptions.insert(
        TypeId::of::<BufferFinderSelect>(),
        BufferFinderSelect::description(),
    );
    descriptions.insert(
        TypeId::of::<BufferFinderDismiss>(),
        BufferFinderDismiss::description(),
    );

    // Pane management actions
    descriptions.insert(TypeId::of::<SplitUp>(), SplitUp::description());
    descriptions.insert(TypeId::of::<SplitDown>(), SplitDown::description());
    descriptions.insert(TypeId::of::<SplitLeft>(), SplitLeft::description());
    descriptions.insert(TypeId::of::<SplitRight>(), SplitRight::description());
    descriptions.insert(TypeId::of::<Quit>(), Quit::description());
    descriptions.insert(TypeId::of::<FocusPaneUp>(), FocusPaneUp::description());
    descriptions.insert(TypeId::of::<FocusPaneDown>(), FocusPaneDown::description());
    descriptions.insert(TypeId::of::<FocusPaneLeft>(), FocusPaneLeft::description());
    descriptions.insert(
        TypeId::of::<FocusPaneRight>(),
        FocusPaneRight::description(),
    );
    descriptions.insert(TypeId::of::<CloseBuffer>(), CloseBuffer::description());
    descriptions.insert(
        TypeId::of::<CloseOtherPanes>(),
        CloseOtherPanes::description(),
    );

    // Application actions
    descriptions.insert(TypeId::of::<QuitAll>(), QuitAll::description());
    descriptions.insert(TypeId::of::<WriteFile>(), WriteFile::description());
    descriptions.insert(TypeId::of::<WriteAll>(), WriteAll::description());

    // View actions
    descriptions.insert(TypeId::of::<ToggleMinimap>(), ToggleMinimap::description());
    descriptions.insert(
        TypeId::of::<ShowMinimapOnScroll>(),
        ShowMinimapOnScroll::description(),
    );

    // Help actions
    descriptions.insert(
        TypeId::of::<OpenHelpOverlay>(),
        OpenHelpOverlay::description(),
    );
    descriptions.insert(TypeId::of::<OpenHelpModal>(), OpenHelpModal::description());
    descriptions.insert(
        TypeId::of::<HelpModalDismiss>(),
        HelpModalDismiss::description(),
    );
    descriptions.insert(
        TypeId::of::<OpenAboutModal>(),
        OpenAboutModal::description(),
    );
    descriptions.insert(
        TypeId::of::<AboutModalDismiss>(),
        AboutModalDismiss::description(),
    );

    // Command line actions
    descriptions.insert(
        TypeId::of::<PrintWorkingDirectory>(),
        PrintWorkingDirectory::description(),
    );

    // KeyContext and Mode actions
    descriptions.insert(TypeId::of::<SetKeyContext>(), SetKeyContext::description());
    descriptions.insert(TypeId::of::<SetMode>(), SetMode::description());

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
    help.insert(TypeId::of::<MoveUp>(), MoveUp::help_text());
    help.insert(TypeId::of::<MoveDown>(), MoveDown::help_text());
    help.insert(TypeId::of::<MoveLeft>(), MoveLeft::help_text());
    help.insert(TypeId::of::<MoveRight>(), MoveRight::help_text());
    help.insert(TypeId::of::<MoveWordLeft>(), MoveWordLeft::help_text());
    help.insert(TypeId::of::<MoveWordRight>(), MoveWordRight::help_text());
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
    help.insert(
        TypeId::of::<MoveNextWordEnd>(),
        MoveNextWordEnd::help_text(),
    );
    help.insert(
        TypeId::of::<MoveNextLongWordEnd>(),
        MoveNextLongWordEnd::help_text(),
    );
    help.insert(
        TypeId::of::<FindCharForward>(),
        FindCharForward::help_text(),
    );
    help.insert(
        TypeId::of::<FindCharBackward>(),
        FindCharBackward::help_text(),
    );
    help.insert(
        TypeId::of::<TillCharForward>(),
        TillCharForward::help_text(),
    );
    help.insert(
        TypeId::of::<TillCharBackward>(),
        TillCharBackward::help_text(),
    );
    help.insert(
        TypeId::of::<MoveToFirstNonWhitespace>(),
        MoveToFirstNonWhitespace::help_text(),
    );
    help.insert(TypeId::of::<HalfPageUp>(), HalfPageUp::help_text());
    help.insert(TypeId::of::<HalfPageDown>(), HalfPageDown::help_text());

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
    help.insert(TypeId::of::<OpenLineBelow>(), OpenLineBelow::help_text());
    help.insert(TypeId::of::<OpenLineAbove>(), OpenLineAbove::help_text());
    help.insert(TypeId::of::<Append>(), Append::help_text());
    help.insert(
        TypeId::of::<AppendAtLineEnd>(),
        AppendAtLineEnd::help_text(),
    );
    help.insert(
        TypeId::of::<InsertAtLineStart>(),
        InsertAtLineStart::help_text(),
    );
    help.insert(
        TypeId::of::<DeleteSelection>(),
        DeleteSelection::help_text(),
    );
    help.insert(
        TypeId::of::<ChangeSelection>(),
        ChangeSelection::help_text(),
    );
    help.insert(TypeId::of::<SelectAll>(), SelectAll::help_text());
    help.insert(TypeId::of::<Yank>(), Yank::help_text());
    help.insert(TypeId::of::<PasteAfter>(), PasteAfter::help_text());
    help.insert(TypeId::of::<PasteBefore>(), PasteBefore::help_text());
    help.insert(TypeId::of::<JoinLines>(), JoinLines::help_text());
    help.insert(TypeId::of::<Indent>(), Indent::help_text());
    help.insert(TypeId::of::<Outdent>(), Outdent::help_text());
    help.insert(TypeId::of::<Lowercase>(), Lowercase::help_text());
    help.insert(TypeId::of::<Uppercase>(), Uppercase::help_text());
    help.insert(TypeId::of::<SwapCase>(), SwapCase::help_text());
    help.insert(TypeId::of::<ReplaceChar>(), ReplaceChar::help_text());

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
    help.insert(
        TypeId::of::<EnterGitFilterMode>(),
        EnterGitFilterMode::help_text(),
    );

    // Selection actions
    help.insert(
        TypeId::of::<MoveNextWordStart>(),
        MoveNextWordStart::help_text(),
    );
    help.insert(
        TypeId::of::<MovePrevWordStart>(),
        MovePrevWordStart::help_text(),
    );
    help.insert(
        TypeId::of::<MoveNextLongWordStart>(),
        MoveNextLongWordStart::help_text(),
    );
    help.insert(
        TypeId::of::<MovePrevLongWordStart>(),
        MovePrevLongWordStart::help_text(),
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
    help.insert(
        TypeId::of::<SplitSelectionIntoLines>(),
        SplitSelectionIntoLines::help_text(),
    );
    help.insert(TypeId::of::<SelectNext>(), SelectNext::help_text());
    help.insert(TypeId::of::<SelectPrevious>(), SelectPrevious::help_text());
    help.insert(
        TypeId::of::<SelectAllMatches>(),
        SelectAllMatches::help_text(),
    );
    help.insert(
        TypeId::of::<AddSelectionAbove>(),
        AddSelectionAbove::help_text(),
    );
    help.insert(
        TypeId::of::<AddSelectionBelow>(),
        AddSelectionBelow::help_text(),
    );
    help.insert(
        TypeId::of::<CollapseSelection>(),
        CollapseSelection::help_text(),
    );
    help.insert(
        TypeId::of::<KeepPrimarySelection>(),
        KeepPrimarySelection::help_text(),
    );
    help.insert(TypeId::of::<FlipSelection>(), FlipSelection::help_text());
    help.insert(
        TypeId::of::<ExtendNextWordEnd>(),
        ExtendNextWordEnd::help_text(),
    );
    help.insert(
        TypeId::of::<ExtendNextLongWordEnd>(),
        ExtendNextLongWordEnd::help_text(),
    );
    help.insert(TypeId::of::<SelectLine>(), SelectLine::help_text());
    help.insert(TypeId::of::<SelectRegex>(), SelectRegex::help_text());

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
    help.insert(
        TypeId::of::<ToggleCommandPaletteHidden>(),
        ToggleCommandPaletteHidden::help_text(),
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
    help.insert(
        TypeId::of::<GitStatusCycleFilter>(),
        GitStatusCycleFilter::help_text(),
    );
    help.insert(
        TypeId::of::<GitStatusSetFilterAll>(),
        GitStatusSetFilterAll::help_text(),
    );
    help.insert(
        TypeId::of::<GitStatusSetFilterStaged>(),
        GitStatusSetFilterStaged::help_text(),
    );
    help.insert(
        TypeId::of::<GitStatusSetFilterUnstaged>(),
        GitStatusSetFilterUnstaged::help_text(),
    );
    help.insert(
        TypeId::of::<GitStatusSetFilterUnstagedWithUntracked>(),
        GitStatusSetFilterUnstagedWithUntracked::help_text(),
    );
    help.insert(
        TypeId::of::<GitStatusSetFilterUntracked>(),
        GitStatusSetFilterUntracked::help_text(),
    );

    // Git diff hunk actions
    help.insert(TypeId::of::<ToggleDiffHunk>(), ToggleDiffHunk::help_text());
    help.insert(TypeId::of::<GotoNextHunk>(), GotoNextHunk::help_text());
    help.insert(TypeId::of::<GotoPrevHunk>(), GotoPrevHunk::help_text());

    // Diff review actions
    help.insert(TypeId::of::<OpenDiffReview>(), OpenDiffReview::help_text());
    help.insert(
        TypeId::of::<DiffReviewNextHunk>(),
        DiffReviewNextHunk::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewPrevHunk>(),
        DiffReviewPrevHunk::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewApproveHunk>(),
        DiffReviewApproveHunk::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewToggleApproval>(),
        DiffReviewToggleApproval::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewNextUnreviewedHunk>(),
        DiffReviewNextUnreviewedHunk::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewResetProgress>(),
        DiffReviewResetProgress::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewDismiss>(),
        DiffReviewDismiss::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewCycleComparisonMode>(),
        DiffReviewCycleComparisonMode::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewPreviousCommit>(),
        DiffReviewPreviousCommit::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewRevertHunk>(),
        DiffReviewRevertHunk::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewToggleFollow>(),
        DiffReviewToggleFollow::help_text(),
    );

    // Conflict review actions
    help.insert(
        TypeId::of::<OpenConflictReview>(),
        OpenConflictReview::help_text(),
    );
    help.insert(
        TypeId::of::<ConflictReviewDismiss>(),
        ConflictReviewDismiss::help_text(),
    );
    help.insert(
        TypeId::of::<ConflictAcceptOurs>(),
        ConflictAcceptOurs::help_text(),
    );
    help.insert(
        TypeId::of::<ConflictAcceptTheirs>(),
        ConflictAcceptTheirs::help_text(),
    );
    help.insert(
        TypeId::of::<ConflictAcceptBoth>(),
        ConflictAcceptBoth::help_text(),
    );
    help.insert(
        TypeId::of::<ConflictNextConflict>(),
        ConflictNextConflict::help_text(),
    );
    help.insert(
        TypeId::of::<ConflictPrevConflict>(),
        ConflictPrevConflict::help_text(),
    );

    // Git repository actions
    help.insert(TypeId::of::<GitStageFile>(), GitStageFile::help_text());
    help.insert(TypeId::of::<GitStageAll>(), GitStageAll::help_text());
    help.insert(TypeId::of::<GitUnstageFile>(), GitUnstageFile::help_text());
    help.insert(TypeId::of::<GitUnstageAll>(), GitUnstageAll::help_text());
    help.insert(TypeId::of::<GitStageHunk>(), GitStageHunk::help_text());
    help.insert(TypeId::of::<GitUnstageHunk>(), GitUnstageHunk::help_text());
    help.insert(
        TypeId::of::<GitToggleStageHunk>(),
        GitToggleStageHunk::help_text(),
    );
    help.insert(
        TypeId::of::<GitToggleStageLine>(),
        GitToggleStageLine::help_text(),
    );

    // Line selection actions
    help.insert(
        TypeId::of::<DiffReviewEnterLineSelect>(),
        DiffReviewEnterLineSelect::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewLineSelectToggle>(),
        DiffReviewLineSelectToggle::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewLineSelectAll>(),
        DiffReviewLineSelectAll::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewLineSelectNone>(),
        DiffReviewLineSelectNone::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewLineSelectStage>(),
        DiffReviewLineSelectStage::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewLineSelectUnstage>(),
        DiffReviewLineSelectUnstage::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewLineSelectCancel>(),
        DiffReviewLineSelectCancel::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewLineSelectDown>(),
        DiffReviewLineSelectDown::help_text(),
    );
    help.insert(
        TypeId::of::<DiffReviewLineSelectUp>(),
        DiffReviewLineSelectUp::help_text(),
    );

    // Buffer finder actions
    help.insert(
        TypeId::of::<OpenBufferFinder>(),
        OpenBufferFinder::help_text(),
    );
    help.insert(
        TypeId::of::<BufferFinderNext>(),
        BufferFinderNext::help_text(),
    );
    help.insert(
        TypeId::of::<BufferFinderPrev>(),
        BufferFinderPrev::help_text(),
    );
    help.insert(
        TypeId::of::<BufferFinderSelect>(),
        BufferFinderSelect::help_text(),
    );
    help.insert(
        TypeId::of::<BufferFinderDismiss>(),
        BufferFinderDismiss::help_text(),
    );

    // Pane management actions
    help.insert(TypeId::of::<SplitUp>(), SplitUp::help_text());
    help.insert(TypeId::of::<SplitDown>(), SplitDown::help_text());
    help.insert(TypeId::of::<SplitLeft>(), SplitLeft::help_text());
    help.insert(TypeId::of::<SplitRight>(), SplitRight::help_text());
    help.insert(TypeId::of::<Quit>(), Quit::help_text());
    help.insert(TypeId::of::<FocusPaneUp>(), FocusPaneUp::help_text());
    help.insert(TypeId::of::<FocusPaneDown>(), FocusPaneDown::help_text());
    help.insert(TypeId::of::<FocusPaneLeft>(), FocusPaneLeft::help_text());
    help.insert(TypeId::of::<FocusPaneRight>(), FocusPaneRight::help_text());
    help.insert(TypeId::of::<CloseBuffer>(), CloseBuffer::help_text());
    help.insert(
        TypeId::of::<CloseOtherPanes>(),
        CloseOtherPanes::help_text(),
    );

    // Application actions
    help.insert(TypeId::of::<QuitAll>(), QuitAll::help_text());
    help.insert(TypeId::of::<WriteFile>(), WriteFile::help_text());
    help.insert(TypeId::of::<WriteAll>(), WriteAll::help_text());

    // View actions
    help.insert(TypeId::of::<ToggleMinimap>(), ToggleMinimap::help_text());
    help.insert(
        TypeId::of::<ShowMinimapOnScroll>(),
        ShowMinimapOnScroll::help_text(),
    );

    // Help actions
    help.insert(
        TypeId::of::<OpenHelpOverlay>(),
        OpenHelpOverlay::help_text(),
    );
    help.insert(TypeId::of::<OpenHelpModal>(), OpenHelpModal::help_text());
    help.insert(
        TypeId::of::<HelpModalDismiss>(),
        HelpModalDismiss::help_text(),
    );
    help.insert(TypeId::of::<OpenAboutModal>(), OpenAboutModal::help_text());
    help.insert(
        TypeId::of::<AboutModalDismiss>(),
        AboutModalDismiss::help_text(),
    );

    // Command line actions
    help.insert(
        TypeId::of::<PrintWorkingDirectory>(),
        PrintWorkingDirectory::help_text(),
    );

    // KeyContext and Mode actions
    help.insert(TypeId::of::<SetKeyContext>(), SetKeyContext::help_text());
    help.insert(TypeId::of::<SetMode>(), SetMode::help_text());

    help
});

/// Get the help text for a given action.
pub fn help_text(action: &dyn Action) -> Option<&'static str> {
    HELP_TEXT.get(&action.type_id()).copied()
}

/// Get help text for an action by its string name (e.g., "MoveLeft").
pub fn help_text_by_name(name: &str) -> Option<&'static str> {
    static HELP_BY_NAME: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
        let mut map = HashMap::new();
        for (type_id, action_name) in ACTION_NAMES.iter() {
            if let Some(text) = HELP_TEXT.get(type_id) {
                map.insert(*action_name, *text);
            }
        }
        map
    });
    HELP_BY_NAME.get(name).copied()
}

/// Map from TypeId to action aliases
pub static ALIASES: LazyLock<HashMap<TypeId, &'static [&'static str]>> = LazyLock::new(|| {
    let mut aliases = HashMap::new();

    // Pane management actions
    aliases.insert(TypeId::of::<Quit>(), Quit::aliases());
    aliases.insert(TypeId::of::<CloseBuffer>(), CloseBuffer::aliases());
    aliases.insert(TypeId::of::<CloseOtherPanes>(), CloseOtherPanes::aliases());

    // Application actions
    aliases.insert(TypeId::of::<QuitAll>(), QuitAll::aliases());
    aliases.insert(TypeId::of::<WriteFile>(), WriteFile::aliases());
    aliases.insert(TypeId::of::<WriteAll>(), WriteAll::aliases());

    // View actions
    aliases.insert(TypeId::of::<ToggleMinimap>(), ToggleMinimap::aliases());

    // Help actions
    aliases.insert(TypeId::of::<OpenHelpOverlay>(), OpenHelpOverlay::aliases());
    aliases.insert(TypeId::of::<OpenAboutModal>(), OpenAboutModal::aliases());

    // Git repository actions
    aliases.insert(TypeId::of::<GitStageFile>(), GitStageFile::aliases());
    aliases.insert(TypeId::of::<GitStageAll>(), GitStageAll::aliases());
    aliases.insert(TypeId::of::<GitUnstageFile>(), GitUnstageFile::aliases());
    aliases.insert(TypeId::of::<GitUnstageAll>(), GitUnstageAll::aliases());
    aliases.insert(TypeId::of::<GitStageHunk>(), GitStageHunk::aliases());
    aliases.insert(TypeId::of::<GitUnstageHunk>(), GitUnstageHunk::aliases());
    aliases.insert(
        TypeId::of::<GitToggleStageHunk>(),
        GitToggleStageHunk::aliases(),
    );
    aliases.insert(
        TypeId::of::<GitToggleStageLine>(),
        GitToggleStageLine::aliases(),
    );

    // Command line actions
    aliases.insert(
        TypeId::of::<PrintWorkingDirectory>(),
        PrintWorkingDirectory::aliases(),
    );

    aliases
});

/// Get the aliases for a given action.
pub fn aliases(action: &dyn Action) -> &'static [&'static str] {
    ALIASES.get(&action.type_id()).copied().unwrap_or(&[])
}

/// Map from TypeId to hidden flag for command palette filtering
pub static HIDDEN: LazyLock<HashMap<TypeId, bool>> = LazyLock::new(|| {
    // Actions hidden from command palette by default (dismiss actions, etc.)
    // Hidden actions are marked via the action_metadata! macro with the 'hidden' parameter

    HashMap::new()
});

/// Get the hidden flag for a given action.
pub fn hidden(action: &dyn Action) -> bool {
    HIDDEN.get(&action.type_id()).copied().unwrap_or(false)
}

mod about_modal;
mod buffer_finder;
mod command_line;
mod command_palette;
mod command_palette_v2;
mod edit;
mod file_finder;
mod git;
mod help_modal;
mod help_overlay;
mod minimap;
mod mode;
#[allow(clippy::module_inception)]
mod r#move;
pub(crate) use r#move::find_char::FindCharMode;
mod pane;
mod scroll;
mod select;
mod set_key_context;
mod set_mode;
mod write_file;
