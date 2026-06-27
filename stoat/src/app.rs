use crate::{
    action_handlers,
    agent_ipc::{AgentControl, AgentEvent},
    agent_session::AgentId,
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
        EnvHost, FsHost, FsWatchHost, GitHost, LocalEnv, LocalFs, LocalGit, LspHost, NoopFsWatcher,
        NoopLsp,
    },
    keymap::{Keymap, ResolvedAction},
    keymap_state::{normalize_shift_event, resolve_action, StoatKeymapState},
    pane::{FocusTarget, Placement, View},
    quit_all_confirm::{ConfirmOutcome, QuitAllConfirm},
    rebase::RebasePause,
    register,
    run::{GridSelection, PtyNotification, RunId},
    workspace::{Workspace, WorkspaceId, WorkspaceUid},
    workspace_picker::{PickerOutcome, WorkspacePicker},
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
use stoat_action::{OpenFile, OpenReview, ReviewExternalEdit};
use stoat_config::Settings;
use stoat_language::{self as language, Language, LanguageRegistry, SyntaxState};
use stoat_scheduler::Executor;
use stoat_text::Bias;
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

pub struct Stoat {
    size: Rect,
    pub mode: String,
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
    /// In-flight working-tree review scan. The git2 diff runs on a
    /// blocking thread; [`pump_review_scan`](action_handlers::pump_review_scan)
    /// polls the ready [`ReviewSession`](crate::review_session::ReviewSession)
    /// off this task and installs it on the main loop, so opening a review
    /// never stalls input on the scan.
    pub(crate) pending_review_scan: Option<action_handlers::PendingReviewScan>,
    pub(crate) modal_run: Option<RunId>,
    pub(crate) render_tick: u64,
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
    pub(crate) marks: std::collections::HashMap<(BufferId, char), stoat_text::Anchor>,
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
    /// [`NoopLsp`] (every method returns the empty success response)
    /// until a real `LocalLsp` is wired in; tests install
    /// [`crate::host::FakeLsp`] to drive end-to-end LSP scenarios.
    pub(crate) lsp_host: Arc<dyn LspHost>,
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
    /// Tracks `$/progress` notifications so the status bar can show
    /// the freshest in-progress operation. Drained from
    /// [`crate::host::LspHost::try_recv_notification`] inside
    /// [`Stoat::update`].
    pub(crate) lsp_progress: crate::lsp::progress::LspProgressMap,
    /// In-flight goto-style LSP request (definition / type definition
    /// / implementation / declaration). Replacing the entry drops the
    /// prior task, cancelling its spawned future before the response
    /// can land. Polled by [`action_handlers::pump_lsp_jumps`] at the
    /// top of each render tick; `Ready(Some)` opens the target file
    /// in the focused pane (when cross-file) and jumps the primary
    /// cursor; `Ready(None)` silently drops.
    pub(crate) pending_lsp_jump:
        Option<stoat_scheduler::Task<Option<action_handlers::lsp::JumpTarget>>>,

    /// In-flight `textDocument/hover` request. Replacing the entry
    /// drops the prior task, cancelling its spawned future before the
    /// response can land. Polled by
    /// [`action_handlers::pump_lsp_hover`] at the top of each render
    /// tick; `Ready(Some)` writes the response to [`Self::pending_hover`].
    pub(crate) pending_hover_request:
        Option<stoat_scheduler::Task<Option<action_handlers::lsp::HoverResponse>>>,

