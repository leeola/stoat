use crate::context::EditorContext;
use gpui::Action;
use std::collections::HashMap;

// Define actions using the GPUI actions macro
gpui::actions!(stoat_test, [TestActionA, TestActionCmdS, TestActionEscape]);

/// A collection of key bindings
#[derive(Default)]
pub struct Keymap {
    bindings: Vec<KeyBinding>,
    /// Index by first keystroke for faster lookup
    binding_index: HashMap<String, Vec<usize>>,
}

/// A single key binding that maps keystrokes to an action
pub struct KeyBinding {
    pub keystrokes: Vec<String>,
    pub action: Box<dyn Action>,
    pub context: Option<String>,
}

impl Keymap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a key binding to the keymap
    pub fn add_binding(&mut self, binding: KeyBinding) {
        // Index by first keystroke for faster lookup
        if let Some(first_key) = binding.keystrokes.first() {
            self.binding_index
                .entry(first_key.clone())
                .or_default()
                .push(self.bindings.len());
        }

        self.bindings.push(binding);
    }

    /// Find a binding that matches the given keystrokes in the current context
    pub fn find_binding(
        &self,
        keystrokes: &[String],
        context: &EditorContext,
    ) -> Option<&KeyBinding> {
        if keystrokes.is_empty() {
            return None;
        }

        // Use index to get candidates with matching first keystroke
        let candidates = self.binding_index.get(&keystrokes[0])?;

        for &binding_idx in candidates {
            let binding = &self.bindings[binding_idx];

            // Check if keystrokes match
            if binding.keystrokes.len() != keystrokes.len() {
                continue;
            }

            if binding.keystrokes != keystrokes {
                continue;
            }

            // Check context if specified
            if let Some(ref context_predicate) = binding.context {
                if !context.matches(context_predicate) {
                    continue;
                }
            }

            return Some(binding);
        }

        None
    }

    /// Check if the given keystrokes could be the start of a valid binding
    pub fn has_partial_match(&self, keystrokes: &[String], context: &EditorContext) -> bool {
        if keystrokes.is_empty() {
            return false;
        }

        // Use index to get candidates with matching first keystroke
        let Some(candidates) = self.binding_index.get(&keystrokes[0]) else {
            return false;
        };

        for &binding_idx in candidates {
            let binding = &self.bindings[binding_idx];

            // Must have more keystrokes than current input
            if binding.keystrokes.len() <= keystrokes.len() {
                continue;
            }

            // Check if current keystrokes match the beginning of this binding
            if binding.keystrokes[..keystrokes.len()] != *keystrokes {
                continue;
            }

            // Check context if specified
            if let Some(ref context_predicate) = binding.context {
                if !context.matches(context_predicate) {
                    continue;
                }
            }

            return true;
        }

        false
    }

    /// Load bindings from a configuration
    pub fn load_default() -> Self {
        // For now, return empty keymap since we're using GPUI's action system directly
        // Key bindings are now registered at the app level

        Self::new()
    }

    /// Get all bindings
    pub fn bindings(&self) -> &[KeyBinding] {
        &self.bindings
    }
}

impl KeyBinding {
    /// Create a new key binding
    pub fn new(keystrokes: Vec<String>, action: Box<dyn Action>, context: Option<String>) -> Self {
        Self {
            keystrokes,
            action,
            context,
        }
    }

    /// Create a simple key binding without context
    pub fn simple(keystrokes: Vec<String>, action: Box<dyn Action>) -> Self {
        Self::new(keystrokes, action, None)
    }

    /// Create a key binding with context
    pub fn with_context(keystrokes: Vec<String>, action: Box<dyn Action>, context: String) -> Self {
        Self::new(keystrokes, action, Some(context))
    }
}

impl Clone for KeyBinding {
    fn clone(&self) -> Self {
        Self {
            keystrokes: self.keystrokes.clone(),
            action: self.action.boxed_clone(),
            context: self.context.clone(),
        }
    }
}
