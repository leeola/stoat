use crate::{
    app::Stoat,
    file_finder::{Browse, BROWSE_PATH_CAP},
    fuzzy,
    host::FsHost,
    input_view::{InputView, SubmitTarget},
    pane::{FocusTarget, View},
    picker::{PathPicker, PreviewPolicy},
    rebase::RebasePause,
    workspace::Workspace,
};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};
use stoat_action::{registry, ActionKind, ParamValue, ValueSource};
use stoat_language::LanguageRegistry;
use stoat_scheduler::{Executor, Task};
use tokio::sync::mpsc::UnboundedReceiver;

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
    /// Inline value-picker shown while collecting a picker-sourced argument
    /// (e.g. `:o ` files, `:cd ` directories, `:b ` buffers). `Some` once the
    /// candidate source has been spawned, and held until the palette closes,
    /// where it is disposed. `None` while listing commands or collecting a
    /// free-typed argument.
    pub(crate) arg_picker: Option<ArgPicker>,
    /// Content-version stamp for the command-list pool, from the shared
    /// generation counter. Bumped when [`Self::last_filter_key`] shows the
    /// filter inputs changed, so the per-frame version is O(1) instead of a
    /// walk of every filtered command name.
    pub(crate) generation: u64,
    /// Hash of the last-refiltered filter inputs (text plus scope). Gates
    /// [`Self::generation`] so it bumps only on a real filter change, not on
    /// every per-frame refilter.
    last_filter_key: u64,
}

/// The inline value-picker the palette shows while collecting a
/// [`ValueSource::Files`], [`ValueSource::Directories`], or
/// [`ValueSource::Buffers`] argument.
///
/// Wraps a shared [`PathPicker`] with the argument's [`ValueSource`], exactly as
/// [`crate::file_finder::FileFinder`] does. The palette parses the command's
/// trailing argument and drives the core's fuzzy list with it, so `:o src/ma`
/// filters the same way the standalone finder does.
pub(crate) struct ArgPicker {
    /// Whether this picker lists workspace files, directories, or open buffers.
    /// Selects the preview policy and whether a streaming walk feeds the list.
    source: ValueSource,
    /// The shared walk / fuzzy-list / preview core. A file picker leaves its
    /// walk feeding `all_paths`. A buffer picker seeds `all_paths` with the
    /// fixed open-buffer set and has no walk.
    pub(crate) core: PathPicker,
    /// Active directory-browse mode for a `/` or `~/` directory argument, or
    /// `None` for the workspace-derived list. Mirrors
    /// [`crate::file_finder::FileFinder`]'s browse: a separate walk rooted at
    /// the typed directory, leaving `core` untouched so backspacing out of the
    /// path restores the workspace list.
    pub(crate) browse: Option<Browse>,
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
        let mut core = PathPicker::new(ws, executor, git_root, walk);
        core.all_paths = all_paths;
        Self {
            source,
            core,
            browse: None,
        }
    }

    /// The picker currently driving the list. Browse mode (a `/` or `~/`
    /// argument) swaps in its own directory-walk picker; every other argument
    /// drives the workspace `core`.
    pub(crate) fn active_core(&mut self) -> &mut PathPicker {
        match &mut self.browse {
            Some(browse) => &mut browse.picker,
            None => &mut self.core,
        }
    }

    pub(crate) fn active_core_ref(&self) -> &PathPicker {
        match &self.browse {
            Some(browse) => &browse.picker,
            None => &self.core,
        }
    }

    /// The argument source this picker was installed for. The palette compares
    /// it against the currently-parsed command's argument source to detect a
    /// stale picker after the command head is edited.
    pub(crate) fn source(&self) -> ValueSource {
        self.source
    }

    /// Absolute path of the currently selected filtered row, if any.
    pub(crate) fn selected_path(&self) -> Option<&Path> {
        self.active_core_ref().selected_path()
    }

    /// Absolute path of the selected row while directory-browse mode is active,
    /// or `None` when listing the workspace directories. Submit prefers this
    /// over the typed path so Enter descends into the highlighted directory.
    pub(crate) fn browse_selected_path(&self) -> Option<&Path> {
        self.browse.as_ref()?.picker.selected_path()
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        self.active_core().move_selection(delta);
    }

    /// Page the selection cursor by the rendered list height in `dir`.
    pub(crate) fn page(&mut self, dir: i32) {
        self.active_core().page(dir);
    }

    fn pump_walk(&mut self) -> bool {
        match &mut self.browse {
            Some(browse) => {
                let pumped = browse.picker.pump_walk();
                if browse.picker.all_paths.len() >= BROWSE_PATH_CAP {
                    browse.picker.all_paths.truncate(BROWSE_PATH_CAP);
                    browse.picker.stop_walk();
                }
                pumped
            },
            None => self.core.pump_walk(),
        }
    }

    fn refilter(&mut self, query: &str) {
        match &mut self.browse {
            Some(browse) => browse.picker.refilter(&browse.partial),
            None => self.core.refilter(query),
        }
    }

    /// Sync the preview pane per this picker's source. A buffer source previews
    /// the live in-memory buffer, falling back to the disk file when the path
    /// has none. A directory source shows nothing. Every other source reads the
    /// file from disk.
    fn sync_preview(
        &mut self,
        ws: &mut Workspace,
        fs_host: &dyn FsHost,
        language_registry: &LanguageRegistry,
    ) {
        let policy = match self.source {
            ValueSource::Buffers => PreviewPolicy::LiveBufferThenFile,
            ValueSource::Directories | ValueSource::Themes => PreviewPolicy::NoPreview,
            _ => PreviewPolicy::File,
        };
        self.active_core()
            .sync_preview(ws, fs_host, language_registry, policy);
    }

    /// Leave directory-browse mode, disposing the browse picker's preview so
    /// the registry returns to its pre-browse size. No-op when not browsing.
    pub(crate) fn leave_browse(&mut self, ws: &mut Workspace) {
        if let Some(browse) = self.browse.take() {
            browse.picker.dispose(ws);
        }
    }

    /// Tear down the preview editor slots owned by the core and any active
    /// browse picker. Called on every palette-close and picker-teardown path.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.core.dispose(ws);
        if let Some(browse) = &self.browse {
            browse.picker.dispose(ws);
        }
    }
}

