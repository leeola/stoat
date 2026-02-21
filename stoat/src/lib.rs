//! Stoat - Modern text editor built on GPUI.
//!
//! Combines entity state management with rendering following Zed's Entity-as-View pattern.
//! All Stoat methods take `&mut Context<Self>` enabling self-updating async tasks.

// Core modules
pub mod action_metadata;
pub mod actions;
pub mod buffer;
pub mod build_info;
pub mod char_classifier;
pub mod claude;
pub mod command;
pub mod command_line;
pub mod command_palette_v2;
pub mod config;
pub mod cursor;
pub mod editor;
pub mod environment;
pub mod file_finder;
pub mod git;
pub mod history;
pub mod index;
pub mod inline_editor;
pub mod input_simulator;
pub mod keymap;
pub use stoat_log as log;
pub mod modal;
pub mod pane;
pub mod paths;
pub mod rel_path;
pub mod scroll;
pub mod selections;
pub mod stoat;
pub mod stoat_actions;
pub mod worktree;

// UI modules
pub mod app;
pub mod app_state;
pub mod content_view;
pub mod gutter;
pub mod minimap;
pub mod pane_group;
pub mod render_stats;
pub mod static_view;
pub mod status_bar;
pub mod syntax;

#[cfg(feature = "dev-tools")]
pub mod dev_tools;

#[cfg(test)]
pub mod test;

#[cfg(test)]
mod tests;

// Re-exports
pub use actions::*;
// Re-export action metadata helpers
pub use actions::{action_name, description};
pub use app_state::{LspState, LspStatus};
pub use buffer::{
    BufferItem, BufferListEntry, BufferStore, DisplayBuffer, DisplayRow, OpenBuffer, RowInfo,
};
pub use config::Config;
pub use cursor::{Cursor, CursorManager};
pub use file_finder::PreviewData;
pub use git::{gather_git_status, load_git_diff, DiffPreviewData, GitStatusEntry};
pub use inline_editor::InlineEditor;
pub use pane::{Member, PaneAxis, PaneGroup, PaneId, SplitDirection};
pub use scroll::{ScrollDelta, ScrollPosition};
pub use selections::SelectionsCollection;
pub use stoat::{CommandInfo, Mode, Stoat, StoatEvent};
