//! Minimal keymap configuration for Stoat v4.
//!
//! Provides default key bindings for the implemented v4 actions, using GPUI's
//! [`KeyBinding`] and context predicate system.

use crate::actions::*;
use gpui::{KeyBinding, Keymap};
use serde::Deserialize;

/// Embedded default keymap TOML configuration
const DEFAULT_KEYMAP_TOML: &str = include_str!("../keymap.toml");

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

    match binding_config.action.as_str() {
        // Movement actions
        "MoveLeft" => Ok(KeyBinding::new(key, MoveLeft, context)),
        "MoveRight" => Ok(KeyBinding::new(key, MoveRight, context)),
        "MoveUp" => Ok(KeyBinding::new(key, MoveUp, context)),
        "MoveDown" => Ok(KeyBinding::new(key, MoveDown, context)),

        // Edit actions
        "DeleteLeft" => Ok(KeyBinding::new(key, DeleteLeft, context)),

        // Modal actions
        "EnterInsertMode" => Ok(KeyBinding::new(key, EnterInsertMode, context)),
        "EnterNormalMode" => Ok(KeyBinding::new(key, EnterNormalMode, context)),

        _ => Err(format!("Unknown action: {}", binding_config.action)),
    }
}

/// Creates the default keymap for Stoat v4.
///
/// Loads minimal key bindings from an embedded TOML configuration file. The keymap
/// contains only the bindings for actions currently implemented in v4.
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
