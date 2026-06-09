//! Command palette picker delegate.
//!
//! Lists every [`stoat_action::ActionDef`] whose
//! [`ActionDef::palette_visible`] returns true, fuzzy-ranks them
//! against the picker's query, and on confirm either constructs the
//! action via [`registry::RegistryEntry::create`] and dispatches it
//! through [`Workspace::dispatch_action`], or transitions into a
//! [`PalettePhase::CollectArgs`] state that walks the user through
//! providing each parameter in sequence before dispatch.

use crate::{
    commit_list::CommitListItem,
    editor::Editor,
    globals::{GitHostGlobal, LanguageRegistry},
    item::ItemKind,
    picker::{
        match_and_rank_aliased, match_highlight_runs, Picker, PickerDelegate, PickerSecondary,
    },
    rebase_item::RebaseItem,
    review_item::ReviewItem,
    run_pane,
    settings::Settings,
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, App, Context, DismissEvent, Entity, HighlightStyle, IntoElement,
    ParentElement, SharedString, Styled, StyledText, Task, WeakEntity, Window,
};
use std::collections::VecDeque;
use stoat::rebase::RebasePause;
use stoat_action::{
    registry::{self, RegistryEntry},
    ActionKind, ParamValue,
};

/// Maximum number of confirmed queries retained for history recall.
pub(crate) const HISTORY_LIMIT: usize = 50;

/// Record `query` as the most recent history entry. An earlier
/// occurrence is removed first so each query appears once at its
/// latest position; the oldest entries are dropped once the list
/// exceeds `limit`. Callers pass a non-empty, trimmed query.
pub(crate) fn record_query_capped(history: &mut VecDeque<String>, query: String, limit: usize) {
    if let Some(pos) = history.iter().position(|q| q == &query) {
        history.remove(pos);
    }
    history.push_back(query);
    while history.len() > limit {
        history.pop_front();
    }
}

pub struct CommandPaletteDelegate {
    /// Every palette-visible entry, captured at construction time.
    entries: Vec<&'static RegistryEntry>,
    /// Index into [`Self::entries`] plus the matched character
    /// indices for the active query, ordered for display. Always
    /// empty while [`Self::phase`] is [`PalettePhase::CollectArgs`].
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    workspace: WeakEntity<Workspace>,
    phase: PalettePhase,
    /// Weak handle to the picker's query editor, captured in
    /// [`Self::on_attach`]. Used to clear the editor text on phase
    /// transitions inside [`Self::confirm`], which cannot reach the
    /// picker through its own context because the picker is being
    /// mutated while the delegate runs.
    query_editor: Option<WeakEntity<Editor>>,
    /// Listing mode -- [`PaletteScope::Active`] hides actions whose
    /// [`action_is_available`] reads `false` against the captured
    /// [`Self::availability`] snapshot. Flipped by
    /// [`Self::toggle_scope`] when the [`ActionKind::PaletteScopeToggle`]
    /// action reaches [`Self::handle_action`].
    scope: PaletteScope,
    /// Frozen [`Availability`] snapshot derived at open time. The
    /// palette is modal, so workspace state can't drift while it is
    /// up; refiltering reads this directly instead of recomputing.
    availability: Availability,
    /// Snapshot of the workspace's confirmed-query history taken at
    /// open, oldest first. Recall reads this local copy so navigation
    /// never re-enters the workspace lease the keystroke dispatch
    /// holds; confirms append here and to the durable workspace store.
    history: VecDeque<String>,
    /// Position into [`Self::history`] currently recalled into the
    /// query editor, or `None` when not navigating history.
    history_cursor: Option<usize>,
    /// The query text typed before history navigation began. Recall
    /// filters history to entries sharing this prefix, and restores it
    /// when the user walks forward past the newest entry.
    history_prefix: Option<String>,
}

/// Listing mode for [`CommandPaletteDelegate`]. Toggled between
/// values by [`PaletteScopeToggle`].
///
/// [`PaletteScopeToggle`]: stoat_action::PaletteScopeToggle
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteScope {
    /// Only actions applicable to the captured [`Availability`].
    Active,
    /// Every `palette_visible()` action, regardless of availability.
    All,
}

/// Snapshot of workspace state relevant to per-action availability.
/// Derived once at palette-open via [`Availability::from_workspace`]
/// so the scope filter is a cheap lookup on every keystroke.
///
/// Mirrors the TUI `stoat::command_palette::Availability` shape.
#[derive(Debug, Clone, Copy, Default)]
pub struct Availability {
    /// A `RebaseItem` is open in some pane (editable rebase plan).
    pub in_rebase_plan: bool,
    /// `Workspace::rebase_active.is_some()`: a rebase is mid-execution.
    pub in_rebase_exec: bool,
    /// The in-flight rebase is paused on [`RebasePause::Reword`].
    pub in_rebase_reword: bool,
    /// The in-flight rebase is paused on [`RebasePause::Conflict`].
    pub in_conflict: bool,
    /// A `ReviewItem` is open in some pane.
    pub review_open: bool,
    /// A `CommitListItem` is open in some pane.
    pub commits_open: bool,
    /// The focused pane's active item is a `Run` pane.
    pub run_focused: bool,
    /// The focused pane item is an [`ItemKind::Editor`]. Gates the
    /// buffer-editing action family so it hides when a non-editor pane
    /// (terminal, review, rebase, commits, run) is focused.
    pub editor_focused: bool,
    /// A git repository exists at the workspace root. Gates the VCS
    /// hunk/stage/blame action family.
    pub in_git_repo: bool,
    /// A language server is configured for the focused editor's language.
    /// Gates the LSP action family.
    pub lsp_configured: bool,
}

impl Availability {
    /// Derive the availability snapshot from the active GUI workspace.
    pub fn from_workspace(workspace: &Workspace, cx: &App) -> Self {
        let pane_tree = workspace.pane_tree().read(cx);
        let mut in_rebase_plan = false;
        let mut review_open = false;
        let mut commits_open = false;
        for pane_id in pane_tree.split_pane_ids() {
            let Some(pane) = pane_tree.pane(pane_id) else {
                continue;
            };
            pane.read(cx).items().iter().for_each(|item| {
                let view = item.to_any_view();
                if view.clone().downcast::<RebaseItem>().is_ok() {
                    in_rebase_plan = true;
                }
                if view.clone().downcast::<ReviewItem>().is_ok() {
                    review_open = true;
                }
                if view.downcast::<CommitListItem>().is_ok() {
                    commits_open = true;
                }
            });
        }

        let (in_rebase_reword, in_conflict) = workspace
            .rebase_active()
            .and_then(|a| a.pause.as_ref())
            .map(|p| {
                (
                    matches!(p, RebasePause::Reword { .. }),
                    matches!(p, RebasePause::Conflict { .. }),
                )
            })
            .unwrap_or((false, false));

        Self {
            in_rebase_plan,
            in_rebase_exec: workspace.rebase_active().is_some(),
            in_rebase_reword,
            in_conflict,
            review_open,
            commits_open,
            run_focused: run_pane::focused_run_pane(workspace, cx).is_some(),
            editor_focused: workspace
                .active_pane_item(cx)
                .map(|item| item.item_kind(cx))
                == Some(ItemKind::Editor),
            in_git_repo: cx
                .try_global::<GitHostGlobal>()
                .map(|g| g.0.discover(workspace.git_root()).is_some())
                .unwrap_or(false),
            lsp_configured: focused_editor_lsp_configured(workspace, cx),
        }
    }
}

/// Whether a language server is configured for the focused editor's
/// language. Resolves the focused pane's active [`Editor`] to a language
/// via [`LanguageRegistry::for_path`], then checks
/// [`Settings`]`::language_servers`. Returns `false` when the focused item
/// is not an editor, the buffer has no path or recognized language, or no
/// server is configured. Mirrors the per-action launch path in
/// [`Workspace`]'s LSP handlers, which surface unconfigured languages as
/// `NotFound` at launch time.
fn focused_editor_lsp_configured(workspace: &Workspace, cx: &App) -> bool {
    let language = {
        let Some(item) = workspace.active_pane_item(cx) else {
            return false;
        };
        let Ok(editor) = item.to_any_view().downcast::<Editor>() else {
            return false;
        };
        let read = editor.read(cx);
        let Some(path) = read.file_path() else {
            return false;
        };
        let Some(language) = cx
            .try_global::<LanguageRegistry>()
            .and_then(|registry| registry.0.for_path(path))
        else {
            return false;
        };
        language
    };
    cx.try_global::<Settings>().is_some_and(|settings| {
        settings
            .resolved
            .language_servers
            .contains_key(language.name)
    })
}

