use crate::{action::define_action, ActionKind, ActionPriority, ActionTarget};

define_action!(
    QuitDef,
    Quit,
    "Quit",
    ActionKind::Quit,
    "close pane or exit",
    "Close the focused pane. Exit the application when closing the last remaining pane.",
    ActionPriority::Common,
    ActionTarget::Root
);

define_action!(
    QuitAllDef,
    QuitAll,
    "QuitAll",
    ActionKind::QuitAll,
    "exit stoat, closing all panes",
    "Exit the application immediately, closing every pane and viewport. See also Quit, which closes the current pane and only exits when it is the last.",
    ActionPriority::Common,
    ActionTarget::Root
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
    fn downcast() {
        let action: Box<dyn Action> = Box::new(Quit);
        assert!(action.as_any().downcast_ref::<Quit>().is_some());
    }
}
