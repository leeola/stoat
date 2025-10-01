//! Headless key dispatch system using GPUI's public Keymap API.
//!
//! This module provides a headless key dispatch system that uses GPUI's [`Keymap`] type
//! directly to match keystrokes against bindings without requiring a GUI or rendering.
//!
//! # Architecture
//!
//! Unlike GUI applications where key dispatch is handled by the rendering system, this module
//! provides a simple wrapper around [`Keymap::bindings_for_input`] that manages pending
//! keystrokes and context state. The dispatch logic is straightforward:
//!
//! 1. Maintain a context stack that reflects the current editor state
//! 2. Accumulate keystrokes for multi-key sequences
//! 3. Use [`Keymap::bindings_for_input`] to match against bindings
//! 4. Return matched actions for execution
//!
//! # Usage
//!
//! ```rust,ignore
//! let dispatch = HeadlessDispatch::new(keymap);
//!
//! // Dispatch a keystroke
//! let keystroke = Keystroke::parse("h").unwrap();
//! let result = dispatch.dispatch_keystroke(keystroke);
//!
//! // Handle matched actions
//! for binding in result.bindings {
//!     execute_action(binding.action());
//! }
//! ```

use crate::EditorMode;
use gpui::{KeyBinding, KeyContext, Keymap, Keystroke};
use smallvec::SmallVec;
use std::{cell::RefCell, rc::Rc};

/// Result of dispatching a keystroke.
///
/// This struct contains the results of matching a keystroke against key bindings, including
/// any matched actions and whether there are pending multi-key sequences.
pub struct DispatchResult {
    /// Bindings that matched the input keystroke(s)
    pub bindings: SmallVec<[KeyBinding; 1]>,
    /// Whether there are keystrokes pending for multi-key sequences
    pub pending: bool,
}

/// Headless key dispatch system using GPUI's Keymap.
///
/// This struct manages keystroke dispatching without a GUI by maintaining a context stack
/// and pending keystroke buffer. It delegates to [`Keymap::bindings_for_input`] for matching.
///
/// # Context Management
///
/// The context stack reflects the current editor state:
/// - Root: "Workspace" context
/// - Editor: "Editor" context with mode (e.g., "mode=normal")
///
/// As the editor mode changes, the context is updated via [`update_context`].
pub struct HeadlessDispatch {
    /// The keymap containing key bindings
    keymap: Rc<RefCell<Keymap>>,
    /// Pending keystrokes for multi-key sequences
    pending_keystrokes: SmallVec<[Keystroke; 1]>,
    /// Current context stack (Workspace -> Editor with mode)
    context_stack: Vec<KeyContext>,
}

impl HeadlessDispatch {
    /// Create a new headless dispatch system.
    ///
    /// Initializes the dispatch system with the given keymap and sets up the initial context
    /// stack with Workspace and Editor (normal mode) contexts.
    ///
    /// # Arguments
    ///
    /// * `keymap` - The keymap containing key bindings
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let keymap = Rc::new(RefCell::new(create_default_keymap()));
    /// let dispatch = HeadlessDispatch::new(keymap);
    /// ```
    pub fn new(keymap: Rc<RefCell<Keymap>>) -> Self {
        // Initialize context stack with Workspace and Editor contexts
        let mut context_stack = Vec::new();
        context_stack.push(KeyContext::parse("Workspace").unwrap());

        let mut editor_context = KeyContext::parse("Editor").unwrap();
        editor_context.set("mode", "normal"); // Default to normal mode
        context_stack.push(editor_context);

        Self {
            keymap,
            pending_keystrokes: SmallVec::new(),
            context_stack,
        }
    }

    /// Dispatch a keystroke.
    ///
    /// This method accumulates keystrokes for multi-key sequences and uses
    /// [`Keymap::bindings_for_input`] to match against bindings. It returns matched bindings
    /// that should be executed by the caller.
    ///
    /// # Arguments
    ///
    /// * `keystroke` - The keystroke to dispatch
    ///
    /// # Returns
    ///
    /// A [`DispatchResult`] containing:
    /// - `bindings`: Actions that matched and should be executed
    /// - `pending`: Whether there are keystrokes pending for multi-key sequences
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = dispatch.dispatch_keystroke(Keystroke::parse("h").unwrap());
    /// if let Some(binding) = result.bindings.first() {
    ///     execute_action(binding.action());
    /// }
    /// ```
    pub fn dispatch_keystroke(&mut self, keystroke: Keystroke) -> DispatchResult {
        // Add the new keystroke to pending buffer
        self.pending_keystrokes.push(keystroke);

        // Try to match against bindings
        let (bindings, pending) = self
            .keymap
            .borrow()
            .bindings_for_input(&self.pending_keystrokes, &self.context_stack);

        // If we got a match or there's no pending sequence, clear the buffer
        if !bindings.is_empty() || !pending {
            self.pending_keystrokes.clear();
        }

        DispatchResult { bindings, pending }
    }

    /// Update the editor context with the current mode.
    ///
    /// This method updates the key context to include the current mode. The mode is used by
    /// key binding predicates (e.g., `"Editor && mode == normal"`) to determine which bindings
    /// are active.
    ///
    /// # Arguments
    ///
    /// * `mode` - The current editor mode
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// dispatch.update_context(EditorMode::Insert);
    /// // Now bindings with "mode == insert" will match
    /// ```
    pub fn update_context(&mut self, mode: EditorMode) {
        // Update the editor context (second element in stack) with the new mode
        if self.context_stack.len() >= 2 {
            let mut editor_context = KeyContext::parse("Editor").unwrap();
            editor_context.set("mode", mode.as_str());
            self.context_stack[1] = editor_context;
        }

        // Clear pending keystrokes when mode changes (vim-style behavior)
        self.clear_pending();
    }

