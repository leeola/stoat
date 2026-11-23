use crate::{
    app::Stoat,
    file_finder::{self, Browse},
    fuzzy,
    host::FsHost,
    input_view::{InputView, SubmitTarget},
    keymap::ActionKeys,
    pane::{FocusTarget, View},
    paths,
    picker::{self, PathPicker, PendingScan, PreviewPolicy, ScanTarget},
    rebase::RebasePause,
    workspace::Workspace,
};
use ratatui::layout::Rect;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    iter,
    path::{Path, PathBuf},
    ptr,
    sync::OnceLock,
};
use stoat_action::{
    registry, ActionDef, ActionKind, ParamDef, ParamError, ParamKind, ParamValue, ValueSource,
};
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
    /// Which parameter of [`Self::command`] the token being typed fills, as
    /// [`arg_index`] counts it in the input tail.
    ///
    /// Derived from the same parse as [`Self::command`], so the two change
    /// together, and only when the input does.
    typed_arg: usize,
    /// The command group the input's first token names while no command
    /// parses, such as `git` for `:git `. The list then holds only that
    /// group's commands, and the text after the token narrows them.
    pub(crate) group: Option<&'static str>,
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
    /// Commands the current scope offers before any query narrows them. The
    /// renderer sizes the modal's list against it and the `action` row.
    ///
    /// Captured from the unfiltered set rather than [`Self::filtered`], so a
    /// typed query leaves the box's size alone under the user typing it. Only
    /// a scope flip changes it.
    pub(crate) offering: usize,
    /// Internal actions the current scope offers, the actions that are no
    /// command, which the header counts as the actions under the `action` row.
    ///
    /// Only a scope flip changes it, as it changes [`Self::offering`].
    pub(crate) action_offering: usize,
    /// Whether the input lists the commands or the internal actions, which a
    /// leading `action ` token opens.
    pub(crate) namespace: Namespace,
    /// The widest plate, in columns, that the argument picker has drawn since
    /// the palette opened.
    ///
    /// The plate fits the widest label on show, and a tail that narrows the
    /// list does not pull the plate in under the user typing it. The width
    /// outlasts a change of the argument's source, and each open starts over
    /// at 0.
    pub(crate) arg_plate_width: u16,
    /// The rows that the Cmd Line took at the box's top in the last frame, 1
    /// before the first.
    ///
    /// An input wider than its row wraps onto more rows, and the body under
    /// them starts lower. The renderer measures the rows as it paints them,
    /// and the hit test and the smooth-scroll emit read the body after the
    /// paint, so they find the rects the frame drew.
    pub(crate) header_rows: u16,
    /// The command token at the input's start and its ghost, as the Cmd Line
    /// draws them, or `None` for an input that names no command.
    ///
    /// Each change of the input sets it, so an idle frame walks no registry.
    pub(crate) resolved: Option<ResolvedCommand>,
    /// The keys that run each action from the state the palette opened in,
    /// which the doc region names under the selected command.
    ///
    /// Built one time at open, as
    /// [`keys_by_action`](crate::keymap::Keymap::keys_by_action) builds it, so
    /// a frame walks no keymap.
    pub(crate) keys: ActionKeys,
    /// The lines of the selected entry's doc that the doc island scrolls
    /// past, 0 for a doc that shows from its first line.
    ///
    /// `C-d` and `C-u` move it by [`DOC_SCROLL_STEP`]. A new selection and a
    /// new filter start the doc at its first line again. The paint holds the
    /// scroll where the doc's last line stands on the island's last row, so a
    /// press past the end shows nothing new.
    pub(crate) doc_scroll: u16,
    /// The cells of each History entry that the last frame drew, with the
    /// index of the entry's line in the palette history, for the pointer.
    ///
    /// The paint stores it, because a row's height comes from the wrap in the
    /// cells before its age, and the age follows the clock. Empty after a
    /// frame that draws no History.
    pub(crate) history_rows: Vec<(Rect, usize)>,
}

/// A command token that the palette's input names, and the ghost that the Cmd
/// Line draws after it.
///
/// The ghost is the rest of the command's name that the user did not type, so
/// a typed `o` reads as `open`. It is display only: the input, its caret, Tab,
/// and Enter never see it.
pub(crate) struct ResolvedCommand {
    /// The entry that the head names, which decides whether the head draws as
    /// a command or as an internal action.
    pub(crate) entry: &'static registry::RegistryEntry,
    /// The typed head's length in chars, from the input's start.
    pub(crate) head_chars: usize,
    /// The text after the head, empty for a head that spells the whole name.
    pub(crate) ghost: String,
}

/// The list that the palette's input names, the commands at the root or the
/// internal actions under the `action` token.
///
/// A command is an action with an alias or a command name of its own, as
/// [`registry::RegistryEntry::is_command`] decides. The internal actions are
/// the lower-level API, so the root keeps them out of the way of the words a
/// user types every day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Namespace {
    Commands,
    Actions,
}

/// The token that opens the internal actions, typed and then a space.
pub(crate) const ACTION_TOKEN: &str = "action";

/// The lines that one `C-d` or `C-u` scrolls the doc island, half of its
/// six rows. A press keeps half of the lines that it shows in view.
pub(crate) const DOC_SCROLL_STEP: u16 = 3;

/// The def of the palette's `action` row, the one root row that opens the
/// internal actions instead of running an action of its own.
///
/// It lives outside the registry, as the def that [`crate::help`] holds for
/// `SetMode` does. With a def, the list, the pool page, the doc region, Tab,
/// and Enter read the row as any other. Its kind is a placeholder, because
/// nothing dispatches the row.
#[derive(Debug)]
struct ActionRowDef;

impl ActionDef for ActionRowDef {
    fn name(&self) -> &'static str {
        ACTION_TOKEN
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenCommandPalette
    }

    fn params(&self) -> &'static [ParamDef] {
        ACTION_ROW_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "Stoat's internal actions, the lower-level API"
    }

    fn long_desc(&self) -> &'static str {
        "List every action that is no command, the lower-level API that key \
         bindings and config files name. Type an action's name after the \
         token to narrow the list. Type its arguments after the name to run \
         it."
    }
}

static ACTION_ROW_PARAMS: &[ParamDef] = &[
    ParamDef {
        name: "action",
        kind: ParamKind::String,
        value_source: ValueSource::None,
        required: true,
        description: "The internal action to run, by its kebab or its PascalCase name.",
    },
    ParamDef {
        name: "args",
        kind: ParamKind::String,
        value_source: ValueSource::None,
        required: false,
        description: "The action's arguments.",
    },
];

/// The palette's `action` row, which ends the root list and opens the list of
/// the internal actions.
pub(crate) fn action_row() -> &'static registry::RegistryEntry {
    static ROW: OnceLock<registry::RegistryEntry> = OnceLock::new();
    ROW.get_or_init(|| registry::RegistryEntry {
        def: &ActionRowDef,
        create: |_| {
            Err(ParamError::Missing {
                name: "action",
                location: snafu::location!(),
            })
        },
        command_name: ACTION_TOKEN.to_owned(),
    })
}

/// Whether `entry` is the palette's `action` row.
pub(crate) fn is_action_row(entry: &registry::RegistryEntry) -> bool {
    ptr::eq(entry, action_row())
}

/// The inline value-picker the palette shows while collecting an argument
/// whose [`ValueSource`] offers candidates, such as [`ValueSource::Files`] or
/// [`ValueSource::Values`].
///
/// Wraps a shared [`PathPicker`] with the argument's [`ValueSource`], exactly as
/// [`crate::file_finder::FileFinder`] does. The palette parses the command's
/// trailing argument and drives the core's fuzzy list with it, so `:o src/ma`
/// filters the same way the standalone finder does.
pub(crate) struct ArgPicker {
    /// What this picker lists, such as workspace files or an argument's
    /// accepted values. Selects the preview policy and whether a streaming
    /// walk feeds the list.
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
    /// The fallback of ignored files under the workspace root, walked by
    /// [`FsHost::walk_all_files_streaming`] for a [`ValueSource::Files`]
    /// argument.
    ///
    /// Installed once [`Self::core`] lists nothing for a tail that names no
    /// explicit path, and kept until the picker is torn down.
    pub(crate) ignored: Option<PathPicker>,
    /// Position within a run of Tab presses, or `None` when the last Tab did
    /// not complete or its result has since been edited. Driven entirely by
    /// [`Self::advance_tab_cycle`].
    cycle: Option<TabCycle>,
}

