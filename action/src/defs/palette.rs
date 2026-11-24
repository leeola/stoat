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

define_action!(
    PaletteRefreshDef,
    PaletteRefresh,
    "PaletteRefresh",
    ActionKind::PaletteRefresh,
    "run the pick list again",
    "Run the list command of the palette's pick list again. The rows on show \
     stay until the new rows land, and the preview of each row runs again. \
     With no pick list on show, nothing changes.",
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

    /// The refresh runs from its key over a pick list, so the palette does not
    /// list it.
    #[test]
    fn palette_refresh_is_not_palette_visible() {
        assert_eq!(
            (
                PaletteRefresh.kind(),
                PaletteRefresh.def().name(),
                PaletteRefresh.def().params().is_empty(),
                PaletteRefresh.def().palette_visible(),
            ),
            (ActionKind::PaletteRefresh, "PaletteRefresh", true, false),
        );
    }
}
