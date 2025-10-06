pub mod actions;
pub mod app;
pub mod command_overlay;
pub mod context;
pub mod cursor;
pub mod editor;
pub mod external_input;
pub mod file_finder;
pub mod input;
pub mod keybinding_hint;
pub mod keymap;
pub mod keymap_query;
pub mod pane_group;
pub mod syntax;

// Re-export the main entry point for convenience
pub use app::run_with_paths;

// Helper function that maintains backward compatibility
pub fn run_with_paths_simple(
    paths: Vec<std::path::PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_with_paths(paths, None)
}
