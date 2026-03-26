use crate::{action::define_action, ActionKind};

define_action!(
    QuitDef,
    Quit,
    "Quit",
    ActionKind::Quit,
    "exit stoat",
    "Exit the application, closing all panes and viewports."
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
        assert_eq!(Quit.def().short_desc(), "exit stoat");
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(Quit);
        assert!(action.as_any().downcast_ref::<Quit>().is_some());
    }
}