/// A run of Tab presses walking one query's matches.
///
/// Completing narrows the picker to the completed value, which would leave
/// every later Tab re-completing that same row. Remembering the query the run
/// started from lets each press restore the original match list and step one
/// further into it.
struct TabCycle {
    /// Filter text the run started from. Refiltering against it undoes the
    /// narrowing the previous completion caused.
    query: String,
    /// Row last completed, as an index into `query`'s match list.
    index: usize,
    /// Text last written into the argument tail. A tail that still equals it
    /// means no edit landed between the two presses, which is what makes the
    /// next press a continuation rather than a new run.
    completed: String,
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
        core.picklist.keeps_base_order =
            matches!(source, ValueSource::Branches | ValueSource::Revisions);
        Self {
            source,
            core,
            browse: None,
            ignored: None,
            cycle: None,
        }
    }

    /// The tail a Tab press should complete into the argument, advancing the
    /// Tab cycle over the candidates matching what the user typed.
    ///
    /// The first press completes the highlighted row. Each press after it,
    /// recognized by `tail` still holding the previous press's result, steps to
    /// the next match of the query the run began with and wraps at the end, so
    /// a run of presses walks every candidate. Any edit to the tail in between
    /// ends the run, and the next press starts a fresh one from the edited
    /// text.
    ///
    /// Returns [`None`] when no row is selectable, leaving the input untouched.
    pub(crate) fn advance_tab_cycle(&mut self, tail: &str, git_root: &Path) -> Option<String> {
        let resumed = self
            .cycle
            .as_ref()
            .filter(|cycle| cycle.completed == tail)
            .map(|cycle| (cycle.query.clone(), cycle.index));

        let (query, index) = match resumed {
            Some((query, previous)) => {
                self.active_core().refilter(&query);
                let matches = self.active_core_ref().picklist.filtered.len();
                if matches == 0 {
                    self.cycle = None;
                    return None;
                }
                let index = (previous + 1) % matches;
                self.active_core().picklist.selected = index;
                (query, index)
            },
            None => (tail.to_string(), self.active_core_ref().picklist.selected),
        };

        let Some(selected) = self.selected_path() else {
            self.cycle = None;
            return None;
        };
        let completed =
            chosen_value(self.source, &paths::display_relative(selected, git_root)).to_string();

        self.cycle = Some(TabCycle {
            query,
            index,
            completed: completed.clone(),
        });
        Some(completed)
    }

    /// The picker currently driving the list. Browse mode (a `/` or `~/`
    /// argument) swaps in its own directory-walk picker; every other argument
    /// drives the workspace `core`, or its fallback of ignored files while
    /// [`Self::fallback_active`] holds.
    pub(crate) fn active_core(&mut self) -> &mut PathPicker {
        let fallback_active = self.fallback_active();
        match (&mut self.browse, &mut self.ignored) {
            (Some(browse), _) => &mut browse.picker,
            (None, Some(ignored)) if fallback_active => ignored,
            (None, _) => &mut self.core,
        }
    }

    pub(crate) fn active_core_ref(&self) -> &PathPicker {
        match (&self.browse, &self.ignored) {
            (Some(browse), _) => &browse.picker,
            (None, Some(ignored)) if self.fallback_active() => ignored,
            (None, _) => &self.core,
        }
    }

    /// Whether the fallback of ignored files stands in for the workspace list.
    pub(crate) fn fallback_active(&self) -> bool {
        self.browse.is_none() && self.ignored.is_some() && self.core.lists_nothing()
    }

    /// The root to walk for a fallback of ignored files, when `tail` needs one
    /// and the picker has none.
    ///
    /// Only a [`ValueSource::Files`] argument falls back, and only once the
    /// workspace list holds nothing for `tail`. A tail that starts with `/` or
    /// `~` names an explicit path, which submit dispatches as typed, so a walk
    /// serves nothing there.
    pub(crate) fn wants_ignored_fallback(&self, tail: &str) -> Option<PathBuf> {
        let tail = tail.trim_start();
        let explicit_path = tail.starts_with('/') || tail.starts_with('~');
        if self.source != ValueSource::Files
            || self.ignored.is_some()
            || explicit_path
            || !self.core.lists_nothing()
        {
            return None;
        }
        Some(self.core.git_root.clone())
    }

    /// The picker a scan for `target` reports back to.
    ///
    /// # Panics
    ///
    /// Panics for [`ScanTarget::Fallback`] when no fallback is installed.
    /// [`Self::begin_refilter`] hands out no such scan.
    pub(crate) fn scan_target_mut(&mut self, target: ScanTarget) -> &mut PathPicker {
        match (target, &mut self.browse) {
            (ScanTarget::List, Some(browse)) => &mut browse.picker,
            (ScanTarget::List, None) => &mut self.core,
            (ScanTarget::Fallback, _) => self
                .ignored
                .as_mut()
                .expect("a fallback scan has a fallback to land in"),
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
        let pumped = match &mut self.browse {
            Some(browse) => file_finder::pump_capped_walk(&mut browse.picker),
            None => self.core.pump_walk(),
        };
        if let Some(ignored) = &mut self.ignored {
            file_finder::pump_capped_walk(ignored);
        }
        pumped
    }

    /// Prepare the rankings for the active list and its fallback of ignored
    /// files, for a caller that runs them.
    ///
    /// Both lists can hold a whole walk, so the matching goes to a worker like
    /// the finder's. Browse filters its own directory walk against the partial
    /// it holds rather than the typed tail, the directory part having already
    /// gone into the walk's root.
    ///
    /// The fallback ranks the tail only while the list shows no row, or while
    /// its own walk still streams. Otherwise it forgets its results and costs
    /// nothing per keystroke.
    fn begin_refilter(&mut self, query: &str) -> Vec<PendingScan> {
        let begun = match &mut self.browse {
            Some(browse) => {
                browse.picker.pump_scan();
                let partial = browse.partial.clone();
                browse.picker.begin_scan(&partial)
            },
            None => {
                self.core.pump_scan();
                self.core.begin_scan(query)
            },
        };
        let mut pending: Vec<PendingScan> = ScanTarget::List.tag(begun).into_iter().collect();
        if let Some(ignored) = &mut self.ignored {
            ignored.pump_scan();
            if !self.core.rows_empty() && !ignored.walking() {
                ignored.forget_results();
            } else {
                pending.extend(ScanTarget::Fallback.tag(ignored.begin_scan(query)));
            }
        }
        pending
    }

    /// Bring the active list and its fallback up to date here and now, for a
    /// caller about to act on the selection rather than paint it.
    ///
    /// The fallback settles only when the list settles to no row, which is
    /// when it stands in for the list.
    fn settle_scan(&mut self, query: &str) {
        match &mut self.browse {
            Some(browse) => {
                let partial = browse.partial.clone();
                browse.picker.settle_scan(&partial);
            },
            None => self.core.settle_scan(query),
        }
        if self.core.lists_nothing()
            && let Some(ignored) = &mut self.ignored
        {
            ignored.settle_scan(query);
        }
    }

    /// Sync the preview pane per this picker's source. A buffer source previews
    /// the live in-memory buffer, falling back to the disk file when the path
    /// has none. A directory source shows nothing, as do a fixed value set, a
    /// branch list, and a revision list, whose rows name values rather than
    /// readable files. Every other source reads the file from disk.
    fn sync_preview(
        &mut self,
        ws: &mut Workspace,
        fs_host: &dyn FsHost,
        language_registry: &LanguageRegistry,
    ) {
        let policy = match self.source {
            ValueSource::Buffers => PreviewPolicy::LiveBufferThenFile,
            ValueSource::Directories => PreviewPolicy::Directory,
            ValueSource::Themes
            | ValueSource::Walkthroughs
            | ValueSource::Branches
            | ValueSource::Revisions
            | ValueSource::Values(_) => PreviewPolicy::NoPreview,
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

    /// Tear down the preview editor slots owned by the core, its fallback, and
    /// any active browse picker. Called on every palette-close and
    /// picker-teardown path.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.core.dispose(ws);
        if let Some(ignored) = &self.ignored {
            ignored.dispose(ws);
        }
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

impl PaletteScope {
    /// The names of the scopes that `S-Tab` reaches from this one, joined by
    /// ` / `, as the palette's key line names them. The names stand in the
    /// order that the key reaches the scopes.
    ///
    /// The cycle holds two scopes, so the answer names the other one.
    pub(crate) fn next_names(self) -> &'static str {
        match self {
            PaletteScope::Active => "all",
            PaletteScope::All => "applicable",
        }
    }
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
    /// The focused pane is an editor showing the side-by-side diff, so the
    /// move-provenance actions have a hunk to resolve against.
    pub diff_view_open: bool,
    /// The focused pane shows a commits list.
    pub commits_open: bool,
    /// The focused pane shows a changes view.
    pub diff_changes_open: bool,
    /// `workspace.review_walk.is_some()`: a commit-by-commit review walk is
    /// running, whether or not one of its diffs is currently open.
    pub review_walk_open: bool,
    /// `workspace.walkthrough.is_some()`: a walkthrough plays, so its stepping
    /// actions have a tour to step.
    pub walkthrough_open: bool,
    /// Focused pane hosts a [`View::Run`], or a modal run is active.
    pub run_focused: bool,
    /// `workspace.pair_panes().is_some()`: two split panes show two different
    /// files, so the pair diff has its two sides.
    pub pair_open: bool,
    /// `workspace.repo_root.is_some()`: the workspace root sits inside a git
    /// repository, so the git commands have one to act on.
    pub in_repo: bool,
    /// `workspace.merge_state().is_some()`: the last tally read a merge in
    /// progress, so the merge's abort and continue commands have one to end.
    pub in_merge: bool,
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
        let diff_view_open = match &focused_view {
            Some(View::Editor(id)) => ws.editors.get(*id).is_some_and(|e| e.diff_view),
            _ => false,
        };

        Self {
            in_rebase_plan: ws.rebase.is_some(),
            in_rebase_exec: ws.rebase_active.is_some(),
            in_rebase_reword,
            in_conflict,
            diff_view_open,
            commits_open: ws.focused_commits().is_some(),
            diff_changes_open: ws.focused_diff_changes().is_some(),
            review_walk_open: ws.review_walk.is_some(),
            walkthrough_open: ws.walkthrough.is_some(),
            run_focused,
            pair_open: ws.pair_panes().is_some(),
            in_repo: ws.repo_root.is_some(),
            in_merge: ws.merge_state().is_some(),
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

        RebaseContinue | GitRebaseContinue => ctx.in_rebase_exec,
        GitRebaseAbort => ctx.in_rebase_plan || ctx.in_rebase_exec,
        RewordConfirm | RewordAbort => ctx.in_rebase_reword,

        RebaseConflictTakeOurs
        | RebaseConflictTakeTheirs
        | RebaseConflictSkipEntry
        | RebaseConflictNextFile
        | RebaseConflictPrevFile
        | RebaseConflictApply
        | RebaseConflictAbort => ctx.in_conflict,

        JumpToMoveSource
        | JumpToMoveTarget
        | JumpToNextMoveSource
        | JumpToPrevMoveSource
        | QueryMoveRelationships => ctx.diff_view_open,

        DiffPair => ctx.pair_open,

        CloseCommits | CommitsNext | CommitsPrev | CommitsPageDown | CommitsPageUp
        | CommitsFirst | CommitsLast | CommitsRefresh | CommitsOpenReview => ctx.commits_open,

        // DiffChanges is absent on purpose. Like DiffFiles, it opens a screen
        // from anywhere, and on the changes view it flips back to the per-file
        // diff.
        CloseDiffChanges
        | DiffChangesNext
        | DiffChangesPrev
        | DiffChangesPageDown
        | DiffChangesPageUp
        | DiffChangesHalfPageDown
        | DiffChangesHalfPageUp
        | DiffChangesFirst
        | DiffChangesLast
        | DiffChangesOpen
        | DiffChangesExpandUp
        | DiffChangesExpandDown
        | DiffChangesExpandAll
        | DiffChangesCollapse => ctx.diff_changes_open,

        ReviewNextCommit | ReviewPrevCommit | ReviewDone => ctx.review_walk_open,

        GitLs | GitReview | Review | GitCheckout | GitBranch | GitCommit | GitAmend | GitAdd
        | GitRebase | GitStatus | GitDiff | GitDiffCached | GitStash | GitStashPush
        | GitStashPop | GitStashList | GitMerge | ToggleBlame => ctx.in_repo,
        GitMergeAbort | GitMergeContinue => ctx.in_repo && ctx.in_merge,

        // WalkthroughOpen is absent on purpose. A reader opens a tour when none
        // plays, so a gate on one hides it exactly when it is wanted.
        WalkthroughNext
        | WalkthroughPrev
        | WalkthroughNextAnnotation
        | WalkthroughPrevAnnotation
        | WalkthroughForward
        | WalkthroughBackward
        | WalkthroughShowNarration
        | WalkthroughShowDescription
        | WalkthroughDone => ctx.walkthrough_open,

        RunSubmit | RunInterrupt | RunHistoryPrev | RunHistoryNext => ctx.run_focused,

        _ => true,
    }
}

pub(crate) enum PaletteOutcome {
    /// Re-render but keep the palette open.
    None,
    /// An action is ready to dispatch, with any inline argument parsed into
    /// its parameter list. The third field is the canonical, re-executable
    /// command line to record in palette history.
    Dispatch(&'static registry::RegistryEntry, Vec<ParamValue>, String),
}

/// Why [`parse_args`] refused the tokens typed for a command with more than one
/// parameter.
#[derive(Debug, PartialEq)]
enum ArgsError {
    /// A token does not parse as its parameter's kind.
    Invalid { param: &'static str, arg: String },
    /// A required parameter has no token.
    Missing { param: &'static str },
    /// A token falls past the last parameter, so no parameter takes it.
    Extra { arg: String },
}

impl CommandPalette {
    pub fn new(
        ws: &mut Workspace,
        executor: Executor,
        availability: Availability,
        keys: ActionKeys,
    ) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::PaletteFilter, "", "insert");
        let scope = PaletteScope::Active;
        let mut filtered = Vec::new();
        let mut match_indices = Vec::new();
        let mut selected = 0;
        refilter(
            "",
            None,
            scope,
            &availability,
            &mut filtered,
            &mut match_indices,
            &mut selected,
        );
        let (offering, action_offering) = scope_offering(scope, &availability);
        Self {
            input,
            filtered,
            match_indices,
            selected,
            command: None,
            typed_arg: 0,
            group: None,
            scope,
            availability,
            viewport_rows: None,
            arg_picker: None,
            generation: picker::next_generation(),
            last_filter_key: 0,
            offering,
            action_offering,
            namespace: Namespace::Commands,
            arg_plate_width: 0,
            header_rows: 1,
            resolved: None,
            keys,
            doc_scroll: 0,
            history_rows: Vec::new(),
        }
    }

    /// Rows the modal's list needs for the whole candidate set behind it,
    /// whichever list is showing.
    ///
    /// The command list counts its `action` row with the commands. The list of
    /// the internal actions keeps the command list's box, so the box holds its
    /// place when `action ` opens that list.
    ///
    /// Arg mode lists the argument picker's candidates, so `:o ` over a large
    /// workspace sizes to the file count rather than the command count. Both
    /// sources are unfiltered bases, so the answer holds while the user types.
    pub(crate) fn list_rows_hint(&self) -> u16 {
        match self.arg_picker.as_ref().filter(|_| self.command.is_some()) {
            Some(picker) => rows_of(picker.active_core_ref().picklist.base.len()),
            None => rows_of(self.offering + 1),
        }
    }

    /// The bytes at the start of each listed command name that the group's
    /// title already shows, the group token and its space, which the rows
    /// leave out. Zero outside a group.
    pub(crate) fn name_offset(&self) -> usize {
        self.group.map_or(0, |group| group.len() + 1)
    }

    /// Flip the palette's [`PaletteScope`] and re-run the current-input
    /// filter against the new scope. Called from the `PaletteScopeToggle`
    /// action handler (Shift-Tab).
    pub(crate) fn toggle_scope(&mut self, ws: &Workspace) {
        self.scope = match self.scope {
            PaletteScope::Active => PaletteScope::All,
            PaletteScope::All => PaletteScope::Active,
        };
        (self.offering, self.action_offering) = scope_offering(self.scope, &self.availability);
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

    /// The parameter the argument being typed fills.
    ///
    /// `None` while no command parses, and once the typed tokens run past the
    /// last parameter. A command with one parameter takes the whole tail as its
    /// value, so that parameter answers however many spaces the tail holds.
    pub(crate) fn arg_param(&self) -> Option<&'static ParamDef> {
        param_at(self.command?.def.params(), self.typed_arg)
    }

    /// The value source of the argument being typed when it drives an inline
    /// picker ([`ValueSource::Files`], [`ValueSource::Directories`],
    /// [`ValueSource::Buffers`], [`ValueSource::Themes`],
    /// [`ValueSource::Branches`], [`ValueSource::Revisions`], or
    /// [`ValueSource::Values`], e.g. `:o `,
    /// `:cd `, `:b `, `:SetTheme `, `:git log `, or `:auto-reload `), or `None`
    /// otherwise. Gates rendering the picker and routing selection keys to it.
    pub(crate) fn arg_source(&self) -> Option<ValueSource> {
        match self.arg_param()?.value_source {
            ValueSource::None => None,
            source => Some(source),
        }
    }

    /// The trailing argument text in picker-argument mode, or `None` otherwise.
    /// Drives the inline picker's filter. The tail is everything after the
    /// command's head, as [`split_head`] finds it, so a path argument keeps
    /// its spaces.
    ///
    /// A command with more than one parameter filters on the token being typed
    /// alone, which is empty right after a space.
    pub(crate) fn arg_tail(&self, ws: &Workspace) -> Option<String> {
        self.arg_source()?;
        let text = self.input.text(ws);
        let (entry, _, tail) = split_head(&text, &self.availability)?;
        let tail = if entry.def.params().len() > 1 {
            split_last_token(tail).1
        } else {
            tail
        };
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
    ) -> Vec<PendingScan> {
        let Some(picker) = self.arg_picker.as_mut() else {
            return Vec::new();
        };
        picker.pump_walk();
        let pending = picker.begin_refilter(tail);
        picker.sync_preview(ws, fs_host, language_registry);
        pending
    }

    /// Bring the arg picker's rows up to date with `tail` on this thread.
    ///
    /// For a caller about to read the selection rather than paint it. A scan
    /// running elsewhere answers the tail that started it, and completing or
    /// submitting between keystrokes would otherwise act on the row the
    /// previous tail selected.
    pub(crate) fn settle_arg_picker(&mut self, tail: &str) {
        if let Some(picker) = self.arg_picker.as_mut() {
            picker.settle_scan(tail);
        }
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
        self.generation = picker::next_generation();
        self.doc_scroll = 0;

        self.namespace = match action_query(&text) {
            Some(_) => Namespace::Actions,
            None => Namespace::Commands,
        };
        self.resolved = resolve_typed(&text, self.scope, &self.availability);
        let parsed = parse_command(&text, &self.availability);
        self.command = parsed.map(|(entry, _)| entry);
        self.typed_arg = parsed.map_or(0, |(_, arg)| arg_index(arg));
        self.group = input_group(&text, &self.availability);
        if self.command.is_some() {
            self.filtered.clear();
            self.match_indices.clear();
            self.selected = 0;
        } else {
            refilter(
                &text,
                self.group,
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
    /// argument is parsed into the command's parameters and dispatched. A
    /// command with one parameter takes the whole tail as its value, and one
    /// with more takes a token per parameter through [`parse_args`].
    /// Otherwise the selected action is taken. A zero-arg action dispatches
    /// immediately, while a parameter-taking action rewrites the input to
    /// `"<name> "` to begin inline argument entry. Called from the
    /// `SubmitPromptInput` action handler while the palette is open.
    ///
    /// While the history walks, Enter runs the recalled line as typed. No row
    /// of an argument list stands in for its argument, and the line runs at
    /// any selection. A recalled line that names no command runs nothing,
    /// because History shows no list row to fall back to.
    pub(crate) fn handle_submit(&mut self, ws: &mut Workspace) -> PaletteOutcome {
        let walking = ws.palette_history.is_walking();
        let arg_picker = self.arg_picker.as_ref().filter(|_| !walking);
        let text = self.input.text(ws);
        if let Some((entry, arg)) = parse_command(&text, &self.availability) {
            let params = entry.def.params();
            if params.len() > 1 {
                let picked = param_at(params, arg_index(arg)).and_then(|param| {
                    let picker =
                        arg_picker.filter(|picker| picker.source() == param.value_source)?;
                    let row = picker.selected_path()?.to_string_lossy();
                    Some(chosen_value(param.value_source, &row).to_string())
                });
                return match parse_args(params, arg, picked.as_deref()) {
                    Ok((values, tokens)) => {
                        self.input.dispose(ws);
                        let line = format!("{} {tokens}", history_head(entry));
                        PaletteOutcome::Dispatch(entry, values, line)
                    },
                    Err(err) => {
                        tracing::warn!(
                            target: "stoat::command",
                            command = entry.def.name(),
                            ?err,
                            "command arguments rejected"
                        );
                        PaletteOutcome::None
                    },
                };
            }

            let param = &params[0];
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
                arg_picker
                    .filter(|picker| picker.source() == param.value_source)
                    .and_then(|picker| picker.browse_selected_path())
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|| arg.to_string())
            } else {
                arg_picker
                    .filter(|picker| picker.source() == param.value_source)
                    .and_then(|picker| picker.selected_path())
                    .map(|path| {
                        chosen_value(param.value_source, &path.to_string_lossy()).to_string()
                    })
                    .unwrap_or_else(|| arg.to_string())
            };
            return match ParamValue::parse(param.kind, &chosen) {
                Ok(value) => {
                    self.input.dispose(ws);
                    let line = format!("{} {}", history_head(entry), chosen);
                    PaletteOutcome::Dispatch(entry, vec![value], line)
                },
                Err(err) => {
                    tracing::warn!(
                        target: "stoat::command",
                        command = entry.def.name(),
                        param = param.name,
                        arg = %chosen,
                        %err,
                        "command argument rejected"
                    );
                    PaletteOutcome::None
                },
            };
        }

        // refilter pins an exact name or alias match to the top slot, so the
        // default selection already sits on that command. This branch is the
        // belt for the frame between a keystroke and the next refilter. While
        // the selection sits at the top slot, an exact name or alias match of
        // the typed text wins, so `w` dispatches SaveBuffer and a name-free
        // alias like `w!` stays reachable. Arrowing to a candidate takes that
        // highlighted entry instead.
        // A conditional token resolves ahead of the alias table so an active
        // context wins, and falls through to it when no context claims the
        // token. Zero-argument commands never reach `parse_command`, so this is
        // where a bare `next` or `done` is resolved. The root resolves only
        // commands, so a bare internal action runs under the `action` token.
        let bare = text.trim();
        if (self.selected == 0 || walking)
            && let Some(entry) = resolve_head(bare, &self.availability)
        {
            if dispatches_bare(entry) {
                self.input.dispose(ws);
                return PaletteOutcome::Dispatch(entry, Vec::new(), history_head(entry));
            }
            self.input
                .replace_text(ws, &format!("{} ", typed_head(entry)));
            return PaletteOutcome::None;
        }
        if walking {
            tracing::info!(target: "stoat::command", text = %text, "no command matched");
            return PaletteOutcome::None;
        }

        match self.filtered.get(self.selected).copied() {
            Some(entry) if dispatches_bare(entry) => {
                self.input.dispose(ws);
                PaletteOutcome::Dispatch(entry, Vec::new(), history_head(entry))
            },
            Some(entry) => {
                self.input
                    .replace_text(ws, &format!("{} ", typed_head(entry)));
                PaletteOutcome::None
            },
            None => {
                tracing::info!(target: "stoat::command", text = %text, "no command matched");
                PaletteOutcome::None
            },
        }
    }
}

/// Bare palette tokens whose meaning depends on application state.
///
/// A conditional command is a short generic word that would be a poor global
/// alias -- `next` is meaningful during a review walk and meaningless outside
/// one -- so instead of claiming the name once, it resolves per invocation
/// against the same [`Availability`] snapshot the Active scope filters by.
///
/// Each token carries an ordered candidate list, so a second context wanting
/// `next` appends its action rather than fighting over the name. The first
/// candidate whose context is active wins. Resolution runs ahead of the static
/// alias table, so an active context beats a 1:1 command name, and falls
/// through to [`registry::lookup_alias`] when no candidate applies.
const CONDITIONAL_COMMANDS: &[(&str, &[ActionKind])] = &[
    (
        "next",
        &[ActionKind::ReviewNextCommit, ActionKind::WalkthroughNext],
    ),
    (
        "prev",
        &[ActionKind::ReviewPrevCommit, ActionKind::WalkthroughPrev],
    ),
    (
        "done",
        &[ActionKind::ReviewDone, ActionKind::WalkthroughDone],
    ),
];

/// Resolve `token` to the first [`CONDITIONAL_COMMANDS`] candidate whose
/// context is active, or `None` when the token is not conditional or no
/// candidate applies.
fn resolve_conditional(
    token: &str,
    ctx: &Availability,
) -> Option<&'static registry::RegistryEntry> {
    let (_, candidates) = CONDITIONAL_COMMANDS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(token))?;
    let kind = candidates
        .iter()
        .copied()
        .find(|kind| action_is_available(*kind, ctx))?;
    registry::all().find(|entry| entry.def.kind() == kind)
}

/// Split palette input into a resolved command and its trailing argument text.
///
/// Returns `Some((entry, arg))` only when the text is a command head followed
/// by a space, as [`split_head`] finds it, and the command takes at least one
/// parameter. `arg` is everything after the head, so a path argument keeps its
/// own spaces. Returns `None` for plain filter text, an unknown head, or a
/// zero-argument command, keeping the palette in command-filter mode.
fn parse_command<'a>(
    text: &'a str,
    ctx: &Availability,
) -> Option<(&'static registry::RegistryEntry, &'a str)> {
    let (entry, _, arg) = split_head(text, ctx)?;
    (!entry.def.params().is_empty()).then_some((entry, arg))
}

/// Split palette input into the command its head names, the head, and the text
/// after the space that ends the head.
///
/// A command name such as `git log` holds spaces, so the head is the longest
/// run of leading tokens that names a command, as [`resolve_head`] resolves
/// one. Returns `None` when neither the whole text nor a head that a space
/// ends names a command.
///
/// The whole text is tried first as a command that takes no argument, and it
/// answers with an empty tail. So `review done` ends a review where `review
/// feature` names a revision, and `git rebase continue` never reads as `git
/// rebase` with the argument `continue`.
pub(crate) fn split_head<'a>(
    text: &'a str,
    ctx: &Availability,
) -> Option<(&'static registry::RegistryEntry, &'a str, &'a str)> {
    if let Some(entry) = resolve_head(text, ctx).filter(|entry| entry.def.params().is_empty()) {
        return Some((entry, text, ""));
    }
    text.rmatch_indices(' ').find_map(|(i, _)| {
        let head = &text[..i];
        resolve_head(head, ctx).map(|entry| (entry, head, &text[i + 1..]))
    })
}

/// The entry that the palette head `head` names.
///
/// Under the `action` token the head names an internal action, as
/// [`lookup_action`] finds one. Otherwise it names a command: a conditional
/// command whose context is active first, then a command as [`lookup_root`]
/// finds one.
fn resolve_head(head: &str, ctx: &Availability) -> Option<&'static registry::RegistryEntry> {
    match action_query(head) {
        Some(name) => lookup_action(name),
        None => resolve_conditional(head, ctx).or_else(|| lookup_root(head)),
    }
}

/// The text after a leading `action` token and its space, ASCII case ignored,
/// or `None` for a text that lists the commands.
pub(crate) fn action_query(text: &str) -> Option<&str> {
    let (token, rest) = text.split_once(' ')?;
    token.eq_ignore_ascii_case(ACTION_TOKEN).then_some(rest)
}

/// The visible command that `token` names, by its exact name, its command
/// name, or an alias, in the order [`registry::lookup_alias`] ranks them.
///
/// Only commands take part, so an internal action's name never hides a
/// command's alias, and a hidden action resolves nowhere in the palette.
fn lookup_root(token: &str) -> Option<&'static registry::RegistryEntry> {
    let command = |entry: &&'static registry::RegistryEntry| {
        entry.is_command() && entry.def.palette_visible()
    };
    registry::lookup(token)
        .filter(command)
        .or_else(|| {
            registry::all()
                .filter(command)
                .find(|entry| entry.command_name.eq_ignore_ascii_case(token))
        })
        .or_else(|| {
            registry::all().filter(command).find(|entry| {
                entry
                    .def
                    .aliases()
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(token))
            })
        })
}

