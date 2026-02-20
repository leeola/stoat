//! Git integration modules.
//!
//! Provides git repository operations, diff computation, status tracking,
//! and diff review functionality.

pub mod conflict;
pub mod diff;
pub mod diff_review;
pub mod line_selection;
pub mod repository;
pub mod status;
pub mod watcher;

// Re-export commonly used items from submodules
pub use diff::{BufferDiff, DiffHunk, DiffHunkStatus};
pub use diff_review::DiffReviewFile;
pub use repository::Repository;
pub use status::{gather_git_status, load_git_diff, DiffPreviewData, GitStatusEntry};
