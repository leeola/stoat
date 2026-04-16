mod action;
pub mod defs;
mod kind;
mod param;
pub mod registry;

pub use action::{Action, ActionDef};
pub use defs::{
    app::Quit,
    claude::{
        ClaudeSubmit, ClaudeToDockLeft, ClaudeToDockRight, ClaudeToPane, OpenClaude,
        ToggleDockLeft, ToggleDockRight,
    },
    editor::{
        AddSelectionBelow, MoveDown, MoveLeft, MoveNextWordEnd, MoveNextWordStart,
        MovePrevWordStart, MoveRight, MoveUp,
    },
    file::OpenFile,
    palette::OpenCommandPalette,
    pane::{
        ClosePane, FocusDown, FocusLeft, FocusNext, FocusPrev, FocusRight, FocusUp, SplitDown,
        SplitRight,
    },
    review::{
        JumpToMoveSource, JumpToMoveTarget, JumpToNextMoveSource, JumpToPrevMoveSource, OpenReview,
        QueryMoveRelationships,
    },
    run::{OpenRun, Run, RunInterrupt, RunSubmit},
};
pub use kind::ActionKind;
pub use param::{ParamDef, ParamError, ParamKind, ParamValue};
