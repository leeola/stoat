pub mod app;
pub mod grid_layout;
pub mod input;
pub mod state;
pub mod widget;

// Re-export key types
pub use app::{App, Message};
pub use state::{NodeContent, NodeRenderData, NodeState, RenderState, Viewport};

/// Run the GUI application
pub fn run_gui() -> iced::Result {
    App::run()
}
