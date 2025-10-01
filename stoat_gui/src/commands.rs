//! Command system for Stoat editor - re-exports from core
//!
//! This module re-exports all actions from the stoat core crate. Actions are defined once
//! in core and used by both GUI and headless modes.

// Re-export all actions from stoat core
pub use stoat::actions::*;