/// The visible internal action that `name` names, by its exact name or its
/// command name, ASCII case ignored.
fn lookup_action(name: &str) -> Option<&'static registry::RegistryEntry> {
    registry::all().find(|entry| {
        !entry.is_command()
            && entry.def.palette_visible()
            && (entry.def.name().eq_ignore_ascii_case(name)
                || entry.command_name.eq_ignore_ascii_case(name))
    })
}

/// The text that names `entry` in the palette's input, which Enter and Tab
/// write.
///
/// A command and the `action` row read by their command name. An internal
/// action reads as the `action` token and its kebab name, because only that
/// form resolves it.
pub(crate) fn typed_head(entry: &registry::RegistryEntry) -> String {
    match entry.is_command() || is_action_row(entry) {
        true => entry.command_name.clone(),
        false => format!("{ACTION_TOKEN} {}", entry.command_name),
    }
}

/// The command token that palette input `text` names, as the Cmd Line draws
/// it, or `None` when the text names no command.
///
/// At the root the head follows [`named_command`], and the ghost follows
/// [`ghost_of`]. Under the `action` token the head is the token and the
/// action's name when the name resolves. Else the head is the token alone,
/// which names the `action` row. Neither has a ghost.
fn resolve_typed(text: &str, scope: PaletteScope, ctx: &Availability) -> Option<ResolvedCommand> {
    if let Some(name) = action_query(text) {
        let (entry, head) = split_head(text, ctx)
            .map(|(entry, head, _)| (entry, head))
            .or_else(|| lookup_action(name.trim_end()).map(|entry| (entry, text.trim_end())))
            .unwrap_or((action_row(), &text[..ACTION_TOKEN.len()]));
        return Some(ResolvedCommand {
            entry,
            head_chars: head.chars().count(),
            ghost: String::new(),
        });
    }

    let (entry, head) = named_command(text, scope, ctx)?;
    Some(ResolvedCommand {
        entry,
        head_chars: head.chars().count(),
        ghost: ghost_of(head, &entry.command_name),
    })
}

/// The command that the typed head of palette input `text` names at the root,
/// and the head, or `None` when no head names a command.
///
/// A head that [`split_head`] finds names its command first. Else the whole
/// text less its trailing whitespace names a command, as [`resolve_head`]
/// resolves one. Else a text with no whitespace names the one command that
/// `scope` offers whose command name starts with it, ASCII case ignored. The
/// `action` row takes part as one more command. So `quit-al` names
/// `quit-all`, `act` names the `action` row, and `wri` names none of the
/// commands that start with it.
fn named_command<'a>(
    text: &'a str,
    scope: PaletteScope,
    ctx: &Availability,
) -> Option<(&'static registry::RegistryEntry, &'a str)> {
    if let Some((entry, head, _)) = split_head(text, ctx) {
        return Some((entry, head));
    }

    let whole = text.trim_end();
    if let Some(entry) = resolve_head(whole, ctx) {
        return Some((entry, whole));
    }

    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    let starts = |entry: &&'static registry::RegistryEntry| {
        entry
            .command_name
            .get(..text.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(text))
    };
    let mut named = registry::all()
        .filter(|entry| entry.is_command() && offers(entry, scope, ctx))
        .chain(iter::once(action_row()))
        .filter(starts);
    match (named.next(), named.next()) {
        (Some(entry), None) => Some((entry, text)),
        _ => None,
    }
}

/// The ghost that the Cmd Line draws after `head`, the typed head of the
/// command named `name`.
///
/// A head that starts the name, ASCII case ignored, has the rest of the name,
/// so `o` gives `pen`, and a head that spells the whole name has no ghost. Any
/// other head is an alias with no rest to show, so its ghost is a space,
/// U+00B7, a space, and the whole name.
fn ghost_of(head: &str, name: &str) -> String {
    match name.get(..head.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(head) => name[head.len()..].to_owned(),
        _ => format!(" \u{b7} {name}"),
    }
}

/// Which parameter the token being typed at the end of `tail` fills, counting
/// from zero.
///
/// A space ends a token, so a tail that ends in whitespace starts the next
/// parameter before its first character arrives.
fn arg_index(tail: &str) -> usize {
    let tokens = tail.split_whitespace().count();
    if tail.ends_with(char::is_whitespace) {
        tokens
    } else {
        tokens.saturating_sub(1)
    }
}

/// `tail` split before the token being typed, as the text kept ahead of it and
/// the token itself.
///
/// The token is empty when `tail` ends in whitespace. The kept text holds its
/// whitespace byte for byte, so a completion written after it keeps the
/// spacing the user typed.
pub(crate) fn split_last_token(tail: &str) -> (&str, &str) {
    let start = tail.trim_end_matches(|c: char| !c.is_whitespace()).len();
    tail.split_at(start)
}

