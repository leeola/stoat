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
use serde::Deserialize;

/// Embedded default keymap JSON configuration
const DEFAULT_KEYMAP_JSON: &str = include_str!("../../keymap.json");

/// Keymap configuration loaded from JSON
#[derive(Debug, Deserialize)]
struct KeymapConfig {
    bindings: Vec<BindingConfig>,
}

/// Individual key binding configuration
#[derive(Debug, Deserialize)]
struct BindingConfig {
    key: String,
    action: String,
    context: String,
}

/// Create a KeyBinding from a binding configuration.
///
/// Maps action names from the JSON config to their corresponding action types
/// and constructs a KeyBinding with the specified keystroke and context.
fn create_keybinding(binding_config: &BindingConfig) -> Result<KeyBinding, String> {
    let key = binding_config.key.as_str();
    let context = Some(binding_config.context.as_str());

    match binding_config.action.as_str() {
        // Movement actions
        "MoveLeft" => Ok(KeyBinding::new(key, MoveLeft, context)),
        "MoveRight" => Ok(KeyBinding::new(key, MoveRight, context)),
        "MoveUp" => Ok(KeyBinding::new(key, MoveUp, context)),
        "MoveDown" => Ok(KeyBinding::new(key, MoveDown, context)),
        "MoveToLineStart" => Ok(KeyBinding::new(key, MoveToLineStart, context)),
        "MoveToLineEnd" => Ok(KeyBinding::new(key, MoveToLineEnd, context)),
        "MoveToFileStart" => Ok(KeyBinding::new(key, MoveToFileStart, context)),
        "MoveToFileEnd" => Ok(KeyBinding::new(key, MoveToFileEnd, context)),
        "MoveWordLeft" => Ok(KeyBinding::new(key, MoveWordLeft, context)),
        "MoveWordRight" => Ok(KeyBinding::new(key, MoveWordRight, context)),
        "PageUp" => Ok(KeyBinding::new(key, PageUp, context)),
        "PageDown" => Ok(KeyBinding::new(key, PageDown, context)),

        // Selection actions
        "SelectNextSymbol" => Ok(KeyBinding::new(key, SelectNextSymbol, context)),
        "SelectPrevSymbol" => Ok(KeyBinding::new(key, SelectPrevSymbol, context)),
        "SelectNextToken" => Ok(KeyBinding::new(key, SelectNextToken, context)),
        "SelectPrevToken" => Ok(KeyBinding::new(key, SelectPrevToken, context)),
        "SelectLeft" => Ok(KeyBinding::new(key, SelectLeft, context)),
        "SelectRight" => Ok(KeyBinding::new(key, SelectRight, context)),
        "SelectUp" => Ok(KeyBinding::new(key, SelectUp, context)),
        "SelectDown" => Ok(KeyBinding::new(key, SelectDown, context)),
        "SelectToLineStart" => Ok(KeyBinding::new(key, SelectToLineStart, context)),
        "SelectToLineEnd" => Ok(KeyBinding::new(key, SelectToLineEnd, context)),
        "SelectAll" => Ok(KeyBinding::new(key, SelectAll, context)),

        // Edit actions
        "DeleteLeft" => Ok(KeyBinding::new(key, DeleteLeft, context)),
        "DeleteRight" => Ok(KeyBinding::new(key, DeleteRight, context)),
        "DeleteLine" => Ok(KeyBinding::new(key, DeleteLine, context)),
        "DeleteToEndOfLine" => Ok(KeyBinding::new(key, DeleteToEndOfLine, context)),
        "DeleteWordLeft" => Ok(KeyBinding::new(key, DeleteWordLeft, context)),
        "DeleteWordRight" => Ok(KeyBinding::new(key, DeleteWordRight, context)),
        "NewLine" => Ok(KeyBinding::new(key, NewLine, context)),
        "Indent" => Ok(KeyBinding::new(key, Indent, context)),
        "Outdent" => Ok(KeyBinding::new(key, Outdent, context)),

        // Modal actions
        "EnterInsertMode" => Ok(KeyBinding::new(key, EnterInsertMode, context)),
        "EnterNormalMode" => Ok(KeyBinding::new(key, EnterNormalMode, context)),
        "EnterVisualMode" => Ok(KeyBinding::new(key, EnterVisualMode, context)),
        "EnterPaneMode" => Ok(KeyBinding::new(key, EnterPaneMode, context)),

        // Clipboard actions
        "Copy" => Ok(KeyBinding::new(key, Copy, context)),
        "Cut" => Ok(KeyBinding::new(key, Cut, context)),
        "Paste" => Ok(KeyBinding::new(key, Paste, context)),

        // File actions
        "Save" => Ok(KeyBinding::new(key, Save, context)),
        "Open" => Ok(KeyBinding::new(key, Open, context)),
        "Quit" => Ok(KeyBinding::new(key, Quit, context)),
        "ExitApp" => Ok(KeyBinding::new(key, ExitApp, context)),

        // Undo/redo
        "Undo" => Ok(KeyBinding::new(key, Undo, context)),
        "Redo" => Ok(KeyBinding::new(key, Redo, context)),

        // Pane actions
        "SplitUp" => Ok(KeyBinding::new(key, SplitUp, context)),
        "SplitDown" => Ok(KeyBinding::new(key, SplitDown, context)),
        "SplitLeft" => Ok(KeyBinding::new(key, SplitLeft, context)),
        "SplitRight" => Ok(KeyBinding::new(key, SplitRight, context)),
        "ClosePane" => Ok(KeyBinding::new(key, ClosePane, context)),
        "FocusPaneLeft" => Ok(KeyBinding::new(key, FocusPaneLeft, context)),
        "FocusPaneRight" => Ok(KeyBinding::new(key, FocusPaneRight, context)),
        "FocusPaneUp" => Ok(KeyBinding::new(key, FocusPaneUp, context)),
        "FocusPaneDown" => Ok(KeyBinding::new(key, FocusPaneDown, context)),

        _ => Err(format!("Unknown action: {}", binding_config.action)),
    }
}

