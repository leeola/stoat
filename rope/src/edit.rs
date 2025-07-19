//! Edit operations for the rope AST

use crate::ast::{AstError, AstNode, MAX_CHILDREN, MIN_CHILDREN, TextInfo, TextRange};
use compact_str::CompactString;
use smallvec::SmallVec;
use std::sync::Arc;

/// An edit operation on the rope
#[derive(Debug, Clone)]
pub enum EditOp {
    /// Insert text at the given offset
    Insert { offset: usize, text: CompactString },
    /// Delete the given range
    Delete { range: TextRange },
    /// Replace the given range with new text
    Replace {
        range: TextRange,
        text: CompactString,
    },
}

impl EditOp {
    /// Get the range affected by this edit
    pub fn affected_range(&self) -> TextRange {
        match self {
            EditOp::Insert { offset, .. } => TextRange::new(*offset, *offset),
            EditOp::Delete { range } | EditOp::Replace { range, .. } => *range,
        }
    }
}

/// Apply an edit to a node, returning a new node with the edit applied
pub fn apply_edit(node: &Arc<AstNode>, edit: &EditOp) -> Result<Arc<AstNode>, AstError> {
    let edited = match edit {
        EditOp::Insert { offset, text } => insert_at(node, *offset, text),
        EditOp::Delete { range } => delete_range(node, *range),
        EditOp::Replace { range, text } => {
            // Replace is delete + insert
            let after_delete = delete_range(node, *range)?;
            insert_at(&after_delete, range.start.0, text)
        },
    }?;

    // Balance the tree after edit if needed
    let balanced = balance_node(&edited)?;

    // Check if we need to split at the root level
    handle_root_split(balanced)
}

/// Insert text at the given offset
fn insert_at(
    node: &Arc<AstNode>,
    offset: usize,
    text: &CompactString,
) -> Result<Arc<AstNode>, AstError> {
    let node_range = node.range();

    // Check if offset is within this node
    if offset < node_range.start.0 || offset > node_range.end.0 {
        return Ok(node.clone());
    }

    match node.as_ref() {
        AstNode::Token {
            kind,
            text: node_text,
            range,
            semantic,
        } => {
            // Calculate offset within the token
            let local_offset = offset - range.start.0;

            if local_offset > node_text.len() {
                return Ok(node.clone());
            }

            // Insert into the token text
            let mut new_text = CompactString::new("");
            new_text.push_str(&node_text[..local_offset]);
            new_text.push_str(text);
            new_text.push_str(&node_text[local_offset..]);

            // Create new token with updated text and range
            Ok(Arc::new(AstNode::Token {
                kind: *kind,
                text: new_text,
                range: TextRange::new(range.start.0, range.end.0 + text.len()),
                semantic: *semantic, // Preserve semantic info
            }))
        },
        AstNode::Syntax {
            kind,
            children,
            semantic,
            ..
        } => {
            // Find which child contains the offset
            let mut new_children = SmallVec::new();
            let mut found = false;
            let mut text_shift = 0;

            for (child, child_info) in children {
                let child_range = child.range();

                if !found && offset >= child_range.start.0 && offset <= child_range.end.0 {
                    // Apply edit to this child
                    let edited_child = insert_at(child, offset, text)?;
                    let new_info = edited_child.text_info();
                    new_children.push((edited_child, new_info));
                    text_shift = text.len();
                    found = true;
                } else if found {
                    // Shift subsequent children
                    let shifted_child = shift_node_range(child, text_shift as isize);
                    new_children.push((shifted_child, *child_info));
                } else {
                    new_children.push((child.clone(), *child_info));
                }
            }

            // Calculate new info
            let new_info = new_children
                .iter()
                .map(|(_, info)| info)
                .fold(TextInfo::empty(), |acc, info| acc.combine(info));

            Ok(Arc::new(AstNode::Syntax {
                kind: *kind,
                children: new_children,
                info: new_info,
                range: TextRange::new(node_range.start.0, node_range.end.0 + text_shift),
                semantic: *semantic, // Preserve semantic info
            }))
        },
    }
}

