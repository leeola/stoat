use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    SplitRightDef,
    SplitRight,
    "SplitRight",
    ActionKind::SplitRight,
    "split pane right",
    "Split the focused pane vertically, creating a new pane to the right.",
    ActionPriority::Common
);
define_action!(
    SplitDownDef,
    SplitDown,
    "SplitDown",
    ActionKind::SplitDown,
    "split pane down",
    "Split the focused pane horizontally, creating a new pane below.",
    ActionPriority::Common
);
define_action!(
    SplitNewRightDef,
    SplitNewRight,
    "SplitNewRight",
    ActionKind::SplitNewRight,
    "split pane right with new buffer",
    "Split the focused pane vertically, opening a new empty scratch buffer in the new pane.",
    ActionPriority::Common
);
define_action!(
    SplitNewDownDef,
    SplitNewDown,
    "SplitNewDown",
    ActionKind::SplitNewDown,
    "split pane down with new buffer",
    "Split the focused pane horizontally, opening a new empty scratch buffer in the new pane.",
    ActionPriority::Common
);
define_action!(
    FocusLeftDef,
    FocusLeft,
    "FocusLeft",
    ActionKind::FocusLeft,
    "focus pane left",
    "Move focus to the pane to the left of the current pane.",
    ActionPriority::Normal
);
define_action!(
    FocusRightDef,
    FocusRight,
    "FocusRight",
    ActionKind::FocusRight,
    "focus pane right",
    "Move focus to the pane to the right of the current pane.",
    ActionPriority::Normal
);
define_action!(
    FocusUpDef,
    FocusUp,
    "FocusUp",
    ActionKind::FocusUp,
    "focus pane up",
    "Move focus to the pane above the current pane.",
    ActionPriority::Normal
);
define_action!(
    FocusDownDef,
    FocusDown,
    "FocusDown",
    ActionKind::FocusDown,
    "focus pane down",
    "Move focus to the pane below the current pane.",
    ActionPriority::Normal
);
define_action!(
    FocusNextDef,
    FocusNext,
    "FocusNext",
    ActionKind::FocusNext,
    "focus next pane",
    "Move focus to the next pane in traversal order, wrapping around.",
    ActionPriority::Normal
);
define_action!(
    FocusPrevDef,
    FocusPrev,
    "FocusPrev",
    ActionKind::FocusPrev,
    "focus previous pane",
    "Move focus to the previous pane in traversal order, wrapping around.",
    ActionPriority::Normal
);
define_action!(
    ClosePaneDef,
    ClosePane,
    "ClosePane",
    ActionKind::ClosePane,
    "close pane",
    "Close the focused pane. Refuses if it is the last remaining pane.",
    ActionPriority::Normal
);
define_action!(
    CloseOtherPanesDef,
    CloseOtherPanes,
    "CloseOtherPanes",
    ActionKind::CloseOtherPanes,
    "close other panes",
    "Close every split pane except the focused one. No-op when the focused pane is the only one.",
    ActionPriority::Normal
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn action_kinds() {
        assert_eq!(SplitRight.kind(), ActionKind::SplitRight);
        assert_eq!(SplitDown.kind(), ActionKind::SplitDown);
        assert_eq!(SplitNewRight.kind(), ActionKind::SplitNewRight);
        assert_eq!(SplitNewDown.kind(), ActionKind::SplitNewDown);
        assert_eq!(FocusLeft.kind(), ActionKind::FocusLeft);
        assert_eq!(FocusRight.kind(), ActionKind::FocusRight);
        assert_eq!(FocusUp.kind(), ActionKind::FocusUp);
        assert_eq!(FocusDown.kind(), ActionKind::FocusDown);
        assert_eq!(FocusNext.kind(), ActionKind::FocusNext);
        assert_eq!(FocusPrev.kind(), ActionKind::FocusPrev);
        assert_eq!(ClosePane.kind(), ActionKind::ClosePane);
        assert_eq!(CloseOtherPanes.kind(), ActionKind::CloseOtherPanes);
    }

    #[test]
    fn action_names() {
        assert_eq!(SplitRight.def().name(), "SplitRight");
        assert_eq!(ClosePane.def().name(), "ClosePane");
        assert_eq!(CloseOtherPanes.def().name(), "CloseOtherPanes");
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(SplitRight);
        assert!(action.as_any().downcast_ref::<SplitRight>().is_some());
    }
}
