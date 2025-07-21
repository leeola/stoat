//! Advanced text editor node with rope-based editing capabilities
//!
//! This module provides [`TextEditNode`], which integrates the rope AST and text editing
//! system from [`stoat_text`] with the node system. It supports multi-cursor editing,
//! efficient text operations, and zero-allocation rendering through direct rope references.

use crate::{
    node::{Node, NodeId, NodeInit, NodePresentation, NodeSockets, NodeStatus, NodeType, Port},
    value::Value,
    Result,
};
use std::collections::HashMap;
use stoat_text::{buffer::Buffer, cursor::Cursor};

/// An advanced text editor node that uses rope-based text storage for efficient editing
///
/// [`TextEditNode`] wraps a [`Buffer`] from the text crate to provide sophisticated text
/// editing capabilities within the node system. It supports multiple cursors, efficient
/// text operations through the rope AST, and integrates with the rendering system for
/// zero-allocation display updates.
///
/// Unlike [`super::TextNode`] which stores text as a simple string, this node leverages
/// the rope data structure for:
/// - Efficient insertion/deletion operations
/// - Multiple cursor support
/// - Token-based positioning
/// - Incremental rendering updates
/// - Large file handling
#[derive(Debug)]
pub struct TextEditNode {
    /// Unique identifier for this node
    id: NodeId,

    /// Display name for this node
    name: String,

    /// Rope-based text buffer for efficient editing operations
    buffer: Buffer,

    /// Active cursors for multi-cursor editing support
    cursors: Vec<Cursor>,

    /// Whether the buffer has been modified since last save
    dirty: bool,
}

impl TextEditNode {
    /// Create a new text editor node with empty content
    pub fn new(id: NodeId, name: String) -> Self {
        Self::with_content(id, name, String::new())
    }

    /// Create a new text editor node with initial content
    pub fn with_content(id: NodeId, name: String, content: String) -> Self {
        // Create a simple rope AST structure for the content
        // FIXME: This should use proper markdown parsing when available
        let buffer = Self::create_buffer_from_content(&content, id.0);

        // Create primary cursor at start of buffer
        let cursors = vec![buffer.cursor_at_start()];

        Self {
            id,
            name,
            buffer,
            cursors,
            dirty: false,
        }
    }

    /// Get a reference to the text buffer
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// Get a mutable reference to the text buffer
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.dirty = true;
        &mut self.buffer
    }

    /// Get the current cursors
    pub fn cursors(&self) -> &[Cursor] {
        &self.cursors
    }

    /// Get the primary cursor (first in the list)
    pub fn primary_cursor(&self) -> &Cursor {
        &self.cursors[0]
    }

    /// Get a mutable reference to the primary cursor
    pub fn primary_cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cursors[0]
    }

    /// Insert a character at the primary cursor position
    pub fn insert_char(&mut self, ch: char) -> Result<()> {
        let cursor = &mut self.cursors[0];
        self.buffer
            .insert_char_at_cursor(cursor, ch)
            .map_err(|e| crate::Error::Generic {
                message: format!("Text insertion failed: {e:?}"),
            })?;
        self.dirty = true;
        Ok(())
    }

    /// Check if the buffer has unsaved changes
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Get cursor positions as line/column for GUI rendering
    pub fn gui_cursor_positions(&self) -> Vec<(usize, usize)> {
        self.cursors
            .iter()
            .map(|cursor| stoat_text::view::cursor_to_line_col(&self.buffer, cursor))
            .collect()
    }

    /// Get the total number of lines in the buffer
    pub fn line_count(&self) -> usize {
        stoat_text::view::View::count_lines(&self.buffer)
    }

    /// Mark the buffer as clean (saved)
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    /// Get the complete text content as a string
    pub fn content(&self) -> String {
        self.buffer.rope().to_string()
    }

    /// Create a buffer from text content using a simple AST structure
    fn create_buffer_from_content(content: &str, buffer_id: u64) -> Buffer {
        use stoat_rope::{ast::TextRange, builder::AstBuilder, kind::SyntaxKind, RopeAst};

        if content.is_empty() {
            // Create minimal empty document structure
            let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 0)).finish();
            let rope = RopeAst::from_root(doc);
            return Buffer::from_rope(rope, buffer_id);
        }

        // Split content into lines and create tokens
        let lines: Vec<&str> = content.lines().collect();
        let mut tokens = Vec::new();
        let mut offset = 0;

        for (i, line) in lines.iter().enumerate() {
            if !line.is_empty() {
                // Add text token for line content
                tokens.push(AstBuilder::token(
                    SyntaxKind::Text,
                    *line,
                    TextRange::new(offset, offset + line.len()),
                ));
                offset += line.len();
            }

            // Add newline token between lines (except for last line)
            if i < lines.len() - 1 {
                tokens.push(AstBuilder::token(
                    SyntaxKind::Newline,
                    "\n",
                    TextRange::new(offset, offset + 1),
                ));
                offset += 1;
            }
        }

        // If we only have one line with no newline, ensure we have at least one token
        if tokens.is_empty() && !content.is_empty() {
            tokens.push(AstBuilder::token(
                SyntaxKind::Text,
                content,
                TextRange::new(0, content.len()),
            ));
        }

        // Create document structure
        let total_len = content.len();
        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, total_len))
            .add_children(tokens)
            .finish();

        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, total_len))
            .add_child(paragraph)
            .finish();

        let rope = RopeAst::from_root(doc);
        Buffer::from_rope(rope, buffer_id)
    }
}