/// Delete a range of text
fn delete_range(node: &Arc<AstNode>, delete_range: TextRange) -> Result<Arc<AstNode>, AstError> {
    let node_range = node.range();

    // Check if ranges overlap
    if delete_range.end.0 <= node_range.start.0 || delete_range.start.0 >= node_range.end.0 {
        return Ok(node.clone());
    }

    match node.as_ref() {
        AstNode::Token {
            kind,
            text,
            range,
            semantic,
        } => {
            // Calculate overlap with token
            let start = delete_range.start.0.max(range.start.0);
            let end = delete_range.end.0.min(range.end.0);

            if start >= end {
                return Ok(node.clone());
            }

            // Convert to local offsets
            let local_start = start - range.start.0;
            let local_end = end - range.start.0;

            // Create new text with deletion
            let mut new_text = CompactString::new("");
            new_text.push_str(&text[..local_start]);
            new_text.push_str(&text[local_end..]);

            // If entire token is deleted, return a special marker
            if new_text.is_empty() {
                // TODO: Handle empty tokens properly
                return Err(AstError::NotImplemented);
            }

            Ok(Arc::new(AstNode::Token {
                kind: *kind,
                text: new_text,
                range: TextRange::new(range.start.0, range.end.0 - (local_end - local_start)),
                semantic: *semantic, // Preserve semantic info
            }))
        },
        AstNode::Syntax {
            kind,
            children,
            semantic,
            ..
        } => {
            let mut new_children = SmallVec::new();
            let mut total_deleted = 0;

            for (child, child_info) in children {
                let child_range = child.range();

                if delete_range.end.0 <= child_range.start.0 {
                    // Child is after deletion, shift it
                    let shifted_child = shift_node_range(child, -(total_deleted as isize));
                    new_children.push((shifted_child, *child_info));
                } else if delete_range.start.0 >= child_range.end.0 {
                    // Child is before deletion, keep as-is
                    new_children.push((child.clone(), *child_info));
                } else {
                    // Child overlaps with deletion
                    match delete_range_from_child(child, delete_range) {
                        Ok(Some(edited_child)) => {
                            let old_len = child_range.len();
                            let new_len = edited_child.range().len();
                            total_deleted += old_len - new_len;

                            let new_info = edited_child.text_info();
                            new_children.push((edited_child, new_info));
                        },
                        Ok(None) => {
                            // Child was completely deleted
                            total_deleted += child_range.len();
                        },
                        Err(e) => return Err(e),
                    }
                }
            }

            // Calculate new info
            let new_info = new_children
                .iter()
                .map(|(_, info)| info)
                .fold(TextInfo::empty(), |acc, info| acc.combine(info));

            Ok(Arc::new(AstNode::Syntax {
                kind: *kind,
                children: new_children,
                info: new_info,
                range: TextRange::new(node_range.start.0, node_range.end.0 - total_deleted),
                semantic: *semantic, // Preserve semantic info
            }))
        },
    }
}

/// Delete a range from a child node, returning None if the entire child is deleted
fn delete_range_from_child(
    child: &Arc<AstNode>,
    del_range: TextRange,
) -> Result<Option<Arc<AstNode>>, AstError> {
    let child_range = child.range();

    // Check if entire child is within delete range
    if del_range.start.0 <= child_range.start.0 && del_range.end.0 >= child_range.end.0 {
        return Ok(None);
    }

    // Otherwise, recursively delete from child
    delete_range(child, del_range).map(Some)
}

