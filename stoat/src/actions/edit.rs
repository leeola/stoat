//! Text editing action implementations.
//!
//! This module contains actions for basic text editing operations like insertion,
//! deletion, and line manipulation. These actions form the core editing functionality
//! and work together with cursor movement and selection actions to provide a complete
//! text editing experience.
//!
//! # Action Organization
//!
//! - Character deletion: [`delete_left`], [`delete_right`]
//! - Word deletion: [`delete_word_left`], [`delete_word_right`]
//! - Line operations: [`new_line`], [`delete_line`], [`delete_to_end_of_line`]
//! - Text insertion: [`insert_text`]
//!
//! # Integration
//!
//! These actions are dispatched through the [`Stoat`](crate::Stoat) action system and
//! integrate with:
//! - [`Cursor`](crate::cursor::Cursor) for position tracking
//! - [`BufferItem`](crate::buffer::item::BufferItem) for text storage
//! - Modal system for routing input to different buffers (file finder, command palette, etc.)

mod delete_left;
mod delete_line;
mod delete_right;
mod delete_to_end_of_line;
mod delete_word_left;
mod delete_word_right;
mod insert_text;
mod new_line;
