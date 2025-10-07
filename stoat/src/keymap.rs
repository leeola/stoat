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

use crate::{actions::*, Mode};
use gpui::{KeyBinding, Keymap};
use serde::Deserialize;

/// Embedded default keymap TOML configuration
const DEFAULT_KEYMAP_TOML: &str = include_str!("../../keymap.toml");

/// Keymap configuration loaded from TOML
#[derive(Debug, Deserialize)]
struct KeymapConfig {
    modes: Vec<ModeConfig>,
    bindings: Vec<BindingConfig>,
}

/// Mode configuration from TOML
#[derive(Debug, Deserialize)]
struct ModeConfig {
    name: String,
    display_name: String,
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
/// Maps action names from the TOML config to their corresponding action types
/// and constructs a KeyBinding with the specified keystroke and context.
fn create_keybinding(binding_config: &BindingConfig) -> Result<KeyBinding, String> {
    let key = binding_config.key.as_str();
    let context = Some(binding_config.context.as_str());

    // Check for parameterized SetMode action: SetMode(mode_name)
    if let Some(mode_name) = binding_config.action.strip_prefix("SetMode(") {
        if let Some(mode_name) = mode_name.strip_suffix(")") {
            return Ok(KeyBinding::new(
                key,
                SetMode(mode_name.to_string()),
                context,
            ));
        }
    }

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
        "OpenFileFinder" => Ok(KeyBinding::new(key, OpenFileFinder, context)),
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

        // File finder actions
        "FileFinderNext" => Ok(KeyBinding::new(key, FileFinderNext, context)),
        "FileFinderPrev" => Ok(KeyBinding::new(key, FileFinderPrev, context)),
        "FileFinderDismiss" => Ok(KeyBinding::new(key, FileFinderDismiss, context)),
        "FileFinderSelect" => Ok(KeyBinding::new(key, FileFinderSelect, context)),

        // Command palette actions
        "OpenCommandPalette" => Ok(KeyBinding::new(key, OpenCommandPalette, context)),
        "CommandPaletteNext" => Ok(KeyBinding::new(key, CommandPaletteNext, context)),
        "CommandPalettePrev" => Ok(KeyBinding::new(key, CommandPalettePrev, context)),
        "CommandPaletteDismiss" => Ok(KeyBinding::new(key, CommandPaletteDismiss, context)),
        "CommandPaletteExecute" => Ok(KeyBinding::new(key, CommandPaletteExecute, context)),

        _ => Err(format!("Unknown action: {}", binding_config.action)),
    }
}

/// Creates the default keymap for Stoat editor.
///
/// Loads key bindings from an embedded TOML configuration file. The keymap is compiled
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
/// The keymap is loaded from `keymap.toml`, which is embedded at compile time using
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
    // Parse the embedded TOML configuration
    let config: KeymapConfig =
        toml::from_str(DEFAULT_KEYMAP_TOML).expect("Failed to parse embedded keymap.toml");

    // Convert TOML bindings to GPUI KeyBindings
    let bindings: Vec<KeyBinding> = config
        .bindings
        .iter()
        .map(|binding_config| {
            create_keybinding(binding_config)
                .unwrap_or_else(|err| panic!("Invalid binding in keymap.toml: {}", err))
        })
        .collect();

    Keymap::new(bindings)
}

/// Loads the default modes for Stoat editor.
///
/// Parses mode definitions from the embedded TOML configuration file. Each mode has a
/// name (used in context predicates) and a display name (shown in the UI).
///
/// # Mode Definitions
///
/// Modes are defined in `keymap.toml`:
/// ```toml
/// [[modes]]
/// name = "normal"
/// display_name = "NORMAL"
/// ```
///
/// # Usage
///
/// This function is typically called once during editor initialization in [`crate::Stoat::new`]:
///
/// ```rust,ignore
/// modes: crate::keymap::load_default_modes(),
/// ```
pub fn load_default_modes() -> Vec<Mode> {
    let config: KeymapConfig =
        toml::from_str(DEFAULT_KEYMAP_TOML).expect("Failed to parse embedded keymap.toml");

    config
        .modes
        .into_iter()
        .map(|m| Mode::new(m.name, m.display_name))
        .collect()
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
        assert!(bindings[0].action().as_any().is::<SetMode>());
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
        assert!(bindings[0].action().as_any().is::<SetMode>());

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
