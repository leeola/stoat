use crate::{
    defs::{
        agent::SpawnClaude,
        app::{OpenLogs, Quit, QuitAll, QuitAllCancel, QuitAllConfirm, ShowVersion},
        commits::{
            CloseCommits, CommitsFirst, CommitsLast, CommitsNext, CommitsOpenReview,
            CommitsPageDown, CommitsPageUp, CommitsPrev, CommitsRefresh, OpenCommits,
        },
        conflict::{
            CloseConflict, Conflict, ConflictApply, ConflictNextChunk, ConflictNextFile,
            ConflictPickBoth, ConflictPickOurs, ConflictPickOursLine, ConflictPickTheirs,
            ConflictPickTheirsLine, ConflictPrevChunk, ConflictPrevFile, ConflictResetChunk,
        },
        dump::Dump,
        editor::{
            AcceptCompletion, AddSelectionAbove, AddSelectionBelow, AlignSelections,
            AlignViewBottom, AlignViewCenter, AlignViewTop, AppendMode, ChangeSelection,
            CloseBuffer, CollapseSelection, CommitUndoCheckpoint, Decrement, DeleteSelection,
            DeleteSelectionNoYank, EnsureSelectionsForward, EnterInsertMode, ExpandSelection,
            ExtendDown, ExtendFindNextChar, ExtendFindPrevChar, ExtendGotoColumn,
            ExtendGotoFileStart, ExtendGotoFirstNonwhitespace, ExtendGotoLastLine,
            ExtendGotoWindowBottom, ExtendGotoWindowCenter, ExtendGotoWindowTop, ExtendLeft,
            ExtendMoveParentNodeEnd, ExtendMoveParentNodeStart, ExtendNextWordEnd,
            ExtendNextWordStart, ExtendPrevWordEnd, ExtendPrevWordStart, ExtendRight,
            ExtendSelectNextSibling, ExtendSelectPrevSibling, ExtendTillNextChar,
            ExtendTillPrevChar, ExtendToFileStart, ExtendToLastLine, ExtendToLineBounds,
            ExtendToLineEnd, ExtendToLineStart, ExtendUp, FindNextChar, FindPrevChar,
            FlipSelections, GotoCallee, GotoCaller, GotoColumn, GotoDiffCalleeDown,
            GotoDiffCallerUp, GotoFileStart, GotoFirstNonwhitespace, GotoImplementors,
            GotoLastLine, GotoLineEnd, GotoLineNumber, GotoLineStart, GotoMark, GotoMarkExact,
            GotoNextChange, GotoNextClass, GotoNextFunction, GotoNextParagraph, GotoPrevChange,
            GotoPrevClass, GotoPrevFunction, GotoPrevParagraph, GotoReferences, GotoWindowBottom,
            GotoWindowCenter, GotoWindowTop, GotoWord, HalfPageDown, HalfPageUp, Increment,
            IndentSelection, InsertAtLineEnd, InsertAtLineStart, InsertRegister, InsertTab,
            JoinSelections, JoinSelectionsSpace, JumpBackward, JumpForward, KeepPrimarySelection,
            KeepSelections, MarkTrailEnd, MarkTrailStart, MatchBrackets, MoveDown, MoveLeft,
            MoveNextLongWordEnd, MoveNextLongWordStart, MoveNextWordEnd, MoveNextWordStart,
            MoveParentNodeEnd, MoveParentNodeStart, MovePrevLongWordEnd, MovePrevLongWordStart,
            MovePrevWordEnd, MovePrevWordStart, MoveRight, MoveUp, OpenAbove, OpenBelow,
            OpenCodeSearch, OpenJumplistPicker, OpenLastPicker, OpenReverseSearchInput,
            OpenSearchInput, PageDown, PageUp, PasteAfter, PasteBefore, PasteClipboardAfter,
            PasteClipboardBefore, RecordMacro, Redo, RemovePrimarySelection, RemoveSelections,
            RepeatLastMotion, ReplaceChar, ReplaceWithYanked, ReplayMacro,
            RotateSelectionContentsBackward, RotateSelectionContentsForward,
            RotateSelectionsBackward, RotateSelectionsForward, SaveBuffer, SaveSelection,
            ScrollDown, ScrollUp, SearchNext, SearchPrev, SelectAll, SelectAllChildren,
            SelectAllSiblings, SelectLineBelow, SelectNextSibling, SelectPrevSibling, SelectRegex,
            SelectRegister, SelectTextobjectAround, SelectTextobjectInner, SetMark,
            ShellAppendOutput, ShellInsertOutput, ShellKeepPipe, ShellPipe, ShellPipeTo,
            ShrinkSelection, ShrinkToLineBounds, SmartTab, SplitSelection, SplitSelectionOnNewline,
            SurroundAdd, SurroundDelete, SurroundReplace, SwitchCase, SwitchToLowercase,
            SwitchToUppercase, TillNextChar, TillPrevChar, ToggleComments, ToggleInlayHints,
            ToggleLspStatus, ToggleSyntaxHighlight, TrailNext, TrailPrev, TriggerCompletion,
            TrimSelections, Undo, UnindentSelection, WriteQuit, Yank, YankMainToClipboard,
            YankToClipboard,
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
            CodeAction, Format, FormatSelections, GotoDeclaration, GotoDefinition,
            GotoImplementation, GotoNextDiagnostic, GotoPrevDiagnostic, GotoTypeDefinition, Hover,
            OpenDiagnosticsPicker, OpenSymbolPicker, OpenWorkspaceDiagnosticsPicker,
            OpenWorkspaceSymbolPicker, RenameSymbol, SymbolFinderComplete, SymbolFinderPageDown,
            SymbolFinderPageUp, SymbolFinderSelectNext, SymbolFinderSelectPrev,
        },
        palette::OpenCommandPalette,
        pane::{
            CloseOtherPanes, ClosePane, DetachPane, FocusDown, FocusLeft, FocusNext, FocusPane,
            FocusPrev, FocusRight, FocusUp, ReattachPane, SplitDown, SplitNewDown, SplitNewRight,
            SplitRight, ToggleDockLeft, ToggleDockRight, TogglePaneWiden,
        },
        picker::{
            CodeSearchClose, CodeSearchModeToggle, CodeSearchNext, CodeSearchPageDown,
            CodeSearchPageUp, CodeSearchPrev, CodeSearchSelect, DiagnosticsPickerClose,
            DiagnosticsPickerNext, DiagnosticsPickerPrev, DiagnosticsPickerSelect,
            JumplistPickerClose, JumplistPickerNext, JumplistPickerPrev, JumplistPickerSelect,
            LocationPickerClose, LocationPickerNext, LocationPickerPrev, LocationPickerSelect,
        },
        prompt::{
            CancelPromptInput, PaletteComplete, PaletteHistoryNext, PaletteHistoryPrev,
            PalettePageDown, PalettePageUp, PaletteScopeToggle, PaletteSelectNext,
            PaletteSelectPrev, PromptInsertNewline, SubmitPromptInput,
        },
        rebase::{
            AbortRebase, EnterRebase, ExecuteRebase, RebaseConflictAbort, RebaseConflictApply,
            RebaseConflictNextFile, RebaseConflictPrevFile, RebaseConflictSkipEntry,
            RebaseConflictTakeOurs, RebaseConflictTakeTheirs, RebaseContinue, RebaseMoveDown,
            RebaseMoveUp, RebaseNext, RebasePrev, RewordAbort, RewordConfirm, SetRebaseOpDrop,
            SetRebaseOpEdit, SetRebaseOpFixup, SetRebaseOpPick, SetRebaseOpReword,
            SetRebaseOpSquash,
        },
        review::{
            CloseReview, Diff, JumpToMoveSource, JumpToMoveTarget, JumpToNextMoveSource,
            JumpToPrevMoveSource, OpenReviewCommit, OpenReviewCommitRange, QueryMoveRelationships,
            ReviewApplyStaged, ReviewNextChunk, ReviewPrevChunk, ReviewRefresh,
            ReviewRemoveSelected, ReviewSkipChunk, ReviewStageChunk, ReviewToggleStage,
            ReviewUnstageChunk, StageHunk, StageLine, ToggleDiff, ToggleStageHunk, ToggleStageLine,
            UnstageHunk, UnstageLine,
        },
        run::{
            OpenRun, Run, RunHistoryNext, RunHistoryPrev, RunInterrupt, RunModalDismiss, RunSubmit,
        },
        set_theme::SetTheme,
        tab::{CloseTab, GotoTab, NewTab, NextTab, PrevTab, RenameTab, ToggleTab, ToggleTabBar},
        terminal::Terminal,
        workspace::{
            CloseWorkspace, CopyWorkspace, NewWorkspace, ReloadEnv, RenameWorkspace, SetCwd,
            ShowCwd, SwitchWorkspace, WorkspacePickerClose, WorkspacePickerComplete,
            WorkspacePickerNext, WorkspacePickerPrev, WorkspacePickerSelect,
        },
    },
    param::{MissingSnafu, ParseFailureSnafu, WrongKindSnafu},
    Action, ActionDef, ParamError, ParamKind, ParamValue,
};
use snafu::OptionExt;
use std::{collections::HashMap, path::PathBuf, sync::OnceLock};

