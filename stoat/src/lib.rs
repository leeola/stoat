//! Stoat - Modern text editor built on GPUI.
//!
//! Combines entity state management with rendering following Zed's Entity-as-View pattern.
//! All Stoat methods take `&mut Context<Self>` enabling self-updating async tasks.

// Core modules
pub mod action_metadata;
pub mod actions;
pub mod buffer_item;
pub mod buffer_store;
pub mod build_info;
pub mod config;
pub mod cursor;
pub mod diff_review;
pub mod display_buffer;
pub mod file_finder;
pub mod git_diff;
pub mod git_repository;
pub mod git_status;
pub mod keymap;
pub mod log;
pub mod pane;
pub mod rel_path;
pub mod scroll;
pub mod selections;
pub mod stoat;
pub mod stoat_actions;
pub mod worktree;

// UI modules
pub mod about_modal;
pub mod app;
pub mod command_overlay;
pub mod command_palette;
pub mod content_view;
pub mod dispatch;
pub mod editor_element;
pub mod editor_style;
pub mod editor_view;
pub mod gutter;
pub mod help_modal;
pub mod keybinding_hint;
pub mod keymap_query;
pub mod minimap;
pub mod pane_group;
pub mod render_stats;
pub mod static_view;
pub mod status_bar;
pub mod syntax;
pub mod workspace_state;

#[cfg(test)]
pub mod test;

#[cfg(test)]
mod tests;

// Re-exports
pub use actions::*;
// Re-export action metadata helpers
pub use actions::{action_name, description};
pub use buffer_item::BufferItem;
pub use buffer_store::{BufferListEntry, BufferStore, OpenBuffer};
pub use config::Config;
pub use cursor::{Cursor, CursorManager};
pub use display_buffer::{DisplayBuffer, DisplayRow, RowInfo};
pub use file_finder::PreviewData;
pub use git_status::{gather_git_status, load_git_diff, DiffPreviewData, GitStatusEntry};
pub use pane::{Member, PaneAxis, PaneGroup, PaneId, SplitDirection};
pub use scroll::{ScrollDelta, ScrollPosition};
pub use selections::SelectionsCollection;
pub use stoat::{CommandInfo, Mode, Stoat, StoatEvent};
