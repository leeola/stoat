//! Modal operations
//!
//! This module provides commands for transitioning between editor modes. Stoat uses
//! modal editing inspired by Vim, where the editor behavior changes based on the
//! current mode.
//!
//! # Editor Modes
//!
//! - **Normal mode** - Default mode for navigation and commands
//! - **Insert mode** - Mode for typing and editing text
//! - **Visual mode** - Mode for selecting and manipulating text regions
//!
//! # Mode Transitions
//!
//! - [`enter_normal_mode`] - transition to Normal mode (usually Escape)
//! - [`enter_insert_mode`] - transition to Insert mode (usually 'i', 'a', 'o', etc.)
//! - [`enter_visual_mode`] - transition to Visual mode (usually 'v')
//!
//! # Mode Behavior
//!
//! ## Normal Mode
//!
//! In Normal mode, keypresses execute commands:
//! - `h`, `j`, `k`, `l` - movement
//! - `w`, `b` - word navigation with selection
//! - `dd` - delete line
//! - `i` - enter Insert mode
//! - `v` - enter Visual mode
//!
//! ## Insert Mode
//!
//! In Insert mode, keypresses insert text:
//! - Most keys insert characters
//! - Escape returns to Normal mode
//! - Arrow keys move cursor
//! - Backspace/Delete remove characters
//!
//! ## Visual Mode
//!
//! In Visual mode, movement extends selection:
//! - Movement commands extend selection
//! - `d` or `x` deletes selection
//! - `y` copies selection
//! - Escape returns to Normal mode
//!
//! # Integration
//!
//! This module is part of the [`crate::actions`] system and integrates with:
//! - [`crate::Stoat`] - the main editor state that tracks current mode
//! - [`crate::EditorMode`] - the mode enumeration
//! - GPUI action system - for keyboard bindings and command dispatch
//! - [`crate::actions::editor_modal`] - the action namespace for modal commands

mod enter_insert_mode;
mod enter_normal_mode;
mod enter_visual_mode;