    /// Clear any pending keystrokes.
    ///
    /// This is useful when mode changes occur or when you want to reset the keystroke buffer.
    pub fn clear_pending(&mut self) {
        self.pending_keystrokes.clear();
    }

    /// Check if there are pending keystrokes waiting for completion.
    ///
    /// Returns `true` if there are keystrokes pending in a multi-key sequence.
    pub fn has_pending(&self) -> bool {
        !self.pending_keystrokes.is_empty()
    }

    /// Get the pending keystrokes.
    ///
    /// Returns a slice of keystrokes that are pending for multi-key sequences.
    pub fn pending_keystrokes(&self) -> &[Keystroke] {
        &self.pending_keystrokes
    }

    /// Get the current context stack.
    ///
    /// Returns the current context stack for debugging or inspection.
    pub fn context_stack(&self) -> &[KeyContext] {
        &self.context_stack
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{actions::*, keymap::create_default_keymap};
    use gpui::TestAppContext;

    #[gpui::test]
    fn test_dispatch_creation(_cx: &mut TestAppContext) {
        let keymap = Rc::new(RefCell::new(create_default_keymap()));
        let _dispatch = HeadlessDispatch::new(keymap);
    }

    #[gpui::test]
    fn test_simple_keystroke_dispatch(_cx: &mut TestAppContext) {
        let keymap = Rc::new(RefCell::new(create_default_keymap()));
        let mut dispatch = HeadlessDispatch::new(keymap);

        // Dispatch 'h' in normal mode (should move left)
        let keystroke = Keystroke::parse("h").unwrap();
        let result = dispatch.dispatch_keystroke(keystroke);

        assert!(!result.bindings.is_empty(), "Expected binding for 'h'");
        assert!(
            result.bindings[0].action().as_any().is::<MoveLeft>(),
            "Expected MoveLeft action"
        );
        assert!(!result.pending, "Should have no pending keystrokes");
    }

    #[gpui::test]
    fn test_multi_key_sequence(_cx: &mut TestAppContext) {
        let keymap = Rc::new(RefCell::new(create_default_keymap()));
        let mut dispatch = HeadlessDispatch::new(keymap);

        // First 'g' should be pending
        let keystroke = Keystroke::parse("g").unwrap();
        let result = dispatch.dispatch_keystroke(keystroke.clone());

        assert!(result.bindings.is_empty(), "First 'g' should not match");
        assert!(result.pending, "Should have pending keystroke");

        // Second 'g' should match 'g g' -> MoveToFileStart
        let result = dispatch.dispatch_keystroke(keystroke);

        assert!(!result.bindings.is_empty(), "Expected binding for 'g g'");
        assert!(
            result.bindings[0].action().as_any().is::<MoveToFileStart>(),
            "Expected MoveToFileStart action"
        );
        assert!(!result.pending, "Should clear pending after match");
    }

    #[gpui::test]
    fn test_context_update(_cx: &mut TestAppContext) {
        let keymap = Rc::new(RefCell::new(create_default_keymap()));
        let mut dispatch = HeadlessDispatch::new(keymap);

        // In normal mode, 'i' should enter insert mode
        let keystroke = Keystroke::parse("i").unwrap();
        let result = dispatch.dispatch_keystroke(keystroke);

        assert!(!result.bindings.is_empty());
        assert!(result.bindings[0].action().as_any().is::<EnterInsertMode>());

        // Update context to insert mode
        dispatch.update_context(EditorMode::Insert);

        // Now 'i' should have no binding (would trigger InsertText in real usage)
        let keystroke = Keystroke::parse("i").unwrap();
        let result = dispatch.dispatch_keystroke(keystroke);

        // In insert mode, 'i' has no binding
        assert!(
            result.bindings.is_empty(),
            "No binding for 'i' in insert mode"
        );
    }

    #[gpui::test]
    fn test_clear_pending(_cx: &mut TestAppContext) {
        let keymap = Rc::new(RefCell::new(create_default_keymap()));
        let mut dispatch = HeadlessDispatch::new(keymap);

        // Create pending keystroke
        let keystroke = Keystroke::parse("g").unwrap();
        let result = dispatch.dispatch_keystroke(keystroke);

        assert!(result.pending);
        assert!(dispatch.has_pending());

        // Clear pending
        dispatch.clear_pending();

        assert!(!dispatch.has_pending());
        assert_eq!(dispatch.pending_keystrokes().len(), 0);
    }

    #[gpui::test]
    fn test_mode_change_clears_pending(_cx: &mut TestAppContext) {
        let keymap = Rc::new(RefCell::new(create_default_keymap()));
        let mut dispatch = HeadlessDispatch::new(keymap);

        // Create pending keystroke
        let keystroke = Keystroke::parse("g").unwrap();
        let _result = dispatch.dispatch_keystroke(keystroke);

        assert!(dispatch.has_pending());

        // Mode change should clear pending
        dispatch.update_context(EditorMode::Insert);

        assert!(!dispatch.has_pending());
    }
}