/// Shift a node's range by the given amount
fn shift_node_range(node: &Arc<AstNode>, shift: isize) -> Arc<AstNode> {
    if shift == 0 {
        return node.clone();
    }

    let shift_pos = |pos: usize| -> usize {
        if shift >= 0 {
            pos + (shift as usize)
        } else {
            pos.saturating_sub((-shift) as usize)
        }
    };

    match node.as_ref() {
        AstNode::Token {
            kind,
            text,
            range,
            semantic,
        } => Arc::new(AstNode::Token {
            kind: *kind,
            text: text.clone(),
            range: TextRange::new(shift_pos(range.start.0), shift_pos(range.end.0)),
            semantic: *semantic, // Preserve semantic info
        }),
        AstNode::Syntax {
            kind,
            children,
            info,
            range,
            semantic,
        } => {
            let new_children = children
                .iter()
                .map(|(child, child_info)| (shift_node_range(child, shift), *child_info))
                .collect();

            Arc::new(AstNode::Syntax {
                kind: *kind,
                children: new_children,
                info: *info,
                range: TextRange::new(shift_pos(range.start.0), shift_pos(range.end.0)),
                semantic: *semantic, // Preserve semantic info
            })
        },
    }
}

/// Balance a node after modifications, splitting or merging as needed
fn balance_node(node: &Arc<AstNode>) -> Result<Arc<AstNode>, AstError> {
    // First, try to merge underfull children
    let node = merge_underfull_children(node)?;

    // Then, handle splitting if needed
    match node.as_ref() {
        AstNode::Token { .. } => {
            // Token nodes don't need balancing beyond size checks
            if node.needs_split() {
                // For now, we don't split tokens automatically during edits
                // This would require more context about where to split
                Ok(node.clone())
            } else {
                Ok(node.clone())
            }
        },
        AstNode::Syntax { children, .. } => {
            if children.len() > MAX_CHILDREN {
                // Node has too many children, needs to be split
                // Return the node as-is, caller should handle the split
                Ok(node.clone())
            } else {
                Ok(node.clone())
            }
        },
    }
}

/// Merge underfull children in a syntax node
fn merge_underfull_children(node: &Arc<AstNode>) -> Result<Arc<AstNode>, AstError> {
    match node.as_ref() {
        AstNode::Token { .. } => Ok(node.clone()),
        AstNode::Syntax {
            kind,
            children,
            semantic,
            ..
        } => {
            if children.len() < MIN_CHILDREN && !children.is_empty() {
                // Try to merge small adjacent children of the same kind
                let mut new_children = SmallVec::new();
                let mut i = 0;

                while i < children.len() {
                    let (child, info) = &children[i];

                    // Check if we can merge with the next child
                    if i + 1 < children.len() && child.is_underfull() {
                        let (next_child, _next_info) = &children[i + 1];

                        if let Ok(merged) = child.try_merge_with(next_child) {
                            let merged_info = merged.text_info();
                            new_children.push((merged, merged_info));
                            i += 2; // Skip the next child since we merged it
                            continue;
                        }
                    }

                    new_children.push((child.clone(), *info));
                    i += 1;
                }

                // If we merged any children, create a new node
                if new_children.len() < children.len() {
                    let new_info = new_children
                        .iter()
                        .map(|(_, info)| info)
                        .fold(TextInfo::empty(), |acc, info| acc.combine(info));

                    let range = node.range();
                    Ok(Arc::new(AstNode::Syntax {
                        kind: *kind,
                        children: new_children,
                        info: new_info,
                        range,
                        semantic: *semantic, // Preserve semantic info
                    }))
                } else {
                    Ok(node.clone())
                }
            } else {
                Ok(node.clone())
            }
        },
    }
}

