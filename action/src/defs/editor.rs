use crate::{action::define_action, ActionKind};

define_action!(
    AddSelectionBelowDef,
    AddSelectionBelow,
    "AddSelectionBelow",
    ActionKind::AddSelectionBelow,
    "add cursor below",
    "Add a new cursor on the line below the newest cursor."
);

define_action!(
    MoveLeftDef,
    MoveLeft,
    "MoveLeft",
    ActionKind::MoveLeft,
    "move cursor left",
    "Move every cursor one grapheme to the left and collapse any selection."
);

define_action!(
    MoveRightDef,
    MoveRight,
    "MoveRight",
    ActionKind::MoveRight,
    "move cursor right",
    "Move every cursor one grapheme to the right and collapse any selection."
);

define_action!(
    MoveUpDef,
    MoveUp,
    "MoveUp",
    ActionKind::MoveUp,
    "move cursor up",
    "Move every cursor one display line up, preserving the goal column."
);

define_action!(
    MoveDownDef,
    MoveDown,
    "MoveDown",
    ActionKind::MoveDown,
    "move cursor down",
    "Move every cursor one display line down, preserving the goal column."
);

define_action!(
    MoveNextWordStartDef,
    MoveNextWordStart,
    "MoveNextWordStart",
    ActionKind::MoveNextWordStart,
    "select to next word start",
    "Select from each cursor head to the start of the next word."
);

define_action!(
    MoveNextWordEndDef,
    MoveNextWordEnd,
    "MoveNextWordEnd",
    ActionKind::MoveNextWordEnd,
    "select to next word end",
    "Select from each cursor head to the end of the next word."
);

define_action!(
    MovePrevWordStartDef,
    MovePrevWordStart,
    "MovePrevWordStart",
    ActionKind::MovePrevWordStart,
    "select to previous word start",
    "Select backward from each cursor head to the start of the previous word."
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
    fn downcast() {
        let action: Box<dyn Action> = Box::new(AddSelectionBelow);
        assert!(action
            .as_any()
            .downcast_ref::<AddSelectionBelow>()
            .is_some());
        let action: Box<dyn Action> = Box::new(MoveLeft);
        assert!(action.as_any().downcast_ref::<MoveLeft>().is_some());
    }
}
