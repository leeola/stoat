use crate::{
    host::{FsHost, GitHost},
    input_view::{InputView, SubmitTarget},
    paths,
    picker::{BaseId, DisplayCache, PathPicker, PendingScan, PreviewPolicy, Scan, ScanTarget},
    render::{picker::PlateFit, pixel_rows::RowMetrics},
    workspace::Workspace,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::{
    collections::{hash_map::DefaultHasher, BTreeMap},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_scheduler::{Executor, Task};
use tokio::sync::mpsc::UnboundedReceiver;

/// Upper bound on the paths a directory-browse walk or a fallback of ignored
/// files collects.
///
/// A bare `/` walk otherwise traverses the whole filesystem, and an ignored
/// build tree such as `target/` holds a very large number of files. Draining
/// stops here and the walk is dropped, which keeps the list and its refilter
/// bounded.
pub(crate) const BROWSE_PATH_CAP: usize = 100_000;

/// Rows the finder's chrome occupies above and below its list: the two border
/// rows, the input row, and the separator under it. Content rows plus this is
/// the box height that shows the whole list.
const FINDER_CHROME_ROWS: u16 = 4;

/// Which subset of files the finder currently lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinderScope {
    /// Every file under `git_root` that is not gitignored. Snapshotted at
    /// open time.
    All,
    /// Files with uncommitted git changes. Refreshed on every scope toggle
    /// so the list stays current.
    Modified,
    /// The files the diff under the finder shows, untracked files with content
    /// among them, captured at open time. Only an open over the changes view or
    /// the per-file diff lands here, and for that open the scope takes
    /// [`FinderScope::Modified`]'s place in the Shift-Tab cycle.
    Diff,
    /// Currently-open path-bound buffers from the workspace's
    /// [`BufferRegistry`]. Captured at open time. Reachable only through the
    /// dedicated `OpenBufferPicker` action. Shift-Tab flips to
    /// [`FinderScope::ModifiedBuffers`] and back, so the buffer picker never
    /// enters a file scope.
    Buffers,
    /// The open path-bound buffers whose `dirty` flag is set, which are the
    /// buffers with unsaved edits. Captured at open time like
    /// [`FinderScope::Buffers`]. Reachable only through the scope toggle from
    /// [`FinderScope::Buffers`].
    ModifiedBuffers,
    /// A config-defined named glob scope (`finder.scope.<name>`). Shift-Tab
    /// cycles through these alphabetically after Modified, and the list shows
    /// only files matching the scope's globs.
    Named(String),
    /// Every file under every known workspace root -- the open workspaces plus
    /// the on-disk registry -- merged into one list. Reached at the end of the
    /// Shift-Tab cycle or via the dedicated `OpenWorkspaceFileFinder` action.
    /// Rows carry the owning workspace's basename so same-named files stay
    /// distinguishable, and selecting one opens it in the current workspace.
    AllWorkspaces,
}

impl FinderScope {
    /// The stable name under which this scope is remembered across sessions,
    /// or `None` for a scope that is never persisted.
    ///
    /// [`FinderScope::Buffers`] and [`FinderScope::ModifiedBuffers`] return
    /// `None`. They make up a dedicated picker, not a sticky mode, so closing
    /// in either leaves the prior remembered scope intact.
    ///
    /// [`FinderScope::Diff`] returns `None` as well. Its rows come from the
    /// diff under the finder, so an open over a plain editor has none to list.
    pub(crate) fn persist_name(&self) -> Option<String> {
        match self {
            FinderScope::All => Some("all".to_string()),
            FinderScope::Modified => Some("modified".to_string()),
            FinderScope::Named(name) => Some(name.clone()),
            FinderScope::AllWorkspaces => Some("allworkspaces".to_string()),
            FinderScope::Buffers | FinderScope::ModifiedBuffers | FinderScope::Diff => None,
        }
    }

    /// Resolve a remembered or configured scope name against the current named
    /// scopes, or `None` when the name matches nothing valid.
    ///
    /// A persisted name whose scope has since been removed from config must
    /// not resurrect, so validation returns `None` and the caller falls
    /// through to its next default. `"buffers"`, `"modified_buffers"`, and
    /// `"diff"` are intentionally not accepted, as none is ever a sticky or
    /// default scope.
    pub(crate) fn from_persist_name(
        name: &str,
        named: &BTreeMap<String, Vec<String>>,
    ) -> Option<FinderScope> {
        match name {
            "all" => Some(FinderScope::All),
            "modified" => Some(FinderScope::Modified),
            "allworkspaces" => Some(FinderScope::AllWorkspaces),
            other if named.contains_key(other) => Some(FinderScope::Named(other.to_string())),
            _ => None,
        }
    }

    /// The `scope` keymap predicate value for this scope.
    ///
    /// The two buffer scopes and [`FinderScope::Diff`] have names here although
    /// they never persist, since a binding scoped to one of them has to name
    /// it. A named scope reads as its config name.
    pub(crate) fn context_name(&self) -> &str {
        match self {
            FinderScope::All => "all",
            FinderScope::Modified => "modified",
            FinderScope::Diff => "diff",
            FinderScope::Buffers => "buffers",
            FinderScope::ModifiedBuffers => "modified_buffers",
            FinderScope::Named(name) => name,
            FinderScope::AllWorkspaces => "allworkspaces",
        }
    }
}

/// What the finder should do with the selected file when the user submits.
/// Set at open time; consumed by the submit handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenIntent {
    /// Open the file in the focused pane, replacing its current view.
    Replace,
    /// Split the focused pane horizontally first, then open the file in
    /// the new pane below.
    HSplit,
    /// Split the focused pane vertically first, then open the file in
    /// the new pane to the right.
    VSplit,
}

/// The finder's directory-browse mode, live while the query starts with `/`
/// or `~/`.
///
/// A separate [`PathPicker`] walks `root`, leaving the workspace `core` and its
/// scope bases untouched underneath so backspacing out of the prefix restores
/// the workspace list. Rows display under `typed_dir` (the query up to and
/// including the last `/`), filtered by `partial` (the text after it).
pub(crate) struct Browse {
    pub(crate) typed_dir: String,
    pub(crate) root: PathBuf,
    pub(crate) partial: String,
    pub(crate) picker: PathPicker,
    /// The fallback of ignored files under [`Self::root`], walked by
    /// [`FsHost::walk_all_files_streaming`].
    ///
    /// The finder installs it once [`Self::picker`] lists nothing, and keeps it
    /// until the browse re-roots or ends. The palette's directory browse lists
    /// directories and never takes one.
    pub(crate) ignored: Option<PathPicker>,
}

impl Browse {
    /// Return the preview slots of the browse picker and its fallback to the
    /// workspace.
    fn dispose(&self, ws: &mut Workspace) {
        self.picker.dispose(ws);
        if let Some(ignored) = &self.ignored {
            ignored.dispose(ws);
        }
    }
}

/// The workspace file list a walk collected, held for whatever asks next.
///
/// Lives on `Stoat` rather than in the finder, since its whole point is to
/// outlive one. See [`crate::app::Stoat::finder_path_cache`].
///
/// The list is behind an `Arc` because the code-search modal walks the same
/// tree and reads the same answer, so the two hand it over for a refcount
/// rather than a copy. A finder open still takes it by value, unwrapping the
/// `Arc` where it holds the only reference.
pub(crate) struct FinderPathCache {
    /// The single root the paths were walked under. A finder opening on a
    /// different workspace walks its own tree rather than reading these.
    pub(crate) root: PathBuf,
    pub(crate) paths: Arc<Vec<PathBuf>>,
    /// The [`crate::app::Stoat::finder_path_epoch`] the walk that produced
    /// `paths` began under, which is what the next open compares against.
    pub(crate) epoch: u64,
    /// The display rows and order derived for a prefix of `paths`, so the
    /// next open lists them rather than deriving a row per path on the loop.
    ///
    /// A path that leaves the list shifts every row past it, so a departure
    /// drops these. An arrival appends past them and leaves them standing.
    pub(crate) display: Option<DisplayCache>,
}

pub struct FileFinder {
    pub input: InputView,
    /// What submit should do with the selected file.
    pub(crate) open_intent: OpenIntent,
    /// The scope on show, the lists that its scopes read, and the named scopes.
    pub scopes: FileScopes,
    /// The [`crate::app::Stoat::finder_path_epoch`] this finder's walk started
    /// under, carried into [`FinderPathCache`] on close.
    ///
    /// Stamped at walk start rather than at close because a removal arriving
    /// mid-session bumps that counter after the walk had already collected the
    /// path it removed. Comparing the start stamp is what keeps that list out of
    /// the cache. A re-root keeps the older stamp, which can only under-report
    /// freshness and so is safe to leave alone.
    pub(crate) walk_epoch: u64,
    /// The shared walk / fuzzy-list / preview core. Its `all_paths` is the
    /// [`FinderScope::All`] base; Modified, Diff, and the two buffer scopes feed
    /// their own vecs through [`PathPicker::refilter_with_base`]. A scope toggle
    /// [`PathPicker::invalidate`]s it to force a re-run under an unchanged
    /// query.
    pub(crate) core: PathPicker,
    /// The fallback of ignored files under the workspace root, walked by
    /// [`FsHost::walk_all_files_streaming`] for [`FinderScope::All`].
    ///
    /// Installed once [`Self::core`] lists nothing in that scope, and kept until
    /// the finder closes. A flip to another scope leaves it in place, so a
    /// return to All reuses it.
    pub(crate) ignored: Option<PathPicker>,
    /// Active directory-browse mode, or `None` for the normal workspace list.
    pub(crate) browse: Option<Browse>,
    /// Rows the modal needs to show the whole unfiltered list, which the
    /// renderer sizes the box's height against.
    ///
    /// Derived from the candidate base rather than the filtered rows, so
    /// narrowing a query never moves the box out from under the user typing it.
    /// The base grows while the background walk streams in, so a large workspace
    /// settles at its full size over the first frames after opening.
    pub content_rows: u16,
    /// The list columns of the plate, measured when the rows of the list last
    /// changed, or `None` before the first measure.
    ///
    /// A filter change refits the plate wider or narrower, and a selection
    /// move keeps it. Each open starts with no fit.
    ///
    /// See also:
    /// - [`refit_finder_plate`](crate::render::file_finder::refit_finder_plate), which the
    ///   background pump calls to measure it.
    pub(crate) plate_fit: Option<PlateFit>,
    /// The cell and scale that the last frame laid the list out by, or `None`
    /// for a list on whole cell rows.
    ///
    /// The pool that pages a pixel list and the hit test read the layout after
    /// the paint, so the paint keeps its measure here, as it keeps
    /// `viewport_rows`.
    pub(crate) row_metrics: Option<RowMetrics>,
}

/// Source tags for [`base_id`], so two scopes carrying the same generation
/// never name each other's list.
const MODIFIED_BASE: u8 = 0;
const BUFFERS_BASE: u8 = 1;
const NAMED_BASE: u8 = 2;
const DIRTY_BUFFERS_BASE: u8 = 3;
const DIFF_BASE: u8 = 4;

/// Name a capped scope's candidate list for [`PathPicker::refilter_with_base`].
///
/// `source` says which list, and `generation` which build of it. Together they
/// hold still for as long as the list does, which is what lets the picker keep
/// the rows it derived from it across a keystroke.
fn base_id(source: u8, generation: u64, len: usize) -> BaseId {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    generation.hash(&mut hasher);
    BaseId {
        identity: hasher.finish(),
        len,
    }
}

/// The walked paths a named scope's globset accepted, and how much of the walk
/// has been tested.
///
/// The walk streams in batches. Re-globbing everything collected so far on each
/// one would cost the whole list per batch, so matches accumulate and only the
/// paths past [`Self::consumed`] are tested.
pub(crate) struct NamedCache {
    /// Scope name these matches belong to. A different name means a different
    /// globset, so the cache starts over.
    name: String,
    /// Paths accepted so far, in walk order.
    filtered: Vec<PathBuf>,
    /// Which build of this cache [`Self::filtered`] belongs to.
    ///
    /// The list only ever grows within one build, so the pick list can hold the
    /// rows it derived for it. A rebuild starts a different list under the same
    /// scope name, and this is what says so.
    epoch: u64,
    /// How many entries of the walk have been tested.
    ///
    /// Only meaningful while the walk grows by appending. A re-root clears the
    /// walked paths, which is why a count past the end forces a rebuild rather
    /// than slicing past the list.
    consumed: usize,
}

