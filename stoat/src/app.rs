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
    file_finder::{FileFinder, FinderPathCache},
    help::Help,
    host::{
        EnvHost, FsHost, FsWatchHost, GitHost, LocalEnv, LocalFs, LocalGit, LspHost, NoopFsWatcher,
    },
    keymap::{Keymap, ResolvedAction, StateValue},
    keymap_state::{
        self, active_modal, debug_assert_modal_exclusivity, modal_predicate, normalize_shift_event,
        resolve_action, ActiveModal, StoatKeymapState,
    },
    lsp::pending::{Pending, StampedPending},
    minimap::emit::{self},
    mouse::{self, mouse_event_kind},
    pane::{DockId, DockVisibility, FocusTarget, NodeId, PaneId, PaneTree, Placement, View},
    quit_all_confirm::QuitAllConfirm,
    rebase::RebasePause,
    register,
    render::{
        pane_cache::PaneCacheEntry,
        undercurl::{self, UndercurlBatch},
    },
    run::{CommandMark, PtyNotification, RunId},
    selection::{merge_overlapping_spans, EndCell},
    symbol_finder::SymbolFinder,
    term_session::{TermId, TermReturnFocus},
    theme_pool::{ThemePool, VscodeSource},
    ui::RenderFrame,
    workspace::{Workspace, WorkspaceId, WorkspaceUid},
    workspace_picker::WorkspacePicker,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};
use futures::FutureExt;
use ratatui::{buffer::Buffer, layout::Rect};
use slotmap::SlotMap;
use std::{
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_action::{Conflict, Diff, OpenFile};
use stoat_config::{MinimapMode, Settings, TabBarMode, WrapMode};
use stoat_language::{self as language, LanguageRegistry};
use stoat_scheduler::Executor;
use stoat_text::{Anchor, Bias, IndentStyle, Rope, Selection, SelectionGoal};
use stoatty_protocol::window_ipc::{MouseButton as IpcMouseButton, MouseKind, WindowIpcEvent};
use stoatty_widgets::{pool::SmoothScrollState, ApcScene};
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
const THEME_ONE_DARK: &str = include_str!("../../themes/one-dark.json");
const THEME_GRUVBOX_DARK: &str = include_str!("../../themes/gruvbox-dark.json");
const THEME_GRUVBOX_LIGHT: &str = include_str!("../../themes/gruvbox-light.json");
const THEME_ONE_LIGHT: &str = include_str!("../../themes/one-light.json");

/// Frame interval for scroll-animation ticks, about 60 fps to match a typical
/// display rather than shipping targets that can never be presented.
/// [`Stoat::run`] arms a timer at this cadence while a scroll glide is active,
/// advancing the inertial scroll one step per fire.
const SCROLL_FRAME: std::time::Duration = std::time::Duration::from_millis(16);

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
    settings: Settings,
    theme: crate::theme::Theme,
    theme_pool: ThemePool,
    syntax_styles: SyntaxStyles,
    minimap_class_table: crate::minimap::ClassTable,
}

/// Resolve `config` into the artifacts the editor runs on.
///
/// `embedded` is the built-in config when `config` came from the user, so its
/// theme blocks can sit underneath as inheritable parents. It is [`None`] when
/// `config` *is* the embedded default, which is also how the theme precedence
/// below tells a user-chosen theme from the shipped one.
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
    config: Option<stoat_config::Config>,
    embedded: Option<stoat_config::Config>,
    imported: &[Arc<VscodeSource>],
    cli_settings: Settings,
    env_theme: Option<String>,
) -> ConfigArtifacts {
    // The whole pool is retained so SetTheme can re-resolve any theme at
    // runtime, and so an `inherits PARENT` sees every candidate parent.
    let theme_pool = {
        let mut pool = ThemePool::default();
        if let Some(base) = embedded.as_ref() {
            for block in &base.themes {
                pool.push_parsed(block.clone());
            }
        }
        for source in imported {
            pool.push_vscode(source.clone());
        }
        if let Some(c) = config.as_ref() {
            for block in &c.themes {
                pool.push_parsed(block.clone());
            }
        }
        pool
    };

    let settings = {
        let cli_theme_set = cli_settings.theme.is_some();
        let from_config = config
            .as_ref()
            .map(Settings::from_config)
            .unwrap_or_default();
        // A clean user config replaces the embedded one wholesale, so the
        // config's own theme is an explicit choice only when it came from the
        // user source. The embedded default sets `theme = default_dark`
        // unconditionally, which an inherited theme is meant to beat.
        let user_theme_set = embedded.is_some() && from_config.theme.is_some();

        let mut settings = from_config.merge(cli_settings);
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

    let keymap = {
        let (keymap, warnings) = match config {
            Some(c) => Keymap::compile_with_warnings(&c),
            None => Keymap::compile_with_warnings(&stoat_config::Config {
                blocks: vec![],
                themes: vec![],
            }),
        };
        for warning in warnings {
            tracing::warn!(target: "stoat::keymap", "{warning}");
        }
        keymap
    };

    let syntax_styles = SyntaxStyles::from_theme(&theme);
    let minimap_class_table = crate::minimap::ClassTable::from_theme(&theme);

    ConfigArtifacts {
        keymap,
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
/// [`crate::action_handlers::file::pump_auto_reload`] can re-read files whose
/// on-disk mtime advanced.
pub(crate) const AUTO_RELOAD_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a transient status message stays visible before it self-retires.
/// [`Stoat::set_status`] stamps a deadline this far ahead and arms a timer that
/// wakes the run loop so [`crate::render::frame`] can clear the expired message.
const STATUS_MESSAGE_TTL: std::time::Duration = std::time::Duration::from_secs(4);

/// Maximum index updates [`Stoat::drain_index_updates`] processes in one call.
/// Bounds the graph work per event-loop turn so a large reindex burst cannot
/// stall input. On hitting the cap the drain reschedules itself to finish the
/// remainder on the next turn.
const INDEX_DRAIN_CAP: usize = 512;

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
/// Carries the resolved `server` command name and `language` so the registry
/// keys the ready host and routes its language on install. `result` is the
/// ready host, or the failure string to surface in the message row when the
/// spawn or handshake failed.
pub(crate) struct PendingSpawn {
    pub(crate) server: String,
    pub(crate) language: String,
    pub(crate) result: Result<Arc<dyn LspHost>, String>,
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
/// target. Modals sized entirely by their content already (the location,
/// diagnostics, jumplist, and workspace pickers) have nothing to zoom and are
/// absent.
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

/// The open modal's list/preview separator: where it sits, and what a drag along
/// it redistributes.
///
/// One descriptor for both splits the modal family uses. Only the axis and the
/// floors differ between them -- the pane on one side takes a share of the body,
/// the separator takes a cell of its own, and the other pane takes the rest --
/// so resolving a modal into this shape lets one hit-test and one clamp serve
/// every kind.
pub(crate) struct ModalSeparator {
    /// Whose [`Stoat::modal_split`] entry a drag writes.
    pub(crate) kind: ModalKind,
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

impl ModalSeparator {
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
    /// Share of its body each modal's list pane takes, as a percentage, for the
    /// kinds whose list/preview separator the user has dragged. An absent kind
    /// sits at [`crate::render::picker::DEFAULT_LIST_PERCENT`].
    ///
    /// Stored per kind and session-scoped for [`Self::modal_zoom`]'s reasons. A
    /// share the user chose outlives the modal it was chosen in, so reopening
    /// that modal restores the split. It is never persisted, because the choice
    /// answers what is on screen right now.
    pub(crate) modal_split: std::collections::BTreeMap<ModalKind, u16>,
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
    /// Stored macros keyed by [`crate::register::Register`]. Filled
    /// when `RecordMacro` toggles off; consumed by [`ReplayMacro`].
    pub(crate) macros: std::collections::HashMap<register::Register, Vec<KeyEvent>>,
    /// Set after [`stoat_action::ReplayMacro`] arms the chord. The
    /// next char keypress in normal/select mode names a register
    /// and the stored macro is replayed; non-char keypresses also
    /// clear the flag.
    pub(crate) pending_macro_replay: bool,
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
    /// Whether the window-event socket is currently connected. Gates pane
    /// detach, which needs stoatty to host the aux window and report its events.
    pub(crate) window_ipc_connected: bool,
    /// Whether the zoom combo is currently claimed from the hosting terminal.
    ///
    /// The claim needs both a stoatty and a window socket reaching this process,
    /// which arrive independently and in either order, so this keeps whichever
    /// lands second from claiming twice and the release from going out unclaimed.
    zoom_claimed: bool,
    /// Aux windows stoatty has been told to open, keyed by window id with the
    /// cell size last sent. [`Self::emit_windows`] diffs it against the detached
    /// panes each frame to emit the WindowOpen and WindowClose commands.
    pub(crate) aux_windows: std::collections::BTreeMap<u32, (u16, u16)>,
    /// The pool cursor last shipped for a focused detached pane, as `(pool, row,
    /// col)`, so [`Self::emit_smooth_scroll`] re-emits only when it moves and an
    /// idle frame ships nothing. `None` when no detached pane holds focus.
    pub(crate) aux_cursor: Option<(u32, u64, u16)>,
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
    /// In-flight working-tree review scan. The git2 diff runs on a
    /// blocking thread; [`pump_review_scan`](action_handlers::pump_review_scan)
    /// polls the ready [`ReviewSession`](crate::review_session::ReviewSession)
    /// off this task and installs it on the main loop, so opening a review
    /// never stalls input on the scan.
    pub(crate) pending_review_scan: Option<action_handlers::PendingReviewScan>,
    /// A cross-file changed-file hop scanning off the UI thread, applied by
    /// [`action_handlers::movement::pump_changed_file_jump`] when it lands.
    pub(crate) pending_changed_file_jump: Option<action_handlers::movement::PendingChangedFileJump>,
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
    /// Review-screen hints footer cached against the review session version, so
    /// the per-chunk `progress()` walk reruns only when the session changes.
    pub(crate) review_footer_cache: Option<(u64, Option<crate::render::hints::HintsFooter>)>,
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
    pub(crate) pending_goto_word: Option<std::collections::BTreeMap<String, usize>>,
    /// Characters typed so far to disambiguate the active goto-word
    /// label. Always paired with [`Self::pending_goto_word`]: when
    /// that field is `None` this is empty.
    pub(crate) pending_goto_word_input: String,
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
    /// Set after `SelectTextobjectAround` or `SelectTextobjectInner`
    /// arms the chord. The next printable char keypress in normal /
    /// select mode names the textobject type (`f` function, `t`
    /// class, `p` paragraph, `a` parameter, `c` comment) and is
    /// resolved via
    /// [`action_handlers::textobject::execute_select_textobject`].
    /// Non-char keypresses also clear the state.
    pub(crate) pending_textobject_select: Option<action_handlers::textobject::TextobjectMode>,
    /// Active search input modal. Some while the user is typing a
    /// `/` (forward) or `?` (reverse) search query; cleared by
    /// [`action_handlers::search::search_submit`] or
    /// [`action_handlers::search::search_cancel`].
    pub(crate) search_input: Option<action_handlers::search::SearchInputState>,
    /// Persisted query + direction from the most recent submitted
    /// search. Drives `SearchNext` / `SearchPrev` repeats.
    pub(crate) last_search: Option<action_handlers::search::LastSearch>,
    /// Most recent text inserted during a complete insert-mode
    /// session, accumulated across every [`Self::editor_insert`]
    /// call between entering and leaving insert mode. Backs the
    /// `.` special register so paste/insert-register can surface
    /// the last typed run.
    pub(crate) last_insert_text: Option<String>,
    /// Buffer accumulating text typed during the current
    /// insert-mode session. `Some` while `mode == "insert"` (or
    /// equivalent), `None` outside. Committed to
    /// [`Self::last_insert_text`] on insert-mode exit.
    pub(crate) current_insert_run: Option<String>,
    /// Set by append-style insert entries (`a`/`A`) so leaving insert moves
    /// each block cursor back one grapheme, landing on the last typed (or
    /// appended-over) char rather than one cell past it. It is cleared on the
    /// insert-to-normal transition. Other insert entries never set it.
    pub(crate) restore_cursor: bool,
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
    /// Register selected via `SelectRegister` for the next yank
    /// or paste operation. `None` means the unnamed register is
    /// the implicit target. Cleared by
    /// [`Self::consume_selected_register`] which yank/paste call
    /// before reading the chosen register.
    pub(crate) selected_register: Option<register::Register>,
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
    /// [`crate::action_handlers::file::pump_auto_reload`] clears this field to
    /// disarm the poll once no buffer wants following.
    pub(crate) auto_reload_poll: Option<stoat_scheduler::Task<()>>,
    /// Poll ticks from [`Self::auto_reload_poll`]'s timer, one per interval.
    ///
    /// The run loop receives them on its own select arm so a tick wakes it
    /// without implying a frame. Only [`crate::action_handlers::file::pump_auto_reload`]
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
    /// Most recent `(FindKind, char)` consumed by `execute_find`.
    /// `RepeatLastMotion` (Alt-.) replays this pair without
    /// reading another keypress.
    pub(crate) last_find: Option<(action_handlers::movement::FindKind, char)>,
    /// Filesystem the UI layer reads through. Swapped to
    /// [`crate::host::FakeFs`] in tests; all IO outside the host module
    /// itself must route through this field.
    pub(crate) fs_host: Arc<dyn FsHost>,
    /// Filesystem-change subscription host. Defaults to
    /// [`NoopFsWatcher`]; the bin layer installs
    /// [`crate::host::LocalFsWatcher`] and tests install
    /// [`crate::host::FakeFsWatcher`]. Drained per-tick by
    /// [`debounce::drain_fs_watch_events`] so external edits can flow
    /// into the active review session.
    pub(crate) fs_watch_host: Arc<dyn FsWatchHost>,
    /// Pending [`ReviewExternalEdit`] debounce timer per path. Each
    /// [`debounce::arm_review_external_edit_debounce`] call replaces the
    /// entry, dropping the prior [`stoat_scheduler::Task`] which
    /// cancels its spawned future before the timer fires; only the
    /// most recent burst-event for a path proceeds to dispatch.
    pub(crate) review_pending_external_edits:
        std::collections::HashMap<PathBuf, stoat_scheduler::Task<()>>,
    /// Channel the per-path debounce tasks push onto once their
    /// 50ms timer fires. Decouples the spawned async work from the
    /// main-thread action dispatch in
    /// [`debounce::drain_pending_external_edits`].
    pub(crate) review_external_edit_tx: Sender<PathBuf>,
    pub(crate) review_external_edit_rx: Receiver<PathBuf>,
    /// Single-slot debounce for a whole-session git refresh. A commit writes
    /// many `.git` files at once, and unlike the per-path
    /// [`Self::review_pending_external_edits`] this collapses that burst to one
    /// [`ReviewRefresh`]. Re-arming replaces the task, cancelling the prior
    /// timer.
    pub(crate) review_pending_git_refresh: Option<stoat_scheduler::Task<()>>,
    /// Channel the git-refresh debounce task pushes onto once its timer fires,
    /// drained by [`debounce::drain_pending_git_refresh`].
    pub(crate) review_git_refresh_tx: Sender<()>,
    pub(crate) review_git_refresh_rx: Receiver<()>,
    /// Per-path debounce tasks for the incremental diff-warm of a file edited
    /// while review is closed. Mirrors [`Self::review_pending_external_edits`];
    /// re-arming a path drops the prior [`stoat_scheduler::Task`], cancelling
    /// its timer so only the latest burst event warms.
    pub(crate) pending_diff_warm_file:
        std::collections::HashMap<PathBuf, stoat_scheduler::Task<()>>,
    /// Channel the diff-warm debounce tasks push a path onto once their timer
    /// fires, drained by [`debounce::drain_pending_diff_warm_files`].
    pub(crate) diff_warm_file_tx: Sender<PathBuf>,
    pub(crate) diff_warm_file_rx: Receiver<PathBuf>,
    /// In-flight single-file diff warms. Held so their tasks are not dropped
    /// (which would cancel them) and so the status bar's diff segment stays up
    /// until every one finishes. [`crate::diff_warm::install_finished`] drops the
    /// completed ones.
    pub(crate) diff_warm_files: Vec<crate::diff_warm::PendingFileWarm>,
    /// Large files reading on the blocking pool, awaiting install by
    /// [`crate::action_handlers::file::install_pending_opens`] in
    /// [`Self::drive_background`]. Holding the task here keeps the read alive;
    /// dropping it (on quit) cancels it.
    pub(crate) pending_file_opens: Vec<action_handlers::file::PendingFileOpen>,
    /// Files changed outside the editor, waiting on the shared debounce window
    /// to be reindexed into the code graph.
    ///
    /// A set covered by one timer rather than a task per path, unlike
    /// [`Self::review_pending_external_edits`]. A checkout or a formatter run
    /// names thousands of files at once, and the burst is what this has to
    /// survive.
    pub(crate) index_pending_external_edits: std::collections::HashSet<PathBuf>,
    /// The one debounce timer covering whatever
    /// [`Self::index_pending_external_edits`] holds.
    ///
    /// Armed when the set goes from empty to occupied, so the window closes a
    /// fixed [`debounce::REVIEW_EXTERNAL_EDIT_DEBOUNCE`] after a burst starts. Under a
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
    pub(crate) base_highlights_cache: crate::workspace::BaseHighlightCache,
    /// Memoized tree-sitter parses of git-base texts for the structural diff,
    /// so the per-file warm that re-runs on every debounced edit parses that
    /// file's unchanged HEAD text once rather than per edit.
    pub(crate) diff_tree_cache: stoat_language::structural_diff::TreeCache,
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
        Option<stoat_scheduler::Task<action_handlers::lsp::HoverOutcome>>,

    /// Hover popup content waiting to be painted. Set by
    /// [`action_handlers::pump_lsp_hover`] when a hover response lands.
    ///
    /// In normal or select mode the next key press closes it (the auto-close
    /// intercept in [`Self::handle_key`]): Escape and Ctrl-c are consumed by the
    /// close, every other key closes it and then dispatches. Any non-Hover action
    /// also clears it, so the popup vanishes on cursor motion.
    pub(crate) pending_hover: Option<crate::render::hover::HoverPopup>,

    /// In-flight `textDocument/signatureHelp` request, armed by
    /// [`crate::lsp::signature_help::signature_help_trigger`] on a trigger
    /// character and polled by
    /// [`crate::lsp::signature_help::pump_lsp_signature_help`].
    pub(crate) pending_signature_help_request:
        Option<stoat_scheduler::Task<Option<crate::lsp::signature_help::SignatureHelpPopup>>>,

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
    /// Set by `:wq` ([`action_handlers::file::write_quit`]) when the save it
    /// triggered was deferred to an in-flight format-on-save write. Consumed by
    /// [`action_handlers::file::pump_format_on_save`] when that write lands: it
    /// sets [`Self::quit_requested`] only if the write succeeded, so a failed
    /// deferred write aborts the quit and leaves the buffer for the user.
    pub(crate) quit_after_save: bool,
    /// Set once a `:wq`-driven write has landed and the app should exit. The run
    /// loop takes it right after [`Self::drive_background`] and quits, so a quit
    /// deferred behind a format-on-save write happens on the frame it completes.
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
    /// Keyed by pane so a split's panes cache independently. An entry for a
    /// pane that has gone away is simply never looked up again, which costs one
    /// pane's cells until the map is next written.
    pub(crate) pane_cache: std::collections::HashMap<PaneId, PaneCacheEntry>,
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
    ) -> Vec<(&crate::keymap::CompiledKey, &[ResolvedAction])> {
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

    /// Construct a [`Stoat`], preferring `user_config` over the embedded default when it parses
    /// clean.
    ///
    /// `user_config` is the raw text of the user's `config.stcfg` (located via
    /// [`user_config_path`](crate::user_config_path)), or [`None`] to use only the
    /// built-in default. A user source that parses without errors replaces the
    /// embedded config wholesale. One that fails to parse is discarded in favour
    /// of the embedded default, logged, and surfaced as a transient status
    /// message. CLI settings layer over the resolved config either way.
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
        let (config, theme_base, config_error) = match user_config {
            Some(source) => {
                let (parsed, errors) = stoat_config::parse(&source);
                if errors.is_empty() {
                    (parsed, Self::parse_default_keymap(), None)
                } else {
                    tracing::error!(
                        "user config parse failed; using built-in defaults: {}",
                        stoat_config::format_errors(&source, &errors)
                    );
                    (
                        Self::parse_default_keymap(),
                        None,
                        Some("user config parse failed; using built-in defaults".to_string()),
                    )
                }
            },
            None => (Self::parse_default_keymap(), None, None),
        };

        // Retaining the sources lets a mid-session reload rebuild the identical
        // pool without re-reading the theme directory, and lets a theme already
        // converted stay converted across the reload.
        let imported_themes: Vec<Arc<VscodeSource>> = {
            let builtins = [
                ("one-dark", THEME_ONE_DARK),
                ("gruvbox-dark", THEME_GRUVBOX_DARK),
                ("gruvbox-light", THEME_GRUVBOX_LIGHT),
                ("one-light", THEME_ONE_LIGHT),
            ];
            builtins
                .into_iter()
                .map(|(stem, source)| (stem.to_string(), source.to_string()))
                .chain(user_themes)
                .map(|(stem, source)| Arc::new(VscodeSource::new(stem, source)))
                .collect()
        };

        let ConfigArtifacts {
            keymap,
            settings,
            theme,
            theme_pool,
            syntax_styles,
            minimap_class_table,
        } = build_config_artifacts(
            config,
            theme_base,
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

        let mut workspaces = SlotMap::with_key();
        let workspace = Workspace::new(initial_git_root.clone(), &executor, redraw_notify.clone());
        let active_workspace = workspaces.insert(workspace);
        workspaces[active_workspace].id = active_workspace;

        let (pty_tx, pty_rx) = tokio::sync::mpsc::channel(256);
        let (agent_event_tx, agent_event_rx) = tokio::sync::mpsc::channel(256);
        let (agent_control_tx, agent_control_rx) = tokio::sync::mpsc::channel(256);
        let (index_update_tx, index_update_rx) = tokio::sync::mpsc::unbounded_channel();
        let (window_ipc_tx, window_ipc_rx) = tokio::sync::mpsc::unbounded_channel();
        let (review_external_edit_tx, review_external_edit_rx) = tokio::sync::mpsc::channel(256);
        let (review_git_refresh_tx, review_git_refresh_rx) = tokio::sync::mpsc::channel(256);
        let (code_search_query_tx, code_search_query_rx) = tokio::sync::mpsc::channel(256);
        let (diff_warm_file_tx, diff_warm_file_rx) = tokio::sync::mpsc::channel(256);
        let (index_external_edit_tx, index_external_edit_rx) = tokio::sync::mpsc::channel(256);
        let (auto_reload_tx, auto_reload_rx) = tokio::sync::mpsc::channel(1);
        // Dropped at once, leaving the channel closed until `set_stoatty_rx`
        // installs the UI thread's end. Closed is the truthful state for a
        // process that has no UI thread to hear from.
        let (_, stoatty_rx) = tokio::sync::mpsc::unbounded_channel::<Option<u32>>();

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
            modal_split: std::collections::BTreeMap::new(),
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
            macros: std::collections::HashMap::new(),
            pending_macro_replay: false,
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
            window_ipc_connected: false,
            zoom_claimed: false,
            aux_windows: std::collections::BTreeMap::new(),
            aux_cursor: None,
            _index_build_task: None,
            redraw_notify,
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            #[cfg(feature = "perf")]
            perf: crate::perf::PerfStats::default(),
            pending_review_scan: None,
            pending_changed_file_jump: None,
            pending_code_search: None,
            code_search_debounce: None,
            code_search_query_tx,
            code_search_query_rx,
            pending_diff_warm: None,
            modal_run: None,
            syntax_highlight: true,
            minimap_override: None,
            tab_bar_override: None,
            single_minimap_rect: None,
            lsp_badge_rect: None,
            lsp_status_pinned: false,
            lsp_badge_hovered: false,
            key_hints_visible: false,
            hints_cache: None,
            review_footer_cache: None,
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
            pending_count: None,
            pending_find: None,
            pending_mark: None,
            marks: std::collections::HashMap::new(),
            global_marks: std::collections::HashMap::new(),
            pending_goto_word: None,
            pending_goto_word_input: String::new(),
            pending_replace: false,
            pending_surround_add: false,
            pending_surround_replace: action_handlers::surround::SurroundReplaceStage::Idle,
            pending_surround_delete: false,
            pending_textobject_select: None,
            search_input: None,
            last_search: None,
            last_insert_text: None,
            current_insert_run: None,
            restore_cursor: false,
            auto_indent_cursors: Vec::new(),
            registers: register::RegisterStore::new(),
            pending_register_select: false,
            selected_register: None,
            pending_insert_register: false,
            replaying_registers: Vec::new(),
            editor_drag: None,
            terminal_drag: None,
            hover_cell: None,
            hover_diag: None,
            divider_drag: None,
            modal_separator_drag: None,
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
            last_find: None,
            fs_host: Arc::new(LocalFs),
            fs_watch_host: Arc::new(NoopFsWatcher::new()),
            review_pending_external_edits: std::collections::HashMap::new(),
            review_external_edit_tx,
            review_external_edit_rx,
            review_pending_git_refresh: None,
            review_git_refresh_tx,
            review_git_refresh_rx,
            pending_diff_warm_file: std::collections::HashMap::new(),
            diff_warm_file_tx,
            diff_warm_file_rx,
            diff_warm_files: Vec::new(),
            pending_file_opens: Vec::new(),
            index_pending_external_edits: std::collections::HashSet::new(),
            index_external_edit_timer: None,
            ignored_dir_cache: std::collections::HashMap::new(),
            index_external_edit_tx,
            index_external_edit_rx,
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
            pending_env: Arc::new(std::sync::Mutex::new(None)),
            pending_workspace_restore: Arc::new(std::sync::Mutex::new(None)),
            pending_workspace_saves: std::collections::HashMap::new(),
            clipboard_host: Arc::new(crate::host::NoopClipboard),
            diff_cache: Arc::new(std::sync::Mutex::new(crate::diff_cache::DiffCache::new(
                256,
            ))),
            base_highlights_cache: Arc::new(std::sync::Mutex::new(
                crate::workspace::BaseHighlightMemo::default(),
            )),
            diff_tree_cache: stoat_language::structural_diff::TreeCache::default(),
            lsp_progress: crate::lsp::progress::LspProgressMap::new(),
            lsp_server_list: crate::render::LspServerList::default(),
            lsp_message: None,
            pending_lsp_jump: None,
            pending_hover_request: None,
            pending_hover: None,
            pending_signature_help_request: None,
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
            quit_after_save: false,
            quit_requested: false,
            pending_completion: None,
            completion_generation: 0,
            pending_completion_request: None,
            pending_completion_resolve: None,
            pending_completion_accept: StampedPending::default(),
            last_completion_signature: None,
            completion_context: None,
            active_snippet: None,
            version_info: "unknown",
            next_aux_window: 1,
            apc_tx: None,
            stoatty: false,
            stoatty_protocol: 0,
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

        stoat
    }

    /// Parse the embedded default keymap ([`DEFAULT_KEYMAP`]), logging any parse errors.
    fn parse_default_keymap() -> Option<stoat_config::Config> {
        let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
        if !errors.is_empty() {
            tracing::error!(
                "default keymap parse errors: {}",
                stoat_config::format_errors(DEFAULT_KEYMAP, &errors)
            );
        }
        config
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
            settings,
            theme,
            theme_pool,
            syntax_styles,
            minimap_class_table,
        } = build_config_artifacts(
            config,
            Self::parse_default_keymap(),
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
        self.minimap_content.clear();
        // The theme, the syntax styles, and the settings the renderer reads
        // directly all just moved, and a parse failure returned above without
        // touching any of them.
        self.paint_generation += 1;
        self.set_status("config reloaded");
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

    /// Shared handle on the in-memory diff cache. The cache-population
    /// hook in [`crate::review_session::ReviewSession::add_files`]
    /// inserts post-extraction hunks here so subsequent
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

    /// Listen for the UI thread's report of the startup ident handshake.
    ///
    /// The bin layer creates the channel before spawning that thread and hands
    /// this end over once the app exists. Left uncalled, [`Self::stoatty`] stays
    /// false for the process's life, which is what a headless or embedded run
    /// wants.
    pub fn set_stoatty_rx(&mut self, stoatty_rx: UnboundedReceiver<Option<u32>>) {
        self.stoatty_rx = stoatty_rx;
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
        let Some(protocol) = protocol.filter(|_| !self.stoatty) else {
            return UpdateEffect::None;
        };

        self.stoatty = true;
        self.stoatty_protocol = protocol;
        apc_emit::emit_theme_default_colors(self);
        self.sync_zoom_claim();

        UpdateEffect::Redraw
    }

    /// Claim the zoom combo once both halves of the round trip exist, and
    /// release it when either goes away.
    ///
    /// The presses come back over the window socket, so claiming without one
    /// swallows them. Stoatty stops stepping its font and queues each press for
    /// a client that never connects. That is the ordinary state over ssh, where
    /// the APC handshake succeeds because APC rides the link while
    /// `STOATTY_WINDOW_SOCKET` does not.
    ///
    /// Called from whichever of the two arrives second, and from the
    /// disconnect, which is what hands the combo back to a stoatty outliving
    /// this process.
    fn sync_zoom_claim(&mut self) {
        let claim = self.stoatty && self.window_ipc_connected;
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
                self.sync_zoom_claim();
                return UpdateEffect::None;
            },
            WindowIpc::Disconnected => {
                self.window_ipc_connected = false;
                self.sync_zoom_claim();
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
            ..
        } = event
        {
            return self.handle_aux_mouse(window, kind, col, row);
        }

        // Routing a zoom step reads the open modal and the zoom ledger, neither
        // of which the pane-tree borrow below leaves reachable.
        if let WindowIpcEvent::Zoom { delta, .. } = event {
            return self.handle_zoom_step(delta);
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
        }
        UpdateEffect::Redraw
    }

    /// Apply `delta` zoom steps to whatever the user is looking at.
    ///
    /// An open modal owns the combo. One with a zoom of its own grows or
    /// shrinks, and one sized entirely by its content swallows the step, since
    /// resizing a pane hidden behind it would be a change the user cannot see.
    /// With no modal open the step resizes the focused pane against its split.
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

        self.active_workspace_mut().panes.resize_focused_pane(delta);
        UpdateEffect::Redraw
    }

    /// Walk the jumplist for a press of the back or forward side button,
    /// returning [`None`] when `kind` names any other button.
    ///
    /// These buttons reach stoat only over the window socket, since no in-band
    /// terminal encoding carries them, so this runs before the window's pane
    /// binding is resolved. The primary window has no binding of its own, and
    /// its presses act on whatever pane holds focus. An aux window's presses
    /// focus its pane first, as its other gestures do.
    ///
    /// Handling every gesture of both buttons here is what keeps them away from
    /// [`mouse_event_kind`], which has no crossterm button to map them onto.
    fn handle_jumplist_buttons(&mut self, window: u32, kind: MouseKind) -> Option<UpdateEffect> {
        let backward = match kind {
            MouseKind::Press(IpcMouseButton::Back) => true,
            MouseKind::Press(IpcMouseButton::Forward) => false,
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

        Some(if backward {
            action_handlers::jump::jump_backward(self)
        } else {
            action_handlers::jump::jump_forward(self)
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
    ) -> UpdateEffect {
        if let Some(effect) = self.handle_jumplist_buttons(window, kind) {
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
            return mouse::scroll_view_at(self, view, area, matches!(kind, MouseKind::WheelDown));
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
    pub fn set_fs_watch_host(&mut self, host: Arc<dyn FsWatchHost>) {
        self.fs_watch_host = host;
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

    pub fn active_workspace(&self) -> &Workspace {
        &self.workspaces[self.active_workspace]
    }

    pub fn active_workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active_workspace]
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

        let Some(bg) = crate::render::review::style_rgb(
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
            .map(|&c| crate::render::review::dim_rgb(c, bg, dim))
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
        action_handlers::dispatch(self, &Diff);
    }

    /// Open the working-tree diff for the `stoat review` entry point.
    ///
    /// `Diff` turns the view on for the current editor. On the pathless startup
    /// scratch, which has no changes of its own, the toggle crosses into the
    /// first changed file, installs its diff map, and lands the cursor on its
    /// first hunk with the view scrolled to it. With no changed files it sets
    /// the "no more changes" status and stays on the scratch.
    pub fn open_working_tree_diff(&mut self) {
        action_handlers::dispatch(self, &Diff);
    }

    /// Open the three-way conflict resolve view for the `stoat conflict` entry
    /// point.
    ///
    /// `Conflict` opens the repository's conflicted files in the ours / result /
    /// theirs view. With no index conflicts it sets the "no merge conflicts"
    /// status and stays on the startup files view.
    pub fn open_conflict_view(&mut self) {
        action_handlers::dispatch(self, &Conflict);
    }

    /// Handle that makes [`Self::run`] quit at its next loop turn when
    /// notified via [`tokio::sync::Notify::notify_one`], regardless of the
    /// editor's current mode or focus. The `--timeout` self-driver holds a
    /// clone and fires it after the delay to auto-close the session.
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
                notif = self.pty_rx.recv() => {
                    let Some(notif) = notif else { continue };
                    self.handle_pty_notification(notif)
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
                }
                // A poll tick is a reason to re-stat the followed files, not a
                // reason to paint. Only the pump finding one of them advanced
                // asks for a frame.
                Some(()) = self.auto_reload_rx.recv() => {
                    if action_handlers::file::pump_auto_reload(self) {
                        UpdateEffect::Redraw
                    } else {
                        UpdateEffect::None
                    }
                }
                _ = self.redraw_notify.notified() => UpdateEffect::Redraw,
                _ = self.shutdown_notify.notified() => UpdateEffect::Quit,
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
            };

            let (drained, coalesced) = self.drain_pending(&mut events);
            let effect = first.merge(drained);
            #[cfg(feature = "perf")]
            self.perf.record_coalesced(coalesced);
            #[cfg(not(feature = "perf"))]
            let _ = coalesced;

            match effect {
                UpdateEffect::Redraw => {
                    self.drive_background();
                    // A `:wq` deferred behind a format-on-save write sets
                    // `quit_requested` from the pump inside `drive_background`
                    // once the write lands, so quit on the frame it completes.
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
    fn tick_scroll_anim(&mut self, dt: f32) -> bool {
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
            let target = editor.scroll_row as f32;
            let viewport = editor
                .viewport_rows
                .unwrap_or(action_handlers::movement::DEFAULT_VIEWPORT_ROWS)
                .max(1);
            let mut closed = 0.0;
            if (target - editor.scroll_offset).abs() > viewport as f32 * 3.0 {
                editor.scroll_offset = target;
                editor.scroll_glide = ScrollGlide::None;
            } else {
                let (offset, settled) = action_handlers::movement::step_scroll_ease(
                    editor.scroll_offset,
                    target,
                    dt,
                    ease,
                );
                closed = (offset - editor.scroll_offset).abs();
                editor.scroll_offset = offset;
                if settled {
                    editor.scroll_glide = ScrollGlide::None;
                }
            }
            // A wheel glide defers its cursor follow to the settle, so when it
            // just cleared, clamp the anchored cursor into the landing band.
            if was == ScrollGlide::Wheel && editor.scroll_glide == ScrollGlide::None {
                action_handlers::movement::clamp_cursor_to_view(editor, scrolloff);
            }
            // While the glide is still in flight but has slowed below the re-home
            // velocity, land the cursor in the band now rather than at the settle.
            // clamp_cursor_to_view no-ops inside the band, so this re-homes about
            // once per notch as the viewport drifts on.
            if was == ScrollGlide::Wheel
                && editor.scroll_glide == ScrollGlide::Wheel
                && closed / dt.max(1e-6) <= WHEEL_REHOME_MAX_VELOCITY
            {
                action_handlers::movement::clamp_cursor_to_view(editor, scrolloff);
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
    /// that arrive mid-drain are handled on the next loop iteration, which
    /// keeps render forward-progress under a sustained producer.
    fn drain_pending(&mut self, events: &mut UnboundedReceiver<Event>) -> (UpdateEffect, usize) {
        let mut effect = UpdateEffect::None;
        let mut coalesced = 0;

        while let Ok(event) = events.try_recv() {
            effect = effect.merge(self.update(event));
            coalesced += 1;
        }
        while let Ok(notif) = self.pty_rx.try_recv() {
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
        self.drain_index_updates();

        (effect, coalesced)
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
        if self.git_host.discover(&git_root).is_none() {
            tracing::info!(
                target: "stoat::app",
                root = %git_root.display(),
                "workspace root is not in a git repository; code indexing and fs-watching disabled",
            );
            return;
        }
        let index_dir = self.index_dir_for_build(&git_root);
        if !self.persistence_disabled {
            // The walk reads the tree, which is what blocks the runtime thread
            // before the first frame on a large repo. Run it on the blocking
            // pool so startup stays interactive.
            let watcher = self.fs_watch_host.clone();
            let fs = self.fs_host.clone();
            let root = git_root.clone();
            self.executor
                .spawn_blocking(move || watch_workspace_dirs(fs.as_ref(), watcher.as_ref(), &root))
                .detach();
        }
        let handles = crate::code_index::build::IndexBuild {
            fs: self.fs_host.clone(),
            languages: self.language_registry.clone(),
            tx: self.index_update_tx.clone(),
            redraw: self.redraw_notify.clone(),
        };
        self._index_build_task = Some(crate::code_index::build::build_index(
            &self.executor,
            handles,
            git_root,
            workspace,
            index_dir,
        ));
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
    /// At most [`INDEX_DRAIN_CAP`] updates are processed per call. On hitting
    /// the cap the drain schedules a redraw and returns, leaving the remainder
    /// queued for the next turn.
    fn drain_index_updates(&mut self) {
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
                } => {
                    resolve_pending.insert(workspace);
                    completed.insert(workspace);
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

            if drained >= INDEX_DRAIN_CAP {
                self.redraw_notify.notify_one();
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
                if completed.contains(&workspace) {
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
    /// clobbered. Otherwise the buffers and panes install and, when the target
    /// is still active, terminals respawn.
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
        if self.active_workspace == workspace {
            action_handlers::respawn_terminal_panes(self);
        }
    }

    /// Persist a workspace's state, serializing it off this thread.
    ///
    /// Runs on every workspace switch, open and close, so the snapshot is taken
    /// here and the RON encoding and writes happen on the blocking pool. What
    /// lands on disk is the workspace as it stood at this call, whatever it does
    /// afterwards.
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
    /// per-path debounces rather than dispatching them, so it has nothing to
    /// report and belongs at the event edge.
    fn drain_external(&mut self) -> bool {
        crate::lsp::drain::drain_lsp_notifications(self);
        crate::lsp::drain::drain_lsp_incoming_requests(self);
        crate::lsp::drain::install_pending_lsp_host(self);

        let external_edits = debounce::drain_pending_external_edits(self);
        let git_refresh = debounce::drain_pending_git_refresh(self);
        let code_search = debounce::drain_pending_code_search(self);
        let diff_warm_files = debounce::drain_pending_diff_warm_files(self);
        let index_edits = debounce::drain_pending_index_edits(self);

        external_edits || git_refresh || code_search || diff_warm_files || index_edits
    }

    pub(crate) fn update(&mut self, event: Event) -> UpdateEffect {
        debounce::drain_fs_watch_events(self);
        self.drain_external();
        let effect = match event {
            Event::Resize(w, h) => {
                self.size = Rect::new(0, 0, w, h);
                let size = self.size;
                self.active_workspace_mut().layout(size);
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
                    action_handlers::movement::clamp_cursor_to_view(editor, scrolloff);
                }

                let before = self.focused_cursor_pos();
                let term_before = self.focused_shell_term_id();
                let origin = self.focus_location();
                let effect = self.handle_key(key);
                self.auto_insert_focused_terminal(term_before, origin);
                let cursor_moved = self.focused_cursor_pos() != before;

                // Re-follow the cursor when a key moved it, pulling the view
                // along so a count jump past the margin lands the view on the
                // cursor rather than stranding it on the edge. A keyboard scroll
                // (z j / z k) never moves the cursor, so its view stays put.
                let scrolled = match action_handlers::focused_editor_mut(self) {
                    Some(editor) => {
                        cursor_moved && action_handlers::movement::follow_jump(editor, scrolloff)
                    },
                    None => false,
                };

                if cursor_moved {
                    self.sync_review_chunk_to_cursor();
                }

                if scrolled {
                    effect.merge(UpdateEffect::Redraw)
                } else {
                    effect
                }
            },
            Event::Mouse(mouse) => {
                let term_before = self.focused_shell_term_id();
                let origin = self.focus_location();
                let effect = mouse::handle_mouse(self, mouse);
                self.auto_insert_focused_terminal(term_before, origin);
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
    pub(crate) fn set_status(&mut self, text: impl Into<String>) {
        self.pending_message = Some(text.into());
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
        self.pending_diff_warm.is_some() || !self.diff_warm_files.is_empty()
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
        // the keymap, and terminal passthrough returns before any reader.
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
                .map(|(actions, _)| actions.iter().map(|a| &a.name).collect::<Vec<_>>()),
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
                self.write_to_term(agent_id, &[0x03]);
                return UpdateEffect::None;
            }
            // Ctrl-C with a keymap binding (`pane == run` -> RunInterrupt) routes
            // through the keymap below. An unbound Ctrl-C quits.
            if self.keymap_lookup(&key, &mut lookup).is_none() {
                return UpdateEffect::Quit;
            }
        }

        if self.pending_macro_replay {
            self.pending_macro_replay = false;
            if let KeyCode::Char(ch) = key.code {
                // The register name is half of what a recording needs to replay
                // this later, and returning here is what would skip the capture
                // every other key goes through below.
                action_handlers::macro_recording::capture(self, &key);
                return action_handlers::macro_recording::execute_replay(self, ch);
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

        if let Some(agent_id) = self.term_input_target() {
            return self.route_key_to_term(agent_id, key);
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
            let printable = matches!(key.code, KeyCode::Char(_))
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT);
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
                    let target = self.consume_selected_register();
                    action_handlers::yank::write_fragments_to_register(self, target, fragments);
                    self.set_status("yanked hover selection");
                    return UpdateEffect::Redraw;
                }
            }
            self.pending_hover = None;
            self.pending_hover_request = None;
            if matches!(key.code, KeyCode::Esc) {
                return UpdateEffect::Redraw;
            }
        }

        if takes_pending && self.pending_find.is_some() {
            if let KeyCode::Char(ch) = key.code {
                let (kind, extend, count) = self.pending_find.take().expect("checked above");
                return action_handlers::movement::execute_find(self, kind, ch, extend, count);
            }
            self.pending_find = None;
        }

        if normal_mode && self.pending_mark.is_some() {
            if let KeyCode::Char(ch) = key.code {
                let request = self.pending_mark.take().expect("checked above");
                return action_handlers::marks::execute_mark(self, request, ch);
            }
            self.pending_mark = None;
        }

        if takes_pending && self.pending_replace {
            if let KeyCode::Char(ch) = key.code {
                self.pending_replace = false;
                return action_handlers::movement::execute_replace(self, ch);
            }
            self.pending_replace = false;
        }

        if takes_pending && self.pending_surround_add {
            if let KeyCode::Char(ch) = key.code {
                self.pending_surround_add = false;
                return action_handlers::surround::execute_surround_add(self, ch);
            }
            self.pending_surround_add = false;
        }

        if takes_pending && self.pending_register_select {
            if let KeyCode::Char(ch) = key.code {
                self.pending_register_select = false;
                action_handlers::yank::execute_select_register(self, ch);
                return UpdateEffect::Redraw;
            }
            self.pending_register_select = false;
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
        }

        if takes_pending && self.pending_surround_delete {
            if let KeyCode::Char(ch) = key.code {
                self.pending_surround_delete = false;
                return action_handlers::surround::execute_surround_delete(self, ch);
            }
            self.pending_surround_delete = false;
        }

        if takes_pending && self.pending_textobject_select.is_some() {
            if let KeyCode::Char(ch) = key.code {
                let mode = self.pending_textobject_select.expect("checked above");
                self.pending_textobject_select = None;
                return action_handlers::textobject::execute_select_textobject(self, mode, ch);
            }
            self.pending_textobject_select = None;
        }

        if takes_pending && self.pending_goto_word.is_some() {
            if let KeyCode::Char(ch) = key.code {
                let labels = self.pending_goto_word.as_ref().expect("checked above");
                match crate::goto_word::step_jump(labels, &self.pending_goto_word_input, ch) {
                    crate::goto_word::JumpStep::Jump(offset) => {
                        self.pending_goto_word = None;
                        self.pending_goto_word_input.clear();
                        return action_handlers::movement::jump_to_offset(self, offset);
                    },
                    crate::goto_word::JumpStep::Continue => {
                        self.pending_goto_word_input.push(ch);
                        return UpdateEffect::Redraw;
                    },
                    crate::goto_word::JumpStep::Cancel => {
                        self.pending_goto_word = None;
                        self.pending_goto_word_input.clear();
                        return UpdateEffect::Redraw;
                    },
                }
            }
            self.pending_goto_word = None;
            self.pending_goto_word_input.clear();
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

        let mut effect = UpdateEffect::None;
        let mut dispatched_action = false;
        let mut dispatched_hover = false;
        let mut dispatched_code_action = false;
        let mut dispatched_rename_symbol = false;
        let mut dispatched_symbol_picker = false;
        for ra in actions.iter() {
            if ra.name == "SetMode" {
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
        if dispatched_action {
            self.pending_count = None;
            if !dispatched_hover {
                self.pending_hover = None;
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

    /// The agent session that should receive raw keystrokes, if any.
    ///
    /// `Some` only in insert mode with a focused `View::Agent` or
    /// `View::Terminal` split pane. This mirrors how insert mode sends typing
    /// to the focused editor, except the bytes go to the pane's PTY. Normal
    /// mode keeps its editor and pane-navigation bindings.
    ///
    /// An overlay input (command palette, finder, search, ...) outranks
    /// terminal passthrough. While one is focused it owns typing, so keys reach
    /// its insert path rather than the PTY behind it. Its InputView sits in
    /// insert, so the mode guard alone would misroute every key to the terminal.
    /// The Ctrl-C branch in [`Self::handle_key`] encodes the same order.
    ///
    /// A `View::Terminal` pane auto-enters insert when focus arrives
    /// ([`Self::auto_insert_focused_terminal`]), so typing reaches the shell
    /// with no `i`. A `View::Agent` pane is entered manually with `i`, and both
    /// leave via the [`Self::route_key_to_term`] escape.
    fn term_input_target(&self) -> Option<TermId> {
        if self.focused_editor_ids().is_some() {
            return None;
        }
        if self.focused_mode() != "insert" {
            return None;
        }
        self.focused_term_id()
    }

    /// Encode `key` and send it to the agent's PTY, or handle the focus escape.
    ///
    /// `Esc` leaves passthrough by returning to normal mode, where the editor
    /// and pane-navigation bindings resume and the user can move focus, split,
    /// or close the pane. That keystroke is not forwarded. Every other key,
    /// including `Ctrl-W`, is encoded by [`encode_key_to_pty`] and written, so
    /// the agent still receives it. Keys with no encoding are swallowed.
    ///
    /// As a result, a literal `Esc` no longer reaches the agent during
    /// passthrough. The deferred per-agent normal-mode bindings would restore
    /// a way to send it.
    ///
    /// For a `View::Terminal` pane the normal mode is a waypoint, not a
    /// destination. A terminal has no cursor to move and no text to operate on,
    /// so `Esc` also sends focus back where it came from
    /// ([`Self::return_from_terminal`]) and refocusing re-enters insert
    /// ([`Self::auto_insert_focused_terminal`]). With no origin to return to the
    /// drop to normal stands on its own, which keeps normal mode reachable on a
    /// lone terminal pane. A `View::Agent` pane never returns and stays in
    /// normal until the user presses `i`.
    fn route_key_to_term(&mut self, agent_id: TermId, key: KeyEvent) -> UpdateEffect {
        mouse::clear_term_selection(self, agent_id);
        if key.code == KeyCode::Esc {
            self.transition_mode("normal".to_string());
            self.return_from_terminal();
            return UpdateEffect::Redraw;
        }

        if let Some(bytes) = encode_key_to_pty(&key) {
            self.write_to_term(agent_id, &bytes);
        }
        UpdateEffect::None
    }

    /// Encode a paste of `text` and send it to the terminal's PTY.
    ///
    /// Returns [`UpdateEffect::None`] on the same reasoning as a forwarded
    /// keystroke. Nothing on screen changes until the child echoes the text
    /// back, and that read is what asks for the repaint.
    fn route_paste_to_term(&mut self, term_id: TermId, text: &str) -> UpdateEffect {
        mouse::clear_term_selection(self, term_id);
        let bracketed = self
            .active_workspace()
            .terms
            .get(term_id)
            .is_some_and(|session| session.term.bracketed_paste());

        self.write_to_term(term_id, &encode_paste_to_pty(text, bracketed));
        UpdateEffect::None
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
    fn write_to_term(&self, agent_id: TermId, bytes: &[u8]) {
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

    /// Returns the register selected via [`SelectRegister`] and
    /// clears the field. Yank / paste call this once each so the
    /// selection is consumed by exactly one operation; subsequent
    /// ops fall back to the unnamed register.
    pub(crate) fn consume_selected_register(&mut self) -> register::Register {
        self.selected_register
            .take()
            .unwrap_or(register::Register::Unnamed)
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

    /// Point the review chunk cursor at the chunk under the focused review
    /// editor's text cursor, so status actions act on the chunk the user is
    /// looking at rather than the last `n`/`p` target.
    ///
    /// No-op unless the focused editor is a review editor. Called after a key
    /// moved the text cursor. Both the chunk cursor and its highlight track the
    /// text cursor, and `n`/`p` move the text cursor too, so they never diverge.
    fn sync_review_chunk_to_cursor(&mut self) {
        let buffer_row = {
            let Some(editor) = action_handlers::focused_editor_mut(self) else {
                return;
            };
            if editor.review_view.is_none() {
                return;
            }
            let snapshot = editor.display_map.snapshot();
            let buffer_snapshot = snapshot.buffer_snapshot();
            let sel = editor.selections.newest_anchor();
            let offset = stoat_text::cursor_offset(
                buffer_snapshot.rope(),
                buffer_snapshot.resolve_anchor(&sel.tail()),
                buffer_snapshot.resolve_anchor(&sel.head()),
            );
            buffer_snapshot.rope().offset_to_point(offset).row
        };

        let ws = self.active_workspace_mut();
        let Some(editor_id) = ws.review.as_ref().and_then(|s| s.view_editor) else {
            return;
        };
        let Some(editor) = ws.editors.get_mut(editor_id) else {
            return;
        };
        let Some(view) = editor.review_view.as_mut() else {
            return;
        };
        let Some((chunk_id, _)) = view.chunk_and_status_at_row(buffer_row) else {
            return;
        };
        let Some(session) = ws.review.as_mut() else {
            return;
        };
        if session.cursor.current != Some(chunk_id) {
            session.cursor.current = Some(chunk_id);
            session.version += 1;
            view.refresh_from_session(session);
        }
    }

    /// The scratch editor the open modal types into, or `None` when no modal is
    /// open or the open one has no input of its own.
    ///
    /// A picker without an input (the jumplist, diagnostics, and location pickers,
    /// the quit prompt, a modal run) answers `None` so the caller keeps resolving
    /// through the panes behind it, which is where the keys it does handle land.
    fn active_modal_input(&self) -> Option<(EditorId, BufferId)> {
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
            ActiveModal::Run
            | ActiveModal::QuitConfirm
            | ActiveModal::Jumplist
            | ActiveModal::Diagnostics
            | ActiveModal::Location => return None,
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
            FocusTarget::Dock(dock_id) => match ws.docks.get(dock_id) {
                Some(dock) => dock.view.clone(),
                None => return None,
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
                && let Some(register) = action_handlers::yank::register_for_char(ch)
                && let Some(fragments) =
                    action_handlers::yank::read_register_fragments(self, register)
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
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                self.editor_insert(editor_id, buffer_id, s);
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
                    let last = popup.items.len().saturating_sub(1);
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
            _ => None,
        }
    }

    pub(crate) fn cursor_after_only_whitespace(
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
        let sel = editor.selections.newest_anchor().clone();
        let rope = buf_snapshot.rope();
        let offset = stoat_text::cursor_offset(
            rope,
            buf_snapshot.resolve_anchor(&sel.tail()),
            buf_snapshot.resolve_anchor(&sel.head()),
        );
        for ch in rope.reversed_chars_at(offset) {
            if ch == '\n' {
                return true;
            }
            if !ch.is_whitespace() {
                return false;
            }
        }
        true
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

    /// The focused pane's [`TermId`] only when it is a shell terminal
    /// ([`View::Terminal`]), never an agent pane. Drives the focus-arrival
    /// auto-insert, which applies to shell terminals alone -- agent panes keep
    /// their manual `i` entry.
    pub(crate) fn focused_shell_term_id(&self) -> Option<TermId> {
        let ws = self.active_workspace();
        let FocusTarget::SplitPane = ws.focus else {
            return None;
        };
        match &ws.panes.pane(ws.panes.focus()).view {
            View::Terminal(id) => Some(*id),
            _ => None,
        }
    }

    /// Where focus sits right now, in the form a terminal records to return to.
    ///
    /// Captured before an event is handled, so it names the place the event is
    /// about to leave.
    fn focus_location(&self) -> TermReturnFocus {
        let ws = self.active_workspace();
        match ws.focus {
            FocusTarget::SplitPane => TermReturnFocus::Pane {
                tab: ws.active_tab,
                pane: ws.panes.focus(),
            },
            FocusTarget::Dock(id) => TermReturnFocus::Dock(id),
        }
    }

    /// Ready a shell terminal that focus has just arrived on.
    ///
    /// The terminal records where focus came from and then enters insert, so
    /// typing reaches the child without a manual `i`.
    ///
    /// `prev` is [`Self::focused_shell_term_id`] and `origin` is
    /// [`Self::focus_location`], both captured before the event was handled.
    /// Both steps run only when a terminal is focused now and it is a different
    /// terminal than `prev`, which is what "focus arrived" means. Comparing ids
    /// this way leaves an in-place `Esc` -- the same terminal focused before and
    /// after -- alone, so it keeps its own record and stays in normal.
    ///
    /// The record is what [`Self::return_from_terminal`] sends `Esc` back to,
    /// and it is overwritten on every arrival, so hopping between two terminals
    /// bounces between them.
    fn auto_insert_focused_terminal(&mut self, prev: Option<TermId>, origin: TermReturnFocus) {
        let Some(term_id) = self.focused_shell_term_id() else {
            return;
        };
        if prev == Some(term_id) {
            return;
        }
        if let Some(term) = self.active_workspace_mut().terms.get_mut(term_id) {
            term.return_focus = Some(origin);
        }
        if self.focused_mode() == "insert" {
            return;
        }
        self.transition_mode("insert".to_string());
    }

    /// Send focus back to wherever it was when it last arrived on the focused
    /// shell terminal. Returns whether focus actually moved.
    ///
    /// False when the focused pane is not a shell terminal, when it holds no
    /// record, or when the record no longer names a reachable place -- a closed
    /// pane, a dropped dock, a tab that has since gone. A record naming the
    /// currently-focused location is also rejected, which is how a `:terminal`
    /// opened in place keeps `Esc` as a plain drop to normal.
    ///
    /// The record is kept rather than consumed. Every arrival overwrites it
    /// anyway, and a return that fails validation should not also erase where
    /// the terminal came from.
    fn return_from_terminal(&mut self) -> bool {
        let Some(term_id) = self.focused_shell_term_id() else {
            return false;
        };
        let Some(record) = self
            .active_workspace()
            .terms
            .get(term_id)
            .and_then(|term| term.return_focus)
        else {
            return false;
        };

        match record {
            TermReturnFocus::Dock(id) => {
                let ws = self.active_workspace_mut();
                if ws.focus == FocusTarget::Dock(id) || !ws.docks.contains_key(id) {
                    return false;
                }
                ws.focus = FocusTarget::Dock(id);
                true
            },
            TermReturnFocus::Pane { tab, pane } if tab == self.active_workspace().active_tab => {
                let ws = self.active_workspace_mut();
                if pane == ws.panes.focus() || !ws.panes.split_pane_ids().contains(&pane) {
                    return false;
                }
                ws.panes.set_focus(pane);
                ws.focus = FocusTarget::SplitPane;
                true
            },
            TermReturnFocus::Pane { tab, pane } => {
                let parked_holds_pane = self
                    .active_workspace()
                    .tabs
                    .get(tab)
                    .and_then(|t| t.parked.as_ref())
                    .is_some_and(|tree| tree.split_pane_ids().contains(&pane));
                if !parked_holds_pane || !self.active_workspace_mut().switch_tab(tab) {
                    return false;
                }
                // A parked tree carries the zero-sized rects it was stored with,
                // so it has to be fitted to the screen before it is focused.
                let size = self.size();
                let ws = self.active_workspace_mut();
                ws.layout(size);
                ws.panes.set_focus(pane);
                ws.focus = FocusTarget::SplitPane;
                true
            },
        }
    }

    /// Switch the focused target's mode to `next`, opening or closing the
    /// insert-run buffer that backs the `.` register. Entering
    /// any insert-like mode (`insert`, `reword_insert`) starts a
    /// fresh run. Leaving commits the run's text into
    /// [`Self::last_insert_text`] (when non-empty) and clears the
    /// scratch buffer.
    pub(crate) fn transition_mode(&mut self, next: String) {
        let was_insert = is_insert_run_mode(self.focused_mode());
        let now_insert = is_insert_run_mode(&next);
        let leaving_insert = was_insert && !now_insert;

        let typed_nothing = if leaving_insert {
            let run = self.current_insert_run.take().unwrap_or_default();
            if run.is_empty() {
                true
            } else {
                self.last_insert_text = Some(run);
                false
            }
        } else {
            false
        };

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
        self.set_focused_mode(next);
    }

    /// Open an undo group so the whole insert session collapses into one undo
    /// step, capturing the pre-session selections to restore on undo.
    fn begin_insert_undo_group(&mut self) {
        let Some((buffer_id, before)) = self.focused_undo_snapshot() else {
            return;
        };
        if let Some(buffer) = self.active_workspace().buffers.get(buffer_id) {
            buffer.write().expect("poisoned").begin_group(before);
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

    /// Move each block cursor in the focused editor back one grapheme, landing
    /// it 1-wide, so leaving an append-style insert lands on the last typed
    /// char rather than one cell past it.
    ///
    /// A cursor at a line start stays put, since retreating across the newline
    /// would land it on the previous line. This covers the buffer start and an
    /// empty line whose auto-indent was stripped on the same transition.
    fn restore_cursor_after_append(&mut self) {
        let Some(editor) = action_handlers::focused_editor_mut(self) else {
            return;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let rope = buf_snap.rope();
        action_handlers::movement::move_cursors(&mut editor.selections, buf_snap, false, |read| {
            let cursor = stoat_text::cursor_offset(rope, read.tail, read.head);
            // The guard peeks at the preceding scalar rather than the cluster,
            // since only a literal newline pins the cursor in place. Any other
            // cluster is stepped over whole.
            let back = match rope.reversed_chars_at(cursor).next() {
                Some(ch) if ch != '\n' => rope.prev_grapheme_boundary(cursor),
                _ => cursor,
            };
            Some((back, SelectionGoal::None))
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

        let mut ranges: Vec<(usize, usize)> = editor
            .selections
            .all_anchors()
            .iter()
            .filter(|sel| auto_indent_cursors.contains(&sel.id))
            .filter_map(|sel| {
                let cursor = stoat_text::cursor_offset(
                    rope,
                    buf_snapshot.resolve_anchor(&sel.tail()),
                    buf_snapshot.resolve_anchor(&sel.head()),
                );
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
            let mut guard = buffer.write().expect("poisoned");
            for (start, end) in ranges.iter().rev() {
                guard.edit(*start..*end, "");
            }
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
        if !text.is_empty()
            && let Some(run) = self.current_insert_run.as_mut()
        {
            run.push_str(text);
        }
        let mut inserts = self.editor_cursor_offsets(editor_id);
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
            .zip(ends.chunks_exact(2))
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
        editor
            .selections
            .land_block_cursors(&landings, EndCell::Empty, new_buf);
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
                .map(|(_, offset, text)| (*offset..*offset, text.as_str()))
                .collect();
            buffer.write().expect("poisoned").edit_batch(&edits);
        }

        // Each cursor lands after its own text, shifted by everything inserted
        // at or before it. Lengths differ per cursor, so this is a running total
        // where the uniform path multiplies one length by the insertion's index.
        let mut inserted = 0usize;
        let landings: Vec<(usize, usize)> = insertions
            .iter()
            .map(|(id, offset, text)| {
                inserted += text.len();
                (*id, *offset + inserted)
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
            .and_then(|lang| lang.indent_query().is_some().then_some(lang))
            .zip(buffers.syntax(buffer_id))
            .filter(|(_, syntax)| syntax.version == guard.version());

        match fresh_tree {
            Some((lang, syntax)) => language::newline_indent(
                lang.indent_query().expect("indent query present"),
                syntax.tree.root_node(),
                &syntax.rope_snapshot,
                cursor_offset,
                guard.indent_style().as_str(),
            ),
            None => language::line_leading_whitespace(rope, row),
        }
    }

    /// The text to insert for a newline at `cursor_offset`, being a line ending
    /// plus the continued indentation.
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
    pub(crate) fn newline_continuation(&self, buffer_id: BufferId, cursor_offset: usize) -> String {
        let continued_comment = {
            let buffers = &self.active_workspace().buffers;
            let tokens = buffers
                .language_for(buffer_id)
                .map_or(&[][..], |lang| lang.line_comments);
            match buffers.get(buffer_id) {
                Some(buffer) => {
                    let guard = buffer.read().expect("buffer poisoned");
                    let rope = guard.rope();
                    let row = rope.offset_to_point(cursor_offset).row;
                    let line_start = rope.point_to_offset(stoat_text::Point::new(row, 0));
                    let line_end =
                        rope.point_to_offset(stoat_text::Point::new(row, rope.line_len(row)));
                    action_handlers::movement::line_comment_continues(
                        rope, line_start, line_end, tokens,
                    )
                    .filter(|&(start, _)| start < cursor_offset)
                    .map(|(_, token)| {
                        format!("{}{token} ", language::line_leading_whitespace(rope, row))
                    })
                },
                None => None,
            }
        };
        match continued_comment {
            Some(prefix) => format!("\n{prefix}"),
            None => format!("\n{}", self.newline_indent_string(buffer_id, cursor_offset)),
        }
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
        self.editor_delete_ranges(editor_id, buffer_id, move |rope, cursor| {
            backspace_range(rope, cursor, indent_width)
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
        // Repeat replays one string, and there is no one string when every
        // cursor continues its own line. The newest cursor's is what the
        // uniform insertion recorded before the others had their own.
        let newest_offset = self.newest_cursor_offset(editor_id);
        let newest = match newest_offset {
            Some(offset) => self.newline_continuation(buffer_id, offset),
            None => "\n".to_string(),
        };
        if let Some(run) = self.current_insert_run.as_mut() {
            run.push_str(&newest);
        }

        let cursors = self.editor_cursor_offsets(editor_id);
        let insertions: Vec<(usize, usize, String)> = cursors
            .into_iter()
            .map(|(id, offset)| {
                // Deriving a continuation runs the indent query, and the newest
                // cursor's was just derived above for the repeat record. The
                // single-cursor case is every cursor.
                let text = match newest_offset == Some(offset) {
                    true => newest.clone(),
                    false => self.newline_continuation(buffer_id, offset),
                };
                (id, offset, text)
            })
            .collect();

        self.editor_insert_each(editor_id, buffer_id, insertions);
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
            .zip(ends.chunks_exact(2))
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
        editor
            .selections
            .land_block_cursors(&new_offsets, EndCell::Empty, new_buf);
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
    /// beside the agent pane, splits a new pane for the file, and stores the
    /// request's oneshot in [`Workspace::editor_bridge_waiters`] keyed by the
    /// opened buffer. Returns [`UpdateEffect::None`] when no live workspace owns
    /// the session or the file cannot be opened. The dropped oneshot then
    /// unblocks the agent so its `$EDITOR` invocation does not hang.
    pub(crate) fn handle_agent_control(&mut self, ctl: AgentControl) -> UpdateEffect {
        match ctl {
            AgentControl::OpenEditor { uid, path, done } => {
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

                let Some(buffer_id) =
                    action_handlers::file::open_file_in_pane(self, new_pane, &path)
                else {
                    return UpdateEffect::None;
                };
                self.active_workspace_mut()
                    .editor_bridge_waiters
                    .insert(buffer_id, done);
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

    /// Start the per-session agent hook server for `uid` on the executor.
    ///
    /// Binds the session's hook socket (see [`crate::run::agent_socket_path`])
    /// and forwards decoded events to [`Self::handle_agent_event`] through the
    /// shared channel. Callers spawn this alongside the owned Claude subshell.
    pub fn serve_term_session(&self, uid: WorkspaceUid) -> io::Result<()> {
        let socket_path = crate::run::agent_socket_path(uid)?;
        let tx = self.agent_event_tx.clone();
        let control_tx = self.agent_control_tx.clone();
        self.executor
            .spawn(crate::agent_ipc::serve_agent_hooks(
                socket_path,
                uid,
                tx,
                control_tx,
            ))
            .detach();
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
                    crate::host::clipboard_copy(clipboard_host.as_ref(), env_host.as_ref(), &text);
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
                if !replies.is_empty() {
                    self.write_to_term(agent_id, &replies);
                }
                for text in clipboard_writes {
                    crate::host::clipboard_copy(clipboard_host.as_ref(), env_host.as_ref(), &text);
                }
                if visible {
                    self.pty_dirty = true;
                }
                UpdateEffect::None
            },
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

                // Only terminal panes retire on exit. An agent pane sharing the
                // same reader keeps its last frame, so bail before touching the
                // session when nothing references it as a terminal.
                if pane_ids.is_empty() && dock_ids.is_empty() {
                    return UpdateEffect::None;
                }

                // Insert keystrokes reach a terminal only when a split pane
                // holds focus (see `term_input_target`), so a focused dock
                // must not trigger the reset. Recorded before the loop closes
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
        let syntax = self.syntax_highlight;
        for editor in self.active_workspace_mut().editors.values_mut() {
            editor.display_map.set_syntax_highlighting(syntax);
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
    pub(crate) fn drive_background(&mut self) {
        crate::project_env::ensure_loaded(self);
        crate::project_env::install_pending(self);
        self.install_pending_workspace_restore();
        crate::diff_warm::ensure_diff_warm(self);
        crate::diff_warm::install_finished(self);
        action_handlers::file::install_pending_opens(self);
        action_handlers::sync_palette_picker(self);
        action_handlers::sync_file_finder_preview(self);
        self.drive_parse_jobs();
        self.drive_diff_jobs();

        self.drive_pumps();
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
        let review = action_handlers::pump_review_scan(self);

        let code_search = action_handlers::code_search::pump_code_search(self);
        action_handlers::code_search::sync_code_search(self);

        let changed_file_jump = action_handlers::movement::pump_changed_file_jump(self);
        let lsp = crate::lsp::pump_all(self);
        action_handlers::workspace::sync_workspace_picker(self);

        let format_on_save = action_handlers::file::pump_format_on_save(self);
        let completion = crate::completion::request::pump(self);
        let completion_resolve = action_handlers::completion::pump_completion_resolve(self);
        let completion_accept = crate::completion::accept::pump_completion_accept(self);

        external
            || commits
            || commit_picker
            || review
            || code_search
            || changed_file_jump
            || lsp
            || format_on_save
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
            action_handlers::movement::land_block_cursor(
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
            action_handlers::movement::land_block_cursor(
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

/// Visual columns a tab advances, for the column math in [`backspace_range`].
/// Matches the editor's default render tab size.
const TAB_WIDTH: usize = 4;

/// The backward-delete span for one insert-mode backspace at `cursor`.
///
/// When the cursor follows only whitespace on its line, backspace works by
/// indent level. A preceding tab is removed on its own, and a run of spaces is
/// trimmed back to the previous `indent_width` column (a full unit when already
/// aligned). Anywhere else it removes a single grapheme. Returns `(start, end)`
/// with `start == end` for a no-op at the buffer start.
fn backspace_range(rope: &Rope, cursor: usize, indent_width: usize) -> (usize, usize) {
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
        return one_back;
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
mod tests {
    use super::*;
    use crate::{
        action_handlers::lsp::RenameInputState,
        agent_status::AgentHookEvent,
        apc_emit::{editor_page_content_version, osc_default_colors, window_content_version},
        debounce::{INDEX_EDIT_DEBOUNCE, REVIEW_EXTERNAL_EDIT_DEBOUNCE},
        display_map::DisplayPoint,
        host::FsEventKind,
        input_view::{InputView, SubmitTarget},
        run::GridSelection,
        term_session::{TermSelection, TermSession},
        test_fixture::{
            finder_layout, focused_editor_pane_area, mouse_event, open_indent_buffer,
            open_scratch_file, open_with_minimap_strip, palette_sizing,
        },
    };
    use crossterm::event::{MouseButton, MouseEventKind};
    use std::path::{Path, PathBuf};
    use stoat_config::LineNumbers;
    use stoatty_protocol::command::{self, PoolRegionCommand};

    fn stoat_with_detached_pane(window: u32) -> (Stoat, PaneId) {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;
        let ws = stoat.active_workspace_mut();
        let detached = ws.panes.split(crate::pane::Axis::Vertical);
        assert!(ws.panes.detach(detached, window));
        (stoat, detached)
    }

    #[test]
    fn window_ipc_resize_sets_detached_pane_area() {
        let (mut stoat, detached) = stoat_with_detached_pane(3);
        stoat.handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Resized {
            window: 3,
            cols: 50,
            rows: 20,
        }));
        assert_eq!(
            stoat.active_workspace().panes.pane(detached).area,
            Rect::new(0, 0, 50, 20),
        );
    }

    #[test]
    fn window_ipc_closed_reattaches_pane() {
        let (mut stoat, detached) = stoat_with_detached_pane(3);
        stoat.handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Closed { window: 3 }));
        let panes = &stoat.active_workspace().panes;
        assert_eq!(panes.pane(detached).placement, Placement::Split);
        assert!(panes.split_pane_ids().contains(&detached));
    }

    /// Nothing may assume a stoatty is listening before the handshake says so,
    /// since the rich protocol splatters raw payload over a foreign terminal.
    #[test]
    fn a_fresh_stoat_assumes_no_stoatty_is_listening() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );

        assert!(!stoat.stoatty, "the default is the safe one");
    }

    /// The first frames go out before the handshake can answer, so confirming a
    /// listener has to repaint them rather than only affect later frames.
    #[test]
    fn a_confirmed_stoatty_sets_the_flag_and_repaints() {
        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        h.stoat.stoatty = false;

        assert_eq!(
            h.stoat
                .handle_stoatty_present(Some(stoatty_protocol::PROTOCOL_VERSION)),
            UpdateEffect::Redraw
        );
        assert!(h.stoat.stoatty, "and the flag stays set");
        assert_eq!(
            h.stoat.stoatty_protocol,
            stoatty_protocol::PROTOCOL_VERSION,
            "the peer's version is kept for gating what may be emitted"
        );
    }

    /// A stoatty from before the version field answers the handshake without
    /// one, and is still a stoatty. Reading the report as a bare presence flag
    /// would lose the distinction the version exists to carry.
    #[test]
    fn a_stoatty_predating_the_version_field_still_confirms_at_version_zero() {
        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        h.stoat.stoatty = false;

        assert_eq!(
            h.stoat.handle_stoatty_present(Some(0)),
            UpdateEffect::Redraw
        );
        assert!(h.stoat.stoatty, "a version-less reply is still a stoatty");
        assert_eq!(h.stoat.stoatty_protocol, 0);
    }

    /// The bin layer wires the app up before the run loop drains the handshake,
    /// so the theme defaults cannot go out there and ride confirmation instead.
    ///
    /// The zoom claim does not ride with them. It needs the window socket to
    /// carry the presses back, and over ssh the handshake succeeds while that
    /// socket never crosses the link.
    #[test]
    fn confirming_a_stoatty_sends_the_theme_defaults_alone() {
        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        h.stoat.stoatty = false;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        h.stoat
            .handle_stoatty_present(Some(stoatty_protocol::PROTOCOL_VERSION));

        let sent: Vec<u8> = std::iter::from_fn(|| rx.try_recv().ok())
            .flatten()
            .collect();
        let expected = osc_default_colors(&h.stoat.theme);
        assert!(
            expected.starts_with(b"\x1b]10;"),
            "the harness theme defines the default colors this covers"
        );
        assert_eq!(
            sent, expected,
            "confirmation pushes the theme defaults and claims nothing, since \
             a claimed combo with no socket back swallows every press"
        );
    }

    /// The zoom claim goes out once both halves of the round trip exist,
    /// whichever arrives second, and comes back when the socket drops.
    ///
    /// A press only reaches stoat over the window socket, so claiming the combo
    /// without one means stoatty stops stepping its font and queues the press
    /// for a client that never connects. Releasing on disconnect is what hands
    /// the combo back to a stoatty that outlives this process.
    #[test]
    fn the_zoom_claim_waits_for_the_socket_and_is_released_with_it() {
        let claim = |on: bool| {
            let mut out = Vec::new();
            command::encode_zoom_capture_into(&mut out, on);
            out
        };

        for present_first in [true, false] {
            let mut h = crate::test_harness::TestHarness::with_size(80, 24);
            h.stoat.stoatty = false;
            h.stoat.window_ipc_connected = false;
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            h.stoat.set_apc_tx(tx);

            let announce =
                |h: &mut Stoat| h.handle_stoatty_present(Some(stoatty_protocol::PROTOCOL_VERSION));
            let connect = |h: &mut Stoat| h.handle_window_ipc(WindowIpc::Connected);
            match present_first {
                true => {
                    announce(&mut h.stoat);
                    connect(&mut h.stoat);
                },
                false => {
                    connect(&mut h.stoat);
                    announce(&mut h.stoat);
                },
            }

            // Every zoom frame, not a count of the claims, so a release sent
            // before anything was claimed shows up here too.
            let zoom_frames = |rx: &mut UnboundedReceiver<Vec<u8>>| {
                std::iter::from_fn(|| rx.try_recv().ok())
                    .filter(|frame| *frame == claim(true) || *frame == claim(false))
                    .collect::<Vec<_>>()
            };

            assert_eq!(
                zoom_frames(&mut rx),
                vec![claim(true)],
                "the claim is the only zoom frame, arriving present-first: {present_first}",
            );

            h.stoat.handle_window_ipc(WindowIpc::Disconnected);
            assert_eq!(
                zoom_frames(&mut rx),
                vec![claim(false)],
                "and losing the socket releases it, arriving present-first: {present_first}",
            );
        }
    }

    #[test]
    fn an_unanswered_handshake_leaves_the_session_foreign() {
        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        h.stoat.stoatty = false;

        assert_eq!(h.stoat.handle_stoatty_present(None), UpdateEffect::None);
        assert!(!h.stoat.stoatty, "a silent terminal is a foreign one");
    }

    fn zoom(delta: i32) -> WindowIpc {
        WindowIpc::Event(WindowIpcEvent::Zoom { window: 0, delta })
    }

    /// With nothing over the panes the combo is a pane resize, so the focused
    /// pane takes room from its neighbor.
    #[test]
    fn a_zoom_step_with_no_modal_open_resizes_the_focused_pane() {
        let mut h = crate::test_harness::TestHarness::with_size(101, 40);
        let left = h.stoat.active_workspace().panes.focus();
        let right = h
            .stoat
            .active_workspace_mut()
            .panes
            .split(crate::pane::Axis::Vertical);
        h.stoat.active_workspace_mut().panes.set_focus(left);
        let width = |h: &crate::test_harness::TestHarness, id| {
            h.stoat.active_workspace().panes.pane(id).area.width
        };
        assert_eq!((width(&h, left), width(&h, right)), (50, 50));

        h.stoat.handle_window_ipc(zoom(1));

        assert_eq!(
            (width(&h, left), width(&h, right)),
            (60, 40),
            "the focused pane grew a step against its neighbor"
        );
    }

    /// An open modal owns the combo, so the panes behind it must not move.
    #[test]
    fn a_zoom_step_with_a_modal_open_zooms_it_and_leaves_the_panes_alone() {
        use stoat_action::OpenFileFinder;

        let mut h = crate::test_harness::TestHarness::with_size(101, 40);
        let left = h.stoat.active_workspace().panes.focus();
        h.stoat
            .active_workspace_mut()
            .panes
            .split(crate::pane::Axis::Vertical);
        h.stoat.active_workspace_mut().panes.set_focus(left);
        action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();

        h.stoat.handle_window_ipc(zoom(1));

        assert_eq!(
            modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
            1,
            "the open finder took the step"
        );
        assert_eq!(
            h.stoat.active_workspace().panes.pane(left).area.width,
            50,
            "and the pane behind it kept its share"
        );
    }

    /// A finder over a terminal and a content size that pin its box arithmetic:
    /// `modal_box` gives it width `(56 + 6z).clamp(40, 58)` and height
    /// `(32 + 7z).clamp(12, 68)`, so the box stops growing at level 6 and stops
    /// shrinking at level -3, both well inside the ledger's own range.
    fn zoomable_finder() -> crate::test_harness::TestHarness {
        use stoat_action::OpenFileFinder;

        let mut h = crate::test_harness::TestHarness::with_size(60, 70);
        action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();
        h.stoat
            .file_finder
            .as_mut()
            .expect("finder open")
            .content_size = (120, 8);
        h
    }

    /// The box the finder draws at the level the ledger currently holds for it.
    fn finder_box(h: &crate::test_harness::TestHarness) -> Option<Rect> {
        mouse::open_modal_box(
            &h.stoat,
            ModalKind::FileFinder,
            modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
        )
    }

    /// A modal saturates against the screen long before the ledger runs out, and
    /// steps the box cannot take must not be counted -- otherwise the user has
    /// to unwind invisible levels before the modal moves again.
    #[test]
    fn zoom_steps_stop_where_the_modal_box_stops_growing() {
        let mut h = zoomable_finder();

        for _ in 0..20 {
            h.stoat.handle_window_ipc(zoom(1));
        }
        assert_eq!(
            modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
            6,
            "growing stops at the last level that moves the box, not at MODAL_ZOOM_MAX"
        );
        assert_eq!(
            finder_box(&h),
            Some(Rect::new(1, 1, 58, 68)),
            "which is the largest box the area allows"
        );

        h.stoat.handle_window_ipc(zoom(-1));

        assert_eq!(
            modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
            5,
            "so the next step back lands one level down"
        );
        assert_eq!(
            finder_box(&h),
            Some(Rect::new(1, 1, 58, 67)),
            "and shrinks the modal on that first press"
        );
    }

    #[test]
    fn zoom_steps_stop_where_the_modal_box_stops_shrinking() {
        let mut h = zoomable_finder();

        for _ in 0..20 {
            h.stoat.handle_window_ipc(zoom(-1));
        }
        assert_eq!(
            modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
            -3,
            "shrinking stops at the last level that moves the box, not at MODAL_ZOOM_MIN"
        );
        assert_eq!(
            finder_box(&h),
            Some(Rect::new(10, 29, 40, 12)),
            "which is the smallest box the modal allows"
        );

        h.stoat.handle_window_ipc(zoom(1));

        assert_eq!(
            modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
            -2,
            "so the next step forward lands one level up"
        );
        assert_eq!(
            finder_box(&h),
            Some(Rect::new(8, 26, 44, 18)),
            "and grows the modal on that first press"
        );
    }

    /// Nothing rewrites the ledger when the terminal shrinks, so a level left
    /// over from a larger one sits past what the box can take. The first press
    /// has to re-enter the range rather than spend itself on a counter.
    #[test]
    fn a_stale_zoom_level_moves_the_modal_on_its_first_step() {
        let mut h = zoomable_finder();
        h.stoat
            .modal_zoom
            .insert(ModalKind::FileFinder, MODAL_ZOOM_MAX);

        h.stoat.handle_window_ipc(zoom(-1));

        assert_eq!(
            modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
            5,
            "the stale level clamps into range before the step applies"
        );
        assert_eq!(
            finder_box(&h),
            Some(Rect::new(1, 1, 58, 67)),
            "so the box is a row shorter than the one the stale level drew"
        );
    }

    /// An area too small to host the modal has no box to measure, leaving the
    /// ledger range as the only bound on a step.
    #[test]
    fn modal_zoom_steps_clamp_at_both_ends() {
        use stoat_action::OpenFileFinder;

        let mut h = crate::test_harness::TestHarness::with_size(30, 20);
        action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();
        assert_eq!(
            mouse::open_modal_box(&h.stoat, ModalKind::FileFinder, 0),
            None,
            "the finder does not fit a terminal this small"
        );

        for _ in 0..20 {
            h.stoat.handle_window_ipc(zoom(1));
        }
        assert_eq!(
            modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
            MODAL_ZOOM_MAX,
            "growing stops at the ceiling"
        );

        for _ in 0..40 {
            h.stoat.handle_window_ipc(zoom(-1));
        }
        assert_eq!(
            modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
            MODAL_ZOOM_MIN,
            "and shrinking stops at the floor"
        );
    }

    /// Kinds read their share independently, so one modal's dragged separator
    /// must not move another's, and a kind never dragged keeps the layout's own
    /// default.
    #[test]
    fn a_dragged_share_is_read_back_per_kind() {
        let mut h = crate::test_harness::TestHarness::with_size(101, 40);

        assert_eq!(
            modal_split_percent(&h.stoat.modal_split, ModalKind::FileFinder),
            crate::render::picker::DEFAULT_LIST_PERCENT,
            "an untouched kind sits at the default"
        );

        h.stoat.modal_split.insert(ModalKind::FileFinder, 65);

        assert_eq!(
            modal_split_percent(&h.stoat.modal_split, ModalKind::FileFinder),
            65,
            "the stored share reads back"
        );
        assert_eq!(
            modal_split_percent(&h.stoat.modal_split, ModalKind::CommitPicker),
            crate::render::picker::DEFAULT_LIST_PERCENT,
            "and its sibling kinds are untouched"
        );
    }

    /// A modal already sized to its content has nothing to zoom, but it still
    /// owns the combo, so the step must not fall through to the panes it hides.
    #[test]
    fn a_zoomless_modal_swallows_the_step() {
        let mut h = crate::test_harness::TestHarness::with_size(101, 40);
        let left = h.stoat.active_workspace().panes.focus();
        h.stoat
            .active_workspace_mut()
            .panes
            .split(crate::pane::Axis::Vertical);
        h.stoat.active_workspace_mut().panes.set_focus(left);
        h.stoat.quit_all_confirm = Some(QuitAllConfirm::new(&[], std::path::Path::new("/")));

        assert_eq!(
            h.stoat.handle_window_ipc(zoom(1)),
            UpdateEffect::None,
            "nothing changed, so no redraw is owed"
        );
        assert_eq!(
            h.stoat.active_workspace().panes.pane(left).area.width,
            50,
            "the panes behind the picker kept their shares"
        );
    }

    #[test]
    fn window_ipc_focused_moves_focus_to_and_from_the_windowed_pane() {
        let (mut stoat, detached) = stoat_with_detached_pane(3);
        let split = stoat.active_workspace().panes.split_pane_ids()[0];
        stoat.active_workspace_mut().panes.set_focus(split);

        stoat.handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Focused { window: 3 }));
        assert_eq!(
            stoat.active_workspace().panes.focus(),
            detached,
            "focused(n) focuses the windowed pane"
        );

        stoat.handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Focused { window: 0 }));
        assert_eq!(
            stoat.active_workspace().panes.focus(),
            split,
            "focused(0) returns focus to the split layout"
        );
    }

    /// The buffer shown in the focused editor.
    fn focused_buffer(h: &crate::test_harness::TestHarness) -> BufferId {
        let (editor_id, _) = h.stoat.focused_editor_ids().expect("focused editor");
        h.stoat
            .active_workspace()
            .editors
            .get(editor_id)
            .expect("editor exists")
            .buffer_id
    }

    #[test]
    fn window_ipc_side_buttons_walk_the_jumplist() {
        use crate::test_harness::TestHarness;
        use stoatty_protocol::window_ipc::MouseButton as IpcMouseButton;

        let mut h = TestHarness::with_size(40, 6);
        let a = h.write_file("a.rs", "aaaa\nbbbb\n");
        let b = h.write_file("b.rs", "xxxx\nyyyy\n");

        h.open_file(&a);
        h.type_keys("l");
        let a_buffer = focused_buffer(&h);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.open_file(&b);
        let b_buffer = focused_buffer(&h);
        h.type_keys("l");

        let side_event = |kind| {
            WindowIpc::Event(WindowIpcEvent::Mouse {
                window: 0,
                kind,
                col: 0,
                row: 0,
                mods: 0,
            })
        };

        h.stoat
            .handle_window_ipc(side_event(MouseKind::Press(IpcMouseButton::Back)));
        assert_eq!(
            (focused_buffer(&h), h.primary_head_offset()),
            (a_buffer, 1),
            "back re-shows a.rs at the saved offset"
        );

        h.stoat
            .handle_window_ipc(side_event(MouseKind::Press(IpcMouseButton::Forward)));
        assert_eq!(
            (focused_buffer(&h), h.primary_head_offset()),
            (b_buffer, 1),
            "forward returns to where the jump left b.rs"
        );

        h.stoat
            .handle_window_ipc(side_event(MouseKind::Release(IpcMouseButton::Back)));
        assert_eq!(
            (focused_buffer(&h), h.primary_head_offset()),
            (b_buffer, 1),
            "a release moves nothing, so one click walks one entry"
        );
    }

    #[test]
    fn a_side_button_jump_glides_the_view_back_to_the_entry() {
        use crate::{editor_state::ScrollGlide, test_harness::TestHarness};
        use stoatty_protocol::window_ipc::MouseButton as IpcMouseButton;

        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);

        // Strand the view far down the file, then clear the glide that put it
        // there so the assertions can only see what the jump itself arms.
        action_handlers::movement::jump_to_offset(&mut h.stoat, body.len());
        let away = {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            action_handlers::movement::ensure_cursor_in_view(editor, 3);
            editor.scroll_glide = ScrollGlide::None;
            editor.scroll_row
        };
        assert!(away > 20, "precondition: the view left the origin");

        h.stoat
            .handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Mouse {
                window: 0,
                kind: MouseKind::Press(IpcMouseButton::Back),
                col: 0,
                row: 0,
                mods: 0,
            }));

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert_eq!(
            editor.scroll_row, 0,
            "back pulls the view to the recorded entry"
        );
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::Page,
            "the jump arms the glide that ships the cursor anchor"
        );
        assert_eq!(
            editor.scroll_offset, away as f32,
            "the glide starts where the view was, so the cursor arrives from below"
        );
    }

    #[test]
    fn scroll_anim_tick_eases_offset_then_settles() {
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let path = h.write_file("glide.rs", &body);
        h.open_file(&path);

        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            action_handlers::movement::wheel_scroll(editor, true);
        }
        assert!(
            h.stoat.is_animating(),
            "an armed wheel glide makes the editor animate"
        );

        h.stoat.tick_scroll_anim(0.016);
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            assert!(
                editor.scroll_offset > 0.0 && editor.scroll_offset < editor.scroll_row as f32,
                "the tick eases the offset up toward the fixed target"
            );
        }

        for _ in 0..1000 {
            if !h.stoat.is_animating() {
                break;
            }
            h.stoat.tick_scroll_anim(0.016);
        }
        assert!(!h.stoat.is_animating(), "repeated ticks settle to rest");
        assert_eq!(
            action_handlers::focused_editor_mut(&mut h.stoat)
                .expect("focused editor")
                .scroll_offset,
            3.0,
            "the offset settles on the wheel target"
        );
    }

    #[test]
    fn wheel_coast_drags_cursor_into_view_no_key_snapback() {
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        // The cursor starts at the top. Wheel-flick the view downward and let
        // it settle.
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            for _ in 0..4 {
                action_handlers::movement::wheel_scroll(editor, true);
            }
        }
        for _ in 0..1000 {
            if !h.stoat.is_animating() {
                break;
            }
            h.stoat.tick_scroll_anim(0.016);
        }

        let (coasted, row) = {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            let snapshot = editor.display_map.snapshot();
            let buffer_snapshot = snapshot.buffer_snapshot();
            let head = editor.selections.newest_anchor().head();
            let offset = buffer_snapshot.resolve_anchor(&head);
            let row = buffer_snapshot.rope().offset_to_point(offset).row;
            (editor.scroll_row, row)
        };
        assert!(coasted > 3, "the wheel coast advanced the view");
        assert!(
            row >= coasted + 3 && row < coasted + 10,
            "the coast dragged the cursor into the scrolloff band \
             (scroll_row {coasted}, cursor_row {row})",
        );

        // A later cursor motion follows normally. The view does not snap back to
        // where the cursor used to be.
        h.type_keys("k");
        let after = action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .scroll_row;
        assert!(
            after + 2 >= coasted,
            "the view stays at the coasted position rather than snapping back \
             (coasted {coasted}, after {after})",
        );
    }

    #[test]
    fn wheel_glide_keeps_cursor_planted_then_a_mid_glide_key_clamps_it() {
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let head_row = |h: &mut TestHarness| -> u32 {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            let snapshot = editor.display_map.snapshot();
            let buffer_snapshot = snapshot.buffer_snapshot();
            let head = editor.selections.newest_anchor().head();
            let offset = buffer_snapshot.resolve_anchor(&head);
            buffer_snapshot.rope().offset_to_point(offset).row
        };

        let cursor_before = head_row(&mut h);
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            for _ in 0..4 {
                action_handlers::movement::wheel_scroll(editor, true);
            }
        }
        // One tick keeps the glide in flight. The selection has not moved.
        h.stoat.tick_scroll_anim(0.016);
        assert!(h.stoat.is_animating(), "the wheel glide is still in flight");
        assert_eq!(
            head_row(&mut h),
            cursor_before,
            "mid-glide the selection stays anchored to its original line"
        );

        // A key pressed mid-glide clamps the cursor into the landing band without
        // snapping the view back up to the stranded cursor.
        let scroll_before = action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .scroll_row;
        h.type_keys("k");
        let scroll_after = action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .scroll_row;
        assert!(
            scroll_after + 2 >= scroll_before,
            "the mid-glide key does not snap the view backward \
             (before {scroll_before}, after {scroll_after})",
        );
        assert!(
            head_row(&mut h) > cursor_before,
            "the mid-glide key clamped the cursor down into the landing viewport"
        );
    }

    #[test]
    fn wheel_glide_rehomes_the_cursor_when_velocity_drops() {
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let head_row = |h: &mut TestHarness| -> u32 {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            let snapshot = editor.display_map.snapshot();
            let buffer_snapshot = snapshot.buffer_snapshot();
            let head = editor.selections.newest_anchor().head();
            let offset = buffer_snapshot.resolve_anchor(&head);
            buffer_snapshot.rope().offset_to_point(offset).row
        };

        let origin = head_row(&mut h);
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            for _ in 0..4 {
                action_handlers::movement::wheel_scroll(editor, true);
            }
        }

        // The first tick is fast, so the cursor stays anchored to its origin line.
        h.stoat.tick_scroll_anim(0.016);
        assert!(h.stoat.is_animating(), "the wheel glide is still in flight");
        assert_eq!(
            head_row(&mut h),
            origin,
            "at high velocity the cursor stays anchored to its origin line"
        );

        // As the glide slows below the re-home velocity the cursor lands in the
        // scrolloff band while the glide is still in flight.
        let mut ticks = 0;
        while head_row(&mut h) == origin {
            assert!(ticks < 100, "the cursor re-homes before the glide ends");
            h.stoat.tick_scroll_anim(0.016);
            ticks += 1;
        }
        assert!(
            h.stoat.is_animating(),
            "the re-home beat the settle, so the glide is still gliding"
        );
        let landed = head_row(&mut h);
        let band_top = action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .scroll_row;
        assert!(
            landed >= band_top + 3 && landed < band_top + 10,
            "the re-home lands the cursor in the scrolloff band \
             (scroll_row {band_top}, cursor_row {landed})"
        );

        // A further notch drifts the viewport on, so the cursor re-homes a second
        // time mid-glide rather than freezing until the settle.
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            action_handlers::movement::wheel_scroll(editor, true);
        }
        let mut ticks = 0;
        while head_row(&mut h) == landed {
            assert!(
                ticks < 100,
                "the cursor re-homes a second time on the slow crawl"
            );
            h.stoat.tick_scroll_anim(0.016);
            ticks += 1;
        }
        assert!(
            h.stoat.is_animating(),
            "the second re-home also lands mid-glide"
        );
        assert!(
            head_row(&mut h) > landed,
            "the second re-home advanced the cursor further down the band"
        );
    }

    fn pane_scroll_state(h: &mut crate::test_harness::TestHarness) -> (u32, f32) {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        (editor.scroll_row, editor.scroll_offset)
    }

    #[test]
    fn wheel_moves_file_finder_selection_not_the_pane() {
        use crate::test_harness::TestHarness;
        use stoat_action::OpenFileFinder;

        let mut h = TestHarness::with_size(80, 24);
        let root = std::path::PathBuf::from("/finder-wheel");
        for name in ["a.rs", "b.rs", "c.rs"] {
            h.fake_fs().insert_file(root.join(name), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();
        let before = pane_scroll_state(&mut h);

        h.stoat
            .update(mouse_event(MouseEventKind::ScrollDown, 10, 10));

        assert_eq!(
            h.stoat
                .file_finder
                .as_ref()
                .expect("finder open")
                .active_core_ref()
                .picklist
                .selected,
            1,
            "a wheel notch moves the finder selection down",
        );
        assert_eq!(
            pane_scroll_state(&mut h),
            before,
            "the pane beneath does not scroll",
        );
    }

    #[test]
    fn wheel_moves_palette_command_selection_not_the_pane() {
        use crate::test_harness::TestHarness;
        use stoat_action::OpenCommandPalette;

        let mut h = TestHarness::with_size(80, 24);
        let path = h.write_file("f.rs", "x\n");
        h.open_file(&path);
        action_handlers::dispatch(&mut h.stoat, &OpenCommandPalette);
        h.settle();
        let before = pane_scroll_state(&mut h);

        h.stoat
            .update(mouse_event(MouseEventKind::ScrollDown, 10, 10));

        assert_eq!(
            h.stoat
                .command_palette
                .as_ref()
                .expect("palette open")
                .selected,
            1,
            "a wheel notch moves the palette command selection down",
        );
        assert_eq!(
            pane_scroll_state(&mut h),
            before,
            "the pane beneath does not scroll"
        );
    }

    #[test]
    fn wheel_moves_palette_arg_picker_selection() {
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(80, 24);
        let root = std::path::PathBuf::from("/arg-wheel");
        for name in ["a.rs", "b.rs", "c.rs"] {
            h.fake_fs().insert_file(root.join(name), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;
        h.type_text(":o ");
        h.settle();

        h.stoat
            .update(mouse_event(MouseEventKind::ScrollDown, 10, 10));

        let selected = h
            .stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .arg_picker
            .as_ref()
            .expect("arg picker active")
            .core
            .picklist
            .selected;
        assert_eq!(
            selected, 1,
            "a wheel notch moves the arg picker selection down"
        );
    }

    /// Open a 40-line document, then a file finder over a four-entry workspace,
    /// and return the finder's list rect. The document beneath stays focused so
    /// callers can assert a swallowed click never disturbs its cursor.
    fn open_finder_with_four(h: &mut crate::test_harness::TestHarness) -> Rect {
        use stoat_action::OpenFileFinder;

        let doc = h.seed_long_file("under.rs", 40);
        h.open_file(&doc);

        let root = std::path::PathBuf::from("/click-finder");
        // Each file previews long enough to scroll, so a wheel over the preview
        // has an observable effect whichever entry is selected.
        let long: String = (0..80).map(|i| format!("line {i}\n")).collect();
        for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            h.fake_fs().insert_file(root.join(name), long.as_bytes());
        }
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();

        finder_layout(h).list
    }

    fn finder_selected(h: &crate::test_harness::TestHarness) -> usize {
        h.stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .active_core_ref()
            .picklist
            .selected
    }

    /// The box sizes to the whole candidate list, so narrowing the query must
    /// not resize it out from under the user still typing that query.
    #[test]
    fn the_finder_box_sizes_to_its_base_list_and_holds_while_filtering() {
        use stoat_action::OpenFileFinder;

        let mut h = crate::test_harness::TestHarness::with_size(140, 60);
        let root = std::path::PathBuf::from("/sized-finder");
        for i in 0..50 {
            h.fake_fs()
                .insert_file(root.join(format!("f{i}.rs")), b"fn main() {}");
        }
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();
        h.snapshot();

        let opened = finder_layout(&h).modal;
        assert_eq!(
            opened.height, 54,
            "fifty rows plus four chrome rows outgrow the recommended 32"
        );

        h.type_text("f1");
        h.settle();
        h.snapshot();

        assert!(
            finder_filtered_len(&h) < 50,
            "the filter has to actually narrow the list for the box to be held over anything"
        );
        assert_eq!(
            finder_layout(&h).modal,
            opened,
            "but the box stays exactly where it opened"
        );
    }

    fn finder_filtered_len(h: &crate::test_harness::TestHarness) -> usize {
        h.stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .active_core_ref()
            .picklist
            .filtered
            .len()
    }

    #[test]
    fn click_finder_row_moves_selection_not_the_pane() {
        use crossterm::event::MouseButton;

        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        let list = open_finder_with_four(&mut h);
        let before = h.stoat.focused_cursor_pos();

        // The third visible row is two below the list top.
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            list.x + 1,
            list.y + 2,
        ));

        assert_eq!(finder_selected(&h), 2, "clicking the third row selects it");
        assert_eq!(
            h.stoat.focused_cursor_pos(),
            before,
            "the click never reaches the buffer beneath",
        );
    }

    #[test]
    fn click_outside_modal_is_swallowed() {
        use crossterm::event::MouseButton;

        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        open_finder_with_four(&mut h);
        let before = h.stoat.focused_cursor_pos();

        // Row 0 sits above the centered modal.
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0));

        assert!(
            h.stoat.file_finder.is_some(),
            "an outside click does not dismiss the finder"
        );
        assert_eq!(finder_selected(&h), 0, "the selection is unchanged");
        assert_eq!(
            h.stoat.focused_cursor_pos(),
            before,
            "the buffer is untouched"
        );
    }

    #[test]
    fn click_empty_row_below_last_item_is_swallowed() {
        use crossterm::event::MouseButton;

        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        let list = open_finder_with_four(&mut h);

        // Only four items are listed, so the sixth row is empty.
        assert!(list.height > 5, "the list is tall enough for an empty row");
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            list.x + 1,
            list.y + 5,
        ));

        assert_eq!(
            finder_selected(&h),
            0,
            "a click on an empty row moves nothing"
        );
        assert!(
            h.stoat.file_finder.is_some(),
            "and does not dismiss the finder"
        );
    }

    fn finder_preview_id(h: &crate::test_harness::TestHarness) -> EditorId {
        h.stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .active_core_ref()
            .preview
            .editor
    }

    #[test]
    fn wheel_over_finder_preview_scrolls_it_not_the_list() {
        let mut h = crate::test_harness::TestHarness::with_size(100, 30);
        open_finder_with_four(&mut h);
        action_handlers::sync_file_finder_preview(&mut h.stoat);
        h.settle();

        let preview = finder_layout(&h)
            .preview
            .expect("the preview pane is present at this width");
        let preview_id = finder_preview_id(&h);

        h.stoat.update(mouse_event(
            MouseEventKind::ScrollDown,
            preview.x + preview.width / 2,
            preview.y + preview.height / 2,
        ));

        assert_eq!(
            finder_selected(&h),
            0,
            "a wheel over the preview leaves the list selection put"
        );
        let scroll_row = h
            .stoat
            .active_workspace()
            .editors
            .get(preview_id)
            .expect("preview editor")
            .scroll_row;
        assert!(scroll_row > 0, "the wheel scrolls the preview down");
    }

    #[test]
    fn wheel_over_palette_arg_preview_scrolls_it_not_the_list() {
        let mut h = crate::test_harness::TestHarness::with_size(100, 30);
        let root = std::path::PathBuf::from("/arg-preview");
        let long: String = (0..80).map(|i| format!("line {i}\n")).collect();
        h.fake_fs().insert_file(root.join("a.rs"), long.as_bytes());
        for name in ["b.rs", "c.rs"] {
            h.fake_fs().insert_file(root.join(name), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;
        h.type_text(":o ");
        h.settle();

        let (rows, zoom) = palette_sizing(&h);
        let preview = crate::render::command_palette::palette_arg_body(h.stoat.size(), rows, zoom)
            .and_then(|(_, preview)| preview)
            .expect("the arg preview pane is present at this width");
        let preview_id = h
            .stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .arg_picker
            .as_ref()
            .expect("arg picker active")
            .active_core_ref()
            .preview
            .editor;

        h.stoat.update(mouse_event(
            MouseEventKind::ScrollDown,
            preview.x + preview.width / 2,
            preview.y + preview.height / 2,
        ));

        let selected = h
            .stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .arg_picker
            .as_ref()
            .expect("arg picker active")
            .active_core_ref()
            .picklist
            .selected;
        assert_eq!(
            selected, 0,
            "a wheel over the preview leaves the arg selection put"
        );
        let scroll_row = h
            .stoat
            .active_workspace()
            .editors
            .get(preview_id)
            .expect("preview editor")
            .scroll_row;
        assert!(scroll_row > 0, "the wheel scrolls the preview down");
    }

    #[test]
    fn preview_scroll_resets_on_selection_change() {
        let mut h = crate::test_harness::TestHarness::with_size(100, 30);
        open_finder_with_four(&mut h);
        action_handlers::sync_file_finder_preview(&mut h.stoat);
        let preview_id = finder_preview_id(&h);

        {
            let editor = h
                .stoat
                .active_workspace_mut()
                .editors
                .get_mut(preview_id)
                .expect("preview editor");
            editor.scroll_offset = 5.0;
            editor.scroll_row = 5;
            editor.scroll_glide = ScrollGlide::Wheel;
        }

        action_handlers::file_finder_move_selection(&mut h.stoat, 1);
        action_handlers::sync_file_finder_preview(&mut h.stoat);

        let editor = h
            .stoat
            .active_workspace()
            .editors
            .get(preview_id)
            .expect("preview editor");
        assert_eq!(
            (editor.scroll_row, editor.scroll_offset, editor.scroll_glide,),
            (0, 0.0, ScrollGlide::None),
            "a new selection resets the preview scroll to the top",
        );
    }

    #[test]
    fn glide_tick_eases_offset_to_target_and_clears_glide() {
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let path = h.write_file("glide.rs", &body);
        h.open_file(&path);
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            editor.scroll_row = 10;
            editor.scroll_offset = 0.0;
            editor.scroll_glide = ScrollGlide::Page;
        }
        assert!(h.stoat.is_animating(), "a page glide animates");

        h.stoat.tick_scroll_anim(0.016);
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            assert!(
                editor.scroll_offset > 0.0 && editor.scroll_offset < 10.0,
                "tick eases the offset toward the target"
            );
            assert_eq!(editor.scroll_row, 10, "scroll_row stays the fixed target");
        }

        for _ in 0..1000 {
            if !h.stoat.is_animating() {
                break;
            }
            h.stoat.tick_scroll_anim(0.016);
        }
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::None,
            "the glide clears on settle"
        );
        assert_eq!(
            editor.scroll_offset, 10.0,
            "the offset settles on the target"
        );
    }

    #[test]
    fn glide_tick_snaps_a_gap_wider_than_three_viewports() {
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let path = h.write_file("glide.rs", &body);
        h.open_file(&path);
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            editor.scroll_row = 100;
            editor.scroll_offset = 0.0;
            editor.scroll_glide = ScrollGlide::Page;
        }

        h.stoat.tick_scroll_anim(0.016);

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert_eq!(
            editor.scroll_offset, 100.0,
            "a gap wider than three viewports snaps straight to the target"
        );
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::None,
            "and clears the glide"
        );
    }

    #[test]
    fn cold_build_shard_merges_into_the_workspace_graph() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;

        let workspace = stoat.active_workspace;
        let shard = codegraph::FileShard {
            content_hash: [0u8; 32],
            symbols: vec![codegraph::Symbol {
                key: codegraph::SymbolKey([1u8; 16]),
                file: codegraph::FileId(0),
                name: "foo".to_string(),
                kind: stoat_language::SymbolKind::Function,
                container: vec![],
                def_range: 0..11,
                name_range: 3..6,
                body_hash: [0u8; 32],
            }],
            edges: vec![],
        };
        stoat
            .index_update_tx
            .send(IndexUpdate::Shard {
                workspace,
                rel_path: "a.rs".to_string(),
                shard,
            })
            .unwrap();

        stoat.drain_index_updates();

        let ws = stoat.active_workspace();
        assert_eq!(ws.index_generation, 1);
        assert_eq!(
            ws.code_graph.symbol_at(codegraph::FileId(0), 5),
            Some(codegraph::SymbolKey([1u8; 16]))
        );
    }

    #[test]
    fn non_repo_root_skips_the_index_build() {
        use crate::host::{FakeFs, FakeGit};

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/scratch"),
        );
        stoat.persistence_disabled = true;

        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/scratch/a.rs", "fn foo() {}\n");
        stoat.set_fs_host(fs);
        stoat.set_git_host(Arc::new(FakeGit::new()));

        stoat.start_index_build();
        scheduler.run_until_parked();
        stoat.drain_index_updates();

        let ws = stoat.active_workspace();
        assert_eq!(
            ws.index_generation, 0,
            "a non-repo root builds no index shards",
        );
        assert_eq!(
            ws.code_graph
                .symbol_at(crate::code_index::build::file_id("a.rs"), 5),
            None,
            "no symbol is indexed when the workspace root is not a repo",
        );
    }

    #[test]
    fn index_build_watches_the_repo_root_off_the_render_thread() {
        use crate::host::{FakeFs, FakeFsWatcher, FakeGit};

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );

        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
        stoat.set_fs_host(fs);
        let git = FakeGit::new();
        git.add_repo("/repo");
        stoat.set_git_host(Arc::new(git));
        let watcher = Arc::new(FakeFsWatcher::new());
        stoat.set_fs_watch_host(watcher.clone());

        let git_root = stoat.active_workspace().git_root.clone();
        stoat.start_index_build();
        scheduler.run_until_parked();

        assert!(
            watcher.is_watching(&git_root),
            "the repo root is watched once the blocking registration runs"
        );
    }

    #[test]
    fn persistence_disabled_index_build_registers_no_watch() {
        use crate::host::{FakeFs, FakeFsWatcher, FakeGit};

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;

        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
        stoat.set_fs_host(fs);
        let git = FakeGit::new();
        git.add_repo("/repo");
        stoat.set_git_host(Arc::new(git));
        let watcher = Arc::new(FakeFsWatcher::new());
        stoat.set_fs_watch_host(watcher.clone());

        let git_root = stoat.active_workspace().git_root.clone();
        stoat.start_index_build();
        scheduler.run_until_parked();

        assert!(
            !watcher.is_watching(&git_root),
            "a persistence-disabled build registers no fs-watch"
        );
    }

    #[test]
    fn batched_reindex_drain_cross_links_like_sequential() {
        let file_a = codegraph::FileId(1);
        let file_b = codegraph::FileId(2);
        let caller = codegraph::SymbolKey([1u8; 16]);
        let callee = codegraph::SymbolKey([2u8; 16]);

        let callees_after = |drain_between: bool| -> Vec<codegraph::SymbolKey> {
            let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
            let mut stoat = Stoat::new(
                scheduler.executor(),
                Settings::default(),
                PathBuf::from("/repo"),
            );
            stoat.persistence_disabled = true;
            let workspace = stoat.active_workspace;

            let symbol = |key, file, name: &str| codegraph::Symbol {
                key,
                file,
                name: name.to_string(),
                kind: stoat_language::SymbolKind::Function,
                container: vec![],
                def_range: 0..10,
                name_range: 3..6,
                body_hash: [0u8; 32],
            };
            let reindex = |file, rel_path: &str, symbols, edges| IndexUpdate::Reindex {
                workspace,
                file,
                rel_path: rel_path.to_string(),
                shard: codegraph::FileShard {
                    content_hash: [0u8; 32],
                    symbols,
                    edges,
                },
                persist: false,
            };

            stoat
                .index_update_tx
                .send(reindex(
                    file_a,
                    "a.rs",
                    vec![symbol(caller, file_a, "caller")],
                    vec![codegraph::Edge {
                        from: caller,
                        to: codegraph::Target::Unresolved {
                            name: "callee".to_string(),
                            kind: stoat_language::RefKind::Call,
                        },
                        kind: codegraph::EdgeKind::Calls,
                        site_range: 0..6,
                        confidence: codegraph::Confidence::NameMatch,
                    }],
                ))
                .unwrap();
            if drain_between {
                stoat.drain_index_updates();
            }
            stoat
                .index_update_tx
                .send(reindex(
                    file_b,
                    "b.rs",
                    vec![symbol(callee, file_b, "callee")],
                    vec![],
                ))
                .unwrap();
            stoat.drain_index_updates();

            stoat.active_workspace().code_graph.step(
                caller,
                codegraph::EdgeKind::Calls,
                codegraph::Dir::Down,
            )
        };

        assert_eq!(
            callees_after(false),
            vec![callee],
            "one batched drain resolves file A's call to file B's definition",
        );
        assert_eq!(
            callees_after(true),
            callees_after(false),
            "batching two reindexes into one drain matches draining them one at a time",
        );
    }

    #[test]
    fn a_capped_drain_leaves_the_remainder_for_the_next_tick() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;
        let workspace = stoat.active_workspace;

        let total = INDEX_DRAIN_CAP + 1;
        for i in 0..total {
            stoat
                .index_update_tx
                .send(IndexUpdate::Shard {
                    workspace,
                    rel_path: format!("f{i}.rs"),
                    shard: codegraph::FileShard {
                        content_hash: [0u8; 32],
                        symbols: vec![],
                        edges: vec![],
                    },
                })
                .unwrap();
        }

        stoat.drain_index_updates();
        assert_eq!(
            stoat.active_workspace().index_generation,
            INDEX_DRAIN_CAP as u64,
            "the drain caps its work and leaves the remainder queued",
        );

        stoat.drain_index_updates();
        assert_eq!(
            stoat.active_workspace().index_generation,
            total as u64,
            "the next drain completes the queued remainder",
        );
    }

    #[test]
    fn reindex_replaces_a_files_symbols_in_the_graph() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;
        let workspace = stoat.active_workspace;
        let file = codegraph::FileId(7);

        let symbol = |key: u8, name: &str| codegraph::Symbol {
            key: codegraph::SymbolKey([key; 16]),
            file,
            name: name.to_string(),
            kind: stoat_language::SymbolKind::Function,
            container: vec![],
            def_range: 0..11,
            name_range: 3..6,
            body_hash: [0u8; 32],
        };

        stoat
            .index_update_tx
            .send(IndexUpdate::Shard {
                workspace,
                rel_path: "a.rs".to_string(),
                shard: codegraph::FileShard {
                    content_hash: [0u8; 32],
                    symbols: vec![symbol(1, "foo")],
                    edges: vec![],
                },
            })
            .unwrap();
        stoat.drain_index_updates();
        assert_eq!(
            stoat.active_workspace().code_graph.symbol_at(file, 5),
            Some(codegraph::SymbolKey([1u8; 16]))
        );

        stoat
            .index_update_tx
            .send(IndexUpdate::Reindex {
                workspace,
                file,
                rel_path: "a.rs".to_string(),
                shard: codegraph::FileShard {
                    content_hash: [9u8; 32],
                    symbols: vec![symbol(2, "bar")],
                    edges: vec![],
                },
                persist: false,
            })
            .unwrap();
        stoat.drain_index_updates();

        let ws = stoat.active_workspace();
        assert_eq!(
            ws.code_graph.symbol_at(file, 5),
            Some(codegraph::SymbolKey([2u8; 16])),
            "reindex evicts the old symbol and inserts the new one"
        );
        assert_eq!(ws.index_generation, 2);
    }

    #[test]
    fn external_change_reindexes_and_remove_evicts() {
        use crate::host::{FakeFs, FakeFsWatcher, FsEventKind};

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;

        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
        stoat.set_fs_host(fs.clone());
        let watcher = Arc::new(FakeFsWatcher::new());
        stoat.set_fs_watch_host(watcher.clone());

        let path = PathBuf::from("/repo/src/a.rs");
        let file = crate::code_index::build::file_id("src/a.rs");

        let drive = |stoat: &mut Stoat, kind: FsEventKind| {
            watcher.inject(&path, kind);
            debounce::drain_fs_watch_events(stoat);
            scheduler.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);
            debounce::drain_pending_index_edits(stoat);
            scheduler.run_until_parked();
            stoat.drain_index_updates();
        };

        drive(&mut stoat, FsEventKind::Modified);
        assert!(
            stoat
                .active_workspace()
                .code_graph
                .symbol_at(file, 4)
                .is_some(),
            "an external modify indexes the file",
        );

        fs.remove_file(&path).unwrap();
        drive(&mut stoat, FsEventKind::Removed);
        assert_eq!(
            stoat.active_workspace().code_graph.symbol_at(file, 4),
            None,
            "an external remove evicts the file",
        );
    }

    /// Watches are registered per directory, so which directories get one is
    /// the whole question. A recursive watch on the root covered `target/`,
    /// `node_modules/`, and the object store, which on a built repo is most of
    /// the tree and enough to exhaust the platform's watch limit.
    ///
    /// Stated as the watch set rather than as events, since whether a write
    /// under an unwatched directory reports anything is the platform's
    /// behavior rather than this code's.
    #[test]
    fn workspace_watches_cover_the_source_tree_and_the_git_refs() {
        use crate::host::{FakeFs, FakeFsWatcher};

        let fs = FakeFs::new();
        fs.insert_files([
            ("/repo/src/a.rs", "fn a() {}".as_bytes()),
            ("/repo/src/deep/b.rs", "fn b() {}".as_bytes()),
            ("/repo/target/debug/gen.rs", "gen".as_bytes()),
            ("/repo/node_modules/pkg/i.js", "js".as_bytes()),
            ("/repo/.git/refs/heads/main", "sha".as_bytes()),
            ("/repo/.git/objects/ab/cdef", "obj".as_bytes()),
        ]);
        let watcher = FakeFsWatcher::new();

        watch_workspace_dirs(&fs, &watcher, Path::new("/repo"));

        assert_eq!(
            watcher.watched_paths(),
            [
                PathBuf::from("/repo"),
                PathBuf::from("/repo/.git"),
                PathBuf::from("/repo/.git/refs"),
                PathBuf::from("/repo/.git/refs/heads"),
                PathBuf::from("/repo/src"),
                PathBuf::from("/repo/src/deep"),
            ],
            "the source tree plus the three .git directories that carry HEAD, \
             the index, and the branch tips, and nothing from a built tree or \
             the object store",
        );
    }

    /// A directory created after startup has no watch of its own, which leaves
    /// files written into it untracked for the rest of the session. Its create
    /// event is the only notice the editor gets.
    #[test]
    fn a_directory_created_after_startup_gets_its_own_watch() {
        use crate::host::{FakeFs, FakeFsWatcher, FsEventKind};

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;

        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn a() {}\n");
        stoat.set_fs_host(fs.clone());
        let watcher = Arc::new(FakeFsWatcher::new());
        stoat.set_fs_watch_host(watcher.clone());

        let fresh = PathBuf::from("/repo/src/fresh");
        fs.insert_dir(&fresh);
        watcher.inject(&fresh, FsEventKind::Created);
        debounce::drain_fs_watch_events(&mut stoat);
        assert!(
            watcher.is_watching(&fresh),
            "the new directory is watched from its create event",
        );

        let file = PathBuf::from("/repo/src/a.rs");
        watcher.inject(&file, FsEventKind::Created);
        debounce::drain_fs_watch_events(&mut stoat);
        assert!(
            !watcher.is_watching(&file),
            "a created file needs no watch of its own, its directory has one",
        );
    }

    /// One external change reads the file once. Deciding whether the change is
    /// stale means reading it, and extracting means reading it, so a gate
    /// standing outside the job read every changed file twice. A checkout or a
    /// formatter run pays that per file.
    ///
    /// The gate moving into the job is also what takes the read off the run
    /// loop, but the test scheduler runs blocking work inline, so the count is
    /// what is observable here rather than which thread it happened on.
    #[test]
    fn an_external_change_reads_the_file_once() {
        use crate::host::{FakeFs, FakeFsWatcher, FsEventKind};

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;

        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
        stoat.set_fs_host(fs.clone());
        let watcher = Arc::new(FakeFsWatcher::new());
        stoat.set_fs_watch_host(watcher.clone());

        let path = PathBuf::from("/repo/src/a.rs");
        let reads = || {
            fs.ops()
                .iter()
                .filter(|op| matches!(op, crate::host::FakeFsOp::Read { path: p } if *p == path))
                .count()
        };
        let drive = |stoat: &mut Stoat| {
            watcher.inject(&path, FsEventKind::Modified);
            debounce::drain_fs_watch_events(stoat);
            scheduler.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);
            debounce::drain_pending_index_edits(stoat);
            scheduler.run_until_parked();
            stoat.drain_index_updates();
        };

        // Index it once, so the rounds below have a recorded hash to differ
        // from and to match.
        drive(&mut stoat);
        let file = crate::code_index::build::file_id("src/a.rs");
        assert!(
            stoat
                .active_workspace()
                .code_graph
                .content_hash(file)
                .is_some(),
            "the first round indexed the file",
        );

        let before = reads();
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\nfn bar() {}\n");
        drive(&mut stoat);
        assert_eq!(
            reads(),
            before + 1,
            "a changed file is fingerprinted and extracted from one read",
        );
        assert!(
            stoat
                .active_workspace()
                .code_graph
                .symbol_at(file, 17)
                .is_some(),
            "and the change reached the graph, so the single read did both jobs",
        );

        // An event for a file nothing touched, which is what the watch echo of
        // the editor's own save looks like.
        let before = reads();
        let generation = stoat.active_workspace().index_generation;
        drive(&mut stoat);
        assert_eq!(
            reads(),
            before + 1,
            "an unchanged file is read once to find that out",
        );
        assert_eq!(
            stoat.active_workspace().index_generation,
            generation,
            "and the matching fingerprint stops it before it reindexes",
        );
    }

    /// One window covers a burst. A checkout naming thousands of files must not
    /// allocate a timer for each, so the second path here joins the window the
    /// first opened rather than starting its own.
    ///
    /// The clock is what shows it. Both paths drain one window after the burst
    /// started, a point at which a timer per path leaves the later path's own
    /// window still open.
    #[test]
    fn a_burst_of_external_changes_shares_one_debounce_window() {
        use crate::host::{FakeFs, FakeFsWatcher, FsEventKind};

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;

        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
        fs.insert_file("/repo/src/b.rs", "fn bar() {}\n");
        stoat.set_fs_host(fs);
        let watcher = Arc::new(FakeFsWatcher::new());
        stoat.set_fs_watch_host(watcher.clone());

        let (a, b) = (
            PathBuf::from("/repo/src/a.rs"),
            PathBuf::from("/repo/src/b.rs"),
        );

        watcher.inject(&a, FsEventKind::Modified);
        debounce::drain_fs_watch_events(&mut stoat);

        // Most of the window elapses, then the second path arrives inside it.
        scheduler.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE / 2);
        watcher.inject(&b, FsEventKind::Modified);
        debounce::drain_fs_watch_events(&mut stoat);
        assert_eq!(
            stoat.index_pending_external_edits.len(),
            2,
            "both paths wait on the same window",
        );

        scheduler.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);
        assert!(
            debounce::drain_pending_index_edits(&mut stoat),
            "the window the first path opened closed and carried both",
        );
        assert!(
            stoat.index_pending_external_edits.is_empty(),
            "one drain took the whole burst",
        );

        scheduler.run_until_parked();
        stoat.drain_index_updates();
        let graph = &stoat.active_workspace().code_graph;
        assert!(
            graph
                .content_hash(crate::code_index::build::file_id("src/a.rs"))
                .is_some()
                && graph
                    .content_hash(crate::code_index::build::file_id("src/b.rs"))
                    .is_some(),
            "both files of the burst reached the graph",
        );
    }

    #[test]
    fn editing_a_buffer_live_reindexes_a_new_calls_edge() {
        use crate::host::FakeFs;

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn caller() {}\nfn callee() {}\n");
        stoat.set_fs_host(fs);

        let pane = stoat.active_workspace().panes.focus();
        let buffer_id =
            action_handlers::file::open_file_in_pane(&mut stoat, pane, Path::new("/repo/src/a.rs"))
                .expect("open the buffer");

        // A parse arms the index debounce rather than extracting, so the
        // extract lands on a later pass once the buffer has gone quiet. The
        // first two drives spawn the parse and poll its output in, since it is
        // the poll that arms the debounce and the clock has to advance after
        // the window opens rather than before.
        let drive = |stoat: &mut Stoat| {
            stoat.drive_parse_jobs();
            scheduler.run_until_parked();
            stoat.drive_parse_jobs();
            scheduler.run_until_parked();
            scheduler.advance_clock(INDEX_EDIT_DEBOUNCE);
            scheduler.run_until_parked();
            stoat.drive_parse_jobs();
            scheduler.run_until_parked();
            stoat.drain_index_updates();
        };

        drive(&mut stoat);
        let file = crate::code_index::build::file_id("src/a.rs");
        let caller = stoat
            .active_workspace()
            .code_graph
            .symbol_at(file, 5)
            .expect("caller indexed");
        assert!(
            stoat
                .active_workspace()
                .code_graph
                .step(caller, codegraph::EdgeKind::Calls, codegraph::Dir::Down)
                .is_empty(),
            "caller has no callee edge before the edit",
        );

        {
            let ws = stoat.active_workspace();
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            buffer.write().expect("poisoned").edit(13..13, "callee();");
        }

        drive(&mut stoat);
        let ws = stoat.active_workspace();
        let caller = ws.code_graph.symbol_at(file, 5).expect("caller reindexed");
        let callee = ws.code_graph.symbol_at(file, 27).expect("callee reindexed");
        assert_eq!(
            ws.code_graph
                .step(caller, codegraph::EdgeKind::Calls, codegraph::Dir::Down),
            vec![callee],
            "the edit's new call appears as a Calls edge in the graph",
        );
    }

    /// A save is the moment the file on disk matches the buffer, so it is the
    /// save's reindex that writes the shard a later open warm-loads. An edit's
    /// reindex must not write one, since the disk still disagrees with the
    /// buffer it extracted from.
    ///
    /// Read off the update channel rather than off the index directory. The
    /// drain resolves that directory through the XDG state dir, and a test
    /// whose result turns on the environment resolving one is not repeatable.
    #[test]
    fn a_save_reindexes_with_persist_where_an_edit_does_not() {
        use crate::host::FakeFs;
        use stoat_action::SaveBuffer;

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn caller() {}\n");
        stoat.set_fs_host(fs);

        let pane = stoat.active_workspace().panes.focus();
        let buffer_id =
            action_handlers::file::open_file_in_pane(&mut stoat, pane, Path::new("/repo/src/a.rs"))
                .expect("open the buffer");

        // Nothing drains here, so the updates queue up and each phase reads the
        // ones it produced. A drain resolves the index directory, which is the
        // environment dependency this test exists without.
        let persists = |stoat: &mut Stoat| {
            let mut seen = Vec::new();
            while let Ok(update) = stoat.index_update_rx.try_recv() {
                if let IndexUpdate::Reindex { persist, .. } = update {
                    seen.push(persist);
                }
            }
            seen
        };
        let settle = |stoat: &mut Stoat| {
            stoat.drive_parse_jobs();
            scheduler.run_until_parked();
            stoat.drive_parse_jobs();
            scheduler.run_until_parked();
            scheduler.advance_clock(INDEX_EDIT_DEBOUNCE);
            scheduler.run_until_parked();
            stoat.drive_parse_jobs();
            scheduler.run_until_parked();
        };

        {
            let ws = stoat.active_workspace();
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            buffer
                .write()
                .expect("poisoned")
                .edit(14..14, "\nfn one() {}");
        }
        settle(&mut stoat);
        assert_eq!(
            persists(&mut stoat),
            [false],
            "the edit's reindex updates the graph and writes nothing"
        );

        assert_eq!(
            action_handlers::dispatch(&mut stoat, &SaveBuffer),
            UpdateEffect::Redraw,
            "the save reached a handler rather than falling through"
        );
        scheduler.run_until_parked();
        // A refused or failed write returns before the enqueue, so a reindex
        // arriving at all is what says the bytes reached disk first.
        assert_eq!(
            persists(&mut stoat),
            [true],
            "the save's reindex carries the shard and manifest entry to disk"
        );
    }

    #[test]
    fn two_parses_inside_the_debounce_window_extract_once() {
        use crate::host::FakeFs;

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn caller() {}\n");
        stoat.set_fs_host(fs);

        let pane = stoat.active_workspace().panes.focus();
        let buffer_id =
            action_handlers::file::open_file_in_pane(&mut stoat, pane, Path::new("/repo/src/a.rs"))
                .expect("open the buffer");

        let parse = |stoat: &mut Stoat| {
            stoat.drive_parse_jobs();
            scheduler.run_until_parked();
        };
        let settle = |stoat: &mut Stoat| {
            scheduler.advance_clock(INDEX_EDIT_DEBOUNCE);
            scheduler.run_until_parked();
            stoat.drive_parse_jobs();
            scheduler.run_until_parked();
            stoat.drain_index_updates();
        };
        let edit = |stoat: &mut Stoat, text: &str| {
            let ws = stoat.active_workspace();
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            buffer.write().expect("poisoned").edit(14..14, text);
        };

        parse(&mut stoat);
        settle(&mut stoat);
        let before = stoat.active_workspace().index_generation;

        edit(&mut stoat, "\nfn one() {}");
        parse(&mut stoat);
        edit(&mut stoat, "\nfn two() {}");
        parse(&mut stoat);

        assert_eq!(
            stoat.active_workspace().index_generation,
            before,
            "neither parse extracts while the buffer is still being typed in",
        );

        settle(&mut stoat);
        assert_eq!(
            stoat.active_workspace().index_generation,
            before + 1,
            "the two parses collapse into a single extract",
        );

        let file = crate::code_index::build::file_id("src/a.rs");
        let ws = stoat.active_workspace();
        assert!(
            ws.code_graph.symbol_at(file, 19).is_some()
                && ws.code_graph.symbol_at(file, 31).is_some(),
            "the one extract sees both edits, having read the rope when it fired",
        );
    }

    #[test]
    fn agent_output_feeds_emulator() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());

        let session: Arc<dyn crate::host::TerminalSession> =
            Arc::new(crate::host::FakeTerminalSession::new());
        let agent_id = stoat.active_workspace_mut().terms.insert(TermSession::new(
            crate::term_screen::TermScreen::new(24, 80),
            session,
        ));
        // Show the agent in the focused pane so its output marks the frame dirty.
        let pane = stoat.active_workspace().panes.focus();
        stoat.active_workspace_mut().panes.pane_mut(pane).view = View::Agent(agent_id);

        let effect = stoat.handle_pty_notification(PtyNotification::TermOutput {
            agent_id,
            data: b"hello".to_vec(),
        });

        assert_eq!(
            effect,
            UpdateEffect::None,
            "a visible agent paces its repaint to the frame tick",
        );
        assert!(stoat.pty_dirty, "the output marked the frame dirty");
        let term = &stoat.active_workspace().terms[agent_id].term;
        let row: String = term.row(0).iter().map(|cell| cell.ch).collect();
        assert!(row.starts_with("hello"), "row: {row:?}");
    }

    #[test]
    fn term_pane_osc52_forwards_to_clipboard() {
        let mut h = Stoat::test();
        let session: Arc<dyn crate::host::TerminalSession> =
            Arc::new(crate::host::FakeTerminalSession::new());
        let agent_id = h
            .stoat
            .active_workspace_mut()
            .terms
            .insert(TermSession::new(
                crate::term_screen::TermScreen::new(24, 80),
                session,
            ));

        // OSC 52 set-clipboard with the base64 of "hi", BEL-terminated.
        h.stoat
            .handle_pty_notification(PtyNotification::TermOutput {
                agent_id,
                data: b"\x1b]52;c;aGk=\x07".to_vec(),
            });

        assert_eq!(
            h.fake_clipboard().writes(),
            vec!["hi".to_string()],
            "an OSC 52 write from a term pane reaches the system clipboard"
        );
    }

    #[test]
    fn async_session_restore_installs_into_a_fresh_workspace() {
        let mut h = Stoat::test();
        let file = h.write_file("restored.txt", "alpha\nbeta\n");
        h.open_file(&file);
        h.settle();

        let state_path = PathBuf::from("/state/session.ron");
        h.stoat
            .active_workspace()
            .save_state(&state_path, &*h.stoat.fs_host)
            .expect("save state");

        let target = h.create_workspace();
        h.set_active_workspace(target);
        assert!(h.stoat.active_workspace().is_fresh());

        h.stoat.spawn_workspace_restore(target, state_path);
        h.settle();
        h.stoat.drive_background();

        let restored: Vec<PathBuf> = {
            let ws = h.stoat.active_workspace();
            ws.editors
                .values()
                .filter_map(|e| ws.buffers.path_for(e.buffer_id).map(|p| p.to_path_buf()))
                .collect()
        };
        assert!(
            restored.contains(&file),
            "the restore installs the saved file buffer: {restored:?}"
        );
        assert!(
            h.stoat
                .active_workspace()
                .badges
                .find_by_source(crate::badge::BadgeSource::SessionRestore)
                .is_none(),
            "the restoring-session badge clears after the restore installs"
        );
    }

    #[test]
    fn async_session_restore_drops_when_the_target_was_edited() {
        let mut h = Stoat::test();
        let file = h.write_file("restored.txt", "alpha\n");
        h.open_file(&file);
        h.settle();

        let state_path = PathBuf::from("/state/session.ron");
        h.stoat
            .active_workspace()
            .save_state(&state_path, &*h.stoat.fs_host)
            .expect("save state");

        let target = h.create_workspace();
        h.set_active_workspace(target);
        let other = h.write_file("other.txt", "live\n");
        h.open_file(&other);
        assert!(!h.stoat.active_workspace().is_fresh());

        h.stoat.spawn_workspace_restore(target, state_path);
        h.settle();
        h.stoat.drive_background();

        let paths: Vec<PathBuf> = {
            let ws = h.stoat.active_workspace();
            ws.editors
                .values()
                .filter_map(|e| ws.buffers.path_for(e.buffer_id).map(|p| p.to_path_buf()))
                .collect()
        };
        assert!(paths.contains(&other), "keeps the live buffer: {paths:?}");
        assert!(
            !paths.contains(&file),
            "does not clobber the live workspace with the saved restore"
        );
        assert!(
            h.stoat
                .active_workspace()
                .badges
                .find_by_source(crate::badge::BadgeSource::SessionRestore)
                .is_none(),
            "the badge clears even when the restore is dropped"
        );
    }

    #[test]
    fn term_query_reply_writes_back_to_pty() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());

        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
        let agent_id = stoat.active_workspace_mut().terms.insert(TermSession::new(
            crate::term_screen::TermScreen::new(24, 80),
            session,
        ));

        // A DSR cursor-position query in the PTY output must be answered back
        // to the PTY. A fresh screen reports the cursor at row 1, column 1.
        stoat.handle_pty_notification(PtyNotification::TermOutput {
            agent_id,
            data: b"\x1b[6n".to_vec(),
        });

        assert_eq!(fake.sent_bytes(), vec![b"\x1b[1;1R".to_vec()]);
    }

    #[test]
    fn layout_fits_agent_emulator_and_pty_to_pane() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        ws.panes.split(crate::pane::Axis::Vertical);
        let focused = ws.panes.focus();

        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
        let agent_id = ws.terms.insert(TermSession::new(
            crate::term_screen::TermScreen::new(24, 80),
            session,
        ));
        ws.panes.pane_mut(focused).view = View::Agent(agent_id);

        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        let ws = h.stoat.active_workspace();
        let (content, _) = crate::render::layout::split_pane_status(ws.panes.pane(focused).area);
        let term = &ws.terms[agent_id].term;
        assert_eq!(
            (term.rows(), term.cols()),
            (content.height as usize, content.width as usize),
            "emulator fits the pane content area",
        );
        assert_eq!(
            fake.last_size(),
            Some((content.height, content.width)),
            "pty resized to the pane content area",
        );
    }

    #[test]
    fn closing_term_pane_kills_pty_child() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        ws.panes.split(crate::pane::Axis::Vertical);
        let focused = ws.panes.focus();

        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
        let agent_id = ws.terms.insert(TermSession::new(
            crate::term_screen::TermScreen::new(24, 80),
            session,
        ));
        ws.panes.pane_mut(focused).view = View::Agent(agent_id);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::ClosePane);
        h.settle();

        assert!(
            fake.was_killed(),
            "closing the agent pane kills its PTY child"
        );
        assert!(
            !h.stoat.active_workspace().terms.contains_key(agent_id),
            "closing the agent pane drops its session",
        );
    }

    #[test]
    fn closing_terminal_pane_kills_pty_child() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        ws.panes.split(crate::pane::Axis::Vertical);
        let focused = ws.panes.focus();

        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
        let term_id = ws.terms.insert(TermSession::new(
            crate::term_screen::TermScreen::new(24, 80),
            session,
        ));
        ws.panes.pane_mut(focused).view = View::Terminal(term_id);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::ClosePane);
        h.settle();

        assert!(
            fake.was_killed(),
            "closing the terminal pane kills its PTY child"
        );
        assert!(
            !h.stoat.active_workspace().terms.contains_key(term_id),
            "closing the terminal pane drops its session",
        );
    }

    fn insert_term_session(ws: &mut Workspace) -> TermId {
        let session: Arc<dyn crate::host::TerminalSession> =
            Arc::new(crate::host::FakeTerminalSession::new());
        ws.terms.insert(TermSession::new(
            crate::term_screen::TermScreen::new(24, 80),
            session,
        ))
    }

    #[test]
    fn terminal_pane_closes_when_shell_exits() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        let editor_pane = ws.panes.focus();
        let term_pane = ws.panes.split(crate::pane::Axis::Vertical);
        let term_id = insert_term_session(ws);
        ws.panes.pane_mut(term_pane).view = View::Terminal(term_id);
        h.stoat.transition_mode("insert".to_string());

        let effect = h
            .stoat
            .handle_pty_notification(PtyNotification::TermExited { term_id });

        assert_eq!(effect, UpdateEffect::Redraw);
        let ws = h.stoat.active_workspace();
        assert!(!ws.terms.contains_key(term_id), "session dropped on exit");
        assert_eq!(
            ws.panes.split_pane_ids(),
            vec![editor_pane],
            "terminal pane closed, editor remains",
        );
        assert_eq!(ws.panes.focus(), editor_pane, "focus moved to the sibling");
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "focused terminal exit leaves insert mode",
        );
    }

    #[test]
    fn last_terminal_pane_restores_scratch_when_no_prev_view() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        let only_pane = ws.panes.focus();
        let term_id = insert_term_session(ws);
        ws.panes.pane_mut(only_pane).view = View::Terminal(term_id);
        h.stoat.transition_mode("insert".to_string());

        h.stoat
            .handle_pty_notification(PtyNotification::TermExited { term_id });

        let ws = h.stoat.active_workspace();
        assert!(!ws.terms.contains_key(term_id), "session dropped on exit");
        assert_eq!(
            ws.panes.split_pane_ids(),
            vec![only_pane],
            "the last split pane is not closed",
        );
        let View::Editor(editor_id) = ws.panes.pane(only_pane).view else {
            panic!("last pane restores a scratch editor with no prev view");
        };
        let buffer_id = ws.editors.get(editor_id).expect("editor is live").buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("scratch buffer is live");
        assert_eq!(
            buffer.read().expect("buffer lock").rope().to_string(),
            "\n",
            "restored scratch buffer holds the seeded newline",
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "focused terminal exit leaves insert mode",
        );
    }

    #[test]
    fn last_terminal_pane_restores_previous_view_on_exit() {
        let mut h = Stoat::test();
        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        h.stoat.terminal_host = Arc::new(crate::host::FakeTerminalHost::new(fake));
        h.allow_host_swap();

        let pane = h.stoat.active_workspace().panes.focus();
        let View::Editor(original) = h.stoat.active_workspace().panes.pane(pane).view else {
            panic!("initial pane holds an editor");
        };

        action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);

        let View::Terminal(term_id) = h.stoat.active_workspace().panes.pane(pane).view else {
            panic!("terminal action points the pane at a terminal");
        };
        h.stoat.transition_mode("insert".to_string());

        h.stoat
            .handle_pty_notification(PtyNotification::TermExited { term_id });

        let ws = h.stoat.active_workspace();
        let View::Editor(restored) = ws.panes.pane(pane).view else {
            panic!("exited terminal restores the previous editor view");
        };
        assert_eq!(
            restored, original,
            "pane restored to its pre-terminal editor"
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "focused terminal exit leaves insert mode",
        );
    }

    #[test]
    fn last_terminal_pane_falls_back_to_scratch_when_prev_view_dangles() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        let only_pane = ws.panes.focus();
        let View::Editor(stale) = ws.panes.pane(only_pane).view else {
            panic!("initial pane holds an editor");
        };
        let term_id = insert_term_session(ws);
        let pane = ws.panes.pane_mut(only_pane);
        pane.prev_view = Some(View::Editor(stale));
        pane.view = View::Terminal(term_id);
        ws.editors.remove(stale);
        h.stoat.transition_mode("insert".to_string());

        h.stoat
            .handle_pty_notification(PtyNotification::TermExited { term_id });

        let ws = h.stoat.active_workspace();
        let View::Editor(restored) = ws.panes.pane(only_pane).view else {
            panic!("dangling prev view falls back to a scratch editor");
        };
        assert_ne!(
            restored, stale,
            "fell back to a fresh editor, not the dead one"
        );
        assert!(ws.editors.contains_key(restored), "scratch editor is live");
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "focused terminal exit leaves insert mode",
        );
    }

    #[test]
    fn terminal_exit_keeps_insert_mode_when_pane_not_focused() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        let editor_pane = ws.panes.focus();
        let term_pane = ws.panes.split(crate::pane::Axis::Vertical);
        let term_id = insert_term_session(ws);
        ws.panes.pane_mut(term_pane).view = View::Terminal(term_id);
        ws.panes.set_focus(editor_pane);
        h.stoat.transition_mode("insert".to_string());

        h.stoat
            .handle_pty_notification(PtyNotification::TermExited { term_id });

        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "an unfocused terminal exit leaves the mode untouched",
        );
    }

    #[test]
    fn agent_pane_survives_shell_exit() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        let only_pane = ws.panes.focus();
        let term_id = insert_term_session(ws);
        ws.panes.pane_mut(only_pane).view = View::Agent(term_id);

        h.stoat
            .handle_pty_notification(PtyNotification::TermExited { term_id });

        let ws = h.stoat.active_workspace();
        assert!(
            ws.terms.contains_key(term_id),
            "agent session retained on exit",
        );
        assert!(
            matches!(ws.panes.pane(only_pane).view, View::Agent(id) if id == term_id),
            "agent pane view unchanged",
        );
    }

    #[test]
    fn hidden_term_output_advances_state_without_a_repaint() {
        use crate::pane::{DockPanel, DockSide, DockVisibility};

        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        let term_id = insert_term_session(ws);
        // Only a hidden dock shows the term, so no visible surface has it.
        ws.docks.insert(DockPanel {
            view: View::Terminal(term_id),
            side: DockSide::Right,
            visibility: DockVisibility::Hidden,
            default_width: 30,
            area: Rect::new(0, 0, 0, 0),
        });

        let effect = h
            .stoat
            .handle_pty_notification(PtyNotification::TermOutput {
                agent_id: term_id,
                data: b"abc".to_vec(),
            });
        assert_eq!(
            effect,
            UpdateEffect::None,
            "a hidden term drives no repaint"
        );

        let cursor = h
            .stoat
            .active_workspace()
            .terms
            .get(term_id)
            .expect("term session")
            .term
            .cursor();
        assert_eq!(
            cursor.map(|c| c.col),
            Some(3),
            "the term still fed its bytes while hidden",
        );
    }

    #[test]
    fn visible_term_output_paces_a_repaint_to_the_tick() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        let pane = ws.panes.focus();
        let term_id = insert_term_session(ws);
        ws.panes.pane_mut(pane).view = View::Terminal(term_id);

        // Rapid bursts each mark the frame dirty and repaint nothing on their own.
        for _ in 0..2 {
            let effect = h
                .stoat
                .handle_pty_notification(PtyNotification::TermOutput {
                    agent_id: term_id,
                    data: b"x".to_vec(),
                });
            assert_eq!(
                effect,
                UpdateEffect::None,
                "a visible term does not repaint per PTY chunk",
            );
        }
        assert!(h.stoat.pty_dirty, "the bursts marked the frame dirty");

        // The next tick coalesces them into one repaint and clears the flag.
        assert_eq!(
            h.stoat.frame_tick(0.016),
            UpdateEffect::Redraw,
            "the frame tick paints the accumulated output once",
        );
        assert!(!h.stoat.pty_dirty, "the tick cleared the dirty flag");
    }

    #[test]
    fn an_idle_frame_tick_repaints_nothing() {
        let mut h = Stoat::test();
        assert_eq!(
            h.stoat.frame_tick(0.016),
            UpdateEffect::None,
            "a tick with no glide, build, or pty output repaints nothing",
        );
    }

    fn stoat_with_focused_term(
        make_view: fn(TermId) -> View,
    ) -> (Stoat, TermId, Arc<crate::host::FakeTerminalSession>) {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());

        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let term_id = ws.terms.insert(TermSession::new(
            crate::term_screen::TermScreen::new(24, 80),
            session,
        ));
        ws.panes.pane_mut(focused).view = make_view(term_id);
        stoat.set_focused_mode("insert".to_string());
        (stoat, term_id, fake)
    }

    fn stoat_with_focused_agent() -> (Stoat, TermId, Arc<crate::host::FakeTerminalSession>) {
        stoat_with_focused_term(View::Agent)
    }

    fn bare(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn compile_keymap(src: &str) -> Keymap {
        let (config, errors) = stoat_config::parse(src);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        Keymap::compile(&config.expect("config"))
    }

    #[test]
    fn set_var_gates_a_binding() {
        let mut h = Stoat::test();
        h.stoat.keymap = compile_keymap(
            r#"on key {
                x -> SetVar(sidebar, on);
                sidebar == "on" { j -> SetVar(pressed, yes); }
            }"#,
        );
        // The hints cache is keyed on state under a fixed keymap, so a new
        // keymap invalidates it.
        h.stoat.hints_cache = None;

        // `j` is inert until `x` sets the variable.
        h.stoat.handle_key(bare(KeyCode::Char('j')));
        assert!(!h.stoat.user_vars.contains_key("pressed"));

        h.stoat.handle_key(bare(KeyCode::Char('x')));
        h.stoat.handle_key(bare(KeyCode::Char('j')));
        assert_eq!(
            h.stoat.user_vars.get("pressed"),
            Some(&StateValue::String("yes".into()))
        );
    }

    #[test]
    fn set_var_collision_with_builtin_is_ignored() {
        let mut h = Stoat::test();
        h.stoat.keymap = compile_keymap("on key { x -> SetVar(mode, hacked); }");
        h.stoat.hints_cache = None;

        h.stoat.handle_key(bare(KeyCode::Char('x')));
        assert!(!h.stoat.user_vars.contains_key("mode"));
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn hints_cache_reuses_rows_across_unchanged_frames() {
        let mut h = Stoat::test();
        // The `?` toggle forces the hints box in normal mode, so the main arm
        // populates the cache.
        h.stoat.key_hints_visible = true;
        let mut buf = Buffer::empty(h.stoat.size());

        h.stoat.paint_into(&mut buf);
        let key = h.stoat.hints_cache.as_ref().expect("cache populated").key;

        // A rebuild would drop this sentinel row. Reuse keeps it.
        h.stoat
            .hints_cache
            .as_mut()
            .unwrap()
            .rows
            .push(("SENTINEL".into(), "SENTINEL".into()));

        h.stoat.paint_into(&mut buf);
        let cache = h.stoat.hints_cache.as_ref().expect("cache retained");
        assert_eq!(cache.key, key, "unchanged state keeps the same cache key");
        assert!(
            cache.rows.iter().any(|(k, _)| k == "SENTINEL"),
            "an unchanged frame reuses the cached rows instead of rewalking",
        );
    }

    /// Paint a whole frame and return the APC scene it built.
    ///
    /// Read before any flush, so the decoration lane still holds this frame
    /// rather than the one before it. Handed back as text because the scene is
    /// ASCII throughout, and a mismatch between two of them reads as commands
    /// rather than as a pair of byte arrays.
    fn frame_scene(stoat: &mut Stoat) -> String {
        let mut buf = Buffer::empty(stoat.size());
        stoat.paint_into(&mut buf);
        String::from_utf8_lossy(stoat.apc_scene.bytes()).into_owned()
    }

    /// The same paint with every spliceable frame dropped, which is the scene a
    /// full encode writes with nothing to splice.
    fn cold_frame_scene(stoat: &mut Stoat) -> String {
        for editor in stoat.active_workspace_mut().editors.values_mut() {
            editor.gutter_geometry_cache = None;
            editor.status_scene_cache = Default::default();
        }
        frame_scene(stoat)
    }

    /// The gutter and the status bar each splice the APC frame they last
    /// emitted, so a repaint is only correct while the spliced bytes are the
    /// bytes a full encode writes. Checked at rest, where every frame splices,
    /// and after a cursor move, which re-encodes both.
    #[test]
    fn a_repaint_splices_the_scene_a_full_encode_writes() {
        let mut h = Stoat::test();
        h.stoat.stoatty = true;

        let root = std::path::PathBuf::from("/scene-memo");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        frame_scene(&mut h.stoat);
        let spliced = frame_scene(&mut h.stoat);

        assert!(!spliced.is_empty(), "a live scene carries the frame");
        assert_eq!(
            spliced,
            cold_frame_scene(&mut h.stoat),
            "an unchanged repaint splices the scene a full encode writes",
        );

        action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveDown);
        let moved = frame_scene(&mut h.stoat);

        assert_ne!(moved, spliced, "the moved cursor changes the frame");
        assert_eq!(
            moved,
            cold_frame_scene(&mut h.stoat),
            "a cursor move re-encodes instead of splicing the frame it cached",
        );
    }

    #[test]
    fn workspace_picker_binding_is_rebindable() {
        let mut h = Stoat::test();
        h.stoat.keymap = compile_keymap(
            "on key { modal == workspace_picker { Ctrl-x -> WorkspacePickerClose(); } }",
        );
        h.stoat.hints_cache = None;

        action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchWorkspace);
        assert!(h.stoat.workspace_picker.is_some());

        // The picker's filter input is in insert mode, so a printable key would
        // type rather than route. `Ctrl-x` is non-printable and not a default
        // picker binding, so closing on it proves the `modal == workspace_picker`
        // block drives the picker, not hardcoded dispatch.
        h.stoat.handle_key(ctrl('x'));
        assert!(h.stoat.workspace_picker.is_none());
    }

    /// The picker-level completion logic is unit-tested in `workspace_picker`.
    /// This covers the wiring that unit test cannot see. Tab has to route
    /// through the default keymap to the handler at all, rather than falling
    /// through to the insert-mode `SmartTab` and indenting the filter input.
    #[test]
    fn tab_completes_the_highlighted_workspace_into_the_filter() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().name = "alpha".into();

        action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchWorkspace);
        let _ = h.snapshot();

        h.type_keys("tab");
        let _ = h.snapshot();

        let picker = h.stoat.workspace_picker.as_ref().expect("picker open");
        assert_eq!(
            picker.input.text(h.stoat.active_workspace()),
            "alpha",
            "Tab completes the highlighted workspace name into the filter input"
        );
    }

    #[test]
    fn modal_over_a_target_keeps_the_target_mode() {
        let mut h = Stoat::test();
        h.stoat.set_focused_mode("select".into());
        assert_eq!(h.stoat.focused_mode(), "select");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "the palette input carries its own mode, not the target's"
        );

        h.stoat.handle_key(ctrl('c'));
        assert_eq!(
            h.stoat.focused_mode(),
            "select",
            "closing the modal leaves the underlying target's mode untouched"
        );
    }

    #[test]
    fn editor_pane_modes_are_independent_across_focus() {
        let mut h = Stoat::test();
        h.type_action("SplitRight()");
        h.stoat.set_focused_mode("insert".into());
        assert_eq!(h.stoat.focused_mode(), "insert");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "the other pane keeps its own mode across the focus switch"
        );

        action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusRight);
        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "returning focus restores the pane's own mode"
        );
    }

    #[test]
    fn encode_key_to_pty_covers_agent_keys() {
        let enc = |k: KeyEvent| encode_key_to_pty(&k);
        assert_eq!(enc(bare(KeyCode::Char('a'))), Some(b"a".to_vec()));
        assert_eq!(enc(bare(KeyCode::Char('Z'))), Some(b"Z".to_vec()));
        assert_eq!(enc(ctrl('c')), Some(vec![0x03]));
        assert_eq!(enc(ctrl('a')), Some(vec![0x01]));
        assert_eq!(enc(bare(KeyCode::Enter)), Some(vec![b'\r']));
        assert_eq!(enc(bare(KeyCode::Tab)), Some(vec![b'\t']));
        assert_eq!(enc(bare(KeyCode::Backspace)), Some(vec![0x7f]));
        assert_eq!(enc(bare(KeyCode::Esc)), Some(vec![0x1b]));
        assert_eq!(enc(bare(KeyCode::Up)), Some(b"\x1b[A".to_vec()));
        assert_eq!(enc(bare(KeyCode::Down)), Some(b"\x1b[B".to_vec()));
        assert_eq!(enc(bare(KeyCode::Right)), Some(b"\x1b[C".to_vec()));
        assert_eq!(enc(bare(KeyCode::Left)), Some(b"\x1b[D".to_vec()));
        assert_eq!(enc(bare(KeyCode::F(1))), None);
    }

    #[test]
    fn focused_term_pane_routes_keys_to_pty() {
        let (mut stoat, _id, fake) = stoat_with_focused_agent();

        assert_eq!(
            stoat.handle_key(bare(KeyCode::Char('h'))),
            UpdateEffect::None
        );
        stoat.handle_key(bare(KeyCode::Char('i')));
        stoat.handle_key(bare(KeyCode::Enter));
        stoat.handle_key(ctrl('d'));
        stoat.handle_key(ctrl('w'));

        assert_eq!(
            fake.sent_bytes(),
            vec![
                b"h".to_vec(),
                b"i".to_vec(),
                vec![b'\r'],
                vec![0x04],
                vec![0x17]
            ],
        );
        assert_eq!(
            stoat.focused_mode(),
            "insert",
            "Ctrl-W passes through, does not leave insert"
        );
    }

    #[test]
    fn focused_terminal_pane_routes_keys_to_pty() {
        let (mut stoat, _id, fake) = stoat_with_focused_term(View::Terminal);

        stoat.handle_key(bare(KeyCode::Char('l')));
        stoat.handle_key(bare(KeyCode::Char('s')));
        stoat.handle_key(bare(KeyCode::Enter));

        assert_eq!(
            fake.sent_bytes(),
            vec![b"l".to_vec(), b"s".to_vec(), vec![b'\r']],
        );
        assert_eq!(stoat.focused_mode(), "insert");
    }

    #[test]
    fn focused_term_pane_sends_interrupt_on_ctrl_c() {
        let (mut stoat, _id, fake) = stoat_with_focused_agent();

        let effect = stoat.handle_key(ctrl('c'));

        assert_eq!(effect, UpdateEffect::None);
        assert_eq!(stoat.focused_mode(), "insert");
        assert_eq!(fake.sent_bytes(), vec![vec![0x03]]);
    }

    #[test]
    fn esc_escapes_term_pane_without_forwarding() {
        let (mut stoat, _id, fake) = stoat_with_focused_agent();

        let effect = stoat.handle_key(bare(KeyCode::Esc));

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(stoat.focused_mode(), "normal");
        assert!(
            fake.sent_bytes().is_empty(),
            "escape must not reach the agent"
        );
    }

    #[test]
    fn terminal_action_enters_insert_and_types_without_i() {
        let mut h = Stoat::test();

        action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "opening a terminal focuses it in insert mode",
        );

        h.stoat.update(Event::Key(bare(KeyCode::Char('x'))));
        assert_eq!(
            h.fake_terminal().sent_bytes(),
            vec![b"x".to_vec()],
            "the first keystroke reaches the shell without pressing i",
        );
    }

    #[test]
    fn refocusing_a_terminal_reenters_insert() {
        let mut h = Stoat::test();
        h.type_action("SplitRight()");
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "the opened terminal is in insert"
        );

        h.stoat.update(Event::Key(bare(KeyCode::Esc)));
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "Esc drops the focused terminal to normal",
        );

        h.type_action("FocusLeft()");
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "the editor pane keeps normal mode",
        );

        h.type_action("FocusRight()");
        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "returning focus to the terminal re-enters insert",
        );
    }

    /// The input mode of the terminal shown in `pane`.
    fn term_mode(stoat: &Stoat, pane: PaneId) -> String {
        let ws = stoat.active_workspace();
        match ws.panes.pane(pane).view {
            View::Terminal(id) => ws.terms[id].mode.clone(),
            ref other => panic!("pane shows {other:?}, not a terminal"),
        }
    }

    /// A two-pane split with an editor on the left and a terminal on the right,
    /// focus left on the editor. Returns the two pane ids.
    fn split_editor_and_terminal(h: &mut crate::test_harness::TestHarness) -> (PaneId, PaneId) {
        h.type_action("SplitRight()");
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        let term_pane = h.stoat.active_workspace().panes.focus();
        h.type_action("FocusLeft()");
        (h.stoat.active_workspace().panes.focus(), term_pane)
    }

    #[test]
    fn esc_in_a_terminal_returns_to_the_pane_focus_arrived_from() {
        let mut h = Stoat::test();
        let (editor_pane, term_pane) = split_editor_and_terminal(&mut h);

        h.type_action("FocusRight()");
        h.stoat.update(Event::Key(bare(KeyCode::Esc)));

        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            editor_pane,
            "Esc sends focus back to the pane it arrived from",
        );
        assert_eq!(
            term_mode(&h.stoat, term_pane),
            "normal",
            "the terminal it left drops to normal",
        );
    }

    #[test]
    fn esc_in_a_terminal_returns_across_tabs() {
        let mut h = Stoat::test();
        let origin_pane = h.stoat.active_workspace().panes.focus();

        action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTab { index: 1 });
        assert_eq!(h.stoat.active_workspace().active_tab, 0);

        // C-a <digit> is GotoTab through the prefix mode's digit placeholder, so
        // the arrival on tab 2's terminal runs through update()'s record seam.
        h.type_keys("C-a 2");
        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "arriving on the terminal auto-inserts",
        );

        h.stoat.update(Event::Key(bare(KeyCode::Esc)));

        assert_eq!(
            h.stoat.active_workspace().active_tab,
            0,
            "Esc returns to the tab focus came from",
        );
        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            origin_pane,
            "and to the pane it was on there",
        );
    }

    #[test]
    fn esc_in_an_in_place_terminal_only_drops_to_normal() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        let pane = h.stoat.active_workspace().panes.focus();

        h.stoat.update(Event::Key(bare(KeyCode::Esc)));

        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            pane,
            "a terminal opened in place has nowhere to return to",
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "so normal mode stays reachable",
        );
    }

    #[test]
    fn esc_in_a_terminal_whose_origin_pane_closed_drops_to_normal() {
        let mut h = Stoat::test();
        let (editor_pane, term_pane) = split_editor_and_terminal(&mut h);
        h.type_action("FocusRight()");

        assert!(
            h.stoat.active_workspace_mut().panes.close(editor_pane),
            "the recorded origin pane is closed out from under the record",
        );

        h.stoat.update(Event::Key(bare(KeyCode::Esc)));

        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            term_pane,
            "a stale record leaves focus put",
        );
        assert_eq!(h.stoat.focused_mode(), "normal", "and only drops to normal");
    }

    #[test]
    fn closing_a_tab_fixes_up_terminal_return_records() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);

        let (at_closed, above_closed) = {
            let ws = h.stoat.active_workspace_mut();
            let pane = ws.panes.focus();
            let at_closed = ws.terms.keys().next().expect("the opened terminal");
            let above_closed = ws.terms.insert(TermSession::new(
                crate::term_screen::TermScreen::new(24, 80),
                Arc::new(crate::host::FakeTerminalSession::default()),
            ));
            ws.terms[at_closed].return_focus = Some(TermReturnFocus::Pane { tab: 1, pane });
            ws.terms[above_closed].return_focus = Some(TermReturnFocus::Pane { tab: 2, pane });
            (at_closed, above_closed)
        };

        h.stoat.active_workspace_mut().close_tab(1);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            ws.terms[at_closed].return_focus, None,
            "a record naming the closed tab is dropped",
        );
        assert_eq!(
            ws.terms[above_closed].return_focus,
            Some(TermReturnFocus::Pane {
                tab: 1,
                pane: ws.panes.focus()
            }),
            "a record above the closed tab shifts down with it",
        );
    }

    #[test]
    fn esc_bounces_between_two_terminals() {
        let mut h = Stoat::test();
        let (left_pane, right_pane) = split_editor_and_terminal(&mut h);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        h.stoat.update(Event::Key(bare(KeyCode::Esc)));

        h.type_action("FocusRight()");
        h.stoat.update(Event::Key(bare(KeyCode::Esc)));
        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            left_pane,
            "Esc in the right terminal lands on the left one",
        );

        h.stoat.update(Event::Key(bare(KeyCode::Esc)));
        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            right_pane,
            "the arrival re-recorded the origin, so Esc bounces back",
        );
    }

    #[test]
    fn mouse_click_into_terminal_pane_enters_insert() {
        use crossterm::event::MouseButton;

        let mut h = Stoat::test();
        let term_pane = {
            let ws = h.stoat.active_workspace_mut();
            let editor_pane = ws.panes.focus();
            let term_pane = ws.panes.split(crate::pane::Axis::Vertical);
            let term_id = insert_term_session(ws);
            ws.panes.pane_mut(term_pane).view = View::Terminal(term_id);
            ws.panes.set_focus(editor_pane);
            ws.panes.pane_mut(editor_pane).area = Rect::new(0, 0, 40, 24);
            ws.panes.pane_mut(term_pane).area = Rect::new(40, 0, 40, 24);
            term_pane
        };
        assert_eq!(h.stoat.focused_mode(), "normal");

        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 50, 5));

        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            term_pane,
            "the click focuses the terminal pane",
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "focusing a terminal by mouse enters insert",
        );
    }

    fn focused_terminal_pane(h: &mut crate::test_harness::TestHarness, content: &[u8]) -> TermId {
        let term_id = {
            let ws = h.stoat.active_workspace_mut();
            let pane = ws.panes.focus();
            let term_id = insert_term_session(ws);
            ws.panes.pane_mut(pane).view = View::Terminal(term_id);
            term_id
        };
        // A focused terminal pane runs in insert, so typing routes to the pty.
        h.stoat.set_focused_mode("insert".to_string());
        // A render fits the emulator to the focused pane, so feed the content
        // afterward to land it in the final grid.
        let _ = h.stoat.render();
        h.stoat.active_workspace_mut().terms[term_id]
            .term
            .feed(content);
        term_id
    }

    #[test]
    fn dragging_over_a_terminal_pane_selects_and_copies() {
        use crossterm::event::MouseButton;

        let mut h = Stoat::test();
        let term_id = focused_terminal_pane(&mut h, b"hello world");

        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0));
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 4, 0));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 4, 0));

        assert_eq!(h.fake_clipboard().writes(), vec!["hello"]);
        assert!(
            h.stoat.active_workspace().terms[term_id]
                .selection
                .is_some(),
            "the selection stays highlighted after release",
        );
        assert!(
            h.stoat.terminal_drag.is_none(),
            "the drag clears on release"
        );
    }

    /// The terminal arm drops the same repeats the editor arm does, and for the
    /// same reason. A release after them still copies, so the dedupe costs the
    /// selection nothing.
    #[test]
    fn a_repeated_terminal_drag_on_the_settled_cell_costs_no_frame() {
        use crossterm::event::MouseButton;

        let mut h = Stoat::test();
        let _ = focused_terminal_pane(&mut h, b"hello world");

        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0));
        let moved = h
            .stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 4, 0));
        let repeat = h
            .stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 4, 0));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 4, 0));

        assert_eq!(moved, UpdateEffect::Redraw, "the head moved, so repaint");
        assert_eq!(repeat, UpdateEffect::None, "nothing moved, so no repaint");
        assert_eq!(h.fake_clipboard().writes(), vec!["hello"]);
    }

    #[test]
    fn a_keystroke_clears_the_terminal_selection() {
        use crossterm::event::MouseButton;

        let mut h = Stoat::test();
        let term_id = focused_terminal_pane(&mut h, b"hello world");

        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0));
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 4, 0));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 4, 0));
        assert!(h.stoat.active_workspace().terms[term_id]
            .selection
            .is_some());

        h.stoat.update(Event::Key(bare(KeyCode::Char('x'))));
        assert!(
            h.stoat.active_workspace().terms[term_id]
                .selection
                .is_none(),
            "typing into the terminal clears the selection",
        );
    }

    #[test]
    fn a_click_without_drag_leaves_no_terminal_selection() {
        use crossterm::event::MouseButton;

        let mut h = Stoat::test();
        let term_id = focused_terminal_pane(&mut h, b"hello world");

        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 0));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 0));

        assert!(
            h.stoat.active_workspace().terms[term_id]
                .selection
                .is_none(),
            "a plain click leaves no selection",
        );
        assert!(
            h.fake_clipboard().writes().is_empty(),
            "a plain click copies nothing",
        );
    }

    #[test]
    fn palette_over_a_terminal_routes_typing_to_the_palette() {
        let mut h = Stoat::test();
        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        {
            let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
            let ws = h.stoat.active_workspace_mut();
            let pane = ws.panes.focus();
            let term_id = ws.terms.insert(TermSession::new(
                crate::term_screen::TermScreen::new(24, 80),
                session,
            ));
            ws.panes.pane_mut(pane).view = View::Terminal(term_id);
        }
        h.stoat.set_focused_mode("insert".to_string());

        // Esc drops the terminal to normal so the next ':' opens the palette.
        h.stoat.update(Event::Key(bare(KeyCode::Esc)));
        assert_eq!(h.stoat.focused_mode(), "normal");

        h.stoat.update(Event::Key(bare(KeyCode::Char(':'))));
        assert!(
            h.stoat.command_palette.is_some(),
            "':' over a terminal pane opens the command palette",
        );

        for ch in "qui".chars() {
            h.stoat.update(Event::Key(bare(KeyCode::Char(ch))));
        }

        let text = {
            let ws = h.stoat.active_workspace();
            let palette = h.stoat.command_palette.as_ref().expect("palette open");
            palette.focused_input().expect("palette input").text(ws)
        };
        assert_eq!(
            text, "qui",
            "typing filters the palette rather than the terminal behind it",
        );
        assert!(
            fake.sent_bytes().is_empty(),
            "the terminal PTY receives nothing while the palette owns typing",
        );

        h.stoat.update(Event::Key(bare(KeyCode::Esc)));
        assert!(h.stoat.command_palette.is_none(), "Esc closes the palette");
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "the terminal is left in normal mode after the palette closes",
        );
    }

    #[test]
    fn mouse_click_into_agent_pane_stays_normal() {
        use crossterm::event::MouseButton;

        let mut h = Stoat::test();
        let agent_pane = {
            let ws = h.stoat.active_workspace_mut();
            let editor_pane = ws.panes.focus();
            let agent_pane = ws.panes.split(crate::pane::Axis::Vertical);
            let term_id = insert_term_session(ws);
            ws.panes.pane_mut(agent_pane).view = View::Agent(term_id);
            ws.panes.set_focus(editor_pane);
            ws.panes.pane_mut(editor_pane).area = Rect::new(0, 0, 40, 24);
            ws.panes.pane_mut(agent_pane).area = Rect::new(40, 0, 40, 24);
            agent_pane
        };

        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 50, 5));

        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            agent_pane,
            "the click focuses the agent pane",
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "focusing an agent pane does not auto-enter insert",
        );
    }

    #[test]
    fn respawn_enters_insert_on_focused_terminal() {
        let mut h = Stoat::test();
        let pane = {
            let ws = h.stoat.active_workspace_mut();
            let pane = ws.panes.focus();
            ws.panes.pane_mut(pane).view = View::Terminal(TermId::default());
            pane
        };
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "a dead terminal reads the fallback mode",
        );

        action_handlers::respawn_terminal_panes(&mut h.stoat);

        let View::Terminal(new_id) = h.stoat.active_workspace().panes.pane(pane).view else {
            panic!("the dead terminal pane is respawned as a terminal");
        };
        assert!(
            h.stoat.active_workspace().terms.contains_key(new_id),
            "respawned session is live",
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "a respawned focused terminal enters insert",
        );
    }

    #[test]
    fn agent_input_ignored_outside_insert_mode() {
        let (mut stoat, _id, fake) = stoat_with_focused_agent();
        stoat.set_focused_mode("normal".to_string());

        stoat.handle_key(bare(KeyCode::Char('x')));

        assert!(
            fake.sent_bytes().is_empty(),
            "normal mode must not route to the agent"
        );
    }

    #[test]
    fn agent_input_requires_agent_focus() {
        let (mut stoat, _id, fake) = stoat_with_focused_agent();
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        ws.panes.pane_mut(focused).view = View::Label("scratch".to_string());

        stoat.handle_key(bare(KeyCode::Char('x')));

        assert!(
            fake.sent_bytes().is_empty(),
            "non-agent focus must not route"
        );
    }

    /// Every cursor a multi-cursor delete moves is carried through the same
    /// merged range list, so the answer is a binary search over a prefix sum
    /// rather than a walk per cursor. The four positions a target can occupy
    /// relative to the ranges are what that search has to get right.
    #[test]
    fn an_offset_moves_back_by_the_deletions_before_it() {
        // Deleting 2..5 and 10..14 removes three bytes then four.
        let ranges = [(2usize, 5usize), (10, 14)];
        let deleted_before = [0usize, 3, 7];
        let moved = |target| Stoat::offset_after_deletions(target, &ranges, &deleted_before);

        assert_eq!(moved(1), 1, "ahead of every range, nothing shifts it");
        assert_eq!(moved(3), 2, "inside a range, it collapses to that start");
        assert_eq!(moved(7), 4, "between ranges, only the first has passed");
        assert_eq!(moved(20), 13, "past both, both have passed");

        assert_eq!(moved(2), 2, "a target on a range's start is not inside it");
        assert_eq!(moved(5), 2, "a target on a range's end sits after it");
    }

    /// A two-pane split with the second file focused, painted once so both
    /// panes' caches are warm.
    ///
    /// The unfocused pane's buffer carries a diagnostic, so its paint reaches
    /// all three channels. Without one it produces no undercurl span at all,
    /// and a replay that dropped them would pass unnoticed.
    fn split_pair(h: &mut crate::test_harness::TestHarness) {
        h.stoat.stoatty = true;
        h.resize(120, 16);
        let a = h.write_file("a.txt", "alpha\nbravo\ncharlie\n");
        let b = h.write_file("b.txt", "delta\necho\nfoxtrot\n");
        h.open_file(&a);
        publish_one_diagnostic(h, &a);
        h.type_action("SplitRight()");
        h.open_file(&b);
        h.settle();
        let _ = h.stoat.render();
    }

    /// An error over the first word of `path`'s first line, anchored against the
    /// buffer as it stands.
    fn publish_one_diagnostic(h: &mut crate::test_harness::TestHarness, path: &std::path::Path) {
        use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range as LspRange};

        let snapshot = {
            let ws = h.stoat.active_workspace();
            let id = ws.buffers.id_for_path(path).expect("buffer registered");
            ws.buffers
                .get(id)
                .expect("buffer")
                .read()
                .expect("poisoned")
                .snapshot
                .clone()
        };
        let anchors = crate::diagnostics::PublishedSpan {
            anchors: Some((
                snapshot.anchors_at_batch(&[0], Bias::Right)[0],
                snapshot.anchors_at_batch(&[5], Bias::Left)[0],
            )),
        };
        h.stoat.diagnostics.replace_from_server(
            path.to_path_buf(),
            "test".into(),
            vec![Diagnostic {
                range: LspRange {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 5,
                    },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                message: String::new(),
                ..Default::default()
            }],
            vec![anchors],
        );
    }

    /// One frame's three output channels, for comparing a replayed frame
    /// against a repainted one.
    type FrameOutput = (Buffer, Vec<u8>, Vec<(u16, u16, u16, [u8; 3])>);

    fn frame_output(h: &mut crate::test_harness::TestHarness) -> FrameOutput {
        let buf = h.stoat.render();
        let scene = h.stoat.apc_scene.bytes().to_vec();
        let spans = h
            .stoat
            .pending_undercurls
            .spans()
            .iter()
            .map(|span| (span.x, span.y, span.len, span.color))
            .collect();
        (buf, scene, spans)
    }

    /// Replaying an unfocused pane produces the frame repainting it would have,
    /// while the focused pane alone is painted.
    ///
    /// A cache that skipped work but changed the output would be worse than no
    /// cache, so the comparison is against the same frame with the cache
    /// emptied rather than against a recorded expectation. All three channels
    /// are compared, since cells alone would pass while the rich gutter, the
    /// minimap strip, and the undercurls were dropped.
    #[test]
    fn a_replayed_pane_paints_what_repainting_it_would() {
        let mut h = Stoat::test();
        split_pair(&mut h);

        h.type_keys("i");
        h.type_text("X");
        h.settle();

        let painted_before = h.stoat.pane_paints;
        let replayed = frame_output(&mut h);
        assert_eq!(
            h.stoat.pane_paints - painted_before,
            1,
            "the edited pane painted and the other replayed",
        );

        h.stoat.pane_cache.clear();
        let painted_before = h.stoat.pane_paints;
        let repainted = frame_output(&mut h);
        assert_eq!(
            h.stoat.pane_paints - painted_before,
            2,
            "and with the cache emptied both panes paint",
        );

        assert_eq!(replayed.0, repainted.0, "the cells match");
        assert_eq!(replayed.1, repainted.1, "the scene bytes match");
        assert_eq!(replayed.2, repainted.2, "the undercurl spans match");
    }

    /// A frame driven by background activity alone paints only the focused
    /// pane.
    /// The key guards used to resolve the mode once each, and resolving it
    /// walks the modal stack and clones a pane-tree view. Reading it once for
    /// the whole chain took a movement key from twenty-two resolutions to nine.
    ///
    /// A bound rather than a count, because the nine that remain belong to
    /// callers this does not touch, and one of them arriving or leaving should
    /// not fail a test about the guards. Anything near twenty means the chain
    /// went back to asking per guard.
    #[test]
    fn the_key_guards_resolve_the_mode_once_between_them() {
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(40, 6);
        let file = h.write_file("a.rs", "hello world\nsecond line\n");
        h.open_file(&file);

        let before = h.stoat.focused_mode_reads.get();
        h.type_keys("l");
        let movement = h.stoat.focused_mode_reads.get() - before;
        assert!(
            movement <= 12,
            "a movement key resolved the mode {movement} times"
        );

        let before = h.stoat.focused_mode_reads.get();
        h.type_keys("i");
        let entering = h.stoat.focused_mode_reads.get() - before;
        assert!(
            entering <= 12,
            "entering insert resolved the mode {entering} times"
        );
    }

    /// An action brackets itself in an undo group, and a group that takes no
    /// edit is discarded on sealing, so the selections the seal would record are
    /// never read. Gathering them copies the whole selection set, which at
    /// multi-cursor scale is the cost that matters.
    ///
    /// The editing action still records both ends, which is what says the count
    /// is measuring something.
    #[test]
    fn a_non_editing_action_captures_no_post_selections() {
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(40, 6);
        let file = h.write_file("a.rs", "hello world\nsecond line\n");
        h.open_file(&file);

        let before = h.stoat.selection_snapshots.get();
        h.type_keys("l");
        assert_eq!(
            h.stoat.selection_snapshots.get() - before,
            1,
            "a movement captures the pre-action set and nothing after it"
        );

        // `x` selects the line, which edits nothing either.
        let before = h.stoat.selection_snapshots.get();
        h.type_keys("x");
        assert_eq!(
            h.stoat.selection_snapshots.get() - before,
            1,
            "selecting a line is not an edit"
        );

        // `d` deletes it, so the group materializes and the seal records where
        // the selections ended up.
        let before = h.stoat.selection_snapshots.get();
        h.type_keys("d");
        assert_eq!(
            h.stoat.selection_snapshots.get() - before,
            2,
            "an edit captures both ends of its undo group"
        );
    }

    /// Typing in insert mode is the busiest thing the editor does, and a
    /// printable character never consults the keymap, so it must not pay to
    /// derive the lookup. Nothing else would notice if it started to: the
    /// derivation only costs time.
    ///
    /// The keys that do read a binding still derive one, which is what says the
    /// counter is measuring something.
    #[test]
    fn typing_a_printable_character_never_derives_a_keymap_lookup() {
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(40, 6);
        let file = h.write_file("a.rs", "hello\n");
        h.open_file(&file);

        // `i` is a normal-mode binding and reads its own lookup.
        h.type_keys("i");
        let entering_insert = h.stoat.keymap_lookups.get();
        assert!(
            entering_insert > 0,
            "entering insert resolves through the keymap"
        );

        h.type_text("abcdef");
        assert_eq!(
            h.stoat.keymap_lookups.get(),
            entering_insert,
            "six printable characters must derive no lookup between them"
        );

        // Escape is non-printable, so it falls through to the keymap.
        h.type_keys("esc");
        assert!(
            h.stoat.keymap_lookups.get() > entering_insert,
            "leaving insert resolves through the keymap"
        );
    }

    /// This is the case the cache exists for. A spinner tick redraws at its own
    /// rate while nothing a pane reads has moved, and every visible pane used to
    /// repaint in full for it. The focused one still paints, since its
    /// selections and cursor are outside the key, so the saving is every pane
    /// but that one.
    #[test]
    fn a_background_tick_paints_only_the_focused_pane() {
        let mut h = Stoat::test();
        split_pair(&mut h);

        let before = h.stoat.pane_paints;
        h.stoat.spinner_clock += 1.0;
        let _ = h.stoat.render();

        assert_eq!(
            h.stoat.pane_paints - before,
            1,
            "the spinner moved nothing the unfocused pane reads",
        );
    }

    /// Every input a pane paints from reaches its key.
    ///
    /// A key that ignored one of these would leave a pane showing stale content
    /// with nothing to say it had, which is why each is moved through the real
    /// session rather than by building two keys by hand.
    ///
    /// The assertion is on the key rather than on a paint count, because the
    /// harness renders a frame per keystroke and the search case is several of
    /// them. A key that moved is a replay refused, since the lookup compares
    /// keys for equality before it hands anything back.
    #[test]
    fn each_input_that_moves_reaches_the_key() {
        type Case = (&'static str, fn(&mut crate::test_harness::TestHarness));
        let cases: [Case; 6] = [
            ("theme", |h| {
                action_handlers::dispatch(
                    &mut h.stoat,
                    &stoat_action::SetTheme {
                        name: "gruvbox-light".to_string(),
                    },
                );
            }),
            ("search", |h| {
                h.type_keys("/");
                h.type_text("alpha");
                h.type_keys("enter");
            }),
            ("inactive dim", |h| {
                h.stoat.settings.ui_inactive_dim = Some(0.6);
            }),
            ("line numbers", |h| {
                h.stoat.settings.editor_line_numbers = Some(LineNumbers::Off);
            }),
            ("diagnostics", |h| {
                let path = h.stoat.active_workspace().git_root.join("a.txt");
                h.stoat.diagnostics.replace_from_server(
                    path,
                    "test".into(),
                    Vec::new(),
                    Vec::new(),
                );
            }),
            ("the unfocused pane's own text", |h| {
                let ws = h.stoat.active_workspace_mut();
                let first = ws.panes.split_pane_ids()[0];
                let editor_id = match ws.panes.pane(first).view {
                    View::Editor(id) => id,
                    _ => panic!("the first pane holds an editor"),
                };
                let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
                let buffer = ws.buffers.get(buffer_id).expect("buffer");
                buffer.write().expect("poisoned").edit(0..0, "Z");
            }),
        ];

        for (what, apply) in cases {
            let mut h = Stoat::test();
            split_pair(&mut h);
            let _ = h.stoat.render();

            let unfocused = {
                let ws = h.stoat.active_workspace();
                let focused = ws.panes.focus();
                ws.panes
                    .split_pane_ids()
                    .into_iter()
                    .find(|id| *id != focused)
                    .expect("the split has an unfocused pane")
            };
            let before = h
                .stoat
                .pane_cache
                .get(&unfocused)
                .expect("the unfocused pane cached its paint")
                .key;

            apply(&mut h);
            h.settle();
            let _ = h.stoat.render();

            let after = h
                .stoat
                .pane_cache
                .get(&unfocused)
                .expect("and cached it again")
                .key;
            assert_ne!(
                before, after,
                "{what} has to reach the unfocused pane's key"
            );
        }
    }

    #[test]
    fn snapshot_initial_plain() {
        let mut h = Stoat::test();
        h.assert_snapshot("initial_plain");
    }

    #[test]
    fn snapshot_initial_styled() {
        let mut h = Stoat::test();
        h.assert_snapshot("initial");
    }

    #[test]
    fn snapshot_space_mode() {
        let mut h = Stoat::test();
        h.type_keys("space");
        h.assert_snapshot("space_mode");
    }

    fn focused_buffer_string(h: &crate::test_harness::TestHarness) -> String {
        let ws = h.stoat.active_workspace();
        let View::Editor(editor_id) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("focused pane is not an editor");
        };
        let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
        ws.buffers
            .get(buffer_id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .rope()
            .to_string()
    }

    fn focused_buffer_version(h: &crate::test_harness::TestHarness) -> u64 {
        let ws = h.stoat.active_workspace();
        let View::Editor(editor_id) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("focused pane is not an editor");
        };
        let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
        ws.buffers
            .get(buffer_id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .version()
    }

    /// A paste arrives whole, so it costs one edit across every cursor rather
    /// than the whole keystroke pipeline per character. Landing as one edit is
    /// also what makes it one thing to undo.
    #[test]
    fn a_paste_lands_as_one_edit_every_cursor_shares() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "note.txt", b"a\nb\n");
        h.type_keys("C");
        let before = focused_buffer_version(&h);

        // Carries a CRLF, which a terminal forwards as the clipboard held it.
        h.stoat.update(Event::Paste("X\r\nY".to_string()));

        // The version counts edit records, and a multi-cursor insert is one per
        // cursor however it arrived. What the batch buys is that the count does
        // not also multiply by the pasted length, which is what a character at a
        // time would have cost.
        assert_eq!(
            focused_buffer_version(&h),
            before + 2,
            "one record per cursor, not one per cursor per pasted character",
        );
        assert_eq!(focused_buffer_string(&h), "X\nYa\nX\nYb\n");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);
        assert_eq!(focused_buffer_string(&h), "a\nb\n", "and one thing to undo",);
    }

    /// The characters of a paste are text, never keys. Pasting in normal mode
    /// used to run what it spelled, so text carrying `d` or `x` edited the
    /// buffer on its way in.
    #[test]
    fn a_normal_mode_paste_inserts_rather_than_running_its_characters() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "note.txt", b"keep\n");
        assert_eq!(h.snapshot().mode, "normal");

        h.stoat.update(Event::Paste("dd".to_string()));

        assert_eq!(focused_buffer_string(&h), "ddkeep\n");
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "and the paste leaves the mode where it found it",
        );
    }

    /// A modal's input is where typing goes while it is open, so a paste goes
    /// there too rather than into the buffer behind it.
    #[test]
    fn a_paste_with_a_modal_open_lands_in_its_input() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "note.txt", b"keep\n");
        h.type_keys("space p");
        assert!(h.stoat.file_finder.is_some(), "the finder is open");

        h.stoat.update(Event::Paste("note".to_string()));

        let ws = h.stoat.active_workspace();
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        assert_eq!(finder.input.text(ws), "note");
        assert_eq!(
            focused_buffer_string(&h),
            "keep\n",
            "and the buffer behind it is untouched",
        );
    }

    /// A modal's input is painted as a single row, so a pasted line break has
    /// nowhere to go.
    ///
    /// The break would put the cursor on a row the region never draws, and the
    /// query would carry a newline no filter expects. Every character of the
    /// paste still arrives, with a space standing in for each break.
    #[test]
    fn a_multi_line_paste_into_a_modal_lands_on_one_row() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "note.txt", b"keep\n");
        h.type_keys("space p");
        assert!(h.stoat.file_finder.is_some(), "the finder is open");

        h.stoat.update(Event::Paste("a\r\nb\nc".to_string()));

        let ws = h.stoat.active_workspace();
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        assert_eq!(finder.input.text(ws), "a b c");

        let rows = ws
            .buffers
            .get(finder.input.buffer_id)
            .expect("input buffer")
            .read()
            .expect("poisoned")
            .rope()
            .max_point()
            .row;
        assert_eq!(rows, 0, "and the input is still the one row it paints");
    }

    /// Pasting a command into a terminal pane sends it to the child.
    ///
    /// Typing there already goes to the child, and paste has no reason to
    /// differ. Newlines arrive as carriage returns because that is what the
    /// Enter key sends, so a pasted command line runs the way a typed one does.
    #[test]
    fn a_paste_into_a_terminal_reaches_the_child() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);

        let effect = h.stoat.update(Event::Paste("echo hi\nls\r\n".to_string()));

        assert_eq!(
            h.fake_terminal().sent_bytes(),
            vec![b"echo hi\rls\r".to_vec()],
            "the paste reaches the child with its newlines as carriage returns",
        );
        assert_eq!(
            effect,
            UpdateEffect::None,
            "and asks for no frame of its own, the child's echo doing that",
        );
    }

    /// A child that asked for bracketed paste gets the guards.
    ///
    /// A shell or editor sets DECSET 2004 so it can tell a paste from typing
    /// and hold it back rather than running each line as it arrives. An
    /// embedded end guard is dropped, since text that closed the bracket early
    /// would have the rest of itself run as keystrokes.
    #[test]
    fn a_bracketed_child_gets_a_guarded_paste() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        let term_id = h
            .stoat
            .focused_term_id()
            .expect("the terminal action focuses a term");
        h.stoat
            .active_workspace_mut()
            .terms
            .get_mut(term_id)
            .expect("term session")
            .term
            .feed(b"\x1b[?2004h");

        h.stoat
            .update(Event::Paste("rm -rf\x1b[201~ /".to_string()));

        assert_eq!(
            h.fake_terminal().sent_bytes(),
            vec![b"\x1b[200~rm -rf /\x1b[201~".to_vec()],
            "the payload is wrapped and cannot close the bracket itself",
        );
    }

    /// A modal takes the paste even over a focused terminal.
    ///
    /// The overlay is where typing goes while it is open, and the terminal
    /// underneath keeps the focus that would otherwise claim it.
    #[test]
    fn a_paste_over_a_terminal_still_lands_in_an_open_modal() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        h.stoat.update(Event::Key(bare(KeyCode::Esc)));
        h.type_keys("space p");
        assert!(h.stoat.file_finder.is_some(), "the finder is open");

        h.stoat.update(Event::Paste("note".to_string()));

        let ws = h.stoat.active_workspace();
        let finder = h.stoat.file_finder.as_ref().expect("finder open");
        assert_eq!(finder.input.text(ws), "note");
        assert!(
            h.fake_terminal().sent_bytes().is_empty(),
            "and nothing reached the terminal behind it",
        );
    }

    #[test]
    fn encode_paste_to_pty_normalizes_newlines_when_unbracketed() {
        assert_eq!(encode_paste_to_pty("a\r\nb\nc", false), b"a\rb\rc".to_vec());
    }

    #[test]
    fn enter_after_open_brace_auto_indents() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"fn a() {\n}\n");
        h.type_keys("A");
        h.type_keys("enter");
        h.settle();
        assert_eq!(focused_buffer_string(&h), "fn a() {\n\t\n}\n");
    }

    /// A buffer written in spaces indents by its own unit, not by a tab.
    ///
    /// The base is copied from the row and only the delta comes from the
    /// indent style, so getting the delta wrong glues a tab onto spaces and the
    /// new line reads a level deeper than it is. The two-space increase on the
    /// second row is what the buffer's detector votes on.
    #[test]
    fn enter_indents_a_space_buffer_by_its_own_unit() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.json", b"{\n  \"a\": {\n  }\n}\n");
        h.type_keys("j");
        h.type_keys("A");
        h.type_keys("enter");
        h.settle();
        assert_eq!(
            focused_buffer_string(&h),
            "{\n  \"a\": {\n    \n  }\n}\n",
            "the opener's row is indented two, so the new line is indented four",
        );
    }

    /// Opening a line below reads the same unit as Enter does.
    #[test]
    fn open_below_indents_a_space_buffer_by_its_own_unit() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.json", b"{\n  \"a\": {\n  }\n}\n");
        h.type_keys("j");
        h.type_keys("o");
        h.type_text("1");
        assert_eq!(focused_buffer_string(&h), "{\n  \"a\": {\n    1\n  }\n}\n");
    }

    /// Re-indenting an existing row reads it too, which is the other entry
    /// point and the other query.
    #[test]
    fn shift_i_indents_a_space_buffer_by_its_own_unit() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.json", b"{\n  \"a\": {\n\n  }\n}\n");
        h.type_keys("j");
        h.type_keys("j");
        h.type_keys("I");
        h.type_text("1");
        assert_eq!(focused_buffer_string(&h), "{\n  \"a\": {\n    1\n  }\n}\n");
    }

    #[test]
    fn enter_plaintext_copies_leading_whitespace() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "note.txt", b"\thello\n");
        h.type_keys("A");
        h.type_keys("enter");
        h.settle();
        assert_eq!(focused_buffer_string(&h), "\thello\n\t\n");
    }

    #[test]
    fn enter_indents_each_cursor_by_its_own_line() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "note.txt", b"\tfoo\nbar\n");
        h.type_keys("C");
        h.type_keys("A");
        h.type_keys("enter");
        h.settle();
        assert_eq!(focused_buffer_string(&h), "\tfoo\n\t\nbar\n\n");
    }

    #[test]
    fn each_cursor_lands_after_its_own_continuation() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "note.txt", b"\tfoo\nbar\n");
        h.type_keys("C");
        h.type_keys("A");
        h.type_keys("enter");
        // Typing next is what reveals where each cursor actually landed, the
        // continuations differing in length so a uniform shift misplaces one.
        h.type_keys("x");
        h.settle();
        assert_eq!(focused_buffer_string(&h), "\tfoo\n\tx\nbar\nx\n");
    }

    #[test]
    fn enter_continues_a_comment_only_on_the_comment_line() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"let a = 1;\n// note\n");
        h.type_keys("C");
        h.type_keys("A");
        h.type_keys("enter");
        h.settle();
        assert_eq!(focused_buffer_string(&h), "let a = 1;\n\n// note\n// \n");
    }

    /// Lay out a hover of `num_lines` lines each `line_width` wide in a
    /// `width` x `height` window, returning the popup and inner rects.
    fn hover_layout(width: u16, height: u16, num_lines: usize, line_width: usize) -> (Rect, Rect) {
        use crate::{render::hover::HoverPopup, test_harness::TestHarness};
        use ratatui::style::Style;

        let mut h = TestHarness::with_size(width, height);
        let root = std::path::PathBuf::from("/hover");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        let text = "x".repeat(line_width);
        let lines = (0..num_lines)
            .map(|_| vec![(text.clone(), Style::default())])
            .collect();
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        h.stoat.pending_hover = Some(HoverPopup::new(lines, 0, editor_id));
        crate::render::hover::hover_popup_layout(&mut h.stoat).expect("hover layout")
    }

    #[test]
    fn hover_popup_stays_compact_on_a_small_window() {
        // Thirty lines of hover in a 12-row window used to fill nearly the pane.
        let (popup, _) = hover_layout(40, 12, 30, 20);
        assert!(
            (3..=6).contains(&popup.height),
            "a tall hover on a small window caps near half the pane, got {}",
            popup.height,
        );
    }

    #[test]
    fn hover_popup_caps_at_helix_absolute_limits() {
        // On a large window the absolute caps bound the popup before half-pane.
        let (popup, _) = hover_layout(200, 60, 40, 130);
        assert_eq!(popup.height, 26, "tall content caps at MAX_HEIGHT");
        assert_eq!(popup.width, 120, "wide content caps at MAX_WIDTH");
    }

    /// A hover popup at a fixed area (`9,1 22x7`) with interior (`10,2 20x5`),
    /// `lines` as single unstyled spans and the given scroll offset.
    fn hover_sel_popup(
        lines: &[&str],
        scroll_half_pages: usize,
    ) -> crate::render::hover::HoverPopup {
        use ratatui::style::Style;
        let mut popup = crate::render::hover::HoverPopup::new(
            lines
                .iter()
                .map(|l| vec![(l.to_string(), Style::default())])
                .collect(),
            0,
            EditorId::default(),
        );
        popup.scroll_half_pages = scroll_half_pages;
        popup.area = Rect {
            x: 9,
            y: 1,
            width: 22,
            height: 7,
        };
        popup.inner = Rect {
            x: 10,
            y: 2,
            width: 20,
            height: 5,
        };
        popup
    }

    #[test]
    fn hover_drag_copies_and_leaves_the_editor_untouched() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "buffer text\n");
        h.stoat.pending_hover = Some(hover_sel_popup(&["hello world", "second line"], 0));

        // Down at inner (10,2) resolves to (line 0, col 0). The drag to (13,2) is
        // three cells in, which the 0.85x popover scale maps to char column 4, so
        // the copied span is "hell".
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 2));
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 13, 2));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 13, 2));

        assert_eq!(h.fake_clipboard().writes(), vec!["hell"]);
        assert!(
            h.stoat.editor_drag.is_none(),
            "a hover drag never arms the editor selection",
        );
        assert!(
            h.stoat.pending_hover.as_ref().unwrap().selection.is_some(),
            "the selection stays live after release",
        );
    }

    #[test]
    fn unplaceable_hover_popup_stops_consuming_mouse_input() {
        use crate::render::hover::HoverPopup;
        use ratatui::style::Style;

        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "alpha beta gamma\n");
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        h.stoat.pending_hover = Some(HoverPopup::new(
            vec![vec![("hover".to_string(), Style::default())]],
            0,
            editor_id,
        ));

        // The first render stamps the popup's real screen rect.
        let _ = h.stoat.render();
        let rendered = h.stoat.pending_hover.as_ref().unwrap().area;
        assert_ne!(rendered, Rect::default(), "the popup renders a rect");

        // Make the anchor unplaceable (past the rope), then render again.
        h.stoat.pending_hover.as_mut().unwrap().anchor_offset = 10_000;
        let _ = h.stoat.render();
        assert_eq!(
            h.stoat.pending_hover.as_ref().unwrap().area,
            Rect::default(),
            "an unplaceable popup resets its stored rect",
        );

        // A Down inside the previously rendered rect falls through to the pane
        // instead of the stale area swallowing it as a hover selection.
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            rendered.x + 1,
            rendered.y + 1,
        ));
        assert!(
            h.stoat.pending_hover.as_ref().unwrap().selection.is_none(),
            "the stale rect no longer consumes the click as a selection",
        );
    }

    #[test]
    fn hover_drag_outside_the_rect_clamps_into_the_popup() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "x\n");
        h.stoat.pending_hover = Some(hover_sel_popup(&["hello world"], 0));

        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 12, 2));
        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            200,
            200,
        ));

        let sel = h.stoat.pending_hover.as_ref().unwrap().selection.unwrap();
        assert_eq!(sel.anchor, (0, 2));
        assert_eq!(
            sel.head,
            (0, 11),
            "a drag past the rect clamps to the last line and its char count",
        );
    }

    #[test]
    fn hover_selection_maps_through_the_scroll_offset() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "x\n");
        let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        // Interior height 5 => half_page 2; scroll 3 => scroll = min(15, 6) = 6.
        h.stoat.pending_hover = Some(hover_sel_popup(&refs, 3));

        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 2));

        let sel = h.stoat.pending_hover.as_ref().unwrap().selection.unwrap();
        assert_eq!(
            sel.anchor.0, 6,
            "the top row maps to the first scrolled line"
        );
    }

    #[test]
    fn hover_hit_test_inverts_the_stoatty_scale() {
        use crate::render::hover::HoverPopup;
        use ratatui::style::Style;

        let mut popup = HoverPopup::new(
            vec![vec![("x".repeat(60), Style::default())]],
            0,
            EditorId::default(),
        );
        popup.inner = Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 3,
        };
        for cell in 0..40u16 {
            let (line, col) = crate::render::hover::hover_hit_test(&popup, cell, 0);
            assert_eq!(line, 0);
            assert_eq!(col, (cell as usize * 256 + 128) / 218);
        }
    }

    #[test]
    fn hover_grid_highlight_paints_the_selection_bg() {
        use crate::{
            render::hover::{HoverPopup, HoverSelection},
            test_harness::TestHarness,
        };
        use ratatui::style::Style;

        let mut h = TestHarness::with_size(60, 20);
        let root = std::path::PathBuf::from("/hover");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        let mut popup = HoverPopup::new(
            vec![vec![("hello world".to_string(), Style::default())]],
            0,
            editor_id,
        );
        popup.selection = Some(HoverSelection {
            anchor: (0, 0),
            head: (0, 4),
            dragging: false,
        });
        h.stoat.pending_hover = Some(popup);

        let buf = h.stoat.render();
        let inner = h.stoat.pending_hover.as_ref().unwrap().inner;
        let sel_bg = h
            .stoat
            .theme
            .get(crate::theme::scope::UI_SELECTION)
            .bg
            .expect("theme has a selection background");

        for c in 0..4u16 {
            assert_eq!(
                buf[(inner.x + c, inner.y)].bg,
                sel_bg,
                "selected cell {c} carries the selection background",
            );
        }
        assert_ne!(
            buf[(inner.x + 5, inner.y)].bg,
            sel_bg,
            "a cell past the selection keeps the modal background",
        );
    }

    #[test]
    fn hover_y_yanks_the_live_selection() {
        use crate::{register::Register, render::hover::HoverSelection};

        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "x\n");
        h.stoat.pending_hover = Some(hover_sel_popup(&["hello world"], 0));
        if let Some(popup) = h.stoat.pending_hover.as_mut() {
            popup.selection = Some(HoverSelection {
                anchor: (0, 0),
                head: (0, 5),
                dragging: false,
            });
        }

        h.type_keys("y");

        assert_eq!(
            h.stoat.registers.read(Register::Unnamed),
            Some(["hello".to_string()].as_slice()),
            "y yanks the selected text into the register",
        );
        assert!(
            h.stoat.pending_hover.is_some(),
            "the popup and selection stay open after a yank",
        );
    }

    #[test]
    fn hover_y_without_a_selection_closes_the_popup() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "x\n");
        h.stoat.pending_hover = Some(hover_sel_popup(&["hello world"], 0));

        h.type_keys("y");

        assert!(
            h.stoat.pending_hover.is_none(),
            "y with no selection closes the popup like any other key",
        );
    }

    #[test]
    fn hover_drag_under_stoatty_maps_through_the_apc_scale() {
        let mut h = Stoat::test();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        let _ = open_scratch_file(&mut h, "x\n");
        let long = "x".repeat(40);
        h.stoat.pending_hover = Some(hover_sel_popup(&[&long], 0));

        // inner.x is 10. A pointer 10 cells in maps through the 0.85x scale.
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 2));
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 20, 2));

        let sel = h.stoat.pending_hover.as_ref().unwrap().selection.unwrap();
        assert_eq!(sel.anchor, (0, 0));
        assert_eq!(
            sel.head.1,
            (10 * 256 + 128) / 218,
            "the drag column maps through the stoatty 256/218 inverse",
        );
    }

    #[test]
    fn space_a_z_widens_and_restores_the_focused_pane() {
        let mut h = Stoat::test();
        let full_width = {
            let panes = &h.stoat.active_workspace().panes;
            panes.pane(panes.focus()).area.width
        };

        h.type_keys("space a s");
        assert_eq!(h.stoat.active_workspace().panes.pane_count(), 2);

        h.type_keys("space a z");
        {
            let panes = &h.stoat.active_workspace().panes;
            let focused = panes.focus();
            assert_eq!(panes.widened(), Some(focused));
            assert_eq!(
                panes.pane(focused).area.width,
                full_width,
                "the widened pane spans full width"
            );
        }
        assert_eq!(h.stoat.pending_message.as_deref(), Some("pane widened"));

        h.type_keys("space a z");
        assert_eq!(
            h.stoat.active_workspace().panes.widened(),
            None,
            "toggling again restores the layout"
        );
        assert_eq!(h.stoat.pending_message.as_deref(), Some("pane widen off"));
    }

    #[test]
    fn space_a_z_reports_when_the_layout_blocks_widen() {
        let mut h = Stoat::test();
        h.type_keys("space a s");
        h.type_keys("space a v");
        h.type_keys("space a k");
        h.type_keys("space a z");

        assert_eq!(h.stoat.active_workspace().panes.widened(), None);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("cannot widen: pane edges don't align"),
        );
    }

    #[test]
    fn detached_focus_parks_no_primary_cursor() {
        let mut h = Stoat::test();
        h.stoat.window_ipc_connected = true;

        let path = std::path::PathBuf::from("/w/a.txt");
        h.fake_fs().insert_file(&path, b"hello\n");
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.resize(80, 24);
        h.type_action("SplitRight()");
        h.settle();

        // Stand in for the live paint's recorded cursor cell so the split-pane
        // baseline is Some. The windowed check must return None regardless of it.
        let (editor_id, _) = h.stoat.focused_editor_ids().expect("focused editor");
        h.stoat
            .active_workspace_mut()
            .editors
            .get_mut(editor_id)
            .unwrap()
            .cursor_screen_cell = Some((7, 3));
        assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((7, 3)));

        h.type_action("DetachPane()");
        assert_eq!(
            h.stoat.primary_cursor_screen_pos(),
            None,
            "a detached focus draws its cursor in its window, not the primary"
        );
    }

    fn window_region() -> PoolRegionCommand {
        PoolRegionCommand {
            pool: 1,
            top: 0,
            left: 0,
            width: 80,
            height: 23,
            window: 2,
        }
    }

    /// Every input the terminal render reads has to reach the version, because
    /// the version is what decides whether the render runs at all. A field left
    /// out shows the user a stale pane until something else happens to move.
    #[test]
    fn a_terminal_pane_versions_on_everything_its_render_reads() {
        let mut h = Stoat::test();
        let session: Arc<dyn crate::host::TerminalSession> =
            Arc::new(crate::host::FakeTerminalSession::new());
        let term_id = h
            .stoat
            .active_workspace_mut()
            .terms
            .insert(TermSession::new(
                crate::term_screen::TermScreen::new(24, 80),
                session,
            ));

        let view = View::Terminal(term_id);
        let region = window_region();
        let version = |ws: &Workspace, focused, epoch, region| {
            window_content_version(&view, region, focused, epoch, ws)
                .expect("a terminal is versioned")
        };

        let ws = h.stoat.active_workspace();
        let base = version(ws, true, 0, region);
        assert_eq!(
            base,
            version(ws, true, 0, region),
            "an untouched terminal holds still"
        );
        assert_ne!(
            base,
            version(ws, false, 0, region),
            "focus draws the cursor cell"
        );
        assert_ne!(
            base,
            version(ws, true, 1, region),
            "the theme recolors every cell"
        );
        assert_ne!(
            base,
            version(
                ws,
                true,
                0,
                PoolRegionCommand {
                    height: 12,
                    ..region
                }
            ),
            "a resized pane must re-declare its region"
        );

        h.stoat.active_workspace_mut().terms[term_id].selection = Some(TermSelection::new(1, 1));
        let selected = version(h.stoat.active_workspace(), true, 0, region);
        assert_ne!(base, selected, "a selection tints the cells it covers");

        h.stoat.active_workspace_mut().terms[term_id]
            .term
            .feed(b"output");
        assert_ne!(
            selected,
            version(h.stoat.active_workspace(), true, 0, region),
            "output repaints the screen"
        );
    }

    /// Every input the run render reads has to reach the version, on the same
    /// terms as the terminal sibling above.
    #[test]
    fn a_run_pane_versions_on_everything_its_render_reads() {
        let mut h = Stoat::test();
        let exec = h.stoat.executor.clone();
        let run_id = {
            let ws = h.stoat.active_workspace_mut();
            let state = crate::run::RunState::new(PathBuf::from("/work"), ws, exec);
            ws.runs.insert(state)
        };

        let view = View::Run(run_id);
        let region = window_region();
        let version = |ws: &Workspace| {
            window_content_version(&view, region, true, 0, ws).expect("a run pane is versioned")
        };

        let base = version(h.stoat.active_workspace());
        assert_eq!(
            base,
            version(h.stoat.active_workspace()),
            "an idle run pane holds still"
        );

        h.stoat.active_workspace_mut().runs[run_id]
            .blocks
            .push(crate::run::OutputBlock::new(
                "ls".into(),
                PathBuf::from("/work"),
                80,
            ));
        let submitted = version(h.stoat.active_workspace());
        assert_ne!(base, submitted, "a submitted command adds a prompt line");

        h.stoat.active_workspace_mut().runs[run_id].blocks[0].feed(b"a.txt\n");
        let fed = version(h.stoat.active_workspace());
        assert_ne!(submitted, fed, "output fills the block's grid");

        h.stoat.active_workspace_mut().runs[run_id].blocks[0].exit_status = Some(1);
        let exited = version(h.stoat.active_workspace());
        assert_ne!(fed, exited, "the exit code flags the next prompt");

        h.stoat.active_workspace_mut().runs[run_id].scroll_offset = 3;
        let scrolled = version(h.stoat.active_workspace());
        assert_ne!(exited, scrolled, "scrolling picks different output lines");

        let input_editor = h.stoat.active_workspace().runs[run_id].input.editor_id;
        h.stoat.active_workspace_mut().editors[input_editor].scroll_row = 1;
        assert_ne!(
            scrolled,
            version(h.stoat.active_workspace()),
            "the input line scrolls under a long command"
        );
    }

    /// A view whose sources are untracked has to say so, since the caller reads
    /// `None` as "paint it and hash what came out" rather than "unchanged".
    #[test]
    fn an_untracked_view_kind_reports_no_input_version() {
        let h = Stoat::test();
        assert_eq!(
            window_content_version(
                &View::Label("scratch".into()),
                window_region(),
                true,
                0,
                h.stoat.active_workspace(),
            ),
            None
        );
    }

    fn stoat_with_detached_editor(lines: usize) -> (crate::test_harness::TestHarness, PaneId, u32) {
        let mut h = Stoat::test();
        h.stoat.window_ipc_connected = true;
        let root = PathBuf::from("/aux-mouse");
        let path = root.join("a.txt");
        let body = (0..lines)
            .map(|i| format!("line {i}\n"))
            .collect::<String>();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.resize(80, 24);
        h.type_action("SplitRight()");
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        h.type_action("DetachPane()");
        h.settle();
        let (detached, window) = h.stoat.active_workspace().panes.windowed_panes()[0];
        (h, detached, window)
    }

    #[test]
    fn aux_click_lands_in_the_bound_pane_not_a_primary() {
        let (mut h, detached, window) = stoat_with_detached_editor(40);

        // Focus a primary split pane, so the click can only reach the detached
        // pane by resolving the window binding, never the grid hit-test.
        action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusPane { index: 1 });
        assert_ne!(
            h.stoat.active_workspace().panes.focus(),
            detached,
            "a primary pane holds focus before the aux click"
        );

        h.stoat
            .handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Mouse {
                window,
                kind: MouseKind::Press(IpcMouseButton::Left),
                col: 3,
                row: 2,
                mods: 0,
            }));

        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            detached,
            "the aux click resolves the bound pane, not a primary pane whose rect overlaps"
        );
        assert!(
            h.stoat.editor_drag.is_some(),
            "the click placed a block cursor in the detached editor and armed drag"
        );
    }

    #[test]
    fn aux_wheel_scrolls_the_bound_editor() {
        let (mut h, detached, window) = stoat_with_detached_editor(200);
        let View::Editor(editor_id) = h.stoat.active_workspace().panes.pane(detached).view else {
            panic!("detached pane is an editor");
        };
        let before = h
            .stoat
            .active_workspace()
            .editors
            .get(editor_id)
            .unwrap()
            .scroll_row;

        h.stoat
            .handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Mouse {
                window,
                kind: MouseKind::WheelDown,
                col: 3,
                row: 2,
                mods: 0,
            }));

        let after = h
            .stoat
            .active_workspace()
            .editors
            .get(editor_id)
            .unwrap()
            .scroll_row;
        assert!(
            after > before,
            "the aux wheel advances the detached editor's scroll target"
        );
    }

    /// A wheel notch moves the scroll row, which the inlay-hint request keys
    /// on, so a flick used to arm and cancel a request per notch and throw
    /// every one away. Only the viewport the glide lands on is worth asking
    /// about, and the settle is where it asks, since a frame tick never reaches
    /// the trigger epilogue at the end of `update`.
    #[test]
    fn a_wheel_glide_requests_inlay_hints_once_at_the_settle() {
        use lsp_types::{OneOf, ServerCapabilities};

        let mut h = Stoat::test();
        h.fake_lsp().set_capabilities(ServerCapabilities {
            inlay_hint_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });

        let root = PathBuf::from("/glide-hints");
        let path = root.join("a.rs");
        let body: String = (0..400).map(|i| format!("let x{i} = 1\n")).collect();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        h.type_keys("space l h");
        h.advance_clock(std::time::Duration::from_millis(150));
        let resting = h
            .stoat
            .last_inlay_hint_key
            .expect("enabling hints requested the resting viewport");

        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            for _ in 0..5 {
                action_handlers::movement::wheel_scroll(editor, true);
            }
        }
        // The trigger every one of those notches ran through, had the glide not
        // held it back.
        action_handlers::lsp::inlay_hints_trigger(&mut h.stoat);
        assert_eq!(
            h.stoat.last_inlay_hint_key,
            Some(resting),
            "a glide in flight requests nothing, however far the rows moved",
        );

        for _ in 0..1000 {
            let animating = h.stoat.is_animating();
            h.stoat.frame_tick(0.016);
            if !animating {
                break;
            }
        }
        assert!(!h.stoat.is_animating(), "the glide settles");

        let landed = h
            .stoat
            .last_inlay_hint_key
            .expect("the settle requested once");
        assert_ne!(landed, resting, "the landed viewport is what finally asks",);
        assert_eq!(
            landed.2,
            action_handlers::focused_editor_mut(&mut h.stoat)
                .expect("focused editor")
                .scroll_row,
            "and it asks about the row the glide landed on",
        );
    }

    #[test]
    fn spinner_phase_advances_and_wraps() {
        assert_eq!(spinner_phase(0.0), 0);
        assert_eq!(spinner_phase(0.05), 0, "within the first frame window");
        assert_eq!(spinner_phase(0.15), 1, "second frame");
        assert_eq!(spinner_phase(0.95), 9, "last frame of the cycle");
        assert_eq!(spinner_phase(1.05), 0, "wraps to the first frame");
        assert_eq!(spinner_phase(1.15), 1);
    }

    #[test]
    fn frame_tick_repaints_the_spinner_only_when_the_phase_advances() {
        use crate::host::LspNotification;
        use lsp_types::{NumberOrString, WorkDoneProgress, WorkDoneProgressBegin};
        let mut h = Stoat::test();
        h.fake_lsp().push_notification(LspNotification::Progress {
            token: NumberOrString::Number(1),
            value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "indexing".into(),
                cancellable: None,
                message: None,
                percentage: None,
            }),
        });
        h.drain_lsp();
        assert!(h.stoat.lsp_progress.current().is_some(), "progress is live");

        assert_eq!(
            h.stoat.frame_tick(0.1),
            UpdateEffect::Redraw,
            "a full frame interval advances the phase and repaints"
        );

        h.stoat.spinner_clock = 0.0;
        assert_eq!(
            h.stoat.frame_tick(0.01),
            UpdateEffect::None,
            "a sub-frame tick leaves the phase put, so no repaint"
        );
    }

    #[test]
    fn snapshot_lsp_progress_indexing() {
        use crate::{action_handlers::dispatch, host::LspNotification};
        use lsp_types::{NumberOrString, WorkDoneProgress, WorkDoneProgressBegin};
        let mut h = Stoat::test();
        h.fake_lsp().push_notification(LspNotification::Progress {
            token: NumberOrString::Number(1),
            value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "indexing".into(),
                cancellable: None,
                message: None,
                percentage: Some(25),
            }),
        });
        h.drain_lsp();
        dispatch(&mut h.stoat, &stoat_action::ToggleLspStatus);
        h.assert_snapshot("lsp_progress_indexing");
    }

    #[test]
    fn error_show_message_wraps_into_a_popout_above_the_bar() {
        use lsp_types::MessageType;
        let mut h = Stoat::test();
        let msg = "rust-analyzer failed to load the workspace: Cargo.toml is malformed and could not be parsed, so diagnostics are unavailable";
        h.stoat.lsp_message = Some((MessageType::ERROR, msg.to_string()));

        let buf = h.render_composited();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let bar = rows.len() - 1;

        assert!(
            !rows[bar].contains("rust-analyzer"),
            "the bar row no longer carries the error text"
        );
        let head = rows
            .iter()
            .position(|r| r.contains("rust-analyzer"))
            .expect("error head painted");
        let tail = rows
            .iter()
            .position(|r| r.contains("diagnostics"))
            .expect("error tail painted");
        assert!(tail < bar, "the popout sits above the bar");
        assert!(tail > head, "the long message wrapped onto a second row");
    }

    #[test]
    fn warning_show_message_still_paints_in_the_bar() {
        use lsp_types::MessageType;
        let mut h = Stoat::test();
        h.stoat.lsp_message = Some((MessageType::WARNING, "cargo check is slow".to_string()));

        let buf = h.render_composited();
        let bar = buf.area.height - 1;
        let bar_row: String = (0..buf.area.width)
            .map(|x| buf[(x, bar)].symbol())
            .collect();

        assert!(
            bar_row.replace('─', " ").contains("cargo check is slow"),
            "a warning keeps painting in the status bar:\n{bar_row}"
        );
    }

    #[test]
    fn snapshot_lsp_show_message_error() {
        use crate::host::LspNotification;
        use lsp_types::MessageType;
        let mut h = Stoat::test();
        h.fake_lsp()
            .push_notification(LspNotification::ShowMessage {
                typ: MessageType::ERROR,
                message: "rust-analyzer failed to load".to_string(),
            });
        h.drain_lsp();
        h.assert_snapshot("lsp_show_message_error");
    }

    #[test]
    fn user_config_overrides_embedded_setting() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let stoat = Stoat::new_with_user_config(
            scheduler.executor(),
            Settings::default(),
            PathBuf::new(),
            Some("on init { format_on_save = true; }".to_string()),
            Vec::new(),
            None,
        );

        assert_eq!(stoat.settings.format_on_save, Some(true));
        assert_eq!(
            stoat.pending_message, None,
            "a clean parse shows no message"
        );
    }

    #[test]
    fn user_theme_block_layers_over_embedded_base() {
        use crate::theme::scope::{UI_MODAL_PALETTE, UI_TEXT};
        use ratatui::style::Color;

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let base = Stoat::new_with_user_config(
            scheduler.executor(),
            Settings::default(),
            PathBuf::new(),
            None,
            Vec::new(),
            None,
        );
        let layered = Stoat::new_with_user_config(
            scheduler.executor(),
            Settings::default(),
            PathBuf::new(),
            Some("theme default_dark { ui.modal.palette.fg = \"#ff0000\"; }".to_string()),
            Vec::new(),
            None,
        );

        let red = Some(Color::Rgb(255, 0, 0));
        assert_eq!(
            layered.theme.get(UI_MODAL_PALETTE).fg,
            red,
            "the user override recolors the modal palette border",
        );
        assert_ne!(
            base.theme.get(UI_MODAL_PALETTE).fg,
            red,
            "the embedded base is not already red on its own",
        );
        assert_eq!(
            layered.theme.get(UI_TEXT).fg,
            base.theme.get(UI_TEXT).fg,
            "a scope the user did not touch keeps the embedded value",
        );
    }

    #[test]
    fn user_only_theme_name_resolves() {
        use crate::theme::scope::UI_TEXT;
        use ratatui::style::Color;

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let stoat = Stoat::new_with_user_config(
            scheduler.executor(),
            Settings::default(),
            PathBuf::new(),
            Some("theme mine { ui.text.fg = \"#00ff00\"; }\non init { theme = mine; }".to_string()),
            Vec::new(),
            None,
        );

        assert_eq!(stoat.theme.name, "mine", "the user-only theme activates");
        assert_eq!(
            stoat.theme.get(UI_TEXT).fg,
            Some(Color::Rgb(0, 255, 0)),
            "its scopes resolve without an embedded base of the same name",
        );
    }

    #[test]
    fn broken_user_config_falls_back_to_embedded_with_status() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let stoat = Stoat::new_with_user_config(
            scheduler.executor(),
            Settings::default(),
            PathBuf::new(),
            Some("on init { format_on_save = ".to_string()),
            Vec::new(),
            None,
        );

        assert_eq!(
            stoat.settings.format_on_save,
            Some(false),
            "the embedded default survives a broken user config"
        );
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("user config parse failed; using built-in defaults")
        );
    }

    #[test]
    fn user_vscode_theme_joins_the_pool() {
        use ratatui::style::Color;

        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let user_themes = vec![(
            "my-gruvbox".to_string(),
            r##"{ "name": "my-gruvbox", "type": "dark", "colors": { "editor.background": "#282828" } }"##
                .to_string(),
        )];
        let stoat = Stoat::new_with_user_config(
            scheduler.executor(),
            Settings::default(),
            PathBuf::new(),
            None,
            user_themes,
            None,
        );

        assert!(
            stoat.theme_pool.contains("my-gruvbox"),
            "the user theme joins the pool",
        );
        assert!(
            stoat.theme_pool.contains("gruvbox-dark"),
            "the built-in themes join the pool too",
        );

        let theme = stoat
            .theme_pool
            .resolve("my-gruvbox")
            .expect("theme resolves");
        assert_eq!(
            theme.get("ui.background").bg,
            Some(Color::Rgb(0x28, 0x28, 0x28))
        );
    }

    #[test]
    fn only_the_resolved_theme_converts() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let user_themes = vec![(
            "unused".to_string(),
            r##"{ "name": "unused", "colors": { "editor.background": "#282828" } }"##.to_string(),
        )];
        let mut stoat = Stoat::new_with_user_config(
            scheduler.executor(),
            Settings::default(),
            PathBuf::new(),
            None,
            user_themes,
            None,
        );

        assert_eq!(
            stoat.theme.name, "default_dark",
            "an embedded theme is active"
        );
        assert!(
            stoat.imported_themes.iter().all(|t| !t.is_converted()),
            "startup resolves an embedded theme, so no VSCode theme is converted",
        );

        action_handlers::dispatch(
            &mut stoat,
            &stoat_action::SetTheme {
                name: "unused".to_string(),
            },
        );
        let converted: Vec<&str> = stoat
            .imported_themes
            .iter()
            .filter(|t| t.is_converted())
            .map(|t| t.name())
            .collect();
        assert_eq!(
            converted,
            ["unused"],
            "selecting a theme converts that theme and no other",
        );
    }

    #[test]
    fn broken_user_theme_surfaces_when_selected() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let user_themes = vec![("bad".to_string(), "{ not json".to_string())];
        let mut stoat = Stoat::new_with_user_config(
            scheduler.executor(),
            Settings::default(),
            PathBuf::new(),
            None,
            user_themes,
            None,
        );

        assert_eq!(
            stoat.pending_message, None,
            "an unselected theme is never read, so startup reports nothing",
        );
        assert!(
            stoat.theme_pool.contains("bad"),
            "the theme is listed by file stem, which reading it cannot change",
        );

        let before = stoat.theme.name.clone();
        action_handlers::dispatch(
            &mut stoat,
            &stoat_action::SetTheme {
                name: "bad".to_string(),
            },
        );
        assert!(
            stoat
                .pending_message
                .as_deref()
                .unwrap_or_default()
                .contains("theme bad failed"),
            "selecting it surfaces the failure in the transient status: {:?}",
            stoat.pending_message,
        );
        assert_eq!(stoat.theme.name, before, "the active theme is kept");
    }

    fn stoat_with_env_theme(user_config: Option<&str>, cli: Settings, env: &str) -> Stoat {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        Stoat::new_with_user_config(
            scheduler.executor(),
            cli,
            PathBuf::new(),
            user_config.map(str::to_string),
            Vec::new(),
            Some(env.to_string()),
        )
    }

    #[test]
    fn env_theme_beats_the_embedded_default() {
        let stoat = stoat_with_env_theme(None, Settings::default(), "gruvbox-dark");

        assert_eq!(
            stoat.theme.name, "gruvbox-dark",
            "with nothing explicit set, the environment names the theme"
        );
    }

    #[test]
    fn env_theme_applies_when_the_user_config_names_no_theme() {
        let stoat = stoat_with_env_theme(
            Some("on init { format_on_save = true; }"),
            Settings::default(),
            "gruvbox-dark",
        );

        assert_eq!(
            stoat.theme.name, "gruvbox-dark",
            "a user config that sets other settings does not claim the theme"
        );
    }

    #[test]
    fn user_config_theme_beats_the_env_theme() {
        let stoat = stoat_with_env_theme(
            Some("theme mine { ui.text.fg = \"#00ff00\"; }\non init { theme = mine; }"),
            Settings::default(),
            "gruvbox-dark",
        );

        assert_eq!(
            stoat.theme.name, "mine",
            "an explicit user-config theme outranks the environment"
        );
    }

    #[test]
    fn cli_theme_beats_the_env_theme() {
        let cli = Settings {
            theme: Some("one-dark".to_string()),
            ..Settings::default()
        };
        let stoat = stoat_with_env_theme(None, cli, "gruvbox-dark");

        assert_eq!(
            stoat.theme.name, "one-dark",
            "an explicit CLI theme outranks the environment"
        );
    }

    #[test]
    fn unknown_env_theme_keeps_the_embedded_default() {
        let stoat = stoat_with_env_theme(None, Settings::default(), "no-such-theme");

        assert_eq!(
            stoat.theme.name, "default_dark",
            "an unresolvable env theme is ignored rather than blanking the theme"
        );
        assert!(
            stoat.theme.try_get("ui.cursor").is_some(),
            "the default theme's caret style survives an unresolvable env theme"
        );
    }

    #[test]
    fn lsp_message_clears_on_key() {
        use crate::host::LspNotification;
        use lsp_types::MessageType;
        let mut h = Stoat::test();
        h.fake_lsp()
            .push_notification(LspNotification::ShowMessage {
                typ: MessageType::INFO,
                message: "checking".to_string(),
            });
        h.drain_lsp();
        assert_eq!(
            h.stoat.lsp_message,
            Some((MessageType::INFO, "default: checking".to_string())),
            "the stored message is attributed to the reporting server",
        );
        h.type_keys("<Esc>");
        assert!(h.stoat.lsp_message.is_none(), "any key retires the message");
    }

    #[test]
    fn status_message_survives_a_later_keypress() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 12);
        h.stoat.set_status("saved");
        assert_eq!(h.stoat.pending_message.as_deref(), Some("saved"));

        h.type_keys("<Esc>");

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("saved"),
            "input no longer clears the status message",
        );
    }

    #[test]
    fn status_message_expires_after_its_ttl() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 12);
        h.stoat.set_status("saved");

        h.stoat.render();
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("saved"),
            "the message stays visible before its ttl elapses",
        );

        h.advance_clock(STATUS_MESSAGE_TTL);
        h.stoat.render();

        assert_eq!(
            h.stoat.pending_message, None,
            "the message retires once its ttl elapses and a frame renders",
        );
    }

    #[test]
    fn diagnostics_notification_updates_store() {
        use crate::host::LspNotification;
        use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Uri};
        use std::{path::PathBuf, str::FromStr};
        let mut h = Stoat::test();
        let path = PathBuf::from("/ws/a.rs");
        let uri = Uri::from_str(&format!("file://{}", path.display())).unwrap();
        let diag = Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 5)),
            severity: Some(DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: None,
            message: "boom".into(),
            related_information: None,
            tags: None,
            data: None,
        };
        h.fake_lsp()
            .push_notification(LspNotification::Diagnostics {
                uri,
                diagnostics: vec![diag],
                version: None,
            });
        h.drain_lsp();
        let summary = h.stoat.diagnostics.summarize(&path);
        assert_eq!(summary.error, 1);
        assert_eq!(summary.worst, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn update_effect_merge_keeps_most_urgent() {
        let none = UpdateEffect::None;
        let redraw = UpdateEffect::Redraw;
        let quit = UpdateEffect::Quit;
        assert_eq!(none.merge(redraw), redraw);
        assert_eq!(redraw.merge(none), redraw);
        assert_eq!(redraw.merge(quit), quit);
        assert_eq!(quit.merge(redraw), quit);
        assert_eq!(none.merge(none), none);
    }

    #[test]
    fn drain_pending_applies_every_queued_event() {
        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        for size in [(80u16, 24u16), (100, 30), (120, 40)] {
            tx.send(Event::Resize(size.0, size.1)).unwrap();
        }
        let (effect, coalesced) = h.stoat.drain_pending(&mut rx);
        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(coalesced, 3, "all three queued events counted");
        assert_eq!(h.stoat.size(), Rect::new(0, 0, 120, 40));
        assert!(rx.try_recv().is_err(), "drain must empty the channel");
    }

    /// A backlog deeper than any queue bound still lands in one drain, with
    /// every send returning rather than waiting for room.
    ///
    /// The sender is the UI thread, inside the same loop that flushes frames
    /// and polls stdin. A send that waited would park that loop, and with it
    /// the only reader of fd 0, so the backpressure would land in the kernel's
    /// tty buffer where an overflow tears escape sequences into garbage. Mouse
    /// capture makes a depth like this ordinary rather than exotic, since
    /// pointer motion alone produces hundreds of events a second.
    #[test]
    fn a_backlog_past_any_bound_queues_without_waiting() {
        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();

        // Well past the 64 the channel used to hold, and past any bound that
        // would replace it.
        let sizes: Vec<(u16, u16)> = (0..500).map(|i| (80 + i % 40, 24 + i % 20)).collect();
        for &(width, rows) in &sizes {
            tx.send(Event::Resize(width, rows))
                .expect("a queued send never waits for room");
        }

        let (effect, coalesced) = h.stoat.drain_pending(&mut rx);
        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(coalesced, sizes.len(), "the whole backlog applies at once");

        let (width, rows) = *sizes.last().expect("fixture");
        assert_eq!(
            h.stoat.size(),
            Rect::new(0, 0, width, rows),
            "and the last one queued is the one left standing",
        );
    }

    #[test]
    fn open_run_spawns_shell_with_echo_disabled() {
        let mut h = Stoat::test();
        let run_id = h.open_run();

        assert_eq!(
            h.fake_terminal().sent_bytes().first().map(Vec::as_slice),
            Some(b"stty -echo\n".as_slice()),
            "eager spawn disables tty echo before anything else",
        );

        let input = h
            .stoat
            .active_workspace()
            .runs
            .get(run_id)
            .expect("run state exists")
            .input
            .clone();
        input.replace_text(h.stoat.active_workspace_mut(), "ls");
        action_handlers::dispatch(&mut h.stoat, &stoat_action::RunSubmit);

        let sent = h.fake_terminal().sent_strings();
        assert!(
            sent.get(1).is_some_and(|s| s.starts_with("ls\n")),
            "submit reuses the eager shell to send the command, got {sent:?}",
        );
    }

    #[test]
    fn osc7_updates_run_cwd() {
        let mut h = Stoat::test();
        let run_id = h.open_run();
        h.submit_run("cd /tmp");
        h.inject_run_output(run_id, b"\x1b]7;file:///tmp\x07");

        assert_eq!(
            h.stoat
                .active_workspace()
                .runs
                .get(run_id)
                .expect("run state")
                .cwd,
            std::path::PathBuf::from("/tmp"),
            "an OSC 7 report updates the run pane's cwd",
        );
    }

    #[test]
    fn snapshot_run_pane_prompt_blocks() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 12);
        let run_id = h.open_run();
        h.stoat
            .active_workspace_mut()
            .runs
            .get_mut(run_id)
            .expect("run state")
            .cwd = std::path::PathBuf::from("/work/proj");

        h.submit_run("true");
        h.inject_run_output(run_id, b"ok\n");
        h.inject_run_done(run_id, 0);

        h.submit_run("false");
        h.inject_run_output(run_id, b"boom\n");
        h.inject_run_done(run_id, 5);

        // The unfinished follow-up leaves both its prompt and the input row
        // showing the previous nonzero exit as a red [5].
        h.submit_run("retry");

        h.assert_snapshot("run_pane_prompt_blocks");
    }

    #[test]
    fn run_pane_abbreviates_cwd_under_home() {
        let paint = |home: Option<&str>| {
            let mut h = crate::test_harness::TestHarness::with_size(40, 12);
            if let Some(home) = home {
                h.fake_env().set("HOME", home);
                // The cached home is resolved when the env host is set, so
                // re-inject it now that HOME is populated.
                h.stoat.set_env_host(h.fake_env().clone());
            }
            let run_id = h.open_run();
            h.stoat
                .active_workspace_mut()
                .runs
                .get_mut(run_id)
                .expect("run state")
                .cwd = std::path::PathBuf::from("/home/tester/proj");
            h.submit_run("ls");
            h.rendered_text()
        };

        assert!(
            paint(Some("/home/tester")).contains("~/proj"),
            "a cwd under $HOME (resolved through EnvHost) paints the ~-abbreviated path",
        );

        let full = paint(None);
        assert!(
            full.contains("/h/t/proj"),
            "with no $HOME the prompt paints the plain path with ancestors abbreviated: {full:?}",
        );
        assert!(
            !full.contains("~/proj"),
            "with no $HOME the prompt does not ~-abbreviate",
        );
    }

    #[test]
    fn open_run_lands_in_insert_mode() {
        let mut h = Stoat::test();
        h.open_run();
        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "opening a run pane enters insert mode"
        );
    }

    #[test]
    fn run_pane_enter_binds_run_submit_through_keymap() {
        let mut h = Stoat::test();
        h.open_run();
        let state = StoatKeymapState::from_stoat(&h.stoat);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        let actions = h
            .stoat
            .keymap
            .lookup(&state, &enter)
            .expect("Enter is bound in a run pane");
        assert!(
            actions.iter().any(|a| a.name == "RunSubmit"),
            "run-pane Enter resolves to RunSubmit, got {actions:?}"
        );
    }

    #[test]
    fn editor_enter_is_unbound_so_it_inserts() {
        let mut h = Stoat::test();
        h.stoat.set_focused_mode("insert".into());
        let state = StoatKeymapState::from_stoat(&h.stoat);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        assert!(
            h.stoat.keymap.lookup(&state, &enter).is_none(),
            "editor Enter has no keymap binding, so it falls to the insert newline"
        );
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    /// Ctrl-C is unbound outside a run pane, so a modal missing from the close
    /// cascade falls through to the quit that ends the session.
    #[test]
    fn ctrl_c_closes_code_search_rather_than_quitting() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        assert!(h.stoat.code_search.is_some(), "the modal opened");

        let effect = h.stoat.handle_key(ctrl_c());

        assert!(
            !matches!(effect, UpdateEffect::Quit),
            "closing a modal must not take the session with it"
        );
        assert!(h.stoat.code_search.is_none(), "and the modal is closed");
    }

    /// A picker owning an input has to be disposed, not dropped: its scratch
    /// editor otherwise stays in the workspace for the rest of the session.
    #[test]
    fn ctrl_c_disposes_the_workspace_pickers_input() {
        let mut h = Stoat::test();
        let before = h.stoat.active_workspace().editors.len();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchWorkspace);
        assert!(h.stoat.workspace_picker.is_some(), "the picker opened");
        assert!(
            h.stoat.active_workspace().editors.len() > before,
            "which took an editor for its input"
        );

        h.stoat.handle_key(ctrl_c());

        assert!(h.stoat.workspace_picker.is_none(), "the picker closed");
        assert_eq!(
            h.stoat.active_workspace().editors.len(),
            before,
            "and gave its editor back"
        );
    }

    /// The transient text inputs carry no Ctrl-C binding of their own, so an
    /// input missing from the cascade quits the session out from under an edit
    /// in progress.
    #[test]
    fn ctrl_c_cancels_a_rename_rather_than_quitting() {
        use lsp_types::{Position, Uri};
        use std::str::FromStr;

        let mut h = Stoat::test();
        let before = h.stoat.active_workspace().editors.len();
        let executor = h.stoat.executor.clone();
        let input = InputView::create(
            h.stoat.active_workspace_mut(),
            executor,
            SubmitTarget::RenameSymbol,
            "old_name",
            "insert",
            1,
        );
        let buffer_id = input.buffer_id;
        h.stoat.rename_input = Some(RenameInputState {
            input,
            source_uri: Uri::from_str("file:///src/lib.rs").expect("valid uri"),
            symbol_position: Position::new(0, 0),
            anchor_offset: 0,
            server: None,
            buffer_id,
        });

        let effect = h.stoat.handle_key(ctrl_c());

        assert!(
            !matches!(effect, UpdateEffect::Quit),
            "cancelling a rename must not take the session with it"
        );
        assert!(h.stoat.rename_input.is_none(), "the rename is cancelled");
        assert_eq!(
            h.stoat.active_workspace().editors.len(),
            before,
            "and gave its editor back"
        );
    }

    #[test]
    fn ctrl_c_cancels_the_search_input_rather_than_quitting() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSearchInput);
        assert!(h.stoat.search_input.is_some(), "the input opened");

        let effect = h.stoat.handle_key(ctrl_c());

        assert!(
            !matches!(effect, UpdateEffect::Quit),
            "cancelling a search must not take the session with it"
        );
        assert!(h.stoat.search_input.is_none(), "the input is cancelled");
    }

    /// Quitting on an unbound Ctrl-C is the behavior the cascade arms carve out
    /// of, so it has to survive them.
    #[test]
    fn ctrl_c_with_no_modal_open_still_quits() {
        let mut h = Stoat::test();

        assert!(matches!(h.stoat.handle_key(ctrl_c()), UpdateEffect::Quit));
    }

    fn resolves(h: &crate::test_harness::TestHarness, key: &KeyEvent) -> bool {
        let state = StoatKeymapState::from_stoat(&h.stoat);
        h.stoat.keymap.lookup(&state, key).is_some()
    }

    fn space() -> KeyEvent {
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty())
    }

    /// A normal-mode modal binds only the keys it handles, so the editor's own
    /// normal-mode block has to stop applying while one is open. Otherwise Space
    /// opens a second modal over the picker and Ctrl-d scrolls the editor hidden
    /// behind it, leaving render painting one modal while keys route to another.
    #[test]
    fn normal_mode_bindings_stop_at_the_location_picker() {
        use crate::location_picker::{LocationEntry, LocationPicker};

        let mut h = Stoat::test();
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(resolves(&h, &space()), "Space starts a chord in the editor");
        assert!(resolves(&h, &ctrl_d), "and Ctrl-d scrolls it");

        h.stoat.location_picker = Some(LocationPicker::new(vec![LocationEntry {
            path: PathBuf::from("/repo/a.rs"),
            offset: 0,
            line: 1,
            column: 1,
            text: "candidate".to_owned(),
        }]));

        assert!(
            !resolves(&h, &space()),
            "Space starts no chord over the picker"
        );
        assert!(
            !resolves(&h, &ctrl_d),
            "and Ctrl-d does not scroll the editor behind it"
        );
    }

    #[test]
    fn space_chord_never_starts_over_the_quit_confirm() {
        let mut h = Stoat::test();
        h.stoat.quit_all_confirm = Some(QuitAllConfirm::new(&[], Path::new("/")));

        assert!(
            !resolves(&h, &space()),
            "without a chord start, `space a s` cannot split behind the prompt"
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "the editor is still in normal mode, so only the guard suppressed it"
        );
    }

    /// The guard must narrow when the block applies without changing how it
    /// ranks, or it ties the equally-specific view blocks and beats them on
    /// source order.
    #[test]
    fn a_view_block_still_outranks_the_guarded_normal_block() {
        let mut h = Stoat::test();
        h.seed_linear_history("/repo", &[("c1", "first", &[("a.rs", "fn a() {}\n")])]);
        h.open_commits("/repo");

        let state = StoatKeymapState::from_stoat(&h.stoat);
        let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty());
        let actions = h
            .stoat
            .keymap
            .lookup(&state, &j)
            .expect("j is bound on the commits screen");
        assert!(
            actions.iter().any(|a| a.name == "CommitsNext"),
            "the commits screen keeps j, got {actions:?}"
        );
    }

    #[test]
    fn run_pane_ctrl_c_interrupts_instead_of_quitting() {
        let mut h = Stoat::test();
        h.open_run();

        let state = StoatKeymapState::from_stoat(&h.stoat);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let actions = h
            .stoat
            .keymap
            .lookup(&state, &ctrl_c)
            .expect("Ctrl-C is bound in a run pane");
        assert!(
            actions.iter().any(|a| a.name == "RunInterrupt"),
            "run-pane Ctrl-C resolves to RunInterrupt, got {actions:?}"
        );

        let effect = h.stoat.handle_key(ctrl_c);
        assert!(
            !matches!(effect, UpdateEffect::Quit),
            "a bound Ctrl-C routes to the keymap rather than quitting"
        );
    }

    #[test]
    fn finished_modal_run_escape_dismisses_via_keymap() {
        let mut h = Stoat::test();
        let executor = h.stoat.executor.clone();
        let run_id = {
            let ws = h.stoat.active_workspace_mut();
            let run = crate::run::RunState::new(std::path::PathBuf::from("/tmp"), ws, executor);
            ws.runs.insert(run)
        };
        h.stoat.modal_run = Some(run_id);

        // A fresh run has no in-flight block, so it reads as finished.
        h.stoat
            .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

        assert!(
            h.stoat.modal_run.is_none(),
            "Escape on a finished modal run dismisses it"
        );
        assert!(
            h.stoat.active_workspace().runs.get(run_id).is_none(),
            "the dismissed run is removed from the registry"
        );
    }

    #[test]
    fn run_enter_submits_from_insert_and_normal() {
        let mut h = Stoat::test();
        let fake = h.fake_terminal().clone();
        h.open_run();

        h.type_text("ls");
        h.type_keys("enter");
        assert!(
            fake.sent_strings().iter().any(|s| s.starts_with("ls\n")),
            "insert-mode Enter submits, sent {:?}",
            fake.sent_strings(),
        );

        h.type_text("pwd");
        h.type_keys("esc");
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "Escape leaves insert mode"
        );
        h.type_keys("enter");
        assert!(
            fake.sent_strings().iter().any(|s| s.starts_with("pwd\n")),
            "normal-mode Enter submits, sent {:?}",
            fake.sent_strings(),
        );
    }

    #[test]
    fn run_up_recalls_history() {
        let mut h = Stoat::test();
        let run_id = h.open_run();

        h.type_text("ls");
        h.type_keys("enter");
        h.type_keys("up");

        let ws = h.stoat.active_workspace();
        let run_state = ws.runs.get(run_id).expect("run state exists");
        assert_eq!(
            run_state.input.text(ws),
            "ls",
            "Up recalls the last command"
        );
    }

    #[test]
    fn run_wheel_scrolls_output_and_clamps() {
        let mut h = Stoat::test();
        // 15 output rows in a 10-row pane (9 visible): output_line_total is 16
        // (prompt + 15 rows), so the top is reachable at offset 16 - 9 = 7.
        let output: Vec<u8> = (0..15)
            .flat_map(|i| format!("line{i}\n").into_bytes())
            .collect();
        let run_id = open_run_with_output(&mut h, &output);
        // Pin a short pane (the captures inside the helper re-layout it to the
        // full terminal) so the 16 output rows overflow the 9 visible rows.
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(0, 0, 40, 10);
        let offset = |h: &crate::test_harness::TestHarness| {
            h.stoat
                .active_workspace()
                .runs
                .get(run_id)
                .unwrap()
                .scroll_offset
        };

        h.stoat.update(mouse_event(MouseEventKind::ScrollUp, 1, 1));
        h.stoat.update(mouse_event(MouseEventKind::ScrollUp, 1, 1));
        h.stoat.update(mouse_event(MouseEventKind::ScrollUp, 1, 1));
        assert_eq!(offset(&h), 7, "scroll up steps by 3 and clamps at the top");

        h.stoat
            .update(mouse_event(MouseEventKind::ScrollDown, 1, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::ScrollDown, 1, 1));
        assert_eq!(offset(&h), 1, "scroll down steps by 3");

        h.stoat
            .update(mouse_event(MouseEventKind::ScrollDown, 1, 1));
        assert_eq!(offset(&h), 0, "scroll down floors at the tail");
    }

    #[test]
    fn run_submit_resets_scroll_offset() {
        let mut h = Stoat::test();
        let output: Vec<u8> = (0..15)
            .flat_map(|i| format!("line{i}\n").into_bytes())
            .collect();
        let run_id = open_run_with_output(&mut h, &output);
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(0, 0, 40, 10);

        h.stoat.update(mouse_event(MouseEventKind::ScrollUp, 1, 1));
        assert!(
            h.stoat
                .active_workspace()
                .runs
                .get(run_id)
                .unwrap()
                .scroll_offset
                > 0,
            "precondition: scrolled up off the tail",
        );

        let input = h
            .stoat
            .active_workspace()
            .runs
            .get(run_id)
            .unwrap()
            .input
            .clone();
        input.replace_text(h.stoat.active_workspace_mut(), "pwd");
        action_handlers::dispatch(&mut h.stoat, &stoat_action::RunSubmit);

        assert_eq!(
            h.stoat
                .active_workspace()
                .runs
                .get(run_id)
                .unwrap()
                .scroll_offset,
            0,
            "submitting snaps the output back to the prompt",
        );
    }

    fn open_run_with_output(h: &mut crate::test_harness::TestHarness, output: &[u8]) -> RunId {
        let run_id = h.open_run();
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(0, 0, 40, 10);
        h.submit_run("ls");
        h.inject_run_output(run_id, output);
        run_id
    }

    #[test]
    fn diff_view_anchors_lsp_popups_to_the_right_text_column() {
        let mut h = Stoat::test();
        open_scratch_file(&mut h, "keep\nnew\ntail\n");

        let (editor_id, buffer_id) = {
            let ws = h.stoat.active_workspace();
            let editor_id = match ws.panes.pane(ws.panes.focus()).view {
                View::Editor(id) => id,
                _ => panic!("focused pane is not an editor"),
            };
            (editor_id, ws.editors[editor_id].buffer_id)
        };
        {
            let base = "keep\nold\ntail\n";
            let text = "keep\nnew\ntail\n";
            let dm = crate::diff_map::DiffMap::from_structural_changes(
                stoat_language::structural_diff::diff(base, text),
                Arc::new(base.to_string()),
                text,
            );
            h.stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer")
                .write()
                .expect("poisoned")
                .diff_map = Some(dm);
        }

        // Column 2 of the first context line, in a pane wide enough for the
        // two-column diff layout.
        let content_area = Rect::new(0, 0, 120, 10);
        let anchor_offset = 2;

        let off = {
            let editor = h
                .stoat
                .active_workspace_mut()
                .editors
                .get_mut(editor_id)
                .expect("editor");
            editor.set_diff_view(false);
            crate::render::hover::cursor_screen_position(editor, content_area, anchor_offset)
        };
        assert_eq!(
            off,
            Some((content_area.x + 2, content_area.y)),
            "with diff off the popup anchors at the pane's left text column"
        );

        let on = {
            let editor = h
                .stoat
                .active_workspace_mut()
                .editors
                .get_mut(editor_id)
                .expect("editor");
            editor.set_diff_view(true);
            crate::render::hover::cursor_screen_position(editor, content_area, anchor_offset)
        };
        let right_text_x = crate::render::review::right_text_x(content_area);
        assert_eq!(
            on,
            Some((right_text_x + 2, content_area.y)),
            "with diff on the popup anchors at the right diff text column"
        );
    }

    #[test]
    fn opening_diff_view_jumps_cursor_to_the_first_hunk() {
        let mut h = Stoat::test();
        open_scratch_file(&mut h, "keep\nnew\ntail\n");

        let buffer_id = {
            let ws = h.stoat.active_workspace();
            match ws.panes.pane(ws.panes.focus()).view {
                View::Editor(id) => ws.editors[id].buffer_id,
                _ => panic!("focused pane is not an editor"),
            }
        };
        {
            let base = "keep\nold\ntail\n";
            let text = "keep\nnew\ntail\n";
            let dm = crate::diff_map::DiffMap::from_structural_changes(
                stoat_language::structural_diff::diff(base, text),
                Arc::new(base.to_string()),
                text,
            );
            h.stoat
                .active_workspace_mut()
                .install_test_diff_map(buffer_id, dm);
        }

        let cursor_row = |stoat: &mut Stoat| {
            let (buffer_id, offset) = stoat.focused_cursor_pos().expect("focused cursor");
            let ws = stoat.active_workspace();
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let guard = buffer.read().expect("poisoned");
            guard.rope().offset_to_point(offset).row
        };

        assert_eq!(cursor_row(&mut h.stoat), 0, "cursor starts at the top");

        h.stoat.toggle_diff_view();
        assert_eq!(
            cursor_row(&mut h.stoat),
            1,
            "opening the diff view lands the cursor on the first hunk",
        );

        h.stoat.toggle_diff_view();
        assert_eq!(
            cursor_row(&mut h.stoat),
            1,
            "toggling the view off leaves the cursor in place",
        );
    }

    /// Opens a scratch buffer with a small HEAD-vs-buffer diff installed, so
    /// `toggle_diff_view` finds a ready map and skips the on-demand compute.
    fn open_scratch_with_diff(h: &mut crate::test_harness::TestHarness) {
        open_scratch_file(h, "keep\nnew\ntail\n");
        let buffer_id = {
            let ws = h.stoat.active_workspace();
            match ws.panes.pane(ws.panes.focus()).view {
                View::Editor(id) => ws.editors[id].buffer_id,
                _ => panic!("focused pane is not an editor"),
            }
        };
        let base = "keep\nold\ntail\n";
        let text = "keep\nnew\ntail\n";
        let dm = crate::diff_map::DiffMap::from_structural_changes(
            stoat_language::structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
        );
        h.stoat
            .active_workspace_mut()
            .install_test_diff_map(buffer_id, dm);
    }

    #[test]
    fn opening_diff_view_widens_the_focused_pane() {
        let mut h = Stoat::test();
        open_scratch_with_diff(&mut h);
        h.type_keys("space a s");

        let (focused, other, focused_area, other_area) = {
            let panes = &h.stoat.active_workspace().panes;
            let focused = panes.focus();
            let other = panes
                .split_pane_ids()
                .into_iter()
                .find(|&id| id != focused)
                .expect("a second pane");
            (
                focused,
                other,
                panes.pane(focused).area,
                panes.pane(other).area,
            )
        };

        h.stoat.toggle_diff_view();
        {
            let panes = &h.stoat.active_workspace().panes;
            assert_eq!(
                panes.widened(),
                Some(focused),
                "opening the diff widens the focused pane"
            );
            assert!(
                panes.pane(focused).area.width > focused_area.width,
                "the widened pane grows past its split width"
            );
        }

        h.stoat.toggle_diff_view();
        {
            let panes = &h.stoat.active_workspace().panes;
            assert_eq!(panes.widened(), None, "closing the diff unwidens");
            assert_eq!(
                panes.pane(focused).area,
                focused_area,
                "the focused pane is restored"
            );
            assert_eq!(
                panes.pane(other).area,
                other_area,
                "the other pane is restored"
            );
        }
    }

    #[test]
    fn opening_diff_view_leaves_an_unwidenable_layout_put() {
        let mut h = Stoat::test();
        open_scratch_with_diff(&mut h);
        h.type_keys("space a s");
        h.type_keys("space a v");
        h.type_keys("space a k");

        let focused = h.stoat.active_workspace().panes.focus();
        h.stoat.toggle_diff_view();

        let ws = h.stoat.active_workspace();
        assert_eq!(
            ws.panes.widened(),
            None,
            "a layout with no clean cover is left unwidened"
        );
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        assert!(
            ws.editors[editor_id].diff_view,
            "the diff still opens even when the pane cannot widen"
        );
    }

    #[test]
    fn focusing_away_from_an_open_diff_unwidens_and_keeps_the_diff() {
        let mut h = Stoat::test();
        open_scratch_with_diff(&mut h);
        h.type_keys("space a s");

        let (focused, other, editor_id) = {
            let ws = h.stoat.active_workspace();
            let focused = ws.panes.focus();
            let other = ws
                .panes
                .split_pane_ids()
                .into_iter()
                .find(|&id| id != focused)
                .expect("a second pane");
            let editor_id = match ws.panes.pane(focused).view {
                View::Editor(id) => id,
                _ => panic!("focused pane is not an editor"),
            };
            (focused, other, editor_id)
        };

        h.stoat.toggle_diff_view();
        assert_eq!(h.stoat.active_workspace().panes.widened(), Some(focused));

        h.stoat.active_workspace_mut().panes.set_focus(other);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            ws.panes.widened(),
            None,
            "focusing another pane restores the layout"
        );
        assert!(
            ws.editors[editor_id].diff_view,
            "the diff view stays open on the original editor"
        );
    }

    #[test]
    fn opening_diff_view_scrolls_a_far_first_hunk_into_the_viewport() {
        let mut h = Stoat::test();
        let base: String = (0..30).map(|i| format!("line {i:02}\n")).collect();
        let text: String = (0..30)
            .map(|i| {
                if i == 20 {
                    "changed\n".to_string()
                } else {
                    format!("line {i:02}\n")
                }
            })
            .collect();
        open_scratch_file(&mut h, &text);

        let buffer_id = {
            let ws = h.stoat.active_workspace();
            match ws.panes.pane(ws.panes.focus()).view {
                View::Editor(id) => ws.editors[id].buffer_id,
                _ => panic!("focused pane is not an editor"),
            }
        };
        {
            let dm = crate::diff_map::DiffMap::from_structural_changes(
                stoat_language::structural_diff::diff(&base, &text),
                Arc::new(base.to_string()),
                &text,
            );
            h.stoat
                .active_workspace_mut()
                .install_test_diff_map(buffer_id, dm);
        }

        // A ten-row viewport with the first hunk (buffer row 20) far below it.
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            editor.viewport_rows = Some(10);
            editor.scroll_row = 0;
        }

        h.stoat.toggle_diff_view();

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let scroll_row = editor.scroll_row;
        let cursor_row = action_handlers::movement::cursor_display_row(editor);
        assert!(
            scroll_row > 0,
            "opening the diff view scrolled away from the top"
        );
        assert!(
            (scroll_row..scroll_row + 10).contains(&cursor_row),
            "the first hunk's display row {cursor_row} sits inside the viewport [{scroll_row}, {})",
            scroll_row + 10,
        );
    }

    #[test]
    fn stoat_review_opens_the_first_changed_file_on_its_first_hunk() {
        let mut h = Stoat::test();
        let workdir = PathBuf::from("/repo");
        h.stage_review_scenario(&workdir, &[("changed.rs", "a\nb\nc\n", "a\nX\nc\n")]);
        h.stoat.set_diff_warm_auto(true);

        // Mirrors the `stoat review` startup, where the diff view opens on the
        // pathless scratch and then crosses into the sole changed file.
        h.stoat.open_working_tree_diff();
        h.settle();

        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("focused editor");
        let path = {
            let ws = h.stoat.active_workspace();
            ws.buffers.path_for(buffer_id).map(|p| p.to_path_buf())
        };
        assert_eq!(
            path,
            Some(workdir.join("changed.rs")),
            "opened the first changed file",
        );
        assert!(
            action_handlers::focused_editor_mut(&mut h.stoat)
                .expect("editor")
                .diff_view,
            "the diff view is on for the opened file",
        );

        let (cursor_buffer, offset) = h.stoat.focused_cursor_pos().expect("focused cursor");
        let cursor_row = {
            let ws = h.stoat.active_workspace();
            let buffer = ws.buffers.get(cursor_buffer).expect("buffer");
            let guard = buffer.read().expect("poisoned");
            guard.rope().offset_to_point(offset).row
        };
        assert_eq!(cursor_row, 1, "the cursor sits on the file's first hunk");
    }

    #[test]
    fn diff_from_an_unchanged_file_crosses_into_the_first_changed_file() {
        let mut h = Stoat::test();
        let workdir = PathBuf::from("/repo");
        h.stage_review_scenario(&workdir, &[("changed.rs", "a\nb\nc\n", "a\nX\nc\n")]);
        // plain.rs is tracked but unchanged. Its HEAD content and working-tree
        // copy are identical, so it never appears in the changed list.
        h.fake_git()
            .add_repo(workdir.clone())
            .with_fs(h.fake_fs())
            .head_file("plain.rs", "one\ntwo\nthree\n");
        h.fake_fs()
            .insert_file(workdir.join("plain.rs"), b"one\ntwo\nthree\n");
        h.stoat.set_diff_warm_auto(true);

        action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: workdir.join("plain.rs"),
            },
        );
        h.settle();

        h.stoat.toggle_diff_view();
        h.settle();

        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("focused editor");
        let path = {
            let ws = h.stoat.active_workspace();
            ws.buffers.path_for(buffer_id).map(|p| p.to_path_buf())
        };
        assert_eq!(
            path,
            Some(workdir.join("changed.rs")),
            "diff from an unchanged file crosses into the first changed file",
        );
        assert!(
            action_handlers::focused_editor_mut(&mut h.stoat)
                .expect("editor")
                .diff_view,
            "the diff view is on for the crossed-into file",
        );

        let (cursor_buffer, offset) = h.stoat.focused_cursor_pos().expect("focused cursor");
        let cursor_row = {
            let ws = h.stoat.active_workspace();
            let buffer = ws.buffers.get(cursor_buffer).expect("buffer");
            let guard = buffer.read().expect("poisoned");
            guard.rope().offset_to_point(offset).row
        };
        assert_eq!(
            cursor_row, 1,
            "the cursor lands on the changed file's first hunk"
        );
    }

    #[test]
    fn stoat_review_with_no_changes_stays_on_the_scratch() {
        let mut h = Stoat::test();
        let workdir = PathBuf::from("/repo");
        h.stage_review_scenario(&workdir, &[]);
        h.stoat.set_diff_warm_auto(true);

        let scratch = h.stoat.focused_editor_ids().expect("editor").1;

        h.stoat.open_working_tree_diff();
        h.settle();

        assert_eq!(
            h.stoat.focused_editor_ids().expect("editor").1,
            scratch,
            "focus stays on the startup scratch when nothing changed",
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no more changes"),
            "the status reports that there are no changes",
        );
    }

    #[test]
    fn a_git_write_stales_open_diffs_with_no_review_and_no_precompute() {
        let mut h = Stoat::test();
        h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.settle_diff_jobs();

        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        assert!(
            h.stoat.active_workspace().diff_map_current(buffer_id),
            "the settled job leaves the buffer's diff current",
        );

        // Neither gate that used to arm the git-refresh debounce applies here.
        // No review session is open, and precompute is off.
        h.stoat.set_diff_warm_auto(false);
        assert!(h.stoat.active_workspace().review.is_none());

        h.fake_fs_watcher()
            .inject(Path::new("/repo/.git/HEAD"), FsEventKind::Modified);
        debounce::drain_fs_watch_events(&mut h.stoat);
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        assert!(
            !h.stoat.active_workspace().diff_map_current(buffer_id),
            "a .git write stales the open buffer's diff map even with no review and no precompute",
        );
    }

    #[test]
    fn minimap_click_scrolls_to_the_proportional_line() {
        let mut h = Stoat::test();
        let editor_id = open_with_minimap_strip(&mut h);

        // Strip cell row 5 of a fits-file (60 <= 10*8) points at line 5*8+4 = 44,
        // centered in the 20-row viewport -> scroll 34.
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 74, 5));

        let editor = &h.stoat.active_workspace().editors[editor_id];
        assert_eq!(
            editor.scroll_row, 34,
            "the click eases to the centered proportional row"
        );
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::Page,
            "the scrub glides like a page motion"
        );
        assert_eq!(
            h.stoat.minimap_drag,
            Some(editor_id),
            "the press arms the scrub"
        );
    }

    /// The strip draws one row per buffer line, so a click resolves to a buffer
    /// line and must be converted before it drives the display-row scroll. With
    /// every line wrapped in two, an unconverted target lands halfway up the
    /// file from the block the pointer was over.
    #[test]
    fn minimap_click_targets_the_clicked_buffer_line_when_wrapped() {
        let mut h = Stoat::test();
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        let body: String = (0..60)
            .map(|_| "w".repeat(100))
            .collect::<Vec<_>>()
            .join("\n");
        open_scratch_file(&mut h, &body);
        let editor_id = h.stoat.focused_editor_ids().expect("editor").0;
        let display_rows = {
            let editor = &mut h.stoat.active_workspace_mut().editors[editor_id];
            editor.minimap_rect = Some(Rect::new(72, 0, 8, 10));
            editor.viewport_rows = Some(20);
            editor.display_map.set_wrap_width(Some(60));
            editor.display_map.snapshot().line_count()
        };
        assert_eq!(display_rows, 120, "every line must wrap into two rows");

        // The 60-line file fits the strip's 10 * LINES_PER_CELL rows, so cell
        // row 5 points at buffer line 5*8+4.
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 74, 5));

        let editor = &mut h.stoat.active_workspace_mut().editors[editor_id];
        let scroll_row = editor.scroll_row;
        let centered = editor
            .display_map
            .snapshot()
            .display_to_buffer(DisplayPoint::new(scroll_row + 10, 0))
            .expect("a text row")
            .row;
        assert_eq!(
            (scroll_row, centered),
            (78, 44),
            "the click centers the buffer line under the pointer, not the display row"
        );
    }

    #[test]
    fn minimap_leaves_text_clicks_to_the_cursor() {
        let mut h = Stoat::test();
        h.stoat
            .active_workspace_mut()
            .panes
            .resize(Rect::new(0, 0, 80, 24));
        let editor_id = open_with_minimap_strip(&mut h);

        // A press in the text area, left of the strip, never arms the scrub.
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 4));

        assert_eq!(
            h.stoat.minimap_drag, None,
            "a text press does not arm the scrub"
        );
        assert_eq!(
            h.stoat.active_workspace().editors[editor_id].scroll_row,
            0,
            "a text press does not scroll the pane"
        );
        assert!(
            h.stoat
                .newest_cursor_offset(editor_id)
                .is_some_and(|o| o > 0),
            "the text press still moves the cursor off the buffer start"
        );
    }

    #[test]
    fn minimap_drag_scrolls_monotonically() {
        let mut h = Stoat::test();
        let editor_id = open_with_minimap_strip(&mut h);

        let mut rows = Vec::new();
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 74, 1));
        rows.push(h.stoat.active_workspace().editors[editor_id].scroll_row);
        for row in [3u16, 5, 7, 9] {
            h.stoat.update(mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                74,
                row,
            ));
            rows.push(h.stoat.active_workspace().editors[editor_id].scroll_row);
        }
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 74, 9));

        assert!(
            rows.windows(2).all(|w| w[1] >= w[0]),
            "dragging down the strip scrolls monotonically down: {rows:?}"
        );
        assert!(rows[4] > rows[0], "the drag moved the viewport: {rows:?}");
        assert_eq!(h.stoat.minimap_drag, None, "releasing clears the scrub");
    }

    #[test]
    fn single_band_click_scrubs_the_focused_editor_not_the_pane_under_it() {
        use stoat_config::MinimapMode;

        let mut h = Stoat::test();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
        h.resize(200, 24);

        let long: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let a = h.write_file("a.txt", &long);
        let b = h.write_file("b.txt", "short\n");
        h.open_file(&a);
        h.type_action("SplitRight()");
        h.open_file(&b);
        // Focus the left pane (the long buffer) while the band overlays the right.
        h.type_action("FocusLeft()");
        h.settle();
        let _ = h.stoat.render();

        let focused = h.stoat.focused_editor_ids().expect("focused editor").0;
        let band = h
            .stoat
            .single_minimap_rect
            .expect("single mode reserves a band");
        let col = band.x + band.width / 2;
        let row = band.y + band.height / 2;

        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            col,
            row,
        ));
        assert_eq!(
            h.stoat.minimap_drag,
            Some(focused),
            "the band press arms a scrub of the focused editor"
        );
        let after_click = h.stoat.active_workspace().editors[focused].scroll_row;
        assert!(
            after_click > 0,
            "the band click scrolls the focused editor toward the clicked line"
        );

        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            col,
            row + 2,
        ));
        let after_drag = h.stoat.active_workspace().editors[focused].scroll_row;
        assert!(
            after_drag >= after_click,
            "dragging down the band re-scrubs monotonically: {after_click} -> {after_drag}"
        );
        h.stoat.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            col,
            row + 2,
        ));
        assert_eq!(h.stoat.minimap_drag, None, "releasing clears the scrub");

        // In per-pane mode the same right-edge coordinates belong to the right
        // pane, so they never scrub the focused-left editor.
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        {
            let editor = h
                .stoat
                .active_workspace_mut()
                .editors
                .get_mut(focused)
                .expect("focused editor");
            editor.scroll_row = 0;
            editor.scroll_offset = 0.0;
        }
        let _ = h.stoat.render();
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            col,
            row,
        ));
        assert_eq!(
            h.stoat.active_workspace().editors[focused].scroll_row,
            0,
            "in per-pane mode the right-edge click leaves the focused-left editor"
        );
    }

    #[test]
    fn divider_drag_resizes_the_split() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        let left = ws.panes.focus();
        let right = ws.panes.split(crate::pane::Axis::Vertical);
        ws.panes.resize(Rect::new(0, 0, 101, 24));

        let la = h.stoat.active_workspace().panes.pane(left).area;
        let divider_col = la.x + la.width;
        let left_w0 = la.width;
        let focus0 = h.stoat.active_workspace().panes.focus();

        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            divider_col,
            5,
        ));
        assert!(
            h.stoat.divider_drag.is_some(),
            "clicking a divider arms a drag"
        );
        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            focus0,
            "a divider click does not move focus"
        );

        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            divider_col + 2,
            5,
        ));
        assert_eq!(
            h.stoat.active_workspace().panes.pane(left).area.width,
            left_w0 + 2,
            "dragging the divider right widens the left pane"
        );
        assert_eq!(
            h.stoat.active_workspace().panes.pane(right).area.width,
            98 - left_w0,
            "the right pane shrinks by the same delta"
        );

        h.stoat.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            divider_col + 2,
            5,
        ));
        assert!(h.stoat.divider_drag.is_none(), "releasing clears the drag");
    }

    /// A finder-family modal wide enough for two panes, with the list and preview
    /// rects the renderer would paint.
    fn side_by_side_layout(h: &crate::test_harness::TestHarness, kind: ModalKind) -> (Rect, Rect) {
        let separator = mouse::open_modal_separator(&h.stoat)
            .expect("the modal shows a preview beside its list");
        assert_eq!(separator.kind, kind, "the expected modal is the open one");
        let content = match kind {
            ModalKind::SymbolFinder => {
                let finder = h.stoat.symbol_finder.as_ref().expect("open");
                let (_, _, list, preview) = crate::render::symbol_finder::symbol_finder_layout(
                    h.stoat.size(),
                    finder.content_rows,
                    modal_zoom_steps(&h.stoat.modal_zoom, kind),
                    modal_split_percent(&h.stoat.modal_split, kind),
                )
                .expect("fits");
                (list, preview)
            },
            _ => {
                let size = h.stoat.size();
                let declared = match kind {
                    ModalKind::FileFinder => {
                        h.stoat.file_finder.as_ref().expect("open").content_size
                    },
                    _ => (u16::MAX, u16::MAX),
                };
                let layout = crate::render::file_finder::file_finder_layout(
                    size,
                    declared,
                    modal_zoom_steps(&h.stoat.modal_zoom, kind),
                    modal_split_percent(&h.stoat.modal_split, kind),
                )
                .expect("fits");
                (layout.list, layout.preview)
            },
        };
        (content.0, content.1.expect("the preview pane is present"))
    }

    /// A harness with the file finder open over a terminal wide enough that the
    /// modal splits into a list and a preview.
    fn wide_finder_harness() -> crate::test_harness::TestHarness {
        let mut h = crate::test_harness::TestHarness::with_size(140, 40);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        h.settle();
        h
    }

    /// Dragging the column between a finder's list and its preview is how the
    /// user gives one of them more room, so the drag has to land the line where
    /// the pointer is.
    #[test]
    fn dragging_the_finder_vline_widens_the_list() {
        let mut h = wide_finder_harness();
        let (list, preview) = side_by_side_layout(&h, ModalKind::FileFinder);
        let vline = list.x + list.width;

        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            vline,
            list.y + 1,
        ));
        assert_eq!(
            h.stoat.modal_separator_drag,
            Some(ModalKind::FileFinder),
            "a press on the vline arms the drag"
        );

        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            vline + 6,
            list.y + 1,
        ));
        let (widened, narrowed) = side_by_side_layout(&h, ModalKind::FileFinder);
        assert_eq!(
            widened.width,
            list.width + 6,
            "the list grows to exactly where the pointer left the line"
        );
        assert_eq!(
            narrowed.width,
            preview.width - 6,
            "and the preview gives up the same columns"
        );

        h.stoat.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            vline + 6,
            list.y + 1,
        ));
        assert_eq!(
            h.stoat.modal_separator_drag, None,
            "releasing clears the arm"
        );
    }

    #[test]
    fn a_vline_drag_clamps_at_both_edges() {
        use crate::render::picker::MIN_PANE_COLUMNS;

        let mut h = wide_finder_harness();
        let (list, preview) = side_by_side_layout(&h, ModalKind::FileFinder);
        let row = list.y + 1;
        let vline = list.x + list.width;
        // The two panes plus the one-column line are the whole body whatever the
        // share, so this is what a clamped split still has to add up to.
        let body = list.width + preview.width + 1;

        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            vline,
            row,
        ));

        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            139,
            row,
        ));
        let (widest, thinnest) = side_by_side_layout(&h, ModalKind::FileFinder);
        assert_eq!(
            thinnest.width, MIN_PANE_COLUMNS,
            "dragged to the screen edge the preview keeps its floor"
        );
        assert_eq!(
            widest.width + thinnest.width + 1,
            body,
            "and the list takes the rest rather than overflowing the body"
        );

        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 0, row));
        let (narrowest, _) = side_by_side_layout(&h, ModalKind::FileFinder);
        assert_eq!(
            narrowest.width, MIN_PANE_COLUMNS,
            "and dragged the other way the list keeps its own floor"
        );
    }

    /// Each kind stores its own share, so dragging one finder's line must not
    /// move another's.
    #[test]
    fn each_finder_kind_drags_its_own_separator() {
        let mut h = crate::test_harness::TestHarness::with_size(140, 40);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        h.settle();

        let (list, _) = side_by_side_layout(&h, ModalKind::CodeSearch);
        let row = list.y + 1;
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            list.x + list.width,
            row,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            list.x + list.width + 5,
            row,
        ));

        assert_eq!(
            side_by_side_layout(&h, ModalKind::CodeSearch).0.width,
            list.width + 5,
            "code search's own line moves"
        );
        assert_eq!(
            h.stoat.modal_split.get(&ModalKind::FileFinder),
            None,
            "and the file finder's share is untouched"
        );
    }

    #[test]
    fn the_symbol_finder_vline_drags_too() {
        let mut h = crate::test_harness::TestHarness::with_size(140, 40);
        h.stoat.symbol_finder = {
            let executor = h.stoat.executor.clone();
            let mut finder = SymbolFinder::new(
                h.stoat.active_workspace_mut(),
                executor,
                BufferId::new(0),
                crate::symbol_finder::SymbolFinderScope::Document,
                Vec::new(),
            );
            finder.content_rows = 12;
            Some(finder)
        };

        let (list, _) = side_by_side_layout(&h, ModalKind::SymbolFinder);
        let row = list.y + 1;
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            list.x + list.width,
            row,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            list.x + list.width + 4,
            row,
        ));

        assert_eq!(
            side_by_side_layout(&h, ModalKind::SymbolFinder).0.width,
            list.width + 4,
            "the symbol list widens with its line"
        );
    }

    /// Code search covers a pane like every other modal, so a press over it must
    /// not reach the editor beneath.
    #[test]
    fn a_press_over_code_search_never_reaches_the_buffer() {
        let mut h = crate::test_harness::TestHarness::with_size(140, 40);
        h.seed_focused_buffer(&"line\n".repeat(200));
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        h.settle();
        let head = h.primary_head_offset();
        let (list, _) = side_by_side_layout(&h, ModalKind::CodeSearch);

        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            list.x + 4,
            list.y + 2,
        ));

        assert_eq!(
            h.primary_head_offset(),
            head,
            "the cursor in the covered editor stays where it was"
        );
    }

    #[test]
    fn mouse_down_anchors_run_pane_selection() {
        let mut h = Stoat::test();
        let run_id = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 1));
        let block = h
            .stoat
            .active_workspace()
            .runs
            .get(run_id)
            .expect("run state exists")
            .active_block()
            .expect("active block exists");
        assert_eq!(
            block.selection,
            Some(GridSelection {
                anchor: (2, 0),
                head: (2, 0),
            }),
        );
    }

    #[test]
    fn mouse_drag_updates_run_pane_selection_head() {
        let mut h = Stoat::test();
        // Two real output rows so the row-1 drag target lands inside the grid
        // (the trailing blank row after the final newline is not rendered).
        let run_id = open_run_with_output(&mut h, b"hello\nworld\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 4, 2));
        let block = h
            .stoat
            .active_workspace()
            .runs
            .get(run_id)
            .expect("run state exists")
            .active_block()
            .expect("active block exists");
        assert_eq!(
            block.selection,
            Some(GridSelection {
                anchor: (1, 0),
                head: (4, 1),
            }),
        );
    }

    #[test]
    fn mouse_up_leaves_run_pane_selection_in_place() {
        let mut h = Stoat::test();
        let run_id = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 3, 1));
        let block = h
            .stoat
            .active_workspace()
            .runs
            .get(run_id)
            .expect("run state exists")
            .active_block()
            .expect("active block exists");
        assert_eq!(
            block.selection,
            Some(GridSelection {
                anchor: (3, 0),
                head: (3, 0),
            }),
        );
    }

    #[test]
    fn mouse_down_outside_active_block_does_not_select() {
        let mut h = Stoat::test();
        let run_id = open_run_with_output(&mut h, b"hello\n");
        for (col, row) in [(2u16, 0u16), (2, 3), (2, 9), (50, 1)] {
            h.stoat.update(mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                col,
                row,
            ));
            let block = h
                .stoat
                .active_workspace()
                .runs
                .get(run_id)
                .expect("run state exists")
                .active_block()
                .expect("active block exists");
            assert_eq!(
                block.selection, None,
                "click at ({col},{row}) should not anchor",
            );
        }
    }

    #[test]
    fn mouse_drag_without_prior_down_is_noop() {
        let mut h = Stoat::test();
        let run_id = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 2, 1));
        let block = h
            .stoat
            .active_workspace()
            .runs
            .get(run_id)
            .expect("run state exists")
            .active_block()
            .expect("active block exists");
        assert_eq!(block.selection, None);
    }

    #[test]
    fn mouse_on_view_without_handler_is_noop() {
        let mut h = Stoat::test();
        let pane_id = h.stoat.active_workspace().panes.focus();
        let pane = h.stoat.active_workspace_mut().panes.pane_mut(pane_id);
        pane.view = View::Label("dummy".into());
        pane.area = Rect::new(0, 0, 40, 10);
        let effect = h
            .stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
        assert_eq!(effect, UpdateEffect::None);
    }

    #[test]
    fn mouse_up_after_drag_writes_selection_to_clipboard() {
        let mut h = Stoat::test();
        let _ = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 3, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 3, 1));
        assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
    }

    #[test]
    fn mouse_up_without_drag_skips_clipboard() {
        let mut h = Stoat::test();
        let _ = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 1));
        assert!(h.fake_clipboard().writes().is_empty());
    }

    #[test]
    fn mouse_up_with_no_selection_skips_clipboard() {
        let mut h = Stoat::test();
        let _ = open_run_with_output(&mut h, b"hello\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 1));
        assert!(h.fake_clipboard().writes().is_empty());
    }

    #[test]
    fn mouse_up_multi_row_drag_writes_joined_lines() {
        let mut h = Stoat::test();
        let _ = open_run_with_output(&mut h, b"foo\nbar\n");
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 1, 2));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 1, 2));
        assert_eq!(h.fake_clipboard().writes(), vec!["oo\nba"]);
    }

    fn drag_select_ell_in_hello(h: &mut crate::test_harness::TestHarness) {
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 3, 1));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 3, 1));
    }

    #[test]
    fn osc52_emit_fires_in_ssh_without_mux() {
        let mut h = Stoat::test();
        h.fake_env().set("SSH_CONNECTION", "1.2.3.4 22 5.6.7.8 22");
        let _ = open_run_with_output(&mut h, b"hello\n");
        drag_select_ell_in_hello(&mut h);
        assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
        assert_eq!(h.fake_clipboard().osc52_emits(), vec!["ell"]);
    }

    #[test]
    fn osc52_emit_skipped_inside_tmux() {
        let mut h = Stoat::test();
        h.fake_env().set("SSH_CONNECTION", "1.2.3.4 22 5.6.7.8 22");
        h.fake_env().set("TMUX", "/tmp/tmux-1000/default,1234,0");
        let _ = open_run_with_output(&mut h, b"hello\n");
        drag_select_ell_in_hello(&mut h);
        assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
        assert!(h.fake_clipboard().osc52_emits().is_empty());
    }

    #[test]
    fn osc52_emit_skipped_inside_zellij() {
        let mut h = Stoat::test();
        h.fake_env().set("SSH_TTY", "/dev/pts/0");
        h.fake_env().set("ZELLIJ", "0");
        let _ = open_run_with_output(&mut h, b"hello\n");
        drag_select_ell_in_hello(&mut h);
        assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
        assert!(h.fake_clipboard().osc52_emits().is_empty());
    }

    #[test]
    fn osc52_emit_skipped_locally() {
        let mut h = Stoat::test();
        let _ = open_run_with_output(&mut h, b"hello\n");
        drag_select_ell_in_hello(&mut h);
        assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
        assert!(h.fake_clipboard().osc52_emits().is_empty());
    }

    fn buffer_text(h: &crate::test_harness::TestHarness, path: &Path) -> String {
        let ws = h.stoat.active_workspace();
        let id = ws.buffers.id_for_path(path).expect("buffer registered");
        let buf = ws.buffers.get(id).expect("buffer present");
        let guard = buf.read().expect("buffer lock");
        guard.rope().to_string()
    }

    fn select_forward(h: &mut crate::test_harness::TestHarness, start: usize, end: usize) {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let (start, end) = {
            let snapshot = editor.display_map.snapshot();
            let buf = snapshot.buffer_snapshot();
            (
                buf.anchor_at(start, Bias::Right),
                buf.anchor_at(end, Bias::Right),
            )
        };
        editor
            .selections
            .set_single_range(start, end, SelectionGoal::None);
    }

    /// Add a second 1-wide block cursor at `offset` in the focused editor,
    /// for building same-line multi-cursor states no keybinding produces.
    fn insert_cursor_at(h: &mut crate::test_harness::TestHarness, offset: usize) {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf = snapshot.buffer_snapshot();
        let head = buf.anchor_at(offset, Bias::Right);
        editor
            .selections
            .insert_cursor(head, SelectionGoal::None, buf);
    }

    #[test]
    fn driven_input_sequence_types_text_into_the_buffer() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");

        // The `--inputs` driver injects plain `Event::Key`s into the same
        // channel real keystrokes use, so feed the parsed sequence through
        // `update` directly rather than the double-firing keystroke helper.
        for key in crate::input_parse::parse_input_sequence("ifoo<Esc>").expect("parse") {
            h.stoat.update(Event::Key(key));
        }

        assert_eq!(buffer_text(&h, &path), "foo");
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn shutdown_notify_quits_the_run_loop() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let mut h = Stoat::test();
            h.stoat.persistence_disabled = true;

            // Pre-firing the notify stores a permit, so the shutdown arm
            // fires on the loop's first poll. This mirrors a `--timeout`
            // timer that elapses before the loop starts.
            let shutdown = h.stoat.shutdown_handle();
            shutdown.notify_one();

            let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
            let (render_tx, render_rx) = watch::channel(None);
            // Hold the event sender and render receiver so a closed channel
            // cannot end the loop. Only the shutdown notify can.
            let _keep = (event_tx, render_rx);

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                h.stoat.run(event_rx, render_tx),
            )
            .await;

            assert!(
                matches!(result, Ok(Ok(()))),
                "run must quit after shutdown notify, got {result:?}"
            );
        });
    }

    #[cfg(feature = "perf")]
    #[test]
    fn input_driven_frame_carries_an_input_timestamp() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let mut h = Stoat::test();
            h.stoat.persistence_disabled = true;

            let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
            let (render_tx, render_rx) = watch::channel(None);
            // Queue one input event and drop the sender. The loop drains the
            // event (publishing its frame), then breaks on the closed channel
            // before any background redraw can supersede it in the watch.
            event_tx.send(Event::Resize(80, 24)).expect("send");
            drop(event_tx);

            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                h.stoat.run(event_rx, render_tx),
            )
            .await
            .expect("run should quit")
            .expect("run ok");

            let frame = render_rx.borrow();
            let frame = frame.as_ref().expect("a frame was published");
            assert!(
                frame.input_time.is_some(),
                "an events.recv()-driven frame carries the input timestamp"
            );
        });
    }

    #[cfg(feature = "perf")]
    #[test]
    fn notify_driven_frame_has_no_input_timestamp() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let mut h = Stoat::test();
            h.stoat.persistence_disabled = true;
            // Give the frame a real size without routing through the event
            // channel, so no input timestamp is captured.
            h.stoat.update(Event::Resize(80, 24));
            let shutdown = h.stoat.shutdown_handle();

            let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
            let (render_tx, render_rx) = watch::channel(None);
            // A redraw-notify wakes a frame with no input behind it. The biased
            // loop takes the redraw arm, publishing a frame, then quits.
            h.stoat.redraw_notify.notify_one();
            shutdown.notify_one();
            let _keep = event_tx;

            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                h.stoat.run(event_rx, render_tx),
            )
            .await
            .expect("run should quit")
            .expect("run ok");

            let frame = render_rx.borrow();
            let frame = frame.as_ref().expect("a frame was published");
            assert!(
                frame.input_time.is_none(),
                "a redraw-notify frame carries no input timestamp"
            );
        });
    }

    #[test]
    fn ctrl_w_in_insert_mode_deletes_previous_word() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("foo bar baz");
        h.type_keys("ctrl-w");
        assert_eq!(buffer_text(&h, &path), "foo bar ");
        h.type_keys("ctrl-w");
        assert_eq!(buffer_text(&h, &path), "foo ");
    }

    #[test]
    fn ctrl_w_at_buffer_start_is_noop() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_keys("ctrl-w");
        assert_eq!(buffer_text(&h, &path), "");
    }

    #[test]
    fn alt_backspace_in_insert_mode_deletes_previous_word() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_text("alpha beta gamma");
        h.type_keys("alt-backspace");
        assert_eq!(buffer_text(&h, &path), "alpha beta ");
    }

    #[test]
    fn backspace_in_insert_mode_deletes_previous_char() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_text("abc");
        h.type_keys("backspace");
        assert_eq!(buffer_text(&h, &path), "ab");
        h.type_keys("backspace");
        assert_eq!(buffer_text(&h, &path), "a");
    }

    #[test]
    fn backspace_at_buffer_start_in_insert_mode_is_noop() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_keys("backspace");
        assert_eq!(buffer_text(&h, &path), "");
    }

    #[test]
    fn delete_in_insert_mode_deletes_next_char() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abcdef");
        h.type_keys("l l i");
        h.type_keys("delete");
        assert_eq!(buffer_text(&h, &path), "abdef");
        h.type_keys("delete");
        assert_eq!(buffer_text(&h, &path), "abef");
    }

    /// A father-mother-daughter ZWJ sequence. Three 4-byte emoji joined by two
    /// 3-byte zero-width joiners, so 18 bytes rendering as one cell.
    const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";

    #[test]
    fn backspace_in_insert_mode_removes_a_whole_cluster() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_text("xe\u{301}");
        h.type_keys("backspace");
        assert_eq!(
            buffer_text(&h, &path),
            "x",
            "the combining acute leaves with the e it sits on",
        );
    }

    #[test]
    fn delete_in_insert_mode_removes_a_whole_cluster() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, &format!("a{FAMILY}b"));
        h.type_keys("l i");
        h.type_keys("delete");
        assert_eq!(
            buffer_text(&h, &path),
            "ab",
            "the whole joined sequence goes, not its first emoji",
        );
    }

    #[test]
    fn a_word_motion_selects_whole_clusters() {
        // A combining mark is its own codepoint and categorizes apart from the
        // letter it sits on, so the motion stops between them. What the
        // selection covers has to be whole characters even when the motion that
        // proposed it did not think so, or deleting the word leaves the mark
        // behind on whatever follows it.
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "cafe\u{301} bar");
        h.type_keys("w d");
        assert_eq!(
            buffer_text(&h, &path),
            " bar",
            "the accent leaves with the e it sits on",
        );
    }

    #[test]
    fn goto_line_end_lands_on_a_whole_final_character() {
        // The step back off the line end has to be by character, and the
        // selection it lands has to cover a whole one. Both mechanisms are
        // needed for this gesture, and it fails if either regresses.
        for (source, remaining) in [
            ("ab cafe\u{301}", "ab caf"),
            (&format!("ab x{FAMILY}"), "ab x"),
        ] {
            let mut h = Stoat::test();
            let path = open_scratch_file(&mut h, source);
            h.type_keys("g l d");
            assert_eq!(
                buffer_text(&h, &path),
                remaining,
                "the whole last character goes, from {source:?}",
            );
        }
    }

    #[test]
    fn replace_gives_one_character_per_character() {
        // The count is per character, not per codepoint it is written with. A
        // decomposed letter is two codepoints and a joined emoji is five, and
        // each is one character on the screen and under the cursor.
        for source in ["e\u{301}b", &format!("{FAMILY}b")] {
            let mut h = Stoat::test();
            let path = open_scratch_file(&mut h, source);
            h.type_keys("r x");
            assert_eq!(
                buffer_text(&h, &path),
                "xb",
                "one x for the character replaced, from {source:?}",
            );
        }
    }

    #[test]
    fn replace_gives_one_character_for_each_one_selected() {
        // Three accented letters written with six codepoints, so what is being
        // asserted is the count rather than the single-character case. The
        // line selection takes the newline too, and that is a character like
        // any other here, so it is replaced rather than kept.
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "e\u{301}a\u{302}i\u{303}\n");
        h.type_keys("x r z");
        assert_eq!(buffer_text(&h, &path), "zzzz", "one z per character");
    }

    /// The block cursor sits on a cluster start at every step of an `l`/`h`
    /// walk, never on a byte inside the joined sequence.
    #[test]
    fn horizontal_motion_never_lands_inside_a_cluster() {
        let mut h = Stoat::test();
        open_scratch_file(&mut h, &format!("a{FAMILY}b"));

        assert_eq!(h.head_offsets(), vec![0], "starts on the a");
        h.type_keys("l");
        assert_eq!(
            h.head_offsets(),
            vec![1],
            "l lands on the family's first byte"
        );
        h.type_keys("l");
        assert_eq!(h.head_offsets(), vec![19], "l clears all 18 bytes of it");
        h.type_keys("h");
        assert_eq!(h.head_offsets(), vec![1], "h returns over it whole");
        h.type_keys("h");
        assert_eq!(h.head_offsets(), vec![0]);
    }

    #[test]
    fn esc_from_append_steps_back_onto_a_cluster_start() {
        let mut h = Stoat::test();
        open_scratch_file(&mut h, "");
        h.type_keys("A");
        h.type_text(FAMILY);
        h.type_keys("escape");
        assert_eq!(
            h.head_offsets(),
            vec![0],
            "the cursor lands on the sequence's first byte, not inside it",
        );
    }

    #[test]
    fn delete_at_buffer_end_in_insert_mode_is_noop() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc");
        h.type_keys("A");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_keys("delete");
        assert_eq!(buffer_text(&h, &path), "abc");
    }

    #[test]
    fn backspace_applies_at_every_cursor() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "xa\nxb\n");
        h.type_keys("l");
        h.type_keys("C");
        h.type_keys("i");
        h.type_keys("backspace");
        assert_eq!(buffer_text(&h, &path), "a\nb\n");
        assert_eq!(h.head_offsets(), vec![0, 2]);
        // The sorted heads above are the same set however the cursors are
        // permuted, so only a named cursor shows each landed at its own delete.
        assert_eq!(h.primary_head_offset(), 2, "cursor added below");
    }

    #[test]
    fn delete_applies_at_every_cursor() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "ax\nbx\n");
        h.type_keys("C");
        h.type_keys("i");
        h.type_keys("delete");
        assert_eq!(buffer_text(&h, &path), "x\nx\n");
        assert_eq!(h.head_offsets(), vec![0, 2]);
    }

    #[test]
    fn alt_backspace_applies_at_every_cursor() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "foo\nbar\n");
        h.type_keys("l l");
        h.type_keys("C");
        h.type_keys("a");
        h.type_keys("alt-backspace");
        assert_eq!(buffer_text(&h, &path), "\n\n");
        assert_eq!(h.head_offsets(), vec![0, 1]);
    }

    #[test]
    fn alt_backspace_merges_overlapping_word_deletes() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "hello\n");
        h.type_keys("l l");
        insert_cursor_at(&mut h, 4);
        h.type_keys("i");
        h.type_keys("alt-backspace");
        assert_eq!(buffer_text(&h, &path), "o\n");
        assert_eq!(h.head_offsets(), vec![0]);
    }

    /// A word deletion never cuts a grapheme cluster in half.
    ///
    /// Word motions stop mid-cluster on purpose, deferring the snap to wherever
    /// their answer is written. Writing a selection snaps, and splicing the rope
    /// does not, so a deletion driven straight off a motion has to snap for
    /// itself or it orphans a combining mark onto whatever text survives.
    #[test]
    fn a_word_delete_forward_keeps_a_combining_mark_with_its_base() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "cafe\u{301} bar");
        h.type_keys("i");
        h.type_keys("alt-d");
        assert_eq!(
            buffer_text(&h, &path),
            " bar",
            "the acute goes with the e it sits on, not onto the space",
        );
    }

    /// The backward sibling, where the motion's endpoint lands inside a cluster
    /// rather than after one.
    ///
    /// `prev_word_start` stops between the `e` and its acute, so the snap grows
    /// the deletion out to the cluster's start and takes both. Leaving the base
    /// behind without its mark, which is what an unsnapped splice does, would
    /// silently rewrite the surviving character.
    #[test]
    fn a_word_delete_backward_keeps_a_combining_mark_with_its_base() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "cafe\u{301}x");
        h.type_keys("A");
        h.type_keys("alt-backspace");
        assert_eq!(
            buffer_text(&h, &path),
            "caf",
            "the accented e goes whole rather than leaving a bare e behind",
        );
    }

    #[test]
    fn ctrl_u_kills_to_first_non_whitespace_then_line_start() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "  foo bar");
        h.type_keys("A");
        h.type_keys("ctrl-u");
        assert_eq!(
            buffer_text(&h, &path),
            "  ",
            "first kill preserves the indent"
        );
        h.type_keys("ctrl-u");
        assert_eq!(buffer_text(&h, &path), "", "second kill removes the indent");
    }

    #[test]
    fn ctrl_u_inside_indent_kills_to_line_start() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "  foo");
        h.type_keys("l i");
        h.type_keys("ctrl-u");
        assert_eq!(buffer_text(&h, &path), " foo");
    }

    #[test]
    fn ctrl_u_at_line_start_joins_previous_line() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "ab\ncd");
        h.type_keys("j i");
        h.type_keys("ctrl-u");
        assert_eq!(buffer_text(&h, &path), "abcd");
    }

    #[test]
    fn ctrl_u_at_buffer_start_is_noop() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc");
        h.type_keys("i");
        h.type_keys("ctrl-u");
        assert_eq!(buffer_text(&h, &path), "abc");
    }

    #[test]
    fn ctrl_k_kills_to_line_end() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "ab cd\nxy");
        h.type_keys("l l i");
        h.type_keys("ctrl-k");
        assert_eq!(buffer_text(&h, &path), "ab\nxy");
    }

    #[test]
    fn ctrl_k_at_line_end_deletes_line_separator() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "ab\ncd");
        h.type_keys("A");
        h.type_keys("ctrl-k");
        assert_eq!(buffer_text(&h, &path), "abcd");
    }

    #[test]
    fn ctrl_k_at_buffer_end_is_noop() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc");
        h.type_keys("A");
        h.type_keys("ctrl-k");
        assert_eq!(buffer_text(&h, &path), "abc");
    }

    #[test]
    fn ctrl_k_applies_at_every_cursor() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "ax\nbx\n");
        h.type_keys("C");
        h.type_keys("i");
        h.type_keys("ctrl-k");
        assert_eq!(buffer_text(&h, &path), "\n\n");
        assert_eq!(h.head_offsets(), vec![0, 1]);
    }

    #[test]
    fn alt_d_deletes_next_word() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "foo bar");
        h.type_keys("i");
        h.type_keys("alt-d");
        assert_eq!(buffer_text(&h, &path), " bar");
        h.type_keys("alt-d");
        assert_eq!(buffer_text(&h, &path), "");
    }

    #[test]
    fn alt_d_at_buffer_end_is_noop() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc");
        h.type_keys("A");
        h.type_keys("alt-d");
        assert_eq!(buffer_text(&h, &path), "abc");
    }

    #[test]
    fn ctrl_h_deletes_previous_char() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc");
        h.type_keys("A");
        h.type_keys("ctrl-h");
        assert_eq!(buffer_text(&h, &path), "ab");
    }

    #[test]
    fn ctrl_d_deletes_next_char() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abcdef");
        h.type_keys("l l i");
        h.type_keys("ctrl-d");
        assert_eq!(buffer_text(&h, &path), "abdef");
    }

    #[test]
    fn ctrl_j_inserts_newline_with_continued_indent() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "  ab");
        h.type_keys("A");
        h.type_keys("ctrl-j");
        assert_eq!(buffer_text(&h, &path), "  ab\n  ");
    }

    #[test]
    fn insert_session_undoes_and_redoes_as_one_step() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_text("hello");
        h.type_keys("esc");
        assert_eq!(buffer_text(&h, &path), "hello");
        h.type_keys("u");
        assert_eq!(
            buffer_text(&h, &path),
            "",
            "one undo clears the whole insert session"
        );
        h.type_keys("U");
        assert_eq!(
            buffer_text(&h, &path),
            "hello",
            "one redo restores the whole session"
        );
    }

    #[test]
    fn a_mid_session_motion_keeps_the_insert_session_one_undo_step() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_text("abc");
        h.type_keys("left");
        h.type_text("x");
        h.type_keys("esc");
        assert_eq!(buffer_text(&h, &path), "abxc");
        h.type_keys("u");
        assert_eq!(
            buffer_text(&h, &path),
            "",
            "one undo reverts the whole session despite the mid-session motion"
        );
        h.type_keys("U");
        assert_eq!(
            buffer_text(&h, &path),
            "abxc",
            "one redo restores the whole session"
        );
    }

    #[test]
    fn a_mid_session_dispatched_action_joins_the_insert_session() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_text("a");
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SmartTab);
        h.type_text("b");
        h.type_keys("esc");
        h.type_keys("u");
        assert_eq!(
            buffer_text(&h, &path),
            "",
            "a mid-session dispatched action leaves the session's single undo step intact"
        );
    }

    #[test]
    fn delete_undoes_both_cursors_and_restores_selections() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "ab\nab\n");
        h.type_keys("l");
        h.type_keys("C");
        let before = h.head_offsets();
        h.type_keys("d");
        assert_eq!(buffer_text(&h, &path), "a\na\n");
        h.type_keys("u");
        assert_eq!(
            buffer_text(&h, &path),
            "ab\nab\n",
            "one undo restores both cursors' deletions"
        );
        assert_eq!(h.head_offsets(), before, "undo restores both selections");
    }

    /// One undo reverts a whole multi-cursor insert session, and redo restores it.
    ///
    /// The delete sibling above pins that side. An insert session is grouped
    /// differently. Its group opens on entering insert mode and seals on leaving
    /// it, rather than being wrapped around one action, so the two paths can fail
    /// independently.
    #[test]
    fn an_insert_session_undoes_and_redoes_at_every_cursor() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "ab\nab\n");
        h.type_keys("l");
        h.type_keys("C");
        let before = h.head_offsets();

        h.type_keys("i");
        h.type_text("XY");
        h.type_keys("esc");
        assert_eq!(buffer_text(&h, &path), "aXYb\naXYb\n");

        h.type_keys("u");
        assert_eq!(
            buffer_text(&h, &path),
            "ab\nab\n",
            "one undo reverts the session at both cursors",
        );
        assert_eq!(h.head_offsets(), before, "and restores both selections");

        h.type_keys("shift-U");
        assert_eq!(
            buffer_text(&h, &path),
            "aXYb\naXYb\n",
            "one redo reapplies the session at both cursors",
        );
    }

    #[test]
    fn ctrl_s_splits_the_insert_session_into_two_undo_steps() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_text("hello");
        h.type_keys("ctrl-s");
        h.type_text("world");
        h.type_keys("esc");
        assert_eq!(buffer_text(&h, &path), "helloworld");
        h.type_keys("u");
        assert_eq!(
            buffer_text(&h, &path),
            "hello",
            "the first undo reverts only the post-checkpoint edits"
        );
        h.type_keys("u");
        assert_eq!(
            buffer_text(&h, &path),
            "",
            "the second undo reverts the pre-checkpoint edits"
        );
    }

    /// A checkpoint taken in normal mode does not swallow later edits.
    ///
    /// Ctrl-s exists to split an insert session in two, so outside one there is
    /// nothing to split and it is a plain checkpoint. It runs without the
    /// action-group wrapper, so a group it opened in normal mode is one nothing
    /// would close, and every later edit would keep joining it -- including
    /// across an undo, which is what makes those edits reachable only as part of
    /// a step they do not belong to.
    #[test]
    fn a_normal_mode_checkpoint_leaves_later_edits_undoable() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abcdef\n");

        h.type_keys("ctrl-s");
        h.type_keys("d");
        assert_eq!(buffer_text(&h, &path), "bcdef\n", "d deletes one character");
        h.type_keys("u");
        assert_eq!(buffer_text(&h, &path), "abcdef\n", "and undoes");

        // A second edit, since x is SelectLineBelow here and would not make one.
        h.type_keys("d");
        assert_eq!(
            buffer_text(&h, &path),
            "bcdef\n",
            "the post-undo edit lands"
        );

        h.type_keys("u");
        assert_eq!(
            buffer_text(&h, &path),
            "abcdef\n",
            "the edit made after the undo undoes on its own",
        );
    }

    /// Edits after a normal-mode checkpoint stay separate undo steps.
    ///
    /// The action-group wrapper leaves an already-open group alone so a
    /// mid-session action joins the insert step it sits in. A group Ctrl-s
    /// opened in normal mode has no session to belong to and nothing to seal it,
    /// so every later action would keep joining it and the whole run would
    /// collapse into one step.
    #[test]
    fn edits_after_a_normal_mode_checkpoint_undo_separately() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abcdef\n");

        h.type_keys("ctrl-s");
        h.type_keys("d");
        h.type_keys("d");
        assert_eq!(buffer_text(&h, &path), "cdef\n", "two characters deleted");

        h.type_keys("u");
        assert_eq!(
            buffer_text(&h, &path),
            "bcdef\n",
            "one undo reverts only the second delete",
        );
    }

    #[test]
    fn insert_types_at_every_cursor() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "aa\nbb\n");
        h.type_keys("C");
        h.type_keys("i");
        h.type_text("XY");
        assert_eq!(buffer_text(&h, &path), "XYaa\nXYbb\n");
        assert_eq!(h.head_offsets(), vec![2, 7]);
    }

    /// Two cursors on one line each land after their own inserted text.
    ///
    /// The landing arithmetic and the batch's descending order are exercised by
    /// the one-cursor-per-line tests too, since both work in offsets and a later
    /// cursor carries the earlier insertions wherever it sits. What is untested
    /// is the shape itself. Nothing else drives two cursors into a single line, so
    /// a change that started treating rows separately would go unnoticed.
    #[test]
    fn same_line_cursors_each_land_after_their_own_insert() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc");
        h.type_keys("l");
        insert_cursor_at(&mut h, 2);
        h.type_keys("i");
        h.type_text("X");
        assert_eq!(buffer_text(&h, &path), "aXbXc");
        assert_eq!(
            h.head_offsets(),
            vec![2, 4],
            "the later cursor carries the earlier insertion as well as its own",
        );
    }

    /// The insert paths reverse this list into `edit_batch`, which takes its
    /// ranges sorted descending by start. Reversing gives descending only
    /// because this answers ascending, and cursors are added in whatever order
    /// the reader made them, so the sort is doing real work.
    ///
    /// `edit_batch` checks the order with a `debug_assert`, so a release build
    /// has nothing but this holding it.
    #[test]
    fn cursor_offsets_come_back_in_ascending_order() {
        let mut h = Stoat::test();
        open_scratch_file(&mut h, "abcdefgh");

        for offset in [6, 2, 4] {
            insert_cursor_at(&mut h, offset);
        }

        let (editor_id, _) = h.stoat.focused_editor_ids().expect("editor");
        let cursors = h.stoat.editor_cursor_offsets(editor_id);
        let offsets: Vec<usize> = cursors.iter().map(|(_, offset)| *offset).collect();

        assert_eq!(
            offsets,
            vec![0, 2, 4, 6],
            "the cursors come back in offset order, not the order they were made",
        );
    }

    /// Adjacent cursors backspacing collapse onto one deletion.
    ///
    /// Their delete ranges touch end to start, which no other end-to-end test
    /// produces. `merge_overlapping_spans` leaves them alone, since touching is not
    /// overlapping, and `edit_batch` takes them as two ranges. Both cursors then
    /// land on the same offset and reduce to one.
    #[test]
    fn adjacent_cursors_backspacing_merge_into_one() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abcd");
        h.type_keys("l");
        insert_cursor_at(&mut h, 2);
        h.type_keys("i");
        h.type_keys("backspace");
        assert_eq!(buffer_text(&h, &path), "cd", "both leading characters go");
        assert_eq!(
            h.head_offsets(),
            vec![0],
            "the two cursors land on the same offset and dedupe",
        );
    }

    #[test]
    fn insert_single_cursor_advances_past_text() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc");
        h.type_keys("i");
        h.type_text("X");
        assert_eq!(buffer_text(&h, &path), "Xabc");
        assert_eq!(h.head_offsets(), vec![1]);
    }

    #[test]
    fn insert_with_forward_selection_types_at_block_cursor() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abcdef");
        select_forward(&mut h, 0, 3);
        h.stoat.transition_mode("insert".to_string());
        h.type_text("X");
        assert_eq!(buffer_text(&h, &path), "abXcdef");
    }

    #[test]
    fn backspace_with_forward_selection_acts_at_block_cursor() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abcdef");
        select_forward(&mut h, 0, 3);
        h.stoat.transition_mode("insert".to_string());
        h.type_keys("backspace");
        assert_eq!(buffer_text(&h, &path), "acdef");
    }

    #[test]
    fn enter_in_insert_mode_inserts_newline_in_file_buffer() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_text("abc");
        h.type_keys("enter");
        h.type_text("xyz");
        assert_eq!(buffer_text(&h, &path), "abc\nxyz");
    }

    #[test]
    fn append_advances_one_char_then_inserts() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\n");
        h.type_keys("a");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("X");
        assert_eq!(buffer_text(&h, &path), "aXbc\n");
    }

    #[test]
    fn shift_i_jumps_to_first_nonwhitespace_then_inserts() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "    code\n");
        h.type_keys("l");
        h.type_keys("I");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("X");
        assert_eq!(buffer_text(&h, &path), "    Xcode\n");
    }

    #[test]
    fn shift_a_jumps_to_line_end_then_inserts() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\nxyz\n");
        h.type_keys("A");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("Z");
        assert_eq!(buffer_text(&h, &path), "abcZ\nxyz\n");
    }

    #[test]
    fn shift_i_on_empty_line_auto_indents() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"fn a() {\n\n}\n");
        h.type_keys("j");
        h.type_keys("I");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("x");
        assert_eq!(focused_buffer_string(&h), "fn a() {\n\tx\n}\n");
    }

    #[test]
    fn shift_i_on_whitespace_line_falls_back_to_line_start() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\n    \ndef\n");
        h.type_keys("j");
        h.type_keys("l");
        h.type_keys("I");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("X");
        assert_eq!(buffer_text(&h, &path), "abc\nX    \ndef\n");
    }

    #[test]
    fn shift_a_on_empty_line_auto_indents() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"fn a() {\n\n}\n");
        h.type_keys("j");
        h.type_keys("A");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("x");
        assert_eq!(focused_buffer_string(&h), "fn a() {\n\tx\n}\n");
    }

    #[test]
    fn open_below_then_escape_strips_untouched_auto_indent() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"fn a() {\n}\n");
        h.type_keys("o");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_keys("escape");
        assert_eq!(focused_buffer_string(&h), "fn a() {\n\n}\n");
    }

    #[test]
    fn open_below_then_type_then_escape_keeps_indent() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"fn a() {\n}\n");
        h.type_keys("o");
        h.type_text("x");
        h.type_keys("escape");
        assert_eq!(focused_buffer_string(&h), "fn a() {\n\tx\n}\n");
    }

    #[test]
    fn shift_i_on_empty_line_then_escape_strips_indent() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"fn a() {\n\n}\n");
        h.type_keys("j");
        h.type_keys("I");
        h.type_keys("escape");
        assert_eq!(focused_buffer_string(&h), "fn a() {\n\n}\n");
    }

    #[test]
    fn shift_a_on_empty_line_then_escape_strips_indent() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"fn a() {\n\n}\n");
        h.type_keys("j");
        h.type_keys("A");
        h.type_keys("escape");
        assert_eq!(focused_buffer_string(&h), "fn a() {\n\n}\n");
        assert_eq!(h.selection_spans(), vec![(9, 10, false)]);
    }

    #[test]
    fn insert_on_whitespace_line_then_escape_keeps_whitespace() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\n    \ndef\n");
        h.type_keys("j");
        h.type_keys("i");
        h.type_keys("escape");
        assert_eq!(buffer_text(&h, &path), "abc\n    \ndef\n");
    }

    #[test]
    fn count_open_below_opens_that_many_lines() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"fn a() {\n}\n");
        h.type_keys("3 o");
        h.type_text("x");
        assert_eq!(focused_buffer_string(&h), "fn a() {\n\tx\n\tx\n\tx\n}\n");
    }

    #[test]
    fn open_below_opens_per_selection_without_row_dedup() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abcdef\n");
        insert_cursor_at(&mut h, 3);
        h.type_keys("o");
        assert_eq!(h.selection_spans().len(), 2);
        h.type_text("X");
        assert_eq!(buffer_text(&h, &path), "abcdef\nX\nX\n");
    }

    #[test]
    fn open_below_continues_line_comment() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"// foo\n");
        h.type_keys("o");
        h.type_text("bar");
        assert_eq!(focused_buffer_string(&h), "// foo\n// bar\n");
    }

    #[test]
    fn open_above_continues_line_comment() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"// foo\n");
        h.type_keys("O");
        h.type_text("bar");
        assert_eq!(focused_buffer_string(&h), "// bar\n// foo\n");
    }

    #[test]
    fn insert_enter_continues_line_comment() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"// foo\n");
        h.type_keys("A");
        h.type_keys("enter");
        h.type_text("bar");
        assert_eq!(focused_buffer_string(&h), "// foo\n// bar\n");
    }

    #[test]
    fn insert_enter_continues_doc_comment_token() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"/// foo\n");
        h.type_keys("A");
        h.type_keys("enter");
        h.type_text("bar");
        assert_eq!(focused_buffer_string(&h), "/// foo\n/// bar\n");
    }

    #[test]
    fn insert_enter_continues_inner_doc_comment_token() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"//! foo\n");
        h.type_keys("A");
        h.type_keys("enter");
        h.type_text("bar");
        assert_eq!(focused_buffer_string(&h), "//! foo\n//! bar\n");
    }

    #[test]
    fn insert_enter_before_the_comment_token_carries_no_token() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"// foo\n");
        h.type_keys("i");
        h.type_keys("enter");
        assert_eq!(focused_buffer_string(&h), "\n// foo\n");
    }

    #[test]
    fn insert_enter_inside_a_comment_indent_carries_no_token() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"    // foo\n");
        h.type_keys("l l i");
        h.type_keys("enter");
        assert_eq!(
            focused_buffer_string(&h),
            "  \n      // foo\n",
            "the split indent takes the plain-indent path, the same as any line",
        );
    }

    #[test]
    fn open_below_continues_doc_comment_token() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"/// foo\n");
        h.type_keys("o");
        h.type_text("bar");
        assert_eq!(focused_buffer_string(&h), "/// foo\n/// bar\n");
    }

    #[test]
    fn open_below_inserts_blank_line_after_current_row() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\ndef\n");
        h.type_keys("o");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("X");
        assert_eq!(buffer_text(&h, &path), "abc\nX\ndef\n");
    }

    #[test]
    fn open_below_after_open_brace_auto_indents() {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"fn a() {\n}\n");
        h.type_keys("o");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("x");
        assert_eq!(focused_buffer_string(&h), "fn a() {\n\tx\n}\n");
    }

    #[test]
    fn open_above_inserts_blank_line_before_current_row() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\ndef\n");
        h.type_keys("o");
        h.type_keys("escape");
        h.type_keys("O");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("Y");
        assert_eq!(buffer_text(&h, &path), "abc\nY\n\ndef\n");
    }

    #[test]
    fn open_below_at_last_line_appends_at_eof() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc");
        h.type_keys("o");
        h.type_text("X");
        assert_eq!(buffer_text(&h, &path), "abc\nX");
    }

    #[test]
    fn open_above_at_first_line_inserts_at_offset_zero() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\n");
        h.type_keys("O");
        h.type_text("Z");
        assert_eq!(buffer_text(&h, &path), "Z\nabc\n");
    }

    #[test]
    fn change_selection_deletes_then_enters_insert() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abcdef");
        h.type_keys("v l l l");
        h.type_keys("c");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("XYZ");
        assert_eq!(buffer_text(&h, &path), "XYZef");
    }

    #[test]
    fn replace_char_replaces_each_char_in_selection() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abcdef");
        h.type_keys("v l l l");
        h.type_keys("r");
        h.type_keys("X");
        assert_eq!(buffer_text(&h, &path), "XXXXef");
        assert_eq!(h.stoat.focused_mode(), "select");
    }

    #[test]
    fn replace_char_on_bare_cursor_replaces_char() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc");
        h.type_keys("r");
        h.type_keys("X");
        assert_eq!(buffer_text(&h, &path), "Xbc");
        assert_eq!(h.stoat.focused_mode(), "normal");
        assert!(!h.stoat.pending_replace);
    }

    #[test]
    fn replace_char_with_multibyte_input_grows_buffer() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc");
        h.type_keys("v l l");
        h.type_keys("r");
        h.type_text("é");
        assert_eq!(buffer_text(&h, &path), "ééé");
    }

    #[test]
    fn tab_at_line_start_inserts_tab() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\n");
        h.type_keys("i");
        h.type_keys("tab");
        assert_eq!(buffer_text(&h, &path), "\tabc\n");
    }

    #[test]
    fn i_on_selection_inserts_before_it() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "foo\n");
        h.type_keys("%");
        h.type_keys("i");
        h.type_keys("X");
        assert_eq!(buffer_text(&h, &path), "Xfoo\n");
    }

    #[test]
    fn append_then_escape_lands_on_last_typed_char() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\n");
        h.type_keys("A");
        h.type_keys("X");
        h.type_keys("escape");
        assert_eq!(buffer_text(&h, &path), "abcX\n");
        assert_eq!(h.selection_spans(), vec![(3, 4, false)]);
    }

    #[test]
    fn tab_after_whitespace_inserts_indent_unit() {
        let mut h = Stoat::test();
        // The 2-space indent makes the buffer space-styled, so Tab inserts it.
        let path = open_scratch_file(&mut h, "  abc\n");
        h.type_keys("l l i");
        h.type_keys("tab");
        assert_eq!(buffer_text(&h, &path), "    abc\n");
    }

    #[test]
    fn tab_after_nonwhitespace_is_noop() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\n");
        h.type_keys("l l l i");
        h.type_keys("tab");
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn backtab_inserts_indent_unit_unconditionally() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\n");
        h.type_keys("l l i");
        h.type_keys("backtab");
        assert_eq!(buffer_text(&h, &path), "ab\tc\n");
    }

    #[test]
    fn backspace_on_leading_indent_removes_one_width() {
        let mut h = Stoat::test();
        // The 4-space indent makes the buffer a 4-space style.
        let path = open_scratch_file(&mut h, "    abc\n");
        h.type_keys("l l l l i");
        h.type_keys("backspace");
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn pending_completion_defaults_to_none() {
        let h = Stoat::test();
        assert_eq!(h.stoat.pending_completion, None);
    }

    #[test]
    fn esc_in_insert_with_open_popup_clears_popup_and_exits_to_normal() {
        use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
        let mut h = Stoat::test();
        let _path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![CompletionItem {
                label: "foo".into(),
                source: CompletionSource::Lsp,
                kind: None,
                detail: None,
                replace_range: 0..0,
                insert_text: "foo".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
                server: None,
            }],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..0,
            prefix: String::new(),
            incomplete: Vec::new(),
        });
        h.type_keys("escape");
        assert_eq!(h.stoat.pending_completion, None);
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "one escape closes the popup and leaves insert mode",
        );
    }

    #[test]
    fn esc_in_insert_with_no_popup_exits_to_normal() {
        let mut h = Stoat::test();
        let _path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        assert_eq!(h.stoat.focused_mode(), "insert");
        assert_eq!(h.stoat.pending_completion, None);
        h.type_keys("escape");
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn tab_with_no_popup_smart_indents_after_whitespace() {
        let mut h = Stoat::test();
        // The 2-space indent makes the buffer space-styled.
        let path = open_scratch_file(&mut h, "  abc\n");
        h.type_keys("l l i");
        assert!(h.stoat.pending_completion.is_none());
        h.type_keys("tab");
        assert_eq!(buffer_text(&h, &path), "    abc\n");
    }

    #[test]
    fn tab_with_popup_open_invokes_acceptance() {
        use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_keys("f o o");
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![CompletionItem {
                label: "foobar".into(),
                source: CompletionSource::Word,
                kind: None,
                detail: None,
                replace_range: 0..3,
                insert_text: "foobar".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
                server: None,
            }],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..3,
            prefix: String::new(),
            incomplete: Vec::new(),
        });

        h.type_keys("tab");

        assert_eq!(buffer_text(&h, &path), "foobar");
        assert!(h.stoat.pending_completion.is_none());
    }

    #[test]
    fn up_and_down_arrows_navigate_popup_without_moving_cursor() {
        use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
        let mut h = Stoat::test();
        let _path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_keys("f");
        let cursor_before = focused_primary_offsets(&mut h);
        assert_eq!(cursor_before.0, 1);

        let popup = || CompletionPopup {
            items: vec![
                CompletionItem {
                    label: "foo".into(),
                    source: CompletionSource::Word,
                    kind: None,
                    detail: None,
                    replace_range: 0..1,
                    insert_text: "foo".into(),
                    is_snippet: false,
                    documentation: None,
                    lsp_item: None,
                    server: None,
                },
                CompletionItem {
                    label: "foobar".into(),
                    source: CompletionSource::Word,
                    kind: None,
                    detail: None,
                    replace_range: 0..1,
                    insert_text: "foobar".into(),
                    is_snippet: false,
                    documentation: None,
                    lsp_item: None,
                    server: None,
                },
                CompletionItem {
                    label: "foobaz".into(),
                    source: CompletionSource::Word,
                    kind: None,
                    detail: None,
                    replace_range: 0..1,
                    insert_text: "foobaz".into(),
                    is_snippet: false,
                    documentation: None,
                    lsp_item: None,
                    server: None,
                },
            ],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..1,
            prefix: String::new(),
            incomplete: Vec::new(),
        };
        h.stoat.pending_completion = Some(popup());

        h.type_keys("down");
        assert_eq!(h.stoat.pending_completion.as_ref().unwrap().selected_idx, 1,);
        h.type_keys("down");
        assert_eq!(h.stoat.pending_completion.as_ref().unwrap().selected_idx, 2,);
        // Clamps at last index.
        h.type_keys("down");
        assert_eq!(h.stoat.pending_completion.as_ref().unwrap().selected_idx, 2,);

        h.type_keys("up");
        assert_eq!(h.stoat.pending_completion.as_ref().unwrap().selected_idx, 1,);
        h.type_keys("up");
        assert_eq!(h.stoat.pending_completion.as_ref().unwrap().selected_idx, 0,);
        // Saturates at zero.
        h.type_keys("up");
        assert_eq!(h.stoat.pending_completion.as_ref().unwrap().selected_idx, 0,);

        let cursor_after = focused_primary_offsets(&mut h);
        assert_eq!(cursor_before, cursor_after);
    }

    #[test]
    fn up_and_down_with_no_popup_move_cursor() {
        let mut h = Stoat::test();
        let _path = open_scratch_file(&mut h, "first\nsecond\n");
        h.type_keys("i");
        let (start, _) = focused_primary_offsets(&mut h);
        assert_eq!(start, 0);
        h.type_keys("down");
        let (after_down, _) = focused_primary_offsets(&mut h);
        assert!(after_down > 0, "down arrow should advance cursor");
    }

    #[test]
    fn tab_advances_active_snippet_to_next_tabstop() {
        use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_keys("p r i");
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![CompletionItem {
                label: "fn".into(),
                source: CompletionSource::Lsp,
                kind: None,
                detail: None,
                replace_range: 0..3,
                insert_text: "${1:name}(${2:arg})$0".into(),
                is_snippet: true,
                documentation: None,
                lsp_item: None,
                server: None,
            }],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..3,
            prefix: String::new(),
            incomplete: Vec::new(),
        });
        h.type_keys("tab");
        assert_eq!(buffer_text(&h, &path), "name(arg)");
        let (start, end) = focused_primary_offsets(&mut h);
        assert_eq!((start, end), (0, 4), "first tabstop");
        assert!(h.stoat.active_snippet.is_some());

        h.type_keys("tab");
        let (start, end) = focused_primary_offsets(&mut h);
        assert_eq!((start, end), (5, 8), "second tabstop");
        assert!(h.stoat.active_snippet.is_some());

        h.type_keys("tab");
        let (start, end) = focused_primary_offsets(&mut h);
        assert_eq!((start, end), (9, 9), "exit landed at $0");
        assert!(
            h.stoat.active_snippet.is_none(),
            "snippet exits after final tab",
        );
    }

    #[test]
    fn leaving_insert_mode_clears_active_snippet() {
        use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
        let mut h = Stoat::test();
        let _path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.type_keys("f");
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![CompletionItem {
                label: "snippet".into(),
                source: CompletionSource::Lsp,
                kind: None,
                detail: None,
                replace_range: 0..1,
                insert_text: "${1:a} ${2:b}".into(),
                is_snippet: true,
                documentation: None,
                lsp_item: None,
                server: None,
            }],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..1,
            prefix: String::new(),
            incomplete: Vec::new(),
        });
        h.type_keys("tab");
        assert!(h.stoat.active_snippet.is_some());

        h.type_keys("escape");
        h.type_keys("escape");
        assert_eq!(h.stoat.focused_mode(), "normal");
        assert!(h.stoat.active_snippet.is_none());
    }

    fn focused_primary_offsets(h: &mut crate::test_harness::TestHarness) -> (usize, usize) {
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        let ws = h.stoat.active_workspace_mut();
        let editor = ws.editors.get_mut(editor_id).expect("editor exists");
        let snap = editor.display_map.snapshot();
        let buf_snap = snap.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        (
            buf_snap.resolve_anchor(&sel.start),
            buf_snap.resolve_anchor(&sel.end),
        )
    }

    fn focused_gutter_width(h: &crate::test_harness::TestHarness) -> u16 {
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        h.stoat
            .active_workspace()
            .editors
            .get(editor_id)
            .expect("editor exists")
            .gutter_width
    }

    #[test]
    fn line_numbers_setting_toggles_the_gutter() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/ln-toggle");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        h.stoat.settings.editor_line_numbers = Some(LineNumbers::Relative);
        h.stoat.render();
        let with_numbers = focused_gutter_width(&h);

        h.stoat.settings.editor_line_numbers = Some(LineNumbers::Off);
        h.stoat.render();
        let without = focused_gutter_width(&h);

        assert!(
            with_numbers > without,
            "line numbers widen the gutter ({with_numbers}) past the \
             diagnostic-only column ({without})"
        );
        assert_eq!(
            without, 0,
            "with no diagnostics and no line numbers there is no gutter"
        );
    }

    #[test]
    fn a_theme_defining_no_colors_pushes_nothing() {
        assert!(
            osc_default_colors(&crate::theme::Theme::empty()).is_empty(),
            "with nothing to say, the terminal's own defaults stand",
        );
    }

    #[test]
    fn editor_page_content_version_tracks_the_cursor_line() {
        let base = editor_page_content_version(true, 3, None, Some(10), 0, false, 0, 0.0, 0, 0);
        assert_eq!(
            base,
            editor_page_content_version(true, 3, None, Some(10), 0, false, 0, 0.0, 0, 0),
            "identical inputs keep a buffered page cached"
        );
        assert_ne!(
            base,
            editor_page_content_version(true, 3, None, Some(11), 0, false, 0, 0.0, 0, 0),
            "a cursor-line move refills buffered pages"
        );
        assert_ne!(
            base,
            editor_page_content_version(true, 3, None, None, 0, false, 0, 0.0, 0, 0),
            "switching to absolute numbering refills"
        );
        assert_ne!(
            base,
            editor_page_content_version(true, 3, Some(72), Some(10), 0, false, 0, 0.0, 0, 0),
            "a wrap-width change refills buffered pages"
        );
        assert_ne!(
            base,
            editor_page_content_version(true, 3, None, Some(10), 0, true, 7, 0.0, 0, 0),
            "a diff-view hunk change refills buffered pages"
        );
        assert_ne!(
            base,
            editor_page_content_version(true, 3, None, Some(10), 0, false, 0, 0.25, 0, 0),
            "a focus change to a dimmed pane refills buffered pages"
        );
        assert_ne!(
            base,
            editor_page_content_version(true, 3, None, Some(10), 0, false, 0, 0.0, 1, 0),
            "a buffer edit (snapshot version bump) refills buffered pages"
        );
        assert_ne!(
            base,
            editor_page_content_version(true, 3, None, Some(10), 0, false, 0, 0.0, 0, 1),
            "a theme switch refills buffered pages"
        );
    }

    #[test]
    fn editor_mouse_down_lands_block_cursor_at_clicked_offset() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "abcdef\nghi");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 3,
            area.y,
        ));
        assert_eq!(focused_primary_offsets(&mut h), (3, 4));
        assert!(h.stoat.editor_drag.is_some(), "drag state armed");
    }

    /// Clicking any cell a joined sequence is drawn over lands on the whole
    /// sequence.
    ///
    /// The display clip walks codepoints, so an interior cell resolves to a
    /// byte offset inside the sequence. Landing there would leave the cursor
    /// covering only its tail, and a delete would strand the codepoints before
    /// it.
    #[test]
    fn editor_mouse_down_inside_a_cluster_lands_on_the_whole_cluster() {
        // Five codepoints over eighteen bytes, rendered across six cells.
        for cell in 1..6 {
            let mut h = Stoat::test();
            let _ = open_scratch_file(&mut h, &format!("{FAMILY}b\n"));
            let area = focused_editor_pane_area(&h);
            h.stoat.update(mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                area.x + cell,
                area.y,
            ));
            assert_eq!(
                focused_primary_offsets(&mut h),
                (0, 18),
                "a click on cell {cell} covers the whole sequence",
            );
        }
    }

    /// Open a location picker over `count` candidates in a seeded file, each
    /// row's text naming its 1-based position, and return the file's path.
    fn open_location_picker(h: &mut crate::test_harness::TestHarness, count: usize) -> PathBuf {
        use crate::location_picker::{LocationEntry, LocationPicker};

        let root = PathBuf::from("/loc-picker");
        let path = root.join("target.rs");
        let text = "line\n".repeat(count.max(1));
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), text.as_bytes())));
        h.stoat.active_workspace_mut().git_root = root;

        let entries = (0..count)
            .map(|i| LocationEntry {
                path: path.clone(),
                offset: i * 5,
                line: i as u32 + 1,
                column: 1,
                text: format!("candidate-{}", i + 1),
            })
            .collect();
        h.stoat.location_picker = Some(LocationPicker::new(entries));
        h.snapshot();
        path
    }

    /// The location picker's inner rows rect for the harness' screen size.
    fn location_picker_rows(h: &crate::test_harness::TestHarness) -> Rect {
        let len = h
            .stoat
            .location_picker
            .as_ref()
            .expect("picker open")
            .entries()
            .len();
        crate::render::location_picker::location_picker_layout(h.stoat.size(), len)
            .expect("picker laid out")
            .1
    }

    #[test]
    fn location_picker_click_selects_the_clicked_row() {
        let mut h = Stoat::test();
        open_location_picker(&mut h, 4);
        let rows = location_picker_rows(&h);

        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            rows.x + 2,
            rows.y + 1,
        ));

        let picker = h.stoat.location_picker.as_ref().expect("picker still open");
        assert_eq!(
            picker.selected(),
            1,
            "the second row is selected, not jumped"
        );
    }

    #[test]
    fn location_picker_click_on_the_selected_row_jumps() {
        let mut h = Stoat::test();
        let path = open_location_picker(&mut h, 4);
        let rows = location_picker_rows(&h);

        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            rows.x + 2,
            rows.y + 1,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            rows.x + 2,
            rows.y + 1,
        ));
        h.settle();

        assert!(h.stoat.location_picker.is_none(), "the picker closed");
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.id_for_path(&path).expect("target opened");
        assert_eq!(
            h.stoat.focused_editor_ids().map(|(editor_id, _)| {
                h.stoat
                    .active_workspace()
                    .editors
                    .get(editor_id)
                    .expect("editor")
                    .buffer_id
            }),
            Some(buffer),
            "the jump landed in the candidate's file"
        );
    }

    #[test]
    fn location_picker_wheel_moves_the_selection() {
        let mut h = Stoat::test();
        open_location_picker(&mut h, 4);

        h.stoat
            .update(mouse_event(MouseEventKind::ScrollDown, 1, 1));
        assert_eq!(
            h.stoat.location_picker.as_ref().expect("open").selected(),
            1
        );

        h.stoat.update(mouse_event(MouseEventKind::ScrollUp, 1, 1));
        assert_eq!(
            h.stoat.location_picker.as_ref().expect("open").selected(),
            0
        );
    }

    #[test]
    fn location_picker_click_outside_is_swallowed() {
        let mut h = Stoat::test();
        open_location_picker(&mut h, 4);
        let rows = location_picker_rows(&h);
        let before = focused_primary_offsets(&mut h);

        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            rows.x,
            rows.y.saturating_sub(2),
        ));

        assert_eq!(
            h.stoat.location_picker.as_ref().expect("open").selected(),
            0,
            "a click outside the rows changes nothing"
        );
        assert_eq!(
            focused_primary_offsets(&mut h),
            before,
            "the buffer beneath keeps its cursor"
        );
    }

    #[test]
    fn location_picker_renders_a_selection_past_the_visible_rows() {
        // Wider than the default terminal so the hints overlay, which is
        // right-aligned and sized by the bindings it lists, sits clear of the
        // centered modal. At 80 columns it covers the rows this reads.
        let mut h = crate::test_harness::TestHarness::with_size(160, 40);
        open_location_picker(&mut h, 15);
        h.stoat
            .location_picker
            .as_mut()
            .expect("open")
            .set_selected(14);
        h.snapshot();

        // Rows are read individually rather than by scanning the whole frame.
        // The key hints overlay paints across the right end of the bottom rows.
        let rows = location_picker_rows(&h);
        let row_text = |row: u16| -> String {
            let buf = h.rendered_buffer();
            (rows.x..rows.x + rows.width)
                .map(|col| buf[(col, row)].symbol())
                .collect()
        };
        let last_row = rows.y + rows.height - 1;

        assert!(
            row_text(rows.y).contains("candidate-4"),
            "the window scrolled the first three candidates off: {}",
            row_text(rows.y)
        );
        assert!(
            row_text(last_row).contains("15:1"),
            "the selected 15th candidate paints on the last row: {}",
            row_text(last_row)
        );

        let selection = h.stoat.theme.get(crate::theme::scope::UI_SELECTION);
        assert_eq!(
            h.rendered_buffer()[(rows.x + 1, last_row)].style().bg,
            selection.bg,
            "the selected candidate is painted as selected"
        );
    }

    /// Seed a definition-capable fake server, open `main.rs` holding
    /// `abc\ndef\nghi\n`, and return its path.
    fn open_file_with_lsp(h: &mut crate::test_harness::TestHarness) -> PathBuf {
        use lsp_types::{OneOf, ServerCapabilities};

        h.fake_lsp().set_capabilities(ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
            ..Default::default()
        });

        let root = PathBuf::from("/mouse-lsp");
        let path = root.join("main.rs");
        h.fake_fs().insert_files(std::iter::once((
            path.clone(),
            b"abc\ndef\nghi\n".as_slice(),
        )));
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        path
    }

    #[test]
    fn middle_click_jumps_to_the_clicked_symbols_definition() {
        let mut h = Stoat::test();
        let path = open_file_with_lsp(&mut h);
        let uri = path.to_str().expect("utf8 path");
        h.fake_lsp().set_definition(uri, 1, 1, uri, 2, 0);

        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Middle),
            area.x + 1,
            area.y + 1,
        ));
        h.settle();

        assert!(
            h.stoat.location_picker.is_none(),
            "a single target skips the picker"
        );
        assert_eq!(
            focused_primary_offsets(&mut h),
            (8, 9),
            "the jump landed on line 3, so the request read the clicked cell"
        );
    }

    #[test]
    fn middle_click_with_several_definitions_opens_the_picker() {
        let mut h = Stoat::test();
        let path = open_file_with_lsp(&mut h);
        let uri = path.to_str().expect("utf8 path");
        h.fake_lsp()
            .set_definitions(uri, 0, 0, &[(uri, 1, 0), (uri, 2, 0)]);

        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Middle),
            area.x,
            area.y,
        ));
        h.settle();

        let picker = h.stoat.location_picker.as_ref().expect("picker open");
        assert_eq!(picker.entries().len(), 2);
    }

    #[test]
    fn right_click_requests_hover_at_the_clicked_cell() {
        let mut h = Stoat::test();
        open_file_with_lsp(&mut h);

        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Right),
            area.x + 2,
            area.y + 1,
        ));

        assert_eq!(
            focused_primary_offsets(&mut h),
            (6, 7),
            "hover reads the cursor, which the click moved to the clicked cell"
        );
        assert!(h.stoat.pending_hover_request.is_some(), "hover requested");
        assert!(
            h.stoat.editor_drag.is_none(),
            "a right click is not a selection gesture"
        );
    }

    #[test]
    fn middle_click_focuses_the_pane_it_lands_in() {
        let mut h = Stoat::test();
        open_file_with_lsp(&mut h);
        let left_pane = {
            let ws = h.stoat.active_workspace_mut();
            let left_pane = ws.panes.focus();
            let right_pane = ws.panes.split(crate::pane::Axis::Vertical);
            ws.panes.set_focus(right_pane);
            ws.panes.pane_mut(left_pane).area = Rect::new(0, 0, 40, 24);
            ws.panes.pane_mut(right_pane).area = Rect::new(40, 0, 40, 24);
            left_pane
        };

        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Middle), 5, 5));

        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            left_pane,
            "the click focuses the pane under the pointer before dispatching"
        );
    }

    #[test]
    fn editor_mouse_down_below_last_line_lands_on_row_zero() {
        let mut h = Stoat::test();
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x,
            area.y + 3,
        ));
        assert_eq!(
            focused_primary_offsets(&mut h),
            (0, 1),
            "a click below the seeded scratch's one line covers the newline on row 0"
        );
    }

    #[test]
    fn editor_click_excludes_the_diagnostic_gutter() {
        // The no-gutter path (no render leaves gutter_width zero) is covered by
        // editor_mouse_down_lands_block_cursor_at_clicked_offset above.
        let mut h = Stoat::test();
        let root = PathBuf::from("/gutter-click");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"abcdef\nghi\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.seed_diagnostics(
            path,
            vec![lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 1,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: String::new(),
                ..Default::default()
            }],
        );
        h.stoat.render();

        let gutter_w = focused_gutter_width(&h);
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + gutter_w + 2,
            area.y,
        ));
        assert_eq!(
            focused_primary_offsets(&mut h),
            (2, 3),
            "the line-number gutter shifts text right, so the click excludes it"
        );
    }

    #[test]
    fn editor_mouse_drag_extends_selection_forward() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "abcdef\nghi");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 1,
            area.y,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 5,
            area.y,
        ));
        assert_eq!(focused_primary_offsets(&mut h), (1, 5));
    }

    #[test]
    fn editor_mouse_drag_extends_selection_backward_reverses() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "abcdef\nghi");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 5,
            area.y,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 1,
            area.y,
        ));
        assert_eq!(focused_primary_offsets(&mut h), (1, 5));
    }

    #[test]
    fn editor_mouse_click_outside_pane_text_is_noop() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "abc");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + area.width + 4,
            area.y,
        ));
        assert!(
            h.stoat.editor_drag.is_none(),
            "click past pane right edge does not arm drag",
        );
    }

    #[test]
    fn editor_mouse_up_clears_drag_state() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "abcdef");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 2,
            area.y,
        ));
        assert!(h.stoat.editor_drag.is_some());
        h.stoat.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            area.x + 2,
            area.y,
        ));
        assert!(h.stoat.editor_drag.is_none(), "Up clears drag state");
    }

    #[test]
    fn editor_mouse_up_after_drag_writes_selection_to_clipboard() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "hello\nworld");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 1,
            area.y,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 4,
            area.y,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            area.x + 4,
            area.y,
        ));
        assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
    }

    /// A terminal in any-motion tracking reports a drag per pointer motion, not
    /// per cell, so most of a sweep's events land where the last one did. The
    /// one that moves the head paints. The repeats behind it have nothing to
    /// show and must not cost a frame. Releasing still copies, which is what
    /// says the drag itself was left alone.
    #[test]
    fn a_repeated_editor_drag_on_the_settled_cell_costs_no_frame() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "hello\nworld");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 1,
            area.y,
        ));

        let moved = h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 4,
            area.y,
        ));
        let repeat = h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 4,
            area.y,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            area.x + 4,
            area.y,
        ));

        assert_eq!(moved, UpdateEffect::Redraw, "the head moved, so repaint");
        assert_eq!(repeat, UpdateEffect::None, "nothing moved, so no repaint");
        assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
    }

    #[test]
    fn editor_mouse_up_without_drag_skips_clipboard() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "hello\nworld");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 2,
            area.y,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            area.x + 2,
            area.y,
        ));
        assert!(h.fake_clipboard().writes().is_empty());
    }

    #[test]
    fn editor_mouse_up_with_no_selection_skips_clipboard() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "hello\nworld");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            area.x + 2,
            area.y,
        ));
        assert!(h.fake_clipboard().writes().is_empty());
    }

    #[test]
    fn editor_mouse_up_multi_line_drag_writes_joined_text() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "hello\nworld");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 2,
            area.y,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 2,
            area.y + 1,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            area.x + 2,
            area.y + 1,
        ));
        assert_eq!(h.fake_clipboard().writes(), vec!["llo\nwo"]);
    }

    #[test]
    fn editor_osc52_emit_fires_in_ssh_without_mux() {
        let mut h = Stoat::test();
        h.fake_env().set("SSH_CONNECTION", "1.2.3.4 22 5.6.7.8 22");
        let _ = open_scratch_file(&mut h, "hello\nworld");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 1,
            area.y,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 4,
            area.y,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            area.x + 4,
            area.y,
        ));
        assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
        assert_eq!(h.fake_clipboard().osc52_emits(), vec!["ell"]);
    }

    #[test]
    fn editor_osc52_emit_skipped_locally() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "hello\nworld");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 1,
            area.y,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 4,
            area.y,
        ));
        h.stoat.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            area.x + 4,
            area.y,
        ));
        assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
        assert!(h.fake_clipboard().osc52_emits().is_empty());
    }

    #[test]
    fn agent_event_drives_owning_workspace_status() {
        let mut h = Stoat::test();
        let uid = h.stoat.active_workspace().uid;

        let effect = h.stoat.handle_agent_event(AgentEvent {
            uid,
            event: AgentHookEvent::PreToolUse {
                tool: "Bash".into(),
            },
        });

        assert_eq!(effect, UpdateEffect::Redraw);
        let label = h
            .stoat
            .active_workspace()
            .agent
            .as_ref()
            .and_then(|status| status.badge())
            .map(|badge| badge.label);
        assert_eq!(label, Some("claude: Bash".to_string()));
    }

    #[test]
    fn agent_event_for_unknown_session_is_ignored() {
        let mut h = Stoat::test();

        let effect = h.stoat.handle_agent_event(AgentEvent {
            uid: WorkspaceUid(0xdead_beef),
            event: AgentHookEvent::SessionStart,
        });

        assert_eq!(effect, UpdateEffect::None);
        assert!(h.stoat.active_workspace().agent.is_none());
    }

    fn open_agent_editor(
        h: &mut crate::test_harness::TestHarness,
    ) -> (BufferId, tokio::sync::oneshot::Receiver<()>) {
        let root = PathBuf::from("/bridge");
        let path = root.join("msg.txt");
        h.fake_fs().insert_file(&path, b"draft\n");
        h.stoat.active_workspace_mut().git_root = root;
        let uid = h.stoat.active_workspace().uid;

        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let effect = h.stoat.handle_agent_control(AgentControl::OpenEditor {
            uid,
            path,
            done: done_tx,
        });
        h.settle();

        assert_eq!(effect, UpdateEffect::Redraw);
        let buffer_id = action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        (buffer_id, done_rx)
    }

    #[test]
    fn agent_open_editor_waiter_fires_on_buffer_close() {
        let mut h = Stoat::test();
        let (buffer_id, mut done_rx) = open_agent_editor(&mut h);

        assert!(
            h.stoat
                .active_workspace()
                .editor_bridge_waiters
                .contains_key(&buffer_id),
            "a waiter is registered for the opened buffer",
        );
        assert!(done_rx.try_recv().is_err(), "waiter not fired before close");

        assert_eq!(
            action_handlers::dispatch(&mut h.stoat, &stoat_action::CloseBuffer),
            UpdateEffect::Redraw
        );

        assert!(
            done_rx.try_recv().is_ok(),
            "closing the buffer fires the waiter"
        );
        assert!(
            !h.stoat
                .active_workspace()
                .editor_bridge_waiters
                .contains_key(&buffer_id),
            "the fired waiter is removed",
        );
    }

    #[test]
    fn agent_open_editor_waiter_fires_on_pane_close() {
        let mut h = Stoat::test();
        let (_buffer_id, mut done_rx) = open_agent_editor(&mut h);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::ClosePane);

        assert!(
            done_rx.try_recv().is_ok(),
            "closing the pane fires the waiter"
        );
    }
}
