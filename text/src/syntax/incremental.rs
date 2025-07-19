//! Incremental parsing for efficient AST updates

use crate::{
    TextSize,
    edit::FlatEdit,
    range::TextRange,
    syntax::flat_ast::{FlatAst, NodeId},
};
use std::collections::HashSet;

/// Describes a change to the text that affects parsing
#[derive(Debug, Clone)]
pub struct TextChange {
    /// The range that was modified
    pub range: TextRange,
    /// The length of text that was deleted
    pub deleted_len: TextSize,
    /// The length of text that was inserted
    pub inserted_len: TextSize,
}

impl TextChange {
    /// Create a new text change
    pub fn new(range: TextRange, deleted_len: TextSize, inserted_len: TextSize) -> Self {
        Self {
            range,
            deleted_len,
            inserted_len,
        }
    }

    /// Create a text change from an edit operation
    pub fn from_edit(_edit: &FlatEdit, _text: &str) -> Self {
        // FIXME: This is a simplified implementation
        // In practice, we'd need to resolve the NodeId to a text range
        let range = TextRange::empty(TextSize::from(0));
        let deleted_len = TextSize::from(0);
        let inserted_len = TextSize::from(0);

        Self::new(range, deleted_len, inserted_len)
    }

    /// Get the net change in text length
    pub fn net_change(&self) -> i64 {
        let inserted: u32 = self.inserted_len.into();
        let deleted: u32 = self.deleted_len.into();
        inserted as i64 - deleted as i64
    }
}

/// Tracks which nodes need to be reparsed after changes
#[derive(Debug, Default)]
pub struct InvalidationSet {
    /// Nodes that need to be reparsed
    invalidated_nodes: HashSet<NodeId>,
    /// Minimum range that needs reparsing
    reparse_range: Option<TextRange>,
}

impl InvalidationSet {
    /// Create a new invalidation set
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a node to the invalidation set
    pub fn invalidate_node(&mut self, node_id: NodeId) {
        self.invalidated_nodes.insert(node_id);
    }

    /// Set the range that needs reparsing
    pub fn set_reparse_range(&mut self, range: TextRange) {
        self.reparse_range = Some(match self.reparse_range {
            Some(existing) => existing.union(range),
            None => range,
        });
    }

    /// Get the nodes that need reparsing
    pub fn invalidated_nodes(&self) -> &HashSet<NodeId> {
        &self.invalidated_nodes
    }

    /// Get the range that needs reparsing
    pub fn reparse_range(&self) -> Option<TextRange> {
        self.reparse_range
    }

    /// Check if any nodes are invalidated
    pub fn is_empty(&self) -> bool {
        self.invalidated_nodes.is_empty() && self.reparse_range.is_none()
    }

    /// Clear all invalidations
    pub fn clear(&mut self) {
        self.invalidated_nodes.clear();
        self.reparse_range = None;
    }
}

/// Incremental parser that efficiently updates flat ASTs
pub struct IncrementalParser {
    /// Current AST being maintained
    ast: FlatAst,
    /// Set of invalidated nodes
    invalidation: InvalidationSet,
}

impl IncrementalParser {
    /// Create a new incremental parser
    pub fn new(ast: FlatAst) -> Self {
        Self {
            ast,
            invalidation: InvalidationSet::new(),
        }
    }

    /// Apply a text change and determine what needs reparsing
    pub fn apply_change(&mut self, change: &TextChange) -> Result<(), IncrementalParseError> {
        // Find all nodes that intersect with the changed range
        let affected_nodes = self.find_affected_nodes(change.range);

        // Mark them for invalidation
        for node_id in affected_nodes {
            self.invalidation.invalidate_node(node_id);
        }

        // Determine the minimal range that needs reparsing
        let reparse_range = self.calculate_reparse_range(change)?;
        self.invalidation.set_reparse_range(reparse_range);

        Ok(())
    }

    /// Get the current AST
    pub fn ast(&self) -> &FlatAst {
        &self.ast
    }

    /// Get the current invalidation set
    pub fn invalidation(&self) -> &InvalidationSet {
        &self.invalidation
    }

