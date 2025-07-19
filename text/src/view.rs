//! Views into text buffers

use crate::{
    TextSize,
    action::{ActionError, ActionResult, ExecutionResult, TextAction},
    buffer::{ChangeEvent, TextBuffer},
    cursor::TextCursor,
    cursor_collection::CursorCollection,
    edit::{Edit, EditOperation},
    range::TextRange,
    syntax::{SyntaxNode, unified_kind::SyntaxKind},
};
use parking_lot::RwLock;
use std::sync::Arc;

/// A view into a text buffer, showing a specific portion
pub struct TextView {
    inner: Arc<TextViewInner>,
}

pub(crate) struct TextViewInner {
    /// Reference to the buffer
    buffer: TextBuffer,
    /// Root node of this view
    view_root: RwLock<SyntaxNode>,
    /// Collection of cursors for this view
    cursors: RwLock<CursorCollection>,
}

impl TextView {
    /// Create a new view
    pub(crate) fn new(buffer: TextBuffer, root: SyntaxNode) -> Self {
        let cursors = CursorCollection::new(0.into(), root.clone());
        let inner = Arc::new(TextViewInner {
            buffer: buffer.clone(),
            view_root: RwLock::new(root),
            cursors: RwLock::new(cursors),
        });

        // Register this view with the buffer
        buffer.register_view(Arc::downgrade(&inner));

        Self { inner }
    }

    /// Get the buffer this view is attached to
    pub fn buffer(&self) -> &TextBuffer {
        &self.inner.buffer
    }

