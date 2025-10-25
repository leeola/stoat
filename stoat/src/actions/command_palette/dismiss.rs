//! Command palette dismiss action - now handled by PaneGroupView.
//!
//! The command palette state has been moved to AppState and is managed by
//! PaneGroupView. See:
//! - `AppState::dismiss_command_palette()` for state cleanup
//! - `PaneGroupView::handle_command_palette_dismiss()` for the action handler

// FIXME: This file can be removed once all command_palette actions are moved to PaneGroupView
