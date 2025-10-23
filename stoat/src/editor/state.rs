//! Multi-cursor state tracking for editor operations.
//!
//! Provides state structures that track ongoing multi-cursor operations across
//! multiple action invocations. These state types are stored in [`EditorView`]
//! and enable stateful multi-cursor workflows.
//!
//! # State Types
//!
//! - [`AddSelectionsState`] - Tracks columnar selection building (AddSelectionAbove/Below)
//! - [`SelectNextState`] - Tracks occurrence-based selection (SelectNext/Prev/SelectAllMatches)
//!
//! # Architecture
//!
//! Based on Zed's proven implementation (`zed/crates/editor/src/editor.rs:1497-1512`).
//! State is stored as `Option<T>` in EditorView and cleared when selections change
//! or actions complete.
//!
//! [`EditorView`]: crate::editor::view::EditorView

/// State for AddSelectionAbove/Below actions.
///
/// Tracks groups of columnar selections being built by repeated invocations
/// of [`crate::actions::AddSelectionAbove`] or [`crate::actions::AddSelectionBelow`].
///
/// Each group represents a stack of vertically aligned selections created by
/// repeatedly pressing the add-selection keybinding. Multiple groups can exist
/// when starting from multiple initial selections.
///
/// # Related
///
/// - Cleared when selection count drops to 1
/// - Used by columnar selection helper functions in Phase 1.2
/// - Based on Zed's `AddSelectionsState` at `editor.rs:1497-1505`
#[derive(Clone, Debug)]
pub struct AddSelectionsState {
    /// Groups of columnar selections, one per original selection
    pub groups: Vec<AddSelectionsGroup>,
}

/// A group of columnar selections moving in one direction.
///
/// Represents a stack of vertically aligned cursors created by repeatedly
/// invoking AddSelectionAbove or AddSelectionBelow. The `stack` contains
/// selection IDs in the order they were created.
///
/// # Fields
///
/// - `above`: Direction of growth (true = moving up, false = moving down)
/// - `stack`: Selection IDs in this columnar group, ordered by creation time
///
/// # Related
///
/// - Part of [`AddSelectionsState`]
/// - Based on Zed's `AddSelectionsGroup` at `editor.rs:1502-1505`
#[derive(Clone, Debug)]
pub struct AddSelectionsGroup {
    /// Direction: true = above (moving up), false = below (moving down)
    pub above: bool,

    /// Selection IDs in this columnar group, in creation order
    pub stack: Vec<usize>,
}

/// State for SelectNext/SelectPrevious/SelectAllMatches actions.
///
/// Tracks the search query and iteration state for occurrence-based multi-cursor
/// selection. Created on first invocation of [`crate::actions::SelectNext`] and
/// reused for subsequent invocations with the same query.
///
/// # Fields
///
/// - `query`: Text to search for (extracted from current selection)
/// - `wordwise`: Whether to match whole words only (set when selection is a word)
/// - `done`: Whether search has wrapped around or exhausted all matches
///
/// # State Lifecycle
///
/// 1. Created on first SelectNext invocation with selected text as query
/// 2. Reused on subsequent invocations if query matches current selection
/// 3. Reset if selection text changes
/// 4. Cleared when selections change outside of SelectNext
///
/// # Future Enhancement
///
/// Currently uses simple String matching. Phase 2+ may integrate `aho-corasick`
/// crate for faster multi-pattern searching, matching Zed's implementation
/// at `editor.rs:1508-1512`.
///
/// # Related
///
/// - Used by both SelectNext and SelectPrevious (stored in separate fields)
/// - Based on Zed's `SelectNextState` at `editor.rs:1508-1512`
#[derive(Clone, Debug)]
pub struct SelectNextState {
    /// Search query text extracted from selection
    pub query: String,

    /// Whether to match whole words only
    pub wordwise: bool,

    /// Whether we've wrapped around buffer or exhausted matches
    pub done: bool,
}