impl FileFinder {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        open_intent: OpenIntent,
        initial_scope: FinderScope,
        git_root: PathBuf,
        walk: Option<(UnboundedReceiver<Vec<PathBuf>>, Task<()>)>,
        seed_paths: Vec<PathBuf>,
        seed_display: Option<DisplayCache>,
        walk_epoch: u64,
        modified: (UnboundedReceiver<Vec<PathBuf>>, Task<()>),
        diff_paths: Option<Vec<PathBuf>>,
        buffer_paths: Vec<PathBuf>,
        dirty_buffer_paths: Vec<PathBuf>,
        finder_scopes: &BTreeMap<String, Vec<String>>,
    ) -> Self {
        let input = InputView::create(ws, executor.clone(), SubmitTarget::FileFinder, "", "insert");
        let mut core = PathPicker::new(ws, executor, git_root, walk);
        core.all_paths = seed_paths;
        core.walk_display = seed_display;

        let mut finder = Self {
            input,
            open_intent,
            scopes: FileScopes::new(
                initial_scope,
                modified,
                diff_paths,
                buffer_paths,
                dirty_buffer_paths,
                finder_scopes,
            ),
            walk_epoch,
            core,
            ignored: None,
            browse: None,
            content_rows: 0,
            plate_fit: None,
            row_metrics: None,
        };
        // Uniformly seed the initial (empty-query) list for whatever scope
        // opened, including a named scope's glob filter.
        finder.refilter_from_input(ws);
        finder
    }

    pub(crate) fn scope(&self) -> &FinderScope {
        &self.scopes.scope
    }

    /// The picker currently driving the list. Browse mode (a `/` or `~/`
    /// query) swaps in its own directory-walk picker; every other query drives
    /// the workspace `core`. Either gives way to its fallback of ignored files
    /// while [`Self::fallback_active`] holds.
    pub fn active_core(&mut self) -> &mut PathPicker {
        let fallback_active = self.fallback_active();
        match self.list_and_fallback_mut() {
            (_, Some(fallback)) if fallback_active => fallback,
            (list, _) => list,
        }
    }

    pub fn active_core_ref(&self) -> &PathPicker {
        match self.list_and_fallback() {
            (_, Some(fallback)) if self.fallback_active() => fallback,
            (list, _) => list,
        }
    }

    /// Whether the list on display holds no row for the query, with every path
    /// delivered.
    ///
    /// The finder then names the query in its list and shows no preview. A
    /// walk that still streams does not count, so a fresh open does not show
    /// that message before the first paths arrive. A scan for a longer query
    /// keeps the answer, so the message stays while the user types on past
    /// the last match.
    pub(crate) fn shows_no_match(&self) -> bool {
        self.active_core_ref().rows_empty()
    }

    /// Whether the fallback of ignored files stands in for the list.
    ///
    /// Only browse and [`FinderScope::All`] fall back. A Modified or buffer list
    /// that holds nothing keeps its empty rows, and a fallback built under All
    /// stays through a flip away, so a return to All reuses it.
    pub(crate) fn fallback_active(&self) -> bool {
        if self.browse.is_none() && self.scopes.scope != FinderScope::All {
            return false;
        }
        let (list, fallback) = self.list_and_fallback();
        fallback.is_some() && list.lists_nothing()
    }

    /// The root to walk for a fallback of ignored files, when the list on
    /// display needs one and has none.
    ///
    /// `None` outside browse and [`FinderScope::All`], once a fallback exists,
    /// and until [`PathPicker::lists_nothing`] holds for the list.
    pub(crate) fn wants_ignored_fallback(&self) -> Option<PathBuf> {
        let (list, fallback) = self.list_and_fallback();
        if fallback.is_some() || !list.lists_nothing() {
            return None;
        }
        match &self.browse {
            Some(browse) => Some(browse.root.clone()),
            None if self.scopes.scope == FinderScope::All => Some(self.core.git_root.clone()),
            None => None,
        }
    }

    /// Set `picker` as the fallback of the list on display.
    pub(crate) fn install_fallback(&mut self, picker: PathPicker) {
        match &mut self.browse {
            Some(browse) => browse.ignored = Some(picker),
            None => self.ignored = Some(picker),
        }
    }

    /// The picker a scan for `target` reports back to.
    ///
    /// # Panics
    ///
    /// Panics for [`ScanTarget::Fallback`] when the list on display has no
    /// fallback. [`Self::refilter_from_input`] hands out no such scan.
    pub(crate) fn scan_target_mut(&mut self, target: ScanTarget) -> &mut PathPicker {
        match (target, self.list_and_fallback_mut()) {
            (ScanTarget::List, (list, _)) => list,
            (ScanTarget::Fallback, (_, fallback)) => {
                fallback.expect("a fallback scan has a fallback to land in")
            },
        }
    }

    /// The list on display, browse or workspace, and the fallback that stands
    /// in for it.
    fn list_and_fallback(&self) -> (&PathPicker, Option<&PathPicker>) {
        match &self.browse {
            Some(browse) => (&browse.picker, browse.ignored.as_ref()),
            None => (&self.core, self.ignored.as_ref()),
        }
    }

    fn list_and_fallback_mut(&mut self) -> (&mut PathPicker, Option<&mut PathPicker>) {
        match &mut self.browse {
            Some(browse) => (&mut browse.picker, browse.ignored.as_mut()),
            None => (&mut self.core, self.ignored.as_mut()),
        }
    }

    /// Absolute path of the currently selected filtered row, if any.
    pub(crate) fn selected_path(&self) -> Option<&Path> {
        self.active_core_ref().selected_path()
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        self.active_core().move_selection(delta);
    }

    /// Flip the scope of [`Self::core`], as [`FileScopes::toggle`] flips it.
    pub(crate) fn toggle_scope(&mut self) {
        self.scopes.toggle(&mut self.core);
    }

    /// Point the Modified list at a freshly spawned git-status query, as
    /// [`FileScopes::set_modified_source`] does.
    pub(crate) fn set_modified_source(
        &mut self,
        rx: UnboundedReceiver<Vec<PathBuf>>,
        task: Task<()>,
    ) {
        self.scopes.set_modified_source(rx, task);
    }

    /// The scopes that the header names as pills, as
    /// [`FileScopes::scope_cycle`] orders them.
    pub(crate) fn scope_cycle(&self) -> Vec<FinderScope> {
        self.scopes.scope_cycle()
    }

    /// Re-run the matcher if the input text or scope has changed since last
    /// filter. Called from the renderer so typing picks up without a dedicated
    /// sync hook. Drains any pending walk result first so freshly arrived
    /// paths participate in the same render tick.
    ///
    /// The fallback of ignored files refilters only while the list it stands in
    /// for shows no row, or while its own walk still streams. Each scan handed
    /// back names the picker it reports to.
    pub(crate) fn refilter_from_input(&mut self, ws: &Workspace) -> Vec<PendingScan> {
        // Ahead of the browse branch so a git-status answer landing while a
        // directory query is typed is taken rather than left in the channel.
        self.scopes.pump_modified(&mut self.core);
        let mut pending = Vec::new();

        let fallback_query = if let Some(browse) = &mut self.browse {
            pump_capped_walk(&mut browse.picker);
            browse.picker.pump_scan();
            // A directory walk runs to the same cap as the repo walk, so
            // ranking it belongs on a worker for the same reason.
            pending.extend(ScanTarget::List.tag(browse.picker.begin_scan(&browse.partial)));
            Some(browse.partial.clone())
        } else {
            self.core.pump_walk();
            self.core.pump_scan();
            let text = self.input.text(ws);
            pending.extend(ScanTarget::List.tag(self.scopes.refilter(&mut self.core, &text)));
            (self.scopes.scope == FinderScope::All).then_some(text)
        };

        if let Some(query) = fallback_query {
            self.refilter_fallback(&query, &mut pending);
        }
        self.remeasure_content();
        pending
    }

    /// Bring the fallback of the list on display up to date with `query`, and
    /// hand back its scan.
    ///
    /// The fallback scans only while the list shows no row, or while its own
    /// walk still streams. Scans during the walk derive display rows only for
    /// the paths that arrived, never for the whole walk in one turn.
    ///
    /// Otherwise the fallback forgets its results and costs nothing per
    /// keystroke. When it stands in again, it shows no row for one turn rather
    /// than the rows of an older query.
    fn refilter_fallback(&mut self, query: &str, out: &mut Vec<PendingScan>) {
        let (list, Some(fallback)) = self.list_and_fallback_mut() else {
            return;
        };
        pump_capped_walk(fallback);
        fallback.pump_scan();
        if !list.rows_empty() && !fallback.walking() {
            fallback.forget_results();
            return;
        }
        out.extend(ScanTarget::Fallback.tag(fallback.begin_scan(query)));
    }

    /// Bring the rows up to date with `query` on this thread.
    ///
    /// Each scope falls behind the query in its own way. A scope that scans
    /// elsewhere answers the query that started its scan. A list held in
    /// memory refilters only when a frame runs, and keys typed in one burst
    /// reach an action with no frame between them.
    ///
    /// A named scope catches up against the cache it filters rather than the
    /// walk, which holds a different set and would answer a different question.
    ///
    /// A fallback of ignored files settles only when its list settles to no
    /// row, so an action reads the row on display whichever of the two shows.
    pub(crate) fn settle_scan(&mut self, query: &str) {
        // Browse filters the directory walk against its own partial rather than
        // the typed query, the leading directory having already been consumed
        // by the walk's root.
        if let Some(browse) = &mut self.browse {
            let partial = browse.partial.clone();
            browse.picker.settle_scan(&partial);
            if browse.picker.lists_nothing()
                && let Some(ignored) = &mut browse.ignored
            {
                ignored.settle_scan(&partial);
            }
            return;
        }
        self.scopes.settle(&mut self.core, query);
        if self.scopes.scope == FinderScope::All
            && self.core.lists_nothing()
            && let Some(ignored) = &mut self.ignored
        {
            ignored.settle_scan(query);
        }
    }

    /// Re-derive [`Self::content_rows`] from the active picker's candidate base.
    ///
    /// The base is the scope's whole list, not the filtered rows, so this lands
    /// the same answer for every query and only moves when the walk delivers or
    /// the scope flips. A list longer than [`u16::MAX`] just asks for the
    /// tallest box there is.
    fn remeasure_content(&mut self) {
        let rows = u16::try_from(self.active_core_ref().picklist.base.len()).unwrap_or(u16::MAX);
        self.content_rows = rows.saturating_add(FINDER_CHROME_ROWS);
    }

    /// Sync the preview pane to the current selection. Clears the pane when
    /// nothing is selected.
    ///
    /// In [`FinderScope::Buffers`] and [`FinderScope::ModifiedBuffers`] the
    /// selection previews the live, possibly modified in-memory buffer. Every
    /// other scope reads the file from disk. A buffer selection whose path has
    /// no open buffer falls back to the disk file.
    pub(crate) fn sync_preview(
        &mut self,
        ws: &mut Workspace,
        fs_host: &dyn FsHost,
        language_registry: &stoat_language::LanguageRegistry,
    ) {
        let policy = if self.browse.is_some() {
            PreviewPolicy::File
        } else if matches!(
            self.scopes.scope,
            FinderScope::Buffers | FinderScope::ModifiedBuffers
        ) {
            PreviewPolicy::LiveBufferThenFile
        } else {
            PreviewPolicy::File
        };
        self.active_core()
            .sync_preview(ws, fs_host, language_registry, policy);
    }

    /// Tear down owned editor slots. Called on every finder-close path.
    /// Removes the preview buffer from [`crate::buffer_registry::BufferRegistry`]
    /// so each file finder lifetime returns the registry to its
    /// pre-open size; without this the preview entry would accumulate
    /// across opens.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
        self.core.dispose(ws);
        if let Some(ignored) = &self.ignored {
            ignored.dispose(ws);
        }
        if let Some(browse) = &self.browse {
            browse.dispose(ws);
        }
    }

    /// Leave directory-browse mode, disposing the previews of the browse picker
    /// and its fallback so the registry returns to its pre-browse size. No-op
    /// when not browsing.
    pub(crate) fn leave_browse(&mut self, ws: &mut Workspace) {
        if let Some(browse) = self.browse.take() {
            browse.dispose(ws);
        }
    }
}

/// The scope state of a file list.
///
/// It holds the scope on show, the lists that the listed scopes read, and the
/// config's named scopes with their cache. [`FileFinder`] owns one over its
/// workspace list. Each method that filters takes the [`PathPicker`] that it
/// filters, so another owner of a file list passes its own picker.
pub struct FileScopes {
    pub(crate) scope: FinderScope,
    /// Absolute paths of currently-modified files. Re-queried on scope toggle.
    ///
    /// Empty until the first background query lands, and thereafter the
    /// previous answer while a re-query is in flight, so a refresh never blinks
    /// the rows empty.
    pub modified_paths: Vec<PathBuf>,
    /// The in-flight git-status query feeding [`Self::modified_paths`], and the
    /// task running it.
    ///
    /// The query is a full status pass with untracked recursion, far too slow
    /// to run where a keystroke waits on it, so it runs on the blocking pool and
    /// reports back here. `None` once its answer has been taken. The task is
    /// held so dropping the finder drops the query with it.
    modified: Option<(UnboundedReceiver<Vec<PathBuf>>, Task<()>)>,
    /// Absolute paths of the [`FinderScope::Diff`] base, or `None` when the
    /// finder opened over no diff. Captured once at open time.
    pub(crate) diff_paths: Option<Vec<PathBuf>>,
    /// Which generation of [`Self::modified_paths`], [`Self::diff_paths`],
    /// [`Self::buffer_paths`], and [`Self::dirty_buffer_paths`] the pick list
    /// shows.
    ///
    /// The lists are fixed between the moments they are replaced, so the
    /// picker only has to be told when one is, and otherwise keeps the rows it
    /// derived from them. Restamped wherever one is rebuilt.
    ///
    /// A row's mark reads these lists too, so the pool that pages the list
    /// feeds this into its content version, and a list that lands after a
    /// page filled refills that page.
    pub base_generation: u64,
    /// Absolute paths of currently-open buffers. Captured once at open time;
    /// not re-queried on scope toggle.
    pub(crate) buffer_paths: Vec<PathBuf>,
    /// Absolute paths of the open buffers with unsaved edits, the
    /// [`FinderScope::ModifiedBuffers`] base. Captured once at open time like
    /// [`Self::buffer_paths`].
    ///
    /// Sorted, because the row paint finds a row's dot here by binary search.
    pub(crate) dirty_buffer_paths: Vec<PathBuf>,
    /// Config-defined named scopes, compiled at open time in alphabetical
    /// (BTreeMap) order. Shift-Tab cycles through them after Modified.
    pub(crate) named_scopes: Vec<(String, GlobSet)>,
    /// Glob-filtered base for the active [`FinderScope::Named`] scope, or
    /// `None` before one is built.
    pub(crate) named_cache: Option<NamedCache>,
}

impl FileScopes {
    /// Build the scope state of a list that opens on `scope`.
    ///
    /// `modified` is the git-status query that feeds the Modified list, and
    /// `finder_scopes` holds the glob lists of the config's named scopes.
    pub(crate) fn new(
        scope: FinderScope,
        modified: (UnboundedReceiver<Vec<PathBuf>>, Task<()>),
        diff_paths: Option<Vec<PathBuf>>,
        buffer_paths: Vec<PathBuf>,
        dirty_buffer_paths: Vec<PathBuf>,
        finder_scopes: &BTreeMap<String, Vec<String>>,
    ) -> Self {
        Self {
            scope,
            modified_paths: Vec::new(),
            modified: Some(modified),
            diff_paths,
            base_generation: crate::picker::next_generation(),
            buffer_paths,
            dirty_buffer_paths,
            named_scopes: compile_named_scopes(finder_scopes),
            named_cache: None,
        }
    }

    /// Flip the scope and make `core` rerun its filter against the new base.
    ///
    /// Landing on [`FinderScope::Modified`] wants a fresh git status, but that
    /// query is the caller's to spawn via [`Self::set_modified_source`], since
    /// it belongs off this thread.
    ///
    /// The file scopes and the buffer scopes are separate cycles. The file
    /// scopes go All -> Modified -> each named scope -> AllWorkspaces -> All.
    /// A finder over a diff has [`FinderScope::Diff`] in Modified's place.
    /// [`FinderScope::Buffers`] and [`FinderScope::ModifiedBuffers`] flip with
    /// each other, so the buffer picker never enters a file scope. A finder
    /// reaches the buffer pair only through the dedicated `OpenBufferPicker`
    /// action.
    pub(crate) fn toggle(&mut self, core: &mut PathPicker) {
        self.scope = self.next_scope();
        core.picklist.selected = 0;
        // Force refilter + preview resync on next render against the new base.
        core.invalidate();
    }

    /// Point the Modified list at a freshly spawned git-status query, replacing
    /// any query still in flight.
    ///
    /// The paths already shown stay until the new answer lands, so a refresh
    /// never blinks the rows empty.
    pub(crate) fn set_modified_source(
        &mut self,
        rx: UnboundedReceiver<Vec<PathBuf>>,
        task: Task<()>,
    ) {
        self.modified = Some((rx, task));
    }

    /// Take the background git-status answer if it has arrived, replacing the
    /// modified list.
    ///
    /// Restamping the generation alone does not show the new list. The
    /// Modified base id keys on that generation, but
    /// [`PathPicker::refilter_with_base`] returns early while the query is
    /// unchanged and the filter still valid, which is exactly the state of
    /// `core` after an open. Invalidating `core` is what that method exists
    /// for. Only the scope actually reading the list needs it, since every
    /// other scope re-runs from [`Self::toggle`]'s own invalidate on the way
    /// in.
    pub(crate) fn pump_modified(&mut self, core: &mut PathPicker) {
        let Some((rx, _)) = self.modified.as_mut() else {
            return;
        };
        let Ok(paths) = rx.try_recv() else {
            return;
        };
        self.modified = None;
        self.modified_paths = paths;
        self.base_generation = crate::picker::next_generation();
        if self.scope == FinderScope::Modified {
            core.invalidate();
        }
    }

    /// The scope Shift-Tab lands on next.
    ///
    /// The file scopes cycle All -> Modified -> each named scope (alphabetical)
    /// -> AllWorkspaces -> All. A finder over a diff has Diff in Modified's
    /// place. Buffers and ModifiedBuffers flip with each other.
    pub(crate) fn next_scope(&self) -> FinderScope {
        self.next_scope_from(&self.scope)
    }

    /// The scopes that the header names as pills, in the order Shift-Tab walks
    /// them.
    ///
    /// The walk starts at [`FinderScope::Buffers`] for the buffer picker and at
    /// [`FinderScope::All`] for every file scope, and it steps until it comes
    /// back to its start. The header always names the scope on show, so a
    /// scope that the walk does not reach, such as a [`FinderScope::Modified`]
    /// that an open forced over a diff, takes the place after the first pill.
    pub(crate) fn scope_cycle(&self) -> Vec<FinderScope> {
        let start = match self.scope {
            FinderScope::Buffers | FinderScope::ModifiedBuffers => FinderScope::Buffers,
            _ => FinderScope::All,
        };
        let mut cycle = vec![start.clone()];
        let mut scope = self.next_scope_from(&start);
        // The walk returns to its start. The bound ends a walk that does not,
        // so the header still paints.
        while scope != start && cycle.len() <= self.named_scopes.len() + 4 {
            let next = self.next_scope_from(&scope);
            cycle.push(scope);
            scope = next;
        }
        if !cycle.contains(&self.scope) {
            cycle.insert(1, self.scope.clone());
        }
        cycle
    }

    /// Refilter `core` against the scope's base, handing back the scan of a
    /// scope that ranks on a worker.
    pub(crate) fn refilter(&mut self, core: &mut PathPicker, text: &str) -> Option<(u64, Scan)> {
        match self.scope.clone() {
            // The uncapped scopes are the whole repo walk, so their scan goes to
            // a worker and the caller spawns what this hands back.
            FinderScope::All | FinderScope::AllWorkspaces => core.begin_scan(text),
            FinderScope::Modified
            | FinderScope::Diff
            | FinderScope::Buffers
            | FinderScope::ModifiedBuffers => {
                self.refilter_listed(core, text);
                None
            },
            // A glob over the whole walk keeps up to all of it, so this scans
            // elsewhere like the uncapped scopes. A base small enough not to
            // need it costs one hop through the worker instead.
            FinderScope::Named(name) => {
                self.sync_named_cache(core, &name);
                // `core` is no part of the scopes, so the matcher reads the
                // cached base in place rather than cloning it.
                match &self.named_cache {
                    Some(cache) => {
                        let id = base_id(NAMED_BASE, cache.epoch, cache.filtered.len());
                        core.begin_scan_with_base(text, &cache.filtered, id)
                    },
                    None => None,
                }
            },
        }
    }

    /// Bring the rows of `core` up to date with `query` on this thread, by the
    /// scope on show.
    ///
    /// A named scope catches up against the cache it filters rather than the
    /// walk, which holds a different set of paths. A listed scope first takes
    /// a git-status answer that has landed.
    pub(crate) fn settle(&mut self, core: &mut PathPicker, query: &str) {
        match self.scope.clone() {
            FinderScope::All | FinderScope::AllWorkspaces => core.settle_scan(query),
            FinderScope::Named(name) => {
                self.sync_named_cache(core, &name);
                if let Some(cache) = &self.named_cache {
                    let id = base_id(NAMED_BASE, cache.epoch, cache.filtered.len());
                    core.settle_scan_with_base(query, &cache.filtered, id);
                }
            },
            FinderScope::Modified
            | FinderScope::Diff
            | FinderScope::Buffers
            | FinderScope::ModifiedBuffers => {
                self.pump_modified(core);
                self.refilter_listed(core, query);
            },
        }
    }

    /// The scope Shift-Tab lands on from `from`, by the cycle that
    /// [`Self::next_scope`] describes.
    fn next_scope_from(&self, from: &FinderScope) -> FinderScope {
        match from {
            FinderScope::All if self.diff_paths.is_some() => FinderScope::Diff,
            FinderScope::All => FinderScope::Modified,
            FinderScope::Modified | FinderScope::Diff => self
                .named_scopes
                .first()
                .map(|(name, _)| FinderScope::Named(name.clone()))
                .unwrap_or(FinderScope::AllWorkspaces),
            FinderScope::Named(current) => self
                .named_scopes
                .iter()
                .position(|(name, _)| name == current)
                .and_then(|idx| self.named_scopes.get(idx + 1))
                .map(|(name, _)| FinderScope::Named(name.clone()))
                .unwrap_or(FinderScope::AllWorkspaces),
            FinderScope::AllWorkspaces => FinderScope::All,
            FinderScope::Buffers => FinderScope::ModifiedBuffers,
            FinderScope::ModifiedBuffers => FinderScope::Buffers,
        }
    }

