pub mod app;
pub mod canvas;
pub mod grid_layout;
pub mod input;
pub mod state;
pub mod widget;

// Re-export key types
pub use app::{App, Message};
pub use canvas::NodeCanvas;
pub use state::{NodeContent, NodeId, NodeRenderData, NodeState, RenderState, Viewport};

/// Run the GUI application
pub fn run_gui() -> iced::Result {
    App::run()
}
