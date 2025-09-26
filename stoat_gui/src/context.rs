/// Represents the current context of the editor for conditional key bindings
#[derive(Debug, Clone)]
pub struct EditorContext {
    /// Current editing mode
    pub mode: EditorMode,
    /// Whether there is an active text selection
    pub has_selection: bool,
    /// Whether the buffer has been modified
    pub buffer_modified: bool,
    /// Whether the editor is focused
    pub focused: bool,
    /// Custom context flags that can be set by various components
    pub flags: std::collections::HashSet<String>,
}

/// Different modes of the editor
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorMode {
    /// Normal mode - for navigation and commands
    Normal,
    /// Insert mode - for text input
    Insert,
    /// Visual mode - for selections
    Visual,
    /// Command mode - for entering commands
    Command,
}

impl EditorContext {
    /// Create a new editor context with default values
    pub fn new() -> Self {
        Self {
            mode: EditorMode::Normal,
            has_selection: false,
            buffer_modified: false,
            focused: true,
            flags: std::collections::HashSet::new(),
        }
    }

    /// Check if a context predicate matches the current context
    pub fn matches(&self, predicate: &str) -> bool {
        // Simple predicate parser for common cases
        // Supports: "mode == normal", "has_selection", "!buffer_modified", etc.

        let predicate = predicate.trim();

        // Handle negation
        if let Some(inner) = predicate.strip_prefix('!') {
            return !self.matches(inner.trim());
        }

        // Handle equality checks
        if let Some((left, right)) = predicate.split_once("==") {
            let left = left.trim();
            let right = right.trim().trim_matches('"').trim_matches('\'');

            return match left {
                "mode" => self.mode_matches(right),
                _ => false,
            };
        }

        // Handle simple flags
        match predicate {
            "has_selection" => self.has_selection,
            "buffer_modified" => self.buffer_modified,
            "focused" => self.focused,
            "normal" => self.mode == EditorMode::Normal,
            "insert" => self.mode == EditorMode::Insert,
            "visual" => self.mode == EditorMode::Visual,
            "command" => self.mode == EditorMode::Command,
            flag => self.flags.contains(flag),
        }
    }

    /// Check if the mode matches a string representation
    fn mode_matches(&self, mode_str: &str) -> bool {
        match mode_str {
            "normal" => self.mode == EditorMode::Normal,
            "insert" => self.mode == EditorMode::Insert,
            "visual" => self.mode == EditorMode::Visual,
            "command" => self.mode == EditorMode::Command,
            _ => false,
        }
    }

    /// Set the editor mode
    pub fn set_mode(&mut self, mode: EditorMode) {
        self.mode = mode;
    }

    /// Set whether there is a selection
    pub fn set_has_selection(&mut self, has_selection: bool) {
        self.has_selection = has_selection;
    }

    /// Set whether the buffer is modified
    pub fn set_buffer_modified(&mut self, modified: bool) {
        self.buffer_modified = modified;
    }

    /// Set whether the editor is focused
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Add a custom context flag
    pub fn add_flag(&mut self, flag: String) {
        self.flags.insert(flag);
    }

    /// Remove a custom context flag
    pub fn remove_flag(&mut self, flag: &str) {
        self.flags.remove(flag);
    }

    /// Check if a custom flag is set
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }

    /// Get the current mode
    pub fn mode(&self) -> &EditorMode {
        &self.mode
    }

    /// Get a string representation of the current mode
    pub fn mode_string(&self) -> &'static str {
        match self.mode {
            EditorMode::Normal => "normal",
            EditorMode::Insert => "insert",
            EditorMode::Visual => "visual",
            EditorMode::Command => "command",
        }
    }
}

impl Default for EditorContext {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EditorMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditorMode::Normal => write!(f, "normal"),
            EditorMode::Insert => write!(f, "insert"),
            EditorMode::Visual => write!(f, "visual"),
            EditorMode::Command => write!(f, "command"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_matching() {
        let mut context = EditorContext::new();

        // Test mode matching
        assert!(context.matches("mode == normal"));
        assert!(context.matches("normal"));
        assert!(!context.matches("insert"));

        // Test negation
        assert!(!context.matches("!normal"));
        assert!(context.matches("!insert"));

        // Test flags
        assert!(!context.matches("has_selection"));
        context.set_has_selection(true);
        assert!(context.matches("has_selection"));

        // Test custom flags
        assert!(!context.matches("custom_flag"));
        context.add_flag("custom_flag".to_string());
        assert!(context.matches("custom_flag"));
    }

    #[test]
    fn test_mode_changes() {
        let mut context = EditorContext::new();

        assert_eq!(context.mode(), &EditorMode::Normal);

        context.set_mode(EditorMode::Insert);
        assert_eq!(context.mode(), &EditorMode::Insert);
        assert!(context.matches("insert"));
        assert!(!context.matches("normal"));
    }
}
