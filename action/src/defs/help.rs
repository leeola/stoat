use crate::{Action, ActionDef, ActionKind, ActionPriority, ParamDef};
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

#[derive(Debug)]
pub struct ToggleKeyHintsDef;

impl ActionDef for ToggleKeyHintsDef {
    fn name(&self) -> &'static str {
        "ToggleKeyHints"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ToggleKeyHints
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "toggle the keybinding hints overlay"
    }

    fn long_desc(&self) -> &'static str {
        "Show or hide the compact keybinding hints overlay for the current \
         mode. Normal mode shows it on nothing else, so this is how to bring up \
         its active-binding list. Invoke it again to dismiss."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["hints"]
    }
}

#[derive(Debug)]
pub struct ToggleKeyHints;

impl ToggleKeyHints {
    pub const DEF: &ToggleKeyHintsDef = &ToggleKeyHintsDef;
}

impl Action for ToggleKeyHints {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct DismissKeyHintsDef;

impl ActionDef for DismissKeyHintsDef {
    fn name(&self) -> &'static str {
        "DismissKeyHints"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::DismissKeyHints
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "dismiss the keybinding hints overlay"
    }

    fn long_desc(&self) -> &'static str {
        "Hide the keybinding hints overlay when it is showing. Bound to Escape \
         in normal mode as a dedicated close for the hints, and a no-op when \
         they are already hidden."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct DismissKeyHints;

impl DismissKeyHints {
    pub const DEF: &DismissKeyHintsDef = &DismissKeyHintsDef;
}

impl Action for DismissKeyHints {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

macro_rules! plumbing_action {
    ($def:ident, $action:ident, $name:expr_2021, $kind:expr_2021, $short:expr_2021, $long:expr_2021) => {
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
