//! Cursor implementation for navigating the rope AST

use crate::{
    ast::{AstNode, TextRange},
    kind::SyntaxKind,
};
use std::sync::Arc;

/// A cursor for navigating through the AST
#[derive(Debug, Clone)]
pub struct AstCursor {
    /// Path from root to current node
    path: Vec<CursorNode>,
    /// Current byte offset in the document
    byte_offset: usize,
    /// Current character offset in the document
    char_offset: usize,
}

/// A node in the cursor path with navigation information
#[derive(Debug, Clone)]
struct CursorNode {
    /// The node itself
    node: Arc<AstNode>,
    /// Index of this node in its parent's children (if applicable)
    child_index: Option<usize>,
    /// Accumulated byte offset at the start of this node
    start_byte_offset: usize,
    /// Accumulated char offset at the start of this node
    start_char_offset: usize,
}

impl AstCursor {
    /// Create a new cursor at the beginning of the document
    pub fn new(root: Arc<AstNode>) -> Self {
        Self {
            path: vec![CursorNode {
                node: root,
                child_index: None,
                start_byte_offset: 0,
                start_char_offset: 0,
            }],
            byte_offset: 0,
            char_offset: 0,
        }
    }

    /// Get the current node
    pub fn current_node(&self) -> &Arc<AstNode> {
        &self
            .path
            .last()
            .expect("cursor path should never be empty")
            .node
    }

    /// Get the current byte offset
    pub fn byte_offset(&self) -> usize {
        self.byte_offset
    }

    /// Get the current character offset
    pub fn char_offset(&self) -> usize {
        self.char_offset
    }

    /// Get the depth of the cursor (0 = root)
    pub fn depth(&self) -> usize {
        self.path.len() - 1
    }

    /// Move cursor to a specific byte offset
    pub fn move_to_byte(&mut self, target_offset: usize) -> bool {
        // Reset to root
        self.path.truncate(1);
        self.byte_offset = 0;
        self.char_offset = 0;

        // Navigate down to find the node containing the offset
        self.move_to_byte_impl(target_offset)
    }

    /// Helper for move_to_byte
    fn move_to_byte_impl(&mut self, target_offset: usize) -> bool {
        loop {
            let current = self.current_node().clone();
            let current_range = current.range();

            // Check if target is within current node
            if target_offset < current_range.start.0 || target_offset > current_range.end.0 {
                return false;
            }

            // If this is a token, we've found our position
            if current.is_token() {
                self.byte_offset = target_offset;
                // Calculate char offset within the token
                if let Some(text) = current.token_text() {
                    let local_byte_offset = target_offset - current_range.start.0;
                    let mut char_count = self.char_offset;
                    let mut byte_count = 0;

                    for ch in text.chars() {
                        if byte_count >= local_byte_offset {
                            break;
                        }
                        byte_count += ch.len_utf8();
                        char_count += 1;
                    }
                    self.char_offset = char_count;
                }
                return true;
            }

            // It's a syntax node, find the child containing the offset
            if let Some(children) = current.children() {
                let mut acc_byte_offset = current_range.start.0;
                let mut acc_char_offset = self.char_offset;

                for (i, (child, info)) in children.iter().enumerate() {
                    let child_range = child.range();

                    if target_offset >= child_range.start.0 && target_offset < child_range.end.0 {
                        // Found the child containing the offset
                        self.path.push(CursorNode {
                            node: child.clone(),
                            child_index: Some(i),
                            start_byte_offset: acc_byte_offset,
                            start_char_offset: acc_char_offset,
                        });
                        self.char_offset = acc_char_offset;
                        break;
                    }

                    acc_byte_offset = child_range.end.0;
                    acc_char_offset += info.chars;
                }

                // If we didn't add a child, check if offset is at the end
                if std::ptr::eq(
                    self.path.last().expect("path not empty").node.as_ref(),
                    current.as_ref(),
                ) {
                    // We're still at the same node, so offset must be at or past the end
                    // Position at the last child if the offset equals the node's end
                    if target_offset == current_range.end.0 && !children.is_empty() {
                        let last_idx = children.len() - 1;
                        let (last_child, _) = &children[last_idx];
                        self.path.push(CursorNode {
                            node: last_child.clone(),
                            child_index: Some(last_idx),
                            start_byte_offset: last_child.range().start.0,
                            start_char_offset: self.char_offset,
                        });
                    } else {
                        self.byte_offset = target_offset;
                        return true;
                    }
                }
            } else {
                // Syntax node with no children
                self.byte_offset = target_offset;
                return true;
            }
        }
    }