/// The parameter of `params` that the argument at `index` fills.
///
/// A lone parameter takes the whole tail, spaces and all, so it answers every
/// index. `None` past the last of several.
fn param_at(params: &'static [ParamDef], index: usize) -> Option<&'static ParamDef> {
    match params {
        [only] => Some(only),
        _ => params.get(index),
    }
}

/// The values a command with more than one parameter takes from `tail`, and
/// the tokens as one space-separated line for palette history.
///
/// Each whitespace-separated token fills the next parameter in order.
/// `picked` is the highlighted row of the picker for the token being typed.
/// It stands in for that token, or follows the tokens when `tail` ends in
/// whitespace, so Enter takes the row as it does for a lone parameter.
fn parse_args(
    params: &'static [ParamDef],
    tail: &str,
    picked: Option<&str>,
) -> Result<(Vec<ParamValue>, String), ArgsError> {
    let mut tokens: Vec<&str> = tail.split_whitespace().collect();
    if let Some(picked) = picked {
        match tokens.get_mut(arg_index(tail)) {
            Some(token) => *token = picked,
            None => tokens.push(picked),
        }
    }
    if let Some(extra) = tokens.get(params.len()) {
        return Err(ArgsError::Extra {
            arg: extra.to_string(),
        });
    }

    let mut values = Vec::with_capacity(tokens.len());
    for (param, token) in params.iter().zip(&tokens) {
        let value = ParamValue::parse(param.kind, token).map_err(|_| ArgsError::Invalid {
            param: param.name,
            arg: token.to_string(),
        })?;
        values.push(value);
    }
    if let Some(param) = params[tokens.len()..].iter().find(|param| param.required) {
        return Err(ArgsError::Missing { param: param.name });
    }

    Ok((values, tokens.join(" ")))
}

/// The command group the palette lists for `text`, which the text's first token
/// names while the text parses as no command.
///
/// The list under the `action` token holds no group, so a text there answers
/// `None`.
fn input_group(text: &str, ctx: &Availability) -> Option<&'static str> {
    if action_query(text).is_some() || parse_command(text, ctx).is_some() {
        return None;
    }
    let (token, _) = text.split_once(' ')?;
    registry::group_of(token)
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

/// The head to record for `entry` in palette history, its first alias when it
/// has one, else its [`typed_head`].
///
/// Each re-resolves through [`parse_command`], so a recalled `head [arg]` line
/// runs again. A command without an alias records its command name. An
/// internal action records the `action` token and its kebab name, because the
/// root resolves no internal action. The alias is preferred because it reads
/// like the short form a user types (`cd`, not `set-cwd`).
fn history_head(entry: &registry::RegistryEntry) -> String {
    match entry.def.aliases().first() {
        Some(alias) => (*alias).to_owned(),
        None => typed_head(entry),
    }
}

/// `line`, a palette history line from a saved state, in the form that the
/// palette runs.
///
/// A line whose first word names an internal action and no command gains the
/// `action` token. The root resolves no internal action, so a saved line that
/// names one bare runs nothing. Any other line stays as it is.
pub(crate) fn migrate_history_line(line: String) -> String {
    let first = line.split_whitespace().next().unwrap_or("");
    match lookup_action(first).is_some() && lookup_root(first).is_none() {
        true => format!("{ACTION_TOKEN} {line}"),
        false => line,
    }
}

/// The value a picker row under `source` stands for.
///
/// A [`ValueSource::Revisions`] row names a branch, or a commit as its short
/// sha and summary. The argument takes the revision alone, so the row is cut
/// at its first space. Every other source's row is its value whole, which
/// keeps a path with spaces in one piece.
fn chosen_value(source: ValueSource, row: &str) -> &str {
    match source {
        ValueSource::Revisions => row.split_once(' ').map_or(row, |(value, _)| value),
        _ => row,
    }
}

/// Whether `entry` belongs in the palette's list under `scope`.
///
/// The Active scope hides actions the current workspace state cannot run, so a
/// scope flip changes how many commands the palette offers, not just their
/// order.
fn offers(
    entry: &registry::RegistryEntry,
    scope: PaletteScope,
    availability: &Availability,
) -> bool {
    entry.def.palette_visible()
        && (scope != PaletteScope::Active || action_is_available(entry.def.kind(), availability))
}

/// How many commands and how many internal actions `scope` offers before any
/// query narrows them.
fn scope_offering(scope: PaletteScope, availability: &Availability) -> (usize, usize) {
    registry::all()
        .filter(|entry| offers(entry, scope, availability))
        .fold((0, 0), |(commands, actions), entry| {
            match entry.is_command() {
                true => (commands + 1, actions),
                false => (commands, actions + 1),
            }
        })
}

/// A candidate count as list rows, saturating past what any screen could show.
fn rows_of(count: usize) -> u16 {
    u16::try_from(count).unwrap_or(u16::MAX)
}

/// Rank the entries that palette input `input` lists under `scope` into
/// `filtered` and `match_indices`, and clamp `selected` to the result.
///
/// At the root the list holds the commands. Under `group` only the group's
/// commands appear, and the text after the group token narrows them. With no
/// group the `action` row ends the list when the query holds no atom or
/// matches the row's name. The row leads the list when the query is the word
/// `action`. Under the `action` token the list holds the internal actions,
/// which the text after the token narrows by their kebab names. Each match
/// position indexes the whole command name either way.
#[allow(clippy::too_many_arguments)]
pub(crate) fn refilter(
    input: &str,
    group: Option<&str>,
    scope: PaletteScope,
    availability: &Availability,
    filtered: &mut Vec<&'static registry::RegistryEntry>,
    match_indices: &mut Vec<Vec<u32>>,
    selected: &mut usize,
) {
    let actions = action_query(input);
    let visible: Vec<&'static registry::RegistryEntry> = registry::all()
        .filter(|entry| offers(entry, scope, availability))
        .filter(|entry| entry.is_command() == actions.is_none())
        .filter(|entry| {
            group.is_none_or(|group| {
                entry
                    .command_name
                    .split_once(' ')
                    .is_some_and(|(head, _)| head == group)
            })
        })
        .collect();

    // A group's members match by their subcommand token, since the letters of
    // the group token, which every member shares, narrow nothing.
    let (query, offset) = match (actions, group) {
        (Some(rest), _) => (rest, 0),
        (None, Some(group)) => (
            input.split_once(' ').map_or("", |(_, rest)| rest),
            group.len() + 1,
        ),
        (None, None) => (input, 0),
    };
    filtered.clear();
    match_indices.clear();
    rank_entries(query, offset, visible, filtered, match_indices);

    // Pin an exact name or alias match to the top so the displayed list agrees
    // with what Enter dispatches. `w` (SaveBuffer) and `o` (OpenFile) resolve by
    // alias, which the name-only fuzzy rank never surfaces first -- or at all,
    // when the name lacks the typed characters.
    let needle = actions.unwrap_or(input).trim();
    let pinned = match actions {
        Some(_) => lookup_action(needle),
        None => lookup_root(needle),
    };
    if let Some(pinned) = pinned.filter(|pinned| offers(pinned, scope, availability)) {
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

    if actions.is_none() && group.is_none() {
        let row = action_row();
        if needle.eq_ignore_ascii_case(ACTION_TOKEN) {
            filtered.insert(0, row);
            match_indices.insert(0, (0..ACTION_TOKEN.len() as u32).collect());
        } else {
            let matched = match fuzzy::match_and_rank(query, iter::once((row, ACTION_TOKEN))) {
                None => Some(Vec::new()),
                Some(matches) => matches.into_iter().next().map(|m| m.matched_indices),
            };
            if let Some(indices) = matched {
                filtered.push(row);
                match_indices.push(indices);
            }
        }
    }

    picker::nav_clamp(filtered.len(), selected);
}

/// Rank `visible` against `query` into `filtered` and `match_indices`, each
/// name matched past its first `offset` bytes.
///
/// A query with no atom lists every entry by priority and then by name. A
/// match ranks by its score, then by priority, then by name.
fn rank_entries(
    query: &str,
    offset: usize,
    visible: Vec<&'static registry::RegistryEntry>,
    filtered: &mut Vec<&'static registry::RegistryEntry>,
    match_indices: &mut Vec<Vec<u32>>,
) {
    let items = visible
        .iter()
        .copied()
        .map(|entry| (entry, &entry.command_name[offset..]));
    let Some(mut matches) = fuzzy::match_and_rank(query, items) else {
        let mut all = visible;
        all.sort_by_key(|e| (e.def.priority().ord(), e.command_name.as_str()));
        for entry in all {
            filtered.push(entry);
            match_indices.push(Vec::new());
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
        match_indices.push(
            m.matched_indices
                .into_iter()
                .map(|index| index + offset as u32)
                .collect(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        badge::BadgeSource,
        buffer_registry::AutoReloadMode,
        host::FakeFsOp,
        input_history::InputHistory,
        picker::PREVIEW_DIR_LIMIT,
        test_harness::{editor, TestHarness},
    };
    use crossterm::event::Event;
    use ratatui::{
        buffer::Buffer,
        style::{Color, Modifier},
    };
    use std::time::Duration;
    use stoat_action::{Action, ParamKind};

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

    /// A harness rooted in an empty fake repository at `/repo`, so the
    /// palette's Active scope offers the git commands.
    fn repo_harness() -> TestHarness {
        let mut h = Stoat::test();
        h.fake_git().add_repo("/repo");
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h
    }

    /// A harness rooted in `/repo`, with HEAD on `main` at the second of two
    /// commits and `feature` at the first.
    fn branch_harness() -> TestHarness {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "one", &[("a.rs", "1\n")]),
                ("b2c3d4e5", "two", &[("a.rs", "2\n")]),
            ],
        );
        h.fake_git()
            .add_repo("/repo")
            .branch("main", "b2c3d4e5")
            .branch("feature", "a1b2c3d4")
            .set_head_branch("main");
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h
    }

    /// The arg picker's filtered rows as the text they display, sorted so an
    /// assertion pins the row set without pinning the fuzzy ranker's order.
    fn arg_rows(h: &TestHarness) -> Vec<String> {
        let mut rows = listed_arg_rows(h);
        rows.sort();
        rows
    }

    /// The arg picker's filtered rows as the text they display, in the order
    /// they display.
    fn listed_arg_rows(h: &TestHarness) -> Vec<String> {
        let picklist = &arg_picker(h).active_core_ref().picklist;
        picklist
            .filtered
            .iter()
            .map(|i| picklist.base[*i].display().to_string())
            .collect()
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

    /// The action names the palette lists under `scope` for `text`, at the root
    /// and then under the `action` token. Use this for availability
    /// assertions, which are about which actions appear rather than how they
    /// are spelled or which list holds them.
    fn action_names_for_scope(
        text: &str,
        scope: PaletteScope,
        availability: &Availability,
    ) -> Vec<&'static str> {
        let actions = format!("{ACTION_TOKEN} {text}");
        entries_for(text, scope, availability)
            .into_iter()
            .chain(entries_for(&actions, scope, availability))
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
            input_group(text, availability),
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
        assert!(listed.contains(&"open"));
        assert!(!listed.contains(&"open-command-palette"));
        assert_eq!(
            listed.last(),
            Some(&"action"),
            "the action row ends the list"
        );

        let commands = &listed[..listed.len() - 1];
        let listed_with_prio: Vec<(u8, &&'static str)> =
            commands.iter().map(|n| (priority_ord_of(n), n)).collect();
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
        // `write` is Common and `version` is Normal. Alphabetically `version`
        // comes first, so priority alone puts `write` ahead of it.
        assert!(pos_in(&listed, "write") < pos_in(&listed, "version"));
    }

    #[test]
    fn tier_boundary_dominates_priority() {
        // `OpenRun` is Common but matches `run` only as a substring. So it
        // sinks below every prefix-tier match regardless of that match's
        // priority, such as the Normal `RunSubmit`.
        let listed = names_for("action run");
        let open_run = pos_in(&listed, "open-run");
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
        // - `reload` starts with "re" (prefix).
        // - `git review` contains "re" as a non-prefix substring.
        // - `write` has r(1),e(4) as a subsequence, no "re" substring.
        let listed = names_for("re");
        let prefix = pos_in(&listed, "reload");
        let substring = pos_in(&listed, "git review");
        let fuzzy = pos_in(&listed, "write");
        assert!(prefix < substring, "prefix ranks above substring");
        assert!(substring < fuzzy, "substring ranks above fuzzy");
    }

    #[test]
    fn multi_token_query_matches_in_either_order() {
        // `changed-files` contains both `changed` and `files` tokens. Pattern
        // splits on whitespace, so the order of tokens does not change
        // the hit set.
        let forward = names_for("changed files");
        let reverse = names_for("files changed");
        assert!(forward.contains(&"changed-files"));
        assert!(reverse.contains(&"changed-files"));
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
        assert!(pos_in(&listed, "minimap") < pos_in(&listed, "open"));
        assert!(pos_in(&listed, "open") < pos_in(&listed, "write"));
    }

    #[test]
    fn prefix_filter_ranks_first() {
        let listed = names_for("action foc");
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
            Some("write"),
            "`w` is SaveBuffer's alias and must pin to the top",
        );
        assert_eq!(
            names_for("o").first().copied(),
            Some("open"),
            "`o` is OpenFile's alias and must pin to the top",
        );
    }

    /// The full name and the alias of a two-token command each pin it, by its
    /// full name, to the top row.
    #[test]
    fn a_two_token_name_and_its_alias_pin_the_command() {
        assert_eq!(
            ["git log", "git ls"].map(|text| names_for(text).first().copied()),
            [Some("git log"); 2],
        );
    }

    /// A group token and a space list every command of the group, by priority
    /// and then name, and nothing outside it.
    #[test]
    fn a_group_token_lists_only_its_commands() {
        let mut members: Vec<&registry::RegistryEntry> = registry::all()
            .filter(|entry| entry.def.palette_visible() && entry.command_name.starts_with("git "))
            .collect();
        members.sort_by_key(|entry| (entry.def.priority().ord(), entry.command_name.as_str()));
        let members: Vec<&str> = members
            .iter()
            .map(|entry| entry.command_name.as_str())
            .collect();
        assert_eq!((members.len() >= 2, names_for("git ")), (true, members));
    }

    /// The text after a group token narrows the group by each member's
    /// subcommand token, so `i`, a letter of the group token, leaves out
    /// `log`. A member's alias pins the member, the group token ignores case,
    /// and a token with no space lists outside the group. A match position
    /// indexes the whole name, so the `l` of `git log` sits at 4.
    #[test]
    fn the_text_after_a_group_token_narrows_the_group() {
        let narrowed = names_for("git l");
        let outside = names_for("gi");
        let positions = {
            let (mut filtered, mut match_indices, mut selected) = (Vec::new(), Vec::new(), 0);
            let ctx = Availability::default();
            let group = input_group("git l", &ctx);
            refilter(
                "git l",
                group,
                PaletteScope::All,
                &ctx,
                &mut filtered,
                &mut match_indices,
                &mut selected,
            );
            filtered
                .iter()
                .position(|entry| entry.command_name == "git log")
                .map(|at| match_indices[at].clone())
        };
        assert_eq!(
            (
                narrowed.iter().all(|name| name.starts_with("git ")),
                narrowed.contains(&"git log"),
                names_for("git i").contains(&"git log"),
                names_for("git ls").first().copied(),
                names_for("GIT ") == names_for("git "),
                outside.iter().any(|name| !name.starts_with("git ")),
                positions,
            ),
            (
                true,
                true,
                false,
                Some("git log"),
                true,
                true,
                Some(vec![4])
            ),
        );
    }

    /// The palette's header is the Cmd Line. It reads the workspace root,
    /// abbreviated against the home, and the prompt glyph. At its right end it
    /// counts the commands on show and the actions behind the `action` row. No
    /// row names the palette.
    #[test]
    fn the_palette_header_reads_the_cwd_the_prompt_glyph_and_the_count() {
        let mut h = Stoat::test();
        h.stoat.home = Some(PathBuf::from("/home/u"));
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/home/u/code/stoat");
        h.type_text(":");
        let _ = h.snapshot();

        let text = h.rendered_text();
        let palette = h
            .stoat
            .command_palette
            .as_ref()
            .expect("the palette is open");
        let (shown, more) = (palette.filtered.len() - 1, palette.action_offering);
        let cmd_line = text
            .lines()
            .find(|line| line.contains('\u{276f}'))
            .unwrap_or_default();
        let count = format!("{shown} commands \u{b7} action for {more} more\u{2502}");
        assert_eq!(
            (
                cmd_line.contains("\u{2502}~/c/stoat \u{276f}"),
                cmd_line.contains(&count),
                text.contains("Palette"),
            ),
            (true, true, false),
            "the header reads {cmd_line:?}",
        );
    }

    /// The root lists the commands and ends in the `action` row, and it lists
    /// no internal action.
    #[test]
    fn the_root_lists_commands_and_ends_in_the_action_row() {
        let listed = names_for("");
        assert_eq!(
            (
                ["open", "quit", "run"].map(|name| listed.contains(&name)),
                listed.contains(&"focus-left"),
                listed.last(),
            ),
            ([true; 3], false, Some(&"action")),
        );
    }

    /// The word `action` puts the `action` row first, ahead of a command whose
    /// name also matches it, such as `walkthrough-description`. So Enter opens
    /// the list of the internal actions.
    #[test]
    fn the_word_action_puts_the_action_row_first() {
        assert_eq!(names_for("action").first(), Some(&"action"));
    }

    /// The `action` token lists the internal actions, and neither the commands
    /// nor the `action` row.
    #[test]
    fn the_action_token_lists_the_internal_actions() {
        let listed = names_for("action ");
        assert_eq!(
            (
                ["focus-left", "close-pane"].map(|name| listed.contains(&name)),
                ["open", "action"].map(|name| listed.contains(&name)),
            ),
            ([true; 2], [false; 2]),
        );
    }

    /// An internal action resolves under the `action` token, by its kebab or
    /// its PascalCase name, and nowhere else. A hidden action resolves under no
    /// token, and a command resolves at the root.
    #[test]
    fn an_internal_action_resolves_only_under_the_action_token() {
        let ctx = Availability::default();
        let split =
            |text| split_head(text, &ctx).map(|(entry, head, tail)| (entry.def.name(), head, tail));
        assert_eq!(
            [
                "focus-left",
                "action open x",
                "action ",
                "action picker-next",
                "action focus-left",
                "action rename-workspace new name",
                "action RenameWorkspace x",
                "OpenFile a",
            ]
            .map(split),
            [
                None,
                None,
                None,
                None,
                Some(("FocusLeft", "action focus-left", "")),
                Some(("RenameWorkspace", "action rename-workspace", "new name")),
                Some(("RenameWorkspace", "action RenameWorkspace", "x")),
                Some(("OpenFile", "OpenFile", "a")),
            ],
        );
    }

    /// Enter on the `action` row writes its token and a space, which opens the
    /// list of the internal actions.
    #[test]
    fn enter_on_the_action_row_opens_the_action_list() {
        let mut h = Stoat::test();
        h.type_text(":action");
        h.type_keys("enter");

        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        assert_eq!(
            (palette_text(&h), palette.namespace),
            ("action ".to_owned(), Namespace::Actions),
        );
    }

    /// Tab under the `action` token completes an internal action with the
    /// token kept. For an action that takes an argument, Tab also adds the
    /// space that opens it.
    #[test]
    fn tab_completes_an_internal_action_under_the_action_token() {
        let completed = |text: &str| {
            let mut h = Stoat::test();
            h.type_text(text);
            let _ = h.snapshot();
            h.type_keys("tab");
            palette_text(&h)
        };
        assert_eq!(
            [completed(":action rename-w"), completed(":action focus-l")],
            ["action rename-workspace ", "action focus-left"].map(String::from),
        );
    }

    /// History records a command by its alias or its command name, so the
    /// recalled line runs again at the root.
    #[test]
    fn a_command_records_its_alias_or_its_command_name() {
        let mut h = Stoat::test();
        h.type_text(":pwd");
        h.type_keys("enter");

        let head = |name| history_head(registry::lookup(name).expect("a registered action"));
        assert_eq!(
            (
                h.stoat.active_workspace().palette_history.entries(),
                ["SetCwd", "ShowCwd", "FocusLeft"].map(head),
            ),
            (
                &["pwd".to_owned()][..],
                ["cd", "pwd", "action focus-left"].map(String::from),
            ),
        );
    }

    /// A submitted line records the time that the executor's clock reads, so
    /// the palette's History names the line's age.
    #[test]
    fn a_submitted_line_records_the_clock() {
        let mut h = Stoat::test();
        h.type_text(":pwd");
        h.type_keys("enter");

        let now = crate::commit_picker::now_epoch(&h.stoat.executor);
        assert_eq!(
            h.stoat.active_workspace().palette_history.times(),
            [Some(now)]
        );
    }

    /// A saved line whose first word names an internal action and no command
    /// gains the `action` token, and any other line stays as it is.
    #[test]
    fn an_old_history_line_of_an_internal_action_gains_the_action_token() {
        let lines = [
            "FocusLeft",
            "RenameWorkspace foo",
            "cd /x",
            "w",
            "git log main",
            "nonsense",
        ];
        assert_eq!(
            lines.map(|line| migrate_history_line(line.to_owned())),
            [
                "action FocusLeft",
                "action RenameWorkspace foo",
                "cd /x",
                "w",
                "git log main",
                "nonsense",
            ]
            .map(String::from),
        );
    }

    /// At the root the header counts the commands on show and the internal
    /// actions behind the `action` row. Under the token it counts the actions
    /// on show out of those the scope offers.
    #[test]
    fn the_header_counts_the_commands_and_the_actions() {
        let mut h = Stoat::test();
        let mut header = |text: &str| {
            h.type_text(text);
            let _ = h.snapshot();
            let palette = h.stoat.command_palette.as_ref().expect("palette open");
            let counts = (palette.filtered.len(), palette.action_offering);
            let text = h.rendered_text();
            let line = text
                .lines()
                .find(|line| line.contains('\u{276f}'))
                .unwrap_or_default()
                .trim_end_matches('\u{2502}')
                .to_owned();
            (counts, line)
        };
        let ((listed, more), root) = header(":");
        let ((_, actions), namespace) = header("action ");

        assert_eq!(
            (
                root.ends_with(&format!(
                    "{} commands \u{b7} action for {more} more",
                    listed - 1
                )),
                namespace.ends_with(&format!("{actions} of {actions} actions")),
            ),
            (true, true),
            "the header reads {root:?} and then {namespace:?}",
        );
    }

    /// A line wider than the Cmd Line's row breaks after its last space that
    /// fits, and its second row starts at the input's column. The rule and the
    /// argument's usage line follow under the second row, and the palette keeps
    /// the count of the rows for the layouts of the body.
    #[test]
    fn a_long_palette_line_wraps_the_header() {
        let mut h = Stoat::test();
        h.type_text(&format!(":run {} {}", "x".repeat(30), "y".repeat(50)));
        let _ = h.snapshot();

        let buf = h.rendered_buffer();
        let cells = |x: u16, y: u16, len: u16| -> String {
            (x..x + len).map(|x| buf[(x, y)].symbol()).collect()
        };
        let palette = h
            .stoat
            .command_palette
            .as_ref()
            .expect("the palette is open");
        assert_eq!(
            (
                palette.header_rows,
                [
                    cells(5, 3, 35),
                    cells(5, 4, 50),
                    cells(3, 5, 74),
                    cells(4, 6, 13)
                ],
            ),
            (
                2,
                [
                    format!("run {} ", "x".repeat(30)),
                    "y".repeat(50),
                    "\u{2500}".repeat(74),
                    "run <command>".to_owned(),
                ],
            ),
            "the header and the body read {:?}",
            h.rendered_text()
                .lines()
                .skip(3)
                .take(4)
                .collect::<Vec<_>>(),
        );
    }

    /// Paste `text` into the open palette, and answer the palette's header rows
    /// with the frame that first draws the paste and the frame after it.
    fn pasted_frames(h: &mut TestHarness, text: &str) -> (u16, Buffer, Buffer) {
        h.stoat.update(Event::Paste(text.to_owned()));
        let first = h.stoat.render();
        let next = h.stoat.render();
        let palette = h
            .stoat
            .command_palette
            .as_ref()
            .expect("the palette is open");
        (palette.header_rows, first, next)
    }

    /// A paste that wraps the Cmd Line over the argument list lowers the list
    /// in the frame that first draws the wrap, so that frame paints as the
    /// next one does.
    #[test]
    fn a_pasted_argument_lays_its_list_under_the_wrap_at_once() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("a.rs", "x\n"), ("ab.rs", "x\n")]);
        h.type_text(":o a");
        h.settle();

        let (rows, first, next) = pasted_frames(&mut h, &" ".repeat(80));
        assert_eq!((rows, first), (2, next));
    }

    /// A paste that wraps the Cmd Line over the command list lowers the list
    /// in the frame that first draws the wrap, so that frame paints as the
    /// next one does.
    #[test]
    fn a_pasted_query_lays_its_list_under_the_wrap_at_once() {
        let mut h = Stoat::test();
        h.type_text(":");
        h.settle();

        let (rows, first, next) = pasted_frames(&mut h, &format!("{}q", " ".repeat(50)));
        assert_eq!((rows, first), (2, next));
    }

    /// A typed head names its command by the head that `split_head` finds, by
    /// the whole text as a name or an alias, or as the one command whose name
    /// it starts. A prefix with whitespace in it names no command, so `git lo`
    /// names none although only `git log` starts with it. The `action` row
    /// takes part as a command, so `act` names it, and an internal action such
    /// as `focus-left` names nothing at the root. The ghost completes the
    /// command's name, or names it after a separator when the head is no
    /// prefix of it.
    #[test]
    fn a_typed_head_resolves_by_name_alias_or_unique_prefix() {
        let ctx = Availability::default();
        let resolved = |text: &str| {
            let name =
                named_command(text, PaletteScope::All, &ctx).map(|(entry, _)| entry.def.name());
            let shown = resolve_typed(text, PaletteScope::All, &ctx);
            name.zip(shown)
                .map(|(name, shown)| (name, shown.head_chars, shown.ghost))
        };
        let texts = [
            "o",
            "open",
            "o src",
            "wq",
            "git ls main",
            "git st",
            "quit-al",
            "wri",
            "zz",
            "git ",
            "git lo",
            "act",
            "focus-left",
        ];

        assert_eq!(
            texts.map(resolved),
            [
                Some(("OpenFile", 1, "pen".to_owned())),
                Some(("OpenFile", 4, String::new())),
                Some(("OpenFile", 1, "pen".to_owned())),
                Some(("WriteQuit", 2, " \u{b7} write-quit".to_owned())),
                Some(("GitLs", 6, " \u{b7} git log".to_owned())),
                Some(("GitStatus", 6, "atus".to_owned())),
                Some(("QuitAll", 7, "l".to_owned())),
                None,
                None,
                None,
                None,
                Some(("action", 3, "ion".to_owned())),
                None,
            ],
        );
    }

    /// A typed alias draws bold in the command color with the rest of the
    /// command's name as a ghost after it, and the input holds only what the
    /// user typed.
    #[test]
    fn an_alias_draws_bold_with_its_ghost_and_leaves_the_buffer_alone() {
        let mut h = Stoat::test();
        h.type_text(":o");
        let _ = h.snapshot();

        let buf = h.rendered_buffer();
        let (glyph, y) = (0..buf.area.height)
            .find_map(|y| {
                (0..buf.area.width)
                    .find(|&x| buf[(x, y)].symbol() == "\u{276f}")
                    .map(|x| (x, y))
            })
            .expect("the Cmd Line draws its glyph");
        let header: String = (glyph..glyph + 6).map(|x| buf[(x, y)].symbol()).collect();
        let (token, ghost) = (&buf[(glyph + 2, y)], &buf[(glyph + 4, y)]);
        assert_eq!(
            (
                header,
                (token.fg, token.modifier.contains(Modifier::BOLD)),
                ghost.fg,
                palette_text(&h),
            ),
            (
                "\u{276f} open".to_owned(),
                (Color::Rgb(197, 154, 240), true),
                Color::Rgb(105, 84, 129),
                "o".to_owned(),
            ),
        );
    }

    /// Tab completes the selected command past the ghost, which the input never
    /// held.
    #[test]
    fn tab_completes_the_command_past_its_ghost() {
        let mut h = Stoat::test();
        h.type_text(":o");
        let _ = h.snapshot();
        h.type_keys("tab");

        assert_eq!(palette_text(&h), "open ");
    }

    /// Every row of the root list ends in the `built-in` Source Tag, because
    /// every command in the registry ships with stoat.
    #[test]
    fn the_root_list_tags_each_command_built_in() {
        let mut h = Stoat::test();
        h.type_text(":");
        let _ = h.snapshot();

        let (rows, zoom, header_rows) = crate::test_fixture::palette_sizing(&h);
        let list = crate::render::command_palette::palette_filter_layout(
            h.stoat.size(),
            rows,
            zoom,
            header_rows,
        )
        .expect("the palette fits the test terminal")
        .list;
        let tagged: Vec<bool> = h
            .rendered_text()
            .lines()
            .skip(usize::from(list.y))
            .take(usize::from(list.height))
            .map(|line| line.contains("built-in"))
            .collect();
        assert_eq!(tagged, vec![true; usize::from(list.height)]);
    }

    /// A group shows in the Cmd Line's input after the prompt glyph and lists
    /// each command by its subcommand token alone. Narrowed to `l`, the row
    /// paints the `l` of `log` as a match and the `o` beside it as plain text.
    #[test]
    fn a_group_shows_in_the_cmd_line_and_drops_its_token_from_each_row() {
        let mut h = repo_harness();
        // A description starts past the name, alias, and signature columns,
        // so the test finds each row by a description the window shows whole.
        h.resize(160, 40);
        h.type_text(":git ");
        let _ = h.snapshot();
        let text = h.rendered_text();
        let short = registry::lookup_alias("git log")
            .expect("git log is a command")
            .def
            .short_desc();
        // The top row always shows, and a lower row moves down as the group
        // grows, so the test reads the top row.
        let top = h
            .stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .filtered
            .first()
            .copied()
            .expect("the group lists a command");
        let row = text
            .lines()
            .find(|line| line.contains(top.def.short_desc()))
            .unwrap_or_default();
        let in_cmd_line = text.lines().any(|line| line.contains("\u{276f} git"));
        let subcommand = &top.command_name["git ".len()..];
        // The name starts one cell in from the box's left border.
        let shown = (
            row.contains(&format!("\u{2502} {subcommand} ")),
            row.contains(top.command_name.as_str()),
        );

        h.type_text("l");
        let _ = h.snapshot();
        let narrowed = h.rendered_text();
        let (y, line) = narrowed
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(short))
            .expect("the git log row");
        let x = line[..line.find(" log ").expect("the log token") + 1]
            .chars()
            .count();
        let buf = h.rendered_buffer();
        let style = |x: usize| buf[(x as u16, y as u16)].style();
        assert_eq!(
            (in_cmd_line, shown, style(x) == style(x + 1)),
            (true, (true, false), false),
        );
    }

    /// `:git ` lists the group inside a repository and no command outside one,
    /// where the group has no repository to act on. The rebase continue and
    /// abort commands wait for a rebase, so a repository with none leaves them
    /// out, and the merge abort and continue commands wait for a merge the
    /// tally reads.
    #[test]
    fn the_git_group_lists_its_commands_only_inside_a_repository() {
        let listed = |mut h: TestHarness| {
            h.type_text(":git ");
            let palette = h.stoat.command_palette.as_ref().expect("palette open");
            palette
                .filtered
                .iter()
                .map(|entry| entry.command_name.as_str())
                .collect::<Vec<_>>()
        };
        let without = |left_out: &[&str]| -> Vec<&str> {
            names_for("git ")
                .into_iter()
                .filter(|name| !left_out.contains(name))
                .collect()
        };
        let rebase_pair = ["git rebase continue", "git rebase abort"];
        let merge_pair = ["git merge abort", "git merge continue"];
        let mut merging = repo_harness();
        merging.fake_git().add_repo("/repo").merge_in_progress(
            "f00dfeed",
            Some("feature"),
            "Merge branch 'feature'",
        );
        merging.stoat.set_diff_warm_auto(true);
        merging.settle_diff_jobs();

        assert_eq!(
            [
                listed(Stoat::test()),
                listed(repo_harness()),
                listed(merging)
            ],
            [
                Vec::new(),
                without(&[rebase_pair, merge_pair].concat()),
                without(&rebase_pair),
            ],
        );
    }

    /// A two-token head reads its argument after the second token. A head with
    /// no command behind it parses to none, and a one-token head keeps every
    /// later space in its argument.
    #[test]
    fn a_two_token_head_parses_ahead_of_its_first_token() {
        let parsed = |text: &'static str| {
            parse_command(text, &Availability::default())
                .map(|(entry, tail)| (entry.def.name(), tail))
        };
        assert_eq!(
            [
                "git log main",
                "git ls main",
                "git ls ",
                "git",
                "git ",
                "o src/a b"
            ]
            .map(parsed),
            [
                Some(("GitLs", "main")),
                Some(("GitLs", "main")),
                Some(("GitLs", "")),
                None,
                None,
                Some(("OpenFile", "src/a b")),
            ],
        );
    }

    /// A command that takes no argument is whole with no space after it, so
    /// `review done` ends a review where `review feature` names a revision, and
    /// `git rebase continue` never reads as `git rebase` with an argument. One
    /// that takes an argument still waits for the space, so `git log` stays
    /// filter text for the git group.
    #[test]
    fn a_whole_command_with_no_argument_wins_over_a_shorter_head() {
        let split = |text: &'static str| {
            split_head(text, &Availability::default())
                .map(|(entry, head, tail)| (entry.def.name(), head, tail))
        };
        assert_eq!(
            [
                "review done",
                "review feature",
                "git log",
                "git rebase continue",
                "git rebase main",
                "git stash",
                "git stash push wip",
            ]
            .map(split),
            [
                Some(("ReviewDone", "review done", "")),
                Some(("Review", "review", "feature")),
                None,
                Some(("GitRebaseContinue", "git rebase continue", "")),
                Some(("GitRebase", "git rebase", "main")),
                Some(("GitStash", "git stash", "")),
                Some(("GitStashPush", "git stash push", "wip")),
            ],
        );
        assert_eq!(names_for("git rebase c"), ["git rebase continue"]);
    }

    #[test]
    fn substring_filter_after_prefix() {
        let listed = names_for("action pane");
        // ClosePane has "Pane" as a substring but not as a prefix.
        assert!(listed.contains(&"close-pane"));
    }

    #[test]
    fn case_insensitive_filter() {
        assert_eq!(
            names_for("quit"),
            vec![
                "quit",
                "quit-all",
                "force-quit",
                "force-quit-all",
                "force-write-quit",
                "write-quit"
            ]
        );
    }

    #[test]
    fn refilter_clamps_selected_when_results_shrink() {
        let mut filtered = Vec::new();
        let mut match_indices = Vec::new();
        let mut selected = 7;
        refilter(
            "quit",
            None,
            PaletteScope::All,
            &Availability::default(),
            &mut filtered,
            &mut match_indices,
            &mut selected,
        );
        assert_eq!(filtered.len(), 6);
        assert_eq!(selected, 5);
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
            "CloseDiffChanges",
            "DiffChangesNext",
            "DiffChangesExpandUp",
            "DiffChangesExpandDown",
            "DiffChangesExpandAll",
            "DiffChangesCollapse",
        ] {
            assert!(!listed.contains(&name), "{name} unexpectedly visible");
        }
        for name in [
            "Quit",
            "OpenFile",
            "DiffFiles",
            "DiffChanges",
            "OpenCommits",
            "FocusLeft",
        ] {
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
    fn active_scope_lists_the_pair_diff_only_with_a_pair() {
        let lists_it = |ctx: &Availability| {
            action_names_for_scope("", PaletteScope::Active, ctx).contains(&"DiffPair")
        };
        assert_eq!(
            [
                lists_it(&Availability::default()),
                lists_it(&Availability {
                    pair_open: true,
                    ..Availability::default()
                }),
            ],
            [false, true],
        );
    }

    /// The git group acts on the workspace's repository, so the Active scope
    /// lists its commands only inside one, the rebase continue and abort
    /// commands only while a rebase runs there, and the merge abort and
    /// continue commands only while a merge is in progress. The All scope lists
    /// them anywhere.
    #[test]
    fn active_scope_lists_the_git_group_only_in_a_repository() {
        let git_group = |scope, ctx: Availability| {
            let mut names: Vec<&str> = entries_for("", scope, &ctx)
                .into_iter()
                .filter(|entry| entry.command_name.starts_with("git "))
                .map(|entry| entry.def.name())
                .collect();
            names.sort_unstable();
            names
        };
        let group = vec![
            "GitAdd",
            "GitAmend",
            "GitBranch",
            "GitCheckout",
            "GitCommit",
            "GitDiff",
            "GitDiffCached",
            "GitLs",
            "GitMerge",
            "GitMergeAbort",
            "GitMergeContinue",
            "GitRebase",
            "GitRebaseAbort",
            "GitRebaseContinue",
            "GitReview",
            "GitStash",
            "GitStashList",
            "GitStashPop",
            "GitStashPush",
            "GitStatus",
            "ToggleBlame",
        ];
        let without = |left_out: &[&str]| -> Vec<&str> {
            group
                .iter()
                .copied()
                .filter(|name| !left_out.contains(name))
                .collect()
        };
        let rebase_pair = ["GitRebaseAbort", "GitRebaseContinue"];
        let merge_pair = ["GitMergeAbort", "GitMergeContinue"];
        let in_repo = Availability {
            in_repo: true,
            ..Availability::default()
        };
        let rebasing = Availability {
            in_rebase_exec: true,
            ..in_repo
        };
        let merging = Availability {
            in_merge: true,
            ..in_repo
        };
        // A merge the last tally read, with the root since moved out of the
        // repository.
        let stale_merge = Availability {
            in_merge: true,
            ..Availability::default()
        };
        assert_eq!(
            [
                git_group(PaletteScope::Active, Availability::default()),
                git_group(PaletteScope::Active, in_repo),
                git_group(PaletteScope::Active, rebasing),
                git_group(PaletteScope::Active, merging),
                git_group(PaletteScope::Active, stale_merge),
                git_group(PaletteScope::All, Availability::default()),
            ],
            [
                Vec::new(),
                without(&[rebase_pair, merge_pair].concat()),
                without(&merge_pair),
                without(&rebase_pair),
                Vec::new(),
                group.clone(),
            ],
        );
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
    fn active_scope_diff_view_surfaces_move_actions() {
        let ctx = Availability {
            diff_view_open: true,
            ..Availability::default()
        };
        let listed = action_names_for_scope("", PaletteScope::Active, &ctx);
        for name in [
            "JumpToMoveSource",
            "JumpToMoveTarget",
            "QueryMoveRelationships",
        ] {
            assert!(
                listed.contains(&name),
                "{name} missing when the diff view is open"
            );
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
    fn active_scope_diff_changes_open_surfaces_diff_changes_actions() {
        let ctx = Availability {
            diff_changes_open: true,
            ..Availability::default()
        };
        let listed = action_names_for_scope("", PaletteScope::Active, &ctx);
        for name in [
            "CloseDiffChanges",
            "DiffChangesNext",
            "DiffChangesOpen",
            "DiffChangesExpandUp",
            "DiffChangesExpandDown",
            "DiffChangesExpandAll",
            "DiffChangesCollapse",
            "DiffChanges",
        ] {
            assert!(
                listed.contains(&name),
                "{name} missing when diff_changes_open"
            );
        }
        assert!(!listed.contains(&"CommitsNext"));
    }

    #[test]
    fn a_focused_changes_view_reads_as_open() {
        let mut h = Stoat::test();
        let before = Availability::from_stoat(&h.stoat).diff_changes_open;
        h.cover_with_diff_changes(false);
        assert_eq!(
            (before, Availability::from_stoat(&h.stoat).diff_changes_open),
            (false, true)
        );
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
            "JumpToMoveSource",
            "CommitsNext",
            "RunSubmit",
        ] {
            assert!(listed.contains(&name), "{name} missing in All scope");
        }
    }

    /// These take a workdir and a revision, which nobody can type at the
    /// palette, so they stay hidden even in the scope that shows everything
    /// else. The commits view dispatches them directly.
    #[test]
    fn commands_taking_untypable_params_stay_out_of_the_palette() {
        let listed = action_names_for_scope("", PaletteScope::All, &Availability::default());
        for name in ["OpenReviewCommit", "OpenReviewCommitRange"] {
            assert!(!listed.contains(&name), "{name} must not be listed");
        }
    }

    #[test]
    fn every_registered_action_is_available_when_all_flags_set() {
        let ctx = Availability {
            in_rebase_plan: true,
            in_rebase_exec: true,
            in_rebase_reword: true,
            in_conflict: true,
            diff_view_open: true,
            commits_open: true,
            diff_changes_open: true,
            review_walk_open: true,
            walkthrough_open: true,
            run_focused: true,
            pair_open: true,
            in_repo: true,
            in_merge: true,
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
    fn command_palette_ctrl_n_then_enter_dispatches_selection() {
        let mut h = Stoat::test();
        h.type_text(":action focus");
        h.type_keys("ctrl-n enter");
        assert!(h.stoat.command_palette.is_none());
    }

    #[test]
    fn palette_ctrl_up_recalls_and_reexecutes_a_recorded_command() {
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
        h.type_keys("ctrl-up enter");
        assert_eq!(
            h.stoat.active_workspace().git_root,
            sub,
            "Ctrl-Up recalls the recorded cd line and Enter re-runs it"
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
        h.type_keys("ctrl-up enter");
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
        h.type_keys("ctrl-up");
        assert_eq!(
            palette_text(&h),
            "cd /beta",
            "an empty needle recalls the newest"
        );

        h.type_text("x");
        h.stoat.drive_background();
        h.type_keys("ctrl-up");
        assert_eq!(
            palette_text(&h),
            "cd /betax",
            "the edit ends the walk, so Ctrl-Up captures the new needle (which matches nothing)"
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
        h.type_keys("ctrl-p");
        assert_eq!(
            palette_place(&h),
            (0, String::new()),
            "Ctrl-P on the first row recalls no history"
        );
    }

    #[test]
    fn palette_arrows_move_the_list_and_up_on_the_first_row_recalls_history() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["w".to_string()]);

        h.type_text(":");
        h.type_keys("down");
        assert_eq!(
            palette_place(&h),
            (1, String::new()),
            "Down moves the list selection down and recalls no history"
        );
        h.type_keys("up");
        assert_eq!(
            palette_place(&h),
            (0, String::new()),
            "Up moves the list selection up and recalls no history"
        );
        h.type_keys("up");
        assert_eq!(
            palette_place(&h),
            (0, "w".to_string()),
            "Up on the first row recalls the history"
        );
    }

    #[test]
    fn palette_ctrl_arrows_recall_history_not_the_list() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["w".to_string()]);

        h.type_text(":");
        h.type_keys("ctrl-up");
        assert_eq!(
            palette_place(&h),
            (0, "w".to_string()),
            "Ctrl-Up recalls the history and leaves the list selection"
        );
        h.type_keys("ctrl-down");
        assert_eq!(
            palette_place(&h),
            (0, String::new()),
            "Ctrl-Down past the newest restores the typed text and leaves the list selection"
        );
    }

    #[test]
    fn a_fuzzy_list_submit_records_the_entry_head() {
        let mut h = Stoat::test();
        h.type_text(":action focus");
        h.type_keys("ctrl-n enter");

        let history = h
            .stoat
            .active_workspace()
            .palette_history
            .entries()
            .to_vec();
        assert_eq!(history.len(), 1, "the submitted entry is recorded");
        assert!(
            history[0].starts_with("action focus-")
                && split_head(&history[0], &Availability::default()).is_some(),
            "the recorded head names the action under its token and re-resolves, got {:?}",
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

    /// The palette's list selection and input text.
    ///
    /// One assertion on the pair shows if a key moved the list or recalled history.
    fn palette_place(h: &TestHarness) -> (usize, String) {
        (
            h.stoat
                .command_palette
                .as_ref()
                .expect("palette open")
                .selected,
            palette_text(h),
        )
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

    /// A lone parameter takes the whole tail, so a path with a space filters as
    /// one value and Tab completes it in one piece.
    #[test]
    fn a_lone_parameter_takes_its_tail_spaces_and_all() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("my notes.md", ""), ("other.md", "")]);

        h.type_text(":o my no");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        let typed = palette_arg_tail(&h);

        h.type_keys("tab");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        assert_eq!(
            (typed.as_deref(), palette_text(&h)),
            (Some("my no"), "o my notes.md".to_string()),
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

        h.type_text(":set-cw");
        let _ = h.snapshot();
        h.type_keys("tab");
        let _ = h.snapshot();
        assert_eq!(
            palette_text(&h),
            "set-cwd ",
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

    /// Tab in the argument of a two-token command completes the highlighted
    /// branch and keeps both tokens of its head.
    #[test]
    fn tab_in_a_two_token_command_keeps_its_head() {
        let mut h = branch_harness();
        h.type_text(":git ls fe");
        let _ = h.snapshot();
        h.type_keys("tab");
        let _ = h.snapshot();
        assert_eq!(palette_text(&h), "git ls feature");
    }

    /// `:git br` opens the palette holding `git checkout `, with the branch
    /// HEAD is on listed first.
    #[test]
    fn git_branch_opens_the_checkout_over_the_branches() {
        let mut h = branch_harness();
        h.type_text(":git br");
        h.type_keys("enter");
        let _ = h.snapshot();
        assert_eq!(
            (palette_text(&h), listed_arg_rows(&h)),
            (
                "git checkout ".to_string(),
                vec!["main".to_string(), "feature".to_string()]
            ),
        );
    }

    /// An argument list is a whole workspace walk, so its ranking is handed
    /// back for a worker to run rather than done inside the keystroke.
    ///
    /// That it went to a worker is what this states. Whether it did so
    /// concurrently is not observable here, the test scheduler running blocking
    /// work inline, so the structure is the thing to pin.
    #[test]
    fn an_argument_list_ranks_off_the_input_thread() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/alpha.rs", ""), ("wsdir/beta.rs", "")]);

        h.type_text(":OpenFile wsdir/al");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        let fs_host = h.stoat.fs_host.clone();
        let language_registry = h.stoat.language_registry.clone();
        let active_idx = h.stoat.active_workspace;
        let ws = &mut h.stoat.workspaces[active_idx];
        let palette = h.stoat.command_palette.as_mut().expect("palette open");

        // Invalidated so the tail is asked afresh, the sync driven by the
        // typing above having already answered this one.
        palette
            .arg_picker
            .as_mut()
            .expect("arg picker")
            .core
            .invalidate();
        assert_eq!(
            palette
                .sync_arg_picker("wsdir/al", ws, &*fs_host, &language_registry)
                .iter()
                .map(|scan| scan.target)
                .collect::<Vec<_>>(),
            [ScanTarget::List],
            "the ranking is handed back for a worker to run",
        );
    }

    /// How many times the fake fs listed `root`.
    ///
    /// Every walk of the workspace starts with that listing. A directory
    /// preview lists only the selected directory, which lies below the root,
    /// so this counts walks and nothing else.
    fn root_listings(h: &TestHarness, root: &Path) -> usize {
        h.fake_fs()
            .ops()
            .iter()
            .filter(|op| matches!(op, FakeFsOp::ListDir { path } if path == root))
            .count()
    }

    /// Open and close the file finder, which files the paths it walked as the
    /// cached list.
    fn file_the_finder_cache(h: &mut TestHarness) {
        h.type_keys("space p");
        h.type_keys("escape");
        h.stoat.drain_index_updates();
    }

    #[test]
    fn a_file_argument_lists_the_finders_cached_paths_without_a_walk() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(&mut h, &[("wsdir/alpha.rs", ""), ("wsdir/beta.rs", "")]);
        file_the_finder_cache(&mut h);
        let walks = root_listings(&h, &root);

        h.type_text(":OpenFile wsdir/al");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        assert_eq!(
            root_listings(&h, &root),
            walks,
            "the argument walked nothing"
        );
        assert_eq!(
            arg_rows(&h),
            [root.join("wsdir/alpha.rs").display().to_string()],
            "and the cached list answers the tail",
        );
    }

    #[test]
    fn a_directory_argument_derives_from_the_cached_paths() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(
            &mut h,
            &[("top.rs", ""), ("src/main.rs", ""), ("docs/readme.md", "")],
        );
        file_the_finder_cache(&mut h);
        let walks = root_listings(&h, &root);

        h.type_text(":cd ");
        let _ = h.snapshot();

        assert_eq!(
            root_listings(&h, &root),
            walks,
            "the argument walked nothing"
        );
        assert_eq!(
            arg_rows(&h),
            [
                root.join("docs").display().to_string(),
                root.join("src").display().to_string(),
            ],
            "src and docs; a root-level file contributes no directory",
        );
    }

    #[test]
    fn a_stale_cache_still_walks_for_an_argument() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(&mut h, &[("wsdir/alpha.rs", ""), ("wsdir/beta.rs", "")]);
        file_the_finder_cache(&mut h);
        let walks = root_listings(&h, &root);
        h.stoat.finder_path_epoch += 1;

        h.type_text(":OpenFile wsdir/al");
        let _ = h.snapshot();

        assert_eq!(
            root_listings(&h, &root),
            walks + 1,
            "a retired cache leaves the argument to walk the tree",
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

    /// Press Tab and return the resulting argument tail.
    fn tab_tail(h: &mut TestHarness) -> Option<String> {
        h.type_keys("tab");
        let _ = h.snapshot();
        palette_arg_tail(h)
    }

    #[test]
    fn repeated_tab_cycles_values_and_wraps() {
        let mut h = Stoat::test();
        h.type_text(":auto-reload ");
        let _ = h.snapshot();

        let cycled: Vec<String> = (0..4).filter_map(|_| tab_tail(&mut h)).collect();
        assert_eq!(cycled, ["follow", "off", "on", "follow"]);
    }

    #[test]
    fn repeated_tab_cycles_only_matched_values() {
        let mut h = Stoat::test();
        h.type_text(":auto-reload f");
        let _ = h.snapshot();

        let cycled: Vec<String> = (0..3).filter_map(|_| tab_tail(&mut h)).collect();
        assert_eq!(
            cycled,
            ["follow", "off", "follow"],
            "\"on\" never matched f"
        );
    }

    #[test]
    fn editing_between_tabs_restarts_the_cycle() {
        let mut h = Stoat::test();
        h.type_text(":auto-reload ");
        let _ = h.snapshot();
        assert_eq!(tab_tail(&mut h).as_deref(), Some("follow"));

        h.type_keys("backspace");
        let _ = h.snapshot();
        assert_eq!(
            tab_tail(&mut h).as_deref(),
            Some("follow"),
            "an edited tail completes afresh rather than advancing to \"off\""
        );
    }

    #[test]
    fn repeated_tab_cycles_file_candidates() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/alpha.rs", ""), ("wsdir/beta.rs", "")]);
        h.type_text(":OpenFile wsdir/");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        let first = tab_tail(&mut h);
        let second = tab_tail(&mut h);
        assert_eq!(
            (first.as_deref(), second.as_deref()),
            (Some("wsdir/alpha.rs"), Some("wsdir/beta.rs"))
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
        for block in cfg.expect("parsed config").themes {
            h.stoat.theme_pool.push_parsed(block);
        }

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
        for block in cfg.expect("parsed config").themes {
            h.stoat.theme_pool.push_parsed(block);
        }

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

    /// A harness whose workspace stores two walkthroughs, neither playing.
    fn harness_with_walkthroughs() -> TestHarness {
        let mut h = Stoat::test();
        let root = PathBuf::from("/repo");
        h.stoat.active_workspace_mut().git_root = root.clone();

        for slug in ["startup", "rendering"] {
            let walkthrough =
                crate::walkthrough::Walkthrough::new(slug.to_owned(), slug.to_owned(), None);
            h.fake_fs().insert_file(
                root.join(".stoat/walkthroughs")
                    .join(format!("{slug}.json")),
                serde_json::to_string(&walkthrough).expect("serialize"),
            );
        }
        h
    }

    #[test]
    fn walkthrough_arg_picker_lists_stored_slugs() {
        let mut h = harness_with_walkthroughs();

        h.type_text(":walkthrough ");
        h.snapshot();
        assert_eq!(
            arg_rows(&h),
            ["rendering", "startup"],
            "the picker offers what the workspace stores, so no slug is recalled",
        );
    }

    #[test]
    fn tab_completes_a_walkthrough_slug() {
        let mut h = harness_with_walkthroughs();

        h.type_text(":walkthrough start");
        h.snapshot();
        h.type_keys("tab");
        h.snapshot();

        assert_eq!(palette_arg_tail(&h).as_deref(), Some("startup"));
    }

    /// A walkthrough with one stop, enough for a run to exist.
    fn one_stop_walkthrough() -> crate::walkthrough::Walkthrough {
        use crate::walkthrough::{Location, Point, Range};

        let mut walkthrough =
            crate::walkthrough::Walkthrough::new("tour".to_owned(), "Tour".to_owned(), None);
        walkthrough
            .add_stop(
                None,
                String::new(),
                Location {
                    path: PathBuf::from("a.rs"),
                    range: Range {
                        start: Point { line: 1, col: 1 },
                        end: Point { line: 1, col: 1 },
                    },
                    snippet: "x".to_owned(),
                },
                None,
                None,
            )
            .expect("append");
        walkthrough
    }

    /// A harness with a walkthrough playing and no review walk.
    fn playing_harness() -> TestHarness {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().walkthrough =
            crate::walkthrough::run::WalkthroughRun::new(one_stop_walkthrough(), 0);
        h
    }

    /// A harness sitting mid-walk over a two-commit `/repo`.
    fn walking_harness() -> TestHarness {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "one", &[("a.rs", "1\n")]),
                ("b2c3d4e5", "two", &[("a.rs", "2\n")]),
            ],
        );
        h.fake_git()
            .add_repo("/repo")
            .branch("main", "b2c3d4e5")
            .set_head_branch("main");
        h.stoat.active_workspace_mut().git_root = "/repo".into();

        h.type_text(":git review main");
        h.type_keys("enter");
        h.settle();
        h.type_keys("down");
        h.type_keys("enter");
        h.settle();
        assert!(
            h.stoat.active_workspace().review_walk.is_some(),
            "the walk must be running for the conditional tokens to resolve"
        );
        h
    }

    #[test]
    fn conditional_tokens_dispatch_the_walk_actions() {
        for (token, action) in [
            ("next", "ReviewNextCommit"),
            ("prev", "ReviewPrevCommit"),
            ("done", "ReviewDone"),
        ] {
            let mut h = walking_harness();
            assert_eq!(
                palette_dispatch_name(&mut h, &format!(":{token}")),
                Some(action),
                "{token} resolves to {action} during a walk"
            );
        }
    }

    /// The walkthrough is the second claimant on these tokens, so it takes them
    /// only where no review walk does.
    #[test]
    fn conditional_tokens_reach_the_walkthrough_without_a_review_walk() {
        for (token, action) in [
            ("next", "WalkthroughNext"),
            ("prev", "WalkthroughPrev"),
            ("done", "WalkthroughDone"),
        ] {
            let mut h = playing_harness();
            assert_eq!(
                palette_dispatch_name(&mut h, &format!(":{token}")),
                Some(action),
                "{token} resolves to {action} while a walkthrough plays"
            );
        }
    }

    /// The review walk claimed the tokens first, so it keeps them when both
    /// contexts are live rather than the two fighting over the name.
    #[test]
    fn a_review_walk_keeps_the_tokens_from_a_walkthrough() {
        let mut h = walking_harness();
        h.stoat.active_workspace_mut().walkthrough =
            crate::walkthrough::run::WalkthroughRun::new(one_stop_walkthrough(), 0);

        assert_eq!(
            palette_dispatch_name(&mut h, ":next"),
            Some("ReviewNextCommit"),
        );
    }

    /// Outside a walk the tokens are ordinary filter text, so Enter takes
    /// whatever the fuzzy list highlights. What must not happen is a walk
    /// action running with no walk to run it against.
    #[test]
    fn conditional_tokens_reach_no_walk_action_outside_their_context() {
        for token in ["next", "prev", "done"] {
            let mut h = Stoat::test();
            let dispatched = palette_dispatch_name(&mut h, &format!(":{token}"));
            assert!(
                !matches!(
                    dispatched,
                    Some("ReviewNextCommit" | "ReviewPrevCommit" | "ReviewDone")
                ),
                "{token} dispatched {dispatched:?} without a walk"
            );
        }
    }

    #[test]
    fn the_active_scope_lists_walk_actions_only_during_a_walk() {
        let walking = Availability {
            review_walk_open: true,
            ..Availability::default()
        };
        let listed = action_names_for_scope("review", PaletteScope::Active, &walking);
        assert!(listed.contains(&"ReviewNextCommit"));

        let idle = action_names_for_scope("review", PaletteScope::Active, &Availability::default());
        assert!(
            !idle.contains(&"ReviewNextCommit"),
            "outside a walk the Active scope hides it"
        );
    }

    /// History stores the resolved command, not the token, so a recalled line
    /// replays as that command whatever the context is at recall time.
    #[test]
    fn a_conditional_token_is_recorded_as_its_command() {
        let mut h = walking_harness();
        h.type_text(":next");
        let mut palette = h.stoat.command_palette.take().expect("palette open");
        let ws = h.stoat.active_workspace_mut();
        let PaletteOutcome::Dispatch(_, _, line) = palette.handle_submit(ws) else {
            panic!("next must dispatch during a walk");
        };
        assert_eq!(line, "review-next-commit");
        assert_eq!(
            registry::lookup_alias(&line).map(|e| e.def.name()),
            Some("ReviewNextCommit"),
            "the recorded line re-resolves without needing the walk context"
        );
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
    fn palette_tab_alias_dispatches_goto_tab_with_its_number() {
        let mut h = Stoat::test();
        h.type_text(":tab 2");
        let mut palette = h.stoat.command_palette.take().expect("palette open");
        let ws = h.stoat.active_workspace_mut();
        match palette.handle_submit(ws) {
            PaletteOutcome::Dispatch(entry, args, _) => {
                assert_eq!(entry.def.name(), "GotoTab");
                assert_eq!(args, vec![ParamValue::Number(2.0)]);
            },
            _ => panic!("`tab 2` should dispatch GotoTab with the index"),
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
        assert_eq!(palette.input.text(ws), "open ");
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
        h.type_text(":action rename-workspace ");
        h.assert_snapshot("command_palette_free_arg");
    }

    #[test]
    fn free_arg_submit_dispatches() {
        let mut h = Stoat::test();
        assert_eq!(
            palette_dispatch_name(&mut h, ":action rename-workspace newname"),
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
    fn palette_reload_bang_routes_to_force_reload_buffer() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/palette-reload");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"disk\n");
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

        h.type_text(":reload!");
        h.type_keys("enter");

        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        let guard = buffer.read().expect("poisoned");
        assert!(!guard.dirty, ":reload! discards the buffer's unsaved edits");
        assert_eq!(
            guard.snapshot.visible_text.to_string(),
            "disk\n",
            ":reload! reverts the buffer to the on-disk content",
        );
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
            h.stoat.command_palette.as_ref().unwrap().scope,
            PaletteScope::Active
        );
        h.type_keys("backtab");
        assert_eq!(
            h.stoat.command_palette.as_ref().unwrap().scope,
            PaletteScope::All
        );
        h.type_keys("backtab");
        assert_eq!(
            h.stoat.command_palette.as_ref().unwrap().scope,
            PaletteScope::Active
        );
    }

    /// Asserts against the painted frame rather than the filtered entries, so
    /// it covers the renderer reading the same string the matcher ranked on.
    /// The doc region under the list names the action the command runs, so the
    /// test reads the row line, which holds the command's short description.
    #[test]
    fn the_list_shows_kebab_command_names() {
        let mut h = TestHarness::with_size(160, 50);
        h.type_text(":auto");
        let _ = h.snapshot();

        let desc = registry::lookup_alias("auto-reload")
            .expect("auto-reload is a command")
            .def
            .short_desc();
        let text = h.rendered_text();
        let row = text
            .lines()
            .find(|line| line.contains(desc))
            .unwrap_or_default();
        assert_eq!(
            (row.contains("auto-reload"), row.contains("AutoReload")),
            (true, false),
            "the row reads as a user command, got:\n{text}",
        );
    }

    /// The doc region ends in the action the selected command runs and the key
    /// that reaches the action from the state the palette opened in. The
    /// description of `files` wraps to three lines at this width, so the action
    /// line is the region's last row.
    #[test]
    fn the_doc_island_names_the_action_and_its_keys() {
        let mut h = TestHarness::with_size(160, 50);
        h.type_text(":files");
        let _ = h.snapshot();

        let text = h.rendered_text();
        assert!(
            text.lines()
                .any(|line| line.contains("runs action OpenFileFinder \u{b7} bound to Spc p")),
            "the doc names the action and its key, got:\n{text}",
        );
    }

    /// A row of the `action` list reads the PascalCase name, the kebab name,
    /// and the keys bound to the action. The query's letters mark both names
    /// in `ui.list.match`.
    #[test]
    fn the_action_list_shows_both_names_and_the_bound_keys() {
        let mut h = TestHarness::with_size(160, 50);
        h.type_text(":action undo");
        let _ = h.snapshot();

        let buf = h.rendered_buffer();
        let cells = |x: u16, y: u16, len: u16| -> String {
            (x..x + len).map(|x| buf[(x, y)].symbol()).collect()
        };
        let (x, y) = (0..buf.area.height)
            .find_map(|y| {
                (0..buf.area.width - 4)
                    .find(|&x| cells(x, y, 4) == "Undo")
                    .map(|x| (x, y))
            })
            .expect("a list row names Undo");
        let kebab = (x + 4..buf.area.width - 4)
            .find(|&kebab| cells(kebab, y, 4) == "undo")
            .expect("the row names undo");
        let words: Vec<String> = cells(x, y, buf.area.width - x)
            .split_whitespace()
            .take(3)
            .map(String::from)
            .collect();
        let marks: Vec<Option<Color>> = (x..x + 4)
            .chain(kebab..kebab + 4)
            .map(|x| Some(buf[(x, y)].fg))
            .collect();

        assert_eq!(
            (words, marks),
            (
                ["Undo", "undo", "u"].map(String::from).to_vec(),
                vec![h.stoat.theme.list_match().fg; 8],
            ),
        );
    }

    /// `C-d` scrolls the doc island down three lines, past the usage line, and
    /// `C-u` scrolls it back to the doc's first line.
    #[test]
    fn ctrl_d_scrolls_the_doc_island_and_ctrl_u_scrolls_it_back() {
        let mut h = TestHarness::with_size(160, 50);
        h.type_text(":diff-files");
        let _ = h.snapshot();
        let first = h.rendered_text().contains("diff-files [rev]");

        h.type_keys("ctrl-d");
        let _ = h.snapshot();
        let scrolled = (
            doc_scroll(&h),
            h.rendered_text().contains("diff-files [rev]"),
        );

        h.type_keys("ctrl-u");
        assert_eq!((first, scrolled, doc_scroll(&h)), (true, (3, false), 0));
    }

    /// A doc scrolled one line shows its usage line on top with no Source Tag
    /// after it. The tag ends the name line, which the scroll moved off. The
    /// `built-in` tag of the list's one row stays.
    #[test]
    fn a_scrolled_doc_shows_no_source_tag() {
        let mut h = TestHarness::with_size(160, 50);
        h.type_text(":diff-files");
        let _ = h.snapshot();
        let tags = h.rendered_text().matches("built-in").count();

        h.stoat
            .command_palette
            .as_mut()
            .expect("palette open")
            .doc_scroll = 1;
        let _ = h.snapshot();
        let text = h.rendered_text();
        assert_eq!(
            (
                tags,
                text.contains("diff-files [rev]"),
                text.matches("built-in").count()
            ),
            (2, true, 1),
        );
    }

    /// The doc island holds its scroll where the doc's last line, the action
    /// that `diff-files` runs, stands on the island's last row.
    #[test]
    fn the_doc_scroll_stops_at_the_last_line() {
        let mut h = TestHarness::with_size(160, 50);
        h.type_text(":diff-files");
        for _ in 0..10 {
            h.type_keys("ctrl-d");
            let _ = h.snapshot();
        }

        let (rows, zoom, header_rows) = crate::test_fixture::palette_sizing(&h);
        let doc = crate::render::command_palette::palette_filter_layout(
            h.stoat.size(),
            rows,
            zoom,
            header_rows,
        )
        .expect("the palette fits the test terminal")
        .doc;
        let text = h.rendered_text();
        let last_row = text
            .lines()
            .nth(usize::from(doc.bottom() - 1))
            .unwrap_or_default();
        assert!(
            last_row.contains("runs action DiffFiles"),
            "the island's last row holds the doc's last line, got:\n{text}",
        );
    }

    /// A new selection starts the doc island at the new entry's first line. A
    /// step that the list's top stops keeps the doc where it is. The
    /// query `diff-f` lists `diff-files` and then `diff-forward`, so a step
    /// down moves the selection off the scrolled doc.
    ///
    /// The steps dispatch with no frame after them, because a paint holds the
    /// scroll inside the short doc of `diff-forward` and hides a missed reset.
    #[test]
    fn a_new_selection_starts_its_doc_at_the_top() {
        let mut h = TestHarness::with_size(160, 50);
        h.type_text(":diff-f");
        h.type_keys("ctrl-d");
        let _ = h.snapshot();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::PickerPrev);
        let held = doc_scroll(&h);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::PickerNext);
        assert_eq!((held, doc_scroll(&h)), (3, 0));
    }

    /// A new filter starts the doc island at its first line, although the
    /// same entry stays selected.
    #[test]
    fn a_new_filter_starts_the_doc_at_the_top() {
        let mut h = TestHarness::with_size(160, 50);
        h.type_text(":diff-files");
        h.type_keys("ctrl-d");
        let _ = h.snapshot();
        let scrolled = doc_scroll(&h);

        h.type_keys("backspace");
        let _ = h.snapshot();
        assert_eq!((scrolled, doc_scroll(&h)), (3, 0));
    }

    /// The root's key line names its run, completion, history, and scope keys.
    /// Over an empty input the history hint names the last command, and the
    /// scope hint names the scope that `S-Tab` reaches next. The note at its
    /// right end names the two namespaces, with `:action` in the key color.
    #[test]
    fn the_root_key_line_names_its_keys_the_next_scope_and_the_namespaces() {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["w".to_string()]);
        h.type_text(":");
        let _ = h.snapshot();

        let (text, colors) = key_line(&h);
        let line = text.trim_end();
        let action = line.chars().count() - " = internals".len() - ":action".len();
        let key = h.stoat.theme.get(crate::theme::scope::UI_KEY_LABEL).fg;
        assert_eq!(
            (
                line.contains("Enter run  Tab complete  \u{2191} last command  S-Tab all"),
                line.ends_with("root = commands \u{b7} :action = internals"),
                colors[action..action + 7]
                    .iter()
                    .map(|&color| Some(color))
                    .collect::<Vec<_>>(),
            ),
            (true, true, vec![key; 7]),
            "the key line reads {text:?}",
        );
    }

    /// After `S-Tab` the scope hint names `applicable`, the scope that the key
    /// reaches next from `all`. With no history line, the history hint drops
    /// out.
    #[test]
    fn the_scope_hint_names_the_scope_that_shift_tab_reaches_next() {
        let mut h = TestHarness::with_size(160, 50);
        h.type_text(":");
        h.type_keys("backtab");
        let _ = h.snapshot();

        let (text, _) = key_line(&h);
        assert!(
            text.contains("Enter run  Tab complete  S-Tab applicable"),
            "the key line reads {text:?}",
        );
    }

    /// The `action` list's key line names its run, completion, doc scroll,
    /// and back keys, with no note.
    #[test]
    fn the_action_list_key_line_names_run_complete_doc_and_back() {
        let mut h = TestHarness::with_size(160, 50);
        h.type_text(":action ");
        let _ = h.snapshot();

        assert_eq!(
            key_line(&h).0.trim(),
            "Enter run  Tab complete  C-d doc  Esc back"
        );
    }

    /// A key line too narrow for the root's note keeps its hints and leaves
    /// the note out. Over typed text the history hint names `history`.
    #[test]
    fn a_narrow_palette_keeps_its_hints_and_drops_the_note() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["pwd".to_string()]);
        h.type_text(":w");
        let _ = h.snapshot();

        assert_eq!(
            key_line(&h).0.trim(),
            "Enter run  Tab complete  \u{2191} history  S-Tab all"
        );
    }

    /// `Esc` under the `action` token returns to the root list. The palette
    /// stays open with an empty input, and the root ends in the `action` row.
    /// A second `Esc` closes the palette.
    ///
    /// The first `Esc` dispatches with no frame after it. An `Enter` right
    /// after the `Esc` reads the list, so the list follows the cleared input
    /// at once.
    #[test]
    fn escape_in_the_action_list_returns_to_the_root_list() {
        let mut h = Stoat::test();
        h.type_text(":action foc");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CancelPromptInput);
        let root = {
            let palette = h
                .stoat
                .command_palette
                .as_ref()
                .expect("the palette stays open");
            let last = palette
                .filtered
                .last()
                .map(|&entry| entry.command_name.as_str());
            (palette_text(&h), palette.namespace, last)
        };

        h.type_keys("escape");
        let closed = h.stoat.command_palette.is_none();
        let mode = h.snapshot().mode.clone();
        assert_eq!(
            (root, closed, mode.as_str()),
            (
                (String::new(), Namespace::Commands, Some(ACTION_TOKEN)),
                true,
                "normal"
            ),
        );
    }

    /// Up on the first row recalls the newest line and lists History, newest
    /// first, under the chip `history 1 / 2`.
    #[test]
    fn up_on_the_first_row_recalls_the_newest_line_and_lists_history() {
        let mut h = history_harness();
        h.type_text(":");
        h.type_keys("up");
        let _ = h.snapshot();

        assert_eq!(
            (
                palette_text(&h),
                [history_list_row(&h, 0), history_list_row(&h, 1)],
                h.rendered_text().contains("history 1 / 2"),
            ),
            (
                "cd /beta".to_owned(),
                [
                    "\u{21ba}  cd /beta".to_owned(),
                    "\u{21ba}  cd /alpha".to_owned()
                ],
                true,
            ),
        );
    }

    /// Up and Down walk History older and newer. Down past the newest line
    /// ends the walk, restores the typed text, and returns the list.
    #[test]
    fn up_and_down_walk_history_and_down_past_the_newest_returns_the_list() {
        let mut h = history_harness();
        h.type_text(":");
        h.type_keys("up up");
        let _ = h.snapshot();
        let oldest = (
            palette_text(&h),
            h.rendered_text().contains("history 2 / 2"),
        );
        h.type_keys("down");
        let newer = palette_text(&h);
        h.type_keys("down");
        let _ = h.snapshot();

        let last = h
            .stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .filtered
            .last()
            .map(|&entry| entry.command_name.as_str());
        assert_eq!(
            (oldest, newer, palette_text(&h), walking(&h), last),
            (
                ("cd /alpha".to_owned(), true),
                "cd /beta".to_owned(),
                String::new(),
                false,
                Some(ACTION_TOKEN),
            ),
        );
    }

    /// An edit of the recalled line ends the walk, and the palette shows the
    /// list for the edited line in place of History.
    #[test]
    fn typing_in_history_returns_the_list_for_the_edited_line() {
        let mut h = history_harness();
        h.type_text(":");
        h.type_keys("up");
        h.type_text("x");
        let _ = h.snapshot();

        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        assert_eq!(
            (
                palette_text(&h),
                walking(&h),
                palette.history_rows.is_empty()
            ),
            ("cd /betax".to_owned(), false, true),
        );
    }

    /// `Esc` in History ends the walk, restores the typed text, and keeps the
    /// palette open. A second `Esc` closes it.
    #[test]
    fn escape_in_history_restores_the_typed_text_and_keeps_the_palette_open() {
        let mut h = history_harness();
        h.type_text(":cd");
        h.type_keys("up escape");
        let restored = (palette_text(&h), walking(&h));
        h.type_keys("escape");

        assert_eq!(
            (restored, h.stoat.command_palette.is_none()),
            (("cd".to_owned(), false), true),
        );
    }

    /// `C-Up` from a lower row starts the walk, which shows History as Up's
    /// walk does.
    #[test]
    fn ctrl_up_from_a_lower_row_shows_history() {
        let mut h = history_harness();
        h.type_text(":");
        h.type_keys("down ctrl-up");
        let _ = h.snapshot();

        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        assert_eq!(
            (palette_text(&h), palette.history_rows.len()),
            ("cd /beta".to_owned(), 2),
        );
    }

    /// Tab in History completes nothing, so the recalled line and the walk
    /// stay.
    #[test]
    fn tab_in_history_leaves_the_recalled_line() {
        let mut h = history_harness();
        h.type_text(":");
        h.type_keys("up tab");
        assert_eq!(
            (palette_text(&h), walking(&h)),
            ("cd /beta".to_owned(), true)
        );
    }

    /// Tab, `S-Tab`, the page keys, and `C-d` change nothing in History. The
    /// recalled line is the start of the name `pwd`, so Tab has a name to
    /// complete. The line names no argument, so `C-d` reaches the doc scroll
    /// of the command list.
    #[test]
    fn tab_scope_page_and_doc_keys_do_nothing_in_history() {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["pw".to_string()]);
        h.type_text(":");
        h.type_keys("up");
        let scope = h
            .stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .scope;
        h.type_keys("tab backtab pagedown pageup ctrl-d");

        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        assert_eq!(
            (
                palette_text(&h),
                walking(&h),
                palette.scope,
                palette.doc_scroll
            ),
            ("pw".to_owned(), true, scope, 0),
        );
    }

    /// History's key line names its walk, run, edit, and back keys. Over a
    /// needle that holds a word, the chip and the back hint name the needle,
    /// and the walk hint names the matches.
    #[test]
    fn history_names_its_keys_and_its_needle() {
        let mut h = history_harness();
        h.type_text(":");
        h.type_keys("up");
        let _ = h.snapshot();
        let bare = key_line(&h).0.trim().to_owned();
        h.type_keys("escape");
        h.type_text("cd");
        h.type_keys("up");
        let _ = h.snapshot();

        assert_eq!(
            (
                bare,
                key_line(&h).0.trim().to_owned(),
                h.rendered_text().contains("history \u{b7} cd \u{b7} 1 / 2"),
            ),
            (
                "\u{2191}\u{2193} older / newer  Enter run  type edit it  Esc back to commands"
                    .to_owned(),
                "\u{2191}\u{2193} older / newer match  Enter run  Esc back to \"cd\"".to_owned(),
                true,
            ),
        );
    }

    /// A walk out of an argument list drops its picker, so the box keeps the
    /// rows of the command list for every recalled line. `Esc` restores the
    /// typed argument and installs its picker again.
    #[test]
    fn a_walk_out_of_an_argument_list_keeps_the_box_of_the_command_list() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("a.rs", "a\n"), ("b.rs", "b\n")]);
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["zoo x".to_string(), "o b.rs".to_string()]);
        let palette = |h: &TestHarness| -> (String, Option<bool>, u16) {
            let palette = h.stoat.command_palette.as_ref().expect("palette open");
            (
                palette_text(h),
                palette.arg_source().map(|_| palette.arg_picker.is_some()),
                palette.list_rows_hint(),
            )
        };

        h.type_text(":");
        let commands = palette(&h).2;
        h.type_text("o ");
        h.settle();
        let files = palette(&h).2;
        h.type_keys("up");
        h.settle();
        let newest = palette(&h);
        h.type_keys("up");
        h.settle();
        let oldest = palette(&h);
        h.type_keys("escape");
        h.settle();

        assert_eq!(
            (files, newest, oldest, palette(&h)),
            (
                2,
                ("o b.rs".to_owned(), Some(false), commands),
                ("zoo x".to_owned(), None, commands),
                ("o ".to_owned(), Some(true), 2),
            ),
        );
    }

    /// Enter in History runs the recalled line as typed. The argument list's
    /// selected row stands in for no argument, so the line opens `a/b.rs`
    /// although `a.rs` heads the list. Up dispatches with no frame after it,
    /// so the argument picker is still installed when Enter runs.
    #[test]
    fn enter_in_history_runs_the_recalled_line_as_typed() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("a.rs", "a\n"), ("a/b.rs", "b\n")]);
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["o a/b.rs".to_string()]);
        h.type_text(":o a");
        h.settle();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::PaletteUp);

        let mut palette = h.stoat.command_palette.take().expect("palette open");
        let installed = palette.arg_picker.is_some();
        let dispatched = match palette.handle_submit(h.stoat.active_workspace_mut()) {
            PaletteOutcome::Dispatch(entry, values, _) => Some((entry.def.name(), values)),
            _ => None,
        };
        assert_eq!(
            (installed, dispatched),
            (
                true,
                Some(("OpenFile", vec![ParamValue::String("a/b.rs".to_owned())]))
            ),
        );
    }

    /// Enter in History runs the recalled line at any selection, and never a
    /// list row in its place. `C-Up` from the third row recalls `w`, whose
    /// filter keeps that row, and Enter saves. The line `pw` names no command,
    /// so Enter runs nothing, although its filter lists `pwd` first.
    #[test]
    fn enter_in_history_runs_the_line_and_never_a_list_row() {
        assert_eq!(
            [
                history_submit("w", "down down ctrl-up"),
                history_submit("pw", "up")
            ],
            [(2, Some("SaveBuffer")), (0, None)],
        );
    }

    /// A step that follows an edit in the same frame ends the old walk first.
    /// Up then walks under the edited text, and Down finds no walk to step.
    #[test]
    fn a_step_after_an_edit_in_the_same_frame_ends_the_old_walk() {
        assert_eq!(
            [
                stepped_after_an_edit("up", &stoat_action::PaletteUp),
                stepped_after_an_edit("up up", &stoat_action::PickerNext),
            ],
            ["cd /beta", "cd"],
        );
    }

    /// A History row ends in the age of its line, two minutes after the line
    /// ran.
    #[test]
    fn a_history_row_shows_its_age() {
        let mut h = TestHarness::with_size(160, 50);
        h.type_text(":pwd");
        h.type_keys("enter");
        h.advance_clock(Duration::from_secs(125));
        h.type_text(":");
        h.type_keys("up");
        let _ = h.snapshot();

        let row = history_list_row(&h, 0);
        assert_eq!(
            (row.starts_with("\u{21ba}  pwd"), row.ends_with("2m")),
            (true, true),
            "the History row reads {row:?}",
        );
    }

    /// A harness 160 by 50 whose palette history holds `cd /alpha` and then
    /// `cd /beta`.
    fn history_harness() -> TestHarness {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["cd /alpha".to_string(), "cd /beta".to_string()]);
        h
    }

    /// Whether the palette history walks.
    fn walking(h: &TestHarness) -> bool {
        h.stoat.active_workspace().palette_history.is_walking()
    }

    /// The selection, and the name of the action that Enter dispatches, after
    /// `keys` typed after `:` over a palette history that holds only `line`.
    fn history_submit(line: &str, keys: &str) -> (usize, Option<&'static str>) {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec![line.to_string()]);
        h.type_text(":");
        h.type_keys(keys);

        let mut palette = h.stoat.command_palette.take().expect("palette open");
        let selected = palette.selected;
        match palette.handle_submit(h.stoat.active_workspace_mut()) {
            PaletteOutcome::Dispatch(entry, _, _) => (selected, Some(entry.def.name())),
            _ => (selected, None),
        }
    }

    /// The palette input after `walk`, an edit of the input to `cd`, and
    /// `action`, with no frame between the edit and the action.
    ///
    /// The history holds `cd /alpha`, `pwd`, and `cd /beta`.
    fn stepped_after_an_edit(walk: &str, action: &dyn Action) -> String {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().palette_history = InputHistory::from_entries(vec![
            "cd /alpha".to_string(),
            "pwd".to_string(),
            "cd /beta".to_string(),
        ]);
        h.type_text(":");
        h.type_keys(walk);

        let ws = &mut h.stoat.workspaces[h.stoat.active_workspace];
        let palette = h.stoat.command_palette.as_mut().expect("palette open");
        palette.input.replace_text(ws, "cd");
        crate::action_handlers::dispatch(&mut h.stoat, action);
        palette_text(&h)
    }

    /// The text of the palette list's row `row`, from its second cell, with
    /// its trailing blanks cut. The first cell holds the selected row's bar.
    fn history_list_row(h: &TestHarness, row: u16) -> String {
        let (rows, zoom, header_rows) = crate::test_fixture::palette_sizing(h);
        let list = crate::render::command_palette::palette_filter_layout(
            h.stoat.size(),
            rows,
            zoom,
            header_rows,
        )
        .expect("the palette fits the test terminal")
        .list;
        let buf = h.rendered_buffer();
        let text: String = (list.x + 1..list.right())
            .map(|x| buf[(x, list.y + row)].symbol())
            .collect();
        text.trim_end().to_owned()
    }

    /// The text of the palette's key line, and the foreground of each of its
    /// cells. The text reads U+2500 as a space, because the cell look's rule
    /// fills the line's blank cells with it.
    fn key_line(h: &TestHarness) -> (String, Vec<Color>) {
        let (rows, zoom, header_rows) = crate::test_fixture::palette_sizing(h);
        let footer = crate::render::command_palette::palette_filter_layout(
            h.stoat.size(),
            rows,
            zoom,
            header_rows,
        )
        .expect("the palette fits the test terminal")
        .footer;
        let buf = h.rendered_buffer();
        let cells = || (footer.x..footer.right()).map(|x| &buf[(x, footer.y)]);
        let text = cells()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .replace('\u{2500}', " ");
        (text, cells().map(|cell| cell.fg).collect())
    }

    /// The doc lines that the palette's doc island scrolls past.
    fn doc_scroll(h: &TestHarness) -> u16 {
        h.stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .doc_scroll
    }

    #[test]
    fn abort_rebase_hidden_by_default_visible_after_backtab() {
        let mut h = Stoat::test();
        h.type_text(":action abort");
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
        h.type_text(":reloadenv");
        h.assert_snapshot("command_palette_filter_narrows_to_one");
    }

    /// `:git log` narrows the group to `log`, whose doc opens with the usage
    /// line `git log [rev]`, then the alias, the parameter, and the
    /// description.
    #[test]
    fn snapshot_command_palette_group_doc() {
        let mut h = repo_harness();
        h.type_text(":git log");
        h.assert_snapshot("command_palette_group_doc");
    }

    /// The argument picker's plate fits the widest label on show. The tail
    /// `zz` shows the 66-char path, so the plate grows from 136 to 176
    /// columns. Clearing the tail fits the list back to 33 columns, and the
    /// plate keeps its width.
    #[test]
    fn the_argument_plate_grows_with_its_widest_row_and_keeps_its_width() {
        let mut h = argument_plate_harness();
        let open = palette_plate(&h);
        h.type_text("zz");
        h.settle();
        h.stoat.render();
        let grown = palette_plate(&h);
        h.type_keys("backspace backspace");
        h.settle();
        h.stoat.render();

        assert_eq!(
            [open, grown, palette_plate(&h)],
            [(136, 33), (176, 69), (176, 33)],
        );
    }

    /// The palette keeps its widest argument plate only while it stays open,
    /// so the next open fits the plate to the labels on show again.
    #[test]
    fn the_argument_plate_starts_over_when_the_palette_opens() {
        let mut h = argument_plate_harness();
        h.type_text("zz");
        h.settle();
        h.stoat.render();
        let grown = palette_plate(&h).0;
        h.type_keys("escape");
        h.type_text(":o ");
        h.settle();
        h.stoat.render();

        assert_eq!((grown, palette_plate(&h).0), (176, 136));
    }

    /// The zoom steps walk the box on show, so the argument picker's zoom box
    /// is its fitted plate and not the box of the command list.
    #[test]
    fn the_zoom_box_of_the_argument_picker_is_its_fitted_plate() {
        let h = argument_plate_harness();
        let zoom_box = crate::mouse::open_modal_box(&h.stoat, crate::app::ModalKind::Palette, 0);

        assert_eq!(
            zoom_box,
            Some(crate::test_fixture::palette_arg_layout(&h).modal)
        );
    }

    /// Zoom steps shrink the argument picker's box down to the palette's
    /// 20-column minimum. On a 300-column window four steps out take the
    /// 136-column plate past it.
    #[test]
    fn zooming_out_stops_the_argument_box_at_twenty_columns() {
        let mut h = TestHarness::with_size(300, 40);
        h.type_text(":o ");
        h.settle();

        let width = crate::mouse::open_modal_box(&h.stoat, crate::app::ModalKind::Palette, -4)
            .map(|modal| modal.width);
        assert_eq!(width, Some(20));
    }

    /// The command list keeps the list modal's width, 160 columns of a
    /// 200-column window, since only the argument picker fits its plate.
    #[test]
    fn the_command_list_keeps_the_list_modal_width() {
        let mut h = TestHarness::with_size(200, 40);
        h.type_text(":");
        h.settle();

        let modal = crate::mouse::open_modal_box(&h.stoat, crate::app::ModalKind::Palette, 0);
        assert_eq!(modal.map(|modal| modal.width), Some(160));
    }

    /// A 200 by 40 harness with `:o ` open over 35 short paths and one path of
    /// 66 chars, which the list shows only under the tail `zz`.
    fn argument_plate_harness() -> TestHarness {
        let mut h = TestHarness::with_size(200, 40);
        let names: Vec<String> = (0..35)
            .map(|n| format!("a{n:02}.rs"))
            .chain([format!("zz/{}.rs", "x".repeat(60))])
            .collect();
        let files: Vec<(&str, &str)> = names.iter().map(|name| (name.as_str(), "")).collect();
        seed_palette_workspace(&mut h, &files);
        h.type_text(":o ");
        h.settle();
        h.stoat.render();
        h
    }

    /// The open argument picker's modal width and list width.
    fn palette_plate(h: &TestHarness) -> (u16, u16) {
        let layout = crate::test_fixture::palette_arg_layout(h);
        (layout.modal.width, layout.list.width)
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

    /// The `:o ` arg-picker preview is syntax-highlighted once its parse lands,
    /// the same as the file finder's. A freshly selected file has no prior tree
    /// to carry, so the frames before its parse completes render in
    /// `fallback_style`.
    #[test]
    fn snapshot_palette_arg_preview_highlighted_once_its_parse_lands() {
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
        // Spawn the preview's parse and run it. The snapshot's own background
        // drive is the poll that installs the result.
        h.stoat.drive_background();
        h.settle();
        h.assert_snapshot_one_frame("palette_arg_preview_highlighted_after_parse");
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

        // Arg-picker navigation rides Up/Down and Ctrl-p/Ctrl-n. Ctrl-Up and
        // Ctrl-Down recall history.
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

    /// Drive frames until a fallback of ignored files stands in with rows.
    ///
    /// Active alone is a frame early, since the fallback's first scan lands a
    /// frame after its walk.
    fn settle_arg_fallback(h: &mut TestHarness) {
        for _ in 0..8 {
            let _ = h.snapshot();
            h.settle();
            if arg_picker(h).fallback_active() && !arg_rows(h).is_empty() {
                return;
            }
        }
        panic!("no fallback of ignored files stood in with rows after eight frames");
    }

    /// Drive the frames a tail with no listed match takes to settle, and
    /// report whether the picker installed a fallback.
    fn has_fallback_after(h: &mut TestHarness, text: &str) -> bool {
        h.type_text(text);
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        arg_picker(h).ignored.is_some()
    }

    #[test]
    fn an_open_tail_no_listed_file_matches_lists_ignored_files() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(
            &mut h,
            &[
                ("src/main.rs", ""),
                (".gitignore", "out/\n"),
                ("out/main.toml", ""),
            ],
        );
        let toml = root.join("out/main.toml");

        h.type_text(":o main.toml");
        settle_arg_fallback(&mut h);
        assert_eq!(arg_rows(&h), [toml.display().to_string()]);

        h.type_keys("enter");
        assert_eq!(editor::focused_buffer_path(&h.stoat), toml);
    }

    /// While ignored files stand in for the list, the Cmd Line's count says so
    /// at the row's right end.
    #[test]
    fn an_open_tail_fallback_marks_the_count_ignored() {
        let mut h = Stoat::test();
        seed_palette_workspace(
            &mut h,
            &[
                ("src/main.rs", ""),
                (".gitignore", "out/\n"),
                ("out/main.toml", ""),
            ],
        );
        h.type_text(":o main.toml");
        settle_arg_fallback(&mut h);
        let _ = h.snapshot();
        let text = h.rendered_text();
        let cmd_line = text
            .lines()
            .find(|line| line.contains('\u{276f}'))
            .unwrap_or_default();
        assert!(
            cmd_line.contains(" ignored\u{2502}"),
            "the count names the fallback before the border:\n{text}"
        );
    }

    #[test]
    fn an_open_tail_fallback_skips_what_stoatignore_lists() {
        let mut h = Stoat::test();
        seed_palette_workspace(
            &mut h,
            &[
                (".stoatignore", "out/\n"),
                (".gitignore", "out/\n"),
                ("src/main.rs", ""),
                ("out/main.toml", ""),
            ],
        );

        h.type_text(":o main.toml");
        for _ in 0..8 {
            let _ = h.snapshot();
            h.settle();
        }

        assert_eq!(
            (
                arg_picker(&h).ignored.as_ref().map(|f| f.lists_nothing()),
                arg_rows(&h),
            ),
            (Some(true), vec![]),
            "the fallback walked and matched nothing, since .stoatignore hides out/",
        );
    }

    /// Seed a workspace whose `:o` fallback of ignored files stood in for
    /// `main.toml`, with the tail then cut back to `main`, which the listed
    /// `src/main.rs` matches. Returns the workspace root.
    fn open_tail_back_on_a_listed_match(h: &mut TestHarness) -> PathBuf {
        let root = seed_palette_workspace(
            h,
            &[
                ("src/main.rs", ""),
                (".gitignore", "out/\n"),
                ("out/main.toml", ""),
            ],
        );
        h.type_text(":o main.toml");
        settle_arg_fallback(h);

        h.type_keys("backspace backspace backspace backspace backspace");
        for _ in 0..2 {
            let _ = h.snapshot();
            h.settle();
        }
        root
    }

    /// The tail the `:o` fallback last took, or `None` with no fallback.
    fn arg_fallback_query(h: &TestHarness) -> Option<String> {
        arg_picker(h)
            .ignored
            .as_ref()
            .map(|f| f.last_filter_text.clone())
    }

    #[test]
    fn a_listed_tail_leaves_the_open_fallback_idle() {
        let mut h = Stoat::test();
        let root = open_tail_back_on_a_listed_match(&mut h);
        assert_eq!(
            (arg_rows(&h), arg_fallback_query(&h)),
            (
                vec![root.join("src/main.rs").display().to_string()],
                Some(String::new()),
            ),
            "the list shows its match and the fallback holds no tail",
        );
    }

    #[test]
    fn settling_a_listed_tail_leaves_the_open_fallback_unsettled() {
        let mut h = Stoat::test();
        open_tail_back_on_a_listed_match(&mut h);
        let palette = h.stoat.command_palette.as_mut().expect("palette open");
        let picker = palette.arg_picker.as_mut().expect("arg picker active");
        picker.settle_scan("main");
        assert_eq!(arg_fallback_query(&h), Some(String::new()));
    }

    #[test]
    fn closing_with_a_fallback_leaves_the_registry_at_its_pre_open_size() {
        let mut h = Stoat::test();
        seed_palette_workspace(
            &mut h,
            &[
                ("src/main.rs", ""),
                (".gitignore", "out/\n"),
                ("out/main.toml", ""),
            ],
        );
        let slots = |h: &TestHarness| {
            let ws = h.stoat.active_workspace();
            (ws.buffers.len(), ws.editors.len())
        };
        let before = slots(&h);

        h.type_text(":o main.toml");
        settle_arg_fallback(&mut h);
        h.type_keys("escape");

        assert!(h.stoat.command_palette.is_none());
        assert_eq!(
            slots(&h),
            before,
            "the fallback's preview buffer and editor go with the palette",
        );
    }

    #[test]
    fn an_absolute_open_tail_takes_no_fallback() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("src/main.rs", "")]);
        assert!(
            !has_fallback_after(&mut h, ":o /nowhere/x"),
            "an explicit path dispatches as typed",
        );
    }

    #[test]
    fn a_cd_tail_takes_no_fallback() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("src/main.rs", "")]);
        assert!(
            !has_fallback_after(&mut h, ":cd zzz"),
            "only a file argument falls back",
        );
    }

    #[test]
    fn arg_picker_lists_an_arguments_fixed_values() {
        let mut h = Stoat::test();
        h.type_text(":auto-reload ");
        h.snapshot();
        assert_eq!(arg_rows(&h), ["follow", "off", "on"]);
    }

    #[test]
    fn arg_picker_lists_open_config_targets() {
        let mut h = Stoat::test();
        h.type_text(":config ");
        h.snapshot();
        assert_eq!(arg_rows(&h), ["stoat", "stoatty"]);
    }

    #[test]
    fn arg_picker_lists_open_logs_targets() {
        let mut h = Stoat::test();
        h.type_text(":logs ");
        h.snapshot();
        assert_eq!(arg_rows(&h), ["stoat", "stoatty"]);
    }

    #[test]
    fn arg_picker_narrows_fixed_values_on_typing() {
        let mut h = Stoat::test();
        h.type_text(":auto-reload ff");
        h.snapshot();
        assert_eq!(arg_rows(&h), ["off"]);
    }

    /// A ref argument lists the local branches with HEAD's first, and previews
    /// nothing, since a branch row names no file. Outside a repository it lists
    /// nothing, and the typed tail still dispatches.
    #[test]
    fn a_ref_argument_lists_the_branches_with_heads_first() {
        let shown = |mut h: TestHarness, text: &str| {
            h.type_text(text);
            h.snapshot();
            h.settle();
            (listed_arg_rows(&h), arg_preview_text(&h))
        };
        let branches = vec!["main".to_string(), "feature".to_string()];
        assert_eq!(
            [
                shown(branch_harness(), ":git log "),
                shown(branch_harness(), ":git review "),
                shown(Stoat::test(), ":git log "),
            ],
            [
                (branches.clone(), String::new()),
                (branches, String::new()),
                (Vec::new(), String::new()),
            ],
        );
    }

    /// A typed prefix narrows the branches, and Enter takes the highlighted
    /// branch rather than the prefix.
    #[test]
    fn enter_on_a_narrowed_ref_takes_the_highlighted_branch() {
        let mut h = branch_harness();
        h.type_text(":git log feat");
        h.snapshot();
        let narrowed = listed_arg_rows(&h);
        h.type_keys("enter");
        h.settle();
        let label = h
            .stoat
            .commit_picker
            .as_ref()
            .and_then(|picker| picker.scope_label.clone());
        assert_eq!(
            (narrowed, label.as_deref()),
            (vec!["feature".to_string()], Some("git log feature")),
        );
    }

    #[test]
    fn chosen_value_cuts_a_revision_row_at_its_sha() {
        assert_eq!(
            [
                chosen_value(ValueSource::Revisions, "c2 chore: tweak a"),
                chosen_value(ValueSource::Revisions, "main"),
                chosen_value(ValueSource::Branches, "c2 chore"),
            ],
            ["c2", "main", "c2 chore"],
        );
    }

    /// A required revision and an optional count, standing in for a command
    /// with two parameters.
    const REV_AND_COUNT: &[ParamDef] = &[
        ParamDef {
            name: "rev",
            kind: ParamKind::String,
            value_source: ValueSource::Revisions,
            required: true,
            description: "",
        },
        ParamDef {
            name: "count",
            kind: ParamKind::Number,
            value_source: ValueSource::None,
            required: false,
            description: "",
        },
    ];

    #[test]
    fn arg_index_counts_the_token_being_typed() {
        assert_eq!(
            ["", "a", "a ", "a b", "a b "].map(arg_index),
            [0, 0, 1, 1, 2]
        );
    }

    #[test]
    fn split_last_token_keeps_the_text_before_the_token() {
        assert_eq!(
            ["feature ma", "feature ", "feature", "", "a  b"].map(split_last_token),
            [
                ("feature ", "ma"),
                ("feature ", ""),
                ("", "feature"),
                ("", ""),
                ("a  ", "b"),
            ],
        );
    }

    #[test]
    fn param_at_gives_a_lone_parameter_every_index() {
        let names = |params: &'static [ParamDef]| {
            [0, 1, 2].map(|index| param_at(params, index).map(|param| param.name))
        };
        assert_eq!(
            [names(REV_AND_COUNT), names(&REV_AND_COUNT[..1])],
            [[Some("rev"), Some("count"), None], [Some("rev"); 3]],
        );
    }

    /// A picked row stands in for the token being typed, or follows the tokens
    /// after a trailing space.
    #[test]
    fn parse_args_reads_one_token_per_parameter() {
        let parse = |tail: &str, picked: Option<&str>| parse_args(REV_AND_COUNT, tail, picked);
        let rev = || ParamValue::String("feature".to_string());
        let count = || ParamValue::Number(4.0);
        assert_eq!(
            [
                parse("feature", None),
                parse("fea", Some("feature")),
                parse("feature 4", None),
                parse("feature ", Some("4")),
            ],
            [
                Ok((vec![rev()], "feature".to_string())),
                Ok((vec![rev()], "feature".to_string())),
                Ok((vec![rev(), count()], "feature 4".to_string())),
                Ok((vec![rev(), count()], "feature 4".to_string())),
            ],
        );
    }

    #[test]
    fn parse_args_refuses_what_no_parameter_takes() {
        assert_eq!(
            [
                parse_args(REV_AND_COUNT, "", None),
                parse_args(REV_AND_COUNT, "feature x", None),
                parse_args(REV_AND_COUNT, "feature 4 extra", None),
            ],
            [
                Err(ArgsError::Missing { param: "rev" }),
                Err(ArgsError::Invalid {
                    param: "count",
                    arg: "x".to_string()
                }),
                Err(ArgsError::Extra {
                    arg: "extra".to_string()
                }),
            ],
        );
    }

    /// A revision list keeps its rows in order on an empty query, and Tab on a
    /// commit row writes its sha, with the next press stepping to the next row.
    #[test]
    fn a_revision_picker_keeps_its_order_and_tabs_in_shas() {
        let mut h = Stoat::test();
        let executor = h.stoat.executor.clone();
        let root = PathBuf::from("/repo");
        let rows = ["main", "feature", "c2 chore: tweak a", "c1 commit c1"];
        let mut picker = ArgPicker::new(
            h.stoat.active_workspace_mut(),
            executor,
            ValueSource::Revisions,
            root.clone(),
            None,
            rows.map(PathBuf::from).to_vec(),
        );

        picker.active_core().refilter("");
        let listed: Vec<String> = {
            let picklist = &picker.core.picklist;
            picklist
                .filtered
                .iter()
                .map(|&i| picklist.base[i].display().to_string())
                .collect()
        };
        picker.active_core().picklist.selected = 2;
        let first = picker.advance_tab_cycle("", &root);
        let second = first
            .clone()
            .and_then(|tail| picker.advance_tab_cycle(&tail, &root));
        picker.dispose(h.stoat.active_workspace_mut());

        assert_eq!(
            (listed, first.as_deref(), second.as_deref()),
            (rows.map(String::from).to_vec(), Some("c2"), Some("c1")),
        );
    }

    /// A revision that matches no branch dispatches as typed. A sha opens its
    /// history, and a name that resolves to nothing badges and opens nothing.
    #[test]
    fn a_ref_matching_no_branch_dispatches_as_typed() {
        let submit = |tail: &str| {
            let mut h = branch_harness();
            h.type_text(&format!(":git ls {tail}"));
            h.type_keys("enter");
            h.settle();
            let label = h
                .stoat
                .commit_picker
                .as_ref()
                .and_then(|picker| picker.scope_label.clone());
            let ws = h.stoat.active_workspace();
            let badge = ws
                .badges
                .find_by_source(BadgeSource::Review)
                .and_then(|id| ws.badges.get(id))
                .map(|badge| badge.label.clone());
            (label, badge)
        };
        assert_eq!(
            [submit("a1b2c3d4"), submit("junk")],
            [
                (Some("git log a1b2c3d4".to_string()), None),
                (None, Some("unknown revision".to_string())),
            ],
        );
    }

    #[test]
    fn arg_submit_dispatches_selected_value() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("note.txt", "hello\n")]);
        h.type_text(":o note");
        h.snapshot();
        h.type_keys("enter");
        h.snapshot();
        let buffer = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor focused")
            .buffer_id;

        h.type_text(":auto-reload on");
        h.snapshot();
        h.type_keys("enter");
        assert_eq!(
            h.stoat.active_workspace().buffers.auto_reload_mode(buffer),
            AutoReloadMode::Tail
        );

        // "ff" parses as no valid state on its own, so a mode change here can
        // only have come from the picker's selected "off" row.
        h.type_text(":auto-reload ff");
        h.snapshot();
        h.type_keys("enter");
        assert_eq!(
            h.stoat.active_workspace().buffers.auto_reload_mode(buffer),
            AutoReloadMode::Off
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

    /// What is rendered into the argument picker's preview pane.
    fn arg_preview_text(h: &TestHarness) -> String {
        let preview_id = arg_picker(h).active_core_ref().preview.buffer;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(preview_id)
            .expect("preview buffer");
        let guard = buffer.read().expect("poisoned");
        guard.rope().to_string()
    }

    /// A directory row had nothing to preview, which is a gap rather than a
    /// decision: what a reader wants to know about a directory is what is in
    /// it. Directories lead, and the slash is what separates a name with a
    /// directory behind it from one that merely has no extension.
    #[test]
    fn a_cd_row_previews_the_directorys_children() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);
        let home = PathBuf::from("/fake-home");
        h.fake_fs().insert_files([
            (home.join("alpha/zeta.rs"), "z".as_bytes()),
            (home.join("alpha/apple.rs"), "a".as_bytes()),
        ]);
        h.fake_fs().insert_dir(home.join("alpha/nested"));
        h.fake_env().set("HOME", home.to_str().unwrap());

        // The first browse row under `~/` is `alpha`, which the picker selects
        // and previews.
        h.type_text(":cd ~/");
        h.snapshot();
        h.settle();

        assert_eq!(
            arg_preview_text(&h),
            "nested/\napple.rs\nzeta.rs\n",
            "the directory lists, directories first and then by name",
        );
    }

    /// A listing past the cap says how much it left out. Silently stopping
    /// would read as a directory holding exactly the cap.
    #[test]
    fn a_long_listing_says_how_much_it_left_out() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "")]);
        let home = PathBuf::from("/fake-home");
        h.fake_fs().insert_files(
            (0..PREVIEW_DIR_LIMIT + 3)
                .map(|i| (home.join(format!("big/f{i:04}.rs")), b"x".as_slice()))
                .collect::<Vec<_>>(),
        );
        h.fake_env().set("HOME", home.to_str().unwrap());

        h.type_text(":cd ~/");
        h.snapshot();
        h.settle();

        let shown = arg_preview_text(&h);
        assert_eq!(
            (
                shown.lines().count(),
                shown.lines().next(),
                shown.lines().last(),
            ),
            (
                PREVIEW_DIR_LIMIT + 1,
                Some("f0000.rs"),
                Some("... and 3 more"),
            ),
            "the cap holds and the tail names what is past it",
        );
    }

    /// A file row still reads the file. The directory listing is a fall-through
    /// for a read that could not have worked, not a new answer for every row.
    #[test]
    fn a_file_row_still_previews_its_contents() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("wsdir/f.rs", "the file body\n")]);

        h.type_text(":o ");
        h.snapshot();
        h.settle();

        assert_eq!(arg_preview_text(&h), "the file body\n");
    }

    #[test]
    fn a_gzip_row_previews_its_text() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(&mut h, &[]);
        h.fake_fs().insert_file(
            root.join("f.rs.gz"),
            crate::test_harness::gzip(b"fn a() {}\n"),
        );

        h.type_text(":o ");
        h.snapshot();
        h.settle();

        assert_eq!(arg_preview_text(&h), "fn a() {}\n");
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
        h.type_text(":action file open");
        h.assert_snapshot("command_palette_multi_token_highlight");
    }

    #[test]
    fn snapshot_command_palette_filter_scrolls_to_selection() {
        let mut h = Stoat::test();
        h.type_text(":");
        h.type_keys(
            "ctrl-n ctrl-n ctrl-n ctrl-n ctrl-n ctrl-n ctrl-n ctrl-n ctrl-n ctrl-n ctrl-n ctrl-n",
        );
        h.assert_snapshot("command_palette_filter_scrolls_to_selection");
    }
}
