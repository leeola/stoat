//! Movement operations
//!
//! This module provides commands for cursor navigation throughout the buffer.
//! Movement commands handle basic directional navigation, line boundaries, file
//! boundaries, and page scrolling.
//!
//! # Basic Movement
//!
//! Character-by-character navigation:
//! - [`move_left`] - moves cursor one character left
//! - [`move_right`] - moves cursor one character right
//! - [`move_up`] - moves cursor one line up
//! - [`move_down`] - moves cursor one line down
//!
//! # Line Boundaries
//!
//! Jump to line start or end:
//! - [`move_to_line_start`] - moves cursor to beginning of current line
//! - [`move_to_line_end`] - moves cursor to end of current line
//!
//! # File Boundaries
//!
//! Jump to file start or end:
//! - [`move_to_file_start`] - moves cursor to beginning of buffer
//! - [`move_to_file_end`] - moves cursor to end of buffer
//!
//! # Page Movement
//!
//! Scroll by viewport height:
//! - [`page_up`] - moves cursor up by one page
//! - [`page_down`] - moves cursor down by one page
//!
//! # Goal Column
//!
//! Vertical movement commands ([`move_up`], [`move_down`], [`page_up`], [`page_down`])
//! maintain a "goal column" that preserves the horizontal position when moving through
//! lines of varying length. This allows the cursor to return to its original column
//! position after passing through shorter lines.
//!
//! # Integration
//!
//! This module is part of the [`crate::actions`] system and integrates with:
//! - [`crate::Stoat`] - the main editor state where movement is applied
//! - [`crate::cursor::CursorManager`] - tracks cursor position and goal column
//! - GPUI action system - for keyboard bindings and command dispatch
//! - [`crate::actions::editor_movement`] - the action namespace for movement commands

mod move_down;
mod move_left;
mod move_right;
mod move_to_file_end;
mod move_to_file_start;
mod move_to_line_end;
mod move_to_line_start;
mod move_up;
mod page_down;
mod page_up;
