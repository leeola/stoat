use crate::{
    defs::{
        app::{DismissModal, Quit, QuitAll},
        claude::{
            ClaudeFocusNextToolCard, ClaudeFocusPrevToolCard, ClaudeJumpToFocusedCard,
            ClaudeSubmit, ClaudeToDockLeft, ClaudeToDockRight, ClaudeToPane, ClaudeToggleFollow,
            ClaudeToggleToolCardExpand, OpenClaude, ToggleDockLeft, ToggleDockRight,
        },
        commits::{
            CloseCommits, CommitsFirst, CommitsLast, CommitsNext, CommitsOpenReview,
            CommitsPageDown, CommitsPageUp, CommitsPrev, CommitsRefresh, OpenCommits,
        },
        dump::Dump,
        editor::{
            AcceptCompletion, AddSelectionAbove, AddSelectionBelow, AlignSelections,
            AlignViewBottom, AlignViewCenter, AlignViewTop, Append, CloseBuffer, CollapseSelection,
            CommitUndoCheckpoint, Decrement, DeleteBackward, DeleteForward, DeleteSelection,
            DeleteWordBackward, DeleteWordForward, ExpandSelection, ExtendDown, ExtendFindNextChar,
            ExtendFindPrevChar, ExtendGotoColumn, ExtendGotoFileStart,
            ExtendGotoFirstNonwhitespace, ExtendGotoLastLine, ExtendGotoWindowBottom,
            ExtendGotoWindowCenter, ExtendGotoWindowTop, ExtendLeft, ExtendMoveParentNodeEnd,
            ExtendMoveParentNodeStart, ExtendNextWordEnd, ExtendNextWordStart, ExtendPrevWordEnd,
            ExtendPrevWordStart, ExtendRight, ExtendSelectNextSibling, ExtendSelectPrevSibling,
            ExtendTillNextChar, ExtendTillPrevChar, ExtendToFileStart, ExtendToLastLine,
            ExtendToLineEnd, ExtendToLineStart, ExtendUp, FindNextChar, FindPrevChar,
            FlipSelections, GotoColumn, GotoFileStart, GotoFirstNonwhitespace, GotoLastLine,
            GotoLineEnd, GotoLineNumber, GotoLineStart, GotoMark, GotoMarkExact, GotoNextClass,
            GotoNextFunction, GotoNextHunk, GotoNextParagraph, GotoPrevClass, GotoPrevFunction,
            GotoPrevHunk, GotoPrevParagraph, GotoWindowBottom, GotoWindowCenter, GotoWindowTop,
            GotoWord, HalfPageDown, HalfPageUp, Increment, IndentSelection, Insert, InsertNewline,
            InsertRegister, JumpBackward, JumpForward, KeepPrimarySelection, KeepSelections,
            MatchBrackets, MoveDown, MoveLeft, MoveNextLongWordEnd, MoveNextLongWordStart,
            MoveNextWordEnd, MoveNextWordStart, MoveParentNodeEnd, MoveParentNodeStart,
            MovePrevLongWordEnd, MovePrevLongWordStart, MovePrevWordEnd, MovePrevWordStart,
            MoveRight, MoveUp, OpenAbove, OpenBelow, OpenGlobalSearch, OpenJumplistPicker,
            OpenLastPicker, OpenReverseSearchInput, OpenSearchInput, PageDown, PageUp, PasteAfter,
            PasteBefore, PasteClipboardAfter, PasteClipboardBefore, RecordMacro, Redo,
            RemovePrimarySelection, RemoveSelections, RepeatLastMotion, ReplaceChar, ReplayMacro,
            RotateSelectionsBackward, RotateSelectionsForward, SaveBuffer, SaveSelection,
            ScrollDown, ScrollUp, SearchNext, SearchPrev, SelectAll, SelectAllChildren,
            SelectAllSiblings, SelectLineBelow, SelectNextSibling, SelectPrevSibling,
            SelectRegister, SelectTextobjectAround, SelectTextobjectInner, SetMark,
            ShellAppendOutput, ShellInsertOutput, ShellKeepPipe, ShellPipe, ShellPipeTo,
            ShrinkSelection, SmartTab, SplitSelection, SplitSelectionOnNewline, SurroundAdd,
            SurroundDelete, SurroundReplace, SwitchCase, SwitchToLowercase, SwitchToUppercase,
            TillNextChar, TillPrevChar, ToggleBlame, ToggleComments, ToggleDiffHunkPanel,
            ToggleMinimap, ToggleTabBar, TriggerCompletion, TrimSelections, Undo,
            UnindentSelection, Yank, YankMainToClipboard, YankToClipboard,
        },
        file::OpenFile,
        file_finder::{
            FileFinderScopeToggle, FileFinderSelectNext, FileFinderSelectPrev, OpenBufferPicker,
            OpenChangedFilePicker, OpenConflictPicker, OpenFileFinder, OpenFileFinderHSplit,
            OpenFileFinderVSplit, OpenGitStatus,
        },
        help::{
            CloseHelp, HelpJumpFirst, HelpJumpLast, HelpScopeToggle, HelpScrollDetailDown,
            HelpScrollDetailUp, HelpSelectNext, HelpSelectPrev, OpenAbout, OpenHelp,
        },
        lsp::{
            CodeAction, FormatSelections, GotoDefinition, GotoImplementation, GotoNextDiagnostic,
            GotoPrevDiagnostic, GotoTypeDefinition, Hover, OpenDiagnosticsPicker, OpenSymbolPicker,
            OpenWorkspaceDiagnosticsPicker, OpenWorkspaceSymbolPicker, RenameSymbol,
        },
        palette::OpenCommandPalette,
        pane::{
            CloseOtherPanes, ClosePane, FocusDown, FocusLeft, FocusNext, FocusPrev, FocusRight,
            FocusUp, SplitDown, SplitNewDown, SplitNewRight, SplitRight,
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
            CloseReview, GitToggleStageHunk, GitToggleStageLine, GitUnstageHunk, JumpToMoveSource,
            JumpToMoveTarget, JumpToNextMoveSource, JumpToPrevMoveSource, OpenReview,
            OpenReviewCommit, OpenReviewCommitRange, OpenReviewWatch, QueryMoveRelationships,
            ReviewApplyStaged, ReviewApproveHunk, ReviewCycleComparisonMode, ReviewEnterLineSelect,
            ReviewLineSelectAll, ReviewLineSelectCancel, ReviewLineSelectStage,
            ReviewLineSelectToggle, ReviewLineSelectUnstage, ReviewNextChunk,
            ReviewNextUnreviewedHunk, ReviewPrevChunk, ReviewRefresh, ReviewRemoveSelected,
            ReviewResetProgress, ReviewRevertHunk, ReviewSkipChunk, ReviewStageChunk,
            ReviewToggleApproval, ReviewToggleFollow, ReviewToggleStage, ReviewUnstageChunk,
        },
        run::{OpenRun, Run, RunHistoryNext, RunHistoryPrev, RunInterrupt, RunSubmit},
        set::Set,
        workspace::{
            CloseWorkspace, CopyWorkspace, Env, NewWorkspace, OpenWorkspacePicker, Pwd,
            RenameWorkspace, SetCwd, SwitchWorkspace, ToggleProjectTree,
        },
    },
    param::{MissingSnafu, WrongKindSnafu},
    Action, ActionDef, ParamError, ParamKind, ParamValue,
};
use snafu::OptionExt;
use std::{collections::HashMap, path::PathBuf, sync::OnceLock};

