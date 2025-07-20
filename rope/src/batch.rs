//! Batched edit operations for efficient multi-edit support

use crate::{
    ast::{AstError, AstNode},
    edit::{EditOp, apply_edit},
};
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

    /// Add a token insert operation to the batch
    pub fn insert(&mut self, token_index: usize, token: AstNode) {
        self.edits.push(EditOp::InsertTokens {
            token_index,
            tokens: vec![token],
        });
    }

    /// Add a token delete operation to the batch
    pub fn delete(&mut self, token_range: std::ops::Range<usize>) {
        self.edits.push(EditOp::DeleteTokens { token_range });
    }

    /// Add a token replace operation to the batch
    pub fn replace(&mut self, token_range: std::ops::Range<usize>, tokens: Vec<AstNode>) {
        self.edits.push(EditOp::ReplaceTokens {
            token_range,
            tokens,
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
    pub fn prepare(&mut self, root: &Arc<AstNode>) -> Result<(), AstError> {
        // Check for overlapping edits
        self.check_overlaps(root)?;

        // Sort in reverse order so we can apply from end to start
        // This avoids having to adjust offsets as we go
        self.edits.sort_by(|a, b| {
            let a_start = match a {
                EditOp::InsertTokens { token_index, .. } => *token_index,
                EditOp::DeleteTokens { token_range } => token_range.start,
                EditOp::ReplaceTokens { token_range, .. } => token_range.start,
            };
            let b_start = match b {
                EditOp::InsertTokens { token_index, .. } => *token_index,
                EditOp::DeleteTokens { token_range } => token_range.start,
                EditOp::ReplaceTokens { token_range, .. } => token_range.start,
            };
            b_start.cmp(&a_start)
        });

        Ok(())
    }

    /// Check for overlapping edits
    fn check_overlaps(&self, root: &Arc<AstNode>) -> Result<(), AstError> {
        // Create a sorted copy for checking
        let mut sorted = self.edits.clone();
        sorted.sort_by_key(|edit| match edit {
            EditOp::InsertTokens { token_index, .. } => *token_index,
            EditOp::DeleteTokens { token_range } => token_range.start,
            EditOp::ReplaceTokens { token_range, .. } => token_range.start,
        });

        // Check each pair of adjacent edits
        for i in 0..sorted.len().saturating_sub(1) {
            if let (Some(curr_range), Some(next_range)) = (
                sorted[i].affected_range(Some(root)),
                sorted[i + 1].affected_range(Some(root)),
            ) {
                // For inserts at the same position, order matters but they don't conflict
                if curr_range.end.0 > next_range.start.0 {
                    // Check if this is two inserts at the same position
                    let both_inserts = matches!(&sorted[i], EditOp::InsertTokens { .. })
                        && matches!(&sorted[i + 1], EditOp::InsertTokens { .. });

                    if !both_inserts {
                        return Err(AstError::OverlappingEdits {
                            first: curr_range,
                            second: next_range,
                        });
                    }
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
            EditOp::InsertTokens { token_index, .. } => *token_index,
            EditOp::DeleteTokens { token_range } => token_range.start,
            EditOp::ReplaceTokens { token_range, .. } => token_range.start,
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
            // Adjacent token deletes can be merged
            (
                EditOp::DeleteTokens { token_range: r1 },
                EditOp::DeleteTokens { token_range: r2 },
            ) if r1.end == r2.start => Some(EditOp::DeleteTokens {
                token_range: r1.start..r2.end,
            }),

            // Delete followed by insert at same position = Replace
            (
                EditOp::DeleteTokens { token_range },
                EditOp::InsertTokens {
                    token_index,
                    tokens,
                },
            ) if token_range.end == *token_index => Some(EditOp::ReplaceTokens {
                token_range: token_range.clone(),
                tokens: tokens.clone(),
            }),

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

    /// Add a token insert operation
    pub fn insert(mut self, token_index: usize, token: AstNode) -> Self {
        self.batch.insert(token_index, token);
        self
    }

    /// Add a token delete operation
    pub fn delete(mut self, token_range: std::ops::Range<usize>) -> Self {
        self.batch.delete(token_range);
        self
    }

    /// Add a token replace operation
    pub fn replace(mut self, token_range: std::ops::Range<usize>, tokens: Vec<AstNode>) -> Self {
        self.batch.replace(token_range, tokens);
        self
    }

    /// Build and prepare the batch
    pub fn build(mut self, root: &Arc<AstNode>) -> Result<BatchedEdit, AstError> {
        self.batch.prepare(root)?;
        Ok(self.batch)
    }
}

impl Default for BatchBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// FIXME: Batch tests temporarily disabled during token API transition
