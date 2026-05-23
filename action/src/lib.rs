mod action;
pub mod defs;
mod kind;
mod param;
pub mod registry;

pub use action::{Action, ActionDef, ActionPriority};
pub use defs::{
    app::{DismissModal, Quit, QuitAll},
    claude::{
        ClaudeFocusNextToolCard, ClaudeFocusPrevToolCard, ClaudeInterrupt, ClaudeJumpToFocusedCard,
        ClaudeSubmit, ClaudeToDockLeft, ClaudeToDockRight, ClaudeToPane, ClaudeToggleFollow,
        ClaudeToggleToolCardExpand, OpenCheckpointPicker, OpenClaude, ToggleDockLeft,
        ToggleDockRight,
    },
    commits::{
        CloseCommits, CommitsFirst, CommitsLast, CommitsNext, CommitsOpenReview, CommitsPageDown,
        CommitsPageUp, CommitsPrev, CommitsRefresh, OpenCommits,
    },
    dump::Dump,
    editor::{
        AcceptCompletion, AddSelectionBelow, AlignSelections, AlignViewBottom, AlignViewCenter,
        AlignViewTop, CloseBuffer, CollapseSelection, Decrement, DeleteSelection, ExpandSelection,
        ExtendDown, ExtendFindNextChar, ExtendFindPrevChar, ExtendGotoColumn, ExtendGotoFileStart,
        ExtendGotoFirstNonwhitespace, ExtendGotoLastLine, ExtendGotoWindowBottom,
        ExtendGotoWindowCenter, ExtendGotoWindowTop, ExtendLeft, ExtendMoveParentNodeEnd,
        ExtendMoveParentNodeStart, ExtendNextWordEnd, ExtendNextWordStart, ExtendPrevWordEnd,
        ExtendPrevWordStart, ExtendRight, ExtendSelectNextSibling, ExtendSelectPrevSibling,
        ExtendTillNextChar, ExtendTillPrevChar, ExtendToFileStart, ExtendToLastLine,
        ExtendToLineEnd, ExtendToLineStart, ExtendUp, FindNextChar, FindPrevChar, FlipSelections,
        GotoColumn, GotoFileStart, GotoFirstNonwhitespace, GotoLastLine, GotoLineEnd,
        GotoLineNumber, GotoLineStart, GotoMark, GotoMarkExact, GotoNextChange, GotoNextClass,
        GotoNextFunction, GotoNextParagraph, GotoPrevChange, GotoPrevClass, GotoPrevFunction,
        GotoPrevParagraph, GotoWindowBottom, GotoWindowCenter, GotoWindowTop, GotoWord,
        HalfPageDown, HalfPageUp, Increment, IndentSelection, InsertRegister, JumpBackward,
        JumpForward, KeepPrimarySelection, KeepSelections, MatchBrackets, MoveDown, MoveLeft,
        MoveNextWordEnd, MoveNextWordStart, MoveParentNodeEnd, MoveParentNodeStart,
        MovePrevWordEnd, MovePrevWordStart, MoveRight, MoveUp, OpenAbove, OpenBelow,
        OpenGlobalSearch, OpenJumplistPicker, OpenLastPicker, OpenReverseSearchInput,
        OpenSearchInput, PageDown, PageUp, PasteAfter, PasteBefore, PasteClipboardAfter,
        PasteClipboardBefore, RecordMacro, Redo, RemovePrimarySelection, RemoveSelections,
        RepeatLastMotion, ReplaceChar, ReplayMacro, RotateSelectionsBackward,
        RotateSelectionsForward, SaveBuffer, SaveSelection, ScrollDown, ScrollUp, SearchNext,
        SearchPrev, SelectAll, SelectAllChildren, SelectAllSiblings, SelectLineBelow,
        SelectNextSibling, SelectPrevSibling, SelectRegister, SelectTextobjectAround,
        SelectTextobjectInner, SetMark, ShellAppendOutput, ShellInsertOutput, ShellKeepPipe,
        ShellPipe, ShellPipeTo, ShrinkSelection, SmartTab, SplitSelection, SurroundAdd,
        SurroundDelete, SurroundReplace, SwitchCase, SwitchToLowercase, SwitchToUppercase,
        TillNextChar, TillPrevChar, ToggleBlame, ToggleComments, ToggleDiffHunkPanel,
        ToggleMinimap, TriggerCompletion, TrimSelections, Undo, UnindentSelection, Yank,
        YankMainToClipboard, YankToClipboard,
    },
    file::OpenFile,
    file_finder::{
        FileFinderScopeToggle, FileFinderSelectNext, FileFinderSelectPrev, OpenBufferPicker,
        OpenChangedFilePicker, OpenFileFinder, OpenFileFinderHSplit, OpenFileFinderVSplit,
        OpenGitStatus,
    },
    help::{
        CloseHelp, HelpJumpFirst, HelpJumpLast, HelpScopeToggle, HelpScrollDetailDown,
        HelpScrollDetailUp, HelpSelectNext, HelpSelectPrev, OpenHelp,
    },
    lsp::{
        CodeAction, FormatSelections, GotoDefinition, GotoImplementation, GotoNextDiagnostic,
        GotoPrevDiagnostic, GotoTypeDefinition, Hover, OpenDiagnosticsPicker, OpenSymbolPicker,
        OpenWorkspaceDiagnosticsPicker, OpenWorkspaceSymbolPicker, RenameSymbol,
    },
    palette::OpenCommandPalette,
    pane::{
        ClosePane, FocusDown, FocusLeft, FocusNext, FocusPrev, FocusRight, FocusUp, SplitDown,
        SplitNewDown, SplitNewRight, SplitRight,
    },
    picker::{
        PickerConfirm, PickerConfirmSplitDown, PickerConfirmSplitRight, PickerSelectNext,
        PickerSelectPrev,
    },
    project_tree::{
        ProjectTreeCollapse, ProjectTreeConfirm, ProjectTreeExpand, ProjectTreeRefresh,
        ProjectTreeSelectNext, ProjectTreeSelectPrev,
    },
    prompt::{
        CancelPromptInput, PaletteScopeToggle, PaletteSelectNext, PaletteSelectPrev,
        PromptInsertNewline, ShellInputSubmit, SubmitPromptInput,
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
        OpenReviewCommitRange, QueryMoveRelationships, ReviewApplyStaged, ReviewExternalEdit,
        ReviewNextChunk, ReviewPrevChunk, ReviewRefresh, ReviewRemoveSelected, ReviewSkipChunk,
        ReviewStageChunk, ReviewToggleStage, ReviewUnstageChunk,
    },
    run::{OpenRun, Run, RunHistoryNext, RunHistoryPrev, RunInterrupt, RunSubmit},
    workspace::{
        CloseWorkspace, CopyWorkspace, NewWorkspace, OpenWorkspacePicker, RenameWorkspace,
        SwitchWorkspace, ToggleProjectTree,
    },
};
pub use kind::ActionKind;
pub use param::{ParamDef, ParamError, ParamKind, ParamValue};
#[doc(hidden)]
pub use serde_json;
