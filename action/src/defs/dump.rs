use crate::{
    action::impl_gpui_action, Action, ActionDef, ActionKind, ActionPriority, ActionTarget,
    ParamDef, ParamKind,
};
use serde::Deserialize;
use std::any::Any;

const PARAMS: &[ParamDef] = &[ParamDef {
    name: "name",
    kind: ParamKind::String,
    required: true,
    description: "Human-readable name for the dump. Sanitized into a path-friendly slug (lowercase, whitespace becomes '-', invalid chars dropped, truncated to 64 chars).",
}];

#[derive(Debug)]
pub struct DumpDef;

impl ActionDef for DumpDef {
    fn name(&self) -> &'static str {
        "Dump"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::Dump
    }

    fn params(&self) -> &'static [ParamDef] {
        PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "capture a workspace dump"
    }

    fn long_desc(&self) -> &'static str {
        "Write a single-file snapshot of the current repository (working tree + `.git/`) plus metadata to `$XDG_DATA_HOME/stoat/dumps/<timestamp>_<name>.dump`. The name is sanitized into a path-safe slug."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }

    fn target(&self) -> ActionTarget {
        ActionTarget::Root
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Dump {
    pub name: String,
}

impl Dump {
    pub const DEF: &DumpDef = &DumpDef;
}

impl Action for Dump {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl_gpui_action!(Dump, "Dump");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_and_name() {
        let action = Dump {
            name: "x".to_string(),
        };
        assert_eq!(action.kind(), ActionKind::Dump);
        assert_eq!(action.def().name(), "Dump");
        assert_eq!(action.def().params().len(), 1);
        assert_eq!(action.def().params()[0].name, "name");
    }

    #[test]
    fn downcast_preserves_name() {
        let boxed: Box<dyn Action> = Box::new(Dump {
            name: "my-bug".to_string(),
        });
        let recovered = boxed.as_any().downcast_ref::<Dump>().expect("downcast");
        assert_eq!(recovered.name, "my-bug");
    }
}
