use crate::{
    app::Stoat,
    fuzzy,
    host::FsHost,
    input_view::{InputView, SubmitTarget},
    pane::{FocusTarget, View},
    picker::{PickList, Preview, PreviewSource},
    rebase::RebasePause,
    workspace::Workspace,
};
use std::path::{Path, PathBuf};
use stoat_action::{registry, ActionKind, ParamValue, ValueSource};
use stoat_language::LanguageRegistry;
use stoat_scheduler::{Executor, Task};
use tokio::sync::mpsc::{error::TryRecvError, UnboundedReceiver};

pub struct CommandPalette {
    /// The single command-line input, holding the raw text typed after `:`.
    /// Parsed each frame into an optional command plus a trailing argument
    /// by [`CommandPalette::refilter_from_input`].
    pub(crate) input: InputView,
    /// Action entries matching the current filter text, in display order.
    /// Empty while [`Self::command`] is set, since arg mode replaces the
    /// action list with the argument picker.
    pub(crate) filtered: Vec<&'static registry::RegistryEntry>,
    /// Per-row matched character offsets into each entry's name, parallel to
    /// [`Self::filtered`], used by the renderer to highlight matched cells.
    pub(crate) match_indices: Vec<Vec<u32>>,
    pub(crate) selected: usize,
    /// `Some` once the input parses as a known command followed by a space:
    /// the palette is collecting that command's trailing argument inline. The
    /// argument text is the input tail after the command token.
    pub(crate) command: Option<&'static registry::RegistryEntry>,
    /// Which subset of actions the palette currently lists. Captured at
    /// open time and toggled by `PaletteScopeToggle` (Shift-Tab).
    pub(crate) scope: PaletteScope,
    /// Snapshot of contextual state derived from [`Stoat`] when the palette
    /// opened. Reused across every [`CommandPalette::refilter_from_input`]
    /// call because the workspace cannot mutate while the palette is modal.
    pub(crate) availability: Availability,
    /// Rendered filter-list height in rows, refreshed each frame while the
    /// palette lists actions so the half-page handler can size its step.
    /// `None` before the first render, where the step is a single row.
    pub(crate) viewport_rows: Option<usize>,
    /// Inline value-picker shown while collecting a [`ValueSource::Files`]
    /// argument (e.g. `:o `). `Some` once the streaming workspace walk has been
    /// spawned, and held until the palette closes, where it is disposed. `None`
    /// while listing commands or collecting a non-file argument.
    pub(crate) arg_picker: Option<ArgPicker>,
}

/// The inline value-picker the palette shows while collecting a
/// [`ValueSource::Files`] or [`ValueSource::Buffers`] argument.
///
/// A trimmed mirror of [`crate::file_finder::FileFinder`]'s picker half: it owns
/// the candidate path set, the fuzzy [`PickList`] over it, and the live
/// [`Preview`] pane. The palette parses the command's trailing argument and
/// drives this list with it, so `:o src/ma` filters the same way the standalone
/// finder does.
pub(crate) struct ArgPicker {
    /// Whether this picker lists workspace files or open buffers. Selects the
    /// preview source and whether a streaming walk feeds the list.
    source: ValueSource,
    /// Workspace root. The base for repo-relative row display (read by the
    /// renderer) and fuzzy ranking, and the root a file walk runs from.
    pub(crate) git_root: PathBuf,
    /// Every candidate path. For a file picker it grows as walk batches arrive
    /// via [`ArgPicker::pump_walk`]. For a buffer picker it is the fixed
    /// open-buffer set captured at construction. Copied into [`PickList::base`]
    /// on each refilter.
    all_paths: Vec<PathBuf>,
    /// Streaming receiver fed by the spawned walker, or `None` for a buffer
    /// picker, which has no walk. Cleared once the sender drops.
    walk_rx: Option<UnboundedReceiver<Vec<PathBuf>>>,
    /// Held only to keep the spawned walker alive, or `None` for a buffer
    /// picker. Dropping it cancels the in-flight walk on runtimes that propagate
    /// cancellation.
    _walk_task: Option<Task<()>>,
    /// Fuzzy result list over [`Self::all_paths`], read by the renderer.
    pub(crate) picklist: PickList,
    /// Last argument text run through the matcher, so the per-frame sync skips
    /// re-running it when nothing changed.
    last_filter_text: String,
    /// Preview pane shown beside the result list.
    pub(crate) preview: Preview,
}

