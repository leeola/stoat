use gpui::{KeyBinding, KeyContext, Keymap};
use std::collections::HashSet;
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
pub fn bindings_for_mode(keymap: &Keymap, mode: EditorMode) -> Vec<(String, String)> {
    // Build context for the given mode
    let mode_str = match mode {
        EditorMode::Normal => "normal",
        EditorMode::Insert => "insert",
        EditorMode::Visual => "visual",
    };

    // Use format matching GPUI's tests: "Editor mode=normal" not "Editor && mode == normal"
    let context_str = format!("Editor mode={}", mode_str);
    let contexts = vec![
        KeyContext::parse("Workspace").unwrap_or_else(|_| KeyContext::default()),
        KeyContext::parse(&context_str).unwrap_or_else(|_| KeyContext::default()),
    ];

    let mut results = Vec::new();
    let mut seen_actions = HashSet::new();

    // Iterate through all single-key bindings first
    for binding in keymap.bindings() {
        // Skip if no short description
        let Some(desc) = stoat::actions::short_desc(binding.action()) else {
            continue;
        };

        // Skip duplicates (same action)
        let action_id = binding.action().type_id();
        if seen_actions.contains(&action_id) {
            continue;
        }

        // Check if binding is active in current context
        let keystrokes = binding.keystrokes();
        if keystrokes.is_empty() {
            continue;
        }

        let (matches, _pending) = keymap.bindings_for_input(keystrokes, &contexts);
        if matches.is_empty() {
            continue;
        }

        // Format and add binding
        let keystroke = format_keystrokes(binding);
        results.push((keystroke, desc.to_string()));
        seen_actions.insert(action_id);
    }

    results
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
