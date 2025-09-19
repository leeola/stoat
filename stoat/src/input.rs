//! Input type definitions using GPUI types.
//!
//! This module provides the core input types for keyboard and mouse events,
//! using GPUI's type system for better integration with the GUI layer.

// Re-export GPUI types
pub use gpui::{Modifiers, MouseButton, Point as GpuiPoint};

// Point type for mouse positions
pub type Point = GpuiPoint<f32>;

// Key is now a String, matching GPUI's approach
pub type Key = String;

/// Common key constants for easier usage.
///
/// These constants provide standard names for special keys,
/// matching GPUI's string-based key representation.
pub mod keys {
    // Navigation keys
    pub const ESCAPE: &str = "escape";
    pub const ENTER: &str = "enter";
    pub const TAB: &str = "tab";
    pub const BACKSPACE: &str = "backspace";
    pub const DELETE: &str = "delete";
    pub const SPACE: &str = "space";

    // Arrow keys
    pub const UP: &str = "up";
    pub const DOWN: &str = "down";
    pub const LEFT: &str = "left";
    pub const RIGHT: &str = "right";

    // Page navigation
    pub const HOME: &str = "home";
    pub const END: &str = "end";
    pub const PAGE_UP: &str = "pageup";
    pub const PAGE_DOWN: &str = "pagedown";

    // Function keys
    pub const F1: &str = "f1";
    pub const F2: &str = "f2";
    pub const F3: &str = "f3";
    pub const F4: &str = "f4";
    pub const F5: &str = "f5";
    pub const F6: &str = "f6";
    pub const F7: &str = "f7";
    pub const F8: &str = "f8";
    pub const F9: &str = "f9";
    pub const F10: &str = "f10";
    pub const F11: &str = "f11";
    pub const F12: &str = "f12";
}

/// Helper functions for working with keys.
pub mod key_helpers {
    use super::Key;

    /// Checks if a key is a single printable character.
    pub fn is_char_key(key: &Key) -> bool {
        key.chars()
            .next()
            .is_some_and(|c| key.len() == 1 && !c.is_control())
    }

    /// Checks if a key is a special named key (not a character).
    pub fn is_named_key(key: &Key) -> bool {
        key.len() > 1
            || key
                .chars()
                .next()
                .is_some_and(|c| key.len() == 1 && c.is_control())
    }

    /// Normalizes a key string to lowercase for consistent matching.
    pub fn normalize_key(key: &Key) -> Key {
        key.to_lowercase()
    }
}
