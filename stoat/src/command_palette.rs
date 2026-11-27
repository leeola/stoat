use crate::{
    app::Stoat,
    cli_spec::CliSpec,
    file_finder::{self, Browse, FileScopes, FinderScope},
    fuzzy,
    host::FsHost,
    input_view::{InputView, SubmitTarget},
    keymap::ActionKeys,
    pane::{FocusTarget, View},
    paths,
    picker::{self, PathPicker, PendingScan, Preview, PreviewPolicy, ScanTarget},
    rebase::RebasePause,
    render::{picker::PlateFit, pixel_rows::RowMetrics},
    run::CmdId,
    user_command::{
        run, ArgKind, CommandArg, CommandSet, Output, PickSpec, RunSpec, TextDefault, UserCommand,
    },
    workspace::Workspace,
};
use ratatui::layout::Rect;
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    fmt,
    hash::{Hash, Hasher},
    iter,
    ops::Range,
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, OnceLock},
};
use stoat_action::{
    registry, ActionDef, ActionKind, ActionPriority, ParamDef, ParamError, ParamKind, ParamValue,
    ValueSource,
};
use stoat_language::LanguageRegistry;
use stoat_scheduler::{Executor, Task};
use tokio::sync::mpsc::UnboundedReceiver;

/// The physical px rows that each pixel History entry on show takes from the
/// top of History's list, with the index of the entry's line in the palette
/// history.
///
/// A press reads it, because pixel rows stand off the cell grid and two
/// entries can share a cell row.
pub(crate) type HistorySpans = Vec<(Range<u32>, usize)>;

pub struct CommandPalette {
    /// The single command-line input, holding the raw text typed after `:`.
    /// Parsed each frame into an optional command plus a trailing argument
    /// by [`CommandPalette::refilter_from_input`].
    pub(crate) input: InputView,
    /// The commands matching the current filter text, in display order. The
    /// built-in commands come first, then the user commands, then the `action`
    /// row. Empty while [`Self::command`] is set, since arg mode replaces the
    /// list with the argument picker.
    pub(crate) filtered: Vec<PaletteCommand>,
    /// Per-row matched character offsets into each entry's name, parallel to
    /// [`Self::filtered`], used by the renderer to highlight matched cells.
    pub(crate) match_indices: Vec<Vec<u32>>,
    pub(crate) selected: usize,
    /// `Some` once the input parses as a known command followed by a space:
    /// the palette is collecting that command's trailing argument inline. The
    /// argument text is the input tail after the command token.
    pub(crate) command: Option<PaletteCommand>,
    /// Which parameter of [`Self::command`] the token being typed fills, as
    /// [`arg_index`] counts it in the input tail, or [`user_arg_index`] for a
    /// user command.
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
    pub arg_picker: Option<ArgPicker>,
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
    /// The list columns of the argument picker's plate, measured when the rows
    /// of its list last changed, or `None` before the first measure.
    ///
    /// A change of the tail or of the argument's source refits the plate wider
    /// or narrower, and a selection move keeps it. Each open starts with no
    /// fit.
    ///
    /// See also:
    /// - [`refit_arg_plate`](crate::render::command_palette::refit_arg_plate), which the
    ///   background pump calls to measure it.
    pub(crate) arg_plate_fit: Option<PlateFit>,
    /// The cell and scale that the last frame laid the command list or the
    /// argument list out by, or `None` for a list on whole cell rows.
    ///
    /// The pool that pages a pixel list and the hit test read the layout after
    /// the paint, so the paint keeps its measure here, as it keeps the
    /// picker's `viewport_rows`.
    pub(crate) row_metrics: Option<RowMetrics>,
    /// The rows that the Cmd Line took at the box's top in the last frame, 1
    /// before the first.
    ///
    /// An input wider than its row wraps onto more rows, and the body under
    /// them starts lower. The renderer measures the rows as it paints them,
    /// and the hit test and the smooth-scroll emit read the body after the
    /// paint, so they find the rects the frame drew.
    pub header_rows: u16,
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
    /// frame that draws no History or draws it on pixel rows.
    pub(crate) history_rows: Vec<(Rect, usize)>,
    /// The spans of the History entries that the last frame drew as pixel
    /// rows, from the top of [`Self::history_body`], for the pointer.
    ///
    /// Empty after a frame that draws History on cell rows or draws none, when
    /// [`Self::history_rows`] maps a press instead.
    pub(crate) history_spans: HistorySpans,
    /// The list rect of the last frame's pixel History, which a press reads its
    /// pixel row against, or `None` after a frame with no pixel History.
    pub history_body: Option<Rect>,
    /// The user commands of the config when the palette opened, which a head
    /// names after the built-in commands.
    pub(crate) commands: Arc<CommandSet>,
    /// The default that the user command's argument in entry shows after the
    /// typed text while that argument holds no text, or `None`.
    ///
    /// [`crate::action_handlers::sync_palette_picker`] sets it each frame.
    pub(crate) arg_ghost: Option<String>,
    /// The text of each default that the ghost has shown, with the name of its
    /// command and the index of its argument.
    ///
    /// A context value reads the editor and the repository, so it resolves
    /// once for each command and argument while the palette is open.
    pub(crate) resolved_defaults: Vec<(String, usize, String)>,
    /// The list of the user command's `pick` argument in entry, or `None`
    /// while no such argument is in entry.
    ///
    /// [`crate::action_handlers::sync_palette_picker`] starts it when the
    /// argument's entry starts, and drops it for any other input.
    pub(crate) pick: Option<PickState>,
    /// The list of the flags or the subcommands of the `cli` command in
    /// entry, with that command's name, or `None` while no such list shows.
    ///
    /// [`crate::action_handlers::sync_palette_picker`] builds it once the
    /// command's help lands, builds it again when the command or the kind
    /// changes, and drops it for a word that is free text.
    pub(crate) cli_list: Option<(String, CliListKind, ArgList)>,
    /// The modal rect that the last paint of a typing stage drew, or `None`
    /// before the first paint.
    ///
    /// The Run Block opens over it, so the plate keeps its top and its width
    /// when Enter runs a Cmd.
    pub(crate) plate: Option<Rect>,
    /// The Cmd that the palette shows as its Run Block, or `None` while the
    /// palette takes typing.
    pub(crate) block: Option<PaletteBlock>,
}

/// The Cmd that the palette shows as its Run Block, which a `block` command's
/// Enter turns the palette into.
pub(crate) struct PaletteBlock {
    pub(crate) cmd: CmdId,
    /// The plate of the typing stage when the Run Block opened, whose top row
    /// and width the Run Block keeps.
    pub(crate) origin: Rect,
    /// The body rows that the Run Block holds, which grow with the output
    /// while the Cmd runs and take the rows of the finished body at its end.
    pub(crate) rows: u16,
}

/// A command that the palette's input names, a registered action or a user
/// command of the config.
#[derive(Clone)]
pub(crate) enum PaletteCommand {
    Action(&'static registry::RegistryEntry),
    User(Arc<UserCommand>),
}

impl PaletteCommand {
    /// The name that the palette's list and input spell, the action's command
    /// name or the user command's name.
    pub(crate) fn name(&self) -> &str {
        match self {
            PaletteCommand::Action(entry) => &entry.command_name,
            PaletteCommand::User(command) => &command.name,
        }
    }

    /// The parameters of the action, or the arguments of the user command. A
    /// `cli` command takes one, its tail.
    pub(crate) fn arg_count(&self) -> usize {
        match self {
            PaletteCommand::Action(entry) => entry.def.params().len(),
            PaletteCommand::User(command) => match command.run {
                RunSpec::Cli { .. } => 1,
                RunSpec::Steps { .. } => command.args.len(),
            },
        }
    }
}

/// A command token that the palette's input names, and the ghost that the Cmd
/// Line draws after it.
///
/// The ghost is the rest of the command's name that the user did not type, so
/// a typed `o` reads as `open`. It is display only: the input, its caret, Tab,
/// and Enter never see it.
pub(crate) struct ResolvedCommand {
    /// The command that the head names, which decides whether the head draws
    /// as a command or as an internal action.
    pub(crate) entry: PaletteCommand,
    /// The typed head's length in chars, from the input's start.
    pub(crate) head_chars: usize,
    /// The text after the head, empty for a head that spells the whole name.
    pub(crate) ghost: String,
}

/// A row of an argument list that the palette builds, as the text it shows
/// and the text it writes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ArgRow {
    /// The text that the row shows, which the typed text matches.
    pub(crate) label: String,
    /// The text that Tab writes for the row, and Enter for a pick row.
    pub(crate) insert: String,
    /// The name of the value that a flag row's flag takes, or `None` for a
    /// row that names no value.
    pub(crate) value_name: Option<String>,
    /// The text that a flag or a subcommand row shows after its label, empty
    /// for a pick row.
    pub(crate) description: String,
}

/// Which list the argument entry of a `cli` command shows, as
/// [`cli_list_kind`] picks it from the word in entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliListKind {
    /// The tool's own flags, for a word that starts with `-`.
    Flags,
    /// The tool's subcommands, for its first word that is no flag.
    Subcommands,
}

/// The [`ArgRow`]s of an argument list, which the typed text narrows by their
/// labels, and the selection among the rows that match.
#[derive(Default)]
pub(crate) struct ArgList {
    pub(crate) rows: Vec<ArgRow>,
    /// The indices into [`Self::rows`] of the rows that match the query, best
    /// first.
    pub(crate) filtered: Vec<usize>,
    /// The matched char offsets of the leading filtered rows, as
    /// [`picker::rank_into`] stores them. Read them through
    /// [`Self::row_indices`], which covers the rows past its end.
    pub(crate) match_indices: Vec<Vec<u32>>,
    /// The place of the selected row in [`Self::filtered`].
    pub(crate) selected: usize,
    /// The rows that the last paint showed, which a page key steps by. `None`
    /// before the first paint.
    pub(crate) viewport_rows: Option<usize>,
    /// The parse of the query that [`Self::filtered`] answers, which derives
    /// the offsets of a row past [`Self::match_indices`].
    pattern: Option<fuzzy::Pattern>,
    /// The query that [`Self::filtered`] answers, or `None` while the rows
    /// have no match yet.
    query: Option<String>,
}

impl ArgList {
    /// Replace the rows, ranked against the query of the last
    /// [`Self::refilter`], with the selection on the best row.
    ///
    /// Rows that land from a worker rank at once. The frame that paints them
    /// and the sync of the selected row's preview then see the matches. A list
    /// with no refilter yet shows no match until its first.
    pub(crate) fn set_rows(&mut self, rows: Vec<ArgRow>) {
        let query = self.query.take();
        self.rows = rows;
        self.filtered.clear();
        self.match_indices.clear();
        self.selected = 0;
        self.pattern = None;
        if let Some(query) = query {
            self.refilter(&query);
        }
    }

    /// Rank the rows by their labels against `query`, and select the best.
    ///
    /// A query with no atom lists every row in its own order. A query that
    /// the list answers already keeps the matches and the selection, so a
    /// frame that calls this with the same text moves nothing.
    pub(crate) fn refilter(&mut self, query: &str) {
        if self.query.as_deref() == Some(query) {
            return;
        }
        let items = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| (index, row.label.as_str()));
        self.pattern = picker::rank_into(
            query,
            items,
            self.rows.len(),
            &mut self.filtered,
            &mut self.match_indices,
        );
        self.selected = 0;
        self.query = Some(query.to_owned());
    }

    /// The selected row, or `None` while no row matches.
    pub(crate) fn selected_row(&self) -> Option<&ArgRow> {
        self.rows.get(*self.filtered.get(self.selected)?)
    }

    /// Step the selection by `delta` rows, saturating at both ends.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        picker::nav_move(self.filtered.len(), &mut self.selected, delta);
    }

    /// Step the selection by half the rows on show, down for a positive `dir`
    /// and up for a negative one.
    pub(crate) fn page(&mut self, dir: i32) {
        self.move_selection(dir * picker::nav_page_step(self.viewport_rows));
    }

    /// The matched char offsets of the label of filtered row `row`.
    ///
    /// See [`picker::row_indices`] for what `scratch` and `matching` are for.
    pub(crate) fn row_indices<'a>(
        &'a self,
        row: usize,
        scratch: &'a mut Vec<u32>,
        matching: &mut fuzzy::Scratch,
    ) -> &'a [u32] {
        let label = self
            .filtered
            .get(row)
            .and_then(|&index| self.rows.get(index))
            .map_or("", |row| row.label.as_str());
        picker::row_indices(
            &self.match_indices,
            self.pattern.as_ref(),
            row,
            label,
            scratch,
            matching,
        )
    }
}

/// The list of a user command's `pick` argument, with the run of its list
/// command that gives the rows.
pub(crate) struct PickState {
    /// The name of the command and the index of the argument whose list this
    /// is. A new key starts a new run.
    pub(crate) key: (String, usize),
    /// The spec of the argument, whose templates write the rows when the run's
    /// result lands.
    pub(crate) spec: Arc<PickSpec>,
    /// The stamp of the run. A result lands only on the list of its own stamp.
    pub(crate) generation: u64,
    pub(crate) load: PickLoad,
    pub(crate) list: ArgList,
    /// The pane that shows the preview of the selected row. Its scratch
    /// editor lives as long as the state, which disposes it when it drops.
    pub(crate) preview: Preview,
    /// The shell line of each row's preview, in the order of
    /// [`ArgList::rows`], or no line for a spec with no preview.
    pub(crate) preview_lines: Vec<String>,
    /// The text of each preview line that ran, `None` while its run is out.
    ///
    /// A line runs once while the list lives, and a refresh of the list
    /// clears the texts.
    pub(crate) previews: HashMap<String, Option<Arc<str>>>,
}

impl PickState {
    /// A state for the list of `spec`, the argument at `key`, whose run of
    /// `generation` is out.
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        key: (String, usize),
        spec: Arc<PickSpec>,
        generation: u64,
    ) -> Self {
        Self {
            key,
            spec,
            generation,
            load: PickLoad::Loading,
            list: ArgList::default(),
            preview: Preview::new(ws, executor),
            preview_lines: Vec::new(),
            previews: HashMap::new(),
        }
    }

    /// The preview line of the selected row, or `None` with no row selected or
    /// no preview.
    pub(crate) fn selected_preview_line(&self) -> Option<&str> {
        let row = *self.list.filtered.get(self.list.selected)?;
        self.preview_lines.get(row).map(String::as_str)
    }
}

/// Where the run of a pick list's command stands.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PickLoad {
    /// The command runs, and no row has landed.
    Loading,
    /// The rows have landed.
    Ready,
    /// The command failed. The text is the first line of its stderr that holds
    /// text, or `exit <code>`.
    Failed(String),
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
pub struct ArgPicker {
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
    /// The file scope of a [`ValueSource::Files`] argument, or `None` for
    /// every other source.
    ///
    /// It starts at [`FinderScope::All`] at each install, and Shift-Tab steps
    /// it through the cycle of the File Finder. The palette keeps no scope
    /// across opens, so the finder's remembered scope stays its own.
    pub(crate) scopes: Option<FileScopes>,
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
        scopes: Option<FileScopes>,
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
            scopes,
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
                // A refilter points the list at the walk, so a scope with a
                // base of its own settles through the scope instead.
                let scoped = self.browse.is_none() && !self.takes_fallback();
                if scoped && let Some(scopes) = &mut self.scopes {
                    scopes.settle(&mut self.core, &query);
                } else {
                    self.active_core().refilter(&query);
                }
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
    pub fn active_core(&mut self) -> &mut PathPicker {
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
        self.takes_fallback()
            && self.browse.is_none()
            && self.ignored.is_some()
            && self.core.lists_nothing()
    }

    /// The root to walk for a fallback of ignored files, when `tail` needs one
    /// and the picker has none.
    ///
    /// Only a [`ValueSource::Files`] argument falls back, and only once the
    /// workspace list holds nothing for `tail`. A tail that starts with `/` or
    /// `~` names an explicit path, which submit dispatches as typed, so a walk
    /// serves nothing there. A file scope other than [`FinderScope::All`]
    /// takes no fallback, as in the File Finder.
    pub(crate) fn wants_ignored_fallback(&self, tail: &str) -> Option<PathBuf> {
        let tail = tail.trim_start();
        let explicit_path = tail.starts_with('/') || tail.starts_with('~');
        if self.source != ValueSource::Files
            || !self.takes_fallback()
            || self.ignored.is_some()
            || explicit_path
            || !self.core.lists_nothing()
        {
            return None;
        }
        Some(self.core.git_root.clone())
    }