impl ArgPicker {
    fn new(
        ws: &mut Workspace,
        executor: Executor,
        source: ValueSource,
        git_root: PathBuf,
        walk: Option<(UnboundedReceiver<Vec<PathBuf>>, Task<()>)>,
        all_paths: Vec<PathBuf>,
    ) -> Self {
        let (walk_rx, walk_task) = match walk {
            Some((rx, task)) => (Some(rx), Some(task)),
            None => (None, None),
        };
        let preview = Preview::new(ws, executor);
        Self {
            source,
            git_root,
            all_paths,
            walk_rx,
            _walk_task: walk_task,
            picklist: PickList::default(),
            last_filter_text: String::new(),
            preview,
        }
    }

    /// Absolute path of the currently selected filtered row, if any.
    pub(crate) fn selected_path(&self) -> Option<&Path> {
        self.picklist.selected_path()
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        self.picklist.move_selection(delta);
    }

    /// Drain every batch the walker emitted since the last call into
    /// [`Self::all_paths`], invalidating the filter cache so the next
    /// [`Self::refilter`] re-runs against the larger base.
    fn pump_walk(&mut self) -> bool {
        let Some(rx) = self.walk_rx.as_mut() else {
            return false;
        };
        let mut received_any = false;
        loop {
            match rx.try_recv() {
                Ok(batch) => {
                    self.all_paths.extend(batch);
                    received_any = true;
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.walk_rx = None;
                    break;
                },
            }
        }
        if received_any {
            self.last_filter_text.clear();
            self.picklist.filtered.clear();
            self.picklist.match_indices.clear();
        }
        received_any
    }

    /// Re-run the matcher for `query` over the discovered paths, short-circuiting
    /// when the query is unchanged and the list is already populated.
    fn refilter(&mut self, query: &str) {
        if query == self.last_filter_text && !self.picklist.filtered.is_empty() {
            return;
        }
        self.picklist.base = self.all_paths.clone();
        self.picklist.refilter(query, &self.git_root);
        self.last_filter_text = query.to_string();
    }

    /// Sync the preview pane to the selected path, or clear it when nothing is
    /// selected. A file picker previews the file on disk. A buffer picker
    /// previews the live, possibly modified in-memory buffer, falling back to a
    /// cleared pane when the path has no open buffer.
    fn sync_preview(
        &mut self,
        ws: &mut Workspace,
        fs_host: &dyn FsHost,
        language_registry: &LanguageRegistry,
    ) {
        let Some(path) = self.selected_path().map(|p| p.to_path_buf()) else {
            self.preview.clear(ws);
            return;
        };
        let preview_source = match self.source {
            ValueSource::Buffers => ws.buffers.id_for_path(&path).map(PreviewSource::Buffer),
            _ => Some(PreviewSource::File(path)),
        };
        match preview_source {
            Some(source) => self.preview.sync(ws, fs_host, language_registry, source),
            None => self.preview.clear(ws),
        }
    }
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
        let run_focused = matches!(focused_view, Some(View::Run(_))) || stoat.modal_run.is_some();

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
        | JumpToMoveSource
        | JumpToMoveTarget
        | JumpToNextMoveSource
        | JumpToPrevMoveSource
        | QueryMoveRelationships => ctx.review_open,

        CloseCommits | CommitsNext | CommitsPrev | CommitsPageDown | CommitsPageUp
        | CommitsFirst | CommitsLast | CommitsRefresh | CommitsOpenReview => ctx.commits_open,