/// Handle splitting at the root level if needed
fn handle_root_split(node: Arc<AstNode>) -> Result<Arc<AstNode>, AstError> {
    match node.as_ref() {
        AstNode::Token { .. } => Ok(node),
        AstNode::Syntax { kind, children, .. } => {
            if children.len() > MAX_CHILDREN {
                // Split the node
                let (left, right) = node.split_syntax_node()?;

                // Create a new root with the same kind
                let mut new_root = AstNode::syntax(
                    *kind,
                    TextRange::new(left.range().start.0, right.range().end.0),
                );

                // Add the split nodes as children
                new_root.add_child(left)?;
                new_root.add_child(right)?;

                Ok(Arc::new(new_root))
            } else {
                Ok(node)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{builder::AstBuilder, kind::SyntaxKind};

    #[test]
    fn test_insert_into_token() {
        let token = AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5));

        // Insert in middle
        let edit = EditOp::Insert {
            offset: 2,
            text: "XXX".into(),
        };

        let result = apply_edit(&token, &edit);
        assert!(result.is_ok());
        let edited = result.expect("edit should succeed");

        assert_eq!(edited.token_text(), Some("heXXXllo"));
        assert_eq!(edited.range(), TextRange::new(0, 8));
    }

    #[test]
    fn test_delete_from_token() {
        let token = AstBuilder::token(SyntaxKind::Text, "hello world", TextRange::new(0, 11));

        // Delete middle portion
        let edit = EditOp::Delete {
            range: TextRange::new(5, 6),
        };

        let result = apply_edit(&token, &edit);
        assert!(result.is_ok());
        let edited = result.expect("edit should succeed");

        assert_eq!(edited.token_text(), Some("helloworld"));
        assert_eq!(edited.range(), TextRange::new(0, 10));
    }

    #[test]
    fn test_replace_in_token() {
        let token = AstBuilder::token(SyntaxKind::Text, "hello world", TextRange::new(0, 11));

        // Replace "world" with "rust"
        let edit = EditOp::Replace {
            range: TextRange::new(6, 11),
            text: "rust".into(),
        };

        let result = apply_edit(&token, &edit);
        assert!(result.is_ok());
        let edited = result.expect("edit should succeed");

        assert_eq!(edited.token_text(), Some("hello rust"));
        assert_eq!(edited.range(), TextRange::new(0, 10));
    }

    #[test]
    fn test_node_splitting() {
        use crate::ast::{MAX_CHILDREN, MIN_CHILDREN};

        // Create a node with MAX_CHILDREN children
        let mut builder = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 100));
        for i in 0..MAX_CHILDREN {
            let token = AstBuilder::token(
                SyntaxKind::Text,
                format!("token{i}"),
                TextRange::new(i * 10, (i + 1) * 10),
            );
            builder = builder.add_child(token);
        }
        let full_node = builder.finish();

        // Now try to add one more child by inserting text that would create a new token
        // For this test, let's just verify the split_syntax_node method works
        let result = full_node.split_syntax_node();
        assert!(result.is_ok());

        let (left, right) = result.expect("split should succeed");

        // Each half should have roughly half the children
        if let Some(left_children) = left.children() {
            assert!(left_children.len() >= MIN_CHILDREN);
            assert!(left_children.len() <= MAX_CHILDREN / 2 + 1);
        }

        if let Some(right_children) = right.children() {
            assert!(right_children.len() >= MIN_CHILDREN);
            assert!(right_children.len() <= MAX_CHILDREN / 2 + 1);
        }
    }

    #[test]
    fn test_node_merging_in_edits() {
        // Create two small adjacent syntax nodes that should be merged
        let token1 = AstBuilder::token(SyntaxKind::Text, "hello", TextRange::new(0, 5));
        let para1 = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, 5))
            .add_child(token1)
            .finish();

        let token2 = AstBuilder::token(SyntaxKind::Text, "world", TextRange::new(5, 10));
        let para2 = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(5, 10))
            .add_child(token2)
            .finish();

        // Create a document with these two small paragraphs
        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 10))
            .add_child(para1)
            .add_child(para2)
            .finish();

        // The merge_underfull_children function should merge these
        let result = merge_underfull_children(&doc);
        assert!(result.is_ok());
        let merged_doc = result.expect("merge should succeed");

        // Check that the paragraphs were merged
        if let Some(children) = merged_doc.children() {
            // Should have 1 paragraph instead of 2
            assert_eq!(children.len(), 1);

            let merged_para = &children[0].0;
            assert_eq!(merged_para.kind(), SyntaxKind::Paragraph);

            // The merged paragraph should have both tokens
            if let Some(tokens) = merged_para.children() {
                assert_eq!(tokens.len(), 2);
                assert_eq!(tokens[0].0.token_text(), Some("hello"));
                assert_eq!(tokens[1].0.token_text(), Some("world"));
            }
        }
    }
}
