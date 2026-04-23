use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    AddSelectionBelowDef,
    AddSelectionBelow,
    "AddSelectionBelow",
    ActionKind::AddSelectionBelow,
    "add cursor below",
    "Add a new cursor on the line below the newest cursor.",
    ActionPriority::Rare
);

define_action!(
    MoveLeftDef,
    MoveLeft,
    "MoveLeft",
    ActionKind::MoveLeft,
    "move cursor left",
    "Move every cursor one grapheme to the left and collapse any selection.",
    ActionPriority::Rare
);

define_action!(
    MoveRightDef,
    MoveRight,
    "MoveRight",
    ActionKind::MoveRight,
    "move cursor right",
    "Move every cursor one grapheme to the right and collapse any selection.",
    ActionPriority::Rare
);

define_action!(
    MoveUpDef,
    MoveUp,
    "MoveUp",
    ActionKind::MoveUp,
    "move cursor up",
    "Move every cursor one display line up, preserving the goal column.",
    ActionPriority::Rare
);

define_action!(
    MoveDownDef,
    MoveDown,
    "MoveDown",
    ActionKind::MoveDown,
    "move cursor down",
    "Move every cursor one display line down, preserving the goal column.",
    ActionPriority::Rare
);

define_action!(
    MoveNextWordStartDef,
    MoveNextWordStart,
    "MoveNextWordStart",
    ActionKind::MoveNextWordStart,
    "select to next word start",
    "Select from each cursor head to the start of the next word.",
    ActionPriority::Rare
);

define_action!(
    MoveNextWordEndDef,
    MoveNextWordEnd,
    "MoveNextWordEnd",
    ActionKind::MoveNextWordEnd,
    "select to next word end",
    "Select from each cursor head to the end of the next word.",
    ActionPriority::Rare
);

define_action!(
    MovePrevWordStartDef,
    MovePrevWordStart,
    "MovePrevWordStart",
    ActionKind::MovePrevWordStart,
    "select to previous word start",
    "Select backward from each cursor head to the start of the previous word.",
    ActionPriority::Rare
);

define_action!(
    ExtendLeftDef,
    ExtendLeft,
    "ExtendLeft",
    ActionKind::ExtendLeft,
    "extend selection left",
    "Move every cursor head one grapheme left, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    ExtendRightDef,
    ExtendRight,
    "ExtendRight",
    ActionKind::ExtendRight,
    "extend selection right",
    "Move every cursor head one grapheme right, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    ExtendUpDef,
    ExtendUp,
    "ExtendUp",
    ActionKind::ExtendUp,
    "extend selection up",
    "Move every cursor head one display line up, keeping the tail fixed and preserving the goal column.",
    ActionPriority::Rare
);

define_action!(
    ExtendDownDef,
    ExtendDown,
    "ExtendDown",
    ActionKind::ExtendDown,
    "extend selection down",
    "Move every cursor head one display line down, keeping the tail fixed and preserving the goal column.",
    ActionPriority::Rare
);

define_action!(
    ExtendNextWordStartDef,
    ExtendNextWordStart,
    "ExtendNextWordStart",
    ActionKind::ExtendNextWordStart,
    "extend selection to next word start",
    "Extend each selection's head to the start of the next word, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    ExtendNextWordEndDef,
    ExtendNextWordEnd,
    "ExtendNextWordEnd",
    ActionKind::ExtendNextWordEnd,
    "extend selection to next word end",
    "Extend each selection's head to the end of the next word, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    ExtendPrevWordStartDef,
    ExtendPrevWordStart,
    "ExtendPrevWordStart",
    ActionKind::ExtendPrevWordStart,
    "extend selection to previous word start",
    "Extend each selection's head backward to the start of the previous word, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    GotoLineStartDef,
    GotoLineStart,
    "GotoLineStart",
    ActionKind::GotoLineStart,
    "goto line start",
    "Collapse every selection to column 0 of the line containing its cursor head.",
    ActionPriority::Rare
);

