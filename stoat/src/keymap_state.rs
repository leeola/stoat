use crate::{
    app::Stoat,
    keymap::{is_text_input_mode, KeymapState, ResolvedAction, StateValue},
};

pub(crate) struct StoatKeymapState {
    mode_value: StateValue,
    palette_open: StateValue,
    help_open: StateValue,
    finder_open: StateValue,
    input_active: StateValue,
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
            input_active: StateValue::Bool(is_text_input_mode(mode)),
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
            "input_active" => Some(&self.input_active),
            _ => None,
        }
    }
}

pub(crate) fn action_display_desc(action: &ResolvedAction) -> String {
    if action.name == "SetMode" {
        let target = action
            .args
            .first()
            .and_then(crate::keymap::arg_as_str)
            .unwrap_or_default();
        return format!("{target} mode");
    }
    stoat_action::registry::lookup(&action.name)
        .map(|e| e.def.short_desc().to_string())
        .unwrap_or_else(|| action.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_active_true_only_for_text_entry_modes() {
        for mode in ["insert", "reword_insert", "prompt", "run"] {
            assert_eq!(
                StoatKeymapState::new(mode).get("input_active"),
                Some(&StateValue::Bool(true)),
                "{mode}"
            );
        }
        for mode in ["normal", "review", "space", "rebase"] {
            assert_eq!(
                StoatKeymapState::new(mode).get("input_active"),
                Some(&StateValue::Bool(false)),
                "{mode}"
            );
        }
    }
}