    /// Refilter `core` on this thread against the modified files, the diff's
    /// files, the open buffers, or the modified buffers, whichever list the
    /// scope shows.
    fn refilter_listed(&self, core: &mut PathPicker, query: &str) {
        // The tag stops two lists that share the finder's generation from
        // reading as each other. A scope flip invalidates too, but the tag does
        // not depend on that.
        let (base, id): (&[PathBuf], BaseId) = match &self.scope {
            FinderScope::Modified => (
                &self.modified_paths,
                base_id(
                    MODIFIED_BASE,
                    self.base_generation,
                    self.modified_paths.len(),
                ),
            ),
            FinderScope::Diff => {
                let paths = self.diff_paths.as_deref().unwrap_or_default();
                (paths, base_id(DIFF_BASE, self.base_generation, paths.len()))
            },
            FinderScope::Buffers => (
                &self.buffer_paths,
                base_id(BUFFERS_BASE, self.base_generation, self.buffer_paths.len()),
            ),
            FinderScope::ModifiedBuffers => (
                &self.dirty_buffer_paths,
                base_id(
                    DIRTY_BUFFERS_BASE,
                    self.base_generation,
                    self.dirty_buffer_paths.len(),
                ),
            ),
            FinderScope::All | FinderScope::AllWorkspaces | FinderScope::Named(_) => return,
        };
        core.refilter_with_base(query, base, id);
    }

    /// Bring [`Self::named_cache`] up to date with the walk of `core` under
    /// `name`.
    ///
    /// Tests only the paths the cache has not seen, so a streaming walk costs
    /// each path once across every batch rather than the whole list per batch.
    /// A scope name with no compiled globset matches nothing, and still marks
    /// its paths seen so they are not retested.
    fn sync_named_cache(&mut self, core: &PathPicker, name: &str) {
        let walked = core.all_paths.len();
        let reusable = self
            .named_cache
            .as_ref()
            .is_some_and(|cache| cache.name == name && cache.consumed <= walked);
        if !reusable {
            self.named_cache = Some(NamedCache {
                name: name.to_string(),
                filtered: Vec::new(),
                consumed: 0,
                epoch: crate::picker::next_generation(),
            });
        }

        let cache = self.named_cache.as_mut().expect("built above when absent");
        if cache.consumed == walked {
            return;
        }

        let Some((_, globset)) = self.named_scopes.iter().find(|(n, _)| n == name) else {
            cache.consumed = walked;
            return;
        };

        // Resolved once for the batch. `display_relative` reads the environment
        // and allocates for the home directory on every call.
        let home = paths::home_dir();
        let git_root = &core.git_root;
        for path in &core.all_paths[cache.consumed..] {
            let display = paths::display_relative_with_home(path, git_root, home.as_deref());
            if globset.is_match(display) {
                cache.filtered.push(path.clone());
            }
        }
        cache.consumed = walked;
    }
}

/// Drain `picker`'s walk, stopping it once it holds [`BROWSE_PATH_CAP`] paths.
///
/// Returns whether a batch arrived, as [`PathPicker::pump_walk`] does. A
/// directory browse and a fallback of ignored files walk outside the workspace
/// ignore rules, so they go through this rather than the plain pump.
pub(crate) fn pump_capped_walk(picker: &mut PathPicker) -> bool {
    let pumped = picker.pump_walk();
    if picker.all_paths.len() >= BROWSE_PATH_CAP {
        picker.all_paths.truncate(BROWSE_PATH_CAP);
        picker.stop_walk();
    }
    pumped
}

/// Split a `/` or `~/` path query into its directory and fuzzy partial.
///
/// The query splits at its last `/`. The part up to and including the slash is
/// the `typed_dir` shown before each row and, once `~` is resolved via `home`,
/// the absolute directory to walk. The part after is the fuzzy `partial`.
/// Returns `None` for a non-path query or a `~/` query with no `home`.
pub(crate) fn split_path_query(
    query: &str,
    home: Option<&str>,
) -> Option<(String, PathBuf, String)> {
    let last_slash = query.rfind('/')?;
    let typed_dir = &query[..=last_slash];
    let partial = query[last_slash + 1..].to_string();
    let root = if let Some(after) = typed_dir.strip_prefix("~/") {
        PathBuf::from(home?).join(after)
    } else if typed_dir.starts_with('/') {
        PathBuf::from(typed_dir)
    } else {
        return None;
    };
    Some((typed_dir.to_string(), root, partial))
}

/// Compile the config's named finder scopes into globsets, in BTreeMap
/// (alphabetical) order so Shift-Tab cycles them predictably.
///
/// Invalid globs and unbuildable sets are warn-logged and skipped, so one bad
/// pattern never breaks the finder.
fn compile_named_scopes(finder_scopes: &BTreeMap<String, Vec<String>>) -> Vec<(String, GlobSet)> {
    finder_scopes
        .iter()
        .filter_map(|(name, globs)| {
            let mut builder = GlobSetBuilder::new();
            for pattern in globs {
                match Glob::new(pattern) {
                    Ok(glob) => {
                        builder.add(glob);
                    },
                    Err(err) => tracing::warn!(
                        target: "stoat::finder",
                        scope = %name,
                        glob = %pattern,
                        %err,
                        "invalid finder scope glob, skipping"
                    ),
                }
            }
            match builder.build() {
                Ok(globset) => Some((name.clone(), globset)),
                Err(err) => {
                    tracing::warn!(
                        target: "stoat::finder",
                        scope = %name,
                        %err,
                        "invalid finder scope globset, skipping"
                    );
                    None
                },
            }
        })
        .collect()
}

