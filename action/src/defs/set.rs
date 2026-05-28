use crate::{Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind};
use std::any::Any;

const PARAMS: &[ParamDef] = &[
    ParamDef {
        name: "key",
        kind: ParamKind::String,
        required: true,
        description: "Dotted setting path, e.g. `ui.pane.show_tab_bar`.",
    },
    ParamDef {
        name: "value",
        kind: ParamKind::String,
        required: true,
        description: "Stringly value parsed against the setting's type. Bool keys accept true/false/yes/no/on/off/1/0.",
    },
];

#[derive(Debug)]
pub struct SetDef;

impl ActionDef for SetDef {
    fn name(&self) -> &'static str {
        "Set"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::Set
    }

    fn params(&self) -> &'static [ParamDef] {
        PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "set a runtime setting"
    }

    fn long_desc(&self) -> &'static str {
        "Apply a key/value pair to the runtime `Settings` global. Session-only -- restart reverts to the stcfg value. Unknown keys and unparseable values are rejected with a warning; the existing Settings stay untouched."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Set {
    pub key: String,
    pub value: String,
}

impl Set {
    pub const DEF: &SetDef = &SetDef;
}

impl Action for Set {
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
    fn set_kind_and_params() {
        let action = Set {
            key: "ui.pane.show_tab_bar".into(),
            value: "false".into(),
        };
        assert_eq!(action.kind(), ActionKind::Set);
        assert_eq!(action.def().name(), "Set");
        assert_eq!(action.def().params().len(), 2);
        assert_eq!(action.def().params()[0].name, "key");
        assert_eq!(action.def().params()[1].name, "value");
        assert!(action.def().palette_visible());
        assert_eq!(action.def().priority(), ActionPriority::Common);
    }
}
