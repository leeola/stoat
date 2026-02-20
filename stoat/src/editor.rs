//! Editor UI modules.
//!
//! Provides editor element rendering, styling, view management, and
//! multi-cursor state tracking.

pub mod element;
pub mod merge;
pub mod state;
pub mod style;
pub mod view;

pub use element::EditorElement;
pub use state::{AddSelectionsGroup, AddSelectionsState, SelectNextState};
pub use style::EditorStyle;
pub use view::EditorView;