pub type CreateFn = fn(&[ParamValue]) -> Result<Box<dyn Action>, ParamError>;

pub struct RegistryEntry {
    pub def: &'static dyn ActionDef,
    pub create: CreateFn,
}

static REGISTRY: OnceLock<HashMap<&'static str, RegistryEntry>> = OnceLock::new();

fn init() -> HashMap<&'static str, RegistryEntry> {
    let mut map = HashMap::with_capacity(16);
    let mut add = |def: &'static dyn ActionDef, create: CreateFn| {
        map.insert(def.name(), RegistryEntry { def, create });
    };

    add(Quit::DEF, |_| Ok(Box::new(Quit)));
    add(QuitAll::DEF, |_| Ok(Box::new(QuitAll)));
    add(DismissModal::DEF, |_| Ok(Box::new(DismissModal)));
    add(SplitRight::DEF, |_| Ok(Box::new(SplitRight)));
    add(SplitDown::DEF, |_| Ok(Box::new(SplitDown)));
    add(SplitNewRight::DEF, |_| Ok(Box::new(SplitNewRight)));
    add(SplitNewDown::DEF, |_| Ok(Box::new(SplitNewDown)));
    add(FocusLeft::DEF, |_| Ok(Box::new(FocusLeft)));
    add(FocusRight::DEF, |_| Ok(Box::new(FocusRight)));
    add(FocusUp::DEF, |_| Ok(Box::new(FocusUp)));
    add(FocusDown::DEF, |_| Ok(Box::new(FocusDown)));
    add(FocusNext::DEF, |_| Ok(Box::new(FocusNext)));
    add(FocusPrev::DEF, |_| Ok(Box::new(FocusPrev)));
    add(ClosePane::DEF, |_| Ok(Box::new(ClosePane)));
    add(CloseOtherPanes::DEF, |_| Ok(Box::new(CloseOtherPanes)));
    add(OpenCommandPalette::DEF, |_| {
        Ok(Box::new(OpenCommandPalette))
    });
    add(OpenFileFinder::DEF, |_| Ok(Box::new(OpenFileFinder)));
    add(OpenFileFinderHSplit::DEF, |_| {
        Ok(Box::new(OpenFileFinderHSplit))
    });
    add(OpenFileFinderVSplit::DEF, |_| {
        Ok(Box::new(OpenFileFinderVSplit))
    });
    add(OpenChangedFilePicker::DEF, |_| {
        Ok(Box::new(OpenChangedFilePicker))
    });
    add(OpenGitStatus::DEF, |_| Ok(Box::new(OpenGitStatus)));
    add(OpenConflictPicker::DEF, |_| {
        Ok(Box::new(OpenConflictPicker))
    });
    add(OpenBufferPicker::DEF, |_| Ok(Box::new(OpenBufferPicker)));
    add(FileFinderSelectPrev::DEF, |_| {
        Ok(Box::new(FileFinderSelectPrev))
    });
    add(FileFinderSelectNext::DEF, |_| {
        Ok(Box::new(FileFinderSelectNext))
    });
    add(FileFinderScopeToggle::DEF, |_| {
        Ok(Box::new(FileFinderScopeToggle))
    });
    add(OpenHelp::DEF, |_| Ok(Box::new(OpenHelp)));
    add(OpenAbout::DEF, |_| Ok(Box::new(OpenAbout)));
    add(OpenReview::DEF, |_| Ok(Box::new(OpenReview)));
    add(JumpToMoveSource::DEF, |_| Ok(Box::new(JumpToMoveSource)));
    add(JumpToMoveTarget::DEF, |_| Ok(Box::new(JumpToMoveTarget)));
    add(JumpToNextMoveSource::DEF, |_| {
        Ok(Box::new(JumpToNextMoveSource))
    });
    add(JumpToPrevMoveSource::DEF, |_| {
        Ok(Box::new(JumpToPrevMoveSource))
    });
    add(QueryMoveRelationships::DEF, |_| {
        Ok(Box::new(QueryMoveRelationships))
    });
    add(GotoNextDiagnostic::DEF, |_| {
        Ok(Box::new(GotoNextDiagnostic))
    });
    add(GotoPrevDiagnostic::DEF, |_| {
        Ok(Box::new(GotoPrevDiagnostic))
    });
    add(GotoDefinition::DEF, |_| Ok(Box::new(GotoDefinition)));
    add(GotoTypeDefinition::DEF, |_| {
        Ok(Box::new(GotoTypeDefinition))
    });
    add(GotoImplementation::DEF, |_| {
        Ok(Box::new(GotoImplementation))
    });
    add(Hover::DEF, |_| Ok(Box::new(Hover)));
    add(CodeAction::DEF, |_| Ok(Box::new(CodeAction)));
    add(RenameSymbol::DEF, |_| Ok(Box::new(RenameSymbol)));
    add(OpenSymbolPicker::DEF, |_| Ok(Box::new(OpenSymbolPicker)));
    add(OpenWorkspaceSymbolPicker::DEF, |_| {
        Ok(Box::new(OpenWorkspaceSymbolPicker))
    });
    add(FormatSelections::DEF, |_| Ok(Box::new(FormatSelections)));
    add(ReviewNextChunk::DEF, |_| Ok(Box::new(ReviewNextChunk)));
    add(ReviewPrevChunk::DEF, |_| Ok(Box::new(ReviewPrevChunk)));
    add(ReviewStageChunk::DEF, |_| Ok(Box::new(ReviewStageChunk)));
    add(ReviewUnstageChunk::DEF, |_| {
        Ok(Box::new(ReviewUnstageChunk))
    });
    add(ReviewToggleStage::DEF, |_| Ok(Box::new(ReviewToggleStage)));
    add(ReviewSkipChunk::DEF, |_| Ok(Box::new(ReviewSkipChunk)));
    add(ReviewApproveHunk::DEF, |_| Ok(Box::new(ReviewApproveHunk)));
    add(ReviewToggleApproval::DEF, |_| {
        Ok(Box::new(ReviewToggleApproval))
    });
    add(ReviewNextUnreviewedHunk::DEF, |_| {
        Ok(Box::new(ReviewNextUnreviewedHunk))
    });
    add(ReviewResetProgress::DEF, |_| {
        Ok(Box::new(ReviewResetProgress))
    });
    add(ReviewEnterLineSelect::DEF, |_| {
        Ok(Box::new(ReviewEnterLineSelect))
    });
    add(ReviewLineSelectCancel::DEF, |_| {
        Ok(Box::new(ReviewLineSelectCancel))
    });
    add(ReviewLineSelectToggle::DEF, |_| {
        Ok(Box::new(ReviewLineSelectToggle))
    });
    add(ReviewLineSelectAll::DEF, |_| {
        Ok(Box::new(ReviewLineSelectAll))
    });
    add(ReviewLineSelectStage::DEF, |_| {
        Ok(Box::new(ReviewLineSelectStage))
    });
    add(ReviewLineSelectUnstage::DEF, |_| {
        Ok(Box::new(ReviewLineSelectUnstage))
    });
    add(GitToggleStageHunk::DEF, |_| {
        Ok(Box::new(GitToggleStageHunk))
    });
    add(GitUnstageHunk::DEF, |_| Ok(Box::new(GitUnstageHunk)));
    add(GitToggleStageLine::DEF, |_| {
        Ok(Box::new(GitToggleStageLine))
    });
    add(ReviewRevertHunk::DEF, |_| Ok(Box::new(ReviewRevertHunk)));
    add(ReviewCycleComparisonMode::DEF, |_| {
        Ok(Box::new(ReviewCycleComparisonMode))
    });
    add(ReviewToggleFollow::DEF, |_| {
        Ok(Box::new(ReviewToggleFollow))
    });
    add(ReviewRefresh::DEF, |_| Ok(Box::new(ReviewRefresh)));
    add(ReviewApplyStaged::DEF, |_| Ok(Box::new(ReviewApplyStaged)));
    add(CloseReview::DEF, |_| Ok(Box::new(CloseReview)));
    add(ReviewRemoveSelected::DEF, |_| {
        Ok(Box::new(ReviewRemoveSelected))
    });
    add(OpenReviewCommit::DEF, |params| {
        let workdir = params
            .first()
            .context(MissingSnafu { name: "workdir" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "workdir",
                expected: ParamKind::String,
            })?;
        let sha = params
            .get(1)
            .context(MissingSnafu { name: "sha" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "sha",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(OpenReviewCommit {
            workdir: PathBuf::from(workdir),
            sha: sha.to_owned(),
        }))
    });
    add(OpenReviewWatch::DEF, |params| {
        let workdir = params
            .first()
            .context(MissingSnafu { name: "workdir" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "workdir",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(OpenReviewWatch {
            workdir: PathBuf::from(workdir),
        }))
    });
    add(OpenReviewCommitRange::DEF, |params| {
        let workdir = params
            .first()
            .context(MissingSnafu { name: "workdir" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "workdir",
                expected: ParamKind::String,
            })?;
        let from = params
            .get(1)
            .context(MissingSnafu { name: "from" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "from",
                expected: ParamKind::String,
            })?;
        let to = params
            .get(2)
            .context(MissingSnafu { name: "to" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "to",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(OpenReviewCommitRange {
            workdir: PathBuf::from(workdir),
            from: from.to_owned(),
            to: to.to_owned(),
        }))
    });
    add(AddSelectionBelow::DEF, |_| Ok(Box::new(AddSelectionBelow)));
    add(AddSelectionAbove::DEF, |_| Ok(Box::new(AddSelectionAbove)));
    add(MoveLeft::DEF, |_| Ok(Box::new(MoveLeft)));
    add(MoveRight::DEF, |_| Ok(Box::new(MoveRight)));
    add(MoveUp::DEF, |_| Ok(Box::new(MoveUp)));
    add(MoveDown::DEF, |_| Ok(Box::new(MoveDown)));
    add(PageUp::DEF, |_| Ok(Box::new(PageUp)));
    add(PageDown::DEF, |_| Ok(Box::new(PageDown)));
    add(HalfPageUp::DEF, |_| Ok(Box::new(HalfPageUp)));
    add(HalfPageDown::DEF, |_| Ok(Box::new(HalfPageDown)));
    add(MoveNextWordStart::DEF, |_| Ok(Box::new(MoveNextWordStart)));
    add(MoveNextWordEnd::DEF, |_| Ok(Box::new(MoveNextWordEnd)));
    add(MovePrevWordStart::DEF, |_| Ok(Box::new(MovePrevWordStart)));
    add(MovePrevWordEnd::DEF, |_| Ok(Box::new(MovePrevWordEnd)));
    add(MoveNextLongWordStart::DEF, |_| {
        Ok(Box::new(MoveNextLongWordStart))
    });
    add(MoveNextLongWordEnd::DEF, |_| {
        Ok(Box::new(MoveNextLongWordEnd))
    });
    add(MovePrevLongWordStart::DEF, |_| {
        Ok(Box::new(MovePrevLongWordStart))
    });
    add(MovePrevLongWordEnd::DEF, |_| {
        Ok(Box::new(MovePrevLongWordEnd))
    });
    add(ExtendLeft::DEF, |_| Ok(Box::new(ExtendLeft)));
    add(ExtendRight::DEF, |_| Ok(Box::new(ExtendRight)));
    add(ExtendUp::DEF, |_| Ok(Box::new(ExtendUp)));
    add(ExtendDown::DEF, |_| Ok(Box::new(ExtendDown)));
    add(ExtendNextWordStart::DEF, |_| {
        Ok(Box::new(ExtendNextWordStart))
    });
    add(ExtendNextWordEnd::DEF, |_| Ok(Box::new(ExtendNextWordEnd)));
    add(ExpandSelection::DEF, |_| Ok(Box::new(ExpandSelection)));
    add(ShrinkSelection::DEF, |_| Ok(Box::new(ShrinkSelection)));
    add(SelectNextSibling::DEF, |_| Ok(Box::new(SelectNextSibling)));
    add(SelectPrevSibling::DEF, |_| Ok(Box::new(SelectPrevSibling)));
    add(SelectAllSiblings::DEF, |_| Ok(Box::new(SelectAllSiblings)));
    add(SelectAllChildren::DEF, |_| Ok(Box::new(SelectAllChildren)));
    add(ExtendSelectNextSibling::DEF, |_| {
        Ok(Box::new(ExtendSelectNextSibling))
    });
    add(ExtendSelectPrevSibling::DEF, |_| {
        Ok(Box::new(ExtendSelectPrevSibling))
    });
    add(MoveParentNodeStart::DEF, |_| {
        Ok(Box::new(MoveParentNodeStart))
    });
    add(MoveParentNodeEnd::DEF, |_| Ok(Box::new(MoveParentNodeEnd)));
    add(ExtendMoveParentNodeStart::DEF, |_| {
        Ok(Box::new(ExtendMoveParentNodeStart))
    });
    add(ExtendMoveParentNodeEnd::DEF, |_| {
        Ok(Box::new(ExtendMoveParentNodeEnd))
    });
    add(SaveSelection::DEF, |_| Ok(Box::new(SaveSelection)));
    add(JumpBackward::DEF, |_| Ok(Box::new(JumpBackward)));
    add(JumpForward::DEF, |_| Ok(Box::new(JumpForward)));
    add(OpenJumplistPicker::DEF, |_| {
        Ok(Box::new(OpenJumplistPicker))
    });
    add(OpenLastPicker::DEF, |_| Ok(Box::new(OpenLastPicker)));
    add(OpenDiagnosticsPicker::DEF, |_| {
        Ok(Box::new(OpenDiagnosticsPicker))
    });
    add(OpenWorkspaceDiagnosticsPicker::DEF, |_| {
        Ok(Box::new(OpenWorkspaceDiagnosticsPicker))
    });
    add(OpenGlobalSearch::DEF, |_| Ok(Box::new(OpenGlobalSearch)));
    add(SplitSelection::DEF, |_| Ok(Box::new(SplitSelection)));
    add(KeepSelections::DEF, |_| Ok(Box::new(KeepSelections)));
    add(RemoveSelections::DEF, |_| Ok(Box::new(RemoveSelections)));
    add(RecordMacro::DEF, |_| Ok(Box::new(RecordMacro)));
    add(ReplayMacro::DEF, |_| Ok(Box::new(ReplayMacro)));
    add(ShellPipe::DEF, |_| Ok(Box::new(ShellPipe)));
    add(ShellPipeTo::DEF, |_| Ok(Box::new(ShellPipeTo)));
    add(ShellInsertOutput::DEF, |_| Ok(Box::new(ShellInsertOutput)));
    add(ShellAppendOutput::DEF, |_| Ok(Box::new(ShellAppendOutput)));
    add(ShellKeepPipe::DEF, |_| Ok(Box::new(ShellKeepPipe)));
    add(SaveBuffer::DEF, |_| Ok(Box::new(SaveBuffer)));
    add(CloseBuffer::DEF, |_| Ok(Box::new(CloseBuffer)));
    add(AcceptCompletion::DEF, |_| Ok(Box::new(AcceptCompletion)));
    add(SmartTab::DEF, |_| Ok(Box::new(SmartTab)));
    add(TriggerCompletion::DEF, |_| Ok(Box::new(TriggerCompletion)));
    add(FindNextChar::DEF, |_| Ok(Box::new(FindNextChar)));
    add(FindPrevChar::DEF, |_| Ok(Box::new(FindPrevChar)));
    add(TillNextChar::DEF, |_| Ok(Box::new(TillNextChar)));
    add(TillPrevChar::DEF, |_| Ok(Box::new(TillPrevChar)));
    add(ExtendFindNextChar::DEF, |_| {
        Ok(Box::new(ExtendFindNextChar))
    });
    add(ExtendFindPrevChar::DEF, |_| {
        Ok(Box::new(ExtendFindPrevChar))
    });
    add(ExtendTillNextChar::DEF, |_| {
        Ok(Box::new(ExtendTillNextChar))
    });
    add(ExtendTillPrevChar::DEF, |_| {
        Ok(Box::new(ExtendTillPrevChar))
    });
    add(SetMark::DEF, |_| Ok(Box::new(SetMark)));
    add(GotoMark::DEF, |_| Ok(Box::new(GotoMark)));
    add(GotoMarkExact::DEF, |_| Ok(Box::new(GotoMarkExact)));
    add(SurroundAdd::DEF, |_| Ok(Box::new(SurroundAdd)));
    add(SurroundReplace::DEF, |_| Ok(Box::new(SurroundReplace)));
    add(SurroundDelete::DEF, |_| Ok(Box::new(SurroundDelete)));
    add(SelectTextobjectAround::DEF, |_| {
        Ok(Box::new(SelectTextobjectAround))
    });
    add(SelectTextobjectInner::DEF, |_| {
        Ok(Box::new(SelectTextobjectInner))
    });
    add(OpenSearchInput::DEF, |_| Ok(Box::new(OpenSearchInput)));
    add(OpenReverseSearchInput::DEF, |_| {
        Ok(Box::new(OpenReverseSearchInput))
    });
    add(SearchNext::DEF, |_| Ok(Box::new(SearchNext)));
    add(SearchPrev::DEF, |_| Ok(Box::new(SearchPrev)));
    add(Yank::DEF, |_| Ok(Box::new(Yank)));
    add(PasteAfter::DEF, |_| Ok(Box::new(PasteAfter)));
    add(PasteBefore::DEF, |_| Ok(Box::new(PasteBefore)));
    add(YankToClipboard::DEF, |_| Ok(Box::new(YankToClipboard)));
    add(YankMainToClipboard::DEF, |_| {
        Ok(Box::new(YankMainToClipboard))
    });
    add(PasteClipboardAfter::DEF, |_| {
        Ok(Box::new(PasteClipboardAfter))
    });
    add(PasteClipboardBefore::DEF, |_| {
        Ok(Box::new(PasteClipboardBefore))
    });
    add(SelectRegister::DEF, |_| Ok(Box::new(SelectRegister)));
    add(InsertRegister::DEF, |_| Ok(Box::new(InsertRegister)));
    add(RepeatLastMotion::DEF, |_| Ok(Box::new(RepeatLastMotion)));
    add(ExtendPrevWordStart::DEF, |_| {
        Ok(Box::new(ExtendPrevWordStart))
    });
    add(ExtendPrevWordEnd::DEF, |_| Ok(Box::new(ExtendPrevWordEnd)));
    add(GotoLineStart::DEF, |_| Ok(Box::new(GotoLineStart)));
    add(GotoLineEnd::DEF, |_| Ok(Box::new(GotoLineEnd)));
    add(GotoFirstNonwhitespace::DEF, |_| {
        Ok(Box::new(GotoFirstNonwhitespace))
    });
    add(OpenBelow::DEF, |_| Ok(Box::new(OpenBelow)));
    add(OpenAbove::DEF, |_| Ok(Box::new(OpenAbove)));
    add(ReplaceChar::DEF, |_| Ok(Box::new(ReplaceChar)));
    add(GotoFileStart::DEF, |_| Ok(Box::new(GotoFileStart)));
    add(GotoLastLine::DEF, |_| Ok(Box::new(GotoLastLine)));
    add(GotoLineNumber::DEF, |_| Ok(Box::new(GotoLineNumber)));
    add(GotoColumn::DEF, |_| Ok(Box::new(GotoColumn)));
    add(ExtendGotoColumn::DEF, |_| Ok(Box::new(ExtendGotoColumn)));
    add(GotoNextHunk::DEF, |_| Ok(Box::new(GotoNextHunk)));
    add(GotoPrevHunk::DEF, |_| Ok(Box::new(GotoPrevHunk)));
    add(GotoNextParagraph::DEF, |_| Ok(Box::new(GotoNextParagraph)));
    add(GotoPrevParagraph::DEF, |_| Ok(Box::new(GotoPrevParagraph)));
    add(GotoNextFunction::DEF, |_| Ok(Box::new(GotoNextFunction)));
    add(GotoPrevFunction::DEF, |_| Ok(Box::new(GotoPrevFunction)));
    add(GotoNextClass::DEF, |_| Ok(Box::new(GotoNextClass)));
    add(GotoPrevClass::DEF, |_| Ok(Box::new(GotoPrevClass)));
    add(MatchBrackets::DEF, |_| Ok(Box::new(MatchBrackets)));
    add(GotoWindowTop::DEF, |_| Ok(Box::new(GotoWindowTop)));
    add(GotoWindowCenter::DEF, |_| Ok(Box::new(GotoWindowCenter)));
    add(GotoWindowBottom::DEF, |_| Ok(Box::new(GotoWindowBottom)));
    add(GotoWord::DEF, |_| Ok(Box::new(GotoWord)));
    add(ExtendGotoFirstNonwhitespace::DEF, |_| {
        Ok(Box::new(ExtendGotoFirstNonwhitespace))
    });
    add(ExtendGotoFileStart::DEF, |_| {
        Ok(Box::new(ExtendGotoFileStart))
    });
    add(ExtendGotoLastLine::DEF, |_| {
        Ok(Box::new(ExtendGotoLastLine))
    });
    add(ExtendGotoWindowTop::DEF, |_| {
        Ok(Box::new(ExtendGotoWindowTop))
    });
    add(ExtendGotoWindowCenter::DEF, |_| {
        Ok(Box::new(ExtendGotoWindowCenter))
    });
    add(ExtendGotoWindowBottom::DEF, |_| {
        Ok(Box::new(ExtendGotoWindowBottom))
    });
    add(AlignViewTop::DEF, |_| Ok(Box::new(AlignViewTop)));
    add(AlignViewCenter::DEF, |_| Ok(Box::new(AlignViewCenter)));
    add(AlignViewBottom::DEF, |_| Ok(Box::new(AlignViewBottom)));
    add(ScrollUp::DEF, |_| Ok(Box::new(ScrollUp)));
    add(ScrollDown::DEF, |_| Ok(Box::new(ScrollDown)));
    add(SwitchCase::DEF, |_| Ok(Box::new(SwitchCase)));
    add(SwitchToUppercase::DEF, |_| Ok(Box::new(SwitchToUppercase)));
    add(SwitchToLowercase::DEF, |_| Ok(Box::new(SwitchToLowercase)));
    add(Increment::DEF, |_| Ok(Box::new(Increment)));
    add(Decrement::DEF, |_| Ok(Box::new(Decrement)));
    add(DeleteSelection::DEF, |_| Ok(Box::new(DeleteSelection)));
    add(DeleteForward::DEF, |_| Ok(Box::new(DeleteForward)));
    add(DeleteBackward::DEF, |_| Ok(Box::new(DeleteBackward)));
    add(DeleteWordForward::DEF, |_| Ok(Box::new(DeleteWordForward)));
    add(DeleteWordBackward::DEF, |_| {
        Ok(Box::new(DeleteWordBackward))
    });
    add(Insert::DEF, |_| Ok(Box::new(Insert)));
    add(Append::DEF, |_| Ok(Box::new(Append)));
    add(InsertNewline::DEF, |_| Ok(Box::new(InsertNewline)));
    add(Undo::DEF, |_| Ok(Box::new(Undo)));
    add(Redo::DEF, |_| Ok(Box::new(Redo)));
    add(CommitUndoCheckpoint::DEF, |_| {
        Ok(Box::new(CommitUndoCheckpoint))
    });
    add(IndentSelection::DEF, |_| Ok(Box::new(IndentSelection)));
    add(UnindentSelection::DEF, |_| Ok(Box::new(UnindentSelection)));
    add(ToggleComments::DEF, |_| Ok(Box::new(ToggleComments)));
    add(ToggleBlame::DEF, |_| Ok(Box::new(ToggleBlame)));
    add(ToggleMinimap::DEF, |_| Ok(Box::new(ToggleMinimap)));
    add(ToggleTabBar::DEF, |_| Ok(Box::new(ToggleTabBar)));
    add(Set::DEF, |params| {
        let key = params
            .first()
            .context(MissingSnafu { name: "key" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "key",
                expected: ParamKind::String,
            })?
            .to_string();
        let value = params
            .get(1)
            .context(MissingSnafu { name: "value" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "value",
                expected: ParamKind::String,
            })?
            .to_string();
        Ok(Box::new(Set { key, value }))
    });
    add(ToggleDiffHunkPanel::DEF, |_| {
        Ok(Box::new(ToggleDiffHunkPanel))
    });
    add(ExtendToLineStart::DEF, |_| Ok(Box::new(ExtendToLineStart)));
    add(ExtendToLineEnd::DEF, |_| Ok(Box::new(ExtendToLineEnd)));
    add(ExtendToFileStart::DEF, |_| Ok(Box::new(ExtendToFileStart)));
    add(ExtendToLastLine::DEF, |_| Ok(Box::new(ExtendToLastLine)));
    add(CollapseSelection::DEF, |_| Ok(Box::new(CollapseSelection)));
    add(FlipSelections::DEF, |_| Ok(Box::new(FlipSelections)));
    add(SelectAll::DEF, |_| Ok(Box::new(SelectAll)));
    add(SelectLineBelow::DEF, |_| Ok(Box::new(SelectLineBelow)));
    add(KeepPrimarySelection::DEF, |_| {
        Ok(Box::new(KeepPrimarySelection))
    });
    add(RemovePrimarySelection::DEF, |_| {
        Ok(Box::new(RemovePrimarySelection))
    });
    add(RotateSelectionsForward::DEF, |_| {
        Ok(Box::new(RotateSelectionsForward))
    });
    add(RotateSelectionsBackward::DEF, |_| {
        Ok(Box::new(RotateSelectionsBackward))
    });
    add(TrimSelections::DEF, |_| Ok(Box::new(TrimSelections)));
    add(SplitSelectionOnNewline::DEF, |_| {
        Ok(Box::new(SplitSelectionOnNewline))
    });
    add(AlignSelections::DEF, |_| Ok(Box::new(AlignSelections)));
    add(OpenFile::DEF, |params| {
        let raw = params
            .first()
            .context(MissingSnafu { name: "path" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "path",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(OpenFile {
            path: PathBuf::from(raw),
        }))
    });
    add(OpenRun::DEF, |_| Ok(Box::new(OpenRun)));
    add(RunSubmit::DEF, |_| Ok(Box::new(RunSubmit)));
    add(RunInterrupt::DEF, |_| Ok(Box::new(RunInterrupt)));
    add(RunHistoryPrev::DEF, |_| Ok(Box::new(RunHistoryPrev)));
    add(RunHistoryNext::DEF, |_| Ok(Box::new(RunHistoryNext)));
    add(HelpSelectPrev::DEF, |_| Ok(Box::new(HelpSelectPrev)));
    add(HelpSelectNext::DEF, |_| Ok(Box::new(HelpSelectNext)));
    add(HelpScopeToggle::DEF, |_| Ok(Box::new(HelpScopeToggle)));
    add(HelpScrollDetailUp::DEF, |_| {
        Ok(Box::new(HelpScrollDetailUp))
    });
    add(HelpScrollDetailDown::DEF, |_| {
        Ok(Box::new(HelpScrollDetailDown))
    });
    add(HelpJumpFirst::DEF, |_| Ok(Box::new(HelpJumpFirst)));
    add(HelpJumpLast::DEF, |_| Ok(Box::new(HelpJumpLast)));
    add(CloseHelp::DEF, |_| Ok(Box::new(CloseHelp)));
    add(OpenClaude::DEF, |_| Ok(Box::new(OpenClaude)));
    add(ClaudeSubmit::DEF, |_| Ok(Box::new(ClaudeSubmit)));
    add(ClaudeToPane::DEF, |_| Ok(Box::new(ClaudeToPane)));
    add(ClaudeToDockLeft::DEF, |_| Ok(Box::new(ClaudeToDockLeft)));
    add(ClaudeToDockRight::DEF, |_| Ok(Box::new(ClaudeToDockRight)));
    add(ClaudeToggleFollow::DEF, |_| {
        Ok(Box::new(ClaudeToggleFollow))
    });
    add(ClaudeFocusNextToolCard::DEF, |_| {
        Ok(Box::new(ClaudeFocusNextToolCard))
    });
    add(ClaudeFocusPrevToolCard::DEF, |_| {
        Ok(Box::new(ClaudeFocusPrevToolCard))
    });
    add(ClaudeToggleToolCardExpand::DEF, |_| {
        Ok(Box::new(ClaudeToggleToolCardExpand))
    });
    add(ClaudeJumpToFocusedCard::DEF, |_| {
        Ok(Box::new(ClaudeJumpToFocusedCard))
    });
    add(ToggleDockRight::DEF, |_| Ok(Box::new(ToggleDockRight)));
    add(ToggleDockLeft::DEF, |_| Ok(Box::new(ToggleDockLeft)));
    add(OpenCommits::DEF, |_| Ok(Box::new(OpenCommits)));
    add(CloseCommits::DEF, |_| Ok(Box::new(CloseCommits)));
    add(CommitsNext::DEF, |_| Ok(Box::new(CommitsNext)));
    add(CommitsPrev::DEF, |_| Ok(Box::new(CommitsPrev)));
    add(CommitsPageDown::DEF, |_| Ok(Box::new(CommitsPageDown)));
    add(CommitsPageUp::DEF, |_| Ok(Box::new(CommitsPageUp)));
    add(CommitsFirst::DEF, |_| Ok(Box::new(CommitsFirst)));
    add(CommitsLast::DEF, |_| Ok(Box::new(CommitsLast)));
    add(CommitsRefresh::DEF, |_| Ok(Box::new(CommitsRefresh)));
    add(CommitsOpenReview::DEF, |_| Ok(Box::new(CommitsOpenReview)));
    add(EnterRebase::DEF, |_| Ok(Box::new(EnterRebase)));
    add(AbortRebase::DEF, |_| Ok(Box::new(AbortRebase)));
    add(ExecuteRebase::DEF, |_| Ok(Box::new(ExecuteRebase)));
    add(RebaseNext::DEF, |_| Ok(Box::new(RebaseNext)));
    add(RebasePrev::DEF, |_| Ok(Box::new(RebasePrev)));
    add(RebaseMoveUp::DEF, |_| Ok(Box::new(RebaseMoveUp)));
    add(RebaseMoveDown::DEF, |_| Ok(Box::new(RebaseMoveDown)));
    add(SetRebaseOpPick::DEF, |_| Ok(Box::new(SetRebaseOpPick)));
    add(SetRebaseOpSquash::DEF, |_| Ok(Box::new(SetRebaseOpSquash)));
    add(SetRebaseOpFixup::DEF, |_| Ok(Box::new(SetRebaseOpFixup)));
    add(SetRebaseOpDrop::DEF, |_| Ok(Box::new(SetRebaseOpDrop)));
    add(SetRebaseOpReword::DEF, |_| Ok(Box::new(SetRebaseOpReword)));
    add(SetRebaseOpEdit::DEF, |_| Ok(Box::new(SetRebaseOpEdit)));
    add(RewordConfirm::DEF, |_| Ok(Box::new(RewordConfirm)));
    add(RewordAbort::DEF, |_| Ok(Box::new(RewordAbort)));
    add(RebaseContinue::DEF, |_| Ok(Box::new(RebaseContinue)));
    add(ConflictTakeOurs::DEF, |_| Ok(Box::new(ConflictTakeOurs)));
    add(ConflictTakeTheirs::DEF, |_| {
        Ok(Box::new(ConflictTakeTheirs))
    });
    add(ConflictSkipEntry::DEF, |_| Ok(Box::new(ConflictSkipEntry)));
    add(ConflictNextFile::DEF, |_| Ok(Box::new(ConflictNextFile)));
    add(ConflictPrevFile::DEF, |_| Ok(Box::new(ConflictPrevFile)));
    add(ConflictApply::DEF, |_| Ok(Box::new(ConflictApply)));
    add(ConflictAbort::DEF, |_| Ok(Box::new(ConflictAbort)));
    add(Run::DEF, |params| {
        let raw = params
            .first()
            .context(MissingSnafu { name: "command" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "command",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(Run {
            command: raw.to_owned(),
        }))
    });
    add(Dump::DEF, |params| {
        let raw = params
            .first()
            .context(MissingSnafu { name: "name" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "name",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(Dump {
            name: raw.to_owned(),
        }))
    });
    add(NewWorkspace::DEF, |_| Ok(Box::new(NewWorkspace)));
    add(CopyWorkspace::DEF, |_| Ok(Box::new(CopyWorkspace)));
    add(SwitchWorkspace::DEF, |_| Ok(Box::new(SwitchWorkspace)));
    add(CloseWorkspace::DEF, |_| Ok(Box::new(CloseWorkspace)));
    add(OpenWorkspacePicker::DEF, |_| {
        Ok(Box::new(OpenWorkspacePicker))
    });
    add(ToggleProjectTree::DEF, |_| Ok(Box::new(ToggleProjectTree)));
    add(ProjectTreeSelectNext::DEF, |_| {
        Ok(Box::new(ProjectTreeSelectNext))
    });
    add(ProjectTreeSelectPrev::DEF, |_| {
        Ok(Box::new(ProjectTreeSelectPrev))
    });
    add(ProjectTreeCollapse::DEF, |_| {
        Ok(Box::new(ProjectTreeCollapse))
    });
    add(ProjectTreeExpand::DEF, |_| Ok(Box::new(ProjectTreeExpand)));
    add(ProjectTreeConfirm::DEF, |_| {
        Ok(Box::new(ProjectTreeConfirm))
    });
    add(ProjectTreeRefresh::DEF, |_| {
        Ok(Box::new(ProjectTreeRefresh))
    });
    add(RenameWorkspace::DEF, |params| {
        let raw = match params.first() {
            Some(p) => p.as_string().context(WrongKindSnafu {
                name: "name",
                expected: ParamKind::String,
            })?,
            None => "",
        };
        Ok(Box::new(RenameWorkspace {
            name: raw.to_owned(),
        }))
    });
    add(SetCwd::DEF, |params| {
        let raw = match params.first() {
            Some(p) => p.as_string().context(WrongKindSnafu {
                name: "path",
                expected: ParamKind::String,
            })?,
            None => "",
        };
        Ok(Box::new(SetCwd {
            path: raw.to_owned(),
        }))
    });
    add(Pwd::DEF, |_| Ok(Box::new(Pwd)));
    add(Env::DEF, |_| Ok(Box::new(Env)));
    add(SubmitPromptInput::DEF, |_| Ok(Box::new(SubmitPromptInput)));
    add(CancelPromptInput::DEF, |_| Ok(Box::new(CancelPromptInput)));
    add(ShellInputSubmit::DEF, |_| Ok(Box::new(ShellInputSubmit)));
    add(PromptInsertNewline::DEF, |_| {
        Ok(Box::new(PromptInsertNewline))
    });
    add(PaletteSelectPrev::DEF, |_| Ok(Box::new(PaletteSelectPrev)));
    add(PaletteSelectNext::DEF, |_| Ok(Box::new(PaletteSelectNext)));
    add(PaletteScopeToggle::DEF, |_| {
        Ok(Box::new(PaletteScopeToggle))
    });
    add(PickerSelectPrev::DEF, |_| Ok(Box::new(PickerSelectPrev)));
    add(PickerSelectNext::DEF, |_| Ok(Box::new(PickerSelectNext)));
    add(PickerConfirm::DEF, |_| Ok(Box::new(PickerConfirm)));
    add(PickerConfirmSplitRight::DEF, |_| {
        Ok(Box::new(PickerConfirmSplitRight))
    });
    add(PickerConfirmSplitDown::DEF, |_| {
        Ok(Box::new(PickerConfirmSplitDown))
    });

    map
}

pub fn lookup(name: &str) -> Option<&'static RegistryEntry> {
    REGISTRY.get_or_init(init).get(name)
}

pub fn all() -> impl Iterator<Item = &'static RegistryEntry> {
    REGISTRY.get_or_init(init).values()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZERO_ARG_NAMES: &[&str] = &[
        "Quit",
        "QuitAll",
        "SplitRight",
        "SplitDown",
        "SplitNewRight",
        "SplitNewDown",
        "FocusLeft",
        "FocusRight",
        "FocusUp",
        "FocusDown",
        "FocusNext",
        "FocusPrev",
        "ClosePane",
        "CloseOtherPanes",
        "OpenCommandPalette",
        "OpenFileFinder",
        "OpenFileFinderHSplit",
        "OpenFileFinderVSplit",
        "OpenChangedFilePicker",
        "OpenGitStatus",
        "OpenBufferPicker",
        "FileFinderSelectPrev",
        "FileFinderSelectNext",
        "FileFinderScopeToggle",
        "OpenHelp",
        "OpenAbout",
        "OpenReview",
        "JumpToMoveSource",
        "JumpToMoveTarget",
        "JumpToNextMoveSource",
        "JumpToPrevMoveSource",
        "QueryMoveRelationships",
        "GotoNextDiagnostic",
        "GotoPrevDiagnostic",
        "GotoDefinition",
        "GotoTypeDefinition",
        "GotoImplementation",
        "Hover",
        "CodeAction",
        "RenameSymbol",
        "OpenSymbolPicker",
        "OpenWorkspaceSymbolPicker",
        "FormatSelections",
        "SetMark",
        "GotoMark",
        "GotoMarkExact",
        "SurroundAdd",
        "SurroundReplace",
        "SurroundDelete",
        "OpenSearchInput",
        "OpenReverseSearchInput",
        "SearchNext",
        "SearchPrev",
        "Yank",
        "PasteAfter",
        "PasteBefore",
        "YankToClipboard",
        "YankMainToClipboard",
        "PasteClipboardAfter",
        "PasteClipboardBefore",
        "SelectRegister",
        "InsertRegister",
        "DeleteForward",
        "DeleteBackward",
        "DeleteWordForward",
        "DeleteWordBackward",
        "Insert",
        "Append",
        "InsertNewline",
        "AddSelectionBelow",
        "MoveLeft",
        "MoveRight",
        "MoveUp",
        "MoveDown",
        "PageUp",
        "PageDown",
        "HalfPageUp",
        "HalfPageDown",
        "MoveNextWordStart",
        "MoveNextWordEnd",
        "MovePrevWordStart",
        "MovePrevWordEnd",
        "GotoLineStart",
        "GotoLineEnd",
        "OpenBelow",
        "OpenAbove",
        "ReplaceChar",
        "CollapseSelection",
        "FlipSelections",
        "SelectAll",
        "SelectLineBelow",
        "KeepPrimarySelection",
        "RemovePrimarySelection",
        "RotateSelectionsForward",
        "RotateSelectionsBackward",
        "TrimSelections",
        "ReviewNextChunk",
        "ReviewPrevChunk",
        "ReviewStageChunk",
        "ReviewUnstageChunk",
        "ReviewToggleStage",
        "ReviewSkipChunk",
        "ReviewApproveHunk",
        "ReviewToggleApproval",
        "ReviewNextUnreviewedHunk",
        "ReviewResetProgress",
        "GitToggleStageHunk",
        "GitUnstageHunk",
        "GitToggleStageLine",
        "ReviewRevertHunk",
        "ReviewCycleComparisonMode",
        "ReviewToggleFollow",
        "ReviewRefresh",
        "ReviewApplyStaged",
        "CloseReview",
        "ReviewRemoveSelected",
        "OpenCommits",
        "CloseCommits",
        "CommitsNext",
        "CommitsPrev",
        "CommitsPageDown",
        "CommitsPageUp",
        "CommitsFirst",
        "CommitsLast",
        "CommitsRefresh",
        "CommitsOpenReview",
        "EnterRebase",
        "AbortRebase",
        "ExecuteRebase",
        "RebaseNext",
        "RebasePrev",
        "RebaseMoveUp",
        "RebaseMoveDown",
        "SetRebaseOpPick",
        "SetRebaseOpSquash",
        "SetRebaseOpFixup",
        "SetRebaseOpDrop",
        "SetRebaseOpReword",
        "SetRebaseOpEdit",
        "RewordConfirm",
        "RewordAbort",
        "RebaseContinue",
        "ConflictTakeOurs",
        "ConflictTakeTheirs",
        "ConflictSkipEntry",
        "ConflictNextFile",
        "ConflictPrevFile",
        "ConflictApply",
        "ConflictAbort",
        "OpenRun",
        "RunSubmit",
        "RunInterrupt",
        "RunHistoryPrev",
        "RunHistoryNext",
        "OpenClaude",
        "ClaudeSubmit",
        "ClaudeToPane",
        "ClaudeToDockLeft",
        "ClaudeToDockRight",
        "ClaudeToggleFollow",
        "ClaudeFocusNextToolCard",
        "ClaudeFocusPrevToolCard",
        "ClaudeToggleToolCardExpand",
        "ClaudeJumpToFocusedCard",
        "ToggleDockRight",
        "ToggleDockLeft",
        "NewWorkspace",
        "CopyWorkspace",
        "SwitchWorkspace",
        "CloseWorkspace",
        "OpenWorkspacePicker",
        "ToggleProjectTree",
        "ProjectTreeSelectNext",
        "ProjectTreeSelectPrev",
        "ProjectTreeCollapse",
        "ProjectTreeExpand",
        "ProjectTreeConfirm",
        "ProjectTreeRefresh",
        "HelpSelectPrev",
        "HelpSelectNext",
        "HelpScopeToggle",
        "HelpScrollDetailUp",
        "HelpScrollDetailDown",
        "HelpJumpFirst",
        "HelpJumpLast",
        "SubmitPromptInput",
        "CancelPromptInput",
        "ShellInputSubmit",
        "PromptInsertNewline",
        "PaletteSelectPrev",
        "PaletteSelectNext",
        "PaletteScopeToggle",
        "PickerSelectPrev",
        "PickerSelectNext",
        "PickerConfirm",
        "PickerConfirmSplitRight",
        "PickerConfirmSplitDown",
    ];

    #[test]
    fn lookup_all_actions() {
        for name in ZERO_ARG_NAMES {
            assert!(lookup(name).is_some(), "missing: {name}");
        }
        assert!(lookup("OpenFile").is_some());
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup("Foo").is_none());
        assert!(lookup("SetMode").is_none());
    }

    #[test]
    fn factory_creates_correct_kind() {
        for name in ZERO_ARG_NAMES {
            let entry = lookup(name).expect(name);
            let action = (entry.create)(&[]).expect(name);
            assert_eq!(action.kind(), entry.def.kind(), "kind mismatch for {name}");
        }
    }

    #[test]
    fn open_file_factory_consumes_path() {
        let entry = lookup("OpenFile").expect("OpenFile");
        let params = vec![ParamValue::String("/tmp/x.rs".into())];
        let action = (entry.create)(&params).expect("create");
        let open = action
            .as_any()
            .downcast_ref::<OpenFile>()
            .expect("downcast");
        assert_eq!(open.path, PathBuf::from("/tmp/x.rs"));
    }

    #[test]
    fn open_file_factory_missing_param_errors() {
        let entry = lookup("OpenFile").expect("OpenFile");
        assert!(matches!(
            (entry.create)(&[]).err(),
            Some(ParamError::Missing { name: "path", .. })
        ));
    }

    #[test]
    fn open_file_factory_wrong_kind_errors() {
        let entry = lookup("OpenFile").expect("OpenFile");
        let err = (entry.create)(&[ParamValue::Number(1.0)]).err();
        assert!(matches!(
            err,
            Some(ParamError::WrongKind {
                name: "path",
                expected: ParamKind::String,
                ..
            })
        ));
    }

    #[test]
    fn all_returns_complete_list() {
        // 70 previous + 13 Phase-5 rebase primitives + 1 Dump + 1 OpenHelp
        // + 4 workspace actions + 5 prompt-input plumbing actions
        // + 1 PaletteScopeToggle + 2 run-history actions
        // + 7 help plumbing actions + 1 CloseHelp + 1 QuitAll
        // + 7 extend-selection variants + 1 CloseOtherPanes
        // + 2 goto-line-boundary actions + 3 goto-file/line/nonwhitespace
        // actions + 4 extend-to goto variants + 3 selection primitives
        // (collapse/flip/select-all) + 2 more (select-line-below,
        // keep-primary) + 2 prev-word-end variants (move + extend).
        // Insert and Backspace in reword mode are handled by the editor
        // directly, not via the action registry.
        // + 4 file-finder actions (open, select prev/next, scope toggle).
        // + 1 ClaudeToggleFollow.
        // + 4 viewport motions (PageUp, PageDown, HalfPageUp, HalfPageDown).
        // + 3 selection ops (RotateSelectionsForward/Backward, TrimSelections).
        // + 3 window-relative gotos (GotoWindowTop/Center/Bottom).
        // + 3 view-alignment ops (AlignViewTop/Center/Bottom).
        // + 2 view-scroll ops (ScrollUp/Down).
        // + 1 case toggle (SwitchCase).
        // + 2 case-force (SwitchToUppercase/Lowercase).
        // + 4 long-word motions (MoveNextLongWordStart/End, MovePrevLongWordStart/End).
        // + 1 AddSelectionAbove (mirror of AddSelectionBelow).
        // + 1 SplitSelectionOnNewline.
        // + 2 number ops (Increment/Decrement).
        // + 1 DeleteSelection.
        // + 2 line indent ops (IndentSelection/UnindentSelection).
        // + 1 AlignSelections.
        // + 1 Undo.
        // + 1 Redo.
        // + 2 GotoNextHunk/GotoPrevHunk.
        // + 1 ExpandSelection.
        // + 1 ShrinkSelection.
        // + 2 SelectNextSibling/SelectPrevSibling.
        // + 2 SelectAllSiblings/SelectAllChildren.
        // + 2 MoveParentNodeStart/MoveParentNodeEnd.
        // + 3 SaveSelection/JumpBackward/JumpForward.
        // + 1 OpenJumplistPicker.
        // + 1 OpenLastPicker.
        // + 1 OpenDiagnosticsPicker.
        // + 1 OpenWorkspaceDiagnosticsPicker.
        // + 1 OpenGlobalSearch.
        // + 1 SplitSelection.
        // + 2 KeepSelections / RemoveSelections.
        // + 2 RecordMacro / ReplayMacro.
        // + 5 ShellPipe / ShellPipeTo / ShellInsertOutput / ShellAppendOutput / ShellKeepPipe.
        // + 1 SaveBuffer.
        // + 1 CloseBuffer.
        // + 1 AcceptCompletion.
        // + 2 SmartTab/TriggerCompletion.
        // + 1 GotoLineNumber.
        // + 4 FindNextChar/FindPrevChar/TillNextChar/TillPrevChar.
        // + 1 RepeatLastMotion.
        // + 1 GotoColumn.
        // + 2 GotoNextParagraph/GotoPrevParagraph.
        // + 1 MatchBrackets.
        // + 4 ExtendFindNextChar/ExtendFindPrevChar/ExtendTillNextChar/ExtendTillPrevChar.
        // + 1 ExtendGotoColumn.
        // + 6 ExtendGoto{FirstNonwhitespace,FileStart,LastLine,WindowTop,WindowCenter,
        //   WindowBottom}.
        // + 1 ToggleComments.
        // + 2 ExtendMoveParentNodeStart/ExtendMoveParentNodeEnd.
        // + 2 ExtendSelectNextSibling/ExtendSelectPrevSibling.
        // + 1 RemovePrimarySelection.
        // + 1 OpenChangedFilePicker.
        // + 1 OpenBufferPicker.
        // + 1 GotoWord.
        // + 2 GotoNextDiagnostic / GotoPrevDiagnostic.
        // + 2 OpenBelow / OpenAbove.
        // + 1 ReplaceChar.
        // + 1 GotoDefinition.
        // + 1 GotoTypeDefinition.
        // + 1 GotoImplementation.
        // + 1 Hover.
        // + 1 CodeAction.
        // + 1 RenameSymbol.
        // + 1 OpenSymbolPicker.
        // + 1 OpenWorkspaceSymbolPicker.
        // + 1 FormatSelections.
        // + 3 marks (SetMark, GotoMark, GotoMarkExact).
        // + 1 SurroundAdd.
        // + 2 SurroundReplace, SurroundDelete.
        // + 2 SelectTextobjectAround, SelectTextobjectInner.
        // + 4 GotoNextFunction, GotoPrevFunction, GotoNextClass, GotoPrevClass.
        // + 4 OpenSearchInput, OpenReverseSearchInput, SearchNext, SearchPrev.
        // + 3 Yank, PasteAfter, PasteBefore.
        // + 4 YankToClipboard, YankMainToClipboard, PasteClipboardAfter, PasteClipboardBefore.
        // + 1 SelectRegister.
        // + 1 InsertRegister.
        // + 1 CommitUndoCheckpoint.
        // + 3 Claude tool-card focus/expand (ClaudeFocusNextToolCard, ClaudeFocusPrevToolCard,
        //   ClaudeToggleToolCardExpand).
        // + 1 ClaudeJumpToFocusedCard.
        // + 1 DismissModal.
        // + 5 Picker actions (SelectPrev/SelectNext/Confirm/ConfirmSplitRight/ ConfirmSplitDown)
        //   that the GUI's `Picker<D>` primitive routes via `Workspace::dispatch_action` -> active
        //   modal -> `Picker::handle_action`.
        // + 7 edit-action primitives (DeleteForward, DeleteBackward, DeleteWordForward,
        //   DeleteWordBackward, Insert, Append, InsertNewline) the GUI dispatches via the
        //   per-handler edit-action module.
        // + 1 ShellInputSubmit (palette-invisible: dismisses the active ShellInputModal).
        // + 3 ToggleBlame / ToggleDiffHunkPanel / OpenGitStatus for the space_git submode.
        // + 1 ToggleMinimap for the space_workspace submode.
        // + 1 ToggleTabBar for the per-pane tab bar.
        // + 1 Set runtime-settings palette action.
        // + 1 ToggleProjectTree (opens the project file tree in a dock).
        // + 6 ProjectTree navigation (SelectNext/SelectPrev/Collapse/Expand/ Confirm/Refresh)
        //   routed to the focused project tree dock.
        // + 1 OpenReviewWatch (workspace-watch review session).
        // + 1 OpenAbout (build info modal).
        // + 2 ReviewApproveHunk / ReviewToggleApproval.
        // + 1 ReviewNextUnreviewedHunk.
        // + 1 ReviewResetProgress.
        // + 2 GitToggleStageHunk / GitUnstageHunk.
        // + 1 GitToggleStageLine.
        // + 1 ReviewRevertHunk.
        // + 1 ReviewCycleComparisonMode.
        // + 1 ReviewToggleFollow.
        // + 1 SetCwd.
        // + 2 Pwd / Env (command-line :pwd / :env palette actions).
        // + 1 OpenConflictPicker (lists conflicted files in the worktree).
        // + 2 ReviewEnterLineSelect / ReviewLineSelectCancel (line-select mode).
        // + 2 ReviewLineSelectToggle / ReviewLineSelectAll (line-select mutation).
        // + 2 ReviewLineSelectStage / ReviewLineSelectUnstage (line-select apply).
        assert_eq!(all().count(), 328);
    }

    #[test]
    fn all_have_descriptions() {
        for entry in all() {
            assert!(!entry.def.short_desc().is_empty(), "{}", entry.def.name());
            assert!(!entry.def.long_desc().is_empty(), "{}", entry.def.name());
        }
    }
}
