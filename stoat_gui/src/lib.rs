pub mod actions;
pub mod app;
pub mod context;
pub mod editor;
pub mod input;
pub mod keymap;

// Re-export the main entry points for convenience
pub use app::{run_with_paths, run_with_stoat};