define_action!(
    GotoLineEndDef,
    GotoLineEnd,
    "GotoLineEnd",
    ActionKind::GotoLineEnd,
    "goto line end",
    "Collapse every selection to the end of the line containing its cursor head (just before the trailing newline).",
    ActionPriority::Rare
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn kind_and_name() {
        assert_eq!(AddSelectionBelow.kind(), ActionKind::AddSelectionBelow);
        assert_eq!(AddSelectionBelow.def().name(), "AddSelectionBelow");
    }

    #[test]
    fn move_kinds_and_names() {
        assert_eq!(MoveLeft.kind(), ActionKind::MoveLeft);
        assert_eq!(MoveLeft.def().name(), "MoveLeft");
        assert_eq!(MoveRight.kind(), ActionKind::MoveRight);
        assert_eq!(MoveRight.def().name(), "MoveRight");
        assert_eq!(MoveUp.kind(), ActionKind::MoveUp);
        assert_eq!(MoveUp.def().name(), "MoveUp");
        assert_eq!(MoveDown.kind(), ActionKind::MoveDown);
        assert_eq!(MoveDown.def().name(), "MoveDown");
        assert_eq!(MoveNextWordStart.kind(), ActionKind::MoveNextWordStart);
        assert_eq!(MoveNextWordStart.def().name(), "MoveNextWordStart");
        assert_eq!(MoveNextWordEnd.kind(), ActionKind::MoveNextWordEnd);
        assert_eq!(MoveNextWordEnd.def().name(), "MoveNextWordEnd");
        assert_eq!(MovePrevWordStart.kind(), ActionKind::MovePrevWordStart);
        assert_eq!(MovePrevWordStart.def().name(), "MovePrevWordStart");
    }

    #[test]
    fn extend_kinds_and_names() {
        assert_eq!(ExtendLeft.kind(), ActionKind::ExtendLeft);
        assert_eq!(ExtendLeft.def().name(), "ExtendLeft");
        assert_eq!(ExtendRight.kind(), ActionKind::ExtendRight);
        assert_eq!(ExtendRight.def().name(), "ExtendRight");
        assert_eq!(ExtendUp.kind(), ActionKind::ExtendUp);
        assert_eq!(ExtendUp.def().name(), "ExtendUp");
        assert_eq!(ExtendDown.kind(), ActionKind::ExtendDown);
        assert_eq!(ExtendDown.def().name(), "ExtendDown");
        assert_eq!(ExtendNextWordStart.kind(), ActionKind::ExtendNextWordStart);
        assert_eq!(ExtendNextWordStart.def().name(), "ExtendNextWordStart");
        assert_eq!(ExtendNextWordEnd.kind(), ActionKind::ExtendNextWordEnd);
        assert_eq!(ExtendNextWordEnd.def().name(), "ExtendNextWordEnd");
        assert_eq!(ExtendPrevWordStart.kind(), ActionKind::ExtendPrevWordStart);
        assert_eq!(ExtendPrevWordStart.def().name(), "ExtendPrevWordStart");
    }

    #[test]
    fn goto_kinds_and_names() {
        assert_eq!(GotoLineStart.kind(), ActionKind::GotoLineStart);
        assert_eq!(GotoLineStart.def().name(), "GotoLineStart");
        assert_eq!(GotoLineEnd.kind(), ActionKind::GotoLineEnd);
        assert_eq!(GotoLineEnd.def().name(), "GotoLineEnd");
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(AddSelectionBelow);
        assert!(action
            .as_any()
            .downcast_ref::<AddSelectionBelow>()
            .is_some());
        let action: Box<dyn Action> = Box::new(MoveLeft);
        assert!(action.as_any().downcast_ref::<MoveLeft>().is_some());
        let action: Box<dyn Action> = Box::new(ExtendLeft);
        assert!(action.as_any().downcast_ref::<ExtendLeft>().is_some());
    }
}
