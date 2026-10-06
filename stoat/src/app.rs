use crate::{
    action_handlers,
    agent_ipc::{AgentControl, AgentEvent},
    agent_status::AgentStatus,
    apc_emit,
    badge::BadgeTray,
    buffer::BufferId,
    code_index::{
        build::IndexUpdate,
        store::{IndexWrites, ManifestEdit},
    },
    command_palette::CommandPalette,
    debounce,
    display_map::syntax_theme::SyntaxStyles,
    editor_state::{EditorId, ScrollGlide},
    emoji_expand,
    file_finder::{FileFinder, FinderPathCache},
    git_jobs::{self, GitJobs},
    help::Help,
    host::{
        ClipboardKind, EnvHost, FsEventKind, FsHost, FsWatchHost, GitHost, LocalEnv, LocalFs,
        LocalGit, LspHost, NoopFsWatcher,
    },
    keymap::{self, Keymap, ResolvedAction, SideButton, StateValue},
    keymap_state::{
        self, active_modal, debug_assert_modal_exclusivity, modal_predicate, normalize_shift_event,
        resolve_action, ActiveModal, StoatKeymapState,
    },
    lsp::pending::{Pending, StampedPending},
    minimap::emit::{self},
    mouse::{self, mouse_event_kind},
    pane::{DockId, DockVisibility, FocusTarget, NodeId, PaneId, PaneTree, Placement, View},
    picker::DisplayCache,
    quit_all_confirm::QuitAllConfirm,
    rebase::RebasePause,
    register,
    render::{
        pane_cache::PaneCacheEntry,
        sanitize,
        undercurl::{self, UndercurlBatch},
    },
    run::{CommandMark, PtyNotification, RunId},
    selection::merge_overlapping_spans,
    session_log::SessionLog,
    ssh,
    symbol_finder::SymbolFinder,
    term_session::{TermId, TermLocation},
    theme_pool::{ThemePool, VscodeSource},
    ui::RenderFrame,
    workspace::{BridgeWaiter, Workspace, WorkspaceId, WorkspaceUid},
    workspace_picker::WorkspacePicker,
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use futures::FutureExt;
use ratatui::{buffer::Buffer, layout::Rect};
use slotmap::SlotMap;
use std::{
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};
use stoat_action::{Conflict, Diff, OpenFile};
use stoat_config::{MinimapMode, Settings, TabBarMode, WrapMode};
use stoat_language::{self as language, LanguageRegistry};
use stoat_scheduler::Executor;
use stoat_text::{
    auto_pairs::{self, AutoPairs, PairAction},
    Anchor, Bias, IndentStyle, Rope, Selection, SelectionGoal,
};
use stoat_widgets::{pool::SmoothScrollState, ApcScene};
use stoatty_protocol::window_ipc::{MouseButton as IpcMouseButton, MouseKind, WindowIpcEvent};
use tokio::{
    io::AsyncBufReadExt,
    sync::{
        mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender},
        watch,
    },
};

pub(crate) const DEFAULT_KEYMAP: &str = include_str!("../../config.stcfg");

/// The default stoatty config, embedded so `:open-config stoatty` can seed a
/// missing one with the same file the terminal ships.
pub(crate) const DEFAULT_STOATTY_CONFIG: &str = include_str!("../../stoatty.toml");

/// [`DEFAULT_KEYMAP`] parsed, once for the process.
static EMBEDDED_CONFIG: OnceLock<stoat_config::Config> = OnceLock::new();

/// The embedded defaults, parsed on the first call and shared from there on.
///
/// Everything the defaults feed reads this one copy: the mouse capture policy a
/// launch resolves before the UI thread starts, the settings, keymap and theme
/// pool the editor compiles from them, and the rebuild a config reload runs.
/// The source is 55 KB, and parsing it is milliseconds a launch used to pay
/// twice before its first frame.
///
/// A parse failure is a defect in the file this binary was built from rather
/// than a runtime condition, so it takes the whole process down with the errors
/// logged rather than each caller inventing a fallback.
pub(crate) fn embedded_config() -> &'static stoat_config::Config {
    EMBEDDED_CONFIG.get_or_init(|| {
        let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
        if !errors.is_empty() {
            tracing::error!(
                "default keymap parse errors: {}",
                stoat_config::format_errors(DEFAULT_KEYMAP, &errors)
            );
        }
        config.expect("the embedded config parses")
    })
}

/// Frame interval for scroll-animation ticks, about 60 fps to match a typical
/// display rather than shipping targets that can never be presented.
/// [`Stoat::run`] arms a timer at this cadence while a scroll glide is active,
/// advancing the inertial scroll one step per fire.
const SCROLL_FRAME: std::time::Duration = std::time::Duration::from_millis(16);

/// Terminal output bytes one run-loop turn parses before it returns to
/// `select!`, about six milliseconds of parse.
///
/// The VT parse runs on the app thread, which is the runtime's only thread, so
/// a command flooding its pty starves the frame timer and the key reader for as
/// long as the flood lasts. The reader relays 64 KiB at a time into a 256-slot
/// channel, which is 16 MiB of parse in one turn without a bound.
///
/// Nothing queued is lost. What is left wakes the `pty_rx.recv()` arm on the
/// next turn, and the frame timer paints between the two.
///
/// This bound alone does not buy that frame. The pty arm sits last in a biased
/// select, behind the timer and behind every other channel, and the turn ends
/// on a yield, without which a channel that refills never hands the runtime
/// back for the timer to come due.
/// Alacritty bounds the same loop the same way, releasing the terminal after a
/// fixed read so the renderer gets a turn.
const PTY_TURN_BUDGET_BYTES: usize = 1 << 20;

/// Frame interval for the LSP work-done spinner popout, about 10 fps. Fast enough
/// to read as motion, slow enough not to churn repaints while progress streams.
const SPINNER_FRAME_SECS: f32 = 0.1;

/// Braille glyphs cycled to animate an in-flight LSP work-done spinner, one per
/// [`SPINNER_FRAME_SECS`] window.
pub(crate) const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Everything a parsed config resolves into, rebuilt wholesale at startup and
/// again whenever the config is reloaded.
///
/// Grouping them keeps the two paths honest with each other. A field added here
/// is one the reload must swap, which the destructuring at both call sites
/// forces the author to confront.
struct ConfigArtifacts {
    keymap: Keymap,
    /// The action names `keymap` binds that no registered action answers to,
    /// sorted. Every one of them is a key that does nothing when pressed.
    unknown_actions: Vec<String>,
    settings: Settings,
    theme: crate::theme::Theme,
    theme_pool: ThemePool,
    syntax_styles: SyntaxStyles,
    minimap_class_table: crate::minimap::ClassTable,
}

/// Resolve `embedded` and the optional `user` config into the artifacts the
/// editor runs on.
///
/// `user` layers over `embedded` rather than replacing it. A user config states
/// the keys, settings, and themes that user cares about, so everything it never
/// mentions keeps working from the shipped config. That is what lets a config
/// written against an older release stay usable.
///
/// `imported` are the VSCode themes, slotted between the embedded and user
/// blocks so a user config's own `theme` block still wins. They are carried as
/// unconverted sources, so only the theme resolved here is paid for.
///
/// `env_theme` names the theme inherited from the environment. It applies only
/// when neither `cli_settings` nor a user config picks one, and only when the
/// theme pool can resolve it. An inherited name is a hint the user never typed,
/// so one naming no known theme is ignored with a warning and the default look
/// survives. A reload passes [`None`], since the environment is read once at
/// startup.
fn build_config_artifacts(
    user: Option<stoat_config::Config>,
    embedded: &stoat_config::Config,
    imported: &[Arc<VscodeSource>],
    cli_settings: Settings,
    env_theme: Option<String>,
) -> ConfigArtifacts {
    // The whole pool is retained so SetTheme can re-resolve any theme at
    // runtime, and so an `inherits PARENT` sees every candidate parent.
    let theme_pool = {
        let mut pool = ThemePool::default();
        for block in &embedded.themes {
            pool.push_parsed(block.clone());
        }
        for source in imported {
            pool.push_vscode(source.clone());
        }
        if let Some(c) = user.as_ref() {
            for block in &c.themes {
                pool.push_parsed(block.clone());
            }
        }
        pool
    };

    let settings = {
        let cli_theme_set = cli_settings.theme.is_some();
        let from_user = user.as_ref().map(Settings::from_config);
        // The embedded default sets `theme = default_dark` unconditionally,
        // which an inherited theme is meant to beat. Only a theme the user's
        // own config names counts as the explicit choice that outranks it.
        let user_theme_set = from_user.as_ref().is_some_and(|s| s.theme.is_some());

        let mut settings = Settings::from_config(embedded)
            .merge(from_user.unwrap_or_default())
            .merge(cli_settings);
        if !cli_theme_set
            && !user_theme_set
            && let Some(name) = env_theme
        {
            if theme_pool.contains(&name) {
                settings.theme = Some(name);
            } else {
                tracing::warn!("STOAT_THEME '{name}' names no known theme; using the default");
            }
        }

        settings
    };

    let theme = {
        let name = settings.theme.as_deref().unwrap_or("default_dark");
        if theme_pool.is_empty() {
            crate::theme::Theme::empty()
        } else {
            theme_pool.resolve(name).unwrap_or_else(|e| {
                tracing::error!("theme '{name}' load failed: {e}");
                crate::theme::Theme::empty()
            })
        }
    };

    let (keymap, unknown_actions) = {
        let (default_keymap, warnings) = Keymap::compile_with_warnings(embedded);
        for warning in warnings {
            tracing::warn!(target: "stoat::keymap", "{warning}");
        }

        match user {
            None => (default_keymap, Vec::new()),
            Some(user) => {
                let (user_keymap, warnings) = Keymap::compile_with_warnings(&user);
                for warning in warnings {
                    tracing::warn!(target: "stoat::keymap", "{warning}");
                }

                // Counted before layering, because layering is what drops these
                // bindings. The layered keymap reports none of them.
                let unknown_actions = user_keymap.unknown_actions();
                for name in &unknown_actions {
                    tracing::warn!(
                        target: "stoat::keymap",
                        "config binds `{name}`, which is not a registered action",
                    );
                }

                (
                    Keymap::layered(user_keymap, default_keymap),
                    unknown_actions,
                )
            },
        }
    };

    let syntax_styles = SyntaxStyles::from_theme(&theme);
    let minimap_class_table = crate::minimap::ClassTable::from_theme(&theme);

    ConfigArtifacts {
        keymap,
        unknown_actions,
        settings,
        theme,
        theme_pool,
        syntax_styles,
        minimap_class_table,
    }
}

/// Point every registered language's highlight map at `styles`' theme keys.
///
/// Must run again after a theme swap, since the keys a capture name maps onto
/// are derived from the active theme.
pub(crate) fn install_highlight_maps(registry: &LanguageRegistry, styles: &SyntaxStyles) {
    let theme_keys = styles.theme_keys();
    for lang in registry.languages() {
        let map = stoat_language::HighlightMap::new(lang.highlight_capture_names(), theme_keys);
        lang.set_highlight_map(map);
    }
}

/// Register one non-recursive watch per directory of the workspace at `root`.
///
/// One recursive watch on the root instead covers `target/`, `node_modules/`,
/// and `.git/`, which the file walker excludes and nothing else in the editor
/// reads. On a repo that has been built, those trees hold most of the
/// directories, enough to exhaust the platform's watch limit and so leave the
/// workspace unwatched entirely.
///
/// The three `.git` directories are added back on purpose. `HEAD`, `index`,
/// `packed-refs`, and branch-tip writes all land in them, and those are what
/// stale every diff base. Deeper `.git` paths going unwatched is the accepted
/// tradeoff for not walking an object store.
///
/// Reads the tree, so it belongs on the blocking pool rather than the run loop.
/// Failures are counted rather than reported one by one, since a watch limit
/// reached partway through fails for every remaining directory.
fn watch_workspace_dirs(fs: &dyn FsHost, watcher: &dyn FsWatchHost, root: &Path) {
    let git_dir = root.join(".git");
    let dirs = fs.walk_workspace_dirs(root).into_iter().chain([
        git_dir.clone(),
        git_dir.join("refs"),
        git_dir.join("refs").join("heads"),
    ]);

    let (mut watched, mut failed) = (0usize, 0usize);
    for dir in dirs {
        match watcher.watch(&dir) {
            Ok(_) => watched += 1,
            Err(_) => failed += 1,
        }
    }

    if failed > 0 {
        tracing::warn!(
            target: "stoat::app",
            watched,
            failed,
            root = %root.display(),
            "some workspace directories could not be watched; external edits under them go untracked",
        );
    }
}

/// The ancestor of `root` that is the repository's working tree root, spelled
/// as `root` spells it.
///
/// The watcher names an event by the watched directory joined with the entry
/// name, and the finder and the index match events against the workspace root
/// by prefix. Through a symlink, git reads the root back in another spelling,
/// and no event under that spelling matches. `root` itself answers when no
/// ancestor equals `workdir`.
fn repo_tree_root(root: &Path, workdir: Option<&Path>) -> PathBuf {
    root.ancestors()
        .find(|ancestor| Some(*ancestor) == workdir)
        .unwrap_or(root)
        .to_path_buf()
}

/// Whether an index drain that has merged `drained` updates in `elapsed` has
/// used up its turn.
///
/// Either bound ends it. The count guards against a flood of updates too
/// cheap for the clock to notice, and the time against the handful that are
/// expensive enough that a count says nothing about what they cost.
fn index_turn_spent(drained: usize, elapsed: std::time::Duration) -> bool {
    drained >= INDEX_DRAIN_CAP || elapsed >= INDEX_DRAIN_BUDGET
}

/// Index into [`SPINNER_FRAMES`] for a spinner that has animated for `clock`
/// seconds, wrapping once per full cycle.
pub(crate) fn spinner_phase(clock: f32) -> u8 {
    ((clock / SPINNER_FRAME_SECS) as u64 % SPINNER_FRAMES.len() as u64) as u8
}

/// Upper bound on one scroll-animation step's `dt`. A render that runs long, or
/// a glide resumed after an idle gap, advances by at most this much rather than
/// a single large jump.
const MAX_FRAME_DT: f32 = 0.1;

/// Poll cadence for auto-reloading buffers. While any buffer is flagged, a
/// timer at this interval wakes [`Stoat::drive_background`] so
/// [`crate::auto_reload::pump_auto_reload`] can re-read files whose
/// on-disk mtime advanced.
pub(crate) const AUTO_RELOAD_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a transient status message stays visible before it self-retires.
/// [`Stoat::set_status`] stamps a deadline this far ahead and arms a timer that
/// wakes the run loop so [`crate::render::frame`] can clear the expired message.
const STATUS_MESSAGE_TTL: std::time::Duration = std::time::Duration::from_secs(4);

/// Shortest gap between two wheel notches that both run a keymap binding.
///
/// A trackpad flick emits a stream of per-line reports far faster than this,
/// so one flick walks a handful of jumps rather than a hundred. A deliberate
/// notch cadence runs near 150ms, so every intended gesture still lands.
pub(crate) const WHEEL_BINDING_COOLDOWN: std::time::Duration = std::time::Duration::from_millis(80);

/// Wheel travel, in lines, that must accrue at an open jump line before a
/// diff-view notch walks to the next change.
///
/// One plain-wheel notch is one line, so a trackpad's fractional reports add up
/// to a deliberate notch, and a single pixel of travel never walks.
pub(crate) const DIFF_WHEEL_JUMP_TRAVEL: f32 = 1.0;

/// Shortest gap between two diff-view wheel walks.
///
/// A free-spinning wheel delivers a burst of notches. Dropping the walks inside
/// this gap keeps one flick from walking a hundred changes, so a spin walks at
/// most four a second.
pub(crate) const DIFF_WHEEL_COOLDOWN: std::time::Duration = std::time::Duration::from_millis(250);

/// Maximum index updates [`Stoat::drain_index_updates`] processes in one call.
/// Bounds the graph work per event-loop turn so a large reindex burst cannot
/// stall input. On hitting the cap the drain reschedules itself to finish the
/// remainder on the next turn.
const INDEX_DRAIN_CAP: usize = 512;

/// Time [`Stoat::drain_index_updates`] spends merging before it returns to the
/// event loop, about half a frame.
///
/// The count alone is no bound on cost. One update is 95 us for a cold shard
/// and tens of milliseconds for a reindex of a large file, and the update
/// channel is unbounded, so a cold build queues every shard at once and a cap
/// of five hundred never engages. One turn then merges the whole build.
///
/// Nothing queued is lost. What is left wakes the next turn through the redraw
/// notify, and the frame timer paints between the two, which is how the pty
/// drain in the same function bounds itself.
const INDEX_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(8);

/// Hidden buffers that keep their full highlight state when `editor.highlight_retention`
/// is unset. Beyond this many, the least-recently-shown hidden buffers are evicted.
const DEFAULT_HIGHLIGHT_RETENTION: u32 = 64;

/// One [`Stoat::drain_index_updates`] pass slower than this warns, naming the
/// drained update count. A drain this slow blocks the event loop, the mechanism
/// behind an index-driven wedge.
const SLOW_DRAIN_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateEffect {
    Redraw,
    Quit,
    None,
}

/// A focusable panel resolved by hit-testing a point, carrying the concrete pane
/// id under the cursor.
///
/// Distinct from [`FocusTarget`], whose `SplitPane` is a unit variant: a hit
/// names the specific pane at the point, which is not necessarily the focused
/// one, so [`Stoat::target_at`] must return the id even though `ws.focus` no
/// longer stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PanelHit {
    Pane(PaneId),
    Dock(DockId),
}

impl UpdateEffect {
    /// Combine two effects, keeping the more urgent outcome.
    ///
    /// A coalesced batch applies several messages in one loop iteration and
    /// must act on the strongest result. Quit outranks Redraw, which outranks
    /// None. The result does not depend on argument order.
    fn merge(self, other: UpdateEffect) -> UpdateEffect {
        match (self, other) {
            (UpdateEffect::Quit, _) | (_, UpdateEffect::Quit) => UpdateEffect::Quit,
            (UpdateEffect::Redraw, _) | (_, UpdateEffect::Redraw) => UpdateEffect::Redraw,
            _ => UpdateEffect::None,
        }
    }
}

/// Shared landing queue for detached LSP spawn tasks, one entry per server.
/// See [`Stoat::pending_lsp_host`].
type PendingLspHost = Arc<std::sync::Mutex<Vec<PendingSpawn>>>;

/// A language server whose spawn task finished, waiting for [`Stoat::update`]
/// to install it.
///
/// Carries the resolved `server` command name and the `scope` it was spawned
/// for, so the registry keys the ready host and installs it into the right
/// routing list. `result` is the ready host, or the failure string to surface
/// in the message row when the spawn or handshake failed.
pub(crate) struct PendingSpawn {
    pub(crate) server: String,
    pub(crate) scope: SpawnScope,
    pub(crate) result: Result<Arc<dyn LspHost>, String>,
}

/// Which routing list a spawning server belongs to.
///
/// A server is spawned either because a buffer's language calls for it or
/// because it serves every buffer. The install has to know which, since the two
/// land in separate registry lists and reopen different sets of documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SpawnScope {
    Language(String),
    Global,
}

/// A finished off-thread `--continue` restore, produced by the blocking task and
/// drained by [`Stoat::install_pending_workspace_restore`].
///
/// `outcome` carries the replayed buffer registry and the remaining workspace
/// state, or the read/parse error. `path` is retained for the log line, and
/// `workspace` identifies the restore target so the install can confirm it is
/// still fresh before clobbering it.
pub(crate) struct PendingWorkspaceRestore {
    workspace: WorkspaceId,
    path: PathBuf,
    outcome: io::Result<(
        crate::buffer_registry::BufferRegistry,
        crate::workspace::persist::WorkspaceStateV1,
    )>,
}

/// A message from the window-event socket reader task to the main loop.
///
/// The socket connection lives on a background task. Connection state and each
/// decoded [`WindowIpcEvent`] cross to the main thread as one of these, so the
/// flag and pane routing only ever mutate on the loop.
enum WindowIpc {
    Connected,
    Disconnected,
    Event(WindowIpcEvent),
}

/// A modal that sizes itself to its content, and so carries its own zoom level.
///
/// The zoom combo is context-relative, so a step has to land on whichever modal
/// the user is looking at rather than on a single global level. This names that
/// target. The workspace picker sizes entirely to its content and has nothing
/// to zoom, so it is absent.
///
/// Every kind here sizes its box against its own [`Stoat::modal_zoom`] entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ModalKind {
    FileFinder,
    CodeSearch,
    CommitPicker,
    Palette,
    Help,
    SymbolFinder,
    LocationPicker,
    DiagnosticsPicker,
    JumplistPicker,
}

/// Rows one wheel notch moves the commit picker's diff preview.
///
/// More than the single row a notch moves the list, because a diff is read by
/// the screenful while a commit list is walked one commit at a time.
pub(crate) const PREVIEW_WHEEL_ROWS: usize = 3;

/// Rows a dragged separator leaves the pane below it.
///
/// A floor on the gesture rather than on the layout, which is free to render a
/// shorter preview when the modal itself is short. Dragging the separator to the
/// modal's bottom edge should leave a diff still worth looking at instead of a
/// sliver.
pub(crate) const MIN_PREVIEW_ROWS: u16 = 3;

/// Which way a modal's list/preview separator runs, and so which pointer
/// coordinate a drag along it reads.
#[derive(Copy, Clone)]
pub(crate) enum SeparatorAxis {
    /// A row between a list above and a preview below, as the commit picker
    /// stacks them. A drag reads the pointer's row.
    Rows,
    /// A column between a list and a preview beside it, as the finder family
    /// splits them. A drag reads the pointer's column.
    Columns,
}

impl SeparatorAxis {
    /// The pointer coordinate a drag along this axis moves.
    fn along(self, mouse: &MouseEvent) -> u16 {
        match self {
            Self::Rows => mouse.row,
            Self::Columns => mouse.column,
        }
    }

    /// The pointer coordinate the separator runs across, which a hit-test bounds
    /// so a press level with the separator but off the modal never arms.
    fn across(self, mouse: &MouseEvent) -> u16 {
        match self {
            Self::Rows => mouse.column,
            Self::Columns => mouse.row,
        }
    }
}

/// A list/preview separator: where it sits, and what a drag along it
/// redistributes.
///
/// Every split surface in the app has the same shape. The pane on one side
/// takes a share of the body, the separator takes a cell of its own, and the
/// other pane takes the rest. Only the axis and the floors differ, so
/// resolving a surface into this descriptor lets one hit-test and one clamp
/// serve them all.
///
/// The descriptor carries geometry alone. Where a drag's resulting share is
/// stored is the caller's business, which is what lets a non-modal surface
/// reuse it.
pub(crate) struct SplitSeparator {
    pub(crate) axis: SeparatorAxis,
    /// The separator's own line, in the axis's units.
    pub(crate) line: u16,
    /// How far the separator runs across the other axis.
    pub(crate) span: Range<u16>,
    /// The extent a drag redistributes, in the axis's units.
    pub(crate) body: Range<u16>,
    /// What the list keeps however far the separator is dragged toward it.
    pub(crate) min_list: u16,
    /// What the preview keeps, the counterpart floor on the other side.
    pub(crate) min_preview: u16,
}

impl SplitSeparator {
    /// Whether `mouse` presses the separator itself rather than a pane beside it.
    pub(crate) fn hit(&self, mouse: &MouseEvent) -> bool {
        self.axis.along(mouse) == self.line && self.span.contains(&self.axis.across(mouse))
    }

    /// The list's share of the body once the separator lands at `mouse`, or
    /// `None` when the body cannot host both floors and so cannot be split.
    ///
    /// Clamping happens in the axis's own units rather than in percent, because a
    /// percent that round-trips through the layout's truncating division lands
    /// the separator a cell short of the pointer. The extent is clamped to leave
    /// both panes their floor, and the percent is rounded up so the layout
    /// recovers exactly that extent.
    pub(crate) fn share_at(&self, mouse: &MouseEvent) -> Option<u16> {
        let extent = self.body.end.saturating_sub(self.body.start);
        let widest_list = extent.saturating_sub(self.min_preview + 1);
        if widest_list < self.min_list {
            return None;
        }

        let list = self
            .axis
            .along(mouse)
            .saturating_sub(self.body.start)
            .clamp(self.min_list, widest_list);

        Some((list * 100).div_ceil(extent))
    }
}

/// Furthest a modal may be zoomed out, in steps of a tenth of the screen.
///
/// Shallower than the grow limit because a modal shrinks toward a minimum size
/// it reaches quickly, while growing has most of the screen to cross.
pub(crate) const MODAL_ZOOM_MIN: i8 = -4;

/// Furthest a modal may be zoomed in, in steps of a tenth of the screen. Enough
/// to reach the screen edge from any starting size.
pub(crate) const MODAL_ZOOM_MAX: i8 = 8;

/// Zoom steps the user has applied to `kind`, zero for a kind they never zoomed.
///
/// Free rather than a [`Stoat`] method because the render dispatch reads this
/// while already holding a mutable borrow of the open modal. The two are
/// disjoint fields, which the borrow checker only sees through direct field
/// access.
pub(crate) fn modal_zoom_steps(
    zooms: &std::collections::BTreeMap<ModalKind, i8>,
    kind: ModalKind,
) -> i8 {
    zooms.get(&kind).copied().unwrap_or(0)
}

/// Share of its body `kind`'s list pane takes, as a percentage.
///
/// A kind whose separator the user never dragged reads the layout family's own
/// default. Free rather than a [`Stoat`] method for the same reason
/// [`modal_zoom_steps`] is.
pub(crate) fn modal_split_percent(
    splits: &std::collections::BTreeMap<ModalKind, u16>,
    kind: ModalKind,
) -> u16 {
    splits
        .get(&kind)
        .copied()
        .unwrap_or(crate::render::picker::DEFAULT_LIST_PERCENT)
}

/// One key press's keymap lookup, derived on demand.
///
/// The outer `Option` is whether the lookup has run, the inner one its answer,
/// so a press that matched no binding is still only looked up once.
///
/// A press mutates the app as it falls through the readers, and every one of
/// those mutations is outside what a keymap predicate reads, which is what lets
/// the derivation happen at whichever reader gets there first rather than up
/// front. See [`Stoat::handle_key`], where that is spelled out against the
/// readers themselves.
#[derive(Default)]
struct KeymapLookup(Option<Option<BoundActions>>);

/// The actions a key press's binding names, with the digit a counted binding
/// captured out of the key itself.
type BoundActions = (Arc<[ResolvedAction]>, Option<f64>);

/// One cursor's part of an edit that differs per cursor.
struct CursorEdit {
    /// The selection this edit belongs to, which is how it finds its cursor
    /// again once the whole batch has moved every offset.
    id: usize,
    /// The bytes `text` replaces. Empty for an ordinary insertion, and wider
    /// for one that takes something out on its way in.
    range: Range<usize>,
    text: String,
    /// Bytes past `range.start` the cursor lands on once the edit is written.
    /// The text's own length for an ordinary insertion, shorter for a cursor
    /// landing inside what it wrote, longer for one stepping over text it left
    /// alone.
    caret: usize,
}

pub struct Stoat {
    pub(crate) size: Rect,
    /// Fallback mode store, read and written only when the focused target has
    /// no mode of its own -- no focused editor, run, or terminal pane, and no
    /// open input modal. The live mode for those targets lives on the target
    /// itself; [`Self::focused_mode`] resolves which store applies.
    fallback_mode: String,
    /// What [`Self::focused_mode`] answered when [`Self::refresh_frame_mode`]
    /// last ran, held so a frame can read the mode as a field.
    ///
    /// [`Self::focused_mode`] borrows the whole app, which a paint holding the
    /// active workspace mutably cannot do. Rewritten only when the mode string
    /// actually differs, so the steady frame reads it without copying it.
    pub(crate) frame_mode: String,
    /// Config-defined session variables set by `SetVar`. Session-local and never
    /// persisted. The keymap reads them after its built-in predicate fields.
    pub(crate) user_vars: std::collections::HashMap<String, StateValue>,
    pub executor: Executor,
    pub(crate) keymap: Keymap,
    pub settings: Settings,
    /// The CLI and environment overrides that layered over the config at
    /// startup, retained so a mid-session config reload can re-apply them. A
    /// flag passed on the command line outranks the file both times.
    pub(crate) cli_settings: Settings,
    pub theme: Arc<crate::theme::Theme>,
    /// Every theme the session can switch to, the active one among them,
    /// retained so [`ActionKind::SetTheme`] can re-resolve a different theme at
    /// runtime without reparsing the config.
    pub(crate) theme_pool: ThemePool,
    /// The VSCode themes read at startup, both built-in and from the user's
    /// theme directory, held as their unconverted JSON. Retained so a config
    /// reload rebuilds the same pool without re-reading the theme files, and so
    /// a theme converted before the reload stays converted after it.
    pub(crate) imported_themes: Vec<Arc<VscodeSource>>,
    /// How far the user has zoomed each modal past the size its content asks
    /// for, in steps of a tenth of the screen. An absent kind sits at zero.
    ///
    /// Steps live here rather than on the modal state itself so a level outlives
    /// the modal it belongs to. Reopening the file finder brings back the size
    /// the user last chose for it. They are session-scoped and deliberately not
    /// persisted, because a zoom is a reaction to what is on screen right now.
    /// Every entry sits within [`MODAL_ZOOM_MIN`]`..=`[`MODAL_ZOOM_MAX`], and
    /// usually within the narrower band that moves the box at the terminal's
    /// current size, which [`Self::handle_zoom_step`] enforces as the only
    /// writer. An entry can still sit outside that band after the terminal
    /// shrinks, since nothing rewrites the ledger on resize.
    pub(crate) modal_zoom: std::collections::BTreeMap<ModalKind, i8>,
    /// Steps the user has applied to how far the diff view recedes unchanged
    /// syntax behind changed content, scaling the shipped fractions per
    /// [`crate::render::review::diff_soften_scale`].
    ///
    /// A higher level recedes further, which is less unchanged code on screen,
    /// so the shrink half of the zoom combo raises it and the grow half lowers
    /// it. [`Self::handle_zoom_step`] is the only writer.
    ///
    /// Session-scoped and deliberately not persisted, for [`Self::modal_zoom`]'s
    /// reason: the level answers what is on screen right now. One level for the
    /// whole session rather than one per editor, because how hard to recede is
    /// a reading preference rather than a property of any one file.
    pub(crate) diff_soften: i8,
    /// How far the diff view shifts a changed row toward its status color, as
    /// a level [`crate::render::review::diff_tint_amount`] turns into a
    /// fraction. Level 0 is off, and [`stoat_action::DiffTintDown`] and
    /// [`stoat_action::DiffTintUp`] step it.
    ///
    /// The whole row moves, not only the chars the refinement matched, so an
    /// unchanged token on an added line reads added rather than keeping the
    /// syntax color the theme gave it.
    ///
    /// The same level drains the color out of the unchanged rows around it, so
    /// raising the dial concentrates color on the change rather than adding
    /// more of it to the screen.
    ///
    /// Color is the cue the soften leaves free. The soften says where a change
    /// is, and this says what kind it is, so added, deleted, modified, and
    /// moved read apart without the gutter.
    ///
    /// Session-scoped and deliberately not persisted, for [`Self::diff_soften`]'s
    /// reason: the level answers what is on screen right now.
    pub(crate) diff_tint: i8,
    /// Whether the diff view paints syntax color, in both of its columns.
    ///
    /// The diff view marks a change by contrast: the chars around it recede and
    /// a prose change bolds. Syntax color competes with that, since a row
    /// carries several colors before the diff says anything. Turning it off
    /// leaves the soften and the bold as the only cues on screen.
    ///
    /// Scoped to the diff view alone. [`stoat_action::ToggleSyntaxHighlight`]
    /// keeps its own session-wide meaning, and this cannot color a plain pane
    /// that toggle left plain.
    ///
    /// Session-scoped and deliberately not persisted, for [`Self::diff_soften`]'s
    /// reason: it answers what is on screen right now.
    pub(crate) diff_syntax: bool,
    /// Whether the diff view bolds every change span, in both of its columns.
    ///
    /// Off, only a prose replacement bolds, because there the receding alone
    /// does not show which char changed. On, every span of any kind bolds on
    /// any theme, so weight marks each change as well as the receding and the
    /// tint do.
    ///
    /// [`stoat_action::DiffBold`] is the only writer, and it answers only on a
    /// diff surface.
    ///
    /// Session-scoped and deliberately not persisted, for the same reason as
    /// [`Self::diff_soften`]. It answers what is on screen right now.
    pub(crate) diff_bold: bool,
    /// Whether the diff view underlines every change span, in both of its
    /// columns.
    ///
    /// Off, a span underlines only on a theme that does not blend, since such a
    /// theme has no receding to lead against, and where it marks a changed part
    /// of unstructured text, such as a string or a comment. On, every theme
    /// underlines every span, so a change stays marked when its color does not
    /// stand out.
    ///
    /// [`stoat_action::DiffUnderline`] is the only writer, and it answers only
    /// on a diff surface.
    ///
    /// Session-scoped and deliberately not persisted, for the same reason as
    /// [`Self::diff_soften`]. It answers what is on screen right now.
    pub(crate) diff_underline: bool,
    /// Share of its body each modal's list pane takes, as a percentage, for the
    /// kinds whose list/preview separator the user has dragged. An absent kind
    /// sits at [`crate::render::picker::DEFAULT_LIST_PERCENT`].
    ///
    /// Stored per kind and session-scoped for [`Self::modal_zoom`]'s reasons. A
    /// share the user chose outlives the modal it was chosen in, so reopening
    /// that modal restores the split. It is never persisted, because the choice
    /// answers what is on screen right now.
    pub(crate) modal_split: std::collections::BTreeMap<ModalKind, u16>,
    /// Share of its body the commits screen's list pane takes, as a
    /// percentage, once the reader has dragged the list/detail separator.
    ///
    /// `None` until the first drag, which leaves the width to the renderer's
    /// own formula. Session-scoped for [`Self::modal_split`]'s reasons: a
    /// share the reader chose outlives the screen it was chosen on, and it is
    /// never persisted, because the choice answers what is on screen now.
    pub(crate) commits_split: Option<u16>,
    pub(crate) command_palette: Option<CommandPalette>,
    pub(crate) help: Option<Help>,
    pub(crate) file_finder: Option<FileFinder>,
    /// The workspace file list the last finder close left behind, so reopening
    /// does not re-walk a tree that has not changed.
    ///
    /// A full ignore-aware walk is seconds of visibly repopulating rows on a
    /// large repo, and the overwhelmingly common case is a finder reopened over
    /// an unchanged tree. Open moves the list out and close moves it back, so
    /// the seed costs nothing to hand over. `None` whenever no finder has closed
    /// yet, or the list was seeded into one that is open right now.
    pub(crate) finder_path_cache: Option<FinderPathCache>,
    /// Counts the changes that would make a cached workspace file list wrong.
    ///
    /// Bumped by [`debounce::drain_fs_watch_events`] for anything that moves the set
    /// of paths a walk would yield, meaning a create, a delete, a rename, or a
    /// `.gitignore` write under the root. Plain content edits leave it alone,
    /// since they change what a file holds and not whether it is listed.
    pub(crate) finder_path_epoch: u64,
    /// Open document-symbol finder modal, or `None`. Fed by
    /// [`action_handlers::lsp::pump_lsp_symbol_picker`] and refiltered on the
    /// render path.
    pub(crate) symbol_finder: Option<SymbolFinder>,
    pub(crate) workspace_picker: Option<WorkspacePicker>,
    /// Confirmation modal shown when [`stoat_action::QuitAll`] fires
    /// with at least one dirty buffer in any workspace. `Some` while
    /// the user is being prompted to discard or cancel; cleared on
    /// cancel and stays `Some` on confirm (the app exits anyway).
    pub(crate) quit_all_confirm: Option<QuitAllConfirm>,
    /// Modal listing the focused editor's jumplist entries; opened by
    /// [`stoat_action::OpenJumplistPicker`] and dismissed on jump or
    /// cancel.
    pub(crate) jumplist_picker: Option<crate::jumplist_picker::JumplistPicker>,
    /// Active diagnostics picker modal (`space l d`). `Some` while
    /// the modal is open; cleared on Esc, on selection (after
    /// jumping the focused editor's cursor), and on Ctrl-C.
    pub(crate) diagnostics_picker: Option<crate::diagnostics_picker::DiagnosticsPicker>,
    /// Active commit picker modal, opened by `:git-review` to choose the
    /// commit a review walk starts from. `Some` while the modal is open;
    /// cleared on Esc, on selection, and on Ctrl-C.
    pub(crate) commit_picker: Option<crate::commit_picker::CommitPicker>,
    /// Active multi-location goto picker modal. `Some` while a goto
    /// request that resolved to two or more locations is awaiting the
    /// user's choice. Cleared on Esc (restoring the prior mode), on
    /// selection (after jumping), and on Ctrl-C.
    pub(crate) location_picker: Option<crate::location_picker::LocationPicker>,
    /// Name of the action that most recently opened a picker
    /// successfully. Used by `OpenLastPicker` (`space '`) to
    /// re-fire the same action and rebuild the picker fresh.
    /// Only set when an opening dispatch returned `Redraw`;
    /// no-op opens do not overwrite the prior recall target.
    pub(crate) last_picker_action: Option<&'static str>,
    pub(crate) code_search: Option<crate::code_search::CodeSearchFinder>,
    /// Active input modal for typing the regex passed to
    /// [`stoat_action::SplitSelection`]. `Some` while the user
    /// composes the pattern; cleared on submit or cancel.
    pub(crate) split_selection_input:
        Option<action_handlers::split_selection::SplitSelectionInputState>,
    /// Active input modal for typing the keep- / remove-selections
    /// regex. `Some` while the user composes the pattern; cleared
    /// on submit or cancel.
    pub(crate) filter_selections_input:
        Option<action_handlers::filter_selections::FilterSelectionsInputState>,
    /// Active macro recording. `Some` between two `Q` presses;
    /// every key dispatched in the meantime is appended via
    /// [`action_handlers::macro_recording::capture`].
    pub(crate) macro_recording: Option<action_handlers::macro_recording::MacroRecording>,
    /// How many times the armed replay chord runs its macro, `None` while no
    /// chord is armed.
    ///
    /// [`stoat_action::ReplayMacro`] arms it, and the next char keypress in
    /// normal/select mode names a register and replays the stored macro that
    /// many times. A non-char keypress disarms it instead.
    ///
    /// The count rides here rather than in [`Self::pending_count`] because the
    /// dispatch that arms the chord clears that one before the register char
    /// arrives, and a count typed for the replay belongs to the whole macro
    /// rather than to whichever key the macro happens to start with.
    pub(crate) pending_macro_replay: Option<u32>,
    /// Active input modal for typing a shell command. `Some` while
    /// the user composes the command; cleared on submit or cancel.
    pub(crate) shell_input: Option<action_handlers::shell::ShellInputState>,
    /// Subprocess executor used by the shell-integration actions.
    /// Tests install [`crate::host::FakeShell`].
    pub(crate) shell_host: Arc<dyn crate::host::ShellHost>,
    /// Opens owned agent (Claude) PTY sessions. Production wires
    /// [`crate::host::LocalTerminalHost`]. Tests can install
    /// [`crate::host::FakeTerminalHost`].
    pub(crate) terminal_host: Arc<dyn crate::host::TerminalHost>,
    /// When true, [`Self::save_workspace`] and the startup load path become
    /// no-ops. Set by the test harness so test runs can't read or write the
    /// real `$XDG_STATE_HOME/stoat/workspaces/` directory.
    pub(crate) persistence_disabled: bool,
    pub(crate) language_registry: Arc<LanguageRegistry>,
    pub(crate) syntax_styles: SyntaxStyles,
    pub(crate) workspaces: SlotMap<WorkspaceId, Workspace>,
    pub(crate) active_workspace: WorkspaceId,
    /// App-level badge tray for cross-workspace notifications. Badges here
    /// render regardless of which workspace is active, complementing each
    /// workspace's own [`Workspace::badges`]. The tray the badge lives in
    /// is the source of truth for its scope.
    pub(crate) badges: BadgeTray,
    pub(crate) pty_tx: Sender<PtyNotification>,
    pty_rx: Receiver<PtyNotification>,
    /// Hook events from the per-session agent IPC servers. Each
    /// [`crate::agent_ipc::serve_agent_hooks`] task holds a clone of the
    /// sender; [`Self::run`] drains the receiver and applies events to the
    /// owning workspace's [`AgentStatus`] off the paint path.
    pub(crate) agent_event_tx: Sender<AgentEvent>,
    agent_event_rx: Receiver<AgentEvent>,
    /// Control requests from the per-session agent IPC servers that expect a
    /// reply, kept separate from [`Self::agent_event_tx`] because each carries a
    /// oneshot the event loop fires on completion. [`Self::run`] drains the
    /// receiver and routes each to [`Self::handle_agent_control`].
    pub(crate) agent_control_tx: Sender<AgentControl>,
    agent_control_rx: Receiver<AgentControl>,
    /// Per-file shards from the cold-build scan, drained each tick into the
    /// owning workspace's [`Workspace::code_graph`]. Unbounded so the
    /// streaming build never blocks on a full channel.
    pub(crate) index_update_tx: UnboundedSender<IndexUpdate>,
    index_update_rx: UnboundedReceiver<IndexUpdate>,
    /// Window focus, resize, and close events forwarded by the reader task that
    /// connects to stoatty's `STOATTY_WINDOW_SOCKET`. Drained each tick into
    /// [`Self::handle_window_ipc`].
    window_ipc_tx: UnboundedSender<WindowIpc>,
    window_ipc_rx: UnboundedReceiver<WindowIpc>,
    /// The UI thread's one report of whether a stoatty answered the ident
    /// handshake, drained into [`Self::handle_stoatty_present`].
    ///
    /// Unlike the side channels above, whose senders this struct holds, the
    /// sender lives on the UI thread and so goes away when that thread exits.
    /// [`Self::run`] matches `Some` on this arm rather than testing for a closed
    /// channel, which parks the arm once it closes instead of waking the loop on
    /// every poll. Born closed, since a process with no UI thread never reports.
    stoatty_rx: UnboundedReceiver<Option<u32>>,
    /// A client attached to this session, so the terminal on fd 0 is a new one
    /// that holds nothing this process sent. Born closed like [`Self::stoatty_rx`],
    /// since a run with no attach server never reports.
    attached_rx: UnboundedReceiver<()>,
    /// Whether the UI thread has answered the startup ident handshake, either
    /// way. Distinct from [`Self::stoatty`], which says a stoatty answered:
    /// a plain terminal reports too, and a reconnect waits on the report
    /// rather than on the protocol.
    pub(crate) terminal_reported: bool,
    /// Whether an active workspace carries a remote target that has not been
    /// reconnected to yet. Set when a restore or a switch makes such a
    /// workspace active, cleared by [`crate::ssh::reconnect_when_ready`] once
    /// the terminal report lets the handoff run.
    pub(crate) remote_pending: bool,
    /// Pixels per cell as the tty reports them, or `None` while it has not.
    ///
    /// An image is transmitted in pixels and placed in cells, so fitting one to
    /// a pane needs the ratio between them, and only the tty knows it. Re-read
    /// on every resize, since a window that changed size or font has changed it.
    pub(crate) cell_pixels: Option<(u16, u16)>,
    /// Images this session has transmitted to the terminal, and where each one
    /// currently sits.
    ///
    /// Held for the session because the protocol has no way to ask the terminal
    /// what it already holds. An editor that forgot would re-transmit a file on
    /// every frame that shows it.
    pub(crate) images: crate::image_emit::ImageRuntime,
    /// Carries [`Self::cell_pixels`] from the ui thread, which owns the tty this
    /// is read from.
    cell_pixels_rx: UnboundedReceiver<Option<(u16, u16)>>,
    /// Whether the window-event socket is currently connected. Gates pane
    /// detach, which needs stoatty to host the aux window and report its events.
    pub(crate) window_ipc_connected: bool,
    /// Whether the zoom combo is currently claimed from the hosting terminal.
    ///
    /// The claim needs both a stoatty and a window socket reaching this process,
    /// which arrive independently and in either order, so this keeps whichever
    /// lands second from claiming twice and the release from going out unclaimed.
    pub(crate) zoom_claimed: bool,
    /// Aux windows stoatty has been told to open, keyed by window id with the
    /// cell size last sent. [`Self::emit_windows`] diffs it against the detached
    /// panes each frame to emit the WindowOpen and WindowClose commands.
    pub(crate) aux_windows: std::collections::BTreeMap<u32, (u16, u16)>,
    /// The pool cursor last shipped for a focused detached pane, as `(pool, row,
    /// col)`, so [`Self::emit_smooth_scroll`] re-emits only when it moves and an
    /// idle frame ships nothing. `None` when no detached pane holds focus.
    pub(crate) aux_cursor: Option<(u32, u64, u16)>,
    /// The main-window pool that last took a cursor anchor from
    /// [`apc_emit::emit_smooth_scroll`].
    ///
    /// The terminal keeps an anchor until it is released, and draws the cursor
    /// on any anchored pool that glides. So the emit releases this pool once its
    /// pane is no longer the focused editor. `None` when no pool holds one.
    pub(crate) pool_cursor_holder: Option<u32>,
    /// Cold-build worker, held only to keep the spawned scan alive while it
    /// runs. Progress arrives through [`Self::index_update_rx`].
    _index_build_task: Option<stoat_scheduler::Task<()>>,
    /// Wake-up signal for [`Self::run`]'s `tokio::select!`. Background
    /// tasks call `notify_one()` to kick the loop into a fresh
    /// `UpdateEffect::Redraw` once their result is ready, so the user
    /// does not have to type a key to see asynchronous output land
    /// (e.g. the file finder's workspace walk completing on the
    /// blocking pool). Multiple notifications collapse into one
    /// pending wake-up.
    pub(crate) redraw_notify: Arc<tokio::sync::Notify>,
    /// The same wake-up for a task whose result is not on screen.
    ///
    /// [`Self::run`] drains the debounce channels on it and paints only when
    /// that drain reports something visible moved. A workspace autosave is the
    /// case in point. Its throttle expiring has a file to write and a grid that
    /// did not change, so waking through [`Self::redraw_notify`] repaints every
    /// pane and re-emits every decoration stream for nothing.
    pub(crate) drain_notify: Arc<tokio::sync::Notify>,
    /// Notified once to make [`Self::run`] quit at the next loop turn,
    /// regardless of editor state. The `--timeout` self-driver uses it to
    /// auto-close a scripted session after a fixed delay. A notification
    /// fired before the loop first polls it is retained, so the quit is not
    /// lost in a race with the timer.
    pub(crate) shutdown_notify: Arc<tokio::sync::Notify>,
    /// Main-thread latency metrics, recorded around the run loop's per-frame
    /// steps. Only present under the `perf` feature.
    #[cfg(feature = "perf")]
    pub(crate) perf: crate::perf::PerfStats,
    /// A cross-file changed-file hop scanning off the UI thread, applied by
    /// [`action_handlers::movement::pump_changed_file_jump`] when it lands.
    pub(crate) pending_changed_file_jump: Option<action_handlers::movement::PendingChangedFileJump>,
    /// A conflicted file read and aligned off the UI thread, applied by
    /// [`action_handlers::conflict_view::pump_conflict_file`] when it lands.
    ///
    /// An open discovers the repository and lists its conflicts, which loads
    /// the whole index. Each file then reads its three stages, and the
    /// alignment runs two structural diffs against the ancestor.
    pub(crate) pending_conflict_file: Option<action_handlers::conflict_view::PendingConflictFile>,
    /// The git writes the loop started, run one at a time in press order.
    ///
    /// Two writes on the pool race, so every write the loop starts waits here
    /// for the one before it to land. See [`git_jobs`].
    pub(crate) git_jobs: GitJobs,
    /// A diff-filtered call-graph hop whose working-tree scan runs off the UI
    /// thread, applied by [`crate::code_index::nav::pump_diff_nav_jump`] when it
    /// lands.
    pub(crate) pending_diff_nav_jump: Option<crate::code_index::nav::PendingDiffNavJump>,
    /// In-flight code-search scan streaming match batches from the blocking pool.
    pub(crate) pending_code_search: Option<action_handlers::code_search::PendingCodeSearch>,
    /// Timer that forwards the latest code-search query on
    /// [`Self::code_search_query_tx`] after [`debounce::CODE_SEARCH_DEBOUNCE`]. A new
    /// keystroke drops it, cancelling the pending scan trigger.
    pub(crate) code_search_debounce: Option<stoat_scheduler::Task<()>>,
    pub(crate) code_search_query_tx: Sender<String>,
    pub(crate) code_search_query_rx: Receiver<String>,
    /// An in-flight background diff-cache warm pass, drained by
    /// [`crate::diff_warm::install_finished`] in [`Self::drive_background`].
    pub(crate) pending_diff_warm: Option<crate::diff_warm::PendingDiffWarm>,
    pub(crate) modal_run: Option<RunId>,
    /// Session-wide toggle for tree-sitter syntax coloring, applied to every
    /// editor at paint time. Not a [`crate::config::Settings`] field:
    /// persistence can come later. Defaults to on.
    pub(crate) syntax_highlight: bool,
    /// Runtime override for the minimap strip's visibility, set by
    /// `ToggleMinimap`. `None` follows the `editor.minimap` setting; `Some`
    /// wins for the session. Not persisted.
    pub(crate) minimap_override: Option<bool>,
    /// Session-only override of the `ui.tab_bar` setting, set by `:tabs`.
    /// `None` leaves the configured mode in force.
    pub(crate) tab_bar_override: Option<TabBarMode>,
    /// The extent of each tab that the last paint drew in the tab bar.
    ///
    /// The extents run in tab order, in sixteenths of a cell from the window's
    /// left edge, and the list is empty while the bar is hidden. The tab bar's
    /// press hit test reads it.
    pub(crate) tab_bar_spans: Vec<Range<u16>>,
    /// The window-right strip band single-minimap mode reserves, stamped every
    /// paint. `Some` only under stoatty in [`stoat_config::MinimapMode::Single`]
    /// on a wide-enough window, and `None` in per-pane and off modes.
    pub(crate) single_minimap_rect: Option<Rect>,
    /// The focused pane's status-bar LSP badge group, in cells, stamped every
    /// paint. `Some` only when a badge painted, `None` when the focused bar shows
    /// no server badge. The badge-hover hit test consumes it.
    pub(crate) lsp_badge_rect: Option<Rect>,
    /// Whether the detailed LSP status popout is pinned open, toggled by
    /// `ToggleLspStatus`. Off by default. A runtime session flag, not persisted.
    pub(crate) lsp_status_pinned: bool,
    /// Whether the pointer currently rests on the LSP badge, which opens the
    /// status popout for as long as it does. Set from [`Self::lsp_badge_rect`] by
    /// the hover handler and cleared when no badge paints.
    pub(crate) lsp_badge_hovered: bool,
    /// Whether the keybinding hints overlay is force-shown in a primary mode,
    /// toggled by `ToggleKeyHints`, off by default. A runtime session flag like
    /// [`Self::syntax_highlight`], not persisted. Contexts that already
    /// auto-show the overlay are unaffected.
    pub(crate) key_hints_visible: bool,
    /// Grouped hint rows cached for the current keymap-state hash, letting an
    /// unchanged frame skip the full keybinding walk and regrouping.
    pub(crate) hints_cache: Option<crate::render::hints::HintsCache>,
    /// Whether LSP inlay hints are requested and rendered for the focused
    /// editor. Toggled by `ToggleInlayHints`, off by default. Not persisted.
    pub(crate) inlay_hints_enabled: bool,
    /// In-flight viewport inlay-hint request, armed by
    /// [`action_handlers::lsp::inlay_hints_trigger`] behind a debounce and
    /// applied by [`action_handlers::lsp::pump_lsp_inlay_hints`].
    pub(crate) pending_inlay_hint_request: Pending<Option<action_handlers::lsp::InlayHintResponse>>,
    /// `(buffer, version, first row, last row)` the inlay-hint trigger last
    /// requested for, so an unchanged tick does not re-request.
    pub(crate) last_inlay_hint_key: Option<(BufferId, u64, u32, u32)>,
    /// In-flight document-highlight request, armed by
    /// [`crate::lsp::document_highlight::document_highlight_trigger`] behind a
    /// debounce and applied by
    /// [`crate::lsp::document_highlight::pump_lsp_document_highlight`].
    pub(crate) pending_document_highlight_request:
        Pending<Option<crate::lsp::document_highlight::DocumentHighlightResponse>>,
    /// `(buffer, version, cursor offset)` the document-highlight trigger last
    /// requested for, so an unchanged tick does not re-request.
    pub(crate) last_document_highlight_key: Option<(BufferId, u64, usize)>,
    /// Last diagnostic `result_id` the server returned per buffer, sent as
    /// `previous_result_id` on the next pull so the server may answer Unchanged.
    pub(crate) pull_diagnostic_result_ids: std::collections::HashMap<BufferId, String>,
    /// In-flight pull-diagnostic requests per buffer, armed by
    /// [`crate::lsp::pull_diagnostics::pull_diagnostics_trigger`] behind a
    /// debounce and applied by
    /// [`crate::lsp::pull_diagnostics::pump_lsp_pull_diagnostics`].
    pub(crate) pending_pull_diagnostics: std::collections::HashMap<
        BufferId,
        stoat_scheduler::Task<Option<crate::lsp::pull_diagnostics::PullDiagnosticsOutcome>>,
    >,
    /// Buffer version the pull-diagnostic trigger last requested for, per buffer,
    /// so an unchanged tick does not re-request.
    pub(crate) last_pull_diagnostic_key: std::collections::HashMap<BufferId, u64>,
    /// In-flight semantic-token request for the focused editor, armed by
    /// [`crate::lsp::semantic_tokens::semantic_tokens_trigger`] behind a
    /// debounce and applied by
    /// [`crate::lsp::semantic_tokens::pump_lsp_semantic_tokens`].
    pub(crate) pending_semantic_tokens:
        Pending<Option<crate::lsp::semantic_tokens::SemanticTokensOutcome>>,
    /// `(buffer, version)` the semantic-token trigger last requested for, so an
    /// unchanged tick does not re-request.
    pub(crate) last_semantic_tokens_key: Option<(BufferId, u64)>,
    /// In-flight folding-range request for the focused editor, armed by
    /// [`crate::lsp::folding::folding_ranges_trigger`] behind a debounce and
    /// applied by [`crate::lsp::folding::pump_lsp_folding_ranges`].
    pub(crate) pending_folding_ranges: Pending<Option<crate::lsp::folding::FoldingRangesOutcome>>,
    /// `(buffer, version)` the folding-range trigger last requested for, so an
    /// unchanged tick does not re-request.
    pub(crate) last_folding_range_key: Option<(BufferId, u64)>,
    pub(crate) render_tick: u64,
    /// The completion popup's geometry for the frame being painted, so the
    /// paint and the pool emit compute it once between them. Stamped with the
    /// [`Self::render_tick`] it was built for, which is what makes a memo left
    /// by an earlier frame recognizable. Transient render state, not persisted.
    pub(crate) completion_layout: Option<crate::render::completion::CompletionLayoutMemo>,
    /// Transient one-line message painted in a reserved bottom row,
    /// such as a failed-save error. Set through [`Self::set_status`],
    /// which stamps [`Self::pending_message_deadline`]. The message
    /// stays visible until that deadline passes or a newer message
    /// replaces it, and input no longer clears it.
    pub(crate) pending_message: Option<String>,
    /// When the current [`Self::pending_message`] expires, on the
    /// scheduler clock. [`crate::render::frame`] clears the message
    /// once [`Executor::now`] reaches this.
    pub(crate) pending_message_deadline: Option<std::time::Instant>,
    /// The timer task that wakes the run loop at the deadline so an
    /// idle screen retires the message without waiting for input.
    /// Replacing it cancels the prior timer.
    pub(crate) pending_message_expiry: Option<stoat_scheduler::Task<()>>,
    /// The timer that wakes the run loop when a retiring walkthrough slide has
    /// finished un-drawing itself, so an idle screen drops it without a key
    /// press. Replacing it cancels the prior timer.
    pub(crate) walkthrough_exit_timer: Option<stoat_scheduler::Task<()>>,
    /// When a modified wheel notch last ran a keymap binding, on the
    /// scheduler clock.
    ///
    /// A trackpad flick arrives as a burst of per-line reports, and an ungated
    /// burst walks a hundred jumps. A notch inside [`WHEEL_BINDING_COOLDOWN`]
    /// of this scrolls instead of dispatching.
    pub(crate) wheel_binding_last: Option<std::time::Instant>,
    /// Sub-notch wheel travel held for the consumers that step by whole
    /// notches: a keymap wheel binding, the boxed modals, the hover popup, and
    /// a run pane.
    ///
    /// A trackpad reports travel worth a fraction of a line, which those
    /// surfaces have no smaller unit to spend. Accruing it here is what turns a
    /// stream of small reports into the notch they each expect, rather than
    /// dropping every one of them. An editor pane never reads this: it consumes
    /// the fraction directly and rests between rows.
    pub(crate) wheel_line_remainder: f32,
    /// Wheel travel accrued at an open diff-view jump line toward the next
    /// walk, signed by direction.
    ///
    /// Travel the other way, a closed jump line, and a walk each clear it, so
    /// only one notch's worth of travel in one direction walks.
    pub(crate) diff_wheel_travel: f32,
    /// When a diff-view wheel notch last walked to a change, on the scheduler
    /// clock. A walk inside [`DIFF_WHEEL_COOLDOWN`] of this is dropped.
    pub(crate) diff_wheel_last: Option<std::time::Instant>,
    /// Whether the plain wheel in the diff view walks from change to change.
    ///
    /// On, the wheel on the focused diff editor scrolls until the change under
    /// the cursor passes the jump line, and the next notch walks to the next
    /// change. Off, the wheel scrolls the pane as in any other editor. The
    /// change keys and Alt-wheel walk either way.
    ///
    /// The `DiffWheelWalk` action is the only writer. Session-scoped and on at
    /// start, never persisted, because it answers how the reader reads the
    /// diff now.
    pub(crate) diff_wheel_walk: bool,
    /// Accumulated digit prefix for the next motion (Vim-style
    /// `<count>j` etc.). Filled by `handle_key` when a digit press
    /// hits an unbound key in normal mode; consumed once via
    /// `take_pending_count` and cleared after every action dispatch.
    pub(crate) pending_count: Option<u32>,
    /// Pending Vim-style find-char prefix (`f`/`F`/`t`/`T`). When
    /// Some, the next printable char keypress runs the matching
    /// find on the focused editor and clears this field. The
    /// trailing `u32` is the count captured from `pending_count`
    /// at the time the chord was armed; defaults to 1.
    pub(crate) pending_find: Option<(action_handlers::movement::FindKind, bool, u32)>,
    /// Pending mark chord (`m`/`'`/`` ` ``). When `Some`, the next
    /// printable char keypress in normal mode either stores or jumps
    /// to the named mark per [`action_handlers::marks::execute_mark`]
    /// and clears this field. A non-char keypress also clears it.
    pub(crate) pending_mark: Option<action_handlers::marks::MarkRequest>,
    /// Buffer-local marks keyed by `(BufferId, char)` -> stable
    /// [`Anchor`]. Anchors resolve to the current byte offset through
    /// the fragment tree, so edits before a mark move it with the
    /// surrounding content.
    pub(crate) marks: std::collections::HashMap<(BufferId, char), Anchor>,
    /// Global marks keyed by uppercase char -> `(path, byte offset)`.
    /// Cross-buffer: `goto` opens the file in the focused pane and
    /// seeks to the stored offset. Offsets are not anchor-tracked --
    /// `Anchor`s tie to a buffer session, while global marks must
    /// survive buffer close+reopen.
    pub(crate) global_marks: std::collections::HashMap<char, (PathBuf, usize)>,
    /// Active label set for an in-progress `goto_word` jump. `Some`
    /// after `GotoWord` is dispatched until the user types a unique
    /// label or types a non-matching prefix. Renderer overlays the
    /// label strings on their target positions while this is set.
    pub(crate) pending_goto_word: Option<std::collections::BTreeMap<String, (usize, usize)>>,
    /// Characters typed so far to disambiguate the active goto-word
    /// label. Always paired with [`Self::pending_goto_word`]: when
    /// that field is `None` this is empty.
    pub(crate) pending_goto_word_input: String,
    /// The primary's span when the labels went up, set only where the
    /// arming was `ExtendToWord`. The landing grows from this rather
    /// than replacing the selection, and `None` means it replaces.
    pub(crate) pending_goto_word_extend: Option<(usize, usize)>,
    /// Set after a `ReplaceChar` action arms the one-shot prompt.
    /// While true, the next printable char keypress in normal/select
    /// mode replaces every character in every non-empty selection
    /// with that char and clears the flag.
    pub(crate) pending_replace: bool,
    /// Set after a `SurroundAdd` action arms the chord. While true,
    /// the next printable char keypress in normal/select mode wraps
    /// every non-empty selection with that char's surround pair via
    /// [`action_handlers::surround::execute_surround_add`] and clears
    /// the flag. Non-char keypresses also clear the flag.
    pub(crate) pending_surround_add: bool,
    /// Two-step capture state for `SurroundReplace`: the action arms
    /// `AwaitFrom`; the next char keypress transitions to
    /// `AwaitTo(from)`; the following char keypress applies the edit
    /// via [`action_handlers::surround::execute_surround_replace`]
    /// and clears the state. Non-char keypresses also clear the
    /// state.
    pub(crate) pending_surround_replace: action_handlers::surround::SurroundReplaceStage,
    /// Set after a `SurroundDelete` action arms the chord. While
    /// true, the next printable char keypress in normal/select mode
    /// finds the enclosing surround pair for that char around every
    /// cursor and removes it via
    /// [`action_handlers::surround::execute_surround_delete`].
    /// Non-char keypresses also clear the flag.
    pub(crate) pending_surround_delete: bool,
    /// How far out the armed surround chord reaches, as the count typed
    /// in front of it. One means the pair nearest each cursor.
    ///
    /// `SurroundDelete` and `SurroundReplace` both write it as they arm,
    /// and neither runs until the other clears, so the one field serves
    /// both. Dispatching those actions consumes the pending count
    /// before the chars completing the chord arrive, which is why the
    /// count is captured here rather than read where the edit runs.
    pub(crate) pending_surround_count: usize,
    /// Set after `SelectTextobjectAround` or `SelectTextobjectInner`
    /// arms the chord. The next printable char keypress in normal /
    /// select mode names the textobject type (`f` function, `t`
    /// class, `p` paragraph, `a` parameter, `c` comment) and is
    /// resolved via
    /// [`action_handlers::textobject::execute_select_textobject`].
    /// Non-char keypresses also clear the state.
    ///
    /// The count rides along because dispatching the arming action consumes
    /// the pending count, leaving the type char's keypress nothing to read.
    pub(crate) pending_textobject_select:
        Option<(action_handlers::textobject::TextobjectMode, usize)>,
    /// Active search input modal. Some while the user is typing a
    /// `/` (forward) or `?` (reverse) search query; cleared by
    /// [`action_handlers::search::search_submit`] or
    /// [`action_handlers::search::search_cancel`].
    pub(crate) search_input: Option<action_handlers::search::SearchInputState>,
    /// Persisted query + direction from the most recent submitted
    /// search. Drives `SearchNext` / `SearchPrev` repeats.
    pub(crate) last_search: Option<action_handlers::search::LastSearch>,
    /// Buffer accumulating text typed during the current
    /// insert-mode session. `Some` while `mode == "insert"` (or
    /// equivalent), `None` outside.
    ///
    /// Read on insert-mode exit to tell a session that typed nothing from one
    /// that did, which is what decides whether an auto-indent the reader never
    /// filled in is left behind.
    pub(crate) current_insert_run: Option<String>,
    /// Set by `a` so leaving insert moves each block cursor back one grapheme,
    /// landing on the last typed (or appended-over) char rather than one cell
    /// past it. It is cleared on the insert-to-normal transition.
    ///
    /// `a` is the only entry that sets it. It is the one that reaches a cursor
    /// out past where it started, so it is the one with something to give back.
    /// `A` and `I` move a cursor without extending it.
    pub(crate) restore_cursor: bool,
    /// Set while dispatching a key whose action list switches to insert mode,
    /// so an editing action in that list leaves its undo group open for the
    /// insert session to adopt.
    ///
    /// The insert-entry keys are `[Action(), SetMode(insert)]` pairs, and the
    /// user made one change, not two. Sealing between the two halves puts the
    /// edit and the typing in separate revisions, so one undo takes back only
    /// the typing.
    ///
    /// Only the key-dispatch loop sets it, since it is the only caller that
    /// sees a whole action list before running it. An action dispatched from
    /// the palette keeps the per-action grouping.
    pub(crate) group_held_for_insert: bool,
    /// Selection IDs whose line was auto-indented by the insert entry
    /// (`o`/`O`/`I`/`A` on an empty line). The insert-to-normal transition
    /// takes it and, when the session typed nothing, strips each recorded
    /// line's untouched indentation back to a clean empty line. Other insert
    /// entries never set it.
    pub(crate) auto_indent_cursors: Vec<usize>,
    /// Process-wide register store for yank, paste, and (later)
    /// macros and `insert_register`. Unnamed and named registers
    /// live in-process; system / primary clipboard variants are
    /// stubbed until the `arboard` backend lands.
    pub(crate) registers: register::RegisterStore,
    /// Set after `SelectRegister` arms the chord. The next
    /// printable char in normal/select mode is captured as the
    /// register name and stored in [`Self::selected_register`].
    pub(crate) pending_register_select: bool,
    /// Register named by `SelectRegister`, waiting for the command that spends
    /// it. `None` means the unnamed register is the implicit target.
    ///
    /// Moved into [`Self::command_register`] by the next dispatch, so whatever
    /// command ran next is what spent the selection.
    pub(crate) selected_register: Option<register::Register>,
    /// Register the command being dispatched right now reads, taken off
    /// [`Self::selected_register`] before its handler runs.
    ///
    /// Every command takes it, so a command with no use for a register still
    /// spends the selection and the one after it starts clean.
    command_register: Option<register::Register>,
    /// Set after `InsertRegister` arms the chord in insert mode.
    /// The next char keypress is captured as the register name;
    /// that register's content is inserted at the cursor and the
    /// flag clears. Non-char keypresses also clear the flag.
    pub(crate) pending_insert_register: bool,
    /// Registers whose macros are being replayed right now, innermost last.
    ///
    /// A replay re-feeds its keys through the same path a real keypress takes,
    /// which without this would record the expansion into whatever macro is
    /// recording, and would let a macro naming itself run forever.
    pub(crate) replaying_registers: Vec<register::Register>,
    /// Set on `MouseEventKind::Down(Left)` over a focused editor pane, as
    /// `(editor, buffer, moved)`. While `Some`, `Drag(Left)` events extend the
    /// matching editor's primary selection head and set `moved`. `Up(Left)`
    /// copies the selection to the clipboard only when `moved`, then clears the
    /// field. The flag keeps a plain click, now a 1-wide block cursor, from
    /// copying a character.
    pub(crate) editor_drag: Option<(EditorId, BufferId, bool)>,
    /// The in-flight terminal-pane selection drag, or `None` when no drag is
    /// active. Holds the dragged pane's [`TermId`] and whether the pointer has
    /// moved since the press, so `Up(Left)` copies the selection only for a real
    /// drag and a plain click leaves no selection behind.
    pub(crate) terminal_drag: Option<(TermId, bool)>,
    /// Terminal cell the mouse last rested over a focused editor pane, or
    /// `None` before any motion. The render resolves the diagnostic under it
    /// to raise a hover popover. Motion events only arrive with mouse capture
    /// enabled, so with capture off this stays `None` and only the cursor
    /// trigger fires.
    pub(crate) hover_cell: Option<(u16, u16)>,
    /// Index of the diagnostic the mouse last resolved to, used to redraw only
    /// when the hovered diagnostic changes rather than on every motion event.
    pub(crate) hover_diag: Option<usize>,
    /// Set on `MouseEventKind::Down(Left)` over a split divider. While `Some`,
    /// `Drag(Left)` moves that boundary via `set_divider` and `Up(Left)` clears
    /// it. Takes over the pointer so pane handlers never see the drag.
    pub(crate) divider_drag: Option<(NodeId, usize)>,
    /// Which open modal's list/preview separator the pointer is moving, set on
    /// `MouseEventKind::Down(Left)` over that separator and cleared on
    /// `Up(Left)`. While `Some`, `Drag(Left)` writes the pointer's position back
    /// as that kind's [`Self::modal_split`] share.
    ///
    /// Named by kind rather than held as a bare flag because the share it writes
    /// is stored per kind, and the modal that armed the drag is the one it has to
    /// land on.
    pub(crate) modal_separator_drag: Option<ModalKind>,
    /// Whether the pointer moves the commits screen's list/detail separator,
    /// set on `MouseEventKind::Down(Left)` over that line and cleared on
    /// `Up(Left)`. While set, `Drag(Left)` writes the pointer's position back
    /// as [`Self::commits_split`].
    ///
    /// A bare flag rather than a kind, because only one screen carries this
    /// separator and its share has one home.
    pub(crate) commits_separator_drag: bool,
    /// Set on `MouseEventKind::Down(Left)` over a pane's minimap strip. While
    /// `Some`, `Drag(Left)` scrubs the named editor's viewport to the pointer
    /// position and `Up(Left)` clears it. Takes over the pointer so the press
    /// never reaches the text-area cursor or selection handling.
    pub(crate) minimap_drag: Option<EditorId>,
    /// Buffers for which `LspHost::did_open` has been dispatched.
    /// Dedupes re-opens of the same path: [`crate::buffer_registry::BufferRegistry::open`]
    /// returns the existing entry on second open, but the LSP
    /// notification must fire exactly once per buffer over its
    /// lifetime.
    pub(crate) lsp_opened: std::collections::HashSet<BufferId>,
    /// Scratch the per-event LSP drains refill instead of allocating.
    ///
    /// Both drains need the whole `&mut Stoat` in their loop bodies, so neither
    /// can walk what it is iterating borrowed. Reusing one buffer across events
    /// keeps that allocation off the keystroke path. Each is emptied before it
    /// is parked, so only its capacity carries over.
    pub(crate) lsp_drain_hosts: Vec<Arc<dyn LspHost>>,
    pub(crate) lsp_drain_buffers: Vec<BufferId>,
    /// Last buffer version a `did_change` debounce has been
    /// scheduled for. Bumped synchronously on the edit-detection
    /// tick so a buffer is never enqueued twice for the same
    /// version. Initialised on `did_open`.
    pub(crate) lsp_buffer_versions: std::collections::HashMap<BufferId, u64>,
    /// Pending `did_change` debounce timer per buffer. Replacing
    /// the entry drops the old [`stoat_scheduler::Task`] which
    /// cancels the spawned future before its 50ms timer fires;
    /// only the most recent edit's snapshot ever reaches the
    /// server.
    pub(crate) lsp_pending_changes: std::collections::HashMap<BufferId, stoat_scheduler::Task<()>>,
    /// Poll task re-reading auto-reload-flagged buffers, live only while at
    /// least one buffer is flagged. Dropping the task cancels its timer loop, so
    /// [`crate::auto_reload::pump_auto_reload`] clears this field to
    /// disarm the poll once no buffer wants following.
    pub(crate) auto_reload_poll: Option<stoat_scheduler::Task<()>>,
    /// Poll ticks from [`Self::auto_reload_poll`]'s timer, one per interval.
    ///
    /// The run loop receives them on its own select arm so a tick wakes it
    /// without implying a frame. Only [`crate::auto_reload::pump_auto_reload`]
    /// reporting a change turns one into a repaint, which is what keeps a
    /// buffer tailing an idle file from painting twice a second. The single
    /// slot coalesces ticks that arrive while the loop is busy.
    pub(crate) auto_reload_tx: Sender<()>,
    auto_reload_rx: Receiver<()>,
    /// LSP-protocol document version per buffer. Starts at 0 from
    /// `did_open` and increments at `did_change` spawn time. Gaps
    /// (e.g. the prior task was cancelled before fire) are allowed
    /// per LSP spec which only requires monotonicity.
    pub(crate) lsp_doc_versions: std::collections::HashMap<BufferId, i32>,
    /// Full document text the server most recently received via a
    /// successful `did_open` or `did_change`. Used by the
    /// Incremental-mode dispatch path to compute LSP positions for
    /// the bytes the server is about to delete; cancelled tasks
    /// never reach the server, so the prior delivered snapshot
    /// remains the right basis for the next patch. Updated by the
    /// spawned dispatch task on success.
    pub(crate) lsp_last_delivered_text:
        Arc<std::sync::Mutex<std::collections::HashMap<BufferId, Rope>>>,
    /// Buffer version at the last successful `did_open` /
    /// `did_change` delivery, paired with `lsp_last_delivered_text`.
    /// `Buffer::edits_since(this)` produces the patch the next
    /// dispatch needs to encode.
    pub(crate) lsp_last_delivered_buffer_version:
        Arc<std::sync::Mutex<std::collections::HashMap<BufferId, u64>>>,
    /// LSP diagnostics keyed by file path. Updated as
    /// `LspNotification::Diagnostics` arrives during
    /// [`Self::drain_lsp_notifications`]; surfaced by the status bar
    /// for the focused buffer.
    pub(crate) diagnostics: crate::diagnostics::DiagnosticSet,
    /// Most recent motion worth stepping through, recorded by the handler that
    /// ran it. `RepeatLastMotion` (Alt-.) replays it without reading another
    /// keypress.
    pub(crate) last_motion: Option<action_handlers::LastMotion>,
    /// Filesystem the UI layer reads through. Swapped to
    /// [`crate::host::FakeFs`] in tests; all IO outside the host module
    /// itself must route through this field.
    pub(crate) fs_host: Arc<dyn FsHost>,
    /// Filesystem-change subscription host. Defaults to
    /// [`NoopFsWatcher`]; the bin layer installs
    /// [`crate::host::LocalFsWatcher`] and tests install
    /// [`crate::host::FakeFsWatcher`]. Drained per-tick by
    /// [`debounce::drain_fs_watch_events`] so an external edit stales the
    /// diffs it affects.
    pub(crate) fs_watch_host: Arc<dyn FsWatchHost>,
    /// The roots whose directories [`Self::fs_watch_host`] watches.
    ///
    /// A workspace entered again, or a second workspace on one root, then
    /// costs no walk of the tree.
    pub(crate) watched_roots: std::collections::HashSet<PathBuf>,
    /// Events [`debounce::drain_fs_watch_events`] made for the entries of a
    /// directory that arrived with content.
    ///
    /// The drain takes these ahead of the host's queue, under the same cap per
    /// turn, so a large tree is adopted over several turns and not in one.
    pub(crate) fs_watch_backlog: std::collections::VecDeque<(PathBuf, FsEventKind)>,
    /// Single-slot debounce for staling every open diff at once. A commit
    /// writes many `.git` files in a burst, and one HEAD move stales them all,
    /// so this collapses the burst into one invalidation. Re-arming replaces
    /// the task, cancelling the prior timer.
    pub(crate) pending_diff_refresh: Option<stoat_scheduler::Task<()>>,
    /// Channel the debounce task pushes onto once its timer fires, drained by
    /// [`debounce::drain_pending_diff_refresh`].
    pub(crate) diff_refresh_tx: Sender<()>,
    pub(crate) diff_refresh_rx: Receiver<()>,
    /// Channel the autosave throttle task pushes onto once its window closes,
    /// drained by [`debounce::drain_pending_workspace_autosave`].
    pub(crate) workspace_autosave_tx: Sender<()>,
    pub(crate) workspace_autosave_rx: Receiver<()>,
    /// Large files reading on the blocking pool, awaiting install by
    /// [`crate::buffer_lifecycle::install_pending_opens`] in
    /// [`Self::drive_background`]. Holding the task here keeps the read alive;
    /// dropping it (on quit) cancels it.
    pub(crate) pending_file_opens: Vec<crate::buffer_lifecycle::PendingFileOpen>,
    /// Followed files reading on the blocking pool, awaiting the edit they
    /// describe from [`crate::auto_reload::pump_auto_reload_install`].
    ///
    /// A followed build log is re-read at the poll cadence, and the read plus
    /// the comparison walk the whole file, so both sit here rather than on the
    /// thread that paints. Holding the task keeps the read alive. Dropping it
    /// on quit cancels it.
    pub(crate) pending_auto_reloads: Vec<crate::auto_reload::PendingAutoReload>,
    /// Files changed outside the editor, waiting on the shared debounce window
    /// to be reindexed into the code graph.
    ///
    /// A set covered by one timer rather than a task per path. A checkout or a
    /// formatter run names thousands of files at once, and the burst is what
    /// this has to survive.
    pub(crate) index_pending_external_edits: std::collections::HashSet<PathBuf>,
    /// The one debounce timer covering whatever
    /// [`Self::index_pending_external_edits`] holds.
    ///
    /// Armed when the set goes from empty to occupied, so the window closes a
    /// fixed [`debounce::FS_WATCH_DEBOUNCE`] after a burst starts. Under a
    /// reset-per-event timer, a build emitting events faster than that window
    /// holds the index off for as long as it runs.
    pub(crate) index_external_edit_timer: Option<stoat_scheduler::Task<()>>,
    /// Memoized [`GitRepo::is_path_ignored`] verdicts, keyed by the directory
    /// asked about rather than the file, so an fs-event storm out of a build
    /// directory costs one libgit2 query instead of one per file.
    ///
    /// Cleared on any `.git` write or `.gitignore` edit, the two events that can
    /// change an answer already in here.
    pub(crate) ignored_dir_cache: std::collections::HashMap<PathBuf, bool>,
    /// Channel [`Self::index_external_edit_timer`] signals when its window
    /// closes, waking [`debounce::drain_pending_index_edits`].
    ///
    /// Carries no path. The window covers whichever paths
    /// [`Self::index_pending_external_edits`] has collected by the time it
    /// fires, so the signal only has to say that it fired.
    pub(crate) index_external_edit_tx: Sender<()>,
    pub(crate) index_external_edit_rx: Receiver<()>,
    /// The last working-tree path a write reached while [`Self::follow_changes`]
    /// is on, waiting for [`Self::follow_timer`] to close its window.
    ///
    /// One slot rather than a set. The pane shows one file at a time, so a burst
    /// of writes follows the last path it wrote, and the diff view's n and p walk
    /// the rest.
    pub(crate) follow_pending: Option<PathBuf>,
    /// The debounce timer covering [`Self::follow_pending`].
    ///
    /// Armed when the slot fills from empty, so the window closes a fixed
    /// [`debounce::FS_WATCH_DEBOUNCE`] after a burst starts, for the reason
    /// [`Self::index_external_edit_timer`] gives.
    pub(crate) follow_timer: Option<stoat_scheduler::Task<()>>,
    /// Channel [`Self::follow_timer`] signals when its window closes, waking
    /// [`crate::auto_reload::drain_followed_change`].
    pub(crate) follow_tx: Sender<()>,
    pub(crate) follow_rx: Receiver<()>,
    /// The paths of open buffers written while [`Self::live_reload`] is on,
    /// waiting for [`Self::live_reload_timer`] to close their window.
    ///
    /// A set where [`Self::follow_pending`] is one slot, because every written
    /// buffer reloads.
    pub(crate) live_reload_pending: std::collections::HashSet<PathBuf>,
    /// The debounce timer covering [`Self::live_reload_pending`].
    ///
    /// Armed when the set fills from empty, for the reason
    /// [`Self::index_external_edit_timer`] gives.
    pub(crate) live_reload_timer: Option<stoat_scheduler::Task<()>>,
    /// Channel [`Self::live_reload_timer`] signals when its window closes,
    /// waking [`crate::auto_reload::drain_live_reload`].
    pub(crate) live_reload_tx: Sender<()>,
    pub(crate) live_reload_rx: Receiver<()>,
    /// Git operations flow through this trait so tests can use
    /// [`crate::host::FakeGit`] without a real repository.
    pub(crate) git_host: Arc<dyn GitHost>,
    /// Environment-variable lookups go through this trait so tests can
    /// install [`crate::host::FakeEnv`] without leaking real env state.
    pub(crate) env_host: Arc<dyn EnvHost>,
    /// The user home directory, resolved from [`Self::env_host`] once at
    /// construction and refreshed by [`Self::set_env_host`]. Lets the per-frame
    /// paint paths abbreviate `~` without an env lookup and allocation each
    /// frame.
    pub(crate) home: Option<PathBuf>,
    /// Language-server requests route through this trait. Defaults to
    /// Language servers keyed by name. Reached through
    /// [`crate::lsp::hosts::lsp_host`] and [`crate::lsp::hosts::lsp_for`],
    /// never directly, and empty until a real `LocalLsp` is
    /// wired in. Tests install [`crate::host::FakeLsp`] as the sole client to
    /// drive end-to-end LSP scenarios.
    pub(crate) lsp_registry: crate::lsp::registry::LspRegistry,
    /// Whether opening a buffer whose language has a known server
    /// command may spawn a real language server, replacing the
    /// [`NoopLsp`] placeholder. Off by default so [`NoopLsp`] stays
    /// side-effect-free for tests. The binary turns it on for a live
    /// session via [`Self::set_lsp_auto_spawn`].
    pub(crate) lsp_auto_spawn: bool,
    /// The spawn or initialize failure that left the [`NoopLsp`]
    /// placeholder in place, retained so a later LSP action can restate
    /// why no server is up. [`Self::pending_lsp_host`] is drained after
    /// one tick, so without this the failure and an in-flight spawn are
    /// indistinguishable.
    pub(crate) lsp_spawn_failed: Option<String>,
    /// Buffer whose language-server spawn was deferred because the
    /// workspace's direnv env was still loading when it opened. Re-fired
    /// by [`crate::project_env::install_pending`] once the env lands, so
    /// the server starts with the project environment rather than racing
    /// the load.
    pub(crate) lsp_spawn_deferred: Option<BufferId>,
    /// Landing slot for the detached language-server spawn task's outcome.
    /// Drained by [`Self::install_pending_lsp_host`] in [`Self::update`]:
    /// `Ok` swaps the ready host in for the [`NoopLsp`] placeholder, `Err`
    /// carries the failure string to surface in the message row while the
    /// placeholder stays. Shared rather than returned because the spawn runs
    /// detached on [`Self::executor`] and cannot borrow `self`.
    pub(crate) pending_lsp_host: PendingLspHost,
    /// Whether workspaces automatically load their direnv environment. Off
    /// by default so the test harness never spawns direnv. The binary
    /// turns it on for a live session via [`Self::set_env_auto_load`].
    pub(crate) env_auto_load: bool,
    /// Whether workspaces warm their diff cache in the background at open. Off
    /// by default so the test harness never spawns a warm pass. The binary
    /// turns it on for a live session via [`Self::set_diff_warm_auto`].
    pub(crate) diff_warm_auto: bool,
    /// Whether the focused pane follows every working-tree change in the repo.
    ///
    /// On, a file written outside the editor opens in the focused pane's diff
    /// view with the cursor on the change, whichever file it is. A buffer with
    /// unsaved edits is skipped, never clobbered.
    ///
    /// The `FollowChanges` action is the only writer. Session-scoped and off at
    /// start, never persisted, because it answers what the reader watches now.
    pub(crate) follow_changes: bool,
    /// Whether every open buffer re-reads its file when the file is written
    /// outside the editor.
    ///
    /// The pane, the view, and each cursor stay where they are, and a buffer
    /// with unsaved edits is skipped.
    ///
    /// The `LiveReload` action is the only writer. Session-scoped, off at
    /// start, and never persisted.
    pub(crate) live_reload: bool,
    /// Directory holding the per-workspace agent sockets, the single source of
    /// the path both [`Self::serve_term_session`] binds and an owned child's
    /// `STOAT_AGENT_SOCK` names.
    ///
    /// `None` by default, which serves no socket and injects no session
    /// environment, so a test drives the spawn paths without binding a
    /// listener or reading the developer's own state directory. The binary and
    /// the fixture harness set it via [`Self::set_agent_socket_dir`].
    pub(crate) agent_socket_dir: Option<PathBuf>,
    /// Whether [`Self::serve_term_session`] actually binds a listener.
    ///
    /// Separate from [`Self::agent_socket_dir`] because the two grant
    /// different things. A directory alone lets an owned child's environment
    /// name a socket, which a test sets to check that naming. Binding one
    /// needs a live Tokio reactor, which only the binary and the fixture
    /// harness run, so no test ever enqueues the server task.
    pub(crate) serve_agent_sockets: bool,
    /// The log file this session writes.
    ///
    /// `None` by default, which is what a test and a `--log-stderr` run have.
    /// The binary sets it via [`Self::set_session_log`]. The session renames
    /// the file as the active workspace's name changes.
    pub(crate) session_log: Option<SessionLog>,
    /// The hook server of each workspace whose socket is served.
    ///
    /// The key lets the spawn paths call [`Self::serve_term_session`] freely
    /// without stacking listeners on one path. A second bind of a live socket
    /// replaces the file and orphans the children already connected through it.
    /// Dropping a task stops its server, which removes its socket file.
    pub(crate) agent_servers: std::collections::HashMap<WorkspaceUid, stoat_scheduler::Task<()>>,
    /// Landing slot for a finished direnv load, drained by
    /// [`crate::project_env::install_pending`] in [`Self::drive_background`].
    /// Shared rather than returned because the load runs detached on
    /// [`Self::executor`] and cannot borrow `self`.
    pub(crate) pending_env: Arc<std::sync::Mutex<Option<crate::project_env::PendingEnvLoad>>>,
    /// Landing slot for a finished `--continue` session restore, drained by
    /// [`Self::install_pending_workspace_restore`] in [`Self::drive_background`].
    /// Shared rather than returned because the restore runs detached on
    /// [`Self::executor`] and cannot borrow `self`, like [`Self::pending_env`].
    pub(crate) pending_workspace_restore: Arc<std::sync::Mutex<Option<PendingWorkspaceRestore>>>,
    /// In-flight session-state writes, one slot per workspace.
    ///
    /// Holding the task is what keeps the write scheduled. Keying by workspace
    /// makes a second save of the same workspace replace the first, so a run of
    /// switches leaves one write per workspace rather than a queue of them.
    pub(crate) pending_workspace_saves:
        std::collections::HashMap<WorkspaceId, stoat_scheduler::Task<()>>,
    /// The open autosave window, armed by user input and cleared when its
    /// drain saves. `Some` means a save is already scheduled, which is what
    /// [`debounce::arm_workspace_autosave`] reads to leave the window alone
    /// rather than pushing it back.
    pub(crate) pending_workspace_autosave: Option<stoat_scheduler::Task<()>>,
    /// When the grid's geometry counts as settled after a resize, and the timer
    /// that wakes the run loop at that moment.
    ///
    /// A scroll pool renders no page for a rectangle that moves before the
    /// deadline, since the next resize event would drop it, and the settled
    /// geometry raises no event of its own to fill it. Armed by
    /// [`debounce::arm_pool_settle`], whose replacement cancels the prior timer.
    pub(crate) pool_settle: Option<(std::time::Instant, stoat_scheduler::Task<()>)>,
    /// System-clipboard writes route through this trait. Defaults to
    /// [`NoopClipboard`] so headless or display-less environments do
    /// not error on the first clipboard event; tests install
    /// [`crate::host::FakeClipboard`] to assert on writes.
    pub(crate) clipboard_host: Arc<dyn crate::host::ClipboardHost>,
    /// Cache of pre-computed review hunks keyed by content hash plus
    /// language. Populated when the editor itself runs
    /// [`crate::review::extract_review_hunks_changeset`]; consulted by
    /// the viewport-socket diff RPC handler so a `stoat diff` CLI
    /// invocation can reuse already-computed work instead of running
    /// the structural diff twice.
    pub(crate) diff_cache: Arc<std::sync::Mutex<crate::diff_cache::DiffCache>>,
    /// Memoized tree-sitter parses of git-base texts, so the diff view's
    /// syntax-highlighted left column parses each base once across edits.
    pub(crate) base_highlights_cache: crate::workspace::diff::BaseHighlightCache,
    /// Tracks `$/progress` notifications so the status bar can show
    /// the freshest in-progress operation. Drained from
    /// [`crate::host::LspHost::try_recv_notification`] inside
    /// [`Stoat::update`].
    pub(crate) lsp_progress: crate::lsp::progress::LspProgressMap,
    /// The status bar's server list for the focused buffer, held across frames
    /// so a steady frame refreshes the busy flags rather than re-deriving the
    /// names. Transient render state, not persisted.
    pub(crate) lsp_server_list: crate::render::LspServerList,
    /// Freshest `window/showMessage` text from the language server,
    /// shown in the status line until the next key press. Set by
    /// [`Self::drain_lsp_notifications`] and cleared at the top of
    /// [`Self::handle_key`]. `MessageType::ERROR` renders as a wrapped popout
    /// card above the status bar. Other levels paint in the bar itself.
    pub(crate) lsp_message: Option<(lsp_types::MessageType, String)>,
    /// In-flight goto-style LSP request, paired with the user-facing
    /// label of the jump kind ("definition", "references", ...) so the
    /// pump can name it in a zero-result message. Replacing the entry
    /// drops the prior task, cancelling its spawned future before the
    /// response can land. Polled by [`action_handlers::lsp::pump_lsp_jumps`]
    /// at the top of each render tick. `Ready(Some)` opens the target
    /// file in the focused pane (when cross-file) and jumps the primary
    /// cursor. A zero-result `Ready` reports "lsp: no {label} found" in
    /// the status bar instead of dropping silently.
    pub(crate) pending_lsp_jump: Option<(
        &'static str,
        stoat_scheduler::Task<Vec<crate::location_picker::LocationEntry>>,
    )>,

    /// In-flight `textDocument/hover` request. Replacing the entry
    /// drops the prior task, cancelling its spawned future before the
    /// response can land. Polled by
    /// [`action_handlers::pump_lsp_hover`] at the top of each render
    /// tick, which resolves the [`HoverOutcome`] into a popup or an
    /// honest status message.
    pub(crate) pending_hover_request:
        Option<stoat_scheduler::Task<crate::lsp::hover::HoverOutcome>>,

    /// Hover popup content waiting to be painted. Set by
    /// [`action_handlers::pump_lsp_hover`] when a hover response lands.
    ///
    /// In normal or select mode the next key press closes it (the auto-close
    /// intercept in [`Self::handle_key`]): Escape and Ctrl-c are consumed by the
    /// close, every other key closes it and then dispatches. Any non-Hover action
    /// also clears it, so the popup vanishes on cursor motion.
    ///
    /// The mouse follows the same rule. A press outside the popup's rect closes
    /// it and then falls through, so the click still lands where it was aimed.
    pub(crate) pending_hover: Option<crate::render::hover::HoverPopup>,

    /// In-flight `textDocument/signatureHelp` request, armed by
    /// [`crate::lsp::signature_help::signature_help_trigger`] on a trigger
    /// character and polled by
    /// [`crate::lsp::signature_help::pump_lsp_signature_help`].
    pub(crate) pending_signature_help_request:
        Option<stoat_scheduler::Task<Option<crate::lsp::signature_help::SignatureHelpPopup>>>,

    /// Single-slot debounce window for the signature-help request.
    ///
    /// Typing an argument list emits a trigger character per argument, and each
    /// one both flushes the didChange window and asks the server. Re-arming
    /// replaces the task, cancelling the prior timer, so a burst costs one of
    /// each.
    pub(crate) pending_signature_help_timer: Option<stoat_scheduler::Task<()>>,
    /// Channel the debounce task pushes onto once its timer fires, drained by
    /// [`debounce::drain_pending_signature_help`].
    pub(crate) signature_help_tx: Sender<()>,
    pub(crate) signature_help_rx: Receiver<()>,

    /// Signature-help popup content waiting to be painted. Cleared when the
    /// editor leaves insert mode or the completion popup opens.
    pub(crate) pending_signature_help: Option<crate::lsp::signature_help::SignatureHelpPopup>,

    /// `(buffer, version)` the signature-help trigger last acted on, so a
    /// cursor-only tick does not re-request. Mirrors [`Self::last_completion_signature`].
    pub(crate) last_signature_help_key: Option<(BufferId, u64)>,

    /// In-flight `textDocument/codeAction` request. Replacing the
    /// entry drops the prior task, cancelling its spawned future.
    /// Polled by [`action_handlers::lsp::pump_lsp_code_actions`] each
    /// render tick; on `Ready(Some)` populates
    /// [`Self::pending_code_action_picker`].
    pub(crate) pending_code_action_request:
        Option<stoat_scheduler::Task<Option<Vec<lsp_types::CodeActionOrCommand>>>>,

    /// Selectable code-action picker waiting for the user to choose
    /// (number keys 1-9) or cancel (Escape / any other action).
    pub(crate) pending_code_action_picker: Option<action_handlers::lsp::CodeActionPicker>,

    /// In-flight `codeAction/resolve` request triggered after the
    /// user picks an unresolved code action. Polled by
    /// [`action_handlers::lsp::pump_lsp_code_action_resolve`]; on
    /// `Ready(Some(edit))` the edit is applied via
    /// [`crate::lsp::edit_apply::apply_workspace_edit`].
    pub(crate) pending_code_action_resolve: StampedPending<Option<lsp_types::WorkspaceEdit>>,

    /// In-flight `textDocument/prepareRename` request. On response,
    /// [`action_handlers::lsp::pump_lsp_prepare_rename`] opens
    /// [`Self::rename_input`] seeded with the symbol placeholder.
    pub(crate) pending_prepare_rename:
        Option<stoat_scheduler::Task<Option<action_handlers::lsp::RenamePrep>>>,

    /// One-line input modal for entering a new symbol name. Created
    /// by the prepare-rename pump after a successful prepare response;
    /// consumed by `rename_input_submit` (Enter) which fires the
    /// rename request, or `rename_input_cancel` (Escape) which discards.
    pub(crate) rename_input: Option<action_handlers::lsp::RenameInputState>,

    /// In-flight `textDocument/rename` request issued after the user
    /// submits the rename input. Polled by
    /// [`action_handlers::lsp::pump_lsp_rename`]; on `Ready(Some(edit))`
    /// the edit is applied via
    /// [`crate::lsp::edit_apply::apply_workspace_edit`].
    pub(crate) pending_rename: StampedPending<Option<lsp_types::WorkspaceEdit>>,

    /// In-flight `textDocument/documentSymbol` request. Polled by
    /// [`action_handlers::lsp::pump_lsp_symbol_picker`], which installs the
    /// entries into [`Self::symbol_finder`] on response.
    pub(crate) pending_symbol_picker_request:
        Option<stoat_scheduler::Task<Vec<crate::symbol_finder::SymbolFinderEntry>>>,

    /// Selectable code-graph navigation picker waiting for the user to
    /// choose a symbol to jump to (number keys 1-9) or cancel.
    pub(crate) pending_symbol_picker: Option<crate::symbol_finder::SymbolPicker>,

    /// In-flight `workspace/symbol` request for the [`Self::symbol_finder`]
    /// modal's workspace scope, re-issued as the query changes. Polled by
    /// [`action_handlers::lsp::pump_lsp_workspace_symbol`], which installs the
    /// merged entries into the finder.
    pub(crate) pending_workspace_symbol_request:
        Option<stoat_scheduler::Task<Vec<action_handlers::lsp::WorkspaceSymbolEntry>>>,

    /// In-flight `textDocument/rangeFormatting` request triggered by
    /// `FormatSelections`. Polled by
    /// [`action_handlers::lsp::pump_lsp_format`]; on `Ready(Some)`
    /// the returned text edits are applied via
    /// [`crate::lsp::edit_apply::apply_workspace_edit`].
    pub(crate) pending_format_request: StampedPending<Option<action_handlers::lsp::FormatResponse>>,
    /// In-flight format-on-save task. Set when a save with `format_on_save`
    /// enabled arms a formatting request bounded by a save-time budget;
    /// [`action_handlers::file::pump_format_on_save`] applies any edits and
    /// writes the buffer. While `Some`, further saves of that buffer are
    /// ignored so a burst does not queue duplicate writes.
    pub(crate) pending_format_on_save:
        Option<stoat_scheduler::Task<action_handlers::file::FormatOnSaveOutcome>>,
    /// In-flight write of a buffer to disk. The bytes stream from a clone of
    /// the rope on a blocking thread, so a slow disk costs the run loop
    /// nothing; [`action_handlers::file::pump_pending_save`] lands the outcome.
    /// While `Some`, further saves are dropped so a burst does not queue
    /// duplicate writes.
    pub(crate) pending_save: Option<action_handlers::file::PendingSave>,
    /// The pane a `:wq` ([`action_handlers::file::write_quit`]) was pressed in,
    /// with its workspace, while the write it started is on its way.
    ///
    /// The pump that lands the write takes it and closes that pane, and sets
    /// [`Self::quit_requested`] only when the pane is the last. A failed write
    /// or a pane that is gone drops the quit and leaves the buffer for the user.
    pub(crate) quit_after_save: Option<(WorkspaceId, PaneId)>,
    /// Set once a `:wq`-driven write has landed and the pane it closes is the
    /// last. The run loop takes it right after [`Self::drive_background`] and
    /// quits, so a quit deferred behind a write happens on the frame it
    /// completes.
    pub(crate) quit_requested: bool,

    /// Editor autocomplete popup waiting to be painted. Set by the
    /// trigger pipeline (item 83) when a completion request resolves;
    /// cleared by `Esc` in insert mode, by motion that leaves the
    /// popup's `prefix_range`, or by acceptance.
    pub(crate) pending_completion: Option<crate::completion::CompletionPopup>,

    /// Monotonic counter bumped each time a popup is installed into
    /// [`Self::pending_completion`], so the pooled list region detects a re-query
    /// by comparing one `u64` rather than hashing every label each emit.
    ///
    /// Installing is the only site that bumps it, so it always matches the shown
    /// popup and can serve as that popup's pool content version.
    pub(crate) completion_generation: u64,

    /// In-flight debounced completion request. Replacing the entry
    /// drops the prior task, cancelling its spawned future before its
    /// debounce timer or downstream LSP request can land. Polled by
    /// [`crate::completion::request::pump`] each render tick, which resolves
    /// its outcome against [`Self::pending_completion`].
    pub(crate) pending_completion_request:
        Option<stoat_scheduler::Task<crate::completion::request::RequestOutcome>>,

    /// The question an in-flight narrow was armed with.
    ///
    /// Held so a keystroke that must not wait for the pool answers it here
    /// instead. `None` whenever no narrow is armed.
    pub(crate) pending_narrow: Option<crate::completion::request::NarrowRequest>,

    /// In-flight debounced `completionItem/resolve` for the popup's
    /// selected row. Replacing the entry drops the prior task, so
    /// navigating past a row cancels its resolve. Polled by
    /// [`action_handlers::completion::pump_completion_resolve`], which
    /// patches the resolved detail/documentation back into
    /// [`Self::pending_completion`].
    pub(crate) pending_completion_resolve:
        Option<stoat_scheduler::Task<Option<action_handlers::completion::ResolvedCompletion>>>,

    /// In-flight `completionItem/resolve` fired when an LSP completion is
    /// accepted, resolving its `additionalTextEdits` (imports) under a
    /// 300ms timeout. Polled by
    /// [`crate::completion::accept::pump_completion_accept`], which
    /// applies the resolved edits to the captured buffer.
    pub(crate) pending_completion_accept:
        StampedPending<Option<crate::completion::accept::AcceptedImports>>,

    /// Buffer signature `(BufferId, version)` recorded at the most
    /// recent completion-trigger call. The trigger pipeline returns
    /// early when this matches the focused buffer's current
    /// signature so a no-op event tick (Esc-dismiss, cursor-only
    /// motion) does not re-arm the request that was just dismissed.
    /// Cleared whenever insert mode exits, so re-entering insert
    /// starts from a clean slate.
    pub(crate) last_completion_signature: Option<(BufferId, u64)>,

    /// The cursor context the completion trigger last computed, keyed by the
    /// `(BufferId, version, cursor offset)` it was computed at.
    ///
    /// Signature help triggers on the same event and asks the same question, so
    /// it reads this rather than walking the rope a second time per keystroke.
    /// Transient, not persisted.
    pub(crate) completion_context: Option<(
        (BufferId, u64, usize),
        crate::completion::request::ContextOwned,
    )>,

    /// In-flight snippet expansion. Populated by
    /// [`crate::completion::accept::execute`] when accepting a
    /// snippet completion item; consumed by
    /// [`crate::completion::snippet::advance`] from the Tab
    /// arbitration arm in `handle_insert_key`. Cleared when insert
    /// mode exits so re-entering insert is not stuck mid-snippet.
    pub(crate) active_snippet: Option<crate::completion::snippet::ActiveSnippet>,

    /// stoat's own `<semver> (<hash>[-dirty] <date>)` version string, shown by
    /// the `ShowVersion` action. Injected by the binary via
    /// [`Self::set_version_info`]. Defaults to "unknown" so tests are
    /// deterministic without a build stamp.
    pub(crate) version_info: &'static str,
    /// Next aux-window id handed out when a pane detaches. Ids are per-process
    /// and monotonic, shared across workspaces, so a window id never aliases a
    /// pane detached from a different workspace.
    pub(crate) next_aux_window: u32,
    /// Ordered, non-dropping channel carrying stoatty APC byte batches from
    /// the app loop to the UI thread, written to stdout right after each
    /// rendered frame. Separate from the latest-wins render watch because
    /// `fill` page content must not be coalesced or dropped. `None` until
    /// [`Self::set_apc_tx`] installs it, which startup does after construction
    /// and a test need not do at all.
    pub(crate) apc_tx: Option<UnboundedSender<Vec<u8>>>,
    /// The remote `:ssh` or `:mosh` session that owns the screen, or `None`
    /// while this process draws it. Set while the window belongs to a remote
    /// stoat, which is what holds back every frame, scene, image, window, pool,
    /// and minimap batch for the duration.
    pub(crate) passthrough: Option<ssh::Passthrough>,
    /// The app's ends of the passthrough plumbing, installed by the bin layer
    /// after construction. `None` in a headless or embedded run, where `:ssh`
    /// and `:mosh` refuse because there is no terminal to hand over.
    pub(crate) passthrough_link: Option<ssh::PassthroughLink>,
    /// Whether a stoatty answered the startup ident handshake.
    ///
    /// False until [`Self::handle_stoatty_present`] hears otherwise, and that
    /// default carries weight. The rich protocol is only safe to emit once a
    /// listener is confirmed, because a foreign terminal prints the parts of it
    /// that are not APC-wrapped rather than dropping them. No frame may assume
    /// rich output before this is set.
    pub(crate) stoatty: bool,
    /// The protocol version the stoatty on the other end announced, or zero
    /// while none has, which is also what a stoatty predating the version field
    /// reports.
    ///
    /// Read before emitting anything a peer might not understand. Meaningless
    /// unless [`Self::stoatty`] is set, since a foreign terminal answers no
    /// handshake at all.
    pub(crate) stoatty_protocol: u32,
    /// Walkthroughs opened this session, which distinguishes one run's mark ids
    /// from the last one's.
    ///
    /// A tour opened while the previous one's strokes are still settling would
    /// otherwise reuse its ids, and the terminal would read the new marks as
    /// the old ones mid-draw.
    pub(crate) walkthrough_runs_opened: u32,
    /// Reused per-frame APC decoration buffer. Widgets append their component
    /// frames while painting; [`Self::emit_apc_scene`] diffs it against the last
    /// flush so unchanged decoration costs no bytes. Empty until a widget appends.
    pub(crate) apc_scene: ApcScene,
    /// Diagnostic underline spans collected during the current paint.
    ///
    /// The editor renderer fills this while painting under stoatty, and
    /// [`Self::paint_into`] turns it into the curly-underline VT re-stamp carried
    /// on the frame. Reused across frames like [`Self::apc_scene`], down to each
    /// span's cell record, so a steady frame allocates nothing here.
    pub(crate) pending_undercurls: UndercurlBatch,
    /// Counter bumped every time the active theme changes.
    ///
    /// Every pooled surface paints theme colors, so a page buffered in the
    /// terminal goes stale the moment the theme does. The pool content versions
    /// hash this in, which is what makes a `:theme` switch refill them instead of
    /// gliding old-theme pixels back onto the screen.
    pub(crate) theme_epoch: u64,
    /// Counts the changes to what a pane paints from outside its display map.
    ///
    /// A pane's own content is answered by
    /// [`DisplaySnapshot::paint_version`](crate::display_map::DisplaySnapshot::paint_version),
    /// but the theme, the settings the renderer reads straight off this struct,
    /// and the search query all reach the screen without passing through any
    /// display layer. A cache keyed on the snapshot alone would hold a pane
    /// still through a theme switch.
    ///
    /// Distinct from [`Self::theme_epoch`], which answers the narrower question
    /// of whether the theme itself moved and is hashed into the pooled page
    /// versions.
    pub(crate) paint_generation: u64,
    /// Each unfocused editor pane's last paint, replayed while its key holds.
    ///
    /// Keyed by pane so a split's panes cache independently, and by workspace
    /// because a pane key comes from a per-workspace map: two workspaces can
    /// name one key, and a split at the same rect in each would replay the
    /// other's cells.
    ///
    /// Swept after every paint down to the panes the active workspace holds. A
    /// pane id never returns, so an entry left behind would hold one pane's
    /// cells for the process. A switch back to another workspace repaints its
    /// unfocused panes once.
    pub(crate) pane_cache: std::collections::HashMap<(WorkspaceId, PaneId), PaneCacheEntry>,
    /// How many panes this session has actually painted, as opposed to
    /// replayed.
    ///
    /// The whole point of the cache is a paint that does not happen, which
    /// leaves no trace in the frame it skipped. Counting is what lets a test
    /// say the skip occurred rather than that the output happened to match.
    pub(crate) pane_paints: u64,
    /// How many key presses derived a keymap lookup.
    ///
    /// Whether a press consults the keymap is otherwise invisible, since the
    /// derivation only costs time. Counting it is what lets a test say the
    /// busiest keys still skip it.
    #[cfg(test)]
    pub(crate) keymap_lookups: std::cell::Cell<u64>,
    /// How many times an editor's selection set was copied for an undo group.
    ///
    /// A copy nobody reads costs only time, so a test needs the count to say
    /// that an action which edits nothing stops paying for one.
    #[cfg(test)]
    pub(crate) selection_snapshots: std::cell::Cell<u64>,
    /// How many times the focused mode was resolved.
    ///
    /// Resolving walks the modal stack and clones a pane-tree view, and costs
    /// nothing else, so a test needs the count to say the key guards ask once
    /// rather than once each.
    #[cfg(test)]
    pub(crate) focused_mode_reads: std::cell::Cell<u64>,
    /// How many times the completion popup's geometry was computed.
    ///
    /// Computing it locks the focused buffer to read the match prefix and
    /// measures every visible label, and the frame has two consumers, so a test
    /// needs the count to say the frame lays out once rather than once each.
    #[cfg(test)]
    pub(crate) completion_layouts: std::cell::Cell<u64>,
    /// The editor chrome resolved from [`Self::theme`], rebuilt by
    /// [`Self::refresh_chrome`] when the theme has been replaced.
    ///
    /// Keyed on the theme's identity rather than [`Self::theme_epoch`], because
    /// the epoch tracks pooled-surface staleness rather than the theme itself
    /// and a config reload replaces the theme without bumping it.
    pub(crate) chrome: Option<(
        Arc<crate::theme::Theme>,
        crate::render::editor::ResolvedChrome,
    )>,
    /// [`Self::minimap_class_table`]'s palette pre-blended toward the editor
    /// background at the inactive dim, for the strips of unfocused panes.
    ///
    /// Every unfocused strip blends this identically, so the blend is kept
    /// rather than redone per frame. `None` means no dim applies or the
    /// background did not resolve, and those strips paint undimmed.
    ///
    /// Keyed on the theme's identity and the dim, the way [`Self::chrome`] is
    /// keyed. That covers the class table too, since a config install replaces
    /// [`Self::theme`] and [`Self::minimap_class_table`] together.
    pub(crate) dimmed_minimap_palette: Option<(Arc<crate::theme::Theme>, f32, Vec<[u8; 3]>)>,
    /// Smooth-scroll pool emit state for the focused editor. Tracks the
    /// last-declared pool region, filled page window, and emitted scroll row
    /// so each frame emits only the deltas.
    pub(crate) smooth_scroll: SmoothScrollState,
    /// Per-line minimap summaries for the strips declared this session, keyed by
    /// `(workspace, buffer)` so a buffer id reused across workspaces never
    /// aliases another workspace's content.
    ///
    /// [`Self::emit_minimap`] syncs each entry from its buffer's edits at the
    /// frame seam and drains the resulting splices into `minimap_lines`.
    pub(crate) minimap_content:
        std::collections::HashMap<(WorkspaceId, BufferId), crate::minimap::MinimapContent>,
    /// Monotonic source of the `content_id`s naming minimap content stores on the
    /// terminal, global so ids stay unique across workspaces.
    pub(crate) minimap_next_content_id: u32,
    /// Whether any visible strip's chunked build has lines left to summarize,
    /// recomputed by [`Self::emit_minimap`]. Keeps the run loop's frame timer
    /// firing so idle frames drive the build to completion.
    pub(crate) minimap_build_pending: bool,
    /// Whether a visible run or terminal fed output since the last frame tick, so
    /// the tick repaints once rather than the output arm repainting per PTY chunk.
    /// A terminal that changed its title sets it too, visible or not.
    pub(crate) pty_dirty: bool,
    /// Wall-clock seconds an in-flight LSP work-done spinner has animated, mapped
    /// to a [`SPINNER_FRAMES`] glyph by [`spinner_phase`]. Advanced by the frame
    /// tick while progress is live and reset to zero when it ends, so each fresh
    /// progress starts at frame zero.
    pub(crate) spinner_clock: f32,
    /// Syntax-scope palette the minimap strips declare and their run summaries
    /// index, resolved from [`Self::theme`].
    pub(crate) minimap_class_table: crate::minimap::ClassTable,
}

impl Stoat {
    #[cfg(test)]
    pub fn test() -> crate::test_harness::TestHarness {
        crate::test_harness::TestHarness::default()
    }

    #[cfg(test)]
    pub(crate) fn active_keys_for_mode(
        &self,
        mode: &str,
    ) -> Vec<(&keymap::CompiledKey, &[ResolvedAction])> {
        let state = StoatKeymapState::new(mode);
        self.keymap.active_keys(&state)
    }

    pub(crate) fn active_bindings_for_current_mode(&self) -> Vec<(String, Vec<ResolvedAction>)> {
        let state = StoatKeymapState::from_stoat(self);
        self.keymap
            .active_bindings(&state)
            .into_iter()
            .map(|(label, actions)| (label, actions.to_vec()))
            .collect()
    }

    pub fn new(executor: Executor, cli_settings: Settings, initial_git_root: PathBuf) -> Self {
        Self::new_with_user_config(
            executor,
            cli_settings,
            initial_git_root,
            None,
            Vec::new(),
            None,
        )
    }

    /// Construct a [`Stoat`], layering `user_config` over the embedded default
    /// when it parses clean.
    ///
    /// `user_config` is the raw text of the user's `config.stcfg` (located via
    /// [`user_config_path`](crate::user_config_path)), or [`None`] to use only the
    /// built-in default. A user source that parses without errors ranks ahead of
    /// the embedded config, which still supplies every key and setting the user
    /// never mentions. One that fails to parse is discarded in favour of the
    /// embedded default, logged, and surfaced as a transient status message.
    /// CLI settings layer over the resolved config either way.
    ///
    /// `user_themes` are `(stem, JSON)` pairs of VSCode color themes from the
    /// user's theme dir. They join the pool after the built-in themes and before
    /// the user config's own `theme` blocks. One that fails to parse is skipped
    /// and surfaced in the same transient status.
    ///
    /// `env_theme` is the theme named by the environment (stoatty exports its
    /// own theme as `STOAT_THEME` so a child stoat matches the terminal). It
    /// applies only when neither `cli_settings` nor the user config names a
    /// theme, so an explicit choice always outranks the inherited one. A name
    /// matching no theme block is ignored with a warning, leaving the default
    /// theme in place, since the environment is inherited rather than chosen.
    pub fn new_with_user_config(
        executor: Executor,
        cli_settings: Settings,
        initial_git_root: PathBuf,
        user_config: Option<String>,
        user_themes: Vec<(String, String)>,
        env_theme: Option<String>,
    ) -> Self {
        let (config, config_error) = match user_config {
            Some(source) => {
                let (parsed, errors) = stoat_config::parse(&source);
                if errors.is_empty() {
                    (parsed, None)
                } else {
                    tracing::error!(
                        "user config parse failed; using built-in defaults: {}",
                        stoat_config::format_errors(&source, &errors)
                    );
                    (
                        None,
                        Some("user config parse failed; using built-in defaults".to_string()),
                    )
                }
            },
            None => (None, None),
        };
        let embedded = embedded_config();

        // Retaining the sources lets a mid-session reload rebuild the identical
        // pool without re-reading the theme directory, and lets a theme already
        // converted stay converted across the reload.
        let imported_themes: Vec<Arc<VscodeSource>> = {
            vscode_theme::builtin_sources()
                .into_iter()
                .chain(user_themes)
                .map(|(stem, source)| Arc::new(VscodeSource::new(stem, source)))
                .collect()
        };

        let ConfigArtifacts {
            keymap,
            unknown_actions,
            settings,
            theme,
            theme_pool,
            syntax_styles,
            minimap_class_table,
        } = build_config_artifacts(
            config,
            embedded,
            &imported_themes,
            cli_settings.clone(),
            env_theme,
        );

        let highlight_retention = settings
            .highlight_retention
            .unwrap_or(DEFAULT_HIGHLIGHT_RETENTION);
        tracing::info!(
            target: "stoat::app",
            highlight_retention,
            configured = settings.highlight_retention.is_some(),
            "highlight retention: caching syntax trees and token sets for hidden buffers"
        );

        let language_registry = Arc::new(LanguageRegistry::standard());
        install_highlight_maps(&language_registry, &syntax_styles);

        // Built before the first workspace, since an editor needs it at
        // construction to wake the run loop when its background rewrap settles.
        let redraw_notify = Arc::new(tokio::sync::Notify::new());
        let drain_notify = Arc::new(tokio::sync::Notify::new());

        let mut workspaces = SlotMap::with_key();
        let workspace = Workspace::new(initial_git_root.clone(), &executor, redraw_notify.clone());
        let active_workspace = workspaces.insert(workspace);
        workspaces[active_workspace].id = active_workspace;

        let (pty_tx, pty_rx) = tokio::sync::mpsc::channel(256);
        let (agent_event_tx, agent_event_rx) = tokio::sync::mpsc::channel(256);
        let (agent_control_tx, agent_control_rx) = tokio::sync::mpsc::channel(256);
        let (index_update_tx, index_update_rx) = tokio::sync::mpsc::unbounded_channel();
        let (window_ipc_tx, window_ipc_rx) = tokio::sync::mpsc::unbounded_channel();
        let (diff_refresh_tx, diff_refresh_rx) = tokio::sync::mpsc::channel(256);
        let (signature_help_tx, signature_help_rx) = tokio::sync::mpsc::channel(256);
        let (workspace_autosave_tx, workspace_autosave_rx) = tokio::sync::mpsc::channel(256);
        let (code_search_query_tx, code_search_query_rx) = tokio::sync::mpsc::channel(256);
        let (index_external_edit_tx, index_external_edit_rx) = tokio::sync::mpsc::channel(256);
        let (follow_tx, follow_rx) = tokio::sync::mpsc::channel(256);
        let (live_reload_tx, live_reload_rx) = tokio::sync::mpsc::channel(256);
        let (auto_reload_tx, auto_reload_rx) = tokio::sync::mpsc::channel(1);
        // Dropped at once, leaving the channel closed until `set_stoatty_rx`
        // installs the UI thread's end. Closed is the truthful state for a
        // process that has no UI thread to hear from.
        let (_, stoatty_rx) = tokio::sync::mpsc::unbounded_channel::<Option<u32>>();
        let (_, attached_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (_, cell_pixels_rx) = tokio::sync::mpsc::unbounded_channel::<Option<(u16, u16)>>();

        let env_host: Arc<dyn EnvHost> = Arc::new(LocalEnv);
        let home = env_host.var("HOME").map(PathBuf::from);

        let mut stoat = Self {
            size: Rect::default(),
            fallback_mode: "normal".into(),
            frame_mode: String::new(),
            user_vars: std::collections::HashMap::new(),
            executor,
            keymap,
            settings,
            cli_settings,
            theme: Arc::new(theme),
            theme_pool,
            imported_themes,
            modal_zoom: std::collections::BTreeMap::new(),
            diff_soften: 0,
            diff_tint: 0,
            diff_syntax: true,
            diff_bold: false,
            diff_underline: false,
            modal_split: std::collections::BTreeMap::new(),
            commits_split: None,
            command_palette: None,
            help: None,
            file_finder: None,
            finder_path_cache: None,
            finder_path_epoch: 0,
            symbol_finder: None,
            workspace_picker: None,
            quit_all_confirm: None,
            jumplist_picker: None,
            diagnostics_picker: None,
            commit_picker: None,
            location_picker: None,
            last_picker_action: None,
            code_search: None,
            split_selection_input: None,
            filter_selections_input: None,
            macro_recording: None,
            pending_macro_replay: None,
            shell_input: None,
            shell_host: Arc::new(crate::host::LocalShell),
            terminal_host: Arc::new(crate::host::LocalTerminalHost),
            persistence_disabled: false,
            language_registry,
            syntax_styles,
            workspaces,
            active_workspace,
            badges: BadgeTray::new(),
            pty_tx,
            pty_rx,
            agent_event_tx,
            agent_event_rx,
            agent_control_tx,
            agent_control_rx,
            index_update_tx,
            index_update_rx,
            window_ipc_tx,
            window_ipc_rx,
            stoatty_rx,
            attached_rx,
            terminal_reported: false,
            remote_pending: false,
            cell_pixels: None,
            images: crate::image_emit::ImageRuntime::default(),
            cell_pixels_rx,
            window_ipc_connected: false,
            zoom_claimed: false,
            aux_windows: std::collections::BTreeMap::new(),
            aux_cursor: None,
            pool_cursor_holder: None,
            _index_build_task: None,
            redraw_notify,
            drain_notify,
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            #[cfg(feature = "perf")]
            perf: crate::perf::PerfStats::default(),
            pending_changed_file_jump: None,
            pending_conflict_file: None,
            git_jobs: GitJobs::default(),
            pending_diff_nav_jump: None,
            pending_code_search: None,
            code_search_debounce: None,
            code_search_query_tx,
            code_search_query_rx,
            pending_diff_warm: None,
            modal_run: None,
            syntax_highlight: true,
            minimap_override: None,
            tab_bar_override: None,
            tab_bar_spans: Vec::new(),
            single_minimap_rect: None,
            lsp_badge_rect: None,
            lsp_status_pinned: false,
            lsp_badge_hovered: false,
            key_hints_visible: false,
            hints_cache: None,
            inlay_hints_enabled: false,
            pending_inlay_hint_request: Pending::default(),
            last_inlay_hint_key: None,
            pending_document_highlight_request: Pending::default(),
            last_document_highlight_key: None,
            pull_diagnostic_result_ids: std::collections::HashMap::new(),
            pending_pull_diagnostics: std::collections::HashMap::new(),
            last_pull_diagnostic_key: std::collections::HashMap::new(),
            pending_semantic_tokens: Pending::default(),
            last_semantic_tokens_key: None,
            pending_folding_ranges: Pending::default(),
            last_folding_range_key: None,
            render_tick: 0,
            completion_layout: None,
            pending_message: None,
            pending_message_deadline: None,
            pending_message_expiry: None,
            walkthrough_exit_timer: None,
            wheel_binding_last: None,
            wheel_line_remainder: 0.0,
            diff_wheel_travel: 0.0,
            diff_wheel_last: None,
            diff_wheel_walk: true,
            pending_count: None,
            pending_find: None,
            pending_mark: None,
            marks: std::collections::HashMap::new(),
            global_marks: std::collections::HashMap::new(),
            pending_goto_word: None,
            pending_goto_word_input: String::new(),
            pending_goto_word_extend: None,
            pending_replace: false,
            pending_surround_add: false,
            pending_surround_replace: action_handlers::surround::SurroundReplaceStage::Idle,
            pending_surround_delete: false,
            pending_surround_count: 1,
            pending_textobject_select: None,
            search_input: None,
            last_search: None,
            current_insert_run: None,
            restore_cursor: false,
            group_held_for_insert: false,
            auto_indent_cursors: Vec::new(),
            registers: register::RegisterStore::new(),
            pending_register_select: false,
            selected_register: None,
            command_register: None,
            pending_insert_register: false,
            replaying_registers: Vec::new(),
            editor_drag: None,
            terminal_drag: None,
            hover_cell: None,
            hover_diag: None,
            divider_drag: None,
            modal_separator_drag: None,
            commits_separator_drag: false,
            minimap_drag: None,
            lsp_opened: std::collections::HashSet::new(),
            lsp_drain_hosts: Vec::new(),
            lsp_drain_buffers: Vec::new(),
            lsp_buffer_versions: std::collections::HashMap::new(),
            lsp_pending_changes: std::collections::HashMap::new(),
            auto_reload_poll: None,
            auto_reload_tx,
            auto_reload_rx,
            lsp_doc_versions: std::collections::HashMap::new(),
            lsp_last_delivered_text: Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            lsp_last_delivered_buffer_version: Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            diagnostics: crate::diagnostics::DiagnosticSet::new(),
            last_motion: None,
            fs_host: Arc::new(LocalFs),
            fs_watch_host: Arc::new(NoopFsWatcher::new()),
            watched_roots: std::collections::HashSet::new(),
            fs_watch_backlog: std::collections::VecDeque::new(),
            pending_diff_refresh: None,
            diff_refresh_tx,
            diff_refresh_rx,
            workspace_autosave_tx,
            workspace_autosave_rx,
            pending_file_opens: Vec::new(),
            pending_auto_reloads: Vec::new(),
            index_pending_external_edits: std::collections::HashSet::new(),
            index_external_edit_timer: None,
            ignored_dir_cache: std::collections::HashMap::new(),
            index_external_edit_tx,
            index_external_edit_rx,
            follow_pending: None,
            follow_timer: None,
            follow_tx,
            follow_rx,
            live_reload_pending: std::collections::HashSet::new(),
            live_reload_timer: None,
            live_reload_tx,
            live_reload_rx,
            git_host: Arc::new(LocalGit::new()),
            env_host,
            home,
            lsp_registry: crate::lsp::registry::LspRegistry::new(),
            lsp_auto_spawn: false,
            lsp_spawn_failed: None,
            lsp_spawn_deferred: None,
            pending_lsp_host: Arc::new(std::sync::Mutex::new(Vec::new())),
            env_auto_load: false,
            diff_warm_auto: false,
            follow_changes: false,
            live_reload: false,
            agent_socket_dir: None,
            serve_agent_sockets: false,
            session_log: None,
            agent_servers: std::collections::HashMap::new(),
            pending_env: Arc::new(std::sync::Mutex::new(None)),
            pending_workspace_restore: Arc::new(std::sync::Mutex::new(None)),
            pending_workspace_saves: std::collections::HashMap::new(),
            pending_workspace_autosave: None,
            pool_settle: None,
            clipboard_host: Arc::new(crate::host::NoopClipboard),
            diff_cache: Arc::new(std::sync::Mutex::new(crate::diff_cache::DiffCache::new(
                256,
            ))),
            base_highlights_cache: Arc::new(std::sync::Mutex::new(
                crate::workspace::diff::BaseHighlightMemo::default(),
            )),
            lsp_progress: crate::lsp::progress::LspProgressMap::new(),
            lsp_server_list: crate::render::LspServerList::default(),
            lsp_message: None,
            pending_lsp_jump: None,
            pending_hover_request: None,
            pending_hover: None,
            pending_signature_help_request: None,
            pending_signature_help_timer: None,
            signature_help_tx,
            signature_help_rx,
            pending_signature_help: None,
            last_signature_help_key: None,
            pending_code_action_request: None,
            pending_code_action_picker: None,
            pending_code_action_resolve: StampedPending::default(),
            pending_prepare_rename: None,
            rename_input: None,
            pending_rename: StampedPending::default(),
            pending_symbol_picker_request: None,
            pending_symbol_picker: None,
            pending_workspace_symbol_request: None,
            pending_format_request: StampedPending::default(),
            pending_format_on_save: None,
            pending_save: None,
            quit_after_save: None,
            quit_requested: false,
            pending_completion: None,
            completion_generation: 0,
            pending_completion_request: None,
            pending_narrow: None,
            pending_completion_resolve: None,
            pending_completion_accept: StampedPending::default(),
            last_completion_signature: None,
            completion_context: None,
            active_snippet: None,
            version_info: "unknown",
            next_aux_window: 1,
            apc_tx: None,
            passthrough: None,
            passthrough_link: None,
            stoatty: false,
            stoatty_protocol: 0,
            walkthrough_runs_opened: 0,
            apc_scene: ApcScene::new(),
            pending_undercurls: UndercurlBatch::default(),
            theme_epoch: 0,
            paint_generation: 0,
            pane_cache: std::collections::HashMap::new(),
            pane_paints: 0,
            #[cfg(test)]
            keymap_lookups: std::cell::Cell::new(0),
            #[cfg(test)]
            selection_snapshots: std::cell::Cell::new(0),
            #[cfg(test)]
            focused_mode_reads: std::cell::Cell::new(0),
            #[cfg(test)]
            completion_layouts: std::cell::Cell::new(0),
            chrome: None,
            dimmed_minimap_palette: None,
            smooth_scroll: SmoothScrollState::default(),
            minimap_content: std::collections::HashMap::new(),
            minimap_next_content_id: 0,
            minimap_build_pending: false,
            pty_dirty: false,
            spinner_clock: 0.0,
            minimap_class_table,
        };

        if let Some(message) = config_error {
            stoat.set_status(message);
        }
        stoat.raise_config_actions_badge(&unknown_actions);

        stoat
    }

    /// Re-resolve the user config from `source` and swap the running keymap,
    /// settings, theme, and theme-derived tables.
    ///
    /// A source that fails to parse leaves everything as it was and reports the
    /// failure. Falling back to the built-in defaults is right at startup, where
    /// there is nothing to lose, but mid-session it would tear down a working
    /// setup over a half-typed edit.
    ///
    /// CLI overrides are re-applied on top, so a flag passed at launch keeps
    /// outranking the file. Runtime state (open buffers, the current mode, user
    /// variables) is untouched. Settings read per use follow the new values
    /// immediately, while those consumed once at launch (mouse capture, the
    /// terminal shell, direnv) wait for the next start.
    pub(crate) fn reload_user_config(&mut self, source: &str) {
        let (config, errors) = stoat_config::parse(source);
        if !errors.is_empty() {
            tracing::error!(
                "config reload parse failed; keeping the current config: {}",
                stoat_config::format_errors(source, &errors)
            );
            self.set_status("config parse failed; keeping the current config");
            return;
        }

        let ConfigArtifacts {
            keymap,
            unknown_actions,
            settings,
            theme,
            theme_pool,
            syntax_styles,
            minimap_class_table,
        } = build_config_artifacts(
            config,
            embedded_config(),
            &self.imported_themes,
            self.cli_settings.clone(),
            None,
        );

        self.keymap = keymap;
        self.settings = settings;
        self.theme = Arc::new(theme);
        self.theme_pool = theme_pool;
        self.syntax_styles = syntax_styles;
        self.minimap_class_table = minimap_class_table;

        install_highlight_maps(&self.language_registry, &self.syntax_styles);
        // The theme, the syntax styles, and the settings the renderer reads
        // directly all just moved, and a parse failure returned above without
        // touching any of them.
        self.paint_generation += 1;
        self.raise_config_actions_badge(&unknown_actions);
        self.set_status("config reloaded");
    }

    /// Stand up the notice that the config binds `unknown_actions`, replacing
    /// whatever the previous config raised.
    ///
    /// A stale binding is a standing fault. The key does nothing every time
    /// the user presses it, and a timed status message retires long before the
    /// user meets that key. The badge stays until a reload finds the config
    /// repaired, which an empty `unknown_actions` expresses by leaving none.
    ///
    /// Only the count reaches the badge, because a badge paints one label
    /// line. The names go to the log, which `:logs` opens.
    fn raise_config_actions_badge(&mut self, unknown_actions: &[String]) {
        self.badges
            .remove_by_source(crate::badge::BadgeSource::ConfigActions);
        if unknown_actions.is_empty() {
            return;
        }

        let count = unknown_actions.len();
        let plural = if count == 1 { "" } else { "s" };
        self.badges.insert(crate::badge::Badge {
            source: crate::badge::BadgeSource::ConfigActions,
            anchor: crate::badge::Anchor::BottomRight,
            state: crate::badge::BadgeState::Error,
            label: format!("config binds {count} unknown action{plural}"),
            detail: None,
        });
    }

    /// Look up a previously-cached diff by content hashes plus
    /// language. Returns the serialized hunk payload on cache hit, or
    /// `None` on miss. Called by the viewport-socket diff RPC handler
    /// to translate `ToMain::DiffRequest` into `ToViewport::DiffResponse`.
    pub fn handle_diff_lookup(&self, key: &crate::diff_cache::DiffCacheKey) -> Option<Vec<u8>> {
        let mut cache = self.diff_cache.lock().expect("diff_cache poisoned");
        let (hunks, _move_aware) = cache.lookup(key)?;
        Some(crate::diff_cache::serialize_hunks(&hunks))
    }

    /// Shared handle on the in-memory diff cache. A diff build inserts its
    /// post-extraction hunks here so subsequent
    /// [`Stoat::handle_diff_lookup`] calls hit instead of recomputing.
    pub fn diff_cache(&self) -> Arc<std::sync::Mutex<crate::diff_cache::DiffCache>> {
        self.diff_cache.clone()
    }

    /// Enable the stoatty smooth-scroll APC path.
    ///
    /// `apc_tx` is the ordered channel the app loop pushes APC byte batches onto
    /// for the UI thread to write after each frame. The bin layer calls this once
    /// at startup, before [`Self::run`].
    pub fn set_apc_tx(&mut self, apc_tx: UnboundedSender<Vec<u8>>) {
        self.apc_tx = Some(apc_tx);
    }

    /// Install the app's ends of the `:ssh` and `:mosh` passthrough plumbing.
    ///
    /// The bin layer creates the slot and the control channel before spawning
    /// the UI thread, which needs the other ends, and hands these over once the
    /// app exists. Left uncalled, both commands refuse for want of a terminal
    /// to give away.
    pub fn set_passthrough_link(&mut self, link: ssh::PassthroughLink) {
        self.passthrough_link = Some(link);
    }

    /// Listen for the UI thread's report of the startup ident handshake.
    ///
    /// The bin layer creates the channel before spawning that thread and hands
    /// this end over once the app exists. Left uncalled, [`Self::stoatty`] stays
    /// false for the process's life, which is what a headless or embedded run
    /// wants.
    pub fn set_stoatty_rx(&mut self, stoatty_rx: UnboundedReceiver<Option<u32>>) {
        self.stoatty_rx = stoatty_rx;
    }

    /// Listen for a client attaching to this session.
    ///
    /// Installed by an attach server, which is the only thing that reports one.
    /// Left uncalled, the channel stays closed and nothing ever re-declares the
    /// terminal, which is right for a session no client attaches to.
    pub fn set_attached_rx(&mut self, attached_rx: UnboundedReceiver<()>) {
        self.attached_rx = attached_rx;
    }

    /// Listen for the UI thread's reports of the tty's cell pixel size.
    ///
    /// Handed over the same way and for the same reason as
    /// [`Self::set_stoatty_rx`]: the ui thread owns the tty. Left uncalled,
    /// [`Self::cell_pixels`] stays absent, which is what a run with no terminal
    /// under it should see.
    pub fn set_cell_pixels_rx(&mut self, cell_pixels_rx: UnboundedReceiver<Option<(u16, u16)>>) {
        self.cell_pixels_rx = cell_pixels_rx;
    }

    /// Record that a stoatty is listening, repainting the frames that went out
    /// before the handshake could say so.
    ///
    /// The handshake waits up to a quarter second for a reply, and the app
    /// renders throughout, so the opening frames are drawn for a foreign
    /// terminal. Without the repaint a real stoatty session would keep that
    /// fallback rendering until something else happened to dirty the screen.
    ///
    /// This is also where the session-scoped claims go out. The bin layer cannot
    /// make them itself, because it wires the app up before the run loop starts
    /// and the flag is necessarily still false there, which would gate them away.
    ///
    /// Only the confirming report does anything. A `false` report is the state
    /// the app already starts in, and a repeat cannot arrive since the handshake
    /// runs once.
    fn handle_stoatty_present(&mut self, protocol: Option<u32>) -> UpdateEffect {
        // Set before the filter below, because a plain terminal reports `None`
        // and that answer still ends the wait a reconnect is gated on.
        self.terminal_reported = true;

        let Some(protocol) = protocol.filter(|_| !self.stoatty) else {
            return UpdateEffect::None;
        };

        self.stoatty = true;
        self.stoatty_protocol = protocol;
        apc_emit::emit_theme_default_colors(self);
        self.sync_zoom_claim();

        UpdateEffect::Redraw
    }

    /// Claim the zoom combo as soon as the terminal answers the handshake.
    ///
    /// The claim asks for the presses in band, so the round trip is the pty
    /// both ways: the frame goes out over it and each press comes back over it
    /// as CSI-u. A link carries the pty, so the claim holds over ssh, which is
    /// exactly where it never used to. The window socket is not part of it --
    /// `STOATTY_WINDOW_SOCKET` names a path on the far machine and never
    /// connects there.
    ///
    /// Nothing here releases it. A terminal drops the claim when this process
    /// leaves the alternate screen, which covers a clean exit and a crash
    /// alike, and does so without needing to hear from a process that may no
    /// longer exist.
    pub(crate) fn sync_zoom_claim(&mut self) {
        let claim = self.stoatty;
        if claim == self.zoom_claimed {
            return;
        }
        self.zoom_claimed = claim;
        apc_emit::emit_zoom_capture(self, claim);
    }

    /// Connect to stoatty's window-event socket at `socket`, if set, so detached
    /// panes receive their windows' focus, resize, and close events.
    ///
    /// A detached reader task forwards decoded events over the channel
    /// [`Self::run`] drains. A `None` path (not launched from stoatty, or over
    /// ssh) leaves the connection closed and detach reporting unavailable.
    pub fn set_window_ipc(&mut self, socket: Option<PathBuf>) {
        let Some(path) = socket else {
            return;
        };
        let tx = self.window_ipc_tx.clone();
        self.executor.spawn(connect_window_ipc(path, tx)).detach();
    }

    /// Apply one window-event socket message to the active workspace.
    ///
    /// Connected/Disconnected track the socket state that gates detach. The rest
    /// route to the pane bound to the reported window. `Focused{0}`, the primary
    /// window, returns focus to the split layout when it currently sits on a
    /// detached pane. `Focused{n}` focuses that window's pane, `Resized` re-sizes
    /// it, and `Closed` reattaches it. Every event is a no-op when no pane
    /// matches, absorbing a report that races a reattach.
    fn handle_window_ipc(&mut self, message: WindowIpc) -> UpdateEffect {
        let event = match message {
            WindowIpc::Connected => {
                self.window_ipc_connected = true;
                return UpdateEffect::None;
            },
            WindowIpc::Disconnected => {
                self.window_ipc_connected = false;
                return UpdateEffect::None;
            },
            WindowIpc::Event(event) => event,
        };

        // A pointer event runs the full pane-apply path, which borrows self more
        // broadly than the pane-only lifecycle arms below can while holding the
        // pane-tree borrow.
        if let WindowIpcEvent::Mouse {
            window,
            kind,
            col,
            row,
            mods,
        } = event
        {
            return self.handle_aux_mouse(window, kind, col, row, mods);
        }

        // A terminal predating the delivery mode reads the claim as the
        // socket-mode one it has always understood and forwards the press here,
        // so this is how the same claim is honored by an older host.
        //
        // Routing a zoom step reads the open modal and the zoom ledger, neither
        // of which the pane-tree borrow below leaves reachable.
        if let WindowIpcEvent::Zoom { delta, .. } = event {
            return self.handle_zoom_step(delta);
        }

        // Precision wheel travel routes through the same surfaces a notch does,
        // and both reads borrow more of self than the pane-tree borrow below
        // leaves reachable.
        if let WindowIpcEvent::Wheel {
            window,
            col,
            row,
            mods,
            lines,
        } = event
        {
            return self.handle_window_wheel(window, col, row, mods, lines);
        }

        // The diff chords read the focused view, which the pane-tree borrow
        // below leaves unreachable for the same reason.
        if let WindowIpcEvent::Chord { ch, .. } = event {
            return self.handle_chord(ch);
        }

        // An aux window's own resize drag moves its pane's rectangle the way the
        // terminal's does, and the pools it feeds are the same pools.
        if let WindowIpcEvent::Resized { .. } = event {
            debounce::arm_pool_settle(self);
        }

        let panes = &mut self.active_workspace_mut().panes;
        match event {
            WindowIpcEvent::Focused { window: 0 } => {
                let focused = panes.focus();
                if matches!(panes.pane(focused).placement, Placement::Window(_))
                    && let Some(target) = panes.last_split_focus()
                {
                    panes.set_focus(target);
                }
            },
            WindowIpcEvent::Focused { window } => {
                if let Some(id) = pane_for_window(panes, window) {
                    panes.set_focus(id);
                }
            },
            WindowIpcEvent::Resized { window, cols, rows } => {
                if let Some(id) = pane_for_window(panes, window) {
                    panes.pane_mut(id).area = Rect::new(0, 0, cols, rows);
                }
            },
            WindowIpcEvent::Closed { window } => {
                if let Some(id) = pane_for_window(panes, window) {
                    panes.attach(id);
                }
            },
            WindowIpcEvent::Mouse { .. } => unreachable!("mouse events return above"),
            WindowIpcEvent::Zoom { .. } => unreachable!("zoom events return above"),
            // Only `6`, `7`, `8`, `9` and `0` are spoken for, and each returns above.
            // Another digit is a chord the terminal forwarded on the claim and
            // nothing here answers.
            WindowIpcEvent::Chord { .. } => return UpdateEffect::None,
            WindowIpcEvent::Wheel { .. } => unreachable!("wheel events return above"),
        }
        UpdateEffect::Redraw
    }

    /// Route `lines` of precision wheel travel from `window` at cell `col`,
    /// `row`, with `mods` as the modifier bitmask.
    ///
    /// The primary window runs the same routing a notch takes, so a binding, a
    /// modal, a popup, and a pane each answer the way they always have. An aux
    /// window scrolls the pane it holds without focusing it, as its notch path
    /// does.
    fn handle_window_wheel(
        &mut self,
        window: u32,
        col: u16,
        row: u16,
        mods: u8,
        lines: f32,
    ) -> UpdateEffect {
        if window == 0 {
            let mouse = MouseEvent {
                kind: match lines >= 0.0 {
                    true => MouseEventKind::ScrollDown,
                    false => MouseEventKind::ScrollUp,
                },
                column: col,
                row,
                modifiers: ipc_modifiers(mods),
            };
            return mouse::handle_mouse_scroll(self, mouse, lines);
        }

        let Some(pane_id) = pane_for_window(&self.active_workspace().panes, window) else {
            return UpdateEffect::None;
        };
        let (view, area) = {
            let pane = self.active_workspace().panes.pane(pane_id);
            (pane.view.clone(), pane.area)
        };
        mouse::scroll_view_at(self, view, area, lines)
    }

    /// Run what the keymap binds to a platform-modifier digit chord.
    ///
    /// The keymap decides what a chord does, so the help and the key hints
    /// list a chord as they list any other key. The chord resolves as super
    /// plus the digit, which a `Cmd` binding names. The terminal forwards a
    /// chord whatever is on screen, so the binding's guard is what scopes it,
    /// and a digit no binding claims changes nothing.
    ///
    /// Shared by both deliveries. The window socket carries a chord as its own
    /// event, while an in-band claim spells it as super plus the digit down the
    /// pty. Both reach the lookup here, so the two do not drift apart.
    fn handle_chord(&mut self, ch: char) -> UpdateEffect {
        let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::SUPER);
        let Some((actions, captured_digit)) = self
            .keymap_lookup(&key, &mut KeymapLookup::default())
            .clone()
        else {
            return UpdateEffect::None;
        };
        self.run_bound_actions(&actions, captured_digit, false)
    }

    /// Flip the syntax coloring of whichever diff surface is on screen, or do
    /// nothing where none is.
    ///
    /// The diff view and the commits screen both answer, because both paint
    /// their rows through the same painter and read the same flag. One key
    /// therefore means one thing wherever a diff is in front of the reader.
    ///
    /// Backs [`stoat_action::DiffSyntax`], which the command line runs
    /// whatever is on screen. Elsewhere there is nothing to toggle, so the flag
    /// holds and the frame is left alone.
    pub(crate) fn handle_diff_syntax_toggle(&mut self) -> UpdateEffect {
        if !self.on_a_diff_surface() {
            return UpdateEffect::None;
        }
        self.diff_syntax = !self.diff_syntax;
        UpdateEffect::Redraw
    }

    /// Flip the bold of every change span on whichever diff surface is on
    /// screen, or do nothing where none is.
    ///
    /// Backs [`stoat_action::DiffBold`]. The command line runs it whatever is
    /// on screen, so the flag defends its own scope exactly as
    /// [`Self::handle_diff_syntax_toggle`] does.
    pub(crate) fn handle_diff_bold_toggle(&mut self) -> UpdateEffect {
        if !self.on_a_diff_surface() {
            return UpdateEffect::None;
        }
        self.diff_bold = !self.diff_bold;
        UpdateEffect::Redraw
    }

    /// Flip the underline of every change span on whichever diff surface is on
    /// screen, or do nothing where none is.
    ///
    /// Backs [`stoat_action::DiffUnderline`]. The command line runs it
    /// whatever is on screen, so the flag defends its own scope exactly as
    /// [`Self::handle_diff_syntax_toggle`] does.
    pub(crate) fn handle_diff_underline_toggle(&mut self) -> UpdateEffect {
        if !self.on_a_diff_surface() {
            return UpdateEffect::None;
        }
        self.diff_underline = !self.diff_underline;
        UpdateEffect::Redraw
    }

    /// Whether the screen in front of the reader paints diff rows, which is
    /// what the styling dials act on.
    ///
    /// The diff view and the commits screen qualify. A commit preview runs the
    /// diff view's own row painter, so a dial stepped on either shows on both.
    fn on_a_diff_surface(&self) -> bool {
        matches!(
            keymap_state::view_predicate(self.active_workspace()),
            Some("diff" | "commits")
        )
    }

    /// Step the tint dial by `delta` levels on whichever diff surface is on
    /// screen, or do nothing where none is.
    ///
    /// Backs [`stoat_action::DiffTintDown`] and [`stoat_action::DiffTintUp`].
    /// Off a diff surface there is no tint to step, so the level holds and the
    /// frame is left alone, exactly as [`Self::handle_diff_syntax_toggle`]
    /// answers there. The level is clamped rather than saturated, so a press
    /// past either end is remembered as the end itself and the next press the
    /// other way moves the paint immediately.
    pub(crate) fn handle_diff_tint_step(&mut self, delta: i32) -> UpdateEffect {
        if !self.on_a_diff_surface() {
            return UpdateEffect::None;
        }
        let stepped = i32::from(self.diff_tint).saturating_add(delta);
        self.diff_tint = stepped.clamp(0, crate::render::review::DIFF_TINT_MAX.into()) as i8;
        UpdateEffect::Redraw
    }

    /// Apply `delta` zoom steps to whatever the user is looking at.
    ///
    /// An open modal owns the combo. One with a zoom of its own grows or
    /// shrinks, and one sized entirely by its content swallows the step, since
    /// resizing a pane hidden behind it would be a change the user cannot see.
    ///
    /// With no modal open, a diff surface takes the step as contrast rather
    /// than size. There is no pane in front of the reader to resize, and how
    /// far unchanged code recedes is the one dial those screens have. The
    /// commits screen paints its overlay over the pane grid, so a resize there
    /// moves a pane the reader never sees. Every other screen and a plain pane
    /// resize the focused pane against its split.
    ///
    /// The diff branch subtracts the step where the others add it, because what
    /// the reader is zooming there is the changed code. Deepening the recede
    /// shows less of the unchanged context around it, so the shrink key raises
    /// the level and the grow key lowers it.
    ///
    /// Modal levels are per modal kind and outlive the modal, so reopening one
    /// brings back the size the user last chose for it. A level is clamped to
    /// [`Self::modal_zoom_range`] rather than the wider ledger range, so a press
    /// the box cannot act on is never remembered and the next press the other
    /// way moves the modal immediately. Clamping the stored level before the
    /// delta also brings an entry left over from a larger terminal back into
    /// range on its first press.
    fn handle_zoom_step(&mut self, delta: i32) -> UpdateEffect {
        if let Some(kind) = crate::render::zoom_target_kind(self) {
            let (lo, hi) =
                mouse::modal_zoom_range(self, kind).unwrap_or((MODAL_ZOOM_MIN, MODAL_ZOOM_MAX));
            let level = self.modal_zoom.entry(kind).or_insert(0);
            let stepped = i32::from((*level).clamp(lo, hi)).saturating_add(delta);
            *level = stepped.clamp(lo.into(), hi.into()) as i8;
            return UpdateEffect::Redraw;
        }
        if crate::render::zoom_context_modal(self) {
            return UpdateEffect::None;
        }

        if self.on_a_diff_surface() {
            let stepped = i32::from(self.diff_soften).saturating_sub(delta);
            self.diff_soften = stepped.clamp(
                crate::render::review::DIFF_SOFTEN_MIN.into(),
                crate::render::review::DIFF_SOFTEN_MAX.into(),
            ) as i8;
            return UpdateEffect::Redraw;
        }

        self.active_workspace_mut().panes.resize_focused_pane(delta);
        UpdateEffect::Redraw
    }

    /// Run whatever the keymap binds to a press of the back or forward side
    /// button, returning [`None`] when `kind` names any other button.
    ///
    /// The keymap resolves first and the jumplist is what an unbound button
    /// falls back to, so a mode that binds the button speaks for it. That is
    /// what lets a pinned `space_goto` walk hunks on these buttons, back as
    /// its n arm and forward as its p arm. Bound actions run with
    /// `dismisses_pinned` false, as a wheel binding does, so the press repeats
    /// under a pin instead of releasing it.
    ///
    /// These buttons reach stoat only over the window socket, since no in-band
    /// terminal encoding carries them, so this runs before the window's pane
    /// binding is resolved. The primary window has no binding of its own, and
    /// its presses act on whatever pane holds focus. An aux window's presses
    /// focus its pane first, as its other gestures do.
    ///
    /// Handling every gesture of both buttons here is what keeps them away from
    /// [`mouse_event_kind`], which has no crossterm button to map them onto.
    fn handle_side_buttons(
        &mut self,
        window: u32,
        kind: MouseKind,
        mods: u8,
    ) -> Option<UpdateEffect> {
        let button = match kind {
            MouseKind::Press(IpcMouseButton::Back) => SideButton::Back,
            MouseKind::Press(IpcMouseButton::Forward) => SideButton::Forward,
            MouseKind::Release(IpcMouseButton::Back | IpcMouseButton::Forward)
            | MouseKind::Drag(IpcMouseButton::Back | IpcMouseButton::Forward) => {
                return Some(UpdateEffect::None);
            },
            _ => return None,
        };

        if window != 0
            && let Some(pane_id) = pane_for_window(&self.active_workspace().panes, window)
        {
            self.active_workspace_mut().panes.set_focus(pane_id);
        }

        let bound = {
            let state = StoatKeymapState::from_stoat(self);
            self.keymap
                .lookup_side_button(&state, button, ipc_modifiers(mods))
        };
        if let Some(actions) = bound {
            return Some(self.run_bound_actions(&actions, None, false));
        }

        Some(match button {
            SideButton::Back => action_handlers::jump::jump_backward(self),
            SideButton::Forward => action_handlers::jump::jump_forward(self),
        })
    }

    /// Route a pointer event from aux window `window` to the pane bound to it.
    ///
    /// Resolves the window's pane and focuses it, so the shared apply targets it
    /// without the primary grid hit-test ever running -- an aux click cannot
    /// land on a primary pane whose rect overlaps. `col`/`row` are
    /// window-relative, which equal pane-relative coordinates since a detached
    /// pane's area sits at (0, 0).
    fn handle_aux_mouse(
        &mut self,
        window: u32,
        kind: MouseKind,
        col: u16,
        row: u16,
        mods: u8,
    ) -> UpdateEffect {
        if let Some(effect) = self.handle_side_buttons(window, kind, mods) {
            return effect;
        }

        let Some(pane_id) = pane_for_window(&self.active_workspace().panes, window) else {
            return UpdateEffect::None;
        };

        // A wheel scrolls the window's pane without focusing it, as the primary
        // wheel scrolls the pane under the cursor rather than the focused one.
        if matches!(kind, MouseKind::WheelUp | MouseKind::WheelDown) {
            let (view, area) = {
                let pane = self.active_workspace().panes.pane(pane_id);
                (pane.view.clone(), pane.area)
            };
            let lines = match matches!(kind, MouseKind::WheelDown) {
                true => 1.0,
                false => -1.0,
            };
            return mouse::scroll_view_at(self, view, area, lines);
        }

        let Some(kind) = mouse_event_kind(kind) else {
            return UpdateEffect::None;
        };
        self.active_workspace_mut().panes.set_focus(pane_id);
        mouse::apply_focused_pane_mouse(self, kind, col, row)
    }

    /// Inject the version string the `ShowVersion` action reports. The binary
    /// passes its build-stamped `VERSION_INFO`. Tests leave the default.
    pub fn set_version_info(&mut self, info: &'static str) {
        self.version_info = info;
    }

    /// The active minimap mode, resolving the runtime visibility override
    /// against the `editor.minimap` setting.
    ///
    /// [`Self::minimap_override`] `Some(false)` forces [`MinimapMode::Off`].
    /// `Some(true)` shows the setting's mode, falling back to
    /// [`MinimapMode::Single`] when the setting itself is `Off`. With no
    /// override the setting wins, defaulting to [`MinimapMode::Single`].
    pub(crate) fn minimap_mode(&self) -> MinimapMode {
        let setting = self.settings.editor_minimap.unwrap_or(MinimapMode::Single);
        match self.minimap_override {
            Some(false) => MinimapMode::Off,
            Some(true) if setting == MinimapMode::Off => MinimapMode::Single,
            _ => setting,
        }
    }

    /// Whether any minimap strip is currently shown.
    pub(crate) fn minimap_enabled(&self) -> bool {
        self.minimap_mode() != MinimapMode::Off
    }

    /// Flip the minimap's visibility for the session, overriding the setting.
    pub(crate) fn toggle_minimap(&mut self) {
        self.minimap_override = Some(!self.minimap_enabled());
    }

    /// Flip the focused editor's soft-wrap override.
    ///
    /// A first toggle overrides the configured `editor.wrap` mode with its
    /// opposite (wrapping the other way); a second clears the override so the
    /// editor follows the setting again.
    pub(crate) fn toggle_wrap(&mut self) {
        let flipped = match self.settings.editor_wrap.unwrap_or(WrapMode::EditorWidth) {
            WrapMode::None => WrapMode::EditorWidth,
            _ => WrapMode::None,
        };
        if let Some(editor) = action_handlers::focused_editor_mut(self) {
            editor.wrap_override = match editor.wrap_override {
                Some(_) => None,
                None => Some(flipped),
            };
        }
    }

    /// Swap in an alternative [`FsHost`]. The default is [`LocalFs`]; the
    /// test harness installs [`crate::host::FakeFs`] so review, open-file,
    /// and other IO paths run in-memory.
    pub fn set_fs_host(&mut self, host: Arc<dyn FsHost>) {
        self.fs_host = host;
    }

    /// Swap in an alternative [`FsWatchHost`]. The default is
    /// [`NoopFsWatcher`] (no events ever fire); the bin layer
    /// installs [`crate::host::LocalFsWatcher`] and tests install
    /// [`crate::host::FakeFsWatcher`].
    ///
    /// Arms the host's wake against [`Self::drain_notify`], so a burst that
    /// arrives while the user sits idle still reaches
    /// [`debounce::drain_fs_watch_events`]. Wiring it here rather than in
    /// [`Self::new`] is what makes it reach every host, since the one `new`
    /// installs produces no events at all.
    ///
    /// The installed host holds no watch, so every root is watched again on
    /// its next entry.
    pub fn set_fs_watch_host(&mut self, host: Arc<dyn FsWatchHost>) {
        host.set_wake(Box::new({
            let drain = self.drain_notify.clone();
            move || drain.notify_one()
        }));
        self.fs_watch_host = host;
        self.watched_roots.clear();
    }

    /// Returns the active [`FsWatchHost`].
    pub fn fs_watch_host(&self) -> &Arc<dyn FsWatchHost> {
        &self.fs_watch_host
    }

    /// Swap in an alternative [`GitHost`]. The default is [`LocalGit`];
    /// tests inject [`crate::host::FakeGit`] to drive the review flow
    /// without a real repository.
    pub fn set_git_host(&mut self, host: Arc<dyn GitHost>) {
        self.git_host = host;
    }

    /// Swap in an alternative [`EnvHost`]. The default is [`LocalEnv`];
    /// the test harness installs [`crate::host::FakeEnv`] so env-var
    /// reads do not pull in real process state.
    pub fn set_env_host(&mut self, host: Arc<dyn EnvHost>) {
        self.home = host.var("HOME").map(PathBuf::from);
        self.env_host = host;
    }

    /// Returns the active [`EnvHost`].
    pub fn env_host(&self) -> &Arc<dyn EnvHost> {
        &self.env_host
    }

    /// Swap in an alternative [`crate::host::ClipboardHost`]. The default
    /// is [`crate::host::NoopClipboard`]; production binaries install
    /// [`crate::host::LocalClipboard`] (arboard-backed) and tests
    /// install [`crate::host::FakeClipboard`].
    pub fn set_clipboard_host(&mut self, host: Arc<dyn crate::host::ClipboardHost>) {
        self.clipboard_host = host;
    }

    /// Returns the active [`crate::host::ClipboardHost`].
    pub fn clipboard_host(&self) -> &Arc<dyn crate::host::ClipboardHost> {
        &self.clipboard_host
    }

    /// Swap in an alternative [`crate::host::ShellHost`]. The default
    /// is [`crate::host::LocalShell`]; the test harness installs
    /// [`crate::host::FakeShell`].
    pub fn set_shell_host(&mut self, host: Arc<dyn crate::host::ShellHost>) {
        self.shell_host = host;
    }

    /// Swap in an alternative [`LspHost`]. The default is [`NoopLsp`]
    /// (every request returns the empty success response); the test
    /// harness installs [`crate::host::FakeLsp`] so LSP-driven flows
    /// run against programmed responses.
    pub fn set_lsp_host(&mut self, host: Arc<dyn LspHost>) {
        self.lsp_registry.set_sole_client(host);
    }

    /// Enable or disable lazily spawning a real language server on the
    /// first open of a buffer whose language has a known server command.
    /// The binary enables it for a live session. Tests leave it off so
    /// the [`NoopLsp`] placeholder performs no IO.
    pub fn set_lsp_auto_spawn(&mut self, enabled: bool) {
        self.lsp_auto_spawn = enabled;
    }

    /// Enable or disable automatic direnv environment loading. Off by
    /// default so tests never spawn direnv. The binary enables it for a
    /// live session.
    pub fn set_env_auto_load(&mut self, enabled: bool) {
        self.env_auto_load = enabled;
    }

    /// Enable background diff-cache warming at workspace open. Off by default so
    /// the test harness never spawns a warm pass. The binary turns it on.
    pub fn set_diff_warm_auto(&mut self, enabled: bool) {
        self.diff_warm_auto = enabled;
    }

    /// Point the per-workspace agent sockets at `dir`, enabling both socket
    /// serving and session-environment injection for owned children.
    ///
    /// Unset by default, which leaves both off. The binary passes
    /// [`crate::run::agent_socket_bind_dir`], and the fixture harness passes a
    /// temporary directory.
    pub fn set_agent_socket_dir(&mut self, dir: PathBuf) {
        self.agent_socket_dir = Some(dir);
    }

    /// Allow [`Self::serve_term_session`] to bind listeners.
    ///
    /// Off by default, since the server task needs a live Tokio reactor. The
    /// binary and the fixture harness turn it on alongside the directory.
    pub fn set_serve_agent_sockets(&mut self, enabled: bool) {
        self.serve_agent_sockets = enabled;
    }

    /// Name the log file this session writes, which `:logs` opens.
    pub fn set_session_log(&mut self, path: PathBuf) {
        self.session_log = Some(SessionLog::new(path));
    }

    pub fn active_workspace(&self) -> &Workspace {
        &self.workspaces[self.active_workspace]
    }

    pub fn active_workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active_workspace]
    }

    /// Run `f` with `workspace` active, then put the active workspace back.
    ///
    /// Background work lands through this in the workspace that started it,
    /// because buffer and pane ids repeat across workspaces. Returns `None` if
    /// `workspace` closed, since nothing is left for the work to act on. The
    /// workspace that was active before the call stays active after it, unless
    /// it closed during `f`.
    pub(crate) fn in_workspace<R>(
        &mut self,
        workspace: WorkspaceId,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Option<R> {
        if !self.workspaces.contains_key(workspace) {
            return None;
        }
        let front = std::mem::replace(&mut self.active_workspace, workspace);
        let result = f(self);
        if self.workspaces.contains_key(front) {
            self.active_workspace = front;
        }
        Some(result)
    }

    /// Resolve [`Self::chrome`] against the active theme when it has not been.
    ///
    /// Separate from reading it, so a caller that goes on to borrow the rest of
    /// the state it paints from can settle the rebuild first.
    pub(crate) fn refresh_chrome(&mut self) {
        // Keyed on the theme handle rather than its contents, and the handle is
        // retained so its allocation cannot be freed and reused by a later
        // theme that would then read as unchanged.
        let fresh = self
            .chrome
            .as_ref()
            .is_some_and(|(theme, _)| Arc::ptr_eq(theme, &self.theme));
        if !fresh {
            let chrome = crate::render::editor::ResolvedChrome::resolve(&self.theme);
            self.chrome = Some((self.theme.clone(), chrome));
        }
    }

    /// Settle [`Self::dimmed_minimap_palette`] for the dim now in force.
    ///
    /// Separate from reading it for the same reason [`Self::refresh_chrome`]
    /// is. A caller passes `wanted` as `false` to keep the blend from being
    /// built at all, which is what the minimap being off means.
    pub(crate) fn refresh_dimmed_minimap_palette(&mut self, wanted: bool, dim: f32) {
        if !wanted || dim <= 0.0 {
            self.dimmed_minimap_palette = None;
            return;
        }
        let fresh = self
            .dimmed_minimap_palette
            .as_ref()
            .is_some_and(|(theme, blended_at, _)| {
                Arc::ptr_eq(theme, &self.theme) && *blended_at == dim
            });
        if fresh {
            return;
        }

        let Some(bg) = crate::render::paint::style_rgb(
            self.theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        ) else {
            self.dimmed_minimap_palette = None;
            return;
        };
        let blended = self
            .minimap_class_table
            .palette()
            .iter()
            .map(|&c| crate::render::paint::dim_rgb(c, bg, dim))
            .collect();
        self.dimmed_minimap_palette = Some((self.theme.clone(), dim, blended));
    }

    pub(crate) fn size(&self) -> Rect {
        self.size
    }

    /// The area the last paint laid the workspace split panes out in.
    ///
    /// This is the full terminal ([`Self::size`]) minus the single-minimap band
    /// when one is reserved, so the panes never overlap the strip. Pane paint,
    /// smooth-scroll pool emit, and pane mouse hit-tests all derive from it so a
    /// pooled region lines up with the painted grid instead of tearing a few
    /// columns off. The stamped `single_minimap_rect` records what that paint
    /// reserved.
    ///
    /// Centered modals do not lay out here. They take the full [`Self::size`]
    /// and the strip yields to them, so their paint, pools, and mouse hit-tests
    /// derive from `size` instead.
    pub(crate) fn layout_size(&self) -> Rect {
        let size = match self.single_minimap_rect {
            Some(band) => Rect {
                width: self.size.width.saturating_sub(band.width),
                ..self.size
            },
            None => self.size,
        };
        if !self.tab_bar_visible() {
            return size;
        }
        Rect {
            y: size.y + 1,
            height: size.height.saturating_sub(1),
            ..size
        }
    }

    /// Whether the tab bar occupies the window's top row this frame.
    ///
    /// [`TabBarMode::Auto`] reveals it only once the active workspace holds a
    /// second tab, so a single-tab session keeps the whole window. `:tabs`
    /// overrides the configured mode for the session.
    pub(crate) fn tab_bar_visible(&self) -> bool {
        let mode = self
            .tab_bar_override
            .or(self.settings.ui_tab_bar)
            .unwrap_or(TabBarMode::Auto);
        match mode {
            TabBarMode::Always => true,
            TabBarMode::Never => false,
            TabBarMode::Auto => self.active_workspace().tabs.len() > 1,
        }
    }

    /// Convenience wrapper that dispatches the [`OpenFile`] action with `path`.
    ///
    /// The action handler reads the file, creates a buffer, and shows it in
    /// the focused pane. A missing file becomes an empty buffer with the path
    /// attached (vim-style); other IO errors are logged and ignored.
    pub fn open_file(&mut self, path: &Path) {
        let action = OpenFile {
            path: path.to_path_buf(),
        };
        action_handlers::dispatch(self, &action);
    }

    /// Toggle the side-by-side diff view on the focused editor, as the `:diff`
    /// command does.
    pub fn toggle_diff_view(&mut self) {
        action_handlers::dispatch(self, &Diff { rev: None });
    }

    /// Open the working-tree diff for the `stoat review` entry point.
    ///
    /// `Diff` turns the view on for the current editor. On the pathless startup
    /// scratch, which has no changes of its own, the toggle crosses into the
    /// first changed file, installs its diff map, and lands the cursor on its
    /// first hunk with the view scrolled to it. With no changed files it sets
    /// the "no more changes" status and stays on the scratch.
    pub fn open_working_tree_diff(&mut self) {
        action_handlers::dispatch(self, &Diff { rev: None });
    }

    /// Open the three-way conflict resolve view for the `stoat conflict` entry
    /// point.
    ///
    /// `Conflict` opens the repository's conflicted files in the ours / result /
    /// theirs view. With no index conflicts, the "no merge conflicts" status
    /// shows when the worker's read lands, and the startup files view stays.
    pub fn open_conflict_view(&mut self) {
        action_handlers::dispatch(self, &Conflict);
    }

    /// Open the workspace finder for a bare launch.
    ///
    /// A launch with nothing to show asks which project to enter rather than
    /// landing on the scratch, so the finder comes up with the saved sessions
    /// already listed. Escape dismisses it onto the scratch the launch shows
    /// otherwise.
    ///
    /// When the list holds only the workspace the launch already sits in, the
    /// finder stays closed and the launch lands on the scratch. A lone row
    /// offers nothing to pick. An explicit `SwitchWorkspace` still opens the
    /// finder at one row, because an ask deserves an answer.
    pub fn open_workspace_picker(&mut self) {
        action_handlers::workspace::open_workspace_picker(self);

        if let Some(picker) = self
            .workspace_picker
            .take_if(|picker| !picker.has_switch_target())
        {
            picker.dispose(self.active_workspace_mut());
        }
    }

    /// Handle that makes [`Self::run`] quit at its next loop turn when
    /// notified via [`tokio::sync::Notify::notify_one`], regardless of the
    /// editor's current mode or focus. The `--timeout` self-driver and the
    /// binary's termination-signal task each hold a clone. The self-driver
    /// fires it after the delay, and the task fires it on SIGHUP, SIGINT, or
    /// SIGTERM.
    pub fn shutdown_handle(&self) -> Arc<tokio::sync::Notify> {
        self.shutdown_notify.clone()
    }

    pub async fn run(
        &mut self,
        mut events: UnboundedReceiver<Event>,
        render: watch::Sender<Option<RenderFrame>>,
    ) -> io::Result<()> {
        self.start_index_build();

        // Frame clock for scroll-animation ticks. A single persistent interval,
        // polled directly in the select! below, keeps the glide at frame rate.
        // Re-creating an Executor::timer each iteration instead ran far below
        // frame rate on the production current-thread runtime.
        let mut frame_timer = tokio::time::interval(SCROLL_FRAME);
        frame_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_tick: Option<std::time::Instant> = None;
        // The prior frame's screen buffer, recycled into the next paint once the
        // render thread has released it, so a redraw reuses one allocation rather
        // than allocating a fresh ~screen-sized buffer per frame.
        let mut recycled: Option<RenderFrame> = None;

        loop {
            let animating = self.is_animating();
            let building = self.minimap_build_pending;
            let dirty = self.pty_dirty;
            let spinning = self.lsp_progress.current().is_some() || self.diff_warm_busy();
            if !animating && !spinning {
                last_tick = None;
            }
            // Wall-clock instant the frame's first event arrived, so
            // input-to-publish latency spans from it to `send_replace`. Set
            // only by the input arm, so notify- and timer-woken frames record
            // no input latency.
            #[cfg(feature = "perf")]
            let mut t_event: Option<std::time::Instant> = None;
            let first = tokio::select! {
                biased;
                event = events.recv() => {
                    let Some(event) = event else { break };
                    #[cfg(feature = "perf")]
                    {
                        t_event = Some(std::time::Instant::now());
                    }
                    #[cfg(feature = "perf")]
                    let started = std::time::Instant::now();
                    let effect = self.update(event);
                    #[cfg(feature = "perf")]
                    self.perf.record_update(started.elapsed());
                    effect
                }
                // Second, ahead of every channel arm. A biased select returns at
                // its first ready arm, and a producer that keeps one of those
                // channels non-empty would otherwise starve the timer for as
                // long as it runs. Terminal output only marks the screen dirty,
                // so this arm is what turns a flood into frames.
                _ = frame_timer.tick(), if animating || building || dirty || spinning => {
                    let now = std::time::Instant::now();
                    let dt = last_tick
                        .map(|prev| (now - prev).as_secs_f32().min(MAX_FRAME_DT))
                        .unwrap_or_else(|| SCROLL_FRAME.as_secs_f32());
                    #[cfg(feature = "perf")]
                    self.perf
                        .record_anim_tick(std::time::Duration::from_secs_f32(dt));
                    let effect = self.frame_tick(dt);
                    // Measure the next dt from here, after any synchronous page
                    // refill inside emit_smooth_scroll. Otherwise a refill's
                    // render time inflates the following step into a visible
                    // multi-row jump instead of smooth motion.
                    last_tick = Some(std::time::Instant::now());
                    effect
                }
                // The spawn waits for the input thread to confirm it stopped
                // parsing fd 0, so a fast remote's ident never reaches
                // crossterm and arrives as garbage keys.
                Some(()) = ssh::ack_recv(&mut self.passthrough_link) => {
                    ssh::spawn_armed(self)
                }
                ev = self.agent_event_rx.recv() => {
                    let Some(ev) = ev else { continue };
                    self.handle_agent_event(ev)
                }
                ctl = self.agent_control_rx.recv() => {
                    let Some(ctl) = ctl else { continue };
                    self.handle_agent_control(ctl)
                }
                msg = self.window_ipc_rx.recv() => {
                    let Some(msg) = msg else { continue };
                    self.handle_window_ipc(msg)
                }
                // Matching `Some` rather than testing for closure parks this arm
                // once the UI thread drops its sender, where the `continue` its
                // neighbours use would wake the loop on every poll.
                Some(present) = self.stoatty_rx.recv() => {
                    self.handle_stoatty_present(present)
                        .merge(ssh::reconnect_when_ready(self))
                }
                // Parks the same way once the server drops its sender, and for
                // the same reason as the arm above.
                Some(()) = self.attached_rx.recv() => ssh::terminal_replaced(self),
                // A poll tick is a reason to re-stat the followed files, not a
                // reason to paint. Only the pump finding one of them advanced
                // asks for a frame.
                Some(()) = self.auto_reload_rx.recv() => {
                    if crate::auto_reload::pump_auto_reload(self) {
                        UpdateEffect::Redraw
                    } else {
                        UpdateEffect::None
                    }
                }
                _ = self.redraw_notify.notified() => UpdateEffect::Redraw,
                // A drain wake has work to land and nothing to show for it
                // unless the drain says otherwise, so the frame is earned
                // rather than assumed.
                _ = self.drain_notify.notified() => {
                    // Ahead of drain_external, so the debounces a burst arms
                    // are visible to the drains in the same turn rather than
                    // waiting on another wake.
                    debounce::drain_fs_watch_events(self);
                    if self.drain_external() {
                        UpdateEffect::Redraw
                    } else {
                        UpdateEffect::None
                    }
                }
                _ = self.shutdown_notify.notified() => UpdateEffect::Quit,
                // Last, because the pty is the one arm whose producer can
                // saturate. A biased select returns at its first ready arm, so
                // an arm ahead of the others that is never empty starves every
                // one of them for as long as the flood lasts, and the fs-watch
                // drain below only runs from its own arm. Every arm above this
                // costs one poll of an empty receiver per turn, which is
                // nanoseconds against that.
                //
                // Terminal output keeps its throughput either way: a biased
                // select reaches the last arm whenever none above it is ready,
                // which is the ordinary case.
                notif = self.pty_rx.recv() => {
                    let Some(notif) = notif else { continue };
                    self.handle_pty_notification(notif)
                }
            };

            let (drained, coalesced) = self.drain_pending(&mut events);

            // A select arm that is ready returns without awaiting, so a turn
            // whose channel refilled never hands the runtime back. The timer
            // driver runs only when it does, so without this the frame timer
            // above never comes due under a flood, whatever order its arm sits
            // in.
            tokio::task::yield_now().await;

            let effect = first.merge(drained);
            #[cfg(feature = "perf")]
            self.perf.record_coalesced(coalesced);
            #[cfg(not(feature = "perf"))]
            let _ = coalesced;

            match effect {
                UpdateEffect::Redraw => {
                    self.drive_background();
                    // A remote session owns the screen, so nothing this process
                    // draws may reach it. Background work still advanced above,
                    // which is what keeps saves and language servers alive
                    // under a long ssh run.
                    if self.passthrough.is_some() {
                        continue;
                    }
                    // A `:wq` deferred behind a write sets `quit_requested` from
                    // the pump inside `drive_background` once the write lands on
                    // the last pane, so quit on the frame it completes.
                    if std::mem::take(&mut self.quit_requested) {
                        self.save_all_workspaces();
                        break;
                    }
                    let (buffer, undercurl) = {
                        // Reuse the released prior frame's allocation. paint_into
                        // resizes and resets it, so it paints as a fresh buffer
                        // would. The fallback fresh buffer double-clears (empty
                        // then reset), acceptable on this rare path.
                        let mut b = recycled
                            .take()
                            .and_then(|f| Arc::try_unwrap(f.buffer).ok())
                            .unwrap_or_else(|| Buffer::empty(self.size));
                        #[cfg(feature = "perf")]
                        let painted = std::time::Instant::now();
                        self.paint_into(&mut b);
                        #[cfg(feature = "perf")]
                        self.perf.record_paint(painted.elapsed());
                        let undercurl = undercurl::build(&b, self.pending_undercurls.spans());
                        (Arc::new(b), undercurl)
                    };
                    let cursor = self.primary_cursor_screen_pos();
                    recycled = render.send_replace(Some(RenderFrame {
                        buffer,
                        cursor,
                        undercurl,
                        #[cfg(feature = "perf")]
                        input_time: t_event,
                    }));
                    #[cfg(feature = "perf")]
                    if let Some(started) = t_event {
                        self.perf.record_input_to_publish(started.elapsed());
                    }
                    apc_emit::emit_apc_scene(self);
                    crate::image_emit::emit_images(self);
                    apc_emit::emit_windows(self);
                    apc_emit::emit_smooth_scroll(self);
                    emit::emit_minimap(self);
                    if render.is_closed() {
                        break;
                    }
                },
                UpdateEffect::Quit => {
                    self.save_all_workspaces();
                    break;
                },
                UpdateEffect::None => {},
            }
        }

        // A dropped task is cancelled on the executor's next turn, and the yield
        // gives it that turn. So the socket files are gone before the caller
        // starts its shutdown, which a kill cuts short when a server is slow.
        self.agent_servers.clear();
        tokio::task::yield_now().await;

        crate::image_emit::emit_drop_all_images(self);
        apc_emit::emit_reset_default_colors(self);

        tracing::info!(target: "stoat::app", "stoat exiting");

        #[cfg(feature = "perf")]
        self.log_perf_table();

        Ok(())
    }

    /// Log every main-thread perf metric's percentiles to `stoat::perf` when
    /// the run loop exits, so a session's latency profile lands in the log.
    #[cfg(feature = "perf")]
    fn log_perf_table(&self) {
        let metrics = [
            ("update", self.perf.update_stats()),
            ("paint", self.perf.paint_stats()),
            ("input_to_publish", self.perf.input_to_publish_stats()),
            ("coalesced", self.perf.coalesced_stats()),
            ("anim_tick", self.perf.anim_tick_stats()),
        ];
        for (metric, stats) in metrics {
            if let Some(s) = stats {
                tracing::info!(
                    target: "stoat::perf",
                    metric,
                    last = s.last,
                    p50 = s.p50,
                    p95 = s.p95,
                    worst = s.worst,
                    "perf percentiles",
                );
            }
        }
    }

    /// Whether the active workspace has an in-flight animation that needs a
    /// per-frame tick.
    ///
    /// True while any editor is mid scroll-glide. Future animation sources
    /// should OR their own condition in here so [`Self::run`]'s frame timer
    /// covers them.
    fn is_animating(&self) -> bool {
        self.active_workspace()
            .editors
            .values()
            .any(|editor| editor.scroll_glide != ScrollGlide::None)
    }

    /// Resolve one frame-timer tick into an [`UpdateEffect`].
    ///
    /// While a glide eases, stoatty pushes the eased scroll target to its pool and
    /// skips the live-grid repaint, since a settled glide repaints once. A plain
    /// terminal has no pool, so it repaints the eased position each tick instead
    /// of freezing until the glide settles. Otherwise advance a pending minimap
    /// build chunk. A visible run or terminal that fed output since the last tick
    /// then merges in a repaint, so streamed output paces to one repaint per
    /// frame rather than one per PTY chunk.
    fn frame_tick(&mut self, dt: f32) -> UpdateEffect {
        let animating = self.is_animating();
        let building = self.minimap_build_pending;
        let effect = if self.tick_scroll_anim(dt) {
            apc_emit::emit_smooth_scroll(self);
            UpdateEffect::None
        } else if animating {
            // The glide just landed. Viewport-keyed LSP work is held back while
            // one is in flight, and a frame tick never reaches the trigger
            // epilogue at the end of `update`, so the landed viewport asks here.
            action_handlers::lsp::inlay_hints_trigger(self);
            UpdateEffect::Redraw
        } else if building {
            // A build-only wakeup advances one minimap build chunk with no
            // repaint. A Redraw frame resumes the build through the seam.
            emit::emit_minimap(self);
            UpdateEffect::None
        } else {
            UpdateEffect::None
        };

        let effect = if self.lsp_progress.current().is_some() || self.diff_warm_busy() {
            let before = spinner_phase(self.spinner_clock);
            self.spinner_clock += dt;
            if spinner_phase(self.spinner_clock) == before {
                effect
            } else {
                effect.merge(UpdateEffect::Redraw)
            }
        } else {
            self.spinner_clock = 0.0;
            effect
        };

        if std::mem::take(&mut self.pty_dirty) {
            effect.merge(UpdateEffect::Redraw)
        } else {
            effect
        }
    }

    /// Advance every animating editor's scroll glide by `dt` seconds, the real
    /// time elapsed since the previous tick. Returns whether any editor is still
    /// gliding after the step.
    ///
    /// A glide eases `scroll_offset` toward the `scroll_row` target the wheel or
    /// page motion already set, clearing [`ScrollGlide`] on settle. It never
    /// writes `scroll_row` -- that is the fixed target the offset eases up to. A
    /// wheel glide eases slower than a page glide, so a stream of reports at
    /// wheel rates overlaps into continuous motion instead of pulsing. A gap
    /// wider than three viewports (a big count-jump or a jump landing mid-glide)
    /// snaps instead so the offset never drags across the pool's buffered window.
    ///
    /// A wheel glide keeps the cursor anchored to its origin line while it moves
    /// fast. Once the glide slows below a velocity threshold the cursor re-homes
    /// into the scrolloff band mid-flight, repeating as a slow crawl drifts the
    /// viewport, so it comes into frame before the glide settles.
    pub(crate) fn tick_scroll_anim(&mut self, dt: f32) -> bool {
        const PAGE_EASE: f32 = 0.35;
        // Slow enough that >=10Hz wheel report trains overlap into continuous
        // motion instead of pulse-stall-pulse, fast enough that a lone notch's
        // three-row glide completes in about 200ms.
        const WHEEL_EASE: f32 = 0.13;
        // Below this glide velocity in rows per second the cursor re-homes into
        // the scrolloff band mid-flight rather than waiting for the settle, so
        // it comes into frame while content still visibly moves. With WHEEL_EASE
        // the per-tick velocity is roughly the remaining gap in rows times a
        // fixed factor, so 15 rows/s lands the cursor once under about one row of
        // glide remains. Raise it to bring the cursor in earlier.
        const WHEEL_REHOME_MAX_VELOCITY: f32 = 15.0;

        // Read before the workspace borrow so the settle clamp below can use it.
        let scrolloff = self.settings.scrolloff.unwrap_or(3);
        let mut animating = false;
        for editor in self.active_workspace_mut().editors.values_mut() {
            let ease = match editor.scroll_glide {
                ScrollGlide::None => continue,
                ScrollGlide::Page => PAGE_EASE,
                ScrollGlide::Wheel => WHEEL_EASE,
            };
            let was = editor.scroll_glide;
            let target = editor.scroll_row as f32 + editor.scroll_frac;
            let viewport = editor
                .viewport_rows
                .unwrap_or(action_handlers::view::DEFAULT_VIEWPORT_ROWS)
                .max(1);
            let mut closed = 0.0;
            if (target - editor.scroll_offset).abs() > viewport as f32 * 3.0 {
                editor.scroll_offset = target;
                editor.scroll_glide = ScrollGlide::None;
            } else {
                let (offset, settled) =
                    action_handlers::view::step_scroll_ease(editor.scroll_offset, target, dt, ease);
                closed = (offset - editor.scroll_offset).abs();
                editor.scroll_offset = offset;
                if settled {
                    editor.scroll_glide = ScrollGlide::None;
                }
            }
            // A wheel glide defers its cursor follow to the settle, so when it
            // just cleared, clamp the anchored cursor into the landing band.
            if was == ScrollGlide::Wheel && editor.scroll_glide == ScrollGlide::None {
                action_handlers::view::clamp_cursor_to_view(editor, scrolloff);
            }
            // While the glide is still in flight but has slowed below the re-home
            // velocity, land the cursor in the band now rather than at the settle.
            // clamp_cursor_to_view no-ops inside the band, so this re-homes about
            // once per notch as the viewport drifts on.
            if was == ScrollGlide::Wheel
                && editor.scroll_glide == ScrollGlide::Wheel
                && closed / dt.max(1e-6) <= WHEEL_REHOME_MAX_VELOCITY
            {
                action_handlers::view::clamp_cursor_to_view(editor, scrolloff);
            }
            animating |= editor.scroll_glide != ScrollGlide::None;
        }
        animating
    }

    /// Apply every message already queued on the input and notification
    /// channels without blocking, returning their combined [`UpdateEffect`]
    /// and the count of messages drained (the frame's coalesce count).
    ///
    /// Called after [`Self::run`] wakes on its first message so a burst
    /// collapses into a single render instead of one render per message. A
    /// paste's worth of keystrokes or a flood of PTY notifications all apply
    /// before that one render.
    ///
    /// Each channel is drained only to its currently-queued extent. Messages
    /// that arrive mid-drain are handled on the next loop iteration.
    ///
    /// That bound decides how much work one turn does, and nothing more. The
    /// frame between two turns comes from the caller's select, where the pty
    /// arm sits behind every other one, and from the yield that turn ends on.
    fn drain_pending(&mut self, events: &mut UnboundedReceiver<Event>) -> (UpdateEffect, usize) {
        let mut effect = UpdateEffect::None;
        let mut coalesced = 0;

        while let Ok(event) = events.try_recv() {
            effect = effect.merge(self.update(event));
            coalesced += 1;
        }
        let mut pty_bytes = 0;
        while pty_bytes < PTY_TURN_BUDGET_BYTES {
            let Ok(notif) = self.pty_rx.try_recv() else {
                break;
            };
            pty_bytes += notif.payload_len();
            effect = effect.merge(self.handle_pty_notification(notif));
            coalesced += 1;
        }
        while let Ok(ev) = self.agent_event_rx.try_recv() {
            effect = effect.merge(self.handle_agent_event(ev));
            coalesced += 1;
        }
        while let Ok(ctl) = self.agent_control_rx.try_recv() {
            effect = effect.merge(self.handle_agent_control(ctl));
            coalesced += 1;
        }
        while let Ok(msg) = self.window_ipc_rx.try_recv() {
            effect = effect.merge(self.handle_window_ipc(msg));
            coalesced += 1;
        }
        while let Ok(present) = self.stoatty_rx.try_recv() {
            effect = effect.merge(self.handle_stoatty_present(present));
            coalesced += 1;
        }
        // A report drained here rather than through the select arm still has to
        // release a workspace waiting on it.
        effect = effect.merge(ssh::reconnect_when_ready(self));
        // The last report wins: an intermediate size from a drag that is already
        // over describes a window that no longer exists.
        while let Ok(cell_pixels) = self.cell_pixels_rx.try_recv() {
            self.cell_pixels = cell_pixels;
            coalesced += 1;
        }
        self.drain_index_updates();

        (effect, coalesced)
    }

    /// Watches the working tree of the repository that holds the active
    /// workspace's root, and reports whether there is one.
    ///
    /// Records that tree's root as [`Workspace::repo_root`]. A tree already
    /// watched costs nothing. A root outside a repository gets no watch, for
    /// the reason [`Self::start_index_build`] gives.
    pub(crate) fn watch_active_root(&mut self) -> bool {
        let root = self.active_workspace().git_root.clone();
        let Some(repo) = self.git_host.discover(&root) else {
            self.active_workspace_mut().repo_root = None;
            return false;
        };
        let tree = repo_tree_root(&root, repo.workdir().as_deref());
        self.active_workspace_mut().repo_root = Some(tree.clone());
        if !self.watched_roots.insert(tree.clone()) {
            return true;
        }

        // The walk reads the tree, which blocks the run loop on a large repo,
        // so it runs on the blocking pool.
        self.executor
            .spawn_blocking({
                let watcher = self.fs_watch_host.clone();
                let fs = self.fs_host.clone();
                move || watch_workspace_dirs(fs.as_ref(), watcher.as_ref(), &tree)
            })
            .detach();
        true
    }

    /// Kick off a background cold build of the active workspace's code index.
    ///
    /// The scan runs on the blocking pool and streams shards back through
    /// [`Self::index_update_rx`], which [`Self::drain_index_updates`] merges
    /// each tick. The worker task is held so the scan is not cancelled.
    ///
    /// Indexing and the recursive fs-watch only run when the workspace root is
    /// inside a git repository. A non-repo root, such as stoat launched from a
    /// bare home directory, returns early without building or watching, so the
    /// index never spans an unbounded tree.
    pub(crate) fn start_index_build(&mut self) {
        let workspace = self.active_workspace;
        let git_root = self.active_workspace().git_root.clone();
        if !self.watch_active_root() {
            tracing::info!(
                target: "stoat::app",
                root = %git_root.display(),
                "workspace root is not in a git repository; code indexing and fs-watching disabled",
            );
            return;
        }
        let index_dir = self.index_dir_for_build(&git_root);

        let handles = crate::code_index::build::IndexBuild {
            fs: self.fs_host.clone(),
            languages: self.language_registry.clone(),
            tx: self.index_update_tx.clone(),
            redraw: self.redraw_notify.clone(),
            drain: self.drain_notify.clone(),
        };
        self._index_build_task = Some(crate::code_index::build::build_index(
            &self.executor,
            handles,
            git_root,
            workspace,
            index_dir,
            self.finder_path_epoch,
        ));
    }

    /// File a completed index build's walk as the finder's cached path list.
    ///
    /// The build walks the whole workspace tree, and the finder walks the same
    /// one on its first open. Filing it here is what makes that first open
    /// instant on a repository large enough for the walk to be felt.
    ///
    /// Takes only what nothing else holds. A cache already filed stands, a
    /// finder open has the paths in hand, and an epoch the tree moved past
    /// since the build spawned names a list the events already overtook.
    ///
    /// `display` holds the rows the build derived for `walked`, which spares
    /// the first open a row per path on the loop.
    fn seed_finder_paths(
        &mut self,
        workspace: WorkspaceId,
        walked: Vec<PathBuf>,
        epoch: u64,
        display: Option<DisplayCache>,
    ) {
        if self.finder_path_cache.is_some()
            || self.file_finder.is_some()
            || epoch != self.finder_path_epoch
        {
            return;
        }
        let Some(ws) = self.workspaces.get(workspace) else {
            return;
        };

        self.finder_path_cache = Some(FinderPathCache {
            root: ws.git_root.clone(),
            paths: Arc::new(walked),
            epoch,
            display,
        });
    }

    /// Where the index for `git_root` persists, or `None` when it does not.
    ///
    /// Only resolves the directory. Whether a usable manifest sits in it, and so
    /// whether the build runs warm or cold, is the build job's to discover, the
    /// read being the part worth keeping off this thread.
    fn index_dir_for_build(&self, git_root: &Path) -> Option<PathBuf> {
        if self.persistence_disabled {
            return None;
        }
        crate::code_index::store::index_dir_for(git_root, self.fs_host.as_ref()).ok()
    }

    /// A workspace's index directory, resolved once per drain.
    ///
    /// Resolution canonicalizes the git root, so a drain touching hundreds of
    /// files would otherwise repeat that syscall per update. `memo` carries the
    /// answers, `None` meaning this workspace persists nothing.
    fn index_dir_for_workspace(
        &self,
        workspace: WorkspaceId,
        memo: &mut std::collections::HashMap<WorkspaceId, Option<PathBuf>>,
    ) -> Option<PathBuf> {
        if let Some(dir) = memo.get(&workspace) {
            return dir.clone();
        }
        let dir = self
            .workspaces
            .get(workspace)
            .map(|ws| ws.git_root.clone())
            .and_then(|git_root| self.index_dir_for_build(&git_root));
        memo.insert(workspace, dir.clone());
        dir
    }

    /// Merge pending index updates into their workspace graphs.
    ///
    /// Reindex and remove updates apply without re-resolving inline. Every
    /// touched workspace has its cross-file references re-resolved once after
    /// the drain, so N queued updates cost one graph sweep rather than N.
    ///
    /// Nothing here touches the disk. Shards to write, shards to delete and
    /// manifest edits are gathered as the updates are merged, then handed to a
    /// blocking thread once, which is what keeps a drain covering a whole
    /// checkout from performing a write per file between two frames.
    ///
    /// The turn ends at [`INDEX_DRAIN_CAP`] updates or [`INDEX_DRAIN_BUDGET`]
    /// of merging, whichever comes first. On either the drain schedules a
    /// redraw and returns, leaving the remainder queued for the next turn.
    ///
    /// The re-resolution after the loop is outside both bounds. It runs once
    /// and has to see every update the loop merged.
    pub(crate) fn drain_index_updates(&mut self) {
        let started = std::time::Instant::now();
        let mut resolve_pending: std::collections::HashSet<WorkspaceId> =
            std::collections::HashSet::new();
        let mut completed: std::collections::HashSet<WorkspaceId> =
            std::collections::HashSet::new();
        let mut drained: usize = 0;
        let mut dirs: std::collections::HashMap<WorkspaceId, Option<PathBuf>> =
            std::collections::HashMap::new();
        let mut writes: std::collections::HashMap<PathBuf, IndexWrites> =
            std::collections::HashMap::new();
        while let Ok(update) = self.index_update_rx.try_recv() {
            drained += 1;
            match update {
                IndexUpdate::Shard {
                    workspace,
                    rel_path,
                    shard,
                } => {
                    let Some(ws) = self.workspaces.get_mut(workspace) else {
                        continue;
                    };
                    ws.code_graph.insert_shard(shard);
                    ws.file_paths.insert(
                        crate::code_index::build::file_id(&rel_path),
                        PathBuf::from(&rel_path),
                    );
                    ws.index_generation += 1;
                },
                IndexUpdate::Complete {
                    workspace,
                    manifest,
                    walked,
                    walk_epoch,
                    display,
                } => {
                    resolve_pending.insert(workspace);
                    completed.insert(workspace);
                    self.seed_finder_paths(workspace, walked, walk_epoch, display);
                    if let Some(dir) = self.index_dir_for_workspace(workspace, &mut dirs) {
                        writes.entry(dir).or_default().completed = Some(manifest);
                    }
                },
                IndexUpdate::Reindex {
                    workspace,
                    file,
                    rel_path,
                    shard,
                    persist,
                } => {
                    let to_persist =
                        persist.then(|| (codegraph::encode_shard(&shard), shard.content_hash));
                    let Some(ws) = self.workspaces.get_mut(workspace) else {
                        continue;
                    };
                    ws.code_graph.apply_reindex(file, shard);
                    ws.file_paths.insert(file, PathBuf::from(&rel_path));
                    ws.index_generation += 1;
                    resolve_pending.insert(workspace);
                    if let Some((bytes, content_hash)) = to_persist
                        && let Some(dir) = self.index_dir_for_workspace(workspace, &mut dirs)
                    {
                        let entry = writes.entry(dir).or_default();
                        entry.shards.push((rel_path.clone(), bytes));
                        entry.manifest_edits.push(ManifestEdit::Set {
                            rel_path,
                            content_hash,
                        });
                    }
                },
                IndexUpdate::Remove {
                    workspace,
                    file,
                    rel_path,
                } => {
                    let Some(ws) = self.workspaces.get_mut(workspace) else {
                        continue;
                    };
                    ws.code_graph.apply_remove(file);
                    ws.file_paths.remove(&file);
                    ws.index_generation += 1;
                    resolve_pending.insert(workspace);
                    if let Some(dir) = self.index_dir_for_workspace(workspace, &mut dirs) {
                        let entry = writes.entry(dir).or_default();
                        entry.deleted_shards.push(rel_path.clone());
                        entry.manifest_edits.push(ManifestEdit::Remove { rel_path });
                    }
                },
            }

            if index_turn_spent(drained, started.elapsed()) {
                // The remainder merges on the next turn, and merging it shows
                // nothing, so the wake asks for the turn and not for a frame.
                self.drain_notify.notify_one();
                break;
            }
        }

        if !writes.is_empty() {
            let fs = self.fs_host.clone();
            self.executor
                .spawn_blocking(move || {
                    for (dir, batch) in writes {
                        match crate::code_index::store::apply_index_writes(&dir, batch, fs.as_ref())
                        {
                            Ok(pruned) if pruned > 0 => tracing::info!(
                                target: "stoat::app",
                                pruned,
                                "pruned stale index shards",
                            ),
                            Ok(_) => {},
                            Err(err) => tracing::warn!(
                                target: "stoat::app",
                                %err,
                                dir = %dir.display(),
                                "index writes failed",
                            ),
                        }
                    }
                })
                .detach();
        }

        for workspace in resolve_pending {
            if let Some(ws) = self.workspaces.get_mut(workspace) {
                ws.code_graph.reresolve_unresolved();
                // Counting the edges walks every live one of them twice, which
                // is the whole graph for a line no subscriber takes.
                if completed.contains(&workspace)
                    && tracing::enabled!(target: "stoat::app", tracing::Level::INFO)
                {
                    let stats = ws.code_graph.stats();
                    tracing::info!(
                        target: "stoat::app",
                        symbols = stats.symbols,
                        edges = stats.edges,
                        unresolved = stats.unresolved_edges,
                        "code graph resolved after index build",
                    );
                }
            }
        }

        let elapsed = started.elapsed();
        if drained > 0 && elapsed > SLOW_DRAIN_THRESHOLD {
            tracing::warn!(
                target: "stoat::app",
                drained,
                elapsed_ms = elapsed.as_millis() as u64,
                "index update drain exceeded the slow threshold",
            );
        }
    }

    /// Rehydrate the active workspace from its most-recently-modified
    /// persisted file under `$XDG_STATE_HOME/stoat/workspaces/<hash>/`. The
    /// binary only invokes this when the user passes `--continue`; a bare
    /// `stoat` launch leaves the default fresh workspace in place so each
    /// session starts clean. Tests intentionally skip this to stay isolated
    /// from the real state directory.
    pub fn load_active_workspace_state(&mut self) {
        let git_root = self.active_workspace().git_root.clone();
        let files = match crate::workspace::list_workspace_files(&git_root, &*self.fs_host) {
            Ok(files) => files,
            Err(err) => {
                tracing::warn!(?err, "could not resolve workspace state directory");
                return;
            },
        };
        let Some(path) = files.into_iter().next() else {
            return;
        };
        let workspace = self.active_workspace;
        self.spawn_workspace_restore(workspace, path);
    }

    /// Kick off an off-thread restore of `workspace` from `path`.
    ///
    /// Shows a "restoring session" badge and replays the persisted buffers on
    /// the blocking pool. [`Self::install_pending_workspace_restore`] installs
    /// the result on the next [`Self::drive_background`], or drops it if the
    /// workspace stopped being fresh while the restore ran. Keeping the read and
    /// op-log replay off the main thread lets the first frame paint immediately.
    pub(crate) fn spawn_workspace_restore(&mut self, workspace: WorkspaceId, path: PathBuf) {
        if let Some(ws) = self.workspaces.get_mut(workspace) {
            ws.badges
                .remove_by_source(crate::badge::BadgeSource::SessionRestore);
            ws.badges.insert(crate::badge::Badge {
                source: crate::badge::BadgeSource::SessionRestore,
                anchor: crate::badge::Anchor::BottomRight,
                state: crate::badge::BadgeState::Active,
                label: "restoring session".to_string(),
                detail: None,
            });
        }

        let executor = self.executor.clone();
        let fs_host = self.fs_host.clone();
        let pending = self.pending_workspace_restore.clone();
        self.spawn_woken(async move {
            let outcome = executor
                .spawn_blocking({
                    let path = path.clone();
                    move || crate::workspace::persist::read_restore_parts(&path, &*fs_host)
                })
                .await;
            *pending.lock().expect("pending workspace restore mutex") =
                Some(PendingWorkspaceRestore {
                    workspace,
                    path,
                    outcome,
                });
        })
        .detach();
    }

    /// Install a finished workspace restore, or drop it.
    ///
    /// Drains [`Self::pending_workspace_restore`], a no-op when nothing
    /// finished. The "restoring session" badge clears regardless of outcome. A
    /// read or parse error logs and leaves the fresh workspace in place. A
    /// target the user edited while the restore ran, no longer
    /// [`Workspace::is_fresh`], is left untouched so live state is never
    /// clobbered. Otherwise the buffers and panes install. When the target is
    /// still active, the restored files open with their language servers and
    /// terminals respawn. A target the reader switched away from waits for
    /// [`Self::start_background_restore`] instead.
    fn install_pending_workspace_restore(&mut self) {
        let pending = self
            .pending_workspace_restore
            .lock()
            .expect("pending workspace restore mutex")
            .take();
        let Some(PendingWorkspaceRestore {
            workspace,
            path,
            outcome,
        }) = pending
        else {
            return;
        };

        if let Some(ws) = self.workspaces.get_mut(workspace) {
            ws.badges
                .remove_by_source(crate::badge::BadgeSource::SessionRestore);
        }

        let (buffers, state) = match outcome {
            Ok(parts) => parts,
            Err(err) => {
                tracing::warn!(
                    ?path,
                    ?err,
                    "failed to restore workspace state; starting fresh"
                );
                return;
            },
        };

        if !self
            .workspaces
            .get(workspace)
            .is_some_and(|ws| ws.is_fresh())
        {
            tracing::warn!(
                ?path,
                "workspace changed before session restore landed; dropping restore"
            );
            return;
        }

        let registry = self.language_registry.clone();
        let executor = self.executor.clone();
        if let Some(ws) = self.workspaces.get_mut(workspace) {
            ws.install_restored(buffers, state, &executor);
            ws.assign_languages_from_paths(&registry);
        }
        if self.active_workspace != workspace {
            if let Some(ws) = self.workspaces.get_mut(workspace) {
                ws.restored_in_background = true;
            }
            return;
        }
        crate::lsp::drain::reopen_buffers(self, None);
        action_handlers::respawn_terminal_panes(self);
        if self.active_workspace().remote.is_some() {
            self.remote_pending = true;
            ssh::reconnect_when_ready(self);
        }
    }

    /// Start the restored buffers and terminal panes of the active workspace
    /// when its restore installed while another workspace was active.
    ///
    /// A server spawn and a terminal shell read the active workspace's root and
    /// environment, so this waits for the switch. Running from
    /// [`Self::drive_background`] serves every path that makes the workspace
    /// active. The environment load has started by then, so a subprocess
    /// server still waits for the direnv diff, as at an in-place restore.
    ///
    /// The remote reconnect needs no deferral, because every workspace switch
    /// reconnects a remote workspace.
    fn start_background_restore(&mut self) {
        if !std::mem::take(&mut self.active_workspace_mut().restored_in_background) {
            return;
        }
        crate::lsp::drain::reopen_buffers(self, None);
        action_handlers::respawn_terminal_panes(self);
    }

    /// Persist a workspace's state, serializing it off this thread.
    ///
    /// Runs on every workspace switch, open and close, and on the input-armed
    /// throttle that bounds how much of a live session a crash costs, so the
    /// snapshot is taken here and the RON encoding and writes happen on the
    /// blocking pool. What lands on disk is the workspace as it stood at this
    /// call, whatever it does afterwards.
    ///
    /// A second save of the same workspace replaces the first's task. Snapshots
    /// are ordered by this thread and every write is atomic, so the most a
    /// late-landing earlier write costs is a stale file the next save replaces.
    ///
    /// No-op when [`Self::persistence_disabled`] is set (used by the test
    /// harness to keep the real `$XDG_STATE_HOME` pristine) or when the
    /// workspace is still in its freshly-created state per
    /// [`Workspace::is_fresh`], so launches without `--continue` do not
    /// write a throwaway session file.
    pub(crate) fn save_workspace(&mut self, workspace: WorkspaceId) {
        let Some(ws) = self.workspaces.get(workspace) else {
            return;
        };
        let Some(path) = self.workspace_save_path(ws) else {
            return;
        };
        let (state, meta) = (ws.to_state(), ws.meta());

        let fs = self.fs_host.clone();
        let task = self.executor.spawn_blocking(move || {
            // The snapshot resolved its selection endpoints on the run loop.
            // Re-taking them rebuilds a buffer per compacted one an editor
            // shows, which is why that half happens here.
            let mut state = state;
            state.resolve_reanchors();
            if let Err(err) = crate::workspace::write_state(&state, &meta, &path, fs.as_ref()) {
                tracing::warn!(?path, ?err, "failed to save workspace state");
            }
        });
        self.pending_workspace_saves.insert(workspace, task);
    }

    /// Persist a workspace's state before returning.
    ///
    /// For callers whose next statement depends on the write having happened.
    /// Quit breaks out of the run loop immediately after, and closing a
    /// workspace deletes the files this writes. A deferred write would lose the
    /// race in the first case and win it in the second, resurrecting the state
    /// of a workspace that was just closed.
    pub(crate) fn save_workspace_now(&self, ws: &Workspace) {
        let Some(path) = self.workspace_save_path(ws) else {
            return;
        };
        if let Err(err) = ws.save_state(&path, &*self.fs_host) {
            tracing::warn!(?path, ?err, "failed to save workspace state");
        }
    }

    /// Where `ws` persists, or `None` when it should not be persisted at all.
    fn workspace_save_path(&self, ws: &Workspace) -> Option<PathBuf> {
        if self.persistence_disabled || ws.is_fresh() {
            return None;
        }
        match crate::workspace::state_path_for(&ws.git_root, ws.uid, &*self.fs_host) {
            Ok(path) => Some(path),
            Err(err) => {
                tracing::warn!(?err, "could not resolve workspace state path");
                None
            },
        }
    }

    /// Persist every open workspace. Invoked on quit so workspaces that were
    /// left in the background get their latest state written out.
    fn save_all_workspaces(&self) {
        for ws in self.workspaces.values() {
            self.save_workspace_now(ws);
        }
    }

    /// Apply what the outside world pushed at the editor since the last pass,
    /// and report whether any of it dispatched.
    ///
    /// Language servers and the debounce timers both deliver on their own
    /// schedule, so every entry point that is about to act on editor state
    /// drains them first. Written once here because a drain enumerated in one
    /// entry point and not another silently never runs there.
    ///
    /// [`debounce::drain_fs_watch_events`] is deliberately absent. It arms the
    /// debounce windows rather than dispatching them, so it has nothing to
    /// report and belongs at the event edge.
    fn drain_external(&mut self) -> bool {
        crate::lsp::drain::drain_lsp_notifications(self);
        crate::lsp::drain::drain_lsp_incoming_requests(self);
        crate::lsp::drain::install_pending_lsp_host(self);

        let diff_refresh = debounce::drain_pending_diff_refresh(self);
        let code_search = debounce::drain_pending_code_search(self);
        let index_edits = debounce::drain_pending_index_edits(self);
        let autosave = debounce::drain_pending_workspace_autosave(self);
        let signature_help = debounce::drain_pending_signature_help(self);

        diff_refresh || code_search || index_edits || autosave || signature_help
    }

    pub(crate) fn update(&mut self, event: Event) -> UpdateEffect {
        // A remote session owns the keyboard. These were already parsed out of
        // the byte stream before the switch, so they are the tail of what the
        // user typed at this editor and belong to nobody now. A resize still
        // lands, or this side lays out against a stale grid on return.
        if self.passthrough.is_some()
            && matches!(event, Event::Key(_) | Event::Mouse(_) | Event::Paste(_))
        {
            return UpdateEffect::None;
        }

        debounce::drain_fs_watch_events(self);
        self.drain_external();

        // Only what the user did. A resize arms nothing because their next
        // input does, and fs-watch or LSP traffic moves disk-backed state the
        // session file does not own.
        let user_input = matches!(
            &event,
            Event::Key(key) if key.kind == KeyEventKind::Press,
        ) || matches!(&event, Event::Mouse(_) | Event::Paste(_));

        let effect = match event {
            Event::Resize(w, h) => {
                self.size = Rect::new(0, 0, w, h);
                let size = self.size;
                self.active_workspace_mut().layout(size);
                // A drag reports a resize per column or row it crosses, and each
                // one moves every pool's rectangle, so the pools hold their
                // fills until this window closes.
                debounce::arm_pool_settle(self);
                UpdateEffect::Redraw
            },
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let scrolloff = self.settings.scrolloff.unwrap_or(3);

                // A key pressed mid wheel-glide first clamps the anchored cursor
                // into the landing scrolloff band, so the deferred follow cannot
                // strand it off-screen and `ensure_cursor_in_view` cannot snap
                // the view backward to where the cursor used to be.
                if let Some(editor) = action_handlers::focused_editor_mut(self)
                    && editor.scroll_glide == ScrollGlide::Wheel
                {
                    action_handlers::view::clamp_cursor_to_view(editor, scrolloff);
                }

                let before = self.focused_cursor_pos();
                let workspace_before = self.active_workspace;
                let term_before = self.focused_term_id();
                let effect = self.handle_key(key);
                self.rest_left_terminal(workspace_before, term_before);
                let cursor_moved = self.focused_cursor_pos() != before;

                // Re-follow the cursor when a key moved it, pulling the view
                // along so a count jump past the margin lands the view on the
                // cursor rather than stranding it on the edge. A keyboard scroll
                // (z j / z k) never moves the cursor, so its view stays put.
                let scrolled = match action_handlers::focused_editor_mut(self) {
                    Some(editor) => {
                        cursor_moved && action_handlers::view::follow_jump(editor, scrolloff)
                    },
                    None => false,
                };

                if scrolled {
                    effect.merge(UpdateEffect::Redraw)
                } else {
                    effect
                }
            },
            Event::Mouse(mouse) => {
                let workspace_before = self.active_workspace;
                let term_before = self.focused_term_id();
                let effect = mouse::handle_mouse(self, mouse);
                self.rest_left_terminal(workspace_before, term_before);
                effect
            },
            Event::Paste(text) => self.handle_paste(&text),
            _ => UpdateEffect::None,
        };
        crate::lsp::sync::notify_buffer_changes_pending(self);
        crate::completion::request::trigger(self);
        crate::lsp::signature_help::signature_help_trigger(self);
        action_handlers::lsp::inlay_hints_trigger(self);
        crate::lsp::document_highlight::document_highlight_trigger(self);
        crate::lsp::pull_diagnostics::pull_diagnostics_trigger(self);
        crate::lsp::semantic_tokens::semantic_tokens_trigger(self);
        crate::lsp::folding::folding_ranges_trigger(self);

        if user_input {
            debounce::arm_workspace_autosave(self);
        }

        effect
    }

    /// Spawn `future` on the executor and wake the run loop once it
    /// resolves, so a background result that drives a render lands
    /// without waiting for the next keystroke.
    ///
    /// Binds [`Executor::spawn_with_redraw`] to this app's
    /// [`Self::redraw_notify`]. The wake fires inside the returned task's
    /// final poll, so [`Self::run`]'s `drive_background` always polls a
    /// completed task when it observes the notification.
    pub(crate) fn spawn_woken<F>(&self, future: F) -> stoat_scheduler::Task<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.executor
            .spawn_with_redraw(self.redraw_notify.clone(), future)
    }

    /// Show `text` as the transient status message for [`STATUS_MESSAGE_TTL`].
    ///
    /// Stamps a fresh deadline and arms a timer that wakes the run loop when it
    /// elapses, so an idle screen retires the message on its own. A later call
    /// replaces the message and cancels the prior timer.
    ///
    /// The status row paints one line, so `text` is flattened into one. A
    /// message carrying the output of a failed command reaches here with the
    /// newlines and control characters that output had.
    pub(crate) fn set_status(&mut self, text: impl Into<String>) {
        self.pending_message = Some(sanitize::sanitize_status_text(&text.into()));
        self.pending_message_deadline = Some(self.executor.now() + STATUS_MESSAGE_TTL);

        let timer = self.executor.timer(STATUS_MESSAGE_TTL);
        self.pending_message_expiry = Some(self.spawn_woken(async move {
            timer.await;
        }));
    }

    /// Whether any background diff warm is in flight, driving the status bar's
    /// transient diff spinner segment and keeping the frame clock ticking so it
    /// animates.
    pub(crate) fn diff_warm_busy(&self) -> bool {
        self.pending_diff_warm.is_some()
    }

    /// The binding `key` resolves to, deriving it on the first reader and
    /// answering the rest from `memo`.
    ///
    /// Takes the memo by reference rather than holding it on the pass, because
    /// it is only good for the one key press it was derived for and a field
    /// would outlive that.
    fn keymap_lookup<'memo>(
        &self,
        key: &KeyEvent,
        memo: &'memo mut KeymapLookup,
    ) -> &'memo Option<BoundActions> {
        memo.0.get_or_insert_with(|| {
            #[cfg(test)]
            self.keymap_lookups.set(self.keymap_lookups.get() + 1);

            let state = StoatKeymapState::from_stoat(self);
            self.keymap.lookup_with_capture(&state, key)
        })
    }

    fn handle_key(&mut self, key: KeyEvent) -> UpdateEffect {
        debug_assert_modal_exclusivity(self);

        // A version notice is a one-shot message. Any key press retires it.
        self.badges
            .remove_by_source(crate::badge::BadgeSource::Version);
        self.lsp_message = None;

        // The keymap state and binding lookup are derived at most once per press
        // and only for a press that reads them. `from_stoat` is expensive (two
        // buffer read locks, a snapshot clone, mode and language allocations)
        // and the lookup scans every compiled binding, while the busiest keys
        // want neither. A printable insert character types without consulting
        // the keymap. Terminal passthrough reads a scoped lookup of its own,
        // and none for a printable key.
        //
        // Deriving late is sound for the same reason deriving once was. None of
        // the fall-through mutations between the readers below feed a keymap
        // predicate, so the state is the same wherever it is read.
        //
        // Normalization runs first so the Ctrl-C block matches on the same key
        // the lookup used.
        let key = normalize_shift_event(key);
        let mut lookup = KeymapLookup::default();

        // This line diagnoses a key that appears dead in a running build. It
        // names which layer dropped the press. An absent line means the event
        // never arrived, an unexpected modal or mode means the keymap context
        // was wrong, and a `None` action field means no binding matched. It is
        // silent under the default `stoat=info` filter.
        tracing::debug!(
            target: "stoat::keys",
            code = ?key.code,
            mods = ?key.modifiers,
            modal = ?modal_predicate(self),
            mode = %self.focused_mode(),
            // Evaluated only when the filter enables this line, so a release
            // run never derives the lookup for it and a debug run keeps
            // today's diagnostics.
            actions = ?self
                .keymap_lookup(&key, &mut lookup)
                .as_ref()
                .map(|(actions, _)| {
                    actions
                        .iter()
                        .map(|a| {
                            if keymap::is_unknown_action(&a.name) {
                                format!("{} (unknown)", a.name)
                            } else {
                                a.name.clone()
                            }
                        })
                        .collect::<Vec<String>>()
                }),
            "key dispatch"
        );

        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if keymap_state::close_topmost_modal(self) {
                return UpdateEffect::Redraw;
            }
            if self.pending_hover.is_some() {
                self.pending_hover = None;
                self.pending_hover_request = None;
                return UpdateEffect::Redraw;
            }
            if let Some(agent_id) = self.term_input_target() {
                mouse::clear_term_selection(self, agent_id);
                let effect = self.show_term_live_screen(agent_id);
                self.write_to_term(agent_id, &[0x03]);
                return effect;
            }
            // Ctrl-C with a keymap binding (`pane == run` -> RunInterrupt) routes
            // through the keymap below. An unbound Ctrl-C quits.
            if self.keymap_lookup(&key, &mut lookup).is_none() {
                return UpdateEffect::Quit;
            }
        }

        // The zoom combo and the diff digit chords, arriving as bytes because
        // the host was asked to deliver them that way. Over the window socket
        // the same press never reaches here at all, being routed straight to
        // the same step, so this sits ahead of every reader that would
        // otherwise take it: terminal passthrough would hand it to a run pane's
        // child, macro capture would record a press the socket transport does
        // not, and the keymap would resolve `=` or `-` to whatever the mode
        // binds, which in insert mode is typing the character.
        //
        // Only the five digits the shipped config binds as diff chords. Any
        // other super-digit falls to the keymap below, the same lookup the
        // socket delivery reaches through the chord handler, so both deliveries
        // treat it alike.
        //
        // Exactly super, not super among others. That is what the host writes,
        // and a press carrying more is a different chord the keymap should get.
        if key.modifiers == KeyModifiers::SUPER {
            match key.code {
                KeyCode::Char('=') => return self.handle_zoom_step(1),
                KeyCode::Char('-') => return self.handle_zoom_step(-1),
                KeyCode::Char(ch @ ('6' | '7' | '8' | '9' | '0')) => return self.handle_chord(ch),
                _ => {},
            }
        }

        if let Some(count) = self.pending_macro_replay.take() {
            if let KeyCode::Char(ch) = key.code {
                // The register name is half of what a recording needs to replay
                // this later, and returning here is what would skip the capture
                // every other key goes through below.
                action_handlers::macro_recording::capture(self, &key);
                return action_handlers::macro_recording::execute_replay(self, ch, count);
            }
            return UpdateEffect::Redraw;
        }

        // Only a session that is recording can be toggled out of it, and the
        // false branch calls `capture`, which is itself a no-op with nothing
        // recording. So a session that never records never looks this up.
        let is_record_macro_toggle = self.macro_recording.is_some()
            && self
                .keymap_lookup(&key, &mut lookup)
                .as_ref()
                .is_some_and(|(actions, _)| actions.iter().any(|a| a.name == "RecordMacro"));
        if !is_record_macro_toggle {
            action_handlers::macro_recording::capture(self, &key);
        }

        if let Some(run_id) = self.modal_run {
            let running = self
                .active_workspace()
                .runs
                .get(run_id)
                .is_some_and(|r| r.is_running());
            if running {
                // Swallow input while the command is still running.
                return UpdateEffect::None;
            }
            // Once finished, keys fall through so the `modal == run` bindings
            // (Escape -> RunModalDismiss) resolve through the keymap.
        }

        if let Some(term_id) = self.term_input_target() {
            if let Some((actions, captured_digit)) = self.passthrough_binding(&key) {
                return self.run_bound_actions(
                    &actions,
                    captured_digit,
                    matches!(key.code, KeyCode::Esc),
                );
            }
            return self.route_key_to_term(term_id, key);
        }

        // The guards below all turn on the mode, and resolving it walks the
        // modal stack and clones a pane-tree view, so it is resolved once here
        // and read as the three questions they ask of it. The answer is taken
        // rather than borrowed because the chain mutates as it goes, and taken
        // apart rather than copied because a String per keystroke is the sort
        // of cost this avoids.
        //
        // One answer serves the whole chain because nothing in it changes the
        // mode. The insert block returns on every path that mutates, and each
        // guard below clears only its own pending flag. The assertion at the
        // end of the chain is what keeps that true.
        let (insert_mode, normal_mode, takes_pending) = {
            let mode = self.focused_mode();
            (
                mode == "insert",
                mode == "normal",
                mode == "normal" || mode == "select",
            )
        };

        if insert_mode {
            // A non-printable key the keymap binds falls through to the lookup
            // below, so bindings like `pane == run { Enter -> RunSubmit }`
            // override the built-in insert arms. Printable characters always
            // type, and an unbound key keeps today's insert defaults.
            let printable = is_printable_key(&key);
            // handle_insert_key keeps priority for printable typing and for its
            // transient sub-modes (a completion popup, a pending insert
            // register), whose keys it owns. Otherwise a keymap binding for a
            // non-printable key wins over the built-in defaults. Esc with a
            // completion popup open is the exception. handle_insert_key returns
            // None for it, so it falls through to the keymap and leaves insert.
            let insert_first =
                printable || self.pending_completion.is_some() || self.pending_insert_register;
            // Short-circuits for a printable character, which is what keeps
            // ordinary typing off the lookup entirely.
            let keymap_binds = !insert_first && self.keymap_lookup(&key, &mut lookup).is_some();
            if !keymap_binds && let Some(effect) = self.handle_insert_key(key) {
                // If help is open, keep its filtered list in sync after every
                // text mutation in the prompt input.
                if self.help.is_some() {
                    let active_idx = self.active_workspace;
                    let workspaces = &mut self.workspaces;
                    if let Some(help) = self.help.as_mut() {
                        help.sync_filter(&workspaces[active_idx]);
                    }
                }
                return effect;
            }
        }

        if takes_pending && self.pending_code_action_picker.is_some() {
            if let KeyCode::Char(ch) = key.code {
                match ch {
                    'j' => {
                        if let Some(picker) = self.pending_code_action_picker.as_mut() {
                            let max = picker.entries.len().saturating_sub(1);
                            picker.selected_idx = (picker.selected_idx + 1).min(max);
                        }
                        return UpdateEffect::Redraw;
                    },
                    'k' => {
                        if let Some(picker) = self.pending_code_action_picker.as_mut() {
                            picker.selected_idx = picker.selected_idx.saturating_sub(1);
                        }
                        return UpdateEffect::Redraw;
                    },
                    _ => {},
                }
                if let Some(digit) = ch.to_digit(10)
                    && (1..=9).contains(&digit)
                {
                    let viewport_top = self
                        .pending_code_action_picker
                        .as_ref()
                        .map(|p| {
                            crate::render::symbol_picker::viewport_top_for_picker(
                                p.selected_idx,
                                p.entries.len(),
                            )
                        })
                        .unwrap_or(0);
                    let index = viewport_top + (digit as usize - 1);
                    action_handlers::lsp::pick_code_action(self, index);
                    return UpdateEffect::Redraw;
                }
            }
            if matches!(key.code, KeyCode::Down) {
                if let Some(picker) = self.pending_code_action_picker.as_mut() {
                    let max = picker.entries.len().saturating_sub(1);
                    picker.selected_idx = (picker.selected_idx + 1).min(max);
                }
                return UpdateEffect::Redraw;
            }
            if matches!(key.code, KeyCode::Up) {
                if let Some(picker) = self.pending_code_action_picker.as_mut() {
                    picker.selected_idx = picker.selected_idx.saturating_sub(1);
                }
                return UpdateEffect::Redraw;
            }
            if matches!(key.code, KeyCode::Enter) {
                let index = self
                    .pending_code_action_picker
                    .as_ref()
                    .map(|p| p.selected_idx);
                if let Some(index) = index {
                    action_handlers::lsp::pick_code_action(self, index);
                    return UpdateEffect::Redraw;
                }
            }
            if matches!(key.code, KeyCode::Esc) {
                self.pending_code_action_picker = None;
                self.pending_code_action_request = None;
                return UpdateEffect::Redraw;
            }
            self.pending_code_action_picker = None;
            self.pending_code_action_request = None;
        }

        if takes_pending && self.pending_symbol_picker.is_some() {
            if let KeyCode::Char(ch) = key.code {
                match ch {
                    'j' => {
                        if let Some(picker) = self.pending_symbol_picker.as_mut() {
                            let max = picker.entries.len().saturating_sub(1);
                            picker.selected_idx = (picker.selected_idx + 1).min(max);
                        }
                        return UpdateEffect::Redraw;
                    },
                    'k' => {
                        if let Some(picker) = self.pending_symbol_picker.as_mut() {
                            picker.selected_idx = picker.selected_idx.saturating_sub(1);
                        }
                        return UpdateEffect::Redraw;
                    },
                    _ => {},
                }
                if let Some(digit) = ch.to_digit(10)
                    && (1..=9).contains(&digit)
                {
                    let viewport_top = self
                        .pending_symbol_picker
                        .as_ref()
                        .map(|p| {
                            crate::render::symbol_picker::viewport_top_for_picker(
                                p.selected_idx,
                                p.entries.len(),
                            )
                        })
                        .unwrap_or(0);
                    let index = viewport_top + (digit as usize - 1);
                    action_handlers::lsp::pick_symbol(self, index);
                    return UpdateEffect::Redraw;
                }
            }
            if matches!(key.code, KeyCode::Down) {
                if let Some(picker) = self.pending_symbol_picker.as_mut() {
                    let max = picker.entries.len().saturating_sub(1);
                    picker.selected_idx = (picker.selected_idx + 1).min(max);
                }
                return UpdateEffect::Redraw;
            }
            if matches!(key.code, KeyCode::Up) {
                if let Some(picker) = self.pending_symbol_picker.as_mut() {
                    picker.selected_idx = picker.selected_idx.saturating_sub(1);
                }
                return UpdateEffect::Redraw;
            }
            if matches!(key.code, KeyCode::Enter) {
                let index = self.pending_symbol_picker.as_ref().map(|p| p.selected_idx);
                if let Some(index) = index {
                    action_handlers::lsp::pick_symbol(self, index);
                    return UpdateEffect::Redraw;
                }
            }
            if matches!(key.code, KeyCode::Esc) {
                self.pending_symbol_picker = None;
                self.pending_symbol_picker_request = None;
                return UpdateEffect::Redraw;
            }
            self.pending_symbol_picker = None;
            self.pending_symbol_picker_request = None;
        }

        // A hover popup consumes half-page scroll keys and auto-closes on any
        // other key (Helix's popup behavior). Ctrl-d/PageDown and Ctrl-u/PageUp
        // scroll it while open, shadowing normal-mode half-page motion. Escape
        // closes and is consumed. Every other key closes it and then dispatches,
        // which also covers the SetMode-only keys that `continue` before the
        // post-dispatch clear below. Ctrl-c is consumed by the close in the
        // Ctrl-c block above, so it never reaches here.
        if takes_pending && self.pending_hover.is_some() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let scroll_down =
                matches!(key.code, KeyCode::PageDown) || (ctrl && key.code == KeyCode::Char('d'));
            let scroll_up =
                matches!(key.code, KeyCode::PageUp) || (ctrl && key.code == KeyCode::Char('u'));
            if scroll_down || scroll_up {
                if let Some(popup) = self.pending_hover.as_mut() {
                    if scroll_down {
                        popup.scroll_half_pages += 1;
                    } else {
                        popup.scroll_half_pages = popup.scroll_half_pages.saturating_sub(1);
                    }
                }
                return UpdateEffect::Redraw;
            }
            // `y` yanks a live hover selection into the register and keeps the
            // popup and selection open. With no selection it falls through to
            // the auto-close, so a bare `y` still dispatches as normal.
            if key.code == KeyCode::Char('y') && !ctrl {
                let text = self
                    .pending_hover
                    .as_ref()
                    .map(crate::render::hover::hover_selected_text)
                    .unwrap_or_default();
                if !text.is_empty() {
                    let fragments = text.split('\n').map(String::from).collect();
                    // This yank is intercepted ahead of the dispatch that
                    // spends the selection for every other command, so it
                    // spends its own.
                    self.take_selected_register();
                    let target = self.active_register();
                    action_handlers::yank::write_fragments_to_register(self, target, fragments);
                    self.set_status("yanked hover selection");
                    return UpdateEffect::Redraw;
                }
            }
            // A pinned card stays up through an incidental key. Esc below is
            // deliberate, so it takes the card down like anything else.
            let pinned = self
                .pending_hover
                .as_ref()
                .is_some_and(|popup| popup.pinned);
            if !pinned || matches!(key.code, KeyCode::Esc) {
                self.pending_hover = None;
                self.pending_hover_request = None;
            }
            if matches!(key.code, KeyCode::Esc) {
                return UpdateEffect::Redraw;
            }
        }

        if takes_pending && self.pending_find.is_some() {
            // Enter names a line ending rather than a character to search for.
            // One line ends in LF where the next ends in CRLF, so the target is
            // computed from the row. Tab is an ordinary target that the terminal
            // reports as its own key rather than as the character it stands for.
            if matches!(key.code, KeyCode::Enter) {
                let (kind, extend, count) = self.pending_find.take().expect("checked above");
                return action_handlers::movement::execute_find_line_ending(
                    self, kind, extend, count,
                );
            }
            let target = match key.code {
                KeyCode::Tab => Some('\t'),
                KeyCode::Char(ch) => Some(ch),
                _ => None,
            };
            if let Some(ch) = target {
                let (kind, extend, count) = self.pending_find.take().expect("checked above");
                return action_handlers::movement::execute_find(self, kind, ch, extend, count);
            }
            // Cancelling costs the one press and nothing else, as it does for
            // every chord below. The hover popup and the symbol picker above
            // let the key through on purpose, being popups rather than chords.
            self.pending_find = None;
            return UpdateEffect::Redraw;
        }

        if normal_mode && self.pending_mark.is_some() {
            if let KeyCode::Char(ch) = key.code {
                let request = self.pending_mark.take().expect("checked above");
                return action_handlers::marks::execute_mark(self, request, ch);
            }
            self.pending_mark = None;
            return UpdateEffect::Redraw;
        }

        if takes_pending && self.pending_replace {
            self.pending_replace = false;
            let mut encoded = [0u8; 4];
            let text = match key.code {
                KeyCode::Char(ch) => Some(&*ch.encode_utf8(&mut encoded)),
                // Enter names a line ending and Tab a tab, so both replace a
                // run with whitespace no printable key reaches. Always LF: a
                // buffer holds LF whatever its file uses, and the CRLF a file
                // arrived with is restored on save.
                KeyCode::Enter => Some("\n"),
                KeyCode::Tab => Some("\t"),
                _ => None,
            };
            let Some(text) = text else {
                return UpdateEffect::Redraw;
            };
            return action_handlers::movement::execute_replace(self, text);
        }

        if takes_pending && self.pending_surround_add {
            self.pending_surround_add = false;
            match key.code {
                KeyCode::Char(ch) => {
                    return action_handlers::surround::execute_surround_add(self, ch);
                },
                // Enter names a line ending, which is how a selection gets put
                // on a line of its own. Always LF: a buffer holds LF whatever
                // its file uses, and the CRLF a file arrived with is restored
                // on save. Inserting one here leaves a stray carriage return in
                // the text instead.
                KeyCode::Enter => {
                    return action_handlers::surround::execute_surround_add_pair(self, "\n", "\n");
                },
                // Cancelling costs the one press and nothing else, as it does
                // for every chord below.
                _ => return UpdateEffect::Redraw,
            }
        }

        if takes_pending && self.pending_register_select {
            if let KeyCode::Char(ch) = key.code {
                self.pending_register_select = false;
                action_handlers::yank::execute_select_register(self, ch);
                return UpdateEffect::Redraw;
            }
            self.pending_register_select = false;
            return UpdateEffect::Redraw;
        }

        if takes_pending
            && self.pending_surround_replace
                != action_handlers::surround::SurroundReplaceStage::Idle
        {
            if let KeyCode::Char(ch) = key.code {
                let stage = self.pending_surround_replace;
                self.pending_surround_replace =
                    action_handlers::surround::SurroundReplaceStage::Idle;
                match stage {
                    action_handlers::surround::SurroundReplaceStage::AwaitFrom => {
                        self.pending_surround_replace =
                            action_handlers::surround::SurroundReplaceStage::AwaitTo(ch);
                        return UpdateEffect::Redraw;
                    },
                    action_handlers::surround::SurroundReplaceStage::AwaitTo(from) => {
                        return action_handlers::surround::execute_surround_replace(self, from, ch);
                    },
                    action_handlers::surround::SurroundReplaceStage::Idle => unreachable!(),
                }
            }
            self.pending_surround_replace = action_handlers::surround::SurroundReplaceStage::Idle;
            return UpdateEffect::Redraw;
        }

        if takes_pending && self.pending_surround_delete {
            if let KeyCode::Char(ch) = key.code {
                self.pending_surround_delete = false;
                return action_handlers::surround::execute_surround_delete(self, ch);
            }
            self.pending_surround_delete = false;
            return UpdateEffect::Redraw;
        }

        if takes_pending && self.pending_textobject_select.is_some() {
            if let KeyCode::Char(ch) = key.code {
                let (mode, count) = self.pending_textobject_select.expect("checked above");
                self.pending_textobject_select = None;
                return action_handlers::textobject::execute_select_textobject(
                    self, mode, ch, count,
                );
            }
            // Cancelling costs the one press and nothing else. A fall-through
            // runs the key's own binding as well, which drops the chord and
            // leaves select mode on the same Escape.
            self.pending_textobject_select = None;
            return UpdateEffect::Redraw;
        }

        if takes_pending && self.pending_goto_word.is_some() {
            if let KeyCode::Char(ch) = key.code {
                let labels = self.pending_goto_word.as_ref().expect("checked above");
                match crate::goto_word::step_jump(labels, &self.pending_goto_word_input, ch) {
                    crate::goto_word::JumpStep::Jump(range) => {
                        self.pending_goto_word = None;
                        self.pending_goto_word_input.clear();
                        return match self.pending_goto_word_extend.take() {
                            Some(primary) => action_handlers::movement::extend_to_word_range(
                                self, primary, range,
                            ),
                            None => action_handlers::movement::jump_to_word_range(self, range),
                        };
                    },
                    crate::goto_word::JumpStep::Continue => {
                        self.pending_goto_word_input.push(ch);
                        return UpdateEffect::Redraw;
                    },
                    crate::goto_word::JumpStep::Cancel => {
                        self.pending_goto_word = None;
                        self.pending_goto_word_extend = None;
                        self.pending_goto_word_input.clear();
                        return UpdateEffect::Redraw;
                    },
                }
            }
            // Consumed for the reason the textobject arm above gives.
            self.pending_goto_word = None;
            self.pending_goto_word_extend = None;
            self.pending_goto_word_input.clear();
            return UpdateEffect::Redraw;
        }

        // The guards above are read at the mode this press started in, which
        // holds only while none of them changes it. They clear pending flags
        // and nothing else today, and this is what says so if that stops being
        // true.
        debug_assert_eq!(
            takes_pending,
            matches!(self.focused_mode(), "normal" | "select"),
            "a key guard changed the mode the guards after it were read at"
        );

        let count_active_mode = takes_pending;
        if count_active_mode
            && self.pending_count.is_some()
            && key.modifiers.is_empty()
            && let KeyCode::Char(ch) = key.code
            && ch.is_ascii_digit()
        {
            let digit = ch.to_digit(10).expect("ascii digit");
            let new_count = self
                .pending_count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(digit);
            self.pending_count = Some(new_count);
            return UpdateEffect::Redraw;
        }

        let Some((actions, captured_digit)) = self.keymap_lookup(&key, &mut lookup).clone() else {
            if count_active_mode
                && let KeyCode::Char(ch) = key.code
                && ch.is_ascii_digit()
                && key.modifiers.is_empty()
            {
                let digit = ch.to_digit(10).expect("ascii digit");
                self.pending_count = Some(digit);
                return UpdateEffect::Redraw;
            }
            return UpdateEffect::None;
        };

        self.run_bound_actions(&actions, captured_digit, matches!(key.code, KeyCode::Esc))
    }

    /// Run a binding's resolved actions, applying the mode switches, variable
    /// sets, and dispatches it names, and report what the frame owes.
    ///
    /// `captured_digit` binds a `$num` argument when the winning key was the
    /// digit placeholder. Pass `None` from a path with no placeholder to bind.
    ///
    /// Every input path that resolves a binding runs it through here, so a
    /// wheel gesture and a key press share one set of chord semantics.
    /// `dismisses_pinned` says the input that produced these actions was a
    /// deliberate dismissal, which is Esc and nothing else here.
    ///
    /// A mode binds Esc to an action, so it never reaches the plain-key branch
    /// while that mode is active, and a pinned card would survive the one key
    /// meant to take it down. The caller knows which key it was; this does not.
    pub(crate) fn run_bound_actions(
        &mut self,
        actions: &Arc<[ResolvedAction]>,
        captured_digit: Option<f64>,
        dismisses_pinned: bool,
    ) -> UpdateEffect {
        let mut effect = UpdateEffect::None;
        let mut dispatched_action = false;
        let mut dispatched_hover = false;
        let mut dispatched_code_action = false;
        let mut dispatched_rename_symbol = false;
        let mut dispatched_symbol_picker = false;
        // A dispatch that raises its own popup keeps it. An unchanged generation
        // means every popup on screen predates the chord, which is what the
        // post-dispatch clear below is for.
        let hover_before = self.pending_hover.as_ref().map(|popup| popup.generation);
        // The editing half of an insert-entry chord runs before the mode
        // switch, so it has to know the switch follows and leave its undo
        // group open for the session to adopt.
        self.group_held_for_insert = actions.iter().any(|ra| {
            ra.name == "SetMode"
                && ra
                    .args
                    .first()
                    .and_then(keymap_state::arg_as_str)
                    .is_some_and(|mode| mode.as_str() == "insert")
        });
        // A pinned mode outlives every binding that rides a mode switch on top
        // of real work, so that switch is dropped and the chord's keys repeat.
        // A binding that switches and nothing else -- Escape -- is the release,
        // so its switch runs.
        let holds_mode = self.focused_editor_pinned()
            && !actions
                .iter()
                .all(|ra| ra.name == "SetMode" || ra.name == "SetVar");
        for ra in actions.iter() {
            if ra.name == "SetMode" {
                if holds_mode {
                    continue;
                }
                if let Some(mode_name) = ra.args.first().and_then(keymap_state::arg_as_str) {
                    self.transition_mode(mode_name);
                    effect = UpdateEffect::Redraw;
                }
                continue;
            }
            if ra.name == "SetVar" {
                self.set_user_var(ra);
                effect = UpdateEffect::Redraw;
                continue;
            }
            if ra.name == "Hover" {
                dispatched_hover = true;
            }
            if ra.name == "CodeAction" {
                dispatched_code_action = true;
            }
            if ra.name == "RenameSymbol" {
                dispatched_rename_symbol = true;
            }
            if ra.name == "OpenSymbolPicker" {
                dispatched_symbol_picker = true;
            }
            if let Some(action) = resolve_action(&ra.name, &ra.args, captured_digit) {
                dispatched_action = true;
                let e = action_handlers::dispatch(self, &*action);
                match e {
                    UpdateEffect::Quit => return UpdateEffect::Quit,
                    UpdateEffect::Redraw => effect = UpdateEffect::Redraw,
                    UpdateEffect::None => {},
                }
            }
        }
        // The chord is over, so a later dispatch from anywhere else seals its
        // own group again.
        self.group_held_for_insert = false;
        // A deliberate dismissal takes the popup down whatever the binding did.
        // It cannot ride the action-dependent clear below, because a mode binds
        // Esc to a mode switch, which returns before anything counts as
        // dispatched.
        if dismisses_pinned {
            self.pending_hover = None;
            self.pending_hover_request = None;
        }
        if dispatched_action {
            self.pending_count = None;
            if !dispatched_hover {
                let popup = self.pending_hover.as_ref();
                let stale = popup.map(|popup| popup.generation) == hover_before;
                // A pinned card outlives an action the same way it outlives a
                // key press. The tour, not the last thing dispatched, decides
                // when it goes.
                if stale && !popup.is_some_and(|popup| popup.pinned) {
                    self.pending_hover = None;
                }
                self.pending_hover_request = None;
            }
            if !dispatched_code_action {
                self.pending_code_action_picker = None;
                self.pending_code_action_request = None;
            }
            if !dispatched_rename_symbol {
                self.pending_prepare_rename = None;
            }
            if !dispatched_symbol_picker {
                self.pending_symbol_picker = None;
                self.pending_symbol_picker_request = None;
            }
        }
        effect
    }

    /// The terminal or agent session that takes raw keystrokes, if any.
    ///
    /// `Some` for a focused `View::Agent` or `View::Terminal` split pane in
    /// normal mode, the mode such a pane rests in. The pane has no text to
    /// edit, so its normal mode sends each key to the child. Any other mode is
    /// a chord in progress, and its keys go to the keymap.
    ///
    /// An open modal outranks passthrough whether or not it has an input, and
    /// so does the reword input of a paused rebase. The Ctrl-C branch in
    /// [`Self::handle_key`] encodes the same order.
    ///
    /// A pane with a file open still on its way answers `None`, so the keys
    /// typed before the buffer shows do not reach the shell it covers.
    ///
    /// A binding that names the pane kind outranks passthrough for a key that
    /// types no character. See [`Self::passthrough_binding`].
    fn term_input_target(&self) -> Option<TermId> {
        if self.focused_editor_ids().is_some() || active_modal(self).is_some() {
            return None;
        }
        let term_id = self.focused_term_id()?;
        if self.focused_mode() != "normal" {
            return None;
        }

        let pane = self.active_workspace().panes.focus();
        (!crate::buffer_lifecycle::pane_awaits_open(self, pane)).then_some(term_id)
    }

    /// The binding that takes `key` away from the child of a pane in
    /// passthrough, if one does.
    ///
    /// Only a binding that names the pane kind is a candidate. A binding that
    /// names the mode alone, such as normal-mode Escape, is an editor key that
    /// a terminal at rest also matches. A printable key is never looked up, so
    /// typing derives no keymap state.
    fn passthrough_binding(&self, key: &KeyEvent) -> Option<BoundActions> {
        if is_printable_key(key) {
            return None;
        }
        let pane = keymap_state::pane_predicate(self.active_workspace())?;

        #[cfg(test)]
        self.keymap_lookups.set(self.keymap_lookups.get() + 1);

        self.keymap
            .lookup_scoped(&StoatKeymapState::from_stoat(self), key, "pane", pane)
    }

    /// Encode `key` and send it to the PTY of `agent_id`.
    ///
    /// Every key that reaches here is sent, Escape included. A key with no
    /// encoding is swallowed. A key that a pane-scoped binding takes never
    /// reaches here. See [`Self::passthrough_binding`].
    ///
    /// The view returns to the live screen first, so the key lands in sight.
    fn route_key_to_term(&mut self, agent_id: TermId, key: KeyEvent) -> UpdateEffect {
        mouse::clear_term_selection(self, agent_id);
        let effect = self.show_term_live_screen(agent_id);
        if let Some(bytes) = encode_key_to_pty(&key) {
            self.write_to_term(agent_id, &bytes);
        }
        effect
    }

    /// Encode a paste of `text` and send it to the terminal's PTY.
    ///
    /// Returns [`UpdateEffect::Redraw`] only when the view comes back from
    /// history. Nothing else on screen changes until the child echoes the text
    /// back, and that read is what asks for the repaint.
    fn route_paste_to_term(&mut self, term_id: TermId, text: &str) -> UpdateEffect {
        mouse::clear_term_selection(self, term_id);
        let effect = self.show_term_live_screen(term_id);
        let bracketed = self
            .active_workspace()
            .terms
            .get(term_id)
            .is_some_and(|session| session.term.bracketed_paste());

        self.write_to_term(term_id, &encode_paste_to_pty(text, bracketed));
        effect
    }

    /// Return the terminal's view to the live screen, so input sent to the
    /// child lands in sight.
    ///
    /// Answers [`UpdateEffect::Redraw`] when the view was back in history,
    /// because the jump shows before the child echoes anything, and a key the
    /// child does not echo asks for no repaint at all.
    fn show_term_live_screen(&mut self, term_id: TermId) -> UpdateEffect {
        let moved = self
            .active_workspace_mut()
            .terms
            .get_mut(term_id)
            .is_some_and(|session| session.term.scroll_to_bottom());
        match moved {
            true => UpdateEffect::Redraw,
            false => UpdateEffect::None,
        }
    }

    /// Write raw bytes to an agent's PTY.
    ///
    /// Uses `now_or_never` because both the local PTY and the test fake finish
    /// the moment the bytes are queued, so keystrokes reach the agent in order
    /// without spawning a task. The local session hands them to a writer
    /// thread, so a child that has stopped reading parks that thread rather
    /// than this one, and nothing here can stall input.
    ///
    /// An error means the session can no longer take bytes at all, which for
    /// the local one means its writer thread has exited. It is warned about and
    /// dropped, since a keystroke has nowhere else to go.
    pub(crate) fn write_to_term(&self, agent_id: TermId, bytes: &[u8]) {
        let Some(session) = self
            .active_workspace()
            .terms
            .get(agent_id)
            .map(|agent| agent.session.clone())
        else {
            return;
        };

        match session.write(bytes).now_or_never() {
            Some(Ok(())) => {},
            Some(Err(err)) => {
                tracing::warn!(target: "stoat::agent", %err, "failed to write to agent pty");
            },
            None => {
                tracing::warn!(target: "stoat::agent", "agent pty write did not complete synchronously");
            },
        }
    }

    pub(crate) fn take_pending_count(&mut self) -> Option<u32> {
        self.pending_count.take()
    }

    /// Move any armed register selection onto the command about to run.
    ///
    /// Called once per dispatch, before the handler, so the selection belongs
    /// to exactly one command and the command after it reads the unnamed
    /// register again.
    pub(crate) fn take_selected_register(&mut self) {
        self.command_register = self.selected_register.take();
    }

    /// The register the running command reads, which is the unnamed one unless
    /// `SelectRegister` named another.
    ///
    /// A handler asking twice gets the same answer, since the dispatch that
    /// called it already spent the selection on its behalf.
    pub(crate) fn active_register(&self) -> register::Register {
        self.command_register.unwrap_or(register::Register::Unnamed)
    }

    /// The register a macro records into or replays from, `@` when the command
    /// named none.
    ///
    /// Macros default away from the unnamed register because that one is where
    /// every yank and delete lands. Sharing it lets any edit between recording
    /// a macro and replaying it destroy the macro.
    pub(crate) fn macro_register(&self) -> register::Register {
        self.command_register
            .unwrap_or(register::Register::Named('@'))
    }

    /// The focused document editor's buffer and primary cursor offset, or `None`
    /// when no document editor has focus.
    ///
    /// Sampled before and after a key so the post-key view-follow can tell when
    /// the key moved the cursor and the view must follow it.
    pub(crate) fn focused_cursor_pos(&mut self) -> Option<(BufferId, usize)> {
        let editor = action_handlers::focused_editor_mut(self)?;
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let offset = stoat_text::cursor_offset(
            buffer_snapshot.rope(),
            buffer_snapshot.resolve_anchor(&sel.tail()),
            buffer_snapshot.resolve_anchor(&sel.head()),
        );
        Some((editor.buffer_id, offset))
    }

    /// The scratch editor the open modal types into, or `None` when no modal is
    /// open or the open one has no input of its own.
    ///
    /// A picker without an input (the jumplist, diagnostics, and location pickers,
    /// the quit prompt, a modal run) answers `None` so the caller keeps resolving
    /// through the panes behind it, which is where the keys it does handle land.
    pub(crate) fn active_modal_input(&self) -> Option<(EditorId, BufferId)> {
        let input = match active_modal(self)? {
            ActiveModal::WorkspacePicker => &self.workspace_picker.as_ref()?.input,
            ActiveModal::CommitPicker => &self.commit_picker.as_ref()?.input,
            ActiveModal::FileFinder => &self.file_finder.as_ref()?.input,
            ActiveModal::SymbolFinder => &self.symbol_finder.as_ref()?.input,
            ActiveModal::CodeSearch => &self.code_search.as_ref()?.input,
            ActiveModal::Palette => self.command_palette.as_ref()?.focused_input()?,
            ActiveModal::Help => &self.help.as_ref()?.input,
            ActiveModal::Rename => &self.rename_input.as_ref()?.input,
            ActiveModal::Search => &self.search_input.as_ref()?.input,
            ActiveModal::SplitSelection => &self.split_selection_input.as_ref()?.input,
            ActiveModal::FilterSelections => &self.filter_selections_input.as_ref()?.input,
            ActiveModal::ShellInput => &self.shell_input.as_ref()?.input,
            ActiveModal::Location => &self.location_picker.as_ref()?.input,
            ActiveModal::Diagnostics => &self.diagnostics_picker.as_ref()?.picker.input,
            ActiveModal::Jumplist => &self.jumplist_picker.as_ref()?.picker.input,
            ActiveModal::Run | ActiveModal::QuitConfirm => return None,
        };

        Some((input.editor_id, input.buffer_id))
    }

    /// Insert a bracketed paste's text wherever typing would land, as one edit.
    ///
    /// The characters arrive as text and never as keys, so a paste in normal
    /// mode inserts rather than running what it spells. It also leaves the mode
    /// alone, which is what bracketed paste means: the reader asked for these
    /// characters in the buffer, not for a mode change.
    ///
    /// One [`Self::editor_insert`] covers the pane's buffer and every modal's
    /// input alike, since [`Self::focused_editor_ids`] already resolves to
    /// whichever of them typing would reach.
    ///
    /// Line endings are normalized because a terminal forwards whatever the
    /// clipboard held, and a buffer holds LF.
    ///
    /// Into a modal's input they then collapse to spaces, since those are drawn
    /// as a single row and a break would leave the cursor on a row that is
    /// never painted. Every character still arrives, which is what a reader
    /// pasting a wrapped path or a wrapped query wants.
    fn handle_paste(&mut self, text: &str) -> UpdateEffect {
        if text.is_empty() {
            return UpdateEffect::None;
        }

        // Asked before the editor resolve, which answers `None` for a focused
        // terminal and would drop the paste. Keystrokes in the same focus state
        // route through this, and paste has no reason to differ. Its overlay
        // precedence is also what keeps a modal's paste in the modal.
        if let Some(term_id) = self.term_input_target() {
            return self.route_paste_to_term(term_id, text);
        }

        let Some((editor_id, buffer_id)) = self.focused_editor_ids() else {
            return UpdateEffect::None;
        };

        let mut normalized = match text.contains('\r') {
            true => text.replace("\r\n", "\n").replace('\r', "\n"),
            false => text.to_owned(),
        };
        if self.active_modal_input().is_some() {
            normalized = normalized.replace('\n', " ");
        }

        let opened = self.begin_paste_undo_group();
        self.editor_insert(editor_id, buffer_id, &normalized);
        if opened {
            self.seal_focused_undo_group();
        }
        UpdateEffect::Redraw
    }

    pub(crate) fn focused_editor_ids(&self) -> Option<(EditorId, BufferId)> {
        let ws = self.active_workspace();

        if let Some(ids) = self.active_modal_input() {
            return Some(ids);
        }

        if let Some((editor_id, buffer_id)) = ws
            .rebase_active
            .as_ref()
            .and_then(|a| a.pause.as_ref())
            .and_then(|p| match p {
                RebasePause::Reword { input, .. } => Some((input.editor_id, input.buffer_id)),
                _ => None,
            })
        {
            return Some((editor_id, buffer_id));
        }

        let view = match ws.focus {
            FocusTarget::SplitPane => {
                let focused = ws.panes.focus();
                ws.panes.pane(focused).view.clone()
            },
            FocusTarget::Dock(dock_id) => {
                let dock = ws.docks.get(dock_id)?;
                dock.view.clone()
            },
        };
        match view {
            View::Editor(id) => {
                let editor = ws.editors.get(id)?;
                Some((id, editor.buffer_id))
            },
            View::Run(id) => {
                let run_state = ws.runs.get(id)?;
                Some((run_state.input.editor_id, run_state.input.buffer_id))
            },
            _ => None,
        }
    }

    /// Absolute terminal cell `(col, row)` of the primary cursor when the
    /// focused pane is a document editor, else `None`.
    ///
    /// Returns the position [`crate::render::editor::render_editor_with_overlay`]
    /// recorded while painting the current frame, so it is exactly where the
    /// cursor cell would otherwise be drawn. `None` for finder/palette/dock/run
    /// focus, where the editor paints its own cursor cell and the terminal
    /// cursor stays hidden. Must be called after a render.
    pub(crate) fn primary_cursor_screen_pos(&self) -> Option<(u16, u16)> {
        let (focused_id, _) = self.focused_editor_ids()?;
        let ws = self.active_workspace();
        let FocusTarget::SplitPane = ws.focus else {
            return None;
        };
        let focused_pane = ws.panes.pane(ws.panes.focus());
        // A detached pane draws its cursor in its own aux window via a pool
        // cursor, so the primary frame parks no terminal cursor over its cells.
        if matches!(focused_pane.placement, Placement::Window(_)) {
            return None;
        }
        let pane_editor = match focused_pane.view {
            View::Editor(id) => id,
            _ => return None,
        };
        if focused_id != pane_editor {
            return None;
        }
        ws.editors.get(pane_editor)?.cursor_screen_cell
    }

    fn handle_insert_key(&mut self, key: KeyEvent) -> Option<UpdateEffect> {
        let (editor_id, buffer_id) = self.focused_editor_ids()?;

        if self.pending_insert_register {
            self.pending_insert_register = false;
            if let KeyCode::Char(ch) = key.code
                && let Some(fragments) = action_handlers::yank::read_register_fragments(
                    self,
                    action_handlers::yank::register_for_char(ch),
                )
            {
                self.editor_insert_register(editor_id, buffer_id, &fragments);
            }
            return Some(UpdateEffect::Redraw);
        }

        match key.code {
            KeyCode::Char('w') if key.modifiers == KeyModifiers::CONTROL => {
                self.editor_delete_word_backward(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.editor_kill_to_line_start(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Char('k') if key.modifiers == KeyModifiers::CONTROL => {
                self.editor_kill_to_line_end(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Char('d') if key.modifiers == KeyModifiers::ALT => {
                self.editor_delete_word_forward(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Char('h') if key.modifiers == KeyModifiers::CONTROL => {
                self.editor_backspace(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
                self.editor_delete(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => {
                self.editor_insert_newline(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.editor_insert_char(editor_id, buffer_id, ch);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Backspace if key.modifiers == KeyModifiers::ALT => {
                self.editor_delete_word_backward(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Backspace => {
                self.editor_backspace(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Delete if key.modifiers == KeyModifiers::ALT => {
                self.editor_delete_word_forward(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Delete => {
                self.editor_delete(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.editor_insert(editor_id, buffer_id, "\n");
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Enter if key.modifiers.is_empty() => {
                self.editor_insert_newline(editor_id, buffer_id);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Left => {
                action_handlers::dispatch(self, &stoat_action::MoveLeft);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Right => {
                action_handlers::dispatch(self, &stoat_action::MoveRight);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Up if self.pending_completion.is_some() => {
                if let Some(popup) = self.pending_completion.as_mut() {
                    popup.selected_idx = popup.selected_idx.saturating_sub(1);
                }
                action_handlers::completion::arm_completion_resolve(self);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Down if self.pending_completion.is_some() => {
                if let Some(popup) = self.pending_completion.as_mut() {
                    let last = popup.len().saturating_sub(1);
                    popup.selected_idx = (popup.selected_idx + 1).min(last);
                }
                action_handlers::completion::arm_completion_resolve(self);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Up => {
                action_handlers::dispatch(self, &stoat_action::MoveUp);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Down => {
                action_handlers::dispatch(self, &stoat_action::MoveDown);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Home => {
                action_handlers::dispatch(self, &stoat_action::GotoLineStart);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::End => {
                // Not the GotoLineEnd action, which lands on the last character
                // for a block cursor to sit on. A caret about to type belongs
                // past it.
                action_handlers::movement::goto_line_end_newline(self, false);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::PageUp => {
                action_handlers::dispatch(self, &stoat_action::PageUp);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::PageDown => {
                action_handlers::dispatch(self, &stoat_action::PageDown);
                Some(UpdateEffect::Redraw)
            },
            _ => None,
        }
    }

    /// True when every cursor in `editor_id` has nothing but whitespace behind
    /// it on its line.
    ///
    /// Every cursor, because one cursor mid-word is enough to make the Tab key
    /// mean something other than indentation. A reader with cursors in two
    /// places wants one answer for the keystroke, not two.
    pub(crate) fn cursors_after_only_whitespace(
        &mut self,
        editor_id: EditorId,
        buffer_id: BufferId,
    ) -> bool {
        let ws = self.active_workspace_mut();
        let Some(editor) = ws.editors.get_mut(editor_id) else {
            return false;
        };
        if ws.buffers.get(buffer_id).is_none() {
            return false;
        }
        let display_snapshot = editor.display_map.snapshot();
        let buf_snapshot = display_snapshot.buffer_snapshot();
        let rope = buf_snapshot.rope();

        let ends = {
            let anchors: Vec<Anchor> = editor
                .selections
                .all_anchors()
                .iter()
                .flat_map(|sel| [sel.tail(), sel.head()])
                .collect();
            buf_snapshot.resolve_anchors_batch(&anchors)
        };

        ends.as_chunks::<2>().0.iter().all(|ends| {
            let offset = stoat_text::cursor_offset(rope, ends[0], ends[1]);
            rope.reversed_chars_at(offset)
                .take_while(|&ch| ch != '\n')
                .all(char::is_whitespace)
        })
    }

    /// The mode of the focused input target.
    ///
    /// The target is resolved the way [`Self::focused_editor_ids`] resolves it.
    /// A topmost open input modal, or else a focused editor or run pane,
    /// supplies its editor's [`EditorState::mode`]. A focused terminal or agent
    /// pane supplies its [`TermSession::mode`]. With no such target the mode
    /// falls back to [`Self::fallback_mode`].
    pub(crate) fn focused_mode(&self) -> &str {
        #[cfg(test)]
        self.focused_mode_reads
            .set(self.focused_mode_reads.get() + 1);

        let ws = self.active_workspace();
        if let Some((editor_id, _)) = self.focused_editor_ids()
            && let Some(editor) = ws.editors.get(editor_id)
        {
            return &editor.mode;
        }
        if let Some(term_id) = self.focused_term_id()
            && let Some(term) = ws.terms.get(term_id)
        {
            return &term.mode;
        }
        &self.fallback_mode
    }

    /// Whether the focused pane builds a selection, which a motion reads to
    /// decide between growing one and replacing it.
    ///
    /// True for `select` and for the submodes a key reaches from it. Those are
    /// named `select_*` by convention, so a chord like `]f` still answers yes
    /// while it sits in `select_bracket_next` waiting for its second key. A
    /// bare comparison against `select` says no there, which turns a motion the
    /// user asked to extend into one that replaces.
    pub(crate) fn in_select_mode(&self) -> bool {
        let mode = self.focused_mode();
        mode == "select" || mode.starts_with("select_")
    }

    /// Settle [`Self::frame_mode`] onto what [`Self::focused_mode`] answers now.
    ///
    /// Separate from reading it, the way [`Self::refresh_chrome`] is, so a
    /// caller that goes on to borrow the workspace mutably can settle the copy
    /// first and then read it as an ordinary field.
    pub(crate) fn refresh_frame_mode(&mut self) {
        let mode = self.focused_mode();
        if self.frame_mode != mode {
            let mode = mode.to_string();
            self.frame_mode = mode;
        }
    }

    /// Set the mode of the focused input target.
    ///
    /// Writes to the same target [`Self::focused_mode`] reads, so a value
    /// written is read back while focus and open modals are unchanged. This is
    /// the raw setter, without the insert-run bookkeeping
    /// [`Self::transition_mode`] layers on top.
    pub(crate) fn set_focused_mode(&mut self, mode: String) {
        if let Some((editor_id, _)) = self.focused_editor_ids()
            && let Some(editor) = self.active_workspace_mut().editors.get_mut(editor_id)
        {
            editor.mode = mode;
            return;
        }
        if let Some(term_id) = self.focused_term_id()
            && let Some(term) = self.active_workspace_mut().terms.get_mut(term_id)
        {
            term.mode = mode;
            return;
        }
        self.fallback_mode = mode;
    }

    /// Whether the focused editor holds its mode against a chained switch.
    ///
    /// A focused terminal, or nothing focused at all, reports `false`. Only an
    /// editor's bindings chain a mode switch onto real work, so only an editor
    /// carries a pin.
    pub(crate) fn focused_editor_pinned(&self) -> bool {
        self.focused_editor_ids()
            .and_then(|(editor_id, _)| self.active_workspace().editors.get(editor_id))
            .is_some_and(|editor| editor.pinned)
    }

    /// Pin or release the focused editor's mode, reporting whether an editor
    /// took the value.
    ///
    /// Reads the same target [`Self::set_focused_mode`] writes, so a pin and
    /// the mode it holds always belong to one editor.
    pub(crate) fn set_focused_pinned(&mut self, pinned: bool) -> bool {
        let Some((editor_id, _)) = self.focused_editor_ids() else {
            return false;
        };
        let Some(editor) = self.active_workspace_mut().editors.get_mut(editor_id) else {
            return false;
        };
        editor.pinned = pinned;
        true
    }

    /// The foreground app screen as the `view` predicate reports it, or `None`
    /// for a plain editor with nothing focused. Screens (review/commits/rebase/
    /// reword/conflict) are derived from session state rather than the mode.
    #[cfg(test)]
    pub(crate) fn current_view(&self) -> Option<&'static str> {
        keymap_state::view_predicate(self.active_workspace())
    }

    /// The focused terminal or agent pane's [`TermId`], if the focused pane is
    /// one. Unlike [`Self::term_input_target`] this does not gate on the mode,
    /// so [`Self::focused_mode`] can consult it without recursing.
    fn focused_term_id(&self) -> Option<TermId> {
        let ws = self.active_workspace();
        let FocusTarget::SplitPane = ws.focus else {
            return None;
        };
        match &ws.panes.pane(ws.panes.focus()).view {
            View::Agent(id) | View::Terminal(id) => Some(*id),
            _ => None,
        }
    }

    /// Move focus to `at`, and switch to its tab when the tab is parked.
    /// Returns whether focus moved.
    ///
    /// Returns `false` for the place focus already sits, a closed pane, a
    /// dropped dock, or a tab that is gone.
    pub(crate) fn focus_location(&mut self, at: TermLocation) -> bool {
        match at {
            TermLocation::Dock(id) => {
                let ws = self.active_workspace_mut();
                if ws.focus == FocusTarget::Dock(id) || !ws.docks.contains_key(id) {
                    return false;
                }
                ws.focus = FocusTarget::Dock(id);
                true
            },
            TermLocation::Pane { tab, pane } if tab == self.active_workspace().active_tab => {
                let ws = self.active_workspace_mut();
                if pane == ws.panes.focus() || !ws.panes.split_pane_ids().contains(&pane) {
                    return false;
                }
                ws.panes.set_focus(pane);
                ws.focus = FocusTarget::SplitPane;
                true
            },
            TermLocation::Pane { tab, pane } => {
                let parked_holds_pane = self
                    .active_workspace()
                    .tabs
                    .get(tab)
                    .and_then(|t| t.parked.as_ref())
                    .is_some_and(|tree| tree.split_pane_ids().contains(&pane));
                if !parked_holds_pane || !self.active_workspace_mut().switch_tab(tab) {
                    return false;
                }
                // A parked tree keeps zero-sized rects, so a layout pass fits it
                // to the screen before focus moves into it.
                let size = self.size();
                let ws = self.active_workspace_mut();
                ws.layout(size);
                ws.panes.set_focus(pane);
                ws.focus = FocusTarget::SplitPane;
                true
            },
        }
    }

    /// Put the terminal or agent pane that held focus before an event back in
    /// normal mode when the event moved focus off it.
    ///
    /// A mode other than normal on such a pane is a chord in progress, and the
    /// chord ends when focus leaves, because a pane without focus takes no
    /// keys. A binding that moves focus before it switches the mode leaves the
    /// switch on the pane that took focus. Without this reset, the pane it left
    /// keeps the chord mode and holds its keys back from the child when focus
    /// returns.
    ///
    /// `workspace` is the active workspace and `term` is
    /// [`Self::focused_term_id`], both read before the event.
    fn rest_left_terminal(&mut self, workspace: WorkspaceId, term: Option<TermId>) {
        let Some(term_id) = term else {
            return;
        };
        if self.active_workspace == workspace && self.focused_term_id() == Some(term_id) {
            return;
        }

        if let Some(session) = self
            .workspaces
            .get_mut(workspace)
            .and_then(|ws| ws.terms.get_mut(term_id))
        {
            session.mode = "normal".into();
        }
    }

    /// Switch the focused target's mode to `next`, opening or closing the
    /// insert-run buffer. Entering any insert-like mode (`insert`,
    /// `reword_insert`) starts a fresh run, and leaving discards it, having
    /// read whether it holds anything.
    pub(crate) fn transition_mode(&mut self, next: String) {
        let was_insert = is_insert_run_mode(self.focused_mode());
        let now_insert = is_insert_run_mode(&next);
        let leaving_insert = was_insert && !now_insert;

        let typed_nothing = leaving_insert
            && self
                .current_insert_run
                .take()
                .is_none_or(|run| run.is_empty());

        if leaving_insert {
            let auto_indent_cursors = std::mem::take(&mut self.auto_indent_cursors);
            if typed_nothing && !auto_indent_cursors.is_empty() {
                self.strip_untouched_auto_indent(&auto_indent_cursors);
            }
        }
        if leaving_insert && std::mem::take(&mut self.restore_cursor) {
            self.restore_cursor_after_append();
        }
        if leaving_insert {
            self.seal_focused_undo_group();
        }
        if !was_insert && now_insert {
            self.current_insert_run = Some(String::new());
            self.begin_insert_undo_group();
        }
        // Every switch that reaches here releases the pin. The dispatch loop
        // drops the switches a pinned mode holds, so what arrives is a switch
        // the pin does not survive.
        self.set_focused_pinned(false);
        self.set_focused_mode(next);
    }

    /// Open an undo group so the whole insert session collapses into one undo
    /// step, capturing the pre-session selections to restore on undo.
    ///
    /// A group left open by the editing half of an insert-entry chord is
    /// adopted rather than sealed, so the change and the typing land in one
    /// revision. Its own pre-action selections are what undo restores, which is
    /// the selection the user had before the change.
    fn begin_insert_undo_group(&mut self) {
        let Some((buffer_id, before)) = self.focused_undo_snapshot() else {
            return;
        };
        if let Some(buffer) = self.active_workspace().buffers.get(buffer_id) {
            buffer.write().expect("poisoned").try_begin_group(|| before);
        }
    }

    /// Open an undo group for a paste unless one is already open, reporting
    /// whether it did so the caller knows to seal it.
    ///
    /// A paste is one thing to undo, and a multi-cursor one lands an edit record
    /// per cursor, so outside an insert session those records would undo one at
    /// a time. Inside one, the session's own group already covers them and
    /// opening another would split the session in two.
    fn begin_paste_undo_group(&mut self) -> bool {
        let Some((buffer_id, before)) = self.focused_undo_snapshot() else {
            return false;
        };
        let Some(buffer) = self.active_workspace().buffers.get(buffer_id) else {
            return false;
        };
        buffer.write().expect("poisoned").try_begin_group(|| before)
    }

    /// Seal the focused buffer's open undo group, capturing the selections to
    /// restore on redo. A group that took no edits was never materialized, so
    /// sealing one leaves no step behind.
    fn seal_focused_undo_group(&mut self) {
        let Some((buffer_id, after)) = self.focused_undo_snapshot() else {
            return;
        };
        if let Some(buffer) = self.active_workspace().buffers.get(buffer_id) {
            buffer.write().expect("poisoned").seal_group(after);
        }
    }

    /// The focused editor's buffer id paired with its current selections, for
    /// opening or sealing an undo group around an insert session.
    fn focused_undo_snapshot(&self) -> Option<(BufferId, Arc<[Selection<Anchor>]>)> {
        let (editor_id, buffer_id) = self.focused_editor_ids()?;
        let editor = self.active_workspace().editors.get(editor_id)?;
        Some((buffer_id, editor.selections.shared_anchors()))
    }

    /// Step each selection's head back one grapheme, undoing the reach
    /// [`action_handlers::movement::append_mode`] added so leaving the insert
    /// lands on the last typed character rather than one cell past it.
    ///
    /// The anchor stays put, so what was selected before `a` comes back rather
    /// than collapsing to a cursor. Only a forward range steps back, which
    /// leaves the empty range at the buffer's end alone. Nothing was added to
    /// that one, so it has nothing to give back.
    ///
    /// A head at a line start stays put, since retreating across the newline
    /// would land it on the previous line. This covers the buffer start and an
    /// empty line whose auto-indent was stripped on the same transition.
    fn restore_cursor_after_append(&mut self) {
        let Some(editor) = action_handlers::focused_editor_mut(self) else {
            return;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let rope = buf_snap.rope();
        editor
            .selections
            .transform_resolved(buf_snap, |sel, head_offset, tail_offset| {
                let (from, to) = (head_offset.min(tail_offset), head_offset.max(tail_offset));
                let back = rope.prev_grapheme_boundary(to);
                if from < back {
                    return Selection {
                        id: sel.id,
                        start: buf_snap.anchor_at(from, Bias::Right),
                        end: buf_snap.anchor_at(back, Bias::Right),
                        reversed: false,
                        goal: SelectionGoal::None,
                    };
                }

                // Stepping the head back empties this one, so it is a bare
                // cursor rather than a selection `a` reached out of. A cursor
                // covers a cell in this model, so the whole cell moves back.
                //
                // The guard peeks at the preceding scalar rather than the
                // cluster, since only a literal newline pins the cursor in
                // place. Any other cluster is stepped over whole.
                let cursor = stoat_text::cursor_offset(rope, tail_offset, head_offset);
                let landed = match rope.reversed_chars_at(cursor).next() {
                    Some(ch) if ch != '\n' => rope.prev_grapheme_boundary(cursor),
                    _ => cursor,
                };
                crate::selection::land_block_cursor(
                    sel.id,
                    landed,
                    SelectionGoal::None,
                    rope,
                    buf_snap,
                )
            });
    }

    /// Strip the untouched auto-indent from each recorded cursor's line, leaving
    /// a clean empty line.
    ///
    /// Called on the insert-to-normal transition when `o`/`O`/`I`/`A` entered
    /// insert on an empty line and nothing was typed. Only a recorded cursor
    /// whose line is entirely whitespace with the cursor at its end is stripped,
    /// so a cursor that moved onto real content, or one that was merely
    /// repositioned on a pre-existing whitespace line, is left alone.
    fn strip_untouched_auto_indent(&mut self, auto_indent_cursors: &[usize]) {
        let Some((editor_id, buffer_id)) = self.focused_editor_ids() else {
            return;
        };
        let ws = self.active_workspace_mut();
        let (Some(editor), Some(buffer)) =
            (ws.editors.get_mut(editor_id), ws.buffers.get(buffer_id))
        else {
            return;
        };
        let display_snapshot = editor.display_map.snapshot();
        let buf_snapshot = display_snapshot.buffer_snapshot();
        let rope = buf_snapshot.rope();

        let recorded = {
            let mut recorded = auto_indent_cursors.to_vec();
            recorded.sort_unstable();
            recorded
        };

        let mut ranges: Vec<(usize, usize)> = editor
            .selections
            .resolved_reads(buf_snapshot)
            .iter()
            .filter(|read| recorded.binary_search(&read.id).is_ok())
            .filter_map(|read| {
                let cursor = stoat_text::cursor_offset(rope, read.tail, read.head);
                let row = rope.offset_to_point(cursor).row;
                let line_start = rope.point_to_offset(stoat_text::Point::new(row, 0));
                let line_end =
                    rope.point_to_offset(stoat_text::Point::new(row, rope.line_len(row)));
                // Spaces and tabs are one byte each, so an all-whitespace line's
                // leading run spans its whole byte length.
                let all_whitespace =
                    language::line_leading_whitespace(rope, row).len() == line_end - line_start;
                (cursor == line_end && line_end > line_start && all_whitespace)
                    .then_some((line_start, line_end))
            })
            .collect();
        if ranges.is_empty() {
            return;
        }
        ranges.sort_unstable();
        ranges.dedup();

        {
            // Descending and disjoint, which is what the batch takes: the
            // ranges are whole lines, sorted ascending and deduped, so no two
            // that survive share a row.
            let edits: Vec<(Range<usize>, &str)> = ranges
                .iter()
                .rev()
                .map(|&(start, end)| (start..end, ""))
                .collect();
            buffer.write().expect("poisoned").edit_batch(&edits);
        }

        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        action_handlers::movement::move_cursors(&mut editor.selections, new_buf, false, |read| {
            let cursor = stoat_text::cursor_offset(new_buf.rope(), read.tail, read.head);
            Some((cursor, SelectionGoal::None))
        });
    }

    /// Apply a `SetVar(name, value)` action to [`Self::user_vars`].
    ///
    /// A name colliding with a built-in predicate field, or a value shape no
    /// predicate can compare against, warns and is dropped, so a config typo
    /// cannot shadow a built-in or store an uncomparable value.
    fn set_user_var(&mut self, action: &ResolvedAction) {
        let Some(name) = action.args.first().and_then(keymap_state::arg_as_str) else {
            return;
        };
        if keymap_state::BUILTIN_FIELDS.contains(&name.as_str()) {
            tracing::warn!(
                target: "stoat::keymap",
                "SetVar name `{name}` shadows a built-in field and was ignored"
            );
            return;
        }
        let Some(value) = action
            .args
            .get(1)
            .and_then(keymap_state::arg_to_state_value)
        else {
            tracing::warn!(
                target: "stoat::keymap",
                "SetVar `{name}` has no usable value and was ignored"
            );
            return;
        };
        self.user_vars.insert(name, value);
    }

    pub(crate) fn editor_insert(&mut self, editor_id: EditorId, buffer_id: BufferId, text: &str) {
        let inserts = self.editor_cursor_offsets(editor_id);
        self.editor_insert_at(editor_id, buffer_id, text, inserts);
    }

    /// Insert `text` at each cursor in `inserts`, which the caller resolved.
    ///
    /// Split out for [`Self::editor_insert_char`], which resolves the cursors to
    /// ask the pair hook about them. Without the split it resolves them twice on
    /// every typed bracket.
    fn editor_insert_at(
        &mut self,
        editor_id: EditorId,
        buffer_id: BufferId,
        text: &str,
        mut inserts: Vec<(usize, usize)>,
    ) {
        if !text.is_empty()
            && let Some(run) = self.current_insert_run.as_mut()
        {
            run.push_str(text);
        }
        if inserts.is_empty() {
            return;
        }

        let ws = self.active_workspace_mut();
        let Some(buffer) = ws.buffers.get(buffer_id) else {
            return;
        };
        {
            let edits: Vec<(Range<usize>, &str)> = inserts
                .iter()
                .rev()
                .map(|(_, offset)| (*offset..*offset, text))
                .collect();
            buffer.write().expect("poisoned").edit_batch(&edits);
        }

        // Each cursor lands after its own inserted text. The k-th insertion in
        // offset order is shifted by the k insertions before it plus its own,
        // so its text ends at offset + (k + 1) * text.len(). Shifting in place
        // keeps that arithmetic on the offset ordering it depends on.
        let text_len = text.len();
        for (k, (_, offset)) in inserts.iter_mut().enumerate() {
            *offset += (k + 1) * text_len;
        }

        self.land_cursors_after_insert(editor_id, inserts);
    }

    /// Insert one typed character at every cursor, completing or stepping over
    /// an auto-pair wherever the buffer's pair table calls for it.
    ///
    /// Typing is the only thing that pairs. A paste and an accepted completion
    /// both reach [`Self::editor_insert`] directly, and neither is a reader
    /// reaching for a bracket.
    fn editor_insert_char(&mut self, editor_id: EditorId, buffer_id: BufferId, ch: char) {
        let mut encoded = [0u8; 4];
        let text = ch.encode_utf8(&mut encoded);

        // A colon pairs with nothing, so this path owns it outright. Handing
        // back a cursor list leaves the bracket path resolving them a second
        // time on every typed colon.
        if ch == ':' && self.emoji_expansion_enabled() {
            self.editor_insert_colon(editor_id, buffer_id);
            return;
        }

        // Most typed characters open nothing, and the table answers that from a
        // six-entry scan before a single cursor is resolved.
        let Some(pairs) = self.auto_pairs_for(buffer_id) else {
            self.editor_insert(editor_id, buffer_id, text);
            return;
        };
        if pairs.get(ch).is_none() && !ch.is_whitespace() {
            self.editor_insert(editor_id, buffer_id, text);
            return;
        }

        let cursors = self.editor_cursor_offsets(editor_id);
        let actions = {
            let ws = self.active_workspace();
            let Some(buffer) = ws.buffers.get(buffer_id) else {
                return;
            };
            let guard = buffer.read().expect("buffer poisoned");
            let rope = guard.rope();
            cursors
                .iter()
                .map(|&(_, offset)| auto_pairs::hook_insert(rope, offset, ch, pairs))
                .collect::<Vec<_>>()
        };

        if actions.iter().all(Option::is_none) {
            self.editor_insert_at(editor_id, buffer_id, text, cursors);
            return;
        }

        let insertions = cursors
            .iter()
            .zip(&actions)
            .map(|(&(id, offset), action)| match action {
                Some(PairAction::Close(pair)) => CursorEdit {
                    id,
                    range: offset..offset,
                    text: String::from_iter([pair.open, pair.close]),
                    caret: pair.open.len_utf8(),
                },
                Some(PairAction::Skip { width }) => CursorEdit {
                    id,
                    range: offset..offset,
                    text: String::new(),
                    caret: *width,
                },
                None => CursorEdit {
                    id,
                    range: offset..offset,
                    text: text.to_owned(),
                    caret: text.len(),
                },
            })
            .collect();

        // A bracket counts as typing even where it wrote nothing, since the run
        // is what decides whether an untouched auto-indent is stripped on the
        // way out of insert mode.
        if let Some(run) = self.current_insert_run.as_mut() {
            run.push(ch);
        }

        self.editor_edit_each(editor_id, buffer_id, insertions);
    }

    /// Type a colon at every cursor, swapping each `:name:` it closes for the
    /// emoji that name belongs to.
    ///
    /// A cursor with nothing to swap types the colon as written, so a
    /// multi-cursor session where only some cursors sit on a shortcode lands
    /// each of them correctly.
    ///
    /// The typed colon never reaches the buffer on a swap. One edit replaces
    /// the whole span, which is what makes a single undo take the glyph back
    /// rather than leaving the name behind.
    fn editor_insert_colon(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        let cursors = self.editor_cursor_offsets(editor_id);
        let expansions = {
            let ws = self.active_workspace();
            let Some(buffer) = ws.buffers.get(buffer_id) else {
                return;
            };
            let guard = buffer.read().expect("buffer poisoned");
            let rope = guard.rope();
            cursors
                .iter()
                .map(|&(_, offset)| emoji_expand::hook_insert(rope, offset))
                .collect::<Vec<_>>()
        };

        if expansions.iter().all(Option::is_none) {
            self.editor_insert_at(editor_id, buffer_id, ":", cursors);
            return;
        }

        let insertions = cursors
            .iter()
            .zip(&expansions)
            .map(|(&(id, offset), expansion)| match expansion {
                Some(swap) => CursorEdit {
                    id,
                    range: swap.open..offset,
                    text: swap.emoji.to_owned(),
                    caret: swap.emoji.len(),
                },
                None => CursorEdit {
                    id,
                    range: offset..offset,
                    text: ":".to_owned(),
                    caret: 1,
                },
            })
            .collect();

        // The colon counts as typing even where it wrote something else, since
        // the run is what decides whether an untouched auto-indent is stripped
        // on the way out of insert mode.
        if let Some(run) = self.current_insert_run.as_mut() {
            run.push(':');
        }

        self.editor_edit_each(editor_id, buffer_id, insertions);
    }

    /// Whether a typed closing colon swaps its shortcode for an emoji.
    ///
    /// A modal input answers `false` for the same reason it declines pairing.
    /// The command prompt is spelled with colons, and swapping one out from
    /// under a command is a nuisance rather than a convenience.
    fn emoji_expansion_enabled(&self) -> bool {
        self.settings.editor_emoji_expansion.unwrap_or(true) && self.active_modal_input().is_none()
    }

    /// The pair table for `buffer_id`, or `None` where nothing pairs.
    ///
    /// A modal input answers `None`. The search bar and the shell prompt both
    /// resolve as editors, and a prompt completing brackets is a nuisance rather
    /// than a convenience.
    fn auto_pairs_for(&self, buffer_id: BufferId) -> Option<AutoPairs> {
        if !self.settings.editor_auto_pairs.unwrap_or(true) {
            return None;
        }
        if self.active_modal_input().is_some() {
            return None;
        }

        let table = self
            .active_workspace()
            .buffers
            .language_for(buffer_id)
            .map(|lang| lang.pairs)
            .unwrap_or(auto_pairs::DEFAULT_PAIRS);
        Some(AutoPairs::new(table))
    }

    /// Every cursor in `editor_id` as `(selection id, byte offset)`, sorted by
    /// offset then id. Empty when the editor is gone.
    ///
    /// One walk for every cursor's endpoints rather than a root descent per
    /// anchor, which a multi-cursor session pays on every typed character.
    fn editor_cursor_offsets(&mut self, editor_id: EditorId) -> Vec<(usize, usize)> {
        let ws = self.active_workspace_mut();
        let Some(editor) = ws.editors.get_mut(editor_id) else {
            return Vec::new();
        };
        let display_snapshot = editor.display_map.snapshot();
        let buf_snapshot = display_snapshot.buffer_snapshot();
        let rope = buf_snapshot.rope();

        let ends = {
            let anchors: Vec<Anchor> = editor
                .selections
                .all_anchors()
                .iter()
                .flat_map(|sel| [sel.tail(), sel.head()])
                .collect();
            buf_snapshot.resolve_anchors_batch(&anchors)
        };

        let mut cursors: Vec<(usize, usize)> = editor
            .selections
            .all_anchors()
            .iter()
            .zip(ends.as_chunks::<2>().0.iter())
            .map(|(sel, ends)| (sel.id, stoat_text::cursor_offset(rope, ends[0], ends[1])))
            .collect();
        cursors.sort_by_key(|(id, offset)| (*offset, *id));
        cursors
    }

    /// Put each selection back as a block cursor at the offset `landings` gives
    /// for its id, leaving any selection the list does not name alone.
    ///
    /// `landings` is re-keyed by id here rather than by the caller, which holds
    /// it in offset order to compute the shifts. A binary search over a list
    /// already in hand beats hashing a fresh map into existence for every typed
    /// character.
    fn land_cursors_after_insert(
        &mut self,
        editor_id: EditorId,
        mut landings: Vec<(usize, usize)>,
    ) {
        landings.sort_unstable_by_key(|(id, _)| *id);

        let ws = self.active_workspace_mut();
        let Some(editor) = ws.editors.get_mut(editor_id) else {
            return;
        };
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let landings: Vec<(usize, usize, SelectionGoal)> = landings
            .into_iter()
            .map(|(id, offset)| (id, offset, SelectionGoal::None))
            .collect();
        editor.selections.land_block_cursors(&landings, new_buf);
    }

    /// Insert one indent step at every cursor.
    ///
    /// A tab style writes a tab, which is a tab stop wherever it lands. A space
    /// style writes only the spaces that reach the next stop, so a cursor at
    /// column 6 under a width of 4 gets two spaces and lands on 8. Writing a
    /// whole unit from an off-grid column keeps the indentation off the grid
    /// for good.
    pub(crate) fn editor_insert_indent(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        let style = self.buffer_indent_style(buffer_id);
        let IndentStyle::Spaces(width) = style else {
            self.editor_insert(editor_id, buffer_id, style.as_str());
            return;
        };
        let width = (width as usize).max(1);

        let cursors = self.editor_cursor_offsets(editor_id);
        let insertions = {
            let ws = self.active_workspace();
            let Some(buffer) = ws.buffers.get(buffer_id) else {
                return;
            };
            let guard = buffer.read().expect("buffer poisoned");
            let rope = guard.rope();

            cursors
                .into_iter()
                .map(|(id, offset)| {
                    // Characters rather than bytes, since the grid a tab stop
                    // sits on counts cells and not the storage behind them.
                    let column = rope
                        .reversed_chars_at(offset)
                        .take_while(|&ch| ch != '\n')
                        .count();
                    let text = " ".repeat(width - column % width);
                    (id, offset, text)
                })
                .collect()
        };

        self.editor_insert_each(editor_id, buffer_id, insertions);
    }

    /// Insert a string per cursor in one multi-edit, mirroring
    /// [`Self::editor_insert`]. `insertions` pairs each selection id with its
    /// cursor offset and the text that cursor inserts, in offset order.
    ///
    /// The uniform [`Self::editor_insert`] stays separate rather than
    /// delegating here. It runs on every typed character, where a string per
    /// cursor would be an allocation per cursor per keystroke that a uniform
    /// insertion has no use for.
    fn editor_insert_each(
        &mut self,
        editor_id: EditorId,
        buffer_id: BufferId,
        insertions: Vec<(usize, usize, String)>,
    ) {
        let insertions = insertions
            .into_iter()
            .map(|(id, offset, text)| CursorEdit {
                id,
                caret: text.len(),
                range: offset..offset,
                text,
            })
            .collect();
        self.editor_edit_each(editor_id, buffer_id, insertions);
    }

    /// Write one edit per cursor in a single multi-edit, landing each cursor
    /// where its own [`CursorEdit::caret`] names rather than after its text.
    ///
    /// Both departures from a plain insertion have callers. Auto-pairing lands
    /// the cursor between the halves it wrote, and steps a cursor over a closer
    /// it left alone. Enter takes out the whitespace it breaks after.
    ///
    /// The ranges must be sorted and must not overlap, which is what lets one
    /// running total carry every cursor's landing.
    fn editor_edit_each(
        &mut self,
        editor_id: EditorId,
        buffer_id: BufferId,
        insertions: Vec<CursorEdit>,
    ) {
        if insertions.is_empty() {
            return;
        }

        let ws = self.active_workspace_mut();
        let Some(buffer) = ws.buffers.get(buffer_id) else {
            return;
        };
        {
            let edits: Vec<(Range<usize>, &str)> = insertions
                .iter()
                .rev()
                .map(|edit| (edit.range.clone(), edit.text.as_str()))
                .collect();
            buffer.write().expect("poisoned").edit_batch(&edits);
        }

        // Each cursor is shifted by the net of everything written before it,
        // then by its own caret. The net differs per cursor, so this is a
        // running total where the uniform path multiplies one length by the
        // insertion's index.
        let mut shift = 0isize;
        let landings: Vec<(usize, usize)> = insertions
            .iter()
            .map(|edit| {
                let landing = (edit.range.start as isize + shift) as usize + edit.caret;
                shift += edit.text.len() as isize - edit.range.len() as isize;
                (edit.id, landing)
            })
            .collect();

        self.land_cursors_after_insert(editor_id, landings);
    }

    /// Byte offset of the focused editor's newest cursor.
    pub(crate) fn newest_cursor_offset(&mut self, editor_id: EditorId) -> Option<usize> {
        let ws = self.active_workspace_mut();
        let editor = ws.editors.get_mut(editor_id)?;
        let snapshot = editor.display_map.snapshot();
        let buf = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buf.resolve_anchor(&sel.tail());
        let head_off = buf.resolve_anchor(&sel.head());
        Some(stoat_text::cursor_offset(buf.rope(), tail_off, head_off))
    }

    /// Leading whitespace to give a new line inserted at `cursor_offset`.
    ///
    /// Uses the buffer's `indents.scm` query against a fresh syntax tree. When
    /// the tree is stale or the language has no indent query, it copies the
    /// cursor row's own leading whitespace instead.
    pub(crate) fn newline_indent_string(
        &self,
        buffer_id: BufferId,
        cursor_offset: usize,
    ) -> String {
        let buffers = &self.active_workspace().buffers;
        let Some(buffer) = buffers.get(buffer_id) else {
            return String::new();
        };
        let guard = buffer.read().expect("buffer poisoned");
        let rope = guard.rope();
        let row = rope.offset_to_point(cursor_offset).row;

        let fresh_tree = buffers
            .language_for(buffer_id)
            .and_then(|lang| lang.newline_indent_queries().is_some().then_some(lang))
            .zip(buffers.syntax(buffer_id))
            .filter(|(_, syntax)| syntax.version == guard.version());

        match fresh_tree {
            Some((lang, syntax)) => language::newline_indent(
                lang.newline_indent_queries()
                    .expect("indent queries present"),
                syntax.tree.root_node(),
                &syntax.rope_snapshot,
                cursor_offset,
                guard.indent_style().as_str(),
            ),
            None => language::line_leading_whitespace(rope, row),
        }
    }

    /// The text one Enter writes at `cursor_offset`, and how far into it the
    /// cursor lands.
    ///
    /// On a line whose first non-whitespace run is one of the language's
    /// line-comment tokens, the new line carries that token forward (indented to
    /// the line's own leading whitespace) so a comment block continues.
    /// Otherwise the indent is the syntax-derived one from
    /// [`Self::newline_indent_string`].
    ///
    /// The cursor must sit past the token for the continuation to apply. A
    /// cursor still inside the leading whitespace has no comment behind it to
    /// continue, and a token carried there lands ahead of the token already on
    /// the line.
    /// A cursor between the halves of a pair opens two lines rather than one.
    /// The first carries an extra indent level and takes the cursor, and the
    /// closing half lands on the second at the outer level.
    fn newline_continuation(&self, buffer_id: BufferId, cursor_offset: usize) -> (String, usize) {
        if let Some(prefix) = self.continued_comment_prefix(buffer_id, cursor_offset) {
            let text = format!("\n{prefix}");
            let caret = text.len();
            return (text, caret);
        }

        let indent = self.newline_indent_string(buffer_id, cursor_offset);
        if self.cursor_inside_pair(buffer_id, cursor_offset) {
            let unit = self.buffer_indent_style(buffer_id).as_str();
            let caret = "\n".len() + indent.len() + unit.len();
            return (format!("\n{indent}{unit}\n{indent}"), caret);
        }

        let text = format!("\n{indent}");
        let caret = text.len();
        (text, caret)
    }

    /// The indent and comment token a new line carries to continue the comment
    /// `cursor_offset` sits in, or `None` outside one.
    fn continued_comment_prefix(
        &self,
        buffer_id: BufferId,
        cursor_offset: usize,
    ) -> Option<String> {
        let buffers = &self.active_workspace().buffers;
        let tokens = buffers
            .language_for(buffer_id)
            .map_or(&[][..], |lang| lang.line_comments);
        let buffer = buffers.get(buffer_id)?;

        let guard = buffer.read().expect("buffer poisoned");
        let rope = guard.rope();
        let row = rope.offset_to_point(cursor_offset).row;
        let line_start = rope.point_to_offset(stoat_text::Point::new(row, 0));
        let line_end = rope.point_to_offset(stoat_text::Point::new(row, rope.line_len(row)));

        action_handlers::movement::line_comment_continues(rope, line_start, line_end, tokens)
            .filter(|&(start, _)| start < cursor_offset)
            .map(|(_, token)| format!("{}{token} ", language::line_leading_whitespace(rope, row)))
    }

    /// True when `cursor_offset` sits between the two halves of a pair.
    ///
    /// Answered through [`Self::auto_pairs_for`], so a reader who turned
    /// pairing off gets none of what it implies either.
    fn cursor_inside_pair(&self, buffer_id: BufferId, cursor_offset: usize) -> bool {
        let Some(pairs) = self.auto_pairs_for(buffer_id) else {
            return false;
        };
        let Some(buffer) = self.active_workspace().buffers.get(buffer_id) else {
            return false;
        };
        let guard = buffer.read().expect("buffer poisoned");
        auto_pairs::enclosing_pair(guard.rope(), cursor_offset, pairs).is_some()
    }

    /// The leading whitespace of `row` in `buffer_id`, for opening a line at the
    /// same indentation as an existing one.
    pub(crate) fn line_indent_string(&self, buffer_id: BufferId, row: u32) -> String {
        let buffers = &self.active_workspace().buffers;
        let Some(buffer) = buffers.get(buffer_id) else {
            return String::new();
        };
        let guard = buffer.read().expect("buffer poisoned");
        language::line_leading_whitespace(guard.rope(), row)
    }

    /// The indentation unit `buffer_id` uses, detected from its content, for
    /// inserting or removing one indent level. Falls back to the default for a
    /// missing buffer.
    pub(crate) fn buffer_indent_style(&self, buffer_id: BufferId) -> IndentStyle {
        self.active_workspace()
            .buffers
            .get(buffer_id)
            .map(|buffer| buffer.read().expect("buffer poisoned").indent_style())
            .unwrap_or_default()
    }

    /// What a formatting request should tell a server about `buffer_id`'s
    /// indentation.
    ///
    /// A server is entitled to take these literally, and the spec's own default
    /// is a tab size of zero with spaces off, which describes no indentation at
    /// all. Answering from the style the buffer was detected to use is also what
    /// keeps a tab-indented file from coming back in spaces.
    pub(crate) fn buffer_formatting_options(
        &self,
        buffer_id: BufferId,
    ) -> lsp_types::FormattingOptions {
        let style = self.buffer_indent_style(buffer_id);
        lsp_types::FormattingOptions {
            tab_size: style.indent_width(TAB_WIDTH) as u32,
            insert_spaces: matches!(style, IndentStyle::Spaces(_)),
            ..lsp_types::FormattingOptions::default()
        }
    }

    /// The leading whitespace `row` in `buffer_id` should carry given its
    /// enclosing syntax, for re-indenting a blank line to its block depth.
    ///
    /// Unlike [`Self::newline_indent_string`], which derives the indent of a new
    /// line from the row it is opened after, this resolves the indent the row
    /// itself belongs at via the buffer's `indents.scm` query. Falls back to the
    /// row's own leading whitespace when the tree is stale, the language has no
    /// indent query, or the query offers no suggestion.
    pub(crate) fn suggested_indent_string(&self, buffer_id: BufferId, row: u32) -> String {
        let buffers = &self.active_workspace().buffers;
        let Some(buffer) = buffers.get(buffer_id) else {
            return String::new();
        };
        let guard = buffer.read().expect("buffer poisoned");
        let rope = guard.rope();

        let fresh_tree = buffers
            .language_for(buffer_id)
            .and_then(|lang| lang.indent_query().is_some().then_some(lang))
            .zip(buffers.syntax(buffer_id))
            .filter(|(_, syntax)| syntax.version == guard.version());

        match fresh_tree {
            Some((lang, syntax)) => language::suggested_indent(
                lang.indent_query().expect("indent query present"),
                syntax.tree.root_node(),
                &syntax.rope_snapshot,
                row,
                guard.indent_style().as_str(),
            )
            .unwrap_or_else(|| language::line_leading_whitespace(rope, row)),
            None => language::line_leading_whitespace(rope, row),
        }
    }

    fn editor_backspace(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        let indent_width = self.buffer_indent_style(buffer_id).indent_width(TAB_WIDTH);
        let pairs = self.auto_pairs_for(buffer_id);
        self.editor_delete_ranges(editor_id, buffer_id, move |rope, cursor| {
            backspace_range(rope, cursor, indent_width, pairs)
        });
    }

    fn editor_delete_word_backward(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        self.editor_delete_ranges(editor_id, buffer_id, |rope, cursor| {
            (stoat_text::prev_word_start(rope, cursor), cursor)
        });
    }

    fn editor_delete(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        self.editor_delete_ranges(editor_id, buffer_id, |rope, cursor| {
            (cursor, rope.next_grapheme_boundary(cursor))
        });
    }

    fn editor_delete_word_forward(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        self.editor_delete_ranges(editor_id, buffer_id, |rope, cursor| {
            (cursor, stoat_text::next_word_end(rope, cursor))
        });
    }

    fn editor_kill_to_line_start(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        self.editor_delete_ranges(editor_id, buffer_id, |rope, cursor| {
            (kill_to_line_start_target(rope, cursor), cursor)
        });
    }

    fn editor_kill_to_line_end(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        self.editor_delete_ranges(editor_id, buffer_id, |rope, cursor| {
            let row = rope.offset_to_point(cursor).row;
            let line_end = rope.point_to_offset(stoat_text::Point::new(row, rope.line_len(row)));
            if cursor < line_end {
                return (cursor, line_end);
            }
            (cursor, rope.next_grapheme_boundary(cursor))
        });
    }

    /// Insert a register's fragments at the cursors, one fragment per cursor in
    /// offset order.
    ///
    /// A register holds one fragment per selection because that is how a
    /// multi-cursor yank recorded what each cursor took, so each fragment goes
    /// back to the cursor in its position. Cursors past the fragment count
    /// repeat the last, which is what pasting the same register does.
    fn editor_insert_register(
        &mut self,
        editor_id: EditorId,
        buffer_id: BufferId,
        fragments: &[String],
    ) {
        let Some(last) = fragments.last() else {
            return;
        };

        let cursors = self.editor_cursor_offsets(editor_id);
        let insertions: Vec<(usize, usize, String)> = cursors
            .into_iter()
            .enumerate()
            .map(|(idx, (id, offset))| (id, offset, fragments.get(idx).unwrap_or(last).clone()))
            .collect();

        // Repeat replays one string, and the newest cursor's fragment is one
        // that was actually inserted, where the blob the fragments used to be
        // joined into is now inserted nowhere.
        let newest = self
            .active_workspace()
            .editors
            .get(editor_id)
            .map(|editor| editor.selections.newest_anchor().id);
        if let Some(newest) = newest
            && let Some((_, _, text)) = insertions.iter().find(|(id, _, _)| *id == newest)
        {
            let text = text.clone();
            if let Some(run) = self.current_insert_run.as_mut() {
                run.push_str(&text);
            }
        }

        self.editor_insert_each(editor_id, buffer_id, insertions);
    }

    fn editor_insert_newline(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        let cursors = self.editor_cursor_offsets(editor_id);

        let breaks = {
            let ws = self.active_workspace();
            let Some(buffer) = ws.buffers.get(buffer_id) else {
                return;
            };
            let guard = buffer.read().expect("buffer poisoned");
            let rope = guard.rope();

            // A cursor whose break trims whitespace bounds the next cursor's
            // trim, so two cursors on one line take the run between them once.
            let mut floor = 0;
            cursors
                .iter()
                .map(|&(_, cursor)| {
                    let brk = line_break_at(rope, cursor, floor);
                    if matches!(brk, LineBreak::Continue { .. }) {
                        floor = cursor;
                    }
                    brk
                })
                .collect::<Vec<_>>()
        };

        let edits: Vec<CursorEdit> = cursors
            .iter()
            .zip(&breaks)
            .map(|(&(id, cursor), brk)| match *brk {
                LineBreak::Continue { from } => {
                    let (text, caret) = self.newline_continuation(buffer_id, cursor);
                    CursorEdit {
                        id,
                        range: from..cursor,
                        caret,
                        text,
                    }
                },
                LineBreak::PushDown { line_start } => CursorEdit {
                    id,
                    range: line_start..line_start,
                    text: "\n".to_string(),
                    caret: 1 + cursor - line_start,
                },
            })
            .collect();

        // Repeat replays one string, and there is no one string when every
        // cursor continues its own line. The newest cursor's is what the
        // uniform insertion recorded before the others had their own.
        let newest = self.newest_cursor_offset(editor_id);
        if let Some(text) = cursors
            .iter()
            .position(|&(_, offset)| Some(offset) == newest)
            .and_then(|idx| edits.get(idx))
            .map(|edit| edit.text.clone())
            && let Some(run) = self.current_insert_run.as_mut()
        {
            run.push_str(&text);
        }

        self.editor_edit_each(editor_id, buffer_id, edits);
    }

    /// Delete a per-selection range at every cursor in one multi-edit, mirroring
    /// [`Self::editor_insert`]. `range_for` maps each cursor offset to its
    /// `[start, end)` deletion span. An empty span means the cursor sits at a
    /// no-op boundary (buffer start, buffer end, or word start), so it deletes
    /// nothing and only follows the leftward shift.
    ///
    /// Overlapping spans merge before the edit, so two cursors inside one word
    /// remove the shared span once rather than double-deleting it. Each cursor
    /// then lands at its deletion start. Cursors that collapse to the same
    /// offset dedupe when the selections are rebuilt.
    fn editor_delete_ranges<F>(&mut self, editor_id: EditorId, buffer_id: BufferId, range_for: F)
    where
        F: Fn(&Rope, usize) -> (usize, usize),
    {
        let ws = self.active_workspace_mut();
        let editor = match ws.editors.get_mut(editor_id) {
            Some(e) => e,
            None => return,
        };
        let buffer = match ws.buffers.get(buffer_id) {
            Some(b) => b,
            None => return,
        };
        let display_snapshot = editor.display_map.snapshot();
        let buf_snapshot = display_snapshot.buffer_snapshot();
        let rope = buf_snapshot.rope();

        let ends = {
            let anchors: Vec<Anchor> = editor
                .selections
                .all_anchors()
                .iter()
                .flat_map(|sel| [sel.tail(), sel.head()])
                .collect();
            buf_snapshot.resolve_anchors_batch(&anchors)
        };

        let per_sel: Vec<(usize, usize, usize)> = editor
            .selections
            .all_anchors()
            .iter()
            .zip(ends.as_chunks::<2>().0.iter())
            .map(|(sel, ends)| {
                let cursor = stoat_text::cursor_offset(rope, ends[0], ends[1]);
                let (start, end) = range_for(rope, cursor);
                // Word motions stop mid-cluster deliberately, leaving the snap to
                // wherever their answer lands. This path writes bytes rather than
                // selections, so `SelectionsCollection::replace_with`'s snap never
                // runs. The same rule applies here, so a deletion only ever grows out
                // to the character it was cutting and never splits one.
                let start = rope.clip_to_grapheme_boundary(start, Bias::Left);
                let end = rope.clip_to_grapheme_boundary(end, Bias::Right);
                (sel.id, start, end)
            })
            .collect();

        let ranges: Vec<(usize, usize)> = per_sel
            .iter()
            .filter(|(_, start, end)| start < end)
            .map(|&(_, start, end)| (start, end))
            .collect();
        if ranges.is_empty() {
            return;
        }

        let merged = merge_overlapping_spans(ranges);

        {
            let edits: Vec<(Range<usize>, &str)> = merged
                .iter()
                .rev()
                .map(|(start, end)| (*start..*end, ""))
                .collect();
            buffer.write().expect("poisoned").edit_batch(&edits);
        }

        // Bytes deleted by everything before each range, computed once for the
        // whole selection set rather than re-accumulated per selection.
        let mut deleted_before = Vec::with_capacity(merged.len() + 1);
        let mut running = 0;
        for (start, end) in &merged {
            deleted_before.push(running);
            running += end - start;
        }
        deleted_before.push(running);

        let mut new_offsets: Vec<(usize, usize, SelectionGoal)> = per_sel
            .iter()
            .map(|&(id, start, _)| {
                (
                    id,
                    Self::offset_after_deletions(start, &merged, &deleted_before),
                    SelectionGoal::None,
                )
            })
            .collect();
        // Sorted so the closure can binary-search rather than hash a map built
        // for one pass over the selections.
        new_offsets.sort_unstable_by_key(|(id, _, _)| *id);

        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        editor.selections.land_block_cursors(&new_offsets, new_buf);
    }

    /// New offset of `target` after deleting the ascending, disjoint `ranges`.
    /// A target inside a deleted range collapses to that range's start.
    ///
    /// `deleted_before[i]` is the total bytes `ranges[..i]` remove, with one
    /// trailing entry for the whole list. The caller builds it once for every
    /// selection it has to move, which is what keeps a delete over many cursors
    /// from walking the range list per cursor.
    fn offset_after_deletions(
        target: usize,
        ranges: &[(usize, usize)],
        deleted_before: &[usize],
    ) -> usize {
        // Ends ascend, so this is the count of ranges lying entirely before
        // `target`, and the first range that could straddle it.
        let ix = ranges.partition_point(|&(_, end)| end <= target);
        let deleted = deleted_before[ix];
        match ranges.get(ix) {
            Some(&(start, _)) if start < target => start - deleted,
            _ => target - deleted,
        }
    }

    /// Apply one agent hook event to the workspace whose session matches
    /// `ev.uid`, creating the [`AgentStatus`] on first contact. Returns
    /// [`UpdateEffect::None`] when no live workspace owns that session, e.g.
    /// the workspace closed before its agent's events drained.
    pub(crate) fn handle_agent_event(&mut self, ev: AgentEvent) -> UpdateEffect {
        let Some((_, ws)) = self.workspaces.iter_mut().find(|(_, ws)| ws.uid == ev.uid) else {
            return UpdateEffect::None;
        };
        ws.agent
            .get_or_insert_with(AgentStatus::new)
            .apply(ev.event);
        UpdateEffect::Redraw
    }

    /// Open a temp-file editor an owned agent shelled out to, in the workspace
    /// whose session matches the request's `uid`, and register a waiter so
    /// closing that buffer or its pane unblocks the agent.
    ///
    /// Switches the active workspace to the owning session so the editor lands
    /// beside the agent pane, splits a new pane for the file, and parks the
    /// request's sender on the opened buffer through [`Workspace::hold_buffer`].
    /// A buffer that is already held gains a second waiter. A file that reads on
    /// the blocking pool parks the sender on that read, which hands it to the
    /// buffer the read installs as. Returns [`UpdateEffect::None`] when no live
    /// workspace owns the session or the file does not open. The dropped sender
    /// then unblocks the agent so its `$EDITOR` invocation does not hang.
    ///
    /// An [`AgentControl::ClientGone`] drops the waiters of a command that
    /// went away, and leaves the active workspace as it is.
    pub(crate) fn handle_agent_control(&mut self, ctl: AgentControl) -> UpdateEffect {
        match ctl {
            AgentControl::OpenEditor {
                uid,
                client,
                path,
                done,
            } => {
                let Some(ws_id) = self
                    .workspaces
                    .iter()
                    .find(|(_, ws)| ws.uid == uid)
                    .map(|(id, _)| id)
                else {
                    return UpdateEffect::None;
                };
                self.active_workspace = ws_id;

                let new_pane = {
                    let ws = self.active_workspace_mut();
                    let new_pane = ws.panes.split(crate::pane::Axis::Vertical);
                    ws.focus = FocusTarget::SplitPane;
                    new_pane
                };

                let waiter = BridgeWaiter {
                    client,
                    label: "agent".to_string(),
                    done,
                };
                let held = match crate::buffer_lifecycle::open_file_in_pane(self, new_pane, &path) {
                    Some(buffer_id) => {
                        self.active_workspace_mut().hold_buffer(buffer_id, waiter);
                        true
                    },
                    None => crate::buffer_lifecycle::hold_pending_open(self, &path, waiter),
                };
                if !held {
                    return UpdateEffect::None;
                }
                UpdateEffect::Redraw
            },
            AgentControl::OpenInTerm {
                uid,
                client,
                term,
                paths,
                hold,
                done,
            } => {
                let Some(ws_id) = self
                    .workspaces
                    .iter()
                    .find(|(_, ws)| ws.uid == uid)
                    .map(|(id, _)| id)
                else {
                    let _ = done.send(false);
                    return UpdateEffect::None;
                };
                self.active_workspace = ws_id;

                let target = {
                    let ws = self.active_workspace_mut();
                    match Self::terminal_pane_for_token(ws, term) {
                        Some((pane, term_id)) => {
                            // The reverse of the record `open_terminal_pane`
                            // makes, so the shell stays reachable behind the
                            // buffer now covering it.
                            ws.panes.pane_mut(pane).prev_view = Some(View::Terminal(term_id));
                            ws.panes.set_focus(pane);
                            ws.focus = FocusTarget::SplitPane;
                            pane
                        },
                        None => ws.panes.focus(),
                    }
                };

                let Some((first, rest)) = paths.split_first() else {
                    let _ = done.send(false);
                    return UpdateEffect::None;
                };
                let mut opens = vec![(
                    first,
                    crate::buffer_lifecycle::open_file_in_pane(self, target, first),
                )];
                for path in rest {
                    let split = self
                        .active_workspace_mut()
                        .panes
                        .split(crate::pane::Axis::Vertical);
                    opens.push((
                        path,
                        crate::buffer_lifecycle::open_file_in_pane(self, split, path),
                    ));
                }

                // The original sender drops with the match, so the clones on
                // the buffers and on the reads still on the pool are the only
                // ones. The command returns when the last buffer leaves the
                // editor.
                let held = match hold {
                    Some(hold) => {
                        let label = self
                            .active_workspace()
                            .terms
                            .values()
                            .find(|session| session.token == term)
                            .and_then(|session| session.session.foreground_process_name())
                            .unwrap_or_else(|| "shell".to_string());
                        let mut held = false;
                        for (path, opened) in opens {
                            let waiter = BridgeWaiter {
                                client,
                                label: label.clone(),
                                done: hold.clone(),
                            };
                            held |= match opened {
                                Some(id) => {
                                    self.active_workspace_mut().hold_buffer(id, waiter);
                                    true
                                },
                                None => {
                                    crate::buffer_lifecycle::hold_pending_open(self, path, waiter)
                                },
                            };
                        }
                        held
                    },
                    None => false,
                };
                let _ = done.send(held);
                UpdateEffect::Redraw
            },
            AgentControl::ClientGone { uid, client } => {
                let Some((id, ws)) = self.workspaces.iter_mut().find(|(_, ws)| ws.uid == uid)
                else {
                    return UpdateEffect::None;
                };
                ws.drop_client_waiters(client);
                crate::buffer_lifecycle::drop_pending_waiters(self, id, client);
                UpdateEffect::Redraw
            },
            AgentControl::Query {
                uid,
                request,
                reply,
            } => {
                crate::agent_ipc::answer_agent_query(self, uid, request, reply);
                UpdateEffect::None
            },
        }
    }

    /// The split pane showing the terminal session named by `token`, with that
    /// session's id.
    ///
    /// `None` when no session carries the token, or when the one that does
    /// shows only in a dock, which holds no buffer to open into.
    fn terminal_pane_for_token(ws: &Workspace, token: u64) -> Option<(PaneId, TermId)> {
        let term_id = ws
            .terms
            .iter()
            .find(|(_, session)| session.token == token)
            .map(|(id, _)| id)?;
        let pane = ws
            .panes
            .split_pane_ids()
            .into_iter()
            .find(|&id| matches!(ws.panes.pane(id).view, View::Terminal(t) if t == term_id))?;
        Some((pane, term_id))
    }

    /// Start the per-session agent hook server for `uid` on the executor.
    ///
    /// Binds the session's hook socket under [`Self::agent_socket_dir`] and
    /// forwards decoded events to [`Self::handle_agent_event`] through the
    /// shared channel. Callers spawn this alongside the owned Claude subshell,
    /// which reaches it by `STOAT_AGENT_SOCK`, and a terminal pane's own shell
    /// carries the same variable.
    ///
    /// Every spawn into a workspace calls this. A `uid` is served once, and
    /// later calls for it do nothing. Serves nothing and reports success
    /// without [`Self::set_serve_agent_sockets`] or a directory, which is the
    /// default a test runs under.
    pub fn serve_term_session(&mut self, uid: WorkspaceUid) -> io::Result<()> {
        if !self.serve_agent_sockets {
            return Ok(());
        }
        let socket_path = {
            let Some(dir) = self.agent_socket_dir.as_deref() else {
                return Ok(());
            };
            crate::run::agent_socket_path_in(dir, uid)
        };
        if self.agent_servers.contains_key(&uid) {
            return Ok(());
        }

        let tx = self.agent_event_tx.clone();
        let control_tx = self.agent_control_tx.clone();
        let task = self.executor.spawn(crate::agent_ipc::serve_agent_hooks(
            socket_path,
            uid,
            tx,
            control_tx,
        ));
        self.agent_servers.insert(uid, task);
        Ok(())
    }

    pub(crate) fn handle_pty_notification(&mut self, notif: PtyNotification) -> UpdateEffect {
        let clipboard_host = self.clipboard_host.clone();
        let env_host = self.env_host.clone();
        let modal_run = self.modal_run;
        let ws = self.active_workspace_mut();
        match notif {
            PtyNotification::Output { run_id, data } => {
                // The block still feeds while hidden, but a hidden run drives no
                // repaint. Revealing it repaints on the toggle's own dispatch.
                let visible = Self::run_visible(ws, run_id, modal_run);
                let Some(run_state) = ws.runs.get_mut(run_id) else {
                    return UpdateEffect::None;
                };
                let Some(block) = run_state.active_block_mut() else {
                    return UpdateEffect::None;
                };
                block.feed(&data);
                // An OSC 133 done mark finalizes the block with its exit code.
                // Start marks are drained but unused. Blocks are created at
                // submit time.
                for mark in std::mem::take(&mut block.grid.command_marks) {
                    if let CommandMark::Done { exit } = mark
                        && !block.finished
                    {
                        block.finished = true;
                        block.exit_status = exit;
                    }
                }
                for text in block.grid.clipboard_writes.drain(..) {
                    crate::host::clipboard_copy(
                        clipboard_host.as_ref(),
                        env_host.as_ref(),
                        ClipboardKind::System,
                        &text,
                    );
                }
                // Adopt the latest OSC 7 cwd report. Captured before the
                // alt-screen branch reborrows run_state below.
                let reported_cwd = std::mem::take(&mut block.grid.cwd_reports).pop();
                if block.grid.alt_screen_detected {
                    block.error = Some("this command requires a full terminal".into());
                    block.finished = true;
                    block.grid.alt_screen_detected = false;
                    if let Some(handle) = &mut run_state.shell_handle {
                        handle.kill();
                    }
                    run_state.shell_handle = None;
                }
                if let Some(cwd) = reported_cwd {
                    run_state.cwd = cwd;
                }
                run_state.trim_blocks();
                if visible {
                    self.pty_dirty = true;
                }
                UpdateEffect::None
            },
            PtyNotification::CommandDone {
                run_id,
                exit_status,
            } => {
                let Some(run_state) = ws.runs.get_mut(run_id) else {
                    return UpdateEffect::None;
                };
                let Some(block) = run_state.active_block_mut() else {
                    return UpdateEffect::None;
                };
                if !block.finished {
                    block.finished = true;
                    block.exit_status = exit_status;
                }
                UpdateEffect::Redraw
            },
            PtyNotification::TermOutput { agent_id, data } => {
                // Computed before the feed so the later self.write_to_term does
                // not collide with a borrow of ws. A hidden term still feeds but
                // drives no repaint until a surface reveals it.
                let visible = Self::term_visible(ws, agent_id);
                let Some(agent) = ws.terms.get_mut(agent_id) else {
                    return UpdateEffect::None;
                };
                let replies = agent.term.feed(&data);
                let clipboard_writes = agent.term.take_clipboard_writes();
                let retitled = agent.term.take_retitled();
                if !replies.is_empty() {
                    self.write_to_term(agent_id, &replies);
                }
                for text in clipboard_writes {
                    crate::host::clipboard_copy(
                        clipboard_host.as_ref(),
                        env_host.as_ref(),
                        ClipboardKind::System,
                        &text,
                    );
                }
                if visible || retitled {
                    self.pty_dirty = true;
                }
                UpdateEffect::None
            },
            PtyNotification::SshOutput { data } => ssh::forward_output(self, data),
            PtyNotification::SshExited { exit_status } => ssh::finish(self, exit_status),
            PtyNotification::TermExited { term_id } => {
                let pane_ids = ws
                    .panes
                    .split_pane_ids()
                    .into_iter()
                    .filter(
                        |&id| matches!(ws.panes.pane(id).view, View::Terminal(t) if t == term_id),
                    )
                    .collect::<Vec<_>>();
                let dock_ids = ws
                    .docks
                    .iter()
                    .filter_map(|(id, dock)| {
                        matches!(dock.view, View::Terminal(t) if t == term_id).then_some(id)
                    })
                    .collect::<Vec<_>>();

                // With nothing showing the session as a terminal, what happens
                // next turns on whether an agent view holds it. An agent shares
                // the reader and keeps its last frame after the shell dies, so
                // its session has to outlive the exit. A session no view holds
                // at all is a terminal hidden behind a buffer, which retires
                // like any other. Its records go with it, since the Terminal
                // action has nothing to return to once the shell is gone.
                if pane_ids.is_empty() && dock_ids.is_empty() {
                    let agent_displayed =
                        ws.panes.split_pane_ids().into_iter().any(
                            |id| matches!(ws.panes.pane(id).view, View::Agent(t) if t == term_id),
                        ) || ws
                            .docks
                            .iter()
                            .any(|(_, dock)| matches!(dock.view, View::Agent(t) if t == term_id));
                    if agent_displayed {
                        return UpdateEffect::None;
                    }

                    ws.terms.remove(term_id);
                    for id in ws.panes.split_pane_ids() {
                        let names_exited = matches!(
                            ws.panes.pane(id).prev_view,
                            Some(View::Terminal(t) | View::Agent(t)) if t == term_id,
                        );
                        if names_exited {
                            ws.panes.pane_mut(id).prev_view = None;
                        }
                    }
                    // Nothing on screen showed it, so nothing on screen changed.
                    return UpdateEffect::None;
                }

                // Keys reach a terminal only when a split pane holds focus
                // (see `term_input_target`), so a focused dock must not
                // trigger the reset. Recorded before the loop closes
                // or restores the pane, which reassigns focus.
                let exited_held_focus = matches!(ws.focus, FocusTarget::SplitPane)
                    && pane_ids.contains(&ws.panes.focus());

                ws.terms.remove(term_id);
                for dock_id in dock_ids {
                    if let Some(dock) = ws.docks.get_mut(dock_id) {
                        dock.view = View::Label("terminal exited".into());
                    }
                }

                for pane_id in pane_ids {
                    if !action_handlers::close_pane_by_id(self, pane_id) {
                        action_handlers::restore_pane_after_term_exit(self, pane_id);
                    }
                }

                if exited_held_focus && self.focused_mode() == "insert" {
                    self.transition_mode("normal".to_string());
                }
                UpdateEffect::Redraw
            },
        }
    }

    /// Whether any visible surface shows run `run_id`.
    ///
    /// A split pane always counts as visible. A dock counts only when not hidden. A run
    /// shown modally counts too. Gates a run block's output-driven repaint.
    fn run_visible(ws: &Workspace, run_id: RunId, modal_run: Option<RunId>) -> bool {
        modal_run == Some(run_id)
            || ws
                .panes
                .split_panes()
                .any(|(_, pane)| matches!(pane.view, View::Run(id) if id == run_id))
            || ws.docks.values().any(|dock| {
                dock.visibility != DockVisibility::Hidden
                    && matches!(dock.view, View::Run(id) if id == run_id)
            })
    }

    /// Whether any visible surface shows terminal `term_id`, as either a terminal
    /// or an agent view.
    ///
    /// A split pane always counts as visible. A dock counts only when not hidden. Gates
    /// a terminal's output-driven repaint.
    fn term_visible(ws: &Workspace, term_id: TermId) -> bool {
        ws.panes.split_panes().any(
            |(_, pane)| matches!(pane.view, View::Agent(id) | View::Terminal(id) if id == term_id),
        ) || ws.docks.values().any(|dock| {
            dock.visibility != DockVisibility::Hidden
                && matches!(dock.view, View::Agent(id) | View::Terminal(id) if id == term_id)
        })
    }

    /// Drive background parse jobs: poll any in-flight tasks for completion,
    /// install their results, then spawn new jobs for visible buffers whose
    /// stored syntax version is stale.
    ///
    /// At most one job per buffer is in flight at a time. If a buffer advances
    /// past the in-flight job's `target_version`, the new job is queued only
    /// after the old one completes. Anchors in the result are computed using
    /// the parsed snapshot, so they remain valid even if the buffer has been
    /// edited further while the parse was running.
    fn drive_parse_jobs(&mut self) {
        let retention = self
            .settings
            .highlight_retention
            .unwrap_or(DEFAULT_HIGHLIGHT_RETENTION) as usize;
        let installed = {
            let Self {
                workspaces,
                active_workspace,
                executor,
                syntax_styles,
                redraw_notify,
                index_update_tx,
                ..
            } = self;
            workspaces[*active_workspace].drive_parse_jobs(
                executor,
                syntax_styles,
                redraw_notify,
                index_update_tx,
                retention,
            )
        };

        // Tell each strip which rows the parse restained, so its recolor sweep
        // covers those instead of re-summarizing the whole file. A buffer with
        // no strip yet needs nothing, since a strip's initial build reads
        // whatever tokens are current by the time it runs.
        let ws_id = self.active_workspace;
        for (buffer_id, rows) in installed {
            if let Some(content) = self.minimap_content.get_mut(&(ws_id, buffer_id)) {
                content.note_syntax_rows(rows);
            }
        }
    }

    /// Populate the active workspace's visible git-tracked buffers' diff maps.
    ///
    /// Gated on [`Self::diff_warm_auto`] like the diff-cache warm, so the test
    /// harness never spawns git diff jobs unbidden. Production enables it at
    /// startup.
    fn drive_diff_jobs(&mut self) {
        if !self.diff_warm_auto {
            return;
        }
        let Self {
            workspaces,
            active_workspace,
            executor,
            git_host,
            language_registry,
            syntax_styles,
            base_highlights_cache,
            redraw_notify,
            ..
        } = self;
        workspaces[*active_workspace].drive_diff_jobs(
            executor,
            git_host,
            language_registry,
            syntax_styles,
            base_highlights_cache,
            redraw_notify,
        );
    }

    /// Paint the current state into a fresh [`Buffer`] and return it.
    ///
    /// A convenience wrapper over [`Self::paint_into`] for the test harness,
    /// which snapshots the returned buffer. The event loop instead recycles a
    /// buffer across frames via [`Self::paint_into`], so this is otherwise
    /// unused.
    #[allow(dead_code)]
    pub(crate) fn render(&mut self) -> Buffer {
        let mut buf = Buffer::empty(self.size);
        self.paint_into(&mut buf);
        buf
    }

    /// Paint the current state into `buf`, reusing its allocation.
    ///
    /// Resizes `buf` to the current screen and blanks it to the theme's own
    /// colors before drawing, so a recycled buffer paints byte-identically to a
    /// fresh one. The event loop recycles the prior frame's buffer this way once
    /// the render thread releases it, avoiding a per-frame screen allocation.
    ///
    /// Blanking to the theme rather than to Reset is what keeps the ambient
    /// screen following `:theme`. The terminal resolves a cell that reaches it
    /// at Reset against its own theme instead.
    fn paint_into(&mut self, buf: &mut Buffer) {
        self.render_tick += 1;
        buf.resize(self.size);
        buf.content.fill(crate::render::themed_blank(&self.theme));

        // Keep every editor's syntax coloring in step with the session toggle
        // before painting, so a newly opened editor inherits the current
        // state. set_syntax_highlighting is a no-op when already in sync.
        //
        // The diff toggle only ever subtracts. A pane the session toggle left
        // plain stays plain, and a diff view under a diff toggle that is off
        // goes plain whatever the session says.
        let syntax = self.syntax_highlight;
        let diff_syntax = self.diff_syntax;
        for editor in self.active_workspace_mut().editors.values_mut() {
            let on = syntax && (diff_syntax || !editor.diff_view);
            editor.display_map.set_syntax_highlighting(on);
        }

        // Take the scene and undercurl buffers out so `frame` can hold `&mut`
        // to them alongside its `&mut self` borrow. Widgets append into the
        // scene and the editor renderer records diagnostic spans during paint.
        let mut scene = std::mem::take(&mut self.apc_scene);
        scene.clear();
        // Re-declared per frame rather than at construction. The session paints
        // before the ident handshake answers, so a scene built with `Stoat` would
        // be stuck at whatever was true then.
        scene.set_live(self.stoatty);
        let mut undercurls = std::mem::take(&mut self.pending_undercurls);
        undercurls.begin();
        crate::render::frame(self, buf, &mut scene, &mut undercurls);
        self.apc_scene = scene;
        self.pending_undercurls = undercurls;
    }

    /// Drive the background work whose results feed the next paint: parse-job
    /// scheduling and the commit, review, LSP, and completion result pumps.
    ///
    /// Run from the event loop after input is handled and before the redraw,
    /// keeping [`Self::render`] a pure paint. Tests that previously relied on
    /// `render` to drive this call it directly.
    ///
    /// This drives the diff jobs before the pumps, so a pump reads the maps
    /// that landed. It drives them again after the pumps, so a map that a pump
    /// staled starts its job in the same frame. A landed stage stales its
    /// buffer's map, and a write under `.git` stales every map.
    pub(crate) fn drive_background(&mut self) {
        crate::project_env::ensure_loaded(self);
        crate::project_env::install_pending(self);
        self.install_pending_workspace_restore();
        self.start_background_restore();
        crate::session_log::sync(self);
        crate::diff_warm::ensure_diff_warm(self);
        crate::diff_warm::install_finished(self);
        crate::buffer_lifecycle::install_pending_opens(self);
        action_handlers::sync_palette_picker(self);
        action_handlers::sync_file_finder_preview(self);
        action_handlers::search::sync_search_preview(self);
        self.drive_parse_jobs();
        self.drive_diff_jobs();

        self.drive_pumps();
        // Not gated on the pumps' progress. The `.git` refresh drain stales
        // every map and still reports none.
        self.drive_diff_jobs();
    }

    /// Advance every asynchronous request the editor has out, and report
    /// whether any of them moved.
    ///
    /// The run loop reaches this through [`Self::drive_background`], once per
    /// painted frame. Tests reach it directly and repeat it until it reports
    /// `false`, which is how a chain that takes several passes to resolve
    /// settles without the test counting the passes.
    ///
    /// This binds each result first and combines them after. A short-circuited
    /// OR leaves later pumps unpolled, and the fixpoint depends on every pump
    /// running on every pass.
    ///
    /// [`Self::drive_background`] keeps the rest to itself. Loading the project
    /// environment, warming diffs, and driving parse jobs answer to a frame
    /// rather than to a request. A fixpoint has no reason to repeat them.
    pub(crate) fn drive_pumps(&mut self) -> bool {
        let external = self.drain_external();

        let commits = action_handlers::pump_commits(self);
        let commit_picker = action_handlers::review_walk::pump_commit_picker(self);
        action_handlers::review_walk::sync_commit_picker(self);

        let code_search = action_handlers::code_search::pump_code_search(self);
        action_handlers::code_search::sync_code_search(self);

        let git_jobs = git_jobs::pump(self);

        let changed_file_jump = action_handlers::movement::pump_changed_file_jump(self);
        let conflict_file = action_handlers::conflict_view::pump_conflict_file(self);
        let diff_nav_jump = crate::code_index::nav::pump_diff_nav_jump(self);
        let lsp = crate::lsp::pump_all(self);
        action_handlers::workspace::sync_workspace_picker(self);
        action_handlers::picker::sync_location_picker(self);
        action_handlers::picker::sync_diagnostics_picker(self);
        action_handlers::picker::sync_jumplist_picker(self);

        let followed_change = crate::auto_reload::drain_followed_change(self);
        let live_reload = crate::auto_reload::drain_live_reload(self);
        let auto_reload = crate::auto_reload::pump_auto_reload_install(self);

        let format_on_save = action_handlers::file::pump_format_on_save(self);
        let pending_save = action_handlers::file::pump_pending_save(self);
        let completion = crate::completion::request::pump(self);
        let completion_resolve = action_handlers::completion::pump_completion_resolve(self);
        let completion_accept = crate::completion::accept::pump_completion_accept(self);

        external
            || commits
            || commit_picker
            || git_jobs
            || code_search
            || changed_file_jump
            || conflict_file
            || diff_nav_jump
            || lsp
            || followed_change
            || live_reload
            || auto_reload
            || format_on_save
            || pending_save
            || completion
            || completion_resolve
            || completion_accept
    }

    /// Resolve a `(line, column)` 0-based point to a byte
    /// offset in the focused editor's rope. Returns `None`
    /// when the focused pane is not an editor.
    pub(crate) fn offset_for_focused_point(
        &mut self,
        line: u32,
        column: u32,
        encoding: crate::host::OffsetEncoding,
    ) -> Option<usize> {
        let ws = self.active_workspace_mut();
        let editor_id = match ws.focus {
            FocusTarget::SplitPane => match ws.panes.pane(ws.panes.focus()).view {
                View::Editor(id) => id,
                _ => return None,
            },
            FocusTarget::Dock(_) => return None,
        };
        let editor = ws.editors.get_mut(editor_id)?;
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let rope = buf_snap.rope();
        let pos = lsp_types::Position::new(line, column);
        Some(crate::lsp::util::lsp_pos_to_byte_offset(
            rope, pos, encoding,
        ))
    }

    /// Collapse the focused editor's primary selection at
    /// `offset`. Used by non-jumplist navigation flows (e.g. the
    /// diagnostics picker) that need to move the cursor without
    /// touching jumplist state.
    pub(crate) fn collapse_focused_cursor_to(&mut self, offset: usize) {
        let ws = self.active_workspace_mut();
        let editor_id = match ws.focus {
            FocusTarget::SplitPane => match ws.panes.pane(ws.panes.focus()).view {
                View::Editor(id) => id,
                _ => return,
            },
            FocusTarget::Dock(_) => return,
        };
        let editor = match ws.editors.get_mut(editor_id) {
            Some(e) => e,
            None => return,
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        editor.selections.transform(buf_snap, |s| {
            crate::selection::land_block_cursor(
                s.id,
                offset,
                SelectionGoal::None,
                buf_snap.rope(),
                buf_snap,
            )
        });
    }

    pub(crate) fn jump_focused_to_match_offset(&mut self, offset: usize) {
        let ws = self.active_workspace_mut();
        let editor_id = match ws.focus {
            FocusTarget::SplitPane => match ws.panes.pane(ws.panes.focus()).view {
                View::Editor(id) => id,
                _ => return,
            },
            FocusTarget::Dock(_) => return,
        };
        let editor = match ws.editors.get_mut(editor_id) {
            Some(e) => e,
            None => return,
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        editor.selections.transform(buf_snap, |s| {
            crate::selection::land_block_cursor(
                s.id,
                offset,
                SelectionGoal::None,
                buf_snap.rope(),
                buf_snap,
            )
        });
    }
}

/// Modes whose `editor_insert` calls accumulate into the `.`
/// register's insert run. Helix tracks this for `insert` and
/// `reword_insert` only; `prompt` and `run` write to scratch
/// inputs that should not surface in the dot register.
fn is_insert_run_mode(mode: &str) -> bool {
    mode == "insert"
}

/// Whether `mode` is named as a pinned chord, which holds until Escape rather
/// than releasing after one key.
///
/// [`EditorState::pinned`] is the primary mark, set by the `PinMode` action.
/// This is the compat path for a user config that still ships a copied `*_pin`
/// mode block, whose bindings hold the mode by leaving the switch out rather
/// than by setting the flag.
///
/// A pinned chord outlives the editor that was focused when the user entered
/// it. A goto chord walks changes across files and a git chord stages hunks
/// across them, so both routinely hop to another file mid-chord, and the swap
/// that opens it must not drop the mode.
///
/// Keyed on the `_pin` naming convention rather than a list, the way
/// [`Stoat::in_select_mode`] keys on the `select_` prefix. A named chord gets
/// the behavior by being named for it.
pub(crate) fn is_pinned_mode(mode: &str) -> bool {
    mode.ends_with("_pin")
}

/// Visual columns a tab advances, for the column math in [`backspace_range`].
/// Matches the editor's default render tab size.
const TAB_WIDTH: usize = 4;

/// Where one insert-mode Enter writes its line ending.
enum LineBreak {
    /// Break the line at the cursor, continuing it below. `from` is where the
    /// line's trailing whitespace starts, and the continuation replaces it, so
    /// breaking after `foo   ` leaves `foo` rather than a line ending in
    /// spaces.
    Continue { from: usize },
    /// Write the line ending at `line_start` instead, which moves the whole
    /// line down and leaves an empty one above it.
    ///
    /// The case is a cursor with nothing but whitespace behind it on its line.
    /// Breaking there splits the indent and re-indents what follows, where what
    /// a reader means is to open a line above the one they are on.
    PushDown { line_start: usize },
}

/// Where one insert-mode Enter at `cursor` writes, given the `floor` a cursor
/// earlier in the same batch already claimed.
///
/// `floor` is the previous trimming cursor's own offset. Without it, two
/// cursors in one run of whitespace both claim the run and their edits overlap.
fn line_break_at(rope: &Rope, cursor: usize, floor: usize) -> LineBreak {
    let row = rope.offset_to_point(cursor).row;
    let line_start = rope.point_to_offset(stoat_text::Point::new(row, 0));

    let mut from = cursor;
    for ch in rope.reversed_chars_at(cursor) {
        if from <= line_start || !ch.is_whitespace() {
            break;
        }
        from -= ch.len_utf8();
    }

    match from > line_start {
        true => LineBreak::Continue {
            from: from.max(floor),
        },
        false => LineBreak::PushDown { line_start },
    }
}

/// The backward-delete span for one insert-mode backspace at `cursor`.
///
/// When the cursor follows only whitespace on its line, backspace works by
/// indent level. A preceding tab is removed on its own, and a run of spaces is
/// trimmed back to the previous `indent_width` column (a full unit when already
/// aligned). A cursor between the halves of a pair in `pairs` removes both.
/// Anywhere else it removes a single grapheme. Returns `(start, end)` with
/// `start == end` for a no-op at the buffer start.
///
/// The indent rule is tried before the pair rule, since an indent run holds no
/// bracket for the pair rule to answer anyway and a dedent is the larger claim.
fn backspace_range(
    rope: &Rope,
    cursor: usize,
    indent_width: usize,
    pairs: Option<AutoPairs>,
) -> (usize, usize) {
    if cursor == 0 {
        return (0, 0);
    }

    let prev = rope.reversed_chars_at(cursor).next();
    let one_back = (rope.prev_grapheme_boundary(cursor), cursor);

    let row = rope.offset_to_point(cursor).row;
    let line_start = rope.point_to_offset(stoat_text::Point::new(row, 0));

    // Visual width of the leading run before the cursor, if it is all whitespace.
    let mut width = 0usize;
    let mut pos = line_start;
    let mut indent_only = line_start < cursor;
    for ch in rope.chars_at(line_start) {
        if pos >= cursor {
            break;
        }
        match ch {
            ' ' => width += 1,
            '\t' => width += TAB_WIDTH,
            _ => {
                indent_only = false;
                break;
            },
        }
        pos += ch.len_utf8();
    }

    if !indent_only || prev == Some('\t') {
        return pairs
            .and_then(|pairs| auto_pairs::hook_delete(rope, cursor, pairs))
            .unwrap_or(one_back);
    }

    let mut drop = width % indent_width;
    if drop == 0 {
        drop = indent_width;
    }
    let mut start = cursor;
    for ch in rope.reversed_chars_at(cursor).take(drop) {
        if ch != ' ' {
            break;
        }
        start -= 1;
    }
    (start, cursor)
}

/// The deletion target for one insert-mode kill-to-line-start at `cursor`,
/// matching Helix's `kill_to_line_start`.
///
/// A cursor already at its line start (below the first line) targets the
/// previous line's content end, so the kill removes the separator and joins
/// the lines. A cursor after the line's first non-whitespace char targets that
/// char, preserving the indent. Anywhere else it targets the line start.
/// Returns `cursor` itself for a no-op at the buffer start.
fn kill_to_line_start_target(rope: &Rope, cursor: usize) -> usize {
    let row = rope.offset_to_point(cursor).row;
    let line_start = rope.point_to_offset(stoat_text::Point::new(row, 0));

    if cursor == line_start {
        if row == 0 {
            return cursor;
        }
        return rope.point_to_offset(stoat_text::Point::new(row - 1, rope.line_len(row - 1)));
    }

    let line_end = rope.point_to_offset(stoat_text::Point::new(row, rope.line_len(row)));
    let mut pos = line_start;
    for ch in rope.chars_at(line_start) {
        if pos >= line_end || !ch.is_whitespace() {
            break;
        }
        pos += ch.len_utf8();
    }

    if pos < line_end && pos < cursor {
        pos
    } else {
        line_start
    }
}

/// Whether `key` types a character, which is a `Char` with no modifier or with
/// Shift alone.
fn is_printable_key(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char(_))
        && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
}

/// The byte sequence a VT terminal sends for `key`, or `None` when the key has
/// no encoding here.
///
/// This encodes the printable characters (UTF-8), `Ctrl`+letter control bytes,
/// and named keys (Enter, Tab, Backspace, Esc, the four arrows) an interactive
/// agent pane needs. Backspace maps to `DEL` (`0x7f`), the xterm default.
/// Modifiers other than `Ctrl` are ignored, so e.g. `Alt`+key encodes as the
/// bare key.
fn encode_key_to_pty(key: &KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                control_byte(c).map(|b| vec![b])
            } else {
                let mut buf = [0u8; 4];
                Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
            }
        },
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        _ => None,
    }
}

/// The byte sequence a VT terminal sends for a paste of `text`.
///
/// `bracketed` is the child's own DECSET 2004 state, read from
/// [`TermScreen::bracketed_paste`]. Under it the payload is wrapped in the
/// guard markers, with any embedded end guard stripped so pasted bytes cannot
/// close the bracket early and have the rest run as typed input. Without it
/// newlines become carriage returns, which is what the Enter key sends, so a
/// pasted multi-line command submits each line the way typing it would.
fn encode_paste_to_pty(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let guarded = text.replace("\x1b[201~", "");
        format!("\x1b[200~{guarded}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// The ASCII control byte for `Ctrl`+`c`, mapping `Ctrl-A`..`Ctrl-Z` to
/// `0x01`..`0x1a`. `None` when `c` is not an ASCII letter.
fn control_byte(c: char) -> Option<u8> {
    c.is_ascii_alphabetic()
        .then(|| (c.to_ascii_lowercase() as u8) - b'a' + 1)
}

/// Crossterm modifiers from a window-IPC modifier bitmask.
///
/// The socket packs shift at `0x1`, control at `0x2`, alt at `0x4`, and super
/// at `0x8`, which is this project's own layout rather than the terminal's, so
/// the mapping back belongs on this side of the wire.
fn ipc_modifiers(mods: u8) -> KeyModifiers {
    let mut out = KeyModifiers::empty();
    for (bit, modifier) in [
        (0x1, KeyModifiers::SHIFT),
        (0x2, KeyModifiers::CONTROL),
        (0x4, KeyModifiers::ALT),
        (0x8, KeyModifiers::SUPER),
    ] {
        if mods & bit != 0 {
            out |= modifier;
        }
    }
    out
}

/// The detached pane bound to aux window `window`, or `None` when none is.
fn pane_for_window(panes: &PaneTree, window: u32) -> Option<PaneId> {
    panes
        .windowed_panes()
        .into_iter()
        .find(|(_, w)| *w == window)
        .map(|(id, _)| id)
}

/// Dispatch every notification `host` has queued, up to a per-tick cap.
///
/// `Progress` updates the [`crate::lsp::progress::LspProgressMap`]. Other
/// variants log via tracing for now and become future per-feature consumer
/// hooks. The cap keeps a pathological notification burst from starving the
/// event loop, and the remainder drains on the next update.
///
/// Takes the three fields it writes rather than the whole [`Stoat`], so
/// [`Stoat::drain_lsp_notifications`] can walk the registry borrowed instead of
/// collecting it per event.
/// Read stoatty's window-event socket, forwarding each event over `tx`.
///
/// Sends [`WindowIpc::Connected`] once the stream opens, then a
/// [`WindowIpc::Event`] per decoded line, and [`WindowIpc::Disconnected`] when
/// the stream ends or errors (stoatty exited, so detach reports unavailable
/// again). Unparseable lines are skipped so the format can grow.
async fn connect_window_ipc(path: PathBuf, tx: UnboundedSender<WindowIpc>) {
    let stream = match tokio::net::UnixStream::connect(&path).await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(?path, %error, "window-event socket connect failed");
            let _ = tx.send(WindowIpc::Disconnected);
            return;
        },
    };

    let _ = tx.send(WindowIpc::Connected);

    let mut lines = tokio::io::BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(event) = stoatty_protocol::window_ipc::parse_line(&line)
            && tx.send(WindowIpc::Event(event)).is_err()
        {
            return;
        }
    }

    let _ = tx.send(WindowIpc::Disconnected);
}

/// Paint one frame into `buf`, for a test reading the cells a frame produced.
///
/// [`Stoat::paint_into`] stays private. The tests that need it sit in other
/// modules, and this keeps painting outside the run loop a test-only ability.
#[cfg(test)]
pub(crate) fn paint_frame(stoat: &mut Stoat, buf: &mut Buffer) {
    stoat.paint_into(buf);
}

/// Deliver one window-IPC event, for a test driving an aux window.
///
/// Takes the event rather than the private [`WindowIpc`] wrapper, so neither the
/// enum nor [`Stoat::handle_window_ipc`] widens for a test elsewhere.
#[cfg(test)]
pub(crate) fn deliver_window_event(stoat: &mut Stoat, event: WindowIpcEvent) -> UpdateEffect {
    stoat.handle_window_ipc(WindowIpc::Event(event))
}

/// Step the scroll animation by `dt` seconds, reporting whether it still runs.
#[cfg(test)]
pub(crate) fn tick_animation(stoat: &mut Stoat, dt: f32) -> bool {
    stoat.tick_scroll_anim(dt)
}

/// Whether any scroll animation is still running.
#[cfg(test)]
pub(crate) fn animating(stoat: &Stoat) -> bool {
    stoat.is_animating()
}

#[cfg(test)]
mod tests;