/// Whether `kind` should appear in the palette's Active scope given
/// `ctx`. Mirrors `stoat::command_palette::action_is_available`. All
/// scope bypasses this function entirely; actions not listed here
/// are always available (globally applicable like `Quit`,
/// `FocusLeft`, etc.).
pub(crate) fn action_is_available(kind: ActionKind, ctx: &Availability) -> bool {
    use ActionKind::*;

    match kind {
        AbortRebase | ExecuteRebase | RebaseNext | RebasePrev | RebaseMoveUp | RebaseMoveDown
        | SetRebaseOpPick | SetRebaseOpSquash | SetRebaseOpFixup | SetRebaseOpDrop
        | SetRebaseOpReword | SetRebaseOpEdit => ctx.in_rebase_plan,

        EnterRebase => ctx.commits_open,

        RebaseContinue => ctx.in_rebase_exec,
        RewordConfirm | RewordAbort => ctx.in_rebase_reword,

        ConflictTakeOurs | ConflictTakeTheirs | ConflictSkipEntry | ConflictNextFile
        | ConflictPrevFile | ConflictApply | ConflictAbort => ctx.in_conflict,

        ReviewNextChunk
        | ReviewPrevChunk
        | ReviewStageChunk
        | ReviewUnstageChunk
        | ReviewToggleStage
        | ReviewSkipChunk
        | ReviewRefresh
        | ReviewApplyStaged
        | CloseReview
        | ReviewRemoveSelected
        | ReviewNextCommit
        | ReviewPrevCommit
        | ReviewApproveHunk
        | ReviewToggleApproval
        | ReviewNextUnreviewedHunk
        | ReviewResetProgress
        | ReviewEnterLineSelect
        | ReviewLineSelectCancel
        | ReviewLineSelectToggle
        | ReviewLineSelectAll
        | ReviewLineSelectStage
        | ReviewLineSelectUnstage
        | ReviewRevertHunk
        | ReviewCycleComparisonMode
        | ReviewToggleFollow
        | ReviewToggleLive
        | JumpToMoveSource
        | JumpToMoveTarget
        | JumpToNextMoveSource
        | JumpToPrevMoveSource
        | QueryMoveRelationships => ctx.review_open,

        CloseCommits
        | CommitsNext
        | CommitsPrev
        | CommitsPageDown
        | CommitsPageUp
        | CommitsFirst
        | CommitsLast
        | CommitsRefresh
        | CommitsOpenReview
        | CommitsOpenBranchReview => ctx.commits_open,

        RunSubmit | RunInterrupt | RunHistoryPrev | RunHistoryNext => ctx.run_focused,

        GotoNextHunk | GotoPrevHunk | ToggleDiffHunkPanel | ToggleBlame | ToggleInlineBlame
        | GitToggleStageHunk | GitUnstageHunk | GitToggleStageLine => ctx.in_git_repo,

        CodeAction
        | FormatSelections
        | GotoDefinition
        | GotoImplementation
        | GotoReferences
        | GotoTypeDefinition
        | GotoNextDiagnostic
        | GotoPrevDiagnostic
        | Hover
        | RenameSymbol
        | OpenSymbolPicker
        | OpenWorkspaceSymbolPicker
        | OpenDiagnosticsPicker
        | OpenWorkspaceDiagnosticsPicker => ctx.lsp_configured,

        AcceptCompletion
        | AddSelectionAbove
        | AddSelectionBelow
        | AlignSelections
        | AlignViewBottom
        | AlignViewCenter
        | AlignViewTop
        | Append
        | CloseBuffer
        | CollapseSelection
        | CommitUndoCheckpoint
        | Decrement
        | DeleteBackward
        | DeleteForward
        | DeleteSelection
        | DeleteWordBackward
        | DeleteWordForward
        | ExpandSelection
        | ExtendDown
        | ExtendFindNextChar
        | ExtendFindPrevChar
        | ExtendGotoColumn
        | ExtendGotoFileStart
        | ExtendGotoFirstNonwhitespace
        | ExtendGotoLastLine
        | ExtendGotoWindowBottom
        | ExtendGotoWindowCenter
        | ExtendGotoWindowTop
        | ExtendLeft
        | ExtendMoveParentNodeEnd
        | ExtendMoveParentNodeStart
        | ExtendNextWordEnd
        | ExtendNextWordStart
        | ExtendPrevWordEnd
        | ExtendPrevWordStart
        | ExtendRight
        | ExtendSelectNextSibling
        | ExtendSelectPrevSibling
        | ExtendTillNextChar
        | ExtendTillPrevChar
        | ExtendToFileStart
        | ExtendToLastLine
        | ExtendToLineEnd
        | ExtendToLineStart
        | ExtendUp
        | FindNextChar
        | FindPrevChar
        | FlipSelections
        | FoldAll
        | FoldAtCursor
        | GotoColumn
        | GotoFileStart
        | GotoFirstNonwhitespace
        | GotoLastLine
        | GotoLineEnd
        | GotoLineNumber
        | GotoLineStart
        | GotoMark
        | GotoMarkExact
        | GotoNextClass
        | GotoNextFunction
        | GotoNextParagraph
        | GotoPrevClass
        | GotoPrevFunction
        | GotoPrevParagraph
        | GotoWindowBottom
        | GotoWindowCenter
        | GotoWindowTop
        | GotoWord
        | HalfPageDown
        | HalfPageUp
        | Increment
        | IndentSelection
        | Insert
        | InsertNewline
        | InsertRegister
        | JumpBackward
        | JumpForward
        | KeepPrimarySelection
        | KeepSelections
        | MatchBrackets
        | MoveDown
        | MoveLeft
        | MoveNextLongWordEnd
        | MoveNextLongWordStart
        | MoveNextWordEnd
        | MoveNextWordStart
        | MoveParentNodeEnd
        | MoveParentNodeStart
        | MovePrevLongWordEnd
        | MovePrevLongWordStart
        | MovePrevWordEnd
        | MovePrevWordStart
        | MoveRight
        | MoveUp
        | OpenAbove
        | OpenBelow
        | OpenJumplistPicker
        | OpenReverseSearchInput
        | OpenSearchInput
        | PageDown
        | PageUp
        | PasteAfter
        | PasteBefore
        | PasteClipboardAfter
        | PasteClipboardBefore
        | RecordMacro
        | Redo
        | RemovePrimarySelection
        | RemoveSelections
        | RepeatLastMotion
        | ReplaceChar
        | ReplayMacro
        | RotateSelectionsBackward
        | RotateSelectionsForward
        | SaveBuffer
        | SaveSelection
        | ScrollDown
        | ScrollUp
        | SearchNext
        | SearchPrev
        | SelectAll
        | SelectAllChildren
        | SelectAllSiblings
        | SelectLineBelow
        | SelectNextSibling
        | SelectPrevSibling
        | SelectRegister
        | SelectTextobjectAround
        | SelectTextobjectInner
        | SetMark
        | ShellAppendOutput
        | ShellInsertOutput
        | ShellKeepPipe
        | ShellPipe
        | ShellPipeTo
        | ShrinkSelection
        | SmartTab
        | SplitSelection
        | SplitSelectionOnNewline
        | SurroundAdd
        | SurroundDelete
        | SurroundReplace
        | SwitchCase
        | SwitchToLowercase
        | SwitchToUppercase
        | TillNextChar
        | TillPrevChar
        | ToggleComments
        | ToggleMinimap
        | TriggerCompletion
        | TrimSelections
        | Undo
        | UnfoldAll
        | UnfoldAtCursor
        | UnindentSelection
        | Yank
        | YankMainToClipboard
        | YankToClipboard => ctx.editor_focused,

        _ => true,
    }
}

