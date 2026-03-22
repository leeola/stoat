mod action;
mod kind;
pub mod pane;
mod param;

pub use action::{Action, ActionDef, Quit};
pub use kind::ActionKind;
pub use pane::{
    ClosePane, FocusDown, FocusLeft, FocusNext, FocusPrev, FocusRight, FocusUp, SplitDown,
    SplitRight,
};
pub use param::{ParamDef, ParamKind, ParamValue};
