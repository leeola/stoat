//! Text buffer with rope storage and lazy AST

use crate::{
    TextSize,
    edit::{Edit, EditError, EditOperation, RopeEdit},
    range::TextRange,
    syntax::{Syntax, SyntaxNode},
    view::TextView,
};
use parking_lot::RwLock;
use ropey::Rope;
use std::sync::{Arc, Weak};

/// Unique identifier for a buffer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(u64);

impl BufferId {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Event describing a change to the buffer
#[derive(Debug, Clone)]
pub struct ChangeEvent {
    /// The range that was affected
    pub range: TextRange,
    /// The length of text that was deleted
    pub deleted_len: TextSize,
    /// The length of text that was inserted
    pub inserted_len: TextSize,
    /// The new version number after the change
    pub new_version: u64,
}

/// A text buffer with efficient rope storage and lazy AST
pub struct TextBuffer<S: Syntax> {
    inner: Arc<BufferInner<S>>,
}

pub(crate) struct BufferInner<S: Syntax> {
    /// Unique ID for this buffer
    id: BufferId,
    /// The rope storing the actual text
    rope: RwLock<Rope>,
    /// Cached AST with version tracking
    ast: RwLock<AstCache<S>>,
    /// Version number for invalidation
    version: RwLock<u64>,
    /// All views of this buffer
    views: RwLock<Vec<Weak<crate::view::TextViewInner<S>>>>,
}

struct AstCache<S: Syntax> {
    /// The cached root node
    root: Option<SyntaxNode<S>>,
    /// Version when this was parsed
    version: u64,
}

impl<S: Syntax> TextBuffer<S> {
    /// Create a new buffer with the given text
    pub fn new(text: &str) -> Self {
        let rope = Rope::from_str(text);
        let inner = Arc::new(BufferInner {
            id: BufferId::new(),
            rope: RwLock::new(rope),
            ast: RwLock::new(AstCache {
                root: None,
                version: 0,
            }),
            version: RwLock::new(1),
            views: RwLock::new(Vec::new()),
        });

        // Parse initially
        let buffer = Self { inner };
        buffer.ensure_parsed();
        buffer
    }

    /// Get the buffer ID
    pub fn id(&self) -> BufferId {
        self.inner.id
    }

    /// Get the current text as a string
    pub fn text(&self) -> String {
        self.inner.rope.read().to_string()
    }

    /// Get the length in bytes
    pub fn len(&self) -> usize {
        self.inner.rope.read().len_bytes()
    }

    /// Check if the buffer is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the root syntax node
    pub fn syntax(&self) -> SyntaxNode<S> {
        self.ensure_parsed();
        self.inner
            .ast
            .read()
            .root
            .as_ref()
            .expect("AST should be parsed")
            .clone()
    }

    /// Create a view of the entire buffer
    pub fn create_view(&self) -> TextView<S> {
        TextView::new(self.clone(), self.syntax())
    }

    /// Create a view of a specific node
    pub fn create_view_of(&self, node: SyntaxNode<S>) -> TextView<S> {
        TextView::new(self.clone(), node)
    }

    /// Find nodes matching a predicate
    pub fn find_nodes(&self, predicate: impl Fn(&SyntaxNode<S>) -> bool) -> Vec<SyntaxNode<S>> {
        let mut results = Vec::new();
        let root = self.syntax();
        self.find_nodes_recursive(&root, &predicate, &mut results);
        results
    }

    fn find_nodes_recursive(
        &self,
        node: &SyntaxNode<S>,
        predicate: &impl Fn(&SyntaxNode<S>) -> bool,
        results: &mut Vec<SyntaxNode<S>>,
    ) {
        if predicate(node) {
            results.push(node.clone());
        }
        // TODO: Iterate through children when node parsing is implemented
    }

    /// Ensure the AST is parsed and up to date
    fn ensure_parsed(&self) {
        let current_version = *self.inner.version.read();
        let mut ast = self.inner.ast.write();

        if ast.version < current_version || ast.root.is_none() {
            // Need to parse
            let text = self.text();
            let parse_result = S::parse(&text);
            ast.root = Some(parse_result.root);
            ast.version = current_version;
        }
    }

    /// Register a view
    pub(crate) fn register_view(&self, view: Weak<crate::view::TextViewInner<S>>) {
        self.inner.views.write().push(view);
    }

    /// Apply an edit to the buffer
    pub fn apply_edit(&self, edit: &Edit<S>) -> Result<(), EditError> {
        // Convert AST edit to rope edit
        let rope_edit = self.convert_to_rope_edit(edit)?;

        // Calculate change metrics before applying
        let deleted_len = rope_edit.range.len();
        let inserted_len = TextSize::from(rope_edit.text.len() as u32);
        let change_range = rope_edit.range;

        // Apply to rope
        self.apply_rope_edit(&rope_edit)?;

        // Increment version and invalidate AST
        let new_version = {
            let mut version = self.inner.version.write();
            *version += 1;

            // Clear AST cache
            let mut ast = self.inner.ast.write();
            ast.root = None;

            *version
        };

        // Create change event
        let event = ChangeEvent {
            range: change_range,
            deleted_len,
            inserted_len,
            new_version,
        };

        // Notify views of the change
        self.notify_views(&event);

        Ok(())
    }