    /// Hover popup content waiting to be painted. Set by
    /// [`action_handlers::pump_lsp_hover`] when a hover response lands;
    /// cleared by [`Self::dispatch_key`] on any non-Hover action so the
    /// popup vanishes on cursor motion.
    pub(crate) pending_hover: Option<action_handlers::lsp::HoverPopup>,

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
    /// Smooth-scroll pool emit state for the focused editor. Tracks the
    /// last-declared pool region, filled page window, and emitted scroll row
    /// so each frame emits only the deltas.
    pub(crate) smooth_scroll: crate::smooth_scroll::SmoothScrollState,
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
        let state = StoatKeymapState::new(&self.mode);
        self.keymap
            .active_bindings(&state)
            .into_iter()
            .map(|(label, actions)| (label, actions.to_vec()))
            .collect()
    }

    pub fn new(executor: Executor, cli_settings: Settings, initial_git_root: PathBuf) -> Self {
        let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
        if !errors.is_empty() {
            tracing::error!(
                "default keymap parse errors: {}",
                stoat_config::format_errors(DEFAULT_KEYMAP, &errors)
            );
        }
        let settings = config
            .as_ref()
            .map(Settings::from_config)
            .unwrap_or_default()
            .merge(cli_settings);

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

        let keymap = config.map(|c| Keymap::compile(&c)).unwrap_or_else(|| {
            Keymap::compile(&stoat_config::Config {
                blocks: vec![],
                themes: vec![],
            })
        });

        let syntax_styles = SyntaxStyles::from_theme(&theme);
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
        let (index_external_edit_tx, index_external_edit_rx) = tokio::sync::mpsc::channel(256);

        Self {
            size: Rect::default(),
            mode: "normal".into(),
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
            pending_review_scan: None,
            modal_run: None,
            render_tick: 0,
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
            registers: register::RegisterStore::new(),
            pending_register_select: false,
            selected_register: None,
            pending_insert_register: false,
            editor_drag: None,
            lsp_opened: std::collections::HashSet::new(),
            lsp_buffer_versions: std::collections::HashMap::new(),
            lsp_pending_changes: std::collections::HashMap::new(),
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
            index_pending_external_edits: std::collections::HashMap::new(),
            index_external_edit_tx,
            index_external_edit_rx,
            git_host: Arc::new(LocalGit::new()),
            env_host: Arc::new(LocalEnv),
            lsp_host: Arc::new(NoopLsp),
            clipboard_host: Arc::new(crate::host::NoopClipboard),
            diff_cache: Arc::new(std::sync::Mutex::new(crate::diff_cache::DiffCache::new(
                256,
            ))),
            lsp_progress: crate::lsp::progress::LspProgressMap::new(),
            pending_lsp_jump: None,
            pending_hover_request: None,
            pending_hover: None,
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
            pending_completion: None,
            pending_completion_request: None,
            last_completion_signature: None,
            active_snippet: None,
            stoatty: false,
            apc_tx: None,
            smooth_scroll: crate::smooth_scroll::SmoothScrollState::default(),
        }
    }

    /// Look up a previously-cached diff by content hashes plus
    /// language. Returns the serialized hunk payload on cache hit, or
    /// `None` on miss. Called by the viewport-socket diff RPC handler
    /// to translate `ToMain::DiffRequest` into `ToViewport::DiffResponse`.
    pub fn handle_diff_lookup(&self, key: &crate::diff_cache::DiffCacheKey) -> Option<Vec<u8>> {
        let mut cache = self.diff_cache.lock().expect("diff_cache poisoned");
        let hunks = cache.lookup(key)?;
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
        self.lsp_host = host;
    }

    /// Returns the active [`LspHost`].
    pub fn lsp_host(&self) -> &Arc<dyn LspHost> {
        &self.lsp_host
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

    pub fn open_review(&mut self) {
        action_handlers::dispatch(self, &OpenReview);
    }

    pub async fn run(
        &mut self,
        mut events: Receiver<Event>,
        render: watch::Sender<Option<Buffer>>,
    ) -> io::Result<()> {
        let mut frame_buf = Buffer::empty(self.size);
        self.start_index_build();
        loop {
            let first = tokio::select! {
                biased;
                event = events.recv() => {
                    let Some(event) = event else { break };
                    self.update(event)
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
            };

            let effect = first.merge(self.drain_pending(&mut events));

            match effect {
                UpdateEffect::Redraw => {
                    self.drive_background();
                    self.paint_into(&mut frame_buf);
                    render.send_modify(|slot| match slot {
                        Some(buf) => {
                            buf.area = frame_buf.area;
                            buf.content.clone_from(&frame_buf.content);
                        },
                        None => *slot = Some(frame_buf.clone()),
                    });
                    self.emit_smooth_scroll();
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
        Ok(())
    }

    /// Apply every message already queued on the input and notification
    /// channels without blocking, returning their combined [`UpdateEffect`].
    ///
    /// Called after [`Self::run`] wakes on its first message so a burst
    /// collapses into a single render instead of one render per message. A
    /// paste's worth of keystrokes or a flood of PTY notifications all apply
    /// before that one render.
    ///
    /// Each channel is drained only to its currently-queued extent. Messages
    /// that arrive mid-drain are handled on the next loop iteration, which
    /// keeps render forward-progress under a sustained producer.
    fn drain_pending(&mut self, events: &mut Receiver<Event>) -> UpdateEffect {
        let mut effect = UpdateEffect::None;

        while let Ok(event) = events.try_recv() {
            effect = effect.merge(self.update(event));
        }
        while let Ok(notif) = self.pty_rx.try_recv() {
            effect = effect.merge(self.handle_pty_notification(notif));
        }
        while let Ok(ev) = self.agent_event_rx.try_recv() {
            effect = effect.merge(self.handle_agent_event(ev));
        }
        while let Ok(ctl) = self.agent_control_rx.try_recv() {
            effect = effect.merge(self.handle_agent_control(ctl));
        }
        self.drain_index_updates();

        effect
    }

    /// Kick off a background cold build of the active workspace's code index.
    ///
    /// The scan runs on the blocking pool and streams shards back through
    /// [`Self::index_update_rx`], which [`Self::drain_index_updates`] merges
    /// each tick. The worker task is held so the scan is not cancelled.
    pub(crate) fn start_index_build(&mut self) {
        let workspace = self.active_workspace;
        let git_root = self.active_workspace().git_root.clone();
        let warm = self.warm_index_load(&git_root);
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

    /// Merge any pending cold-build shards into their workspace graphs.
    ///
    /// Each shard is inserted and, in non-test runs, written to disk. On
    /// [`IndexUpdate::Complete`] the workspace's cross-file references are
    /// re-resolved and the manifest is persisted.
    fn drain_index_updates(&mut self) {
        while let Ok(update) = self.index_update_rx.try_recv() {
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
                    if let Some(ws) = self.workspaces.get_mut(workspace) {
                        ws.code_graph.reresolve_unresolved();
                    }
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
                    ws.code_graph.reindex(file, shard);
                    ws.file_paths.insert(file, PathBuf::from(&rel_path));
                    ws.index_generation += 1;
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
                    ws.code_graph.evict_file(file);
                    ws.code_graph.reresolve_unresolved();
                    ws.file_paths.remove(&file);
                    ws.index_generation += 1;
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
        if let Err(err) = self
            .active_workspace_mut()
            .restore_state(&path, &*fs_host, &executor)
        {
            tracing::warn!(
                ?path,
                ?err,
                "failed to restore workspace state; starting fresh"
            );
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
        self.drain_fs_watch_events();
        self.drain_pending_external_edits();
        self.drain_pending_index_edits();
        let effect = match event {
            Event::Resize(w, h) => {
                self.size = Rect::new(0, 0, w, h);
                let size = self.size;
                self.active_workspace_mut().layout(size);
                UpdateEffect::Redraw
            },
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            _ => UpdateEffect::None,
        };
        action_handlers::lsp::notify_buffer_changes_pending(self);
        crate::completion::request::trigger(self);
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
        let review_active = self.active_workspace().review.is_some();
        let git_root = self.active_workspace().git_root.clone();
        for path in paths {
            if review_active {
                self.arm_review_external_edit_debounce(path.clone());
            }
            if path.starts_with(&git_root) && self.language_registry.for_path(&path).is_some() {
                self.arm_index_external_edit_debounce(path);
            }
        }
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
        use crate::host::LspNotification;
        use futures::FutureExt;
        let host = self.lsp_host.clone();
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
            if self.lsp_progress.update(&notification) {
                continue;
            }
            match &notification {
                LspNotification::Diagnostics {
                    uri, diagnostics, ..
                } => {
                    if let Some(path) = lsp_uri_to_path(uri) {
                        self.diagnostics.replace_for_path(path, diagnostics.clone());
                    } else {
                        tracing::debug!(
                            target: "stoat::app",
                            uri = uri.as_str(),
                            "diagnostics arrived for non-file URI; dropped",
                        );
                    }
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

    fn handle_mouse(&mut self, mouse: MouseEvent) -> UpdateEffect {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
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

    /// Routes left-button Down/Drag/Up events on a focused run pane into
    /// the active block's [`GridSelection`]. Returns `true` when the event
    /// mutated state. `Up(Left)` finalises the drag by extracting the
    /// row-major selection text and pushing it to the
    /// [`crate::host::ClipboardHost`]; the selection itself persists in
    /// place. Click-without-drag (`anchor == head`) is a no-op.
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
                if let Err(err) = clipboard_host.set(&text) {
                    tracing::warn!(
                        target: "stoat::app",
                        error = %err,
                        "clipboard write failed"
                    );
                }
                if crate::host::osc52_should_emit(env_host.as_ref())
                    && let Err(err) = clipboard_host.osc52_emit(&text)
                {
                    tracing::warn!(
                        target: "stoat::app",
                        error = %err,
                        "OSC 52 emit failed"
                    );
                }
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
    fn handle_editor_pane_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) -> bool {
        let target = {
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
        };
        let Some((editor_id, area)) = target else {
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
                if let Err(err) = clipboard_host.set(&text) {
                    tracing::warn!(
                        target: "stoat::app",
                        error = %err,
                        "clipboard write failed"
                    );
                }
                if crate::host::osc52_should_emit(env_host.as_ref())
                    && let Err(err) = clipboard_host.osc52_emit(&text)
                {
                    tracing::warn!(
                        target: "stoat::app",
                        error = %err,
                        "OSC 52 emit failed"
                    );
                }
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
        let display_row = editor.scroll_row + row as u32;
        let display_col = col as u32;
        let snapshot = editor.display_map.snapshot();
        let raw = crate::display_map::DisplayPoint::new(display_row, display_col);
        let clipped = snapshot.clip_point(raw, Bias::Left);
        let buffer_pt = snapshot.display_to_buffer(clipped)?;
        Some(snapshot.buffer_snapshot().rope().point_to_offset(buffer_pt))
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
        let ws = self.active_workspace_mut();
        ws.focus = target;
        if let FocusTarget::SplitPane(id) = target {
            ws.panes.set_focus(id);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> UpdateEffect {
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
                self.mode = palette.previous_mode;
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
            if let Some(picker) = self.jumplist_picker.take() {
                self.mode = picker.previous_mode;
                return UpdateEffect::Redraw;
            }
            if let Some(picker) = self.diagnostics_picker.take() {
                self.mode = picker.previous_mode;
                return UpdateEffect::Redraw;
            }
            if let Some(picker) = self.global_search.take() {
                self.mode = picker.previous_mode;
                return UpdateEffect::Redraw;
            }
            if self.mode == "run" {
                return action_handlers::dispatch(self, &stoat_action::RunInterrupt);
            }
            if let Some(agent_id) = self.agent_input_target() {
                self.write_to_agent(agent_id, &[0x03]);
                return UpdateEffect::None;
            }
            return UpdateEffect::Quit;
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
            let finished = self
                .active_workspace()
                .runs
                .get(run_id)
                .is_some_and(|r| !r.is_running());
            if finished && key.code == KeyCode::Esc {
                self.active_workspace_mut().runs.remove(run_id);
                self.modal_run = None;
                return UpdateEffect::Redraw;
            }
            return UpdateEffect::None;
        }

        if self.workspace_picker.is_some() {
            return self.dispatch_workspace_picker_key(key);
        }

        if self.quit_all_confirm.is_some() {
            return self.dispatch_quit_all_confirm_key(key);
        }

        if self.jumplist_picker.is_some() {
            return self.dispatch_jumplist_picker_key(key);
        }

        if self.diagnostics_picker.is_some() {
            return self.dispatch_diagnostics_picker_key(key);
        }

        if self.global_search.is_some() {
            return self.dispatch_global_search_key(key);
        }

        if let Some(agent_id) = self.agent_input_target() {
            return self.route_key_to_agent(agent_id, key);
        }

        if (self.mode == "insert"
            || self.mode == "reword_insert"
            || self.mode == "prompt"
            || self.mode == "run")
            && let Some(effect) = self.handle_insert_key(key)
        {
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

        if (self.mode == "normal" || self.mode == "select")
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

        if (self.mode == "normal" || self.mode == "select") && self.pending_symbol_picker.is_some()
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

        if (self.mode == "normal" || self.mode == "select")
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

        if (self.mode == "normal" || self.mode == "select") && self.pending_find.is_some() {
            if let KeyCode::Char(ch) = key.code {
                let (kind, extend, count) = self.pending_find.take().expect("checked above");
                return action_handlers::movement::execute_find(self, kind, ch, extend, count);
            }
            self.pending_find = None;
        }

        if self.mode == "normal" && self.pending_mark.is_some() {
            if let KeyCode::Char(ch) = key.code {
                let request = self.pending_mark.take().expect("checked above");
                return action_handlers::marks::execute_mark(self, request, ch);
            }
            self.pending_mark = None;
        }

        if (self.mode == "normal" || self.mode == "select") && self.pending_replace {
            if let KeyCode::Char(ch) = key.code {
                self.pending_replace = false;
                return action_handlers::movement::execute_replace(self, ch);
            }
            self.pending_replace = false;
        }

        if (self.mode == "normal" || self.mode == "select") && self.pending_surround_add {
            if let KeyCode::Char(ch) = key.code {
                self.pending_surround_add = false;
                return action_handlers::surround::execute_surround_add(self, ch);
            }
            self.pending_surround_add = false;
        }

        if (self.mode == "normal" || self.mode == "select") && self.pending_register_select {
            if let KeyCode::Char(ch) = key.code {
                self.pending_register_select = false;
                action_handlers::yank::execute_select_register(self, ch);
                return UpdateEffect::Redraw;
            }
            self.pending_register_select = false;
        }

        if (self.mode == "normal" || self.mode == "select")
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

        if (self.mode == "normal" || self.mode == "select") && self.pending_surround_delete {
            if let KeyCode::Char(ch) = key.code {
                self.pending_surround_delete = false;
                return action_handlers::surround::execute_surround_delete(self, ch);
            }
            self.pending_surround_delete = false;
        }

        if (self.mode == "normal" || self.mode == "select")
            && self.pending_textobject_select.is_some()
        {
            if let KeyCode::Char(ch) = key.code {
                let mode = self.pending_textobject_select.expect("checked above");
                self.pending_textobject_select = None;
                return action_handlers::textobject::execute_select_textobject(self, mode, ch);
            }
            self.pending_textobject_select = None;
        }

        if (self.mode == "normal" || self.mode == "select") && self.pending_goto_word.is_some() {
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

        let count_active_mode = self.mode == "normal" || self.mode == "select";
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
    /// `Some` only in insert mode with a focused `View::Agent` split pane. This
    /// mirrors how insert mode sends typing to the focused editor, except the
    /// bytes go to the agent's PTY. Normal mode keeps its editor and
    /// pane-navigation bindings, so the user enters the agent with `i` and
    /// leaves via the [`Self::route_key_to_agent`] escape.
    fn agent_input_target(&self) -> Option<AgentId> {
        if self.mode != "insert" {
            return None;
        }

        let ws = self.active_workspace();
        let FocusTarget::SplitPane(_) = ws.focus else {
            return None;
        };
        match &ws.panes.pane(ws.panes.focus()).view {
            View::Agent(id) => Some(*id),
            _ => None,
        }
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
    fn route_key_to_agent(&mut self, agent_id: AgentId, key: KeyEvent) -> UpdateEffect {
        if key.code == KeyCode::Esc {
            self.transition_mode("normal".to_string());
            return UpdateEffect::Redraw;
        }

        if let Some(bytes) = encode_key_to_pty(&key) {
            self.write_to_agent(agent_id, &bytes);
        }
        UpdateEffect::None
    }

    /// Write raw bytes to an agent's PTY.
    ///
    /// Uses `now_or_never` because the local PTY and the test fake complete
    /// writes synchronously, so keystrokes reach the agent in order without
    /// spawning a task. A write that errors or cannot complete synchronously is
    /// dropped with a warning rather than stalling input.
    fn write_to_agent(&self, agent_id: AgentId, bytes: &[u8]) {
        let Some(session) = self
            .active_workspace()
            .agents
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

    fn handle_insert_key(&mut self, key: KeyEvent) -> Option<UpdateEffect> {
        let (editor_id, buffer_id) = self.focused_editor_ids()?;

        if self.pending_insert_register {
            self.pending_insert_register = false;
            if let KeyCode::Char(ch) = key.code
                && let Some(register) = action_handlers::yank::register_for_char(ch)
                && let Some(content) = action_handlers::yank::read_register_content(self, register)
            {
                self.editor_insert(editor_id, buffer_id, &content);
            }
            return Some(UpdateEffect::Redraw);
        }

        match key.code {
            KeyCode::Char('w') if key.modifiers == KeyModifiers::CONTROL => {
                self.editor_delete_word_backward(editor_id, buffer_id);
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
                if self.mode == "prompt" || self.mode == "run" {
                    None
                } else {
                    self.editor_insert(editor_id, buffer_id, "\n");
                    Some(UpdateEffect::Redraw)
                }
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
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Down if self.pending_completion.is_some() => {
                if let Some(popup) = self.pending_completion.as_mut() {
                    let last = popup.items.len().saturating_sub(1);
                    popup.selected_idx = (popup.selected_idx + 1).min(last);
                }
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Up if self.mode != "run" && self.mode != "prompt" => {
                action_handlers::dispatch(self, &stoat_action::MoveUp);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Down if self.mode != "run" && self.mode != "prompt" => {
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
        let offset = buf_snapshot.resolve_anchor(&sel.head());
        let rope = buf_snapshot.rope();
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

    /// Switch [`Self::mode`] to `next`, opening or closing the
    /// insert-run buffer that backs the `.` register. Entering
    /// any insert-like mode (`insert`, `reword_insert`) starts a
    /// fresh run; leaving commits the run's text into
    /// [`Self::last_insert_text`] (when non-empty) and clears the
    /// scratch buffer.
    pub(crate) fn transition_mode(&mut self, next: String) {
        let was_insert = is_insert_run_mode(&self.mode);
        let now_insert = is_insert_run_mode(&next);
        if was_insert
            && !now_insert
            && let Some(run) = self.current_insert_run.take()
            && !run.is_empty()
        {
            self.last_insert_text = Some(run);
        }
        if !was_insert && now_insert {
            self.current_insert_run = Some(String::new());
        }
        self.mode = next;
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
        let sel = editor.selections.newest_anchor().clone();
        let offset = buf_snapshot.resolve_anchor(&sel.head());
        {
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(offset..offset, text);
        }
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let new_offset = offset + text.len();
        let anchor = new_buf.anchor_at(new_offset, Bias::Right);
        editor.selections.transform(new_buf, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, stoat_text::SelectionGoal::None);
            new
        });
    }

    fn editor_backspace(&mut self, editor_id: EditorId, buffer_id: BufferId) {
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
        let sel = editor.selections.newest_anchor().clone();
        let offset = buf_snapshot.resolve_anchor(&sel.head());
        if offset == 0 {
            return;
        }
        let rope = buf_snapshot.rope();
        let prev_len = rope
            .reversed_chars_at(offset)
            .next()
            .map(|ch| ch.len_utf8())
            .unwrap_or(0);
        let start = offset - prev_len;
        {
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(start..offset, "");
        }
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let anchor = new_buf.anchor_at(start, Bias::Right);
        editor.selections.transform(new_buf, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, stoat_text::SelectionGoal::None);
            new
        });
    }

    fn editor_delete_word_backward(&mut self, editor_id: EditorId, buffer_id: BufferId) {
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
        let sel = editor.selections.newest_anchor().clone();
        let offset = buf_snapshot.resolve_anchor(&sel.head());
        let start = stoat_text::prev_word_start(buf_snapshot.rope(), offset);
        if start == offset {
            return;
        }
        {
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(start..offset, "");
        }
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let anchor = new_buf.anchor_at(start, Bias::Right);
        editor.selections.transform(new_buf, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, stoat_text::SelectionGoal::None);
            new
        });
    }

    fn editor_delete(&mut self, editor_id: EditorId, buffer_id: BufferId) {
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
        let sel = editor.selections.newest_anchor().clone();
        let offset = buf_snapshot.resolve_anchor(&sel.head());
        let rope = buf_snapshot.rope();
        let next_len = rope
            .chars_at(offset)
            .next()
            .map(|ch| ch.len_utf8())
            .unwrap_or(0);
        if next_len == 0 {
            return;
        }
        let end = offset + next_len;
        let mut guard = buffer.write().expect("poisoned");
        guard.edit(offset..end, "");
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
        let AgentControl::OpenEditor { uid, path, done } = ctl;
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

        let Some(buffer_id) = action_handlers::file::open_file_in_pane(self, new_pane, &path)
        else {
            return UpdateEffect::None;
        };
        self.active_workspace_mut()
            .editor_bridge_waiters
            .insert(buffer_id, done);
        UpdateEffect::Redraw
    }

    /// Start the per-session agent hook server for `uid` on the executor.
    ///
    /// Binds the session's hook socket (see [`crate::run::agent_socket_path`])
    /// and forwards decoded events to [`Self::handle_agent_event`] through the
    /// shared channel. Callers spawn this alongside the owned Claude subshell.
    pub fn serve_agent_session(&self, uid: WorkspaceUid) -> io::Result<()> {
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
                for text in block.grid.clipboard_writes.drain(..) {
                    if let Err(err) = clipboard_host.set(&text) {
                        tracing::warn!(
                            target: "stoat::app",
                            error = %err,
                            "clipboard write failed"
                        );
                    }
                }
                if block.grid.alt_screen_detected {
                    block.error = Some("this command requires a full terminal".into());
                    block.finished = true;
                    block.grid.alt_screen_detected = false;
                    if let Some(handle) = &mut run_state.shell_handle {
                        handle.kill();
                    }
                    run_state.shell_handle = None;
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
            PtyNotification::AgentOutput { agent_id, data } => {
                let Some(agent) = ws.agents.get_mut(agent_id) else {
                    return UpdateEffect::None;
                };
                agent.term.feed(&data);
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
        crate::render::frame(self, buf);
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

        // A full-screen overlay mode hides every editor, so nothing is pooled this
        // frame and any live pools are retired.
        let overlay = matches!(
            self.mode.as_str(),
            "commits" | "rebase" | "reword" | "reword_insert" | "conflict"
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

        // The command palette is a modal over normal mode like the finder; its
        // fixed list region pools as a non-pane surface. Only the Filter phase
        // has a list -- CollectArgs walks parameters with no scrolling list.
        let palette_list = (!overlay
            && self.command_palette.as_ref().is_some_and(|p| {
                matches!(p.phase, crate::command_palette::PalettePhase::Filter { .. })
            }))
        .then(|| crate::render::command_palette::palette_filter_layout(self.size()))
        .flatten()
        .map(|layout| layout.list);

        // The commits overlay renders into the focused pane; its left list pools
        // as a non-pane surface while editor panes stay suppressed in this mode.
        let commits_region = (self.mode == "commits")
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

        let mut out = Vec::new();
        let mut active: Vec<u32> = panes.iter().map(|(pool, _, _)| *pool).collect();
        if finder_list.is_some() {
            active.push(crate::smooth_scroll::non_pane_pool::FINDER);
        }
        if palette_list.is_some() {
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
        self.smooth_scroll.drop_absent(&mut out, &active);

        let ws = &mut self.workspaces[self.active_workspace];
        let theme = &self.theme;
        let fallback_style = theme.get(crate::theme::scope::UI_TEXT);
        for (_, editor_id, region) in &panes {
            let region = *region;
            let Some(editor) = ws.editors.get_mut(*editor_id) else {
                continue;
            };
            let scroll_row = editor.scroll_row;
            // Review rows regenerate on accept/reject, so their count is the
            // pool's content version; plain editors stay stable while scrolling.
            let is_review = editor.review_view.is_some();
            let content_version = editor
                .review_view
                .as_ref()
                .map_or(0, |view| view.rows.len() as u64);
            crate::smooth_scroll::emit_into(
                &mut out,
                &mut self.smooth_scroll,
                region,
                scroll_row,
                content_version,
                |page| {
                    if is_review {
                        crate::smooth_scroll::render_review_page(
                            editor,
                            page,
                            fallback_style,
                            theme,
                            region.width,
                            region.height,
                        )
                    } else {
                        crate::smooth_scroll::render_editor_page(
                            editor,
                            page,
                            fallback_style,
                            theme,
                            region.width,
                            region.height,
                        )
                    }
                },
            );
        }

        if let (Some(list), Some(finder)) = (finder_list, self.file_finder.as_ref()) {
            let region = stoatty_protocol::command::PoolRegionCommand {
                pool: crate::smooth_scroll::non_pane_pool::FINDER,
                top: list.y,
                left: list.x,
                width: list.width,
                height: list.height,
            };
            let scroll_row = finder
                .selected
                .saturating_sub(list.height.saturating_sub(1) as usize)
                as u32;
            // The visible row set is the finder's filtered indices; a re-filter
            // changes it, so its hash is the pool's content version.
            let content_version = {
                let mut hasher = DefaultHasher::new();
                finder.filtered.hash(&mut hasher);
                hasher.finish()
            };
            crate::smooth_scroll::emit_into(
                &mut out,
                &mut self.smooth_scroll,
                region,
                scroll_row,
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

        if let (Some(list), Some(palette)) = (palette_list, self.command_palette.as_ref())
            && let crate::command_palette::PalettePhase::Filter {
                filtered,
                match_indices,
                selected,
                ..
            } = &palette.phase
        {
            let region = stoatty_protocol::command::PoolRegionCommand {
                pool: crate::smooth_scroll::non_pane_pool::PALETTE,
                top: list.y,
                left: list.x,
                width: list.width,
                height: list.height,
            };
            let scroll_row = selected.saturating_sub(list.height.saturating_sub(1) as usize) as u32;
            // The visible row set is the filtered entries; a re-filter changes it,
            // so a hash of their names is the pool's content version.
            let content_version = {
                let mut hasher = DefaultHasher::new();
                for entry in filtered {
                    entry.def.name().hash(&mut hasher);
                }
                hasher.finish()
            };
            crate::smooth_scroll::emit_into(
                &mut out,
                &mut self.smooth_scroll,
                region,
                scroll_row,
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
                scroll_row,
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
                scroll_row,
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
                list_scroll,
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
                detail_scroll,
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

        if !out.is_empty() {
            let _ = apc_tx.send(out);
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
        self.drive_parse_jobs();
        action_handlers::pump_commits(self);
        action_handlers::pump_review_scan(self);
        action_handlers::pump_lsp_jumps(self);
        action_handlers::lsp::pump_lsp_hover(self);
        action_handlers::lsp::pump_lsp_code_actions(self);
        action_handlers::lsp::pump_lsp_code_action_resolve(self);
        action_handlers::lsp::pump_lsp_prepare_rename(self);
        action_handlers::lsp::pump_lsp_rename(self);
        action_handlers::lsp::pump_lsp_symbol_picker(self);
        action_handlers::lsp::pump_lsp_workspace_symbol(self);
        action_handlers::lsp::pump_lsp_format(self);
        crate::completion::request::pump(self);
    }

    fn dispatch_workspace_picker_key(&mut self, key: KeyEvent) -> UpdateEffect {
        let outcome = match self.workspace_picker.as_mut() {
            Some(picker) => picker.handle_key(key),
            None => return UpdateEffect::None,
        };
        match outcome {
            PickerOutcome::None => UpdateEffect::Redraw,
            PickerOutcome::Close => {
                self.workspace_picker = None;
                UpdateEffect::Redraw
            },
            PickerOutcome::Select(id) => {
                self.workspace_picker = None;
                if id == self.active_workspace {
                    return UpdateEffect::Redraw;
                }
                self.save_workspace(self.active_workspace());
                self.active_workspace = id;
                let size = self.size;
                self.active_workspace_mut().layout(size);
                UpdateEffect::Redraw
            },
        }
    }

    fn dispatch_quit_all_confirm_key(&mut self, key: KeyEvent) -> UpdateEffect {
        let outcome = match self.quit_all_confirm.as_mut() {
            Some(modal) => modal.handle_key(key),
            None => return UpdateEffect::None,
        };
        match outcome {
            ConfirmOutcome::None => UpdateEffect::Redraw,
            ConfirmOutcome::Cancel => {
                self.quit_all_confirm = None;
                UpdateEffect::Redraw
            },
            ConfirmOutcome::Confirm => UpdateEffect::Quit,
        }
    }

    fn dispatch_jumplist_picker_key(&mut self, key: KeyEvent) -> UpdateEffect {
        use crate::jumplist_picker::PickerOutcome;
        let outcome = match self.jumplist_picker.as_mut() {
            Some(picker) => picker.handle_key(key),
            None => return UpdateEffect::None,
        };
        match outcome {
            PickerOutcome::None => UpdateEffect::Redraw,
            PickerOutcome::Close => {
                if let Some(picker) = self.jumplist_picker.take() {
                    self.mode = picker.previous_mode;
                }
                UpdateEffect::Redraw
            },
            PickerOutcome::Select(idx) => {
                let Some(picker) = self.jumplist_picker.take() else {
                    return UpdateEffect::None;
                };
                let target_offset = match picker.entries().get(idx) {
                    Some(entry) => entry.offset,
                    None => return UpdateEffect::Redraw,
                };
                self.mode = picker.previous_mode;
                self.jump_focused_to_offset(target_offset, idx);
                UpdateEffect::Redraw
            },
        }
    }

    fn dispatch_diagnostics_picker_key(&mut self, key: KeyEvent) -> UpdateEffect {
        use crate::diagnostics_picker::PickerOutcome;
        let outcome = match self.diagnostics_picker.as_mut() {
            Some(picker) => picker.handle_key(key),
            None => return UpdateEffect::None,
        };
        match outcome {
            PickerOutcome::None => UpdateEffect::Redraw,
            PickerOutcome::Close => {
                if let Some(picker) = self.diagnostics_picker.take() {
                    self.mode = picker.previous_mode;
                }
                UpdateEffect::Redraw
            },
            PickerOutcome::Select(idx) => {
                let Some(picker) = self.diagnostics_picker.take() else {
                    return UpdateEffect::None;
                };
                let entry = match picker.entries().get(idx) {
                    Some(entry) => entry,
                    None => return UpdateEffect::Redraw,
                };
                let path = entry.path.clone();
                let zero_based_line = entry.line.saturating_sub(1);
                let zero_based_column = entry.column.saturating_sub(1);
                let local_offset = entry.offset;
                self.mode = picker.previous_mode;
                let offset = match path {
                    Some(path) => {
                        action_handlers::file::open_file(self, &path);
                        self.offset_for_focused_point(zero_based_line, zero_based_column)
                            .unwrap_or(0)
                    },
                    None => local_offset,
                };
                self.collapse_focused_cursor_to(offset);
                UpdateEffect::Redraw
            },
        }
    }

    /// Resolve a `(line, column)` 0-based point to a byte
    /// offset in the focused editor's rope. Returns `None`
    /// when the focused pane is not an editor.
    fn offset_for_focused_point(&mut self, line: u32, column: u32) -> Option<usize> {
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
    fn collapse_focused_cursor_to(&mut self, offset: usize) {
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
        let anchor = buf_snap.anchor_at(offset, Bias::Right);
        editor.selections.transform(buf_snap, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, stoat_text::SelectionGoal::None);
            new
        });
    }

    fn dispatch_global_search_key(&mut self, key: KeyEvent) -> UpdateEffect {
        use crate::global_search::PickerOutcome;
        let outcome = match self.global_search.as_mut() {
            Some(picker) => picker.handle_key(key),
            None => return UpdateEffect::None,
        };
        match outcome {
            PickerOutcome::None => UpdateEffect::Redraw,
            PickerOutcome::Close => {
                if let Some(picker) = self.global_search.take() {
                    self.mode = picker.previous_mode;
                }
                UpdateEffect::Redraw
            },
            PickerOutcome::Select(idx) => {
                let Some(picker) = self.global_search.take() else {
                    return UpdateEffect::None;
                };
                let target = match picker.matches().get(idx) {
                    Some(m) => (m.path.clone(), m.offset),
                    None => return UpdateEffect::Redraw,
                };
                self.mode = picker.previous_mode;
                action_handlers::dispatch(self, &OpenFile { path: target.0 });
                self.jump_focused_to_match_offset(target.1);
                UpdateEffect::Redraw
            },
        }
    }

    fn jump_focused_to_match_offset(&mut self, offset: usize) {
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
        let anchor = buf_snap.anchor_at(offset, Bias::Right);
        editor.selections.transform(buf_snap, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, stoat_text::SelectionGoal::None);
            new
        });
    }

    fn jump_focused_to_offset(&mut self, offset: usize, jumplist_idx: usize) {
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
        let anchor = buf_snap.anchor_at(offset, Bias::Right);
        editor.selections.transform(buf_snap, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, stoat_text::SelectionGoal::None);
            new
        });
        editor.jumplist.set_cursor(jumplist_idx);
    }
}

/// Convert an LSP `file:` URI to a [`PathBuf`]. Returns `None` for any
/// other scheme; non-`file:` diagnostic notifications are silently
/// dropped because stoat has no concept of remote-path buffers today.
/// Modes whose `editor_insert` calls accumulate into the `.`
/// register's insert run. Helix tracks this for `insert` and
/// `reword_insert` only; `prompt` and `run` write to scratch
/// inputs that should not surface in the dot register.
fn is_insert_run_mode(mode: &str) -> bool {
    mode == "insert" || mode == "reword_insert"
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

    let prev_injection_trees = prior
        .take()
        .map(|prev| prev.injection_trees)
        .unwrap_or_default();

    let extracted =
        language::extract_highlights_rope_with_cache(lang, &tree, &new_rope, prev_injection_trees);
    // Theme-driven path: span.id is set to the theme key index by
    // collect_highlights_into via language.highlight_map(). Spans whose id
    // is DEFAULT (capture not in the active theme) are skipped because
    // they have no rendered style.
    let tokens: Arc<[SemanticTokenHighlight]> = extracted
        .spans
        .into_iter()
        .filter_map(|sp| {
            let style_id = styles.id_for_highlight(sp.id)?;
            Some(SemanticTokenHighlight {
                // Insertions at the start of a token attach to the previous
                // span, not this one; insertions at the end attach to the
                // next span. Keeps a typed character from silently extending
                // a keyword or string into neighboring text.
                range: snapshot.anchor_at(sp.byte_range.start, Bias::Right)
                    ..snapshot.anchor_at(sp.byte_range.end, Bias::Left),
                style: style_id,
            })
        })
        .collect();

    // Drive the multi-layer SyntaxMap alongside the legacy SyntaxState.
    // We don't have an interpolation pass on the host side yet (it would
    // need anchored byte offsets), so each parse produces a fresh SyntaxMap
    // from scratch; the prior_syntax_map is consumed but only its captured
    // tree is reused via SyntaxMap::reparse's internal `prior_injections`
    // snapshot.
    let mut syntax_map = prior_syntax_map.take().unwrap_or_default();
    let _ = syntax_map.reparse(&new_rope, lang.clone(), cur_version);

    Some(ParseJobOutput {
        buffer_id,
        syntax: SyntaxState {
            tree,
            version: cur_version,
            rope_snapshot: new_rope,
            injection_trees: extracted.injection_trees,
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
                .agents
                .insert(crate::agent_session::AgentSession {
                    term: crate::agent_term::AgentTerm::new(24, 80),
                    session,
                });

        let effect = stoat.handle_pty_notification(PtyNotification::AgentOutput {
            agent_id,
            data: b"hello".to_vec(),
        });

        assert_eq!(effect, UpdateEffect::Redraw);
        let term = &stoat.active_workspace().agents[agent_id].term;
        let row: String = term.row(0).iter().map(|cell| cell.ch).collect();
        assert!(row.starts_with("hello"), "row: {row:?}");
    }

    #[test]
    fn layout_fits_agent_emulator_and_pty_to_pane() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        ws.panes.split(crate::pane::Axis::Vertical);
        let focused = ws.panes.focus();

        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
        let agent_id = ws.agents.insert(crate::agent_session::AgentSession {
            term: crate::agent_term::AgentTerm::new(24, 80),
            session,
        });
        ws.panes.pane_mut(focused).view = View::Agent(agent_id);

        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        let ws = h.stoat.active_workspace();
        let (content, _) = crate::render::layout::split_pane_status(ws.panes.pane(focused).area);
        let term = &ws.agents[agent_id].term;
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
    fn closing_agent_pane_kills_pty_child() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        ws.panes.split(crate::pane::Axis::Vertical);
        let focused = ws.panes.focus();

        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
        let agent_id = ws.agents.insert(crate::agent_session::AgentSession {
            term: crate::agent_term::AgentTerm::new(24, 80),
            session,
        });
        ws.panes.pane_mut(focused).view = View::Agent(agent_id);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::ClosePane);
        h.settle();

        assert!(
            fake.was_killed(),
            "closing the agent pane kills its PTY child"
        );
        assert!(
            !h.stoat.active_workspace().agents.contains_key(agent_id),
            "closing the agent pane drops its session",
        );
    }

    fn stoat_with_focused_agent() -> (Stoat, AgentId, Arc<crate::host::FakeTerminalSession>) {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());
        stoat.mode = "insert".to_string();

        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let agent_id = ws.agents.insert(crate::agent_session::AgentSession {
            term: crate::agent_term::AgentTerm::new(24, 80),
            session,
        });
        ws.panes.pane_mut(focused).view = View::Agent(agent_id);
        (stoat, agent_id, fake)
    }

    fn bare(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
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
    fn focused_agent_pane_routes_keys_to_pty() {
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
            stoat.mode, "insert",
            "Ctrl-W passes through, does not leave insert"
        );
    }

    #[test]
    fn focused_agent_pane_sends_interrupt_on_ctrl_c() {
        let (mut stoat, _id, fake) = stoat_with_focused_agent();

        let effect = stoat.handle_key(ctrl('c'));

        assert_eq!(effect, UpdateEffect::None);
        assert_eq!(stoat.mode, "insert");
        assert_eq!(fake.sent_bytes(), vec![vec![0x03]]);
    }

    #[test]
    fn esc_escapes_agent_pane_without_forwarding() {
        let (mut stoat, _id, fake) = stoat_with_focused_agent();

        let effect = stoat.handle_key(bare(KeyCode::Esc));

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(stoat.mode, "normal");
        assert!(
            fake.sent_bytes().is_empty(),
            "escape must not reach the agent"
        );
    }

    #[test]
    fn agent_input_ignored_outside_insert_mode() {
        let (mut stoat, _id, fake) = stoat_with_focused_agent();
        stoat.mode = "normal".to_string();

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
        let _ = rx.try_recv().expect("first emit declares the editor pool");

        // Entering a full-screen overlay mode retires the editor pool.
        h.stoat.mode = "rebase".into();
        h.stoat.emit_smooth_scroll();
        let bytes = rx.try_recv().expect("overlay emit retires the pool");
        let cmds = decode_apc_stream(&bytes);
        assert!(
            !cmds.is_empty() && cmds.iter().all(|cmd| matches!(cmd, Command::PoolDrop(_))),
            "overlay mode only drops pools, got {cmds:?}"
        );
    }

    #[test]
    fn review_pane_is_pooled_for_smooth_scroll() {
        use crate::test_harness::{TestHarness, REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER};
        use stoatty_protocol::command::Command;

        let mut h = TestHarness::with_size(80, 24);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);

        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
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
        let bytes = rx.try_recv().expect("the open finder emits an APC batch");
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
            decode_apc_stream(&bytes).contains(&Command::PoolRegion(expected)),
            "the finder list declares a pool at its list rect"
        );

        h.stoat.file_finder = None;
        h.stoat.emit_smooth_scroll();
        let bytes = rx.try_recv().expect("closing the finder emits a drop");
        assert!(
            decode_apc_stream(&bytes).contains(&Command::PoolDrop(PoolDropCommand {
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
        let bytes = rx.try_recv().expect("the open palette emits an APC batch");
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
            decode_apc_stream(&bytes).contains(&Command::PoolRegion(expected)),
            "the palette list declares a pool at its list rect"
        );

        h.stoat.command_palette = None;
        h.stoat.emit_smooth_scroll();
        let bytes = rx.try_recv().expect("closing the palette emits a drop");
        assert!(
            decode_apc_stream(&bytes).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            })),
            "closing the palette retires its pool"
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
        let bytes = rx.try_recv().expect("the open popup emits an APC batch");
        assert!(
            decode_apc_stream(&bytes).contains(&Command::PoolRegion(expected)),
            "the completion popup declares a pool at its inner rect"
        );

        h.stoat.pending_completion = None;
        h.stoat.emit_smooth_scroll();
        let bytes = rx.try_recv().expect("closing the popup emits a drop");
        assert!(
            decode_apc_stream(&bytes).contains(&Command::PoolDrop(PoolDropCommand {
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
        let cmds = decode_apc_stream(&rx.try_recv().expect("the open help emits an APC batch"));
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
        let cmds = decode_apc_stream(&rx.try_recv().expect("closing help emits drops"));
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
        h.stoat.mode = "commits".to_string();

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

        h.stoat.mode = "normal".to_string();
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
        let effect = h.stoat.drain_pending(&mut rx);
        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(h.stoat.size(), Rect::new(0, 0, 120, 40));
        assert!(rx.try_recv().is_err(), "drain must empty the channel");
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
        let run_id = open_run_with_output(&mut h, b"hello\n");
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

    #[test]
    fn ctrl_w_in_insert_mode_deletes_previous_word() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        assert_eq!(h.stoat.mode, "insert");
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
        assert_eq!(h.stoat.mode, "insert");
        h.type_keys("delete");
        assert_eq!(buffer_text(&h, &path), "abc");
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
        assert_eq!(h.stoat.mode, "insert");
        h.type_text("X");
        assert_eq!(buffer_text(&h, &path), "aXbc\n");
    }

    #[test]
    fn shift_i_jumps_to_first_nonwhitespace_then_inserts() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "    code\n");
        h.type_keys("l");
        h.type_keys("I");
        assert_eq!(h.stoat.mode, "insert");
        h.type_text("X");
        assert_eq!(buffer_text(&h, &path), "    Xcode\n");
    }

    #[test]
    fn shift_a_jumps_to_line_end_then_inserts() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\nxyz\n");
        h.type_keys("A");
        assert_eq!(h.stoat.mode, "insert");
        h.type_text("Z");
        assert_eq!(buffer_text(&h, &path), "abcZ\nxyz\n");
    }

    #[test]
    fn open_below_inserts_blank_line_after_current_row() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\ndef\n");
        h.type_keys("o");
        assert_eq!(h.stoat.mode, "insert");
        h.type_text("X");
        assert_eq!(buffer_text(&h, &path), "abc\nX\ndef\n");
    }

    #[test]
    fn open_above_inserts_blank_line_before_current_row() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc\ndef\n");
        h.type_keys("o");
        h.type_keys("escape");
        h.type_keys("O");
        assert_eq!(h.stoat.mode, "insert");
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
        assert_eq!(h.stoat.mode, "insert");
        h.type_text("XYZ");
        assert_eq!(buffer_text(&h, &path), "XYZdef");
    }

    #[test]
    fn replace_char_replaces_each_char_in_selection() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abcdef");
        h.type_keys("v l l l");
        h.type_keys("r");
        h.type_keys("X");
        assert_eq!(buffer_text(&h, &path), "XXXdef");
        assert_eq!(h.stoat.mode, "select");
    }

    #[test]
    fn replace_char_on_collapsed_selection_is_noop() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc");
        h.type_keys("r");
        h.type_keys("X");
        assert_eq!(buffer_text(&h, &path), "abc");
        assert_eq!(h.stoat.mode, "normal");
        assert!(!h.stoat.pending_replace);
    }

    #[test]
    fn replace_char_with_multibyte_input_grows_buffer() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "abc");
        h.type_keys("v l l");
        h.type_keys("r");
        h.type_text("é");
        assert_eq!(buffer_text(&h, &path), "ééc");
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
    fn tab_after_whitespace_inserts_tab() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "  abc\n");
        h.type_keys("l l i");
        h.type_keys("tab");
        assert_eq!(buffer_text(&h, &path), "  \tabc\n");
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
        assert_eq!(h.stoat.mode, "insert");
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![CompletionItem {
                label: "foo".into(),
                source: CompletionSource::Lsp,
                kind: None,
                detail: None,
                replace_range: 0..0,
                insert_text: "foo".into(),
                is_snippet: false,
            }],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..0,
        });
        h.type_keys("escape");
        assert_eq!(h.stoat.pending_completion, None);
        assert_eq!(h.stoat.mode, "insert");
    }

    #[test]
    fn esc_in_insert_with_no_popup_exits_to_normal() {
        let mut h = Stoat::test();
        let _path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        assert_eq!(h.stoat.mode, "insert");
        assert_eq!(h.stoat.pending_completion, None);
        h.type_keys("escape");
        assert_eq!(h.stoat.mode, "normal");
    }

    #[test]
    fn tab_with_no_popup_smart_indents_after_whitespace() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "  abc\n");
        h.type_keys("l l i");
        assert!(h.stoat.pending_completion.is_none());
        h.type_keys("tab");
        assert_eq!(buffer_text(&h, &path), "  \tabc\n");
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
                },
                CompletionItem {
                    label: "foobar".into(),
                    source: CompletionSource::Word,
                    kind: None,
                    detail: None,
                    replace_range: 0..1,
                    insert_text: "foobar".into(),
                    is_snippet: false,
                },
                CompletionItem {
                    label: "foobaz".into(),
                    source: CompletionSource::Word,
                    kind: None,
                    detail: None,
                    replace_range: 0..1,
                    insert_text: "foobaz".into(),
                    is_snippet: false,
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
            }],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..1,
        });
        h.type_keys("tab");
        assert!(h.stoat.active_snippet.is_some());

        h.type_keys("escape");
        h.type_keys("escape");
        assert_eq!(h.stoat.mode, "normal");
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

    /// Three writes inside the [`REVIEW_EXTERNAL_EDIT_DEBOUNCE`]
    /// window must collapse into a single pending dispatch task,
    /// matching the formatter-on-save burst the production
    /// watcher receives. A separate
    /// [`crate::test_harness::TestHarness::advance_clock`] then
    /// fires the surviving timer; the channel drains and the
    /// pending map empties.
    #[test]
    fn review_external_edit_burst_within_debounce_coalesces() {
        use crate::test_harness::{TestHarness, REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER};

        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();

        h.external_edit("a.rs", "burst1\n");
        h.external_edit("a.rs", "burst2\n");
        h.external_edit("a.rs", "burst3\n");

        assert_eq!(
            h.stoat.review_pending_external_edits.len(),
            1,
            "three writes within the debounce window must coalesce to one task",
        );

        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        assert_eq!(
            h.stoat.review_pending_external_edits.len(),
            0,
            "the surviving task fires once advance_clock crosses the debounce window",
        );
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
