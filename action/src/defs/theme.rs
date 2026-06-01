use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    OpenThemePickerDef,
    OpenThemePicker,
    "OpenThemePicker",
    ActionKind::OpenThemePicker,
    "open theme picker",
    "Open a picker to switch the active theme. Arrow through entries to \
     preview each theme live; Enter keeps the highlighted theme, Escape \
     restores the original.",
    ActionPriority::Common
);
