use crate::{
    action_handlers,
    buffer::{BufferId, TextBufferSnapshot},
    display_map::{highlights::SemanticTokenHighlight, syntax_theme::SyntaxStyles},
    editor_state::EditorId,
    host::{
        EnvHost, FsHost, FsWatchHost, GitHost, LocalEnv, LocalFs, LocalGit, LspServer,
        NoopFsWatcher, NoopLspServer,
    },
    keymap::{is_text_input_mode, normalize_shift_event, resolve_action, Keymap},
    keymap_state::StoatKeymapState,
    pane::{FocusTarget, View},
    rebase::RebasePause,
    register,
    run::{CommandMark, GridSelection, PtyNotification},
    workspace::{Workspace, WorkspaceId},
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{buffer::Buffer, layout::Rect};
use slotmap::SlotMap;
use std::{
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_action::{OpenFile, OpenReview, ReviewExternalEdit};
use stoat_config::Settings;
use stoat_language::{self as language, Language, LanguageRegistry, SyntaxState};
use stoat_scheduler::Executor;
use stoat_text::Bias;
use tokio::sync::mpsc::{Receiver, Sender};

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

pub struct Stoat {
    size: Rect,
    pub mode: String,
    pub executor: Executor,
    pub(crate) keymap: Keymap,
    pub settings: Settings,
    pub theme: crate::theme::Theme,
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
    /// When true, [`Self::save_workspace`] and the startup load path become
    /// no-ops. Set by the test harness so test runs can't read or write the
    /// real `$XDG_STATE_HOME/stoat/workspaces/` directory.
    pub(crate) persistence_disabled: bool,
    pub(crate) language_registry: Arc<LanguageRegistry>,
    pub(crate) syntax_styles: SyntaxStyles,
    pub(crate) workspaces: SlotMap<WorkspaceId, Workspace>,
    pub(crate) active_workspace: WorkspaceId,
    pub(crate) pty_tx: Sender<PtyNotification>,
    pty_rx: Receiver<PtyNotification>,
    /// Wake-up signal for [`Self::run`]'s `tokio::select!`. Background
    /// tasks call `notify_one()` to kick the loop into a fresh
    /// `UpdateEffect::Redraw` once their result is ready, so the user
    /// does not have to type a key to see asynchronous output land
    /// (e.g. the file finder's workspace walk completing on the
    /// blocking pool). Multiple notifications collapse into one
    /// pending wake-up.
    pub(crate) redraw_notify: Arc<tokio::sync::Notify>,
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
    pub(crate) pending_surround_replace: stoat_language::surround::SurroundReplaceStage,
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
    /// Buffers for which `LspServer::did_open` has been dispatched.
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
    /// Git operations flow through this trait so tests can use
    /// [`crate::host::FakeGit`] without a real repository.
    pub(crate) git_host: Arc<dyn GitHost>,
    /// Environment-variable lookups go through this trait so tests can
    /// install [`crate::host::FakeEnv`] without leaking real env state.
    pub(crate) env_host: Arc<dyn EnvHost>,
    /// Language-server requests route through this trait. Defaults to
    /// [`NoopLspServer`] (every method returns the empty success response)
    /// until a real `LocalLsp` is wired in; tests install
    /// [`crate::host::FakeLsp`] to drive end-to-end LSP scenarios.
    pub(crate) lsp_server: Arc<dyn LspServer>,
    /// System-clipboard writes route through this trait. Defaults to
    /// [`NoopClipboard`] so headless or display-less environments do
    /// not error on the first clipboard event; tests install
    /// [`crate::host::FakeClipboard`] to assert on writes.
    pub(crate) clipboard_host: Arc<dyn crate::host::ClipboardHost>,
    /// Cache of pre-computed review hunks keyed by content hash plus
    /// language. Populated when the editor itself runs
    /// [`crate::review::extract_review_hunks_changeset`] so subsequent
    /// lookups against the same content can skip the structural-diff
    /// pass.
    pub(crate) diff_cache: Arc<std::sync::Mutex<crate::diff_cache::DiffCache>>,
    /// Tracks `$/progress` notifications so the status bar can show
    /// the freshest in-progress operation. Drained from
    /// [`crate::host::LspServer::try_recv_notification`] inside
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
        let (review_external_edit_tx, review_external_edit_rx) = tokio::sync::mpsc::channel(256);

        Self {
            size: Rect::default(),
            mode: "normal".into(),
            executor,
            keymap,
            settings,
            theme,
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
            persistence_disabled: false,
            language_registry,
            syntax_styles,
            workspaces,
            active_workspace,
            pty_tx,
            pty_rx,
            redraw_notify: Arc::new(tokio::sync::Notify::new()),
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
            pending_surround_replace: stoat_language::surround::SurroundReplaceStage::Idle,
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
            git_host: Arc::new(LocalGit::new()),
            env_host: Arc::new(LocalEnv),
            lsp_server: Arc::new(NoopLspServer),
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
        }
    }

    /// Look up a previously-cached diff by content hashes plus
    /// language. Returns the serialized hunk payload on cache hit, or
    /// `None` on miss.
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

    /// Swap in an alternative [`LspServer`]. The default is
    /// [`NoopLspServer`] (every request returns the empty success
    /// response); the test harness installs [`crate::host::FakeLsp`]
    /// so LSP-driven flows run against programmed responses.
    pub fn set_lsp_server(&mut self, server: Arc<dyn LspServer>) {
        self.lsp_server = server;
    }

    /// Returns the active [`LspServer`].
    pub fn lsp_server(&self) -> &Arc<dyn LspServer> {
        &self.lsp_server
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
        render: Sender<Buffer>,
    ) -> io::Result<()> {
        loop {
            let effect = tokio::select! {
                biased;
                event = events.recv() => {
                    let Some(event) = event else { break };
                    self.update(event)
                }
                notif = self.pty_rx.recv() => {
                    let Some(notif) = notif else { continue };
                    self.handle_pty_notification(notif)
                }
                _ = self.redraw_notify.notified() => UpdateEffect::Redraw,
            };
            match effect {
                UpdateEffect::Redraw => {
                    if render.send(self.render()).await.is_err() {
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

        if paths.is_empty() || self.active_workspace().review.is_none() {
            return;
        }

        let watch_workdir = match self.active_workspace().review.as_ref().map(|s| &s.source) {
            Some(crate::review_session::ReviewSource::WorkspaceWatch { workdir }) => {
                Some(workdir.clone())
            },
            _ => None,
        };
        if let Some(workdir) = &watch_workdir {
            let fs = self.fs_host.clone();
            paths.retain(|p| p.starts_with(workdir) && !fs.is_ignored(workdir, p));
        }

        for path in paths {
            self.arm_review_external_edit_debounce(path);
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

    /// Drains every notification currently buffered on
    /// [`crate::host::LspServer::try_recv_notification`] and dispatches
    /// each by variant. `Progress` updates the [`crate::lsp::progress::LspProgressMap`];
    /// other variants log via tracing for now and become future
    /// per-feature consumer hooks. Cap is per-tick to avoid starving
    /// the event loop on a pathological notification burst; the
    /// remainder drains on the next update.
    pub(crate) fn drain_lsp_notifications(&mut self) {
        use crate::host::LspNotification;
        use futures::FutureExt;
        let host = self.lsp_server.clone();
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
        let pos = run_state.active_block_grid_pos(area.width, area.height, col, row);
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
                if crate::host::osc52_should_emit(env_host.as_ref()) {
                    if let Err(err) = clipboard_host.osc52_emit(&text) {
                        tracing::warn!(
                            target: "stoat::app",
                            error = %err,
                            "OSC 52 emit failed"
                        );
                    }
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
            let area = match ws.focus {
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
            };
            area
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
                if crate::host::osc52_should_emit(env_host.as_ref()) {
                    if let Err(err) = clipboard_host.osc52_emit(&text) {
                        tracing::warn!(
                            target: "stoat::app",
                            error = %err,
                            "OSC 52 emit failed"
                        );
                    }
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
        let buffer_pt = snapshot.display_to_buffer(clipped, Bias::Left)?;
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

    fn handle_key(&mut self, key: KeyEvent) -> UpdateEffect {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(picker) = self.global_search.take() {
                self.mode = picker.previous_mode;
                return UpdateEffect::Redraw;
            }
            if self.mode == "run" {
                return action_handlers::dispatch(self, &stoat_action::RunInterrupt);
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

        if self.global_search.is_some() {
            return self.dispatch_global_search_key(key);
        }

        if is_text_input_mode(&self.mode) {
            if let Some(effect) = self.handle_insert_key(key) {
                return effect;
            }
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
                if let Some(digit) = ch.to_digit(10) {
                    if (1..=9).contains(&digit) {
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
                if let Some(digit) = ch.to_digit(10) {
                    if (1..=9).contains(&digit) {
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
                if let Some(digit) = ch.to_digit(10) {
                    if (1..=9).contains(&digit) {
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
            && self.pending_surround_replace != stoat_language::surround::SurroundReplaceStage::Idle
        {
            if let KeyCode::Char(ch) = key.code {
                let stage = self.pending_surround_replace;
                self.pending_surround_replace =
                    stoat_language::surround::SurroundReplaceStage::Idle;
                match stage {
                    stoat_language::surround::SurroundReplaceStage::AwaitFrom => {
                        self.pending_surround_replace =
                            stoat_language::surround::SurroundReplaceStage::AwaitTo(ch);
                        return UpdateEffect::Redraw;
                    },
                    stoat_language::surround::SurroundReplaceStage::AwaitTo(from) => {
                        return action_handlers::surround::execute_surround_replace(self, from, ch);
                    },
                    stoat_language::surround::SurroundReplaceStage::Idle => unreachable!(),
                }
            }
            self.pending_surround_replace = stoat_language::surround::SurroundReplaceStage::Idle;
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
        if count_active_mode && self.pending_count.is_some() && key.modifiers.is_empty() {
            if let KeyCode::Char(ch) = key.code {
                if ch.is_ascii_digit() {
                    let digit = ch.to_digit(10).expect("ascii digit");
                    let new_count = self
                        .pending_count
                        .unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add(digit);
                    self.pending_count = Some(new_count);
                    return UpdateEffect::Redraw;
                }
            }
        }

        let state = StoatKeymapState::from_stoat(self);
        let actions = self.keymap.lookup(&state, &key).map(|a| a.to_vec());
        let Some(actions) = actions else {
            if count_active_mode {
                if let KeyCode::Char(ch) = key.code {
                    if ch.is_ascii_digit() && key.modifiers.is_empty() {
                        let digit = ch.to_digit(10).expect("ascii digit");
                        self.pending_count = Some(digit);
                        return UpdateEffect::Redraw;
                    }
                }
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
                if let Some(mode_name) = ra.args.first().and_then(crate::keymap::arg_as_str) {
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
            if ra.name == "symbols" {
                dispatched_symbol_picker = true;
            }
            if ra.name == "workspace-symbols" {
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

        let in_reword = matches!(
            ws.rebase_active.as_ref().and_then(|a| a.pause.as_ref()),
            Some(RebasePause::Reword { .. })
        );
        if in_reword {
            if let Some(input) = ws.reword_input.as_ref() {
                return Some((input.editor_id, input.buffer_id));
            }
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
            if let KeyCode::Char(ch) = key.code {
                if let Some(register) = register::register_for_char(ch) {
                    if let Some(content) =
                        action_handlers::yank::read_register_content(self, register)
                    {
                        self.editor_insert(editor_id, buffer_id, &content);
                    }
                }
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
        if was_insert && !now_insert {
            if let Some(run) = self.current_insert_run.take() {
                if !run.is_empty() {
                    self.last_insert_text = Some(run);
                }
            }
        }
        if !was_insert && now_insert {
            self.current_insert_run = Some(String::new());
        }
        self.mode = next;
    }

    pub(crate) fn editor_insert(&mut self, editor_id: EditorId, buffer_id: BufferId, text: &str) {
        if !text.is_empty() {
            if let Some(run) = self.current_insert_run.as_mut() {
                run.push_str(text);
            }
        }
        self.edit_at_each_cursor(editor_id, buffer_id, |_rope, offset| {
            Some((offset..offset, text.to_string()))
        });
    }

    fn editor_backspace(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        self.edit_at_each_cursor(editor_id, buffer_id, |rope, offset| {
            if offset == 0 {
                return None;
            }
            let prev_len = rope
                .reversed_chars_at(offset)
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(0);
            if prev_len == 0 {
                return None;
            }
            Some(((offset - prev_len)..offset, String::new()))
        });
    }

    fn editor_delete_word_backward(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        self.edit_at_each_cursor(editor_id, buffer_id, |rope, offset| {
            let start = stoat_text::prev_word_start(rope, offset);
            if start == offset {
                return None;
            }
            Some((start..offset, String::new()))
        });
    }

    fn editor_delete_word_forward(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        self.edit_at_each_cursor(editor_id, buffer_id, |rope, offset| {
            let end = stoat_text::next_word_start(rope, offset);
            if end == offset {
                return None;
            }
            Some((offset..end, String::new()))
        });
    }

    fn editor_delete(&mut self, editor_id: EditorId, buffer_id: BufferId) {
        self.edit_at_each_cursor(editor_id, buffer_id, |rope, offset| {
            let next_len = rope
                .chars_at(offset)
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(0);
            if next_len == 0 {
                return None;
            }
            Some((offset..(offset + next_len), String::new()))
        });
    }

    /// Apply a per-cursor edit at every selection head, then re-place each
    /// edited cursor after its replacement. `edit_for` maps a head offset to
    /// the byte range to replace and the replacement text, or `None` to
    /// leave that cursor untouched. Edits run lowest-offset-first with a
    /// running delta so each later range lands at its shifted position;
    /// ranges that overlap an earlier edit are dropped. Cursors whose edit
    /// is skipped ride along on their existing anchors, which the applied
    /// edits shift automatically.
    fn edit_at_each_cursor<F>(&mut self, editor_id: EditorId, buffer_id: BufferId, edit_for: F)
    where
        F: Fn(&stoat_text::Rope, usize) -> Option<(Range<usize>, String)>,
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

        let mut edits: Vec<(usize, Range<usize>, String)> = {
            let display_snapshot = editor.display_map.snapshot();
            let buf_snapshot = display_snapshot.buffer_snapshot();
            let rope = buf_snapshot.rope();
            let mut per_cursor: Vec<(usize, Range<usize>, String)> = editor
                .selections
                .all_anchors()
                .iter()
                .filter_map(|s| {
                    let head = buf_snapshot.resolve_anchor(&s.head());
                    edit_for(rope, head).map(|(range, text)| (s.id, range, text))
                })
                .collect();
            per_cursor.sort_by_key(|(_, range, _)| range.start);
            per_cursor
        };

        let mut last_end = 0;
        edits.retain(|(_, range, _)| {
            if range.start < last_end {
                return false;
            }
            last_end = range.end;
            true
        });
        if edits.is_empty() {
            return;
        }

        let mut new_heads: Vec<(usize, usize)> = Vec::with_capacity(edits.len());
        {
            let mut guard = buffer.write().expect("poisoned");
            let mut delta: isize = 0;
            for (id, range, text) in &edits {
                let start = (range.start as isize + delta) as usize;
                let end = (range.end as isize + delta) as usize;
                guard.edit(start..end, text);
                new_heads.push((*id, start + text.len()));
                delta += text.len() as isize - (range.end - range.start) as isize;
            }
        }

        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        editor.selections.transform(new_buf, |s| {
            let mut new = s.clone();
            if let Some(&(_, offset)) = new_heads.iter().find(|(id, _)| *id == s.id) {
                let anchor = new_buf.anchor_at(offset, Bias::Right);
                new.collapse_to(anchor, stoat_text::SelectionGoal::None);
            }
            new
        });
    }

    pub(crate) fn handle_pty_notification(&mut self, notif: PtyNotification) -> UpdateEffect {
        let clipboard_host = self.clipboard_host.clone();
        let now = self.executor.now();
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
                let marks: Vec<CommandMark> = block.grid.command_marks.drain(..).collect();
                for text in block.grid.clipboard_writes.drain(..) {
                    if let Err(err) = clipboard_host.set(&text) {
                        tracing::warn!(
                            target: "stoat::app",
                            error = %err,
                            "clipboard write failed"
                        );
                    }
                }
                run_state.apply_command_marks(&marks, now);
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
                    block.finish(exit_status, now);
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
        let Self {
            workspaces,
            active_workspace,
            executor,
            syntax_styles,
            redraw_notify,
            ..
        } = self;
        workspaces[*active_workspace].drive_parse_jobs(executor, syntax_styles, redraw_notify);
    }

    pub(crate) fn render(&mut self) -> Buffer {
        self.render_tick += 1;
        self.drive_parse_jobs();
        action_handlers::pump_commits(self);
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
        let mut buf = Buffer::empty(self.size);
        crate::render::frame(self, &mut buf);
        buf
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
    use crate::buffer::TextBuffer;
    use std::path::Path;

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
}