    /// Move cursor to a specific character offset
    pub fn move_to_char(&mut self, target_offset: usize) -> bool {
        // Reset to root
        self.path.truncate(1);
        self.byte_offset = 0;
        self.char_offset = 0;

        // Navigate to find the position
        self.move_to_char_impl(target_offset)
    }

    /// Helper for move_to_char
    fn move_to_char_impl(&mut self, target_offset: usize) -> bool {
        let mut remaining_chars = target_offset;

        loop {
            let current = self.current_node().clone();

            // If this is a token, calculate byte position
            if current.is_token() {
                if let Some(text) = current.token_text() {
                    let mut byte_pos = 0;
                    let mut char_count = 0;

                    for ch in text.chars() {
                        if char_count >= remaining_chars {
                            break;
                        }
                        byte_pos += ch.len_utf8();
                        char_count += 1;
                    }

                    self.byte_offset = current.range().start.0 + byte_pos;
                    self.char_offset += char_count;
                    return true;
                }
                return false;
            }

            // It's a syntax node, find the right child
            if let Some(children) = current.children() {
                for (i, (child, info)) in children.iter().enumerate() {
                    if remaining_chars <= info.chars {
                        // This child contains our target
                        self.path.push(CursorNode {
                            node: child.clone(),
                            child_index: Some(i),
                            start_byte_offset: self.byte_offset,
                            start_char_offset: self.char_offset,
                        });
                        break;
                    }
                    remaining_chars -= info.chars;
                    self.char_offset += info.chars;
                    self.byte_offset = child.range().end.0;
                }

                // If we didn't find a child, position is at end
                if self.path.len() == self.depth() + 1 {
                    return true;
                }
            } else {
                return true;
            }
        }
    }

    /// Move to the next token
    pub fn next_token(&mut self) -> bool {
        // Save current position if we're at a token
        let saved_state = if self.current_node().is_token() {
            Some(self.clone())
        } else {
            None
        };

        // First, move to the deepest token if we're not already there
        while !self.current_node().is_token() {
            if !self.first_child() {
                break;
            }
        }

        // Now find the next token
        loop {
            if self.next_sibling() {
                // Move down to first token in this subtree
                while !self.current_node().is_token() {
                    if !self.first_child() {
                        break;
                    }
                }
                if self.current_node().is_token() {
                    return true;
                }
            }

            // No next sibling, go up
            if !self.parent() {
                // At root, no next token - restore position if we had one
                if let Some(saved) = saved_state {
                    *self = saved;
                }
                return false;
            }
        }
    }

    /// Move to the previous token
    pub fn prev_token(&mut self) -> bool {
        // If we're at a token, try to move to previous sibling first
        if self.current_node().is_token() {
            if self.prev_sibling() {
                // If the sibling is not a token, find last token in it
                while !self.current_node().is_token() {
                    if !self.last_child() {
                        break;
                    }
                }
                return true;
            }
            // Go up and continue
            if !self.parent() {
                return false;
            }
        }

        // Similar to next_token but in reverse
        loop {
            if self.prev_sibling() {
                // Move down to last token in this subtree
                while !self.current_node().is_token() {
                    if !self.last_child() {
                        break;
                    }
                }
                return true;
            }

            // No previous sibling, go up
            if !self.parent() {
                return false; // At root, no previous token
            }
        }
    }

