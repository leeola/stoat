pub mod actions;
mod app;
pub mod buffer;
pub mod editor;
pub mod keymap;
pub mod pane;
pub mod view;
pub mod workspace;

pub use actions::Action;
pub use app::{run, Stoat};
pub use buffer::{BufferId, BufferStore, SharedBuffer, TextBuffer};
pub use editor::Editor;
pub use keymap::Key;
pub use pane::Pane;
pub use stoat_log as log;
pub use view::View;
pub use workspace::Workspace;
