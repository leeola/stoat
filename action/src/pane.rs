use crate::{ActionDef, ActionKind, ParamDef};
use std::any::Any;

macro_rules! define_action {
    ($def:ident, $action:ident, $name:expr, $kind:expr) => {
        #[derive(Debug)]
        pub struct $def;

        impl ActionDef for $def {
            fn name(&self) -> &'static str {
                $name
            }

            fn kind(&self) -> ActionKind {
                $kind
            }

            fn params(&self) -> &'static [ParamDef] {
                &[]
            }
        }

        #[derive(Debug)]
        pub struct $action;

        impl $action {
            pub const DEF: &$def = &$def;
        }

        impl crate::Action for $action {
            fn def(&self) -> &'static dyn ActionDef {
                Self::DEF
            }

            fn as_any(&self) -> &dyn Any {
                self
            }
        }
    };
}

define_action!(
    SplitRightDef,
    SplitRight,
    "SplitRight",
    ActionKind::SplitRight
);
define_action!(SplitDownDef, SplitDown, "SplitDown", ActionKind::SplitDown);
define_action!(FocusLeftDef, FocusLeft, "FocusLeft", ActionKind::FocusLeft);
define_action!(
    FocusRightDef,
    FocusRight,
    "FocusRight",
    ActionKind::FocusRight
);
define_action!(FocusUpDef, FocusUp, "FocusUp", ActionKind::FocusUp);
define_action!(FocusDownDef, FocusDown, "FocusDown", ActionKind::FocusDown);
define_action!(FocusNextDef, FocusNext, "FocusNext", ActionKind::FocusNext);
define_action!(FocusPrevDef, FocusPrev, "FocusPrev", ActionKind::FocusPrev);
define_action!(ClosePaneDef, ClosePane, "ClosePane", ActionKind::ClosePane);

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