/// Creates the default keymap for Stoat editor.
///
/// Loads key bindings from an embedded JSON configuration file. The keymap is compiled
/// into the binary and contains all default bindings organized by mode (Normal, Insert,
/// Visual, Pane).
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
/// # Configuration
///
/// The keymap is loaded from `keymap.json`, which is embedded at compile time using
/// `include_str!`. This ensures the binary is portable and requires no external files.
///
/// # Usage
///
/// This function is typically called once during editor initialization:
///
/// ```rust,ignore
/// let keymap = Rc::new(RefCell::new(create_default_keymap()));
/// ```
pub fn create_default_keymap() -> Keymap {
    // Parse the embedded JSON configuration
    let config: KeymapConfig =
        serde_json::from_str(DEFAULT_KEYMAP_JSON).expect("Failed to parse embedded keymap.json");

    // Convert JSON bindings to GPUI KeyBindings
    let bindings: Vec<KeyBinding> = config
        .bindings
        .iter()
        .map(|binding_config| {
            create_keybinding(binding_config)
                .unwrap_or_else(|err| panic!("Invalid binding in keymap.json: {}", err))
        })
        .collect();

    Keymap::new(bindings)
}

// Old hardcoded implementation preserved below for reference (can be deleted later)
#[allow(dead_code)]
fn create_default_keymap_old() -> Keymap {
    let bindings_old = vec![
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
        KeyBinding::new("ctrl-w", EnterPaneMode, Some("Editor && mode == normal")),
        KeyBinding::new("escape", EnterNormalMode, Some("Editor && mode == insert")),
        KeyBinding::new("escape", EnterNormalMode, Some("Editor && mode == visual")),
        KeyBinding::new("escape", EnterNormalMode, Some("Editor && mode == pane")),
        KeyBinding::new("escape", ExitApp, Some("Editor && mode == normal")),
        // Editing in normal mode
        KeyBinding::new("x", DeleteRight, Some("Editor && mode == normal")),
        KeyBinding::new("shift-x", DeleteLeft, Some("Editor && mode == normal")),
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
        // ===== PANE MODE BINDINGS =====
        // Split panes - simple keys in pane mode
        KeyBinding::new("v", SplitRight, Some("Editor && mode == pane")),
        KeyBinding::new("s", SplitDown, Some("Editor && mode == pane")),
        KeyBinding::new("ctrl-v", SplitRight, Some("Editor && mode == pane")),
        KeyBinding::new("ctrl-s", SplitDown, Some("Editor && mode == pane")),
        // Close pane
        KeyBinding::new("q", ClosePane, Some("Editor && mode == pane")),
        // Navigate panes - hjkl in pane mode
        KeyBinding::new("h", FocusPaneLeft, Some("Editor && mode == pane")),
        KeyBinding::new("j", FocusPaneDown, Some("Editor && mode == pane")),
        KeyBinding::new("k", FocusPaneUp, Some("Editor && mode == pane")),
        KeyBinding::new("l", FocusPaneRight, Some("Editor && mode == pane")),
        KeyBinding::new("ctrl-h", FocusPaneLeft, Some("Editor && mode == pane")),
        KeyBinding::new("ctrl-j", FocusPaneDown, Some("Editor && mode == pane")),
        KeyBinding::new("ctrl-k", FocusPaneUp, Some("Editor && mode == pane")),
        KeyBinding::new("ctrl-l", FocusPaneRight, Some("Editor && mode == pane")),
    ];

    Keymap::new(bindings_old)
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
