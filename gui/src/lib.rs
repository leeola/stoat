//! GUI: Iced-based graphical interface for the Stoat editor.
//!
//! This crate provides a graphical user interface built on the iced framework.
//! The GUI handles rendering and I/O effects, while all editing logic is
//! implemented in the stoat core library.
//!
//! # Architecture
//!
//! - [`App`]: Main iced application using [`stoat::EditorEngine`]
//! - [`EditorWidget`]: Custom widget that renders [`stoat::EditorState`]
//! - [`EffectRunner`]: Converts [`stoat::Effect`] to iced [`Task`]
//! - Separation: GUI handles rendering and I/O effects

pub mod app;
pub mod command_info;
pub mod editor;
pub mod effect_runner;
pub mod messages;
pub mod theme;
pub mod widget;

// Re-export main types
pub use app::App;
pub use effect_runner::run_effect;
pub use messages::Message;

/// Run the GUI application.
pub fn run() -> iced::Result {
    tracing::info!("Starting Stoat GUI application");
    app::run()
}