    /// Convert an AST-based edit to a rope edit
    fn convert_to_rope_edit(&self, edit: &Edit<S>) -> Result<RopeEdit, EditError> {
        let node_range = edit.target.text_range();

        match &edit.operation {
            EditOperation::Replace(text) => Ok(RopeEdit::replace(node_range, text.clone())),
            EditOperation::InsertBefore(text) => {
                Ok(RopeEdit::insert(node_range.start(), text.clone()))
            },
            EditOperation::InsertAfter(text) => {
                Ok(RopeEdit::insert(node_range.end(), text.clone()))
            },
            EditOperation::InsertAt { offset, text } => {
                // Convert offset relative to node to absolute position
                let position = node_range.start() + TextSize::from(*offset as u32);
                if position > node_range.end() {
                    return Err(EditError::RangeOutOfBounds {
                        position,
                        length: node_range.end(),
                    });
                }
                Ok(RopeEdit::insert(position, text.clone()))
            },
            EditOperation::Delete => Ok(RopeEdit::delete(node_range)),
            EditOperation::WrapWith { before, after } => {
                // This requires two edits, for now just do a simple replace
                let current_text = edit.target.text();
                let wrapped = format!("{before}{current_text}{after}");
                Ok(RopeEdit::replace(node_range, wrapped))
            },
            EditOperation::Unwrap => {
                // This is complex and requires understanding node structure
                // For now, return an error
                Err(EditError::NodeNotFound)
            },
        }
    }

    /// Apply a rope edit
    fn apply_rope_edit(&self, edit: &RopeEdit) -> Result<(), EditError> {
        let mut rope = self.inner.rope.write();
        let rope_len = rope.len_bytes();

        // Validate range
        let start_byte = edit.range.start().into();
        let end_byte = edit.range.end().into();

        if start_byte > end_byte {
            return Err(EditError::InvalidRange {
                start: edit.range.start(),
                end: edit.range.end(),
            });
        }

        if end_byte > rope_len {
            return Err(EditError::RangeOutOfBounds {
                position: edit.range.end(),
                length: TextSize::from(rope_len as u32),
            });
        }

        // Convert byte indices to char indices (ropey works with char indices)
        let start_char = rope.byte_to_char(start_byte);
        let end_char = rope.byte_to_char(end_byte);

        // Apply the edit
        rope.remove(start_char..end_char);
        rope.insert(start_char, &edit.text);

        Ok(())
    }

    /// Notify all registered views of a change
    fn notify_views(&self, event: &ChangeEvent) {
        let mut views = self.inner.views.write();

        // Remove any dead weak references
        views.retain(|weak| weak.strong_count() > 0);

        // Notify each view
        for view_weak in views.iter() {
            if let Some(view) = view_weak.upgrade() {
                view.on_buffer_change(event);
            }
        }
    }
}

impl<S: Syntax> Clone for TextBuffer<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{edit::EditOperation, syntax::simple::SimpleText};

    #[test]
    fn test_buffer_creation() {
        let buffer = TextBuffer::<SimpleText>::new("hello world");
        assert_eq!(buffer.text(), "hello world");
        assert_eq!(buffer.len(), 11);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_empty_buffer() {
        let buffer = TextBuffer::<SimpleText>::new("");
        assert_eq!(buffer.text(), "");
        assert_eq!(buffer.len(), 0);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_apply_replace_edit() {
        let buffer = TextBuffer::<SimpleText>::new("hello world");
        let root = buffer.syntax();

        let edit = Edit::replace(root, "goodbye world".to_string());
        buffer.apply_edit(&edit).expect("Edit should succeed");

        assert_eq!(buffer.text(), "goodbye world");
    }

    #[test]
    fn test_apply_insert_before_edit() {
        let buffer = TextBuffer::<SimpleText>::new("world");
        let root = buffer.syntax();

        let edit = Edit::insert_before(root, "hello ".to_string());
        buffer.apply_edit(&edit).expect("Edit should succeed");

        assert_eq!(buffer.text(), "hello world");
    }

    #[test]
    fn test_apply_insert_after_edit() {
        let buffer = TextBuffer::<SimpleText>::new("hello");
        let root = buffer.syntax();

        let edit = Edit::insert_after(root, " world".to_string());
        buffer.apply_edit(&edit).expect("Edit should succeed");

        assert_eq!(buffer.text(), "hello world");
    }

    #[test]
    fn test_apply_delete_edit() {
        let buffer = TextBuffer::<SimpleText>::new("hello world");
        let root = buffer.syntax();

        let edit = Edit::delete(root);
        buffer.apply_edit(&edit).expect("Edit should succeed");

        assert_eq!(buffer.text(), "");
    }

    #[test]
    fn test_apply_wrap_edit() {
        let buffer = TextBuffer::<SimpleText>::new("content");
        let root = buffer.syntax();

        let edit = Edit {
            target: root,
            operation: EditOperation::WrapWith {
                before: "(".to_string(),
                after: ")".to_string(),
            },
        };
        buffer.apply_edit(&edit).expect("Edit should succeed");

        assert_eq!(buffer.text(), "(content)");
    }

    #[test]
    fn test_version_increments_on_edit() {
        let buffer = TextBuffer::<SimpleText>::new("hello");
        let initial_version = *buffer.inner.version.read();

        let root = buffer.syntax();
        let edit = Edit::replace(root, "world".to_string());
        buffer.apply_edit(&edit).expect("Edit should succeed");

        let new_version = *buffer.inner.version.read();
        assert_eq!(new_version, initial_version + 1);
    }
}
