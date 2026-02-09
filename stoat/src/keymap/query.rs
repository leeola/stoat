use crate::keymap::compiled::{action_name, CompiledKeymap};
use std::collections::HashSet;

/// Query keybindings for a specific editor mode.
///
/// Returns a list of (keystroke, description) pairs for bindings whose
/// predicates include `mode == <mode>`. Prioritizes mode-specific bindings
/// over global ones and limits total entries for the overlay.
///
/// Used by [`crate::command::overlay::CommandOverlay`].
pub fn bindings_for_mode(keymap: &CompiledKeymap, mode: &str) -> Vec<(String, String)> {
    let mut mode_specific = Vec::new();
    let mut global = Vec::new();
    let mut seen_actions = HashSet::new();

    for binding in &keymap.bindings {
        let name = action_name(&binding.action);

        let Some(desc) = crate::actions::help_text_by_name(name) else {
            continue;
        };

        if seen_actions.contains(name) {
            continue;
        }

        let has_mode_pred = has_mode_predicate(&binding.predicates);
        let matches_mode = !has_mode_pred || matches_mode_predicate(&binding.predicates, mode);

        if !matches_mode {
            continue;
        }

        seen_actions.insert(name.to_string());

        let keystroke = binding.key.display();
        let entry = (keystroke, desc.to_string());

        if has_mode_pred {
            mode_specific.push(entry);
        } else {
            global.push(entry);
        }
    }

    let mut results = Vec::new();
    results.extend(mode_specific.into_iter().take(12));
    let remaining = 15usize.saturating_sub(results.len());
    results.extend(global.into_iter().take(remaining));
    results
}

fn has_mode_predicate(predicates: &[stoat_config::Predicate]) -> bool {
    predicates.iter().any(|p| is_mode_predicate(p))
}

fn matches_mode_predicate(predicates: &[stoat_config::Predicate], mode: &str) -> bool {
    predicates
        .iter()
        .filter(|p| is_mode_predicate(p))
        .all(|p| mode_predicate_matches(p, mode))
}

fn is_mode_predicate(pred: &stoat_config::Predicate) -> bool {
    match pred {
        stoat_config::Predicate::Eq(field, _) => field.node == "mode",
        stoat_config::Predicate::NotEq(field, _) => field.node == "mode",
        _ => false,
    }
}

fn mode_predicate_matches(pred: &stoat_config::Predicate, mode: &str) -> bool {
    match pred {
        stoat_config::Predicate::Eq(_, val) => match &val.node {
            stoat_config::Value::String(s) | stoat_config::Value::Ident(s) => s == mode,
            _ => false,
        },
        stoat_config::Predicate::NotEq(_, val) => match &val.node {
            stoat_config::Value::String(s) | stoat_config::Value::Ident(s) => s != mode,
            _ => true,
        },
        _ => true,
    }
}
