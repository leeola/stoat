use crate::{
    action::define_action, Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind,
    ValueSource,
};
use std::any::Any;

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
    DetachPaneDef,
    DetachPane,
    "DetachPane",
    ActionKind::DetachPane,
    "detach pane into its own window",
    "Detach the focused pane into a separate stoatty OS window. Requires running under stoatty; the last split pane cannot detach."
);
define_action!(
    ReattachPaneDef,
    ReattachPane,
    "ReattachPane",
    ActionKind::ReattachPane,
    "reattach a detached pane",
    "Reattach the focused detached pane back into the split layout, closing its window."
);

const FOCUS_PANE_PARAMS: &[ParamDef] = &[ParamDef {
    name: "index",
    kind: ParamKind::Number,
    value_source: ValueSource::None,
    required: true,
    description: "1-based pane position in layout order. 10 addresses the pane displayed as 0.",
}];

#[derive(Debug)]
pub struct FocusPaneDef;

impl ActionDef for FocusPaneDef {
    fn name(&self) -> &'static str {
        "FocusPane"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::FocusPane
    }

    fn params(&self) -> &'static [ParamDef] {
        FOCUS_PANE_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "focus pane by number"
    }

    fn long_desc(&self) -> &'static str {
        "Move focus to the pane at the given 1-based position in layout order, the same order pane-ID badges number panes. Out-of-range indices leave focus unchanged."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

#[derive(Debug)]
pub struct FocusPane {
    pub index: usize,
}

impl FocusPane {
    pub const DEF: &FocusPaneDef = &FocusPaneDef;
}

impl Action for FocusPane {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

define_action!(
    ClosePaneDef,
    ClosePane,
    "ClosePane",
    ActionKind::ClosePane,
    "close pane",
    "Close the focused pane. Refuses if it is the last remaining pane."
);
define_action!(
    CloseOtherPanesDef,
    CloseOtherPanes,
    "CloseOtherPanes",
    ActionKind::CloseOtherPanes,
    "close other panes",
    "Close every split pane except the focused one. No-op when the focused pane is the only one."
);

define_action!(
    ToggleDockRightDef,
    ToggleDockRight,
    "ToggleDockRight",
    ActionKind::ToggleDockRight,
    "toggle right dock",
    "Cycle the right dock panel through visible, minimized, and hidden states.",
    ActionPriority::Rare
);

define_action!(
    ToggleDockLeftDef,
    ToggleDockLeft,
    "ToggleDockLeft",
    ActionKind::ToggleDockLeft,
    "toggle left dock",
    "Cycle the left dock panel through visible, minimized, and hidden states.",
    ActionPriority::Rare
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

    #[test]
    fn focus_pane_carries_index() {
        let action = FocusPane { index: 3 };
        assert_eq!(action.kind(), ActionKind::FocusPane);
        assert_eq!(action.def().name(), "FocusPane");
        assert_eq!(action.def().params().len(), 1);
        assert_eq!(action.def().params()[0].name, "index");

        let boxed: Box<dyn Action> = Box::new(FocusPane { index: 7 });
        let recovered = boxed
            .as_any()
            .downcast_ref::<FocusPane>()
            .expect("downcast");
        assert_eq!(recovered.index, 7);
    }
}