    /// Move to parent node
    pub fn parent(&mut self) -> bool {
        if self.path.len() > 1 {
            self.path.pop();
            // Update offsets
            if let Some(parent_node) = self.path.last() {
                self.byte_offset = parent_node.start_byte_offset;
                self.char_offset = parent_node.start_char_offset;
            }
            true
        } else {
            false
        }
    }

    /// Move to first child
    pub fn first_child(&mut self) -> bool {
        let current = self.current_node().clone();
        if let Some(children) = current.children() {
            if let Some((first_child, _)) = children.first() {
                self.path.push(CursorNode {
                    node: first_child.clone(),
                    child_index: Some(0),
                    start_byte_offset: self.byte_offset,
                    start_char_offset: self.char_offset,
                });
                return true;
            }
        }
        false
    }

    /// Move to last child
    pub fn last_child(&mut self) -> bool {
        let current = self.current_node().clone();
        if let Some(children) = current.children() {
            if !children.is_empty() {
                let last_index = children.len() - 1;
                let (last_child, _) = &children[last_index];

                // Calculate offsets for the last child
                let mut byte_offset = current.range().start.0;
                let mut char_offset = self.char_offset;
                for (i, (_, info)) in children.iter().enumerate() {
                    if i >= last_index {
                        break;
                    }
                    byte_offset = children[i].0.range().end.0;
                    char_offset += info.chars;
                }

                self.byte_offset = byte_offset;
                self.char_offset = char_offset;
                self.path.push(CursorNode {
                    node: last_child.clone(),
                    child_index: Some(last_index),
                    start_byte_offset: byte_offset,
                    start_char_offset: char_offset,
                });
                return true;
            }
        }
        false
    }

    /// Move to next sibling
    pub fn next_sibling(&mut self) -> bool {
        if self.path.len() < 2 {
            return false; // Root has no siblings
        }

        let current_node = self.path.last().expect("path not empty");
        let parent_node = &self.path[self.path.len() - 2];

        if let Some(current_index) = current_node.child_index {
            if let Some(children) = parent_node.node.children() {
                if current_index + 1 < children.len() {
                    let (next_sibling, _info) = &children[current_index + 1];
                    let next_sibling = next_sibling.clone();

                    // Update offsets
                    self.byte_offset = self.current_node().range().end.0;
                    self.char_offset += self.current_node().text_info().chars;

                    // Replace current node with sibling
                    self.path.pop();
                    self.path.push(CursorNode {
                        node: next_sibling,
                        child_index: Some(current_index + 1),
                        start_byte_offset: self.byte_offset,
                        start_char_offset: self.char_offset,
                    });
                    return true;
                }
            }
        }
        false
    }

    /// Move to previous sibling
    pub fn prev_sibling(&mut self) -> bool {
        if self.path.len() < 2 {
            return false; // Root has no siblings
        }

        let current_node = self.path.last().expect("path not empty");
        let parent_node = &self.path[self.path.len() - 2];

        if let Some(current_index) = current_node.child_index {
            if current_index > 0 {
                if let Some(children) = parent_node.node.children() {
                    let (prev_sibling, prev_info) = &children[current_index - 1];
                    let prev_sibling = prev_sibling.clone();
                    let prev_chars = prev_info.chars;

                    // Update offsets
                    self.byte_offset = prev_sibling.range().start.0;
                    self.char_offset -= prev_chars;

                    // Replace current node with sibling
                    self.path.pop();
                    self.path.push(CursorNode {
                        node: prev_sibling,
                        child_index: Some(current_index - 1),
                        start_byte_offset: self.byte_offset,
                        start_char_offset: self.char_offset - prev_chars,
                    });
                    return true;
                }
            }
        }
        false
    }

    /// Get the syntax kind at the cursor
    pub fn kind(&self) -> SyntaxKind {
        self.current_node().kind()
    }

    /// Get the text of the current token (if at a token)
    pub fn token_text(&self) -> Option<&str> {
        self.current_node().token_text()
    }

    /// Get the range of the current node
    pub fn node_range(&self) -> TextRange {
        self.current_node().range()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ast::TextRange, builder::AstBuilder};

