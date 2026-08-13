use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    OpenHelpDef,
    OpenHelp,
    "OpenHelp",
    ActionKind::OpenHelp,
    "open the help modal",
    "Open an interactive help modal that lists keybindings active for the \
     current state. Type to filter, Up/Down to browse, Shift-Tab to toggle \
     between active bindings and every registered action, Enter to invoke \
     the selected action, Escape to switch to normal mode (then Escape \
     again to close).",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    ToggleKeyHintsDef,
    ToggleKeyHints,
    "ToggleKeyHints",
    ActionKind::ToggleKeyHints,
    "toggle the keybinding hints overlay",
    "Show or hide the compact keybinding hints overlay for the current \
     mode. Normal mode shows it on nothing else, so this is how to bring up \
     its active-binding list. Invoke it again to dismiss.",
    ActionPriority::Common,
    command_name = "hints"
);

define_action!(
    DismissKeyHintsDef,
    DismissKeyHints,
    "DismissKeyHints",
    ActionKind::DismissKeyHints,
    "dismiss the keybinding hints overlay",
    "Hide the keybinding hints overlay when it is showing. Bound to Escape \
     in normal mode as a dedicated close for the hints, and a no-op when \
     they are already hidden.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    HelpSelectPrevDef,
    HelpSelectPrev,
    "HelpSelectPrev",
    ActionKind::HelpSelectPrev,
    "previous help entry",
    "Move the help selection up by one row while the help modal is open.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    HelpSelectNextDef,
    HelpSelectNext,
    "HelpSelectNext",
    ActionKind::HelpSelectNext,
    "next help entry",
    "Move the help selection down by one row while the help modal is open.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    HelpCompleteDef,
    HelpComplete,
    "HelpComplete",
    ActionKind::HelpComplete,
    "complete selected help entry",
    "Complete the highlighted action's name into the help search input, \
     replacing what was typed. The completed action stays selected. Bound by \
     default to Tab while the help modal's search input is focused; a no-op \
     when the list is empty.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    HelpScopeToggleDef,
    HelpScopeToggle,
    "HelpScopeToggle",
    ActionKind::HelpScopeToggle,
    "toggle help scope",
    "Toggle the help listing between active-bindings-only and all registered actions.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    HelpScrollDetailUpDef,
    HelpScrollDetailUp,
    "HelpScrollDetailUp",
    ActionKind::HelpScrollDetailUp,
    "scroll help detail up",
    "Scroll the help detail pane toward the top by five rows.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    HelpScrollDetailDownDef,
    HelpScrollDetailDown,
    "HelpScrollDetailDown",
    ActionKind::HelpScrollDetailDown,
    "scroll help detail down",
    "Scroll the help detail pane toward the bottom by five rows.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    HelpJumpFirstDef,
    HelpJumpFirst,
    "HelpJumpFirst",
    ActionKind::HelpJumpFirst,
    "jump to first help entry",
    "Move the help selection to the first entry in the current filter.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    HelpJumpLastDef,
    HelpJumpLast,
    "HelpJumpLast",
    ActionKind::HelpJumpLast,
    "jump to last help entry",
    "Move the help selection to the last entry in the current filter.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    CloseHelpDef,
    CloseHelp,
    "CloseHelp",
    ActionKind::CloseHelp,
    "close help",
    "Close the help modal and restore the mode that was active before it opened.",
    ActionPriority::Normal,
    palette_visible = false
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn kind_and_name() {
        assert_eq!(OpenHelp.kind(), ActionKind::OpenHelp);
        assert_eq!(OpenHelp.def().name(), "OpenHelp");
        assert!(OpenHelp.def().params().is_empty());
        assert!(!OpenHelp.def().palette_visible());
    }
}
