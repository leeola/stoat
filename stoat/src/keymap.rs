//! Minimal keymap configuration for Stoat v4.
//!
//! Provides default key bindings for the implemented v4 actions, using GPUI's
//! [`KeyBinding`] and context predicate system.

use crate::{actions::*, stoat::Mode};
use gpui::{KeyBinding, Keymap};
use serde::Deserialize;
use std::collections::HashMap;

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
    previous: Option<String>,
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
                "visual" => Ok(KeyBinding::new(key, EnterVisualMode, context)),
                "space" => Ok(KeyBinding::new(key, EnterSpaceMode, context)),
                "pane" => Ok(KeyBinding::new(key, EnterPaneMode, context)),
                "git_filter" => Ok(KeyBinding::new(key, EnterGitFilterMode, context)),
                "git_status" => Ok(KeyBinding::new(key, OpenGitStatus, context)),
                _ => Err(format!("Unsupported mode in SetMode: {mode_name}")),
            };
        }
    }

    match binding_config.action.as_str() {
        // Movement actions
        "MoveLeft" => Ok(KeyBinding::new(key, MoveLeft, context)),
        "MoveRight" => Ok(KeyBinding::new(key, MoveRight, context)),
        "MoveUp" => Ok(KeyBinding::new(key, MoveUp, context)),
        "MoveDown" => Ok(KeyBinding::new(key, MoveDown, context)),
        "MoveWordLeft" => Ok(KeyBinding::new(key, MoveWordLeft, context)),
        "MoveWordRight" => Ok(KeyBinding::new(key, MoveWordRight, context)),
        "MoveToLineStart" => Ok(KeyBinding::new(key, MoveToLineStart, context)),
        "MoveToLineEnd" => Ok(KeyBinding::new(key, MoveToLineEnd, context)),
        "MoveToFileStart" => Ok(KeyBinding::new(key, MoveToFileStart, context)),
        "MoveToFileEnd" => Ok(KeyBinding::new(key, MoveToFileEnd, context)),
        "PageUp" => Ok(KeyBinding::new(key, PageUp, context)),
        "PageDown" => Ok(KeyBinding::new(key, PageDown, context)),

        // Edit actions
        "DeleteLeft" => Ok(KeyBinding::new(key, DeleteLeft, context)),
        "DeleteRight" => Ok(KeyBinding::new(key, DeleteRight, context)),
        "DeleteWordLeft" => Ok(KeyBinding::new(key, DeleteWordLeft, context)),
        "DeleteWordRight" => Ok(KeyBinding::new(key, DeleteWordRight, context)),
        "NewLine" => Ok(KeyBinding::new(key, NewLine, context)),
        "DeleteLine" => Ok(KeyBinding::new(key, DeleteLine, context)),
        "DeleteToEndOfLine" => Ok(KeyBinding::new(key, DeleteToEndOfLine, context)),

        // Modal actions
        "EnterInsertMode" => Ok(KeyBinding::new(key, EnterInsertMode, context)),
        "EnterNormalMode" => Ok(KeyBinding::new(key, EnterNormalMode, context)),
        "EnterVisualMode" => Ok(KeyBinding::new(key, EnterVisualMode, context)),

        // File finder actions
        "OpenFileFinder" => Ok(KeyBinding::new(key, OpenFileFinder, context)),
        "FileFinderNext" => Ok(KeyBinding::new(key, FileFinderNext, context)),
        "FileFinderPrev" => Ok(KeyBinding::new(key, FileFinderPrev, context)),
        "FileFinderSelect" => Ok(KeyBinding::new(key, FileFinderSelect, context)),
        "FileFinderDismiss" => Ok(KeyBinding::new(key, FileFinderDismiss, context)),

        // Buffer finder actions
        "OpenBufferFinder" => Ok(KeyBinding::new(key, OpenBufferFinder, context)),
        "BufferFinderNext" => Ok(KeyBinding::new(key, BufferFinderNext, context)),
        "BufferFinderPrev" => Ok(KeyBinding::new(key, BufferFinderPrev, context)),
        "BufferFinderSelect" => Ok(KeyBinding::new(key, BufferFinderSelect, context)),
        "BufferFinderDismiss" => Ok(KeyBinding::new(key, BufferFinderDismiss, context)),

        // Command palette actions
        "OpenCommandPalette" => Ok(KeyBinding::new(key, OpenCommandPalette, context)),
        "CommandPaletteNext" => Ok(KeyBinding::new(key, CommandPaletteNext, context)),
        "CommandPalettePrev" => Ok(KeyBinding::new(key, CommandPalettePrev, context)),
        "CommandPaletteExecute" => Ok(KeyBinding::new(key, CommandPaletteExecute, context)),
        "CommandPaletteDismiss" => Ok(KeyBinding::new(key, CommandPaletteDismiss, context)),

        // Git status actions
        "OpenGitStatus" => Ok(KeyBinding::new(key, OpenGitStatus, context)),
        "GitStatusNext" => Ok(KeyBinding::new(key, GitStatusNext, context)),
        "GitStatusPrev" => Ok(KeyBinding::new(key, GitStatusPrev, context)),
        "GitStatusSelect" => Ok(KeyBinding::new(key, GitStatusSelect, context)),
        "GitStatusDismiss" => Ok(KeyBinding::new(key, GitStatusDismiss, context)),
        "GitStatusCycleFilter" => Ok(KeyBinding::new(key, GitStatusCycleFilter, context)),
        "GitStatusSetFilterAll" => Ok(KeyBinding::new(key, GitStatusSetFilterAll, context)),
        "GitStatusSetFilterStaged" => Ok(KeyBinding::new(key, GitStatusSetFilterStaged, context)),
        "GitStatusSetFilterUnstaged" => {
            Ok(KeyBinding::new(key, GitStatusSetFilterUnstaged, context))
        },
        "GitStatusSetFilterUnstagedWithUntracked" => Ok(KeyBinding::new(
            key,
            GitStatusSetFilterUnstagedWithUntracked,
            context,
        )),
        "GitStatusSetFilterUntracked" => {
            Ok(KeyBinding::new(key, GitStatusSetFilterUntracked, context))
        },

        // Git diff hunk actions
        "ToggleDiffHunk" => Ok(KeyBinding::new(key, ToggleDiffHunk, context)),
        "GotoNextHunk" => Ok(KeyBinding::new(key, GotoNextHunk, context)),
        "GotoPrevHunk" => Ok(KeyBinding::new(key, GotoPrevHunk, context)),

        // Diff review actions
        "OpenDiffReview" => Ok(KeyBinding::new(key, OpenDiffReview, context)),
        "DiffReviewNextHunk" => Ok(KeyBinding::new(key, DiffReviewNextHunk, context)),
        "DiffReviewPrevHunk" => Ok(KeyBinding::new(key, DiffReviewPrevHunk, context)),
        "DiffReviewApproveHunk" => Ok(KeyBinding::new(key, DiffReviewApproveHunk, context)),
        "DiffReviewToggleApproval" => Ok(KeyBinding::new(key, DiffReviewToggleApproval, context)),
        "DiffReviewNextUnreviewedHunk" => {
            Ok(KeyBinding::new(key, DiffReviewNextUnreviewedHunk, context))
        },
        "DiffReviewResetProgress" => Ok(KeyBinding::new(key, DiffReviewResetProgress, context)),
        "DiffReviewDismiss" => Ok(KeyBinding::new(key, DiffReviewDismiss, context)),

        // Help actions
        "OpenHelpOverlay" => Ok(KeyBinding::new(key, OpenHelpOverlay, context)),
        "OpenHelpModal" => Ok(KeyBinding::new(key, OpenHelpModal, context)),
        "HelpModalDismiss" => Ok(KeyBinding::new(key, HelpModalDismiss, context)),

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

        // Pane management actions
        "SplitUp" => Ok(KeyBinding::new(key, SplitUp, context)),
        "SplitDown" => Ok(KeyBinding::new(key, SplitDown, context)),
        "SplitLeft" => Ok(KeyBinding::new(key, SplitLeft, context)),
        "SplitRight" => Ok(KeyBinding::new(key, SplitRight, context)),
        "ClosePane" => Ok(KeyBinding::new(key, ClosePane, context)),
        "FocusPaneUp" => Ok(KeyBinding::new(key, FocusPaneUp, context)),
        "FocusPaneDown" => Ok(KeyBinding::new(key, FocusPaneDown, context)),
        "FocusPaneLeft" => Ok(KeyBinding::new(key, FocusPaneLeft, context)),
        "FocusPaneRight" => Ok(KeyBinding::new(key, FocusPaneRight, context)),

        // Application actions
        "QuitApp" => Ok(KeyBinding::new(key, QuitApp, context)),

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
/// Parse mode definitions from keymap.toml.
///
/// Reads the embedded keymap configuration and constructs a [`HashMap`] of mode
/// definitions with their display names and optional previous mode overrides.
///
/// # Returns
///
/// HashMap mapping mode names to [`Mode`] structs.
pub fn parse_modes_from_config() -> HashMap<String, Mode> {
    // Parse the embedded TOML configuration
    let config: KeymapConfig =
        toml::from_str(DEFAULT_KEYMAP_TOML).expect("Failed to parse embedded keymap.toml");

    // Convert mode configs to Mode structs
    config
        .modes
        .into_iter()
        .map(|mode_config| {
            let mode = if let Some(previous) = mode_config.previous {
                Mode::with_previous(mode_config.name.clone(), mode_config.display_name, previous)
            } else {
                Mode::new(mode_config.name.clone(), mode_config.display_name)
            };
            (mode_config.name, mode)
        })
        .collect()
}

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