    #[test]
    fn test_cursor_navigation() {
        // Build a simple tree
        let token1 = AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5));
        let token2 = AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6));
        let token3 = AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11));

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_child(token1)
            .add_child(token2)
            .add_child(token3)
            .finish();

        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 11))
            .add_child(paragraph)
            .finish();

        let mut cursor = AstCursor::new(doc);

        // Initial position is at root
        assert_eq!(cursor.kind(), SyntaxKind::Document);
        assert_eq!(cursor.depth(), 0);

        // Move to first child
        assert!(cursor.first_child());
        assert_eq!(cursor.kind(), SyntaxKind::Paragraph);
        assert_eq!(cursor.depth(), 1);

        // Move to first token
        assert!(cursor.first_child());
        assert_eq!(cursor.kind(), SyntaxKind::Text);
        assert_eq!(cursor.token_text(), Some("hello"));

        // Move to next sibling
        assert!(cursor.next_sibling());
        assert_eq!(cursor.kind(), SyntaxKind::Whitespace);
        assert_eq!(cursor.token_text(), Some(" "));

        // Move to next sibling again
        assert!(cursor.next_sibling());
        assert_eq!(cursor.kind(), SyntaxKind::Text);
        assert_eq!(cursor.token_text(), Some("world"));

        // No more siblings
        assert!(!cursor.next_sibling());

        // Go back up
        assert!(cursor.parent());
        assert_eq!(cursor.kind(), SyntaxKind::Paragraph);
    }

    #[test]
    fn test_cursor_move_to_byte() {
        // Token ranges: hello[0,5), " "[5,6), world[6,11)
        let token1 = AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5));
        let token2 = AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6));
        let token3 = AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11));

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_child(token1)
            .add_child(token2)
            .add_child(token3)
            .finish();

        let mut cursor = AstCursor::new(paragraph);

        // Move to offset 0 (beginning of "hello")
        assert!(cursor.move_to_byte(0));
        assert_eq!(cursor.kind(), SyntaxKind::Text);
        assert_eq!(cursor.token_text(), Some("hello"));
        assert_eq!(cursor.byte_offset(), 0);

        // Move to offset 7 (in "world")
        assert!(cursor.move_to_byte(7));
        assert_eq!(cursor.kind(), SyntaxKind::Text);
        assert_eq!(cursor.token_text(), Some("world"));
        assert_eq!(cursor.byte_offset(), 7);

        // Move to offset 5 (the space)
        assert!(cursor.move_to_byte(5));
        assert_eq!(cursor.kind(), SyntaxKind::Whitespace);
        assert_eq!(cursor.token_text(), Some(" "));
        assert_eq!(cursor.byte_offset(), 5);
    }

    #[test]
    fn test_cursor_next_prev_token() {
        let tokens = vec![
            AstBuilder::token(SyntaxKind::Text, "one", TextRange::new(0, 3)),
            AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(3, 4)),
            AstBuilder::token(SyntaxKind::Text, "two", TextRange::new(4, 7)),
            AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(7, 8)),
            AstBuilder::token(SyntaxKind::Text, "three", TextRange::new(8, 13)),
        ];

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 13))
            .add_children(tokens)
            .finish();

        let mut cursor = AstCursor::new(paragraph);

        // Start at root, move to first token
        cursor.first_child();
        assert_eq!(cursor.token_text(), Some("one"));

        // Move through all tokens
        assert!(cursor.next_token());
        assert_eq!(cursor.token_text(), Some(" "));

        assert!(cursor.next_token());
        assert_eq!(cursor.token_text(), Some("two"));

        assert!(cursor.next_token());
        assert_eq!(cursor.token_text(), Some(" "));

        assert!(cursor.next_token());
        assert_eq!(cursor.token_text(), Some("three"));

        // No more tokens
        assert!(!cursor.next_token());

        // Go back
        assert!(cursor.prev_token());
        assert_eq!(cursor.token_text(), Some(" "));

        assert!(cursor.prev_token());
        assert_eq!(cursor.token_text(), Some("two"));
    }
}