impl Node for TextEditNode {
    fn id(&self) -> NodeId {
        self.id
    }

    fn node_type(&self) -> NodeType {
        NodeType::TextEdit
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn execute(&mut self, inputs: &HashMap<String, Value>) -> Result<HashMap<String, Value>> {
        // Process text editing commands from inputs
        if let Some(command) = inputs.get("command") {
            match command {
                Value::String(cmd) if cmd.as_str() == "clear" => {
                    *self = Self::new(self.id, self.name.clone());
                },
                Value::Map(map) => {
                    if let Some(Value::String(op)) = map.0.get("operation") {
                        match op.as_str() {
                            "insert_char" => {
                                if let Some(Value::String(ch_str)) = map.0.get("character") {
                                    if let Some(ch) = ch_str.chars().next() {
                                        let _ = self.insert_char(ch);
                                    }
                                }
                            },
                            "set_content" => {
                                if let Some(Value::String(content)) = map.0.get("content") {
                                    *self = Self::with_content(
                                        self.id,
                                        self.name.clone(),
                                        content.to_string(),
                                    );
                                }
                            },
                            _ => {}, // Unknown operation
                        }
                    }
                },
                _ => {}, // Unknown command format
            }
        }

        // Return current content and status
        let mut outputs = HashMap::new();
        outputs.insert("text".to_string(), Value::String(self.content().into()));
        outputs.insert("dirty".to_string(), Value::Bool(self.dirty));
        outputs.insert(
            "cursor_count".to_string(),
            Value::U64(self.cursors.len() as u64),
        );

        Ok(outputs)
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new(
            "command",
            "Text editing commands (insert, delete, etc.)",
        )]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![
            Port::new("text", "The complete text content"),
            Port::new("dirty", "Whether the buffer has unsaved changes"),
            Port::new("cursor_count", "Number of active cursors"),
        ]
    }

    fn sockets(&self) -> NodeSockets {
        NodeSockets::new(vec![], vec![])
    }

    fn presentation(&self) -> NodePresentation {
        NodePresentation::TableViewer // Use TableViewer as it's the most expanded option available
    }

    fn status(&self) -> NodeStatus {
        if self.dirty {
            NodeStatus::Ready // Use Ready to indicate the node needs attention (has changes)
        } else {
            NodeStatus::Idle
        }
    }

    fn get_config_values(&self) -> HashMap<String, Value> {
        let mut config = HashMap::new();
        config.insert("content".to_string(), Value::String(self.content().into()));
        config.insert("dirty".to_string(), Value::Bool(self.dirty));
        config
    }
}

/// Initialization struct for creating [`TextEditNode`] instances
#[derive(Debug)]
pub struct TextEditNodeInit;

impl NodeInit for TextEditNodeInit {
    fn init(&self, id: NodeId, name: String, config: Value) -> Result<Box<dyn Node>> {
        // Extract content from config
        let content = match config {
            Value::Map(map) => map
                .0
                .get("content")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.to_string()),
                    _ => None,
                })
                .unwrap_or_default(),
            Value::String(s) => s.to_string(),
            _ => String::new(),
        };

        Ok(Box::new(TextEditNode::with_content(id, name, content)))
    }

    fn name(&self) -> &'static str {
        "text_edit"
    }
}
