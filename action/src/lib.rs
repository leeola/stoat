mod action;
pub mod defs;
mod kind;
mod param;
pub mod registry;

pub use action::{Action, ActionDef};
pub use defs::{
    app::Quit,
    file::OpenFile,
    palette::OpenCommandPalette,
    pane::{
        ClosePane, FocusDown, FocusLeft, FocusNext, FocusPrev, FocusRight, FocusUp, SplitDown,
        SplitRight,
    },
};
pub use kind::ActionKind;
pub use param::{ParamDef, ParamError, ParamKind, ParamValue};
