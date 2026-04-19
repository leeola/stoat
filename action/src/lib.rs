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
    commits::{
        CloseCommits, CommitsFirst, CommitsLast, CommitsNext, CommitsOpenReview, CommitsPageDown,
        CommitsPageUp, CommitsPrev, CommitsRefresh, OpenCommits,
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
    rebase::{
        AbortRebase, EnterRebase, ExecuteRebase, RebaseMoveDown, RebaseMoveUp, RebaseNext,
        RebasePrev, SetRebaseOpDrop, SetRebaseOpFixup, SetRebaseOpPick, SetRebaseOpSquash,
    },
    review::{
        AgentEdit, CloseReview, JumpToMoveSource, JumpToMoveTarget, JumpToNextMoveSource,
        JumpToPrevMoveSource, OpenReview, OpenReviewAgentEdits, OpenReviewCommit,
        OpenReviewCommitRange, QueryMoveRelationships, ReviewApplyStaged, ReviewNextChunk,
        ReviewPrevChunk, ReviewRefresh, ReviewRemoveSelected, ReviewSkipChunk, ReviewStageChunk,
        ReviewToggleStage, ReviewUnstageChunk,
    },
    run::{OpenRun, Run, RunInterrupt, RunSubmit},
};
pub use kind::ActionKind;
pub use param::{ParamDef, ParamError, ParamKind, ParamValue};
