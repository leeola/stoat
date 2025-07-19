//! Iterator implementations for traversing the rope AST

use crate::{ast::AstNode, kind::SyntaxKind};
use std::sync::Arc;

/// Iterator over nodes in the AST using depth-first traversal
pub struct NodeIter<'a> {
    /// Stack of nodes to visit with their depth
    stack: Vec<(&'a Arc<AstNode>, usize)>,
    /// Traversal order
    order: TraversalOrder,
}

/// Traversal order for tree iteration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraversalOrder {
    /// Visit parent before children
    PreOrder,
    /// Visit parent after children
    PostOrder,
}

impl<'a> NodeIter<'a> {
    /// Create a new node iterator starting from the given node
    pub fn new(root: &'a Arc<AstNode>, order: TraversalOrder) -> Self {
        Self {
            stack: vec![(root, 0)],
            order,
        }
    }

    /// Create a pre-order iterator
    pub fn pre_order(root: &'a Arc<AstNode>) -> Self {
        Self::new(root, TraversalOrder::PreOrder)
    }

    /// Create a post-order iterator
    pub fn post_order(root: &'a Arc<AstNode>) -> Self {
        Self::new(root, TraversalOrder::PostOrder)
    }
}

impl<'a> Iterator for NodeIter<'a> {
    type Item = (&'a Arc<AstNode>, usize);

    fn next(&mut self) -> Option<Self::Item> {
        match self.order {
            TraversalOrder::PreOrder => {
                if let Some((node, depth)) = self.stack.pop() {
                    // Add children to stack in reverse order for left-to-right traversal
                    if let Some(children) = node.children() {
                        for (child, _) in children.iter().rev() {
                            self.stack.push((child, depth + 1));
                        }
                    }
                    Some((node, depth))
                } else {
                    None
                }
            },
            TraversalOrder::PostOrder => {
                // Simplified post-order implementation
                // In a full implementation, we'd need to track visited state
                if let Some((node, depth)) = self.stack.pop() {
                    // For now, just do pre-order traversal
                    // TODO: Implement proper post-order traversal
                    if let Some(children) = node.children() {
                        for (child, _) in children.iter().rev() {
                            self.stack.push((child, depth + 1));
                        }
                    }
                    Some((node, depth))
                } else {
                    None
                }
            },
        }
    }
}

/// Iterator over token nodes only
pub struct TokenIter<'a> {
    node_iter: NodeIter<'a>,
}

impl<'a> TokenIter<'a> {
    /// Create a new token iterator
    pub fn new(root: &'a Arc<AstNode>) -> Self {
        Self {
            node_iter: NodeIter::pre_order(root),
        }
    }
}

impl<'a> Iterator for TokenIter<'a> {
    type Item = &'a Arc<AstNode>;

    fn next(&mut self) -> Option<Self::Item> {
        self.node_iter
            .by_ref()
            .map(|(node, _)| node)
            .find(|&node| node.is_token())
    }
}

/// Iterator over text chunks
pub struct TextChunkIter<'a> {
    token_iter: TokenIter<'a>,
    current_buffer: String,
    chunk_size: usize,
}

impl<'a> TextChunkIter<'a> {
    /// Create a new text chunk iterator
    pub fn new(root: &'a Arc<AstNode>, chunk_size: usize) -> Self {
        Self {
            token_iter: TokenIter::new(root),
            current_buffer: String::new(),
            chunk_size,
        }
    }
}

impl<'a> Iterator for TextChunkIter<'a> {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        // Fill buffer until we have a full chunk or no more tokens
        while self.current_buffer.len() < self.chunk_size {
            if let Some(token) = self.token_iter.next() {
                if let Some(text) = token.token_text() {
                    self.current_buffer.push_str(text);
                }
            } else {
                // No more tokens
                if self.current_buffer.is_empty() {
                    return None;
                } else {
                    // Return remaining text
                    return Some(std::mem::take(&mut self.current_buffer));
                }
            }
        }

        // Extract a chunk
        if self.current_buffer.len() >= self.chunk_size {
            // Find a good split point (char boundary)
            let mut split_pos = self.chunk_size;
            while !self.current_buffer.is_char_boundary(split_pos) && split_pos > 0 {
                split_pos -= 1;
            }

            let chunk = self.current_buffer[..split_pos].to_string();
            self.current_buffer.drain(..split_pos);
            Some(chunk)
        } else {
            None
        }
    }
}

/// Iterator over lines based on newline tokens
pub struct LineIter<'a> {
    token_iter: TokenIter<'a>,
    current_line: String,
    finished: bool,
}