        RunSubmit | RunInterrupt | RunHistoryPrev | RunHistoryNext => ctx.run_focused,

        _ => true,
    }
}

pub(crate) enum PaletteOutcome {
    /// Re-render but keep the palette open.
    None,
    /// User cancelled. Currently unused because `CancelPromptInput` closes
    /// the palette directly via `close_palette`. Retained as a shape that a
    /// future submit path may want when a context-specific cancel becomes
    /// distinct from a global cancel (e.g. "clear the typed argument" vs
    /// "close the palette").
    #[allow(dead_code)]
    Close,
    /// An action is ready to dispatch, with any inline argument parsed into
    /// its parameter list.
    Dispatch(&'static registry::RegistryEntry, Vec<ParamValue>),
}

impl CommandPalette {
    pub fn new(ws: &mut Workspace, executor: Executor, availability: Availability) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::PaletteFilter, "", "prompt", 1);
        let scope = PaletteScope::Active;
        let mut filtered = Vec::new();
        let mut match_indices = Vec::new();
        let mut selected = 0;
        refilter(
            "",
            scope,
            &availability,
            &mut filtered,
            &mut match_indices,
            &mut selected,
        );
        Self {
            input,
            filtered,
            match_indices,
            selected,
            command: None,
            scope,
            availability,
            viewport_rows: None,
            arg_picker: None,
        }
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

    /// Returns the palette's [`InputView`]. Used by the focus-resolution path
    /// in `Stoat::focused_editor_ids` so keymap-routed typing hits the correct
    /// scratch buffer.
    pub(crate) fn focused_input(&self) -> Option<&InputView> {
        Some(&self.input)
    }