    /// Whether the list falls back to ignored files when it lists nothing.
    ///
    /// A picker with no file scope falls back by its source alone. A file
    /// argument falls back only under [`FinderScope::All`], because every
    /// other scope lists a set that the ignored files are no part of.
    fn takes_fallback(&self) -> bool {
        self.scopes
            .as_ref()
            .is_none_or(|scopes| scopes.scope == FinderScope::All)
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

    /// The modified files, whose rows end in the `M` mark, or no file for a
    /// picker with no file scope.
    ///
    /// The list comes from the git-status query of the file scope, so it is
    /// empty until that answer lands.
    pub(crate) fn modified_paths(&self) -> &[PathBuf] {
        self.scopes
            .as_ref()
            .map_or(&[], |scopes| scopes.modified_paths.as_slice())
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
    /// nothing per keystroke. A file scope refilters the list against its own
    /// base, and a scope other than [`FinderScope::All`] leaves the fallback
    /// alone.
    fn begin_refilter(&mut self, query: &str) -> Vec<PendingScan> {
        let begun = match (&mut self.browse, &mut self.scopes) {
            (Some(browse), _) => {
                browse.picker.pump_scan();
                let partial = browse.partial.clone();
                browse.picker.begin_scan(&partial)
            },
            (None, Some(scopes)) => {
                self.core.pump_scan();
                scopes.pump_modified(&mut self.core);
                scopes.refilter(&mut self.core, query)
            },
            (None, None) => {
                self.core.pump_scan();
                self.core.begin_scan(query)
            },
        };
        let mut pending: Vec<PendingScan> = ScanTarget::List.tag(begun).into_iter().collect();
        if self.takes_fallback()
            && let Some(ignored) = &mut self.ignored
        {
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
    /// when it stands in for the list. A file scope settles the list against
    /// its own base.
    fn settle_scan(&mut self, query: &str) {
        match (&mut self.browse, &mut self.scopes) {
            (Some(browse), _) => {
                let partial = browse.partial.clone();
                browse.picker.settle_scan(&partial);
            },
            (None, Some(scopes)) => scopes.settle(&mut self.core, query),
            (None, None) => self.core.settle_scan(query),
        }
        if self.takes_fallback()
            && self.core.lists_nothing()
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
    /// The block commands of the config alone, with no built-in command and
    /// no `action` row.
    Config,
    /// The `cli` commands of the config alone, the tools that its
    /// `command NAME = cli(...)` items wrap, with no built-in command and no
    /// `action` row.
    Cli,
}

impl PaletteScope {
    /// The scopes that `S-Tab` steps through, in order.
    ///
    /// [`PaletteScope::Config`] joins the cycle while `commands` holds a block
    /// command, and [`PaletteScope::Cli`] while it holds a `cli` command,
    /// because a scope stays out of the cycle until it lists a row.
    pub(crate) fn cycle(commands: &CommandSet) -> Vec<PaletteScope> {
        let mut scopes = vec![PaletteScope::Active, PaletteScope::All];
        for scope in [PaletteScope::Config, PaletteScope::Cli] {
            if commands.iter().any(|command| scope.lists(command)) {
                scopes.push(scope);
            }
        }
        scopes
    }

    /// Whether the scope lists the user command `command`.
    ///
    /// The Config scope lists the block commands and the Cli scope the `cli`
    /// commands. Every other scope lists each user command.
    fn lists(self, command: &UserCommand) -> bool {
        let cli = matches!(command.run, RunSpec::Cli { .. });
        match self {
            PaletteScope::Active | PaletteScope::All => true,
            PaletteScope::Config => !cli,
            PaletteScope::Cli => cli,
        }
    }

    /// Whether the scope lists the registered actions.
    ///
    /// They are the built-in commands, the `action` row, and the internal
    /// actions under that row. The Config and Cli scopes list user commands
    /// alone.
    pub(crate) fn lists_registry(self) -> bool {
        matches!(self, PaletteScope::Active | PaletteScope::All)
    }

    /// The names of the scopes that `S-Tab` reaches from this one, joined by
    /// ` / `, as the palette's key line names them. The names stand in the
    /// order that the key reaches the scopes in `cycle`.
    pub(crate) fn next_names(self, cycle: &[PaletteScope]) -> String {
        let at = cycle.iter().position(|scope| *scope == self).unwrap_or(0);
        let names: Vec<&str> = cycle
            .iter()
            .cycle()
            .skip(at + 1)
            .take(cycle.len().saturating_sub(1))
            .map(|scope| scope.name())
            .collect();
        names.join(" / ")
    }

    /// The name that the key line gives the scope.
    fn name(self) -> &'static str {
        match self {
            PaletteScope::Active => "applicable",
            PaletteScope::All => "all",
            PaletteScope::Config => "config",
            PaletteScope::Cli => "cli",
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
    /// Focused pane hosts a [`View::Run`].
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
        let run_focused = matches!(focused_view, Some(View::Run(_)));
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
    /// A user command is ready to run, with one value for each of its
    /// arguments. The third field is the line `<name> <tail>` to record in
    /// palette history.
    Run(Arc<UserCommand>, Vec<String>, String),
}

/// A line of the Run pane prompt whose head names a command, with the values
/// that its arguments parse to, as [`resolve_line`] reads it.
pub(crate) enum RunLine {
    /// A registered action, with one value for each parameter that the line
    /// gives.
    Action(&'static registry::RegistryEntry, Vec<ParamValue>),
    /// A user command of the config, with one value for each of its
    /// arguments.
    User(Arc<UserCommand>, Vec<String>),
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

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgsError::Invalid { param, arg } => write!(f, "cannot parse `{arg}` as {param}"),
            ArgsError::Missing { param } => write!(f, "{param} is missing"),
            ArgsError::Extra { arg } => write!(f, "no parameter takes `{arg}`"),
        }
    }
}

impl CommandPalette {
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        availability: Availability,
        keys: ActionKeys,
        commands: Arc<CommandSet>,
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
            &commands,
            &mut filtered,
            &mut match_indices,
            &mut selected,
        );
        let (offering, action_offering) = scope_offering(scope, &availability, &commands);
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
            arg_plate_fit: None,
            row_metrics: None,
            header_rows: 1,
            resolved: None,
            keys,
            doc_scroll: 0,
            history_rows: Vec::new(),
            history_spans: Vec::new(),
            history_body: None,
            commands,
            arg_ghost: None,
            resolved_defaults: Vec::new(),
            pick: None,
            cli_list: None,
            plate: None,
            block: None,
        }
    }

    /// Rows the modal's list needs for the whole candidate set behind it,
    /// whichever list is showing.
    ///
    /// The command list counts its `action` row with the commands, except under
    /// the Config and Cli scopes, which list no `action` row. The list of the
    /// internal actions keeps the command list's box, so the box holds its
    /// place when `action ` opens that list.
    ///
    /// Arg mode lists the argument picker's candidates, so `:o ` over a large
    /// workspace sizes to the file count rather than the command count. Both
    /// sources are unfiltered bases, so the answer holds while the user types.
    /// A pick list counts its rows, none while its command runs, and so does
    /// the list of a `cli` command.
    pub fn list_rows_hint(&self) -> u16 {
        if let Some(pick) = &self.pick {
            return rows_of(pick.list.rows.len());
        }
        if let Some((_, _, list)) = &self.cli_list {
            return rows_of(list.rows.len());
        }
        match self.arg_picker.as_ref().filter(|_| self.command.is_some()) {
            Some(picker) => rows_of(picker.active_core_ref().picklist.base.len()),
            None => rows_of(self.offering + usize::from(self.scope.lists_registry())),
        }
    }

    /// Rows the modal's doc takes, for the measure that the last frame laid
    /// the list out by.
    ///
    /// The paint, the pool, and the hit test each lay the box out, so each
    /// reads the doc's rows here and all three split the body alike.
    pub fn doc_rows(&self) -> u16 {
        crate::render::command_palette::doc_rows(self.row_metrics)
    }

    /// The bytes at the start of each listed command name that the group's
    /// title already shows, the group token and its space, which the rows
    /// leave out. Zero outside a group.
    pub(crate) fn name_offset(&self) -> usize {
        self.group.map_or(0, |group| group.len() + 1)
    }

    /// Step the palette's [`PaletteScope`] to the next one of its cycle, as
    /// [`PaletteScope::cycle`] orders them, and re-run the current-input
    /// filter against the new scope. Called from the `PaletteScopeToggle`
    /// action handler (Shift-Tab).
    pub(crate) fn toggle_scope(&mut self, ws: &Workspace) {
        let cycle = PaletteScope::cycle(&self.commands);
        let at = cycle
            .iter()
            .position(|scope| *scope == self.scope)
            .unwrap_or(0);
        self.scope = cycle[(at + 1) % cycle.len()];
        (self.offering, self.action_offering) =
            scope_offering(self.scope, &self.availability, &self.commands);
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
    /// so no input scratch and no preview of an inline picker or a pick list
    /// lingers in the workspace's slotmaps.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
        if let Some(picker) = &self.arg_picker {
            picker.dispose(ws);
        }
        if let Some(pick) = &self.pick {
            pick.preview.dispose(ws);
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
        match self.command.as_ref()? {
            PaletteCommand::Action(entry) => param_at(entry.def.params(), self.typed_arg),
            PaletteCommand::User(_) => None,
        }
    }

    /// The argument of the user command that the text being typed fills, or
    /// `None` while no user command parses.
    pub(crate) fn user_arg(&self) -> Option<&CommandArg> {
        match self.command.as_ref()? {
            PaletteCommand::User(command) => command.args.get(self.typed_arg),
            PaletteCommand::Action(_) => None,
        }
    }

    /// The default of the user command's argument in entry while that argument
    /// holds no text, with the command's name and the argument's index.
    pub(crate) fn empty_default(&self, ws: &Workspace) -> Option<(String, usize, TextDefault)> {
        let (command, index, typed) = self.user_arg_in_entry(ws)?;
        let default = command.args.get(index)?.default()?;
        typed
            .is_empty()
            .then(|| (command.name.clone(), index, default.clone()))
    }

    /// The spec of the user command's `pick` argument in entry, with the
    /// command's name, the argument's index, and the argument's text.
    pub(crate) fn pick_in_entry(
        &self,
        ws: &Workspace,
    ) -> Option<(String, usize, Arc<PickSpec>, String)> {
        let (command, index, typed) = self.user_arg_in_entry(ws)?;
        let ArgKind::Pick(spec) = &command.args.get(index)?.kind else {
            return None;
        };
        Some((command.name.clone(), index, spec.clone(), typed))
    }

    /// The user command that the input names, the index of its argument in
    /// entry, and that argument's text with no whitespace at its ends.
    fn user_arg_in_entry(&self, ws: &Workspace) -> Option<(&Arc<UserCommand>, usize, String)> {
        let PaletteCommand::User(command) = self.command.as_ref()? else {
            return None;
        };
        let text = self.input.text(ws);
        let (_, _, tail) = split_head(&text, &self.availability, &self.commands)?;
        let typed = current_user_arg(tail, command.args.len())
            .1
            .trim()
            .to_owned();
        Some((command, self.typed_arg, typed))
    }

    /// The value source of the argument being typed when it drives an inline
    /// picker ([`ValueSource::Files`], [`ValueSource::Directories`],
    /// [`ValueSource::Buffers`], [`ValueSource::Themes`],
    /// [`ValueSource::Branches`], [`ValueSource::Revisions`], or
    /// [`ValueSource::Values`], e.g. `:o `,
    /// `:cd `, `:b `, `:SetTheme `, `:git log `, or `:auto-reload `), or `None`
    /// otherwise. Gates rendering the picker and routing selection keys to it.
    ///
    /// The argument of a user command picks from the list of its kind, as
    /// [`kind_source`] maps it.
    pub(crate) fn arg_source(&self) -> Option<ValueSource> {
        let source = match self.command.as_ref()? {
            PaletteCommand::Action(_) => self.arg_param()?.value_source,
            PaletteCommand::User(_) => kind_source(&self.user_arg()?.kind),
        };
        match source {
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
    /// alone, which is empty right after a space. A user command filters on the
    /// text of its argument in entry, as [`current_user_arg`] splits it.
    pub(crate) fn arg_tail(&self, ws: &Workspace) -> Option<String> {
        self.arg_source()?;
        let text = self.input.text(ws);
        let (command, _, tail) = split_head(&text, &self.availability, &self.commands)?;
        let tail = match &command {
            PaletteCommand::User(user) => current_user_arg(tail, user.args.len()).1,
            PaletteCommand::Action(_) if command.arg_count() > 1 => split_last_token(tail).1,
            PaletteCommand::Action(_) => tail,
        };
        Some(tail.to_string())
    }

    /// Install the inline picker for `source`. A file picker is fed by an
    /// already-spawned workspace `walk`. A buffer picker is fed by the fixed
    /// `all_paths` set with no walk. `scopes` is the file scope of a file
    /// argument, and `None` for every other source. No-op when a picker
    /// already exists, so the per-frame sync calls this unconditionally on
    /// entering argument mode.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn install_arg_picker(
        &mut self,
        ws: &mut Workspace,
        executor: Executor,
        source: ValueSource,
        git_root: PathBuf,
        walk: Option<(UnboundedReceiver<Vec<PathBuf>>, Task<()>)>,
        all_paths: Vec<PathBuf>,
        scopes: Option<FileScopes>,
    ) {
        if self.arg_picker.is_none() {
            self.arg_picker = Some(ArgPicker::new(
                ws, executor, source, git_root, walk, all_paths, scopes,
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
        self.resolved = resolve_typed(&text, self.scope, &self.availability, &self.commands);
        let parsed = parse_command(&text, &self.availability, &self.commands);
        self.typed_arg = parsed.as_ref().map_or(0, |(command, arg)| match command {
            PaletteCommand::Action(_) => arg_index(arg),
            PaletteCommand::User(command) => user_arg_index(arg, command.args.len()),
        });
        self.command = parsed.map(|(command, _)| command);
        self.group = input_group(&text, &self.availability, &self.commands);
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
                &self.commands,
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
        if let Some((command, arg)) = parse_command(&text, &self.availability, &self.commands) {
            let entry = match command {
                PaletteCommand::Action(entry) => entry,
                PaletteCommand::User(command) => return self.submit_user_command(ws, command, arg),
            };
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
                        let line =
                            format!("{} {tokens}", history_head(&PaletteCommand::Action(entry)));
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
                    let line = format!(
                        "{} {}",
                        history_head(&PaletteCommand::Action(entry)),
                        chosen
                    );
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
            && let Some(command) = resolve_command(bare, &self.availability, &self.commands)
        {
            return self.enter_bare(ws, command);
        }
        if walking {
            tracing::info!(target: "stoat::command", text = %text, "no command matched");
            return PaletteOutcome::None;
        }

        match self.filtered.get(self.selected).cloned() {
            Some(command) => self.enter_bare(ws, command),
            None => {
                tracing::info!(target: "stoat::command", text = %text, "no command matched");
                PaletteOutcome::None
            },
        }
    }

    /// The outcome of Enter on `command` with no argument typed.
    ///
    /// A command that runs with no argument runs. A user command does when
    /// each of its arguments has a default, and it runs with each argument
    /// empty. Any other command opens the entry of its argument, with its name
    /// and a space in the input.
    ///
    /// A `cli` command opens the entry of its tail. While the history walks,
    /// its recalled bare name runs the tool alone, as the run that recorded
    /// the line did.
    fn enter_bare(&mut self, ws: &mut Workspace, command: PaletteCommand) -> PaletteOutcome {
        let runs = match &command {
            PaletteCommand::Action(entry) => dispatches_bare(entry),
            PaletteCommand::User(user) => match user.run {
                RunSpec::Cli { .. } => ws.palette_history.is_walking(),
                RunSpec::Steps { .. } => user.args.iter().all(CommandArg::optional),
            },
        };
        if !runs {
            self.input
                .replace_text(ws, &format!("{} ", typed_head(&command)));
            return PaletteOutcome::None;
        }

        let line = history_head(&command);
        match command {
            PaletteCommand::Action(entry) => PaletteOutcome::Dispatch(entry, Vec::new(), line),
            PaletteCommand::User(user) => {
                let values = vec![String::new(); user.args.len()];
                PaletteOutcome::Run(user, values, line)
            },
        }
    }

    /// The outcome of Enter on the user command `command` with the typed
    /// `tail`.
    ///
    /// The command runs when the tail gives a value to each argument that has
    /// no default, and an argument with a default runs empty. Else the palette
    /// stays open, and the refusal goes to the log as the refusal of a built-in
    /// command does.
    ///
    /// The list of a `file` or `dir` argument in entry gives that argument its
    /// value, as [`Self::picked_user_value`] takes it. The history line holds
    /// the values that are not empty, so a recalled line runs on the picked
    /// path again, and its empty arguments take their defaults again.
    fn submit_user_command(
        &mut self,
        ws: &mut Workspace,
        command: Arc<UserCommand>,
        tail: &str,
    ) -> PaletteOutcome {
        let Some(values) = self.enter_values(ws, &command, tail) else {
            tracing::warn!(
                target: "stoat::command",
                command = %command.name,
                tail = %tail,
                "command arguments rejected"
            );
            return PaletteOutcome::None;
        };

        let line = iter::once(command.name.as_str())
            .chain(values.iter().map(String::as_str))
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        PaletteOutcome::Run(command, values, line)
    }

    /// The values that Enter runs `command` with for the typed `tail`, one for
    /// each argument, or `None` while an argument with no default has none.
    ///
    /// The tail gives the typed values, and the argument list gives the value
    /// of the argument in entry, as [`Self::picked_user_value`] takes it. An
    /// argument with a default stays empty. A `cli` command takes the whole
    /// tail, trimmed, as its one value, which may be empty.
    fn enter_values(
        &mut self,
        ws: &Workspace,
        command: &UserCommand,
        tail: &str,
    ) -> Option<Vec<String>> {
        if let RunSpec::Cli { .. } = command.run {
            return Some(vec![tail.trim().to_owned()]);
        }
        let count = command.args.len();
        let mut values: Vec<String> = split_user_args(tail, count)
            .into_iter()
            .map(str::to_owned)
            .collect();
        if let Some(picked) = self.picked_user_value(ws, command, tail) {
            match values.get_mut(user_arg_index(tail, count)) {
                Some(value) => *value = picked,
                None => values.push(picked),
            }
        }
        if command.args[values.len()..]
            .iter()
            .any(|arg| !arg.optional())
        {
            return None;
        }
        values.resize(count, String::new());
        Some(values)
    }

    /// Whether a submit of the parsed command starts a Cmd that a Run Block
    /// or a Run pane shows, which is true for the built-in `run` and for a
    /// user command under the `block` or the `run_pane` output.
    ///
    /// The key line of an argument stage names `C-Enter` while this holds,
    /// because that key runs such a Cmd as a Job. It reads the output alone,
    /// so a command of action steps under `block` holds it too.
    pub(crate) fn starts_cmd(&self) -> bool {
        match &self.command {
            Some(PaletteCommand::Action(entry)) => entry.def.kind() == ActionKind::Run,
            Some(PaletteCommand::User(user)) => {
                matches!(user.output, Output::Block | Output::RunPane)
            },
            None => false,
        }
    }

    /// The shell line that Enter runs for the input, or `None` for an input
    /// that runs no shell line.
    ///
    /// The key line of a pick list names it. The line reads the values of
    /// [`Self::enter_values`], as the run reads them. A `file` or `dir` value
    /// keeps its leading `~/`, which only the run expands.
    pub(crate) fn enter_line(&mut self, ws: &Workspace) -> Option<String> {
        let PaletteCommand::User(command) = self.command.clone()? else {
            return None;
        };
        let text = self.input.text(ws);
        let (_, _, tail) = split_head(&text, &self.availability, &self.commands)?;
        let values = self.enter_values(ws, &command, tail)?;
        run::run_line_preview(&command, &self.commands, &values)
    }

    /// The value that the argument list gives the argument of `command` that
    /// `tail` puts in entry, or `None` when the typed text stands.
    ///
    /// No list gives a value while the history walks. A `pick` argument takes
    /// the insert of its list's selected row, ranked against the typed text,
    /// and the typed text stands when no row matches. A `file` or `dir`
    /// argument takes a row of its picker. A typed path that starts with `/` or
    /// `~` takes the selected row of its directory browse. Any other text takes
    /// the selected row of the list. The value is the row's path as
    /// [`shell_path`] writes it for the workspace root.
    fn picked_user_value(
        &mut self,
        ws: &Workspace,
        command: &UserCommand,
        tail: &str,
    ) -> Option<String> {
        if ws.palette_history.is_walking() {
            return None;
        }
        let count = command.args.len();
        let index = user_arg_index(tail, count);
        let typed = current_user_arg(tail, count).1.trim();
        let kind = &command.args.get(index)?.kind;
        if let ArgKind::Pick(_) = kind {
            let key = (command.name.clone(), index);
            let pick = self.pick.as_mut().filter(|pick| pick.key == key)?;
            pick.list.refilter(typed);
            return pick.list.selected_row().map(|row| row.insert.clone());
        }

        let source = kind_source(kind);
        let picker = self
            .arg_picker
            .as_ref()
            .filter(|picker| picker.source() == source)?;
        let path = match typed.starts_with('/') || typed.starts_with('~') {
            true => picker.browse_selected_path()?,
            false => picker.selected_path()?,
        };
        Some(shell_path(path, &ws.git_root))
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

/// Whether `token` is one of the [`CONDITIONAL_COMMANDS`], ASCII case ignored,
/// whatever context is active.
pub(crate) fn is_conditional_token(token: &str) -> bool {
    CONDITIONAL_COMMANDS
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(token))
}

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
    commands: &CommandSet,
) -> Option<(PaletteCommand, &'a str)> {
    let (command, _, arg) = split_head(text, ctx, commands)?;
    (command.arg_count() > 0).then_some((command, arg))
}

/// Split palette input into the command its head names, the head, and the text
/// after the space that ends the head.
///
/// A command name such as `git log` holds spaces, so the head is the longest
/// run of leading tokens that names a command, as [`resolve_command`] resolves
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
    commands: &CommandSet,
) -> Option<(PaletteCommand, &'a str, &'a str)> {
    if let Some(command) =
        resolve_command(text, ctx, commands).filter(|command| command.arg_count() == 0)
    {
        return Some((command, text, ""));
    }
    text.rmatch_indices(' ').find_map(|(i, _)| {
        let head = &text[..i];
        resolve_command(head, ctx, commands).map(|command| (command, head, &text[i + 1..]))
    })
}

/// The command that the palette head `head` names.
///
/// A built-in command comes first, as [`resolve_head`] resolves one, and then
/// a user command of `commands`. A user command's name holds no space, so no
/// head under the `action` token names one.
pub(crate) fn resolve_command(
    head: &str,
    ctx: &Availability,
    commands: &CommandSet,
) -> Option<PaletteCommand> {
    resolve_head(head, ctx)
        .map(PaletteCommand::Action)
        .or_else(|| commands.get(head).cloned().map(PaletteCommand::User))
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

/// The head of a Run pane line, which the prompt resolves by the palette's
/// rule for a head.
///
/// It is the line's first word, or its first two words when the first is the
/// `action` token, because an internal action runs as `action <name>`. The
/// leading whitespace of the line is no part of it. A command name that holds
/// a space, such as `git log`, is no head here, so a line of git runs git.
pub(crate) fn line_head(text: &str) -> &str {
    let text = text.trim_start();
    let word_end = |from: usize| {
        text[from..]
            .find(char::is_whitespace)
            .map_or(text.len(), |at| from + at)
    };
    let first = word_end(0);
    if !text[..first].eq_ignore_ascii_case(ACTION_TOKEN) {
        return &text[..first];
    }
    let name = first + (text[first..].len() - text[first..].trim_start().len());
    match name < text.len() {
        true => &text[..word_end(name)],
        false => &text[..first],
    }
}

/// The command that a Run pane line names, with the values that its arguments
/// parse to, or the line of text that says why they do not parse.
///
/// `None` when the line's head, as [`line_head`] takes it, names no command,
/// or names a built-in command that takes no parameter while text follows the
/// head. Such a line runs as a shell line, so `pwd -P` runs the shell's `pwd`.
///
/// A built-in command parses one parameter from the whole rest of the line,
/// as the palette's argument entry does, and more parameters one token each,
/// as [`parse_args`] reads them. A user command takes one value for each
/// argument, as [`split_user_args`] splits the rest, and an empty one for an
/// absent argument. A `cli` command takes the rest as its tail. A required
/// parameter or an argument with no default that gets no value is an error,
/// and so is a value that does not parse. Each error names the head as typed,
/// as a shell names its program.
pub(crate) fn resolve_line(
    text: &str,
    ctx: &Availability,
    commands: &CommandSet,
) -> Option<Result<RunLine, String>> {
    let head = line_head(text);
    let command = Some(head)
        .filter(|head| !head.is_empty())
        .and_then(|head| resolve_command(head, ctx, commands))?;
    let rest = text.trim_start()[head.len()..].trim();
    let missing = |name: &str| format!("{head}: {name} is missing");

    let user = match command {
        PaletteCommand::Action(entry) => {
            let values = match entry.def.params() {
                [] if !rest.is_empty() => return None,
                [] => Ok(Vec::new()),
                [param] if rest.is_empty() => match param.required {
                    true => Err(missing(param.name)),
                    false => Ok(Vec::new()),
                },
                [param] => ParamValue::parse(param.kind, rest)
                    .map(|value| vec![value])
                    .map_err(|err| format!("{head}: {err}")),
                params => parse_args(params, rest, None)
                    .map(|(values, _)| values)
                    .map_err(|err| format!("{head}: {err}")),
            };
            return Some(values.map(|values| RunLine::Action(entry, values)));
        },
        PaletteCommand::User(user) => user,
    };

    let values = match user.run {
        RunSpec::Cli { .. } => vec![rest.to_owned()],
        RunSpec::Steps { .. } => {
            let count = user.args.len();
            let mut values: Vec<String> = split_user_args(rest, count)
                .into_iter()
                .map(str::to_owned)
                .collect();
            values.resize(count, String::new());
            let absent = user
                .args
                .iter()
                .zip(&values)
                .find(|(arg, value)| value.is_empty() && !arg.optional());
            if let Some((arg, _)) = absent {
                return Some(Err(missing(&arg.name)));
            }
            values
        },
    };
    Some(Ok(RunLine::User(user, values)))
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

/// The text that names `command` in the palette's input, which Enter and Tab
/// write.
///
/// A command and the `action` row read by their command name, and a user
/// command by its name. An internal action reads as the `action` token and its
/// kebab name, because only that form resolves it.
pub(crate) fn typed_head(command: &PaletteCommand) -> String {
    let entry = match command {
        PaletteCommand::Action(entry) => entry,
        PaletteCommand::User(command) => return command.name.clone(),
    };
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
fn resolve_typed(
    text: &str,
    scope: PaletteScope,
    ctx: &Availability,
    commands: &CommandSet,
) -> Option<ResolvedCommand> {
    if let Some(name) = action_query(text) {
        let (entry, head) = split_head(text, ctx, commands)
            .map(|(command, head, _)| (command, head))
            .or_else(|| {
                lookup_action(name.trim_end())
                    .map(|entry| (PaletteCommand::Action(entry), text.trim_end()))
            })
            .unwrap_or((
                PaletteCommand::Action(action_row()),
                &text[..ACTION_TOKEN.len()],
            ));
        return Some(ResolvedCommand {
            entry,
            head_chars: head.chars().count(),
            ghost: String::new(),
        });
    }

    let (entry, head) = named_command(text, scope, ctx, commands)?;
    let ghost = match &entry {
        PaletteCommand::Action(action) => ghost_of(head, &action.command_name),
        PaletteCommand::User(user) => ghost_of(head, &user.name),
    };
    Some(ResolvedCommand {
        entry,
        head_chars: head.chars().count(),
        ghost,
    })
}

/// The command that the typed head of palette input `text` names at the root,
/// and the head, or `None` when no head names a command.
///
/// A head that [`split_head`] finds names its command first. Else the whole
/// text less its trailing whitespace names a command, as [`resolve_command`]
/// resolves one. Else a text with no whitespace names the one command that
/// `scope` offers whose name starts with it, ASCII case ignored. The user
/// commands that `scope` lists take part, and in a scope that lists the
/// registry the `action` row takes part as one more command. So `quit-al`
/// names `quit-all`, `act` names the `action` row, and `wri` names none of the
/// commands that start with it.
fn named_command<'a>(
    text: &'a str,
    scope: PaletteScope,
    ctx: &Availability,
    commands: &CommandSet,
) -> Option<(PaletteCommand, &'a str)> {
    if let Some((command, head, _)) = split_head(text, ctx, commands) {
        return Some((command, head));
    }

    let whole = text.trim_end();
    if let Some(command) = resolve_command(whole, ctx, commands) {
        return Some((command, whole));
    }

    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    let starts = |command: &PaletteCommand| {
        command
            .name()
            .get(..text.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(text))
    };
    let mut named = registry::all()
        .filter(|entry| entry.is_command() && offers(entry, scope, ctx))
        .chain(iter::once(action_row()).filter(|_| scope.lists_registry()))
        .map(PaletteCommand::Action)
        .chain(
            commands
                .iter()
                .filter(|command| scope.lists(command))
                .cloned()
                .map(PaletteCommand::User),
        )
        .filter(starts);
    match (named.next(), named.next()) {
        (Some(command), None) => Some((command, text)),
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

/// The values that a user command with `count` arguments takes from the typed
/// `tail`, each trimmed.
///
/// One argument takes the whole tail. With two or more, each argument but the
/// last takes one word, and the last takes the rest, so a message keeps its
/// spaces. No value is empty, so a tail with too few words gives fewer than
/// `count` values.
pub(crate) fn split_user_args(tail: &str, count: usize) -> Vec<&str> {
    let mut values = Vec::with_capacity(count);
    let mut rest = tail.trim();
    while values.len() + 1 < count && !rest.is_empty() {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        values.push(&rest[..end]);
        rest = rest[end..].trim_start();
    }
    if count > 0 && !rest.is_empty() {
        values.push(rest);
    }
    values
}

/// Which argument of a user command with `count` arguments the text at the
/// end of `tail` fills, counting from zero.
///
/// The last argument takes the rest of the tail, so the count stops there.
fn user_arg_index(tail: &str, count: usize) -> usize {
    arg_index(tail).min(count.saturating_sub(1))
}

/// `tail` split before the argument that it puts in entry, for a user command
/// with `count` arguments. The parts are the text kept ahead of that argument
/// and the argument's own text.
///
/// An argument before the last is one word, and the last takes the rest of
/// the tail, as [`split_user_args`] reads them. The kept text holds its
/// whitespace byte for byte, so a completion written after it keeps the
/// spacing that the user typed.
pub(crate) fn current_user_arg(tail: &str, count: usize) -> (&str, &str) {
    let mut start = tail.len() - tail.trim_start().len();
    for _ in 0..user_arg_index(tail, count) {
        let word = tail[start..]
            .find(char::is_whitespace)
            .map_or(tail.len(), |end| start + end);
        start = tail.len() - tail[word..].trim_start().len();
    }
    tail.split_at(start)
}

/// `path` as a value of a user command's shell step, which runs in the
/// workspace root `root`. A path under the root is relative to it, and any
/// other path stays whole.
pub(crate) fn shell_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// The value source of the list that a user command's argument of kind `kind`
/// picks from. A `text` argument picks from no list, and a `pick` argument
/// from the palette's own [`PickState`] list.
fn kind_source(kind: &ArgKind) -> ValueSource {
    match kind {
        ArgKind::Text { .. } | ArgKind::Pick(_) => ValueSource::None,
        ArgKind::File => ValueSource::Files,
        ArgKind::Dir => ValueSource::Directories,
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

/// The list that the argument entry of a `cli` command whose help gives
/// `spec` shows for the typed `tail`, or `None` while the word in entry is
/// free text.
///
/// The word in entry is the tail's last word, as [`split_last_token`] splits
/// it. A word that starts with `-` lists the flags, and the first word that is
/// no flag lists the subcommands. Each list shows only while the spec holds
/// rows for it, so a tool with an empty spec shows no list.
pub(crate) fn cli_list_kind(spec: &CliSpec, tail: &str) -> Option<CliListKind> {
    let (kept, word) = split_last_token(tail);
    if word.starts_with('-') {
        return (!spec.flags.is_empty()).then_some(CliListKind::Flags);
    }
    let first = kept.split_whitespace().all(|kept| kept.starts_with('-'));
    (first && !spec.subcommands.is_empty()).then_some(CliListKind::Subcommands)
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
fn input_group(text: &str, ctx: &Availability, commands: &CommandSet) -> Option<&'static str> {
    if action_query(text).is_some() || parse_command(text, ctx, commands).is_some() {
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

/// The head to record for `command` in palette history, its first alias when it
/// has one, else its [`typed_head`]. A user command records its name.
///
/// Each re-resolves through [`parse_command`], so a recalled `head [arg]` line
/// runs again. A command without an alias records its command name. An
/// internal action records the `action` token and its kebab name, because the
/// root resolves no internal action. The alias is preferred because it reads
/// like the short form a user types (`cd`, not `set-cwd`).
pub(crate) fn history_head(command: &PaletteCommand) -> String {
    let alias = match command {
        PaletteCommand::Action(entry) => entry.def.aliases().first(),
        PaletteCommand::User(_) => None,
    };
    match alias {
        Some(alias) => (*alias).to_owned(),
        None => typed_head(command),
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
/// The Active scope hides the actions that do not apply to the current
/// workspace state. So a scope flip changes how many commands the palette
/// offers, not just their order. The Config and Cli scopes offer no registered
/// action, as [`PaletteScope::lists_registry`] says.
fn offers(
    entry: &registry::RegistryEntry,
    scope: PaletteScope,
    availability: &Availability,
) -> bool {
    scope.lists_registry()
        && entry.def.palette_visible()
        && (scope != PaletteScope::Active || action_is_available(entry.def.kind(), availability))
}

/// How many commands and how many internal actions `scope` offers before any
/// query narrows them. The commands count the user commands of `commands`
/// that `scope` lists.
fn scope_offering(
    scope: PaletteScope,
    availability: &Availability,
    commands: &CommandSet,
) -> (usize, usize) {
    let users = commands
        .iter()
        .filter(|command| scope.lists(command))
        .count();
    registry::all()
        .filter(|entry| offers(entry, scope, availability))
        .fold((users, 0), |(commands, actions), entry| {
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
/// At the root the list holds the commands, the user commands of `commands`
/// that `scope` lists among them. With a query they rank with the built-in
/// commands by name, and with none they follow the built-in commands in
/// declaration order. Under `group` only the group's commands appear, and the
/// text after the group token narrows them.
///
/// With no group the `action` row ends the list when the query holds no atom
/// or matches the row's name. The row leads the list when the query is the
/// word `action`. The Config and Cli scopes list no `action` row. Under the
/// `action` token the list holds the internal actions, which the text after
/// the token narrows by their kebab names. Each match position indexes the
/// whole command name either way. Neither a group nor the `action` list holds
/// a user command.
#[allow(clippy::too_many_arguments)]
pub(crate) fn refilter(
    input: &str,
    group: Option<&str>,
    scope: PaletteScope,
    availability: &Availability,
    commands: &CommandSet,
    filtered: &mut Vec<PaletteCommand>,
    match_indices: &mut Vec<Vec<u32>>,
    selected: &mut usize,
) {
    let actions = action_query(input);
    let users: Vec<Arc<UserCommand>> = match (actions, group) {
        (None, None) => commands
            .iter()
            .filter(|command| scope.lists(command))
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
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
    rank_entries(query, offset, visible, &users, filtered, match_indices);

    // Pin an exact name or alias match to the top so the displayed list agrees
    // with what Enter dispatches. `w` (SaveBuffer) and `o` (OpenFile) resolve by
    // alias, which the name-only fuzzy rank never surfaces first -- or at all,
    // when the name lacks the typed characters.
    let needle = actions.unwrap_or(input).trim();
    let pinned = match actions {
        Some(_) => lookup_action(needle),
        None => lookup_root(needle),
    };
    let pinned = pinned
        .filter(|pinned| offers(pinned, scope, availability))
        .map(PaletteCommand::Action)
        .or_else(|| {
            let user = users
                .iter()
                .find(|command| command.name.eq_ignore_ascii_case(needle));
            user.cloned().map(PaletteCommand::User)
        });
    if let Some(pinned) = pinned {
        let indices = match filtered
            .iter()
            .position(|command| same_command(command, &pinned))
        {
            Some(pos) => {
                filtered.remove(pos);
                match_indices.remove(pos)
            },
            None if pinned.name().eq_ignore_ascii_case(needle) => {
                (0..pinned.name().chars().count() as u32).collect()
            },
            None => Vec::new(),
        };
        filtered.insert(0, pinned);
        match_indices.insert(0, indices);
    }

    if actions.is_none() && group.is_none() && scope.lists_registry() {
        let row = PaletteCommand::Action(action_row());
        if needle.eq_ignore_ascii_case(ACTION_TOKEN) {
            filtered.insert(0, row);
            match_indices.insert(0, (0..ACTION_TOKEN.len() as u32).collect());
        } else {
            let matched = match fuzzy::match_and_rank(query, iter::once(((), ACTION_TOKEN))) {
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

/// Whether `a` and `b` name one command, the same registry entry or the same
/// user command.
fn same_command(a: &PaletteCommand, b: &PaletteCommand) -> bool {
    match (a, b) {
        (PaletteCommand::Action(a), PaletteCommand::Action(b)) => ptr::eq(*a, *b),
        (PaletteCommand::User(a), PaletteCommand::User(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

/// Rank the registered `visible` and the user commands `users` against
/// `query` into `filtered` and `match_indices`, each name matched past its
/// first `offset` bytes.
///
/// A query with no atom lists every entry by priority and then by name, and
/// then the user commands in their order. A match ranks by its score, then by
/// priority, then by name. A user command ranks at the `Normal` priority,
/// because it has no priority of its own.
fn rank_entries(
    query: &str,
    offset: usize,
    visible: Vec<&'static registry::RegistryEntry>,
    users: &[Arc<UserCommand>],
    filtered: &mut Vec<PaletteCommand>,
    match_indices: &mut Vec<Vec<u32>>,
) {
    let items = visible
        .iter()
        .map(|&entry| (PaletteCommand::Action(entry), &entry.command_name[offset..]))
        .chain(users.iter().map(|command| {
            let name = command.name.get(offset..).unwrap_or("");
            (PaletteCommand::User(command.clone()), name)
        }));
    let Some(mut matches) = fuzzy::match_and_rank(query, items) else {
        let mut all = visible;
        all.sort_by_key(|e| (e.def.priority().ord(), e.command_name.as_str()));
        for entry in all {
            filtered.push(PaletteCommand::Action(entry));
            match_indices.push(Vec::new());
        }
        for command in users {
            filtered.push(PaletteCommand::User(command.clone()));
            match_indices.push(Vec::new());
        }
        return;
    };

    let priority = |command: &PaletteCommand| match command {
        PaletteCommand::Action(entry) => entry.def.priority().ord(),
        PaletteCommand::User(_) => ActionPriority::Normal.ord(),
    };
    matches.sort_by(|a, b| {
        b.score.cmp(&a.score).then_with(|| {
            priority(&a.item)
                .cmp(&priority(&b.item))
                .then_with(|| a.item.name().cmp(b.item.name()))
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
        app::UpdateEffect,
        badge::BadgeSource,
        buffer_registry::AutoReloadMode,
        cli_spec::{self, CARGO_HELP, RG_HELP},
        host::{FakeFsOp, ShellOutput},
        input_history::InputHistory,
        picker::{PreviewSource, PREVIEW_BYTE_LIMIT, PREVIEW_DIR_LIMIT},
        test_harness::{editor, TestHarness},
        user_command::run::CliSpecState,
    };
    use crossterm::event::Event;
    use ratatui::{
        buffer::Buffer,
        style::{Color, Modifier},
    };
    use std::{collections::BTreeMap, time::Duration};
    use stoat_action::{Action, ParamKind};

    /// A Run pane line resolves by its head. A command that takes no parameter
    /// resolves only with nothing after it, and a required parameter with no
    /// value is an error that names the head.
    #[test]
    fn resolve_line_reads_a_command_and_its_arguments() {
        let (ctx, commands) = (Availability::default(), CommandSet::default());
        let read = |text: &str| {
            resolve_line(text, &ctx, &commands).map(|line| {
                line.map(|line| match line {
                    RunLine::Action(entry, values) => (entry.def.name(), values),
                    RunLine::User(command, _) => panic!("`{}` is a user command", command.name),
                })
            })
        };
        assert_eq!(
            [
                "cd sub",
                "pwd",
                "pwd -P",
                "nosuchword x",
                "cd",
                "review a b c",
            ]
            .map(read),
            [
                Some(Ok(("SetCwd", vec![ParamValue::String("sub".to_owned())]))),
                Some(Ok(("ShowCwd", Vec::new()))),
                None,
                None,
                Some(Err("cd: path is missing".to_owned())),
                Some(Err("review: no parameter takes `c`".to_owned())),
            ],
        );
    }

    /// Each refusal of the tokens of a command with more than one parameter
    /// reads as one line, which the Run pane prompt's status names its head
    /// before.
    #[test]
    fn args_errors_read_as_one_line_each() {
        let errors = [
            ArgsError::Invalid {
                param: "count",
                arg: "x".to_owned(),
            },
            ArgsError::Missing { param: "rev" },
            ArgsError::Extra {
                arg: "y".to_owned(),
            },
        ];
        assert_eq!(
            errors.map(|err| err.to_string()),
            [
                "cannot parse `x` as count",
                "rev is missing",
                "no parameter takes `y`"
            ],
        );
    }

    /// A line's head is its first word, past leading whitespace, or the
    /// `action` token with the name after it.
    #[test]
    fn line_head_takes_the_action_token_with_its_name() {
        assert_eq!(
            ["cd sub", "action focus-left x", "", "  git log", "action",].map(line_head),
            ["cd", "action focus-left", "", "git", "action"],
        );
    }

    /// The def name of the action that `command` names. These tests resolve
    /// over no user command, so a user command fails the test.
    fn action_name(command: PaletteCommand) -> &'static str {
        match command {
            PaletteCommand::Action(entry) => entry.def.name(),
            PaletteCommand::User(command) => panic!("`{}` is a user command", command.name),
        }
    }

    /// The registry entry that `command` names. These tests list no user
    /// command, so a user command fails the test.
    fn entry_of(command: &PaletteCommand) -> &'static registry::RegistryEntry {
        match command {
            PaletteCommand::Action(entry) => entry,
            PaletteCommand::User(command) => panic!("`{}` is a user command", command.name),
        }
    }

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

    /// A harness over `a.rs` and `b.rs` in a fake repository in which `b.rs`
    /// is modified.
    fn modified_harness() -> TestHarness {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(&mut h, &[("a.rs", "v1\n"), ("b.rs", "v1\n")]);
        h.fake_git()
            .add_repo(&root)
            .with_fs(h.fake_fs())
            .head_file("a.rs", "v1\n")
            .modified("b.rs", "v1\n", "v2\n");
        h
    }

    /// The file scope of the argument list, and the files it lists, relative
    /// to the workspace root and sorted.
    fn file_scope(h: &TestHarness) -> (FinderScope, Vec<String>) {
        let picker = arg_picker(h);
        let scope = picker.scopes.as_ref().expect("a file scope").scope.clone();
        let list = &picker.active_core_ref().picklist;
        let mut files: Vec<String> = list
            .filtered
            .iter()
            .map(|&i| {
                let path = &list.base[i];
                let relative = path.strip_prefix(&picker.core.git_root).unwrap_or(path);
                relative.display().to_string()
            })
            .collect();
        files.sort();
        (scope, files)
    }

    /// The Cmd Line row of the argument list as the frame paints it, with its
    /// trailing blanks cut.
    fn arg_header(h: &mut TestHarness) -> String {
        let _ = h.snapshot();
        let inner = crate::test_fixture::palette_arg_layout(h).inner;
        let buf = h.rendered_buffer();
        let row: String = (inner.x..inner.right())
            .map(|x| buf[(x, inner.y)].symbol())
            .collect();
        row.trim_end().to_owned()
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
            input_group(text, availability, &CommandSet::default()),
            scope,
            availability,
            &CommandSet::default(),
            &mut filtered,
            &mut match_indices,
            &mut selected,
        );
        filtered.iter().map(entry_of).collect()
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
            let group = input_group("git l", &ctx, &CommandSet::default());
            refilter(
                "git l",
                group,
                PaletteScope::All,
                &ctx,
                &CommandSet::default(),
                &mut filtered,
                &mut match_indices,
                &mut selected,
            );
            filtered
                .iter()
                .position(|command| command.name() == "git log")
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
        let split = |text| {
            split_head(text, &ctx, &CommandSet::default())
                .map(|(command, head, tail)| (action_name(command), head, tail))
        };
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

        let head = |name| {
            history_head(&PaletteCommand::Action(
                registry::lookup(name).expect("a registered action"),
            ))
        };
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

    /// The user command `greet`, which takes one argument.
    const GREET: &str =
        r#"command greet { arg who = text; run = shell("echo hello {who}"); output = notify; }"#;

    /// The user commands that `source` declares, over no embedded command.
    fn user_commands(source: &str) -> CommandSet {
        let config = |source| stoat_config::parse(source).0.expect("the config parses");
        let (set, errors) = CommandSet::build((&config(""), ""), Some((&config(source), source)));
        assert_eq!(errors, Vec::new(), "the commands load");
        set
    }

    #[test]
    fn split_user_args_gives_the_last_argument_the_rest() {
        assert_eq!(
            [
                split_user_args("a  b c", 2),
                split_user_args("a", 2),
                split_user_args(" x y ", 1),
            ],
            [vec!["a", "b c"], vec!["a"], vec!["x y"]],
        );
    }

    #[test]
    fn current_user_arg_splits_before_the_argument_in_entry() {
        assert_eq!(
            [
                current_user_arg("src/ma", 1),
                current_user_arg("my file", 1),
                current_user_arg("a", 2),
                current_user_arg("a  b c", 2),
                current_user_arg("a b ", 3),
                current_user_arg(" x", 1),
            ],
            [
                ("", "src/ma"),
                ("", "my file"),
                ("", "a"),
                ("a  ", "b c"),
                ("a b ", ""),
                (" ", "x"),
            ],
        );
    }

    /// Enter on the bare name of a user command that takes an argument opens
    /// the entry of that argument.
    #[test]
    fn a_typed_user_command_name_enters_argument_entry() {
        let mut h = Stoat::test();
        h.stoat.reload_user_config(GREET);
        h.type_text(":greet");
        h.type_keys("enter");

        let ws = h.stoat.active_workspace();
        let input = h
            .stoat
            .command_palette
            .as_ref()
            .map(|palette| palette.input.text(ws));
        assert_eq!(input.as_deref(), Some("greet "));
    }

    /// The body of a user command names its usage and the argument that the
    /// typed text fills. The last argument takes the rest of the words.
    #[test]
    fn a_user_command_shows_its_usage_and_the_argument_in_entry() {
        let mut h = TestHarness::with_size(120, 30);
        h.stoat.reload_user_config(
            r#"command pair { arg first = text; arg rest = text; run = shell("echo {first} {rest}"); output = notify; }"#,
        );
        let entry = |h: &mut TestHarness| {
            let _ = h.snapshot();
            let text = h.rendered_text();
            let lines: Vec<&str> = text.lines().collect();
            let usage = lines
                .iter()
                .position(|line| line.contains("pair <first> <rest>"));
            let arg = usage
                .and_then(|usage| lines.get(usage + 1))
                .map(|line| line.trim_matches(|c: char| !c.is_alphanumeric()).to_owned());
            (arg, text)
        };

        h.type_text(":pair ");
        let (first, _) = entry(&mut h);
        h.type_text("a b c");
        let (rest, text) = entry(&mut h);
        assert_eq!(
            (first, rest),
            (Some("first".to_owned()), Some("rest".to_owned())),
            "{text}",
        );
    }

    /// A user command's name resolves as a command token with no ghost, and no
    /// head under the `action` token names one.
    #[test]
    fn a_user_command_head_resolves_as_a_command_token() {
        let commands = user_commands(GREET);
        let ctx = Availability::default();
        let resolved =
            resolve_typed("greet x", PaletteScope::All, &ctx, &commands).expect("a command token");
        let user = match &resolved.entry {
            PaletteCommand::User(command) => Some(command.name.as_str()),
            PaletteCommand::Action(_) => None,
        };
        assert_eq!(
            (
                user,
                resolved.head_chars,
                resolved.ghost.as_str(),
                resolve_command("action greet", &ctx, &commands).is_none(),
            ),
            (Some("greet"), 5, "", true),
        );
    }

    /// The user commands `greet`, on line 1 with the argument `who` and the
    /// doc `say hello`, and `ping`, on line 2 with no argument.
    const GREET_AND_PING: &str = concat!(
        "command greet { doc = \"say hello\"; arg who = text; ",
        "run = shell(\"echo hello {who}\"); output = notify; }\n",
        "command ping { run = Notify(\"pong\"); }",
    );

    /// A `width` by `height` harness with [`GREET_AND_PING`] loaded and the
    /// palette open.
    fn user_palette(width: u16, height: u16) -> TestHarness {
        let mut h = TestHarness::with_size(width, height);
        h.stoat.reload_user_config(GREET_AND_PING);
        h.type_text(":");
        h
    }

    /// The names of the rows that the open palette lists.
    fn row_names(h: &TestHarness) -> Vec<String> {
        h.stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .filtered
            .iter()
            .map(|command| command.name().to_owned())
            .collect()
    }

    /// Select the row of the open palette named `name`.
    fn select_row(h: &mut TestHarness, name: &str) {
        let palette = h.stoat.command_palette.as_mut().expect("palette open");
        palette.selected = palette
            .filtered
            .iter()
            .position(|command| command.name() == name)
            .expect("the row is listed");
    }

    #[test]
    fn the_root_list_holds_user_commands_before_the_action_row() {
        let names = row_names(&user_palette(80, 24));
        assert_eq!(names[names.len() - 3..], ["greet", "ping", "action"]);
    }

    /// A user command ranks as a Normal action does. Each name below matches
    /// `di` at its start, so the priority orders them, and then the name does.
    #[test]
    fn a_query_ranks_a_user_command_with_the_built_in_commands() {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat
            .reload_user_config("command dia { run = Notify(\"x\"); }");
        h.type_text(":di");
        assert_eq!(
            row_names(&h)[..5],
            [
                "diagnostics",
                "diff-changes",
                "diff-files",
                "dia",
                "diff-against-head"
            ]
        );
    }

    /// The exact name of a user command pins its row first, and the list
    /// holds the row once. The `terminal` command matches `ter` as well and
    /// takes the tie on its Common priority.
    #[test]
    fn an_exact_user_command_name_pins_its_row_first() {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat
            .reload_user_config("command ter { run = Notify(\"x\"); }");
        h.type_text(":ter");
        let names = row_names(&h);
        let rows = names.iter().filter(|name| *name == "ter").count();
        assert_eq!(
            (names[0].as_str(), names[1].as_str(), rows),
            ("ter", "terminal", 1)
        );
    }

    /// Two `S-Tab` presses reach the Config scope, which lists the user
    /// commands alone and counts them in the header. A third returns to the
    /// applicable scope.
    #[test]
    fn the_config_scope_joins_the_cycle_and_lists_the_user_commands_alone() {
        let mut h = user_palette(120, 30);
        h.type_keys("backtab");
        h.type_keys("backtab");
        let scope = |h: &TestHarness| {
            h.stoat
                .command_palette
                .as_ref()
                .expect("palette open")
                .scope
        };
        let config = (scope(&h), row_names(&h));
        let _ = h.snapshot();
        let header = h
            .rendered_text()
            .lines()
            .find(|line| line.contains('\u{276f}'))
            .unwrap_or_default()
            .to_owned();
        h.type_keys("backtab");

        assert_eq!(
            (config, header.contains("2 commands\u{2502}"), scope(&h)),
            (
                (
                    PaletteScope::Config,
                    vec!["greet".to_owned(), "ping".to_owned()]
                ),
                true,
                PaletteScope::Active,
            ),
            "the header reads {header:?}",
        );
    }

    /// The list under the Config scope sizes to the user commands alone, with
    /// no row for an `action` row that the scope does not list.
    #[test]
    fn the_config_scope_sizes_its_list_to_the_user_commands() {
        let source: String = (0..12)
            .map(|at| format!("command c{at} {{ run = Notify(\"x\"); }}\n"))
            .collect();
        let mut h = TestHarness::with_size(120, 60);
        h.stoat.reload_user_config(&source);
        h.type_text(":");
        h.type_keys("backtab");
        h.type_keys("backtab");
        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        assert_eq!(
            (
                palette.scope,
                palette.filtered.len(),
                palette.list_rows_hint()
            ),
            (PaletteScope::Config, 12, 12),
        );
    }

    /// The scope hint names the Config scope while the set holds a user
    /// command, and names the All scope alone while it holds none.
    #[test]
    fn the_scope_hint_names_config_while_the_set_holds_a_command() {
        let hint = |source: &str| {
            let mut h = TestHarness::with_size(160, 50);
            h.stoat.reload_user_config(source);
            h.type_text(":");
            let _ = h.snapshot();
            let (text, _) = key_line(&h);
            ["S-Tab all / config", "S-Tab all"].map(|hint| text.contains(hint))
        };
        assert_eq!(
            [hint(GREET_AND_PING), hint("")],
            [[true, true], [false, true]],
        );
    }

    /// With a block command and a `cli` command, the Cli scope follows the
    /// Config scope in the cycle. Each lists and counts its own kind of user
    /// command alone. With a `cli` command alone, the cycle holds no Config
    /// scope.
    #[test]
    fn the_cli_scope_joins_the_cycle_and_lists_the_wrap_commands_alone() {
        let both = user_commands(&format!("{GREET}\ncommand rg = cli(\"rg\");"));
        let ctx = Availability::default();
        let listed = |scope| {
            let (mut filtered, mut indices, mut selected) = (Vec::new(), Vec::new(), 0);
            refilter(
                "",
                None,
                scope,
                &ctx,
                &both,
                &mut filtered,
                &mut indices,
                &mut selected,
            );
            let names: Vec<String> = filtered
                .iter()
                .map(|command| command.name().to_owned())
                .collect();
            (names, scope_offering(scope, &ctx, &both))
        };
        let cycle = PaletteScope::cycle(&both);
        assert_eq!(
            (
                PaletteScope::Active.next_names(&cycle),
                cycle,
                listed(PaletteScope::Config),
                listed(PaletteScope::Cli),
                PaletteScope::cycle(&user_commands("command rg = cli(\"rg\");")),
            ),
            (
                "all / config / cli".to_owned(),
                vec![
                    PaletteScope::Active,
                    PaletteScope::All,
                    PaletteScope::Config,
                    PaletteScope::Cli
                ],
                (vec!["greet".to_owned()], (1, 0)),
                (vec!["rg".to_owned()], (1, 0)),
                vec![PaletteScope::Active, PaletteScope::All, PaletteScope::Cli],
            ),
        );
    }

    /// Enter in History runs a recalled bare `cli` name with an empty tail,
    /// where a typed bare name opens the entry of its tail.
    #[test]
    fn enter_in_history_runs_a_recalled_bare_wrap_command() {
        let mut h = Stoat::test();
        h.stoat
            .reload_user_config(r#"command st = cli("git status");"#);
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["st".to_owned()]);
        h.type_text(":");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::PaletteUp);

        let mut palette = h.stoat.command_palette.take().expect("palette open");
        let run = match palette.handle_submit(h.stoat.active_workspace_mut()) {
            PaletteOutcome::Run(command, values, line) => {
                Some((command.name.clone(), values, line))
            },
            _ => None,
        };
        assert_eq!(run, Some(("st".to_owned(), Vec::new(), "st".to_owned())));
    }

    /// The user command `rg`, which wraps ripgrep.
    const RG: &str = r#"command rg = cli("rg");"#;

    /// The root list loads no help. Argument entry loads it, and a later
    /// palette loads it no more.
    #[test]
    fn the_spec_loads_when_argument_entry_opens_and_once() {
        let mut h = Stoat::test();
        h.stoat.reload_user_config(RG);
        type_settled(&mut h, ":");
        let at_root = shell_lines(&h);
        type_settled(&mut h, "rg ");
        let in_entry = shell_lines(&h);
        h.type_keys("escape");
        type_settled(&mut h, ":rg ");

        let loads = vec!["rg --help".to_owned(), "rg --version".to_owned()];
        assert_eq!(
            (at_root, in_entry, shell_lines(&h)),
            (Vec::new(), loads.clone(), loads),
        );
    }

    #[test]
    fn a_config_reload_drops_the_loaded_specs() {
        let mut h = Stoat::test();
        h.stoat.reload_user_config(RG);
        type_settled(&mut h, ":rg ");
        let loaded = matches!(h.stoat.cli_specs.get("rg"), Some(CliSpecState::Ready(_)));
        h.stoat.reload_user_config(RG);
        assert_eq!((loaded, h.stoat.cli_specs.len()), (true, 0));
    }

    /// The note after the Source Tag of a tool that the shell does not find
    /// reads `<tool>: not found`, and the command still runs its line as
    /// typed.
    #[test]
    fn a_missing_tool_reads_not_found_and_still_runs() {
        let mut h = TestHarness::with_size(120, 30);
        h.stoat
            .reload_user_config(r#"command nope = cli("nope", output: notify);"#);
        h.fake_shell.set_response(
            "nope --help",
            ShellOutput {
                stdout: Vec::new(),
                stderr: b"sh: nope: not found\n".to_vec(),
                exit_code: 127,
            },
        );
        type_settled(&mut h, ":nope ");
        h.type_keys("backspace");
        let _ = h.snapshot();
        let doc = h.rendered_text();
        type_settled(&mut h, " x");
        enter_settled(&mut h);

        assert_eq!(
            (doc.contains("nope: not found"), shell_lines(&h)),
            (true, vec!["nope --help".to_owned(), "nope x".to_owned()]),
            "the palette reads {doc}",
        );
    }

    /// A free word after a subcommand lists nothing, and a tool with no
    /// subcommand lists none for its first word.
    #[test]
    fn cli_list_kind_follows_the_word_in_entry() {
        let cargo = cli_spec::parse_help(CARGO_HELP);
        let rg = cli_spec::parse_help(RG_HELP);
        let subcommands = Some(CliListKind::Subcommands);
        let flags = Some(CliListKind::Flags);
        assert_eq!(
            (
                ["", "t", "test -", "-", "test foo"].map(|tail| cli_list_kind(&cargo, tail)),
                cli_list_kind(&rg, ""),
            ),
            ([subcommands, subcommands, flags, flags, None], None),
        );
    }

    /// A harness 160 by 50 with the `cli` command that `source` declares, whose
    /// help run `help_line` prints `help`.
    fn cli_harness(source: &str, help_line: &str, help: &str) -> TestHarness {
        let mut h = TestHarness::with_size(160, 50);
        h.fake_shell.set_response(
            help_line,
            ShellOutput {
                stdout: help.as_bytes().to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        h.stoat.reload_user_config(source);
        h
    }

    /// The `cli` command `rg` over ripgrep's help.
    fn rg_harness() -> TestHarness {
        cli_harness(
            r#"command rg = cli("rg", output: notify);"#,
            "rg --help",
            RG_HELP,
        )
    }

    /// The `cli` command `cargo` over cargo's help.
    fn cargo_harness() -> TestHarness {
        cli_harness(
            r#"command cargo = cli("cargo");"#,
            "cargo --help",
            CARGO_HELP,
        )
    }

    /// The labels of the rows that the open palette's `cli` list shows, in
    /// order, none while no such list shows.
    fn cli_rows(h: &TestHarness) -> Vec<String> {
        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        palette
            .cli_list
            .as_ref()
            .map(|(_, _, list)| {
                list.filtered
                    .iter()
                    .map(|&index| list.rows[index].label.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The word `-` lists every flag, and the status counts the help's flags.
    /// More of the word narrows the list by the flags' labels.
    #[test]
    fn a_dash_word_lists_the_flags() {
        let mut h = rg_harness();
        type_settled(&mut h, ":rg -t rust -");
        let listed = cli_rows(&h).len();
        let header = header_line(&h);
        type_settled(&mut h, "-ig");

        assert_eq!(
            (
                listed,
                header.contains("flags \u{b7} 5 from --help\u{2502}"),
                cli_rows(&h),
            ),
            (5, true, vec!["-i, --ignore-case".to_owned()]),
            "the header reads {header:?}",
        );
    }

    #[test]
    fn the_first_word_lists_the_subcommands() {
        let mut h = cargo_harness();
        type_settled(&mut h, ":cargo t");
        let header = header_line(&h);
        assert_eq!(
            (
                cli_rows(&h),
                header.contains("cli subcommands \u{b7} 5 from --help\u{2502}"),
            ),
            (vec!["t".to_owned(), "test".to_owned()], true),
            "the header reads {header:?}",
        );
    }

    /// Tab writes a flag's long form, the short form of a flag with no long
    /// form, or a subcommand's name, and a space, in place of the word.
    #[test]
    fn tab_writes_the_long_form_and_a_space() {
        let completed = |mut h: TestHarness, typed: &str| {
            type_settled(&mut h, typed);
            h.type_keys("tab");
            palette_text(&h)
        };
        assert_eq!(
            [
                completed(rg_harness(), ":rg -i"),
                completed(cargo_harness(), ":cargo -C"),
                completed(cargo_harness(), ":cargo te"),
            ],
            ["rg --ignore-case ", "cargo -C ", "cargo test "],
        );
    }

    #[test]
    fn enter_runs_the_line_as_typed_whatever_row_is_selected() {
        let mut h = rg_harness();
        type_settled(&mut h, ":rg -");
        h.type_keys("down");
        let selected = h
            .stoat
            .command_palette
            .as_ref()
            .and_then(|palette| palette.cli_list.as_ref())
            .map(|(_, _, list)| list.selected);
        enter_settled(&mut h);
        assert_eq!(
            (selected, shell_lines(&h)),
            (
                Some(1),
                vec![
                    "rg --help".to_owned(),
                    "rg --version".to_owned(),
                    "rg -".to_owned()
                ],
            ),
        );
    }

    #[test]
    fn the_wrap_key_line_names_run_complete_flag_and_history() {
        let mut h = rg_harness();
        type_settled(&mut h, ":rg -");
        let (text, _) = key_line(&h);
        let line = text.trim_end();
        assert_eq!(
            (
                line.contains("Enter run  Tab complete flag  \u{2191} history"),
                line.ends_with("cli command \u{b7} auto-wrapped"),
            ),
            (true, true),
            "the key line reads {text:?}",
        );
    }

    /// A `cli` command under the default `block` output names `C-Enter` after
    /// its `Enter` pair, because its run starts a Cmd.
    #[test]
    fn a_block_cli_key_line_names_run_as_job() {
        let mut h = cargo_harness();
        type_settled(&mut h, ":cargo ");
        let (text, _) = key_line(&h);
        assert!(
            text.contains("Enter run  C-Enter run as Job  Tab complete  \u{2191} history"),
            "the key line reads {text:?}"
        );
    }

    #[test]
    fn a_unique_prefix_of_a_user_command_shows_its_ghost() {
        let commands = user_commands(GREET);
        let resolved = resolve_typed(
            "gre",
            PaletteScope::All,
            &Availability::default(),
            &commands,
        )
        .expect("a command token");
        let user = match &resolved.entry {
            PaletteCommand::User(command) => Some(command.name.as_str()),
            PaletteCommand::Action(_) => None,
        };
        assert_eq!(
            (user, resolved.head_chars, resolved.ghost.as_str()),
            (Some("greet"), 3, "et"),
        );
    }

    /// Tab on a user row writes its name and a space, because the command
    /// takes an argument.
    #[test]
    fn tab_on_a_user_row_completes_its_name() {
        let mut h = user_palette(80, 24);
        select_row(&mut h, "greet");
        h.type_keys("tab");
        assert_eq!(palette_text(&h), "greet ");
    }

    /// The run also records the command's name in the palette history.
    #[test]
    fn enter_on_a_user_row_with_no_argument_runs_it() {
        let mut h = user_palette(80, 24);
        select_row(&mut h, "ping");
        h.type_keys("enter");
        h.settle();
        assert_eq!(
            (
                h.stoat.command_palette.is_none(),
                h.stoat.pending_message.as_deref(),
                h.stoat.active_workspace().palette_history.entries(),
            ),
            (true, Some("pong"), &["ping".to_owned()][..]),
        );
    }

    /// The root list with its selection on the user rows.
    #[test]
    fn snapshot_command_palette_user_commands() {
        let mut h = user_palette(80, 24);
        select_row(&mut h, "ping");
        h.assert_snapshot("command_palette_user_commands");
    }

    /// The user command `show`, which passes a file to `cat`.
    const SHOW: &str =
        r#"command show { arg path = file; run = shell("cat {path}"); output = notify; }"#;

    /// A harness over `src/main.rs` and `README.md` in the palette workspace,
    /// with the user config `source` loaded.
    fn file_harness(source: &str) -> TestHarness {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("src/main.rs", ""), ("README.md", "")]);
        h.stoat.reload_user_config(source);
        h
    }

    /// Type `text` and let the argument list settle on it.
    fn type_settled(h: &mut TestHarness, text: &str) {
        h.type_text(text);
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
    }

    /// Press Enter and let the run settle.
    fn enter_settled(h: &mut TestHarness) {
        h.type_keys("enter");
        h.settle();
    }

    /// The lines that the fake shell ran, in order.
    fn shell_lines(h: &TestHarness) -> Vec<String> {
        h.fake_shell
            .invocations()
            .into_iter()
            .map(|run| run.cmd)
            .collect()
    }

    #[test]
    fn a_file_argument_lists_the_workspace_files() {
        let mut h = file_harness(SHOW);
        type_settled(&mut h, ":show ma");
        assert_eq!(arg_rows(&h), ["/stoat-palette-test/src/main.rs"]);
    }

    /// The history line holds the picked path, so a recalled line runs on the
    /// same file.
    #[test]
    fn enter_passes_the_selected_file_relative_to_the_root() {
        let mut h = file_harness(SHOW);
        type_settled(&mut h, ":show ma");
        enter_settled(&mut h);
        assert_eq!(
            (
                shell_lines(&h),
                h.stoat.active_workspace().palette_history.entries(),
            ),
            (
                vec!["cat src/main.rs".to_owned()],
                &["show src/main.rs".to_owned()][..],
            ),
        );
    }

    /// A typed path that starts with `/` passes as typed, although the row
    /// `src/main.rs` matches `/main.rs` too. A text that no row matches passes
    /// as typed as well.
    #[test]
    fn a_typed_path_outside_the_list_passes_as_typed() {
        let mut h = file_harness(SHOW);
        type_settled(&mut h, ":show /main.rs");
        let matched = arg_rows(&h);
        enter_settled(&mut h);
        type_settled(&mut h, ":show zzz");
        enter_settled(&mut h);
        assert_eq!(
            (matched, shell_lines(&h)),
            (
                vec!["/stoat-palette-test/src/main.rs".to_owned()],
                vec!["cat /main.rs".to_owned(), "cat zzz".to_owned()],
            ),
        );
    }

    /// Enter in History runs a recalled user command line as typed. Up
    /// dispatches with no frame after it, so the argument list is still
    /// installed, and its selected row stands in for no argument.
    #[test]
    fn enter_in_history_runs_a_recalled_user_command_as_typed() {
        let mut h = file_harness(SHOW);
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["show ma".to_string()]);
        type_settled(&mut h, ":show ma");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::PaletteUp);

        let mut palette = h.stoat.command_palette.take().expect("palette open");
        let installed = palette.arg_picker.is_some();
        let run = match palette.handle_submit(h.stoat.active_workspace_mut()) {
            PaletteOutcome::Run(command, values, _) => Some((command.name.clone(), values)),
            _ => None,
        };
        assert_eq!(
            (installed, run),
            (true, Some(("show".to_owned(), vec!["ma".to_owned()]))),
        );
    }

    /// A `dir` argument lists the workspace directories. A typed `/` path
    /// browses that directory, and Enter passes the selected child whole,
    /// because it lies outside the workspace root.
    #[test]
    fn a_dir_argument_lists_directories_and_browses_a_typed_path() {
        let mut h = file_harness(
            r#"command lsd { arg where = dir; run = shell("ls {where}"); output = notify; }"#,
        );
        h.fake_fs().insert_dir("/elsewhere/a");
        h.fake_fs().insert_dir("/elsewhere/b");
        type_settled(&mut h, ":lsd ");
        let listed = arg_rows(&h);
        type_settled(&mut h, "/elsewhere/");
        let browsed = browse_dir_rows(&h);
        enter_settled(&mut h);
        assert_eq!(
            (listed, browsed, shell_lines(&h)),
            (
                vec!["/stoat-palette-test/src".to_owned()],
                vec!["a".to_owned(), "b".to_owned()],
                vec!["ls /elsewhere/a".to_owned()],
            ),
        );
    }

    /// The user command `note`, whose second argument is a file.
    const NOTE: &str = r#"command note { arg tag = text; arg path = file; run = shell("note {tag} {path}"); output = notify; }"#;

    /// Tab completes the selected file into the second argument of a command
    /// and keeps the first argument as typed.
    #[test]
    fn tab_completes_the_selected_file_into_the_argument() {
        let mut h = file_harness(NOTE);
        type_settled(&mut h, ":note urgent ma");
        h.type_keys("tab");
        assert_eq!(palette_text(&h), "note urgent src/main.rs");
    }

    /// The selected row gives the value of a second argument that has no
    /// text yet, after the argument typed before it.
    #[test]
    fn enter_on_an_empty_file_argument_takes_the_selected_row() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("src/main.rs", "")]);
        h.stoat.reload_user_config(NOTE);
        type_settled(&mut h, ":note urgent ");
        enter_settled(&mut h);
        assert_eq!(shell_lines(&h), ["note urgent src/main.rs"]);
    }

    /// The palette's argument ghost after a frame, `None` for no ghost.
    fn arg_ghost(h: &mut TestHarness) -> Option<String> {
        let _ = h.snapshot();
        h.stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .arg_ghost
            .clone()
    }

    /// The ghost shows the default of an empty argument and resolves it once.
    /// A typed char hides the ghost, and an argument emptied again shows it
    /// from the palette's cache.
    #[test]
    fn the_ghost_text_shows_the_default_of_an_empty_argument() {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[("a1b2c3d4", "fix the parser", &[("a.rs", "1\n")])],
        );
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h.stoat.reload_user_config(
            r#"command title { arg message = text(default: git.last_subject); run = shell("echo {message}"); }"#,
        );
        h.type_text(":title ");
        let shown = arg_ghost(&mut h);
        h.type_text("x");
        let typed = arg_ghost(&mut h);
        h.type_keys("backspace");
        let again = arg_ghost(&mut h);
        let cmd_line = h
            .rendered_text()
            .lines()
            .find(|line| line.contains('\u{276f}'))
            .unwrap_or_default()
            .to_owned();

        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        assert_eq!(
            (
                shown,
                typed,
                again,
                palette.resolved_defaults.clone(),
                cmd_line.contains("\u{276f} title fix the parser"),
            ),
            (
                Some("fix the parser".to_owned()),
                None,
                Some("fix the parser".to_owned()),
                vec![("title".to_owned(), 0, "fix the parser".to_owned())],
                true,
            ),
            "the Cmd Line reads {cmd_line:?}",
        );
    }

    /// A `file` default reads the buffer of the pane under the palette, and
    /// not the palette's own input, which takes the keys.
    #[test]
    fn a_file_ghost_reads_the_pane_under_the_palette() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h.fake_fs().insert_file("/repo/src/a.rs", "one\n");
        h.open_file(Path::new("/repo/src/a.rs"));
        h.stoat.reload_user_config(
            r#"command where { arg f = text(default: file); run = shell("echo {f}"); }"#,
        );
        h.type_text(":where ");
        assert_eq!(arg_ghost(&mut h).as_deref(), Some("src/a.rs"));
    }

    /// The user command `pr`, whose argument picks a pull request from the
    /// rows of `gh pr list`, with no preview, so the fake shell runs only the
    /// list command and the checkout.
    const PR: &str = concat!(
        "command pr { arg pr = pick(\"gh pr list --json number,title,headRefName,state\", ",
        "show: \"#{number} {title}\", value: \"{number}\"); ",
        "run = shell(\"gh pr checkout {pr}\"); output = notify; }",
    );

    /// [`PR`] as the board declares it, with the preview of each row.
    const PR_PREVIEW: &str = concat!(
        "command pr { arg pr = pick(\"gh pr list --json number,title,headRefName,state\", ",
        "show: \"#{number} {title}\", value: \"{number}\", preview: \"gh pr view {number}\"); ",
        "run = shell(\"gh pr checkout {pr}\"); output = notify; }",
    );

    /// The list command of [`PR`].
    const PR_LIST: &str = "gh pr list --json number,title,headRefName,state";

    /// A harness with [`PR`] loaded, whose fake shell answers the list command
    /// with `output`.
    fn pr_harness(output: ShellOutput) -> TestHarness {
        pick_harness(PR, output)
    }

    /// A harness with the user config `source` loaded, whose fake shell
    /// answers the list command of [`PR`] with `output`.
    fn pick_harness(source: &str, output: ShellOutput) -> TestHarness {
        let mut h = TestHarness::with_size(120, 30);
        h.stoat.reload_user_config(source);
        h.fake_shell.set_response(PR_LIST, output);
        h
    }

    /// A list command's answer of three pull requests as a JSON array.
    fn three_prs() -> ShellOutput {
        let rows = concat!(
            r#"[{"number":412,"title":"Ermine: diff status row and minimap islands","headRefName":"ermine","state":"OPEN"},"#,
            r#"{"number":409,"title":"Fix the parser","headRefName":"parser","state":"OPEN"},"#,
            r#"{"number":401,"title":"Docs for commands","headRefName":"docs","state":"OPEN"}]"#,
        );
        ShellOutput {
            stdout: rows.as_bytes().to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }

    /// The palette's pick list.
    fn pick(h: &TestHarness) -> &PickState {
        h.stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .pick
            .as_ref()
            .expect("a pick list")
    }

    /// The labels of the pick rows that match, in their order.
    fn pick_labels(h: &TestHarness) -> Vec<String> {
        let list = &pick(h).list;
        list.filtered
            .iter()
            .map(|&index| list.rows[index].label.clone())
            .collect()
    }

    /// The Cmd Line's row of the last frame.
    fn header_line(h: &TestHarness) -> String {
        h.rendered_text()
            .lines()
            .find(|line| line.contains('\u{276f}'))
            .unwrap_or_default()
            .to_owned()
    }

    /// The list command runs when the argument's entry starts. Its rows fill
    /// the list, the box sizes to them, and the Cmd Line counts them and names
    /// the command.
    #[test]
    fn a_pick_argument_runs_its_list_command_when_entry_starts() {
        let mut h = pr_harness(three_prs());
        type_settled(&mut h, ":pr ");
        let header = header_line(&h);
        assert_eq!(
            (
                shell_lines(&h),
                pick_labels(&h).first().cloned(),
                h.stoat
                    .command_palette
                    .as_ref()
                    .map(CommandPalette::list_rows_hint),
                header.contains("3 prs \u{b7} from gh pr list\u{2502}"),
            ),
            (
                vec![PR_LIST.to_owned()],
                Some("#412 Ermine: diff status row and minimap islands".to_owned()),
                Some(3),
                true,
            ),
            "the header reads {header:?}",
        );
    }

    /// Text typed past the argument's start drops the list, and the command
    /// list shows again.
    #[test]
    fn leaving_the_argument_drops_the_pick_list() {
        let mut h = pr_harness(three_prs());
        type_settled(&mut h, ":pr ");
        h.type_keys("backspace");
        type_settled(&mut h, "");
        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        assert_eq!(
            (palette.pick.is_none(), palette_text(&h)),
            (true, "pr".to_owned())
        );
    }

    #[test]
    fn the_list_command_runs_once_while_the_argument_stays_in_entry() {
        let mut h = pr_harness(three_prs());
        type_settled(&mut h, ":pr ");
        type_settled(&mut h, "4");
        type_settled(&mut h, "1");
        h.type_keys("backspace");
        type_settled(&mut h, "");
        assert_eq!(shell_lines(&h), [PR_LIST]);
    }

    /// Typed text narrows the rows and puts the selection back on the first
    /// row, which Enter then runs. The Cmd Line counts the one row.
    #[test]
    fn typed_text_narrows_the_pick_rows() {
        let mut h = pr_harness(three_prs());
        type_settled(&mut h, ":pr ");
        h.type_keys("down down");
        type_settled(&mut h, "parser");
        let narrowed = (pick_labels(&h), pick(&h).list.selected);
        let header = header_line(&h);
        enter_settled(&mut h);
        assert_eq!(
            (
                narrowed,
                header.contains("1 pr \u{b7} from gh pr list\u{2502}"),
                shell_lines(&h),
            ),
            (
                (vec!["#409 Fix the parser".to_owned()], 0),
                true,
                vec![PR_LIST.to_owned(), "gh pr checkout 409".to_owned()],
            ),
            "the header reads {header:?}",
        );
    }

    #[test]
    fn enter_runs_with_the_selected_row_s_value() {
        let mut h = pr_harness(three_prs());
        type_settled(&mut h, ":pr ");
        enter_settled(&mut h);
        assert_eq!(shell_lines(&h), [PR_LIST, "gh pr checkout 412"]);
    }

    #[test]
    fn enter_with_no_matching_row_passes_the_typed_text() {
        let mut h = pr_harness(three_prs());
        type_settled(&mut h, ":pr 777");
        enter_settled(&mut h);
        assert_eq!(shell_lines(&h), [PR_LIST, "gh pr checkout 777"]);
    }

    #[test]
    fn tab_writes_the_selected_value_into_the_argument() {
        let mut h = pr_harness(three_prs());
        type_settled(&mut h, ":pr ");
        h.type_keys("tab");
        assert_eq!(palette_text(&h), "pr 412");
    }

    /// A harness over the `pr` pick list, with the input changed to `pr docs`
    /// and no frame since.
    fn unseen_docs() -> TestHarness {
        let mut h = pr_harness(three_prs());
        type_settled(&mut h, ":pr ");
        let active = h.stoat.active_workspace;
        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        palette
            .input
            .replace_text(&mut h.stoat.workspaces[active], "pr docs");
        h
    }

    /// Enter and Tab rank the rows against text that no frame has seen yet,
    /// as a burst of keys or a macro gives it. So each takes the row that the
    /// text names.
    #[test]
    fn enter_and_tab_rank_text_that_no_frame_has_seen() {
        let mut tab = unseen_docs();
        crate::action_handlers::dispatch(&mut tab.stoat, &stoat_action::PickerComplete);
        let tabbed = palette_text(&tab);

        let mut enter = unseen_docs();
        let mut palette = enter.stoat.command_palette.take().expect("palette open");
        let ws = enter.stoat.active_workspace_mut();
        palette.refilter_from_input(ws);
        let values = match palette.handle_submit(ws) {
            PaletteOutcome::Run(_, values, _) => Some(values),
            _ => None,
        };
        assert_eq!(
            (tabbed, values),
            ("pr 401".to_owned(), Some(vec!["401".to_owned()]))
        );
    }

    /// PageDown, Up, and Down step the selection of a pick list, and Enter
    /// runs the row that the selection stands on. A page steps by half the
    /// rows on show, so it reaches the last of the three rows.
    #[test]
    fn the_keys_step_the_pick_selection() {
        let mut h = pr_harness(three_prs());
        type_settled(&mut h, ":pr ");
        let steps = ["pagedown", "up", "down"].map(|key| {
            h.type_keys(key);
            pick(&h).list.selected
        });
        enter_settled(&mut h);
        assert_eq!(
            (steps, shell_lines(&h)),
            (
                [2, 1, 2],
                vec![PR_LIST.to_owned(), "gh pr checkout 401".to_owned()]
            ),
        );
    }

    #[test]
    fn a_failed_list_command_shows_its_stderr_in_the_list_s_place() {
        let mut h = pr_harness(ShellOutput {
            stdout: Vec::new(),
            stderr: b"\ngh: not logged in\nmore\n".to_vec(),
            exit_code: 1,
        });
        type_settled(&mut h, ":pr ");
        let header = header_line(&h);
        assert_eq!(
            (
                pick(&h).load.clone(),
                header.contains("failed \u{b7} from gh pr list\u{2502}"),
                h.rendered_text().contains("gh: not logged in"),
            ),
            (PickLoad::Failed("gh: not logged in".to_owned()), true, true),
            "the header reads {header:?}",
        );
    }

    /// The rows fill the whole body down to the key line, the doc rows among
    /// them, as History's rows do. So the last of twenty rows shows in a box
    /// whose list region holds fourteen.
    #[test]
    fn the_pick_rows_fill_the_body_down_to_the_key_line() {
        let rows: String = (1..=20).map(|at| format!("row {at:02}\n")).collect();
        let mut h = TestHarness::with_size(160, 50);
        h.stoat
            .reload_user_config(r#"command peek { arg f = pick("ls"); run = shell("cat {f}"); }"#);
        h.fake_shell.set_response(
            "ls",
            ShellOutput {
                stdout: rows.into_bytes(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );
        type_settled(&mut h, ":peek ");
        let text = h.rendered_text();
        assert_eq!(
            (text.contains("row 01"), text.contains("row 20")),
            (true, true)
        );
    }

    /// A pixel pick list pages by the rows whose boxes fit whole in its list,
    /// which are fewer than its cell rows.
    #[test]
    fn a_pixel_pick_list_pages_by_the_rows_that_fit() {
        let rows: String = (1..=40).map(|at| format!("row {at:02}\n")).collect();
        let mut h = TestHarness::with_size(120, 30);
        h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
        h.stoat.cell_pixels = Some((9, 18));
        h.stoat.display_scale = Some(1.0);
        h.stoat
            .reload_user_config(r#"command peek { arg f = pick("ls"); run = shell("cat {f}"); }"#);
        h.fake_shell.set_response("ls", stdout(&rows));
        type_settled(&mut h, ":peek ");

        let list = crate::mouse::boxed_modal_surfaces(&h.stoat)
            .expect("the palette is open")
            .list;
        let metrics = h
            .stoat
            .command_palette
            .as_ref()
            .and_then(|palette| palette.row_metrics)
            .expect("the list lays its rows out by pixels");
        let fit = metrics.fit(list.height);
        assert_eq!(
            (pick(&h).list.viewport_rows, fit < usize::from(list.height)),
            (Some(fit), true),
        );
    }

    /// A fake shell answer of `stdout`, exit 0.
    fn stdout(text: &str) -> ShellOutput {
        ShellOutput {
            stdout: text.as_bytes().to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }

    /// A harness over the board's `pr` command with its preview, whose fake
    /// shell answers the previews of the first two pull requests.
    fn preview_harness() -> TestHarness {
        let h = pick_harness(PR_PREVIEW, three_prs());
        h.fake_shell
            .set_response("gh pr view 412", stdout("the status row\n"));
        h.fake_shell
            .set_response("gh pr view 409", stdout("the parser fix\n"));
        h
    }

    /// Whether the pick list's preview pane shows `text`.
    fn previews(h: &TestHarness, text: &str) -> bool {
        pick(h).preview.source() == Some(&PreviewSource::Text(Arc::from(text)))
    }

    /// The selected row's preview runs once, and its stdout shows in the pane.
    /// A step down runs the second row's preview, and a step back up runs
    /// nothing and shows the first text again.
    ///
    /// Each text shows once the pumps settle, with no frame after its land, as
    /// the binary shows it with no key to start a frame.
    #[test]
    fn the_selected_row_s_preview_runs_once_and_shows_its_stdout() {
        let mut h = preview_harness();
        h.type_text(":pr ");
        h.settle();
        let first = previews(&h, "the status row\n");
        h.type_keys("down");
        let second = previews(&h, "the parser fix\n");
        h.type_keys("up");
        assert_eq!(
            (
                first,
                second,
                previews(&h, "the status row\n"),
                shell_lines(&h)
            ),
            (
                true,
                true,
                true,
                vec![
                    PR_LIST.to_owned(),
                    "gh pr view 412".to_owned(),
                    "gh pr view 409".to_owned()
                ]
            ),
        );
    }

    #[test]
    fn a_failed_preview_shows_its_stderr_line() {
        let mut h = pick_harness(PR_PREVIEW, three_prs());
        h.fake_shell.set_response(
            "gh pr view 412",
            ShellOutput {
                stdout: Vec::new(),
                stderr: b"\nno pull request\n".to_vec(),
                exit_code: 1,
            },
        );
        h.type_text(":pr ");
        h.settle();
        assert_eq!(
            pick(&h).preview.source(),
            Some(&PreviewSource::Text(Arc::from("no pull request")))
        );
    }

    /// A refresh whose list command fails blanks the preview, so no row's text
    /// stands beside the failure.
    #[test]
    fn a_failed_refresh_blanks_the_preview() {
        let mut h = preview_harness();
        h.type_text(":pr ");
        h.settle();
        let shown = previews(&h, "the status row\n");
        h.fake_shell.set_response(
            PR_LIST,
            ShellOutput {
                stdout: Vec::new(),
                stderr: b"gh: offline\n".to_vec(),
                exit_code: 1,
            },
        );
        h.type_keys("ctrl-r");
        h.settle();
        assert_eq!(
            (shown, pick(&h).load.clone(), pick(&h).preview.source()),
            (true, PickLoad::Failed("gh: offline".to_owned()), None),
        );
    }

    /// Rows that replace a ranked list rank against its query at once, with
    /// the selection on the best row.
    #[test]
    fn set_rows_ranks_the_rows_against_the_last_query() {
        let row = |label: &str| ArgRow {
            label: label.to_owned(),
            insert: label.to_owned(),
            value_name: None,
            description: String::new(),
        };
        let mut list = ArgList::default();
        list.refilter("bet");
        list.set_rows(vec![row("alpha"), row("beta")]);
        assert_eq!(
            (list.filtered.clone(), list.selected_row()),
            (vec![1], Some(&row("beta")))
        );
    }

    /// A pick list with no preview template keeps the whole body for its rows,
    /// so a long label shows whole. A list with a preview clips the label at
    /// its edge, left of the preview.
    #[test]
    fn a_pick_with_no_preview_keeps_the_full_width_list() {
        let shows_whole = |source| {
            let mut h = pick_harness(source, three_prs());
            type_settled(&mut h, ":pr ");
            h.rendered_text()
                .contains("#412 Ermine: diff status row and minimap islands")
        };
        assert_eq!([shows_whole(PR), shows_whole(PR_PREVIEW)], [true, false]);
    }

    /// `Ctrl-r` runs the list command again, and the list reads `loading`
    /// until the rows land. The preview blanks meanwhile and runs no line. The
    /// preview texts clear, so the selected row's preview runs again.
    ///
    /// The first refresh comes while the first preview runs. That text belongs
    /// to the list before the refresh, so it lands nowhere, and the preview
    /// runs again.
    ///
    /// The second refresh dispatches, and one frame follows with no scheduler
    /// run. The harness's keys land the fake shell's rows before the load is
    /// read. The key then runs the refresh a third time.
    #[test]
    fn ctrl_r_runs_the_list_command_again_and_clears_the_previews() {
        let mut h = preview_harness();
        h.type_text(":pr ");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::PaletteRefresh);
        h.settle();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::PaletteRefresh);
        h.stoat.drive_background();
        let loading = (
            pick(&h).load.clone(),
            pick(&h).previews.is_empty(),
            pick(&h).preview.source().is_none(),
        );
        h.settle();
        h.type_keys("ctrl-r");
        h.settle();
        let run = [PR_LIST.to_owned(), "gh pr view 412".to_owned()];
        assert_eq!(
            (loading, shell_lines(&h), previews(&h, "the status row\n")),
            ((PickLoad::Loading, true, true), vec![run; 4].concat(), true),
        );
    }

    /// With no pick list on show, the refresh changes nothing and asks for no
    /// frame.
    #[test]
    fn the_refresh_without_a_pick_list_does_nothing() {
        let mut h = pick_harness(PR, three_prs());
        type_settled(&mut h, ":pr");
        let effect = crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::PaletteRefresh);
        type_settled(&mut h, "");
        assert_eq!(
            (effect, shell_lines(&h)),
            (UpdateEffect::None, Vec::<String>::new())
        );
    }

    /// The key line names the shell line that Enter runs and the refresh key.
    /// Its note names the config line that declares the command, with
    /// `config` in the foreground of `ui.cmd.source.config`.
    #[test]
    fn the_key_line_names_the_line_enter_runs() {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.reload_user_config(PR);
        h.fake_shell.set_response(PR_LIST, three_prs());
        type_settled(&mut h, ":pr ");
        let (text, colors) = key_line(&h);
        let line = text.trim_end();
        let note = "config command \u{b7} config.stcfg:1";
        let at = line.chars().count() - note.chars().count();
        let config = h
            .stoat
            .theme
            .get(crate::theme::scope::UI_CMD_SOURCE_CONFIG)
            .fg;
        assert_eq!(
            (
                line.contains("Enter gh pr checkout 412  C-r refresh list"),
                line.ends_with(note),
                colors[at..at + "config".len()]
                    .iter()
                    .map(|&color| Some(color))
                    .collect::<Vec<_>>(),
            ),
            (true, true, vec![config; 6]),
            "the key line reads {text:?}",
        );
    }

    /// A script, a block command whose `run` lists its steps, lists under the
    /// Config scope. Its row ends in the `script` Source Tag, and its doc
    /// names it a script command.
    #[test]
    fn a_script_reads_the_script_tag_and_lists_under_the_config_scope() {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.reload_user_config(
            r#"command ship { doc = "test and push"; run = [shell("cargo test"), shell("git push")]; }"#,
        );
        h.type_text(":");
        h.type_keys("backtab");
        h.type_keys("backtab");
        let _ = h.snapshot();
        let text = h.rendered_text();
        let (rows, zoom, header_rows, doc_rows) = crate::test_fixture::palette_sizing(&h);
        let list = crate::render::command_palette::palette_filter_layout(
            h.stoat.size(),
            rows,
            zoom,
            header_rows,
            doc_rows,
        )
        .expect("the palette fits the test terminal")
        .list;
        let row: String = text
            .lines()
            .nth(usize::from(list.y))
            .unwrap_or_default()
            .chars()
            .skip(usize::from(list.x))
            .take(usize::from(list.width))
            .collect();
        assert_eq!(
            (
                h.stoat
                    .command_palette
                    .as_ref()
                    .map(|palette| palette.scope),
                row_names(&h),
                row.contains("script"),
                text.contains("script command \u{b7} config.stcfg:1"),
            ),
            (
                Some(PaletteScope::Config),
                vec!["ship".to_owned()],
                true,
                true
            ),
            "the palette reads {text:?}",
        );
    }

    /// The key line of a script's pick list names the first shell line that
    /// Enter runs, and `C-Enter` after it. Its note names the config line, with
    /// `script` in the foreground of `ui.cmd.source.script`.
    #[test]
    fn the_key_line_of_a_script_names_its_first_shell_line_and_its_tag() {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.reload_user_config(concat!(
            "command prgo { arg pr = pick(\"gh pr list --json number,title,headRefName,state\", ",
            "show: \"#{number} {title}\", value: \"{number}\"); ",
            "run = [shell(\"gh pr checkout {pr}\"), Notify(\"checked out\")]; }",
        ));
        h.fake_shell.set_response(PR_LIST, three_prs());
        type_settled(&mut h, ":prgo ");
        let (text, colors) = key_line(&h);
        let line = text.trim_end();
        let note = "script command \u{b7} config.stcfg:1";
        let at = line.chars().count() - note.chars().count();
        let script = h
            .stoat
            .theme
            .get(crate::theme::scope::UI_CMD_SOURCE_SCRIPT)
            .fg;
        assert_eq!(
            (
                line.contains("Enter gh pr checkout 412  C-Enter run as Job  C-r refresh list"),
                line.ends_with(note),
                colors[at..at + "script".len()]
                    .iter()
                    .map(|&color| Some(color))
                    .collect::<Vec<_>>(),
            ),
            (true, true, vec![script; 6]),
            "the key line reads {text:?}",
        );
    }

    /// The pick list's preview pane goes with its list. That holds when the
    /// argument leaves entry, when another pick list takes its place, and when
    /// the palette closes over the list.
    #[test]
    fn a_pick_preview_buffer_goes_with_its_list() {
        let source =
            format!("{PR}\ncommand peek {{ arg f = pick(\"ls\"); run = shell(\"cat {{f}}\"); }}");
        let mut h = pick_harness(&source, three_prs());
        type_settled(&mut h, ":pr ");
        let left = pick(&h).preview.buffer;
        h.type_keys("backspace");
        let _ = h.snapshot();
        type_settled(&mut h, " ");
        let replaced = pick(&h).preview.buffer;
        let active = h.stoat.active_workspace;
        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        palette
            .input
            .replace_text(&mut h.stoat.workspaces[active], "peek ");
        let _ = h.snapshot();
        let closed = pick(&h).preview.buffer;
        h.type_keys("escape");
        let buffers = &h.stoat.active_workspace().buffers;
        assert_eq!(
            [left, replaced, closed].map(|id| buffers.get(id).is_none()),
            [true; 3],
        );
    }

    /// `C-d` scrolls the pick list's preview by half its rows, as it scrolls
    /// every modal preview.
    #[test]
    fn ctrl_d_scrolls_the_pick_preview_by_half_its_rows() {
        let mut h = pick_harness(PR_PREVIEW, three_prs());
        let long: String = (0..80).map(|i| format!("line {i}\n")).collect();
        h.fake_shell.set_response("gh pr view 412", stdout(&long));
        h.type_text(":pr ");
        h.settle();
        let rows = crate::mouse::modal_preview_rows(&h.stoat).expect("the pick preview pane");
        h.type_keys("ctrl-d");
        let editors = &h.stoat.active_workspace().editors;
        let scrolled = editors.get(pick(&h).preview.editor).map(|e| e.scroll_row);
        assert_eq!(scrolled, Some((rows / 2).max(1) as u32));
    }

    /// A preview shows its stdout cut at the byte limit of every preview.
    #[test]
    fn a_long_preview_is_cut_at_the_byte_limit() {
        let mut h = pick_harness(PR_PREVIEW, three_prs());
        let long = "x".repeat(PREVIEW_BYTE_LIMIT + 1);
        h.fake_shell.set_response("gh pr view 412", stdout(&long));
        h.type_text(":pr ");
        h.settle();
        let shown = match pick(&h).preview.source() {
            Some(PreviewSource::Text(text)) => Some(text.len()),
            _ => None,
        };
        assert_eq!(shown, Some(PREVIEW_BYTE_LIMIT));
    }

    /// The preview paints the selected row's text in its rect, past a
    /// separator column at the list's right edge.
    #[test]
    fn the_pick_preview_paints_past_a_separator() {
        let mut h = preview_harness();
        h.type_text(":pr ");
        h.settle();
        let _ = h.snapshot();
        let (rows, zoom, header_rows, doc_rows) = crate::test_fixture::palette_sizing(&h);
        let layout = crate::render::command_palette::palette_filter_layout(
            h.stoat.size(),
            rows,
            zoom,
            header_rows,
            doc_rows,
        )
        .expect("the palette fits the test terminal");
        let (list, preview) = crate::render::command_palette::pick_body(&layout, true);
        let preview = preview.expect("the body holds a preview");

        let buf = h.rendered_buffer();
        let separator: String = (list.y..list.bottom())
            .map(|y| buf[(list.right(), y)].symbol())
            .collect();
        let first_row: String = (preview.x..preview.right())
            .map(|x| buf[(x, preview.y)].symbol())
            .collect();
        assert_eq!(
            (separator, first_row.trim_end()),
            (
                "\u{2502}".repeat(usize::from(list.height)),
                "the status row"
            ),
        );
    }

    /// A pick list whose command runs an action names `run` for Enter,
    /// because an action has no shell line to show. The `block` output names
    /// `C-Enter` after it, because the key line reads the output alone.
    #[test]
    fn the_key_line_names_run_for_an_action_step() {
        let source = concat!(
            "command pr { arg pr = pick(\"gh pr list --json number,title,headRefName,state\"); ",
            "run = Notify(\"{pr}\"); }",
        );
        let mut h = pick_harness(source, three_prs());
        type_settled(&mut h, ":pr ");
        let (text, _) = key_line(&h);
        assert!(
            text.contains("Enter run  C-Enter run as Job  C-r refresh list"),
            "the key line reads {text:?}"
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
            let name = named_command(text, PaletteScope::All, &ctx, &CommandSet::default())
                .map(|(command, _)| action_name(command));
            let shown = resolve_typed(text, PaletteScope::All, &ctx, &CommandSet::default());
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

        let (rows, zoom, header_rows, doc_rows) = crate::test_fixture::palette_sizing(&h);
        let list = crate::render::command_palette::palette_filter_layout(
            h.stoat.size(),
            rows,
            zoom,
            header_rows,
            doc_rows,
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
            .map(entry_of)
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
                .map(|command| entry_of(command).command_name.as_str())
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
            parse_command(text, &Availability::default(), &CommandSet::default())
                .map(|(command, tail)| (action_name(command), tail))
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
            split_head(text, &Availability::default(), &CommandSet::default())
                .map(|(command, head, tail)| (action_name(command), head, tail))
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
            &CommandSet::default(),
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
                && split_head(
                    &history[0],
                    &Availability::default(),
                    &CommandSet::default()
                )
                .is_some(),
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
        let expected = entry_of(&palette.filtered[palette.selected]).def.name();
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
        let expected = entry_of(&palette.filtered[1]).def.name();
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

    /// S-Tab in the list of a file argument steps the file scope through the
    /// cycle of the File Finder. Each step puts the selection on the first
    /// row. AllWorkspaces shows each row under its root, and the return to
    /// All drops the roots. A named scope stands after Modified. The palette
    /// scope stays Active.
    #[test]
    fn backtab_in_the_file_argument_steps_the_file_scope() {
        let mut h = modified_harness();
        h.type_text(":o ");
        h.settle();
        let all = file_scope(&h);
        h.type_keys("down");
        let mut steps = Vec::new();
        for _ in 0..3 {
            h.type_keys("backtab");
            h.settle();
            let picker = arg_picker(&h);
            let selected = picker.active_core_ref().picklist.selected;
            let rooted = picker.core.picklist.display_roots.is_some();
            steps.push((file_scope(&h), selected, rooted));
        }
        let palette_scope = h
            .stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .scope;

        let mut named = modified_harness();
        named.stoat.settings.finder_scopes =
            BTreeMap::from([("code".to_owned(), vec!["*.rs".to_owned()])]);
        named.type_text(":o ");
        let mut cycle = vec![file_scope(&named).0];
        for _ in 0..4 {
            named.type_keys("backtab");
            cycle.push(file_scope(&named).0);
        }

        let both = || vec!["a.rs".to_owned(), "b.rs".to_owned()];
        assert_eq!(
            (all, steps, cycle, palette_scope),
            (
                (FinderScope::All, both()),
                vec![
                    ((FinderScope::Modified, vec!["b.rs".to_owned()]), 0, false),
                    ((FinderScope::AllWorkspaces, both()), 0, true),
                    ((FinderScope::All, both()), 0, false),
                ],
                vec![
                    FinderScope::All,
                    FinderScope::Modified,
                    FinderScope::Named("code".to_owned()),
                    FinderScope::AllWorkspaces,
                    FinderScope::All,
                ],
                PaletteScope::Active,
            ),
        );
    }

    /// S-Tab flips the palette scope at the root and in the list of an
    /// argument with no file scope, which keeps its rows.
    #[test]
    fn backtab_in_any_other_palette_stage_flips_the_palette_scope() {
        let mut h = Stoat::test();
        seed_palette_workspace(&mut h, &[("src/a.rs", "")]);
        h.type_text(":");
        h.type_keys("backtab");
        let root = h
            .stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .scope;
        h.type_keys("escape");
        h.type_text(":cd ");
        h.settle();
        let dirs = arg_rows(&h);
        h.type_keys("backtab");
        h.settle();

        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        assert_eq!(
            (
                root,
                palette.scope,
                arg_rows(&h),
                arg_picker(&h).scopes.is_none()
            ),
            (PaletteScope::All, PaletteScope::All, dirs, true),
        );
    }

    /// The file argument starts at All at each install, and a step of its
    /// scope leaves the scope that the File Finder remembers alone.
    #[test]
    fn the_file_argument_starts_at_all_each_time_and_leaves_the_remembered_scope() {
        let mut h = modified_harness();
        h.type_text(":o ");
        h.type_keys("backtab");
        let stepped = file_scope(&h).0;
        h.type_keys("escape");
        h.type_text(":o ");
        h.settle();

        let remembered = h.stoat.active_workspace().last_finder_scope.clone();
        assert_eq!(
            (stepped, file_scope(&h).0, remembered),
            (FinderScope::Modified, FinderScope::All, None),
        );
    }

    /// Over the changes view the file argument holds Diff in the place of
    /// Modified, with the files that the diff shows.
    #[test]
    fn the_file_argument_over_a_diff_holds_diff_in_place_of_modified() {
        let mut h = TestHarness::with_size(100, 24);
        let root = PathBuf::from("/repo");
        h.fake_fs().insert_files(
            [("a.txt", "A\n"), ("b.txt", "B\n"), ("c.txt", "c\n")]
                .map(|(rel, text)| (root.join(rel), text.as_bytes())),
        );
        h.stoat.active_workspace_mut().git_root = root.clone();
        h.fake_git()
            .add_repo(&root)
            .with_fs(h.fake_fs())
            .modified("a.txt", "a\n", "A\n")
            .modified("b.txt", "b\n", "B\n")
            .head_file("c.txt", "c\n");
        h.open_file(&root.join("a.txt"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        h.type_text("o ");
        h.settle();
        h.type_keys("backtab");
        h.settle();
        let diff = file_scope(&h);
        h.type_keys("backtab");
        h.settle();

        assert_eq!(
            (diff, file_scope(&h).0),
            (
                (
                    FinderScope::Diff,
                    vec!["a.txt".to_owned(), "b.txt".to_owned()]
                ),
                FinderScope::AllWorkspaces,
            ),
        );
    }

    /// The row of a modified file ends in the `M` mark, and the row of an
    /// unchanged file ends blank.
    #[test]
    fn a_modified_file_carries_its_mark_in_the_file_argument_list() {
        let mut h = modified_harness();
        h.type_text(":o ");
        h.settle();
        let _ = h.snapshot();

        let list = crate::test_fixture::palette_arg_layout(&h).list;
        let buf = h.rendered_buffer();
        let rows: Vec<(String, String)> = (list.y..list.bottom())
            .filter_map(|y| {
                let label: String = (list.x..list.right() - 1)
                    .map(|x| buf[(x, y)].symbol())
                    .collect();
                let mark = buf[(list.right() - 1, y)].symbol().to_owned();
                let label = label.trim().to_owned();
                (!label.is_empty()).then_some((label, mark))
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("a.rs".to_owned(), " ".to_owned()),
                ("b.rs".to_owned(), "M".to_owned())
            ],
        );
    }

    /// The Cmd Line names a file scope other than All after the count, and
    /// names no scope under All.
    #[test]
    fn the_cmd_line_names_a_file_scope_other_than_all() {
        let mut h = modified_harness();
        h.type_text(":o ");
        h.settle();
        let all = arg_header(&mut h);
        h.type_keys("backtab");
        h.settle();
        let modified = arg_header(&mut h);

        assert_eq!(
            (
                all.ends_with("2 of 2"),
                modified.ends_with("1 of 1 \u{b7} modified")
            ),
            (true, true),
            "the header reads {all:?}, then {modified:?}",
        );
    }

    /// A file scope other than All takes no fallback of ignored files. A
    /// fallback built under All stays through the step, but stands in for no
    /// other scope. The typing and the settles of that scope leave the query
    /// of the fallback as it was.
    #[test]
    fn a_file_scope_other_than_all_takes_no_fallback() {
        let mut h = modified_harness();
        h.type_text(":o ");
        let built = has_fallback_after(&mut h, "zzz");
        let active = arg_picker(&h).fallback_active();
        h.type_keys("backtab");
        h.settle();
        let after_step = arg_picker(&h).fallback_active();
        h.type_text("q");
        h.settle();
        let typed = arg_fallback_query(&h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::PickerComplete);
        let completed = arg_fallback_query(&h);

        let mut stepped = modified_harness();
        stepped.type_text(":o ");
        stepped.type_keys("backtab");
        let unbuilt = !has_fallback_after(&mut stepped, "zzz");
        let wanted = arg_picker(&stepped).wants_ignored_fallback("zzz");

        let kept = Some("zzz".to_owned());
        assert_eq!(
            (built, active, after_step, typed, completed, unbuilt, wanted),
            (true, true, false, kept.clone(), kept, true, None),
        );
    }

    /// An action that runs before a frame settles the list against the scope
    /// on show. Under Modified, Tab over a tail that only an unmodified file
    /// holds completes nothing.
    #[test]
    fn an_action_between_frames_settles_against_the_scope_on_show() {
        let mut h = modified_harness();
        h.type_text(":o ");
        h.type_keys("backtab");
        h.settle();
        let ws = &mut h.stoat.workspaces[h.stoat.active_workspace];
        let palette = h.stoat.command_palette.as_mut().expect("palette open");
        palette.input.replace_text(ws, "o a");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::PickerComplete);
        assert_eq!(palette_text(&h), "o a");
    }

    /// Tab under Modified completes a modified file, and a run of presses
    /// walks the Modified list alone.
    #[test]
    fn tab_under_the_modified_scope_completes_a_modified_file() {
        let mut h = modified_harness();
        h.type_text(":o ");
        h.type_keys("backtab");
        h.settle();
        h.type_keys("tab");
        let first = palette_text(&h);
        h.type_keys("tab tab");

        assert_eq!(
            (first, palette_text(&h)),
            ("o b.rs".to_owned(), "o b.rs".to_owned()),
        );
    }

    /// The step onto Modified asks git again, so a file that turns modified
    /// after the list opened lists.
    #[test]
    fn backtab_onto_modified_asks_git_again() {
        let mut h = Stoat::test();
        let root = seed_palette_workspace(&mut h, &[("a.rs", "v1\n"), ("b.rs", "v1\n")]);
        h.fake_git()
            .add_repo(&root)
            .with_fs(h.fake_fs())
            .head_file("a.rs", "v1\n")
            .head_file("b.rs", "v1\n");
        h.type_text(":o ");
        h.settle();
        h.fake_git()
            .add_repo(&root)
            .with_fs(h.fake_fs())
            .modified("b.rs", "v1\n", "v2\n");
        h.type_keys("backtab");
        h.settle();

        assert_eq!(
            file_scope(&h),
            (FinderScope::Modified, vec!["b.rs".to_owned()])
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

        let (rows, zoom, header_rows, doc_rows) = crate::test_fixture::palette_sizing(&h);
        let doc = crate::render::command_palette::palette_filter_layout(
            h.stoat.size(),
            rows,
            zoom,
            header_rows,
            doc_rows,
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
                .map(|command| entry_of(command).command_name.as_str());
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
            .map(|command| entry_of(command).command_name.as_str());
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

    /// The words typed before Up narrow History to the lines that hold each of
    /// them, in any order. The chip names the words one space apart, although
    /// the input holds two spaces. The words mark their chars in the recalled
    /// input and in the row.
    #[test]
    fn typed_words_narrow_history_in_any_order() {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.active_workspace_mut().palette_history = InputHistory::from_entries(vec![
            "git checkout -b foo-stoat-bar".to_string(),
            "git log".to_string(),
            "cd /x".to_string(),
        ]);
        h.type_text(":stoat  git");
        h.type_keys("up");
        let _ = h.snapshot();

        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        assert_eq!(
            (
                palette_text(&h),
                palette.history_rows.len(),
                history_list_row(&h, 0),
                h.rendered_text()
                    .contains("history \u{b7} stoat git \u{b7} 1 / 1"),
                palette_marks(&h),
            ),
            (
                "git checkout -b foo-stoat-bar".to_owned(),
                1,
                "\u{21ba}  git checkout -b foo-stoat-bar".to_owned(),
                true,
                ["gitstoat".to_owned(), "gitstoat".to_owned()],
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

    /// The chars that draw in the foreground of `ui.list.match` on the Cmd
    /// Line's first row and on the first row of the palette list.
    fn palette_marks(h: &TestHarness) -> [String; 2] {
        let (rows, zoom, header_rows, doc_rows) = crate::test_fixture::palette_sizing(h);
        let layout = crate::render::command_palette::palette_filter_layout(
            h.stoat.size(),
            rows,
            zoom,
            header_rows,
            doc_rows,
        )
        .expect("the palette fits the test terminal");
        let fg = h.stoat.theme.list_match().fg;
        let buf = h.rendered_buffer();
        [layout.inner.y, layout.list.y].map(|y| {
            (layout.inner.x..layout.inner.right())
                .filter(|&x| Some(buf[(x, y)].fg) == fg)
                .map(|x| buf[(x, y)].symbol())
                .collect()
        })
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
        let (rows, zoom, header_rows, doc_rows) = crate::test_fixture::palette_sizing(h);
        let list = crate::render::command_palette::palette_filter_layout(
            h.stoat.size(),
            rows,
            zoom,
            header_rows,
            doc_rows,
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
        let (rows, zoom, header_rows, doc_rows) = crate::test_fixture::palette_sizing(h);
        let footer = crate::render::command_palette::palette_filter_layout(
            h.stoat.size(),
            rows,
            zoom,
            header_rows,
            doc_rows,
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
            let names: Vec<_> = palette
                .filtered
                .iter()
                .map(|e| entry_of(e).def.name())
                .collect();
            assert!(!names.contains(&"AbortRebase"), "got {names:?}");
        }
        h.type_keys("backtab");
        {
            let palette = h.stoat.command_palette.as_ref().unwrap();
            let names: Vec<_> = palette
                .filtered
                .iter()
                .map(|e| entry_of(e).def.name())
                .collect();
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

    /// The argument picker's plate refits to the widest label on show each
    /// time the rows change. The files hold no line, so the preview takes the
    /// 60 columns of its floor. The tail `zz` shows the 66-char path, so the
    /// plate grows from 96 to 132 columns. Clearing the tail fits the list back
    /// to 33 columns, and the plate narrows back to 96.
    #[test]
    fn the_argument_plate_refits_when_its_rows_change() {
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
            [(96, 33), (132, 69), (96, 33)],
        );
    }

    /// Each open of the palette fits the argument plate to the labels its list
    /// shows, whatever plate the last open drew.
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

        assert_eq!((grown, palette_plate(&h).0), (132, 96));
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
    /// 96-column plate past it.
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
            None,
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
