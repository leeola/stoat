mod action;
pub mod defs;
mod kind;
mod param;
pub mod registry;

pub use action::{Action, ActionDef, ActionPriority};
pub use defs::{
    app::{OpenLogs, Quit, QuitAll, ShowVersion},
    commits::{
        CloseCommits, CommitsFirst, CommitsLast, CommitsNext, CommitsOpenReview, CommitsPageDown,
        CommitsPageUp, CommitsPrev, CommitsRefresh, OpenCommits,
    },
    dump::Dump,
    editor::{
        AcceptCompletion, AddSelectionBelow, AlignSelections, AlignViewBottom, AlignViewCenter,
        AlignViewTop, ChangeSelection, CloseBuffer, CollapseSelection, Decrement, DeleteSelection,
        DeleteSelectionNoYank, EnsureSelectionsForward, ExpandSelection, ExtendDown,
        ExtendFindNextChar, ExtendFindPrevChar, ExtendGotoColumn, ExtendGotoFileStart,
        ExtendGotoFirstNonwhitespace, ExtendGotoLastLine, ExtendGotoWindowBottom,
        ExtendGotoWindowCenter, ExtendGotoWindowTop, ExtendLeft, ExtendMoveParentNodeEnd,
        ExtendMoveParentNodeStart, ExtendNextWordEnd, ExtendNextWordStart, ExtendPrevWordEnd,
        ExtendPrevWordStart, ExtendRight, ExtendSelectNextSibling, ExtendSelectPrevSibling,
        ExtendTillNextChar, ExtendTillPrevChar, ExtendToFileStart, ExtendToLastLine,
        ExtendToLineBounds, ExtendToLineEnd, ExtendToLineStart, ExtendUp, FindNextChar,
        FindPrevChar, FlipSelections, GotoColumn, GotoFileStart, GotoFirstNonwhitespace,
        GotoLastLine, GotoLineEnd, GotoLineNumber, GotoLineStart, GotoMark, GotoMarkExact,
        GotoNextChange, GotoNextClass, GotoNextFunction, GotoNextParagraph, GotoPrevChange,
        GotoPrevClass, GotoPrevFunction, GotoPrevParagraph, GotoWindowBottom, GotoWindowCenter,
        GotoWindowTop, GotoWord, HalfPageDown, HalfPageUp, Increment, IndentSelection,
        InsertRegister, JoinSelections, JoinSelectionsSpace, JumpBackward, JumpForward,
        KeepPrimarySelection, KeepSelections, MatchBrackets, MoveDown, MoveLeft, MoveNextWordEnd,
        MoveNextWordStart, MoveParentNodeEnd, MoveParentNodeStart, MovePrevWordEnd,
        MovePrevWordStart, MoveRight, MoveUp, OpenAbove, OpenBelow, OpenGlobalSearch,
        OpenJumplistPicker, OpenLastPicker, OpenReverseSearchInput, OpenSearchInput, PageDown,
        PageUp, PasteAfter, PasteBefore, PasteClipboardAfter, PasteClipboardBefore, RecordMacro,
        Redo, RemovePrimarySelection, RemoveSelections, RepeatLastMotion, ReplaceChar,
        ReplaceWithYanked, ReplayMacro, RotateSelectionContentsBackward,
        RotateSelectionContentsForward, RotateSelectionsBackward, RotateSelectionsForward,
        SaveBuffer, SaveSelection, ScrollDown, ScrollUp, SearchNext, SearchPrev, SelectAll,
        SelectAllChildren, SelectAllSiblings, SelectLineBelow, SelectNextSibling,
        SelectPrevSibling, SelectRegex, SelectRegister, SelectTextobjectAround,
        SelectTextobjectInner, SetMark, ShellAppendOutput, ShellInsertOutput, ShellKeepPipe,
        ShellPipe, ShellPipeTo, ShrinkSelection, ShrinkToLineBounds, SmartTab, SplitSelection,
        SurroundAdd, SurroundDelete, SurroundReplace, SwitchCase, SwitchToLowercase,
        SwitchToUppercase, TillNextChar, TillPrevChar, ToggleComments, ToggleInlayHints,
        ToggleLspStatus, ToggleSyntaxHighlight, TriggerCompletion, TrimSelections, Undo,
        UnindentSelection, WriteQuit, Yank, YankMainToClipboard, YankToClipboard,
    },
    file::{
        AutoReload, ForceSaveBuffer, OpenBuffer, OpenConfig, OpenFile, ToggleMinimap, ToggleWrap,
    },
    file_finder::{
        FileFinderPageDown, FileFinderPageUp, FileFinderScopeToggle, FileFinderSelectNext,
        FileFinderSelectPrev, OpenBufferPicker, OpenChangedFilePicker, OpenFileFinder,
        OpenFileFinderHSplit, OpenFileFinderVSplit,
    },
    help::{
        CloseHelp, DismissKeyHints, HelpJumpFirst, HelpJumpLast, HelpScopeToggle,
        HelpScrollDetailDown, HelpScrollDetailUp, HelpSelectNext, HelpSelectPrev, OpenHelp,
        ToggleKeyHints,
    },
    lsp::{
        CodeAction, Format, FormatSelections, GotoDeclaration, GotoDefinition, GotoImplementation,
        GotoNextDiagnostic, GotoPrevDiagnostic, GotoTypeDefinition, Hover, OpenDiagnosticsPicker,
        OpenSymbolPicker, OpenWorkspaceDiagnosticsPicker, OpenWorkspaceSymbolPicker, RenameSymbol,
    },
    palette::OpenCommandPalette,
    pane::{
        ClosePane, FocusDown, FocusLeft, FocusNext, FocusPane, FocusPrev, FocusRight, FocusUp,
        SplitDown, SplitNewDown, SplitNewRight, SplitRight, ToggleDockLeft, ToggleDockRight,
    },
    prompt::{
        CancelPromptInput, PaletteCompletePath, PaletteHistoryNext, PaletteHistoryPrev,
        PalettePageDown, PalettePageUp, PaletteScopeToggle, PaletteSelectNext, PaletteSelectPrev,
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
        AgentEdit, CloseReview, Diff, JumpToMoveSource, JumpToMoveTarget, JumpToNextMoveSource,
        JumpToPrevMoveSource, OpenReviewAgentEdits, OpenReviewCommit, OpenReviewCommitRange,
        QueryMoveRelationships, ReviewApplyStaged, ReviewExternalEdit, ReviewNextChunk,
        ReviewPrevChunk, ReviewRefresh, ReviewRemoveSelected, ReviewSkipChunk, ReviewStageChunk,
        ReviewToggleStage, ReviewUnstageChunk, StageHunk, ToggleDiff, ToggleStageHunk, UnstageHunk,
    },
    run::{OpenRun, Run, RunHistoryNext, RunHistoryPrev, RunInterrupt, RunSubmit},
    set_theme::SetTheme,
    terminal::Terminal,
    workspace::{
        CloseWorkspace, CopyWorkspace, NewWorkspace, ReloadEnv, RenameWorkspace, SetCwd, ShowCwd,
        SwitchWorkspace,
    },
};
pub use kind::ActionKind;
pub use param::{ParamDef, ParamError, ParamKind, ParamValue, ValueSource};
