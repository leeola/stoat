use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    QuitDef,
    Quit,
    "Quit",
    ActionKind::Quit,
    "close pane or exit",
    "Close the focused pane. Exit the application when closing the last remaining pane.",
    ActionPriority::Common
);

define_action!(
    QuitAllDef,
    QuitAll,
    "QuitAll",
    ActionKind::QuitAll,
    "exit stoat, closing all panes",
    "Exit the application immediately, closing every pane and viewport. See also Quit, which closes the current pane and only exits when it is the last.",
    ActionPriority::Common
);

define_action!(
    DismissModalDef,
    DismissModal,
    "DismissModal",
    ActionKind::DismissModal,
    "dismiss the active modal",
    "Close the topmost modal in the workspace's modal layer. Dispatched by the modal layer's backdrop click handler and bindable from the keymap (typically `Escape` while a modal is open).",
    ActionPriority::Common
);

define_action!(
    IncreaseFontSizeDef,
    IncreaseFontSize,
    "IncreaseFontSize",
    ActionKind::IncreaseFontSize,
    "increase the editor font size",
    "Scale the editor buffer font up one step for the current session. The override is session-only (not persisted) and affects only the editor buffer font, not the UI or terminal fonts.",
    ActionPriority::Common,
    false
);

define_action!(
    DecreaseFontSizeDef,
    DecreaseFontSize,
    "DecreaseFontSize",
    ActionKind::DecreaseFontSize,
    "decrease the editor font size",
    "Scale the editor buffer font down one step for the current session. The override is session-only (not persisted) and affects only the editor buffer font, not the UI or terminal fonts.",
    ActionPriority::Common,
    false
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn quit() {
        assert_eq!(Quit.kind(), ActionKind::Quit);
        assert_eq!(Quit.def().name(), "Quit");
        assert!(Quit.def().params().is_empty());
        assert_eq!(Quit.def().short_desc(), "close pane or exit");
    }

    #[test]
    fn quit_all() {
        assert_eq!(QuitAll.kind(), ActionKind::QuitAll);
        assert_eq!(QuitAll.def().name(), "QuitAll");
        assert!(QuitAll.def().params().is_empty());
        assert_eq!(QuitAll.def().short_desc(), "exit stoat, closing all panes");
    }

    #[test]
    fn dismiss_modal() {
        assert_eq!(DismissModal.kind(), ActionKind::DismissModal);
        assert_eq!(DismissModal.def().name(), "DismissModal");
        assert!(DismissModal.def().params().is_empty());
    }

    #[test]
    fn increase_font_size() {
        assert_eq!(IncreaseFontSize.kind(), ActionKind::IncreaseFontSize);
        assert_eq!(IncreaseFontSize.def().name(), "IncreaseFontSize");
        assert!(IncreaseFontSize.def().params().is_empty());
        assert!(!IncreaseFontSize.def().hint_visible());
    }

    #[test]
    fn decrease_font_size() {
        assert_eq!(DecreaseFontSize.kind(), ActionKind::DecreaseFontSize);
        assert_eq!(DecreaseFontSize.def().name(), "DecreaseFontSize");
        assert!(DecreaseFontSize.def().params().is_empty());
        assert!(!DecreaseFontSize.def().hint_visible());
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(Quit);
        assert!(action.as_any().downcast_ref::<Quit>().is_some());
    }
}