/// Query git for currently-modified files (staged + unstaged), returning
/// absolute paths. Empty when no repo or no changes.
pub(crate) fn query_modified(git_host: &dyn GitHost, git_root: &Path) -> Vec<PathBuf> {
    let Some(repo) = git_host.discover(git_root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = repo.changed_files().into_iter().map(|c| c.path).collect();
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::ModalKind, test_fixture, test_harness::keys};
    use crossterm::event::{Event, KeyCode};
    use ratatui::style::{Color, Modifier};

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Path used as the workspace root in walker unit tests. Every entry
    /// inserted into the FakeFs lives under this prefix; the helper below
    /// strips it so assertions compare repo-relative paths.
    const WALK_ROOT: &str = "/repo";

    fn seeded_fake_fs(files: &[(&str, &str)]) -> crate::host::FakeFs {
        let fs = crate::host::FakeFs::new();
        let root = Path::new(WALK_ROOT);
        fs.insert_files(
            files
                .iter()
                .map(|(rel, content)| (root.join(rel), content.as_bytes())),
        );
        fs
    }

    fn walked_rels(fs: &dyn FsHost) -> Vec<String> {
        let root = Path::new(WALK_ROOT);
        let mut rels: Vec<String> = fs
            .walk_workspace_files(root)
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        rels.sort();
        rels
    }

    #[test]
    fn walk_workspace_files_returns_files_not_dirs() {
        let fs = seeded_fake_fs(&[("a.rs", "a"), ("sub/b.rs", "b")]);
        assert_eq!(walked_rels(&fs), vec!["a.rs", "sub/b.rs"]);
    }

    #[test]
    fn walk_workspace_files_ignores_dot_git() {
        let fs = seeded_fake_fs(&[
            (".git/HEAD", "ref: refs/heads/main"),
            (".git/config", "[core]"),
            (".git/refs/heads/main", "deadbeef"),
            ("src/main.rs", "fn main() {}"),
        ]);
        assert_eq!(walked_rels(&fs), vec!["src/main.rs"]);
    }

    #[test]
    fn walk_workspace_files_ignores_baked_in_dirs() {
        let fs = seeded_fake_fs(&[
            ("target/debug/foo", "bin"),
            ("node_modules/pkg/index.js", "module.exports = {}"),
            ("src/main.rs", "fn main() {}"),
        ]);
        assert_eq!(walked_rels(&fs), vec!["src/main.rs"]);
    }

    #[test]
    fn walk_workspace_files_honors_stoatignore() {
        let fs = seeded_fake_fs(&[
            (".stoatignore", "vendor/\n"),
            ("vendor/blob.rs", "// generated"),
            ("src/main.rs", "fn main() {}"),
        ]);
        assert_eq!(
            walked_rels(&fs),
            vec![".stoatignore".to_string(), "src/main.rs".to_string()],
        );
    }

    #[test]
    fn walk_workspace_files_honors_nested_gitignore() {
        let fs = seeded_fake_fs(&[
            ("src/main.rs", "fn main() {}"),
            ("src/generated/.gitignore", "*.rs\n"),
            ("src/generated/auto.rs", "// auto"),
            ("src/generated/keep.txt", "keep"),
        ]);
        assert_eq!(
            walked_rels(&fs),
            vec![
                "src/generated/.gitignore".to_string(),
                "src/generated/keep.txt".to_string(),
                "src/main.rs".to_string(),
            ],
        );
    }

    #[test]
    fn walk_workspace_files_inner_negation_overrides_outer_ignore() {
        let fs = seeded_fake_fs(&[
            (".gitignore", "*.log\n"),
            ("trace.log", "outer"),
            ("logs/.gitignore", "!*.log\n"),
            ("logs/trace.log", "inner"),
        ]);
        assert_eq!(
            walked_rels(&fs),
            vec![
                ".gitignore".to_string(),
                "logs/.gitignore".to_string(),
                "logs/trace.log".to_string(),
            ],
        );
    }

    #[test]
    fn walk_workspace_files_still_walks_non_git_dotfiles() {
        let fs = seeded_fake_fs(&[
            (".claude/settings.json", "{}"),
            (".vscode/launch.json", "{}"),
            ("src/main.rs", "fn main() {}"),
        ]);
        assert_eq!(
            walked_rels(&fs),
            vec![
                ".claude/settings.json".to_string(),
                ".vscode/launch.json".to_string(),
                "src/main.rs".to_string(),
            ],
        );
    }

    // ----- TestHarness integration tests -----

    use crate::{buffer_registry::OpenOrigin, debounce, test_harness::TestHarness};
    use stoat_action::Action;

    /// Insert `files` into the harness' [`crate::host::FakeFs`] under a
    /// fixed virtual root and point the active workspace at it. Returns the
    /// virtual root so callers that need to seed extra git state (or assert
    /// on absolute paths) can join against it.
    fn seed_finder_workspace(h: &mut TestHarness, files: &[(&str, &str)]) -> PathBuf {
        let root = PathBuf::from("/stoat-finder-test");
        h.fake_fs().insert_files(
            files
                .iter()
                .map(|(rel, content)| (root.join(rel), content.as_bytes())),
        );
        h.stoat.active_workspace_mut().git_root = root.clone();
        root
    }

    #[test]
    fn space_p_opens_finder_and_switches_to_insert_mode() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "fn a() {}")]);
        h.type_keys("space p");
        assert!(h.stoat.file_finder.is_some(), "finder not opened");
        assert_eq!(h.snapshot().mode, "insert");
    }

    #[test]
    fn escape_closes_finder_and_restores_mode() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "")]);
        h.type_keys("space p");
        h.type_keys("escape");
        assert!(h.stoat.file_finder.is_none(), "finder still open");
        assert_eq!(h.snapshot().mode, "normal");
    }

    #[test]
    fn ctrl_c_closes_finder() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "")]);
        h.type_keys("space p");
        h.type_keys("Ctrl-c");
        assert!(h.stoat.file_finder.is_none());
        assert_eq!(h.snapshot().mode, "normal");
    }

    #[test]
    fn second_open_is_noop() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "")]);
        h.type_keys("space p");
        let ptr_before = h.stoat.file_finder.as_ref().unwrap() as *const FileFinder;
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        let ptr_after = h.stoat.file_finder.as_ref().unwrap() as *const FileFinder;
        assert_eq!(ptr_before, ptr_after, "re-open should not replace state");
    }

    #[test]
    fn enter_dispatches_open_file_for_selected_path() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("target.txt", "loaded via finder")]);
        h.type_keys("space p");
        // Only one file in the workspace, so it is the selected row.
        h.type_keys("enter");
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        assert!(
            frame.content.contains("loaded via finder"),
            "file content missing from pane:\n{}",
            frame.content
        );
        assert!(h.stoat.file_finder.is_none());
    }

    #[test]
    fn space_a_f_opens_finder_with_hsplit_intent() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("target.txt", "loaded via finder")]);
        h.type_keys("space a f");
        let finder = h.stoat.file_finder.as_ref().expect("finder should be open");
        assert_eq!(finder.open_intent, OpenIntent::HSplit);
        assert_eq!(h.snapshot().mode, "insert");
    }

    #[test]
    fn space_a_capital_f_opens_finder_with_vsplit_intent() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("target.txt", "loaded via finder")]);
        h.type_keys("space a F");
        let finder = h.stoat.file_finder.as_ref().expect("finder should be open");
        assert_eq!(finder.open_intent, OpenIntent::VSplit);
    }

    #[test]
    fn enter_with_hsplit_intent_creates_new_pane_and_opens_file() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("target.txt", "loaded via finder")]);
        assert_eq!(h.snapshot().pane_count, 1);

        h.type_keys("space a f");
        h.type_keys("enter");

        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 2, "split should create a second pane");
        assert!(
            frame.content.contains("loaded via finder"),
            "file content missing from frame:\n{}",
            frame.content
        );
        assert!(h.stoat.file_finder.is_none());
    }

    #[test]
    fn changed_file_picker_opens_in_modified_scope() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[("a.rs", "v1\n"), ("b.rs", "v1\n"), ("c.rs", "v1\n")],
        );
        {
            let mut builder = h.fake_git().add_repo(&root).with_fs(h.fake_fs());
            builder.head_file("a.rs", "v1\n");
            builder.modified("b.rs", "v1\n", "v2\n");
            builder.head_file("c.rs", "v1\n");
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenChangedFilePicker);
        let finder = h.stoat.file_finder.as_ref().expect("finder should be open");
        assert_eq!(finder.scope(), &FinderScope::Modified);
        let base: Vec<PathBuf> = finder.core.picklist.base.to_vec();
        assert_eq!(base.len(), 1, "Modified scope should list only b.rs");
        assert!(base[0].ends_with("b.rs"));
        assert_eq!(h.snapshot().mode, "insert");
    }

    /// A repository at `/repo` with two modified files, a clean file, and an
    /// untracked file, with `a.txt` open.
    fn diff_repo() -> TestHarness {
        let mut h = TestHarness::with_size(100, 24);
        let root = PathBuf::from("/repo");
        h.fake_fs().insert_files(
            [
                ("a.txt", "A\n"),
                ("b.txt", "B\n"),
                ("c.txt", "c\n"),
                ("d.txt", "d\n"),
            ]
            .map(|(rel, text)| (root.join(rel), text.as_bytes())),
        );
        h.stoat.active_workspace_mut().git_root = root.clone();
        h.fake_git()
            .add_repo(&root)
            .with_fs(h.fake_fs())
            .modified("a.txt", "a\n", "A\n")
            .modified("b.txt", "b\n", "B\n")
            .head_file("c.txt", "c\n")
            .untracked("d.txt");
        h.open_file(&root.join("a.txt"));
        h
    }

    /// The [`diff_repo`] harness with `a.txt` in the per-file diff.
    ///
    /// Over the per-file diff only the frame drive reads the repository tally,
    /// and the harness keeps that drive off unless a test asks.
    fn per_file_diff() -> TestHarness {
        let mut h = diff_repo();
        h.stoat.set_diff_warm_auto(true);
        settle_into(&mut h, &stoat_action::DiffFiles { rev: None });
        h.settle_diff_jobs();
        h
    }

    fn repo_paths(rels: &[&str]) -> Vec<PathBuf> {
        rels.iter()
            .map(|rel| Path::new("/repo").join(rel))
            .collect()
    }

    /// Dispatch `action` and let the views it opens settle.
    fn settle_into(h: &mut TestHarness, action: &dyn Action) {
        crate::action_handlers::dispatch(&mut h.stoat, action);
        h.settle();
    }

    /// The scopes a `space p` finder shows after zero through `toggles`
    /// Shift-Tabs. The finder closes after the last.
    fn scope_cycle(h: &mut TestHarness, toggles: usize) -> Vec<FinderScope> {
        h.type_keys("space p");
        let mut scopes = vec![scope_and_base(h).0];
        for _ in 0..toggles {
            h.type_keys("backtab");
            scopes.push(scope_and_base(h).0);
        }
        h.type_keys("escape");
        scopes
    }

    fn scope_and_base(h: &TestHarness) -> (FinderScope, Vec<PathBuf>) {
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        (finder.scope().clone(), finder.core.picklist.base.to_vec())
    }

    #[test]
    fn space_p_over_the_changes_view_lists_the_stacks_files() {
        let mut h = diff_repo();
        settle_into(&mut h, &stoat_action::DiffChanges);
        h.type_keys("space p");

        assert_eq!(
            scope_and_base(&h),
            (FinderScope::Diff, repo_paths(&["a.txt", "b.txt", "d.txt"]))
        );
        assert!(
            h.snapshot().content.contains(" all   diff   workspaces "),
            "the header's pills name the diff scope in Modified's place"
        );
    }

    #[test]
    fn space_p_over_the_per_file_diff_lists_the_walks_files() {
        let mut h = per_file_diff();
        h.type_keys("space p");

        assert_eq!(
            scope_and_base(&h),
            (FinderScope::Diff, repo_paths(&["a.txt", "b.txt", "d.txt"]))
        );
    }

    #[test]
    fn the_diff_scope_takes_modifieds_place_in_the_cycle() {
        let mut h = diff_repo();
        let plain = scope_cycle(&mut h, 2);
        settle_into(&mut h, &stoat_action::DiffChanges);
        let over_changes = scope_cycle(&mut h, 3);

        assert_eq!(
            (plain, over_changes),
            (
                vec![
                    FinderScope::All,
                    FinderScope::Modified,
                    FinderScope::AllWorkspaces
                ],
                vec![
                    FinderScope::Diff,
                    FinderScope::AllWorkspaces,
                    FinderScope::All,
                    FinderScope::Diff
                ],
            )
        );
    }

    #[test]
    fn a_close_in_the_diff_scope_keeps_the_remembered_scope() {
        let mut h = diff_repo();
        h.stoat.active_workspace_mut().last_finder_scope = Some("modified".to_string());
        settle_into(&mut h, &stoat_action::DiffChanges);

        let cycle = scope_cycle(&mut h, 0);
        assert_eq!(
            (
                cycle,
                h.stoat.active_workspace().last_finder_scope.as_deref()
            ),
            (vec![FinderScope::Diff], Some("modified"))
        );
    }

    /// The picked file shows in the diff view from the moment it opens, so no
    /// plain layout shows while its hunks are on the pool.
    #[test]
    fn a_finder_pick_over_the_per_file_diff_keeps_the_diff_view() {
        let mut h = per_file_diff();
        h.type_keys("space p b");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SubmitPromptInput);
        let opened = diff_view_state(&h);
        h.settle_diff_jobs();

        let shown = (Some(PathBuf::from("/repo/b.txt")), true, true);
        assert_eq!((opened, diff_view_state(&h)), (shown.clone(), shown));
    }

    /// The focused editor's file, whether it shows the diff view, and whether
    /// its pane is latched to the diff.
    fn diff_view_state(h: &TestHarness) -> (Option<PathBuf>, bool, bool) {
        let (editor_id, buffer_id) = h.stoat.focused_editor_ids().expect("a focused editor");
        let ws = h.stoat.active_workspace();
        (
            ws.buffers.path_for(buffer_id).map(Path::to_path_buf),
            ws.editors
                .get(editor_id)
                .is_some_and(|editor| editor.diff_view),
            ws.panes.pane(ws.panes.focus()).diff_mode,
        )
    }

    #[test]
    fn space_b_b_opens_finder_in_buffers_scope() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[
                ("a.rs", "fn a() {}"),
                ("b.rs", "fn b() {}"),
                ("c.rs", "fn c() {}"),
            ],
        );
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("a.rs"),
            },
        );
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("c.rs"),
            },
        );

        h.type_keys("space b b");
        let finder = h.stoat.file_finder.as_ref().expect("finder should be open");
        assert_eq!(finder.scope(), &FinderScope::Buffers);
        let base: Vec<PathBuf> = finder.core.picklist.base.to_vec();
        assert_eq!(base.len(), 2, "Buffers scope should list only open buffers");
        assert!(base.iter().any(|p| p.ends_with("a.rs")));
        assert!(base.iter().any(|p| p.ends_with("c.rs")));
        assert!(!base.iter().any(|p| p.ends_with("b.rs")));
        assert_eq!(h.snapshot().mode, "insert");
    }

    /// A finder workspace over `a.rs`, `b.rs`, and `c.rs`, with `a.rs` opened by
    /// name and then `b.rs` reached by a navigation in the same pane.
    fn named_a_then_visited_b(h: &mut TestHarness) -> PathBuf {
        let root = seed_finder_workspace(
            h,
            &[
                ("a.rs", "fn a() {}"),
                ("b.rs", "fn b() {}"),
                ("c.rs", "fn c() {}"),
            ],
        );
        open_named(h, root.join("a.rs"));
        let pane = h.stoat.active_workspace().panes.focus();
        crate::buffer_lifecycle::open_file_in_pane(
            &mut h.stoat,
            pane,
            &root.join("b.rs"),
            OpenOrigin::Visited,
        );
        root
    }

    fn open_named(h: &mut TestHarness, path: PathBuf) {
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFile { path });
    }

    /// The file names the buffer picker lists after `space b b`.
    fn buffer_picker_names(h: &mut TestHarness) -> Vec<String> {
        h.type_keys("space b b");
        let finder = h.stoat.file_finder.as_ref().expect("finder should be open");
        finder
            .core
            .picklist
            .base
            .iter()
            .map(|path| {
                path.file_name()
                    .expect("a file path")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn the_buffer_picker_omits_a_file_a_navigation_only_visited() {
        let mut h = crate::Stoat::test();
        let root = named_a_then_visited_b(&mut h);
        open_named(&mut h, root.join("c.rs"));

        assert_eq!(buffer_picker_names(&mut h), ["a.rs", "c.rs"]);
    }

    #[test]
    fn the_buffer_picker_lists_a_visited_file_while_a_pane_shows_it() {
        let mut h = crate::Stoat::test();
        named_a_then_visited_b(&mut h);

        assert_eq!(buffer_picker_names(&mut h), ["a.rs", "b.rs"]);
    }

    #[test]
    fn the_buffer_picker_lists_a_visited_file_with_unsaved_edits() {
        let mut h = crate::Stoat::test();
        let root = named_a_then_visited_b(&mut h);
        h.type_keys("i x <esc>");
        open_named(&mut h, root.join("c.rs"));

        assert_eq!(buffer_picker_names(&mut h), ["a.rs", "b.rs", "c.rs"]);
    }

    /// Keys typed in one burst reach Enter with no frame between them, so the
    /// submit brings the buffer list up to date with the query itself.
    #[test]
    fn enter_in_a_burst_opens_the_buffer_the_query_selects() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", ""), ("b.rs", ""), ("c.rs", "")]);
        for name in ["c.rs", "a.rs"] {
            let path = root.join(name);
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFile { path });
        }
        h.settle();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenBufferPicker);

        h.stoat.update(Event::Key(keys::key(KeyCode::Char('c'))));
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("focused editor");
        assert_eq!(
            (
                h.stoat.file_finder.is_none(),
                h.stoat.active_workspace().buffers.path_for(buffer_id),
            ),
            (true, Some(root.join("c.rs").as_path())),
        );
    }

    /// A capped scope's base does not move between keystrokes, so the picker
    /// has to be told that and keep what it derived from it. This fails if a
    /// caller hands over a fresh identity per call, which is the whole of the
    /// wiring the picker cannot check for itself.
    #[test]
    fn typing_in_a_capped_scope_keeps_the_display_cache() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[("alpha.rs", "fn a() {}"), ("also_beta.rs", "fn b() {}")],
        );
        for rel in ["alpha.rs", "also_beta.rs"] {
            crate::action_handlers::dispatch(
                &mut h.stoat,
                &stoat_action::OpenFile {
                    path: root.join(rel),
                },
            );
        }

        h.type_keys("space b b");
        h.type_text("a");
        let built = {
            let finder = h.stoat.file_finder.as_ref().expect("finder open");
            assert_eq!(finder.scope(), &FinderScope::Buffers);
            finder
                .core
                .picklist
                .display
                .as_ref()
                .expect("a cache")
                .generation
        };

        h.type_text("l");
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        assert_eq!(
            finder
                .core
                .picklist
                .display
                .as_ref()
                .expect("a cache")
                .generation,
            built,
            "the second keystroke is over the base the first one was"
        );
    }

    #[test]
    fn space_b_b_previews_live_buffer_not_disk() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("note.txt", "on disk\n")]);
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("note.txt"),
            },
        );
        h.settle();

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

        h.type_keys("space b b");
        h.snapshot();
        let preview_id = h
            .stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .core
            .preview
            .buffer;
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
            "buffers-scope finder previews the live in-memory buffer, not the disk file",
        );
    }

    #[test]
    fn the_finder_previews_a_gzip_file_as_its_text() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[]);
        h.fake_fs().insert_file(
            root.join("a.rs.gz"),
            crate::test_harness::gzip(b"fn a() {}\n"),
        );

        h.type_keys("space p");
        h.snapshot();

        let preview = h
            .stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .core
            .preview
            .buffer;
        let language = h.stoat.active_workspace().buffers.language_for(preview);
        assert_eq!(
            (preview_text(&h), language.map(|language| language.name)),
            ("fn a() {}\n".to_string(), Some("rust")),
        );
    }

    /// The buffer picker's two scopes flip with each other, so Shift-Tab in the
    /// picker never lands on a file scope.
    #[test]
    fn backtab_in_the_buffer_picker_flips_between_all_and_modified_buffers() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[
                ("a.rs", "fn a() {}"),
                ("b.rs", "fn b() {}"),
                ("c.rs", "fn c() {}"),
            ],
        );
        for rel in ["a.rs", "c.rs"] {
            crate::action_handlers::dispatch(
                &mut h.stoat,
                &stoat_action::OpenFile {
                    path: root.join(rel),
                },
            );
        }
        h.settle();
        edit_in_memory(&h, &root.join("a.rs"));

        let scope_and_base = |h: &TestHarness| {
            let finder = h.stoat.file_finder.as_ref().expect("finder open");
            (finder.scope().clone(), finder.core.picklist.base.to_vec())
        };
        h.type_keys("space b b");
        h.type_keys("backtab");
        let modified = scope_and_base(&h);
        h.type_keys("backtab");

        assert_eq!(
            (modified, scope_and_base(&h)),
            (
                (FinderScope::ModifiedBuffers, vec![root.join("a.rs")]),
                (
                    FinderScope::Buffers,
                    vec![root.join("a.rs"), root.join("c.rs")]
                ),
            ),
        );
    }

    /// Give the open buffer at `path` an edit that is not on disk.
    fn edit_in_memory(h: &TestHarness, path: &Path) {
        let ws = h.stoat.active_workspace();
        let id = ws.buffers.id_for_path(path).expect("open buffer");
        let buffer = ws.buffers.get(id).expect("buffer");
        buffer.write().expect("poisoned").edit(0..0, "// ");
    }

    /// The pane finder lists the terminals, so neither buffer scope lists the
    /// shell that a pane shows.
    #[test]
    fn neither_buffer_scope_lists_a_terminal() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", "fn a() {}")]);
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("a.rs"),
            },
        );
        h.settle();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        edit_in_memory(&h, &root.join("a.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenBufferPicker);

        let base = |h: &TestHarness| {
            let finder = h.stoat.file_finder.as_ref().expect("finder open");
            finder.core.picklist.base.to_vec()
        };
        let buffers = base(&h);
        h.type_keys("backtab");

        assert_eq!(
            (buffers, base(&h)),
            (vec![root.join("a.rs")], vec![root.join("a.rs")]),
        );
    }

    /// The buffer picker has no path to complete, so Tab flips its scope as
    /// Shift-Tab does.
    #[test]
    fn tab_in_the_buffer_picker_toggles_the_scope() {
        let mut h = crate::Stoat::test();
        h.type_keys("space b b");
        h.type_keys("tab");
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        assert_eq!(finder.scope(), &FinderScope::ModifiedBuffers);
    }

    #[test]
    fn backtab_from_changed_file_picker_enters_all_workspaces() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[("a.rs", "v1\n"), ("b.rs", "v1\n"), ("c.rs", "v1\n")],
        );
        {
            let mut builder = h.fake_git().add_repo(&root).with_fs(h.fake_fs());
            builder.head_file("a.rs", "v1\n");
            builder.modified("b.rs", "v1\n", "v2\n");
            builder.head_file("c.rs", "v1\n");
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenChangedFilePicker);
        h.type_keys("backtab");
        h.settle();
        let _ = h.snapshot();
        let finder = h.stoat.file_finder.as_ref().unwrap();
        assert_eq!(
            finder.scope(),
            &FinderScope::AllWorkspaces,
            "with no named scopes, Modified backtabs into AllWorkspaces",
        );
        assert_eq!(
            finder.core.picklist.base.len(),
            3,
            "AllWorkspaces lists the sole workspace's files",
        );
    }

    #[test]
    fn backtab_toggles_scope_to_modified() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[("a.rs", "v1\n"), ("b.rs", "v1\n"), ("c.rs", "v1\n")],
        );
        // Seed the fake git repo so only b.rs is reported as modified.
        {
            let mut builder = h.fake_git().add_repo(&root).with_fs(h.fake_fs());
            builder.head_file("a.rs", "v1\n");
            builder.modified("b.rs", "v1\n", "v2\n");
            builder.head_file("c.rs", "v1\n");
        }

        h.type_keys("space p");
        {
            let finder = h.stoat.file_finder.as_ref().unwrap();
            assert_eq!(finder.scope(), &FinderScope::All);
            let base: Vec<PathBuf> = finder.core.picklist.base.to_vec();
            assert_eq!(base.len(), 3, "All scope should list all 3 files");
        }
        h.type_keys("backtab");
        {
            let finder = h.stoat.file_finder.as_ref().unwrap();
            assert_eq!(finder.scope(), &FinderScope::Modified);
            let base: Vec<PathBuf> = finder.core.picklist.base.to_vec();
            assert_eq!(base.len(), 1);
            assert!(base[0].ends_with("b.rs"));
        }
    }

    /// The toggle onto Modified re-asks git rather than reusing whatever the
    /// open-time query answered, so a file changed while the finder sat open
    /// still shows up.
    #[test]
    fn backtab_onto_modified_sees_a_change_made_since_the_finder_opened() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", "v1\n"), ("b.rs", "v1\n")]);
        {
            let mut builder = h.fake_git().add_repo(&root).with_fs(h.fake_fs());
            builder.head_file("a.rs", "v1\n");
            builder.head_file("b.rs", "v1\n");
        }

        h.type_keys("space p");

        {
            let mut builder = h.fake_git().add_repo(&root).with_fs(h.fake_fs());
            builder.modified("b.rs", "v1\n", "v2\n");
        }
        h.type_keys("backtab");

        let finder = h.stoat.file_finder.as_ref().unwrap();
        assert_eq!(finder.scope(), &FinderScope::Modified);
        let base: Vec<PathBuf> = finder.core.picklist.base.to_vec();
        assert_eq!(base.len(), 1, "the toggle re-queried: {base:?}");
        assert!(base[0].ends_with("b.rs"));
    }

    /// In production the git status pass lands after the finder has already
    /// painted its empty list, which is the case the test scheduler's inline
    /// blocking work never reaches on its own. Restamping the base generation
    /// alone would leave those rows empty, since the refilter short-circuits on
    /// the unchanged query.
    #[test]
    fn a_modified_answer_arriving_after_the_first_paint_fills_the_rows() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", "v1\n"), ("b.rs", "v1\n")]);
        {
            let mut builder = h.fake_git().add_repo(&root).with_fs(h.fake_fs());
            builder.head_file("a.rs", "v1\n");
            builder.head_file("b.rs", "v1\n");
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenChangedFilePicker);
        let _ = h.snapshot();
        assert!(
            h.stoat
                .file_finder
                .as_ref()
                .expect("finder open")
                .core
                .picklist
                .base
                .is_empty(),
            "nothing is modified yet",
        );

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = h.stoat.executor.spawn_blocking(|| {});
        tx.send(vec![root.join("b.rs")])
            .expect("receiver held below");
        h.stoat
            .file_finder
            .as_mut()
            .expect("finder open")
            .set_modified_source(rx, task);

        let _ = h.snapshot();

        let base: Vec<PathBuf> = h
            .stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .core
            .picklist
            .base
            .to_vec();
        assert_eq!(base.len(), 1, "the late answer replaced the empty list");
        assert!(base[0].ends_with("b.rs"));
    }

    /// The scope state filters a picker that no finder owns. All lists the
    /// walk, and a toggle lands on Modified at the first row. The git-status
    /// answer then becomes the base that a later query settles against.
    #[test]
    fn the_modified_scope_lists_the_modified_paths_in_a_picker_that_no_finder_owns() {
        let mut h = crate::Stoat::test();
        let (mut core, mut scopes, [a, b]) = scopes_over_two_files(&mut h, FinderScope::All);
        let rows = |core: &PathPicker| -> Vec<PathBuf> {
            let list = &core.picklist;
            list.filtered
                .iter()
                .map(|&i| list.base[i].clone())
                .collect()
        };

        scopes.refilter(&mut core, "");
        let all = rows(&core);
        core.move_selection(1);
        scopes.toggle(&mut core);
        let toggled = (scopes.scope.clone(), core.picklist.selected);
        scopes.pump_modified(&mut core);
        scopes.refilter(&mut core, "");
        let base = core.picklist.base.clone();
        scopes.settle(&mut core, "a");

        assert_eq!(
            (all, toggled, base, rows(&core)),
            (
                vec![a, b.clone()],
                (FinderScope::Modified, 0),
                vec![b],
                Vec::new()
            ),
        );
    }

    /// A settle in a listed scope takes a git-status answer that landed since
    /// the last refilter, so an action between two frames lists the answer.
    #[test]
    fn a_settle_takes_a_git_status_answer_that_landed_since_the_last_refilter() {
        let mut h = crate::Stoat::test();
        let (mut core, mut scopes, [_, b]) = scopes_over_two_files(&mut h, FinderScope::Modified);
        scopes.settle(&mut core, "");
        assert_eq!(core.picklist.base, [b]);
    }

    /// A settle in a named scope settles against the walked paths that the
    /// scope's globs keep, and never against the whole walk.
    #[test]
    fn a_settle_in_a_named_scope_lists_only_the_paths_its_globs_keep() {
        let mut h = crate::Stoat::test();
        let named = FinderScope::Named("b".to_owned());
        let (mut core, mut scopes, [_, b]) = scopes_over_two_files(&mut h, named);
        scopes.named_scopes =
            compile_named_scopes(&BTreeMap::from([("b".to_owned(), vec!["b.*".to_owned()])]));
        scopes.settle(&mut core, "");
        assert_eq!(core.picklist.base, [b]);
    }

    /// A picker over `/scopes/a.rs` and `/scopes/b.rs`, the scope state at
    /// `scope`, and the two paths. The scope state's git-status answer holds
    /// `b.rs` and waits for a pump to take it.
    fn scopes_over_two_files(
        h: &mut TestHarness,
        scope: FinderScope,
    ) -> (PathPicker, FileScopes, [PathBuf; 2]) {
        let root = PathBuf::from("/scopes");
        let paths = [root.join("a.rs"), root.join("b.rs")];
        let executor = h.stoat.executor.clone();
        let ws = h.stoat.active_workspace_mut();
        let mut core = PathPicker::new(ws, executor.clone(), root, None);
        core.all_paths = paths.to_vec();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(vec![paths[1].clone()])
            .expect("the receiver is open");
        let modified = (rx, executor.spawn(async {}));
        let scopes = FileScopes::new(
            scope,
            modified,
            None,
            Vec::new(),
            Vec::new(),
            &BTreeMap::new(),
        );
        (core, scopes, paths)
    }

    #[test]
    fn backtab_cycles_through_named_scopes() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("src/a.rs", ""), ("docs/b.md", "")]);
        h.stoat.settings.finder_scopes = BTreeMap::from([
            ("code".to_string(), vec!["src/**".to_string()]),
            ("prose".to_string(), vec!["docs/**".to_string()]),
        ]);

        h.type_keys("space p");
        assert_eq!(
            h.stoat.file_finder.as_ref().unwrap().scope(),
            &FinderScope::All
        );

        h.type_keys("backtab");
        assert_eq!(
            h.stoat.file_finder.as_ref().unwrap().scope(),
            &FinderScope::Modified
        );

        h.type_keys("backtab");
        assert_eq!(
            h.stoat.file_finder.as_ref().unwrap().scope(),
            &FinderScope::Named("code".to_string()),
            "first backtab past Modified lands on the alphabetically-first scope"
        );

        h.type_keys("backtab");
        assert_eq!(
            h.stoat.file_finder.as_ref().unwrap().scope(),
            &FinderScope::Named("prose".to_string())
        );

        h.type_keys("backtab");
        assert_eq!(
            h.stoat.file_finder.as_ref().unwrap().scope(),
            &FinderScope::AllWorkspaces,
            "backtab past the last named scope lands on AllWorkspaces"
        );

        h.type_keys("backtab");
        assert_eq!(
            h.stoat.file_finder.as_ref().unwrap().scope(),
            &FinderScope::All,
            "backtab past AllWorkspaces wraps to All"
        );
    }

    /// The header's pills walk the file scopes from All in the order Shift-Tab
    /// takes, with Diff in Modified's place over a diff, and the buffer
    /// picker's two scopes on their own. A Modified forced over a diff takes
    /// the place after All, so the scope on show always has a pill.
    #[test]
    fn the_scope_cycle_walks_the_file_scopes_from_all() {
        use FinderScope::{All, AllWorkspaces, Buffers, Diff, Modified, ModifiedBuffers, Named};

        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("src/a.rs", "")]);
        h.type_keys("space p");
        let finder = h.stoat.file_finder.as_mut().expect("the finder is open");
        let plain = finder.scope_cycle();
        finder.scopes.named_scopes = compile_named_scopes(&BTreeMap::from([
            ("docs".to_string(), vec!["docs/**".to_string()]),
            ("src".to_string(), vec!["src/**".to_string()]),
        ]));
        let named = finder.scope_cycle();
        finder.scopes.named_scopes.clear();
        finder.scopes.diff_paths = Some(Vec::new());
        let over_diff = finder.scope_cycle();
        finder.scopes.scope = Modified;
        let forced = finder.scope_cycle();
        finder.scopes.diff_paths = None;
        finder.scopes.scope = Buffers;
        let buffers = finder.scope_cycle();

        assert_eq!(
            [plain, named, over_diff, buffers, forced],
            [
                vec![All, Modified, AllWorkspaces],
                vec![
                    All,
                    Modified,
                    Named("docs".to_string()),
                    Named("src".to_string()),
                    AllWorkspaces,
                ],
                vec![All, Diff, AllWorkspaces],
                vec![Buffers, ModifiedBuffers],
                vec![All, Modified, Diff, AllWorkspaces],
            ],
        );
    }

    /// The plate refits to the widest label on show each time the rows change.
    /// The files hold no line, so the preview takes the 60 columns of its floor.
    /// The query `zz` shows the 66-char path, so the list grows to 69 columns.
    /// Clearing the query fits the list back to 33 columns, and the plate
    /// narrows with it.
    #[test]
    fn the_finder_plate_refits_when_its_rows_change() {
        let mut h = fitted_plate_harness();
        let open = plate_widths(&h);
        h.type_text("zz");
        h.settle();
        h.stoat.render();
        let grown = plate_widths(&h);
        h.type_keys("backspace backspace");
        h.settle();
        h.stoat.render();

        assert_eq!(
            [open, grown, plate_widths(&h)],
            [(96, 33, Some(60)), (132, 69, Some(60)), (96, 33, Some(60)),],
        );
    }

    /// A selection move leaves the list's rows as they are, so a selection
    /// that scrolls the long path into view keeps the plate, and the path wraps
    /// in the list.
    #[test]
    fn a_selection_move_keeps_the_plate() {
        let mut h = fitted_plate_harness();
        let open = plate_widths(&h).0;
        for _ in 0..35 {
            h.type_keys("down");
        }

        assert_eq!((open, plate_widths(&h)), (96, (96, 33, Some(60))));
    }

    /// Each open fits the plate to the labels its list shows, whatever plate
    /// the last open drew.
    #[test]
    fn the_finder_plate_starts_over_at_each_open() {
        let mut h = fitted_plate_harness();
        h.type_text("zz");
        h.settle();
        h.stoat.render();
        let grown = plate_widths(&h).0;
        h.type_keys("escape");
        h.type_keys("space p");
        h.settle();
        h.stoat.render();

        assert_eq!((grown, plate_widths(&h).0), (132, 96));
    }

    /// A dragged share sets the list while the finder stays open, and the
    /// close drops it, so the next open fits the list to its labels again.
    #[test]
    fn a_dragged_split_holds_until_the_finder_closes() {
        let mut h = fitted_plate_harness();
        h.stoat.modal_split.insert(ModalKind::FileFinder, 5_000);
        let dragged = plate_widths(&h);
        h.type_keys("escape");
        let share = h.stoat.modal_split.get(&ModalKind::FileFinder).copied();
        h.type_keys("space p");
        h.settle();

        assert_eq!(
            (dragged, share, plate_widths(&h)),
            ((96, 47, Some(46)), None, (96, 33, Some(60))),
        );
    }

    /// A file of 100-column lines asks for a preview of 100 columns, and a
    /// window too narrow for it beside a list of 33 keeps the list at its
    /// minimum. The plate stops one column inside each side of the window, and
    /// the preview takes the rest of it.
    #[test]
    fn a_window_too_narrow_for_both_minimums_keeps_the_list_at_thirty_columns() {
        let mut h = TestHarness::with_size(120, 30);
        let lines = format!("{}\n", "x".repeat(100)).repeat(10);
        seed_finder_workspace(&mut h, &[("a.rs", &lines)]);
        h.type_keys("space p");
        h.settle();

        assert_eq!(plate_widths(&h), (118, 33, Some(82)));
    }

    /// The preview takes the typical line of the files on show, so a file of
    /// 80-column lines gets a preview of 80 columns beside the 33-column list.
    #[test]
    fn the_finder_preview_fits_the_typical_line_of_its_files() {
        let lines = format!("{}\n", "x".repeat(80)).repeat(10);
        assert_eq!(widths_over_one_file(&lines), (116, 33, Some(80)));
    }

    /// One long line among short ones leaves the preview at the 60 columns of
    /// its floor.
    #[test]
    fn one_long_line_does_not_widen_the_preview() {
        let short = format!("{}\n", "x".repeat(20)).repeat(19);
        let text = format!("{short}{}\n", "y".repeat(300));
        assert_eq!(widths_over_one_file(&text), (96, 33, Some(60)));
    }

    /// The plate widths of the finder that a 200 by 40 harness opens over one
    /// file `a.rs` holding `text`.
    fn widths_over_one_file(text: &str) -> (u16, u16, Option<u16>) {
        let mut h = TestHarness::with_size(200, 40);
        seed_finder_workspace(&mut h, &[("a.rs", text)]);
        h.type_keys("space p");
        h.settle();
        plate_widths(&h)
    }

    /// A 200 by 40 harness with the finder open over 35 short paths and one
    /// path of 66 chars, which the list shows only under the query `zz`.
    fn fitted_plate_harness() -> TestHarness {
        let mut h = TestHarness::with_size(200, 40);
        let names: Vec<String> = (0..35)
            .map(|n| format!("a{n:02}.rs"))
            .chain([format!("zz/{}.rs", "x".repeat(60))])
            .collect();
        let files: Vec<(&str, &str)> = names.iter().map(|name| (name.as_str(), "")).collect();
        seed_finder_workspace(&mut h, &files);
        h.type_keys("space p");
        h.settle();
        h.stoat.render();
        h
    }

    /// The open finder's modal width and the widths of its list and preview.
    fn plate_widths(h: &TestHarness) -> (u16, u16, Option<u16>) {
        let layout = test_fixture::finder_layout(h);
        (
            layout.modal.width,
            layout.list.width,
            layout.preview.map(|preview| preview.width),
        )
    }

    /// A directory browse walks to the same cap as the repo walk, so its
    /// ranking is handed back for a worker like every other large list.
    #[test]
    fn a_directory_browse_ranks_off_the_input_thread() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("dir/a.rs", ""), ("dir/b.rs", "")]);

        h.type_keys("space p");
        h.type_text("/a");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        let active_idx = h.stoat.active_workspace;
        let ws = &h.stoat.workspaces[active_idx];
        let finder = h.stoat.file_finder.as_mut().expect("finder open");
        assert!(finder.browse.is_some(), "a path query browses");

        // Invalidated so the partial is asked afresh, the sync driven by the
        // typing above having already answered it. The browse list is named
        // rather than taken from `active_core`, because `/` lists nothing in the
        // fake filesystem and so a fallback of ignored files stands in for it.
        finder
            .browse
            .as_mut()
            .expect("a path query browses")
            .picker
            .invalidate();
        assert_eq!(
            finder
                .refilter_from_input(ws)
                .iter()
                .map(|scan| scan.target)
                .collect::<Vec<_>>(),
            [ScanTarget::List],
            "the ranking is handed back for a worker to run",
        );
    }

    /// A named scope's base is whatever its glob kept, which on a large repo is
    /// most of the walk, so its ranking goes to a worker like the uncapped
    /// scopes rather than running inside the keystroke.
    #[test]
    fn a_named_scope_ranks_off_the_input_thread() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(
            &mut h,
            &[("src/a.rs", ""), ("src/b.rs", ""), ("docs/c.md", "")],
        );
        h.stoat.settings.finder_scopes =
            BTreeMap::from([("code".to_string(), vec!["src/**".to_string()])]);

        h.type_keys("space p");
        h.type_keys("backtab");
        h.type_keys("backtab");
        h.type_text("a");

        let active_idx = h.stoat.active_workspace;
        let ws = &h.stoat.workspaces[active_idx];
        let finder = h.stoat.file_finder.as_mut().expect("finder open");
        assert_eq!(finder.scope(), &FinderScope::Named("code".to_string()));

        // Invalidated so the query is asked afresh, since the sync driven by
        // the typing above has already answered this one.
        finder.core.invalidate();
        assert_eq!(
            finder
                .refilter_from_input(ws)
                .iter()
                .map(|scan| scan.target)
                .collect::<Vec<_>>(),
            [ScanTarget::List],
            "the ranking is handed back for a worker to run",
        );
    }

    #[test]
    fn named_scope_lists_only_glob_matching_files() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(
            &mut h,
            &[("src/a.rs", ""), ("docs/b.md", ""), ("README.md", "")],
        );
        h.stoat.settings.finder_scopes =
            BTreeMap::from([("code".to_string(), vec!["src/**".to_string()])]);

        h.type_keys("space p");
        h.type_keys("backtab");
        h.type_keys("backtab");

        let finder = h.stoat.file_finder.as_ref().unwrap();
        assert_eq!(finder.scope(), &FinderScope::Named("code".to_string()));
        let base: Vec<PathBuf> = finder.core.picklist.base.to_vec();
        assert_eq!(base.len(), 1, "code scope should list only src/a.rs");
        assert!(base[0].ends_with("src/a.rs"));
    }

    /// Repo-relative paths in the finder's candidate base, sorted.
    /// Directory listings the fake filesystem has served, which is what a
    /// workspace walk spends itself on. An open that reuses the cached paths
    /// adds none.
    fn walked_dirs(h: &TestHarness) -> usize {
        h.fake_fs()
            .ops()
            .iter()
            .filter(|op| matches!(op, crate::host::FakeFsOp::ListDir { .. }))
            .count()
    }

    /// Reopening over an unchanged tree must not re-walk it. On a large repo
    /// that walk is seconds of visibly repopulating rows, matched against a
    /// base that is still filling in.
    #[test]
    fn reopening_the_finder_reuses_the_paths_the_first_walk_found() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", ""), ("b.rs", ""), ("src/c.rs", "")]);

        h.type_keys("space p");
        h.type_keys("escape");
        h.stoat.drain_index_updates();

        let walked = walked_dirs(&h);

        h.type_keys("space p");

        assert_eq!(walked_dirs(&h), walked, "the reopen listed no directories");
        assert_eq!(
            base_paths(&h),
            ["a.rs", "b.rs", "src/c.rs"],
            "and still lists every file",
        );
    }

    /// The display rows the open finder's list holds.
    fn finder_display_rows(h: &TestHarness) -> Arc<Vec<Arc<str>>> {
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        let display = finder.core.picklist.display.as_ref().expect("a display");
        Arc::clone(display.rows())
    }

    /// The display rows the cached path list holds while no finder is open.
    fn cached_display(h: &TestHarness) -> Option<&DisplayCache> {
        let cache = h.stoat.finder_path_cache.as_ref().expect("a cached list");
        cache.display.as_ref()
    }

    /// Deriving a row per path and sorting them is the cost of an open over a
    /// large list, so the rows a close filed come back with the paths.
    #[test]
    fn reopening_the_finder_reuses_the_rows_it_closed_with() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", ""), ("b.rs", ""), ("src/c.rs", "")]);

        h.type_keys("space p");
        let _ = h.snapshot();
        let closed_with = finder_display_rows(&h);
        h.type_keys("escape");
        h.stoat.drain_index_updates();

        h.type_keys("space p");
        let _ = h.snapshot();

        assert!(
            Arc::ptr_eq(&finder_display_rows(&h), &closed_with),
            "the reopen lists the rows the close filed rather than deriving them again"
        );
    }

    #[test]
    fn a_removed_file_retires_the_cached_rows() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", ""), ("b.rs", "")]);

        h.type_keys("space p");
        h.type_keys("escape");
        h.stoat.drain_index_updates();
        assert!(cached_display(&h).is_some(), "the close filed the rows");

        h.fake_fs_watcher()
            .inject(root.join("b.rs"), crate::host::FsEventKind::Removed);
        debounce::drain_fs_watch_events(&mut h.stoat);

        assert!(
            cached_display(&h).is_none(),
            "a departure shifts the rows past it, so they go"
        );
        h.type_keys("space p");
        assert_eq!(
            base_paths(&h),
            ["a.rs"],
            "and the reopen lists what remains"
        );
    }

    #[test]
    fn a_created_file_keeps_the_cached_rows() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", "")]);

        h.type_keys("space p");
        h.type_keys("escape");
        h.stoat.drain_index_updates();
        let filed = Arc::clone(cached_display(&h).expect("the close filed the rows").rows());

        h.fake_fs()
            .insert_files([(root.join("b.rs"), "".as_bytes())]);
        h.fake_fs_watcher()
            .inject(root.join("b.rs"), crate::host::FsEventKind::Created);
        debounce::drain_fs_watch_events(&mut h.stoat);

        let kept = cached_display(&h)
            .expect("an arrival keeps the rows")
            .rows();
        assert!(
            Arc::ptr_eq(kept, &filed),
            "an arrival appends past the rows, so they stand as filed"
        );
        h.type_keys("space p");
        assert_eq!(
            base_paths(&h),
            ["a.rs", "b.rs"],
            "and the reopen lists both files"
        );
    }

    /// A removal drops one path, which the cached list answers itself.
    ///
    /// Walking the tree again to learn that one file left it is the whole
    /// cost this cache exists to avoid, and a single save is enough to force
    /// it if the list retires on every change.
    #[test]
    fn a_removed_file_leaves_the_cached_list_without_a_walk() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", ""), ("b.rs", "")]);

        h.type_keys("space p");
        h.type_keys("escape");
        h.stoat.drain_index_updates();

        let walked = walked_dirs(&h);

        h.fake_fs_watcher()
            .inject(root.join("b.rs"), crate::host::FsEventKind::Removed);
        debounce::drain_fs_watch_events(&mut h.stoat);
        h.type_keys("space p");

        assert_eq!(walked_dirs(&h), walked, "the removal listed no directories",);
        assert_eq!(base_paths(&h), ["a.rs"], "and the removed file is gone");
    }

    /// A created file is one path the cached list takes on its own.
    #[test]
    fn a_created_file_joins_the_cached_list_without_a_walk() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", "")]);

        h.type_keys("space p");
        h.type_keys("escape");
        h.stoat.drain_index_updates();

        let walked = walked_dirs(&h);
        let epoch = h.stoat.finder_path_epoch;

        h.fake_fs()
            .insert_files([(root.join("b.rs"), "".as_bytes())]);
        h.fake_fs_watcher()
            .inject(root.join("b.rs"), crate::host::FsEventKind::Created);
        debounce::drain_fs_watch_events(&mut h.stoat);

        assert_eq!(
            h.stoat.finder_path_epoch, epoch,
            "the cached list stands rather than being retired",
        );
        h.type_keys("space p");
        assert_eq!(walked_dirs(&h), walked, "the create listed no directories");
        assert_eq!(
            base_paths(&h),
            ["a.rs", "b.rs"],
            "and the new file is there"
        );
    }

    /// A save writes a temp name and renames it over the target, which reaches
    /// the drain as two renames. Neither changes what the list holds, so the
    /// list stands as it was.
    #[test]
    fn a_save_through_a_temp_name_leaves_the_cached_list_unchanged() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", ""), ("b.rs", "")]);

        h.type_keys("space p");
        h.type_keys("escape");
        h.stoat.drain_index_updates();

        let walked = walked_dirs(&h);
        let epoch = h.stoat.finder_path_epoch;

        h.fake_fs_watcher()
            .inject(root.join(".tmpAb12"), crate::host::FsEventKind::Renamed);
        h.fake_fs_watcher()
            .inject(root.join("b.rs"), crate::host::FsEventKind::Renamed);
        debounce::drain_fs_watch_events(&mut h.stoat);

        assert_eq!(
            h.stoat.finder_path_epoch, epoch,
            "the cached list stands rather than being retired",
        );
        h.type_keys("space p");
        assert_eq!(walked_dirs(&h), walked, "the save listed no directories");
        assert_eq!(
            base_paths(&h),
            ["a.rs", "b.rs"],
            "and the list is as it was"
        );
    }

    /// What a walk finds inside a new directory is the question the cache
    /// exists to avoid asking, so the directory retires it.
    #[test]
    fn a_created_directory_retires_the_cached_list() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", "")]);

        h.type_keys("space p");
        h.type_keys("escape");
        h.stoat.drain_index_updates();

        let walked = walked_dirs(&h);
        let epoch = h.stoat.finder_path_epoch;

        h.fake_fs()
            .insert_files([(root.join("nested/c.rs"), "".as_bytes())]);
        h.fake_fs_watcher()
            .inject(root.join("nested"), crate::host::FsEventKind::Created);
        debounce::drain_fs_watch_events(&mut h.stoat);

        assert!(
            h.stoat.finder_path_epoch > epoch,
            "an unlistable change retires the cached list",
        );
        h.type_keys("space p");
        assert!(
            walked_dirs(&h) > walked,
            "so the next open walks the tree again",
        );
        assert_eq!(
            base_paths(&h),
            ["a.rs", "nested/c.rs"],
            "and finds what the directory holds",
        );
    }

    /// The cross-workspace walk collects paths from every known root, so filing
    /// them under the one workspace this finder was rooted at would hand the
    /// next open another workspace's files.
    #[test]
    fn a_cross_workspace_close_caches_nothing() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", ""), ("b.rs", "")]);

        h.type_keys("space p");
        h.type_keys("backtab");
        h.type_keys("backtab");
        assert_eq!(
            h.stoat.file_finder.as_ref().expect("finder open").scope(),
            &FinderScope::AllWorkspaces,
            "with no named scopes, two backtabs land on AllWorkspaces",
        );
        h.type_keys("escape");

        assert!(
            h.stoat.finder_path_cache.is_none(),
            "a multi-root list is not this root's list",
        );
    }

    fn base_paths(h: &TestHarness) -> Vec<String> {
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        let mut base: Vec<String> = finder
            .core
            .picklist
            .base
            .iter()
            .map(|path| paths::display_relative(path, &finder.core.git_root))
            .collect();
        base.sort();
        base
    }

    /// Land `rel` on the finder the way a walk batch does, then let it refilter.
    ///
    /// Stands in for `PathPicker::pump_walk` without a live channel, so it has
    /// to reproduce both of its effects. The paths are appended, and the query
    /// cache is invalidated so the matcher re-runs over the grown base.
    fn deliver_walk_batch(h: &mut TestHarness, rel: &[&str]) {
        {
            let finder = h.stoat.file_finder.as_mut().expect("finder open");
            let root = finder.core.git_root.clone();
            finder
                .core
                .all_paths
                .extend(rel.iter().map(|r| root.join(r)));
            finder.core.invalidate();
        }
        crate::action_handlers::sync_file_finder_preview(&mut h.stoat);
    }

    /// A walk streams in batches, so the scope's matches accumulate across them.
    /// Testing only the new tail is what keeps that from costing the whole list
    /// per batch, and the earlier batches' matches have to survive it.
    #[test]
    fn a_named_scope_keeps_matches_from_every_walk_batch() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("src/a.rs", ""), ("docs/x.md", "")]);
        h.stoat.settings.finder_scopes =
            BTreeMap::from([("code".to_string(), vec!["src/**".to_string()])]);

        h.type_keys("space p");
        h.type_keys("backtab");
        h.type_keys("backtab");
        assert_eq!(base_paths(&h), vec!["src/a.rs"], "the first batch matched");

        deliver_walk_batch(&mut h, &["src/b.rs", "docs/y.md"]);
        assert_eq!(
            base_paths(&h),
            vec!["src/a.rs", "src/b.rs"],
            "the later batch adds its match without losing or repeating the earlier one",
        );

        deliver_walk_batch(&mut h, &["docs/z.md"]);
        assert_eq!(
            base_paths(&h),
            vec!["src/a.rs", "src/b.rs"],
            "a batch matching nothing leaves the accumulated matches alone",
        );
    }

    /// A different name is a different globset, so its matches cannot be
    /// appended to what the previous scope accumulated.
    #[test]
    fn switching_named_scopes_rebuilds_rather_than_appending() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("src/a.rs", ""), ("docs/b.md", "")]);
        h.stoat.settings.finder_scopes = BTreeMap::from([
            ("code".to_string(), vec!["src/**".to_string()]),
            ("prose".to_string(), vec!["docs/**".to_string()]),
        ]);

        h.type_keys("space p");
        h.type_keys("backtab");
        h.type_keys("backtab");
        assert_eq!(base_paths(&h), vec!["src/a.rs"], "the code scope");

        h.type_keys("backtab");
        assert_eq!(
            base_paths(&h),
            vec!["docs/b.md"],
            "the prose scope lists its own matches, not both scopes'",
        );
    }

    /// Displayed rows of the finder's current filtered list, through the same
    /// resolver the renderer uses.
    fn finder_rows(h: &TestHarness) -> Vec<String> {
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        let core = finder.active_core_ref();
        let list = &core.picklist;
        let mut rows: Vec<String> = list
            .filtered
            .iter()
            .map(|&i| {
                crate::picker::row_display(
                    &list.base[i],
                    &core.git_root,
                    list.display_roots.as_deref(),
                    None,
                )
            })
            .collect();
        rows.sort();
        rows
    }

    /// Drive frames until a fallback of ignored files stands in with rows.
    ///
    /// The list settles in one frame, the fallback installs in the next, and
    /// its first scan lands a frame after its walk, so active alone is too
    /// early to read rows from.
    fn settle_fallback(h: &mut TestHarness) {
        for _ in 0..8 {
            let _ = h.snapshot();
            h.settle();
            let finder = h.stoat.file_finder.as_ref().expect("finder open");
            if finder.fallback_active() && !finder_rows(h).is_empty() {
                return;
            }
        }
        panic!("no fallback of ignored files stood in with rows after eight frames");
    }

    #[test]
    fn a_query_no_listed_file_matches_lists_ignored_files() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(
            &mut h,
            &[
                ("src/main.rs", ""),
                (".gitignore", "out/\n"),
                ("out/main.toml", ""),
            ],
        );

        h.type_keys("space p");
        h.type_text("main.toml");
        settle_fallback(&mut h);
        assert_eq!(finder_rows(&h), ["out/main.toml"]);

        h.type_keys("backspace backspace backspace backspace backspace");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        assert_eq!(
            (finder_rows(&h), finder.fallback_active()),
            (vec!["src/main.rs".to_string()], false),
            "a listed match takes the list back",
        );
    }

    #[test]
    fn a_browse_of_a_directory_whose_files_are_all_ignored_lists_them() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[("build/.gitignore", "*\n"), ("build/out.bin", "")],
        );

        h.type_keys("space p");
        h.type_text(&format!("{}/build/", root.display()));
        settle_fallback(&mut h);

        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        assert_eq!(
            (
                finder_rows(&h),
                finder
                    .browse
                    .as_ref()
                    .map(|browse| browse.ignored.is_some()),
            ),
            (
                vec![".gitignore".to_string(), "out.bin".to_string()],
                Some(true),
            ),
        );
    }

    #[test]
    fn a_scope_other_than_all_takes_no_fallback() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("src/main.rs", "")]);

        h.type_keys("space p");
        h.type_keys("backtab");
        h.type_text("zzz");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        assert_eq!(
            (finder.scope(), finder.ignored.is_none()),
            (&FinderScope::Modified, true),
            "a Modified list that matches nothing keeps its empty rows",
        );
    }

    /// A fallback built under All stays through a flip to Modified, and it
    /// stands in for no list but the one of All.
    #[test]
    fn a_fallback_built_under_all_stands_in_for_all_alone() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(
            &mut h,
            &[
                ("src/main.rs", ""),
                (".gitignore", "out/\n"),
                ("out/main.toml", ""),
            ],
        );
        h.type_keys("space p");
        h.type_text("main.toml");
        settle_fallback(&mut h);
        h.type_keys("backtab");
        for _ in 0..2 {
            let _ = h.snapshot();
            h.settle();
        }

        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        assert_eq!(
            (
                finder.scope(),
                finder.ignored.is_some(),
                finder.fallback_active(),
                finder_rows(&h)
            ),
            (&FinderScope::Modified, true, false, Vec::<String>::new()),
        );
    }

    #[test]
    fn closing_with_a_fallback_leaves_the_registry_at_its_pre_open_size() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(
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

        h.type_keys("space p");
        h.type_text("main.toml");
        settle_fallback(&mut h);
        h.type_keys("escape");

        assert!(h.stoat.file_finder.is_none());
        assert_eq!(
            slots(&h),
            before,
            "the fallback's preview buffer and editor go with the finder",
        );
    }

    /// A finder whose fallback of ignored files stood in for `main.toml`, with
    /// the query then cut back to `main`, which the listed `src/main.rs`
    /// matches.
    fn finder_back_on_a_listed_match() -> TestHarness {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(
            &mut h,
            &[
                ("src/main.rs", ""),
                (".gitignore", "out/\n"),
                ("out/main.toml", ""),
            ],
        );
        h.type_keys("space p");
        h.type_text("main.toml");
        settle_fallback(&mut h);

        h.type_keys("backspace backspace backspace backspace backspace");
        for _ in 0..2 {
            let _ = h.snapshot();
            h.settle();
        }
        h
    }

    /// The query the workspace fallback last took, or `None` with no fallback.
    fn fallback_query(h: &TestHarness) -> Option<String> {
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        finder.ignored.as_ref().map(|f| f.last_filter_text.clone())
    }

    #[test]
    fn a_listed_match_leaves_the_fallback_idle() {
        let mut h = finder_back_on_a_listed_match();
        assert_eq!(
            (finder_rows(&h), fallback_query(&h)),
            (vec!["src/main.rs".to_string()], Some(String::new())),
            "the list shows its match and the fallback holds no query",
        );

        h.type_text(".toml");
        settle_fallback(&mut h);
        assert_eq!(finder_rows(&h), ["out/main.toml"]);
    }

    #[test]
    fn settling_a_listed_match_leaves_the_fallback_unsettled() {
        let mut h = finder_back_on_a_listed_match();
        let finder = h.stoat.file_finder.as_mut().expect("finder open");
        finder.settle_scan("main");
        assert_eq!(fallback_query(&h), Some(String::new()));
    }

    /// A settle of a query that no listed file matches settles the fallback
    /// too. An action then reads the row that stands in for the list.
    #[test]
    fn settling_a_query_no_listed_file_matches_settles_the_fallback() {
        let mut h = finder_back_on_a_listed_match();
        let finder = h.stoat.file_finder.as_mut().expect("finder open");
        finder.settle_scan("toml");
        assert_eq!(fallback_query(&h), Some("toml".to_owned()));
    }

    /// A browse settles its fallback by the same rule. `md` lists `notes.md`,
    /// so the fallback that stood in for `mdx` stays unsettled.
    #[test]
    fn settling_a_listed_browse_match_leaves_its_fallback_unsettled() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[
                ("build/.gitignore", "*.bin\n"),
                ("build/mdx.bin", ""),
                ("build/notes.md", ""),
            ],
        );
        h.type_keys("space p");
        h.type_text(&format!("{}/build/mdx", root.display()));
        settle_fallback(&mut h);

        h.type_keys("backspace");
        for _ in 0..2 {
            let _ = h.snapshot();
            h.settle();
        }
        let finder = h.stoat.file_finder.as_mut().expect("finder open");
        finder.settle_scan("");

        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        let ignored = finder.browse.as_ref().and_then(|b| b.ignored.as_ref());
        assert_eq!(
            (finder_rows(&h), ignored.map(|f| f.last_filter_text.clone())),
            (vec!["notes.md".to_string()], Some(String::new())),
        );
    }

    #[test]
    fn workspace_finder_merges_roots_with_the_owning_root_prefix() {
        let mut h = crate::Stoat::test();
        let root_a = PathBuf::from("/ws-a");
        let root_b = PathBuf::from("/ws-b");
        h.fake_fs().insert_files([
            (root_a.join("alpha.rs"), b"a".as_slice()),
            (root_b.join("beta.rs"), b"b".as_slice()),
        ]);
        h.stoat.active_workspace_mut().git_root = root_a.clone();
        let second = h.create_workspace();
        h.stoat.workspaces[second].git_root = root_b.clone();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceFileFinder);
        h.settle();
        let _ = h.snapshot();

        assert_eq!(
            h.stoat.file_finder.as_ref().unwrap().scope(),
            &FinderScope::AllWorkspaces
        );
        assert_eq!(
            finder_rows(&h),
            vec!["ws-a/alpha.rs".to_string(), "ws-b/beta.rs".to_string()],
            "both roots merge and rows carry the owning workspace basename"
        );
    }

    #[test]
    fn workspace_leader_binding_opens_the_cross_workspace_finder() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "")]);
        h.type_action("OpenWorkspaceFileFinder()");
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        assert_eq!(finder.scope(), &FinderScope::AllWorkspaces);
        assert_eq!(h.snapshot().mode, "insert");
    }

    #[test]
    fn backtab_cycle_reaches_all_workspaces() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "")]);

        h.type_keys("space p");
        assert_eq!(
            h.stoat.file_finder.as_ref().unwrap().scope(),
            &FinderScope::All
        );

        h.type_keys("backtab");
        assert_eq!(
            h.stoat.file_finder.as_ref().unwrap().scope(),
            &FinderScope::Modified
        );

        h.type_keys("backtab");
        let finder = h.stoat.file_finder.as_ref().unwrap();
        assert_eq!(
            finder.scope(),
            &FinderScope::AllWorkspaces,
            "with no named scopes, backtab past Modified lands on AllWorkspaces"
        );
        assert!(
            finder.core.picklist.display_roots.is_some(),
            "entering AllWorkspaces installs the cross-workspace display resolver"
        );

        h.type_keys("backtab");
        let finder = h.stoat.file_finder.as_ref().unwrap();
        assert_eq!(
            finder.scope(),
            &FinderScope::All,
            "backtab past AllWorkspaces wraps to All"
        );
        assert!(
            finder.core.picklist.display_roots.is_none(),
            "leaving AllWorkspaces drops the display resolver"
        );
    }

    #[test]
    fn space_p_reopens_last_scope() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", ""), ("b.rs", "")]);

        h.type_keys("space p");
        h.type_keys("backtab");
        h.type_keys("escape");
        assert!(h.stoat.file_finder.is_none(), "escape closes the finder");

        h.type_keys("space p");
        assert_eq!(
            h.stoat.file_finder.as_ref().unwrap().scope(),
            &FinderScope::Modified,
            "space p reopens in the scope the finder last closed in"
        );
    }

    #[test]
    fn space_p_falls_through_stale_remembered_scope_to_default() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("src/a.rs", "")]);
        h.stoat.settings.finder_scopes =
            BTreeMap::from([("code".to_string(), vec!["src/**".to_string()])]);
        h.stoat.settings.finder_default_scope = Some("code".to_string());
        h.stoat.active_workspace_mut().last_finder_scope = Some("ghost".to_string());

        h.type_keys("space p");
        assert_eq!(
            h.stoat.file_finder.as_ref().unwrap().scope(),
            &FinderScope::Named("code".to_string()),
            "a remembered name with no matching scope falls through to the default"
        );
    }

    #[test]
    fn space_p_opens_all_when_nothing_valid_is_remembered_or_configured() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "")]);
        h.stoat.active_workspace_mut().last_finder_scope = Some("ghost".to_string());

        h.type_keys("space p");
        assert_eq!(
            h.stoat.file_finder.as_ref().unwrap().scope(),
            &FinderScope::All,
            "a stale name with no valid default resolves to All"
        );
    }

    #[test]
    fn scope_persist_name_and_validation() {
        let named = BTreeMap::from([("code".to_string(), vec!["src/**".to_string()])]);

        assert_eq!(FinderScope::All.persist_name().as_deref(), Some("all"));
        assert_eq!(
            FinderScope::Modified.persist_name().as_deref(),
            Some("modified")
        );
        assert_eq!(
            FinderScope::Named("code".to_string())
                .persist_name()
                .as_deref(),
            Some("code")
        );
        assert_eq!(FinderScope::Buffers.persist_name(), None);
        assert_eq!(FinderScope::ModifiedBuffers.persist_name(), None);
        assert_eq!(FinderScope::Diff.persist_name(), None);

        assert_eq!(
            FinderScope::from_persist_name("all", &named),
            Some(FinderScope::All)
        );
        assert_eq!(
            FinderScope::from_persist_name("modified", &named),
            Some(FinderScope::Modified)
        );
        assert_eq!(
            FinderScope::from_persist_name("code", &named),
            Some(FinderScope::Named("code".to_string()))
        );
        assert_eq!(FinderScope::from_persist_name("ghost", &named), None);
        assert_eq!(
            FinderScope::from_persist_name("buffers", &named),
            None,
            "buffers is never a sticky or default scope"
        );
        assert_eq!(
            FinderScope::from_persist_name("modified_buffers", &named),
            None,
            "modified_buffers is never a sticky or default scope"
        );
        assert_eq!(
            FinderScope::from_persist_name("diff", &named),
            None,
            "diff is never a sticky or default scope"
        );
    }

    #[test]
    fn walk_completion_signals_redraw_notify() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", ""), ("b.rs", "")]);
        h.type_keys("space p");
        let notified = h.stoat.redraw_notify.notified();
        tokio::pin!(notified);
        assert!(
            notified.enable(),
            "walk task should signal redraw_notify on completion so the \
             main loop wakes up and renders the populated list",
        );
    }

    #[test]
    fn typing_narrows_filtered_list() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(
            &mut h,
            &[("alpha.rs", ""), ("beta.rs", ""), ("gamma.rs", "")],
        );
        h.type_keys("space p");
        // refilter is driven by the render loop; force a snapshot so
        // filtered reflects the current (empty) query.
        let _ = h.snapshot();
        assert_eq!(
            h.stoat
                .file_finder
                .as_ref()
                .unwrap()
                .core
                .picklist
                .filtered
                .len(),
            3
        );
        h.type_text("alp");
        let _ = h.snapshot();
        let finder = h.stoat.file_finder.as_ref().unwrap();
        assert_eq!(finder.core.picklist.filtered.len(), 1);
        let idx = finder.core.picklist.filtered[0];
        assert!(finder.core.picklist.base[idx].ends_with("alpha.rs"));
    }

    /// The finder's current query text.
    fn finder_query(h: &TestHarness) -> String {
        h.stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .input
            .text(h.stoat.active_workspace())
    }

    #[test]
    fn tab_completes_the_highlighted_row() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("src/alpha.rs", ""), ("src/beta.rs", "")]);

        h.type_keys("space p");
        h.type_text("alp");
        let _ = h.snapshot();

        h.type_keys("tab");
        let _ = h.snapshot();

        assert_eq!(
            finder_query(&h),
            "src/alpha.rs",
            "Tab completes the highlighted row into the query"
        );
        assert_eq!(
            h.stoat
                .file_finder
                .as_ref()
                .expect("finder open")
                .active_core_ref()
                .picklist
                .selected,
            0,
            "the completed row is the selection"
        );
    }

    #[test]
    fn tab_lists_the_rows_of_the_completed_query() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("src/alpha.rs", ""), ("src/alpine.rs", "")]);

        h.type_keys("space p");
        h.type_text("alp");
        let _ = h.snapshot();

        h.type_keys("tab");
        let _ = h.snapshot();

        assert_eq!(
            finder_rows(&h),
            vec!["src/alpha.rs".to_string()],
            "the completed query drops the row whose name has no h"
        );
    }

    #[test]
    fn tab_completes_a_browse_row_under_the_typed_prefix() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("f.rs", "")]);
        let home = PathBuf::from("/fake-home");
        h.fake_fs()
            .insert_files([(home.join("alpha.rs"), "a".as_bytes())]);
        h.fake_env().set("HOME", home.to_str().unwrap());

        h.type_keys("space p");
        h.type_text("~/al");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();

        h.type_keys("tab");
        let _ = h.snapshot();

        assert_eq!(
            finder_query(&h),
            "~/alpha.rs",
            "a browse completion keeps the typed prefix and appends the row name"
        );
    }

    /// The completed query still matches more than one row here, so the
    /// selection reset is what decides which file Enter opens. A refilter alone
    /// only clamps the stale index into range, which would leave it pointing at
    /// the longer sibling.
    #[test]
    fn tab_then_enter_opens_the_completed_file() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(
            &mut h,
            &[
                ("src/alpha.rs", "ALPHA-CONTENT"),
                ("src/alphabet.rs", "ALPHABET-CONTENT"),
                ("src/alphabetical.rs", "ALPHABETICAL-CONTENT"),
            ],
        );

        h.type_keys("space p");
        h.type_text("alp");
        let _ = h.snapshot();

        h.type_keys("down");
        let _ = h.snapshot();
        assert_eq!(
            finder_rows(&h),
            vec![
                "src/alpha.rs".to_string(),
                "src/alphabet.rs".to_string(),
                "src/alphabetical.rs".to_string(),
            ],
            "all three rows match the partial query"
        );

        // Enter must read the selection the completion already reset, so the
        // two keys go through with no settle between them.
        h.type_keys("tab enter");
        h.settle();

        let frame = h.snapshot();
        assert!(
            frame.content.contains("ALPHABET-CONTENT")
                && !frame.content.contains("ALPHABETICAL-CONTENT"),
            "Tab then Enter opens the completed row, not its longer sibling, got:\n{}",
            frame.content
        );
        assert!(h.stoat.file_finder.is_none(), "the finder closed");
    }

    #[test]
    fn split_path_query_parses_path_shaped_queries() {
        let home = Some("/home/u");
        assert_eq!(
            split_path_query("/etc/ho", home),
            Some(("/etc/".to_string(), p("/etc/"), "ho".to_string()))
        );
        assert_eq!(
            split_path_query("~/proj/sto", home),
            Some(("~/proj/".to_string(), p("/home/u/proj"), "sto".to_string()))
        );
        assert_eq!(
            split_path_query("~/", home),
            Some(("~/".to_string(), p("/home/u"), String::new()))
        );
        assert_eq!(
            split_path_query("/", home),
            Some(("/".to_string(), p("/"), String::new()))
        );
        assert_eq!(split_path_query("foo", home), None, "not path-shaped");
        assert_eq!(
            split_path_query("foo/bar", home),
            None,
            "not / or ~/ prefixed"
        );
        assert_eq!(split_path_query("~/x", None), None, "no HOME");
    }

    fn browse_root(h: &TestHarness) -> PathBuf {
        h.stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .browse
            .as_ref()
            .expect("browse active")
            .root
            .clone()
    }

    #[test]
    fn browse_activates_on_home_query_and_lists_files() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("ws.rs", "")]);
        let home = PathBuf::from("/fake-home");
        h.fake_fs().insert_files([
            (home.join("note.md"), "n".as_bytes()),
            (home.join("todo.md"), "t".as_bytes()),
        ]);
        h.fake_env().set("HOME", home.to_str().unwrap());

        h.type_keys("space p");
        h.type_text("~/");
        let _ = h.snapshot();
        h.settle();
        let content = h.snapshot().content.clone();

        assert!(
            content.contains("Files \u{00B7} browse"),
            "browse label missing:\n{content}"
        );
        let header = content
            .lines()
            .find(|line| line.contains("Files \u{00B7} browse"))
            .expect("the header row");
        assert!(
            header
                .trim_end()
                .trim_end_matches('\u{2502}')
                .trim_end()
                .ends_with("2 of 2"),
            "a browse ends its header at the count, with no scope pill:\n{header}"
        );
        assert_eq!(browse_root(&h), home);
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        let browse = finder.browse.as_ref().expect("browse active");
        let rows: Vec<String> = browse
            .picker
            .picklist
            .filtered
            .iter()
            .map(|&i| {
                paths::display_relative(&browse.picker.picklist.base[i], &browse.picker.git_root)
            })
            .collect();
        assert_eq!(rows, vec!["note.md", "todo.md"]);
    }

    #[test]
    fn browse_reroots_on_a_deeper_segment() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("ws.rs", "")]);
        let home = PathBuf::from("/fake-home");
        h.fake_fs()
            .insert_files([(home.join("sub/deep.md"), "d".as_bytes())]);
        h.fake_env().set("HOME", home.to_str().unwrap());

        h.type_keys("space p");
        h.type_text("~/");
        let _ = h.snapshot();
        assert_eq!(browse_root(&h), home);

        h.type_text("sub/");
        let _ = h.snapshot();
        assert_eq!(
            browse_root(&h),
            home.join("sub"),
            "re-roots to the deeper dir"
        );
    }

    #[test]
    fn leaving_browse_disposes_the_browse_preview() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("ws.rs", "")]);
        let home = PathBuf::from("/fake-home");
        h.fake_fs()
            .insert_files([(home.join("note.md"), "n".as_bytes())]);
        h.fake_env().set("HOME", home.to_str().unwrap());

        h.type_keys("space p");
        h.type_text("~/");
        let _ = h.snapshot();
        let browse_preview = h
            .stoat
            .file_finder
            .as_ref()
            .unwrap()
            .browse
            .as_ref()
            .expect("browse active")
            .picker
            .preview
            .buffer;
        assert!(h
            .stoat
            .active_workspace()
            .buffers
            .get(browse_preview)
            .is_some());

        h.type_keys("backspace backspace");
        let _ = h.snapshot();
        assert!(
            h.stoat.file_finder.as_ref().unwrap().browse.is_none(),
            "browse leaves when the path prefix is deleted"
        );
        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .get(browse_preview)
                .is_none(),
            "browse preview disposed on leave"
        );

        // The query `~` on the way out matches nothing in the workspace list,
        // which installs a fallback of ignored files with a preview of its own.
        let finder = h.stoat.file_finder.as_ref().unwrap();
        let mut live: Vec<_> = [Some(&finder.core), finder.ignored.as_ref()]
            .into_iter()
            .flatten()
            .map(|picker| picker.preview.buffer)
            .collect();
        live.sort();
        let mut previews = h.stoat.active_workspace().buffers.preview_buffer_ids();
        previews.sort();
        assert_eq!(
            previews, live,
            "every preview left belongs to a picker still live"
        );
    }

    // ----- Snapshot tests -----

    /// Point the active workspace at a fixed, nonexistent path so the walker
    /// returns nothing. Produces a stable empty-list snapshot regardless of
    /// test-run cwd; the workspace basename also renders deterministically
    /// in the status bar.
    fn seed_empty_finder_workspace(h: &mut TestHarness) {
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/stoat-finder-test-empty");
    }

    #[test]
    fn snapshot_file_finder_empty() {
        let mut h = crate::Stoat::test();
        seed_empty_finder_workspace(&mut h);
        h.type_keys("space p");
        h.assert_snapshot("file_finder_empty");
    }

    #[test]
    fn snapshot_file_finder_tiny_terminal_no_render() {
        let mut h = TestHarness::with_size(30, 8);
        seed_empty_finder_workspace(&mut h);
        h.type_keys("space p");
        h.assert_snapshot("file_finder_tiny_terminal_no_render");
    }

    #[test]
    fn snapshot_file_finder_multi_token_highlight() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(
            &mut h,
            &[
                ("src/foo.rs", "fn foo() {}"),
                ("src/bar.rs", "fn bar() {}"),
                ("docs/foo.md", "foo"),
            ],
        );
        h.type_keys("space p");
        h.type_text(".rs foo");
        h.assert_snapshot("file_finder_multi_token_highlight");
    }

    /// Under ermine_night the finder's selected row fills with the tab wash.
    /// Over that fill the directory reads muted, the name bold in the text
    /// color, a matched char warning in bold, and the extension bold in the
    /// Rust tag color.
    #[test]
    fn the_finder_paints_its_rows_in_the_list_scopes() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("src/foo.rs", "fn foo() {}")]);
        h.type_keys("space p");
        h.type_text("fo");
        h.settle();

        let list = test_fixture::finder_layout(&h).list;
        let buf = h.render_composited();
        let style = |col: u16| {
            let cell = &buf[(list.x + col, list.y)];
            (cell.fg, cell.bg, cell.modifier)
        };
        let wash = Color::Rgb(0x24, 0x35, 0x52);
        let muted = Color::Rgb(0x83, 0x8a, 0x98);
        let warning = Color::Rgb(0xe8, 0xbd, 0x6a);
        let text = Color::Rgb(0xdf, 0xe2, 0xe8);

        assert_eq!(
            [style(1), style(5), style(7), style(9)],
            [
                (muted, wash, Modifier::empty()),
                (warning, wash, Modifier::BOLD),
                (text, wash, Modifier::BOLD),
                (warning, wash, Modifier::BOLD),
            ],
            "the s of src/, the matched f, the unmatched second o, and the r of .rs",
        );
    }

    /// A row shows its directory muted, its name bold in the text color, and
    /// its extension bold in the tag color, and a modified file's row ends in
    /// an `M` in the danger color that ermine_night gives `ui.list.modified`.
    /// A file with no tag keeps its extension in the style of its name, and
    /// both rows start their labels in the same column.
    #[test]
    fn a_row_shows_its_directory_name_extension_and_modified_mark() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("src/a.rs", "v2\n"), ("b.txt", "b\n")]);
        h.fake_git()
            .add_repo(&root)
            .with_fs(h.fake_fs())
            .modified("src/a.rs", "v1\n", "v2\n")
            .head_file("b.txt", "b\n");
        h.type_keys("space p");
        h.settle();

        let list = test_fixture::finder_layout(&h).list;
        let buf = h.render_composited();
        let text = |y: u16| -> String {
            (list.x..list.x + list.width)
                .map(|x| buf[(x, y)].symbol())
                .collect()
        };
        let row_of = |name: &str| {
            (list.y..list.y + list.height)
                .find(|&y| text(y).contains(name))
                .expect("the file lists")
        };
        let (a, b) = (row_of("src/a.rs"), row_of("b.txt"));
        let glyph = |col: u16, y: u16| {
            let cell = &buf[(list.x + col, y)];
            let bold = cell.modifier.contains(Modifier::BOLD);
            (cell.symbol().to_owned(), cell.fg, bold)
        };

        let warning = Color::Rgb(0xe8, 0xbd, 0x6a);
        let danger = Color::Rgb(0xf2, 0x77, 0x7f);
        let muted = Color::Rgb(0x83, 0x8a, 0x98);
        let body = Color::Rgb(0xdf, 0xe2, 0xe8);
        let last = list.width - 1;
        assert_eq!(
            [
                glyph(1, a),
                glyph(5, a),
                glyph(7, a),
                glyph(last, a),
                glyph(1, b),
                glyph(2, b),
                glyph(last, b),
            ],
            [
                ("s".to_owned(), muted, false),
                ("a".to_owned(), body, true),
                ("r".to_owned(), warning, true),
                ("M".to_owned(), danger, false),
                ("b".to_owned(), body, true),
                (".".to_owned(), body, true),
                (" ".to_owned(), body, false),
            ],
            "the s of src/, the a of a.rs, the r of .rs, and the mark, then the untagged clean row",
        );
    }

    /// The window of a list whose labels wrap shows fewer results than it has
    /// rows, so a page step moves by half the results that it shows whole,
    /// rounded up.
    ///
    /// Each of the 30 files wraps onto `h` rows at any list width, so the list
    /// shows `list.height / h` of them whole, at least one.
    #[test]
    fn a_page_step_moves_by_half_the_results_that_the_window_shows() {
        let mut h = crate::Stoat::test();
        let stem = "x".repeat(300);
        let names: Vec<String> = (0..30).map(|n| format!("d/{stem}{n:02}.rs")).collect();
        let files: Vec<(&str, &str)> = names.iter().map(|name| (name.as_str(), "x")).collect();
        seed_finder_workspace(&mut h, &files);
        h.type_keys("space p");
        h.settle();
        h.stoat.render();

        let list = test_fixture::finder_layout(&h).list;
        let rows = 1 + 305usize.div_ceil(crate::render::picker::label_columns(list.width) - 4);
        let shown = (usize::from(list.height) / rows).max(1);
        let picklist = |h: &TestHarness| {
            let finder = h.stoat.file_finder.as_ref().expect("the finder is open");
            let list = &finder.active_core_ref().picklist;
            (list.viewport_rows, list.selected)
        };
        let rendered = picklist(&h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::PickerPageDown);

        assert_eq!(
            (rendered, picklist(&h)),
            ((Some(shown), 0), (Some(shown), shown.div_ceil(2))),
        );
    }

    /// The preview opens with the shown file's directory and name, which stand
    /// with no cell between them, and the lines the pane shows one column from
    /// its right edge.
    #[test]
    fn the_preview_header_names_the_file_and_its_shown_lines() {
        let mut h = TestHarness::with_size(120, 30);
        seed_finder_workspace(&mut h, &[("src/a.rs", "one\ntwo\nthree")]);
        h.type_keys("space p");
        h.settle();

        let header = test_fixture::finder_layout(&h)
            .preview_header
            .expect("the preview has a header");
        let buf = h.render_composited();
        let text: String = (header.x..header.x + header.width)
            .map(|x| buf[(x, header.y)].symbol())
            .collect();
        assert_eq!(
            (
                text.split_whitespace().collect::<Vec<_>>(),
                text.ends_with("1\u{2013}3 ")
            ),
            (vec!["src/a.rs", "lines", "1\u{2013}3"], true),
            "the header row reads {text:?}",
        );
    }

    /// A query that matches nothing, with no ignored file to fall back on,
    /// keeps the full box. Its list names the query, its preview reads no
    /// preview, its key line stays, and the count's zero reads in the deleted
    /// color.
    #[test]
    fn snapshot_finder_with_no_match_names_its_query() {
        let mut h = TestHarness::with_size(120, 30);
        seed_finder_workspace(&mut h, &[("src/a.rs", "one"), ("b.txt", "two")]);
        h.type_keys("space p");
        h.settle();
        let full = test_fixture::finder_layout(&h).modal;

        h.type_text("zzqxj");
        h.settle();
        assert_eq!(
            test_fixture::finder_layout(&h).modal,
            full,
            "the box keeps its size and place",
        );
        h.assert_snapshot("file_finder_no_match");
    }

    /// A walk that still streams does not count as no match, though no row has
    /// arrived. A fresh open thus does not flash the no-match message before
    /// its first paths.
    ///
    /// The harness walk delivers inside the key that opens the finder, so the
    /// test holds a fresh walk open in its place.
    #[test]
    fn a_finder_shows_no_message_while_the_walk_streams() {
        let mut h = TestHarness::with_size(120, 30);
        seed_finder_workspace(&mut h, &[("a.rs", "one")]);
        h.type_keys("space p");
        h.settle();

        let (_walk, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = h.stoat.executor.spawn(async {});
        h.stoat
            .file_finder
            .as_mut()
            .expect("the finder is open")
            .core
            .reset_walk(rx, task);

        let finder = h.stoat.file_finder.as_ref().expect("the finder is open");
        let core = finder.active_core_ref();
        assert_eq!(
            (
                core.walking(),
                core.picklist.filtered.len(),
                finder.shows_no_match(),
            ),
            (true, 0, false),
            "a walk with no row yet shows no message",
        );
    }

    /// The finder modal is opaque, so a short preview file's blank rows render
    /// blank rather than leaking the editor content the centered modal is drawn
    /// over.
    #[test]
    fn snapshot_finder_preview_clears_short_file_background() {
        let mut h = TestHarness::with_size(120, 30);
        let filler: String = (0..40)
            .map(|i| format!("background row {i:02} {}\n", "=".repeat(100)))
            .collect();
        let root = seed_finder_workspace(
            &mut h,
            &[("short.txt", "alpha\nbravo\n"), ("filler.txt", &filler)],
        );
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("filler.txt"),
            },
        );
        h.settle();

        h.type_keys("space p");
        h.type_text("short");
        h.assert_snapshot("finder_preview_clears_short_file_background");
    }

    /// The preview pane is syntax-highlighted once its parse lands, which is a
    /// frame or two after the selection changes.
    ///
    /// A freshly selected file has no prior tree to carry, so the frames before
    /// its parse completes render in `fallback_style`. That is the price of
    /// parsing off the input thread, and scrolling a preview list is itself an
    /// input-latency path, so it is not one to buy back with a synchronous
    /// first parse.
    #[test]
    fn snapshot_finder_preview_highlighted_once_its_parse_lands() {
        let mut h = TestHarness::with_size(120, 16);
        seed_finder_workspace(
            &mut h,
            &[
                ("aaa.rs", "fn aaa() {}\n"),
                ("zzz.rs", "fn zzz() -> u32 { 0 }\n"),
            ],
        );
        h.type_keys("space p");
        h.settle();

        h.stoat
            .file_finder
            .as_mut()
            .expect("finder open")
            .move_selection(1);
        // Spawn the preview's parse and run it. The snapshot's own background
        // drive is the poll that installs the result.
        h.stoat.drive_background();
        h.settle();
        h.assert_snapshot_one_frame("finder_preview_highlighted_after_parse");
    }

    #[test]
    fn preview_buffer_assigned_language_for_selected_path() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("main.rs", "fn main() {}\n")]);
        h.type_keys("space p");
        h.snapshot();
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        let preview_id = finder.core.preview.buffer;
        let ws = h.stoat.active_workspace();
        let lang = ws.buffers.language_for(preview_id).expect("language set");
        assert_eq!(lang.name, "rust");
    }

    #[test]
    fn switching_preview_clears_prior_syntax_state() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "fn a() {}\n"), ("b.toml", "[pkg]\n")]);
        h.type_keys("space p");
        h.snapshot();

        let preview_id = h
            .stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .core
            .preview
            .buffer;
        let lang_first = h
            .stoat
            .active_workspace()
            .buffers
            .language_for(preview_id)
            .expect("first language");

        h.type_keys("down");
        h.snapshot();

        let lang_second = h
            .stoat
            .active_workspace()
            .buffers
            .language_for(preview_id)
            .expect("second language");
        assert_ne!(
            lang_first.name, lang_second.name,
            "language should reflect new path",
        );
    }

    /// The pane shows forty rows, so it has no business holding every file the
    /// session scrolled past. An edit over the whole range would keep two
    /// copies of each: one in the deleted rope and one in the op log.
    #[test]
    fn arrowing_through_the_finder_keeps_only_the_shown_file() {
        let mut h = crate::Stoat::test();
        let bulk = "// filler\n".repeat(400);
        seed_finder_workspace(
            &mut h,
            &[
                ("a.rs", bulk.as_str()),
                ("b.rs", bulk.as_str()),
                ("c.rs", "fn c() {}\n"),
            ],
        );
        h.type_keys("space p");
        h.snapshot();

        let preview_id = h
            .stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .core
            .preview
            .buffer;

        for _ in 0..2 {
            h.type_keys("down");
            h.snapshot();
        }

        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(preview_id)
            .expect("the preview buffer is open");
        let guard = buffer.read().expect("poisoned");
        let shown = guard.snapshot.visible_text.len();

        assert!(shown > 0, "the pane shows a file");
        assert!(
            shown < bulk.len(),
            "and the file it shows is the short one, not one it scrolled past",
        );
        // The op log is the other copy an edit would leave. Its contents are
        // private to the buffer module, where `a_reset_holds_only_the_text_it_was_given`
        // pins them.
        assert_eq!(
            guard.snapshot.deleted_text.len(),
            0,
            "and keeps none of the files it scrolled past",
        );
    }

    /// The pane text the finder currently shows.
    fn preview_text(h: &TestHarness) -> String {
        let preview_id = h
            .stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .core
            .preview
            .buffer;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(preview_id)
            .expect("preview buffer");
        let guard = buffer.read().expect("poisoned");
        guard.rope().to_string()
    }

    /// Walking a list and back up reads nothing it already read.
    ///
    /// Stated through an injected read failure rather than a read count: the
    /// second view of a file shows its text when the cache served it, and the
    /// unreadable placeholder when it went back to disk.
    #[test]
    fn walking_back_up_the_finder_rereads_nothing() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(
            &mut h,
            &[("a.txt", "first file\n"), ("b.txt", "second file\n")],
        );
        h.type_keys("space p");
        h.snapshot();

        let first = preview_text(&h);
        assert!(!first.is_empty(), "the pane shows the first selection");

        h.type_keys("down");
        h.snapshot();
        let second = preview_text(&h);
        assert_ne!(second, first, "and moving down shows the other file");

        // Whichever file the first selection was, reading it again now fails.
        let shown_path = match first.as_str() {
            "first file\n" => root.join("a.txt"),
            _ => root.join("b.txt"),
        };
        h.fake_fs()
            .fail_next_read(&shown_path, std::io::ErrorKind::PermissionDenied);

        h.type_keys("up");
        h.snapshot();

        assert_eq!(
            preview_text(&h),
            first,
            "the walk back up reads nothing, so the armed failure never fires",
        );
    }

    /// Past the cap the oldest path goes, so one session's cache is bounded.
    #[test]
    fn the_preview_cache_evicts_the_oldest_path() {
        let mut h = crate::Stoat::test();
        let files: Vec<(String, String)> = (0..=crate::picker::PREVIEW_TEXT_CACHE_CAP)
            .map(|i| (format!("f{i:03}.txt"), format!("file {i}\n")))
            .collect();
        let root = seed_finder_workspace(
            &mut h,
            &files
                .iter()
                .map(|(name, text)| (name.as_str(), text.as_str()))
                .collect::<Vec<_>>(),
        );
        h.type_keys("space p");
        h.snapshot();

        let first = preview_text(&h);
        // Down past the cap and back to the top, so the first path was evicted.
        for _ in 0..crate::picker::PREVIEW_TEXT_CACHE_CAP {
            h.type_keys("down");
            h.snapshot();
        }

        let evicted = match files.iter().find(|(_, text)| *text == first) {
            Some((name, _)) => root.join(name),
            None => panic!("the first selection shows one of the seeded files"),
        };
        h.fake_fs()
            .fail_next_read(&evicted, std::io::ErrorKind::PermissionDenied);

        for _ in 0..crate::picker::PREVIEW_TEXT_CACHE_CAP {
            h.type_keys("up");
            h.snapshot();
        }

        assert_eq!(
            preview_text(&h),
            "<unreadable>",
            "the oldest path went with the cap, so it read from disk again",
        );
    }

    #[test]
    fn preview_buffer_evicted_on_close() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("main.rs", "fn main() {}\n")]);
        h.type_keys("space p");
        let preview_id = h
            .stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .core
            .preview
            .buffer;
        assert!(h.stoat.active_workspace().buffers.get(preview_id).is_some());

        h.type_keys("escape");

        assert!(h.stoat.file_finder.is_none());
        assert!(
            h.stoat.active_workspace().buffers.get(preview_id).is_none(),
            "preview buffer should be evicted on close",
        );
        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .preview_buffer_ids()
                .is_empty(),
            "no preview buffers remain after close",
        );
    }

    #[test]
    fn finder_input_scratch_not_left_dirty_on_close() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("main.rs", "fn main() {}\n")]);
        let baseline = h.stoat.active_workspace().buffers.dirty_buffers().len();

        h.type_keys("space p");
        h.type_text("main");
        h.type_keys("escape");

        assert!(h.stoat.file_finder.is_none());
        assert_eq!(
            h.stoat.active_workspace().buffers.dirty_buffers().len(),
            baseline,
            "input scratch must not linger as a dirty buffer after the finder closes",
        );
    }

    #[test]
    fn buffer_preview_source_shows_live_text_not_disk() {
        use crate::picker::{Preview, PreviewSource};
        let mut h = crate::Stoat::test();
        let executor = h.stoat.executor.clone();
        let language_registry = h.stoat.language_registry.clone();
        // The fs is empty, so a stray disk read would render a placeholder.
        // Matching the live text proves the Buffer source never touches disk.
        let fs = crate::host::FakeFs::new();

        let ws = h.stoat.active_workspace_mut();
        let (id, _) = ws.buffers.open(&PathBuf::from("/mem/note.txt"), "saved\n");
        {
            let buffer = ws.buffers.get(id).expect("source buffer");
            let mut guard = buffer.write().expect("source buffer poisoned");
            let len = guard.snapshot.visible_text.len();
            guard.edit(0..len, "edited in memory\n");
        }

        let mut preview = Preview::new(ws, executor);
        preview.sync(ws, &fs, &language_registry, PreviewSource::Buffer(id));

        let shown = {
            let buffer = ws.buffers.get(preview.buffer).expect("preview buffer");
            let guard = buffer.read().expect("preview buffer poisoned");
            guard.rope().to_string()
        };
        assert_eq!(shown, "edited in memory\n");

        preview.dispose(ws);
    }

    /// The preview pane scrolls from the keyboard, not the wheel alone, and
    /// doing so leaves the list where it is.
    #[test]
    fn ctrl_d_scrolls_the_preview_and_leaves_the_selection() {
        let mut h = TestHarness::with_size(120, 40);
        let body: String = (0..400).map(|i| format!("line {i}\n")).collect();
        seed_finder_workspace(&mut h, &[("a.rs", &body), ("b.rs", "")]);
        h.type_keys("space p");
        h.snapshot();

        let preview_editor = {
            let finder = h.stoat.file_finder.as_ref().expect("finder open");
            finder.active_core_ref().preview.editor
        };
        let scroll = |h: &TestHarness| {
            h.stoat
                .active_workspace()
                .editors
                .get(preview_editor)
                .expect("preview editor")
                .scroll_row
        };
        let selected_before = h
            .stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .active_core_ref()
            .picklist
            .selected;
        assert_eq!(scroll(&h), 0, "the preview starts at the top");

        h.type_keys("Ctrl-d");

        let selected_after = h
            .stoat
            .file_finder
            .as_ref()
            .expect("the key does not close the finder")
            .active_core_ref()
            .picklist
            .selected;
        assert_eq!(
            (scroll(&h) > 0, selected_after),
            (true, selected_before),
            "the preview scrolls down and the list selection holds"
        );
    }

    /// The code-search modal walks the same tree for the same reason, so a
    /// finder's list answers its first scan.
    #[test]
    fn a_code_search_after_a_finder_close_walks_nothing() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "fn a() {}"), ("b.rs", "fn b() {}")]);

        h.type_keys("space p");
        h.type_keys("escape");
        h.stoat.drain_index_updates();

        let walked = walked_dirs(&h);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        h.type_text("fn");
        h.settle();
        h.advance_clock(debounce::CODE_SEARCH_DEBOUNCE);
        h.settle();

        assert_eq!(
            walked_dirs(&h),
            walked,
            "the scan read the finder's list rather than walking",
        );
        assert_eq!(
            h.stoat
                .code_search
                .as_ref()
                .expect("the modal is open")
                .matches
                .len(),
            2,
            "and matched in both files",
        );
    }

    /// And the other way, so whichever opens first pays for the walk.
    #[test]
    fn a_finder_after_a_code_search_walk_walks_nothing() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(&mut h, &[("a.rs", "fn a() {}"), ("b.rs", "fn b() {}")]);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        h.type_text("fn");
        h.settle();
        h.advance_clock(debounce::CODE_SEARCH_DEBOUNCE);
        h.settle();
        h.type_keys("escape");
        h.settle();
        h.stoat.drain_index_updates();

        let walked = walked_dirs(&h);

        h.type_keys("space p");

        assert_eq!(
            walked_dirs(&h),
            walked,
            "the finder read the modal's walk rather than walking",
        );
        assert_eq!(base_paths(&h), ["a.rs", "b.rs"], "and lists every file");
    }

    /// The startup index build walks the whole tree, so the first finder open
    /// has nothing left to find out.
    ///
    /// That open is the one a user reaches for seconds after launch, and on a
    /// large repository the walk it repeats is the wait they notice.
    #[test]
    fn the_index_build_leaves_the_first_finder_open_nothing_to_walk() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", "fn a() {}"), ("src/b.rs", "")]);
        h.fake_git.add_repo(root.clone());

        h.stoat.start_index_build();
        h.settle();

        h.stoat.drain_index_updates();

        let walked = walked_dirs(&h);
        assert!(walked > 0, "the build walked the tree");

        h.type_keys("space p");

        assert_eq!(
            walked_dirs(&h),
            walked,
            "the open read the build's list rather than walking",
        );
        assert_eq!(
            base_paths(&h),
            ["a.rs", "src/b.rs"],
            "and lists every file the build found",
        );
    }

    /// The build derives the display rows on the pool with the paths, so the
    /// first open derives none on the loop either.
    #[test]
    fn the_index_build_seeds_the_rows_the_first_open_lists() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", "fn a() {}"), ("src/b.rs", "")]);
        h.fake_git.add_repo(root.clone());

        h.stoat.start_index_build();
        h.settle();
        h.stoat.drain_index_updates();
        let seeded = Arc::clone(cached_display(&h).expect("the build seeded rows").rows());

        h.type_keys("space p");

        assert!(
            Arc::ptr_eq(&finder_display_rows(&h), &seeded),
            "the open lists the rows the build derived"
        );
    }

    /// A pixel list counts as its viewport the results whose 21 px rows fit
    /// the list's px and the one gap that stands past its last row.
    #[test]
    fn a_pixel_list_counts_the_rows_that_fit_as_its_viewport() {
        let h = test_fixture::pixel_finder(40);

        let list = test_fixture::finder_layout(&h).list;
        let fit = (usize::from(list.height) * 18 + 1) / 21;
        let finder = h.stoat.file_finder.as_ref().expect("the finder is open");
        assert_eq!(finder.active_core_ref().picklist.viewport_rows, Some(fit));
    }
}
