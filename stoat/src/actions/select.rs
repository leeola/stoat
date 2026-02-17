//! Selection action implementations.
//!
//! This module contains actions for creating and extending text selections. Selection actions
//! work together with cursor movement and editing actions to provide flexible text manipulation.
//!
//! # Action Organization
//!
//! - Token-based selection: [`next_symbol`], [`prev_symbol`], [`next_token`], [`prev_token`]
//! - Directional selection: [`left`], [`right`], [`up`], [`down`]
//! - Line-boundary selection: [`to_line_start`], [`to_line_end`]
//! - Multi-cursor operations: [`split_into_lines`]
//!
//! # Selection Model
//!
//! Selections have an anchor (fixed point) and a cursor (moving point). The direction of the
//! selection determines which end is the cursor:
//! - Forward (non-reversed): cursor at end, anchor at start
//! - Backward (reversed): cursor at start, anchor at end
//!
//! # Integration
//!
//! These actions are dispatched through the [`Stoat`](crate::Stoat) action system and
//! integrate with:
//! - [`Cursor`](crate::cursor::Cursor) for selection state management
//! - [`Selection`](crate::cursor::Selection) for anchor/cursor tracking
//! - [`BufferItem`](crate::buffer::item::BufferItem) for buffer state
//! - Visual mode for interactive selection extension

mod above;
mod all_matches;
mod below;
mod down;
mod left;
mod next;
mod next_symbol;
mod next_token;
mod prev_symbol;
mod prev_token;
mod previous;
mod right;
mod split_into_lines;
mod to_line_end;
mod to_line_start;
mod up;
