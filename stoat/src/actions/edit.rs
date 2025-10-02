//! Edit operations
//!
//! This module provides commands for text modification operations. Edit commands
//! handle text insertion, deletion, and buffer manipulation.
//!
//! # Text Insertion
//!
//! - [`insert_text`] - inserts text at cursor position
//!
//! # Deletion Commands
//!
//! Character-level deletion:
//! - [`delete_left`] - deletes character before cursor (backspace)
//! - [`delete_right`] - deletes character after cursor (delete key)
//!
//! Line-level deletion:
//! - [`delete_line`] - deletes entire current line
//! - [`delete_to_end_of_line`] - deletes from cursor to end of line
//!
//! # Helper Functions
//!
//! - [`delete_range`] - internal helper for range-based deletion
//!
//! # Buffer Re-parsing
//!
//! All edit operations automatically trigger a full buffer re-parse to update syntax
//! highlighting and token maps. This ensures that language-aware features remain
//! accurate after modifications.
//!
//! # Implementation Pattern
//!
//! Deletion commands use a common pattern:
//! 1. Calculate the range to delete (as Point positions)
//! 2. Call [`delete_range`] to perform the deletion
//! 3. Update cursor position
//!
//! This ensures consistent behavior and simplifies maintenance.
//!
//! # Integration
//!
//! This module is part of the [`crate::actions`] system and integrates with:
//! - [`crate::Stoat`] - the main editor state where edits are applied
//! - [`text::Buffer`] - the underlying text storage
//! - [`stoat_text_v3::Parser`] - for re-parsing after edits
//! - GPUI action system - for keyboard bindings and command dispatch
//! - [`crate::actions::editor_edit`] - the action namespace for edit commands

mod delete_left;
mod delete_line;
mod delete_range;
mod delete_right;
mod delete_to_end_of_line;
mod insert_text;
