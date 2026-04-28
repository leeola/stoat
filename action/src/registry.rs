use crate::{
    defs::{
        app::{Quit, QuitAll},
        claude::{
            ClaudeSubmit, ClaudeToDockLeft, ClaudeToDockRight, ClaudeToPane, ClaudeToggleFollow,
            OpenClaude, ToggleDockLeft, ToggleDockRight,
        },
        commits::{
            CloseCommits, CommitsFirst, CommitsLast, CommitsNext, CommitsOpenReview,
            CommitsPageDown, CommitsPageUp, CommitsPrev, CommitsRefresh, OpenCommits,
        },
        dump::Dump,
        editor::{
            AddSelectionAbove, AddSelectionBelow, AlignSelections, AlignViewBottom,
            AlignViewCenter, AlignViewTop, CollapseSelection, Decrement, DeleteSelection,
            ExpandSelection, ExtendDown, ExtendFindNextChar, ExtendFindPrevChar, ExtendLeft,
            ExtendNextWordEnd, ExtendNextWordStart, ExtendPrevWordEnd, ExtendPrevWordStart,
            ExtendRight, ExtendTillNextChar, ExtendTillPrevChar, ExtendToFileStart,
            ExtendToLastLine, ExtendToLineEnd, ExtendToLineStart, ExtendUp, FindNextChar,
            FindPrevChar, FlipSelections, GotoColumn, GotoFileStart, GotoFirstNonwhitespace,
            GotoLastLine, GotoLineEnd, GotoLineNumber, GotoLineStart, GotoNextChange,
            GotoNextParagraph, GotoPrevChange, GotoPrevParagraph, GotoWindowBottom,
            GotoWindowCenter, GotoWindowTop, HalfPageDown, HalfPageUp, Increment, IndentSelection,
            JumpBackward, JumpForward, KeepPrimarySelection, MatchBrackets, MoveDown, MoveLeft,
            MoveNextLongWordEnd, MoveNextLongWordStart, MoveNextWordEnd, MoveNextWordStart,
            MoveParentNodeEnd, MoveParentNodeStart, MovePrevLongWordEnd, MovePrevLongWordStart,
            MovePrevWordEnd, MovePrevWordStart, MoveRight, MoveUp, PageDown, PageUp, Redo,
            RepeatLastMotion, RotateSelectionsBackward, RotateSelectionsForward, SaveSelection,
            ScrollDown, ScrollUp, SelectAll, SelectLineBelow, SelectNextSibling, SelectPrevSibling,
            ShrinkSelection, SplitSelectionOnNewline, SwitchCase, SwitchToLowercase,
            SwitchToUppercase, TillNextChar, TillPrevChar, TrimSelections, Undo, UnindentSelection,
        },
        file::OpenFile,
        file_finder::{
            FileFinderScopeToggle, FileFinderSelectNext, FileFinderSelectPrev, OpenFileFinder,
            OpenFileFinderHSplit, OpenFileFinderVSplit,
        },
        help::{
            CloseHelp, HelpJumpFirst, HelpJumpLast, HelpScopeToggle, HelpScrollDetailDown,
            HelpScrollDetailUp, HelpSelectNext, HelpSelectPrev, OpenHelp,
        },
        palette::OpenCommandPalette,
        pane::{
            CloseOtherPanes, ClosePane, FocusDown, FocusLeft, FocusNext, FocusPrev, FocusRight,
            FocusUp, SplitDown, SplitNewDown, SplitNewRight, SplitRight,
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
            CloseReview, JumpToMoveSource, JumpToMoveTarget, JumpToNextMoveSource,
            JumpToPrevMoveSource, OpenReview, OpenReviewCommit, OpenReviewCommitRange,
            QueryMoveRelationships, ReviewApplyStaged, ReviewNextChunk, ReviewPrevChunk,
            ReviewRefresh, ReviewRemoveSelected, ReviewSkipChunk, ReviewStageChunk,
            ReviewToggleStage, ReviewUnstageChunk,
        },
        run::{OpenRun, Run, RunHistoryNext, RunHistoryPrev, RunInterrupt, RunSubmit},
        workspace::{
            CloseWorkspace, CopyWorkspace, NewWorkspace, RenameWorkspace, SwitchWorkspace,
        },
    },
    Action, ActionDef, ParamError, ParamKind, ParamValue,
};
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
    add(ReviewNextChunk::DEF, |_| Ok(Box::new(ReviewNextChunk)));
    add(ReviewPrevChunk::DEF, |_| Ok(Box::new(ReviewPrevChunk)));
    add(ReviewStageChunk::DEF, |_| Ok(Box::new(ReviewStageChunk)));
    add(ReviewUnstageChunk::DEF, |_| {
        Ok(Box::new(ReviewUnstageChunk))
    });
    add(ReviewToggleStage::DEF, |_| Ok(Box::new(ReviewToggleStage)));
    add(ReviewSkipChunk::DEF, |_| Ok(Box::new(ReviewSkipChunk)));
    add(ReviewRefresh::DEF, |_| Ok(Box::new(ReviewRefresh)));
    add(ReviewApplyStaged::DEF, |_| Ok(Box::new(ReviewApplyStaged)));
    add(CloseReview::DEF, |_| Ok(Box::new(CloseReview)));
    add(ReviewRemoveSelected::DEF, |_| {
        Ok(Box::new(ReviewRemoveSelected))
    });
    add(OpenReviewCommit::DEF, |params| {
        let workdir = params
            .first()
            .ok_or(ParamError::Missing("workdir"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "workdir",
                expected: ParamKind::String,
            })?;
        let sha = params
            .get(1)
            .ok_or(ParamError::Missing("sha"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "sha",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(OpenReviewCommit {
            workdir: PathBuf::from(workdir),
            sha: sha.to_owned(),
        }))
    });
    add(OpenReviewCommitRange::DEF, |params| {
        let workdir = params
            .first()
            .ok_or(ParamError::Missing("workdir"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "workdir",
                expected: ParamKind::String,
            })?;
        let from = params
            .get(1)
            .ok_or(ParamError::Missing("from"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "from",
                expected: ParamKind::String,
            })?;
        let to = params
            .get(2)
            .ok_or(ParamError::Missing("to"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
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
    add(MoveParentNodeStart::DEF, |_| {
        Ok(Box::new(MoveParentNodeStart))
    });
    add(MoveParentNodeEnd::DEF, |_| Ok(Box::new(MoveParentNodeEnd)));
    add(SaveSelection::DEF, |_| Ok(Box::new(SaveSelection)));
    add(JumpBackward::DEF, |_| Ok(Box::new(JumpBackward)));
    add(JumpForward::DEF, |_| Ok(Box::new(JumpForward)));
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
    add(GotoFileStart::DEF, |_| Ok(Box::new(GotoFileStart)));
    add(GotoLastLine::DEF, |_| Ok(Box::new(GotoLastLine)));
    add(GotoLineNumber::DEF, |_| Ok(Box::new(GotoLineNumber)));
    add(GotoColumn::DEF, |_| Ok(Box::new(GotoColumn)));
    add(GotoNextChange::DEF, |_| Ok(Box::new(GotoNextChange)));
    add(GotoPrevChange::DEF, |_| Ok(Box::new(GotoPrevChange)));
    add(GotoNextParagraph::DEF, |_| Ok(Box::new(GotoNextParagraph)));
    add(GotoPrevParagraph::DEF, |_| Ok(Box::new(GotoPrevParagraph)));
    add(MatchBrackets::DEF, |_| Ok(Box::new(MatchBrackets)));
    add(GotoWindowTop::DEF, |_| Ok(Box::new(GotoWindowTop)));
    add(GotoWindowCenter::DEF, |_| Ok(Box::new(GotoWindowCenter)));
    add(GotoWindowBottom::DEF, |_| Ok(Box::new(GotoWindowBottom)));
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
    add(Undo::DEF, |_| Ok(Box::new(Undo)));
    add(Redo::DEF, |_| Ok(Box::new(Redo)));
    add(IndentSelection::DEF, |_| Ok(Box::new(IndentSelection)));
    add(UnindentSelection::DEF, |_| Ok(Box::new(UnindentSelection)));
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
            .ok_or(ParamError::Missing("path"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
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
            .ok_or(ParamError::Missing("command"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
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
            .ok_or(ParamError::Missing("name"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
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
    add(RenameWorkspace::DEF, |params| {
        let raw = params
            .first()
            .ok_or(ParamError::Missing("name"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "name",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(RenameWorkspace {
            name: raw.to_owned(),
        }))
    });
    add(SubmitPromptInput::DEF, |_| Ok(Box::new(SubmitPromptInput)));
    add(CancelPromptInput::DEF, |_| Ok(Box::new(CancelPromptInput)));
    add(PromptInsertNewline::DEF, |_| {
        Ok(Box::new(PromptInsertNewline))
    });
    add(PaletteSelectPrev::DEF, |_| Ok(Box::new(PaletteSelectPrev)));
    add(PaletteSelectNext::DEF, |_| Ok(Box::new(PaletteSelectNext)));
    add(PaletteScopeToggle::DEF, |_| {
        Ok(Box::new(PaletteScopeToggle))
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
        "FileFinderSelectPrev",
        "FileFinderSelectNext",
        "FileFinderScopeToggle",
        "OpenHelp",
        "OpenReview",
        "JumpToMoveSource",
        "JumpToMoveTarget",
        "JumpToNextMoveSource",
        "JumpToPrevMoveSource",
        "QueryMoveRelationships",
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
        "CollapseSelection",
        "FlipSelections",
        "SelectAll",
        "SelectLineBelow",
        "KeepPrimarySelection",
        "RotateSelectionsForward",
        "RotateSelectionsBackward",
        "TrimSelections",
        "ReviewNextChunk",
        "ReviewPrevChunk",
        "ReviewStageChunk",
        "ReviewUnstageChunk",
        "ReviewToggleStage",
        "ReviewSkipChunk",
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
        "ToggleDockRight",
        "ToggleDockLeft",
        "NewWorkspace",
        "CopyWorkspace",
        "SwitchWorkspace",
        "CloseWorkspace",
        "HelpSelectPrev",
        "HelpSelectNext",
        "HelpScopeToggle",
        "HelpScrollDetailUp",
        "HelpScrollDetailDown",
        "HelpJumpFirst",
        "HelpJumpLast",
        "SubmitPromptInput",
        "CancelPromptInput",
        "PromptInsertNewline",
        "PaletteSelectPrev",
        "PaletteSelectNext",
        "PaletteScopeToggle",
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
        assert_eq!((entry.create)(&[]).err(), Some(ParamError::Missing("path")));
    }

    #[test]
    fn open_file_factory_wrong_kind_errors() {
        let entry = lookup("OpenFile").expect("OpenFile");
        let err = (entry.create)(&[ParamValue::Number(1.0)]).err();
        assert_eq!(
            err,
            Some(ParamError::WrongKind {
                name: "path",
                expected: ParamKind::String,
            })
        );
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
        // + 2 GotoNextChange/GotoPrevChange.
        // + 1 ExpandSelection.
        // + 1 ShrinkSelection.
        // + 2 SelectNextSibling/SelectPrevSibling.
        // + 2 MoveParentNodeStart/MoveParentNodeEnd.
        // + 3 SaveSelection/JumpBackward/JumpForward.
        // + 1 GotoLineNumber.
        // + 4 FindNextChar/FindPrevChar/TillNextChar/TillPrevChar.
        // + 1 RepeatLastMotion.
        // + 1 GotoColumn.
        // + 2 GotoNextParagraph/GotoPrevParagraph.
        // + 1 MatchBrackets.
        // + 4 ExtendFindNextChar/ExtendFindPrevChar/ExtendTillNextChar/ExtendTillPrevChar.
        assert_eq!(all().count(), 196);
    }

    #[test]
    fn all_have_descriptions() {
        for entry in all() {
            assert!(!entry.def.short_desc().is_empty(), "{}", entry.def.name());
            assert!(!entry.def.long_desc().is_empty(), "{}", entry.def.name());
        }
    }
}
