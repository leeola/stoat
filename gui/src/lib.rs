pub mod app;
pub mod input;
pub mod widget;

// Re-export key types
pub use app::{App, Message};

/// Run the GUI application
pub fn run_gui() -> iced::Result {
    App::run()
}
