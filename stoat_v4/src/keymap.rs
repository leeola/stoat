//! Minimal keymap configuration for Stoat v4.
//!
//! Provides default key bindings for the implemented v4 actions, using GPUI's
//! [`KeyBinding`] and context predicate system.

use crate::actions::*;
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

/// Create a [`KeyBinding`] from a binding configuration.
///
/// Maps action names from the TOML config to their corresponding action types
/// and constructs a [`KeyBinding`] with the specified keystroke and context.
fn create_keybinding(binding_config: &BindingConfig) -> Result<KeyBinding, String> {
    let key = binding_config.key.as_str();
    let context = Some(binding_config.context.as_str());

    // Handle parameterized SetMode action: SetMode(mode_name)
    if let Some(mode_name) = binding_config.action.strip_prefix("SetMode(") {
        if let Some(mode_name) = mode_name.strip_suffix(")") {
            return match mode_name {
                "insert" => Ok(KeyBinding::new(key, EnterInsertMode, context)),
                "normal" => Ok(KeyBinding::new(key, EnterNormalMode, context)),
                _ => Err(format!("Unsupported mode in SetMode: {}", mode_name)),
            };
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

        // Edit actions
        "DeleteLeft" => Ok(KeyBinding::new(key, DeleteLeft, context)),
        "DeleteRight" => Ok(KeyBinding::new(key, DeleteRight, context)),
        "NewLine" => Ok(KeyBinding::new(key, NewLine, context)),

        // Modal actions
        "EnterInsertMode" => Ok(KeyBinding::new(key, EnterInsertMode, context)),
        "EnterNormalMode" => Ok(KeyBinding::new(key, EnterNormalMode, context)),

        // File finder actions
        "OpenFileFinder" => Ok(KeyBinding::new(key, OpenFileFinder, context)),
        "FileFinderNext" => Ok(KeyBinding::new(key, FileFinderNext, context)),
        "FileFinderPrev" => Ok(KeyBinding::new(key, FileFinderPrev, context)),
        "FileFinderSelect" => Ok(KeyBinding::new(key, FileFinderSelect, context)),
        "FileFinderDismiss" => Ok(KeyBinding::new(key, FileFinderDismiss, context)),

        // Application actions
        "ExitApp" => Ok(KeyBinding::new(key, ExitApp, context)),

        _ => Err(format!("Unknown action: {}", binding_config.action)),
    }
}

/// Creates the default keymap for Stoat v4.
///
/// Loads key bindings from the main keymap TOML configuration file. Only bindings
/// for actions currently implemented in v4 are included. Unknown actions are
/// silently skipped.
///
/// # Key Bindings
///
/// ## Normal Mode
/// - `h/j/k/l` - Vim-style movement
/// - `i` - Enter insert mode
///
/// ## Insert Mode
/// - Arrow keys - Movement
/// - `escape` - Return to normal mode
/// - `backspace` - Delete character before cursor
///
/// # Usage
///
/// Called during editor initialization to register keybindings:
///
/// ```rust,ignore
/// let keymap = create_default_keymap();
/// cx.bind_keys(keymap.bindings());
/// ```
pub fn create_default_keymap() -> Keymap {
    // Parse the embedded TOML configuration
    let config: KeymapConfig =
        toml::from_str(DEFAULT_KEYMAP_TOML).expect("Failed to parse embedded keymap.toml");

    // Convert TOML bindings to GPUI KeyBindings, filtering out unknown actions
    let bindings: Vec<KeyBinding> = config
        .bindings
        .iter()
        .filter_map(|binding_config| create_keybinding(binding_config).ok())
        .collect();

    Keymap::new(bindings)
}