/// Two-step state machine driving the palette's interaction model.
///
/// [`Filter`] is the standard fuzzy-filter view. Confirming a
/// zero-parameter action dispatches it immediately. Confirming a
/// param-taking action transitions into [`CollectArgs`].
///
/// [`CollectArgs`] walks the user through each parameter in
/// sequence. Each [`Self::confirm`] parses the query text against
/// the current parameter's [`stoat_action::ParamKind`], either
/// advancing to the next parameter (clearing the query editor) or
/// dispatching the action once every parameter has been collected.
/// A parse failure leaves the phase intact and surfaces the error
/// next to the prompt.
enum PalettePhase {
    Filter,
    CollectArgs {
        entry: &'static RegistryEntry,
        collected: Vec<ParamValue>,
        current: usize,
        error: Option<String>,
    },
}

/// Snapshot of the phase data needed to drive [`CommandPaletteDelegate::confirm`]
/// once the borrow on [`CommandPaletteDelegate::phase`] is released. Lifts
/// the per-arm decision out of the match so the body can mutate the phase
/// freely without conflicting borrows.
enum ConfirmStep {
    Filter,
    CollectArgs {
        entry: &'static RegistryEntry,
        current: usize,
    },
}

impl CommandPaletteDelegate {
    pub fn new(workspace: WeakEntity<Workspace>, availability: Availability) -> Self {
        let entries: Vec<&'static RegistryEntry> = registry::all()
            .filter(|entry| entry.def.palette_visible())
            .collect();
        let mut delegate = Self {
            entries,
            matches: Vec::new(),
            selected: 0,
            workspace,
            phase: PalettePhase::Filter,
            query_editor: None,
            scope: PaletteScope::Active,
            availability,
            history: VecDeque::new(),
            history_cursor: None,
            history_prefix: None,
        };
        delegate.set_matches_for_empty_query();
        delegate
    }

    fn is_entry_visible(&self, entry: &RegistryEntry) -> bool {
        match self.scope {
            PaletteScope::All => true,
            PaletteScope::Active => action_is_available(entry.def.kind(), &self.availability),
        }
    }

    fn set_matches_for_empty_query(&mut self) {
        let mut indexed: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.is_entry_visible(self.entries[i]))
            .collect();
        indexed.sort_by_key(|&i| {
            let def = self.entries[i].def;
            (def.priority().ord(), def.name())
        });
        self.matches = indexed.into_iter().map(|i| (i, Vec::new())).collect();
    }

    /// Flip [`Self::scope`] between [`PaletteScope::Active`] and
    /// [`PaletteScope::All`] and re-run the current filter against
    /// the new scope.
    pub fn toggle_scope(&mut self, cx: &mut Context<'_, Picker<Self>>) {
        self.scope = match self.scope {
            PaletteScope::Active => PaletteScope::All,
            PaletteScope::All => PaletteScope::Active,
        };
        let query = self.query_editor_text(cx);
        self.refilter(&query);
        cx.notify();
    }

    fn selected_entry(&self) -> Option<&'static RegistryEntry> {
        let (idx, _) = self.matches.get(self.selected)?;
        self.entries.get(*idx).copied()
    }

    /// Replace the picker query editor's text with `text`. No-op when
    /// the editor has been dropped (the picker entity is gone),
    /// [`on_attach`] hasn't run yet, or the editor is already empty and
    /// `text` is empty.
    fn set_query_editor(&self, text: &str, cx: &mut Context<'_, Picker<Self>>) {
        let Some(editor) = self.query_editor.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        let buffer = editor.read(cx).multi_buffer().clone();
        let Some(singleton) = buffer.read(cx).as_singleton().cloned() else {
            return;
        };
        let len = singleton.read(cx).text().len();
        if len == 0 && text.is_empty() {
            return;
        }
        singleton.update(cx, |b, cx| b.edit(0..len, text, cx));
    }

    /// Clear the picker query editor's text.
    fn clear_query_editor(&self, cx: &mut Context<'_, Picker<Self>>) {
        self.set_query_editor("", cx);
    }

    fn query_editor_text(&self, cx: &Context<'_, Picker<Self>>) -> String {
        let Some(editor) = self.query_editor.as_ref().and_then(WeakEntity::upgrade) else {
            return String::new();
        };
        let buffer = editor.read(cx).multi_buffer().clone();
        let Some(singleton) = buffer.read(cx).as_singleton().cloned() else {
            return String::new();
        };
        singleton.read(cx).text()
    }

    fn refilter(&mut self, query: &str) {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            self.set_matches_for_empty_query();
            if self.selected >= self.matches.len() {
                self.selected = self.matches.len().saturating_sub(1);
            }
            return;
        }

        let items = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.is_entry_visible(entry))
            .map(|(i, entry)| {
                let aliases = entry.def.aliases().iter().map(|a| a.to_string()).collect();
                (i, entry.def.name().to_string(), aliases)
            });
        let ranked = match match_and_rank_aliased(trimmed, items) {
            Some(r) => r,
            None => {
                self.set_matches_for_empty_query();
                if self.selected >= self.matches.len() {
                    self.selected = self.matches.len().saturating_sub(1);
                }
                return;
            },
        };

        let mut tie_broken = ranked;
        tie_broken.sort_by(|a, b| {
            b.score.cmp(&a.score).then_with(|| {
                let a_def = self.entries[a.item].def;
                let b_def = self.entries[b.item].def;
                a_def
                    .priority()
                    .ord()
                    .cmp(&b_def.priority().ord())
                    .then_with(|| a_def.name().cmp(b_def.name()))
            })
        });

        self.matches = tie_broken
            .into_iter()
            .map(|m| (m.item, m.matched_indices))
            .collect();
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    fn is_navigating_history(&self) -> bool {
        self.history_cursor.is_some()
    }

    fn reset_history_cursor(&mut self) {
        self.history_cursor = None;
        self.history_prefix = None;
    }

    /// Drop the recall cursor when the live query no longer matches the
    /// history entry it last recalled -- i.e. the user edited the text
    /// after recalling. Returns the still-valid cursor, if any.
    fn validate_history_cursor(&mut self, current_query: &str) -> Option<usize> {
        if let Some(pos) = self.history_cursor {
            if self.history.get(pos).map(String::as_str) != Some(current_query) {
                self.reset_history_cursor();
            }
        }
        self.history_cursor
    }

    /// Walk one step toward older history, returning the newest entry
    /// before the cursor that shares the navigation prefix. The first
    /// step captures `current_query` as that prefix.
    fn history_previous(&mut self, current_query: &str) -> Option<String> {
        if self.validate_history_cursor(current_query).is_none() {
            self.history_prefix = Some(current_query.to_string());
        }
        let prefix = self.history_prefix.clone().unwrap_or_default();
        let start = self.history_cursor.unwrap_or(self.history.len());
        for i in (0..start).rev() {
            if self.history.get(i).is_some_and(|e| e.starts_with(&prefix)) {
                self.history_cursor = Some(i);
                return self.history.get(i).cloned();
            }
        }
        None
    }

    /// Walk one step toward newer history, returning the next entry
    /// after the cursor that shares the navigation prefix. Returns
    /// `None` once past the newest match, leaving the caller to restore
    /// the typed prefix.
    fn history_next(&mut self, current_query: &str) -> Option<String> {
        let selected = self.validate_history_cursor(current_query)?;
        let prefix = self.history_prefix.clone().unwrap_or_default();
        for i in (selected + 1)..self.history.len() {
            if self.history.get(i).is_some_and(|e| e.starts_with(&prefix)) {
                self.history_cursor = Some(i);
                return self.history.get(i).cloned();
            }
        }
        None
    }

    /// Record the confirmed `query` as the most recent history entry
    /// and end any in-progress recall. The local copy updates
    /// immediately; the durable workspace store updates on a deferred
    /// pass so confirm does not re-enter the workspace lease the
    /// keystroke dispatch already holds. Empty queries are ignored.
    fn record_query(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        self.reset_history_cursor();
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return;
        }
        let query = trimmed.to_string();
        record_query_capped(&mut self.history, query.clone(), HISTORY_LIMIT);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        window.defer(cx, move |_window, cx| {
            workspace.update(cx, |ws, _| ws.push_command_palette_query(query));
        });
    }

    /// Handle `PaletteSelectPrev`: recall an older history entry when
    /// the selection sits at the top of the list or recall is already
    /// under way. Returns `true` when an entry was recalled, so the
    /// picker skips its own selection move.
    fn history_navigate_prev(&mut self, cx: &mut Context<'_, Picker<Self>>) -> bool {
        if !matches!(self.phase, PalettePhase::Filter) {
            return false;
        }
        if self.selected != 0 && !self.is_navigating_history() {
            return false;
        }
        let current = self.query_editor_text(cx);
        match self.history_previous(&current) {
            Some(entry) => {
                self.set_query_editor(&entry, cx);
                true
            },
            None => false,
        }
    }

    /// Handle `PaletteSelectNext`: walk recall toward newer entries
    /// while navigating, restoring the typed prefix once past the
    /// newest match. Returns `true` while recall is active so the
    /// picker skips its own selection move.
    fn history_navigate_next(&mut self, cx: &mut Context<'_, Picker<Self>>) -> bool {
        if !matches!(self.phase, PalettePhase::Filter) || !self.is_navigating_history() {
            return false;
        }
        let current = self.query_editor_text(cx);
        match self.history_next(&current) {
            Some(entry) => self.set_query_editor(&entry, cx),
            None => {
                let prefix = self.history_prefix.take().unwrap_or_default();
                self.reset_history_cursor();
                self.set_query_editor(&prefix, cx);
            },
        }
        true
    }
}

