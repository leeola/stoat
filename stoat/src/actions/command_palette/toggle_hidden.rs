//! Command palette toggle hidden action - now handled by PaneGroupView.
//!
//! The command palette state has been moved to WorkspaceState and is managed by
//! PaneGroupView. See:
//! - `PaneGroupView::handle_command_palette_toggle_hidden()` for the action handler
//! - `PaneGroupView::filter_command_palette_commands()` for the filtering logic

// FIXME: This file can be removed once all command_palette actions are moved to PaneGroupView