impl<'a> LineIter<'a> {
    /// Create a new line iterator
    pub fn new(root: &'a Arc<AstNode>) -> Self {
        Self {
            token_iter: TokenIter::new(root),
            current_line: String::new(),
            finished: false,
        }
    }
}

impl<'a> Iterator for LineIter<'a> {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        for token in self.token_iter.by_ref() {
            if let Some(text) = token.token_text() {
                if token.kind() == SyntaxKind::Newline {
                    // Found newline, return current line
                    let line = std::mem::take(&mut self.current_line);
                    return Some(line);
                } else {
                    self.current_line.push_str(text);
                }
            }
        }

        // No more tokens
        if self.current_line.is_empty() {
            self.finished = true;
            None
        } else {
            // Return last line
            self.finished = true;
            Some(std::mem::take(&mut self.current_line))
        }
    }
}

/// Iterator that filters nodes by a predicate
pub struct FilteredNodeIter<'a, F>
where
    F: FnMut(&Arc<AstNode>) -> bool,
{
    node_iter: NodeIter<'a>,
    predicate: F,
}

impl<'a, F> FilteredNodeIter<'a, F>
where
    F: FnMut(&Arc<AstNode>) -> bool,
{
    /// Create a new filtered iterator
    pub fn new(root: &'a Arc<AstNode>, predicate: F) -> Self {
        Self {
            node_iter: NodeIter::pre_order(root),
            predicate,
        }
    }
}

impl<'a, F> Iterator for FilteredNodeIter<'a, F>
where
    F: FnMut(&Arc<AstNode>) -> bool,
{
    type Item = &'a Arc<AstNode>;

    fn next(&mut self) -> Option<Self::Item> {
        self.node_iter
            .by_ref()
            .map(|(node, _)| node)
            .find(|&node| (self.predicate)(node))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ast::TextRange, builder::AstBuilder};

    #[test]
    fn test_node_iterator_pre_order() {
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

        let mut iter = NodeIter::pre_order(&doc);

        // First should be document
        let (node, depth) = iter.next().expect("should have document");
        assert_eq!(node.kind(), SyntaxKind::Document);
        assert_eq!(depth, 0);

        // Then paragraph
        let (node, depth) = iter.next().expect("should have paragraph");
        assert_eq!(node.kind(), SyntaxKind::Paragraph);
        assert_eq!(depth, 1);

        // Then tokens
        let (node, depth) = iter.next().expect("should have first token");
        assert_eq!(node.kind(), SyntaxKind::Text);
        assert_eq!(depth, 2);
    }

    #[test]
    fn test_token_iterator() {
        // Build a tree with mixed nodes
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

        let tokens: Vec<_> = TokenIter::new(&doc).collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].token_text(), Some("hello"));
        assert_eq!(tokens[1].token_text(), Some(" "));
        assert_eq!(tokens[2].token_text(), Some("world"));
    }

    #[test]
    fn test_text_chunk_iterator() {
        let token1 = AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5));
        let token2 = AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6));
        let token3 = AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11));

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_child(token1)
            .add_child(token2)
            .add_child(token3)
            .finish();

        let chunks: Vec<_> = TextChunkIter::new(&paragraph, 4).collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], "hell");
        assert_eq!(chunks[1], "o wo");
        assert_eq!(chunks[2], "rld");
    }

    #[test]
    fn test_line_iterator() {
        let tokens = vec![
            AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5)),
            AstBuilder::token(SyntaxKind::Newline, "\n", TextRange::new(5, 6)),
            AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11)),
            AstBuilder::token(SyntaxKind::Newline, "\n", TextRange::new(11, 12)),
            AstBuilder::token(SyntaxKind::Text, "foo", TextRange::new(12, 15)),
        ];

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 15))
            .add_children(tokens)
            .finish();

        let lines: Vec<_> = LineIter::new(&paragraph).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "hello");
        assert_eq!(lines[1], "world");
        assert_eq!(lines[2], "foo");
    }

    #[test]
    fn test_filtered_iterator() {
        let token1 = AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5));
        let token2 = AstBuilder::token(SyntaxKind::Whitespace, " ", TextRange::new(5, 6));
        let token3 = AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(6, 11));

        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_child(token1)
            .add_child(token2)
            .add_child(token3)
            .finish();

        // Filter for text tokens only
        let text_nodes: Vec<_> =
            FilteredNodeIter::new(&paragraph, |node| node.kind() == SyntaxKind::Text).collect();

        assert_eq!(text_nodes.len(), 2);
        assert_eq!(text_nodes[0].token_text(), Some("hello"));
        assert_eq!(text_nodes[1].token_text(), Some("world"));
    }
}
