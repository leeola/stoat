pub mod app;
pub mod command_overlay;
pub mod command_palette;
pub mod dispatch;
pub mod editor_element;
pub mod editor_style;
pub mod editor_view;
pub mod file_finder;
pub mod keybinding_hint;
pub mod keymap_query;
pub mod syntax;

pub use app::run_with_paths;