    /// Tear down the editor slots owned by the palette. Called on any palette
    /// close path (`CancelPromptInput`, `Ctrl-C`, or post-`Dispatch` cleanup)
    /// so neither the input scratch nor the inline picker's preview lingers in
    /// the workspace's slotmaps.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
        if let Some(picker) = &self.arg_picker {
            picker.preview.dispose(ws);
        }
    }

    /// The value source of the current command's first argument when it drives
    /// an inline picker ([`ValueSource::Files`] or [`ValueSource::Buffers`], e.g.
    /// `:o ` or `:b `), or `None` otherwise. Gates rendering the picker and
    /// routing selection keys to it.
    pub(crate) fn arg_source(&self) -> Option<ValueSource> {
        let param = self.command?.def.params().first()?;
        matches!(
            param.value_source,
            ValueSource::Files | ValueSource::Buffers
        )
        .then_some(param.value_source)
    }

    /// The trailing argument text in picker-argument mode, or `None` otherwise.
    /// Drives the inline picker's filter. The tail is everything after the
    /// command token, so a path argument may contain spaces.
    pub(crate) fn arg_tail(&self, ws: &Workspace) -> Option<String> {
        self.arg_source()?;
        let text = self.input.text(ws);
        let (_, tail) = text.split_once(' ')?;
        Some(tail.to_string())
    }

    /// Install the inline picker for `source`. A file picker is fed by an
    /// already-spawned workspace `walk`. A buffer picker is fed by the fixed
    /// `all_paths` set with no walk. No-op when a picker already exists, so the
    /// per-frame sync can call this unconditionally on entering argument mode.
    pub(crate) fn install_arg_picker(
        &mut self,
        ws: &mut Workspace,
        executor: Executor,
        source: ValueSource,
        git_root: PathBuf,
        walk: Option<(UnboundedReceiver<Vec<PathBuf>>, Task<()>)>,
        all_paths: Vec<PathBuf>,
    ) {
        if self.arg_picker.is_none() {
            self.arg_picker = Some(ArgPicker::new(
                ws, executor, source, git_root, walk, all_paths,
            ));
        }
    }

    /// Drive the inline file picker for one frame, draining walk batches,
    /// refiltering against the argument `tail`, and syncing the preview to the
    /// selection. No-op when no picker is installed.
    pub(crate) fn sync_arg_picker(
        &mut self,
        tail: &str,
        ws: &mut Workspace,
        fs_host: &dyn FsHost,
        language_registry: &LanguageRegistry,
    ) {
        let Some(picker) = self.arg_picker.as_mut() else {
            return;
        };
        picker.pump_walk();
        picker.refilter(tail);
        picker.sync_preview(ws, fs_host, language_registry);
    }

    /// Re-parse the input into an optional command and refilter the action
    /// list. `ws` is required to read the [`InputView`]'s current rope
    /// contents. Called every frame by
    /// [`crate::action_handlers::sync_palette_picker`] before the palette is
    /// painted, so mutations picked up by `handle_insert_key` (typing /
    /// backspace / cursor motion) are reflected without a dedicated sync hook.
    ///
    /// When the input parses as `<command> <arg>` (see [`parse_command`]) the
    /// palette enters arg mode, setting [`Self::command`] and clearing the
    /// action list since the argument picker replaces it. Otherwise the action
    /// list is refiltered against the full text.
    pub(crate) fn refilter_from_input(&mut self, ws: &Workspace) {
        let text = self.input.text(ws);
        self.command = parse_command(&text).map(|(entry, _)| entry);
        if self.command.is_some() {
            self.filtered.clear();
            self.match_indices.clear();
            self.selected = 0;
        } else {
            refilter(
                &text,
                self.scope,
                &self.availability,
                &mut self.filtered,
                &mut self.match_indices,
                &mut self.selected,
            );
        }
    }

    /// Invoke the effective "submit" step for the palette.
    ///
    /// In arg mode (the input parses as `<command> <arg>`) the trailing
    /// argument is parsed into the command's first parameter and dispatched.
    /// Otherwise the selected action is taken. A zero-arg action dispatches
    /// immediately, while a parameter-taking action rewrites the input to
    /// `"<name> "` to begin inline argument entry. Called from the
    /// `SubmitPromptInput` action handler while the palette is open.
    pub(crate) fn handle_submit(&mut self, ws: &mut Workspace) -> PaletteOutcome {
        let text = self.input.text(ws);
        if let Some((entry, arg)) = parse_command(&text) {
            let param = &entry.def.params()[0];
            let chosen = self
                .arg_picker
                .as_ref()
                .filter(|_| {
                    matches!(
                        param.value_source,
                        ValueSource::Files | ValueSource::Buffers
                    )
                })
                .and_then(|picker| picker.selected_path())
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|| arg.to_string());
            return match ParamValue::parse(param.kind, &chosen) {
                Ok(value) => {
                    self.input.dispose(ws);
                    PaletteOutcome::Dispatch(entry, vec![value])
                },
                Err(_) => PaletteOutcome::None,
            };
        }

        match self.filtered.get(self.selected).copied() {
            Some(entry) if entry.def.params().is_empty() => {
                self.input.dispose(ws);
                PaletteOutcome::Dispatch(entry, Vec::new())
            },
            Some(entry) => {
                self.input
                    .replace_text(ws, &format!("{} ", entry.def.name()));
                PaletteOutcome::None
            },
            // The fuzzy filter only matches action names, so a name-free alias
            // like `w!` produces no candidates. Fall back to resolving the raw
            // input as a no-argument command alias so those stay dispatchable.
            None => match registry::lookup_alias(text.trim()) {
                Some(entry) if entry.def.params().is_empty() => {
                    self.input.dispose(ws);
                    PaletteOutcome::Dispatch(entry, Vec::new())
                },
                _ => PaletteOutcome::None,
            },
        }
    }
}