impl PickerDelegate for CommandPaletteDelegate {
    fn match_count(&self) -> usize {
        match &self.phase {
            PalettePhase::Filter => self.matches.len(),
            PalettePhase::CollectArgs { .. } => 1,
        }
    }

    fn selected_index(&self) -> usize {
        match &self.phase {
            PalettePhase::Filter => self.selected,
            PalettePhase::CollectArgs { .. } => 0,
        }
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
        if matches!(self.phase, PalettePhase::CollectArgs { .. }) {
            return;
        }
        if ix < self.matches.len() {
            self.selected = ix;
        }
    }

    fn update_matches(&mut self, query: String, _cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        if matches!(self.phase, PalettePhase::CollectArgs { .. }) {
            return Task::ready(());
        }
        self.refilter(&query);
        Task::ready(())
    }

    fn confirm(
        &mut self,
        _secondary: Option<PickerSecondary>,
        window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        let step = match &self.phase {
            PalettePhase::Filter => ConfirmStep::Filter,
            PalettePhase::CollectArgs { entry, current, .. } => ConfirmStep::CollectArgs {
                entry,
                current: *current,
            },
        };
        match step {
            ConfirmStep::Filter => {
                let Some(entry) = self.selected_entry() else {
                    return;
                };
                let query = self.query_editor_text(cx);
                self.record_query(query, window, cx);
                if entry.def.params().is_empty() {
                    dispatch_action(entry, &[], &self.workspace, window, cx);
                    return;
                }
                self.phase = PalettePhase::CollectArgs {
                    entry,
                    collected: Vec::new(),
                    current: 0,
                    error: None,
                };
                self.clear_query_editor(cx);
                cx.notify();
            },
            ConfirmStep::CollectArgs { entry, current } => {
                let params = entry.def.params();
                let kind = params[current].kind;
                let text = self.query_editor_text(cx);
                match ParamValue::parse(kind, &text) {
                    Ok(value) => {
                        let PalettePhase::CollectArgs {
                            collected, error, ..
                        } = &mut self.phase
                        else {
                            return;
                        };
                        collected.push(value);
                        *error = None;
                        if collected.len() == params.len() {
                            let collected = std::mem::take(collected);
                            self.phase = PalettePhase::Filter;
                            self.clear_query_editor(cx);
                            dispatch_action(entry, &collected, &self.workspace, window, cx);
                        } else {
                            let next_current = collected.len();
                            let collected = std::mem::take(collected);
                            self.phase = PalettePhase::CollectArgs {
                                entry,
                                collected,
                                current: next_current,
                                error: None,
                            };
                            self.clear_query_editor(cx);
                            cx.notify();
                        }
                    },
                    Err(e) => {
                        let PalettePhase::CollectArgs { error, .. } = &mut self.phase else {
                            return;
                        };
                        *error = Some(e.to_string());
                        cx.notify();
                    },
                }
            },
        }
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(&self, ix: usize, cx: &mut Context<'_, Picker<Self>>) -> AnyElement {
        match &self.phase {
            PalettePhase::Filter => self.render_filter_match(ix, cx),
            PalettePhase::CollectArgs {
                entry,
                current,
                error,
                ..
            } => render_collect_args_prompt(entry, *current, error.as_deref(), cx),
        }
    }

    fn on_attach(&mut self, query_editor: &Entity<Editor>) {
        self.query_editor = Some(query_editor.downgrade());
    }

    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        _window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> bool {
        match action.kind() {
            ActionKind::PaletteScopeToggle => {
                self.toggle_scope(cx);
                true
            },
            ActionKind::PaletteSelectPrev => self.history_navigate_prev(cx),
            ActionKind::PaletteSelectNext => self.history_navigate_next(cx),
            _ => false,
        }
    }

    fn keybinding_for_index(
        &self,
        ix: usize,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> Option<SharedString> {
        if !matches!(self.phase, PalettePhase::Filter) {
            return None;
        }
        let (entry_idx, _) = self.matches.get(ix)?;
        let name = self.entries.get(*entry_idx)?.def.name();
        let workspace = self.workspace.upgrade()?;
        let state_machine = workspace.read(cx).input_state_machine().clone();
        let chord = state_machine.read(cx).keymap().chord_for_action(name)?;
        Some(SharedString::from(chord))
    }
}

impl CommandPaletteDelegate {
    fn render_filter_match(&self, ix: usize, cx: &mut Context<'_, Picker<Self>>) -> AnyElement {
        let Some((entry_idx, matched)) = self.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(entry) = self.entries.get(*entry_idx) else {
            return div().into_any_element();
        };
        let name = entry.def.name();
        let color = cx.theme().modal_palette;
        let runs = match_highlight_runs(
            name,
            matched,
            HighlightStyle {
                color: Some(cx.theme().text_accent),
                ..Default::default()
            },
        );
        let label = StyledText::new(SharedString::from(name)).with_highlights(runs);
        div()
            .px_2()
            .text_color(color)
            .child(label)
            .into_any_element()
    }
}

fn dispatch_action(
    entry: &'static RegistryEntry,
    args: &[ParamValue],
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<'_, Picker<CommandPaletteDelegate>>,
) {
    let action = match (entry.create)(args) {
        Ok(a) => a,
        Err(err) => {
            tracing::warn!(
                target: "stoat_gui::command_palette",
                action = entry.def.name(),
                ?err,
                "command palette could not build action from collected params",
            );
            return;
        },
    };
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    // Pop the palette before the deferred dispatch runs so the workspace
    // observer at `workspace.rs:304-312` rebroadcasts the pane editor as
    // `active_editor`. Order matters across two effect-flush passes:
    //   1. `cx.emit(DismissEvent)` lands `Effect::Emit` ahead of the outer `Effect::Defer`. The
    //      dismiss handler pops the modal and schedules `Effect::Notify` for the modal layer.
    //   2. The outer defer fires and schedules another defer -- pushing `Effect::Defer` (inner)
    //      onto the queue behind the pending modal-layer `Effect::Notify`.
    //   3. The notify runs `broadcast_active_editor`, restoring the pane editor as `active_editor`,
    //      before the inner defer dispatches.
    // A single defer would race the notify and dispatch against the
    // palette's query editor; the dispatch must wait one extra cycle.
    cx.emit(DismissEvent);
    window.defer(cx, move |window, cx| {
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |ws, cx| ws.dispatch_action(action, window, cx));
        });
    });
}

fn render_collect_args_prompt(
    entry: &'static RegistryEntry,
    current: usize,
    error: Option<&str>,
    cx: &mut Context<'_, Picker<CommandPaletteDelegate>>,
) -> AnyElement {
    let params = entry.def.params();
    let total = params.len();
    let Some(param) = params.get(current) else {
        return div().into_any_element();
    };
    let color = cx.theme().modal_palette;
    let header = format!(
        "[{}/{}] {} ({})",
        current + 1,
        total,
        param.name,
        param.kind,
    );
    let description = param.description;
    let mut block = div()
        .flex()
        .flex_col()
        .px_2()
        .text_color(color)
        .child(div().child(SharedString::from(header)))
        .child(div().child(SharedString::from(description)));
    if let Some(message) = error {
        block = block.child(
            div()
                .text_color(cx.theme().error)
                .child(SharedString::from(message.to_string())),
        );
    }
    block.into_any_element()
}

