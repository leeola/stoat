mod action;
pub mod defs;
mod kind;
mod param;
pub mod registry;

pub use action::{Action, ActionDef, ActionPriority};
pub use defs::{
    app::{Quit, QuitAll},
    claude::{
        ClaudeSubmit, ClaudeToDockLeft, ClaudeToDockRight, ClaudeToPane, ClaudeToggleFollow,
        OpenClaude, ToggleDockLeft, ToggleDockRight,
    },
    commits::{
        CloseCommits, CommitsFirst, CommitsLast, CommitsNext, CommitsOpenReview, CommitsPageDown,
        CommitsPageUp, CommitsPrev, CommitsRefresh, OpenCommits,
    },
    dump::Dump,
    editor::{
        AddSelectionBelow, AlignSelections, AlignViewBottom, AlignViewCenter, AlignViewTop,
        CollapseSelection, Decrement, DeleteSelection, ExpandSelection, ExtendDown,
        ExtendFindNextChar, ExtendFindPrevChar, ExtendGotoColumn, ExtendGotoFileStart,
        ExtendGotoFirstNonwhitespace, ExtendGotoLastLine, ExtendGotoWindowBottom,
        ExtendGotoWindowCenter, ExtendGotoWindowTop, ExtendLeft, ExtendMoveParentNodeEnd,
        ExtendMoveParentNodeStart, ExtendNextWordEnd, ExtendNextWordStart, ExtendPrevWordEnd,
        ExtendPrevWordStart, ExtendRight, ExtendSelectNextSibling, ExtendSelectPrevSibling,
        ExtendTillNextChar, ExtendTillPrevChar, ExtendToFileStart, ExtendToLastLine,
        ExtendToLineEnd, ExtendToLineStart, ExtendUp, FindNextChar, FindPrevChar, FlipSelections,
        GotoColumn, GotoFileStart, GotoFirstNonwhitespace, GotoLastLine, GotoLineEnd,
        GotoLineNumber, GotoLineStart, GotoMark, GotoMarkExact, GotoNextChange, GotoNextParagraph,
        GotoPrevChange, GotoPrevParagraph, GotoWindowBottom, GotoWindowCenter, GotoWindowTop,
        GotoWord, HalfPageDown, HalfPageUp, Increment, IndentSelection, InsertRegister,
        JumpBackward, JumpForward, KeepPrimarySelection, MatchBrackets, MoveDown, MoveLeft,
        MoveNextWordEnd, MoveNextWordStart, MoveParentNodeEnd, MoveParentNodeStart,
        MovePrevWordEnd, MovePrevWordStart, MoveRight, MoveUp, OpenAbove, OpenBelow,
        OpenGlobalSearch, OpenJumplistPicker, OpenReverseSearchInput, OpenSearchInput, PageDown,
        PageUp, PasteAfter, PasteBefore, PasteClipboardAfter, PasteClipboardBefore, Redo,
        RemovePrimarySelection, RepeatLastMotion, ReplaceChar, RotateSelectionsBackward,
        RotateSelectionsForward, SaveSelection, ScrollDown, ScrollUp, SearchNext, SearchPrev,
        SelectAll, SelectLineBelow, SelectNextSibling, SelectPrevSibling, SelectRegister, SetMark,
        ShrinkSelection, SplitSelection, SurroundAdd, SurroundDelete, SurroundReplace, SwitchCase,
        SwitchToLowercase, SwitchToUppercase, TillNextChar, TillPrevChar, ToggleComments,
        TrimSelections, Undo, UnindentSelection, Yank, YankMainToClipboard, YankToClipboard,
    },
    file::OpenFile,
    file_finder::{
        FileFinderScopeToggle, FileFinderSelectNext, FileFinderSelectPrev, OpenBufferPicker,
        OpenChangedFilePicker, OpenFileFinder, OpenFileFinderHSplit, OpenFileFinderVSplit,
    },
    help::{
        CloseHelp, HelpJumpFirst, HelpJumpLast, HelpScopeToggle, HelpScrollDetailDown,
        HelpScrollDetailUp, HelpSelectNext, HelpSelectPrev, OpenHelp,
    },
    lsp::{
        CodeAction, FormatSelections, GotoDefinition, GotoImplementation, GotoNextDiagnostic,
        GotoPrevDiagnostic, GotoTypeDefinition, Hover, OpenSymbolPicker, OpenWorkspaceSymbolPicker,
        RenameSymbol,
    },
    palette::OpenCommandPalette,
    pane::{
        ClosePane, FocusDown, FocusLeft, FocusNext, FocusPrev, FocusRight, FocusUp, SplitDown,
        SplitNewDown, SplitNewRight, SplitRight,
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
    workspace::{CloseWorkspace, CopyWorkspace, NewWorkspace, RenameWorkspace, SwitchWorkspace},
};
pub use kind::ActionKind;
pub use param::{ParamDef, ParamError, ParamKind, ParamValue};
