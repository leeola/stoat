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

macro_rules! plumbing_action {
    ($def:ident, $action:ident, $name:expr, $kind:expr, $short:expr, $long:expr) => {
        #[derive(Debug)]
        pub struct $def;

        impl ActionDef for $def {
            fn name(&self) -> &'static str {
                $name
            }

            fn kind(&self) -> ActionKind {
                $kind
            }

            fn params(&self) -> &'static [ParamDef] {
                &[]
            }

            fn short_desc(&self) -> &'static str {
                $short
            }

            fn long_desc(&self) -> &'static str {
                $long
            }

            fn palette_visible(&self) -> bool {
                false
            }
        }

        #[derive(Debug)]
        pub struct $action;

        impl $action {
            pub const DEF: &$def = &$def;
        }

        impl Action for $action {
            fn def(&self) -> &'static dyn ActionDef {
                Self::DEF
            }

            fn as_any(&self) -> &dyn Any {
                self
            }
        }
    };
}

plumbing_action!(
    HelpSelectPrevDef,
    HelpSelectPrev,
    "HelpSelectPrev",
    ActionKind::HelpSelectPrev,
    "previous help entry",
    "Move the help selection up by one row while the help modal is open."
);

plumbing_action!(
    HelpSelectNextDef,
    HelpSelectNext,
    "HelpSelectNext",
    ActionKind::HelpSelectNext,
    "next help entry",
    "Move the help selection down by one row while the help modal is open."
);

plumbing_action!(
    HelpScopeToggleDef,
    HelpScopeToggle,
    "HelpScopeToggle",
    ActionKind::HelpScopeToggle,
    "toggle help scope",
    "Toggle the help listing between active-bindings-only and all registered actions."
);

plumbing_action!(
    HelpScrollDetailUpDef,
    HelpScrollDetailUp,
    "HelpScrollDetailUp",
    ActionKind::HelpScrollDetailUp,
    "scroll help detail up",
    "Scroll the help detail pane toward the top by five rows."
);

plumbing_action!(
    HelpScrollDetailDownDef,
    HelpScrollDetailDown,
    "HelpScrollDetailDown",
    ActionKind::HelpScrollDetailDown,
    "scroll help detail down",
    "Scroll the help detail pane toward the bottom by five rows."
);

plumbing_action!(
    HelpJumpFirstDef,
    HelpJumpFirst,
    "HelpJumpFirst",
    ActionKind::HelpJumpFirst,
    "jump to first help entry",
    "Move the help selection to the first entry in the current filter."
);

plumbing_action!(
    HelpJumpLastDef,
    HelpJumpLast,
    "HelpJumpLast",
    ActionKind::HelpJumpLast,
    "jump to last help entry",
    "Move the help selection to the last entry in the current filter."
);

plumbing_action!(
    CloseHelpDef,
    CloseHelp,
    "CloseHelp",
    ActionKind::CloseHelp,
    "close help",
    "Close the help modal and restore the mode that was active before it opened."
);

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
