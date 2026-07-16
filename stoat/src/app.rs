use crate::{
    action_handlers,
    agent_ipc::{AgentControl, AgentEvent},
    agent_status::AgentStatus,
    badge::BadgeTray,
    buffer::{BufferId, TextBufferSnapshot},
    code_index::build::IndexUpdate,
    command_palette::CommandPalette,
    display_map::{highlights::SemanticTokenHighlight, syntax_theme::SyntaxStyles},
    editor_state::EditorId,
    file_finder::FileFinder,
    help::Help,
    host::{
        EnvHost, FsHost, FsWatchHost, GitHost, GitRepo, LanguageServerFeature, LocalEnv, LocalFs,
        LocalGit, LspHost, NoopFsWatcher,
    },
    keymap::{Keymap, ResolvedAction, StateValue},
    keymap_state::{normalize_shift_event, resolve_action, StoatKeymapState},
    pane::{FocusTarget, NodeId, Placement, View},
    quit_all_confirm::QuitAllConfirm,
    rebase::RebasePause,
    register,
    render::undercurl::{self, UndercurlSpan},
    review_session::ReviewSource,
    run::{CommandMark, GridSelection, PtyNotification, RunId},
    term_session::TermId,
    ui::RenderFrame,
    workspace::{Workspace, WorkspaceId, WorkspaceUid},
    workspace_picker::WorkspacePicker,
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use futures::FutureExt;
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
};
use slotmap::SlotMap;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_action::{Diff, OpenFile, ReviewExternalEdit, ReviewRefresh};
use stoat_config::{LineNumbers, Settings};
use stoat_language::{self as language, Language, LanguageRegistry, SyntaxState};
use stoat_scheduler::Executor;
use stoat_text::{Anchor, Bias, IndentStyle, Selection};
use stoatty_widgets::ApcScene;
use tokio::sync::{
    mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender},
    watch,
};

pub(crate) const DEFAULT_KEYMAP: &str = include_str!("../../config.stcfg");

/// Quiet window after the last filesystem-watch event for a path
/// before [`ReviewExternalEdit`] dispatches. Mirrors
/// [`crate::action_handlers::lsp::LSP_DID_CHANGE_DEBOUNCE`] so a
/// formatter-on-save burst (or an agent edit chain) collapses
/// into one diff rebuild rather than three.
pub(crate) const REVIEW_EXTERNAL_EDIT_DEBOUNCE: std::time::Duration =
    std::time::Duration::from_millis(50);

/// Frame interval for scroll-animation ticks, about 120 fps. [`Stoat::run`]
/// arms a timer at this cadence while a scroll glide is active, advancing the
/// inertial scroll one step per fire.
const SCROLL_FRAME: std::time::Duration = std::time::Duration::from_millis(8);

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

pub struct Stoat {
    size: Rect,
    /// Fallback mode store, read and written only when the focused target has
    /// no mode of its own -- no focused editor, run, or terminal pane, and no
    /// open input modal. The live mode for those targets lives on the target
    /// itself; [`Self::focused_mode`] resolves which store applies.
    fallback_mode: String,
    /// Config-defined session variables set by `SetVar`. Session-local and never
    /// persisted. The keymap reads them after its built-in predicate fields.
    pub(crate) user_vars: std::collections::HashMap<String, StateValue>,
    pub executor: Executor,
    pub(crate) keymap: Keymap,
    pub settings: Settings,
    pub theme: crate::theme::Theme,
    pub(crate) command_palette: Option<CommandPalette>,
    pub(crate) help: Option<Help>,
    pub(crate) file_finder: Option<FileFinder>,
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
    /// Active input modal for typing a global-search regex pattern.
    /// `Some` while the user is composing the query; cleared on
    /// submit (the picker takes over) or cancel.
    pub(crate) global_search_input: Option<crate::global_search::GlobalSearchInputState>,
    /// Results modal listing every workspace match for the most-recent
    /// global-search submit. `Some` until the user picks a match
    /// (jumps to it) or cancels.
    pub(crate) global_search: Option<crate::global_search::GlobalSearchPicker>,
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
    /// Whether LSP inlay hints are requested and rendered for the focused
    /// editor. Toggled by `ToggleInlayHints`, off by default. Not persisted.
    pub(crate) inlay_hints_enabled: bool,
    /// In-flight viewport inlay-hint request, armed by
    /// [`action_handlers::lsp::inlay_hints_trigger`] behind a debounce and
    /// applied by [`action_handlers::lsp::pump_lsp_inlay_hints`].
    pub(crate) pending_inlay_hint_request:
        Option<stoat_scheduler::Task<Option<action_handlers::lsp::InlayHintResponse>>>,
    /// `(buffer, version, first row, last row)` the inlay-hint trigger last
    /// requested for, so an unchanged tick does not re-request.
    pub(crate) last_inlay_hint_key: Option<(BufferId, u64, u32, u32)>,
    /// In-flight document-highlight request, armed by
    /// [`action_handlers::lsp::document_highlight_trigger`] behind a debounce
    /// and applied by [`action_handlers::lsp::pump_lsp_document_highlight`].
    pub(crate) pending_document_highlight_request:
        Option<stoat_scheduler::Task<Option<action_handlers::lsp::DocumentHighlightResponse>>>,
    /// `(buffer, version, cursor offset)` the document-highlight trigger last
    /// requested for, so an unchanged tick does not re-request.
    pub(crate) last_document_highlight_key: Option<(BufferId, u64, usize)>,
    /// Last diagnostic `result_id` the server returned per buffer, sent as
    /// `previous_result_id` on the next pull so the server may answer Unchanged.
    pub(crate) pull_diagnostic_result_ids: std::collections::HashMap<BufferId, String>,
    /// In-flight pull-diagnostic requests per buffer, armed by
    /// [`action_handlers::lsp::pull_diagnostics_trigger`] behind a debounce and
    /// applied by [`action_handlers::lsp::pump_lsp_pull_diagnostics`].
    pub(crate) pending_pull_diagnostics: std::collections::HashMap<
        BufferId,
        stoat_scheduler::Task<Option<action_handlers::lsp::PullDiagnosticsOutcome>>,
    >,
    /// Buffer version the pull-diagnostic trigger last requested for, per buffer,
    /// so an unchanged tick does not re-request.
    pub(crate) last_pull_diagnostic_key: std::collections::HashMap<BufferId, u64>,
    /// In-flight semantic-token request for the focused editor, armed by
    /// [`action_handlers::lsp::semantic_tokens_trigger`] behind a debounce and
    /// applied by [`action_handlers::lsp::pump_lsp_semantic_tokens`].
    pub(crate) pending_semantic_tokens:
        Option<stoat_scheduler::Task<Option<action_handlers::lsp::SemanticTokensOutcome>>>,
    /// `(buffer, version)` the semantic-token trigger last requested for, so an
    /// unchanged tick does not re-request.
    pub(crate) last_semantic_tokens_key: Option<(BufferId, u64)>,
    /// In-flight folding-range request for the focused editor, armed by
    /// [`action_handlers::lsp::folding_ranges_trigger`] behind a debounce and
    /// applied by [`action_handlers::lsp::pump_lsp_folding_ranges`].
    pub(crate) pending_folding_ranges:
        Option<stoat_scheduler::Task<Option<action_handlers::lsp::FoldingRangesOutcome>>>,
    /// `(buffer, version)` the folding-range trigger last requested for, so an
    /// unchanged tick does not re-request.
    pub(crate) last_folding_range_key: Option<(BufferId, u64)>,
    pub(crate) render_tick: u64,
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
    /// Set on `MouseEventKind::Down(Left)` over a focused editor pane.
    /// While `Some`, `Drag(Left)` events extend the matching editor's
    /// primary selection head; `Up(Left)` clears the field.
    pub(crate) editor_drag: Option<(EditorId, BufferId)>,
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
        Arc<std::sync::Mutex<std::collections::HashMap<BufferId, Arc<String>>>>,
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
    /// [`Self::drain_fs_watch_events`] so external edits can flow
    /// into the active review session.
    pub(crate) fs_watch_host: Arc<dyn FsWatchHost>,
    /// Pending [`ReviewExternalEdit`] debounce timer per path. Each
    /// [`Self::arm_review_external_edit_debounce`] call replaces the
    /// entry, dropping the prior [`stoat_scheduler::Task`] which
    /// cancels its spawned future before the timer fires; only the
    /// most recent burst-event for a path proceeds to dispatch.
    pub(crate) review_pending_external_edits:
        std::collections::HashMap<PathBuf, stoat_scheduler::Task<()>>,
    /// Channel the per-path debounce tasks push onto once their
    /// 50ms timer fires. Decouples the spawned async work from the
    /// main-thread action dispatch in
    /// [`Self::drain_pending_external_edits`].
    pub(crate) review_external_edit_tx: Sender<PathBuf>,
    review_external_edit_rx: Receiver<PathBuf>,
    /// Single-slot debounce for a whole-session git refresh. A commit writes
    /// many `.git` files at once, and unlike the per-path
    /// [`Self::review_pending_external_edits`] this collapses that burst to one
    /// [`ReviewRefresh`]. Re-arming replaces the task, cancelling the prior
    /// timer.
    review_pending_git_refresh: Option<stoat_scheduler::Task<()>>,
    /// Channel the git-refresh debounce task pushes onto once its timer fires,
    /// drained by [`Self::drain_pending_git_refresh`].
    review_git_refresh_tx: Sender<()>,
    review_git_refresh_rx: Receiver<()>,
    /// Per-path debounce tasks for the incremental diff-warm of a file edited
    /// while review is closed. Mirrors [`Self::review_pending_external_edits`];
    /// re-arming a path drops the prior [`stoat_scheduler::Task`], cancelling
    /// its timer so only the latest burst event warms.
    pending_diff_warm_file: std::collections::HashMap<PathBuf, stoat_scheduler::Task<()>>,
    /// Channel the diff-warm debounce tasks push a path onto once their timer
    /// fires, drained by [`Self::drain_pending_diff_warm_files`].
    diff_warm_file_tx: Sender<PathBuf>,
    diff_warm_file_rx: Receiver<PathBuf>,
    /// In-flight single-file diff warms. Held so their tasks are not dropped
    /// (which would cancel them) and so the DiffWarm badge stays up until every
    /// one finishes; [`crate::diff_warm::install_finished`] drops the completed
    /// ones.
    pub(crate) diff_warm_files: Vec<crate::diff_warm::PendingFileWarm>,
    /// Per-path debounce tasks for reindexing files changed outside the
    /// editor, mirroring [`Self::review_pending_external_edits`] but
    /// feeding the code graph instead of the review session.
    index_pending_external_edits: std::collections::HashMap<PathBuf, stoat_scheduler::Task<()>>,
    /// Channel the index debounce tasks push onto when their timer fires,
    /// drained by [`Self::drain_pending_index_edits`].
    pub(crate) index_external_edit_tx: Sender<PathBuf>,
    index_external_edit_rx: Receiver<PathBuf>,
    /// Git operations flow through this trait so tests can use
    /// [`crate::host::FakeGit`] without a real repository.
    pub(crate) git_host: Arc<dyn GitHost>,
    /// Environment-variable lookups go through this trait so tests can
    /// install [`crate::host::FakeEnv`] without leaking real env state.
    pub(crate) env_host: Arc<dyn EnvHost>,
    /// Language-server requests route through this trait. Defaults to
    /// Language servers keyed by name. Reached through [`Self::lsp_host`] and
    /// [`Self::lsp_for`], never directly, and empty until a real `LocalLsp` is
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
    /// Tracks `$/progress` notifications so the status bar can show
    /// the freshest in-progress operation. Drained from
    /// [`crate::host::LspHost::try_recv_notification`] inside
    /// [`Stoat::update`].
    pub(crate) lsp_progress: crate::lsp::progress::LspProgressMap,
    /// Freshest `window/showMessage` text from the language server,
    /// shown in the status line until the next key press. Set by
    /// [`Self::drain_lsp_notifications`] and cleared at the top of
    /// [`Self::handle_key`]. `MessageType::ERROR` renders in the error
    /// style. Other levels use the default status style.
    pub(crate) lsp_message: Option<(lsp_types::MessageType, String)>,
    /// In-flight goto-style LSP request, paired with the user-facing
    /// label of the jump kind ("definition", "references", ...) so the
    /// pump can name it in a zero-result message. Replacing the entry
    /// drops the prior task, cancelling its spawned future before the
    /// response can land. Polled by [`action_handlers::pump_lsp_jumps`]
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
    /// tick; `Ready(Some)` writes the response to [`Self::pending_hover`].
    pub(crate) pending_hover_request:
        Option<stoat_scheduler::Task<Option<action_handlers::lsp::HoverResponse>>>,

    /// Hover popup content waiting to be painted. Set by
    /// [`action_handlers::pump_lsp_hover`] when a hover response lands.
    ///
    /// In normal or select mode the next key press closes it (the auto-close
    /// intercept in [`Self::handle_key`]): Escape and Ctrl-c are consumed by the
    /// close, every other key closes it and then dispatches. Any non-Hover action
    /// also clears it, so the popup vanishes on cursor motion.
    pub(crate) pending_hover: Option<action_handlers::lsp::HoverPopup>,

    /// In-flight `textDocument/signatureHelp` request, armed by
    /// [`action_handlers::lsp::signature_help_trigger`] on a trigger character
    /// and polled by [`action_handlers::lsp::pump_lsp_signature_help`].
    pub(crate) pending_signature_help_request:
        Option<stoat_scheduler::Task<Option<action_handlers::lsp::SignatureHelpPopup>>>,

    /// Signature-help popup content waiting to be painted. Cleared when the
    /// editor leaves insert mode or the completion popup opens.
    pub(crate) pending_signature_help: Option<action_handlers::lsp::SignatureHelpPopup>,

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
    pub(crate) pending_code_action_resolve:
        Option<stoat_scheduler::Task<Option<lsp_types::WorkspaceEdit>>>,

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
    pub(crate) pending_rename: Option<stoat_scheduler::Task<Option<lsp_types::WorkspaceEdit>>>,

    /// In-flight `textDocument/documentSymbol` request. Polled by
    /// [`action_handlers::lsp::pump_lsp_symbol_picker`]; on response
    /// populates [`Self::pending_symbol_picker`].
    pub(crate) pending_symbol_picker_request:
        Option<stoat_scheduler::Task<Option<lsp_types::DocumentSymbolResponse>>>,

    /// Selectable document-symbol picker waiting for the user to
    /// choose a symbol to jump to (number keys 1-9) or cancel.
    pub(crate) pending_symbol_picker: Option<action_handlers::lsp::SymbolPicker>,

    /// One-line input modal for the workspace-symbol query. Created
    /// by `open_workspace_symbol_picker`; consumed by
    /// `workspace_symbol_submit` (Enter) which fires the request, or
    /// `workspace_symbol_cancel` (Escape) which discards.
    pub(crate) workspace_symbol_input: Option<action_handlers::lsp::WorkspaceSymbolInputState>,

    /// In-flight `workspace/symbol` request. Polled by
    /// [`action_handlers::lsp::pump_lsp_workspace_symbol`]; on
    /// `Ready(Some)` populates [`Self::pending_workspace_symbol_picker`].
    pub(crate) pending_workspace_symbol_request:
        Option<stoat_scheduler::Task<Option<lsp_types::WorkspaceSymbolResponse>>>,

    /// Selectable workspace-symbol picker. The user chose a query
    /// from the input modal; this picker shows up to nine matching
    /// symbols. Picking opens the symbol's file at its location.
    pub(crate) pending_workspace_symbol_picker: Option<action_handlers::lsp::WorkspaceSymbolPicker>,

    /// In-flight `textDocument/rangeFormatting` request triggered by
    /// `FormatSelections`. Polled by
    /// [`action_handlers::lsp::pump_lsp_format`]; on `Ready(Some)`
    /// the returned text edits are applied via
    /// [`crate::lsp::edit_apply::apply_workspace_edit`].
    pub(crate) pending_format_request:
        Option<stoat_scheduler::Task<Option<action_handlers::lsp::FormatResponse>>>,
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

    /// In-flight debounced completion request. Replacing the entry
    /// drops the prior task, cancelling its spawned future before its
    /// debounce timer or downstream LSP request can land. Polled by
    /// [`crate::completion::request::pump`] each render tick; on
    /// `Ready` writes the resolved popup to [`Self::pending_completion`].
    pub(crate) pending_completion_request:
        Option<stoat_scheduler::Task<crate::completion::CompletionPopup>>,

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
        Option<stoat_scheduler::Task<Option<crate::completion::accept::AcceptedImports>>>,

    /// Buffer signature `(BufferId, version)` recorded at the most
    /// recent completion-trigger call. The trigger pipeline returns
    /// early when this matches the focused buffer's current
    /// signature so a no-op event tick (Esc-dismiss, cursor-only
    /// motion) does not re-arm the request that was just dismissed.
    /// Cleared whenever insert mode exits, so re-entering insert
    /// starts from a clean slate.
    pub(crate) last_completion_signature: Option<(BufferId, u64)>,

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
    /// Whether stoat is running inside the stoatty terminal, detected from
    /// the `STOATTY` env var at startup. Gates the smooth-scroll APC emit:
    /// when `false`, [`Self::emit_smooth_scroll`] is a no-op and the byte
    /// stream is identical to a run in any other terminal.
    pub(crate) stoatty: bool,
    /// Ordered, non-dropping channel carrying stoatty APC byte batches from
    /// the app loop to the UI thread, written to stdout right after each
    /// rendered frame. Separate from the latest-wins render watch because
    /// `fill` page content must not be coalesced or dropped. `None` outside
    /// stoatty or before [`Self::set_stoatty_apc`] is called.
    pub(crate) apc_tx: Option<UnboundedSender<Vec<u8>>>,
    /// Reused per-frame APC decoration buffer. Widgets append their component
    /// frames while painting; [`Self::emit_apc_scene`] diffs it against the last
    /// flush so unchanged decoration costs no bytes. Empty until a widget appends.
    pub(crate) apc_scene: ApcScene,
    /// Diagnostic underline spans collected during the current paint. The editor
    /// renderer fills this while painting under stoatty; [`Self::paint_into`]
    /// turns it into the curly-underline VT re-stamp carried on the frame. Reused
    /// across frames like [`Self::apc_scene`] to avoid a per-frame allocation.
    pub(crate) pending_undercurls: Vec<UndercurlSpan>,
    /// Smooth-scroll pool emit state for the focused editor. Tracks the
    /// last-declared pool region, filled page window, and emitted scroll row
    /// so each frame emits only the deltas.
    pub(crate) smooth_scroll: crate::smooth_scroll::SmoothScrollState,
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
    /// Syntax-scope palette the minimap strips declare and their run summaries
    /// index, resolved from [`Self::theme`].
    pub(crate) minimap_class_table: crate::minimap::ClassTable,
}

