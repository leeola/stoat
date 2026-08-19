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
