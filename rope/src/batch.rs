//! Batched edit operations for efficient multi-edit support

use crate::{
    ast::{AstError, AstNode, TextRange},
    edit::{EditOp, apply_edit},
};
use compact_str::CompactString;
use std::sync::Arc;

/// A collection of edit operations to be applied as a batch
#[derive(Debug, Clone)]
pub struct BatchedEdit {
    /// The edit operations, sorted by position
    edits: Vec<EditOp>,
}

impl BatchedEdit {
    /// Create a new empty batch
    pub fn new() -> Self {
        Self { edits: Vec::new() }
    }

    /// Add an insert operation to the batch
    pub fn insert(&mut self, offset: usize, text: impl Into<CompactString>) {
        self.edits.push(EditOp::Insert {
            offset,
            text: text.into(),
        });
    }

    /// Add a delete operation to the batch
    pub fn delete(&mut self, range: TextRange) {
        self.edits.push(EditOp::Delete { range });
    }

    /// Add a replace operation to the batch
    pub fn replace(&mut self, range: TextRange, text: impl Into<CompactString>) {
        self.edits.push(EditOp::Replace {
            range,
            text: text.into(),
        });
    }

    /// Get the number of edits in the batch
    pub fn len(&self) -> usize {
        self.edits.len()
    }

    /// Check if the batch is empty
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    /// Sort edits by position (reverse order for correct application)
    pub fn prepare(&mut self) -> Result<(), AstError> {
        // Check for overlapping edits
        self.check_overlaps()?;

        // Sort in reverse order so we can apply from end to start
        // This avoids having to adjust offsets as we go
        self.edits.sort_by(|a, b| {
            let a_start = match a {
                EditOp::Insert { offset, .. } => *offset,
                EditOp::Delete { range } | EditOp::Replace { range, .. } => range.start.0,
            };
            let b_start = match b {
                EditOp::Insert { offset, .. } => *offset,
                EditOp::Delete { range } | EditOp::Replace { range, .. } => range.start.0,
            };
            b_start.cmp(&a_start)
        });

        Ok(())
    }

    /// Check for overlapping edits
    fn check_overlaps(&self) -> Result<(), AstError> {
        // Create a sorted copy for checking
        let mut sorted = self.edits.clone();
        sorted.sort_by_key(|edit| match edit {
            EditOp::Insert { offset, .. } => *offset,
            EditOp::Delete { range } | EditOp::Replace { range, .. } => range.start.0,
        });

        // Check each pair of adjacent edits
        for i in 0..sorted.len().saturating_sub(1) {
            let curr_range = sorted[i].affected_range();
            let next_range = sorted[i + 1].affected_range();

            // For inserts at the same position, order matters but they don't conflict
            if curr_range.end.0 > next_range.start.0 {
                // Check if this is two inserts at the same position
                let both_inserts = matches!(&sorted[i], EditOp::Insert { .. })
                    && matches!(&sorted[i + 1], EditOp::Insert { .. });

                if !both_inserts {
                    return Err(AstError::OverlappingEdits {
                        first: curr_range,
                        second: next_range,
                    });
                }
            }
        }

        Ok(())
    }

    /// Apply all edits to a node
    pub fn apply(&self, node: &Arc<AstNode>) -> Result<Arc<AstNode>, AstError> {
        let mut result = node.clone();

        // Apply edits in the order they were prepared (reverse positional order)
        for edit in &self.edits {
            result = apply_edit(&result, edit)?;
        }

        Ok(result)
    }

    /// Merge adjacent edits where possible
    pub fn optimize(&mut self) {
        if self.edits.len() < 2 {
            return;
        }

        // Sort by position for merging
        self.edits.sort_by_key(|edit| match edit {
            EditOp::Insert { offset, .. } => *offset,
            EditOp::Delete { range } | EditOp::Replace { range, .. } => range.start.0,
        });

        let mut optimized = Vec::new();
        let mut i = 0;

        while i < self.edits.len() {
            let current = self.edits[i].clone();

            // Try to merge with next edits
            let mut merged = current.clone();
            let mut j = i + 1;

            while j < self.edits.len() {
                if let Some(new_merged) = Self::try_merge_edits(&merged, &self.edits[j]) {
                    merged = new_merged;
                    j += 1;
                } else {
                    break;
                }
            }

            optimized.push(merged);
            i = j;
        }

        self.edits = optimized;
    }

    /// Try to merge two edits if possible
    fn try_merge_edits(first: &EditOp, second: &EditOp) -> Option<EditOp> {
        match (first, second) {
            // Adjacent deletes can be merged
            (EditOp::Delete { range: r1 }, EditOp::Delete { range: r2 })
                if r1.end.0 == r2.start.0 =>
            {
                Some(EditOp::Delete {
                    range: TextRange::new(r1.start.0, r2.end.0),
                })
            },

            // Delete followed by insert at same position = Replace
            (EditOp::Delete { range }, EditOp::Insert { offset, text })
                if range.end.0 == *offset =>
            {
                Some(EditOp::Replace {
                    range: *range,
                    text: text.clone(),
                })
            },

            // Other combinations cannot be easily merged
            _ => None,
        }
    }
}

