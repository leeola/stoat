//! Mode transition actions.
//!
//! This module contains actions for switching between different editor modes (insert, normal,
//! visual, space, pane, git_filter). Each mode changes the active keybinding set and determines
//! how the editor responds to input.

mod enter_git_filter;
mod enter_insert;
mod enter_normal;
mod enter_pane;
mod enter_space;
mod enter_visual;