    /// Get a reference to the primary cursor
    pub fn primary_cursor(&self) -> impl std::ops::Deref<Target = TextCursor> + '_ {
        parking_lot::RwLockReadGuard::map(self.inner.cursors.read(), |c| c.primary())
    }

    /// Get a mutable reference to the primary cursor
    pub fn primary_cursor_mut(&self) -> impl std::ops::DerefMut<Target = TextCursor> + '_ {
        parking_lot::RwLockWriteGuard::map(self.inner.cursors.write(), |c| c.primary_mut())
    }

    /// Access the cursor collection
    pub fn cursors(&self) -> parking_lot::RwLockReadGuard<'_, CursorCollection> {
        self.inner.cursors.read()
    }

    /// Access the cursor collection mutably
    pub fn cursors_mut(&self) -> parking_lot::RwLockWriteGuard<'_, CursorCollection> {
        self.inner.cursors.write()
    }

    /// Get the root node of this view
    pub fn root(&self) -> SyntaxNode {
        self.inner.view_root.read().clone()
    }

    /// Set the root node of this view
    pub fn set_root(&mut self, node: SyntaxNode) {
        *self.inner.view_root.write() = node;
    }

    /// Expand the view to show the parent of the current root
    pub fn expand_to_parent(&mut self) -> bool {
        let current_root = self.inner.view_root.read().clone();
        if let Some(parent) = current_root.parent() {
            self.set_root(parent);
            true
        } else {
            false
        }
    }

    /// Narrow the view to show a specific child
    pub fn narrow_to_child(&mut self, index: usize) -> bool {
        let current_root = self.inner.view_root.read().clone();
        if let Some(child) = current_root.child(index) {
            self.set_root(child);
            true
        } else {
            false
        }
    }

    /// Get the visible text in this view
    pub fn text(&self) -> String {
        // TODO: Extract text from rope using view range
        self.inner.view_root.read().text().to_string()
    }

    /// Get the text range of this view
    pub fn text_range(&self) -> TextRange {
        self.inner.view_root.read().text_range()
    }

    /// Check if a buffer offset is visible in this view
    pub fn contains_offset(&self, offset: usize) -> bool {
        let range = self.text_range();
        range.contains((offset as u32).into())
    }

    /// Execute a action on this view
    pub fn execute_action(&self, action: &TextAction) -> ActionResult<ExecutionResult> {
        match action {
            // Movement actions
            TextAction::MoveLeft { count } => self.execute_move_left(*count),
            TextAction::MoveRight { count } => self.execute_move_right(*count),
            TextAction::MoveUp { count } => self.execute_move_up(*count),
            TextAction::MoveDown { count } => self.execute_move_down(*count),
            TextAction::MoveWordForward => self.execute_move_word_forward(),
            TextAction::MoveWordBackward => self.execute_move_word_backward(),
            TextAction::MoveToLineStart => self.execute_move_to_line_start(),
            TextAction::MoveToLineEnd => self.execute_move_to_line_end(),
            TextAction::MoveToDocumentStart => self.execute_move_to_document_start(),
            TextAction::MoveToDocumentEnd => self.execute_move_to_document_end(),
            TextAction::MoveToLine { line } => self.execute_move_to_line(*line),
            TextAction::MoveToOffset { offset } => self.execute_move_to_offset(*offset),

            // Selection actions
            TextAction::ExtendSelectionLeft { count } => self.execute_extend_selection_left(*count),
            TextAction::ExtendSelectionRight { count } => {
                self.execute_extend_selection_right(*count)
            },
            TextAction::ExtendSelectionToWordEnd => self.execute_extend_selection_to_word_end(),
            TextAction::ExtendSelectionToWordStart => self.execute_extend_selection_to_word_start(),
            TextAction::SelectLine => self.execute_select_line(),
            TextAction::SelectWord => self.execute_select_word(),
            TextAction::SelectAll => self.execute_select_all(),
            TextAction::ClearSelection => self.execute_clear_selection(),

            // Edit actions
            TextAction::InsertText { text } => self.execute_insert_text(text),
            TextAction::DeleteForward => self.execute_delete_forward(),
            TextAction::DeleteBackward => self.execute_delete_backward(),
            TextAction::DeleteWordForward => self.execute_delete_word_forward(),
            TextAction::DeleteWordBackward => self.execute_delete_word_backward(),
            TextAction::DeleteLine => self.execute_delete_line(),
            TextAction::ReplaceSelection { text } => self.execute_replace_selection(text),

            // Multi-cursor actions
            TextAction::AddCursorAbove => self.execute_add_cursor_above(),
            TextAction::AddCursorBelow => self.execute_add_cursor_below(),
            TextAction::AddCursorAtOffset { offset } => self.execute_add_cursor_at_offset(*offset),
            TextAction::RemoveSecondaryCursors => self.execute_remove_secondary_cursors(),
        }
    }

    // Movement implementations
    fn execute_move_left(&self, count: usize) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let old_pos = cursor.position();
            let new_pos = TextSize::from(u32::from(old_pos).saturating_sub(count as u32));
            cursor.set_position(new_pos);
            cursor.clear_selection();
            affected_ranges.push(TextRange::new(new_pos, old_pos));
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_move_right(&self, count: usize) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let buffer_len = self.inner.buffer.len() as u32;
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let old_pos = cursor.position();
            let new_pos = TextSize::from(
                u32::from(old_pos)
                    .saturating_add(count as u32)
                    .min(buffer_len),
            );
            cursor.set_position(new_pos);
            cursor.clear_selection();
            affected_ranges.push(TextRange::new(old_pos, new_pos));
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_move_word_forward(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let old_pos = cursor.position();
            let new_pos = self.inner.buffer.next_word_boundary(old_pos);
            cursor.set_position(new_pos);
            cursor.clear_selection();
            affected_ranges.push(TextRange::new(old_pos, new_pos));
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_move_word_backward(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let old_pos = cursor.position();
            let new_pos = self.inner.buffer.prev_word_boundary(old_pos);
            cursor.set_position(new_pos);
            cursor.clear_selection();
            affected_ranges.push(TextRange::new(new_pos, old_pos));
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_move_to_line_start(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let old_pos = cursor.position();
            let new_pos = self.inner.buffer.line_start_offset(old_pos);
            cursor.set_position(new_pos);
            cursor.clear_selection();
            affected_ranges.push(TextRange::new(new_pos, old_pos));
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_move_to_line_end(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let old_pos = cursor.position();
            let new_pos = self.inner.buffer.line_end_offset(old_pos);
            cursor.set_position(new_pos);
            cursor.clear_selection();
            affected_ranges.push(TextRange::new(old_pos, new_pos));
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_move_to_document_start(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let old_pos = cursor.position();
            let new_pos = TextSize::from(0);
            cursor.set_position(new_pos);
            cursor.clear_selection();
            affected_ranges.push(TextRange::new(new_pos, old_pos));
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_move_to_document_end(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let buffer_len = self.inner.buffer.len() as u32;
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let old_pos = cursor.position();
            let new_pos = TextSize::from(buffer_len);
            cursor.set_position(new_pos);
            cursor.clear_selection();
            affected_ranges.push(TextRange::new(old_pos, new_pos));
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_move_to_line(&self, line: usize) -> ActionResult<ExecutionResult> {
        if line == 0 {
            return Err(ActionError::InvalidLine { line });
        }

        let line_idx = line - 1; // Convert to 0-indexed
        let line_count = self.inner.buffer.line_count();

        if line_idx >= line_count {
            return Err(ActionError::InvalidLine { line });
        }

        let mut cursors = self.inner.cursors.write();
        let new_pos = self.inner.buffer.line_to_offset(line_idx);
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let old_pos = cursor.position();
            cursor.set_position(new_pos);
            cursor.clear_selection();
            affected_ranges.push(TextRange::new(old_pos.min(new_pos), old_pos.max(new_pos)));
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_move_to_offset(&self, offset: TextSize) -> ActionResult<ExecutionResult> {
        let buffer_len = self.inner.buffer.len() as u32;
        if u32::from(offset) > buffer_len {
            return Err(ActionError::InvalidPosition { position: offset });
        }

        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let old_pos = cursor.position();
            cursor.set_position(offset);
            cursor.clear_selection();
            affected_ranges.push(TextRange::new(old_pos.min(offset), old_pos.max(offset)));
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    // Stub implementations for other actions - these will be implemented next
    fn execute_move_up(&self, count: usize) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let current_pos = cursor.position();
            let current_line_idx = self.inner.buffer.offset_to_line(current_pos);

            // Calculate target line index
            let target_line_idx = current_line_idx.saturating_sub(count);

            if target_line_idx != current_line_idx {
                // Get current column position
                let current_line_start = self.inner.buffer.line_to_offset(current_line_idx);
                let current_col = current_pos - current_line_start;

                // Calculate new position on target line
                let target_line_start = self.inner.buffer.line_to_offset(target_line_idx);
                let target_line_end = self.inner.buffer.line_end_offset(target_line_start);
                let target_line_len = target_line_end - target_line_start;

                let new_pos = if current_col <= target_line_len {
                    target_line_start + current_col
                } else {
                    target_line_end
                };

                cursor.set_position(new_pos);
                cursor.clear_selection();
                affected_ranges.push(TextRange::new(
                    current_pos.min(new_pos),
                    current_pos.max(new_pos),
                ));
            }
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_move_down(&self, count: usize) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();
        let line_count = self.inner.buffer.line_count();

        for cursor in cursors.iter_mut() {
            let current_pos = cursor.position();
            let current_line_idx = self.inner.buffer.offset_to_line(current_pos);

            // Calculate target line index
            let target_line_idx = (current_line_idx + count).min(line_count.saturating_sub(1));

            if target_line_idx != current_line_idx {
                // Get current column position
                let current_line_start = self.inner.buffer.line_to_offset(current_line_idx);
                let current_col = current_pos - current_line_start;

                // Calculate new position on target line
                let target_line_start = self.inner.buffer.line_to_offset(target_line_idx);
                let target_line_end = self.inner.buffer.line_end_offset(target_line_start);
                let target_line_len = target_line_end - target_line_start;

                let new_pos = if current_col <= target_line_len {
                    target_line_start + current_col
                } else {
                    target_line_end
                };

                cursor.set_position(new_pos);
                cursor.clear_selection();
                affected_ranges.push(TextRange::new(
                    current_pos.min(new_pos),
                    current_pos.max(new_pos),
                ));
            }
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_extend_selection_left(&self, count: usize) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let current_pos = cursor.position();
            let new_pos = TextSize::from(u32::from(current_pos).saturating_sub(count as u32));

            // Extend or create selection
            let selection = if let Some(existing) = cursor.selection() {
                // Extend existing selection
                TextRange::new(existing.start(), new_pos)
            } else {
                // Create new selection from current position
                TextRange::new(current_pos, new_pos)
            };

            cursor.set_selection(Some(selection));
            cursor.set_position(new_pos);
            affected_ranges.push(selection);
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_extend_selection_right(&self, count: usize) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let buffer_len = self.inner.buffer.len() as u32;
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let current_pos = cursor.position();
            let new_pos = TextSize::from(
                u32::from(current_pos)
                    .saturating_add(count as u32)
                    .min(buffer_len),
            );

            // Extend or create selection
            let selection = if let Some(existing) = cursor.selection() {
                // Extend existing selection
                TextRange::new(existing.start(), new_pos)
            } else {
                // Create new selection from current position
                TextRange::new(current_pos, new_pos)
            };

            cursor.set_selection(Some(selection));
            cursor.set_position(new_pos);
            affected_ranges.push(selection);
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_extend_selection_to_word_end(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let current_pos = cursor.position();

            // Find next word boundary
            let new_pos = self.inner.buffer.next_word_boundary(current_pos);

            // Extend or create selection
            let selection = if let Some(existing) = cursor.selection() {
                // Extend existing selection
                TextRange::new(existing.start(), new_pos)
            } else {
                // Create new selection from current position
                TextRange::new(current_pos, new_pos)
            };

            cursor.set_selection(Some(selection));
            cursor.set_position(new_pos);
            affected_ranges.push(selection);
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_extend_selection_to_word_start(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let current_pos = cursor.position();

            // Find previous word boundary
            let new_pos = self.inner.buffer.prev_word_boundary(current_pos);

            // Extend or create selection
            let selection = if let Some(existing) = cursor.selection() {
                // Extend existing selection
                TextRange::new(existing.start(), new_pos)
            } else {
                // Create new selection from current position
                TextRange::new(current_pos, new_pos)
            };

            cursor.set_selection(Some(selection));
            cursor.set_position(new_pos);
            affected_ranges.push(selection);
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: None,
        })
    }

    fn execute_select_line(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        for cursor in cursors.iter_mut() {
            let current_pos = cursor.position();

            // Use buffer methods to find line boundaries
            // TODO: Use AST when we have proper generic trait methods
            let line_start = self.inner.buffer.line_start_offset(current_pos);
            let line_end = self.inner.buffer.line_end_offset(current_pos);
            let line_range = TextRange::new(line_start, line_end);
            cursor.set_selection(Some(line_range));
            cursor.set_position(line_start);
            affected_ranges.push(line_range);
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges,
            message: Some("Line selected".to_string()),
        })
    }

    fn execute_select_word(&self) -> ActionResult<ExecutionResult> {
        // FIXME: multi-cursor support - currently only handles primary cursor
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();
        let buffer = &self.inner.buffer;

        for cursor in cursors.iter_mut() {
            let current_pos = cursor.position();

            // Check if we're at a word boundary
            if buffer.is_word_boundary(current_pos) {
                // If at boundary, we might be at start or end of a word
                // Try moving one character forward to see if we're at start of word
                let next_pos = TextSize::from(u32::from(current_pos) + 1);
                if !buffer.is_word_boundary(next_pos) {
                    // We're at the start of a word, select it
                    let word_end = buffer.next_word_boundary(current_pos);
                    if word_end > current_pos {
                        let word_range = TextRange::new(current_pos, word_end);
                        cursor.set_selection(Some(word_range));
                        cursor.set_position(current_pos);
                        affected_ranges.push(word_range);
                    }
                }
                // If not at start of word, we're in whitespace - don't select
            } else {
                // We're inside a word, find its boundaries
                let word_start = buffer.prev_word_boundary(current_pos);
                // Find end of current word (not start of next word)
                let word_end = self.find_word_end(current_pos);

                if word_end > word_start {
                    let word_range = TextRange::new(word_start, word_end);
                    cursor.set_selection(Some(word_range));
                    cursor.set_position(word_start);
                    affected_ranges.push(word_range);
                }
            }
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges: affected_ranges.clone(),
            message: if affected_ranges.is_empty() {
                Some("No word to select".to_string())
            } else {
                Some("Word selected".to_string())
            },
        })
    }

    fn execute_select_all(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let root = self.inner.buffer.syntax();
        let document_range = root.text_range();

        for cursor in cursors.iter_mut() {
            cursor.set_selection(Some(document_range));
            cursor.set_position(document_range.start());
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges: vec![document_range],
            message: Some("All text selected".to_string()),
        })
    }

    fn execute_clear_selection(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        for cursor in cursors.iter_mut() {
            cursor.clear_selection();
        }
        Ok(ExecutionResult {
            success: true,
            affected_ranges: Vec::new(),
            message: Some("Selection cleared".to_string()),
        })
    }

    fn execute_insert_text(&self, text: &str) -> ActionResult<ExecutionResult> {
        // FIXME: Handle all cursors, not just primary. Multi-cursor editing needs:
        // - Batch edits for all cursor positions
        // - Handle overlapping edits
        // - Maintain cursor ordering
        let mut cursors = self.inner.cursors.write();
        let primary_cursor = cursors.primary_mut();
        let pos = primary_cursor.position();
        let root = self.inner.buffer.syntax();

        primary_cursor.set_current_node(root.clone());

        // NOTE: Uses document-level InsertAt which is acceptable for insertion
        // Unlike delete operations, insertions don't need precise node targeting
        // since buffer.convert_to_rope_edit() handles offset conversion correctly
        let edit = Edit {
            target: root.clone(),
            operation: EditOperation::InsertAt {
                offset: u32::from(pos) as usize,
                text: text.to_string(),
            },
        };

        // Drop the cursor lock before applying edit to avoid deadlock
        drop(cursors);

        // Apply edit
        self.inner
            .buffer
            .apply_edit(&edit)
            .map_err(|e| ActionError::EditFailed { source: e })?;

        Ok(ExecutionResult {
            success: true,
            affected_ranges: vec![TextRange::new(pos, pos + TextSize::from(text.len() as u32))],
            message: None,
        })
    }

    fn execute_delete_forward(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let buffer_len = self.inner.buffer.len();

        // FIXME: Handle all cursors for multi-cursor delete forward
        let primary_cursor = cursors.primary_mut();
        let pos = primary_cursor.position();

        if u32::from(pos) >= buffer_len as u32 {
            return Ok(ExecutionResult {
                success: false,
                affected_ranges: Vec::new(),
                message: Some("Already at end of document".to_string()),
            });
        }

        // Delete a single character forward
        let delete_end = pos + TextSize::from(1);
        let delete_range = TextRange::new(pos, delete_end);

        // Find the node containing the position to delete
        let root = self.inner.buffer.syntax();
        let node = root
            .find_node_at_offset(pos)
            .ok_or(ActionError::AstNotAvailable)?;

        // Calculate the position within the node
        let node_range = node.text_range();
        let start_in_node = (u32::from(pos) - u32::from(node_range.start())) as usize;
        let end_in_node = (u32::from(delete_end) - u32::from(node_range.start())) as usize;

        // Use the precise DeleteRange operation
        let edit = Edit::delete_range(node, start_in_node, end_in_node);

        // Cursor stays at the same position for forward delete

        drop(cursors);
        self.inner
            .buffer
            .apply_edit(&edit)
            .map_err(|e| ActionError::EditFailed { source: e })?;

        Ok(ExecutionResult {
            success: true,
            affected_ranges: vec![delete_range],
            message: None,
        })
    }

    fn execute_delete_backward(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();

        // FIXME: Handle all cursors for multi-cursor delete backward
        let primary_cursor = cursors.primary_mut();
        let pos = primary_cursor.position();

        if pos == TextSize::from(0) {
            return Ok(ExecutionResult {
                success: false,
                affected_ranges: Vec::new(),
                message: Some("Already at start of document".to_string()),
            });
        }

        // Delete a single character backward
        let delete_start = pos - TextSize::from(1);
        let delete_range = TextRange::new(delete_start, pos);

        // Find the node containing the position to delete
        let root = self.inner.buffer.syntax();
        let node = root
            .find_node_at_offset(delete_start)
            .ok_or(ActionError::AstNotAvailable)?;

        // Calculate the position within the node
        let node_range = node.text_range();
        let start_in_node = (u32::from(delete_start) - u32::from(node_range.start())) as usize;
        let end_in_node = (u32::from(pos) - u32::from(node_range.start())) as usize;

        // Use the precise DeleteRange operation
        let edit = Edit::delete_range(node, start_in_node, end_in_node);

        // Don't manually set cursor position - let on_buffer_change handle it
        // The cursor will be automatically moved to delete_start due to the change event

        drop(cursors);
        self.inner
            .buffer
            .apply_edit(&edit)
            .map_err(|e| ActionError::EditFailed { source: e })?;

        Ok(ExecutionResult {
            success: true,
            affected_ranges: vec![delete_range],
            message: None,
        })
    }

    fn execute_delete_word_forward(&self) -> ActionResult<ExecutionResult> {
        // FIXME: multi-cursor support - currently only handles primary cursor
        let cursors = self.inner.cursors.read();
        let current_pos = cursors.primary().position();
        drop(cursors);

        // Find the end of current word or next word if in whitespace
        let word_end = self.find_delete_word_forward_end(current_pos);

        // Check if there's anything to delete
        if word_end <= current_pos {
            return Ok(ExecutionResult {
                success: false,
                affected_ranges: Vec::new(),
                message: Some("No word to delete forward".to_string()),
            });
        }

        // Find the node containing the deletion range
        let root = self.inner.buffer.syntax();
        if let Some(node) = root.find_node_at_offset(current_pos) {
            let node_range = node.text_range();
            let delete_start = u32::from(current_pos - node_range.start()) as usize;
            let delete_end = u32::from(word_end - node_range.start()) as usize;

            // Ensure deletion doesn't exceed node bounds
            let node_len = u32::from(node_range.len()) as usize;
            if delete_end <= node_len {
                // Create precise delete edit within the node
                let edit = Edit::delete_range(node, delete_start, delete_end);

                // Apply the edit
                self.inner
                    .buffer
                    .apply_edit(&edit)
                    .map_err(|e| ActionError::EditFailed { source: e })?;

                let delete_range = TextRange::new(current_pos, word_end);
                return Ok(ExecutionResult {
                    success: true,
                    affected_ranges: vec![delete_range],
                    message: None,
                });
            }
        }

        Ok(ExecutionResult {
            success: false,
            affected_ranges: Vec::new(),
            message: Some("Could not delete word forward".to_string()),
        })
    }

    fn execute_delete_word_backward(&self) -> ActionResult<ExecutionResult> {
        // FIXME: multi-cursor support - currently only handles primary cursor
        let mut cursors = self.inner.cursors.write();
        let current_pos = cursors.primary().position();

        // Find the start and end positions for delete word backward operation
        let (start_pos, end_pos) = self.find_delete_word_backward_range(current_pos);

        // Check if there's anything to delete
        if end_pos == start_pos {
            return Ok(ExecutionResult {
                success: false,
                affected_ranges: Vec::new(),
                message: Some("No word to delete backward".to_string()),
            });
        }

        // Update cursor position to where deletion starts
        cursors.primary_mut().set_position(start_pos);
        drop(cursors);

        // Find the node containing the deletion range
        let root = self.inner.buffer.syntax();
        if let Some(node) = root.find_node_at_offset(start_pos) {
            let node_range = node.text_range();
            let delete_start = u32::from(start_pos - node_range.start()) as usize;
            let delete_end = u32::from(end_pos - node_range.start()) as usize;

            // Ensure deletion doesn't exceed node bounds
            let node_len = u32::from(node_range.len()) as usize;
            if delete_end <= node_len {
                // Create precise delete edit within the node
                let edit = Edit::delete_range(node, delete_start, delete_end);

                // Apply the edit
                self.inner
                    .buffer
                    .apply_edit(&edit)
                    .map_err(|e| ActionError::EditFailed { source: e })?;

                let delete_range = TextRange::new(start_pos, end_pos);
                return Ok(ExecutionResult {
                    success: true,
                    affected_ranges: vec![delete_range],
                    message: None,
                });
            }
        }

        Ok(ExecutionResult {
            success: false,
            affected_ranges: Vec::new(),
            message: Some("Could not delete word backward".to_string()),
        })
    }

    fn execute_delete_line(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();

        // FIXME: Handle all cursors for multi-cursor delete line
        let primary_cursor = cursors.primary_mut();
        let pos = primary_cursor.position();

        // Get line boundaries using buffer methods
        let line_start = self.inner.buffer.line_start_offset(pos);
        let line_end = self.inner.buffer.line_end_offset(pos);

        // Determine actual deletion range (include newline if not at end of buffer)
        let buffer_len = TextSize::from(self.inner.buffer.len() as u32);
        let delete_end = if line_end < buffer_len {
            // Include the newline character
            line_end + TextSize::from(1)
        } else {
            // Last line, no newline to include
            line_end
        };

        let delete_range = TextRange::new(line_start, delete_end);

        // Update cursor to start of deleted line
        primary_cursor.set_position(line_start);
        drop(cursors);

        // Find the node containing the deletion range and apply precise edit
        let root = self.inner.buffer.syntax();
        if let Some(node) = root.find_node_at_offset(line_start) {
            let node_range = node.text_range();
            let delete_start_in_node = u32::from(line_start - node_range.start()) as usize;
            let delete_end_in_node = u32::from(delete_end - node_range.start()) as usize;

            // Ensure deletion doesn't exceed node bounds
            let node_len = u32::from(node_range.len()) as usize;
            if delete_end_in_node <= node_len {
                // Create precise delete edit within the node
                let edit = Edit::delete_range(node, delete_start_in_node, delete_end_in_node);

                // Apply the edit
                self.inner
                    .buffer
                    .apply_edit(&edit)
                    .map_err(|e| ActionError::EditFailed { source: e })?;

                return Ok(ExecutionResult {
                    success: true,
                    affected_ranges: vec![delete_range],
                    message: None,
                });
            }
        }

        Ok(ExecutionResult {
            success: false,
            affected_ranges: Vec::new(),
            message: Some("Could not delete line".to_string()),
        })
    }

    fn execute_replace_selection(&self, text: &str) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let mut affected_ranges = Vec::new();

        // FIXME: Handle all cursors for multi-cursor replace selection
        let primary_cursor = cursors.primary_mut();

        if let Some(selection) = primary_cursor.selection() {
            let selection_start = selection.start();
            let selection_end = selection.end();

            // Update cursor position to end of inserted text
            let new_pos = selection_start + TextSize::from(text.len() as u32);
            primary_cursor.set_position(new_pos);
            primary_cursor.clear_selection();

            drop(cursors);

            // Find the node containing the selection and apply precise edit
            let root = self.inner.buffer.syntax();
            if let Some(node) = root.find_node_at_offset(selection_start) {
                let node_range = node.text_range();
                let replace_start_in_node =
                    u32::from(selection_start - node_range.start()) as usize;
                let replace_end_in_node = u32::from(selection_end - node_range.start()) as usize;

                // Ensure replacement doesn't exceed node bounds
                let node_len = u32::from(node_range.len()) as usize;
                if replace_end_in_node <= node_len {
                    // Create precise replace edit within the node
                    let edit = Edit::replace_range(
                        node,
                        replace_start_in_node,
                        replace_end_in_node,
                        text.to_string(),
                    );

                    // Apply the edit
                    self.inner
                        .buffer
                        .apply_edit(&edit)
                        .map_err(|e| ActionError::EditFailed { source: e })?;

                    let new_range = TextRange::new(
                        selection_start,
                        selection_start + TextSize::from(text.len() as u32),
                    );
                    affected_ranges.push(new_range);

                    return Ok(ExecutionResult {
                        success: true,
                        affected_ranges,
                        message: None,
                    });
                }
            }

            Ok(ExecutionResult {
                success: false,
                affected_ranges: Vec::new(),
                message: Some("Could not replace selection".to_string()),
            })
        } else {
            // No selection, insert at cursor position
            drop(cursors);
            self.execute_insert_text(text)
        }
    }

    fn execute_add_cursor_above(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let root = self.inner.buffer.syntax();

        // Get the primary cursor position
        let primary_pos = cursors.primary().position();
        let current_line_idx = self.inner.buffer.offset_to_line(primary_pos);

        if current_line_idx > 0 {
            // Get current column position
            let current_line_start = self.inner.buffer.line_to_offset(current_line_idx);
            let current_col = primary_pos - current_line_start;

            // Calculate new position on line above
            let target_line_idx = current_line_idx - 1;
            let target_line_start = self.inner.buffer.line_to_offset(target_line_idx);
            let target_line_end = self.inner.buffer.line_end_offset(target_line_start);
            let target_line_len = target_line_end - target_line_start;

            let new_pos = if current_col <= target_line_len {
                target_line_start + current_col
            } else {
                target_line_end
            };

            cursors.add_cursor(new_pos, root);

            Ok(ExecutionResult {
                success: true,
                affected_ranges: vec![TextRange::new(new_pos, new_pos)],
                message: Some("Cursor added above".to_string()),
            })
        } else {
            Ok(ExecutionResult {
                success: false,
                affected_ranges: Vec::new(),
                message: Some("Already at first line".to_string()),
            })
        }
    }

    fn execute_add_cursor_below(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();
        let root = self.inner.buffer.syntax();
        let line_count = self.inner.buffer.line_count();

        // Get the primary cursor position
        let primary_pos = cursors.primary().position();
        let current_line_idx = self.inner.buffer.offset_to_line(primary_pos);

        if current_line_idx + 1 < line_count {
            // Get current column position
            let current_line_start = self.inner.buffer.line_to_offset(current_line_idx);
            let current_col = primary_pos - current_line_start;

            // Calculate new position on line below
            let target_line_idx = current_line_idx + 1;
            let target_line_start = self.inner.buffer.line_to_offset(target_line_idx);
            let target_line_end = self.inner.buffer.line_end_offset(target_line_start);
            let target_line_len = target_line_end - target_line_start;

            let new_pos = if current_col <= target_line_len {
                target_line_start + current_col
            } else {
                target_line_end
            };

            cursors.add_cursor(new_pos, root);

            Ok(ExecutionResult {
                success: true,
                affected_ranges: vec![TextRange::new(new_pos, new_pos)],
                message: Some("Cursor added below".to_string()),
            })
        } else {
            Ok(ExecutionResult {
                success: false,
                affected_ranges: Vec::new(),
                message: Some("Already at last line".to_string()),
            })
        }
    }

    fn execute_add_cursor_at_offset(&self, offset: TextSize) -> ActionResult<ExecutionResult> {
        let buffer_len = self.inner.buffer.len() as u32;
        if u32::from(offset) > buffer_len {
            return Err(ActionError::InvalidPosition { position: offset });
        }

        let mut cursors = self.inner.cursors.write();
        let root = self.inner.buffer.syntax();
        cursors.add_cursor(offset, root);

        Ok(ExecutionResult {
            success: true,
            affected_ranges: vec![TextRange::new(offset, offset)],
            message: Some("Cursor added".to_string()),
        })
    }

    fn execute_remove_secondary_cursors(&self) -> ActionResult<ExecutionResult> {
        let mut cursors = self.inner.cursors.write();

        // Get all non-primary cursor IDs
        let to_remove: Vec<_> = cursors
            .iter()
            .map(|c| c.id())
            .filter(|&id| id != cursors.primary().id())
            .collect();

        for id in to_remove {
            cursors.remove_cursor(id);
        }

        Ok(ExecutionResult {
            success: true,
            affected_ranges: Vec::new(),
            message: Some("Secondary cursors removed".to_string()),
        })
    }

    /// Find the end of the word containing the given position
    fn find_word_end(&self, position: TextSize) -> TextSize {
        let buffer = &self.inner.buffer;
        let text = buffer.text();
        let bytes = text.as_bytes();
        let byte_idx = u32::from(position) as usize;

        if byte_idx >= bytes.len() {
            return TextSize::from(bytes.len() as u32);
        }

        let mut pos = byte_idx;

        // Scan forward while we're in a word (non-whitespace)
        while pos < bytes.len() && !bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }

        TextSize::from(pos as u32)
    }

    /// Find the end position for delete word forward operation
    fn find_delete_word_forward_end(&self, position: TextSize) -> TextSize {
        let buffer = &self.inner.buffer;
        let text = buffer.text();
        let bytes = text.as_bytes();
        let byte_idx = u32::from(position) as usize;

        if byte_idx >= bytes.len() {
            return TextSize::from(bytes.len() as u32);
        }

        let mut pos = byte_idx;

        // If we're on whitespace, skip whitespace first
        if pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
                pos += 1;
            }
        }

        // Now skip the word (non-whitespace)
        while pos < bytes.len() && !bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }

        TextSize::from(pos as u32)
    }

    /// Find the range to delete for delete word backward operation
    fn find_delete_word_backward_range(&self, position: TextSize) -> (TextSize, TextSize) {
        let buffer = &self.inner.buffer;
        let text = buffer.text();
        let bytes = text.as_bytes();
        let byte_idx = u32::from(position) as usize;

        if byte_idx == 0 {
            return (TextSize::from(0), TextSize::from(0));
        }

        let current_char_pos = byte_idx.saturating_sub(1);

        // Check if we're currently in a word or in whitespace
        let in_word =
            current_char_pos < bytes.len() && !bytes[current_char_pos].is_ascii_whitespace();

        if in_word {
            // Case 1: Cursor is inside a word - delete from word start to cursor position
            let mut word_start = current_char_pos;
            while word_start > 0 && !bytes[word_start - 1].is_ascii_whitespace() {
                word_start -= 1;
            }

            (TextSize::from(word_start as u32), position)
        } else {
            // Case 2: Cursor is in whitespace - find and delete the previous word
            let mut pos = current_char_pos;

            // Skip whitespace backward to find the end of previous word
            while pos > 0 && bytes[pos].is_ascii_whitespace() {
                pos = pos.saturating_sub(1);
            }

            if pos == 0 && bytes[0].is_ascii_whitespace() {
                return (TextSize::from(0), TextSize::from(0)); // No word to delete
            }

            // Find the end of the word (where we are now)
            let word_end = pos + 1;

            // Find the start of the word
            while pos > 0 && !bytes[pos - 1].is_ascii_whitespace() {
                pos -= 1;
            }

            let word_start = pos;

            (
                TextSize::from(word_start as u32),
                TextSize::from(word_end as u32),
            )
        }
    }
}

