use crate::keymap::{
    compiled::{action_name, CompiledKeymap},
    infobox::{Infobox, InfoboxEntry},
    usage::UsageTracker,
};
use std::collections::{HashMap, HashSet};

/// Query keybindings for a specific editor mode.
///
/// Returns a list of (keystroke, description) pairs for bindings whose
/// predicates include `mode == <mode>`. Prioritizes mode-specific bindings
/// over global ones and limits total entries.
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

/// Build an [`Infobox`] for the given mode, grouping keys by action and filtering
/// out actions the user has already mastered (according to [`UsageTracker`]).
pub fn bindings_for_infobox(
    keymap: &CompiledKeymap,
    mode: &str,
    mode_display: &str,
    usage: &UsageTracker,
) -> Infobox {
    let mut mode_specific: Vec<(String, String, String)> = Vec::new();
    let mut global: Vec<(String, String, String)> = Vec::new();

    for binding in &keymap.bindings {
        let name = action_name(&binding.action);

        let Some(desc) = crate::actions::help_text_by_name(name) else {
            continue;
        };

        let has_mode_pred = has_mode_predicate(&binding.predicates);
        let matches_mode = !has_mode_pred || matches_mode_predicate(&binding.predicates, mode);

        if !matches_mode {
            continue;
        }

        if usage.should_hide(mode, name) {
            continue;
        }

        let keystroke = binding.key.display();

        if has_mode_pred {
            mode_specific.push((name.to_string(), keystroke, desc.to_string()));
        } else {
            global.push((name.to_string(), keystroke, desc.to_string()));
        }
    }

    // Group keys by action name, preserving order of first appearance
    fn group_entries(items: Vec<(String, String, String)>) -> Vec<InfoboxEntry> {
        let mut order: Vec<String> = Vec::new();
        let mut grouped: HashMap<String, (Vec<String>, String)> = HashMap::new();
        for (name, key, desc) in items {
            if let Some(entry) = grouped.get_mut(&name) {
                if !entry.0.contains(&key) {
                    entry.0.push(key);
                }
            } else {
                order.push(name.clone());
                grouped.insert(name, (vec![key], desc));
            }
        }
        order
            .into_iter()
            .filter_map(|name| {
                grouped
                    .remove(&name)
                    .map(|(keys, description)| InfoboxEntry { keys, description })
            })
            .collect()
    }

    let mut entries = group_entries(mode_specific);
    entries.extend(group_entries(global));

    Infobox {
        title: mode_display.to_string(),
        entries,
    }
}

fn has_mode_predicate(predicates: &[stoat_config::Predicate]) -> bool {
    predicates.iter().any(is_mode_predicate)
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