/// Palette listing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
            FocusTarget::SplitPane => Some(ws.panes.pane(ws.panes.focus()).view.clone()),
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

        RebaseConflictTakeOurs
        | RebaseConflictTakeTheirs
        | RebaseConflictSkipEntry
        | RebaseConflictNextFile
        | RebaseConflictPrevFile
        | RebaseConflictApply
        | RebaseConflictAbort => ctx.in_conflict,

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
    /// its parameter list. The third field is the canonical, re-executable
    /// command line to record in palette history.
    Dispatch(&'static registry::RegistryEntry, Vec<ParamValue>, String),
}

impl CommandPalette {
    pub fn new(ws: &mut Workspace, executor: Executor, availability: Availability) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::PaletteFilter, "", "insert", 1);
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
            generation: crate::picker::next_generation(),
            last_filter_key: 0,
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
            picker.dispose(ws);
        }
    }

    /// Tear down the installed argument picker, disposing its preview editor so
    /// the scratch buffer does not leak. Called when the parsed command's
    /// argument source changes so a fresh picker installs in its place.
    pub(crate) fn dispose_arg_picker(&mut self, ws: &mut Workspace) {
        if let Some(picker) = self.arg_picker.take() {
            picker.dispose(ws);
        }
    }

    /// The value source of the current command's first argument when it drives
    /// an inline picker ([`ValueSource::Files`], [`ValueSource::Directories`],
    /// [`ValueSource::Buffers`], or [`ValueSource::Themes`], e.g. `:o `, `:cd `,
    /// `:b `, or `:SetTheme `), or `None` otherwise. Gates rendering the picker
    /// and routing selection keys to it.
    pub(crate) fn arg_source(&self) -> Option<ValueSource> {
        let param = self.command?.def.params().first()?;
        match param.value_source {
            ValueSource::None => None,
            source @ (ValueSource::Files
            | ValueSource::Directories
            | ValueSource::Buffers
            | ValueSource::Themes) => Some(source),
        }
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

        // The filter output is a pure function of the input text and scope
        // (availability is fixed for the palette's lifetime), so an unchanged
        // key means an identical result. Skip the whole registry walk rather
        // than recompute it every idle frame the palette is open.
        let key = {
            let mut hasher = DefaultHasher::new();
            text.hash(&mut hasher);
            self.scope.hash(&mut hasher);
            hasher.finish()
        };
        if key == self.last_filter_key {
            return;
        }
        self.last_filter_key = key;
        self.generation = crate::picker::next_generation();

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
            // An explicit `/` or `~` path browses the real filesystem, so a
            // highlighted browse directory wins and Enter descends into it.
            // With no browse selection (a bare `~`, or an empty browse list)
            // the typed path dispatches verbatim rather than being overridden
            // by a fuzzy workspace-directory match.
            let explicit_path = {
                let arg = arg.trim();
                arg.starts_with('/') || arg.starts_with('~')
            };
            let chosen = if explicit_path {
                self.arg_picker
                    .as_ref()
                    .filter(|picker| picker.source() == param.value_source)
                    .and_then(|picker| picker.browse_selected_path())
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|| arg.to_string())
            } else {
                self.arg_picker
                    .as_ref()
                    .filter(|picker| picker.source() == param.value_source)
                    .and_then(|picker| picker.selected_path())
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|| arg.to_string())
            };
            return match ParamValue::parse(param.kind, &chosen) {
                Ok(value) => {
                    self.input.dispose(ws);
                    let line = format!("{} {}", history_head(entry), chosen);
                    PaletteOutcome::Dispatch(entry, vec![value], line)
                },
                Err(_) => PaletteOutcome::None,
            };
        }

        // refilter pins an exact name or alias match to the top slot, so the
        // default selection already sits on that command. This branch is the
        // belt for the frame between a keystroke and the next refilter. While
        // the selection sits at the top slot, an exact name or alias match of
        // the typed text wins, so `w` dispatches SaveBuffer and a name-free
        // alias like `w!` stays reachable. Arrowing to a candidate takes that
        // highlighted entry instead.
        if self.selected == 0
            && let Some(entry) = registry::lookup_alias(text.trim())
        {
            if dispatches_bare(entry) {
                self.input.dispose(ws);
                return PaletteOutcome::Dispatch(
                    entry,
                    Vec::new(),
                    history_head(entry).to_string(),
                );
            }
            self.input
                .replace_text(ws, &format!("{} ", entry.command_name));
            return PaletteOutcome::None;
        }

        match self.filtered.get(self.selected).copied() {
            Some(entry) if dispatches_bare(entry) => {
                self.input.dispose(ws);
                PaletteOutcome::Dispatch(entry, Vec::new(), history_head(entry).to_string())
            },
            Some(entry) => {
                self.input
                    .replace_text(ws, &format!("{} ", entry.command_name));
                PaletteOutcome::None
            },
            None => PaletteOutcome::None,
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

/// Whether submitting `entry`'s bare name runs it, rather than opening argument
/// entry.
///
/// True for a command taking no arguments, and for one whose first argument is
/// optional. The latter would otherwise be unreachable in its no-argument form,
/// since the palette rewrites the input to `"<name> "` and waits for a value
/// that the command does not require.
fn dispatches_bare(entry: &registry::RegistryEntry) -> bool {
    entry
        .def
        .params()
        .first()
        .is_none_or(|param| !param.required)
}

/// The head token to record for `entry` in palette history. It is the first
/// alias when one exists, else the name.
///
/// Both re-resolve through [`parse_command`]'s [`registry::lookup_alias`], so a
/// recalled `head [arg]` line replays. The alias is preferred because it reads
/// like the short form a user types (`cd`, not `SetCwd`).
fn history_head(entry: &registry::RegistryEntry) -> &'static str {
    entry
        .def
        .aliases()
        .first()
        .copied()
        .unwrap_or_else(|| entry.def.name())
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
    let passes = |entry: &registry::RegistryEntry| {
        entry.def.palette_visible()
            && (scope != PaletteScope::Active
                || action_is_available(entry.def.kind(), availability))
    };

    let visible: Vec<&'static registry::RegistryEntry> =
        registry::all().filter(|entry| passes(entry)).collect();

    filtered.clear();
    match_indices.clear();

    let items = visible
        .iter()
        .copied()
        .map(|entry| (entry, entry.command_name.clone()));
    let Some(mut matches) = fuzzy::match_and_rank(input, items) else {
        let mut all = visible;
        all.sort_by_key(|e| (e.def.priority().ord(), e.command_name.as_str()));
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
                .then_with(|| a.item.command_name.cmp(&b.item.command_name))
        })
    });
    for m in matches {
        filtered.push(m.item);
        match_indices.push(m.matched_indices);
    }

    // Pin an exact name or alias match to the top so the displayed list agrees
    // with what Enter dispatches. `w` (SaveBuffer) and `o` (OpenFile) resolve by
    // alias, which the name-only fuzzy rank never surfaces first -- or at all,
    // when the name lacks the typed characters.
    let needle = input.trim();
    if let Some(pinned) = registry::lookup_alias(needle)
        && passes(pinned)
    {
        let indices = match filtered
            .iter()
            .position(|e| e.def.name() == pinned.def.name())
        {
            Some(pos) => {
                filtered.remove(pos);
                match_indices.remove(pos)
            },
            None if pinned.command_name.eq_ignore_ascii_case(needle) => {
                (0..pinned.command_name.chars().count() as u32).collect()
            },
            None => Vec::new(),
        };
        filtered.insert(0, pinned);
        match_indices.insert(0, indices);
    }

    if *selected >= filtered.len() {
        *selected = filtered.len().saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{input_history::InputHistory, test_harness::TestHarness};

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

    /// The command names the palette lists for `text`, in display order. Use
    /// this for ranking and display assertions, which are about the text the
    /// user sees.
    fn names_for(text: &str) -> Vec<&'static str> {
        entries_for(text, PaletteScope::All, &Availability::default())
            .iter()
            .map(|e| e.command_name.as_str())
            .collect()
    }

    /// The action names the palette lists under `scope`. Use this for
    /// availability assertions, which are about which actions appear rather
    /// than how they are spelled.
    fn action_names_for_scope(
        text: &str,
        scope: PaletteScope,
        availability: &Availability,
    ) -> Vec<&'static str> {
        entries_for(text, scope, availability)
            .iter()
            .map(|e| e.def.name())
            .collect()
    }

    fn entries_for(
        text: &str,
        scope: PaletteScope,
        availability: &Availability,
    ) -> Vec<&'static registry::RegistryEntry> {
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
        filtered
    }

    fn priority_ord_of(command_name: &str) -> u8 {
        registry::all()
            .find(|e| e.command_name == command_name)
            .unwrap_or_else(|| panic!("action {command_name} not registered"))
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
        assert!(listed.contains(&"quit"));
        assert!(listed.contains(&"open-file"));
        assert!(!listed.contains(&"open-command-palette"));

        let listed_with_prio: Vec<(u8, &&'static str)> =
            listed.iter().map(|n| (priority_ord_of(n), n)).collect();
        let mut sorted = listed_with_prio.clone();
        sorted.sort();
        assert_eq!(
            listed_with_prio, sorted,
            "listing not sorted by (priority, command name)"
        );
    }

    #[test]
    fn priority_orders_within_prefix_tier() {
        let listed = names_for("");
        // `Run` is Common; `CloseCommits` is Normal. Alphabetically
        // `CloseCommits` < `Run`, so without priority it would come first.
        assert!(pos_in(&listed, "run") < pos_in(&listed, "close-commits"));
    }

    #[test]
    fn tier_boundary_dominates_priority() {
        // `OpenRun` is Common but matches `"Run"` only as a substring, so it
        // must sink below every prefix-tier match regardless of that match's
        // priority (Common `Run`, Normal `RunSubmit`, etc.).
        let listed = names_for("run");
        let open_run = pos_in(&listed, "open-run");
        assert!(pos_in(&listed, "run") < open_run);
        assert!(pos_in(&listed, "run-submit") < open_run);
        assert!(pos_in(&listed, "run-history-next") < open_run);
    }

    #[test]
    fn fuzzy_matches_noncontiguous_subsequence() {
        // `:qa` matches `QuitAll` via subsequence Q(0),A(4); `Quit` has no `a`.
        let listed = names_for("qa");
        assert!(listed.contains(&"quit-all"), "QuitAll must match via fuzzy");
        assert!(
            !listed.contains(&"quit"),
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
        let prefix = pos_in(&listed, "review-refresh");
        let substring = pos_in(&listed, "close-review");
        let fuzzy = pos_in(&listed, "run-interrupt");
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
        assert!(forward.contains(&"open-file"));
        assert!(reverse.contains(&"open-file"));
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
        assert!(pos_in(&listed, "close-commits") < pos_in(&listed, "close-review"));
        assert!(pos_in(&listed, "close-review") < pos_in(&listed, "close-workspace"));
    }

    #[test]
    fn prefix_filter_ranks_first() {
        let listed = names_for("foc");
        assert!(listed.contains(&"focus-left"));
        let first_non_prefix = listed.iter().position(|n| !n.starts_with("focus"));
        if let Some(idx) = first_non_prefix {
            assert!(
                listed[idx..].iter().all(|n| !n.starts_with("focus")),
                "prefix matches must come before any fuzzy matches",
            );
        }
    }

    #[test]
    fn exact_alias_match_pins_to_top() {
        assert_eq!(
            names_for("w").first().copied(),
            Some("save-buffer"),
            "`w` is SaveBuffer's alias and must pin to the top",
        );
        assert_eq!(
            names_for("o").first().copied(),
            Some("open-file"),
            "`o` is OpenFile's alias and must pin to the top",
        );
    }

    #[test]
    fn substring_filter_after_prefix() {
        let listed = names_for("pane");
        // ClosePane has "Pane" as a substring but not as a prefix.
        assert!(listed.contains(&"close-pane"));
    }

    #[test]
    fn case_insensitive_filter() {
        assert_eq!(names_for("quit"), vec!["quit", "quit-all", "write-quit"]);
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
        assert_eq!(filtered.len(), 3);
        assert_eq!(selected, 2);
    }

    #[test]
    fn palette_refilter_skips_when_key_unchanged() {
        let mut h = Stoat::test();
        h.type_text(":quit");
        h.snapshot();

        // Clear the derived list, then re-sync with identical input. A skipped
        // refilter leaves it cleared. A re-run would repopulate it.
        let generation = {
            let palette = h.stoat.command_palette.as_mut().expect("palette open");
            assert!(!palette.filtered.is_empty(), "query should match entries");
            palette.filtered.clear();
            palette.generation
        };

        h.snapshot();

        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        assert!(
            palette.filtered.is_empty(),
            "unchanged input must skip the refilter and leave filtered untouched"
        );
        assert_eq!(
            palette.generation, generation,
            "unchanged input must not bump the generation"
        );
    }

    #[test]
    fn active_scope_default_availability_hides_contextual_actions() {
        let listed = action_names_for_scope("", PaletteScope::Active, &Availability::default());
        for name in [
            "AbortRebase",
            "ExecuteRebase",
            "RewordConfirm",
            "RewordAbort",
            "RebaseContinue",
            "RebaseConflictTakeOurs",
            "RebaseConflictApply",
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
        let listed = action_names_for_scope("", PaletteScope::Active, &ctx);
        for name in [
            "AbortRebase",
            "ExecuteRebase",
            "SetRebaseOpPick",
            "SetRebaseOpSquash",
        ] {
            assert!(listed.contains(&name), "{name} missing when in_rebase_plan");
        }
        assert!(!listed.contains(&"RewordConfirm"));
        assert!(!listed.contains(&"RebaseConflictApply"));
    }

    #[test]
    fn active_scope_in_reword_surfaces_reword_actions() {
        let ctx = Availability {
            in_rebase_exec: true,
            in_rebase_reword: true,
            ..Availability::default()
        };
        let listed = action_names_for_scope("", PaletteScope::Active, &ctx);
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
        let listed = action_names_for_scope("", PaletteScope::Active, &ctx);
        for name in [
            "RebaseConflictTakeOurs",
            "RebaseConflictTakeTheirs",
            "RebaseConflictApply",
            "RebaseConflictAbort",
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
        let listed = action_names_for_scope("", PaletteScope::Active, &ctx);
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
        let listed = action_names_for_scope("", PaletteScope::Active, &ctx);
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
        let listed = action_names_for_scope("", PaletteScope::Active, &ctx);
        for name in ["RunSubmit", "RunInterrupt"] {
            assert!(listed.contains(&name), "{name} missing when run_focused");
        }
    }

    #[test]
    fn all_scope_shows_contextual_actions_regardless_of_state() {
        let listed = action_names_for_scope("", PaletteScope::All, &Availability::default());
        for name in [
            "AbortRebase",
            "RewordConfirm",
            "RebaseConflictApply",
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
    fn palette_generation_gates_on_filter_change() {
        let mut h = Stoat::test();
        h.type_text(":");
        h.snapshot();
        let g1 = h
            .stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .generation;
        h.snapshot();
        let g2 = h
            .stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .generation;
        assert_eq!(
            g1, g2,
            "an identical re-sync leaves the generation unchanged"
        );

        h.type_text("quit");
        h.snapshot();
        let g3 = h
            .stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .generation;
        assert_ne!(g2, g3, "a changed query bumps the generation");
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
        h.type_text(":focus");
        h.type_keys("down enter");
        assert!(h.stoat.command_palette.is_none());
    }

    #[test]
    fn palette_alt_up_recalls_and_reexecutes_a_recorded_command() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/hist");
        let sub = root.join("sub");
        h.fake_fs().insert_dir(&sub);
        h.stoat.active_workspace_mut().git_root = root.clone();

        h.type_text(&format!(":cd {}", sub.display()));
        h.type_keys("enter");
        assert_eq!(h.stoat.active_workspace().git_root, sub);

        h.stoat.active_workspace_mut().git_root = root;
        h.type_text(":");
        h.type_keys("alt-up enter");
        assert_eq!(
            h.stoat.active_workspace().git_root,
            sub,
            "Alt-Up recalls the recorded cd line and Enter re-runs it"
        );
    }

    #[test]
    fn palette_needle_recalls_the_matching_history_entry() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/hist");
        let sub = root.join("sub");
        h.fake_fs().insert_dir(&sub);
        h.stoat.active_workspace_mut().git_root = root;
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec![format!("cd {}", sub.display()), "w".to_string()]);

        h.type_text(":sub");
        h.type_keys("alt-up enter");
        assert_eq!(
            h.stoat.active_workspace().git_root,
            sub,
            "the needle skips w and recalls the cd line"
        );
    }

    #[test]
    fn typing_after_a_recall_recaptures_the_needle() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["cd /alpha".to_string(), "cd /beta".to_string()]);

        h.type_text(":");
        h.type_keys("alt-up");
        assert_eq!(
            palette_text(&h),
            "cd /beta",
            "an empty needle recalls the newest"
        );

        h.type_text("x");
        h.stoat.drive_background();
        h.type_keys("alt-up");
        assert_eq!(
            palette_text(&h),
            "cd /betax",
            "the edit ends the walk, so Alt-Up captures the new needle (which matches nothing)"
        );
    }

    #[test]
    fn palette_ctrl_keys_move_the_list_not_history() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["w".to_string()]);

        h.type_text(":");
        h.type_keys("ctrl-n");
        assert_eq!(
            h.stoat.command_palette.as_ref().expect("open").selected,
            1,
            "Ctrl-N moves the list selection down"
        );
        h.type_keys("ctrl-p");
        assert_eq!(
            h.stoat.command_palette.as_ref().expect("open").selected,
            0,
            "Ctrl-P moves the list selection up"
        );
        assert_eq!(palette_text(&h), "", "the Ctrl keys never recall history");
    }

    #[test]
    fn palette_arrows_move_the_list_not_history() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["w".to_string()]);

        h.type_text(":");
        h.type_keys("down");
        assert_eq!(
            h.stoat.command_palette.as_ref().expect("open").selected,
            1,
            "Down moves the list selection down"
        );
        h.type_keys("up");
        assert_eq!(
            h.stoat.command_palette.as_ref().expect("open").selected,
            0,
            "Up moves the list selection up"
        );
        assert_eq!(palette_text(&h), "", "the arrows never recall history");
    }

    #[test]
    fn a_fuzzy_list_submit_records_the_entry_head() {
        let mut h = Stoat::test();
        h.type_text(":focus");
        h.type_keys("down enter");

        let history = h
            .stoat
            .active_workspace()
            .palette_history
            .entries()
            .to_vec();
        assert_eq!(history.len(), 1, "the submitted entry is recorded");
        assert!(
            registry::lookup_alias(&history[0]).is_some(),
            "the recorded head re-resolves through lookup_alias, got {:?}",
            history[0]
        );
    }

    #[test]
    fn palette_cd_sets_git_root() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/cd-test");
        let sub = root.join("sub");
        h.fake_fs().insert_dir(&sub);
        h.stoat.active_workspace_mut().git_root = root;

        h.type_text(&format!(":cd {}", sub.display()));
        h.type_keys("enter");

        assert_eq!(h.stoat.active_workspace().git_root, sub);
        assert_eq!(
            h.stoat.pending_message,
            Some(format!(
                "Current working directory is now {}",
                sub.display()
            )),
        );
        assert!(
            h.stoat.command_palette.is_none(),
            "palette closes on submit"
        );
    }

    #[test]
    fn palette_cd_nonexistent_leaves_root() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/cd-test");
        h.fake_fs().insert_dir(&root);
        h.stoat.active_workspace_mut().git_root = root.clone();

        h.type_text(":cd /cd-test/nope");
        h.type_keys("enter");

        assert_eq!(
            h.stoat.active_workspace().git_root,
            root,
            "an unresolvable path leaves the root untouched"
        );
        assert!(
            h.stoat
                .pending_message
                .as_deref()
                .is_some_and(|m| m.starts_with("cd: cannot resolve")),
            "the failure surfaces as a status message"
        );
    }

    #[test]
    fn palette_cd_expands_tilde() {
        let mut h = Stoat::test();
        h.fake_env().set("HOME", "/home/tester");
        h.fake_fs().insert_dir("/home/tester/proj");
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/elsewhere");

        h.type_text(":cd ~/proj");
        h.type_keys("enter");

        assert_eq!(
            h.stoat.active_workspace().git_root,
            PathBuf::from("/home/tester/proj"),
        );
    }

    /// Sorted directory names listed by the arg picker's active browse walk.
    fn browse_dir_rows(h: &TestHarness) -> Vec<String> {
        let picker = arg_picker(h);
        let browse = picker.browse.as_ref().expect("browse active");
        let mut rows: Vec<String> = browse
            .picker
            .picklist
            .filtered
            .iter()
            .filter_map(|&i| {
                browse.picker.picklist.base[i]
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .collect();
        rows.sort();
        rows
    }

    #[test]
    fn dir_arg_browses_home() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);
        let home = PathBuf::from("/fake-home");
        h.fake_fs().insert_files([
            (home.join("alpha/nested/f.rs"), "a".as_bytes()),
            (home.join("beta/f.rs"), "b".as_bytes()),
        ]);
        h.fake_fs().insert_dir(home.join("gamma"));
        h.fake_env().set("HOME", home.to_str().unwrap());

        h.type_text(":cd ~/");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        assert!(
            arg_picker(&h).browse.is_some(),
            "a ~/ tail enters browse mode"
        );
        // Only home's immediate child dirs: the `nested` dir under alpha never
        // appears (no recursive walk), and the empty `gamma` does.
        assert_eq!(browse_dir_rows(&h), ["alpha", "beta", "gamma"]);
    }

    #[test]
    fn dir_arg_relative_slash_browses_workspace_child() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(
            &mut h,
            &[("wsdir/sub/f.rs", ""), ("wsdir/deep/nested/f.rs", "")],
        );

        h.type_text(":cd wsdir/");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        assert!(
            arg_picker(&h).browse.is_some(),
            "a relative dir/ tail enters browse mode"
        );
        assert_eq!(
            arg_picker(&h).browse.as_ref().map(|b| b.root.clone()),
            Some(root.join("wsdir")),
            "the browse roots at git_root/wsdir",
        );
        // Only wsdir's immediate child dirs. `nested` under deep never appears.
        assert_eq!(browse_dir_rows(&h), ["deep", "sub"]);
    }

    #[test]
    fn dir_arg_relative_backspace_to_bare_tail_leaves_browse() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/sub/f.rs", "")]);

        h.type_text(":cd wsdir/");
        let _ = h.snapshot();
        assert!(arg_picker(&h).browse.is_some(), "wsdir/ enters browse mode");

        h.type_keys("backspace");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        assert!(
            arg_picker(&h).browse.is_none(),
            "backspacing to a bare tail with no slash leaves browse mode"
        );
        let picker = arg_picker(&h);
        let idx = picker.core.picklist.filtered[0];
        assert!(
            picker.core.picklist.base[idx].ends_with("wsdir"),
            "the recursive workspace directory list is restored"
        );
    }

    #[test]
    fn dir_arg_relative_enter_sets_git_root_to_child() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(&mut h, &[("wsdir/sub/f.rs", "")]);

        h.type_text(":cd wsdir/sub");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        assert!(
            arg_picker(&h)
                .selected_path()
                .is_some_and(|p| p.ends_with("sub")),
            "the browse surfaces wsdir/sub",
        );

        h.type_keys("enter");
        assert_eq!(h.stoat.active_workspace().git_root, root.join("wsdir/sub"));
    }

    #[test]
    fn dir_arg_relative_nonexistent_shows_empty_and_enter_leaves_root() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);

        h.type_text(":cd zz/");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        assert!(
            arg_picker(&h).browse.is_some(),
            "a nonexistent relative dir/ still enters browse mode"
        );
        assert_eq!(
            browse_dir_rows(&h),
            Vec::<String>::new(),
            "a nonexistent directory lists nothing"
        );

        h.type_keys("enter");
        assert_eq!(
            h.stoat.active_workspace().git_root,
            root,
            "Enter on an unresolvable relative path leaves the root untouched"
        );
    }

    /// The palette argument tail, or `None` when not in argument mode.
    fn palette_arg_tail(h: &TestHarness) -> Option<String> {
        h.stoat
            .command_palette
            .as_ref()
            .and_then(|p| p.arg_tail(h.stoat.active_workspace()))
    }

    /// The full palette input text.
    fn palette_text(h: &TestHarness) -> String {
        h.stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .input
            .text(h.stoat.active_workspace())
    }

    #[test]
    fn tab_completes_browse_path() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);
        let home = PathBuf::from("/fake-home");
        h.fake_fs()
            .insert_files([(home.join("proj/sub/f.rs"), "x".as_bytes())]);
        h.fake_env().set("HOME", home.to_str().unwrap());

        h.type_text(":cd ~/pr");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        assert!(
            arg_picker(&h)
                .selected_path()
                .is_some_and(|p| p.ends_with("proj")),
            "the browse surfaces ~/proj",
        );

        h.type_keys("tab");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        assert_eq!(
            palette_arg_tail(&h).as_deref(),
            Some("~/proj"),
            "Tab completes the highlighted directory with no trailing slash"
        );
        assert_eq!(
            arg_picker(&h).browse.as_ref().map(|b| b.root.clone()),
            Some(home.clone()),
            "the completion keeps the browse rooted at ~, not descended into proj",
        );
        assert_eq!(browse_dir_rows(&h), ["proj"]);
        assert!(
            arg_picker(&h)
                .selected_path()
                .is_some_and(|p| p.ends_with("proj")),
            "the completed name is the highlighted row",
        );

        // A further `/` now descends the browse into the completed dir.
        h.type_text("/");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        assert_eq!(
            arg_picker(&h).browse.as_ref().map(|b| b.root.clone()),
            Some(home.join("proj")),
            "typing `/` after the completion descends the browse into ~/proj",
        );
        assert_eq!(browse_dir_rows(&h), ["sub"]);
    }

    #[test]
    fn tab_completes_workspace_dir() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);
        h.fake_fs().insert_dir(root.join("wsdir/kid"));

        h.type_text(":cd wsd");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        assert!(
            arg_picker(&h).browse.is_none(),
            "a bare tail lists the recursive workspace dirs"
        );

        h.type_keys("tab");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        assert_eq!(
            palette_arg_tail(&h).as_deref(),
            Some("wsdir"),
            "Tab completes the workspace directory with no trailing slash"
        );
        assert!(
            arg_picker(&h).browse.is_none(),
            "the completed bare tail stays a workspace list, no browse re-root",
        );
        assert!(
            arg_picker(&h)
                .selected_path()
                .is_some_and(|p| p == root.join("wsdir")),
            "the completed workspace dir is the highlighted row",
        );
    }

    #[test]
    fn tab_then_enter_opens_the_completed_dir() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);
        let home = PathBuf::from("/fake-home");
        h.fake_fs()
            .insert_files([(home.join("proj/sub/f.rs"), "x".as_bytes())]);
        h.fake_env().set("HOME", home.to_str().unwrap());

        h.type_text(":cd ~/pr");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        // The completion refilters synchronously, so Enter opens the completed
        // dir rather than its first child even though the intervening capture
        // frame re-syncs the browse.
        h.type_keys("tab enter");

        assert_eq!(
            h.stoat.active_workspace().git_root,
            home.join("proj"),
            "tab-then-enter opens ~/proj, not ~/proj/sub",
        );
        assert!(
            h.stoat.command_palette.is_none(),
            "palette closes on submit"
        );
    }

    #[test]
    fn tab_with_empty_list_leaves_input_unchanged() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);

        h.type_text(":cd zz/");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        assert_eq!(
            browse_dir_rows(&h),
            Vec::<String>::new(),
            "empty browse list"
        );

        h.type_keys("tab");
        let _ = h.snapshot();

        assert_eq!(
            palette_arg_tail(&h).as_deref(),
            Some("zz/"),
            "Tab with no selectable row leaves the input unchanged"
        );
    }

    #[test]
    fn tab_completes_the_selected_command() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);

        h.type_text(":set-them");
        let _ = h.snapshot();
        h.type_keys("tab");
        let _ = h.snapshot();
        assert_eq!(
            palette_text(&h),
            "set-theme ",
            "a param-taking command completes with the space that opens its arg picker"
        );

        h.type_keys("escape");
        h.type_text(":quit-al");
        let _ = h.snapshot();
        h.type_keys("tab");
        let _ = h.snapshot();
        assert_eq!(
            palette_text(&h),
            "quit-all",
            "a parameterless command completes bare, ready for Enter"
        );
    }

    #[test]
    fn tab_completes_file_arg() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/alpha.rs", ""), ("wsdir/beta.rs", "")]);

        h.type_text(":OpenFile wsdir/al");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        h.type_keys("tab");
        let _ = h.snapshot();

        assert_eq!(
            palette_arg_tail(&h).as_deref(),
            Some("wsdir/alpha.rs"),
            "Tab completes the highlighted file row into the tail"
        );
    }

    #[test]
    fn tab_completes_theme_arg() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);

        h.type_text(":SetTheme gruv");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        h.type_keys("tab");
        let _ = h.snapshot();

        assert_eq!(
            palette_arg_tail(&h).as_deref(),
            Some("gruvbox-dark"),
            "Tab completes the highlighted theme row into the tail"
        );
    }

    #[test]
    fn snapshot_palette_cd_browse_rows() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);
        let home = PathBuf::from("/fake-home");
        h.fake_fs().insert_files([
            (home.join("alpha/f.rs"), "a".as_bytes()),
            (home.join("beta/f.rs"), "b".as_bytes()),
        ]);
        h.fake_env().set("HOME", home.to_str().unwrap());

        h.type_text(":cd ~/");
        let _ = h.snapshot();
        h.settle();

        h.assert_snapshot("snapshot_palette_cd_browse_rows");
    }

    #[test]
    fn dir_arg_browse_reroots_on_deeper_segment() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);
        let home = PathBuf::from("/fake-home");
        h.fake_fs()
            .insert_files([(home.join("alpha/nested/f.rs"), "d".as_bytes())]);
        h.fake_env().set("HOME", home.to_str().unwrap());

        h.type_text(":cd ~/");
        let _ = h.snapshot();
        assert_eq!(
            arg_picker(&h).browse.as_ref().map(|b| b.root.clone()),
            Some(home.clone()),
        );

        h.type_text("alpha/");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        assert_eq!(
            arg_picker(&h).browse.as_ref().map(|b| b.root.clone()),
            Some(home.join("alpha")),
            "a deeper path segment re-roots the browse walk",
        );
        assert_eq!(browse_dir_rows(&h), ["nested"]);
    }

    #[test]
    fn dir_arg_browse_enter_sets_git_root() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);
        let home = PathBuf::from("/fake-home");
        h.fake_fs()
            .insert_files([(home.join("projects/f.rs"), "p".as_bytes())]);
        h.fake_env().set("HOME", home.to_str().unwrap());

        h.type_text(":cd ~/proj");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        assert!(
            arg_picker(&h)
                .selected_path()
                .is_some_and(|p| p.ends_with("projects")),
            "the browse walk surfaces ~/projects",
        );

        h.type_keys("enter");
        assert_eq!(h.stoat.active_workspace().git_root, home.join("projects"));
    }

    #[test]
    fn dir_arg_browse_backspace_restores_workspace_dirs() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);

        h.type_text(":cd /");
        let _ = h.snapshot();
        assert!(
            arg_picker(&h).browse.is_some(),
            "a / tail enters browse mode"
        );

        h.type_keys("backspace");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        assert!(
            arg_picker(&h).browse.is_none(),
            "backspacing out of the path shape leaves browse mode"
        );
        let picker = arg_picker(&h);
        let idx = picker.core.picklist.filtered[0];
        assert!(
            picker.core.picklist.base[idx].ends_with("wsdir"),
            "the workspace directory list is restored"
        );
    }

    #[test]
    fn dir_arg_picker_lists_workspace_dirs() {
        let mut h = Stoat::test();
        seed_palette_workspace(
            &mut h,
            &[("top.rs", ""), ("src/main.rs", ""), ("docs/readme.md", "")],
        );
        h.type_text(":cd ");
        h.snapshot();
        assert_eq!(
            arg_picker(&h).core.picklist.filtered.len(),
            2,
            "lists src and docs; a root-level file contributes no directory",
        );
    }

    #[test]
    fn set_theme_arg_picker_lists_themes_and_switches() {
        let mut h = Stoat::test();
        let (cfg, errors) = stoat_config::parse("theme probe { ui.text.fg = \"#abcdef\"; }");
        assert!(errors.is_empty(), "seed theme parses");
        h.stoat
            .theme_blocks
            .extend(cfg.expect("parsed config").themes);

        h.type_text(":SetTheme ");
        h.snapshot();
        let listed: Vec<String> = {
            let picker = arg_picker(&h);
            picker
                .core
                .picklist
                .filtered
                .iter()
                .map(|&i| picker.core.picklist.base[i].to_string_lossy().into_owned())
                .collect()
        };
        assert!(
            listed.iter().any(|n| n == "default_dark"),
            "lists the embedded theme: {listed:?}",
        );
        assert!(
            listed.iter().any(|n| n == "probe"),
            "lists the injected theme: {listed:?}",
        );

        h.type_text("probe");
        h.snapshot();
        h.type_keys("enter");
        assert!(
            h.stoat.command_palette.is_none(),
            "submitting the theme closes the palette",
        );
        assert_eq!(
            h.stoat.theme.name, "probe",
            "selecting a theme switches to it"
        );
    }

    #[test]
    fn theme_alias_reaches_set_theme() {
        let mut h = Stoat::test();
        let (cfg, errors) = stoat_config::parse("theme probe { ui.text.fg = \"#abcdef\"; }");
        assert!(errors.is_empty(), "seed theme parses");
        h.stoat
            .theme_blocks
            .extend(cfg.expect("parsed config").themes);

        h.type_text(":theme ");
        h.snapshot();
        let listed: Vec<String> = {
            let picker = arg_picker(&h);
            picker
                .core
                .picklist
                .filtered
                .iter()
                .map(|&i| picker.core.picklist.base[i].to_string_lossy().into_owned())
                .collect()
        };
        assert!(
            listed.iter().any(|n| n == "probe"),
            "the alias opens the same theme arg picker: {listed:?}",
        );

        h.type_text("probe");
        h.snapshot();
        h.type_keys("enter");
        assert_eq!(
            h.stoat.theme.name, "probe",
            "`:theme NAME` dispatches SetTheme with the argument"
        );
    }

    #[test]
    fn dir_arg_picker_narrows_on_typing() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("src/main.rs", ""), ("docs/readme.md", "")]);
        h.type_text(":cd ");
        h.snapshot();
        assert_eq!(arg_picker(&h).core.picklist.filtered.len(), 2);

        h.type_text("src");
        h.snapshot();
        let picker = arg_picker(&h);
        assert_eq!(picker.core.picklist.filtered.len(), 1);
        let idx = picker.core.picklist.filtered[0];
        assert!(picker.core.picklist.base[idx].ends_with("src"));
    }

    #[test]
    fn dir_arg_submit_sets_git_root() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(&mut h, &[("src/main.rs", "")]);
        h.type_text(":cd src");
        h.snapshot();
        h.type_keys("enter");
        assert!(h.stoat.command_palette.is_none());
        assert_eq!(h.stoat.active_workspace().git_root, root.join("src"));
    }

    /// Replace the open palette's input text, standing in for the user editing
    /// the command head, so the next sync re-parses it.
    fn replace_palette_input(h: &mut TestHarness, text: &str) {
        let idx = h.stoat.active_workspace;
        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        let ws = &mut h.stoat.workspaces[idx];
        palette.input.replace_text(ws, text);
    }

    #[test]
    fn arg_picker_follows_edited_command() {
        let mut h = Stoat::test();
        seed_palette_workspace(
            &mut h,
            &[("top.rs", ""), ("src/main.rs", ""), ("docs/readme.md", "")],
        );
        h.type_text(":o ");
        h.snapshot();
        assert_eq!(arg_picker(&h).core.picklist.filtered.len(), 3);
        let stale_preview = arg_picker(&h).core.preview.buffer;

        replace_palette_input(&mut h, "cd ");
        h.snapshot();

        let picker = arg_picker(&h);
        assert_eq!(picker.source(), ValueSource::Directories);
        assert_eq!(
            picker.core.picklist.filtered.len(),
            2,
            "the picker follows the parsed command to the two directories",
        );
        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .get(stale_preview)
                .is_none(),
            "the stale Files preview buffer is evicted, not leaked",
        );
    }

    #[test]
    fn arg_submit_after_flip_to_cd_sets_dir_not_file() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(&mut h, &[("src/main.rs", "")]);
        h.type_text(":o ");
        h.snapshot();

        replace_palette_input(&mut h, "cd src");
        h.snapshot();
        h.type_keys("enter");

        assert!(h.stoat.command_palette.is_none());
        assert_eq!(
            h.stoat.active_workspace().git_root,
            root.join("src"),
            "Enter after the flip sets cwd to the directory, not the stale file",
        );
    }

    /// Open the palette, type `typed`, and return the action name the Enter
    /// handler dispatches, or `None` when it does not dispatch.
    fn palette_dispatch_name(h: &mut TestHarness, typed: &str) -> Option<&'static str> {
        h.type_text(typed);
        let mut palette = h.stoat.command_palette.take().expect("palette open");
        let ws = h.stoat.active_workspace_mut();
        match palette.handle_submit(ws) {
            PaletteOutcome::Dispatch(entry, _, _) => Some(entry.def.name()),
            _ => None,
        }
    }

    #[test]
    fn palette_alias_w_dispatches_save_buffer() {
        let mut h = Stoat::test();
        assert_eq!(palette_dispatch_name(&mut h, ":w"), Some("SaveBuffer"));
    }

    #[test]
    fn palette_alias_write_dispatches_save_buffer() {
        let mut h = Stoat::test();
        assert_eq!(palette_dispatch_name(&mut h, ":write"), Some("SaveBuffer"));
    }

    #[test]
    fn palette_alias_w_bang_dispatches_force_save_buffer() {
        let mut h = Stoat::test();
        assert_eq!(
            palette_dispatch_name(&mut h, ":w!"),
            Some("ForceSaveBuffer"),
        );
    }

    #[test]
    fn palette_optional_param_command_dispatches_bare() {
        let mut h = Stoat::test();
        assert_eq!(
            palette_dispatch_name(&mut h, ":config"),
            Some("OpenConfig"),
            "an optional first argument still dispatches on a bare submit",
        );
    }

    #[test]
    fn palette_optional_param_command_dispatches_with_its_arg() {
        let mut h = Stoat::test();
        h.type_text(":config stoatty");
        let mut palette = h.stoat.command_palette.take().expect("palette open");
        let ws = h.stoat.active_workspace_mut();
        match palette.handle_submit(ws) {
            PaletteOutcome::Dispatch(entry, args, _) => {
                assert_eq!(entry.def.name(), "OpenConfig");
                assert_eq!(args, vec![ParamValue::String("stoatty".to_string())]);
            },
            _ => panic!("`config stoatty` should dispatch with the target argument"),
        }
    }

    #[test]
    fn palette_partial_text_dispatches_fuzzy_selection() {
        let mut h = Stoat::test();
        h.type_text(":qui");
        let mut palette = h.stoat.command_palette.take().expect("palette open");
        let expected = palette.filtered[palette.selected].def.name();
        let ws = h.stoat.active_workspace_mut();
        match palette.handle_submit(ws) {
            PaletteOutcome::Dispatch(entry, _, _) => assert_eq!(entry.def.name(), expected),
            _ => panic!("partial fuzzy text should dispatch the top candidate"),
        }
    }

    #[test]
    fn palette_arrowed_selection_wins_over_alias() {
        let mut h = Stoat::test();
        h.type_text(":w");
        let mut palette = h.stoat.command_palette.take().expect("palette open");
        assert!(
            palette.filtered.len() >= 2,
            "expected >=2 fuzzy candidates for `w`"
        );
        palette.selected = 1;
        let expected = palette.filtered[1].def.name();
        assert_ne!(
            expected, "SaveBuffer",
            "SaveBuffer is not a fuzzy `w` match"
        );
        let ws = h.stoat.active_workspace_mut();
        match palette.handle_submit(ws) {
            PaletteOutcome::Dispatch(entry, _, _) => assert_eq!(entry.def.name(), expected),
            _ => panic!("an arrowed-to selection should dispatch that entry"),
        }
    }

    #[test]
    fn palette_param_alias_expands_to_the_command_name() {
        let mut h = Stoat::test();
        h.type_text(":o");
        let mut palette = h.stoat.command_palette.take().expect("palette open");
        let ws = h.stoat.active_workspace_mut();
        assert!(matches!(palette.handle_submit(ws), PaletteOutcome::None));
        assert_eq!(palette.input.text(ws), "open-file ");
    }

    /// `:cd ` with no query lists the workspace's directories beside a cleared
    /// preview pane.
    #[test]
    fn snapshot_command_palette_dir_arg() {
        let mut h = TestHarness::with_size(120, 30);
        seed_palette_workspace(
            &mut h,
            &[
                ("src/main.rs", ""),
                ("src/lib.rs", ""),
                ("docs/readme.md", ""),
            ],
        );
        h.type_text(":cd ");
        h.assert_snapshot("command_palette_dir_arg");
    }

    /// A free-typed argument command (no inline picker) shows the parameter it
    /// is collecting -- the name and description, then the command's long
    /// description -- instead of the emptied command list.
    #[test]
    fn snapshot_command_palette_free_arg() {
        let mut h = TestHarness::with_size(120, 30);
        h.type_text(":RenameWorkspace ");
        h.assert_snapshot("command_palette_free_arg");
    }

    #[test]
    fn free_arg_submit_dispatches() {
        let mut h = Stoat::test();
        assert_eq!(
            palette_dispatch_name(&mut h, ":RenameWorkspace newname"),
            Some("RenameWorkspace"),
        );
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

    /// Asserts against the painted frame rather than the filtered entries, so
    /// it covers the renderer reading the same string the matcher ranked on.
    #[test]
    fn the_list_shows_kebab_command_names() {
        let mut h = Stoat::test();
        h.type_text(":auto");
        let frame = h.snapshot();

        assert!(
            frame.content.contains("auto-reload"),
            "the row reads as a user command, got:\n{}",
            frame.content
        );
        assert!(
            !frame.content.contains("AutoReload"),
            "no row still reads as a code identifier, got:\n{}",
            frame.content
        );
    }

    #[test]
    fn abort_rebase_hidden_by_default_visible_after_backtab() {
        let mut h = Stoat::test();
        h.type_text(":abort");
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
        h.type_text(":foc");
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
            .core
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
        assert_eq!(arg_picker(&h).core.picklist.filtered.len(), 3);
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
        assert_eq!(arg_picker(&h).core.picklist.filtered.len(), 3);

        h.type_text("alp");
        h.snapshot();
        let picker = arg_picker(&h);
        assert_eq!(picker.core.picklist.filtered.len(), 1);
        let idx = picker.core.picklist.filtered[0];
        assert!(picker.core.picklist.base[idx].ends_with("alpha.rs"));
    }

    #[test]
    fn arg_picker_ctrl_n_moves_selection() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("a.rs", ""), ("b.rs", ""), ("c.rs", "")]);
        h.type_text(":o ");
        h.snapshot();
        assert_eq!(arg_picker(&h).core.picklist.selected, 0);

        // Arg-picker navigation rides Ctrl-p/Ctrl-n. Bare Up/Down recall history.
        h.type_keys("ctrl-n");
        h.snapshot();
        assert_eq!(arg_picker(&h).core.picklist.selected, 1);
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
        let preview_id = arg_picker(&h).core.preview.buffer;
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
            arg_picker(&h).core.picklist.filtered.len(),
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
        let preview_id = arg_picker(&h).core.preview.buffer;
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
