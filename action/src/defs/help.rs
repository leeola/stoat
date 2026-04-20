use crate::{Action, ActionDef, ActionKind, ParamDef};
use std::any::Any;

#[derive(Debug)]
pub struct OpenHelpDef;

impl ActionDef for OpenHelpDef {
    fn name(&self) -> &'static str {
        "OpenHelp"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenHelp
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "open the help modal"
    }

    fn long_desc(&self) -> &'static str {
        "Open an interactive help modal that lists keybindings active for the \
         current state. Type to filter, Up/Down to browse, Shift-Tab to toggle \
         between active bindings and every registered action, Enter to invoke \
         the selected action, Escape to switch to normal mode (then Escape \
         again to close)."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct OpenHelp;

impl OpenHelp {
    pub const DEF: &OpenHelpDef = &OpenHelpDef;
}

impl Action for OpenHelp {
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
        assert_eq!(OpenHelp.kind(), ActionKind::OpenHelp);
        assert_eq!(OpenHelp.def().name(), "OpenHelp");
        assert!(OpenHelp.def().params().is_empty());
        assert!(!OpenHelp.def().palette_visible());
    }
}
