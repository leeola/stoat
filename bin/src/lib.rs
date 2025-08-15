pub mod commands;

#[cfg(test)]
mod tests {
    use stoat::EditorEngine;

    #[test]
    fn test_editor_engine_initialization() {
        let engine = EditorEngine::new();

        // Basic initialization test
        assert_eq!(engine.line_count(), 0); // Empty editor has no lines
        assert!(engine.text().is_empty());
        assert!(!engine.is_dirty());
    }

    #[test]
    fn test_editor_engine_with_text() {
        let text = "Hello, World!";
        let engine = EditorEngine::with_text(text);

        assert_eq!(engine.text(), text);
        assert_eq!(engine.line_count(), 1);
        assert!(!engine.is_dirty()); // Initial text doesn't mark as dirty
    }
}
