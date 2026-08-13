use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    OpenCommandPaletteDef,
    OpenCommandPalette,
    "OpenCommandPalette",
    ActionKind::OpenCommandPalette,
    "open the command palette",
    "Open an interactive list of every visible action. Type to filter, \
     Up/Down to navigate, Enter to invoke, Escape to cancel.",
    ActionPriority::Normal,
    palette_visible = false
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn kind_and_name() {
        assert_eq!(OpenCommandPalette.kind(), ActionKind::OpenCommandPalette);
        assert_eq!(OpenCommandPalette.def().name(), "OpenCommandPalette");
        assert!(OpenCommandPalette.def().params().is_empty());
        assert!(!OpenCommandPalette.def().palette_visible());
    }
}
