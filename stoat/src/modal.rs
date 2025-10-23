//! Modal dialog modules.
//!
//! Provides modal dialogs for about information and help documentation.

pub mod about;
pub mod help;

// Re-export modal components
pub use about::AboutModal;
pub use help::HelpModal;
