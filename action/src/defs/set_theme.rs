use crate::{
    action::define_action_def, Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind,
    ValueSource,
};
use std::any::Any;

const PARAMS: &[ParamDef] = &[ParamDef {
    name: "name",
    kind: ParamKind::String,
    value_source: ValueSource::Themes,
    required: true,
    description: "Name of the theme block to activate, resolved against the config's `theme NAME { ... }` blocks.",
}];

define_action_def!(
    SetThemeDef,
    "SetTheme",
    ActionKind::SetTheme,
    "switch the active theme",
    "Re-resolve the named theme against the loaded theme blocks and apply it immediately, without restarting. Keeps the current theme and shows a message when the name is unknown.",
    ActionPriority::Normal,
    command_name = "theme",
    params = PARAMS
);

#[derive(Debug)]
pub struct SetTheme {
    pub name: String,
}

impl SetTheme {
    pub const DEF: &SetThemeDef = &SetThemeDef;
}

impl Action for SetTheme {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_and_name() {
        let action = SetTheme {
            name: "default_dark".to_string(),
        };
        assert_eq!(action.kind(), ActionKind::SetTheme);
        assert_eq!(action.def().name(), "SetTheme");
        assert_eq!(action.def().params().len(), 1);
        assert_eq!(action.def().params()[0].name, "name");
        assert_eq!(action.def().command_name(), Some("theme"));
    }
}
