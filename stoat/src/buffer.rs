//! Buffer modules.
//!
//! Provides buffer storage, display buffer with git diffs, and buffer items
//! with syntax highlighting.

pub mod display;
pub mod item;
pub mod store;

// Re-export main types
pub use display::{DisplayBuffer, DisplayRow, RowInfo};
pub use item::BufferItem;
pub use store::{BufferListEntry, BufferStore, OpenBuffer};
