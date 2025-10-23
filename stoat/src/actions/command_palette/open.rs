//! Command palette open action - now handled by PaneGroupView.
//!
//! The command palette state has been moved to WorkspaceState and is managed by
//! PaneGroupView. See:
//! - `WorkspaceState::open_command_palette()` for state initialization
//! - `PaneGroupView::handle_open_command_palette()` for the action handler

// FIXME: This file can be removed once all command_palette actions are moved to PaneGroupView
