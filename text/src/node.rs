//! Text editor node implementation for Stoat
//!
//! Provides the [`Node`] type that will integrate with the Stoat node system to provide
//! text editing capabilities in the canvas. This node type manages buffers, views, and
//! cursors to provide a complete text editing experience.

use crate::{buffer::Buffer, cursor::Cursor, view::View};
use std::collections::HashMap;

/// A text editor node in the Stoat canvas
///
/// The Node brings together [`Buffer`], [`View`], and [`Cursor`] to create a fully
/// functional text editor that can be connected to other nodes. Multiple nodes can
/// share the same buffer while maintaining independent views and cursor sets.
#[allow(dead_code)]
pub struct Node {
    /// Node identifier
    id: u64,

    /// Node display name
    name: String,

    /// The buffer this node is editing
    buffer: Buffer,

    /// The view configuration for this node
    view: View,

    /// Active cursors in this node
    cursors: Vec<Cursor>,

    /// Configuration values
    config: HashMap<String, String>,
}

impl Node {
    // Implementation will follow in later phases
}
