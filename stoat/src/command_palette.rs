use crate::{
    app::Stoat,
    fuzzy,
    input_view::{InputView, SubmitTarget},
    pane::{FocusTarget, View},
    rebase::RebasePause,
    workspace::Workspace,
};
use stoat_action::{registry, ActionKind, ParamValue};
use stoat_scheduler::Executor;

pub struct CommandPalette {
    pub(crate) phase: PalettePhase,
    /// Mode to restore when the palette closes. Saved at `new()` time so
    /// the palette can transition [`crate::app::Stoat::mode`] back to whatever
    /// the user was in before `:` was pressed.
    pub(crate) previous_mode: String,
    /// Which subset of actions the palette currently lists. Captured at
    /// open time and toggled by `PaletteScopeToggle` (Shift-Tab).
    pub(crate) scope: PaletteScope,
    /// Snapshot of contextual state derived from [`Stoat`] when the palette
    /// opened. Reused across every [`CommandPalette::refilter_from_input`]
    /// call because the workspace cannot mutate while the palette is modal.
    pub(crate) availability: Availability,
}

/// Palette listing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteScope {
    /// Only actions applicable to the captured [`Availability`] snapshot.
    Active,
    /// Every `palette_visible()` action, regardless of availability.
    All,
}

/// Snapshot of stoat state relevant to per-action availability. Booleans are
/// derived once at palette-open via [`Availability::from_stoat`] so the scope
/// filter is a cheap lookup on every keystroke.
#[derive(Debug, Clone, Copy, Default)]
pub struct Availability {
    /// `workspace.rebase.is_some()`: user has an editable rebase plan.
    pub in_rebase_plan: bool,
    /// `workspace.rebase_active.is_some()`: a rebase is mid-execution
    /// (paused on reword/edit/conflict, or running).
    pub in_rebase_exec: bool,
    /// The in-flight rebase is paused on [`RebasePause::Reword`].
    pub in_rebase_reword: bool,
    /// The in-flight rebase is paused on [`RebasePause::Conflict`].
    pub in_conflict: bool,
    /// `workspace.review.is_some()`.
    pub review_open: bool,
    /// `workspace.commits.is_some()`.
    pub commits_open: bool,
    /// Focused pane hosts a [`View::Run`], or a modal run is active.
    pub run_focused: bool,
}

impl Availability {
    /// Derive the availability snapshot from the active workspace.
    pub fn from_stoat(stoat: &Stoat) -> Self {
        let ws = &stoat.workspaces[stoat.active_workspace];

        let (in_rebase_reword, in_conflict) = ws
            .rebase_active
            .as_ref()
            .and_then(|a| a.pause.as_ref())
            .map(|p| {
                (
                    matches!(p, RebasePause::Reword { .. }),
                    matches!(p, RebasePause::Conflict { .. }),
                )
            })
            .unwrap_or((false, false));

        let focused_view = match ws.focus {
            FocusTarget::SplitPane(_) => Some(ws.panes.pane(ws.panes.focus()).view.clone()),
            FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).map(|d| d.view.clone()),
        };
        let run_focused = matches!(focused_view, Some(View::Run(_)));

        Self {
            in_rebase_plan: ws.rebase.is_some(),
            in_rebase_exec: ws.rebase_active.is_some(),
            in_rebase_reword,
            in_conflict,
            review_open: ws.review.is_some(),
            commits_open: ws.commits.is_some(),
            run_focused,
        }
    }
}

/// Whether `kind` should appear in the palette's Active scope given `ctx`.
/// All scope bypasses this function entirely. Actions not listed here are
/// always available (globally applicable like `Quit`, `FocusLeft`, etc.).
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

        _ => true,
    }
}

pub(crate) enum PalettePhase {
    /// Filtering the action list. The user is typing to narrow candidates and
    /// using Up/Down (or Ctrl-P/N) to navigate. `match_indices` is parallel
    /// to `filtered`: each element is the sorted, deduplicated character
    /// offsets of the query match within the entry's name, used by the
    /// renderer to highlight matched cells. Empty when no pattern is
    /// active.
    Filter {
        input: InputView,
        filtered: Vec<&'static registry::RegistryEntry>,
        match_indices: Vec<Vec<u32>>,
        selected: usize,
    },
    /// A param-taking action has been chosen and the palette is walking the
    /// user through providing each parameter in sequence. Each parameter
    /// step owns its own [`InputView`]; disposed and replaced when the step
    /// advances so each param has independent edit/undo history.
    CollectArgs {
        entry: &'static registry::RegistryEntry,
        collected: Vec<ParamValue>,
        current: usize,
        input: InputView,
        error: Option<String>,
    },
}

