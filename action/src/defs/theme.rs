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
    ActionPriority::Common,
    false
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn open_theme_picker() {
        assert_eq!(OpenThemePicker.kind(), ActionKind::OpenThemePicker);
        assert_eq!(OpenThemePicker.def().name(), "OpenThemePicker");
        assert!(!OpenThemePicker.def().hint_visible());
    }
}