    /// Perform incremental reparse of invalidated regions
    pub fn reparse(&mut self, text: &str) -> Result<(), IncrementalParseError> {
        if self.invalidation.is_empty() {
            return Ok(());
        }

        // Get the range that needs reparsing
        let reparse_range = self
            .invalidation
            .reparse_range()
            .ok_or(IncrementalParseError::NoReparseRange)?;

        // Extract the text to reparse
        let text_to_parse = self.extract_text_range(text, reparse_range)?;

        // Find the parent node that should contain the reparsed content
        let parent_node = self.find_reparse_parent(reparse_range)?;

        // Perform the reparse
        self.reparse_range(parent_node, &text_to_parse, reparse_range)?;

        // Clear invalidations
        self.invalidation.clear();

        Ok(())
    }

    /// Find all nodes that intersect with the given range
    fn find_affected_nodes(&self, range: TextRange) -> Vec<NodeId> {
        let mut affected = Vec::new();

        // FIXME: This is a simplified implementation
        // In practice, we'd traverse the AST to find intersecting nodes
        for node_id in self.ast.node_ids() {
            if let Some(node) = self.ast.get_node(node_id) {
                if node.range.intersects(range) {
                    affected.push(node_id);
                }
            }
        }

        affected
    }

    /// Calculate the minimal range that needs reparsing
    fn calculate_reparse_range(
        &self,
        change: &TextChange,
    ) -> Result<TextRange, IncrementalParseError> {
        // Start with the changed range
        let mut reparse_range = change.range;

        // Expand to include complete syntax constructs
        // FIXME: This is language-specific and needs proper implementation
        reparse_range = self.expand_to_syntax_boundaries(reparse_range)?;

        Ok(reparse_range)
    }

    /// Expand a range to include complete syntax constructs
    fn expand_to_syntax_boundaries(
        &self,
        range: TextRange,
    ) -> Result<TextRange, IncrementalParseError> {
        // FIXME: This is a placeholder implementation
        // In practice, this would be language-specific and would expand
        // the range to include complete statements, expressions, etc.
        Ok(range)
    }

    /// Extract text from a specific range
    fn extract_text_range(
        &self,
        text: &str,
        range: TextRange,
    ) -> Result<String, IncrementalParseError> {
        let start: usize = range.start().into();
        let end: usize = range.end().into();

        if start > text.len() || end > text.len() || start > end {
            return Err(IncrementalParseError::InvalidRange { range });
        }

        Ok(text[start..end].to_string())
    }

    /// Find the parent node that should contain the reparsed content
    fn find_reparse_parent(&self, _range: TextRange) -> Result<NodeId, IncrementalParseError> {
        // FIXME: This is a simplified implementation
        // In practice, we'd find the smallest node that completely contains the range
        self.ast.root_id().ok_or(IncrementalParseError::NoRootNode)
    }

    /// Reparse a specific range and update the AST
    fn reparse_range(
        &mut self,
        _parent_node: NodeId,
        _text: &str,
        _range: TextRange,
    ) -> Result<(), IncrementalParseError> {
        // FIXME: This is a placeholder implementation
        // In practice, this would:
        // 1. Create a new FlatTreeBuilder
        // 2. Parse the text range
        // 3. Replace the affected nodes in the AST
        // 4. Update node IDs and references

        // For now, fall back to full reparse
        // FIXME: Implement proper incremental parsing
        // This would parse the text and update the AST incrementally

        Ok(())
    }
}

/// Errors that can occur during incremental parsing
#[derive(Debug, thiserror::Error)]
pub enum IncrementalParseError {
    #[error("Invalid text range: {range:?}")]
    InvalidRange { range: TextRange },

    #[error("No reparse range specified")]
    NoReparseRange,

    #[error("No root node found")]
    NoRootNode,

    #[error("Failed to find parent node for range")]
    NoParentNode,

    #[error("Parse error: {message}")]
    ParseError { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_change_net_change() {
        let change = TextChange::new(
            TextRange::new(0.into(), 5.into()),
            TextSize::from(5),
            TextSize::from(3),
        );
        assert_eq!(change.net_change(), -2);
    }

    #[test]
    fn test_invalidation_set() {
        let mut invalidation = InvalidationSet::new();
        assert!(invalidation.is_empty());

        let node_id = NodeId(1);
        invalidation.invalidate_node(node_id);
        assert!(!invalidation.is_empty());
        assert!(invalidation.invalidated_nodes().contains(&node_id));

        invalidation.clear();
        assert!(invalidation.is_empty());
    }

    #[test]
    fn test_incremental_parser_creation() {
        let ast = FlatAst::new();
        let parser = IncrementalParser::new(ast);
        assert!(parser.invalidation().is_empty());
    }
}
