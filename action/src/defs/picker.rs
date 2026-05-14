use crate::{Action, ActionDef, ActionKind, ActionPriority, ParamDef};
use std::any::Any;

#[derive(Debug)]
pub struct PickerSelectPrevDef;

impl ActionDef for PickerSelectPrevDef {
    fn name(&self) -> &'static str {
        "PickerSelectPrev"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PickerSelectPrev
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "select previous picker entry"
    }

    fn long_desc(&self) -> &'static str {
        "Move the active picker's selection up by one row. Routed to the top \
         modal when a picker is the active modal."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

#[derive(Debug)]
pub struct PickerSelectPrev;

impl PickerSelectPrev {
    pub const DEF: &PickerSelectPrevDef = &PickerSelectPrevDef;
}

impl Action for PickerSelectPrev {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PickerSelectNextDef;

impl ActionDef for PickerSelectNextDef {
    fn name(&self) -> &'static str {
        "PickerSelectNext"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PickerSelectNext
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "select next picker entry"
    }

    fn long_desc(&self) -> &'static str {
        "Move the active picker's selection down by one row. Routed to the \
         top modal when a picker is the active modal."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

#[derive(Debug)]
pub struct PickerSelectNext;

impl PickerSelectNext {
    pub const DEF: &PickerSelectNextDef = &PickerSelectNextDef;
}

impl Action for PickerSelectNext {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PickerConfirmDef;

impl ActionDef for PickerConfirmDef {
    fn name(&self) -> &'static str {
        "PickerConfirm"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PickerConfirm
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "confirm picker selection"
    }

    fn long_desc(&self) -> &'static str {
        "Confirm the active picker's current selection without a secondary \
         intent. Bound to Enter by default."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

#[derive(Debug)]
pub struct PickerConfirm;

impl PickerConfirm {
    pub const DEF: &PickerConfirmDef = &PickerConfirmDef;
}

impl Action for PickerConfirm {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PickerConfirmSplitRightDef;

impl ActionDef for PickerConfirmSplitRightDef {
    fn name(&self) -> &'static str {
        "PickerConfirmSplitRight"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PickerConfirmSplitRight
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "confirm picker into right split"
    }

    fn long_desc(&self) -> &'static str {
        "Confirm the active picker's current selection, opening the result \
         in a new split to the right of the focused pane. Bound to Ctrl-V \
         by default."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

#[derive(Debug)]
pub struct PickerConfirmSplitRight;

impl PickerConfirmSplitRight {
    pub const DEF: &PickerConfirmSplitRightDef = &PickerConfirmSplitRightDef;
}

impl Action for PickerConfirmSplitRight {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PickerConfirmSplitDownDef;

impl ActionDef for PickerConfirmSplitDownDef {
    fn name(&self) -> &'static str {
        "PickerConfirmSplitDown"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PickerConfirmSplitDown
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "confirm picker into bottom split"
    }

    fn long_desc(&self) -> &'static str {
        "Confirm the active picker's current selection, opening the result \
         in a new split below the focused pane. Bound to Ctrl-X by default."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

#[derive(Debug)]
pub struct PickerConfirmSplitDown;

impl PickerConfirmSplitDown {
    pub const DEF: &PickerConfirmSplitDownDef = &PickerConfirmSplitDownDef;
}

impl Action for PickerConfirmSplitDown {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_names() {
        assert_eq!(PickerSelectPrev.kind(), ActionKind::PickerSelectPrev);
        assert_eq!(PickerSelectPrev.def().name(), "PickerSelectPrev");
        assert_eq!(PickerSelectNext.kind(), ActionKind::PickerSelectNext);
        assert_eq!(PickerSelectNext.def().name(), "PickerSelectNext");
        assert_eq!(PickerConfirm.kind(), ActionKind::PickerConfirm);
        assert_eq!(PickerConfirm.def().name(), "PickerConfirm");
        assert_eq!(
            PickerConfirmSplitRight.kind(),
            ActionKind::PickerConfirmSplitRight
        );
        assert_eq!(
            PickerConfirmSplitRight.def().name(),
            "PickerConfirmSplitRight"
        );
        assert_eq!(
            PickerConfirmSplitDown.kind(),
            ActionKind::PickerConfirmSplitDown
        );
        assert_eq!(
            PickerConfirmSplitDown.def().name(),
            "PickerConfirmSplitDown"
        );
    }

    #[test]
    fn picker_actions_are_not_palette_visible() {
        for def in [
            PickerSelectPrev.def(),
            PickerSelectNext.def(),
            PickerConfirm.def(),
            PickerConfirmSplitRight.def(),
            PickerConfirmSplitDown.def(),
        ] {
            assert!(!def.palette_visible(), "{}", def.name());
        }
    }
}