/// Result of a successful background parse, ready to be installed on the
/// foreground thread.
pub(crate) struct ParseJobOutput {
    pub(crate) buffer_id: BufferId,
    pub(crate) syntax: SyntaxState,
    /// Multi-layer parse state from [`stoat_language::SyntaxMap::reparse`].
    /// Populated alongside [`Self::syntax`] so the legacy single-tree
    /// highlight path and the capture-merging path can run side by side
    /// while consumers migrate.
    pub(crate) syntax_map: stoat_language::SyntaxMap,
    pub(crate) tokens: Arc<[SemanticTokenHighlight]>,
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
        Self::new_with_user_config(executor, cli_settings, initial_git_root, None)
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
    pub fn new_with_user_config(
        executor: Executor,
        cli_settings: Settings,
        initial_git_root: PathBuf,
        user_config: Option<String>,
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
                        Self::parse_default_keymap(),
                        Some("user config parse failed; using built-in defaults".to_string()),
                    )
                }
            },
            None => (Self::parse_default_keymap(), None),
        };

        let settings = config
            .as_ref()
            .map(Settings::from_config)
            .unwrap_or_default()
            .merge(cli_settings);

        let highlight_retention = settings
            .highlight_retention
            .unwrap_or(DEFAULT_HIGHLIGHT_RETENTION);
        tracing::info!(
            target: "stoat::app",
            highlight_retention,
            configured = settings.highlight_retention.is_some(),
            "highlight retention: caching syntax trees and token sets for hidden buffers"
        );

        let theme = {
            let name = settings.theme.as_deref().unwrap_or("default_dark");
            match config.as_ref() {
                Some(c) => crate::theme::Theme::from_config(c, name).unwrap_or_else(|e| {
                    tracing::error!("theme '{name}' load failed: {e}");
                    crate::theme::Theme::empty()
                }),
                None => crate::theme::Theme::empty(),
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
        let language_registry = Arc::new(LanguageRegistry::standard());
        let theme_keys = syntax_styles.theme_keys();
        for lang in language_registry.languages() {
            let map = stoat_language::HighlightMap::new(lang.highlight_capture_names(), theme_keys);
            lang.set_highlight_map(map);
        }

        let mut workspaces = SlotMap::with_key();
        let workspace = Workspace::new(initial_git_root.clone(), &executor);
        let active_workspace = workspaces.insert(workspace);
        workspaces[active_workspace].id = active_workspace;

        let (pty_tx, pty_rx) = tokio::sync::mpsc::channel(256);
        let (agent_event_tx, agent_event_rx) = tokio::sync::mpsc::channel(256);
        let (agent_control_tx, agent_control_rx) = tokio::sync::mpsc::channel(256);
        let (index_update_tx, index_update_rx) = tokio::sync::mpsc::unbounded_channel();
        let (review_external_edit_tx, review_external_edit_rx) = tokio::sync::mpsc::channel(256);
        let (review_git_refresh_tx, review_git_refresh_rx) = tokio::sync::mpsc::channel(256);
        let (diff_warm_file_tx, diff_warm_file_rx) = tokio::sync::mpsc::channel(256);
        let (index_external_edit_tx, index_external_edit_rx) = tokio::sync::mpsc::channel(256);

        let mut stoat = Self {
            size: Rect::default(),
            fallback_mode: "normal".into(),
            user_vars: std::collections::HashMap::new(),
            executor,
            keymap,
            settings,
            theme,
            command_palette: None,
            help: None,
            file_finder: None,
            workspace_picker: None,
            quit_all_confirm: None,
            jumplist_picker: None,
            diagnostics_picker: None,
            location_picker: None,
            last_picker_action: None,
            global_search_input: None,
            global_search: None,
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
            _index_build_task: None,
            redraw_notify: Arc::new(tokio::sync::Notify::new()),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            #[cfg(feature = "perf")]
            perf: crate::perf::PerfStats::default(),
            pending_review_scan: None,
            pending_diff_warm: None,
            modal_run: None,
            syntax_highlight: true,
            minimap_override: None,
            inlay_hints_enabled: false,
            pending_inlay_hint_request: None,
            last_inlay_hint_key: None,
            pending_document_highlight_request: None,
            last_document_highlight_key: None,
            pull_diagnostic_result_ids: std::collections::HashMap::new(),
            pending_pull_diagnostics: std::collections::HashMap::new(),
            last_pull_diagnostic_key: std::collections::HashMap::new(),
            pending_semantic_tokens: None,
            last_semantic_tokens_key: None,
            pending_folding_ranges: None,
            last_folding_range_key: None,
            render_tick: 0,
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
            editor_drag: None,
            hover_cell: None,
            hover_diag: None,
            divider_drag: None,
            minimap_drag: None,
            lsp_opened: std::collections::HashSet::new(),
            lsp_buffer_versions: std::collections::HashMap::new(),
            lsp_pending_changes: std::collections::HashMap::new(),
            auto_reload_poll: None,
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
            index_pending_external_edits: std::collections::HashMap::new(),
            index_external_edit_tx,
            index_external_edit_rx,
            git_host: Arc::new(LocalGit::new()),
            env_host: Arc::new(LocalEnv),
            lsp_registry: crate::lsp::registry::LspRegistry::new(),
            lsp_auto_spawn: false,
            lsp_spawn_failed: None,
            lsp_spawn_deferred: None,
            pending_lsp_host: Arc::new(std::sync::Mutex::new(Vec::new())),
            env_auto_load: false,
            diff_warm_auto: false,
            pending_env: Arc::new(std::sync::Mutex::new(None)),
            clipboard_host: Arc::new(crate::host::NoopClipboard),
            diff_cache: Arc::new(std::sync::Mutex::new(crate::diff_cache::DiffCache::new(
                256,
            ))),
            base_highlights_cache: Arc::new(
                std::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            lsp_progress: crate::lsp::progress::LspProgressMap::new(),
            lsp_message: None,
            pending_lsp_jump: None,
            pending_hover_request: None,
            pending_hover: None,
            pending_signature_help_request: None,
            pending_signature_help: None,
            last_signature_help_key: None,
            pending_code_action_request: None,
            pending_code_action_picker: None,
            pending_code_action_resolve: None,
            pending_prepare_rename: None,
            rename_input: None,
            pending_rename: None,
            pending_symbol_picker_request: None,
            pending_symbol_picker: None,
            workspace_symbol_input: None,
            pending_workspace_symbol_request: None,
            pending_workspace_symbol_picker: None,
            pending_format_request: None,
            pending_format_on_save: None,
            quit_after_save: false,
            quit_requested: false,
            pending_completion: None,
            pending_completion_request: None,
            pending_completion_resolve: None,
            pending_completion_accept: None,
            last_completion_signature: None,
            active_snippet: None,
            version_info: "unknown",
            stoatty: false,
            apc_tx: None,
            apc_scene: ApcScene::new(),
            pending_undercurls: Vec::new(),
            smooth_scroll: crate::smooth_scroll::SmoothScrollState::default(),
            minimap_content: std::collections::HashMap::new(),
            minimap_next_content_id: 0,
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
    /// `stoatty` is whether the process is running inside the stoatty
    /// terminal; when `false` the smooth-scroll emit stays a no-op. `apc_tx`
    /// is the ordered channel the app loop pushes APC byte batches onto for
    /// the UI thread to write after each frame. The bin layer calls this once
    /// at startup, before [`Self::run`].
    pub fn set_stoatty_apc(&mut self, stoatty: bool, apc_tx: UnboundedSender<Vec<u8>>) {
        self.stoatty = stoatty;
        self.apc_tx = Some(apc_tx);
    }

    /// Inject the version string the `ShowVersion` action reports. The binary
    /// passes its build-stamped `VERSION_INFO`. Tests leave the default.
    pub fn set_version_info(&mut self, info: &'static str) {
        self.version_info = info;
    }

    /// Whether the minimap strip is currently shown.
    ///
    /// The runtime [`Self::minimap_override`] wins, else the `editor.minimap`
    /// setting, else enabled.
    pub(crate) fn minimap_enabled(&self) -> bool {
        self.minimap_override
            .or(self.settings.editor_minimap)
            .unwrap_or(true)
    }

    /// Flip the minimap's visibility for the session, overriding the setting.
    pub(crate) fn toggle_minimap(&mut self) {
        self.minimap_override = Some(!self.minimap_enabled());
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

    /// The single active language server, or a noop when none is up.
    ///
    /// Editor-wide LSP traffic (shutdown, notification pumps) routes through
    /// this. Buffer-specific requests use [`Self::lsp_for`].
    pub(crate) fn lsp_host(&self) -> Arc<dyn LspHost> {
        self.lsp_registry.sole_or_noop()
    }

    /// The language server that should serve `buffer_id`.
    ///
    /// A buffer with a language routes to that language's own server, an
    /// injected sole client, or a noop. A buffer with no language falls back
    /// to the sole client, or a noop.
    pub(crate) fn lsp_for(&self, buffer_id: BufferId) -> Arc<dyn LspHost> {
        match action_handlers::lsp::lsp_language_name(&self.active_workspace().buffers, buffer_id) {
            Some(name) => self.lsp_registry.route(&name),
            None => self.lsp_registry.sole_or_noop(),
        }
    }

    /// Every language server that mirrors `buffer_id`'s document, for
    /// fan-out of `did_open` / `did_change` / `did_save` / `did_close`.
    ///
    /// Every running server for the buffer's language needs the document, so
    /// this returns all of them (or the injected sole client when none are up).
    pub(crate) fn hosts_for_buffer(&self, buffer_id: BufferId) -> Vec<Arc<dyn LspHost>> {
        let name =
            action_handlers::lsp::lsp_language_name(&self.active_workspace().buffers, buffer_id)
                .unwrap_or_default();
        self.lsp_registry.hosts_for_language(&name)
    }

    /// The language server that should answer a single-target `feature` request
    /// for `buffer_id`, the first of its language's servers whose selector
    /// routes the feature and whose capabilities support it.
    ///
    /// Falls back to [`Self::lsp_host`] (a noop when nothing supports it), so a
    /// caller's `supports_feature` guard still rejects unavailable features.
    pub(crate) fn lsp_for_feature(
        &self,
        buffer_id: BufferId,
        feature: LanguageServerFeature,
    ) -> Arc<dyn LspHost> {
        self.feature_hosts(buffer_id, feature)
            .into_iter()
            .next()
            .map(|(_, host)| host)
            .unwrap_or_else(|| self.lsp_host())
    }

    /// Every server, with its registry name, that routes `feature` for
    /// `buffer_id`'s language and advertises it.
    ///
    /// Fan-out requests (completion) dispatch to all of them; single-target
    /// requests take the first via [`Self::lsp_for_feature`].
    pub(crate) fn feature_hosts(
        &self,
        buffer_id: BufferId,
        feature: LanguageServerFeature,
    ) -> Vec<(String, Arc<dyn LspHost>)> {
        let name =
            action_handlers::lsp::lsp_language_name(&self.active_workspace().buffers, buffer_id)
                .unwrap_or_default();
        self.lsp_registry.hosts_with_feature(&name, feature)
    }

    /// Reap the language server on quit. Awaits [`LspHost::shutdown`]
    /// bounded by a 500ms timeout so a server that ignores the request
    /// cannot block the editor's exit. [`NoopLsp`] and the test fake
    /// return immediately, so the call is unconditional. Errors and
    /// timeouts are ignored -- the process is exiting regardless.
    pub async fn shutdown_lsp(&self) {
        let hosts = self.lsp_registry.hosts();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async move {
            for host in hosts {
                let _ = host.shutdown().await;
            }
        })
        .await;
    }

    pub fn active_workspace(&self) -> &Workspace {
        &self.workspaces[self.active_workspace]
    }

    pub fn active_workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active_workspace]
    }

    pub(crate) fn size(&self) -> Rect {
        self.size
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
    /// command does. Used by the `--review` TUI-start entry point.
    pub fn toggle_diff_view(&mut self) {
        action_handlers::dispatch(self, &Diff);
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
        mut events: Receiver<Event>,
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

        loop {
            let animating = self.is_animating();
            if !animating {
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
                _ = self.redraw_notify.notified() => UpdateEffect::Redraw,
                _ = self.shutdown_notify.notified() => UpdateEffect::Quit,
                _ = frame_timer.tick(), if animating => {
                    let now = std::time::Instant::now();
                    let dt = last_tick
                        .map(|prev| (now - prev).as_secs_f32().min(MAX_FRAME_DT))
                        .unwrap_or_else(|| SCROLL_FRAME.as_secs_f32());
                    #[cfg(feature = "perf")]
                    self.perf
                        .record_anim_tick(std::time::Duration::from_secs_f32(dt));
                    let effect = if self.tick_scroll_anim(dt) {
                        // While the glide continues, push the eased scroll
                        // target to stoatty's pool and skip the full live-grid
                        // repaint. The pool composites the smooth position over
                        // the live grid, which only needs repainting once the
                        // glide settles, so a glide frame costs microseconds
                        // rather than a full editor re-render.
                        self.emit_smooth_scroll();
                        UpdateEffect::None
                    } else {
                        UpdateEffect::Redraw
                    };
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
                        let mut b = Buffer::empty(self.size);
                        #[cfg(feature = "perf")]
                        let painted = std::time::Instant::now();
                        self.paint_into(&mut b);
                        #[cfg(feature = "perf")]
                        self.perf.record_paint(painted.elapsed());
                        let undercurl = undercurl::build(&b, &self.pending_undercurls);
                        (Arc::new(b), undercurl)
                    };
                    let cursor = self.primary_cursor_screen_pos();
                    render.send_replace(Some(RenderFrame {
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
                    self.emit_apc_scene();
                    self.emit_smooth_scroll();
                    self.emit_minimap();
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
            .any(|editor| editor.scroll_velocity != 0.0 || editor.scroll_glide)
    }

    /// Advance every animating editor's inertial scroll by `dt` seconds, the
    /// real time elapsed since the previous tick. Returns whether any editor is
    /// still gliding after the step.
    ///
    /// A wheel coast (nonzero velocity) steps through
    /// [`step_scroll_momentum`](action_handlers::movement::step_scroll_momentum),
    /// writing back the decayed velocity and new offset and keeping `scroll_row`
    /// at `floor(scroll_offset)` so the integer-row render and pool paths track
    /// the coast. On settle it arms a page glide onto the rounded resting row
    /// rather than snapping `scroll_offset` there, so the final sub-row fraction
    /// eases in instead of jumping after the coast has slowed to a standstill.
    ///
    /// A keyboard page glide (`scroll_glide`, zero velocity) eases `scroll_offset`
    /// toward the `scroll_row` target the jump already set, clearing the flag on
    /// settle. It never writes `scroll_row` -- that is the fixed target the offset
    /// glides up to. A gap wider than three viewports (a big count-jump or a jump
    /// landing mid-glide) snaps instead so the offset never drags across the
    /// pool's buffered window.
    fn tick_scroll_anim(&mut self, dt: f32) -> bool {
        let mut animating = false;
        // Resolved before the loop because its &mut borrow forbids reading
        // settings inside. The default mirrors the post-key view-follow.
        let scrolloff = self.settings.scrolloff.unwrap_or(3);
        for editor in self.active_workspace_mut().editors.values_mut() {
            if editor.scroll_velocity != 0.0 {
                let max_offset = action_handlers::movement::max_scroll_offset(editor);
                let (offset, velocity, settled) = action_handlers::movement::step_scroll_momentum(
                    editor.scroll_offset,
                    editor.scroll_velocity,
                    dt,
                    max_offset,
                );
                editor.scroll_offset = offset;
                editor.scroll_velocity = velocity;
                if settled {
                    // A low settle speed can leave the offset up to half a row
                    // from its resting row. Snapping scroll_offset onto the
                    // rounded row jumps that remainder visibly, right as the
                    // coast slows to a near standstill. Arm a page glide onto
                    // the rounded row instead so the final fraction eases in.
                    editor.scroll_row = offset.round().clamp(0.0, max_offset) as u32;
                    action_handlers::movement::clamp_cursor_to_view(editor, scrolloff);
                    editor.scroll_glide = true;
                } else {
                    editor.scroll_row = offset.floor() as u32;
                    action_handlers::movement::clamp_cursor_to_view(editor, scrolloff);
                }
                animating = true;
            } else if editor.scroll_glide {
                let target = editor.scroll_row as f32;
                let viewport = editor
                    .viewport_rows
                    .unwrap_or(action_handlers::movement::DEFAULT_VIEWPORT_ROWS)
                    .max(1);
                if (target - editor.scroll_offset).abs() > viewport as f32 * 3.0 {
                    editor.scroll_offset = target;
                    editor.scroll_glide = false;
                } else {
                    let (offset, settled) = action_handlers::movement::step_scroll_ease(
                        editor.scroll_offset,
                        target,
                        dt,
                    );
                    editor.scroll_offset = offset;
                    if settled {
                        editor.scroll_glide = false;
                    }
                }
                animating |= editor.scroll_glide;
            }
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
    fn drain_pending(&mut self, events: &mut Receiver<Event>) -> (UpdateEffect, usize) {
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
        let warm = self.warm_index_load(&git_root);
        tracing::info!(
            target: "stoat::app",
            root = %git_root.display(),
            mode = if warm.is_some() { "warm" } else { "cold" },
            "index build starting",
        );
        if !self.persistence_disabled {
            let _ = self.fs_watch_host.watch_recursive(&git_root);
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
            warm,
        ));
    }

    /// Read the persisted manifest for a warm index load.
    ///
    /// Returns `None` to fall through to a full cold build when persistence
    /// is disabled, no manifest is present, or its [`codegraph::SCHEMA_VERSION`]
    /// no longer matches.
    fn warm_index_load(&self, git_root: &Path) -> Option<(PathBuf, codegraph::Manifest)> {
        if self.persistence_disabled {
            return None;
        }
        let dir = crate::code_index::store::index_dir_for(git_root, self.fs_host.as_ref()).ok()?;
        let manifest = crate::code_index::store::read_manifest(&dir, self.fs_host.as_ref()).ok()?;
        (manifest.schema_version == codegraph::SCHEMA_VERSION).then_some((dir, manifest))
    }

    /// Merge pending index updates into their workspace graphs.
    ///
    /// Each shard is inserted and, in non-test runs, written to disk. Reindex
    /// and remove updates apply without re-resolving inline. Every touched
    /// workspace has its cross-file references re-resolved once after the
    /// drain, so N queued updates cost one graph sweep rather than N.
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
        while let Ok(update) = self.index_update_rx.try_recv() {
            drained += 1;
            match update {
                IndexUpdate::Shard {
                    workspace,
                    rel_path,
                    shard,
                    persist,
                } => {
                    let bytes = (persist && !self.persistence_disabled)
                        .then(|| codegraph::encode_shard(&shard));
                    let Some(ws) = self.workspaces.get_mut(workspace) else {
                        continue;
                    };
                    ws.code_graph.insert_shard(shard);
                    ws.file_paths.insert(
                        crate::code_index::build::file_id(&rel_path),
                        PathBuf::from(&rel_path),
                    );
                    ws.index_generation += 1;
                    if let Some(bytes) = bytes {
                        let git_root = ws.git_root.clone();
                        if let Ok(dir) = crate::code_index::store::index_dir_for(
                            &git_root,
                            self.fs_host.as_ref(),
                        ) {
                            let _ = crate::code_index::store::write_shard(
                                &dir,
                                &rel_path,
                                &bytes,
                                self.fs_host.as_ref(),
                            );
                        }
                    }
                },
                IndexUpdate::Complete {
                    workspace,
                    manifest,
                } => {
                    resolve_pending.insert(workspace);
                    completed.insert(workspace);
                    if !self.persistence_disabled {
                        let git_root = self.workspaces.get(workspace).map(|ws| ws.git_root.clone());
                        if let Some(git_root) = git_root
                            && let Ok(dir) = crate::code_index::store::index_dir_for(
                                &git_root,
                                self.fs_host.as_ref(),
                            )
                        {
                            let _ = crate::code_index::store::write_manifest(
                                &dir,
                                &manifest,
                                self.fs_host.as_ref(),
                            );
                            if let Ok(pruned) = crate::code_index::store::prune_shards(
                                &dir,
                                &manifest,
                                self.fs_host.as_ref(),
                            ) && pruned > 0
                            {
                                tracing::info!(
                                    target: "stoat::app",
                                    pruned,
                                    "pruned stale index shards",
                                );
                            }
                        }
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
                    let git_root = ws.git_root.clone();
                    if let Some((bytes, content_hash)) = to_persist
                        && !self.persistence_disabled
                        && let Ok(dir) = crate::code_index::store::index_dir_for(
                            &git_root,
                            self.fs_host.as_ref(),
                        )
                    {
                        let _ = crate::code_index::store::write_shard(
                            &dir,
                            &rel_path,
                            &bytes,
                            self.fs_host.as_ref(),
                        );
                        let _ = crate::code_index::store::update_manifest_entry(
                            &dir,
                            &rel_path,
                            content_hash,
                            self.fs_host.as_ref(),
                        );
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
                    let git_root = ws.git_root.clone();
                    if !self.persistence_disabled
                        && let Ok(dir) = crate::code_index::store::index_dir_for(
                            &git_root,
                            self.fs_host.as_ref(),
                        )
                    {
                        let _ = crate::code_index::store::delete_shard(
                            &dir,
                            &rel_path,
                            self.fs_host.as_ref(),
                        );
                        let _ = crate::code_index::store::remove_manifest_entry(
                            &dir,
                            &rel_path,
                            self.fs_host.as_ref(),
                        );
                    }
                },
            }

            if drained >= INDEX_DRAIN_CAP {
                self.redraw_notify.notify_one();
                break;
            }
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

    /// Persist a saved buffer's shard and manifest entry so a later open
    /// warm-loads it instead of re-extracting.
    ///
    /// No-op when persistence is disabled or the buffer has no indexable
    /// language. Re-extracts from the saved text on the calling thread,
    /// which is acceptable on the infrequent save path.
    pub(crate) fn persist_saved_shard(&self, buffer_id: BufferId, path: &Path, text: &str) {
        if self.persistence_disabled {
            return;
        }
        let ws = self.active_workspace();
        let Some(language) = ws.buffers.language_for(buffer_id) else {
            return;
        };
        let git_root = ws.git_root.clone();
        let Some((rel_path, shard)) =
            crate::code_index::build::extract_shard(&language, &git_root, path, text)
        else {
            return;
        };
        let Ok(dir) = crate::code_index::store::index_dir_for(&git_root, self.fs_host.as_ref())
        else {
            return;
        };
        let _ = crate::code_index::store::write_shard(
            &dir,
            &rel_path,
            &codegraph::encode_shard(&shard),
            self.fs_host.as_ref(),
        );
        let _ = crate::code_index::store::update_manifest_entry(
            &dir,
            &rel_path,
            shard.content_hash,
            self.fs_host.as_ref(),
        );
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
        let executor = self.executor.clone();
        let fs_host = self.fs_host.clone();
        let registry = self.language_registry.clone();
        match self
            .active_workspace_mut()
            .restore_state(&path, &*fs_host, &executor)
        {
            Ok(()) => {
                self.active_workspace_mut()
                    .assign_languages_from_paths(&registry);
                action_handlers::respawn_terminal_panes(self);
            },
            Err(err) => tracing::warn!(
                ?path,
                ?err,
                "failed to restore workspace state; starting fresh"
            ),
        }
    }

    /// Persist a workspace's state to disk. Failures are logged and swallowed
    /// so a write error never prevents a clean shutdown or workspace switch.
    /// No-op when [`Self::persistence_disabled`] is set (used by the test
    /// harness to keep the real `$XDG_STATE_HOME` pristine) or when the
    /// workspace is still in its freshly-created state per
    /// [`Workspace::is_fresh`], so launches without `--continue` do not
    /// write a throwaway session file on quit.
    pub(crate) fn save_workspace(&self, ws: &Workspace) {
        if self.persistence_disabled {
            return;
        }
        if ws.is_fresh() {
            return;
        }
        let path = match crate::workspace::state_path_for(&ws.git_root, ws.uid, &*self.fs_host) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(?err, "could not resolve workspace state path");
                return;
            },
        };
        if let Err(err) = ws.save_state(&path, &*self.fs_host) {
            tracing::warn!(?path, ?err, "failed to save workspace state");
        }
    }

    /// Persist every open workspace. Invoked on quit so workspaces that were
    /// left in the background get their latest state written out.
    fn save_all_workspaces(&self) {
        for ws in self.workspaces.values() {
            self.save_workspace(ws);
        }
    }

    pub(crate) fn update(&mut self, event: Event) -> UpdateEffect {
        self.drain_lsp_notifications();
        self.drain_lsp_incoming_requests();
        self.install_pending_lsp_host();
        self.drain_fs_watch_events();
        self.drain_pending_external_edits();
        self.drain_pending_git_refresh();
        self.drain_pending_diff_warm_files();
        self.drain_pending_index_edits();
        let effect = match event {
            Event::Resize(w, h) => {
                self.size = Rect::new(0, 0, w, h);
                let size = self.size;
                self.active_workspace_mut().layout(size);
                UpdateEffect::Redraw
            },
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let before = self.focused_cursor_pos();
                let term_before = self.focused_shell_term_id();
                let effect = self.handle_key(key);
                self.auto_insert_focused_terminal(term_before);
                let cursor_moved = self.focused_cursor_pos() != before;

                // Re-follow the cursor when a key moved it, pulling the view
                // along so a count jump past the margin lands the view on the
                // cursor rather than stranding it on the edge. A keyboard scroll
                // (z j / z k) never moves the cursor, so its view stays put, and
                // a wheel coast already drags the cursor into view as it scrolls.
                let scrolloff = self.settings.scrolloff.unwrap_or(3);
                let scrolled = match action_handlers::focused_editor_mut(self) {
                    Some(editor) => {
                        cursor_moved
                            && action_handlers::movement::ensure_cursor_in_view(editor, scrolloff)
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
                let effect = self.handle_mouse(mouse);
                self.auto_insert_focused_terminal(term_before);
                effect
            },
            _ => UpdateEffect::None,
        };
        action_handlers::lsp::notify_buffer_changes_pending(self);
        crate::completion::request::trigger(self);
        action_handlers::lsp::signature_help_trigger(self);
        action_handlers::lsp::inlay_hints_trigger(self);
        action_handlers::lsp::document_highlight_trigger(self);
        action_handlers::lsp::pull_diagnostics_trigger(self);
        action_handlers::lsp::semantic_tokens_trigger(self);
        action_handlers::lsp::folding_ranges_trigger(self);
        effect
    }

    /// Drain queued [`crate::host::FsWatchEvent`]s from the active
    /// [`FsWatchHost`]. Each event arms (or resets) a 50ms
    /// per-path debounce via
    /// [`Self::arm_review_external_edit_debounce`] when a review
    /// session is active; the dispatch itself lands later from
    /// [`Self::drain_pending_external_edits`] once the timer
    /// fires. Cap matches [`Self::drain_lsp_notifications`] so a
    /// pathological burst can't starve the event loop.
    pub(crate) fn drain_fs_watch_events(&mut self) {
        let host = self.fs_watch_host.clone();
        let mut paths: Vec<PathBuf> = Vec::new();
        for _ in 0..256 {
            let Some(event) = host.try_recv() else {
                break;
            };
            tracing::trace!(
                target: "stoat::app",
                path = %event.path.display(),
                kind = ?event.kind,
                "fs watch event observed",
            );
            paths.push(event.path);
        }

        if paths.is_empty() {
            return;
        }
        // `review.follow` gates every automatic refresh below. A manual `r`
        // (ReviewRefresh) dispatches through a separate path and still works.
        let review_active =
            self.active_workspace().review.is_some() && self.settings.review_follow.unwrap_or(true);
        // With no review open, keep the diff cache warm incrementally instead,
        // gated the same as the full background warm.
        let precompute = self.active_workspace().review.is_none()
            && self.diff_warm_auto
            && self.settings.review_precompute.unwrap_or(true);
        let git_root = self.active_workspace().git_root.clone();
        let git_dir = git_root.join(".git");
        let mut repo: Option<Option<Arc<dyn GitRepo>>> = None;
        for path in paths {
            if review_active {
                let in_session = self
                    .active_workspace()
                    .review
                    .as_ref()
                    .is_some_and(|s| s.files.iter().any(|f| f.path == path));
                if in_session {
                    // A tracked file keeps the per-path debounce, which scrolls
                    // the review to the edited chunk when the refresh lands.
                    self.arm_review_external_edit_debounce(path.clone());
                } else if path.starts_with(&git_dir) {
                    // A .git write (a commit, reset, or branch switch) refreshes
                    // the whole session through one shared debounce.
                    self.arm_review_git_refresh_debounce();
                } else if path.starts_with(&git_root) {
                    // A change to a working-tree file not yet in the session
                    // pulls it in on the next refresh, unless gitignored so
                    // build churn such as target/ cannot thrash the rescan.
                    let repo = repo.get_or_insert_with(|| self.git_host.discover(&git_root));
                    if !repo.as_ref().is_some_and(|r| r.is_path_ignored(&path)) {
                        self.arm_review_git_refresh_debounce();
                    }
                }
            } else if precompute {
                if path.starts_with(&git_dir) {
                    // A .git write moved HEAD and staled every cached base, so
                    // re-arm the full warm through the shared git-refresh
                    // debounce. Its drain clears the diff_warmed flag.
                    self.arm_review_git_refresh_debounce();
                } else if path.starts_with(&git_root) {
                    // An edited working-tree file warms its own diff, unless
                    // gitignored so build churn cannot thrash the recompute.
                    let repo = repo.get_or_insert_with(|| self.git_host.discover(&git_root));
                    if !repo.as_ref().is_some_and(|r| r.is_path_ignored(&path)) {
                        self.arm_diff_warm_file_debounce(path.clone());
                    }
                }
            }
            if path.starts_with(&git_root) && self.language_registry.for_path(&path).is_some() {
                self.arm_index_external_edit_debounce(path);
            }
        }
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

    /// Schedule a debounced [`ReviewExternalEdit`] dispatch for
    /// `path`. Inserting into [`Self::review_pending_external_edits`]
    /// drops any prior task for the same path, which cancels the
    /// spawned future at its [`Executor::timer`] await; only the
    /// most recent burst event proceeds. The spawned task forwards
    /// `path` on [`Self::review_external_edit_tx`] when its
    /// [`REVIEW_EXTERNAL_EDIT_DEBOUNCE`] window elapses; the main
    /// loop drains the channel via
    /// [`Self::drain_pending_external_edits`] and dispatches the
    /// action there because async tasks cannot mutate `Stoat`.
    fn arm_review_external_edit_debounce(&mut self, path: PathBuf) {
        let executor = self.executor.clone();
        let tx = self.review_external_edit_tx.clone();
        let redraw = self.redraw_notify.clone();
        let path_for_send = path.clone();
        let task = self.executor.spawn_with_redraw(redraw, async move {
            executor.timer(REVIEW_EXTERNAL_EDIT_DEBOUNCE).await;
            let _ = tx.send(path_for_send).await;
        });
        self.review_pending_external_edits.insert(path, task);
    }

    /// Schedule a debounced whole-session [`ReviewRefresh`] after a git-state
    /// change under `<git_root>/.git`.
    ///
    /// The debounce is single-slot rather than per-path, so re-arming drops the
    /// prior task, which cancels its future at the [`Executor::timer`] await. A
    /// burst of `.git` writes from one commit then fires a single refresh once
    /// the [`REVIEW_EXTERNAL_EDIT_DEBOUNCE`] window elapses. The main loop drains
    /// it via [`Self::drain_pending_git_refresh`], since async tasks cannot
    /// mutate `Stoat`.
    fn arm_review_git_refresh_debounce(&mut self) {
        let executor = self.executor.clone();
        let tx = self.review_git_refresh_tx.clone();
        let redraw = self.redraw_notify.clone();
        let task = self.executor.spawn_with_redraw(redraw, async move {
            executor.timer(REVIEW_EXTERNAL_EDIT_DEBOUNCE).await;
            let _ = tx.send(()).await;
        });
        self.review_pending_git_refresh = Some(task);
    }

    /// Schedule a debounced single-file diff warm for `path` edited while review
    /// is closed. Mirrors [`Self::arm_review_external_edit_debounce`]: inserting
    /// into [`Self::pending_diff_warm_file`] drops any prior task for the same
    /// path, so only the latest burst event warms once its
    /// [`REVIEW_EXTERNAL_EDIT_DEBOUNCE`] window elapses. The spawned task
    /// forwards `path` on [`Self::diff_warm_file_tx`], drained by
    /// [`Self::drain_pending_diff_warm_files`], which spawns the warm off-thread.
    fn arm_diff_warm_file_debounce(&mut self, path: PathBuf) {
        let executor = self.executor.clone();
        let tx = self.diff_warm_file_tx.clone();
        let redraw = self.redraw_notify.clone();
        let path_for_send = path.clone();
        let task = self.executor.spawn_with_redraw(redraw, async move {
            executor.timer(REVIEW_EXTERNAL_EDIT_DEBOUNCE).await;
            let _ = tx.send(path_for_send).await;
        });
        self.pending_diff_warm_file.insert(path, task);
    }

    /// Drain every path the per-path debounce tasks have pushed onto
    /// [`Self::review_external_edit_tx`] since the last call. Each
    /// path becomes one [`ReviewExternalEdit`] dispatch when a review
    /// session is active; otherwise the path is dropped. Returns
    /// `true` if any dispatch fired so the test harness's settle
    /// loop can re-iterate. Cap matches
    /// [`Self::drain_fs_watch_events`].
    pub(crate) fn drain_pending_external_edits(&mut self) -> bool {
        let mut progressed = false;
        for _ in 0..256 {
            let Ok(path) = self.review_external_edit_rx.try_recv() else {
                break;
            };
            self.review_pending_external_edits.remove(&path);
            if self.active_workspace().review.is_some() {
                action_handlers::dispatch(self, &ReviewExternalEdit { path });
                progressed = true;
            }
        }
        progressed
    }

    /// Drain the git-refresh debounce marker and refresh the review when one
    /// fired since the last call.
    ///
    /// Only a [`ReviewSource::WorkingTree`] session refreshes. Commit and
    /// commit-range sources are fixed snapshots the rebase edit-pause review
    /// relies on not churning, and in-memory agent-edit sources are not
    /// git-backed. With no working-tree review, a `.git` change instead re-arms
    /// the full background warm, since HEAD moved and staled every cached base.
    /// Returns `true` if a refresh dispatched so the test harness settle loop
    /// re-iterates.
    pub(crate) fn drain_pending_git_refresh(&mut self) -> bool {
        let mut progressed = false;
        for _ in 0..256 {
            let Ok(()) = self.review_git_refresh_rx.try_recv() else {
                break;
            };
            self.review_pending_git_refresh = None;
            // A working-tree review refreshes on any git write. An auto_source
            // session refreshes too even when it currently displays a Commit,
            // so a rebase-fallback view re-decides and follows each rebase step.
            let refreshes = matches!(
                self.active_workspace().review.as_ref(),
                Some(s) if matches!(s.source, ReviewSource::WorkingTree { .. }) || s.auto_source
            );
            if refreshes {
                action_handlers::dispatch(self, &ReviewRefresh);
                progressed = true;
            } else {
                self.active_workspace_mut().diff_warmed = false;
            }
        }
        progressed
    }

    /// Drain the diff-warm debounce channel, spawning a single-file warm for
    /// each path edited while review was closed.
    ///
    /// Mirrors [`Self::drain_pending_external_edits`]. Skips a path when review
    /// has since opened -- its own refresh covers the edit -- or when
    /// `review.precompute` or the warm auto-gate is off. Returns `true` if a
    /// warm spawned so the test harness settle loop re-iterates.
    pub(crate) fn drain_pending_diff_warm_files(&mut self) -> bool {
        let mut progressed = false;
        for _ in 0..256 {
            let Ok(path) = self.diff_warm_file_rx.try_recv() else {
                break;
            };
            self.pending_diff_warm_file.remove(&path);
            let precompute = self.diff_warm_auto && self.settings.review_precompute.unwrap_or(true);
            if precompute && self.active_workspace().review.is_none() {
                crate::diff_warm::spawn_file_warm(self, path);
                progressed = true;
            }
        }
        progressed
    }

    /// Schedule a debounced reindex of `path` after an external change.
    ///
    /// Mirrors [`Self::arm_review_external_edit_debounce`]. A new event for
    /// the same path replaces the prior task, so only the latest of a burst
    /// proceeds once the [`REVIEW_EXTERNAL_EDIT_DEBOUNCE`] window elapses.
    fn arm_index_external_edit_debounce(&mut self, path: PathBuf) {
        let executor = self.executor.clone();
        let tx = self.index_external_edit_tx.clone();
        let redraw = self.redraw_notify.clone();
        let path_for_send = path.clone();
        let task = self.executor.spawn_with_redraw(redraw, async move {
            executor.timer(REVIEW_EXTERNAL_EDIT_DEBOUNCE).await;
            let _ = tx.send(path_for_send).await;
        });
        self.index_pending_external_edits.insert(path, task);
    }

    /// Drain the debounced external-change paths and reindex each. Returns
    /// `true` if any path was handled so the harness settle loop re-iterates.
    pub(crate) fn drain_pending_index_edits(&mut self) -> bool {
        let mut progressed = false;
        for _ in 0..256 {
            let Ok(path) = self.index_external_edit_rx.try_recv() else {
                break;
            };
            self.index_pending_external_edits.remove(&path);
            self.reindex_external_path(path);
            progressed = true;
        }
        progressed
    }

    /// Reindex a file changed outside the editor.
    ///
    /// A still-present file whose on-disk content matches the graph's
    /// recorded hash is skipped, since the editor's own save already
    /// indexed it. A changed file is re-extracted from disk. A file that no
    /// longer exists is removed from the graph.
    fn reindex_external_path(&mut self, path: PathBuf) {
        let workspace = self.active_workspace;
        let git_root = self.active_workspace().git_root.clone();
        let Some(rel_path) = crate::code_index::build::relpath(&git_root, &path) else {
            return;
        };
        let file = crate::code_index::build::file_id(&rel_path);

        let Some(hash) =
            crate::code_index::build::current_fingerprint(self.fs_host.as_ref(), &path)
        else {
            let _ = self.index_update_tx.send(IndexUpdate::Remove {
                workspace,
                file,
                rel_path,
            });
            self.redraw_notify.notify_one();
            return;
        };
        if self.active_workspace().code_graph.content_hash(file) == Some(hash) {
            return;
        }

        let handles = crate::code_index::build::IndexBuild {
            fs: self.fs_host.clone(),
            languages: self.language_registry.clone(),
            tx: self.index_update_tx.clone(),
            redraw: self.redraw_notify.clone(),
        };
        crate::code_index::build::reindex_path(&self.executor, handles, git_root, workspace, path)
            .detach();
    }

    /// Drains every notification currently buffered on
    /// [`crate::host::LspHost::try_recv_notification`] and dispatches
    /// each by variant. `Progress` updates the [`crate::lsp::progress::LspProgressMap`];
    /// other variants log via tracing for now and become future
    /// per-feature consumer hooks. Cap is per-tick to avoid starving
    /// the event loop on a pathological notification burst; the
    /// remainder drains on the next update.
    pub(crate) fn drain_lsp_notifications(&mut self) {
        for (server, host) in self.lsp_registry.named_hosts() {
            self.drain_notifications_from(&server, &host);
        }
    }

    fn drain_notifications_from(&mut self, server: &str, host: &Arc<dyn LspHost>) {
        use crate::host::LspNotification;
        use futures::FutureExt;
        for _ in 0..256 {
            // try_recv_notification is implemented on top of a
            // non-blocking channel poll, so its future resolves
            // synchronously; now_or_never returns Some immediately.
            // Any host that breaks that contract returns None here
            // and the drain ends safely.
            let Some(slot) = host.try_recv_notification().now_or_never() else {
                break;
            };
            let Some(notification) = slot else {
                break;
            };
            if self.lsp_progress.update(server, &notification) {
                continue;
            }
            match &notification {
                LspNotification::Diagnostics {
                    uri, diagnostics, ..
                } => {
                    if let Some(path) = lsp_uri_to_path(uri) {
                        let count = diagnostics.len();
                        self.diagnostics.replace_from_server(
                            path.clone(),
                            server.to_string(),
                            diagnostics.clone(),
                        );
                        tracing::info!(
                            target: "stoat::lsp",
                            path = %path.display(),
                            count,
                            "diagnostics applied",
                        );
                    } else {
                        tracing::debug!(
                            target: "stoat::app",
                            uri = uri.as_str(),
                            "diagnostics arrived for non-file URI; dropped",
                        );
                    }
                },
                LspNotification::ShowMessage { typ, message } => {
                    self.lsp_message = Some((*typ, message.clone()));
                },
                _ => {
                    tracing::debug!(
                        target: "stoat::app",
                        ?notification,
                        "unhandled LSP notification"
                    );
                },
            }
        }
    }

    /// Drain and answer server-to-client requests the LSP host has
    /// queued, so a server that pulls configuration or requests an edit
    /// does not block waiting on the editor.
    ///
    /// Mirrors [`Self::drain_lsp_notifications`] with a bounded
    /// `now_or_never` loop over
    /// [`crate::host::LspHost::try_recv_incoming_request`]. Each request
    /// carries an id the server blocks on, so every one is answered on a
    /// detached [`crate::host::LspHost::reply`] task. A `workspace/applyEdit`
    /// mutates buffers synchronously here because it needs `&mut self`. Only
    /// the reply is deferred.
    pub(crate) fn drain_lsp_incoming_requests(&mut self) {
        for host in self.lsp_registry.hosts() {
            self.drain_incoming_requests_from(&host);
        }
    }

    fn drain_incoming_requests_from(&mut self, host: &Arc<dyn LspHost>) {
        use crate::host::lsp::{IncomingRequest, LspResponseError};
        use futures::FutureExt;
        use lsp_types::ApplyWorkspaceEditResponse;
        use serde_json::Value;

        for _ in 0..256 {
            let Some(slot) = host.try_recv_incoming_request().now_or_never() else {
                break;
            };
            let Some(request) = slot else {
                break;
            };

            let id = request.id().clone();
            let result: Result<Value, LspResponseError> = match request {
                IncomingRequest::WorkDoneProgressCreate { params, .. } => {
                    tracing::debug!(target: "stoat::lsp", ?params, "workDoneProgress/create");
                    Ok(Value::Null)
                },
                IncomingRequest::RegisterCapability { params, .. } => {
                    tracing::debug!(target: "stoat::lsp", ?params, "client/registerCapability");
                    Ok(Value::Null)
                },
                IncomingRequest::UnregisterCapability { params, .. } => {
                    tracing::debug!(target: "stoat::lsp", ?params, "client/unregisterCapability");
                    Ok(Value::Null)
                },
                IncomingRequest::WorkspaceConfiguration { params, .. } => {
                    Ok(Value::Array(vec![Value::Null; params.items.len()]))
                },
                IncomingRequest::ShowMessageRequest { .. } => Ok(Value::Null),
                IncomingRequest::WorkspaceApplyEdit { params, .. } => {
                    let response = match crate::lsp::edit_apply::apply_workspace_edit(
                        self,
                        params.edit,
                    ) {
                        Ok(_) => ApplyWorkspaceEditResponse {
                            applied: true,
                            failure_reason: None,
                            failed_change: None,
                        },
                        Err(err) => {
                            tracing::warn!(target: "stoat::lsp", %err, "workspace/applyEdit failed");
                            ApplyWorkspaceEditResponse {
                                applied: false,
                                failure_reason: Some(err.to_string()),
                                failed_change: None,
                            }
                        },
                    };
                    serde_json::to_value(response).map_err(|err| LspResponseError {
                        code: -32603,
                        message: err.to_string(),
                        data: None,
                    })
                },
                IncomingRequest::Unknown { method, .. } => {
                    tracing::debug!(target: "stoat::lsp", %method, "unhandled server->client request");
                    Err(LspResponseError {
                        code: -32601,
                        message: "method not found".to_string(),
                        data: None,
                    })
                },
            };

            let reply_host = host.clone();
            self.executor
                .spawn(async move {
                    if let Err(err) = reply_host.reply(id, result).await {
                        tracing::warn!(target: "stoat::lsp", ?err, "lsp reply failed");
                    }
                })
                .detach();
        }
    }

    /// Install every language server that finished spawning since the last
    /// tick.
    ///
    /// The lazy-spawn tasks armed by
    /// [`action_handlers::lsp::notify_buffer_opened`] park ready
    /// [`crate::host::LocalLsp`] hosts in [`Self::pending_lsp_host`]. This
    /// drains the queue. Each ready host is registered via
    /// [`Self::install_ready_server`], and a failed spawn surfaces in the
    /// message row while its language keeps the [`NoopLsp`] placeholder.
    fn install_pending_lsp_host(&mut self) {
        let pending = std::mem::take(
            &mut *self
                .pending_lsp_host
                .lock()
                .expect("pending lsp host mutex"),
        );
        for spawn in pending {
            match spawn.result {
                Ok(host) => self.install_ready_server(spawn.server, spawn.language, host),
                Err(msg) => {
                    // The server never came up, so its language keeps the noop
                    // placeholder and the failure surfaces in the status bar
                    // rather than only the log. Retained so a later LSP action
                    // can restate why no server is up.
                    self.lsp_spawn_failed = Some(msg.clone());
                    self.set_status(format!("lsp: {msg}"));
                },
            }
        }
    }

    /// Register a ready `host` under its `server` name and `language`, then
    /// re-fire `did_open` for the open buffers of that language.
    ///
    /// Those buffers already sent `did_open` to the noop while the server was
    /// starting, so they are dropped from [`Self::lsp_opened`] and reopened to
    /// deliver the documents to the real server. Buffers of other languages
    /// keep their own servers untouched.
    fn install_ready_server(&mut self, server: String, language: String, host: Arc<dyn LspHost>) {
        self.lsp_registry.insert(server, host);
        let selectors = crate::lsp::servers::resolve_servers(&self.settings, &language)
            .iter()
            .map(|resolved| resolved.to_selector())
            .collect();
        self.lsp_registry.set_selectors(language.clone(), selectors);

        let reopen: Vec<(BufferId, PathBuf, String)> = {
            let buffers = &self.active_workspace().buffers;
            buffers
                .open_paths()
                .into_iter()
                .filter_map(|path| {
                    let id = buffers.id_for_path(&path)?;
                    if action_handlers::lsp::lsp_language_name(buffers, id).as_deref()
                        != Some(language.as_str())
                    {
                        return None;
                    }
                    let text = buffers
                        .get(id)?
                        .read()
                        .expect("buffer poisoned")
                        .rope()
                        .to_string();
                    Some((id, path, text))
                })
                .collect()
        };

        for (id, path, text) in reopen {
            self.lsp_opened.remove(&id);
            self.lsp_doc_versions.remove(&id);
            self.lsp_buffer_versions.remove(&id);
            action_handlers::lsp::notify_buffer_opened(self, id, &path, &text);
        }
    }

    /// Route a mouse press to the open finder or palette modal.
    ///
    /// Only a left [`MouseEventKind::Down`] acts: it selects the clicked list
    /// row. Drags, releases, and non-left presses return [`UpdateEffect::None`]
    /// so the buffer beneath keeps its cursor and focus. A click outside the
    /// list rect, or on an empty row past the last filtered item, is also a
    /// swallowed no-op.
    fn handle_modal_mouse(&mut self, mouse: MouseEvent) -> UpdateEffect {
        let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
            return UpdateEffect::None;
        };
        let size = self.size();

        let (list, selected, filtered_len) = if let Some(finder) = self.file_finder.as_ref() {
            let Some(layout) = crate::render::file_finder::file_finder_layout(size) else {
                return UpdateEffect::None;
            };
            let core = finder.active_core_ref();
            (
                layout.list,
                core.picklist.selected,
                core.picklist.filtered.len(),
            )
        } else if let Some(palette) = self.command_palette.as_ref() {
            if palette.command.is_none() {
                let Some(layout) = crate::render::command_palette::palette_filter_layout(size)
                else {
                    return UpdateEffect::None;
                };
                (layout.list, palette.selected, palette.filtered.len())
            } else if palette.arg_source().is_some()
                && let Some(picker) = palette.arg_picker.as_ref()
            {
                let Some(list) = crate::render::command_palette::palette_arg_list_rect(size) else {
                    return UpdateEffect::None;
                };
                let core = picker.active_core_ref();
                (list, core.picklist.selected, core.picklist.filtered.len())
            } else {
                return UpdateEffect::None;
            }
        } else {
            return UpdateEffect::None;
        };

        if !list.contains(Position::new(mouse.column, mouse.row)) {
            return UpdateEffect::None;
        }
        let rows = list.height as usize;
        let start_row = selected.saturating_sub(rows.saturating_sub(1));
        let index = start_row + (mouse.row - list.y) as usize;
        if index >= filtered_len {
            return UpdateEffect::None;
        }

        let delta = index as i32 - selected as i32;
        if self.file_finder.is_some() {
            action_handlers::file_finder_move_selection(self, delta)
        } else {
            action_handlers::palette_move_selection(self, delta).unwrap_or(UpdateEffect::Redraw)
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> UpdateEffect {
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            return self.handle_mouse_scroll(mouse);
        }
        if let MouseEventKind::Moved = mouse.kind {
            return self.handle_hover(mouse.column, mouse.row);
        }

        // While a finder or palette modal is open, its list owns the pointer: a
        // left click selects a row and every other press, drag, or release is
        // swallowed so nothing reaches divider arming, focus, or the panes.
        if self.file_finder.is_some() || self.command_palette.is_some() {
            return self.handle_modal_mouse(mouse);
        }

        // A divider drag owns the pointer once armed. It resizes on drag,
        // releases on up, and swallows the rest so pane handlers never see it.
        if self.divider_drag.is_some() {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some((node, gap)) = self.divider_drag {
                        self.active_workspace_mut().panes.set_divider(
                            node,
                            gap,
                            mouse.column,
                            mouse.row,
                        );
                    }
                },
                MouseEventKind::Up(MouseButton::Left) => self.divider_drag = None,
                _ => {},
            }
            return UpdateEffect::Redraw;
        }

        // A left-button drag over an open hover popup selects its text. Routed
        // ahead of focus_at and the pane handlers so the click never reaches the
        // editor, leaving the buffer selection and cursor untouched.
        if self.pending_hover.is_some()
            && let Some(effect) = self.handle_hover_selection_mouse(mouse)
        {
            return effect;
        }

        // A press or drag over a pane's minimap strip scrubs that pane, ahead of
        // focus_at and the text-area handlers so the strip owns the gesture.
        if let Some(effect) = self.handle_minimap_mouse(mouse) {
            return effect;
        }

        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            if let Some(hit) = self
                .active_workspace()
                .panes
                .divider_at(mouse.column, mouse.row)
            {
                self.divider_drag = Some(hit);
                return UpdateEffect::Redraw;
            }
            self.focus_at(mouse.column, mouse.row);
        }
        let Some((col, row)) = self.translate_mouse_to_focused(mouse.column, mouse.row) else {
            return UpdateEffect::None;
        };
        if self.handle_run_pane_mouse(mouse.kind, col, row) {
            return UpdateEffect::Redraw;
        }
        if self.handle_editor_pane_mouse(mouse.kind, col, row) {
            return UpdateEffect::Redraw;
        }
        tracing::trace!(
            target: "stoat::app",
            kind = ?mouse.kind,
            col,
            row,
            "mouse event routed to focused element"
        );
        UpdateEffect::None
    }

    /// Route a left press or drag over a pane's minimap strip to a viewport
    /// scrub. Returns `Some` when the event is consumed, `None` when it should
    /// fall through to focus and the text-area handlers.
    ///
    /// Once a press arms [`Self::minimap_drag`], every drag re-scrubs the named
    /// editor and the release clears the field, so the strip owns the pointer for
    /// the whole gesture and the text area never sees it.
    fn handle_minimap_mouse(&mut self, mouse: MouseEvent) -> Option<UpdateEffect> {
        if let Some(editor_id) = self.minimap_drag {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    self.scrub_minimap_editor(editor_id, mouse.row);
                },
                MouseEventKind::Up(MouseButton::Left) => self.minimap_drag = None,
                _ => {},
            }
            return Some(UpdateEffect::Redraw);
        }

        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            let pos = Position::new(mouse.column, mouse.row);
            let ws = self.active_workspace();
            let hit = ws.panes.split_panes().find_map(|(_, pane)| {
                let View::Editor(editor_id) = pane.view else {
                    return None;
                };
                let strip = ws.editors.get(editor_id)?.minimap_rect?;
                strip.contains(pos).then_some(editor_id)
            });
            if let Some(editor_id) = hit {
                self.minimap_drag = Some(editor_id);
                self.scrub_minimap_editor(editor_id, mouse.row);
                return Some(UpdateEffect::Redraw);
            }
        }

        None
    }

    /// Ease `editor_id`'s viewport onto the file line its minimap strip row under
    /// `screen_row` points at, centered in the viewport.
    ///
    /// Maps the strip-local cell row to a line with the same proportional math
    /// the strip renders with, then jumps `scroll_row` and glides the offset up
    /// to it like a page motion. A no-op if the editor has no strip this frame.
    fn scrub_minimap_editor(&mut self, editor_id: EditorId, screen_row: u16) {
        let ws = &mut self.workspaces[self.active_workspace];
        let Some(editor) = ws.editors.get_mut(editor_id) else {
            return;
        };
        let Some(strip) = editor.minimap_rect else {
            return;
        };

        let total = editor.display_map.snapshot().line_count();
        let viewport = editor.viewport_rows.unwrap_or(strip.height as u32).max(1);
        let strip_local_row = screen_row.saturating_sub(strip.y);
        let target_line = crate::minimap::click_target_line(
            strip.height,
            strip_local_row,
            total as f32,
            editor.scroll_offset,
            viewport as f32,
        );

        let max_scroll = total
            .saturating_sub(1)
            .saturating_sub(viewport.saturating_sub(1));
        let target_row = target_line.saturating_sub(viewport / 2).min(max_scroll);

        let prev = editor.scroll_row;
        editor.scroll_row = target_row;
        if editor.scroll_offset.floor() as u32 != prev {
            editor.scroll_offset = prev as f32;
        }
        editor.scroll_velocity = 0.0;
        editor.scroll_glide = true;
    }

    /// Route a left-button press over the open hover popup to its text
    /// selection. Returns `Some` when the event is consumed (a press, drag, or
    /// release over the popup), `None` when it should fall through to normal
    /// mouse handling.
    ///
    /// A press inside starts a drag selection. A press outside clears any
    /// selection and falls through, leaving the popup open. A release keeps the
    /// selection live and copies it to the clipboard when non-empty.
    fn handle_hover_selection_mouse(&mut self, mouse: MouseEvent) -> Option<UpdateEffect> {
        let popup_area = self.pending_hover.as_ref()?.area;
        let inside = popup_area.contains(Position {
            x: mouse.column,
            y: mouse.row,
        });
        let stoatty = self.stoatty;

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if !inside {
                    if let Some(popup) = self.pending_hover.as_mut() {
                        popup.selection = None;
                    }
                    return None;
                }
                let pos = crate::render::hover::hover_hit_test(
                    self.pending_hover.as_ref()?,
                    stoatty,
                    mouse.column,
                    mouse.row,
                );
                if let Some(popup) = self.pending_hover.as_mut() {
                    popup.selection = Some(action_handlers::lsp::HoverSelection {
                        anchor: pos,
                        head: pos,
                        dragging: true,
                    });
                }
                Some(UpdateEffect::Redraw)
            },
            MouseEventKind::Drag(MouseButton::Left) => {
                if !self.hover_selection_dragging() {
                    return None;
                }
                let pos = crate::render::hover::hover_hit_test(
                    self.pending_hover.as_ref()?,
                    stoatty,
                    mouse.column,
                    mouse.row,
                );
                if let Some(sel) = self
                    .pending_hover
                    .as_mut()
                    .and_then(|p| p.selection.as_mut())
                {
                    sel.head = pos;
                }
                Some(UpdateEffect::Redraw)
            },
            MouseEventKind::Up(MouseButton::Left) => {
                if !self.hover_selection_dragging() {
                    return None;
                }
                if let Some(sel) = self
                    .pending_hover
                    .as_mut()
                    .and_then(|p| p.selection.as_mut())
                {
                    sel.dragging = false;
                }
                let text = crate::render::hover::hover_selected_text(self.pending_hover.as_ref()?);
                if text.is_empty() {
                    if let Some(popup) = self.pending_hover.as_mut() {
                        popup.selection = None;
                    }
                } else {
                    crate::host::clipboard_copy(
                        self.clipboard_host().as_ref(),
                        self.env_host().as_ref(),
                        &text,
                    );
                }
                Some(UpdateEffect::Redraw)
            },
            _ => None,
        }
    }

    fn hover_selection_dragging(&self) -> bool {
        self.pending_hover
            .as_ref()
            .and_then(|p| p.selection)
            .is_some_and(|s| s.dragging)
    }

    /// Scrolls the pane under the wheel pointer.
    ///
    /// A `View::Editor` split pane gets inertial velocity, so a notch starts
    /// or accelerates a momentum glide. A `View::Run` pane (split or dock) does
    /// plain stepped scrolling of its output, three rows per notch, clamped to
    /// the top. Anything else drops the event.
    fn handle_mouse_scroll(&mut self, mouse: MouseEvent) -> UpdateEffect {
        // A wheel while a finder or palette modal is open moves its selection
        // rather than scrolling the pane beneath, so the event never falls
        // through. The two modals are mutually exclusive, so two checks suffice.
        if self.file_finder.is_some() || self.command_palette.is_some() {
            let down = match mouse.kind {
                MouseEventKind::ScrollDown => true,
                MouseEventKind::ScrollUp => false,
                _ => return UpdateEffect::None,
            };
            let size = self.size();

            // A wheel over the visible preview pane scrolls the preview content
            // instead of moving the selection, mirroring the editor-pane path.
            let preview = if let Some(finder) = self.file_finder.as_ref() {
                crate::render::file_finder::file_finder_layout(size)
                    .and_then(|layout| layout.preview)
                    .map(|rect| (rect, finder.active_core_ref().preview.editor))
            } else if let Some(palette) = self.command_palette.as_ref() {
                if palette.arg_source().is_some()
                    && let Some(picker) = palette.arg_picker.as_ref()
                {
                    crate::render::command_palette::palette_arg_body(size)
                        .and_then(|(_, preview)| preview)
                        .map(|rect| (rect, picker.active_core_ref().preview.editor))
                } else {
                    None
                }
            } else {
                None
            };

            if let Some((rect, editor_id)) = preview
                && rect.contains(Position::new(mouse.column, mouse.row))
            {
                if let Some(editor) = self.active_workspace_mut().editors.get_mut(editor_id) {
                    action_handlers::movement::wheel_impulse(editor, down);
                }
                return UpdateEffect::None;
            }

            let delta = if down { 1 } else { -1 };
            return if self.file_finder.is_some() {
                action_handlers::file_finder_move_selection(self, delta)
            } else {
                action_handlers::palette_move_selection(self, delta).unwrap_or(UpdateEffect::Redraw)
            };
        }

        // A wheel over the open hover popup scrolls the popup, not the pane
        // beneath it. The bump mirrors the Ctrl-d/Ctrl-u path. render_hover
        // clamps it to the content height.
        if let Some(popup) = self.pending_hover.as_mut()
            && popup.area.contains(Position::new(mouse.column, mouse.row))
        {
            match mouse.kind {
                MouseEventKind::ScrollDown => popup.scroll_half_pages += 1,
                MouseEventKind::ScrollUp => {
                    popup.scroll_half_pages = popup.scroll_half_pages.saturating_sub(1)
                },
                _ => {},
            }
            return UpdateEffect::Redraw;
        }

        let Some(target) = self.target_at(mouse.column, mouse.row) else {
            return UpdateEffect::None;
        };
        let ws = self.active_workspace_mut();

        // Snapshot the view and pane area under the cursor so the scroll below
        // can take a fresh mutable borrow of the run or editor state.
        let (view, area) = match target {
            FocusTarget::SplitPane(pid) => {
                let pane = ws.panes.pane(pid);
                (pane.view.clone(), pane.area)
            },
            FocusTarget::Dock(dock_id) => match ws.docks.get(dock_id) {
                Some(dock) => (dock.view.clone(), dock.area),
                None => return UpdateEffect::None,
            },
        };

        match view {
            View::Editor(id) => {
                let Some(editor) = ws.editors.get_mut(id) else {
                    return UpdateEffect::None;
                };
                let down = matches!(mouse.kind, MouseEventKind::ScrollDown);
                action_handlers::movement::wheel_impulse(editor, down);
                // No repaint here. The wheel only imparts velocity, and the
                // frame tick drives the glide and its renders. A trackpad sends
                // ~100 events per flick, so repainting per event would saturate
                // the loop with re-renders of an unscrolled view.
                UpdateEffect::None
            },
            View::Run(id) => {
                let Some(run_state) = ws.runs.get_mut(id) else {
                    return UpdateEffect::None;
                };
                let page = (area.height as usize).saturating_sub(1);
                let max = run_state.output_line_total().saturating_sub(page);
                run_state.scroll_offset = match mouse.kind {
                    MouseEventKind::ScrollUp => (run_state.scroll_offset + 3).min(max),
                    MouseEventKind::ScrollDown => run_state.scroll_offset.saturating_sub(3),
                    _ => return UpdateEffect::None,
                };
                UpdateEffect::Redraw
            },
            _ => UpdateEffect::None,
        }
    }

    fn handle_run_pane_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) -> bool {
        let target = {
            let ws = self.active_workspace();
            match ws.focus {
                FocusTarget::SplitPane(pane_id) => {
                    let pane = ws.panes.pane(pane_id);
                    if let View::Run(id) = pane.view {
                        Some((id, pane.area))
                    } else {
                        None
                    }
                },
                FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).and_then(|dock| {
                    if let View::Run(id) = dock.view {
                        Some((id, dock.area))
                    } else {
                        None
                    }
                }),
            }
        };
        let Some((run_id, area)) = target else {
            return false;
        };
        let clipboard_host = self.clipboard_host.clone();
        let env_host = self.env_host.clone();
        let ws = self.active_workspace_mut();
        let Some(run_state) = ws.runs.get_mut(run_id) else {
            return false;
        };
        let pos = run_state.active_block_grid_pos(area, col, row);
        let Some(block) = run_state.active_block_mut() else {
            return false;
        };
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(pos) = pos else {
                    return false;
                };
                block.selection = Some(GridSelection {
                    anchor: pos,
                    head: pos,
                });
                true
            },
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(pos) = pos else {
                    return false;
                };
                let Some(sel) = block.selection.as_mut() else {
                    return false;
                };
                if sel.head == pos {
                    return false;
                }
                sel.head = pos;
                true
            },
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(sel) = block.selection.as_ref() else {
                    return false;
                };
                if sel.anchor == sel.head {
                    return false;
                }
                let text = block.grid.text_for_selection(sel);
                if text.is_empty() {
                    return false;
                }
                crate::host::clipboard_copy(clipboard_host.as_ref(), env_host.as_ref(), &text);
                false
            },
            _ => false,
        }
    }

    /// Handles left-button Down/Drag/Up events on a focused editor
    /// pane. `Down(Left)` collapses the primary selection at the
    /// clicked offset and arms `editor_drag`; `Drag(Left)` extends
    /// the head of the dragged editor's primary selection;
    /// `Up(Left)` writes any non-empty primary-selection text to
    /// the clipboard (and conditionally OSC 52 emits) before
    /// clearing `editor_drag`. Clicks outside the pane's rendered
    /// text area saturate to the nearest valid offset via
    /// `clip_point` (Bias::Left). Returns `true` when the event
    /// mutated state.
    /// The focused pane's editor id and area, or `None` when the focus is not
    /// on an editor view.
    fn focused_editor_target(&self) -> Option<(EditorId, Rect)> {
        let ws = self.active_workspace();
        match ws.focus {
            FocusTarget::SplitPane(pane_id) => {
                let pane = ws.panes.pane(pane_id);
                if let View::Editor(id) = pane.view {
                    Some((id, pane.area))
                } else {
                    None
                }
            },
            FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).and_then(|dock| {
                if let View::Editor(id) = dock.view {
                    Some((id, dock.area))
                } else {
                    None
                }
            }),
        }
    }

    /// Index of the diagnostic under the terminal cell `(column, row)` in the
    /// focused editor pane, or `None` when the pointer is off a diagnostic or
    /// off the focused editor.
    fn resolve_hover_diagnostic(&mut self, column: u16, row: u16) -> Option<usize> {
        let (col, row) = self.translate_mouse_to_focused(column, row)?;
        let (editor_id, area) = self.focused_editor_target()?;
        let offset = self.editor_screen_to_offset(editor_id, area, col, row)?;

        let path = {
            let ws = self.active_workspace();
            let editor = ws.editors.get(editor_id)?;
            ws.buffers.path_for(editor.buffer_id)?.to_owned()
        };
        let snapshot = {
            let ws = self.active_workspace_mut();
            ws.editors.get_mut(editor_id)?.display_map.snapshot()
        };
        let rope = snapshot.buffer_snapshot().rope();
        crate::render::editor::diagnostic_at_offset(&self.diagnostics, &path, rope, offset)
    }

    /// Track the hovered cell and redraw only when the diagnostic under it
    /// changes, so mouse motion within one span does not repaint every event.
    fn handle_hover(&mut self, column: u16, row: u16) -> UpdateEffect {
        self.hover_cell = Some((column, row));
        let resolved = self.resolve_hover_diagnostic(column, row);
        if self.hover_diag == resolved {
            return UpdateEffect::None;
        }
        self.hover_diag = resolved;
        UpdateEffect::Redraw
    }

    fn handle_editor_pane_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) -> bool {
        let Some((editor_id, area)) = self.focused_editor_target() else {
            return false;
        };

        let clipboard_host = self.clipboard_host.clone();
        let env_host = self.env_host.clone();

        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(offset) = self.editor_screen_to_offset(editor_id, area, col, row) else {
                    return false;
                };
                let buffer_id = {
                    let ws = self.active_workspace_mut();
                    let editor = ws.editors.get_mut(editor_id).expect("editor exists");
                    let snapshot = editor.display_map.snapshot();
                    let buf_snap = snapshot.buffer_snapshot();
                    let anchor = buf_snap.anchor_at(offset, Bias::Right);
                    editor.selections.set_single_range(
                        anchor,
                        anchor,
                        stoat_text::SelectionGoal::None,
                    );
                    editor.buffer_id
                };
                self.editor_drag = Some((editor_id, buffer_id));
                true
            },
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some((drag_editor, _)) = self.editor_drag else {
                    return false;
                };
                if drag_editor != editor_id {
                    return false;
                }
                let Some(offset) = self.editor_screen_to_offset(editor_id, area, col, row) else {
                    return false;
                };
                let ws = self.active_workspace_mut();
                let editor = ws.editors.get_mut(editor_id).expect("editor exists");
                let snapshot = editor.display_map.snapshot();
                let buf_snap = snapshot.buffer_snapshot();
                let head_anchor = buf_snap.anchor_at(offset, Bias::Right);
                editor.selections.transform(buf_snap, |sel| {
                    let tail_anchor = sel.tail();
                    let tail_offset = buf_snap.resolve_anchor(&tail_anchor);
                    let mut new = sel.clone();
                    new.goal = stoat_text::SelectionGoal::None;
                    if offset < tail_offset {
                        new.start = head_anchor;
                        new.end = tail_anchor;
                        new.reversed = true;
                    } else {
                        new.start = tail_anchor;
                        new.end = head_anchor;
                        new.reversed = false;
                    }
                    new
                });
                true
            },
            MouseEventKind::Up(MouseButton::Left) => {
                if self.editor_drag.is_none() {
                    return false;
                }
                let text = {
                    let ws = self.active_workspace_mut();
                    let editor = ws.editors.get_mut(editor_id).expect("editor exists");
                    let snapshot = editor.display_map.snapshot();
                    let buf_snap = snapshot.buffer_snapshot();
                    let sel = editor.selections.newest_anchor();
                    let start = buf_snap.resolve_anchor(&sel.start);
                    let end = buf_snap.resolve_anchor(&sel.end);
                    if start == end {
                        String::new()
                    } else {
                        buf_snap.rope().slice(start..end).to_string()
                    }
                };
                self.editor_drag = None;
                if text.is_empty() {
                    return false;
                }
                crate::host::clipboard_copy(clipboard_host.as_ref(), env_host.as_ref(), &text);
                false
            },
            _ => false,
        }
    }

    fn editor_screen_to_offset(
        &mut self,
        editor_id: EditorId,
        area: Rect,
        col: u16,
        row: u16,
    ) -> Option<usize> {
        if col >= area.width || row >= area.height {
            return None;
        }
        let ws = self.active_workspace_mut();
        let editor = ws.editors.get_mut(editor_id)?;
        let scroll_row = editor.scroll_row;
        // The diff view puts the editable text in the right column, so a click
        // maps against the right column's start rather than the left gutter.
        // Cells left of it (the base column and gutters) clamp to the line start.
        let gutter_width = if editor.diff_view {
            crate::render::review::right_text_x(area).saturating_sub(area.x)
        } else {
            editor.gutter_width
        };
        let snapshot = editor.display_map.snapshot();
        crate::render::editor::display_cell_to_offset(&snapshot, scroll_row, gutter_width, col, row)
    }

    /// Returns the focused element's area-relative cell for the given
    /// terminal-relative `(column, row)`. Coordinates above or left of
    /// the focused element saturate to `0`. Returns `None` when the
    /// focus points at a dock that no longer exists in the workspace's
    /// dock map.
    pub(crate) fn translate_mouse_to_focused(&self, column: u16, row: u16) -> Option<(u16, u16)> {
        let ws = self.active_workspace();
        let area = match ws.focus {
            FocusTarget::SplitPane(pane_id) => ws.panes.pane(pane_id).area,
            FocusTarget::Dock(dock_id) => ws.docks.get(dock_id)?.area,
        };
        Some((column.saturating_sub(area.x), row.saturating_sub(area.y)))
    }

    /// Hit-tests a terminal-global `(column, row)` against the active
    /// workspace's focusable panels.
    ///
    /// Split panes are tested before docks. Returns `None` for a point in a
    /// divider gap or over no panel. A hidden dock has a zero-width `area`, so
    /// it never matches.
    fn target_at(&self, column: u16, row: u16) -> Option<FocusTarget> {
        let ws = self.active_workspace();
        let pos = Position::new(column, row);
        for (id, pane) in ws.panes.split_panes() {
            if pane.area.contains(pos) {
                return Some(FocusTarget::SplitPane(id));
            }
        }
        ws.docks
            .iter()
            .find(|(_, dock)| dock.area.contains(pos))
            .map(|(id, _)| FocusTarget::Dock(id))
    }

    /// Moves focus to the panel under a terminal-global `(column, row)`. A
    /// point over no panel is a no-op.
    ///
    /// A split-pane target updates both the pane tree's focus and the
    /// workspace focus so the two stay in sync, mirroring the keyboard focus
    /// path. A dock target leaves the pane tree's focus at the last split
    /// pane.
    fn focus_at(&mut self, column: u16, row: u16) {
        let Some(target) = self.target_at(column, row) else {
            return;
        };
        // A mouse focus change closes any open hover. Its popup was anchored
        // against the previously focused editor and must not re-anchor here.
        if self.active_workspace().focus != target {
            self.pending_hover = None;
            self.pending_hover_request = None;
        }
        let ws = self.active_workspace_mut();
        ws.focus = target;
        if let FocusTarget::SplitPane(id) = target {
            ws.panes.set_focus(id);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> UpdateEffect {
        // A version notice is a one-shot message. Any key press retires it.
        self.badges
            .remove_by_source(crate::badge::BadgeSource::Version);
        self.lsp_message = None;

        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(run_id) = self.modal_run {
                let ws = self.active_workspace_mut();
                if let Some(run_state) = ws.runs.get_mut(run_id) {
                    if let Some(handle) = &mut run_state.shell_handle {
                        handle.kill();
                    }
                    if let Some(block) = run_state.active_block_mut() {
                        block.finished = true;
                    }
                }
                return UpdateEffect::Redraw;
            }
            if self.help.is_some() {
                action_handlers::close_help(self);
                return UpdateEffect::Redraw;
            }
            if self.file_finder.is_some() {
                action_handlers::close_file_finder(self);
                return UpdateEffect::Redraw;
            }
            if let Some(palette) = self.command_palette.take() {
                let active_idx = self.active_workspace;
                palette.dispose(&mut self.workspaces[active_idx]);
                return UpdateEffect::Redraw;
            }
            if self.workspace_picker.is_some() {
                self.workspace_picker = None;
                return UpdateEffect::Redraw;
            }
            if self.quit_all_confirm.is_some() {
                self.quit_all_confirm = None;
                return UpdateEffect::Redraw;
            }
            if self.jumplist_picker.take().is_some() {
                return UpdateEffect::Redraw;
            }
            if self.diagnostics_picker.take().is_some() {
                return UpdateEffect::Redraw;
            }
            if self.location_picker.take().is_some() {
                return UpdateEffect::Redraw;
            }
            if self.global_search.take().is_some() {
                return UpdateEffect::Redraw;
            }
            if self.pending_hover.is_some() {
                self.pending_hover = None;
                self.pending_hover_request = None;
                return UpdateEffect::Redraw;
            }
            if let Some(agent_id) = self.term_input_target() {
                self.write_to_term(agent_id, &[0x03]);
                return UpdateEffect::None;
            }
            // Ctrl-C with a keymap binding (`pane == run` -> RunInterrupt) routes
            // through the keymap below. An unbound Ctrl-C quits.
            let bound = {
                let state = StoatKeymapState::from_stoat(self);
                self.keymap.lookup(&state, &key).is_some()
            };
            if !bound {
                return UpdateEffect::Quit;
            }
        }

        let key = normalize_shift_event(key);

        if self.pending_macro_replay {
            self.pending_macro_replay = false;
            if let KeyCode::Char(ch) = key.code {
                return action_handlers::macro_recording::execute_replay(self, ch);
            }
            return UpdateEffect::Redraw;
        }

        let is_record_macro_toggle = {
            let state = StoatKeymapState::from_stoat(self);
            self.keymap
                .lookup(&state, &key)
                .map(|actions| actions.iter().any(|a| a.name == "RecordMacro"))
                .unwrap_or(false)
        };
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

        if self.focused_mode() == "insert" {
            // A non-printable key the keymap binds falls through to the lookup
            // below, so bindings like `pane == run { Enter -> RunSubmit }`
            // override the built-in insert arms. Printable characters always
            // type, and an unbound key keeps today's insert defaults.
            let printable = matches!(key.code, KeyCode::Char(_))
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT);
            // handle_insert_key keeps priority for printable typing and for its
            // transient sub-modes (a completion popup, a pending insert
            // register), whose keys it owns; otherwise a keymap binding for a
            // non-printable key wins over the built-in defaults.
            let insert_first =
                printable || self.pending_completion.is_some() || self.pending_insert_register;
            let keymap_binds = !insert_first && {
                let state = StoatKeymapState::from_stoat(self);
                self.keymap.lookup(&state, &key).is_some()
            };
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

        if (self.focused_mode() == "normal" || self.focused_mode() == "select")
            && self.pending_code_action_picker.is_some()
        {
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

        if (self.focused_mode() == "normal" || self.focused_mode() == "select")
            && self.pending_symbol_picker.is_some()
        {
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

        if (self.focused_mode() == "normal" || self.focused_mode() == "select")
            && self.pending_workspace_symbol_picker.is_some()
        {
            if let KeyCode::Char(ch) = key.code {
                match ch {
                    'j' => {
                        if let Some(picker) = self.pending_workspace_symbol_picker.as_mut() {
                            let max = picker.entries.len().saturating_sub(1);
                            picker.selected_idx = (picker.selected_idx + 1).min(max);
                        }
                        return UpdateEffect::Redraw;
                    },
                    'k' => {
                        if let Some(picker) = self.pending_workspace_symbol_picker.as_mut() {
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
                        .pending_workspace_symbol_picker
                        .as_ref()
                        .map(|p| {
                            crate::render::symbol_picker::viewport_top_for_picker(
                                p.selected_idx,
                                p.entries.len(),
                            )
                        })
                        .unwrap_or(0);
                    let index = viewport_top + (digit as usize - 1);
                    action_handlers::lsp::pick_workspace_symbol(self, index);
                    return UpdateEffect::Redraw;
                }
            }
            if matches!(key.code, KeyCode::Enter) {
                let index = self
                    .pending_workspace_symbol_picker
                    .as_ref()
                    .map(|p| p.selected_idx);
                if let Some(index) = index {
                    action_handlers::lsp::pick_workspace_symbol(self, index);
                    return UpdateEffect::Redraw;
                }
            }
            if matches!(key.code, KeyCode::Esc) {
                self.pending_workspace_symbol_picker = None;
                self.pending_workspace_symbol_request = None;
                return UpdateEffect::Redraw;
            }
            self.pending_workspace_symbol_picker = None;
            self.pending_workspace_symbol_request = None;
        }

        // A hover popup consumes half-page scroll keys and auto-closes on any
        // other key (Helix's popup behavior). Ctrl-d/PageDown and Ctrl-u/PageUp
        // scroll it while open, shadowing normal-mode half-page motion. Escape
        // closes and is consumed. Every other key closes it and then dispatches,
        // which also covers the SetMode-only keys that `continue` before the
        // post-dispatch clear below. Ctrl-c is consumed by the close in the
        // Ctrl-c block above, so it never reaches here.
        if (self.focused_mode() == "normal" || self.focused_mode() == "select")
            && self.pending_hover.is_some()
        {
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

        if (self.focused_mode() == "normal" || self.focused_mode() == "select")
            && self.pending_find.is_some()
        {
            if let KeyCode::Char(ch) = key.code {
                let (kind, extend, count) = self.pending_find.take().expect("checked above");
                return action_handlers::movement::execute_find(self, kind, ch, extend, count);
            }
            self.pending_find = None;
        }

        if self.focused_mode() == "normal" && self.pending_mark.is_some() {
            if let KeyCode::Char(ch) = key.code {
                let request = self.pending_mark.take().expect("checked above");
                return action_handlers::marks::execute_mark(self, request, ch);
            }
            self.pending_mark = None;
        }

        if (self.focused_mode() == "normal" || self.focused_mode() == "select")
            && self.pending_replace
        {
            if let KeyCode::Char(ch) = key.code {
                self.pending_replace = false;
                return action_handlers::movement::execute_replace(self, ch);
            }
            self.pending_replace = false;
        }

        if (self.focused_mode() == "normal" || self.focused_mode() == "select")
            && self.pending_surround_add
        {
            if let KeyCode::Char(ch) = key.code {
                self.pending_surround_add = false;
                return action_handlers::surround::execute_surround_add(self, ch);
            }
            self.pending_surround_add = false;
        }

        if (self.focused_mode() == "normal" || self.focused_mode() == "select")
            && self.pending_register_select
        {
            if let KeyCode::Char(ch) = key.code {
                self.pending_register_select = false;
                action_handlers::yank::execute_select_register(self, ch);
                return UpdateEffect::Redraw;
            }
            self.pending_register_select = false;
        }

        if (self.focused_mode() == "normal" || self.focused_mode() == "select")
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

        if (self.focused_mode() == "normal" || self.focused_mode() == "select")
            && self.pending_surround_delete
        {
            if let KeyCode::Char(ch) = key.code {
                self.pending_surround_delete = false;
                return action_handlers::surround::execute_surround_delete(self, ch);
            }
            self.pending_surround_delete = false;
        }

        if (self.focused_mode() == "normal" || self.focused_mode() == "select")
            && self.pending_textobject_select.is_some()
        {
            if let KeyCode::Char(ch) = key.code {
                let mode = self.pending_textobject_select.expect("checked above");
                self.pending_textobject_select = None;
                return action_handlers::textobject::execute_select_textobject(self, mode, ch);
            }
            self.pending_textobject_select = None;
        }

        if (self.focused_mode() == "normal" || self.focused_mode() == "select")
            && self.pending_goto_word.is_some()
        {
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

        let count_active_mode = self.focused_mode() == "normal" || self.focused_mode() == "select";
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

        let state = StoatKeymapState::from_stoat(self);
        let actions = self.keymap.lookup(&state, &key).map(|a| a.to_vec());
        let Some(actions) = actions else {
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
        let mut dispatched_workspace_symbol_picker = false;
        for ra in &actions {
            if ra.name == "SetMode" {
                if let Some(mode_name) = ra.args.first().and_then(crate::keymap_state::arg_as_str) {
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
            if ra.name == "OpenWorkspaceSymbolPicker" {
                dispatched_workspace_symbol_picker = true;
            }
            if let Some(action) = resolve_action(&ra.name, &ra.args) {
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
            if !dispatched_workspace_symbol_picker {
                self.pending_workspace_symbol_picker = None;
                self.pending_workspace_symbol_request = None;
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
    /// A `View::Terminal` pane auto-enters insert when focus arrives
    /// ([`Self::auto_insert_focused_terminal`]), so typing reaches the shell
    /// with no `i`. A `View::Agent` pane is entered manually with `i`, and both
    /// leave via the [`Self::route_key_to_term`] escape.
    fn term_input_target(&self) -> Option<TermId> {
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
    /// For a `View::Terminal` pane the normal mode is temporary. Refocusing it
    /// re-enters insert ([`Self::auto_insert_focused_terminal`]), so `Esc` is a
    /// drop to normal for pane navigation rather than a lasting exit. A
    /// `View::Agent` pane stays in normal until the user presses `i`.
    fn route_key_to_term(&mut self, agent_id: TermId, key: KeyEvent) -> UpdateEffect {
        if key.code == KeyCode::Esc {
            self.transition_mode("normal".to_string());
            return UpdateEffect::Redraw;
        }

        if let Some(bytes) = encode_key_to_pty(&key) {
            self.write_to_term(agent_id, &bytes);
        }
        UpdateEffect::None
    }

    /// Write raw bytes to an agent's PTY.
    ///
    /// Uses `now_or_never` because the local PTY and the test fake complete
    /// writes synchronously, so keystrokes reach the agent in order without
    /// spawning a task. A write that errors or cannot complete synchronously is
    /// dropped with a warning rather than stalling input.
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
    fn focused_cursor_pos(&mut self) -> Option<(BufferId, usize)> {
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

    pub(crate) fn focused_editor_ids(&self) -> Option<(EditorId, BufferId)> {
        let ws = self.active_workspace();

        if let Some(finder) = &self.file_finder {
            return Some((finder.input.editor_id, finder.input.buffer_id));
        }

        if let Some(palette) = &self.command_palette
            && let Some(input) = palette.focused_input()
        {
            return Some((input.editor_id, input.buffer_id));
        }

        if let Some(help) = &self.help {
            return Some((help.input.editor_id, help.input.buffer_id));
        }

        if let Some(rename) = &self.rename_input {
            return Some((rename.input.editor_id, rename.input.buffer_id));
        }

        if let Some(ws_sym) = &self.workspace_symbol_input {
            return Some((ws_sym.input.editor_id, ws_sym.input.buffer_id));
        }

        if let Some(search) = &self.search_input {
            return Some((search.input.editor_id, search.input.buffer_id));
        }

        if let Some(gs) = &self.global_search_input {
            return Some((gs.input.editor_id, gs.input.buffer_id));
        }

        if let Some(ss) = &self.split_selection_input {
            return Some((ss.input.editor_id, ss.input.buffer_id));
        }

        if let Some(fs) = &self.filter_selections_input {
            return Some((fs.input.editor_id, fs.input.buffer_id));
        }

        if let Some(sh) = &self.shell_input {
            return Some((sh.input.editor_id, sh.input.buffer_id));
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
            FocusTarget::SplitPane(_) => {
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
    /// focused pane is a document editor running inside stoatty, else `None`.
    ///
    /// Returns the position [`crate::render::editor::render_editor_with_overlay`]
    /// recorded while painting the current frame, so it is exactly where the
    /// cursor cell would otherwise be drawn. `None` for finder/palette/dock/run
    /// focus and outside stoatty, where the editor paints its own cursor cell
    /// and the terminal cursor stays hidden. Must be called after a render.
    pub(crate) fn primary_cursor_screen_pos(&self) -> Option<(u16, u16)> {
        let (focused_id, _) = self.focused_editor_ids()?;
        let ws = self.active_workspace();
        let FocusTarget::SplitPane(_) = ws.focus else {
            return None;
        };
        let pane_editor = match ws.panes.pane(ws.panes.focus()).view {
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
                self.editor_insert(editor_id, buffer_id, &fragments.join("\n"));
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
            KeyCode::Esc if self.pending_completion.is_some() => {
                self.pending_completion = None;
                self.pending_completion_request = None;
                crate::completion::request::record_dismiss(self);
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
        crate::keymap_state::view_predicate(self.active_workspace())
    }

    /// The focused terminal or agent pane's [`TermId`], if the focused pane is
    /// one. Unlike [`Self::term_input_target`] this does not gate on the mode,
    /// so [`Self::focused_mode`] can consult it without recursing.
    fn focused_term_id(&self) -> Option<TermId> {
        let ws = self.active_workspace();
        let FocusTarget::SplitPane(_) = ws.focus else {
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
        let FocusTarget::SplitPane(_) = ws.focus else {
            return None;
        };
        match &ws.panes.pane(ws.panes.focus()).view {
            View::Terminal(id) => Some(*id),
            _ => None,
        }
    }

    /// Enter insert on a shell terminal that focus has just arrived on, so
    /// typing reaches the child without a manual `i`.
    ///
    /// `prev` is [`Self::focused_shell_term_id`] captured before the event was
    /// handled. Insert is forced only when a terminal is focused now, it is a
    /// different terminal than `prev` (so focus genuinely arrived), and its mode
    /// is not already insert. Comparing ids means an in-place `Esc` -- the same
    /// terminal focused before and after -- is left in normal for pane
    /// navigation rather than being re-entered.
    fn auto_insert_focused_terminal(&mut self, prev: Option<TermId>) {
        let Some(term_id) = self.focused_shell_term_id() else {
            return;
        };
        if prev == Some(term_id) || self.focused_mode() == "insert" {
            return;
        }
        self.transition_mode("insert".to_string());
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
            self.seal_insert_undo_group();
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

    /// Seal the insert-session group, capturing the post-session selections to
    /// restore on redo. A session that typed nothing discards its group.
    fn seal_insert_undo_group(&mut self) {
        let Some((buffer_id, after)) = self.focused_undo_snapshot() else {
            return;
        };
        if let Some(buffer) = self.active_workspace().buffers.get(buffer_id) {
            buffer.write().expect("poisoned").seal_group(after);
        }
    }

    /// The focused editor's buffer id paired with its current selections, for
    /// opening or sealing an undo group around an insert session.
    fn focused_undo_snapshot(&self) -> Option<(BufferId, Vec<Selection<Anchor>>)> {
        let (editor_id, buffer_id) = self.focused_editor_ids()?;
        let editor = self.active_workspace().editors.get(editor_id)?;
        Some((buffer_id, editor.selections.all_anchors().to_vec()))
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
        editor.selections.transform(buf_snap, |sel| {
            let cursor = stoat_text::cursor_offset(
                rope,
                buf_snap.resolve_anchor(&sel.tail()),
                buf_snap.resolve_anchor(&sel.head()),
            );
            let back = match rope.reversed_chars_at(cursor).next() {
                Some(ch) if ch != '\n' => cursor - ch.len_utf8(),
                _ => cursor,
            };
            action_handlers::movement::land_block_cursor(
                sel.id,
                back,
                stoat_text::SelectionGoal::None,
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
        editor.selections.transform(new_buf, |sel| {
            let cursor = stoat_text::cursor_offset(
                new_buf.rope(),
                new_buf.resolve_anchor(&sel.tail()),
                new_buf.resolve_anchor(&sel.head()),
            );
            action_handlers::movement::land_block_cursor(
                sel.id,
                cursor,
                stoat_text::SelectionGoal::None,
                new_buf.rope(),
                new_buf,
            )
        });
    }

    /// Apply a `SetVar(name, value)` action to [`Self::user_vars`].
    ///
    /// A name colliding with a built-in predicate field, or a value shape no
    /// predicate can compare against, warns and is dropped, so a config typo
    /// cannot shadow a built-in or store an uncomparable value.
    fn set_user_var(&mut self, action: &ResolvedAction) {
        let Some(name) = action
            .args
            .first()
            .and_then(crate::keymap_state::arg_as_str)
        else {
            return;
        };
        if crate::keymap_state::BUILTIN_FIELDS.contains(&name.as_str()) {
            tracing::warn!(
                target: "stoat::keymap",
                "SetVar name `{name}` shadows a built-in field and was ignored"
            );
            return;
        }
        let Some(value) = action
            .args
            .get(1)
            .and_then(crate::keymap_state::arg_to_state_value)
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

        let mut inserts: Vec<(usize, usize)> = editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let tail = buf_snapshot.resolve_anchor(&sel.tail());
                let head = buf_snapshot.resolve_anchor(&sel.head());
                (sel.id, stoat_text::cursor_offset(rope, tail, head))
            })
            .collect();
        inserts.sort_by_key(|(id, offset)| (*offset, *id));

        {
            let mut guard = buffer.write().expect("poisoned");
            for (_, offset) in inserts.iter().rev() {
                guard.edit(*offset..*offset, text);
            }
        }

        // Each cursor lands after its own inserted text. The k-th insertion in
        // offset order is shifted by the k insertions before it plus its own,
        // so its text ends at offset + (k + 1) * text.len().
        let text_len = text.len();
        let new_offsets: std::collections::HashMap<usize, usize> = inserts
            .iter()
            .enumerate()
            .map(|(k, (id, offset))| (*id, offset + (k + 1) * text_len))
            .collect();

        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        editor.selections.transform(new_buf, |s| {
            if let Some(&new_offset) = new_offsets.get(&s.id) {
                action_handlers::movement::forward_block_cursor(
                    s.id,
                    new_offset,
                    stoat_text::SelectionGoal::None,
                    new_buf.rope(),
                    new_buf,
                )
            } else {
                s.clone()
            }
        });
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
            .and_then(|lang| lang.indent_query.is_some().then_some(lang))
            .zip(buffers.syntax(buffer_id))
            .filter(|(_, syntax)| syntax.version == guard.version());

        match fresh_tree {
            Some((lang, syntax)) => language::newline_indent(
                lang.indent_query.as_ref().expect("indent query present"),
                syntax.tree.root_node(),
                &syntax.rope_snapshot,
                cursor_offset,
            ),
            None => language::line_leading_whitespace(rope, row),
        }
    }

    /// The text to insert for a newline at `cursor_offset`, being a line ending
    /// plus the continued indentation.
    ///
    /// On a line whose first non-whitespace run is the language's line-comment
    /// token, the new line carries the token forward (indented to the line's own
    /// leading whitespace) so a comment block continues. Otherwise the indent is
    /// the syntax-derived one from [`Self::newline_indent_string`].
    pub(crate) fn newline_continuation(&self, buffer_id: BufferId, cursor_offset: usize) -> String {
        let continued_comment = {
            let buffers = &self.active_workspace().buffers;
            let token = buffers
                .language_for(buffer_id)
                .and_then(|lang| lang.line_comment);
            match buffers.get(buffer_id) {
                Some(buffer) => {
                    let guard = buffer.read().expect("buffer poisoned");
                    let rope = guard.rope();
                    let row = rope.offset_to_point(cursor_offset).row;
                    let line_start = rope.point_to_offset(stoat_text::Point::new(row, 0));
                    let line_end =
                        rope.point_to_offset(stoat_text::Point::new(row, rope.line_len(row)));
                    token
                        .filter(|&token| {
                            action_handlers::movement::line_comment_continues(
                                rope, line_start, line_end, token,
                            )
                        })
                        .map(|token| {
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
            .and_then(|lang| lang.indent_query.is_some().then_some(lang))
            .zip(buffers.syntax(buffer_id))
            .filter(|(_, syntax)| syntax.version == guard.version());

        match fresh_tree {
            Some((lang, syntax)) => language::suggested_indent(
                lang.indent_query.as_ref().expect("indent query present"),
                syntax.tree.root_node(),
                &syntax.rope_snapshot,
                row,
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
            let next_len = rope
                .chars_at(cursor)
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(0);
            (cursor, cursor + next_len)
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
            let next_len = rope
                .chars_at(cursor)
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(0);
            (cursor, cursor + next_len)
        });
    }

    fn editor_insert_newline(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        let insertion = match self.newest_cursor_offset(editor_id) {
            Some(offset) => self.newline_continuation(buffer_id, offset),
            None => "\n".to_string(),
        };
        self.editor_insert(editor_id, buffer_id, &insertion);
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
        F: Fn(&stoat_text::Rope, usize) -> (usize, usize),
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

        let per_sel: Vec<(usize, usize, usize)> = editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let tail = buf_snapshot.resolve_anchor(&sel.tail());
                let head = buf_snapshot.resolve_anchor(&sel.head());
                let cursor = stoat_text::cursor_offset(rope, tail, head);
                let (start, end) = range_for(rope, cursor);
                (sel.id, start, end)
            })
            .collect();

        let mut ranges: Vec<(usize, usize)> = per_sel
            .iter()
            .filter(|(_, start, end)| start < end)
            .map(|&(_, start, end)| (start, end))
            .collect();
        if ranges.is_empty() {
            return;
        }
        ranges.sort_unstable();

        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            match merged.last_mut() {
                Some(last) if start < last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }

        {
            let mut guard = buffer.write().expect("poisoned");
            for (start, end) in merged.iter().rev() {
                guard.edit(*start..*end, "");
            }
        }

        let new_offsets: std::collections::HashMap<usize, usize> = per_sel
            .iter()
            .map(|&(id, start, _)| (id, Self::offset_after_deletions(start, &merged)))
            .collect();

        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        editor.selections.transform(new_buf, |s| {
            let offset = new_offsets[&s.id];
            action_handlers::movement::forward_block_cursor(
                s.id,
                offset,
                stoat_text::SelectionGoal::None,
                new_buf.rope(),
                new_buf,
            )
        });
    }

    /// New offset of `target` after deleting the ascending, disjoint `ranges`.
    /// A target inside a deleted range collapses to that range's start.
    fn offset_after_deletions(target: usize, ranges: &[(usize, usize)]) -> usize {
        let mut deleted_before = 0;
        for &(start, end) in ranges {
            if end <= target {
                deleted_before += end - start;
            } else if start < target {
                return start - deleted_before;
            } else {
                break;
            }
        }
        target - deleted_before
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
                    ws.focus = FocusTarget::SplitPane(new_pane);
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
                action_handlers::lsp::answer_agent_query(self, uid, request, reply);
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
        let ws = self.active_workspace_mut();
        match notif {
            PtyNotification::Output { run_id, data } => {
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
                UpdateEffect::Redraw
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
                UpdateEffect::Redraw
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
                let exited_held_focus = matches!(ws.focus, FocusTarget::SplitPane(_))
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
        );
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
    /// which snapshots the returned buffer. The event loop paints into one
    /// reused buffer via [`Self::paint_into`] instead, so this is otherwise
    /// unused.
    #[allow(dead_code)]
    pub(crate) fn render(&mut self) -> Buffer {
        let mut buf = Buffer::empty(self.size);
        self.paint_into(&mut buf);
        buf
    }

    /// Paint the current state into `buf`, reusing its allocation.
    ///
    /// Resizes `buf` to the current screen and resets it to blank before
    /// drawing, so a recycled buffer paints byte-identically to a fresh one.
    /// The event loop paints into one long-lived buffer this way to avoid a
    /// per-frame screen allocation.
    fn paint_into(&mut self, buf: &mut Buffer) {
        self.render_tick += 1;
        buf.resize(self.size);
        buf.reset();

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
        let mut undercurls = std::mem::take(&mut self.pending_undercurls);
        undercurls.clear();
        crate::render::frame(self, buf, &mut scene, &mut undercurls);
        self.apc_scene = scene;
        self.pending_undercurls = undercurls;
    }

    /// Flush the frame's APC decoration scene to the channel, when it changed.
    ///
    /// A no-op unless running inside stoatty. [`ApcScene::flush_to`] writes
    /// nothing when the scene matches the previous flush, so steady-state or
    /// widget-free frames push no batch at all. Runs at the frame seam after the
    /// live frame is published, beside [`Self::emit_smooth_scroll`].
    fn emit_apc_scene(&mut self) {
        if !self.stoatty {
            return;
        }
        let Some(apc_tx) = self.apc_tx.clone() else {
            return;
        };

        let mut batch = Vec::new();
        let _ = self.apc_scene.flush_to(&mut batch);
        if !batch.is_empty() {
            let _ = apc_tx.send(batch);
        }
    }

    /// Assign a `content_id` to each visible split-editor buffer a minimap strip
    /// may render, so the declare widget can read the id while the frame paints.
    ///
    /// The summaries themselves sync afterward at the frame seam in
    /// [`Self::emit_minimap`]. A no-op outside stoatty or with the minimap off.
    pub(crate) fn ensure_minimap_content_ids(&mut self) {
        if !self.stoatty || !self.minimap_enabled() {
            return;
        }
        let ws_id = self.active_workspace;
        let ws = &self.workspaces[ws_id];
        let buffer_ids: Vec<BufferId> = ws
            .panes
            .split_panes()
            .filter_map(|(_, pane)| {
                let View::Editor(editor_id) = pane.view else {
                    return None;
                };
                Some(ws.editors.get(editor_id)?.buffer_id)
            })
            .collect();

        for buffer_id in buffer_ids {
            if let std::collections::hash_map::Entry::Vacant(slot) =
                self.minimap_content.entry((ws_id, buffer_id))
            {
                slot.insert(crate::minimap::MinimapContent::new(
                    self.minimap_next_content_id,
                ));
                self.minimap_next_content_id += 1;
            }
        }
    }

    /// Sync each visible minimap strip's buffer and drain its summary changes to
    /// the terminal as `minimap_lines`, retiring content for buffers that closed.
    ///
    /// Runs at the frame seam after [`Self::emit_smooth_scroll`], so each editor's
    /// reserved strip rect from the paint is current. The strip declaration rides
    /// the diffed scene from the paint. This sends only the persistent content
    /// stores. A no-op outside stoatty.
    fn emit_minimap(&mut self) {
        if !self.stoatty {
            return;
        }
        let Some(apc_tx) = self.apc_tx.clone() else {
            return;
        };
        let ws_id = self.active_workspace;

        let strips: Vec<(BufferId, EditorId)> = {
            let ws = &self.workspaces[ws_id];
            ws.panes
                .split_panes()
                .filter_map(|(_, pane)| {
                    let View::Editor(editor_id) = pane.view else {
                        return None;
                    };
                    let editor = ws.editors.get(editor_id)?;
                    editor.minimap_rect?;
                    Some((editor.buffer_id, editor_id))
                })
                .collect()
        };

        let mut out = Vec::new();
        for (buffer_id, editor_id) in strips {
            self.sync_minimap_strip(ws_id, buffer_id, editor_id, &mut out);
        }

        let dropped: Vec<(WorkspaceId, BufferId)> = self
            .minimap_content
            .keys()
            .filter(|(ws, buffer_id)| {
                *ws == ws_id && self.workspaces[ws_id].buffers.get(*buffer_id).is_none()
            })
            .copied()
            .collect();
        for key in dropped {
            if let Some(content) = self.minimap_content.remove(&key) {
                stoatty_protocol::command::encode_minimap_drop_into(
                    &mut out,
                    &stoatty_protocol::command::MinimapDropCommand {
                        content_id: content.content_id(),
                    },
                );
            }
        }

        if !out.is_empty() {
            let _ = apc_tx.send(out);
        }
    }

    /// Sync one strip's [`crate::minimap::MinimapContent`] to its buffer and
    /// append the drained splices to `out` as a `minimap_lines` frame.
    fn sync_minimap_strip(
        &mut self,
        ws_id: WorkspaceId,
        buffer_id: BufferId,
        editor_id: EditorId,
        out: &mut Vec<u8>,
    ) {
        let (content_id, synced_version) = match self.minimap_content.get(&(ws_id, buffer_id)) {
            Some(content) => (content.content_id(), content.synced_version()),
            None => return,
        };

        let buffer_syntax_version = self.workspaces[ws_id]
            .buffers
            .syntax_version(buffer_id)
            .unwrap_or(0);
        let (snapshot, diff_version, diag_version, severity_map) =
            match self.workspaces[ws_id].editors.get_mut(editor_id) {
                Some(editor) => {
                    let snapshot = editor.display_map.snapshot();
                    let diff_version = editor.display_map.diff_version();
                    let (diag_version, severity_map) = editor
                        .gutter_severity_cache
                        .as_ref()
                        .map(|cache| (cache.version, cache.map.clone()))
                        .unwrap_or_default();
                    (snapshot, diff_version, diag_version, severity_map)
                },
                None => return,
            };
        let (rope, version, edits) = {
            let buf_snap = snapshot.buffer_snapshot();
            (
                buf_snap.rope().clone(),
                buf_snap.version(),
                buf_snap.edits_since(synced_version),
            )
        };

        let decoration_version = {
            let mut hasher = DefaultHasher::new();
            diff_version.hash(&mut hasher);
            diag_version.hash(&mut hasher);
            hasher.finish()
        };
        let syntax_version = {
            let mut hasher = DefaultHasher::new();
            self.syntax_highlight.hash(&mut hasher);
            buffer_syntax_version.hash(&mut hasher);
            hasher.finish()
        };

        let syntax_on = self.syntax_highlight;
        let class_table = &self.minimap_class_table;

        // Resolve the whole buffer's tokens once, on the first summarized row, so
        // a steady frame that summarizes nothing pays nothing.
        let line_bucket = std::cell::OnceCell::new();
        let line_tokens = |row: u32, _text: &str| {
            line_bucket
                .get_or_init(|| minimap_line_tokens(&snapshot, buffer_id, syntax_on, class_table))
                .get(&row)
                .cloned()
                .unwrap_or_default()
        };
        let edge_of = |row: u32| minimap_edge_class(&snapshot, &severity_map, class_table, row);

        let content = self
            .minimap_content
            .get_mut(&(ws_id, buffer_id))
            .expect("checked above");
        content.sync(
            &rope,
            version,
            &edits,
            crate::minimap::SyncVersions {
                decoration: decoration_version,
                syntax: syntax_version,
            },
            line_tokens,
            edge_of,
        );

        for splice in content.take_queued() {
            stoatty_protocol::command::encode_minimap_lines_into(
                out,
                &stoatty_protocol::command::MinimapLinesCommand {
                    content_id,
                    start: splice.start,
                    removed: splice.removed,
                    lines: splice.lines.into_iter().map(convert_minimap_runs).collect(),
                },
            );
        }
    }

    /// Emit the stoatty smooth-scroll APC for every visible editor pane's current
    /// scroll position, pushing one byte batch onto the APC channel.
    ///
    /// A no-op unless running inside stoatty. Each plain-editor split pane (a
    /// [`View::Editor`] that is not a review view) gets its own pool, keyed by the
    /// pane's stable index, so split panes glide independently and at once. A pane
    /// that is no longer pooled -- closed, switched to another view, turned into a
    /// review, or hidden behind a full-screen overlay (commits, rebase, reword,
    /// conflict) -- is retired with `pool_drop`, so returning to it re-declares the
    /// region and refills the page window.
    ///
    /// Runs at the frame seam after the live frame is published, so the pane
    /// layout (and thus each editor rectangle) reflects the frame just drawn and
    /// the APC bytes are written to stdout right after the grid frame.
    fn emit_smooth_scroll(&mut self) {
        if !self.stoatty {
            return;
        }
        let Some(apc_tx) = self.apc_tx.clone() else {
            return;
        };

        // A full-screen overlay screen hides every editor, so nothing is pooled
        // this frame and any live pools are retired. The diff screen renders in
        // the real editor pool, so it is not an overlay.
        let overlay = matches!(
            crate::keymap_state::view_predicate(self.active_workspace()),
            Some("commits" | "rebase" | "reword" | "conflict")
        );
        let panes = if overlay {
            Vec::new()
        } else {
            self.editor_pool_panes()
        };

        // The file finder is a modal over normal mode (not a full-screen overlay
        // mode); its result list pools as a non-pane surface above the panes.
        let finder_list = (!overlay && self.file_finder.is_some())
            .then(|| crate::render::file_finder::file_finder_layout(self.size()))
            .flatten()
            .map(|layout| layout.list);

        // The command palette is a modal over normal mode like the finder. Its
        // fixed list region pools as a non-pane surface. Command-filter mode
        // pools the command list.
        let palette_list = (!overlay
            && self
                .command_palette
                .as_ref()
                .is_some_and(|p| p.command.is_none()))
        .then(|| crate::render::command_palette::palette_filter_layout(self.size()))
        .flatten()
        .map(|layout| layout.list);

        // Argument mode (`:o `/`:cd `/`:b `) shows the inline picker in place of
        // the command list, and its result list pools into the same PALETTE id.
        // Filter and arg modes are mutually exclusive -- arg mode needs a parsed
        // command, filter mode needs none -- so one pool id serves both.
        let palette_arg_list = (!overlay
            && self
                .command_palette
                .as_ref()
                .is_some_and(|p| p.arg_picker.is_some() && p.arg_source().is_some()))
        .then(|| crate::render::command_palette::palette_arg_list_rect(self.size()))
        .flatten();

        // The commits overlay renders into the focused pane; its left list pools
        // as a non-pane surface while editor panes stay suppressed in this mode.
        let commits_region = (self.focused_mode() == "commits")
            .then(|| {
                let ws = self.active_workspace();
                ws.commits.as_ref()?;
                let pane = ws.panes.pane(ws.panes.focus());
                crate::render::commits::commits_list_rect(pane.area)
            })
            .flatten();

        // The completion popup is cursor-anchored: its inner list region pools
        // and moves with the cursor each frame (emit_into re-emits the region on
        // a move). The layout reads the focused editor, so it borrows self.
        let completion = (!overlay)
            .then(|| crate::render::completion::completion_popup_layout(self))
            .flatten();

        // The help view is a fixed centered modal over the editor like the
        // finder; its list and detail panes pool as two non-pane surfaces.
        let help_layout = (!overlay && self.help.is_some())
            .then(|| crate::render::help::help_layout(self.size()))
            .flatten();

        // The hover popup is cursor-anchored like the completion popup. Its
        // interior body region pools so Ctrl-u/Ctrl-d and the wheel ease. The
        // layout reads the focused editor, so it borrows self.
        // A live hover selection is painted by the live frame's highlight, so
        // the pooled body defers, since its glide path carries no selection.
        // Skipping the layout retires the HOVER pool via drop_absent below.
        let hover_selected = self
            .pending_hover
            .as_ref()
            .and_then(|p| p.selection.as_ref())
            .is_some();
        let hover_layout = (!overlay && !hover_selected)
            .then(|| crate::render::hover::hover_popup_layout(self))
            .flatten();

        let mut out = Vec::new();
        let mut active: Vec<u32> = panes.iter().map(|(pool, _, _)| *pool).collect();
        if finder_list.is_some() {
            active.push(crate::smooth_scroll::non_pane_pool::FINDER);
        }
        if palette_list.is_some() || palette_arg_list.is_some() {
            active.push(crate::smooth_scroll::non_pane_pool::PALETTE);
        }
        if commits_region.is_some() {
            active.push(crate::smooth_scroll::non_pane_pool::COMMITS);
        }
        if completion.is_some() {
            active.push(crate::smooth_scroll::non_pane_pool::COMPLETION);
        }
        if help_layout.is_some() {
            active.push(crate::smooth_scroll::non_pane_pool::HELP_LIST);
            active.push(crate::smooth_scroll::non_pane_pool::HELP_DETAIL);
        }
        if hover_layout.is_some() {
            active.push(crate::smooth_scroll::non_pane_pool::HOVER);
        }
        self.smooth_scroll.drop_absent(&mut out, &active);

        // Editor and review pages render off the run loop. The loop collects a
        // snapshot -- plus the cloned view state and theme for review -- and the
        // newly-entered page indices per pane, then spawns the renders after the
        // APC batch ships, so region and scroll always reach the terminal before
        // any fill.
        // The review view and theme a pooled review page needs, boxed so the
        // Review variant does not dwarf Editor.
        struct ReviewFillParts {
            view: crate::review_session::ReviewViewState,
            theme: crate::theme::Theme,
        }
        enum PoolFill {
            Editor {
                snapshot: crate::display_map::DisplaySnapshot,
                pages: Vec<u64>,
                pool: u32,
                width: u16,
                height: u16,
                gutter: crate::smooth_scroll::PageGutter,
                diff_view: bool,
            },
            Review {
                snapshot: crate::display_map::DisplaySnapshot,
                parts: Box<ReviewFillParts>,
                pages: Vec<u64>,
                pool: u32,
                width: u16,
                height: u16,
            },
        }
        let mut async_jobs: Vec<PoolFill> = Vec::new();
        let syntax_highlight = self.syntax_highlight;
        let line_numbers = self
            .settings
            .editor_line_numbers
            .unwrap_or(LineNumbers::Relative);
        let stoatty = self.stoatty;
        // Relative numbering follows the same pane the live render calls focused:
        // the focused split editor outside insert mode. Resolved before the ws
        // borrow so the per-pane loop can gate on it.
        let focused_editor = self.focused_editor_ids().map(|(id, _)| id);
        let focused_insert = self.focused_mode() == "insert";
        let ws = &mut self.workspaces[self.active_workspace];
        let theme = &self.theme;
        let fallback_style = theme.get(crate::theme::scope::UI_TEXT);
        for (_, editor_id, region) in &panes {
            let region = *region;
            let Some(editor) = ws.editors.get_mut(*editor_id) else {
                continue;
            };
            // scroll_row is the source of truth for the pool page. The wheel
            // glide refines it sub-row through scroll_offset, but cursor-follow
            // and jumps move scroll_row without the fraction, so trust the offset
            // only while it still floors to scroll_row. A page glide is the
            // exception. scroll_row jumped to the target and the offset lags
            // behind easing up to it, so trust the fraction throughout the glide
            // and let the pool ease from the lagging offset to the target.
            let scroll_offset = if editor.scroll_glide
                || editor.scroll_offset.floor() as u32 == editor.scroll_row
            {
                editor.scroll_offset
            } else {
                editor.scroll_row as f32
            };
            // Review rows regenerate on accept/reject and their gutter glyphs
            // change on stage/unstage, so the session version is the pool's
            // content version. Plain editors stay stable while scrolling, save
            // for the syntax-highlight toggle (recolors every row), a diagnostics
            // change (restyles the gutter), and a gutter-width change (reflows the
            // inset), so a buffered page must refill when any of those move.
            // Relative numbers reference the cursor's buffer line, so a page
            // stays valid across a wheel glide (cursor fixed) and must refill
            // when the cursor line moves. Folding current_line into the content
            // version below does exactly that.
            let current_line = (line_numbers == LineNumbers::Relative
                && !focused_insert
                && focused_editor == Some(*editor_id))
            .then(|| crate::render::editor::editor_cursor_position(editor).map(|(line, _)| line))
            .flatten();

            let content_version = match editor.review_view.as_ref() {
                Some(view) => view.session_version,
                None => editor_page_content_version(
                    syntax_highlight,
                    editor.gutter_width,
                    current_line,
                    editor
                        .gutter_severity_cache
                        .as_ref()
                        .map_or(0, |cache| cache.version),
                    editor.diff_view,
                    editor.display_map.diff_version(),
                ),
            };
            let entered = crate::smooth_scroll::emit_into(
                &mut out,
                &mut self.smooth_scroll,
                region,
                scroll_offset,
                content_version,
                // Editor and review pages both fill asynchronously below, so the
                // synchronous render emits nothing here.
                |_| Vec::new(),
            );

            if editor.minimap_rect.is_some() {
                self.smooth_scroll.emit_minimap_view(
                    &mut out,
                    region.pool,
                    (scroll_offset * 256.0) as u32,
                    region.height,
                );
            }

            if !entered.is_empty() {
                let snapshot = editor.display_map.snapshot();
                if let Some(view) = editor.review_view.as_ref() {
                    async_jobs.push(PoolFill::Review {
                        snapshot,
                        parts: Box::new(ReviewFillParts {
                            view: view.clone(),
                            theme: theme.clone(),
                        }),
                        pages: entered,
                        pool: region.pool,
                        width: region.width,
                        height: region.height,
                    });
                } else {
                    let severity = editor
                        .gutter_severity_cache
                        .as_ref()
                        .map(|cache| cache.map.clone())
                        .unwrap_or_default();
                    let rich =
                        crate::render::editor::resolve_rich_gutter(theme, fallback_style, stoatty);
                    async_jobs.push(PoolFill::Editor {
                        snapshot,
                        pages: entered,
                        pool: region.pool,
                        width: region.width,
                        height: region.height,
                        gutter: crate::smooth_scroll::PageGutter::new(
                            line_numbers != LineNumbers::Off,
                            severity,
                            theme.clone(),
                            rich,
                            current_line,
                        ),
                        diff_view: editor.diff_view,
                    });
                }
            }
        }

        if let (Some(list), Some(finder)) = (finder_list, self.file_finder.as_ref()) {
            let region = stoatty_protocol::command::PoolRegionCommand {
                pool: crate::smooth_scroll::non_pane_pool::FINDER,
                top: list.y,
                left: list.x,
                width: list.width,
                height: list.height,
            };
            let core = finder.active_core_ref();
            let scroll_row =
                core.picklist
                    .selected
                    .saturating_sub(list.height.saturating_sub(1) as usize) as u32;
            // The visible rows are the active picker's filtered indices, and in
            // browse mode the typed directory re-roots them, so both feed the
            // pool's content version: a re-filter or a re-root refills it.
            let content_version = {
                let mut hasher = DefaultHasher::new();
                finder
                    .browse
                    .as_ref()
                    .map(|browse| browse.typed_dir.as_str())
                    .unwrap_or_default()
                    .hash(&mut hasher);
                core.picklist.filtered.hash(&mut hasher);
                hasher.finish()
            };
            crate::smooth_scroll::emit_into(
                &mut out,
                &mut self.smooth_scroll,
                region,
                scroll_row as f32,
                content_version,
                |page| {
                    crate::smooth_scroll::render_finder_page(
                        finder,
                        page,
                        theme,
                        region.width,
                        region.height,
                    )
                },
            );
        }

        if let (Some(list), Some(palette)) = (palette_list, self.command_palette.as_ref()) {
            let filtered = &palette.filtered;
            let match_indices = &palette.match_indices;
            let selected = &palette.selected;
            let region = stoatty_protocol::command::PoolRegionCommand {
                pool: crate::smooth_scroll::non_pane_pool::PALETTE,
                top: list.y,
                left: list.x,
                width: list.width,
                height: list.height,
            };
            let scroll_row = selected.saturating_sub(list.height.saturating_sub(1) as usize) as u32;
            // The visible row set is the filtered entries, so a hash of their
            // names is the pool's content version and a re-filter refills it. The
            // leading discriminant keeps a filter-mode list from aliasing an
            // arg-mode list that shares this pool id and matches region and scroll.
            let content_version = {
                let mut hasher = DefaultHasher::new();
                0u8.hash(&mut hasher);
                for entry in filtered {
                    entry.def.name().hash(&mut hasher);
                }
                hasher.finish()
            };
            crate::smooth_scroll::emit_into(
                &mut out,
                &mut self.smooth_scroll,
                region,
                scroll_row as f32,
                content_version,
                |page| {
                    crate::smooth_scroll::render_palette_page(
                        filtered,
                        match_indices,
                        *selected,
                        page,
                        theme,
                        region.width,
                        region.height,
                    )
                },
            );
        }

        if let (Some(list), Some(picker)) = (
            palette_arg_list,
            self.command_palette
                .as_ref()
                .and_then(|palette| palette.arg_picker.as_ref()),
        ) {
            let region = stoatty_protocol::command::PoolRegionCommand {
                pool: crate::smooth_scroll::non_pane_pool::PALETTE,
                top: list.y,
                left: list.x,
                width: list.width,
                height: list.height,
            };
            let core = picker.active_core_ref();
            let scroll_row =
                core.picklist
                    .selected
                    .saturating_sub(list.height.saturating_sub(1) as usize) as u32;
            // The visible rows are the active picker's filtered paths, so their
            // hash is the pool's content version and a re-filter refills it. The
            // leading discriminant keeps this arg-mode list from aliasing a
            // filter-mode list that shares this pool id, and the browse typed
            // directory folds re-roots in so two same-shaped filtered sets from
            // different roots cannot alias.
            let content_version = {
                let mut hasher = DefaultHasher::new();
                1u8.hash(&mut hasher);
                picker
                    .browse
                    .as_ref()
                    .map(|browse| browse.typed_dir.as_str())
                    .unwrap_or_default()
                    .hash(&mut hasher);
                core.picklist.filtered.hash(&mut hasher);
                hasher.finish()
            };
            crate::smooth_scroll::emit_into(
                &mut out,
                &mut self.smooth_scroll,
                region,
                scroll_row as f32,
                content_version,
                |page| {
                    crate::smooth_scroll::render_arg_page(
                        picker,
                        page,
                        theme,
                        region.width,
                        region.height,
                    )
                },
            );
        }

        if let (Some(list), Some(state)) = (
            commits_region,
            self.workspaces[self.active_workspace].commits.as_ref(),
        ) {
            let region = stoatty_protocol::command::PoolRegionCommand {
                pool: crate::smooth_scroll::non_pane_pool::COMMITS,
                top: list.y,
                left: list.x,
                width: list.width,
                height: list.height,
            };
            let scroll_row = state.scroll_top as u32;
            // Commits stream in lazily, so the length plus the load/end flags
            // form the content version; new commits refill the pages.
            let content_version = (state.commits.len() as u64) << 2
                | ((state.pending_load.is_some() as u64) << 1)
                | (state.reached_end as u64);
            crate::smooth_scroll::emit_into(
                &mut out,
                &mut self.smooth_scroll,
                region,
                scroll_row as f32,
                content_version,
                |page| {
                    crate::smooth_scroll::render_commits_page(
                        state,
                        page,
                        theme,
                        region.width,
                        region.height,
                    )
                },
            );
        }

        if let Some((popup, prefix, layout)) = completion {
            let region = stoatty_protocol::command::PoolRegionCommand {
                pool: crate::smooth_scroll::non_pane_pool::COMPLETION,
                top: layout.inner.y,
                left: layout.inner.x,
                width: layout.inner.width,
                height: layout.inner.height,
            };
            let scroll_row = layout.viewport_top as u32;
            // Items are replaced wholesale when the prefix re-queries, so a hash
            // of their labels is the pool's content version: a re-query refills.
            let content_version = {
                let mut hasher = DefaultHasher::new();
                for item in &popup.items {
                    item.label.hash(&mut hasher);
                }
                hasher.finish()
            };
            crate::smooth_scroll::emit_into(
                &mut out,
                &mut self.smooth_scroll,
                region,
                scroll_row as f32,
                content_version,
                |page| {
                    crate::smooth_scroll::render_completion_page(
                        &popup.items,
                        popup.selected_idx,
                        &prefix,
                        page,
                        theme,
                        region.width,
                        region.height,
                    )
                },
            );
        }

        if let (Some(layout), Some(help)) = (help_layout, self.help.as_ref()) {
            let list = layout.list;
            let list_region = stoatty_protocol::command::PoolRegionCommand {
                pool: crate::smooth_scroll::non_pane_pool::HELP_LIST,
                top: list.y,
                left: list.x,
                width: list.width,
                height: list.height,
            };
            let list_scroll =
                help.selected()
                    .saturating_sub(list.height.saturating_sub(1) as usize) as u32;
            // The filtered entry set changes on every search refilter, so its
            // hash is the list pool's content version.
            let list_version = {
                let mut hasher = DefaultHasher::new();
                help.filtered().hash(&mut hasher);
                hasher.finish()
            };
            crate::smooth_scroll::emit_into(
                &mut out,
                &mut self.smooth_scroll,
                list_region,
                list_scroll as f32,
                list_version,
                |page| {
                    crate::smooth_scroll::render_help_list_page(
                        help,
                        page,
                        theme,
                        list.width,
                        list.height,
                    )
                },
            );

            let detail = layout.detail;
            let detail_region = stoatty_protocol::command::PoolRegionCommand {
                pool: crate::smooth_scroll::non_pane_pool::HELP_DETAIL,
                top: detail.y,
                left: detail.x,
                width: detail.width,
                height: detail.height,
            };
            let detail_scroll = help.detail_scroll() as u32;
            // The detail body is the selected entry's, so a hash of its name is
            // the content version: it bumps on a selection move and on a filter
            // change that lands a different entry at the same index.
            let detail_version = {
                let mut hasher = DefaultHasher::new();
                help.selected_entry()
                    .map(|entry| entry.def.name())
                    .hash(&mut hasher);
                hasher.finish()
            };
            crate::smooth_scroll::emit_into(
                &mut out,
                &mut self.smooth_scroll,
                detail_region,
                detail_scroll as f32,
                detail_version,
                |page| {
                    crate::smooth_scroll::render_help_detail_page(
                        help,
                        page,
                        theme,
                        detail.width,
                        detail.height,
                    )
                },
            );
        }

        if let (Some((_, inner)), Some(popup)) = (hover_layout, self.pending_hover.as_ref()) {
            let region = stoatty_protocol::command::PoolRegionCommand {
                pool: crate::smooth_scroll::non_pane_pool::HOVER,
                top: inner.y,
                left: inner.x,
                width: inner.width,
                height: inner.height,
            };
            let interior = inner.height.max(1) as usize;
            let half_page = (interior / 2).max(1);
            let scroll = popup
                .lines
                .len()
                .saturating_sub(interior)
                .min(popup.scroll_half_pages * half_page);
            // The body changes only when a new hover replaces this one, so a hash
            // of the line texts is the pool's content version.
            let content_version = {
                let mut hasher = DefaultHasher::new();
                for line in &popup.lines {
                    for (text, _) in line {
                        text.hash(&mut hasher);
                    }
                }
                hasher.finish()
            };
            crate::smooth_scroll::emit_into(
                &mut out,
                &mut self.smooth_scroll,
                region,
                scroll as f32,
                content_version,
                |page| {
                    crate::render::hover::render_hover_page(
                        popup,
                        page,
                        theme,
                        inner.width,
                        inner.height,
                    )
                },
            );
        }

        if !out.is_empty() {
            let _ = apc_tx.send(out);
        }

        // The batch (regions, scrolls) has shipped, so the terminal has every pool's
        // geometry before a fill lands. Render each newly-entered plain editor page on
        // a blocking worker and deliver its fill frame through the same channel, off
        // the run loop.
        for job in async_jobs {
            match job {
                PoolFill::Editor {
                    snapshot,
                    pages,
                    pool,
                    width,
                    height,
                    gutter,
                    diff_view,
                } => {
                    for index in pages {
                        let snapshot = snapshot.clone();
                        let gutter = gutter.clone();
                        let apc_tx = apc_tx.clone();
                        self.executor
                            .spawn_blocking(move || {
                                let fill = crate::smooth_scroll::render_page_fill(
                                    &snapshot,
                                    pool,
                                    index,
                                    fallback_style,
                                    width,
                                    height,
                                    &gutter,
                                    diff_view,
                                );
                                let _ = apc_tx.send(fill);
                            })
                            .detach();
                    }
                },
                PoolFill::Review {
                    snapshot,
                    parts,
                    pages,
                    pool,
                    width,
                    height,
                } => {
                    let ReviewFillParts { view, theme } = *parts;
                    for index in pages {
                        let snapshot = snapshot.clone();
                        let view = view.clone();
                        let theme = theme.clone();
                        let apc_tx = apc_tx.clone();
                        self.executor
                            .spawn_blocking(move || {
                                let fill = crate::smooth_scroll::render_review_page_from_parts(
                                    &snapshot,
                                    &view,
                                    &theme,
                                    pool,
                                    index,
                                    fallback_style,
                                    width,
                                    height,
                                );
                                let _ = apc_tx.send(fill);
                            })
                            .detach();
                    }
                },
            }
        }
    }

    /// Every visible split pane showing an editor, as `(pool id, editor id,
    /// pool region)`.
    ///
    /// One entry per [`Placement::Split`] pane whose [`View::Editor`] has a
    /// non-empty content area, plain or review alike. The pool id is the pane's
    /// stable [`crate::pane::Pane::index`], so a pane keeps its pool across
    /// frames; the region is the pane area minus its bottom status row, the same
    /// content area the editor is painted into. The caller pools nothing while a
    /// full-screen overlay mode is active.
    fn editor_pool_panes(
        &self,
    ) -> Vec<(u32, EditorId, stoatty_protocol::command::PoolRegionCommand)> {
        let ws = self.active_workspace();
        ws.panes
            .split_panes()
            .filter_map(|(_, pane)| {
                if pane.placement != Placement::Split {
                    return None;
                }
                let View::Editor(editor_id) = pane.view else {
                    return None;
                };
                ws.editors.get(editor_id)?;

                let (content, _) = crate::render::layout::split_pane_status(pane.area);
                if content.width == 0 || content.height == 0 {
                    return None;
                }

                Some((
                    pane.index,
                    editor_id,
                    stoatty_protocol::command::PoolRegionCommand {
                        pool: pane.index,
                        top: content.y,
                        left: content.x,
                        width: content.width,
                        height: content.height,
                    },
                ))
            })
            .collect()
    }

    /// Drive the background work whose results feed the next paint: parse-job
    /// scheduling and the commit, review, LSP, and completion result pumps.
    ///
    /// Run from the event loop after input is handled and before the redraw,
    /// keeping [`Self::render`] a pure paint. Tests that previously relied on
    /// `render` to drive this call it directly.
    pub(crate) fn drive_background(&mut self) {
        self.drain_lsp_notifications();
        self.drain_lsp_incoming_requests();
        self.install_pending_lsp_host();
        crate::project_env::ensure_loaded(self);
        crate::project_env::install_pending(self);
        crate::diff_warm::ensure_diff_warm(self);
        crate::diff_warm::install_finished(self);
        action_handlers::sync_palette_picker(self);
        action_handlers::sync_file_finder_preview(self);
        action_handlers::file::pump_auto_reload(self);
        self.drive_parse_jobs();
        self.drive_diff_jobs();
        action_handlers::pump_commits(self);
        action_handlers::pump_review_scan(self);
        action_handlers::pump_lsp_jumps(self);
        action_handlers::lsp::pump_lsp_hover(self);
        action_handlers::lsp::pump_lsp_signature_help(self);
        action_handlers::lsp::pump_lsp_inlay_hints(self);
        action_handlers::lsp::pump_lsp_document_highlight(self);
        action_handlers::lsp::pump_lsp_pull_diagnostics(self);
        action_handlers::lsp::pump_lsp_semantic_tokens(self);
        action_handlers::lsp::pump_lsp_folding_ranges(self);
        action_handlers::lsp::pump_lsp_code_actions(self);
        action_handlers::lsp::pump_lsp_code_action_resolve(self);
        action_handlers::lsp::pump_lsp_prepare_rename(self);
        action_handlers::lsp::pump_lsp_rename(self);
        action_handlers::lsp::pump_lsp_symbol_picker(self);
        action_handlers::lsp::pump_lsp_workspace_symbol(self);
        action_handlers::lsp::pump_lsp_format(self);
        action_handlers::file::pump_format_on_save(self);
        crate::completion::request::pump(self);
        action_handlers::completion::pump_completion_resolve(self);
        crate::completion::accept::pump_completion_accept(self);
    }

    /// Resolve a `(line, column)` 0-based point to a byte
    /// offset in the focused editor's rope. Returns `None`
    /// when the focused pane is not an editor.
    pub(crate) fn offset_for_focused_point(&mut self, line: u32, column: u32) -> Option<usize> {
        let ws = self.active_workspace_mut();
        let editor_id = match ws.focus {
            FocusTarget::SplitPane(pane_id) => match ws.panes.pane(pane_id).view {
                View::Editor(id) => id,
                _ => return None,
            },
            FocusTarget::Dock(_) => return None,
        };
        let editor = ws.editors.get_mut(editor_id)?;
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let rope = buf_snap.rope();
        let point = stoat_text::Point::new(line, column);
        Some(rope.point_to_offset(point).min(rope.len()))
    }

    /// Collapse the focused editor's primary selection at
    /// `offset`. Used by non-jumplist navigation flows (e.g. the
    /// diagnostics picker) that need to move the cursor without
    /// touching jumplist state.
    pub(crate) fn collapse_focused_cursor_to(&mut self, offset: usize) {
        let ws = self.active_workspace_mut();
        let editor_id = match ws.focus {
            FocusTarget::SplitPane(pane_id) => match ws.panes.pane(pane_id).view {
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
                stoat_text::SelectionGoal::None,
                buf_snap.rope(),
                buf_snap,
            )
        });
    }

    pub(crate) fn jump_focused_to_match_offset(&mut self, offset: usize) {
        let ws = self.active_workspace_mut();
        let editor_id = match ws.focus {
            FocusTarget::SplitPane(pane_id) => match ws.panes.pane(pane_id).view {
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
                stoat_text::SelectionGoal::None,
                buf_snap.rope(),
                buf_snap,
            )
        });
    }
}

/// Content version of a pooled editor page, hashing the inputs whose change
/// forces a buffered page to refill.
///
/// A page stays cached while the surface scrolls, but must repaint when the
/// syntax-highlight toggle recolors every row, a diagnostics change restyles
/// the gutter, a gutter-width change reflows the inset, or the cursor's buffer
/// line moves under relative numbering.
fn editor_page_content_version(
    syntax_highlight: bool,
    gutter_width: u16,
    current_line: Option<u32>,
    severity_version: u64,
    diff_view: bool,
    diff_version: usize,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    (!syntax_highlight).hash(&mut hasher);
    gutter_width.hash(&mut hasher);
    current_line.hash(&mut hasher);
    severity_version.hash(&mut hasher);
    diff_view.hash(&mut hasher);
    diff_version.hash(&mut hasher);
    hasher.finish()
}

/// Resolve a buffer's syntax highlights into minimap line tokens bucketed by row.
///
/// Reads the tree-sitter and LSP semantic tokens, which are buffer-anchored, so
/// the byte ranges are exact regardless of tab expansion, soft-wrap, or inlays --
/// unlike display chunks. Each token splits across the buffer lines it spans, and
/// the pieces bucket per row as line-relative [`crate::minimap::LineToken`]s
/// carrying their foreground's palette class. Tokens resolving to class 0 drop,
/// and `syntax_on` off yields an empty map.
fn minimap_line_tokens(
    snapshot: &crate::display_map::DisplaySnapshot,
    buffer_id: BufferId,
    syntax_on: bool,
    class_table: &crate::minimap::ClassTable,
) -> std::collections::HashMap<u32, Vec<crate::minimap::LineToken>> {
    let mut by_row: std::collections::HashMap<u32, Vec<crate::minimap::LineToken>> =
        std::collections::HashMap::new();
    if !syntax_on {
        return by_row;
    }

    let buffer_snap = snapshot.buffer_snapshot();
    let rope = buffer_snap.rope();

    for highlights in [
        snapshot.semantic_token_highlights(),
        snapshot.lsp_token_highlights(),
    ] {
        let Some((spans, interner)) = highlights.get(&buffer_id) else {
            continue;
        };
        for span in spans.iter() {
            let class = interner[span.style]
                .foreground
                .map_or(0, |fg| class_table.class_of_color(fg));
            if class == 0 {
                continue;
            }
            let start = buffer_snap.resolve_anchor(&span.range.start);
            let end = buffer_snap.resolve_anchor(&span.range.end);
            if start >= end {
                continue;
            }

            let start_row = rope.offset_to_point(start).row;
            let end_row = rope.offset_to_point(end).row;
            for row in start_row..=end_row {
                let line_start = rope.point_to_offset(stoat_text::Point::new(row, 0));
                let line_end =
                    rope.point_to_offset(stoat_text::Point::new(row, rope.line_len(row)));
                let s = start.max(line_start);
                let e = end.min(line_end);
                if s < e {
                    by_row
                        .entry(row)
                        .or_default()
                        .push(crate::minimap::LineToken {
                            range: (s - line_start)..(e - line_start),
                            class,
                        });
                }
            }
        }
    }

    for tokens in by_row.values_mut() {
        tokens.sort_by_key(|token| token.range.start);
    }
    by_row
}

/// The minimap edge-lane class for buffer `row`, or `None` when the line carries
/// no diff or diagnostic mark.
///
/// A diagnostic on the row wins over its diff status, mirroring the gutter. The
/// severity or diff status resolves to a class against `class_table`.
fn minimap_edge_class(
    snapshot: &crate::display_map::DisplaySnapshot,
    severity_map: &std::collections::BTreeMap<u32, lsp_types::DiagnosticSeverity>,
    class_table: &crate::minimap::ClassTable,
    row: u32,
) -> Option<u8> {
    use crate::{host::DiffStatus, minimap::EdgeClass};
    use lsp_types::DiagnosticSeverity;

    if let Some(severity) = severity_map.get(&row) {
        let kind = match *severity {
            DiagnosticSeverity::ERROR => EdgeClass::Error,
            DiagnosticSeverity::WARNING => EdgeClass::Warning,
            _ => EdgeClass::Info,
        };
        return Some(class_table.edge_class(kind));
    }

    match snapshot.line_diff_status(row) {
        DiffStatus::Added => Some(class_table.edge_class(EdgeClass::Added)),
        DiffStatus::Modified | DiffStatus::Moved => {
            Some(class_table.edge_class(EdgeClass::Modified))
        },
        DiffStatus::Unchanged => None,
    }
}

/// Convert the engine's [`crate::minimap::Run`]s to their `minimap_lines` wire form.
fn convert_minimap_runs(
    runs: Vec<crate::minimap::Run>,
) -> Vec<stoatty_protocol::command::MinimapRun> {
    runs.into_iter()
        .map(|run| stoatty_protocol::command::MinimapRun {
            start_col: run.start_col,
            len: run.len,
            class: run.class,
        })
        .collect()
}

/// Convert an LSP `file:` URI to a [`PathBuf`]. Returns `None` for any
/// other scheme; non-`file:` diagnostic notifications are silently
/// dropped because stoat has no concept of remote-path buffers today.
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
fn backspace_range(rope: &stoat_text::Rope, cursor: usize, indent_width: usize) -> (usize, usize) {
    if cursor == 0 {
        return (0, 0);
    }

    let prev = rope.reversed_chars_at(cursor).next();
    let one_back = (cursor - prev.map(|ch| ch.len_utf8()).unwrap_or(1), cursor);

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
fn kill_to_line_start_target(rope: &stoat_text::Rope, cursor: usize) -> usize {
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

/// The ASCII control byte for `Ctrl`+`c`, mapping `Ctrl-A`..`Ctrl-Z` to
/// `0x01`..`0x1a`. `None` when `c` is not an ASCII letter.
fn control_byte(c: char) -> Option<u8> {
    c.is_ascii_alphabetic()
        .then(|| (c.to_ascii_lowercase() as u8) - b'a' + 1)
}

pub(crate) fn lsp_uri_to_path(uri: &lsp_types::Uri) -> Option<PathBuf> {
    if uri.scheme().map(|s| s.as_str()) != Some("file") {
        return None;
    }
    Some(PathBuf::from(uri.path().as_str()))
}

/// Synchronous core of the parse pipeline. When `deadline` is `Some`, the
/// host parse aborts if it would exceed it and the function returns `None`,
/// signalling that the caller should fall back to the background path.
/// `None` is also returned for ordinary parse failures (unsupported
/// language, etc.); the difference does not matter for the call sites.
pub(crate) fn parse_buffer_step(
    buffer_id: BufferId,
    snapshot: TextBufferSnapshot,
    lang: &Arc<Language>,
    prior: &mut Option<SyntaxState>,
    prior_syntax_map: &mut Option<stoat_language::SyntaxMap>,
    styles: &SyntaxStyles,
    deadline: Option<(std::time::Instant, &Executor)>,
) -> Option<ParseJobOutput> {
    let cur_version = snapshot.version;
    let new_rope = snapshot.visible_text.clone();

    // Edit a clone of the prior tree rather than mutating it in place. If
    // the parse aborts (deadline exceeded, etc.) the caller's prior must
    // remain valid for the next attempt; an in-place edit would leave the
    // registry holding a half-edited tree that would double-stamp position
    // offsets when re-edited next call.
    //
    // tree_sitter::Tree::clone is O(1) (refcount bump on the root subtree),
    // and tree.edit goes through ts_subtree_edit which is copy-on-write, so
    // editing the clone leaves the original untouched.
    let edited_tree = prior.as_ref().map(|prev| {
        let mut tree = prev.tree.clone();
        let edits = snapshot.edits_since(prev.version);
        language::edit_tree(&mut tree, edits.edits(), &prev.rope_snapshot, &new_rope);
        tree
    });

    let tree = match edited_tree.as_ref() {
        Some(old_tree) => match deadline {
            Some((dl, exec)) => {
                language::parse_rope_within(lang, &new_rope, Some(old_tree), dl, exec)?
            },
            None => language::parse_rope(lang, &new_rope, Some(old_tree))?,
        },
        None => match deadline {
            Some((dl, exec)) => language::parse_rope_within(lang, &new_rope, None, dl, exec)?,
            None => language::parse_rope(lang, &new_rope, None)?,
        },
    };

    prior.take();

    // Rebuild the multi-layer SyntaxMap from scratch, then read highlights from
    // its captures. There is no host-side interpolation pass to replay this
    // version's edits onto the prior layers, so reusing the prior map would
    // reparse incrementally against a stale, un-edited tree and drop every
    // highlight past the edit. Drop the prior map and parse fresh.
    //
    // FIXME: full-parses every keystroke. A host-side interpolation pass would
    // let the prior layers be reused for incremental reparse.
    prior_syntax_map.take();
    let mut syntax_map = stoat_language::SyntaxMap::default();
    let _ = syntax_map.reparse(&new_rope, lang.clone(), cur_version);

    // A capture resolves to a theme key index through its originating layer's
    // highlight_map(). A DEFAULT id (capture absent from the active theme)
    // carries no style and is skipped. captures() document order
    // (start, Reverse(end), depth) is kept so deeper injection layers land later
    // and win under the display map's endpoint precedence. highlight_map() clones
    // a locked map, so memoize it per layer language.
    let tokens: Arc<[SemanticTokenHighlight]> = {
        use std::collections::HashMap;

        let mut highlight_maps = HashMap::new();
        syntax_map
            .snapshot()
            .captures(0..new_rope.len(), &new_rope, |l| Some(&l.highlight_query))
            .into_iter()
            .filter_map(|cap| {
                let range = cap.node.byte_range();
                if range.start == range.end {
                    return None;
                }
                let map = highlight_maps
                    .entry(cap.language as *const Language as usize)
                    .or_insert_with(|| cap.language.highlight_map());
                let style_id = styles.id_for_highlight(map.get(cap.index))?;
                Some(SemanticTokenHighlight {
                    // Insertions at the start of a token attach to the previous
                    // span, not this one; insertions at the end attach to the
                    // next span. Keeps a typed character from silently extending
                    // a keyword or string into neighboring text.
                    range: snapshot.anchor_at(range.start, Bias::Right)
                        ..snapshot.anchor_at(range.end, Bias::Left),
                    style: style_id,
                })
            })
            .collect()
    };

    Some(ParseJobOutput {
        buffer_id,
        syntax: SyntaxState {
            tree,
            version: cur_version,
            rope_snapshot: new_rope,
        },
        syntax_map,
        tokens,
    })
}

/// Background parse worker. Owns all inputs by value so the future is `Send`
/// and can run on any executor thread.
pub(crate) async fn parse_buffer_async(
    buffer_id: BufferId,
    snapshot: TextBufferSnapshot,
    lang: Arc<Language>,
    mut prior: Option<SyntaxState>,
    mut prior_syntax_map: Option<stoat_language::SyntaxMap>,
    styles: SyntaxStyles,
) -> Option<ParseJobOutput> {
    parse_buffer_step(
        buffer_id,
        snapshot,
        &lang,
        &mut prior,
        &mut prior_syntax_map,
        &styles,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent_status::AgentHookEvent, buffer::TextBuffer};
    use std::path::{Path, PathBuf};

    #[test]
    fn scroll_anim_tick_advances_offset_then_settles() {
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let path = h.write_file("glide.rs", &body);
        h.open_file(&path);

        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .scroll_velocity = 200.0;
        assert!(
            h.stoat.is_animating(),
            "seeded velocity makes the editor animate"
        );

        h.stoat.tick_scroll_anim(0.016);
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            assert!(
                editor.scroll_offset > 0.0,
                "tick advances the fractional offset"
            );
            assert_eq!(
                editor.scroll_row,
                editor.scroll_offset.floor() as u32,
                "scroll_row tracks floor(scroll_offset)"
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
                .scroll_velocity,
            0.0,
            "settled velocity is zero"
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
                action_handlers::movement::wheel_impulse(editor, true);
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

    fn pane_scroll_state(h: &mut crate::test_harness::TestHarness) -> (u32, f32) {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        (editor.scroll_row, editor.scroll_velocity)
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
        for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            h.fake_fs().insert_file(root.join(name), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();

        crate::render::file_finder::file_finder_layout(h.stoat.size())
            .expect("finder fits the test terminal")
            .list
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

        let preview = crate::render::file_finder::file_finder_layout(h.stoat.size())
            .and_then(|layout| layout.preview)
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
        let velocity = h
            .stoat
            .active_workspace()
            .editors
            .get(preview_id)
            .expect("preview editor")
            .scroll_velocity;
        assert!(
            velocity > 0.0,
            "the wheel imparts downward scroll to the preview"
        );
    }

    #[test]
    fn wheel_over_palette_arg_preview_scrolls_it_not_the_list() {
        let mut h = crate::test_harness::TestHarness::with_size(100, 30);
        let root = std::path::PathBuf::from("/arg-preview");
        for name in ["a.rs", "b.rs", "c.rs"] {
            h.fake_fs().insert_file(root.join(name), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;
        h.type_text(":o ");
        h.settle();

        let preview = crate::render::command_palette::palette_arg_body(h.stoat.size())
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
        let velocity = h
            .stoat
            .active_workspace()
            .editors
            .get(preview_id)
            .expect("preview editor")
            .scroll_velocity;
        assert!(
            velocity > 0.0,
            "the wheel imparts downward scroll to the preview"
        );
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
            editor.scroll_velocity = 100.0;
            editor.scroll_row = 5;
            editor.scroll_glide = true;
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
            (
                editor.scroll_row,
                editor.scroll_offset,
                editor.scroll_velocity,
                editor.scroll_glide,
            ),
            (0, 0.0, 0.0, false),
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
            editor.scroll_glide = true;
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
        assert!(!editor.scroll_glide, "the glide clears on settle");
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
            editor.scroll_glide = true;
        }

        h.stoat.tick_scroll_anim(0.016);

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert_eq!(
            editor.scroll_offset, 100.0,
            "a gap wider than three viewports snaps straight to the target"
        );
        assert!(!editor.scroll_glide, "and clears the glide");
    }

    #[test]
    fn momentum_settle_arms_a_glide_instead_of_snapping() {
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let path = h.write_file("glide.rs", &body);
        h.open_file(&path);
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            editor.scroll_row = 42;
            editor.scroll_offset = 42.4;
            editor.scroll_velocity = 1.0;
        }

        h.stoat.tick_scroll_anim(0.016);

        assert!(
            h.stoat.is_animating(),
            "the coast eases onto the row instead of stopping dead"
        );
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert_eq!(editor.scroll_velocity, 0.0, "the coast has settled");
        assert!(editor.scroll_glide, "settle arms an ease glide");
        assert_eq!(
            editor.scroll_row, 42,
            "the glide targets the rounded resting row"
        );
        assert!(
            editor.scroll_offset > 42.0 && editor.scroll_offset < 43.0,
            "the offset keeps its fraction for the glide to ease in, not a snapped integer"
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
                persist: true,
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
                    persist: false,
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
                persist: false,
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
            stoat.drain_fs_watch_events();
            scheduler.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);
            stoat.drain_pending_index_edits();
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

        let drive = |stoat: &mut Stoat| {
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

    #[test]
    fn agent_output_feeds_emulator() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());

        let session: Arc<dyn crate::host::TerminalSession> =
            Arc::new(crate::host::FakeTerminalSession::new());
        let agent_id =
            stoat
                .active_workspace_mut()
                .terms
                .insert(crate::term_session::TermSession::new(
                    crate::term_screen::TermScreen::new(24, 80),
                    session,
                ));

        let effect = stoat.handle_pty_notification(PtyNotification::TermOutput {
            agent_id,
            data: b"hello".to_vec(),
        });

        assert_eq!(effect, UpdateEffect::Redraw);
        let term = &stoat.active_workspace().terms[agent_id].term;
        let row: String = term.row(0).iter().map(|cell| cell.ch).collect();
        assert!(row.starts_with("hello"), "row: {row:?}");
    }

    #[test]
    fn term_pane_osc52_forwards_to_clipboard() {
        let mut h = Stoat::test();
        let session: Arc<dyn crate::host::TerminalSession> =
            Arc::new(crate::host::FakeTerminalSession::new());
        let agent_id =
            h.stoat
                .active_workspace_mut()
                .terms
                .insert(crate::term_session::TermSession::new(
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
    fn term_query_reply_writes_back_to_pty() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());

        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
        let agent_id =
            stoat
                .active_workspace_mut()
                .terms
                .insert(crate::term_session::TermSession::new(
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
        let agent_id = ws.terms.insert(crate::term_session::TermSession::new(
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
        let agent_id = ws.terms.insert(crate::term_session::TermSession::new(
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
        let term_id = ws.terms.insert(crate::term_session::TermSession::new(
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
        ws.terms.insert(crate::term_session::TermSession::new(
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

    fn stoat_with_focused_term(
        make_view: fn(TermId) -> View,
    ) -> (Stoat, TermId, Arc<crate::host::FakeTerminalSession>) {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());

        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let term_id = ws.terms.insert(crate::term_session::TermSession::new(
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

        h.stoat.handle_key(bare(KeyCode::Char('x')));
        assert!(!h.stoat.user_vars.contains_key("mode"));
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn workspace_picker_binding_is_rebindable() {
        let mut h = Stoat::test();
        h.stoat.keymap =
            compile_keymap("on key { modal == workspace_picker { q -> WorkspacePickerClose(); } }");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchWorkspace);
        assert!(h.stoat.workspace_picker.is_some());

        // `q` is not a default picker binding, so closing on it proves the
        // `modal == workspace_picker` block drives the picker, not hardcoded
        // dispatch.
        h.stoat.handle_key(bare(KeyCode::Char('q')));
        assert!(h.stoat.workspace_picker.is_none());
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

    /// When `parse_buffer_step` aborts on the deadline, the prior state
    /// passed via `&mut Option<_>` must remain populated so the caller
    /// can hand it to a follow-up parse without losing incrementality.
    #[test]
    fn parse_buffer_step_preserves_prior_on_deadline_abort() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let executor = scheduler.executor();
        let lang = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .unwrap();
        let styles = SyntaxStyles::from_theme(&crate::theme::Theme::empty());
        let buffer_id = BufferId::new(1);

        let text = "fn a() {}\n".repeat(10_000);
        let mut buf = TextBuffer::with_text(buffer_id, &text);
        let snap1 = buf.snapshot.clone();

        let mut prior: Option<SyntaxState> = None;
        let mut prior_map: Option<stoat_language::SyntaxMap> = None;
        let out = parse_buffer_step(
            buffer_id,
            snap1,
            &lang,
            &mut prior,
            &mut prior_map,
            &styles,
            None,
        )
        .expect("first parse should succeed");
        let initial_version = out.syntax.version;

        let mut prior: Option<SyntaxState> = Some(out.syntax);
        let mut prior_map: Option<stoat_language::SyntaxMap> = Some(out.syntax_map);
        buf.edit(0..0, "// edit\n");
        let snap2 = buf.snapshot.clone();

        let deadline = executor.now();
        let result = parse_buffer_step(
            buffer_id,
            snap2.clone(),
            &lang,
            &mut prior,
            &mut prior_map,
            &styles,
            Some((deadline, &executor)),
        );
        assert!(result.is_none(), "expected deadline abort to return None");
        let prior_state = prior
            .as_ref()
            .expect("prior must survive deadline abort, was consumed");
        assert_eq!(
            prior_state.version, initial_version,
            "prior version must be unchanged",
        );
        assert!(
            prior_map.is_some(),
            "prior_syntax_map must survive deadline abort",
        );

        let recovery = parse_buffer_step(
            buffer_id,
            snap2,
            &lang,
            &mut prior,
            &mut prior_map,
            &styles,
            None,
        )
        .expect("recovery parse should succeed");
        assert!(recovery.syntax.version > initial_version);
        assert!(prior.is_none(), "successful parse must consume the prior");
        assert!(prior_map.is_none());
    }

    /// The deadline check inside `parse_rope_within` reads time from the
    /// `Executor`, not the wall clock, so `TestScheduler::advance_clock`
    /// drives the timeout deterministically.
    #[test]
    fn parse_buffer_step_deadline_uses_executor_clock() {
        use std::time::Duration;
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let executor = scheduler.executor();
        let lang = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .unwrap();
        let styles = SyntaxStyles::from_theme(&crate::theme::Theme::empty());
        let buffer_id = BufferId::new(1);

        let text = "fn a() {}\n".repeat(10_000);
        let mut buf = TextBuffer::with_text(buffer_id, &text);
        let snap1 = buf.snapshot.clone();

        let mut prior: Option<SyntaxState> = None;
        let mut prior_map: Option<stoat_language::SyntaxMap> = None;
        let out = parse_buffer_step(
            buffer_id,
            snap1,
            &lang,
            &mut prior,
            &mut prior_map,
            &styles,
            None,
        )
        .expect("first parse should succeed");

        let mut prior: Option<SyntaxState> = Some(out.syntax);
        let mut prior_map: Option<stoat_language::SyntaxMap> = Some(out.syntax_map);
        buf.edit(0..0, "// edit\n");
        let snap2 = buf.snapshot.clone();

        let deadline = executor.now() + Duration::from_secs(3600);
        let succeeded = parse_buffer_step(
            buffer_id,
            snap2,
            &lang,
            &mut prior,
            &mut prior_map,
            &styles,
            Some((deadline, &executor)),
        )
        .expect("deadline far in the future should not abort");

        let mut prior: Option<SyntaxState> = Some(succeeded.syntax);
        let mut prior_map: Option<stoat_language::SyntaxMap> = Some(succeeded.syntax_map);
        buf.edit(0..0, "// edit2\n");
        let snap3 = buf.snapshot.clone();

        scheduler.advance_clock(Duration::from_secs(7200));

        let aborted = parse_buffer_step(
            buffer_id,
            snap3,
            &lang,
            &mut prior,
            &mut prior_map,
            &styles,
            Some((deadline, &executor)),
        );
        assert!(
            aborted.is_none(),
            "after advance_clock past the deadline, parse must abort",
        );
    }

    #[test]
    fn minimap_emits_declare_and_line_summaries() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/minimap");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.resize(80, 24);

        let _ = h.stoat.render();
        h.stoat.emit_apc_scene();
        h.stoat.emit_minimap();
        let first = drain_apc(&mut rx);
        assert!(
            first.iter().any(|cmd| matches!(cmd, Command::Minimap(_))),
            "the first frame declares the strip, got {first:?}"
        );
        assert!(
            first
                .iter()
                .any(|cmd| matches!(cmd, Command::MinimapLines(_))),
            "the first frame sends the initial line summaries, got {first:?}"
        );

        h.type_keys("i z");
        h.settle();
        let _ = h.stoat.render();
        h.stoat.emit_apc_scene();
        h.stoat.emit_minimap();
        let edited = drain_apc(&mut rx);
        assert!(
            edited
                .iter()
                .any(|cmd| matches!(cmd, Command::MinimapLines(_))),
            "an edit splices the changed line, got {edited:?}"
        );
    }

    #[test]
    fn minimap_drops_content_when_the_buffer_closes() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/minimap-drop");
        let a = root.join("a.txt");
        let b = root.join("b.txt");
        h.fake_fs().insert_file(&a, b"alpha\nbravo\n");
        h.fake_fs().insert_file(&b, b"charlie\ndelta\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: a });
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: b });
        h.settle();
        h.resize(80, 24);
        let _ = h.stoat.render();
        h.stoat.emit_apc_scene();
        h.stoat.emit_minimap();
        let _ = drain_apc(&mut rx);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::CloseBuffer);
        h.settle();
        let _ = h.stoat.render();
        h.stoat.emit_minimap();
        let closed = drain_apc(&mut rx);
        assert!(
            closed
                .iter()
                .any(|cmd| matches!(cmd, Command::MinimapDrop(_))),
            "closing a buffer drops its minimap content, got {closed:?}"
        );
    }

    #[test]
    fn minimap_view_tracks_the_scroll_position() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/minimap-view");
        let path = root.join("a.txt");
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.resize(80, 24);

        let _ = h.stoat.render();
        h.stoat.emit_smooth_scroll();
        let first = drain_apc(&mut rx);
        let top_at_origin = first.iter().find_map(|cmd| match cmd {
            Command::MinimapView(v) => Some(v.top_256),
            _ => None,
        });
        assert_eq!(
            top_at_origin,
            Some(0),
            "the origin thumb sits at line 0, got {first:?}"
        );

        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        {
            let editor = h
                .stoat
                .active_workspace_mut()
                .editors
                .get_mut(editor_id)
                .expect("editor");
            editor.scroll_row = 50;
            editor.scroll_offset = 50.0;
        }
        let _ = h.stoat.render();
        h.stoat.emit_smooth_scroll();
        let scrolled = drain_apc(&mut rx);
        let top_after_scroll = scrolled.iter().find_map(|cmd| match cmd {
            Command::MinimapView(v) => Some(v.top_256),
            _ => None,
        });
        assert_eq!(
            top_after_scroll,
            Some(50 * 256),
            "the thumb tracks the scrolled top row, got {scrolled:?}"
        );
    }

    #[test]
    fn minimap_marks_diff_and_diagnostic_lines() {
        use crate::minimap::EdgeClass;
        use lsp_types::DiagnosticSeverity;
        use stoatty_protocol::command::{Command, MinimapRun};

        fn diag(line: u32, severity: DiagnosticSeverity) -> lsp_types::Diagnostic {
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position { line, character: 0 },
                    end: lsp_types::Position { line, character: 1 },
                },
                severity: Some(severity),
                ..Default::default()
            }
        }

        // The leading run of buffer line `n` in the most recent emit.
        fn line_lead(cmds: &[Command], n: u32) -> Option<MinimapRun> {
            cmds.iter().rev().find_map(|cmd| match cmd {
                Command::MinimapLines(l) => {
                    let idx = n.checked_sub(l.start)? as usize;
                    l.lines.get(idx).and_then(|runs| runs.first().copied())
                },
                _ => None,
            })
        }

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/minimap-marks");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"keep\nnew\ntail\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
        {
            let base = "keep\nold\ntail\n";
            let text = "keep\nnew\ntail\n";
            let dm = crate::diff_map::DiffMap::from_structural_changes(
                stoat_language::structural_diff::diff(base, text),
                base,
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
        h.stoat
            .active_workspace_mut()
            .panes
            .resize(Rect::new(0, 0, 80, 24));

        let modified = h.stoat.minimap_class_table.edge_class(EdgeClass::Modified);
        let error = h.stoat.minimap_class_table.edge_class(EdgeClass::Error);

        let _ = h.stoat.render();
        h.stoat.emit_apc_scene();
        h.stoat.emit_minimap();
        let first = drain_apc(&mut rx);
        assert_eq!(
            line_lead(&first, 1).map(|r| r.class),
            Some(modified),
            "the modified line leads with the modified edge class, got {first:?}"
        );

        h.stoat
            .diagnostics
            .replace_for_path(path.clone(), vec![diag(1, DiagnosticSeverity::ERROR)]);
        let _ = h.stoat.render();
        h.stoat.emit_minimap();
        let errored = drain_apc(&mut rx);
        assert_eq!(
            line_lead(&errored, 1).map(|r| r.class),
            Some(error),
            "an error overrides the diff mark, got {errored:?}"
        );

        h.stoat.diagnostics.replace_for_path(path, vec![]);
        let _ = h.stoat.render();
        h.stoat.emit_minimap();
        let cleared = drain_apc(&mut rx);
        assert_eq!(
            line_lead(&cleared, 1).map(|r| r.class),
            Some(modified),
            "clearing the diagnostic reverts to the modified mark, got {cleared:?}"
        );
        let touched: Vec<u32> = cleared
            .iter()
            .filter_map(|c| match c {
                Command::MinimapLines(l) => Some(l.start),
                _ => None,
            })
            .collect();
        assert_eq!(touched, vec![1], "only the formerly-marked line re-splices");
    }

    #[test]
    fn minimap_colors_align_past_a_leading_tab() {
        use crate::display_map::highlights::{
            HighlightStyle, HighlightStyleInterner, SemanticTokenHighlight,
        };
        use ratatui::style::Color;
        use std::sync::Arc;
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/minimap-tab");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"\tfoo\nbar\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let (editor_id, buffer_id) = {
            let ids = h.stoat.focused_editor_ids().expect("editor");
            (ids.0, ids.1)
        };

        // Color the token a real syntax-scope color so it maps to a class.
        let [r, g, b] = h.stoat.minimap_class_table.palette()[1];
        let color = Color::Rgb(r, g, b);
        let expected_class = h.stoat.minimap_class_table.class_of_color(color);
        assert_ne!(
            expected_class, 0,
            "the test color must map to a syntax class"
        );

        let mut interner = HighlightStyleInterner::default();
        let style = interner.intern(HighlightStyle {
            foreground: Some(color),
            background: None,
            bold: None,
            italic: None,
            underline: None,
            strikethrough: None,
        });
        let range = {
            let shared = h
                .stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer");
            let snap = shared.read().expect("poisoned").snapshot.clone();
            snap.anchor_at(1, Bias::Right)..snap.anchor_at(4, Bias::Left)
        };
        let tokens: Arc<[SemanticTokenHighlight]> =
            Arc::from(vec![SemanticTokenHighlight { range, style }]);
        h.stoat.active_workspace_mut().editors[editor_id]
            .display_map
            .set_semantic_token_highlights(buffer_id, tokens, Arc::new(interner));

        h.stoat
            .active_workspace_mut()
            .panes
            .resize(Rect::new(0, 0, 80, 24));
        let _ = h.stoat.render();
        h.stoat.emit_apc_scene();
        h.stoat.emit_minimap();
        let cmds = drain_apc(&mut rx);

        let line0 = cmds
            .iter()
            .rev()
            .find_map(|cmd| match cmd {
                Command::MinimapLines(l) if l.start == 0 => l.lines.first().cloned(),
                _ => None,
            })
            .expect("line 0 summary");

        // The tab expands content to column 4, where the token's colored run
        // begins. The old display-chunk mapping placed the token past the raw
        // line's bytes, dropping the color.
        assert_eq!(
            line0.first().map(|run| (run.start_col, run.class)),
            Some((4, expected_class)),
            "the run starts at the tab-expanded column in the syntax class, got {line0:?}"
        );
    }

    #[test]
    fn minimap_recolors_on_syntax_toggle() {
        use crate::display_map::highlights::{
            HighlightStyle, HighlightStyleInterner, SemanticTokenHighlight,
        };
        use ratatui::style::Color;
        use std::sync::Arc;
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/minimap-toggle");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"foo\nbar\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let (editor_id, buffer_id) = {
            let ids = h.stoat.focused_editor_ids().expect("editor");
            (ids.0, ids.1)
        };

        let [r, g, b] = h.stoat.minimap_class_table.palette()[1];
        let color = Color::Rgb(r, g, b);
        let colored_class = h.stoat.minimap_class_table.class_of_color(color);
        assert_ne!(
            colored_class, 0,
            "the test color must map to a syntax class"
        );

        let mut interner = HighlightStyleInterner::default();
        let style = interner.intern(HighlightStyle {
            foreground: Some(color),
            background: None,
            bold: None,
            italic: None,
            underline: None,
            strikethrough: None,
        });
        let range = {
            let shared = h
                .stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer");
            let snap = shared.read().expect("poisoned").snapshot.clone();
            snap.anchor_at(0, Bias::Right)..snap.anchor_at(3, Bias::Left)
        };
        let tokens: Arc<[SemanticTokenHighlight]> =
            Arc::from(vec![SemanticTokenHighlight { range, style }]);
        h.stoat.active_workspace_mut().editors[editor_id]
            .display_map
            .set_semantic_token_highlights(buffer_id, tokens, Arc::new(interner));
        h.stoat
            .active_workspace_mut()
            .panes
            .resize(Rect::new(0, 0, 80, 24));

        let line0_class = |cmds: &[Command]| {
            cmds.iter().rev().find_map(|cmd| match cmd {
                Command::MinimapLines(l) if l.start == 0 => l
                    .lines
                    .first()
                    .and_then(|runs| runs.first())
                    .map(|r| r.class),
                _ => None,
            })
        };

        let _ = h.stoat.render();
        h.stoat.emit_apc_scene();
        h.stoat.emit_minimap();
        let colored = drain_apc(&mut rx);
        assert_eq!(
            line0_class(&colored),
            Some(colored_class),
            "line 0 is colored under syntax highlighting, got {colored:?}"
        );

        // Toggling syntax off re-summarizes the built lines monochrome, with no
        // buffer edit.
        h.stoat.syntax_highlight = false;
        let _ = h.stoat.render();
        h.stoat.emit_minimap();
        let mono = drain_apc(&mut rx);
        assert_eq!(
            line0_class(&mono),
            Some(0),
            "the toggle recolors line 0 monochrome, got {mono:?}"
        );
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

    #[test]
    fn mouse_translates_to_focused_pane_coords() {
        let mut h = Stoat::test();
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(10, 5, 20, 8);
        let translated = h.stoat.translate_mouse_to_focused(15, 9);
        assert_eq!(translated, Some((5, 4)));
    }

    #[test]
    fn mouse_above_focused_pane_saturates_to_zero() {
        let mut h = Stoat::test();
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(10, 5, 20, 8);
        let translated = h.stoat.translate_mouse_to_focused(3, 2);
        assert_eq!(translated, Some((0, 0)));
    }

    #[test]
    fn editor_pool_pane_region_is_content_rect() {
        let mut h = Stoat::test();
        let root = std::path::PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(2, 1, 76, 23);

        let panes = h.stoat.editor_pool_panes();
        assert_eq!(panes.len(), 1, "one editor pane is pooled");
        let (_, _, region) = panes[0];
        // Content rect is the pane area minus its one-row status bar.
        assert_eq!(
            (region.top, region.left, region.width, region.height),
            (1, 2, 76, 22)
        );
    }

    #[test]
    fn no_pool_pane_when_pane_is_not_an_editor() {
        let mut h = Stoat::test();
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).view = View::Label("scratch".into());
        assert!(h.stoat.editor_pool_panes().is_empty());
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

    fn open_indent_buffer(h: &mut crate::test_harness::TestHarness, name: &str, contents: &[u8]) {
        let root = std::path::PathBuf::from("/indent");
        let path = root.join(name);
        h.fake_fs().insert_file(&path, contents);
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        // The parse job completes during settle, but its result is installed by
        // drive_background (the per-tick background pass), so drive it to store
        // the syntax tree before auto-indent reads it.
        h.stoat.drive_background();
        h.settle();
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
    fn emit_smooth_scroll_retires_pools_in_overlay_mode() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        h.stoat.emit_smooth_scroll();
        let first = drain_apc(&mut rx);
        assert!(
            first
                .iter()
                .any(|cmd| matches!(cmd, Command::PoolRegion(_))),
            "first emit declares the editor pool, got {first:?}"
        );

        // Entering a full-screen overlay screen retires the editor pool.
        h.stoat.active_workspace_mut().rebase = Some(crate::rebase::RebaseState::new(
            std::path::PathBuf::from("/pool"),
            "onto".into(),
            vec![],
        ));
        h.stoat.emit_smooth_scroll();
        let cmds = drain_apc(&mut rx);
        assert!(
            !cmds.is_empty() && cmds.iter().all(|cmd| matches!(cmd, Command::PoolDrop(_))),
            "overlay mode only drops pools, got {cmds:?}"
        );
    }

    #[test]
    fn emit_smooth_scroll_pools_the_hover_and_retires_it_on_close() {
        use crate::action_handlers::lsp::HoverPopup;
        use ratatui::{layout::Rect, style::Style};
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        h.stoat.pending_hover = Some(HoverPopup {
            lines: vec![vec![("hovered".to_string(), Style::default())]],
            anchor_offset: 0,
            editor_id,
            scroll_half_pages: 0,
            area: Rect::default(),
            inner: Rect::default(),
            selection: None,
        });
        h.stoat.emit_smooth_scroll();
        let opened = drain_apc(&mut rx);
        assert!(
            opened.iter().any(|cmd| matches!(
                cmd,
                Command::PoolRegion(r) if r.pool == crate::smooth_scroll::non_pane_pool::HOVER
            )),
            "an open hover emits its pool region, got {opened:?}"
        );

        h.stoat.pending_hover = None;
        h.stoat.emit_smooth_scroll();
        let closed = drain_apc(&mut rx);
        assert!(
            closed.iter().any(|cmd| matches!(
                cmd,
                Command::PoolDrop(d) if d.pool == crate::smooth_scroll::non_pane_pool::HOVER
            )),
            "closing the hover retires its pool, got {closed:?}"
        );
    }

    /// Lay out a hover of `num_lines` lines each `line_width` wide in a
    /// `width` x `height` window, returning the popup and inner rects.
    fn hover_layout(width: u16, height: u16, num_lines: usize, line_width: usize) -> (Rect, Rect) {
        use crate::{action_handlers::lsp::HoverPopup, test_harness::TestHarness};
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
        h.stoat.pending_hover = Some(HoverPopup {
            lines,
            anchor_offset: 0,
            editor_id,
            scroll_half_pages: 0,
            area: Rect::default(),
            inner: Rect::default(),
            selection: None,
        });
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

    #[test]
    fn hover_popup_overflows_across_a_vertical_split() {
        use crate::{action_handlers::lsp::HoverPopup, test_harness::TestHarness};
        use ratatui::style::Style;

        let mut h = TestHarness::with_size(80, 24);
        let root = std::path::PathBuf::from("/hover");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let left = {
            let ws = h.stoat.active_workspace_mut();
            let left = ws.panes.focus();
            ws.panes.split(crate::pane::Axis::Vertical);
            ws.panes.resize(Rect::new(0, 0, 80, 24));
            left
        };
        let left_content = crate::render::layout::split_pane_status(
            h.stoat.active_workspace().panes.pane(left).area,
        )
        .0;
        h.stoat.focus_at(left_content.x + 1, left_content.y + 1);
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;

        // A hover wider than the left pane.
        h.stoat.pending_hover = Some(HoverPopup {
            lines: vec![vec![("x".repeat(60), Style::default())]],
            anchor_offset: 0,
            editor_id,
            scroll_half_pages: 0,
            area: Rect::default(),
            inner: Rect::default(),
            selection: None,
        });

        let (popup, _) = crate::render::hover::hover_popup_layout(&mut h.stoat).expect("layout");
        assert!(
            popup.width > left_content.width,
            "the popup widens past the left pane ({} > {})",
            popup.width,
            left_content.width,
        );
        assert!(
            popup.x + popup.width > left_content.x + left_content.width,
            "the popup crosses the divider into the right pane"
        );
    }

    #[test]
    fn hover_popup_overflows_into_the_pane_below() {
        use crate::{action_handlers::lsp::HoverPopup, test_harness::TestHarness};
        use ratatui::style::Style;

        let mut h = TestHarness::with_size(40, 24);
        let root = std::path::PathBuf::from("/hover");
        let path = root.join("a.txt");
        let content: String = (0..40).map(|_| "x\n").collect();
        h.fake_fs().insert_file(&path, content.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let top = {
            let ws = h.stoat.active_workspace_mut();
            let top = ws.panes.focus();
            ws.panes.split(crate::pane::Axis::Horizontal);
            ws.panes.resize(Rect::new(0, 0, 40, 24));
            top
        };
        let top_content = crate::render::layout::split_pane_status(
            h.stoat.active_workspace().panes.pane(top).area,
        )
        .0;
        h.stoat.focus_at(top_content.x + 1, top_content.y + 1);
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;

        // Anchor on the top pane's last visible row (each "x\n" line is 2 bytes).
        let last_row_line = top_content.height as usize - 1;
        h.stoat.pending_hover = Some(HoverPopup {
            lines: vec![vec![("hi".to_string(), Style::default())]],
            anchor_offset: last_row_line * 2,
            editor_id,
            scroll_half_pages: 0,
            area: Rect::default(),
            inner: Rect::default(),
            selection: None,
        });

        let (popup, _) = crate::render::hover::hover_popup_layout(&mut h.stoat).expect("layout");
        let cursor_row = top_content.y + last_row_line as u16;
        assert!(
            popup.y > cursor_row,
            "the popup places below the cursor ({} > {}) instead of flipping above",
            popup.y,
            cursor_row,
        );
        assert!(
            popup.y >= top_content.y + top_content.height,
            "the popup overflows into the pane below"
        );
    }

    /// A hover popup at a fixed area (`9,1 22x7`) with interior (`10,2 20x5`),
    /// `lines` as single unstyled spans and the given scroll offset.
    fn hover_sel_popup(
        lines: &[&str],
        scroll_half_pages: usize,
    ) -> action_handlers::lsp::HoverPopup {
        use ratatui::style::Style;
        action_handlers::lsp::HoverPopup {
            lines: lines
                .iter()
                .map(|l| vec![(l.to_string(), Style::default())])
                .collect(),
            anchor_offset: 0,
            editor_id: EditorId::default(),
            scroll_half_pages,
            area: Rect {
                x: 9,
                y: 1,
                width: 22,
                height: 7,
            },
            inner: Rect {
                x: 10,
                y: 2,
                width: 20,
                height: 5,
            },
            selection: None,
        }
    }

    #[test]
    fn hover_drag_copies_and_leaves_the_editor_untouched() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "buffer text\n");
        h.stoat.pending_hover = Some(hover_sel_popup(&["hello world", "second line"], 0));

        // Down at inner (10,2) = (line 0, col 0); drag to (13,2) = col 3.
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 2));
        h.stoat
            .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 13, 2));
        h.stoat
            .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 13, 2));

        assert_eq!(h.fake_clipboard().writes(), vec!["hel"]);
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
        use crate::action_handlers::lsp::HoverPopup;
        use ratatui::style::Style;

        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "alpha beta gamma\n");
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        h.stoat.pending_hover = Some(HoverPopup {
            lines: vec![vec![("hover".to_string(), Style::default())]],
            anchor_offset: 0,
            editor_id,
            scroll_half_pages: 0,
            area: Rect::default(),
            inner: Rect::default(),
            selection: None,
        });

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
        use crate::action_handlers::lsp::HoverPopup;
        use ratatui::style::Style;

        let popup = HoverPopup {
            lines: vec![vec![("x".repeat(60), Style::default())]],
            anchor_offset: 0,
            editor_id: EditorId::default(),
            scroll_half_pages: 0,
            area: Rect::default(),
            inner: Rect {
                x: 0,
                y: 0,
                width: 50,
                height: 3,
            },
            selection: None,
        };
        for cell in 0..40u16 {
            let (line, col) = crate::render::hover::hover_hit_test(&popup, true, cell, 0);
            assert_eq!(line, 0);
            assert_eq!(col, (cell as usize * 256 + 128) / 218);
        }
    }

    #[test]
    fn hover_grid_highlight_paints_the_selection_bg() {
        use crate::{
            action_handlers::lsp::{HoverPopup, HoverSelection},
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
        h.stoat.pending_hover = Some(HoverPopup {
            lines: vec![vec![("hello world".to_string(), Style::default())]],
            anchor_offset: 0,
            editor_id,
            scroll_half_pages: 0,
            area: Rect::default(),
            inner: Rect::default(),
            selection: Some(HoverSelection {
                anchor: (0, 0),
                head: (0, 4),
                dragging: false,
            }),
        });

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
        use crate::{action_handlers::lsp::HoverSelection, register::Register};

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
        h.stoat.set_stoatty_apc(true, tx);
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
    fn a_live_hover_selection_retires_the_pool() {
        use crate::action_handlers::lsp::{HoverPopup, HoverSelection};
        use ratatui::{layout::Rect, style::Style};
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);
        let root = std::path::PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        h.stoat.pending_hover = Some(HoverPopup {
            lines: vec![vec![("hovered".to_string(), Style::default())]],
            anchor_offset: 0,
            editor_id,
            scroll_half_pages: 0,
            area: Rect::default(),
            inner: Rect::default(),
            selection: None,
        });
        h.stoat.emit_smooth_scroll();
        let opened = drain_apc(&mut rx);
        assert!(
            opened.iter().any(|cmd| matches!(
                cmd,
                Command::PoolRegion(r) if r.pool == crate::smooth_scroll::non_pane_pool::HOVER
            )),
            "an unselected hover pools its body, got {opened:?}",
        );

        if let Some(popup) = h.stoat.pending_hover.as_mut() {
            popup.selection = Some(HoverSelection {
                anchor: (0, 0),
                head: (0, 3),
                dragging: false,
            });
        }
        h.stoat.emit_smooth_scroll();
        let dropped = drain_apc(&mut rx);
        assert!(
            dropped.iter().any(|cmd| matches!(
                cmd,
                Command::PoolDrop(d) if d.pool == crate::smooth_scroll::non_pane_pool::HOVER
            )),
            "a live selection retires the pool so the live frame owns it, got {dropped:?}",
        );
    }

    #[test]
    fn apc_scene_emits_nothing_for_a_plain_editor_frame() {
        let mut h = Stoat::test();
        // The default theme resolves status colors to RGB, which drives the
        // status bar into the scene as sub-cell components. A theme without RGB
        // status colors keeps the status bar in cells, so with line numbers off
        // the frame stays genuinely widget-free.
        h.stoat.theme = rgb_review_theme();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        // Line numbers off so the paint carries no off-grid gutter, and the
        // minimap off so no strip declare rides the scene. Both keep the frame
        // genuinely widget-free.
        h.stoat.settings.editor_line_numbers = Some(LineNumbers::Off);
        h.stoat.settings.editor_minimap = Some(false);

        let root = std::path::PathBuf::from("/scene");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let mut buf = Buffer::empty(h.stoat.size());
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        assert!(
            drain_apc(&mut rx).is_empty(),
            "a widget-free paint appends nothing, so the scene flush stays silent"
        );
    }

    #[test]
    fn apc_scene_flush_is_silent_outside_stoatty() {
        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(false, tx);

        let mut buf = Buffer::empty(h.stoat.size());
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        assert!(
            drain_apc(&mut rx).is_empty(),
            "the scene flush never touches the channel outside stoatty"
        );
    }

    /// The rich review gutter engages only when every color resolves to RGB, so
    /// tests need a hex theme. The default theme uses named colors.
    fn rgb_review_theme() -> crate::theme::Theme {
        let src = r##"theme rgbtest {
            diff.context.fg = "#808080";
            diff.added.fg = "#00ff00";
            diff.deleted.fg = "#ff0000";
            diff.current_hunk.fg = "#00ffff";
            ui.text.muted.fg = "#606060";
            ui.background.bg = "#282c34";
        }"##;
        let (config, _) = stoat_config::parse(src);
        crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbtest")
            .expect("rgb theme builds")
    }

    #[test]
    fn review_gutter_emits_sub_cell_components_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = rgb_review_theme();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        h.open_review_from_texts(&[("a.rs", "fn a() {}\n", "fn a_renamed() {}\n")]);

        let mut buf = Buffer::empty(h.stoat.size());
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(c, Command::TextRun(_))),
            "line numbers emit as sub-cell text runs, got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Command::Bar(_))),
            "status marks and the separator emit as sub-cell bars, got {cmds:?}"
        );
    }

    #[test]
    fn status_bar_emits_sub_cell_components_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        // Line numbers off so the only off-grid components are the status bar's.
        h.stoat.settings.editor_line_numbers = Some(LineNumbers::Off);

        let root = std::path::PathBuf::from("/status");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let mut buf = Buffer::empty(h.stoat.size());
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::TextRun(t) if t.col == 0)),
            "the mode segment emits as a text run at col 0, got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::Bar(b) if b.height == 1)),
            "the status hairline emits as a one-sixteenth bar, got {cmds:?}"
        );
    }

    #[test]
    fn overlay_status_bar_emits_sub_cell_components_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        // A rebase overlay routes its status row through render_overlay_status.
        h.stoat.active_workspace_mut().rebase = Some(crate::rebase::RebaseState::new(
            std::path::PathBuf::from("/overlay"),
            "onto".into(),
            vec![],
        ));

        let mut buf = Buffer::empty(h.stoat.size());
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::TextRun(t) if t.col == 0)),
            "the overlay status row emits a text run at col 0, got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::Bar(b) if b.height == 1)),
            "the overlay status hairline emits as a one-sixteenth bar, got {cmds:?}"
        );
    }

    fn rgb_diagnostic_theme() -> crate::theme::Theme {
        let src = r##"theme rgbdiag {
            ui.diagnostic.error.fg = "#ff0000";
            ui.diagnostic.warning.fg = "#ffff00";
            ui.diagnostic.info.fg = "#00ffff";
            ui.diagnostic.hint.fg = "#808080";
            ui.text.muted.fg = "#606060";
            ui.background.bg = "#282c34";
        }"##;
        let (config, _) = stoat_config::parse(src);
        crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbdiag")
            .expect("rgb theme builds")
    }

    #[test]
    fn diagnostic_gutter_emits_sub_cell_bars_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = rgb_diagnostic_theme();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/diag-rich");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.stoat.diagnostics.replace_for_path(
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
                ..Default::default()
            }],
        );

        let mut buf = Buffer::empty(h.stoat.size());
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(c, Command::Bar(_))),
            "a severity mark emits a sub-cell bar, got {cmds:?}"
        );
    }

    #[test]
    fn diagnostic_popover_emits_a_popover_frame_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = rgb_diagnostic_theme();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/diag-popover");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"let x = 1;\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        // A span covering the start, so the cursor at offset zero sits inside it.
        h.stoat.diagnostics.replace_for_path(
            path,
            vec![lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "unexpected token".to_string(),
                ..Default::default()
            }],
        );

        let mut buf = Buffer::empty(h.stoat.size());
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(c, Command::Popover(_))),
            "a diagnostic under the cursor emits a popover frame, got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::Icon(icon) if icon.offset == [3, 6])),
            "the severity icon carries the popover offset so it sits inside the card, got {cmds:?}"
        );
    }

    #[test]
    fn diagnostic_popover_dodges_a_cursor_under_the_below_placement() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = rgb_diagnostic_theme();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/diag-popover-dodge");
        let path = root.join("a.txt");
        h.fake_fs()
            .insert_file(&path, b"aaaaa\nbbbbb\nccccc\nddddd\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        // A multi-line span keeps the cursor inside the diagnostic after it moves
        // down, and a multi-line message makes the below-anchor popover tall
        // enough to sit over the row beneath the diagnostic's start.
        h.stoat.diagnostics.replace_for_path(
            path,
            vec![lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 3,
                        character: 0,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "line one\nline two\nline three".to_string(),
                ..Default::default()
            }],
        );

        // Drop the cursor onto the row the below-anchor popover would occupy.
        action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveDown);

        let mut buf = Buffer::empty(h.stoat.size());
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let (cx, cy) = h
            .stoat
            .primary_cursor_screen_pos()
            .expect("primary cursor on screen");
        let cmds = drain_apc(&mut rx);
        let popover = cmds
            .iter()
            .find_map(|c| match c {
                Command::Popover(p) => Some(p),
                _ => None,
            })
            .expect("a diagnostic popover frame");

        let covers_cursor = cx >= popover.left
            && cx < popover.left + popover.width
            && cy >= popover.top
            && cy < popover.top + popover.height;
        assert!(
            !covers_cursor,
            "popover rect {popover:?} must not cover the cursor cell {:?}",
            (cx, cy)
        );
    }

    /// modal_frame's rich arm engages only when the border fg and the mask bg
    /// both resolve to RGB, so the modal APC tests need a hex theme. The default
    /// theme uses named colors and would fall back to glyphs.
    fn rgb_modal_theme() -> crate::theme::Theme {
        let src = r##"theme rgbmodal {
            ui.modal.help.fg = "#8899aa";
            ui.modal.hints.fg = "#8899aa";
            ui.text.fg = "#c8ccd4";
            ui.text.muted.fg = "#606060";
            ui.key_label.fg = "#d19a66";
            ui.background.bg = "#282c34";
        }"##;
        let (config, _) = stoat_config::parse(src);
        crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbmodal")
            .expect("rgb theme builds")
    }

    /// A hex theme for the pane-divider APC test, so the border colors resolve
    /// to RGB and the stoatty arm emits bars instead of glyphs.
    fn rgb_border_theme() -> crate::theme::Theme {
        let src = r##"theme rgbborder {
            ui.border.focused.fg = "#aabbcc";
            ui.border.inactive.fg = "#556677";
        }"##;
        let (config, _) = stoat_config::parse(src);
        crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbborder")
            .expect("rgb theme builds")
    }

    #[test]
    fn help_modal_emits_a_panel_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = rgb_modal_theme();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
        h.settle();

        let size = h.stoat.size();
        let mut buf = Buffer::empty(size);
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let modal = crate::render::help::help_layout(size)
            .expect("help modal fits the test viewport")
            .modal;
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                Command::Panel(p)
                    if p.top == modal.y
                        && p.left == modal.x
                        && p.width == modal.width
                        && p.height == modal.height
            )),
            "the help modal emits a panel at its layout rect, got {cmds:?}"
        );
    }

    #[test]
    fn help_modal_emits_a_panel_under_the_default_theme() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
        h.settle();

        let size = h.stoat.size();
        let mut buf = Buffer::empty(size);
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let modal = crate::render::help::help_layout(size)
            .expect("help modal fits the test viewport")
            .modal;
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                Command::Panel(p)
                    if p.top == modal.y
                        && p.left == modal.x
                        && p.width == modal.width
                        && p.height == modal.height
            )),
            "the shipped default theme resolves named colors to RGB, so the \
             help modal takes the rich arm and emits a panel, got {cmds:?}"
        );
    }

    #[test]
    fn help_separator_emits_a_hairline_bar_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = rgb_modal_theme();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
        h.settle();

        let size = h.stoat.size();
        let mut buf = Buffer::empty(size);
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let list = crate::render::help::help_layout(size)
            .expect("help modal fits the test viewport")
            .list;
        let sep_x = (list.x + list.width) as i16 * 16 + 8;
        let sep_y = list.y as i16 * 16;
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                Command::Bar(b)
                    if b.x == sep_x
                        && b.y == sep_y
                        && b.width == 1
                        && b.height == list.height * 16
            )),
            "the help list/detail separator emits a hairline bar, got {cmds:?}"
        );
    }

    #[test]
    fn hints_overlay_emits_scaled_text_runs_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = rgb_modal_theme();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
        h.settle();

        let mut buf = Buffer::empty(h.stoat.size());
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::TextRun(t) if t.scale == 218)),
            "the hints overlay emits 0.85x hint-row text runs, got {cmds:?}"
        );
    }

    #[test]
    fn hover_body_emits_scaled_text_runs_inside_stoatty() {
        use lsp_types::{HoverProviderCapability, ServerCapabilities};
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);
        h.fake_lsp().set_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..Default::default()
        });

        let root = std::path::PathBuf::from("/hover-apc");
        let path = root.join("main.rs");
        h.fake_fs().insert_file(&path, b"fn foo() {}\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "fn foo() -> u32");
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let mut buf = Buffer::empty(h.stoat.size());
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::TextRun(t) if t.scale == 218)),
            "the hover body emits 0.85x scaled text runs under stoatty, got {cmds:?}"
        );
    }

    #[test]
    fn pane_divider_emits_a_hairline_bar_inside_stoatty() {
        use crate::pane::DividerOrientation;
        use stoatty_protocol::command::{BarCommand, Command};

        let mut h = Stoat::test();
        h.stoat.theme = rgb_border_theme();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);

        let size = h.stoat.size();
        let mut buf = Buffer::empty(size);
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let dividers = h.stoat.active_workspace().panes.dividers();
        let d = dividers
            .iter()
            .find(|d| matches!(d.orientation, DividerOrientation::Vertical))
            .expect("the split has a vertical divider");
        let end_y = d.y.saturating_add(d.len).min(size.height);
        let expected = BarCommand {
            x: d.x as i16 * 16 + 8,
            y: d.y as i16 * 16,
            width: 1,
            height: (end_y - d.y) * 16,
            color: if d.touches_focus {
                [0xaa, 0xbb, 0xcc]
            } else {
                [0x55, 0x66, 0x77]
            },
        };
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.contains(&Command::Bar(expected)),
            "the split divider emits a hairline bar in the border color, got {cmds:?}"
        );
    }

    #[test]
    fn completion_popup_emits_a_panel_inside_stoatty() {
        use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = rgb_modal_theme();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/complete");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.type_keys("i");

        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![CompletionItem {
                label: "println".into(),
                source: CompletionSource::Lsp,
                kind: None,
                detail: None,
                replace_range: 0..0,
                insert_text: "println".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
                server: None,
            }],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..0,
        });

        let mut buf = Buffer::empty(h.stoat.size());
        h.stoat.paint_into(&mut buf);
        h.stoat.emit_apc_scene();

        let popup_area = crate::render::completion::completion_popup_layout(&mut h.stoat)
            .expect("completion popup lays out")
            .2
            .popup_area;
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                Command::Panel(p)
                    if p.top == popup_area.y
                        && p.left == popup_area.x
                        && p.width == popup_area.width
                        && p.height == popup_area.height
            )),
            "the completion popup emits a panel at its layout rect, got {cmds:?}"
        );
    }

    #[test]
    fn editor_pool_pages_fill_asynchronously() {
        use stoatty_protocol::command::{Command, FillCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/async-pool");
        let path = root.join("a.txt");
        let body = (0..150).map(|i| format!("line {i}\n")).collect::<String>();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        h.stoat.emit_smooth_scroll();

        // The first batch carries the pool geometry and scroll, but no editor fill:
        // plain editor pages are rendered off the run loop, not inline.
        let first = rx.try_recv().expect("region/scroll batch");
        let first_cmds = decode_apc_stream(&first);
        assert!(
            first_cmds
                .iter()
                .any(|c| matches!(c, Command::PoolRegion(_))),
            "first batch declares the pool, got {first_cmds:?}"
        );
        assert!(
            !first_cmds.iter().any(|c| matches!(c, Command::Fill(_))),
            "first batch carries no synchronous editor fill, got {first_cmds:?}"
        );

        // The blocking renders run inline under the test scheduler, so their fills
        // arrive as later batches on the same channel. The initial visible page is 0,
        // whose buffered window is pages 0..5.
        let mut filled = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            for cmd in decode_apc_stream(&batch) {
                if let Command::Fill(FillCommand { index, .. }) = cmd {
                    filled.push(index);
                }
            }
        }
        filled.sort_unstable();
        assert_eq!(
            filled,
            vec![0, 1, 2, 3, 4],
            "the initial window's pages fill asynchronously, got {filled:?}"
        );
    }

    #[test]
    fn review_pane_is_pooled_for_smooth_scroll() {
        use crate::test_harness::{TestHarness, REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER};
        use stoatty_protocol::command::Command;

        let mut h = TestHarness::with_size(80, 24);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        h.open_review_from_texts(&[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        h.stoat.emit_smooth_scroll();

        let bytes = rx.try_recv().expect("the review pane emits an APC batch");
        let cmds = decode_apc_stream(&bytes);
        assert!(
            cmds.iter().any(|cmd| matches!(cmd, Command::PoolRegion(_))),
            "a review split pane declares a smooth-scroll pool, got {cmds:?}"
        );
    }

    #[test]
    fn file_finder_list_is_pooled_and_retired() {
        use stoat_action::OpenFileFinder;
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/finder");
        for name in ["a.rs", "b.rs", "c.rs"] {
            h.fake_fs().insert_file(root.join(name), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        h.stoat.emit_smooth_scroll();
        let list = crate::render::file_finder::file_finder_layout(size)
            .expect("finder fits the test terminal")
            .list;
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::FINDER,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
        };
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "the finder list declares a pool at its list rect"
        );

        h.stoat.file_finder = None;
        h.stoat.emit_smooth_scroll();
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::FINDER,
            })),
            "closing the finder retires its pool"
        );
    }

    #[test]
    fn palette_list_is_pooled_and_retired() {
        use stoat_action::OpenCommandPalette;
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        action_handlers::dispatch(&mut h.stoat, &OpenCommandPalette);
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        h.stoat.emit_smooth_scroll();
        let list = crate::render::command_palette::palette_filter_layout(size)
            .expect("the palette fits the test terminal")
            .list;
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
        };
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "the palette list declares a pool at its list rect"
        );

        h.stoat.command_palette = None;
        h.stoat.emit_smooth_scroll();
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            })),
            "closing the palette retires its pool"
        );
    }

    #[test]
    fn palette_arg_list_is_pooled_and_retired() {
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/argpool");
        for name in ["a.rs", "b.rs", "c.rs"] {
            h.fake_fs().insert_file(root.join(name), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;
        // Typing `:o ` opens the palette and installs the Files arg picker. The
        // snapshot drives drive_background so the picker is live before emit.
        h.type_text(":o ");
        h.snapshot();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        let _ = drain_apc(&mut rx);

        h.stoat.emit_smooth_scroll();
        let list = crate::render::command_palette::palette_arg_list_rect(size)
            .expect("the arg picker fits the test terminal");
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
        };
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "the arg-picker list declares a pool at its list rect"
        );

        h.stoat.command_palette = None;
        h.stoat.emit_smooth_scroll();
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            })),
            "closing the palette retires its pool"
        );
    }

    #[test]
    fn palette_filter_to_arg_flip_repools() {
        use stoat_action::OpenCommandPalette;
        use stoatty_protocol::command::{Command, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/argflip");
        for name in ["a.rs", "b.rs"] {
            h.fake_fs().insert_file(root.join(name), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;

        // Filter mode holds the PALETTE pool through the command list.
        action_handlers::dispatch(&mut h.stoat, &OpenCommandPalette);
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        h.stoat.emit_smooth_scroll();
        let _ = drain_apc(&mut rx);

        // Flipping to arg mode re-declares the same pool at the arg-list rect.
        h.type_text("o ");
        h.snapshot();
        h.stoat.active_workspace_mut().layout(size);
        h.stoat.emit_smooth_scroll();

        let arg_list = crate::render::command_palette::palette_arg_list_rect(size)
            .expect("the arg picker fits the test terminal");
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            top: arg_list.y,
            left: arg_list.x,
            width: arg_list.width,
            height: arg_list.height,
        };
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "flipping filter to arg mode re-declares the pool at the arg-list rect"
        );
    }

    #[test]
    fn completion_popup_is_pooled_and_retired() {
        use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        let item = |label: &str| CompletionItem {
            label: label.into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range: 0..0,
            insert_text: label.into(),
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        };
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![item("alpha"), item("beta"), item("gamma")],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..0,
        });

        h.stoat.emit_smooth_scroll();
        let (_, _, layout) = crate::render::completion::completion_popup_layout(&mut h.stoat)
            .expect("the popup anchors in the test terminal");
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMPLETION,
            top: layout.inner.y,
            left: layout.inner.x,
            width: layout.inner.width,
            height: layout.inner.height,
        };
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "the completion popup declares a pool at its inner rect"
        );

        h.stoat.pending_completion = None;
        h.stoat.emit_smooth_scroll();
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::COMPLETION,
            })),
            "closing the popup retires its pool"
        );
    }

    #[test]
    fn help_list_and_detail_are_pooled_and_retired() {
        use stoat_action::OpenHelp;
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        action_handlers::dispatch(&mut h.stoat, &OpenHelp);
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        h.stoat.emit_smooth_scroll();
        let layout = crate::render::help::help_layout(size).expect("help fits the test terminal");
        let list = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::HELP_LIST,
            top: layout.list.y,
            left: layout.list.x,
            width: layout.list.width,
            height: layout.list.height,
        };
        let detail = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::HELP_DETAIL,
            top: layout.detail.y,
            left: layout.detail.x,
            width: layout.detail.width,
            height: layout.detail.height,
        };
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.contains(&Command::PoolRegion(list)),
            "the help list declares a pool at its rect"
        );
        assert!(
            cmds.contains(&Command::PoolRegion(detail)),
            "the help detail declares a pool at its rect"
        );

        h.stoat.help = None;
        h.stoat.emit_smooth_scroll();
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::HELP_LIST,
            })),
            "closing help retires the list pool"
        );
        assert!(
            cmds.contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::HELP_DETAIL,
            })),
            "closing help retires the detail pool"
        );
    }

    #[test]
    fn commits_list_is_pooled_and_retired() {
        use crate::commit_list::CommitListState;
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        h.stoat.active_workspace_mut().commits =
            Some(CommitListState::new(std::path::PathBuf::from("/work")));
        h.stoat.set_focused_mode("commits".to_string());

        h.stoat.emit_smooth_scroll();
        let focused = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let list = crate::render::commits::commits_list_rect(focused)
            .expect("the commits list fits the test terminal");
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMMITS,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
        };
        let bytes = rx
            .try_recv()
            .expect("the commits overlay emits an APC batch");
        assert!(
            decode_apc_stream(&bytes).contains(&Command::PoolRegion(expected)),
            "the commits list declares a pool at its list rect"
        );

        h.stoat.set_focused_mode("normal".to_string());
        h.stoat.active_workspace_mut().commits = None;
        h.stoat.emit_smooth_scroll();
        let bytes = rx.try_recv().expect("leaving commits emits a drop");
        assert!(
            decode_apc_stream(&bytes).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::COMMITS,
            })),
            "leaving commits mode retires its pool"
        );
    }

    #[test]
    fn emit_smooth_scroll_pushes_pool_region_then_scroll() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        // Lay the panes out so the focused editor has a non-zero rect.
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        h.stoat.emit_smooth_scroll();

        let bytes = rx.try_recv().expect("an APC batch was pushed");
        let cmds = decode_apc_stream(&bytes);
        assert!(
            matches!(cmds.first(), Some(Command::PoolRegion(_))),
            "first frame should declare the pool region, got {cmds:?}"
        );
        assert!(
            matches!(cmds.last(), Some(Command::Scroll(_))),
            "last frame should be the scroll target, got {cmds:?}"
        );
    }

    #[test]
    fn emit_smooth_scroll_glide_uses_the_eased_offset() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        let root = std::path::PathBuf::from("/pool");
        let path = root.join("a.txt");
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        // Simulate a page glide. scroll_row jumped to a distant row while
        // scroll_offset still lags near the top. The emit must carry the lagging
        // offset (its page is 0), not the target row's page, so the pool eases
        // up to it.
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.scroll_row = 50;
            editor.scroll_offset = 1.0;
            editor.scroll_glide = true;
        }

        h.stoat.emit_smooth_scroll();

        let bytes = rx.try_recv().expect("an APC batch was pushed");
        let cmds = decode_apc_stream(&bytes);
        let scroll = cmds
            .iter()
            .find_map(|c| match c {
                Command::Scroll(s) => Some(*s),
                _ => None,
            })
            .expect("a scroll command");
        assert_eq!(
            scroll.page, 0,
            "a glide emits the eased offset's page (1.0 -> page 0), not scroll_row 50's page"
        );
    }

    #[test]
    fn emit_smooth_scroll_is_noop_outside_stoatty() {
        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(false, tx);

        let root = std::path::PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        h.stoat.emit_smooth_scroll();
        assert!(rx.try_recv().is_err(), "no APC bytes outside stoatty");
    }

    /// Decode the sequence of stoatty commands in `bytes`, skipping the raw page
    /// VT that rides between `fill`/`fill_end` markers.
    fn decode_apc_stream(bytes: &[u8]) -> Vec<stoatty_protocol::command::Command> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while let Some(start) = rest.windows(2).position(|w| w == b"\x1b_") {
            let after = &rest[start..];
            let Some(end) = after.windows(2).position(|w| w == b"\x1b\\") else {
                break;
            };
            if let Some(cmd) = stoatty_protocol::command::decode(&after[..end + 2]) {
                out.push(cmd);
            }
            rest = &after[end + 2..];
        }
        out
    }

    /// Drain every APC batch currently queued on `rx` into one decoded command
    /// list. A plain editor pane fills its pages asynchronously, so one
    /// `emit_smooth_scroll` pushes the region/scroll batch plus a fill batch per
    /// page. Draining folds them together so a test reads the whole emit at once.
    fn drain_apc(rx: &mut UnboundedReceiver<Vec<u8>>) -> Vec<stoatty_protocol::command::Command> {
        let mut cmds = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            cmds.extend(decode_apc_stream(&batch));
        }
        cmds
    }

    #[test]
    fn mouse_routes_to_focused_dock_when_focus_is_dock() {
        use crate::pane::{DockPanel, DockSide, DockVisibility, View};
        let mut h = Stoat::test();
        let dock_id = h.stoat.active_workspace_mut().docks.insert(DockPanel {
            view: View::Label("dock".into()),
            side: DockSide::Right,
            visibility: DockVisibility::Open { width: 30 },
            default_width: 30,
            area: Rect::new(50, 0, 30, 24),
        });
        h.stoat.active_workspace_mut().focus = FocusTarget::Dock(dock_id);
        let translated = h.stoat.translate_mouse_to_focused(60, 7);
        assert_eq!(translated, Some((10, 7)));
    }

    #[test]
    fn mouse_returns_none_when_focused_dock_missing() {
        use crate::pane::DockId;
        let mut h = Stoat::test();
        let dangling = DockId::default();
        h.stoat.active_workspace_mut().focus = FocusTarget::Dock(dangling);
        let translated = h.stoat.translate_mouse_to_focused(10, 10);
        assert_eq!(translated, None);
    }

    #[test]
    fn snapshot_lsp_progress_indexing() {
        use crate::host::LspNotification;
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
        h.assert_snapshot("lsp_progress_indexing");
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
    fn lsp_spawn_failure_surfaces_in_message_row() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());
        stoat
            .pending_lsp_host
            .lock()
            .expect("pending lsp host mutex")
            .push(PendingSpawn {
                server: "rust-analyzer".to_string(),
                language: "rust".to_string(),
                result: Err("rust-analyzer: NotFound".to_string()),
            });

        stoat.install_pending_lsp_host();

        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("lsp: rust-analyzer: NotFound")
        );
        assert!(
            stoat.lsp_host().is_noop(),
            "the placeholder stays after a spawn failure"
        );
    }

    #[test]
    fn lsp_ready_host_installs_without_a_message() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());
        let host: Arc<dyn LspHost> = Arc::new(crate::host::FakeLsp::new());
        stoat
            .pending_lsp_host
            .lock()
            .expect("pending lsp host mutex")
            .push(PendingSpawn {
                server: "rust-analyzer".to_string(),
                language: "rust".to_string(),
                result: Ok(host),
            });

        stoat.install_pending_lsp_host();

        assert!(
            !stoat.lsp_host().is_noop(),
            "a ready host replaces the placeholder"
        );
        assert_eq!(
            stoat.pending_message, None,
            "a successful install shows no message"
        );
    }

    #[test]
    fn user_config_overrides_embedded_setting() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let stoat = Stoat::new_with_user_config(
            scheduler.executor(),
            Settings::default(),
            PathBuf::new(),
            Some("on init { format_on_save = true; }".to_string()),
        );

        assert_eq!(stoat.settings.format_on_save, Some(true));
        assert_eq!(
            stoat.pending_message, None,
            "a clean parse shows no message"
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
            Some((MessageType::INFO, "checking".to_string())),
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
    fn diagnostics_notification_with_non_file_uri_dropped() {
        use crate::host::LspNotification;
        use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Uri};
        use std::str::FromStr;
        let mut h = Stoat::test();
        let uri = Uri::from_str("https://example.com/a.rs").unwrap();
        let diag = Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 5)),
            severity: Some(DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: None,
            message: "ignored".into(),
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
        let summary = h
            .stoat
            .diagnostics
            .summarize(std::path::Path::new("/ws/a.rs"));
        assert!(summary.is_empty());
    }

    #[test]
    fn incoming_apply_edit_mutates_buffer_and_replies_applied() {
        use crate::host::lsp::IncomingRequest;
        use lsp_types::{
            ApplyWorkspaceEditParams, ApplyWorkspaceEditResponse, DocumentChanges, NumberOrString,
            OneOf, OptionalVersionedTextDocumentIdentifier, Position, Range, TextDocumentEdit,
            TextEdit, WorkspaceEdit,
        };
        use std::path::PathBuf;

        let mut h = Stoat::test();
        let path = PathBuf::from("/ws/a.rs");
        h.fake_fs().insert_file(&path, b"abcde\n");
        h.stoat
            .active_workspace_mut()
            .buffers
            .open(&path, "abcde\n");

        let uri = action_handlers::lsp::path_to_uri(&path).expect("uri");
        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
                edits: vec![OneOf::Left(TextEdit {
                    range: Range::new(Position::new(0, 1), Position::new(0, 4)),
                    new_text: "X".to_string(),
                })],
            }])),
            change_annotations: None,
        };
        let id = NumberOrString::Number(7);
        h.fake_lsp()
            .push_incoming_request(IncomingRequest::WorkspaceApplyEdit {
                id: id.clone(),
                params: ApplyWorkspaceEditParams { label: None, edit },
            });
        h.stoat.drain_lsp_incoming_requests();
        h.settle();

        let buffer_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&path)
            .expect("buffer");
        let text = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .unwrap()
            .read()
            .unwrap()
            .rope()
            .to_string();
        assert_eq!(text, "aXe\n");

        let applied = serde_json::to_value(ApplyWorkspaceEditResponse {
            applied: true,
            failure_reason: None,
            failed_change: None,
        })
        .unwrap();
        assert_eq!(h.fake_lsp().observed_replies(), vec![(id, Ok(applied))]);
    }

    #[test]
    fn incoming_configuration_replies_null_per_item() {
        use crate::host::lsp::IncomingRequest;
        use lsp_types::{ConfigurationItem, ConfigurationParams, NumberOrString};

        let mut h = Stoat::test();
        let id = NumberOrString::Number(8);
        let item = |section: &str| ConfigurationItem {
            scope_uri: None,
            section: Some(section.to_string()),
        };
        h.fake_lsp()
            .push_incoming_request(IncomingRequest::WorkspaceConfiguration {
                id: id.clone(),
                params: ConfigurationParams {
                    items: vec![item("a"), item("b")],
                },
            });
        h.stoat.drain_lsp_incoming_requests();
        h.settle();

        let nulls = serde_json::Value::Array(vec![serde_json::Value::Null; 2]);
        assert_eq!(h.fake_lsp().observed_replies(), vec![(id, Ok(nulls))]);
    }

    #[test]
    fn incoming_unknown_request_replies_method_not_found() {
        use crate::host::lsp::{IncomingRequest, LspResponseError};
        use lsp_types::NumberOrString;

        let mut h = Stoat::test();
        let id = NumberOrString::Number(9);
        h.fake_lsp()
            .push_incoming_request(IncomingRequest::Unknown {
                id: id.clone(),
                method: "experimental/foo".to_string(),
                params: serde_json::Value::Null,
            });
        h.stoat.drain_lsp_incoming_requests();
        h.settle();

        assert_eq!(
            h.fake_lsp().observed_replies(),
            vec![(
                id,
                Err(LspResponseError {
                    code: -32601,
                    message: "method not found".to_string(),
                    data: None,
                }),
            )],
        );
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
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(16);
        for size in [(80u16, 24u16), (100, 30), (120, 40)] {
            tx.try_send(Event::Resize(size.0, size.1)).unwrap();
        }
        let (effect, coalesced) = h.stoat.drain_pending(&mut rx);
        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(coalesced, 3, "all three queued events counted");
        assert_eq!(h.stoat.size(), Rect::new(0, 0, 120, 40));
        assert!(rx.try_recv().is_err(), "drain must empty the channel");
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
        // A non-home cwd keeps abbreviate_path deterministic across hosts.
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

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn diff_view_click_maps_into_the_right_column() {
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
                base,
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
        h.stoat.active_workspace_mut().editors[editor_id].set_diff_view(true);

        let area = Rect::new(0, 0, 40, 10);
        // For width 40 the right text begins at col 26, so col 28 row 0 is the
        // right column's 2nd character of the context line "keep".
        assert_eq!(
            h.stoat.editor_screen_to_offset(editor_id, area, 28, 0),
            Some(2),
            "a click in the right column lands on the buffer character"
        );
        assert_eq!(
            h.stoat.editor_screen_to_offset(editor_id, area, 8, 0),
            Some(0),
            "a click left of the right column clamps to the buffer line start"
        );
    }

    fn open_with_minimap_strip(h: &mut crate::test_harness::TestHarness) -> EditorId {
        let body: String = (0..60)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        open_scratch_file(h, &body);
        let editor_id = h.stoat.focused_editor_ids().expect("editor").0;
        let editor = &mut h.stoat.active_workspace_mut().editors[editor_id];
        editor.minimap_rect = Some(Rect::new(72, 0, 8, 10));
        editor.viewport_rows = Some(20);
        editor_id
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
        assert!(editor.scroll_glide, "the scrub glides like a page motion");
        assert_eq!(
            h.stoat.minimap_drag,
            Some(editor_id),
            "the press arms the scrub"
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

    #[test]
    fn mouse_focus_change_closes_the_hover_popup() {
        use crate::action_handlers::lsp::HoverPopup;
        use ratatui::style::Style;

        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        let left = ws.panes.focus();
        let right = ws.panes.split(crate::pane::Axis::Vertical);
        ws.panes.resize(Rect::new(0, 0, 101, 24));
        let left_area = h.stoat.active_workspace().panes.pane(left).area;
        let right_area = h.stoat.active_workspace().panes.pane(right).area;

        // Focus the right pane through the real path so ws.focus tracks it, then
        // open a popup there. Its empty area makes any click land outside it.
        h.stoat.focus_at(right_area.x + 1, right_area.y + 1);
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        h.stoat.pending_hover = Some(HoverPopup {
            lines: vec![vec![("hi".to_string(), Style::default())]],
            anchor_offset: 0,
            editor_id,
            scroll_half_pages: 0,
            area: Rect::default(),
            inner: Rect::default(),
            selection: None,
        });

        // A left-button Down over the other pane moves focus and closes it.
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            left_area.x + 1,
            left_area.y + 1,
        ));
        assert!(
            h.stoat.pending_hover.is_none(),
            "a mouse focus change closes the hover popup"
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

    fn open_scratch_file(h: &mut crate::test_harness::TestHarness, contents: &str) -> PathBuf {
        let path = PathBuf::from("/ws/buf.txt");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/ws");
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        path
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
            .set_single_range(start, end, stoat_text::SelectionGoal::None);
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
            .insert_cursor(head, stoat_text::SelectionGoal::None, buf);
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

            let (event_tx, event_rx) = tokio::sync::mpsc::channel::<Event>(64);
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

    #[test]
    fn shutdown_lsp_reaps_the_server_on_quit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let h = Stoat::test();
            assert!(!h.fake_lsp().was_shut_down());
            h.stoat.shutdown_lsp().await;
            assert!(
                h.fake_lsp().was_shut_down(),
                "the quit teardown shuts the language server down",
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

            let (event_tx, event_rx) = tokio::sync::mpsc::channel::<Event>(64);
            let (render_tx, render_rx) = watch::channel(None);
            // Queue one input event and drop the sender. The loop drains the
            // event (publishing its frame), then breaks on the closed channel
            // before any background redraw can supersede it in the watch.
            event_tx.send(Event::Resize(80, 24)).await.expect("send");
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

            let (event_tx, event_rx) = tokio::sync::mpsc::channel::<Event>(64);
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
    fn esc_in_insert_with_open_popup_clears_popup_and_keeps_mode() {
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
        });
        h.type_keys("escape");
        assert_eq!(h.stoat.pending_completion, None);
        assert_eq!(h.stoat.focused_mode(), "insert");
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
        });
        h.type_keys("tab");
        assert!(h.stoat.active_snippet.is_some());

        h.type_keys("escape");
        h.type_keys("escape");
        assert_eq!(h.stoat.focused_mode(), "normal");
        assert!(h.stoat.active_snippet.is_none());
    }

    fn focused_editor_pane_area(h: &crate::test_harness::TestHarness) -> Rect {
        let ws = h.stoat.active_workspace();
        match ws.focus {
            FocusTarget::SplitPane(pane_id) => ws.panes.pane(pane_id).area,
            FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).expect("dock").area,
        }
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
    fn editor_page_content_version_tracks_the_cursor_line() {
        let base = editor_page_content_version(true, 3, Some(10), 0, false, 0);
        assert_eq!(
            base,
            editor_page_content_version(true, 3, Some(10), 0, false, 0),
            "identical inputs keep a buffered page cached"
        );
        assert_ne!(
            base,
            editor_page_content_version(true, 3, Some(11), 0, false, 0),
            "a cursor-line move refills buffered pages"
        );
        assert_ne!(
            base,
            editor_page_content_version(true, 3, None, 0, false, 0),
            "switching to absolute numbering refills"
        );
        assert_ne!(
            base,
            editor_page_content_version(true, 3, Some(10), 0, true, 7),
            "a diff-view hunk change refills buffered pages"
        );
    }

    #[test]
    fn editor_mouse_down_collapses_cursor_at_clicked_offset() {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "abcdef\nghi");
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 3,
            area.y,
        ));
        assert_eq!(focused_primary_offsets(&mut h), (3, 3));
        assert!(h.stoat.editor_drag.is_some(), "drag state armed");
    }

    #[test]
    fn editor_click_excludes_the_diagnostic_gutter() {
        // The no-gutter path (no render leaves gutter_width zero) is covered by
        // editor_mouse_down_collapses_cursor_at_clicked_offset above.
        let mut h = Stoat::test();
        let root = PathBuf::from("/gutter-click");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"abcdef\nghi\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.stoat.diagnostics.replace_for_path(
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
            (2, 2),
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
