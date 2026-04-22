mod action;
pub mod defs;
mod kind;
mod param;
pub mod registry;

pub use action::{Action, ActionDef, ActionPriority};
pub use defs::{
    app::{Quit, QuitAll},
    claude::{
        ClaudeSubmit, ClaudeToDockLeft, ClaudeToDockRight, ClaudeToPane, OpenClaude,
        ToggleDockLeft, ToggleDockRight,
    },
    commits::{
        CloseCommits, CommitsFirst, CommitsLast, CommitsNext, CommitsOpenReview, CommitsPageDown,
        CommitsPageUp, CommitsPrev, CommitsRefresh, OpenCommits,
    },
    dump::Dump,
    editor::{
        AddSelectionBelow, MoveDown, MoveLeft, MoveNextWordEnd, MoveNextWordStart,
        MovePrevWordStart, MoveRight, MoveUp,
    },
    file::OpenFile,
    help::{
        CloseHelp, HelpJumpFirst, HelpJumpLast, HelpScopeToggle, HelpScrollDetailDown,
        HelpScrollDetailUp, HelpSelectNext, HelpSelectPrev, OpenHelp,
    },
    palette::OpenCommandPalette,
    pane::{
        ClosePane, FocusDown, FocusLeft, FocusNext, FocusPrev, FocusRight, FocusUp, SplitDown,
        SplitRight,
    },
    prompt::{
        CancelPromptInput, PaletteScopeToggle, PaletteSelectNext, PaletteSelectPrev,
        PromptInsertNewline, SubmitPromptInput,
    },
    rebase::{
        AbortRebase, ConflictAbort, ConflictApply, ConflictNextFile, ConflictPrevFile,
        ConflictSkipEntry, ConflictTakeOurs, ConflictTakeTheirs, EnterRebase, ExecuteRebase,
        RebaseContinue, RebaseMoveDown, RebaseMoveUp, RebaseNext, RebasePrev, RewordAbort,
        RewordConfirm, SetRebaseOpDrop, SetRebaseOpEdit, SetRebaseOpFixup, SetRebaseOpPick,
        SetRebaseOpReword, SetRebaseOpSquash,
    },
    review::{
        AgentEdit, CloseReview, JumpToMoveSource, JumpToMoveTarget, JumpToNextMoveSource,
        JumpToPrevMoveSource, OpenReview, OpenReviewAgentEdits, OpenReviewCommit,
        OpenReviewCommitRange, QueryMoveRelationships, ReviewApplyStaged, ReviewNextChunk,
        ReviewPrevChunk, ReviewRefresh, ReviewRemoveSelected, ReviewSkipChunk, ReviewStageChunk,
        ReviewToggleStage, ReviewUnstageChunk,
    },
    run::{OpenRun, Run, RunHistoryNext, RunHistoryPrev, RunInterrupt, RunSubmit},
    workspace::{CloseWorkspace, CopyWorkspace, NewWorkspace, SwitchWorkspace},
};
pub use kind::ActionKind;
pub use param::{ParamDef, ParamError, ParamKind, ParamValue};