/// Open the command palette as a modal picker. Constructed in
/// `Workspace::dispatch_action` when `OpenCommandPalette` is dispatched.
pub fn open_command_palette(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let weak = cx.weak_entity();
    let availability = Availability::from_workspace(workspace, cx);
    let history = workspace.command_palette_history().clone();
    workspace.toggle_modal::<Picker<CommandPaletteDelegate>, _>(window, cx, move |window, cx| {
        let mut delegate = CommandPaletteDelegate::new(weak, availability);
        delegate.history = history;
        Picker::new(delegate, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_action::ActionKind;

    fn new_delegate() -> CommandPaletteDelegate {
        CommandPaletteDelegate::new(WeakEntity::new_invalid(), Availability::default())
    }

    fn new_all_scope_delegate() -> CommandPaletteDelegate {
        let mut d = new_delegate();
        d.scope = PaletteScope::All;
        d.set_matches_for_empty_query();
        d
    }

    fn matched_names(delegate: &CommandPaletteDelegate) -> Vec<&'static str> {
        delegate
            .matches
            .iter()
            .map(|(i, _)| delegate.entries[*i].def.name())
            .collect()
    }

    #[test]
    fn empty_query_lists_every_palette_visible_entry() {
        let delegate = new_delegate();
        let names = matched_names(&delegate);
        assert!(!names.is_empty());
        assert!(names.contains(&"quit"));
        assert!(names.contains(&"open"));
        assert!(
            !names.contains(&"OpenCommandPalette"),
            "OpenCommandPalette is palette_visible=false",
        );
    }

    #[test]
    fn empty_query_orders_by_priority_then_alphabetical() {
        let delegate = new_delegate();
        let names = matched_names(&delegate);
        let pairs: Vec<(u8, &'static str)> = names
            .iter()
            .map(|n| {
                let prio = registry::all()
                    .find(|e| e.def.name() == *n)
                    .map(|e| e.def.priority().ord())
                    .expect("listed entry must be in registry");
                (prio, *n)
            })
            .collect();
        let mut sorted = pairs.clone();
        sorted.sort();
        assert_eq!(pairs, sorted, "not sorted by (priority, name)");
    }

    #[test]
    fn refilter_open_query_lists_open_actions() {
        let mut delegate = new_delegate();
        delegate.refilter("Open");

        let names = matched_names(&delegate);
        assert!(
            names.contains(&"OpenCommits"),
            "OpenCommits expected in {names:?}"
        );
        assert!(
            names.contains(&"OpenGlobalSearch"),
            "OpenGlobalSearch expected in {names:?}",
        );
    }

    #[test]
    fn whitespace_query_falls_back_to_full_list() {
        let mut delegate = new_delegate();
        delegate.refilter("   ");

        let names = matched_names(&delegate);
        assert!(names.contains(&"quit"));
        assert!(names.contains(&"open"));
    }

    #[test]
    fn refilter_clamps_selected_when_results_shrink() {
        let mut delegate = new_delegate();
        delegate.selected = delegate.matches.len() - 1;
        delegate.refilter("quit");

        assert!(!delegate.matches.is_empty());
        assert!(delegate.selected < delegate.matches.len());
    }

    #[test]
    fn refilter_quit_all_selects_quit_all() {
        let mut delegate = new_delegate();
        delegate.refilter("quit-all");

        let entry = delegate.selected_entry().expect("selected entry");
        assert_eq!(entry.def.kind(), ActionKind::QuitAll);
    }

    #[test]
    fn refilter_non_matching_query_yields_empty_match_list() {
        let mut delegate = new_delegate();
        delegate.refilter("zzzzzzzzzzz");

        assert!(
            delegate.matches.is_empty(),
            "query with no matches should produce an empty list, got {:?}",
            matched_names(&delegate),
        );
    }

    mod availability {
        use super::*;

        #[test]
        fn active_scope_default_hides_contextual_actions() {
            let delegate = new_delegate();
            let names = matched_names(&delegate);
            for name in [
                "AbortRebase",
                "ExecuteRebase",
                "RewordConfirm",
                "RewordAbort",
                "RebaseContinue",
                "ConflictTakeOurs",
                "ConflictApply",
                "ReviewStageChunk",
                "ReviewApplyStaged",
                "CommitsNext",
                "CommitsOpenReview",
                "RunSubmit",
                "EnterRebase",
                "MoveDown",
                "SelectAll",
                "Undo",
                "ToggleBlame",
                "GitToggleStageHunk",
                "GotoDefinition",
                "Hover",
            ] {
                assert!(
                    !names.contains(&name),
                    "{name} unexpectedly visible in Active scope with empty Availability",
                );
            }
            for name in [
                "quit",
                "open",
                "review",
                "OpenCommits",
                "FocusLeft",
                "OpenGlobalSearch",
            ] {
                assert!(
                    names.contains(&name),
                    "{name} missing from globally-applicable listing"
                );
            }
        }

        #[test]
        fn active_scope_editor_focused_surfaces_editor_actions() {
            let availability = Availability {
                editor_focused: true,
                ..Availability::default()
            };
            let delegate = CommandPaletteDelegate::new(WeakEntity::new_invalid(), availability);
            let names = matched_names(&delegate);
            for name in ["MoveDown", "SelectAll", "Undo", "write"] {
                assert!(names.contains(&name), "{name} missing when editor_focused");
            }
            assert!(!names.contains(&"RunSubmit"));
        }

        #[test]
        fn active_scope_in_git_repo_surfaces_vcs_actions() {
            let availability = Availability {
                in_git_repo: true,
                ..Availability::default()
            };
            let delegate = CommandPaletteDelegate::new(WeakEntity::new_invalid(), availability);
            let names = matched_names(&delegate);
            for name in ["ToggleBlame", "GotoNextHunk", "GitToggleStageHunk"] {
                assert!(names.contains(&name), "{name} missing when in_git_repo");
            }
            assert!(!names.contains(&"RunSubmit"));
        }

        #[test]
        fn active_scope_lsp_configured_surfaces_lsp_actions() {
            let availability = Availability {
                lsp_configured: true,
                ..Availability::default()
            };
            let delegate = CommandPaletteDelegate::new(WeakEntity::new_invalid(), availability);
            let names = matched_names(&delegate);
            for name in ["GotoDefinition", "Hover", "RenameSymbol", "CodeAction"] {
                assert!(names.contains(&name), "{name} missing when lsp_configured");
            }
            assert!(!names.contains(&"RunSubmit"));
        }

        #[test]
        fn all_scope_shows_contextual_actions_regardless_of_state() {
            let delegate = new_all_scope_delegate();
            let names = matched_names(&delegate);
            for name in [
                "AbortRebase",
                "RewordConfirm",
                "ConflictApply",
                "ReviewStageChunk",
                "CommitsNext",
                "RunSubmit",
            ] {
                assert!(names.contains(&name), "{name} missing in All scope");
            }
        }

        #[test]
        fn active_scope_in_rebase_plan_surfaces_rebase_actions() {
            let availability = Availability {
                in_rebase_plan: true,
                ..Availability::default()
            };
            let delegate = CommandPaletteDelegate::new(WeakEntity::new_invalid(), availability);
            let names = matched_names(&delegate);
            for name in [
                "AbortRebase",
                "ExecuteRebase",
                "SetRebaseOpPick",
                "SetRebaseOpSquash",
            ] {
                assert!(names.contains(&name), "{name} missing when in_rebase_plan");
            }
            assert!(!names.contains(&"RewordConfirm"));
            assert!(!names.contains(&"ConflictApply"));
        }

        #[test]
        fn active_scope_in_reword_surfaces_reword_actions() {
            let availability = Availability {
                in_rebase_exec: true,
                in_rebase_reword: true,
                ..Availability::default()
            };
            let delegate = CommandPaletteDelegate::new(WeakEntity::new_invalid(), availability);
            let names = matched_names(&delegate);
            for name in ["RewordConfirm", "RewordAbort", "RebaseContinue"] {
                assert!(names.contains(&name), "{name} missing in reword");
            }
            assert!(!names.contains(&"AbortRebase"));
        }

        #[test]
        fn active_scope_in_conflict_surfaces_conflict_actions() {
            let availability = Availability {
                in_rebase_exec: true,
                in_conflict: true,
                ..Availability::default()
            };
            let delegate = CommandPaletteDelegate::new(WeakEntity::new_invalid(), availability);
            let names = matched_names(&delegate);
            for name in [
                "ConflictTakeOurs",
                "ConflictTakeTheirs",
                "ConflictApply",
                "ConflictAbort",
            ] {
                assert!(names.contains(&name), "{name} missing in conflict");
            }
            assert!(!names.contains(&"RewordConfirm"));
        }

        #[test]
        fn active_scope_review_open_surfaces_review_actions() {
            let availability = Availability {
                review_open: true,
                ..Availability::default()
            };
            let delegate = CommandPaletteDelegate::new(WeakEntity::new_invalid(), availability);
            let names = matched_names(&delegate);
            for name in ["ReviewStageChunk", "ReviewApplyStaged", "review-close"] {
                assert!(names.contains(&name), "{name} missing when review_open");
            }
            assert!(!names.contains(&"CommitsNext"));
        }

        #[test]
        fn active_scope_commits_open_surfaces_commits_actions() {
            let availability = Availability {
                commits_open: true,
                ..Availability::default()
            };
            let delegate = CommandPaletteDelegate::new(WeakEntity::new_invalid(), availability);
            let names = matched_names(&delegate);
            for name in ["CommitsNext", "CommitsOpenReview", "EnterRebase"] {
                assert!(names.contains(&name), "{name} missing when commits_open");
            }
            assert!(!names.contains(&"ReviewStageChunk"));
        }

        #[test]
        fn every_registered_action_is_available_when_all_flags_set() {
            let ctx = Availability {
                in_rebase_plan: true,
                in_rebase_exec: true,
                in_rebase_reword: true,
                in_conflict: true,
                review_open: true,
                commits_open: true,
                run_focused: true,
                editor_focused: true,
                in_git_repo: true,
                lsp_configured: true,
            };
            for entry in registry::all() {
                assert!(
                    action_is_available(entry.def.kind(), &ctx),
                    "{} missing from availability predicate",
                    entry.def.name(),
                );
            }
        }

        #[test]
        fn refilter_in_active_scope_respects_availability() {
            let mut delegate = new_delegate();
            delegate.refilter("Abort");

            let names = matched_names(&delegate);
            assert!(
                !names.contains(&"AbortRebase"),
                "AbortRebase visible without in_rebase_plan: {names:?}",
            );
        }

        #[test]
        fn refilter_in_all_scope_lists_contextual_matches() {
            let mut delegate = new_all_scope_delegate();
            delegate.refilter("Abort");

            let names = matched_names(&delegate);
            assert!(
                names.contains(&"AbortRebase"),
                "AbortRebase missing in All scope: {names:?}",
            );
        }
    }

    mod scope_toggle {
        use super::*;
        use crate::globals::ExecutorGlobal;
        use gpui::{AppContext, TestAppContext, VisualTestContext};
        use std::sync::Arc;
        use stoat_scheduler::{Executor, TestScheduler};

        fn install_executor_global(cx: &mut TestAppContext) {
            let executor = Executor::new(Arc::new(TestScheduler::new()));
            cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
        }

        struct Harness<'a> {
            picker: Entity<Picker<CommandPaletteDelegate>>,
            vcx: &'a mut VisualTestContext,
        }

        fn new_harness(cx: &mut TestAppContext) -> Harness<'_> {
            install_executor_global(cx);
            let delegate =
                CommandPaletteDelegate::new(WeakEntity::new_invalid(), Availability::default());
            let vcx = cx.add_empty_window();
            let picker = vcx.update(|window, cx| cx.new(|cx| Picker::new(delegate, window, cx)));
            Harness { picker, vcx }
        }

        fn dispatch_scope_toggle(harness: &mut Harness<'_>) {
            let picker = harness.picker.clone();
            harness.vcx.update(|window, cx| {
                picker.update(cx, |p, cx| {
                    p.handle_action(&stoat_action::PaletteScopeToggle, window, cx)
                });
            });
            harness.vcx.run_until_parked();
        }

        fn names(harness: &Harness<'_>) -> Vec<&'static str> {
            harness.picker.read_with(harness.vcx, |p, _| {
                p.delegate()
                    .matches
                    .iter()
                    .map(|(i, _)| p.delegate().entries[*i].def.name())
                    .collect()
            })
        }

        #[test]
        fn picker_starts_in_active_scope() {
            let mut cx = TestAppContext::single();
            let h = new_harness(&mut cx);
            let scope = h.picker.read_with(h.vcx, |p, _| p.delegate().scope);
            assert_eq!(scope, PaletteScope::Active);
            let listed = names(&h);
            assert!(
                !listed.contains(&"AbortRebase"),
                "Active scope should hide AbortRebase: {listed:?}",
            );
        }

        #[test]
        fn palette_scope_toggle_flips_scope_to_all() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            dispatch_scope_toggle(&mut h);
            let scope = h.picker.read_with(h.vcx, |p, _| p.delegate().scope);
            assert_eq!(scope, PaletteScope::All);
            let listed = names(&h);
            assert!(
                listed.contains(&"AbortRebase"),
                "All scope should list AbortRebase: {listed:?}",
            );
        }

        #[test]
        fn palette_scope_toggle_round_trips_back_to_active() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            dispatch_scope_toggle(&mut h);
            dispatch_scope_toggle(&mut h);
            let scope = h.picker.read_with(h.vcx, |p, _| p.delegate().scope);
            assert_eq!(scope, PaletteScope::Active);
        }
    }

    mod param_collection {
        use super::*;
        use crate::globals::ExecutorGlobal;
        use gpui::{AppContext, TestAppContext, VisualTestContext};
        use std::sync::Arc;
        use stoat_action::{ActionKind, ParamKind};
        use stoat_scheduler::{Executor, TestScheduler};

        fn install_executor_global(cx: &mut TestAppContext) {
            let executor = Executor::new(Arc::new(TestScheduler::new()));
            cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
        }

        struct Harness<'a> {
            picker: Entity<Picker<CommandPaletteDelegate>>,
            vcx: &'a mut VisualTestContext,
        }

        fn new_harness(cx: &mut TestAppContext) -> Harness<'_> {
            install_executor_global(cx);
            let delegate =
                CommandPaletteDelegate::new(WeakEntity::new_invalid(), Availability::default());
            let vcx = cx.add_empty_window();
            let picker = vcx.update(|window, cx| cx.new(|cx| Picker::new(delegate, window, cx)));
            Harness { picker, vcx }
        }

        fn select_entry_by_name(harness: &mut Harness<'_>, name: &str) -> &'static RegistryEntry {
            harness.picker.update(harness.vcx, |p, _cx| {
                let delegate = p.delegate_mut();
                let idx = delegate
                    .entries
                    .iter()
                    .position(|e| e.def.name() == name)
                    .unwrap_or_else(|| panic!("entry {name} missing from registry"));
                delegate.matches = vec![(idx, Vec::new())];
                delegate.selected = 0;
                delegate.entries[idx]
            })
        }

        fn type_query(harness: &mut Harness<'_>, text: &str) {
            let buffer = harness.picker.read_with(harness.vcx, |p, cx| {
                p.query_editor()
                    .read(cx)
                    .multi_buffer()
                    .read(cx)
                    .as_singleton()
                    .expect("single-line editor has singleton buffer")
                    .clone()
            });
            buffer.update(harness.vcx, |b, cx| {
                let len = b.text().len();
                b.edit(0..len, text, cx);
            });
            harness.vcx.run_until_parked();
        }

        fn confirm(harness: &mut Harness<'_>) {
            let picker = harness.picker.clone();
            harness.vcx.update(|window, cx| {
                picker.update(cx, |p, cx| {
                    p.handle_action(&stoat_action::PickerConfirm, window, cx)
                });
            });
            harness.vcx.run_until_parked();
        }

        #[test]
        fn on_attach_captures_query_editor_weak_handle() {
            let mut cx = TestAppContext::single();
            let h = new_harness(&mut cx);
            let attached = h
                .picker
                .read_with(h.vcx, |p, _cx| p.delegate().query_editor.is_some());
            assert!(attached, "delegate did not capture query editor handle");
        }

        #[test]
        fn confirm_filter_zero_arg_stays_in_filter() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            let entry = select_entry_by_name(&mut h, "quit");
            assert!(
                entry.def.params().is_empty(),
                "quit is expected to be zero-arg",
            );
            confirm(&mut h);
            let in_filter = h.picker.read_with(h.vcx, |p, _cx| {
                matches!(p.delegate().phase, PalettePhase::Filter)
            });
            assert!(
                in_filter,
                "Filter phase must persist after zero-arg confirm"
            );
        }

        #[test]
        fn confirm_filter_param_action_enters_collect_args() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "open");
            confirm(&mut h);
            let snapshot = h
                .picker
                .read_with(h.vcx, |p, _cx| match &p.delegate().phase {
                    PalettePhase::CollectArgs {
                        entry,
                        current,
                        collected,
                        error,
                    } => Some((entry.def.kind(), *current, collected.len(), error.clone())),
                    PalettePhase::Filter => None,
                });
            assert_eq!(
                snapshot,
                Some((ActionKind::OpenFile, 0, 0, None)),
                "param-taking confirm must transition into CollectArgs",
            );
        }

        #[test]
        fn match_count_in_collect_args_returns_one() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "open");
            confirm(&mut h);
            let count = h
                .picker
                .read_with(h.vcx, |p, _cx| p.delegate().match_count());
            assert_eq!(count, 1);
        }

        #[test]
        fn selected_index_in_collect_args_pinned_to_zero() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "open");
            confirm(&mut h);
            h.picker.update(h.vcx, |p, cx| {
                p.delegate_mut().set_selected_index(5, cx);
            });
            let ix = h
                .picker
                .read_with(h.vcx, |p, _cx| p.delegate().selected_index());
            assert_eq!(ix, 0);
        }

        #[test]
        fn update_matches_in_collect_args_does_not_refilter() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "open");
            confirm(&mut h);
            type_query(&mut h, "anything goes");
            let in_collect = h.picker.read_with(h.vcx, |p, _cx| {
                matches!(p.delegate().phase, PalettePhase::CollectArgs { .. })
                    && p.delegate().match_count() == 1
            });
            assert!(
                in_collect,
                "typing must not pull the delegate out of CollectArgs"
            );
        }

        #[test]
        fn entering_collect_args_clears_query_editor() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            type_query(&mut h, "OpenF");
            select_entry_by_name(&mut h, "open");
            confirm(&mut h);
            let text = h.picker.read_with(h.vcx, |p, cx| {
                p.query_editor()
                    .read(cx)
                    .multi_buffer()
                    .read(cx)
                    .as_singleton()
                    .expect("singleton")
                    .read(cx)
                    .text()
            });
            assert_eq!(text, "");
        }

        #[test]
        fn collect_args_with_valid_input_resets_to_filter_phase() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "open");
            confirm(&mut h);
            type_query(&mut h, "/tmp/example.rs");
            confirm(&mut h);
            let back_to_filter = h.picker.read_with(h.vcx, |p, _cx| {
                matches!(p.delegate().phase, PalettePhase::Filter)
            });
            assert!(
                back_to_filter,
                "single-param OpenFile must collect its arg and return to Filter",
            );
        }

        #[test]
        fn collect_args_clears_query_between_steps_when_advancing() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "open");
            confirm(&mut h);
            type_query(&mut h, "/tmp/a.rs");
            confirm(&mut h);
            let text = h.picker.read_with(h.vcx, |p, cx| {
                p.query_editor()
                    .read(cx)
                    .multi_buffer()
                    .read(cx)
                    .as_singleton()
                    .expect("singleton")
                    .read(cx)
                    .text()
            });
            assert_eq!(text, "");
        }

        #[test]
        fn render_collect_args_renders_param_prompt_at_ix_zero() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "open");
            confirm(&mut h);
            let entry = h
                .picker
                .read_with(h.vcx, |p, _cx| match &p.delegate().phase {
                    PalettePhase::CollectArgs { entry, .. } => *entry,
                    PalettePhase::Filter => panic!("expected CollectArgs"),
                });
            assert_eq!(entry.def.params()[0].kind, ParamKind::String);
            assert_eq!(entry.def.params()[0].name, "path");
        }
    }

    mod dispatch_dismisses_palette {
        use super::*;
        use crate::{
            editor::Editor,
            globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal},
            picker::Picker,
            settings::Settings,
        };
        use gpui::TestAppContext;
        use std::{path::PathBuf, sync::Arc};
        use stoat::host::{FakeFs, FsHost, FsWatchHost};
        use stoat_host::NoopFsWatcher;
        use stoat_scheduler::{Executor, TestScheduler};

        fn install_globals(cx: &mut TestAppContext, fs: Arc<FakeFs>) {
            cx.update(|cx| {
                cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>));
                cx.set_global(FsWatchHostGlobal(
                    Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
                ));
                cx.set_global(ExecutorGlobal(Executor::new(
                    Arc::new(TestScheduler::new()),
                )));
            });
        }

        #[test]
        fn keybinding_for_index_shows_action_chord() {
            let mut cx = TestAppContext::single();
            let fs = Arc::new(FakeFs::new());
            install_globals(&mut cx, fs);
            cx.update(|cx| {
                cx.set_global(Settings::load_from_source(
                    "on key { mode == normal { Space -> SetMode(space); } \
                     mode == space { p -> OpenFileFinder(); } }",
                ));
            });
            let (ws, vcx) = cx.add_window_view(|_, cx| {
                Workspace::new("main".to_string(), PathBuf::from("/tmp/repo"), cx)
            });
            vcx.run_until_parked();

            ws.update_in(vcx, |w, window, cx| {
                w.dispatch_action(Box::new(stoat_action::OpenCommandPalette), window, cx);
            });
            vcx.run_until_parked();

            let picker = ws
                .read_with(vcx, |w, cx| {
                    w.modal_layer()
                        .read(cx)
                        .active_modal::<Picker<CommandPaletteDelegate>>()
                })
                .expect("command palette modal active after OpenCommandPalette");

            picker.update(vcx, |p, cx| {
                let delegate = p.delegate();
                let row = delegate
                    .matches
                    .iter()
                    .position(|(entry_idx, _)| {
                        delegate.entries[*entry_idx].def.name() == "OpenFileFinder"
                    })
                    .expect("OpenFileFinder row present in palette");
                assert_eq!(
                    delegate.keybinding_for_index(row, cx),
                    Some(SharedString::from("Spc p")),
                    "palette resolves the leader chord for the action"
                );
            });
        }

        #[test]
        fn confirm_zero_param_action_dismisses_palette_and_targets_pane_editor() {
            let mut cx = TestAppContext::single();
            let fs = Arc::new(FakeFs::new());
            fs.insert_file("/tmp/repo/main.rs", b"hi\n");
            install_globals(&mut cx, fs);
            let (ws, vcx) = cx.add_window_view(|_, cx| {
                Workspace::new("main".to_string(), PathBuf::from("/tmp/repo"), cx)
            });
            ws.update(vcx, |w, cx| {
                w.open_paths(&[PathBuf::from("/tmp/repo/main.rs")], cx)
            });
            vcx.run_until_parked();

            let pane_editor = ws.read_with(vcx, |w, cx| {
                let pane_id = w.pane_tree().read(cx).focus();
                w.pane_tree()
                    .read(cx)
                    .pane(pane_id)
                    .expect("focused pane")
                    .read(cx)
                    .active_item()
                    .expect("editor active in pane")
                    .to_any_view()
                    .downcast::<Editor>()
                    .expect("active item is Editor")
            });

            ws.update_in(vcx, |w, window, cx| {
                w.dispatch_action(Box::new(stoat_action::OpenCommandPalette), window, cx);
            });
            vcx.run_until_parked();

            let picker = ws
                .read_with(vcx, |w, cx| {
                    w.modal_layer()
                        .read(cx)
                        .active_modal::<Picker<CommandPaletteDelegate>>()
                })
                .expect("command palette modal active after OpenCommandPalette");

            picker.update(vcx, |p, _| {
                let delegate = p.delegate_mut();
                let idx = delegate
                    .entries
                    .iter()
                    .position(|e| e.def.name() == "ToggleMinimap")
                    .expect("ToggleMinimap registered");
                delegate.matches = vec![(idx, Vec::new())];
                delegate.selected = 0;
            });

            assert!(!ws.read_with(vcx, |w, _| w.minimap_visible()));

            picker.update_in(vcx, |p, window, cx| {
                p.handle_action(&stoat_action::PickerConfirm, window, cx)
            });
            vcx.run_until_parked();

            assert!(
                !ws.read_with(vcx, |w, cx| w.modal_layer().read(cx).has_active_modal()),
                "palette must dismiss itself when confirming a zero-param action",
            );
            assert!(
                ws.read_with(vcx, |w, _| w.minimap_visible()),
                "ToggleMinimap must reach the workspace, not the palette query editor",
            );
            let minimap_target = ws.read_with(vcx, |w, cx| {
                w.minimap()
                    .and_then(|mm| mm.read(cx).minimap_target())
                    .and_then(|weak| weak.upgrade())
                    .map(|editor| editor.entity_id())
            });
            assert_eq!(
                minimap_target,
                Some(pane_editor.entity_id()),
                "minimap mirrors the pane editor, not the palette query editor",
            );
        }
    }

    mod query_history {
        use super::*;
        use crate::{
            globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal},
            picker::Picker,
        };
        use gpui::{TestAppContext, VisualTestContext};
        use std::{collections::VecDeque, path::PathBuf, sync::Arc};
        use stoat::host::{FakeFs, FsHost, FsWatchHost};
        use stoat_host::NoopFsWatcher;
        use stoat_scheduler::{Executor, TestScheduler};

        fn deque(items: &[&str]) -> VecDeque<String> {
            items.iter().map(|s| s.to_string()).collect()
        }

        #[test]
        fn record_query_capped_dedups_and_caps_oldest_first() {
            let mut h = deque(&["a", "b", "c"]);
            record_query_capped(&mut h, "b".to_string(), 5);
            assert_eq!(h, deque(&["a", "c", "b"]), "duplicate moves to most-recent");
            record_query_capped(&mut h, "d".to_string(), 3);
            assert_eq!(h, deque(&["c", "b", "d"]), "oldest dropped past the cap");
        }

        #[test]
        fn history_previous_walks_newest_to_oldest() {
            let mut d = new_delegate();
            d.history = deque(&["old", "mid", "new"]);
            assert_eq!(d.history_previous(""), Some("new".to_string()));
            assert_eq!(d.history_previous("new"), Some("mid".to_string()));
            assert_eq!(d.history_previous("mid"), Some("old".to_string()));
            assert_eq!(d.history_previous("old"), None);
        }

        #[test]
        fn history_next_walks_oldest_to_newest_then_restores() {
            let mut d = new_delegate();
            d.history = deque(&["old", "mid", "new"]);
            assert_eq!(d.history_previous(""), Some("new".to_string()));
            assert_eq!(d.history_previous("new"), Some("mid".to_string()));
            assert_eq!(d.history_previous("mid"), Some("old".to_string()));
            assert_eq!(d.history_next("old"), Some("mid".to_string()));
            assert_eq!(d.history_next("mid"), Some("new".to_string()));
            assert_eq!(
                d.history_next("new"),
                None,
                "no entry past the newest; caller restores the prefix",
            );
        }

        #[test]
        fn history_previous_filters_by_typed_prefix() {
            let mut d = new_delegate();
            d.history = deque(&["open file", "quit", "open recent"]);
            assert_eq!(d.history_previous("open"), Some("open recent".to_string()));
            assert_eq!(
                d.history_previous("open recent"),
                Some("open file".to_string()),
                "prefix walk skips the non-matching `quit` entry",
            );
            assert_eq!(d.history_previous("open file"), None);
        }

        #[test]
        fn history_cursor_resets_when_query_edited_away() {
            let mut d = new_delegate();
            d.history = deque(&["alpha", "beta"]);
            assert_eq!(d.history_previous(""), Some("beta".to_string()));
            assert_eq!(
                d.history_previous("zzz"),
                None,
                "editing away from the recalled entry resets recall",
            );
            assert!(!d.is_navigating_history());
        }

        fn install_globals(cx: &mut TestAppContext, fs: Arc<FakeFs>) {
            cx.update(|cx| {
                cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>));
                cx.set_global(FsWatchHostGlobal(
                    Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
                ));
                cx.set_global(ExecutorGlobal(Executor::new(
                    Arc::new(TestScheduler::new()),
                )));
            });
        }

        fn open_palette(
            ws: &Entity<Workspace>,
            vcx: &mut VisualTestContext,
        ) -> Entity<Picker<CommandPaletteDelegate>> {
            ws.update_in(vcx, |w, window, cx| {
                w.dispatch_action(Box::new(stoat_action::OpenCommandPalette), window, cx);
            });
            vcx.run_until_parked();
            ws.read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<CommandPaletteDelegate>>()
            })
            .expect("command palette modal active")
        }

        fn query_text(
            picker: &Entity<Picker<CommandPaletteDelegate>>,
            vcx: &mut VisualTestContext,
        ) -> String {
            picker.read_with(vcx, |p, cx| {
                p.query_editor()
                    .read(cx)
                    .multi_buffer()
                    .read(cx)
                    .as_singleton()
                    .expect("singleton")
                    .read(cx)
                    .text()
            })
        }

        fn type_query(
            picker: &Entity<Picker<CommandPaletteDelegate>>,
            vcx: &mut VisualTestContext,
            text: &str,
        ) {
            let buffer = picker.read_with(vcx, |p, cx| {
                p.query_editor()
                    .read(cx)
                    .multi_buffer()
                    .read(cx)
                    .as_singleton()
                    .expect("singleton")
                    .clone()
            });
            buffer.update(vcx, |b, cx| {
                let len = b.text().len();
                b.edit(0..len, text, cx);
            });
            vcx.run_until_parked();
        }

        #[test]
        fn palette_select_prev_recalls_workspace_history_newest_first() {
            let mut cx = TestAppContext::single();
            let fs = Arc::new(FakeFs::new());
            install_globals(&mut cx, fs);
            let (ws, vcx) = cx.add_window_view(|_, cx| {
                Workspace::new("main".to_string(), PathBuf::from("/tmp/repo"), cx)
            });
            vcx.run_until_parked();
            ws.update(vcx, |w, _| {
                w.push_command_palette_query("first".to_string());
                w.push_command_palette_query("second".to_string());
            });

            let picker = open_palette(&ws, vcx);

            picker.update_in(vcx, |p, window, cx| {
                assert!(p.handle_action(&stoat_action::PaletteSelectPrev, window, cx));
            });
            vcx.run_until_parked();
            assert_eq!(query_text(&picker, vcx), "second");

            picker.update_in(vcx, |p, window, cx| {
                assert!(p.handle_action(&stoat_action::PaletteSelectPrev, window, cx));
            });
            vcx.run_until_parked();
            assert_eq!(query_text(&picker, vcx), "first");
        }

        #[test]
        fn confirm_records_typed_query_into_workspace_history() {
            let mut cx = TestAppContext::single();
            let fs = Arc::new(FakeFs::new());
            fs.insert_file("/tmp/repo/main.rs", b"hi\n");
            install_globals(&mut cx, fs);
            let (ws, vcx) = cx.add_window_view(|_, cx| {
                Workspace::new("main".to_string(), PathBuf::from("/tmp/repo"), cx)
            });
            ws.update(vcx, |w, cx| {
                w.open_paths(&[PathBuf::from("/tmp/repo/main.rs")], cx)
            });
            vcx.run_until_parked();

            let picker = open_palette(&ws, vcx);
            type_query(&picker, vcx, "ToggleMinimap");
            picker.update_in(vcx, |p, window, cx| {
                p.handle_action(&stoat_action::PickerConfirm, window, cx);
            });
            vcx.run_until_parked();

            assert_eq!(
                ws.read_with(vcx, |w, _| w.command_palette_history().clone()),
                VecDeque::from(vec!["ToggleMinimap".to_string()]),
                "confirming records the typed search query",
            );
        }
    }
}
