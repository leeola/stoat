//! Editor UI modules.
//!
//! Provides editor element rendering, styling, and view management.

pub mod element;
pub mod style;
pub mod view;

// Re-export main components
pub use element::EditorElement;
pub use style::EditorStyle;
pub use view::EditorView;