impl Default for BatchedEdit {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder pattern for batched edits
pub struct BatchBuilder {
    batch: BatchedEdit,
}

impl BatchBuilder {
    /// Create a new batch builder
    pub fn new() -> Self {
        Self {
            batch: BatchedEdit::new(),
        }
    }

    /// Add an insert operation
    pub fn insert(mut self, offset: usize, text: impl Into<CompactString>) -> Self {
        self.batch.insert(offset, text);
        self
    }

    /// Add a delete operation
    pub fn delete(mut self, range: TextRange) -> Self {
        self.batch.delete(range);
        self
    }

    /// Add a replace operation
    pub fn replace(mut self, range: TextRange, text: impl Into<CompactString>) -> Self {
        self.batch.replace(range, text);
        self
    }

    /// Build and prepare the batch
    pub fn build(mut self) -> Result<BatchedEdit, AstError> {
        self.batch.prepare()?;
        Ok(self.batch)
    }
}

impl Default for BatchBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ast::TextRange, builder::AstBuilder, kind::SyntaxKind};

    #[test]
    fn test_batch_builder() {
        let batch = BatchBuilder::new()
            .insert(5, "hello")
            .delete(TextRange::new(10, 15))
            .replace(TextRange::new(20, 25), "world")
            .build();

        assert!(batch.is_ok());
        let batch = batch.expect("batch should build successfully");
        assert_eq!(batch.len(), 3);
    }

    #[test]
    fn test_overlapping_edits_detection() {
        let mut batch = BatchedEdit::new();
        batch.delete(TextRange::new(5, 10));
        batch.delete(TextRange::new(8, 15)); // Overlaps!

        let result = batch.prepare();
        assert!(result.is_err());
    }

    #[test]
    fn test_non_overlapping_edits() {
        let mut batch = BatchedEdit::new();
        batch.delete(TextRange::new(5, 10));
        batch.delete(TextRange::new(15, 20));
        batch.insert(25, "hello");

        let result = batch.prepare();
        assert!(result.is_ok());
    }

    #[test]
    fn test_adjacent_inserts_allowed() {
        let mut batch = BatchedEdit::new();
        batch.insert(10, "hello");
        batch.insert(10, "world"); // Same position is OK for inserts

        let result = batch.prepare();
        assert!(result.is_ok());
    }

    #[test]
    fn test_edit_optimization() {
        let mut batch = BatchedEdit::new();

        // These adjacent deletes should merge
        batch.delete(TextRange::new(5, 10));
        batch.delete(TextRange::new(10, 15));

        // This delete followed by insert should become replace
        batch.delete(TextRange::new(20, 25));
        batch.insert(25, "hello");

        batch.optimize();

        // Should have 2 edits: one merged delete and one replace
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn test_batch_apply() {
        // Build a simple AST
        let token = AstBuilder::token(SyntaxKind::Text, "hello world", TextRange::new(0, 11));
        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 11))
            .add_child(token)
            .finish();
        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 11))
            .add_child(paragraph)
            .finish();

        // Create a batch of edits
        // Note: When applying in reverse order, insert at 11 (end of original text)
        // will happen before the replace, so it inserts after "world"
        let batch = BatchBuilder::new()
            .replace(TextRange::new(0, 5), "hi") // "hello" -> "hi"
            .insert(11, "!") // Insert ! at end of "hello world"
            .build()
            .expect("batch should build");

        // Apply the batch
        let result = batch.apply(&doc);
        assert!(result.is_ok());

        let edited = result.expect("batch apply should succeed");
        let mut text = String::new();
        edited.collect_text(&mut text);
        assert_eq!(text, "hi world!");
    }

    #[test]
    fn test_reverse_order_application() {
        // Ensure edits are applied in reverse order
        let token = AstBuilder::token(SyntaxKind::Text, "abcdef", TextRange::new(0, 6));
        let root = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 6))
            .add_child(token)
            .finish();

        let batch = BatchBuilder::new()
            .insert(2, "X") // "ab[X]cdef"
            .insert(4, "Y") // "abcd[Y]ef" (offset 4 in original)
            .build()
            .expect("batch should build");

        let result = batch.apply(&root);
        assert!(result.is_ok());

        let edited = result.expect("batch apply should succeed");
        let mut text = String::new();
        edited.collect_text(&mut text);
        // When applied in reverse order: first Y at 4 -> "abcd[Y]ef", then X at 2 -> "ab[X]cd[Y]ef"
        assert_eq!(text, "abXcdYef");
    }
}
