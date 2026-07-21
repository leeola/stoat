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
    conflict::{
        CloseConflict, Conflict, ConflictApply, ConflictNextChunk, ConflictNextFile,
        ConflictPickBoth, ConflictPickOurs, ConflictPickOursLine, ConflictPickTheirs,
        ConflictPickTheirsLine, ConflictPrevChunk, ConflictPrevFile, ConflictResetChunk,
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
        MovePrevWordStart, MoveRight, MoveUp, OpenAbove, OpenBelow, OpenCodeSearch,
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
        AutoReload, AutoReloadConfig, ForceSaveBuffer, OpenBuffer, OpenConfig, OpenFile,
        ToggleMinimap, ToggleWrap,
    },
    file_finder::{
        FileFinderComplete, FileFinderPageDown, FileFinderPageUp, FileFinderScopeToggle,
        FileFinderSelectNext, FileFinderSelectPrev, OpenBufferPicker, OpenChangedFilePicker,
        OpenFileFinder, OpenFileFinderHSplit, OpenFileFinderVSplit, OpenWorkspaceFileFinder,
    },
    help::{
        CloseHelp, DismissKeyHints, HelpComplete, HelpJumpFirst, HelpJumpLast, HelpScopeToggle,
        HelpScrollDetailDown, HelpScrollDetailUp, HelpSelectNext, HelpSelectPrev, OpenHelp,
        ToggleKeyHints,
    },
    lsp::{
        CodeAction, Format, FormatSelections, GotoDeclaration, GotoDefinition, GotoImplementation,
        GotoNextDiagnostic, GotoPrevDiagnostic, GotoTypeDefinition, Hover, OpenDiagnosticsPicker,
        OpenSymbolPicker, OpenWorkspaceDiagnosticsPicker, OpenWorkspaceSymbolPicker, RenameSymbol,
        SymbolFinderComplete, SymbolFinderPageDown, SymbolFinderPageUp, SymbolFinderSelectNext,
        SymbolFinderSelectPrev,
    },
    palette::OpenCommandPalette,
    pane::{
        ClosePane, FocusDown, FocusLeft, FocusNext, FocusPane, FocusPrev, FocusRight, FocusUp,
        SplitDown, SplitNewDown, SplitNewRight, SplitRight, ToggleDockLeft, ToggleDockRight,
    },
    prompt::{
        CancelPromptInput, PaletteComplete, PaletteHistoryNext, PaletteHistoryPrev,
        PalettePageDown, PalettePageUp, PaletteScopeToggle, PaletteSelectNext, PaletteSelectPrev,
        PromptInsertNewline, SubmitPromptInput,
    },
    rebase::{
        AbortRebase, EnterRebase, ExecuteRebase, RebaseConflictAbort, RebaseConflictApply,
        RebaseConflictNextFile, RebaseConflictPrevFile, RebaseConflictSkipEntry,
        RebaseConflictTakeOurs, RebaseConflictTakeTheirs, RebaseContinue, RebaseMoveDown,
        RebaseMoveUp, RebaseNext, RebasePrev, RewordAbort, RewordConfirm, SetRebaseOpDrop,
        SetRebaseOpEdit, SetRebaseOpFixup, SetRebaseOpPick, SetRebaseOpReword, SetRebaseOpSquash,
    },
    review::{
        AgentEdit, CloseReview, Diff, JumpToMoveSource, JumpToMoveTarget, JumpToNextMoveSource,
        JumpToPrevMoveSource, OpenReviewAgentEdits, OpenReviewCommit, OpenReviewCommitRange,
        QueryMoveRelationships, ReviewApplyStaged, ReviewExternalEdit, ReviewNextChunk,
        ReviewPrevChunk, ReviewRefresh, ReviewRemoveSelected, ReviewSkipChunk, ReviewStageChunk,
        ReviewToggleStage, ReviewUnstageChunk, StageHunk, StageLine, ToggleDiff, ToggleStageHunk,
        ToggleStageLine, UnstageHunk, UnstageLine,
    },
    run::{OpenRun, Run, RunHistoryNext, RunHistoryPrev, RunInterrupt, RunSubmit},
    set_theme::SetTheme,
    tab::{CloseTab, GotoTab, NewTab, ToggleTab},
    terminal::Terminal,
    workspace::{
        CloseWorkspace, CopyWorkspace, NewWorkspace, ReloadEnv, RenameWorkspace, SetCwd, ShowCwd,
        SwitchWorkspace,
    },
};
pub use kind::ActionKind;
pub use param::{ParamDef, ParamError, ParamKind, ParamValue, ValueSource};
