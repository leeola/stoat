//! Cursor movement action implementations.
//!
//! This module contains actions for moving the cursor through the buffer. Movement actions
//! form the foundation of text navigation and work together with selection and editing actions
//! to provide a complete text editing experience.
//!
//! # Action Organization
//!
//! - Basic movement: [`up`], [`down`], [`left`], [`right`]
//! - Word movement: [`word_left`], [`word_right`]
//! - Line navigation: [`to_line_start`], [`to_line_end`]
//! - File navigation: [`to_file_start`], [`to_file_end`]
//! - Page scrolling: [`page_up`], [`page_down`]
//!
//! # Goal Column
//!
//! Vertical movement actions ([`up`], [`down`], [`page_up`], [`page_down`]) preserve a "goal
//! column" which remembers the horizontal position across vertical movements. This allows
//! moving through lines of varying lengths while maintaining the desired column position.
//!
//! # Integration
//!
//! These actions are dispatched through the [`Stoat`](crate::Stoat) action system and
//! integrate with:
//! - [`Cursor`](crate::cursor::Cursor) for position tracking and goal column
//! - [`BufferItem`](crate::buffer::item::BufferItem) for buffer snapshots and bounds checking
//! - [`BufferItem`](crate::buffer::item::BufferItem) for buffer state
//! - Scroll animation system for smooth viewport transitions

mod down;
pub(crate) mod find_char;
mod half_page_down;
mod half_page_up;
mod left;
mod next_long_word_end;
mod next_long_word_start;
mod next_word_end;
mod next_word_start;
mod page_down;
mod page_up;
mod prev_long_word_start;
mod prev_word_start;
mod right;
mod to_file_end;
mod to_file_start;
mod to_first_non_whitespace;
mod to_line_end;
mod to_line_start;
mod up;
mod word_left;
mod word_right;
