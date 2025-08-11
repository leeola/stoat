//! Modal input system for handling keyboard input with vim-like modes
//!
//! This module provides a flexible, data-driven modal input system inspired by vim.
//! Modes and keybindings are defined in RON configuration files, making the system
//! fully customizable without code changes.
//!
//! # Future: Context-Aware Bindings
//!
//! The system is designed to eventually support context-aware keybindings where
//! different bindings can be active based on:
//! - The type of node currently selected
//! - Whether there's an active selection
//! - The current view type (canvas, text editor, etc.)
//! - Custom application-defined conditions
//!
//! This will enable sophisticated modal behaviors like:
//! - Different delete behavior for text vs code nodes
//! - Context-sensitive completion based on cursor position
//! - Mode-specific bindings that only activate in certain views
//!
//! The architecture supports adding this without breaking changes - bindings
//! will be able to include conditions that must be met for activation.

pub use action::{Action, Direction, JumpTarget, Mode};
pub use config::{ModalConfig, ModeDefinition};
pub use key::{Key, ModifiedKey, NamedKey};
pub use modal::ModalSystem;
pub use user::UserInput;

pub mod action;
pub mod config;
pub mod key;
pub mod keymap;
pub mod modal;
pub mod user;
