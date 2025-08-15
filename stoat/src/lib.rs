//! Stoat v2: Core text editor library.
//!
//! This crate provides a data-driven architecture where all business logic
//! is implemented as pure functions operating on immutable state. The core
//! principle is that all state transitions are predictable and testable.
//!
//! # Architecture Overview
//!
//! - [`EditorState`]: Immutable state containing buffer, cursor, modes, etc.
//! - [`EditorEvent`]: Input events using iced types directly
//! - [`Effect`]: Side effects as data (file I/O, clipboard, etc.)
//! - [`EditorAction`]: Pure state transformations
//! - [`EditorEngine`]: Stateful wrapper for convenient API
//!
//! # Example
//!
//! ```rust
//! use stoat::*;
//! use iced::keyboard;
//!
//! let mut engine = EditorEngine::new();
//! let effects = engine.handle_event(EditorEvent::KeyPress {
//!     key: keyboard::Key::Character("i".to_string().into()),
//!     modifiers: keyboard::Modifiers::default()
//! });
//! ```

pub mod actions;
pub mod cli;
pub mod effects;
pub mod engine;
pub mod events;
pub mod log;
pub mod processor;
pub mod state;

#[cfg(test)]
pub mod testing;

// Re-export core types for convenient use
pub use actions::EditorAction;
pub use effects::Effect;
pub use engine::EditorEngine;
pub use events::EditorEvent;
// Re-export commonly used iced types for consumers
pub use iced::{keyboard, mouse, Point};
pub use processor::process_event;
pub use state::EditorState;
