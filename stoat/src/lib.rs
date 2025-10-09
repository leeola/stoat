//! Stoat v4 - Editor with proper GPUI Entity pattern.
//!
//! Key architectural change: Stoat methods take `&mut Context<Self>` instead of `&mut App`,
//! enabling self-updating async tasks.

pub mod actions;
pub mod buffer_item;
pub mod cursor;
pub mod file_finder;
pub mod git_diff;
pub mod git_repository;
pub mod git_status;
pub mod keymap;
pub mod log;
pub mod pane;
pub mod rel_path;
pub mod scroll;
pub mod stoat;
pub mod stoat_actions;
pub mod worktree;

#[cfg(test)]
pub mod test;

#[cfg(test)]
mod tests;

// Re-exports
pub use actions::*;
// Re-export action metadata helpers
pub use actions::{action_name, description};
pub use buffer_item::BufferItem;
pub use cursor::{Cursor, CursorManager};
pub use file_finder::PreviewData;
pub use git_status::{gather_git_status, load_git_diff, DiffPreviewData, GitStatusEntry};
pub use pane::{Member, PaneAxis, PaneGroup, PaneId, SplitDirection};
pub use scroll::{ScrollDelta, ScrollPosition};
pub use stoat::{CommandInfo, Mode, Stoat, StoatEvent};
