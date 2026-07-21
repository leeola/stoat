use crate::{Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind, ValueSource};
use std::any::Any;

const PARAMS: &[ParamDef] = &[ParamDef {
    name: "name",
    kind: ParamKind::String,
    value_source: ValueSource::Themes,
    required: true,
    description: "Name of the theme block to activate, resolved against the config's `theme NAME { ... }` blocks.",
}];

#[derive(Debug)]
pub struct SetThemeDef;

impl ActionDef for SetThemeDef {
    fn name(&self) -> &'static str {
        "SetTheme"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::SetTheme
    }

    fn params(&self) -> &'static [ParamDef] {
        PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "switch the active theme"
    }

    fn long_desc(&self) -> &'static str {
        "Re-resolve the named theme against the loaded theme blocks and apply it immediately, without restarting. Keeps the current theme and shows a message when the name is unknown."
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["theme"]
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Normal
    }
}

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
        assert_eq!(action.def().aliases(), ["theme"]);
    }
}
