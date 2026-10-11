use crate::{
    host::{FsHost, GitHost},
    input_view::{InputView, SubmitTarget},
    paths,
    picker::{
        BaseId, DisplayCache, PathPicker, PendingScan, PreviewPolicy, PreviewSource, Scan,
        ScanTarget,
    },
    term_session::TermId,
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

/// Columns [`FileFinder::content_size`] always asks for, matching the recommended
/// box width the renderer sizes against. The list is one path per row, so its
/// length drives height alone and the box only ever widens through zoom.
const FINDER_CONTENT_COLS: u16 = 120;

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
    /// Currently-open path-bound buffers from the workspace's
    /// [`BufferRegistry`], then the workspace's shell terminals. Captured at
    /// open time. Reachable only through the dedicated `OpenBufferPicker`
    /// action. Shift-Tab flips to [`FinderScope::ModifiedBuffers`] and back, so
    /// the buffer picker never enters a file scope.
    Buffers,
    /// The open path-bound buffers whose `dirty` flag is set, which are the
    /// buffers with unsaved edits. Captured at open time like
    /// [`FinderScope::Buffers`]. A terminal carries no dirty flag, so none is
    /// listed. Reachable only through the scope toggle from
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
    pub(crate) fn persist_name(&self) -> Option<String> {
        match self {
            FinderScope::All => Some("all".to_string()),
            FinderScope::Modified => Some("modified".to_string()),
            FinderScope::Named(name) => Some(name.clone()),
            FinderScope::AllWorkspaces => Some("allworkspaces".to_string()),
            FinderScope::Buffers | FinderScope::ModifiedBuffers => None,
        }
    }

    /// Resolve a remembered or configured scope name against the current named
    /// scopes, or `None` when the name matches nothing valid.
    ///
    /// A persisted name whose scope has since been removed from config must
    /// not resurrect, so validation returns `None` and the caller falls
    /// through to its next default. `"buffers"` and `"modified_buffers"` are
    /// intentionally not accepted, as neither is ever a sticky or default scope.
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
    /// The two buffer scopes have names here although they never persist,
    /// since a binding scoped to the buffer picker has to name them. A named
    /// scope reads as its config name.
    pub(crate) fn context_name(&self) -> &str {
        match self {
            FinderScope::All => "all",
            FinderScope::Modified => "modified",
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

/// A terminal row of the [`FinderScope::Buffers`] list.
///
/// The pick list holds paths and derives each row's text from its path, and a
/// relative path displays as written. A terminal rides in the list as a
/// relative path that spells its label, which is then both the row text and
/// what the query matches. This maps that path back to the session.
///
/// The key stays relative for the row paint too. A long relative row loses its
/// tail rather than its head, so the `term <n>` that names it stays on screen.
pub(crate) struct TermRow {
    pub(crate) key: PathBuf,
    pub(crate) term: TermId,
}

pub struct FileFinder {
    pub(crate) input: InputView,
    /// What submit should do with the selected file.
    pub(crate) open_intent: OpenIntent,
    pub(crate) scope: FinderScope,
    /// Absolute paths of currently-modified files. Re-queried on scope toggle.
    ///
    /// Empty until the first background query lands, and thereafter the
    /// previous answer while a re-query is in flight, so a refresh never blinks
    /// the rows empty.
    pub(crate) modified_paths: Vec<PathBuf>,
    /// The in-flight git-status query feeding [`Self::modified_paths`], and the
    /// task running it.
    ///
    /// The query is a full status pass with untracked recursion, far too slow
    /// to run where a keystroke waits on it, so it runs on the blocking pool and
    /// reports back here. `None` once its answer has been taken. The task is
    /// held so dropping the finder drops the query with it.
    modified: Option<(UnboundedReceiver<Vec<PathBuf>>, Task<()>)>,
    /// Which generation of [`Self::modified_paths`], [`Self::buffer_paths`],
    /// and [`Self::dirty_buffer_paths`] the pick list shows.
    ///
    /// The lists are fixed between the moments they are replaced, so the
    /// picker only has to be told when one is, and otherwise keeps the rows it
    /// derived from them. Restamped wherever one is rebuilt.
    base_generation: u64,
    /// Absolute paths of currently-open buffers. Captured once at open time;
    /// not re-queried on scope toggle. The keys of [`Self::term_rows`] follow
    /// the buffer paths.
    pub(crate) buffer_paths: Vec<PathBuf>,
    /// Absolute paths of the open buffers with unsaved edits, the
    /// [`FinderScope::ModifiedBuffers`] base. Captured once at open time like
    /// [`Self::buffer_paths`].
    pub(crate) dirty_buffer_paths: Vec<PathBuf>,
    /// The terminal rows of the [`FinderScope::Buffers`] list, captured at
    /// open time like the buffer paths.
    pub(crate) term_rows: Vec<TermRow>,
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
    /// [`FinderScope::All`] base; Modified and the two buffer scopes feed their
    /// own vecs through [`PathPicker::refilter_with_base`]. A scope toggle
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
    /// Config-defined named scopes, compiled at open time in alphabetical
    /// (BTreeMap) order. Shift-Tab cycles through them after Modified.
    pub(crate) named_scopes: Vec<(String, GlobSet)>,
    /// Glob-filtered base for the active [`FinderScope::Named`] scope, or
    /// `None` before one is built.
    pub(crate) named_cache: Option<NamedCache>,
    /// Cells the modal would need to show the whole unfiltered list, which the
    /// renderer sizes the box against.
    ///
    /// Derived from the candidate base rather than the filtered rows, so
    /// narrowing a query never moves the box out from under the user typing it.
    /// The base grows while the background walk streams in, so a large workspace
    /// settles at its full size over the first frames after opening.
    pub(crate) content_size: (u16, u16),
}

/// Source tags for [`base_id`], so two scopes carrying the same generation
/// never name each other's list.
const MODIFIED_BASE: u8 = 0;
const BUFFERS_BASE: u8 = 1;
const NAMED_BASE: u8 = 2;
const DIRTY_BUFFERS_BASE: u8 = 3;

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
        mut buffer_paths: Vec<PathBuf>,
        dirty_buffer_paths: Vec<PathBuf>,
        term_rows: Vec<TermRow>,
        finder_scopes: &BTreeMap<String, Vec<String>>,
    ) -> Self {
        let input = InputView::create(
            ws,
            executor.clone(),
            SubmitTarget::FileFinder,
            "",
            "insert",
            1,
        );
        let mut core = PathPicker::new(ws, executor, git_root, walk);
        core.all_paths = seed_paths;
        core.walk_display = seed_display;
        buffer_paths.extend(term_rows.iter().map(|row| row.key.clone()));

        let mut finder = Self {
            input,
            open_intent,
            scope: initial_scope,
            modified_paths: Vec::new(),
            modified: Some(modified),
            buffer_paths,
            dirty_buffer_paths,
            term_rows,
            walk_epoch,
            base_generation: crate::picker::next_generation(),
            core,
            ignored: None,
            browse: None,
            named_scopes: compile_named_scopes(finder_scopes),
            named_cache: None,
            content_size: (FINDER_CONTENT_COLS, 0),
        };
        // Uniformly seed the initial (empty-query) list for whatever scope
        // opened, including a named scope's glob filter.
        finder.refilter_from_input(ws);
        finder
    }

    pub(crate) fn scope(&self) -> &FinderScope {
        &self.scope
    }

    /// The picker currently driving the list. Browse mode (a `/` or `~/`
    /// query) swaps in its own directory-walk picker; every other query drives
    /// the workspace `core`. Either gives way to its fallback of ignored files
    /// while [`Self::fallback_active`] holds.
    pub(crate) fn active_core(&mut self) -> &mut PathPicker {
        let fallback_active = self.fallback_active();
        match self.list_and_fallback_mut() {
            (_, Some(fallback)) if fallback_active => fallback,
            (list, _) => list,
        }
    }

    pub(crate) fn active_core_ref(&self) -> &PathPicker {
        match self.list_and_fallback() {
            (_, Some(fallback)) if self.fallback_active() => fallback,
            (list, _) => list,
        }
    }

    /// Whether the fallback of ignored files stands in for the list.
    ///
    /// Only browse and [`FinderScope::All`] fall back. A Modified or buffer list
    /// that holds nothing keeps its empty rows, and a fallback built under All
    /// stays through a flip away, so a return to All reuses it.
    pub(crate) fn fallback_active(&self) -> bool {
        if self.browse.is_none() && self.scope != FinderScope::All {
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
            None if self.scope == FinderScope::All => Some(self.core.git_root.clone()),
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

    /// The terminal the selected row names, or `None` for a file row.
    pub(crate) fn selected_term(&self) -> Option<TermId> {
        if self.scope != FinderScope::Buffers || self.browse.is_some() {
            return None;
        }
        let selected = self.core.selected_path()?;
        self.term_rows
            .iter()
            .find(|row| row.key == selected)
            .map(|row| row.term)
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        self.active_core().move_selection(delta);
    }

    /// Flip the scope and rerun the filter against the new base.
    ///
    /// Landing on [`FinderScope::Modified`] wants a fresh git status, but that
    /// query is the caller's to spawn via [`Self::set_modified_source`], since
    /// it belongs off this thread.
    ///
    /// The file scopes and the buffer scopes are separate cycles. The file
    /// scopes go All -> Modified -> each named scope -> AllWorkspaces -> All.
    /// [`FinderScope::Buffers`] and [`FinderScope::ModifiedBuffers`] flip with
    /// each other, so the buffer picker never enters a file scope. A finder
    /// reaches the buffer pair only through the dedicated `OpenBufferPicker`
    /// action.
    pub(crate) fn toggle_scope(&mut self) {
        self.scope = self.next_scope();
        self.core.picklist.selected = 0;
        // Force refilter + preview resync on next render against the new base.
        self.core.invalidate();
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
    /// Restamping the generation alone would not show the new list. The
    /// Modified base id keys on that generation, but
    /// [`PathPicker::refilter_with_base`] returns early while the query is
    /// unchanged and the filter still valid, which is exactly the state a
    /// finder sits in after opening. Invalidating is what that method exists
    /// for. Only the scope actually reading the list needs it, since every
    /// other scope re-runs from [`Self::toggle_scope`]'s own invalidate on the
    /// way in.
    fn pump_modified(&mut self) {
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
            self.core.invalidate();
        }
    }

    /// The scope Shift-Tab lands on next.
    ///
    /// The file scopes cycle All -> Modified -> each named scope (alphabetical)
    /// -> AllWorkspaces -> All. Buffers and ModifiedBuffers flip with each
    /// other.
    fn next_scope(&self) -> FinderScope {
        match &self.scope {
            FinderScope::All => FinderScope::Modified,
            FinderScope::Modified => self
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

    /// Re-run the matcher if the input text or scope has changed since last
    /// filter. Called from the renderer so typing picks up without a dedicated
    /// sync hook. Drains any pending walk result first so freshly arrived
    /// paths participate in the same render tick.
    ///
    /// The fallback of ignored files refilters with the list it stands in for,
    /// so its rows answer the query the moment the list runs dry. Each scan
    /// handed back names the picker it reports to.
    pub(crate) fn refilter_from_input(&mut self, ws: &Workspace) -> Vec<PendingScan> {
        // Ahead of the browse branch so a git-status answer landing while a
        // directory query is typed is taken rather than left in the channel.
        self.pump_modified();
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
            pending.extend(ScanTarget::List.tag(self.refilter_scope(&text)));
            (self.scope == FinderScope::All).then_some(text)
        };

        if let Some(query) = fallback_query {
            self.refilter_fallback(&query, &mut pending);
        }
        self.remeasure_content();
        pending
    }

    /// Refilter [`Self::core`] against the scope's base, handing back the scan
    /// of a scope that ranks on a worker.
    fn refilter_scope(&mut self, text: &str) -> Option<(u64, Scan)> {
        match self.scope.clone() {
            // The uncapped scopes are the whole repo walk, so their scan goes to
            // a worker and the caller spawns what this hands back.
            FinderScope::All | FinderScope::AllWorkspaces => self.core.begin_scan(text),
            FinderScope::Modified | FinderScope::Buffers | FinderScope::ModifiedBuffers => {
                self.refilter_listed(text);
                None
            },
            // A glob over the whole walk can keep most of it, so this scans
            // elsewhere like the uncapped scopes. A base small enough not to
            // need it costs one hop through the worker instead.
            FinderScope::Named(name) => {
                self.sync_named_cache(&name);
                // `named_cache` and `core` are disjoint fields, so the matcher
                // reads the cached base in place rather than cloning it.
                match &self.named_cache {
                    Some(cache) => {
                        let id = base_id(NAMED_BASE, cache.epoch, cache.filtered.len());
                        self.core.begin_scan_with_base(text, &cache.filtered, id)
                    },
                    None => None,
                }
            },
        }
    }

    /// Refilter [`Self::core`] on this thread against the modified files, the
    /// open buffers, or the modified buffers, whichever list the scope shows.
    fn refilter_listed(&mut self, query: &str) {
        // The tag stops two lists that share the finder's generation from
        // reading as each other. A scope flip invalidates too, but the tag does
        // not depend on that.
        let (base, id) = match &self.scope {
            FinderScope::Modified => (
                &self.modified_paths,
                base_id(
                    MODIFIED_BASE,
                    self.base_generation,
                    self.modified_paths.len(),
                ),
            ),
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
        self.core.refilter_with_base(query, base, id);
    }

    /// Bring the fallback of the list on display up to date with `query`, and
    /// hand back its scan.
    ///
    /// Runs while the fallback stands aside too, so its rows already answer the
    /// query when the list runs dry again.
    fn refilter_fallback(&mut self, query: &str, out: &mut Vec<PendingScan>) {
        let (_, Some(fallback)) = self.list_and_fallback_mut() else {
            return;
        };
        pump_capped_walk(fallback);
        fallback.pump_scan();
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
    /// A fallback of ignored files settles with its list, so an action reads the
    /// row on display whichever of the two shows.
    pub(crate) fn settle_scan(&mut self, query: &str) {
        // Browse filters the directory walk against its own partial rather than
        // the typed query, the leading directory having already been consumed
        // by the walk's root.
        if let Some(browse) = &mut self.browse {
            let partial = browse.partial.clone();
            browse.picker.settle_scan(&partial);
            if let Some(ignored) = &mut browse.ignored {
                ignored.settle_scan(&partial);
            }
            return;
        }
        match self.scope.clone() {
            FinderScope::All => {
                self.core.settle_scan(query);
                if let Some(ignored) = &mut self.ignored {
                    ignored.settle_scan(query);
                }
            },
            FinderScope::AllWorkspaces => self.core.settle_scan(query),
            FinderScope::Named(name) => {
                self.sync_named_cache(&name);
                if let Some(cache) = &self.named_cache {
                    let id = base_id(NAMED_BASE, cache.epoch, cache.filtered.len());
                    self.core.settle_scan_with_base(query, &cache.filtered, id);
                }
            },
            FinderScope::Modified | FinderScope::Buffers | FinderScope::ModifiedBuffers => {
                self.pump_modified();
                self.refilter_listed(query);
            },
        }
    }

    /// Re-derive [`Self::content_size`] from the active picker's candidate base.
    ///
    /// The base is the scope's whole list, not the filtered rows, so this lands
    /// the same answer for every query and only moves when the walk delivers or
    /// the scope flips. A list longer than [`u16::MAX`] just asks for the
    /// largest box there is.
    fn remeasure_content(&mut self) {
        let rows = u16::try_from(self.active_core_ref().picklist.base.len()).unwrap_or(u16::MAX);
        self.content_size = (FINDER_CONTENT_COLS, rows.saturating_add(FINDER_CHROME_ROWS));
    }

    /// Bring [`Self::named_cache`] up to date with the walk under `name`.
    ///
    /// Tests only the paths the cache has not seen, so a streaming walk costs
    /// each path once across every batch rather than the whole list per batch.
    /// A scope name with no compiled globset matches nothing, and still marks
    /// its paths seen so they are not retested.
    fn sync_named_cache(&mut self, name: &str) {
        let walked = self.core.all_paths.len();
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
        let git_root = &self.core.git_root;
        for path in &self.core.all_paths[cache.consumed..] {
            let display = paths::display_relative_with_home(path, git_root, home.as_deref());
            if globset.is_match(display) {
                cache.filtered.push(path.clone());
            }
        }
        cache.consumed = walked;
    }

    /// Sync the preview pane to the current selection. Clears the pane when
    /// nothing is selected.
    ///
    /// In [`FinderScope::Buffers`] and [`FinderScope::ModifiedBuffers`] the
    /// selection previews the live, possibly modified in-memory buffer. Every
    /// other scope reads the file from disk. A buffer selection whose path has
    /// no open buffer falls back to the disk file. A terminal row previews the
    /// session's screen.
    pub(crate) fn sync_preview(
        &mut self,
        ws: &mut Workspace,
        fs_host: &dyn FsHost,
        language_registry: &stoat_language::LanguageRegistry,
    ) {
        if let Some(term) = self.selected_term() {
            self.core.preview.sync(
                ws,
                fs_host,
                language_registry,
                PreviewSource::Terminal(term),
            );
            return;
        }
        let policy = if self.browse.is_some() {
            PreviewPolicy::File
        } else if matches!(
            self.scope,
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

/// The terminal rows of the [`FinderScope::Buffers`] list, one per shell
/// terminal of `ws`, oldest first.
///
/// A row reads `term <n>: <title>`, or `term <n>` when the child set no title.
/// The number keeps two terminals with one title apart.
pub(crate) fn term_rows(ws: &Workspace) -> Vec<TermRow> {
    ws.shell_terms()
        .into_iter()
        .enumerate()
        .map(|(i, term)| {
            let label = match ws.terms[term].term.title() {
                Some(title) => format!("term {}: {title}", i + 1),
                None => format!("term {}", i + 1),
            };
            TermRow {
                key: PathBuf::from(label),
                term,
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
    use crate::test_harness::keys;
    use crossterm::event::{Event, KeyCode};

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

    /// A terminal has no path, so the buffer list names it by its place among
    /// the workspace's shells and by the title its child set.
    #[test]
    fn the_buffer_picker_lists_terminals_after_the_buffers() {
        let mut h = crate::Stoat::test();
        let root = seed_finder_workspace(&mut h, &[("a.rs", "fn a() {}")]);
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("a.rs"),
            },
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        let first = focused_terminal(&h);
        h.stoat.active_workspace_mut().terms[first]
            .term
            .feed(b"\x1b]0;build\x07");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitNewRight);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);

        // A key chord goes to the shell, because a terminal pane sends its
        // keys to the child.
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenBufferPicker);

        let finder = h.stoat.file_finder.as_ref().expect("finder should be open");
        assert_eq!(
            finder.core.picklist.base.to_vec(),
            [
                root.join("a.rs"),
                PathBuf::from("term 1: build"),
                PathBuf::from("term 2"),
            ],
        );
    }

    #[test]
    fn enter_on_a_terminal_row_switches_to_the_tab_that_shows_it() {
        let mut h = crate::Stoat::test();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        let term_id = focused_terminal(&h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenBufferPicker);

        h.type_text("term");
        h.type_keys("enter");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.stoat.file_finder.is_none(),
                ws.active_tab,
                focused_terminal(&h)
            ),
            (true, 0, term_id),
        );
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

    /// A terminal row previews the screen as plain text, since program output
    /// has no language to highlight it with.
    #[test]
    fn a_terminal_row_previews_the_screen() {
        let mut h = crate::Stoat::test();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        let term_id = focused_terminal(&h);
        h.stoat.active_workspace_mut().terms[term_id]
            .term
            .feed(b"hello");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenBufferPicker);

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
            ("hello".to_string(), None),
        );
    }

    /// The terminal session the focused pane shows.
    fn focused_terminal(h: &TestHarness) -> TermId {
        let ws = h.stoat.active_workspace();
        let crate::pane::View::Terminal(term_id) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the focused pane shows a terminal");
        };
        term_id
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

    /// A terminal carries no dirty flag, so the modified-buffers scope keeps
    /// the edited buffer and leaves out the shell that the buffer list shows.
    #[test]
    fn the_modified_buffers_scope_lists_no_terminal() {
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
            (
                vec![root.join("a.rs"), PathBuf::from("term 1")],
                vec![root.join("a.rs")],
            ),
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
            &[("src/main.rs", ""), ("target/debug/main.toml", "")],
        );

        h.type_keys("space p");
        h.type_text("main.toml");
        settle_fallback(&mut h);
        assert_eq!(finder_rows(&h), ["target/debug/main.toml"]);

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

    #[test]
    fn closing_with_a_fallback_leaves_the_registry_at_its_pre_open_size() {
        let mut h = crate::Stoat::test();
        seed_finder_workspace(
            &mut h,
            &[("src/main.rs", ""), ("target/debug/main.toml", "")],
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
            content.contains("(browse)"),
            "browse title missing:\n{content}"
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
}
