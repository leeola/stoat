pub mod app;
pub mod editor;

// Re-export the main entry points for convenience
pub use app::{run_with_paths, run_with_stoat};
