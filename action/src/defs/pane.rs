use crate::{action::define_action, ActionKind};

define_action!(
    SplitRightDef,
    SplitRight,
    "SplitRight",
    ActionKind::SplitRight,
    "split pane right",
    "Split the focused pane vertically, creating a new pane to the right."
);
define_action!(
    SplitDownDef,
    SplitDown,
    "SplitDown",
    ActionKind::SplitDown,
    "split pane down",
    "Split the focused pane horizontally, creating a new pane below."
);
define_action!(
    FocusLeftDef,
    FocusLeft,
    "FocusLeft",
    ActionKind::FocusLeft,
    "focus pane left",
    "Move focus to the pane to the left of the current pane."
);
define_action!(
    FocusRightDef,
    FocusRight,
    "FocusRight",
    ActionKind::FocusRight,
    "focus pane right",
    "Move focus to the pane to the right of the current pane."
);
define_action!(
    FocusUpDef,
    FocusUp,
    "FocusUp",
    ActionKind::FocusUp,
    "focus pane up",
    "Move focus to the pane above the current pane."
);
define_action!(
    FocusDownDef,
    FocusDown,
    "FocusDown",
    ActionKind::FocusDown,
    "focus pane down",
    "Move focus to the pane below the current pane."
);
define_action!(
    FocusNextDef,
    FocusNext,
    "FocusNext",
    ActionKind::FocusNext,
    "focus next pane",
    "Move focus to the next pane in traversal order, wrapping around."
);
define_action!(
    FocusPrevDef,
    FocusPrev,
    "FocusPrev",
    ActionKind::FocusPrev,
    "focus previous pane",
    "Move focus to the previous pane in traversal order, wrapping around."
);
define_action!(
    ClosePaneDef,
    ClosePane,
    "ClosePane",
    ActionKind::ClosePane,
    "close pane",
    "Close the focused pane. Refuses if it is the last remaining pane."
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn action_kinds() {
        assert_eq!(SplitRight.kind(), ActionKind::SplitRight);
        assert_eq!(SplitDown.kind(), ActionKind::SplitDown);
        assert_eq!(FocusLeft.kind(), ActionKind::FocusLeft);
        assert_eq!(FocusRight.kind(), ActionKind::FocusRight);
        assert_eq!(FocusUp.kind(), ActionKind::FocusUp);
        assert_eq!(FocusDown.kind(), ActionKind::FocusDown);
        assert_eq!(FocusNext.kind(), ActionKind::FocusNext);
        assert_eq!(FocusPrev.kind(), ActionKind::FocusPrev);
        assert_eq!(ClosePane.kind(), ActionKind::ClosePane);
    }

    #[test]
    fn action_names() {
        assert_eq!(SplitRight.def().name(), "SplitRight");
        assert_eq!(ClosePane.def().name(), "ClosePane");
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(SplitRight);
        assert!(action.as_any().downcast_ref::<SplitRight>().is_some());
    }
}