pub(crate) enum PaletteOutcome {
    /// Re-render but keep the palette open.
    None,
    /// User cancelled. Currently unused because `CancelPromptInput` closes
    /// the palette directly via `close_palette`; retained as a shape that
    /// future submit paths may want when a per-phase cancel becomes distinct
    /// from a global cancel (e.g. "back up one arg step" vs "close palette").
    #[allow(dead_code)]
    Close,
    /// User selected an action with all required parameters collected.
    Dispatch(&'static registry::RegistryEntry, Vec<ParamValue>),
}

impl CommandPalette {
    pub fn new(
        ws: &mut Workspace,
        executor: Executor,
        previous_mode: String,
        availability: Availability,
    ) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::PaletteFilter, "", "prompt", 1);
        let scope = PaletteScope::Active;
        let mut phase = PalettePhase::Filter {
            input,
            filtered: Vec::new(),
            match_indices: Vec::new(),
            selected: 0,
        };
        if let PalettePhase::Filter {
            filtered,
            match_indices,
            selected,
            ..
        } = &mut phase
        {
            refilter("", scope, &availability, filtered, match_indices, selected);
        }
        Self {
            phase,
            previous_mode,
            scope,
            availability,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn phase(&self) -> &PalettePhase {
        &self.phase
    }

    pub(crate) fn scope(&self) -> PaletteScope {
        self.scope
    }

    /// Flip the palette's [`PaletteScope`] and re-run the current-input
    /// filter against the new scope. Called from the `PaletteScopeToggle`
    /// action handler (Shift-Tab).
    pub(crate) fn toggle_scope(&mut self, ws: &Workspace) {
        self.scope = match self.scope {
            PaletteScope::Active => PaletteScope::All,
            PaletteScope::All => PaletteScope::Active,
        };
        self.refilter_from_input(ws);
    }

    /// Returns the palette's focused [`InputView`], which is always present
    /// since every palette phase is backed by an [`InputView`]. Used by the
    /// focus-resolution path in `Stoat::focused_editor_ids` so keymap-routed
    /// typing hits the correct scratch buffer.
    pub(crate) fn focused_input(&self) -> Option<&InputView> {
        match &self.phase {
            PalettePhase::Filter { input, .. } => Some(input),
            PalettePhase::CollectArgs { input, .. } => Some(input),
        }
    }

    /// Tear down all editor slots owned by the palette. Called on any palette
    /// close path (`CancelPromptInput`, `Ctrl-C`, or post-`Dispatch` cleanup)
    /// so the scratch editor for the current phase doesn't linger in the
    /// workspace's slotmap.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        match &self.phase {
            PalettePhase::Filter { input, .. } => input.dispose(ws),
            PalettePhase::CollectArgs { input, .. } => input.dispose(ws),
        }
    }

    /// Refilter the action list against the current filter text. `ws` is
    /// required to read the [`InputView`]'s current rope contents. Called
    /// every frame from the renderer so mutations picked up by
    /// `handle_insert_key` (typing / backspace / cursor motion) are reflected
    /// without a dedicated sync hook.
    pub(crate) fn refilter_from_input(&mut self, ws: &Workspace) {
        if let PalettePhase::Filter {
            input,
            filtered,
            match_indices,
            selected,
        } = &mut self.phase
        {
            let text = input.text(ws);
            refilter(
                &text,
                self.scope,
                &self.availability,
                filtered,
                match_indices,
                selected,
            );
        }
    }

    /// Invoke the effective "submit" step for the palette's current phase.
    /// In [`PalettePhase::Filter`] this either dispatches a zero-arg action
    /// or transitions to [`PalettePhase::CollectArgs`] when the chosen action
    /// takes parameters. In [`PalettePhase::CollectArgs`] it parses the
    /// current parameter value and either advances to the next parameter or
    /// dispatches with the fully collected argument list. Called from the
    /// `SubmitPromptInput` action handler while the palette is open.
    pub(crate) fn handle_submit(
        &mut self,
        ws: &mut Workspace,
        executor: Executor,
    ) -> PaletteOutcome {
        match &mut self.phase {
            PalettePhase::Filter {
                input,
                filtered,
                match_indices: _,
                selected,
            } => {
                let picked = filtered.get(*selected).copied();
                match picked {
                    Some(entry) if entry.def.params().is_empty() => {
                        input.dispose(ws);
                        PaletteOutcome::Dispatch(entry, Vec::new())
                    },
                    Some(entry) => {
                        input.dispose(ws);
                        let arg_input = InputView::create(
                            ws,
                            executor,
                            SubmitTarget::PaletteArg,
                            "",
                            "prompt",
                            1,
                        );
                        self.phase = PalettePhase::CollectArgs {
                            entry,
                            collected: Vec::new(),
                            current: 0,
                            input: arg_input,
                            error: None,
                        };
                        PaletteOutcome::None
                    },
                    None => PaletteOutcome::None,
                }
            },
            PalettePhase::CollectArgs {
                entry,
                collected,
                current,
                input,
                error,
            } => {
                let params = entry.def.params();
                let kind = params[*current].kind;
                let text = input.text(ws);
                match ParamValue::parse(kind, &text) {
                    Ok(value) => {
                        collected.push(value);
                        *current += 1;
                        if *current == params.len() {
                            input.dispose(ws);
                            let entry = *entry;
                            let collected = std::mem::take(collected);
                            return PaletteOutcome::Dispatch(entry, collected);
                        }
                        input.dispose(ws);
                        *input = InputView::create(
                            ws,
                            executor,
                            SubmitTarget::PaletteArg,
                            "",
                            "prompt",
                            1,
                        );
                        *error = None;
                        PaletteOutcome::None
                    },
                    Err(e) => {
                        *error = Some(e.to_string());
                        PaletteOutcome::None
                    },
                }
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn refilter(
    input: &str,
    scope: PaletteScope,
    availability: &Availability,
    filtered: &mut Vec<&'static registry::RegistryEntry>,
    match_indices: &mut Vec<Vec<u32>>,
    selected: &mut usize,
) {
    let visible: Vec<&'static registry::RegistryEntry> = registry::all()
        .filter(|entry| {
            entry.def.palette_visible()
                && (scope != PaletteScope::Active
                    || action_is_available(entry.def.kind(), availability))
        })
        .collect();

    filtered.clear();
    match_indices.clear();

    let items = visible
        .iter()
        .copied()
        .map(|entry| (entry, entry.def.name().to_string()));
    let Some(mut matches) = fuzzy::match_and_rank(input, items) else {
        let mut all = visible;
        all.sort_by_key(|e| (e.def.priority().ord(), e.def.name()));
        for entry in all {
            filtered.push(entry);
            match_indices.push(Vec::new());
        }
        if *selected >= filtered.len() {
            *selected = filtered.len().saturating_sub(1);
        }
        return;
    };

    matches.sort_by(|a, b| {
        b.score.cmp(&a.score).then_with(|| {
            a.item
                .def
                .priority()
                .ord()
                .cmp(&b.item.def.priority().ord())
                .then_with(|| a.item.def.name().cmp(b.item.def.name()))
        })
    });
    for m in matches {
        filtered.push(m.item);
        match_indices.push(m.matched_indices);
    }

    if *selected >= filtered.len() {
        *selected = filtered.len().saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_for(text: &str) -> Vec<&'static str> {
        names_for_scope(text, PaletteScope::All, &Availability::default())
    }

    fn names_for_scope(
        text: &str,
        scope: PaletteScope,
        availability: &Availability,
    ) -> Vec<&'static str> {
        let mut filtered = Vec::new();
        let mut match_indices = Vec::new();
        let mut selected = 0;
        refilter(
            text,
            scope,
            availability,
            &mut filtered,
            &mut match_indices,
            &mut selected,
        );
        filtered.iter().map(|e| e.def.name()).collect()
    }

    fn priority_ord_of(name: &str) -> u8 {
        registry::all()
            .find(|e| e.def.name() == name)
            .unwrap_or_else(|| panic!("action {name} not registered"))
            .def
            .priority()
            .ord()
    }

    fn pos_in(listed: &[&'static str], name: &str) -> usize {
        listed
            .iter()
            .position(|n| *n == name)
            .unwrap_or_else(|| panic!("{name} missing from listing"))
    }

    #[test]
    fn empty_filter_groups_by_priority_then_alphabetical() {
        let listed = names_for("");
        assert!(listed.contains(&"Quit"));
        assert!(listed.contains(&"OpenFile"));
        assert!(!listed.contains(&"OpenCommandPalette"));

        let listed_with_prio: Vec<(u8, &&'static str)> =
            listed.iter().map(|n| (priority_ord_of(n), n)).collect();
        let mut sorted = listed_with_prio.clone();
        sorted.sort();
        assert_eq!(
            listed_with_prio, sorted,
            "listing not sorted by (priority, name)"
        );
    }

    #[test]
    fn priority_orders_within_prefix_tier() {
        let listed = names_for("");
        // `Run` is Common; `CloseCommits` is Normal. Alphabetically
        // `CloseCommits` < `Run`, so without priority it would come first.
        assert!(pos_in(&listed, "Run") < pos_in(&listed, "CloseCommits"));
    }

    #[test]
    fn tier_boundary_dominates_priority() {
        // `OpenRun` is Common but matches `"Run"` only as a substring, so it
        // must sink below every prefix-tier match regardless of that match's
        // priority (Common `Run`, Normal `RunSubmit`, etc.).
        let listed = names_for("Run");
        let open_run = pos_in(&listed, "OpenRun");
        assert!(pos_in(&listed, "Run") < open_run);
        assert!(pos_in(&listed, "RunSubmit") < open_run);
        assert!(pos_in(&listed, "RunHistoryNext") < open_run);
    }

    #[test]
    fn fuzzy_matches_noncontiguous_subsequence() {
        // `:qa` matches `QuitAll` via subsequence Q(0),A(4); `Quit` has no `a`.
        let listed = names_for("qa");
        assert!(listed.contains(&"QuitAll"), "QuitAll must match via fuzzy");
        assert!(
            !listed.contains(&"Quit"),
            "Quit lacks 'a' and must not match"
        );
    }

    #[test]
    fn tiers_order_prefix_then_substring_then_fuzzy() {
        // For query `re`:
        // - `ReviewRefresh` starts with "re" (prefix).
        // - `OpenReview` contains "re" as a non-prefix substring.
        // - `RunInterrupt` has r(0),e(6) as a subsequence, no "re" substring.
        let listed = names_for("re");
        let prefix = pos_in(&listed, "ReviewRefresh");
        let substring = pos_in(&listed, "OpenReview");
        let fuzzy = pos_in(&listed, "RunInterrupt");
        assert!(prefix < substring, "prefix ranks above substring");
        assert!(substring < fuzzy, "substring ranks above fuzzy");
    }

    #[test]
    fn multi_token_query_matches_in_either_order() {
        // `OpenFile` contains both `open` and `file` tokens. Pattern
        // splits on whitespace, so the order of tokens does not change
        // the hit set.
        let forward = names_for("open file");
        let reverse = names_for("file open");
        assert!(forward.contains(&"OpenFile"));
        assert!(reverse.contains(&"OpenFile"));
    }

    #[test]
    fn whitespace_only_query_lists_all_actions() {
        // Whitespace-only query has no atoms; falls through to the
        // empty-needle path that lists everything.
        let blank = names_for("   ");
        let empty = names_for("");
        assert_eq!(blank, empty);
    }

    #[test]
    fn alphabetical_within_same_priority() {
        let listed = names_for("");
        assert!(pos_in(&listed, "CloseCommits") < pos_in(&listed, "CloseReview"));
        assert!(pos_in(&listed, "CloseReview") < pos_in(&listed, "CloseWorkspace"));
    }

    #[test]
    fn prefix_filter_ranks_first() {
        let listed = names_for("Foc");
        assert!(listed.contains(&"FocusLeft"));
        let first_non_prefix = listed.iter().position(|n| !n.starts_with("Focus"));
        if let Some(idx) = first_non_prefix {
            assert!(
                listed[idx..].iter().all(|n| !n.starts_with("Focus")),
                "prefix matches must come before any fuzzy matches",
            );
        }
    }

    #[test]
    fn substring_filter_after_prefix() {
        let listed = names_for("Pane");
        // ClosePane has "Pane" as a substring but not as a prefix.
        assert!(listed.contains(&"ClosePane"));
    }

    #[test]
    fn case_insensitive_filter() {
        assert_eq!(names_for("quit"), vec!["Quit", "QuitAll"]);
    }

    #[test]
    fn refilter_clamps_selected_when_results_shrink() {
        let mut filtered = Vec::new();
        let mut match_indices = Vec::new();
        let mut selected = 7;
        refilter(
            "quit",
            PaletteScope::All,
            &Availability::default(),
            &mut filtered,
            &mut match_indices,
            &mut selected,
        );
        assert_eq!(filtered.len(), 2);
        assert_eq!(selected, 1);
    }

    #[test]
    fn active_scope_default_availability_hides_contextual_actions() {
        let listed = names_for_scope("", PaletteScope::Active, &Availability::default());
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
        ] {
            assert!(!listed.contains(&name), "{name} unexpectedly visible");
        }
        for name in ["Quit", "OpenFile", "OpenReview", "OpenCommits", "FocusLeft"] {
            assert!(
                listed.contains(&name),
                "{name} missing from applicable list"
            );
        }
    }

    #[test]
    fn active_scope_in_rebase_plan_surfaces_rebase_actions() {
        let ctx = Availability {
            in_rebase_plan: true,
            ..Availability::default()
        };
        let listed = names_for_scope("", PaletteScope::Active, &ctx);
        for name in [
            "AbortRebase",
            "ExecuteRebase",
            "SetRebaseOpPick",
            "SetRebaseOpSquash",
        ] {
            assert!(listed.contains(&name), "{name} missing when in_rebase_plan");
        }
        assert!(!listed.contains(&"RewordConfirm"));
        assert!(!listed.contains(&"ConflictApply"));
    }

    #[test]
    fn active_scope_in_reword_surfaces_reword_actions() {
        let ctx = Availability {
            in_rebase_exec: true,
            in_rebase_reword: true,
            ..Availability::default()
        };
        let listed = names_for_scope("", PaletteScope::Active, &ctx);
        for name in ["RewordConfirm", "RewordAbort", "RebaseContinue"] {
            assert!(listed.contains(&name), "{name} missing in reword");
        }
        assert!(!listed.contains(&"AbortRebase"));
    }

    #[test]
    fn active_scope_in_conflict_surfaces_conflict_actions() {
        let ctx = Availability {
            in_rebase_exec: true,
            in_conflict: true,
            ..Availability::default()
        };
        let listed = names_for_scope("", PaletteScope::Active, &ctx);
        for name in [
            "ConflictTakeOurs",
            "ConflictTakeTheirs",
            "ConflictApply",
            "ConflictAbort",
        ] {
            assert!(listed.contains(&name), "{name} missing in conflict");
        }
        assert!(!listed.contains(&"RewordConfirm"));
    }

    #[test]
    fn active_scope_review_open_surfaces_review_actions() {
        let ctx = Availability {
            review_open: true,
            ..Availability::default()
        };
        let listed = names_for_scope("", PaletteScope::Active, &ctx);
        for name in ["ReviewStageChunk", "ReviewApplyStaged", "CloseReview"] {
            assert!(listed.contains(&name), "{name} missing when review_open");
        }
        assert!(!listed.contains(&"CommitsNext"));
    }

    #[test]
    fn active_scope_commits_open_surfaces_commits_actions() {
        let ctx = Availability {
            commits_open: true,
            ..Availability::default()
        };
        let listed = names_for_scope("", PaletteScope::Active, &ctx);
        for name in ["CommitsNext", "CommitsOpenReview", "EnterRebase"] {
            assert!(listed.contains(&name), "{name} missing when commits_open");
        }
        assert!(!listed.contains(&"ReviewStageChunk"));
    }

    #[test]
    fn active_scope_run_focused_surfaces_run_actions() {
        let ctx = Availability {
            run_focused: true,
            ..Availability::default()
        };
        let listed = names_for_scope("", PaletteScope::Active, &ctx);
        for name in ["RunSubmit", "RunInterrupt"] {
            assert!(listed.contains(&name), "{name} missing when run_focused");
        }
    }

    #[test]
    fn all_scope_shows_contextual_actions_regardless_of_state() {
        let listed = names_for_scope("", PaletteScope::All, &Availability::default());
        for name in [
            "AbortRebase",
            "RewordConfirm",
            "ConflictApply",
            "ReviewStageChunk",
            "CommitsNext",
            "RunSubmit",
        ] {
            assert!(listed.contains(&name), "{name} missing in All scope");
        }
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
    fn command_palette_opens_file_end_to_end() {
        let mut h = Stoat::test();
        let path = h.write_file("palette_target.txt", "loaded via palette");
        let path_str = path.to_str().expect("utf8 path");

        h.type_text(":OpenFile");
        h.type_keys("enter");
        h.type_text(path_str);
        h.type_keys("enter");
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        assert!(
            frame.content.contains("loaded via palette"),
            "buffer not visible in frame:\n{}",
            frame.content
        );
    }

    #[test]
    fn command_palette_escape_cancels() {
        let mut h = Stoat::test();
        h.type_text(":Open");
        h.type_keys("escape");
        let frame = h.snapshot();
        assert_eq!(frame.mode, "normal");
    }

    #[test]
    fn command_palette_filter_narrows_on_typing() {
        let mut h = Stoat::test();
        h.type_text(":quit");
        h.type_keys("enter");
        let frame = h.snapshot();
        assert_eq!(frame.mode, "normal");
    }

    #[test]
    fn command_palette_down_then_enter_dispatches_selection() {
        let mut h = Stoat::test();
        h.type_text(":Focus");
        h.type_keys("down enter");
        assert!(h.stoat.command_palette.is_none());
    }

    #[test]
    fn snapshot_command_palette_filter_empty() {
        let mut h = Stoat::test();
        h.type_text(":");
        h.assert_snapshot("command_palette_filter_empty");
    }

    #[test]
    fn snapshot_command_palette_scope_all_after_backtab() {
        let mut h = Stoat::test();
        h.type_text(":");
        h.type_keys("backtab");
        h.assert_snapshot("command_palette_scope_all_after_backtab");
    }

    #[test]
    fn backtab_toggles_scope_to_all_and_back() {
        let mut h = Stoat::test();
        h.type_text(":");
        assert_eq!(
            h.stoat.command_palette.as_ref().unwrap().scope(),
            PaletteScope::Active
        );
        h.type_keys("backtab");
        assert_eq!(
            h.stoat.command_palette.as_ref().unwrap().scope(),
            PaletteScope::All
        );
        h.type_keys("backtab");
        assert_eq!(
            h.stoat.command_palette.as_ref().unwrap().scope(),
            PaletteScope::Active
        );
    }

    #[test]
    fn abort_rebase_hidden_by_default_visible_after_backtab() {
        let mut h = Stoat::test();
        h.type_text(":Abort");
        {
            let palette = h.stoat.command_palette.as_ref().unwrap();
            let PalettePhase::Filter { filtered, .. } = &palette.phase else {
                panic!("expected filter phase");
            };
            let names: Vec<_> = filtered.iter().map(|e| e.def.name()).collect();
            assert!(!names.contains(&"AbortRebase"), "got {names:?}");
        }
        h.type_keys("backtab");
        {
            let palette = h.stoat.command_palette.as_ref().unwrap();
            let PalettePhase::Filter { filtered, .. } = &palette.phase else {
                panic!("expected filter phase");
            };
            let names: Vec<_> = filtered.iter().map(|e| e.def.name()).collect();
            assert!(names.contains(&"AbortRebase"), "got {names:?}");
        }
    }

    #[test]
    fn snapshot_command_palette_filter_typing() {
        let mut h = Stoat::test();
        h.type_text(":Foc");
        h.assert_snapshot("command_palette_filter_typing");
    }

    #[test]
    fn snapshot_command_palette_filter_narrows_to_one() {
        let mut h = Stoat::test();
        h.type_text(":quitall");
        h.assert_snapshot("command_palette_filter_narrows_to_one");
    }

    #[test]
    fn snapshot_command_palette_collect_args_empty() {
        let mut h = Stoat::test();
        h.type_text(":OpenFile");
        h.type_keys("enter");
        h.assert_snapshot("command_palette_collect_args_empty");
    }

    #[test]
    fn snapshot_command_palette_collect_args_typing() {
        let mut h = Stoat::test();
        h.type_text(":OpenFile");
        h.type_keys("enter");
        h.type_text("/tmp/example.rs");
        h.assert_snapshot("command_palette_collect_args_typing");
    }

    #[test]
    fn snapshot_command_palette_multi_token_highlight() {
        let mut h = Stoat::test();
        h.type_text(":open file");
        h.assert_snapshot("command_palette_multi_token_highlight");
    }
}
