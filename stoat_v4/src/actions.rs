//! Action definitions for stoat_v4.
//!
//! Actions are dispatched through GPUI's action system and handled by [`crate::Stoat`].

use gpui::{actions, Action};

// Editing actions
actions!(
    stoat_v4,
    [
        /// Insert text at cursor position
        InsertText,
        /// Delete character before cursor
        DeleteLeft,
        /// Delete character after cursor
        DeleteRight,
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
        /// Move cursor to start of line
        MoveToLineStart,
        /// Move cursor to end of line
        MoveToLineEnd,
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

// Scroll actions
actions!(
    stoat_v4,
    [
        /// Scroll view
        Scroll,
    ]
);

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
