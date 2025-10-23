//! Buffer finder open action - now handled by PaneGroupView.
//!
//! The buffer finder state has been moved to WorkspaceState and is managed by
//! PaneGroupView. See:
//! - `PaneGroupView::handle_open_buffer_finder()` for the action handler
//! - `PaneGroupView::update_buffer_finder_list()` for list management
//! - `PaneGroupView::filter_buffer_finder_buffers()` for filtering logic

// FIXME: This file can be removed once all buffer_finder actions are moved to PaneGroupView
