use gpui::{KeyBinding, KeyContext, Keymap};
use std::collections::HashSet;

/// Query keybindings for a specific editor mode.
///
/// Returns a list of (keystroke, description) pairs for all bindings that match
/// the given mode's context. Prioritizes mode-specific bindings over global ones
/// and limits the total number shown to keep the overlay manageable.
///
/// # Arguments
/// * `keymap` - The keymap to query
/// * `mode` - The editor mode name to get bindings for
///
/// # Returns
/// A vector of (keystroke_string, description) tuples, limited to ~15 entries
pub fn bindings_for_mode(keymap: &Keymap, mode: &str) -> Vec<(String, String)> {
    // Build context for the given mode
    let context_str = format!("Editor mode={}", mode);
    let this_contexts = vec![
        KeyContext::parse("Workspace").unwrap_or_else(|_| KeyContext::default()),
        KeyContext::parse(&context_str).unwrap_or_else(|_| KeyContext::default()),
    ];

    let mut mode_specific = Vec::new();
    let mut global = Vec::new();
    let mut seen_actions = HashSet::new();

    // Separate mode-specific and global bindings
    for binding in keymap.bindings() {
        // Skip if no help text
        let Some(desc) = stoat::actions::help_text(binding.action()) else {
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

        let (matches_list, _pending) = keymap.bindings_for_input(keystrokes, &this_contexts);
        // Check if THIS specific binding is in the matches by comparing action types
        // (pointer equality doesn't work since bindings_for_input returns cloned bindings)
        if !matches_list
            .iter()
            .any(|b| b.action().type_id() == binding.action().type_id())
        {
            continue;
        }

        seen_actions.insert(action_id);

        // Check if this binding is mode-specific by testing it against all modes
        let is_mode_specific = is_binding_mode_specific(keymap, binding);

        let keystroke = format_keystrokes(binding);
        let entry = (keystroke, desc.to_string());

        if is_mode_specific {
            mode_specific.push(entry);
        } else {
            global.push(entry);
        }
    }

    // Prioritize mode-specific bindings, then add globals up to limit
    let mut results = Vec::new();
    results.extend(mode_specific.into_iter().take(12)); // Show up to 12 mode-specific

    let remaining = 15usize.saturating_sub(results.len());
    results.extend(global.into_iter().take(remaining)); // Fill to 15 total with globals

    results
}

/// Check if a binding is specific to one mode (vs available in all modes)
fn is_binding_mode_specific(keymap: &Keymap, binding: &KeyBinding) -> bool {
    let keystrokes = binding.keystrokes();

    // Test against all modes
    let modes = ["normal", "insert", "visual", "pane", "file_finder", "space"];
    let mut active_in_count = 0;

    for mode_name in modes {
        let context_str = format!("Editor mode={}", mode_name);
        let contexts = vec![
            KeyContext::parse("Workspace").unwrap_or_else(|_| KeyContext::default()),
            KeyContext::parse(&context_str).unwrap_or_else(|_| KeyContext::default()),
        ];

        let (matches, _) = keymap.bindings_for_input(keystrokes, &contexts);
        if !matches.is_empty() {
            active_in_count += 1;
        }
    }

    // If active in only 1 mode, it's mode-specific
    active_in_count == 1
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
            // Use GPUI's Display impl
            format!("{}", k)
        })
        .collect::<Vec<_>>()
        .join(" ")
}