impl Clone for TextView {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl TextViewInner {
    /// Handle a change event from the buffer
    pub(crate) fn on_buffer_change(&self, event: &ChangeEvent) {
        // Update the view root to point to the new AST
        let new_root = self.buffer.syntax();
        *self.view_root.write() = new_root.clone();

        // Adjust cursor positions based on the change
        let mut cursors = self.cursors.write();

        // Calculate the adjustment needed
        let change_start = event.range.start();
        let change_end = event.range.end();
        let inserted_len = u32::from(event.inserted_len);
        let deleted_len = u32::from(event.deleted_len);

        // Update each cursor
        for cursor in cursors.iter_mut() {
            let current_pos = cursor.position();

            // If cursor is before the change, no adjustment needed
            if current_pos < change_start {
                // Position is unchanged
            }
            // If cursor is at the exact start of an insertion (no deletion), move after the
            // insertion
            else if current_pos == change_start && deleted_len == 0 {
                cursor.set_position(change_start + event.inserted_len);
            }
            // If cursor is within the deleted range, move to the start of the change
            else if current_pos <= change_end {
                cursor.set_position(change_start);
            }
            // If cursor is after the change, adjust by the size delta
            else {
                let current_pos_u32 = u32::from(current_pos);
                let new_pos_u32 = if inserted_len >= deleted_len {
                    // Net insertion
                    current_pos_u32 + (inserted_len - deleted_len)
                } else {
                    // Net deletion
                    current_pos_u32.saturating_sub(deleted_len - inserted_len)
                };
                cursor.set_position(TextSize::from(new_pos_u32));
            }

            // Update the cursor's current node to match the new AST
            // For now, just reset to the root - proper node tracking will come later
            cursor.set_current_node(new_root.clone());

            // Clear selection if it was affected by the change
            if let Some(selection) = cursor.selection() {
                // If selection overlaps with the change, clear it
                if selection.start() < event.range.end() && selection.end() > event.range.start() {
                    cursor.clear_selection();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::unified_kind::SyntaxKind;

    #[test]
    fn test_view_creation() {
        let buffer = TextBuffer::new("hello world");
        let view = buffer.create_view();
        assert_eq!(view.text(), "hello world");
    }

    #[test]
    fn test_view_cursor_position() {
        let buffer = TextBuffer::new("hello world");
        let view = buffer.create_view();
        assert_eq!(view.primary_cursor().position(), 0.into());

        view.primary_cursor_mut().set_position(5.into());
        assert_eq!(view.primary_cursor().position(), 5.into());
    }

    #[test]
    fn test_view_multiple_cursors() {
        let buffer = TextBuffer::new("hello world");
        let view = buffer.create_view();
        let root = view.root();

        assert_eq!(view.cursors().len(), 1);

        view.cursors_mut().add_cursor(5.into(), root.clone());
        view.cursors_mut().add_cursor(10.into(), root);
        assert_eq!(view.cursors().len(), 3);
    }

    #[test]
    fn test_view_cursor_adjustment_on_edit() {
        use crate::edit::Edit;

        let buffer = TextBuffer::new("hello world");
        let view = buffer.create_view();
        let root = view.root();

        // Place cursors at different positions
        view.cursors_mut().add_cursor(5.into(), root.clone()); // After "hello"
        view.cursors_mut().add_cursor(11.into(), root.clone()); // At end

        // Apply an edit that inserts text at position 0
        let edit = Edit::insert_before(root.clone(), "Hi! ".to_string());
        buffer.apply_edit(&edit).expect("Edit should succeed");

        // Check that cursors were adjusted
        let cursors = view.cursors();
        let positions: Vec<u32> = cursors.iter().map(|c| u32::from(c.position())).collect();

        // Original positions: 0, 5, 11
        // After inserting "Hi! " (4 chars) at start: 4, 9, 15
        assert_eq!(positions[0], 4); // Primary cursor moved
        assert_eq!(positions[1], 9); // Cursor at 5 moved to 9
        assert_eq!(positions[2], 15); // Cursor at 11 moved to 15
    }

    #[test]
    fn test_view_cursor_in_deleted_range() {
        use crate::edit::Edit;

        let buffer = TextBuffer::new("hello world");
        let view = buffer.create_view();
        let root = view.root();

        // Place a cursor in the middle of "hello"
        view.cursors_mut().add_cursor(3.into(), root.clone());

        // Delete the entire content
        let edit = Edit::delete(root);
        buffer.apply_edit(&edit).expect("Edit should succeed");

        // Check that cursor was moved to position 0
        let cursors = view.cursors();
        for cursor in cursors.iter() {
            assert_eq!(u32::from(cursor.position()), 0);
        }
    }
}
