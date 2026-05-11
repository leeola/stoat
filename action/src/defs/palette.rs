use crate::{Action, ActionDef, ActionKind, ParamDef};
use std::any::Any;

#[derive(Debug)]
pub struct OpenCommandPaletteDef;

impl ActionDef for OpenCommandPaletteDef {
    fn name(&self) -> &'static str {
        "OpenCommandPalette"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenCommandPalette
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "open the command palette"
    }

    fn long_desc(&self) -> &'static str {
        "Open an interactive list of every visible action. Type to filter, \
         Up/Down to navigate, Enter to invoke, Escape to cancel."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct OpenCommandPalette;

impl OpenCommandPalette {
    pub const DEF: &OpenCommandPaletteDef = &OpenCommandPaletteDef;
}

impl Action for OpenCommandPalette {
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
        assert_eq!(OpenCommandPalette.kind(), ActionKind::OpenCommandPalette);
        assert_eq!(OpenCommandPalette.def().name(), "OpenCommandPalette");
        assert!(OpenCommandPalette.def().params().is_empty());
        assert!(!OpenCommandPalette.def().palette_visible());
    }
}
