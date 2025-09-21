//! Event types for the Stoat editor.
//!
//! This module defines all possible input events that the editor can process.
//! Events use GPUI-compatible types for better integration with the GUI layer.

use crate::input::{Key, Modifiers, MouseButton, Point};

/// Events that the editor can process.
///
/// Each event represents some form of input or external trigger that should
/// cause the editor state to change. All events are pure data and contain
/// no behavior themselves.
#[derive(Debug, Clone)]
pub enum EditorEvent {
    /// A key was pressed with optional modifiers
    KeyPress { key: Key, modifiers: Modifiers },

    /// Text was pasted (from clipboard or drag-drop)
    TextPasted { content: String },

    /// Mouse was clicked at a specific point
    MouseClick {
        position: Point,
        button: MouseButton,
    },

    /// Mouse was moved (for hover, selection extension, etc.)
    MouseMove { position: Point },

    /// Create new empty buffer
    NewFile,

    /// Exit the editor
    Exit,

    /// Undo last change
    Undo,

    /// Redo last undone change
    Redo,

    /// Resize viewport
    Resize {
        width: f32,
        height: f32,
        line_height: f32,
    },

    /// Scroll viewport
    Scroll { delta_x: f32, delta_y: f32 },
}
