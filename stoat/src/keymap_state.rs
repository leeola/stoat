use crate::{
    app::Stoat,
    keymap::{KeymapState, ResolvedAction, ResolvedArg, StateValue},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use stoat_action::Action;

pub(crate) struct StoatKeymapState {
    mode_value: StateValue,
    palette_open: StateValue,
    help_open: StateValue,
    finder_open: StateValue,
}

impl StoatKeymapState {
    pub(crate) fn new(mode: &str) -> Self {
        Self::with_flags(mode, false, false, false)
    }

    pub(crate) fn with_flags(
        mode: &str,
        palette_open: bool,
        help_open: bool,
        finder_open: bool,
    ) -> Self {
        Self {
            mode_value: StateValue::String(mode.into()),
            palette_open: StateValue::Bool(palette_open),
            help_open: StateValue::Bool(help_open),
            finder_open: StateValue::Bool(finder_open),
        }
    }

    pub(crate) fn from_stoat(stoat: &Stoat) -> Self {
        Self::with_flags(
            stoat.mode.as_str(),
            stoat.command_palette.is_some(),
            stoat.help.is_some(),
            stoat.file_finder.is_some(),
        )
    }
}

impl KeymapState for StoatKeymapState {
    fn get(&self, field: &str) -> Option<&StateValue> {
        match field {
            "mode" => Some(&self.mode_value),
            "palette_open" => Some(&self.palette_open),
            "help_open" => Some(&self.help_open),
            "finder_open" => Some(&self.finder_open),
            _ => None,
        }
    }
}

/// Collapse Shift+letter events onto the bare uppercase form so keymap bindings
/// written as `A` or `S-a` both match what terminals emit.
///
/// Default crossterm without the kitty keyboard protocol reports Shift+a as
/// `(Char('A'), SHIFT)`, but a binding written as `A` compiles to
/// `(Char('A'), NONE)`, and modifier comparison is strict. Normalizing the
/// event up-front keeps bindings terminal-agnostic.
pub(crate) fn normalize_shift_letter(key: KeyEvent) -> KeyEvent {
    if !key.modifiers.contains(KeyModifiers::SHIFT) {
        return key;
    }
    let KeyCode::Char(ch) = key.code else {
        return key;
    };
    if !ch.is_ascii_alphabetic() {
        return key;
    }
    let mut modifiers = key.modifiers;
    modifiers.remove(KeyModifiers::SHIFT);
    KeyEvent::new(KeyCode::Char(ch.to_ascii_uppercase()), modifiers)
}

pub(crate) fn arg_as_str(arg: &ResolvedArg) -> Option<String> {
    match &arg.value {
        stoat_config::Value::String(s) => Some(s.clone()),
        stoat_config::Value::Ident(s) => Some(s.clone()),
        _ => None,
    }
}

fn arg_to_param_value(arg: &ResolvedArg) -> Option<stoat_action::ParamValue> {
    match &arg.value {
        stoat_config::Value::String(s) => Some(stoat_action::ParamValue::String(s.clone())),
        stoat_config::Value::Ident(s) => Some(stoat_action::ParamValue::String(s.clone())),
        stoat_config::Value::Number(n) => Some(stoat_action::ParamValue::Number(*n)),
        stoat_config::Value::Bool(b) => Some(stoat_action::ParamValue::Bool(*b)),
        _ => None,
    }
}

pub(crate) fn action_display_desc(action: &ResolvedAction) -> String {
    if action.name == "SetMode" {
        let target = action.args.first().and_then(arg_as_str).unwrap_or_default();
        return format!("{target} mode");
    }
    stoat_action::registry::lookup(&action.name)
        .map(|e| e.def.short_desc().to_string())
        .unwrap_or_else(|| action.name.clone())
}

pub(crate) fn resolve_action(name: &str, args: &[ResolvedArg]) -> Option<Box<dyn Action>> {
    let entry = stoat_action::registry::lookup(name)?;
    let mut params = Vec::with_capacity(args.len());
    for arg in args {
        match arg_to_param_value(arg) {
            Some(value) => params.push(value),
            None => {
                tracing::warn!("action `{name}`: cannot convert arg {:?}", arg.value);
                return None;
            },
        }
    }
    match (entry.create)(&params) {
        Ok(action) => Some(action),
        Err(e) => {
            tracing::warn!("action `{name}`: {e}");
            None
        },
    }
}
