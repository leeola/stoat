//! Default keymap configuration for Stoat editor.
//!
//! This module provides the default key bindings for Stoat, organized by mode (Normal, Insert,
//! Visual). The keymap uses GPUI's [`KeyBinding`] and context predicate system to ensure
//! bindings activate in the correct context.
//!
//! # Context Predicates
//!
//! Bindings use context predicates to control when they activate:
//! - `"Editor"` - Active in any editor
//! - `"Editor && mode == normal"` - Only in Normal mode
//! - `"Editor && mode == insert"` - Only in Insert mode
//! - `"Editor && mode == visual"` - Only in Visual mode
//!
//! # Key Binding Format
//!
//! Keys are specified in the format: `[modifiers-]key`
//! - Modifiers: `ctrl`, `alt`, `shift`, `cmd` (macOS), `super` (Linux), `win` (Windows)
//! - Examples: `"h"`, `"ctrl-c"`, `"cmd-s"`, `"escape"`

use crate::actions::*;
use gpui::{KeyBinding, Keymap};

/// Creates the default keymap for Stoat editor.
///
/// Returns a [`Keymap`] containing all default key bindings organized by mode. The keymap
/// includes vim-style bindings in Normal mode and standard text editing bindings in Insert mode.
///
/// # Key Binding Organization
///
/// ## Normal Mode
/// - **Movement**: `h/j/k/l` for character/line navigation, `w/b` for word movement
/// - **Mode transitions**: `i` for insert, `v` for visual, `escape` to return to normal
/// - **Editing**: `x` for delete, `d` commands for advanced deletion
/// - **File operations**: `:w` for save, `:q` for quit
///
/// ## Insert Mode
/// - **Navigation**: Arrow keys for movement
/// - **Mode transitions**: `escape` to return to normal mode
/// - **Text input**: Regular characters insert text (handled via [`InsertText`] action)
///
/// ## Visual Mode
/// - **Selection**: `h/j/k/l` and arrow keys extend selection
/// - **Mode transitions**: `escape` to return to normal mode
/// - **Operations**: `y` for copy, `d` for cut
///
/// # Usage
///
/// This function is typically called once during editor initialization:
///
/// ```rust,ignore
/// let keymap = Rc::new(RefCell::new(create_default_keymap()));
/// ```
pub fn create_default_keymap() -> Keymap {
    let bindings = vec![
        // ===== NORMAL MODE BINDINGS =====
        // Movement - vim-style hjkl
        KeyBinding::new("h", MoveLeft, Some("Editor && mode == normal")),
        KeyBinding::new("j", MoveDown, Some("Editor && mode == normal")),
        KeyBinding::new("k", MoveUp, Some("Editor && mode == normal")),
        KeyBinding::new("l", MoveRight, Some("Editor && mode == normal")),
        // Symbol selection
        KeyBinding::new("w", SelectNextSymbol, Some("Editor && mode == normal")),
        KeyBinding::new("b", SelectPrevSymbol, Some("Editor && mode == normal")),
        // Token selection
        KeyBinding::new("W", SelectNextToken, Some("Editor && mode == normal")),
        KeyBinding::new("B", SelectPrevToken, Some("Editor && mode == normal")),
        // Line start/end
        KeyBinding::new("0", MoveToLineStart, Some("Editor && mode == normal")),
        KeyBinding::new("$", MoveToLineEnd, Some("Editor && mode == normal")),
        // File start/end
        KeyBinding::new("g g", MoveToFileStart, Some("Editor && mode == normal")),
        KeyBinding::new("G", MoveToFileEnd, Some("Editor && mode == normal")),
        // Page up/down
        KeyBinding::new("ctrl-u", PageUp, Some("Editor && mode == normal")),
        KeyBinding::new("ctrl-d", PageDown, Some("Editor && mode == normal")),
        KeyBinding::new("ctrl-b", PageUp, Some("Editor && mode == normal")),
        KeyBinding::new("ctrl-f", PageDown, Some("Editor && mode == normal")),
        KeyBinding::new("pageup", PageUp, Some("Editor && mode == normal")),
        KeyBinding::new("pagedown", PageDown, Some("Editor && mode == normal")),
        // Mode transitions
        KeyBinding::new("i", EnterInsertMode, Some("Editor && mode == normal")),
        KeyBinding::new("v", EnterVisualMode, Some("Editor && mode == normal")),
        KeyBinding::new("escape", EnterNormalMode, Some("Editor && mode == insert")),
        KeyBinding::new("escape", EnterNormalMode, Some("Editor && mode == visual")),
        KeyBinding::new("escape", ExitApp, Some("Editor && mode == normal")),
        // Editing in normal mode
        KeyBinding::new("x", DeleteRight, Some("Editor && mode == normal")),
        KeyBinding::new("d d", DeleteLine, Some("Editor && mode == normal")),
        KeyBinding::new("D", DeleteToEndOfLine, Some("Editor && mode == normal")),
        // Undo/redo
        KeyBinding::new("u", Undo, Some("Editor && mode == normal")),
        KeyBinding::new("ctrl-r", Redo, Some("Editor && mode == normal")),
        // File operations (available in normal mode)
        KeyBinding::new("cmd-s", Save, Some("Editor && mode == normal")),
        KeyBinding::new("ctrl-s", Save, Some("Editor && mode == normal")),
        // ===== INSERT MODE BINDINGS =====
        // Navigation with arrow keys
        KeyBinding::new("left", MoveLeft, Some("Editor && mode == insert")),
        KeyBinding::new("right", MoveRight, Some("Editor && mode == insert")),
        KeyBinding::new("up", MoveUp, Some("Editor && mode == insert")),
        KeyBinding::new("down", MoveDown, Some("Editor && mode == insert")),
        // Word movement with modifiers
        KeyBinding::new("alt-left", MoveWordLeft, Some("Editor && mode == insert")),
        KeyBinding::new("alt-right", MoveWordRight, Some("Editor && mode == insert")),
        KeyBinding::new(
            "cmd-left",
            MoveToLineStart,
            Some("Editor && mode == insert"),
        ),
        KeyBinding::new("cmd-right", MoveToLineEnd, Some("Editor && mode == insert")),
        KeyBinding::new("ctrl-a", MoveToLineStart, Some("Editor && mode == insert")),
        KeyBinding::new("ctrl-e", MoveToLineEnd, Some("Editor && mode == insert")),
        // Deletion in insert mode
        KeyBinding::new("backspace", DeleteLeft, Some("Editor && mode == insert")),
        KeyBinding::new("delete", DeleteRight, Some("Editor && mode == insert")),
        KeyBinding::new(
            "alt-backspace",
            DeleteWordLeft,
            Some("Editor && mode == insert"),
        ),
        KeyBinding::new(
            "alt-delete",
            DeleteWordRight,
            Some("Editor && mode == insert"),
        ),
        // Newline
        KeyBinding::new("enter", NewLine, Some("Editor && mode == insert")),
        // File operations (available in insert mode too)
        KeyBinding::new("cmd-s", Save, Some("Editor && mode == insert")),
        KeyBinding::new("ctrl-s", Save, Some("Editor && mode == insert")),
        // Undo/redo (available in insert mode)
        KeyBinding::new("cmd-z", Undo, Some("Editor && mode == insert")),
        KeyBinding::new("ctrl-z", Undo, Some("Editor && mode == insert")),
        KeyBinding::new("cmd-shift-z", Redo, Some("Editor && mode == insert")),
        KeyBinding::new("ctrl-shift-z", Redo, Some("Editor && mode == insert")),
        // Clipboard operations
        KeyBinding::new("cmd-c", Copy, Some("Editor")),
        KeyBinding::new("ctrl-c", Copy, Some("Editor")),
        KeyBinding::new("cmd-x", Cut, Some("Editor")),
        KeyBinding::new("ctrl-x", Cut, Some("Editor")),
        KeyBinding::new("cmd-v", Paste, Some("Editor")),
        KeyBinding::new("ctrl-v", Paste, Some("Editor")),
        // ===== VISUAL MODE BINDINGS =====
        // Movement extends selection
        KeyBinding::new("h", SelectLeft, Some("Editor && mode == visual")),
        KeyBinding::new("j", SelectDown, Some("Editor && mode == visual")),
        KeyBinding::new("k", SelectUp, Some("Editor && mode == visual")),
        KeyBinding::new("l", SelectRight, Some("Editor && mode == visual")),
        KeyBinding::new("left", SelectLeft, Some("Editor && mode == visual")),
        KeyBinding::new("right", SelectRight, Some("Editor && mode == visual")),
        KeyBinding::new("up", SelectUp, Some("Editor && mode == visual")),
        KeyBinding::new("down", SelectDown, Some("Editor && mode == visual")),
        // Symbol selection
        KeyBinding::new("w", SelectNextSymbol, Some("Editor && mode == visual")),
        KeyBinding::new("b", SelectPrevSymbol, Some("Editor && mode == visual")),
        // Token selection
        KeyBinding::new("W", SelectNextToken, Some("Editor && mode == visual")),
        KeyBinding::new("B", SelectPrevToken, Some("Editor && mode == visual")),
        // Line start/end extends selection
        KeyBinding::new("0", SelectToLineStart, Some("Editor && mode == visual")),
        KeyBinding::new("$", SelectToLineEnd, Some("Editor && mode == visual")),
        // Operations on selection
        KeyBinding::new("y", Copy, Some("Editor && mode == visual")),
        KeyBinding::new("d", Cut, Some("Editor && mode == visual")),
        KeyBinding::new("x", Cut, Some("Editor && mode == visual")),
        // Select all (available in all modes)
        KeyBinding::new("cmd-a", SelectAll, Some("Editor")),
        KeyBinding::new("ctrl-a", SelectAll, Some("Editor && mode != insert")), /* Avoid conflict with ctrl-a in insert */
        // ===== GLOBAL BINDINGS =====
        // Quit
        KeyBinding::new("cmd-q", Quit, Some("Editor")),
        KeyBinding::new("ctrl-q", Quit, Some("Editor")),
        // Open
        KeyBinding::new("cmd-o", Open, Some("Editor")),
        KeyBinding::new("ctrl-o", Open, Some("Editor")),
        // Indentation
        KeyBinding::new("tab", Indent, Some("Editor")),
        KeyBinding::new("shift-tab", Outdent, Some("Editor")),
    ];

    Keymap::new(bindings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyContext, Keystroke, TestAppContext};

    #[gpui::test]
    fn test_keymap_creation(_cx: &mut TestAppContext) {
        let keymap = create_default_keymap();
        assert!(keymap.bindings().len() > 0);
    }

    #[gpui::test]
    fn test_normal_mode_bindings(_cx: &mut TestAppContext) {
        let keymap = create_default_keymap();

        let contexts = vec![
            KeyContext::parse("Workspace").unwrap(),
            KeyContext::parse("Editor mode=normal").unwrap(),
        ];

        // Test 'h' key moves left in normal mode
        let (bindings, _pending) =
            keymap.bindings_for_input(&[Keystroke::parse("h").unwrap()], &contexts);

        assert!(
            !bindings.is_empty(),
            "Expected binding for 'h' in normal mode"
        );
        assert!(bindings[0].action().as_any().is::<MoveLeft>());

        // Test 'i' enters insert mode
        let (bindings, _pending) =
            keymap.bindings_for_input(&[Keystroke::parse("i").unwrap()], &contexts);

        assert!(
            !bindings.is_empty(),
            "Expected binding for 'i' in normal mode"
        );
        assert!(bindings[0].action().as_any().is::<EnterInsertMode>());
    }

    #[gpui::test]
    fn test_insert_mode_bindings(_cx: &mut TestAppContext) {
        let keymap = create_default_keymap();

        let contexts = vec![
            KeyContext::parse("Workspace").unwrap(),
            KeyContext::parse("Editor mode=insert").unwrap(),
        ];

        // Test escape returns to normal mode
        let (bindings, _pending) =
            keymap.bindings_for_input(&[Keystroke::parse("escape").unwrap()], &contexts);

        assert!(
            !bindings.is_empty(),
            "Expected binding for 'escape' in insert mode"
        );
        assert!(bindings[0].action().as_any().is::<EnterNormalMode>());

        // Test arrow keys work in insert mode
        let (bindings, _pending) =
            keymap.bindings_for_input(&[Keystroke::parse("left").unwrap()], &contexts);

        assert!(
            !bindings.is_empty(),
            "Expected binding for 'left' in insert mode"
        );
        assert!(bindings[0].action().as_any().is::<MoveLeft>());
    }

    #[gpui::test]
    fn test_multi_keystroke_sequence(_cx: &mut TestAppContext) {
        let keymap = create_default_keymap();

        let contexts = vec![
            KeyContext::parse("Workspace").unwrap(),
            KeyContext::parse("Editor mode=normal").unwrap(),
        ];

        // Test 'g g' goes to file start
        let (bindings, pending) =
            keymap.bindings_for_input(&[Keystroke::parse("g").unwrap()], &contexts);

        assert!(bindings.is_empty(), "First 'g' should not match anything");
        assert!(
            pending,
            "First 'g' should be pending for multi-key sequence"
        );

        let (bindings, pending) = keymap.bindings_for_input(
            &[
                Keystroke::parse("g").unwrap(),
                Keystroke::parse("g").unwrap(),
            ],
            &contexts,
        );

        assert!(
            !bindings.is_empty(),
            "Expected binding for 'g g' in normal mode"
        );
        assert!(!pending, "'g g' should be complete match");
        assert!(bindings[0].action().as_any().is::<MoveToFileStart>());
    }
}