/// Split palette input into a resolved command and its trailing argument text.
///
/// Returns `Some((entry, arg))` only when the text is a command token followed
/// by a space. The token is a command name or alias (resolved by
/// [`registry::lookup_alias`]) and the command must take at least one
/// parameter. `arg` is everything after the first space, so a path argument may
/// itself contain spaces. Returns `None` for plain filter text, an unknown
/// head, or a zero-argument command, keeping the palette in command-filter
/// mode.
fn parse_command(text: &str) -> Option<(&'static registry::RegistryEntry, &str)> {
    let (head, arg) = text.split_once(' ')?;
    let entry = registry::lookup_alias(head)?;
    (!entry.def.params().is_empty()).then_some((entry, arg))
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
    use crate::test_harness::TestHarness;

    /// Seed `files` into the harness' fake fs under a fixed virtual root and
    /// point the active workspace at it, so the palette's inline file picker
    /// walks a deterministic, cwd-independent file set. Returns the root.
    fn seed_palette_workspace(h: &mut TestHarness, files: &[(&str, &str)]) -> PathBuf {
        let root = PathBuf::from("/stoat-palette-test");
        h.fake_fs().insert_files(
            files
                .iter()
                .map(|(rel, content)| (root.join(rel), content.as_bytes())),
        );
        h.stoat.active_workspace_mut().git_root = root.clone();
        root
    }

    fn arg_picker(h: &TestHarness) -> &ArgPicker {
        h.stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .arg_picker
            .as_ref()
            .expect("arg picker active")
    }

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
        // - `CloseReview` contains "re" as a non-prefix substring.
        // - `RunInterrupt` has r(0),e(6) as a subsequence, no "re" substring.
        let listed = names_for("re");
        let prefix = pos_in(&listed, "ReviewRefresh");
        let substring = pos_in(&listed, "CloseReview");
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
        for name in ["Quit", "OpenFile", "Diff", "OpenCommits", "FocusLeft"] {
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

        h.type_text(&format!(":o {path_str}"));
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
    fn palette_w_bang_routes_to_force_save_buffer() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/palette-force");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"original\n");
        h.stoat.active_workspace_mut().git_root = root.clone();
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile { path: path.clone() },
        );
        h.settle();

        let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        buffer.write().expect("poisoned").edit(0..0, "edited ");
        // Advance the on-disk mtime so plain SaveBuffer would refuse. Only
        // ForceSaveBuffer clears the dirty flag here.
        h.fake_fs().insert_file(&path, b"external\n");

        h.type_text(":w!");
        h.type_keys("enter");

        let dirty = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .dirty;
        assert!(!dirty, ":w! force-saves despite the disk change");
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
            let names: Vec<_> = palette.filtered.iter().map(|e| e.def.name()).collect();
            assert!(!names.contains(&"AbortRebase"), "got {names:?}");
        }
        h.type_keys("backtab");
        {
            let palette = h.stoat.command_palette.as_ref().unwrap();
            let names: Vec<_> = palette.filtered.iter().map(|e| e.def.name()).collect();
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

    /// `:o ` with no query lists every workspace file beside a live preview of
    /// the selected one.
    #[test]
    fn snapshot_command_palette_arg_empty() {
        let mut h = TestHarness::with_size(120, 30);
        seed_palette_workspace(
            &mut h,
            &[
                ("src/main.rs", "fn main() {\n    run();\n}\n"),
                ("src/lib.rs", "pub fn run() {}\n"),
                ("README.md", "# project\n"),
            ],
        );
        h.type_text(":o ");
        h.assert_snapshot("command_palette_arg_empty");
    }

    /// Typing after `:o ` filters the file list and repoints the preview.
    #[test]
    fn snapshot_command_palette_arg_typing() {
        let mut h = TestHarness::with_size(120, 30);
        seed_palette_workspace(
            &mut h,
            &[
                ("src/main.rs", "fn main() {\n    run();\n}\n"),
                ("src/lib.rs", "pub fn run() {}\n"),
                ("README.md", "# project\n"),
            ],
        );
        h.type_text(":o main");
        h.assert_snapshot("command_palette_arg_typing");
    }

    /// The `:o ` arg-picker preview is syntax-highlighted on the first idle
    /// frame after the selection changes. Like the file finder, the preview
    /// parse runs in `drive_background` ahead of the scheduler rather than
    /// during the paint pass, so it is not left in `fallback_style` until the
    /// next unrelated event.
    #[test]
    fn snapshot_palette_arg_preview_highlighted_on_first_idle_frame() {
        let mut h = TestHarness::with_size(120, 16);
        seed_palette_workspace(
            &mut h,
            &[
                ("aaa.rs", "fn aaa() {}\n"),
                ("zzz.rs", "fn zzz() -> u32 { 0 }\n"),
            ],
        );
        h.type_text(":o ");
        h.settle();

        h.stoat
            .command_palette
            .as_mut()
            .expect("palette open")
            .arg_picker
            .as_mut()
            .expect("arg picker active")
            .picklist
            .move_selection(1);
        h.assert_snapshot_one_frame("palette_arg_preview_highlighted_first_frame");
    }

    #[test]
    fn arg_picker_lists_workspace_files() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("a.rs", ""), ("b.rs", ""), ("sub/c.rs", "")]);
        h.type_text(":o ");
        h.snapshot();
        assert_eq!(arg_picker(&h).picklist.filtered.len(), 3);
    }

    #[test]
    fn arg_picker_narrows_on_typing() {
        let mut h = Stoat::test();
        seed_palette_workspace(
            &mut h,
            &[("alpha.rs", ""), ("beta.rs", ""), ("gamma.rs", "")],
        );
        h.type_text(":o ");
        h.snapshot();
        assert_eq!(arg_picker(&h).picklist.filtered.len(), 3);

        h.type_text("alp");
        h.snapshot();
        let picker = arg_picker(&h);
        assert_eq!(picker.picklist.filtered.len(), 1);
        let idx = picker.picklist.filtered[0];
        assert!(picker.picklist.base[idx].ends_with("alpha.rs"));
    }

    #[test]
    fn arg_picker_arrow_moves_selection() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("a.rs", ""), ("b.rs", ""), ("c.rs", "")]);
        h.type_text(":o ");
        h.snapshot();
        assert_eq!(arg_picker(&h).picklist.selected, 0);

        h.type_keys("down");
        h.snapshot();
        assert_eq!(arg_picker(&h).picklist.selected, 1);
    }

    #[test]
    fn arg_submit_opens_selected_candidate() {
        let mut h = Stoat::test();
        seed_palette_workspace(
            &mut h,
            &[
                ("note.txt", "UNIQUE-PICKER-MARKER\n"),
                ("other.txt", "nope\n"),
            ],
        );
        h.type_text(":o note");
        h.snapshot();
        h.type_keys("enter");
        assert!(h.stoat.command_palette.is_none());

        let frame = h.snapshot();
        assert!(
            frame.content.contains("UNIQUE-PICKER-MARKER"),
            "selected candidate not opened:\n{}",
            frame.content
        );
    }

    #[test]
    fn arg_picker_preview_buffer_evicted_on_close() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("a.rs", "fn a() {}\n")]);
        h.type_text(":o ");
        h.snapshot();
        let preview_id = arg_picker(&h).preview.buffer;
        assert!(h.stoat.active_workspace().buffers.get(preview_id).is_some());

        h.type_keys("escape");
        assert!(h.stoat.command_palette.is_none());
        assert!(
            h.stoat.active_workspace().buffers.get(preview_id).is_none(),
            "preview buffer should be evicted on close",
        );
        assert!(h
            .stoat
            .active_workspace()
            .buffers
            .preview_buffer_ids()
            .is_empty(),);
    }

    #[test]
    fn arg_picker_scratch_not_left_dirty_on_close() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("main.rs", "fn main() {}\n")]);
        let baseline = h.stoat.active_workspace().buffers.dirty_buffers().len();

        h.type_text(":o main");
        h.snapshot();
        h.type_keys("escape");

        assert!(h.stoat.command_palette.is_none());
        assert_eq!(
            h.stoat.active_workspace().buffers.dirty_buffers().len(),
            baseline,
            "no dirty scratch should linger after the palette closes",
        );
    }

    fn open_buffers(h: &mut TestHarness, root: &Path, rels: &[&str]) {
        for rel in rels {
            crate::action_handlers::dispatch(
                &mut h.stoat,
                &stoat_action::OpenFile {
                    path: root.join(rel),
                },
            );
        }
        h.settle();
    }

    #[test]
    fn buffer_arg_picker_lists_open_buffers() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(&mut h, &[("a.rs", ""), ("b.rs", ""), ("c.rs", "")]);
        open_buffers(&mut h, &root, &["a.rs", "b.rs"]);
        h.type_text(":b ");
        h.snapshot();
        assert_eq!(
            arg_picker(&h).picklist.filtered.len(),
            2,
            "lists only the two open buffers, not every workspace file",
        );
    }

    #[test]
    fn buffer_arg_picker_previews_live_modified_text() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(&mut h, &[("note.txt", "on disk\n")]);
        open_buffers(&mut h, &root, &["note.txt"]);
        let id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&root.join("note.txt"))
            .expect("open buffer");
        {
            let buffer = h.stoat.active_workspace().buffers.get(id).expect("buffer");
            let mut guard = buffer.write().expect("poisoned");
            let len = guard.snapshot.visible_text.len();
            guard.edit(0..len, "edited in memory\n");
        }

        h.type_text(":b ");
        h.snapshot();
        let preview_id = arg_picker(&h).preview.buffer;
        let shown = {
            let buffer = h
                .stoat
                .active_workspace()
                .buffers
                .get(preview_id)
                .expect("preview buffer");
            let guard = buffer.read().expect("poisoned");
            guard.rope().to_string()
        };
        assert_eq!(
            shown, "edited in memory\n",
            "buffer preview shows live in-memory text, not the disk file",
        );
    }

    #[test]
    fn buffer_arg_submit_activates_selected_buffer() {
        let mut h = Stoat::test();
        let root =
            seed_palette_workspace(&mut h, &[("alpha.rs", "ALPHA\n"), ("beta.rs", "BETA\n")]);
        open_buffers(&mut h, &root, &["alpha.rs", "beta.rs"]);

        h.type_text(":b alpha");
        h.snapshot();
        h.type_keys("enter");
        assert!(h.stoat.command_palette.is_none());

        let frame = h.snapshot();
        assert!(
            frame.content.contains("ALPHA"),
            "selected buffer not activated:\n{}",
            frame.content
        );
    }

    /// `:b ` lists the open buffers beside a live preview, mirroring `:o ` but
    /// sourced from buffers rather than disk files.
    #[test]
    fn snapshot_command_palette_buffer_arg() {
        let mut h = TestHarness::with_size(120, 30);
        let root = seed_palette_workspace(
            &mut h,
            &[
                ("src/main.rs", "fn main() {\n    run();\n}\n"),
                ("README.md", "# project\n"),
            ],
        );
        open_buffers(&mut h, &root, &["src/main.rs", "README.md"]);
        h.type_text(":b ");
        h.assert_snapshot("command_palette_buffer_arg");
    }

    #[test]
    fn snapshot_command_palette_multi_token_highlight() {
        let mut h = Stoat::test();
        h.type_text(":file open");
        h.assert_snapshot("command_palette_multi_token_highlight");
    }

    #[test]
    fn snapshot_command_palette_filter_scrolls_to_selection() {
        let mut h = Stoat::test();
        h.type_text(":");
        h.type_keys("down down down down down down down down down down down down");
        h.assert_snapshot("command_palette_filter_scrolls_to_selection");
    }
}
