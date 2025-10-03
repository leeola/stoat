use gpui::{KeyBinding, Keymap};
use stoat::EditorMode;

/// Query keybindings for a specific editor mode.
///
/// Returns a list of (keystroke, description) pairs for all bindings that match
/// the given mode's context. The keystrokes are formatted for display and the
/// descriptions come from the action's registered short description.
///
/// # Arguments
/// * `keymap` - The keymap to query
/// * `mode` - The editor mode to get bindings for
///
/// # Returns
/// A vector of (keystroke_string, description) tuples
pub fn bindings_for_mode(keymap: &Keymap, _mode: EditorMode) -> Vec<(String, String)> {
    // FIXME: Context filtering disabled - KeyContext::parse causes stack overflow
    // For now, show all bindings regardless of mode

    keymap
        .bindings()
        .filter_map(|binding| {
            // Format keystrokes for display
            let keystroke = format_keystrokes(binding);

            // Get description from action
            let desc = stoat::actions::short_desc(binding.action())?;

            Some((keystroke, desc.to_string()))
        })
        .take(10) // FIXME: Limit to 10 bindings for now to keep overlay manageable
        .collect()
}

/// Format keystrokes for display.
///
/// Converts GPUI keystroke representation to user-friendly strings.
/// Examples: "h", "Ctrl-W V", "Cmd-S"
fn format_keystrokes(binding: &KeyBinding) -> String {
    binding
        .keystrokes()
        .iter()
        .map(|k| {
            // FIXME: Use GPUI's Display impl for now, refine formatting later
            format!("{}", k)
        })
        .collect::<Vec<_>>()
        .join(" ")
}