pub type CreateFn = fn(&[ParamValue]) -> Result<Box<dyn Action>, ParamError>;

pub struct RegistryEntry {
    pub def: &'static dyn ActionDef,
    pub create: CreateFn,
    /// How the palette spells this action for the user, from the def's
    /// [`ActionDef::command_name`] override or [`kebab`] of its name.
    pub command_name: String,
}

static REGISTRY: OnceLock<HashMap<&'static str, RegistryEntry>> = OnceLock::new();

/// Rewrite a PascalCase action name as the kebab-case token a user types, so
/// `SetCwd` reads as `set-cwd`.
///
/// A dash goes before every uppercase letter except a leading one, and the
/// result is lowercased throughout. A name that is already lowercase passes
/// through unchanged.
fn kebab(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push('-');
        }
        out.extend(ch.to_lowercase());
    }
    out
}

fn init() -> HashMap<&'static str, RegistryEntry> {
    let mut map = HashMap::with_capacity(16);
    let mut add = |def: &'static dyn ActionDef, create: CreateFn| {
        let command_name = def
            .command_name()
            .map(str::to_string)
            .unwrap_or_else(|| kebab(def.name()));
        map.insert(
            def.name(),
            RegistryEntry {
                def,
                create,
                command_name,
            },
        );
    };

    add(Quit::DEF, |_| Ok(Box::new(Quit)));
    add(QuitAll::DEF, |_| Ok(Box::new(QuitAll)));
    add(QuitAllConfirm::DEF, |_| Ok(Box::new(QuitAllConfirm)));
    add(QuitAllCancel::DEF, |_| Ok(Box::new(QuitAllCancel)));
    add(ShowVersion::DEF, |_| Ok(Box::new(ShowVersion)));
    add(OpenLogs::DEF, |_| Ok(Box::new(OpenLogs)));
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
    add(FocusPane::DEF, |params| {
        let raw = params
            .first()
            .context(MissingSnafu { name: "index" })?
            .as_number()
            .context(WrongKindSnafu {
                name: "index",
                expected: ParamKind::Number,
            })?;
        if raw < 1.0 || raw.fract() != 0.0 {
            return ParseFailureSnafu {
                expected: ParamKind::Number,
                input: raw.to_string(),
            }
            .fail();
        }
        Ok(Box::new(FocusPane {
            index: raw as usize,
        }))
    });
    add(GotoTab::DEF, |params| {
        let raw = params
            .first()
            .context(MissingSnafu { name: "index" })?
            .as_number()
            .context(WrongKindSnafu {
                name: "index",
                expected: ParamKind::Number,
            })?;
        if raw < 1.0 || raw.fract() != 0.0 {
            return ParseFailureSnafu {
                expected: ParamKind::Number,
                input: raw.to_string(),
            }
            .fail();
        }
        Ok(Box::new(GotoTab {
            index: raw as usize,
        }))
    });
    add(RenameTab::DEF, |params| {
        // A present-but-empty argument (the palette's `tab-rename ` submit)
        // clears the override, so unlike OpenConfig an empty string is kept as
        // `Some("")` rather than folded to `None`. Only a fully absent argument
        // (the bare keybinding) is `None`, which opens the palette.
        let name = params
            .first()
            .map(|param| {
                param.as_string().context(WrongKindSnafu {
                    name: "name",
                    expected: ParamKind::String,
                })
            })
            .transpose()?
            .map(str::to_owned);
        Ok(Box::new(RenameTab { name }))
    });
    add(NewTab::DEF, |_| Ok(Box::new(NewTab)));
    add(NextTab::DEF, |_| Ok(Box::new(NextTab)));
    add(PrevTab::DEF, |_| Ok(Box::new(PrevTab)));
    add(CloseTab::DEF, |_| Ok(Box::new(CloseTab)));
    add(ToggleTab::DEF, |_| Ok(Box::new(ToggleTab)));
    add(ToggleTabBar::DEF, |_| Ok(Box::new(ToggleTabBar)));
    add(ClosePane::DEF, |_| Ok(Box::new(ClosePane)));
    add(CloseOtherPanes::DEF, |_| Ok(Box::new(CloseOtherPanes)));
    add(TogglePaneWiden::DEF, |_| Ok(Box::new(TogglePaneWiden)));
    add(DetachPane::DEF, |_| Ok(Box::new(DetachPane)));
    add(ReattachPane::DEF, |_| Ok(Box::new(ReattachPane)));
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
    add(OpenBufferPicker::DEF, |_| Ok(Box::new(OpenBufferPicker)));
    add(OpenWorkspaceFileFinder::DEF, |_| {
        Ok(Box::new(OpenWorkspaceFileFinder))
    });
    add(FileFinderSelectPrev::DEF, |_| {
        Ok(Box::new(FileFinderSelectPrev))
    });
    add(FileFinderSelectNext::DEF, |_| {
        Ok(Box::new(FileFinderSelectNext))
    });
    add(FileFinderPageUp::DEF, |_| Ok(Box::new(FileFinderPageUp)));
    add(FileFinderPageDown::DEF, |_| {
        Ok(Box::new(FileFinderPageDown))
    });
    add(FileFinderScopeToggle::DEF, |_| {
        Ok(Box::new(FileFinderScopeToggle))
    });
    add(FileFinderComplete::DEF, |_| {
        Ok(Box::new(FileFinderComplete))
    });
    add(OpenHelp::DEF, |_| Ok(Box::new(OpenHelp)));
    add(Diff::DEF, |_| Ok(Box::new(Diff)));
    add(ToggleDiff::DEF, |_| Ok(Box::new(ToggleDiff)));
    add(Conflict::DEF, |_| Ok(Box::new(Conflict)));
    add(CloseConflict::DEF, |_| Ok(Box::new(CloseConflict)));
    add(ConflictPickOurs::DEF, |_| Ok(Box::new(ConflictPickOurs)));
    add(ConflictPickTheirs::DEF, |_| {
        Ok(Box::new(ConflictPickTheirs))
    });
    add(ConflictPickBoth::DEF, |_| Ok(Box::new(ConflictPickBoth)));
    add(ConflictPickOursLine::DEF, |_| {
        Ok(Box::new(ConflictPickOursLine))
    });
    add(ConflictPickTheirsLine::DEF, |_| {
        Ok(Box::new(ConflictPickTheirsLine))
    });
    add(ConflictResetChunk::DEF, |_| {
        Ok(Box::new(ConflictResetChunk))
    });
    add(ConflictNextChunk::DEF, |_| Ok(Box::new(ConflictNextChunk)));
    add(ConflictPrevChunk::DEF, |_| Ok(Box::new(ConflictPrevChunk)));
    add(ConflictNextFile::DEF, |_| Ok(Box::new(ConflictNextFile)));
    add(ConflictPrevFile::DEF, |_| Ok(Box::new(ConflictPrevFile)));
    add(ConflictApply::DEF, |_| Ok(Box::new(ConflictApply)));
    add(StageHunk::DEF, |_| Ok(Box::new(StageHunk)));
    add(UnstageHunk::DEF, |_| Ok(Box::new(UnstageHunk)));
    add(ToggleStageHunk::DEF, |_| Ok(Box::new(ToggleStageHunk)));
    add(StageLine::DEF, |_| Ok(Box::new(StageLine)));
    add(UnstageLine::DEF, |_| Ok(Box::new(UnstageLine)));
    add(ToggleStageLine::DEF, |_| Ok(Box::new(ToggleStageLine)));
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
    add(GotoDeclaration::DEF, |_| Ok(Box::new(GotoDeclaration)));
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
    add(SymbolFinderSelectPrev::DEF, |_| {
        Ok(Box::new(SymbolFinderSelectPrev))
    });
    add(SymbolFinderSelectNext::DEF, |_| {
        Ok(Box::new(SymbolFinderSelectNext))
    });
    add(SymbolFinderComplete::DEF, |_| {
        Ok(Box::new(SymbolFinderComplete))
    });
    add(SymbolFinderPageUp::DEF, |_| {
        Ok(Box::new(SymbolFinderPageUp))
    });
    add(SymbolFinderPageDown::DEF, |_| {
        Ok(Box::new(SymbolFinderPageDown))
    });
    add(OpenWorkspaceSymbolPicker::DEF, |_| {
        Ok(Box::new(OpenWorkspaceSymbolPicker))
    });
    add(FormatSelections::DEF, |_| Ok(Box::new(FormatSelections)));
    add(Format::DEF, |_| Ok(Box::new(Format)));
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
    add(OpenCodeSearch::DEF, |_| Ok(Box::new(OpenCodeSearch)));
    add(CodeSearchNext::DEF, |_| Ok(Box::new(CodeSearchNext)));
    add(CodeSearchPrev::DEF, |_| Ok(Box::new(CodeSearchPrev)));
    add(CodeSearchPageDown::DEF, |_| {
        Ok(Box::new(CodeSearchPageDown))
    });
    add(CodeSearchPageUp::DEF, |_| Ok(Box::new(CodeSearchPageUp)));
    add(CodeSearchSelect::DEF, |_| Ok(Box::new(CodeSearchSelect)));
    add(CodeSearchClose::DEF, |_| Ok(Box::new(CodeSearchClose)));
    add(CodeSearchModeToggle::DEF, |_| {
        Ok(Box::new(CodeSearchModeToggle))
    });
    add(JumplistPickerNext::DEF, |_| {
        Ok(Box::new(JumplistPickerNext))
    });
    add(JumplistPickerPrev::DEF, |_| {
        Ok(Box::new(JumplistPickerPrev))
    });
    add(JumplistPickerSelect::DEF, |_| {
        Ok(Box::new(JumplistPickerSelect))
    });
    add(JumplistPickerClose::DEF, |_| {
        Ok(Box::new(JumplistPickerClose))
    });
    add(DiagnosticsPickerNext::DEF, |_| {
        Ok(Box::new(DiagnosticsPickerNext))
    });
    add(DiagnosticsPickerPrev::DEF, |_| {
        Ok(Box::new(DiagnosticsPickerPrev))
    });
    add(DiagnosticsPickerSelect::DEF, |_| {
        Ok(Box::new(DiagnosticsPickerSelect))
    });
    add(DiagnosticsPickerClose::DEF, |_| {
        Ok(Box::new(DiagnosticsPickerClose))
    });
    add(LocationPickerNext::DEF, |_| {
        Ok(Box::new(LocationPickerNext))
    });
    add(LocationPickerPrev::DEF, |_| {
        Ok(Box::new(LocationPickerPrev))
    });
    add(LocationPickerSelect::DEF, |_| {
        Ok(Box::new(LocationPickerSelect))
    });
    add(LocationPickerClose::DEF, |_| {
        Ok(Box::new(LocationPickerClose))
    });
    add(SplitSelection::DEF, |_| Ok(Box::new(SplitSelection)));
    add(SelectRegex::DEF, |_| Ok(Box::new(SelectRegex)));
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
    add(ForceSaveBuffer::DEF, |_| Ok(Box::new(ForceSaveBuffer)));
    add(OpenConfig::DEF, |params| {
        // The palette autospaces a submitted command, so a bare `open-config`
        // arrives with an empty argument. That means the default target, not a
        // target literally named "".
        let target = params
            .first()
            .map(|param| {
                param.as_string().context(WrongKindSnafu {
                    name: "target",
                    expected: ParamKind::String,
                })
            })
            .transpose()?
            .filter(|target| !target.is_empty())
            .map(str::to_owned);
        Ok(Box::new(OpenConfig { target }))
    });
    add(ToggleMinimap::DEF, |_| Ok(Box::new(ToggleMinimap)));
    add(ToggleWrap::DEF, |_| Ok(Box::new(ToggleWrap)));
    add(ToggleKeyHints::DEF, |_| Ok(Box::new(ToggleKeyHints)));
    add(DismissKeyHints::DEF, |_| Ok(Box::new(DismissKeyHints)));
    add(WriteQuit::DEF, |_| Ok(Box::new(WriteQuit)));
    add(CloseBuffer::DEF, |_| Ok(Box::new(CloseBuffer)));
    add(AcceptCompletion::DEF, |_| Ok(Box::new(AcceptCompletion)));
    add(SmartTab::DEF, |_| Ok(Box::new(SmartTab)));
    add(InsertTab::DEF, |_| Ok(Box::new(InsertTab)));
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
    add(ReplaceWithYanked::DEF, |_| Ok(Box::new(ReplaceWithYanked)));
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
    add(EnterInsertMode::DEF, |_| Ok(Box::new(EnterInsertMode)));
    add(AppendMode::DEF, |_| Ok(Box::new(AppendMode)));
    add(InsertAtLineEnd::DEF, |_| Ok(Box::new(InsertAtLineEnd)));
    add(InsertAtLineStart::DEF, |_| Ok(Box::new(InsertAtLineStart)));
    add(OpenBelow::DEF, |_| Ok(Box::new(OpenBelow)));
    add(OpenAbove::DEF, |_| Ok(Box::new(OpenAbove)));
    add(ReplaceChar::DEF, |_| Ok(Box::new(ReplaceChar)));
    add(GotoFileStart::DEF, |_| Ok(Box::new(GotoFileStart)));
    add(GotoLastLine::DEF, |_| Ok(Box::new(GotoLastLine)));
    add(GotoLineNumber::DEF, |_| Ok(Box::new(GotoLineNumber)));
    add(GotoColumn::DEF, |_| Ok(Box::new(GotoColumn)));
    add(GotoCaller::DEF, |_| Ok(Box::new(GotoCaller)));
    add(GotoCallee::DEF, |_| Ok(Box::new(GotoCallee)));
    add(GotoReferences::DEF, |_| Ok(Box::new(GotoReferences)));
    add(GotoImplementors::DEF, |_| Ok(Box::new(GotoImplementors)));
    add(GotoDiffCallerUp::DEF, |_| Ok(Box::new(GotoDiffCallerUp)));
    add(GotoDiffCalleeDown::DEF, |_| {
        Ok(Box::new(GotoDiffCalleeDown))
    });
    add(MarkTrailStart::DEF, |_| Ok(Box::new(MarkTrailStart)));
    add(MarkTrailEnd::DEF, |_| Ok(Box::new(MarkTrailEnd)));
    add(TrailNext::DEF, |_| Ok(Box::new(TrailNext)));
    add(TrailPrev::DEF, |_| Ok(Box::new(TrailPrev)));
    add(ExtendGotoColumn::DEF, |_| Ok(Box::new(ExtendGotoColumn)));
    add(GotoNextChange::DEF, |_| Ok(Box::new(GotoNextChange)));
    add(GotoPrevChange::DEF, |_| Ok(Box::new(GotoPrevChange)));
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
    add(DeleteSelectionNoYank::DEF, |_| {
        Ok(Box::new(DeleteSelectionNoYank))
    });
    add(ChangeSelection::DEF, |_| Ok(Box::new(ChangeSelection)));
    add(Undo::DEF, |_| Ok(Box::new(Undo)));
    add(Redo::DEF, |_| Ok(Box::new(Redo)));
    add(CommitUndoCheckpoint::DEF, |_| {
        Ok(Box::new(CommitUndoCheckpoint))
    });
    add(IndentSelection::DEF, |_| Ok(Box::new(IndentSelection)));
    add(UnindentSelection::DEF, |_| Ok(Box::new(UnindentSelection)));
    add(ToggleComments::DEF, |_| Ok(Box::new(ToggleComments)));
    add(ToggleSyntaxHighlight::DEF, |_| {
        Ok(Box::new(ToggleSyntaxHighlight))
    });
    add(ToggleLspStatus::DEF, |_| Ok(Box::new(ToggleLspStatus)));
    add(ToggleInlayHints::DEF, |_| Ok(Box::new(ToggleInlayHints)));
    add(ExtendToLineStart::DEF, |_| Ok(Box::new(ExtendToLineStart)));
    add(ExtendToLineEnd::DEF, |_| Ok(Box::new(ExtendToLineEnd)));
    add(ExtendToFileStart::DEF, |_| Ok(Box::new(ExtendToFileStart)));
    add(ExtendToLastLine::DEF, |_| Ok(Box::new(ExtendToLastLine)));
    add(CollapseSelection::DEF, |_| Ok(Box::new(CollapseSelection)));
    add(FlipSelections::DEF, |_| Ok(Box::new(FlipSelections)));
    add(EnsureSelectionsForward::DEF, |_| {
        Ok(Box::new(EnsureSelectionsForward))
    });
    add(SelectAll::DEF, |_| Ok(Box::new(SelectAll)));
    add(SelectLineBelow::DEF, |_| Ok(Box::new(SelectLineBelow)));
    add(ExtendToLineBounds::DEF, |_| {
        Ok(Box::new(ExtendToLineBounds))
    });
    add(ShrinkToLineBounds::DEF, |_| {
        Ok(Box::new(ShrinkToLineBounds))
    });
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
    add(RotateSelectionContentsForward::DEF, |_| {
        Ok(Box::new(RotateSelectionContentsForward))
    });
    add(RotateSelectionContentsBackward::DEF, |_| {
        Ok(Box::new(RotateSelectionContentsBackward))
    });
    add(JoinSelections::DEF, |_| Ok(Box::new(JoinSelections)));
    add(JoinSelectionsSpace::DEF, |_| {
        Ok(Box::new(JoinSelectionsSpace))
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
    add(AutoReload::DEF, |params| {
        let raw = params
            .first()
            .context(MissingSnafu { name: "state" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "state",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(AutoReload {
            state: raw.to_string(),
        }))
    });
    add(AutoReloadConfig::DEF, |params| {
        let raw = params
            .first()
            .context(MissingSnafu { name: "state" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "state",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(AutoReloadConfig {
            state: raw.to_string(),
        }))
    });
    add(OpenBuffer::DEF, |params| {
        let raw = params
            .first()
            .context(MissingSnafu { name: "path" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "path",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(OpenBuffer {
            path: PathBuf::from(raw),
        }))
    });
    add(OpenRun::DEF, |_| Ok(Box::new(OpenRun)));
    add(SpawnClaude::DEF, |_| Ok(Box::new(SpawnClaude)));
    add(Terminal::DEF, |_| Ok(Box::new(Terminal)));
    add(RunSubmit::DEF, |_| Ok(Box::new(RunSubmit)));
    add(RunInterrupt::DEF, |_| Ok(Box::new(RunInterrupt)));
    add(RunModalDismiss::DEF, |_| Ok(Box::new(RunModalDismiss)));
    add(RunHistoryPrev::DEF, |_| Ok(Box::new(RunHistoryPrev)));
    add(RunHistoryNext::DEF, |_| Ok(Box::new(RunHistoryNext)));
    add(HelpSelectPrev::DEF, |_| Ok(Box::new(HelpSelectPrev)));
    add(HelpSelectNext::DEF, |_| Ok(Box::new(HelpSelectNext)));
    add(HelpComplete::DEF, |_| Ok(Box::new(HelpComplete)));
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
    add(RebaseConflictTakeOurs::DEF, |_| {
        Ok(Box::new(RebaseConflictTakeOurs))
    });
    add(RebaseConflictTakeTheirs::DEF, |_| {
        Ok(Box::new(RebaseConflictTakeTheirs))
    });
    add(RebaseConflictSkipEntry::DEF, |_| {
        Ok(Box::new(RebaseConflictSkipEntry))
    });
    add(RebaseConflictNextFile::DEF, |_| {
        Ok(Box::new(RebaseConflictNextFile))
    });
    add(RebaseConflictPrevFile::DEF, |_| {
        Ok(Box::new(RebaseConflictPrevFile))
    });
    add(RebaseConflictApply::DEF, |_| {
        Ok(Box::new(RebaseConflictApply))
    });
    add(RebaseConflictAbort::DEF, |_| {
        Ok(Box::new(RebaseConflictAbort))
    });
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
    add(SetTheme::DEF, |params| {
        let raw = params
            .first()
            .context(MissingSnafu { name: "name" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "name",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(SetTheme {
            name: raw.to_owned(),
        }))
    });
    add(NewWorkspace::DEF, |_| Ok(Box::new(NewWorkspace)));
    add(CopyWorkspace::DEF, |_| Ok(Box::new(CopyWorkspace)));
    add(SwitchWorkspace::DEF, |_| Ok(Box::new(SwitchWorkspace)));
    add(WorkspacePickerNext::DEF, |_| {
        Ok(Box::new(WorkspacePickerNext))
    });
    add(WorkspacePickerComplete::DEF, |_| {
        Ok(Box::new(WorkspacePickerComplete))
    });
    add(WorkspacePickerPrev::DEF, |_| {
        Ok(Box::new(WorkspacePickerPrev))
    });
    add(WorkspacePickerSelect::DEF, |_| {
        Ok(Box::new(WorkspacePickerSelect))
    });
    add(WorkspacePickerClose::DEF, |_| {
        Ok(Box::new(WorkspacePickerClose))
    });
    add(CloseWorkspace::DEF, |_| Ok(Box::new(CloseWorkspace)));
    add(RenameWorkspace::DEF, |params| {
        let raw = params
            .first()
            .context(MissingSnafu { name: "name" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "name",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(RenameWorkspace {
            name: raw.to_owned(),
        }))
    });
    add(SetCwd::DEF, |params| {
        let raw = params
            .first()
            .context(MissingSnafu { name: "path" })?
            .as_string()
            .context(WrongKindSnafu {
                name: "path",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(SetCwd {
            path: raw.to_owned(),
        }))
    });
    add(ShowCwd::DEF, |_| Ok(Box::new(ShowCwd)));
    add(ReloadEnv::DEF, |_| Ok(Box::new(ReloadEnv)));
    add(SubmitPromptInput::DEF, |_| Ok(Box::new(SubmitPromptInput)));
    add(CancelPromptInput::DEF, |_| Ok(Box::new(CancelPromptInput)));
    add(PromptInsertNewline::DEF, |_| {
        Ok(Box::new(PromptInsertNewline))
    });
    add(PaletteSelectPrev::DEF, |_| Ok(Box::new(PaletteSelectPrev)));
    add(PaletteSelectNext::DEF, |_| Ok(Box::new(PaletteSelectNext)));
    add(PaletteHistoryPrev::DEF, |_| {
        Ok(Box::new(PaletteHistoryPrev))
    });
    add(PaletteHistoryNext::DEF, |_| {
        Ok(Box::new(PaletteHistoryNext))
    });
    add(PalettePageUp::DEF, |_| Ok(Box::new(PalettePageUp)));
    add(PalettePageDown::DEF, |_| Ok(Box::new(PalettePageDown)));
    add(PaletteScopeToggle::DEF, |_| {
        Ok(Box::new(PaletteScopeToggle))
    });
    add(PaletteComplete::DEF, |_| Ok(Box::new(PaletteComplete)));

    map
}

pub fn lookup(name: &str) -> Option<&'static RegistryEntry> {
    REGISTRY.get_or_init(init).get(name)
}

pub fn all() -> impl Iterator<Item = &'static RegistryEntry> {
    REGISTRY.get_or_init(init).values()
}

/// Resolve `token` to a registered action by exact name first, then by
/// case-insensitive [`RegistryEntry::command_name`], then by a case-insensitive
/// alias match.
///
/// A full name always wins, so a command stays reachable even if another action
/// spells its name as a command name or alias.
pub fn lookup_alias(token: &str) -> Option<&'static RegistryEntry> {
    if let Some(entry) = lookup(token) {
        return Some(entry);
    }
    if let Some(entry) = all().find(|entry| entry.command_name.eq_ignore_ascii_case(token)) {
        return Some(entry);
    }
    all().find(|entry| {
        entry
            .def
            .aliases()
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(token))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZERO_ARG_NAMES: &[&str] = &[
        "Quit",
        "WriteQuit",
        "QuitAll",
        "QuitAllConfirm",
        "QuitAllCancel",
        "ShowVersion",
        "OpenLogs",
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
        "TogglePaneWiden",
        "OpenCommandPalette",
        "OpenFileFinder",
        "OpenFileFinderHSplit",
        "OpenFileFinderVSplit",
        "OpenChangedFilePicker",
        "OpenBufferPicker",
        "OpenWorkspaceFileFinder",
        "FileFinderSelectPrev",
        "FileFinderSelectNext",
        "FileFinderPageUp",
        "FileFinderPageDown",
        "FileFinderScopeToggle",
        "FileFinderComplete",
        "OpenHelp",
        "Diff",
        "ToggleDiff",
        "Conflict",
        "CloseConflict",
        "JumpToMoveSource",
        "JumpToMoveTarget",
        "JumpToNextMoveSource",
        "JumpToPrevMoveSource",
        "QueryMoveRelationships",
        "GotoNextDiagnostic",
        "GotoPrevDiagnostic",
        "GotoDefinition",
        "GotoDeclaration",
        "GotoTypeDefinition",
        "GotoImplementation",
        "GotoCaller",
        "GotoCallee",
        "GotoReferences",
        "GotoImplementors",
        "GotoDiffCallerUp",
        "GotoDiffCalleeDown",
        "MarkTrailStart",
        "MarkTrailEnd",
        "TrailNext",
        "TrailPrev",
        "Hover",
        "CodeAction",
        "RenameSymbol",
        "OpenSymbolPicker",
        "OpenWorkspaceSymbolPicker",
        "FormatSelections",
        "Format",
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
        "DeleteSelectionNoYank",
        "ChangeSelection",
        "PasteAfter",
        "PasteBefore",
        "YankToClipboard",
        "YankMainToClipboard",
        "PasteClipboardAfter",
        "PasteClipboardBefore",
        "SelectRegister",
        "InsertRegister",
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
        "ToggleSyntaxHighlight",
        "ToggleLspStatus",
        "ToggleInlayHints",
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
        "RebaseConflictTakeOurs",
        "RebaseConflictTakeTheirs",
        "RebaseConflictSkipEntry",
        "RebaseConflictNextFile",
        "RebaseConflictPrevFile",
        "RebaseConflictApply",
        "RebaseConflictAbort",
        "OpenRun",
        "SpawnClaude",
        "terminal",
        "RunSubmit",
        "RunInterrupt",
        "RunModalDismiss",
        "RunHistoryPrev",
        "RunHistoryNext",
        "ToggleDockRight",
        "ToggleDockLeft",
        "NewWorkspace",
        "CopyWorkspace",
        "SwitchWorkspace",
        "WorkspacePickerNext",
        "WorkspacePickerPrev",
        "WorkspacePickerComplete",
        "WorkspacePickerSelect",
        "WorkspacePickerClose",
        "JumplistPickerNext",
        "JumplistPickerPrev",
        "JumplistPickerSelect",
        "JumplistPickerClose",
        "DiagnosticsPickerNext",
        "DiagnosticsPickerPrev",
        "DiagnosticsPickerSelect",
        "DiagnosticsPickerClose",
        "LocationPickerNext",
        "LocationPickerPrev",
        "LocationPickerSelect",
        "LocationPickerClose",
        "OpenCodeSearch",
        "CodeSearchNext",
        "CodeSearchPrev",
        "CodeSearchPageDown",
        "CodeSearchPageUp",
        "CodeSearchSelect",
        "CodeSearchClose",
        "CodeSearchModeToggle",
        "CloseWorkspace",
        "ShowCwd",
        "ReloadEnv",
        "HelpSelectPrev",
        "HelpSelectNext",
        "HelpComplete",
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
        "PalettePageUp",
        "PalettePageDown",
        "PaletteScopeToggle",
        "PaletteComplete",
    ];

    #[test]
    fn kebab_rewrites_pascal_case_names() {
        assert_eq!(kebab("SetCwd"), "set-cwd");
        assert_eq!(kebab("ToggleLspStatus"), "toggle-lsp-status");
        assert_eq!(kebab("QuitAll"), "quit-all");
        assert_eq!(
            kebab("terminal"),
            "terminal",
            "already lowercase passes through"
        );
        assert_eq!(kebab("Quit"), "quit", "a single word loses only its case");
    }

    #[test]
    fn lookup_alias_resolves_a_command_name() {
        let entry = lookup_alias("set-cwd").expect("set-cwd resolves");
        assert_eq!(entry.def.name(), "SetCwd");
        assert_eq!(
            lookup_alias("SET-CWD").map(|e| e.def.name()),
            Some("SetCwd"),
            "command names resolve case-insensitively"
        );
    }

    /// Curating a command name lets the identical alias be deleted, so this
    /// pins that every such token still reaches the action it used to. A
    /// deletion that outran its replacement would make the command silently
    /// unreachable by the word users already type.
    #[test]
    fn tokens_whose_aliases_were_dropped_still_resolve() {
        let cases = [
            ("theme", "SetTheme"),
            ("pwd", "ShowCwd"),
            ("reload-env", "ReloadEnv"),
            ("minimap", "ToggleMinimap"),
            ("wrap", "ToggleWrap"),
            ("hints", "ToggleKeyHints"),
            ("config", "OpenConfig"),
            ("logs", "OpenLogs"),
            ("auto-reload", "AutoReload"),
            ("auto-reload-config", "AutoReloadConfig"),
            ("open", "OpenFile"),
            ("buffer", "OpenBuffer"),
            ("write", "SaveBuffer"),
        ];
        for (token, action) in cases {
            assert_eq!(
                lookup_alias(token).map(|e| e.def.name()),
                Some(action),
                "{token} must still reach {action}"
            );
        }
    }

    /// The short forms kept or added alongside the curated names.
    #[test]
    fn short_aliases_resolve() {
        for (token, action) in [
            ("q", "Quit"),
            ("qa", "QuitAll"),
            ("w", "SaveBuffer"),
            ("o", "OpenFile"),
            ("b", "OpenBuffer"),
            ("cd", "SetCwd"),
        ] {
            assert_eq!(
                lookup_alias(token).map(|e| e.def.name()),
                Some(action),
                "{token} must reach {action}"
            );
        }
    }

    /// Every way of addressing an action has to name exactly one of them, or
    /// typing that token in the palette silently reaches whichever entry the
    /// hash map happened to yield first.
    #[test]
    fn no_token_resolves_to_two_different_actions() {
        let mut owner: HashMap<String, &'static str> = HashMap::new();
        let mut collisions: Vec<String> = Vec::new();

        for entry in all() {
            let name = entry.def.name();
            let tokens = std::iter::once(name.to_lowercase())
                .chain(std::iter::once(entry.command_name.to_lowercase()))
                .chain(entry.def.aliases().iter().map(|a| a.to_lowercase()));

            for token in tokens {
                match owner.insert(token.clone(), name) {
                    Some(prev) if prev != name => {
                        collisions.push(format!("{token:?}: {prev} and {name}"))
                    },
                    _ => {},
                }
            }
        }

        collisions.sort();
        assert!(
            collisions.is_empty(),
            "tokens reaching two actions:\n{}",
            collisions.join("\n")
        );
    }

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
        assert!(lookup("SetVar").is_none());
    }

    #[test]
    fn lookup_alias_resolves_alias_and_exact_name_wins() {
        // An alias resolves to its action, case-insensitively.
        assert_eq!(lookup_alias("o").expect("o").def.name(), "OpenFile");
        assert_eq!(lookup_alias("EDIT").expect("EDIT").def.name(), "OpenFile");
        assert!(lookup_alias("not-a-command").is_none());

        // Every exact action name resolves to that action, never to one that
        // merely lists the name as an alias.
        for entry in all() {
            let name = entry.def.name();
            assert_eq!(lookup_alias(name).expect(name).def.name(), name);
        }
    }

    #[test]
    fn force_save_buffer_aliases_resolve() {
        for token in ["w!", "write!", "W!"] {
            assert_eq!(
                lookup_alias(token).expect(token).def.name(),
                "ForceSaveBuffer",
            );
        }
    }

    #[test]
    fn save_buffer_aliases_resolve() {
        for token in ["w", "write", "W"] {
            assert_eq!(lookup_alias(token).expect(token).def.name(), "SaveBuffer");
        }
    }

    #[test]
    fn write_quit_aliases_resolve() {
        for token in ["wq", "x", "WQ"] {
            assert_eq!(lookup_alias(token).expect(token).def.name(), "WriteQuit");
        }
    }

    #[test]
    fn terminal_alias_resolves() {
        for token in ["term", "TERM"] {
            assert_eq!(lookup_alias(token).expect(token).def.name(), "terminal");
        }
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
    fn focus_pane_factory_rejects_fractional_and_nonpositive() {
        let entry = lookup("FocusPane").expect("FocusPane");
        for bad in [1.5, 0.0, -1.0] {
            assert!(
                matches!(
                    (entry.create)(&[ParamValue::Number(bad)]),
                    Err(ParamError::ParseFailure { .. })
                ),
                "{bad} must be rejected",
            );
        }
        let action = (entry.create)(&[ParamValue::Number(2.0)]).expect("valid index");
        let focus = action
            .as_any()
            .downcast_ref::<FocusPane>()
            .expect("FocusPane");
        assert_eq!(focus.index, 2);
    }

    #[test]
    fn rename_tab_factory_keeps_an_empty_name_distinct_from_no_name() {
        let entry = lookup("RenameTab").expect("RenameTab");
        let name = |params: &[ParamValue]| {
            (entry.create)(params)
                .expect("valid")
                .as_any()
                .downcast_ref::<RenameTab>()
                .expect("RenameTab")
                .name
                .clone()
        };

        // A bare keybinding passes no argument and opens the palette. The
        // palette's empty submit passes `""` and clears. Folding empty to None
        // would collapse the two.
        assert_eq!(name(&[]), None, "no argument stays None");
        assert_eq!(
            name(&[ParamValue::String(String::new())]),
            Some(String::new()),
            "an empty argument is kept, not folded to None"
        );
        assert_eq!(
            name(&[ParamValue::String("work".into())]),
            Some("work".to_string())
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
        // + 1 DeleteSelectionNoYank.
        // + 1 ChangeSelection.
        // + 2 line indent ops (IndentSelection/UnindentSelection).
        // + 1 AlignSelections.
        // + 1 Undo.
        // + 1 Redo.
        // + 2 GotoNextChange/GotoPrevChange.
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
        // + 1 SplitSelection.
        // + 2 KeepSelections / RemoveSelections.
        // + 2 RecordMacro / ReplayMacro.
        // + 5 ShellPipe / ShellPipeTo / ShellInsertOutput / ShellAppendOutput / ShellKeepPipe.
        // + 1 SaveBuffer.
        // + 1 ForceSaveBuffer.
        // + 1 WriteQuit.
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
        // + 4 SymbolFinder SelectPrev/Next/PageUp/PageDown.
        // + 1 SymbolFinderComplete.
        // + 1 HelpComplete.
        // + 1 WorkspacePickerComplete.
        // + 4 NewTab, CloseTab, GotoTab, ToggleTab.
        // + 1 ToggleTabBar.
        // + 1 RenameTab.
        // + 2 NextTab, PrevTab.
        // + 2 CodeSearchPageDown, CodeSearchPageUp.
        // + 1 OpenWorkspaceSymbolPicker.
        // + 1 FormatSelections.
        // + 1 Format.
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
        // + 1 SpawnClaude.
        // + 1 terminal.
        // + 2 GotoCaller, GotoCallee.
        // + 1 GotoReferences.
        // + 1 GotoImplementors.
        // + 2 GotoDiffCallerUp, GotoDiffCalleeDown.
        // + 4 MarkTrailStart, MarkTrailEnd, TrailNext, TrailPrev.
        // + 2 FileFinderPageUp, FileFinderPageDown.
        // + 1 FileFinderComplete.
        // + 2 PalettePageUp, PalettePageDown.
        // + 1 OpenBuffer.
        // + 1 ToggleDiff.
        // + 3 StageHunk, UnstageHunk, ToggleStageHunk.
        // + 3 StageLine, UnstageLine, ToggleStageLine.
        // + 1 OpenConfig.
        // + 1 ToggleMinimap.
        // + 1 ToggleWrap.
        // + 1 ToggleKeyHints.
        // + 1 DismissKeyHints.
        // + 1 ToggleSyntaxHighlight.
        // + 1 ToggleLspStatus.
        // + 1 ToggleInlayHints.
        // + 1 ShowVersion.
        // + 1 OpenLogs.
        // + 1 GotoDeclaration.
        // + 4 JumplistPicker Next/Prev/Select/Close.
        // + 4 DiagnosticsPicker Next/Prev/Select/Close.
        // + 4 LocationPicker Next/Prev/Select/Close.
        // + 1 SetCwd.
        // + 1 ShowCwd.
        // + 1 ReloadEnv.
        // + 2 AppendMode / InsertAtLineEnd.
        // + 1 EnterInsertMode.
        // + 1 InsertAtLineStart.
        // + 1 InsertTab.
        // + 1 SelectRegex.
        // + 2 ExtendToLineBounds / ShrinkToLineBounds.
        // + 1 EnsureSelectionsForward.
        // + 2 RotateSelectionContentsForward/Backward.
        // + 1 ReplaceWithYanked.
        // + 2 JoinSelections / JoinSelectionsSpace.
        // + 1 AutoReload.
        // + 1 AutoReloadConfig.
        // + 1 PaletteComplete.
        // + 1 FocusPane.
        // + 2 PaletteHistoryPrev/Next.
        // + 1 SetTheme.
        // + 2 DetachPane, ReattachPane.
        // + 1 TogglePaneWiden.
        // + 5 OpenCodeSearch + CodeSearch Next/Prev/Select/Close.
        // + 1 CodeSearchModeToggle.
        // + 2 Conflict, CloseConflict.
        // + 6 ConflictPickOurs/Theirs/Both, ConflictPickOursLine/TheirsLine, ConflictResetChunk.
        // + 2 ConflictNextChunk, ConflictPrevChunk.
        // + 2 ConflictNextFile, ConflictPrevFile.
        // + 1 ConflictApply.
        // + 1 OpenWorkspaceFileFinder.
        assert_eq!(all().count(), 391);
    }

    #[test]
    fn all_have_descriptions() {
        for entry in all() {
            assert!(!entry.def.short_desc().is_empty(), "{}", entry.def.name());
            assert!(!entry.def.long_desc().is_empty(), "{}", entry.def.name());
        }
    }
}
