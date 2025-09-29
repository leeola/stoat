pub mod actions;
pub mod app;
pub mod commands;
pub mod context;
pub mod cursor;
pub mod editor;
pub mod external_input;
pub mod input;
pub mod keymap;
pub mod modal;
pub mod syntax;

// Re-export the main entry points for convenience
pub use app::{run_with_paths, run_with_stoat};

// Helper function that maintains backward compatibility
pub fn run_with_paths_simple(
    paths: Vec<std::path::PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_with_paths(paths, None)
}
