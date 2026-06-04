use crate::{
    buffer::Buffer,
    buffer_registry::{BufferRegistry, BufferRegistryEvent},
    claude_permission_modal::PermissionModal,
    conflict_item::{ConflictItem, ConflictSide},
    diagnostics::DiagnosticSet,
    diff_coordinator::DiffCoordinator,
    diff_map::DiffMap,
    display_map::DisplayMap,
    dock::{Dock, DockSide},
    editor::{Editor, EditorEvent, EditorMode},
    editor_input::EditorInput,
    fold_actions,
    fs_watcher_driver::{FsWatcherDriver, FsWatcherDriverEvent},
    git::coordinator::BlameCoordinator,
    globals::{
        EnvHostGlobal, ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal, PermissionPromptHostGlobal,
    },
    input_state_machine::InputStateMachine,
    item::ItemHandle,
    key_hint_banner::KeyHintBanner,
    keymap_loader::{compile_default_keymap, compile_from_settings},
    lsp_state::LspState,
    modal_layer::{ModalLayer, ModalView},
    multi_buffer::MultiBuffer,
    pane::{Pane, PaneEvent},
    pane_tree::{PaneTree, PaneTreeEvent},
    project_tree::ProjectTree,
    rebase_item::{RebaseItem, RebaseMoveDir},
    render_stats::{render_stats_enabled, FrameTimer, RenderStatsOverlay},
    review_session::ReviewApplyResult,
    settings::Settings,
    status_bar::{
        active_file::ActiveFileLabel, count_prefix::CountPrefix, cursor_position::CursorPosition,
        diagnostics_badge::DiagnosticsBadge, encoding::EncodingItem, line_ending::LineEndingItem,
        lsp_progress::LspProgress, mode_badge::ModeBadge, review_progress::ReviewProgress,
        search_indicator::SearchQueryIndicator, workspace_label::WorkspaceLabel, StatusBar,
        StatusItemView,
    },
    theme::{ActiveTheme, DEFAULT_UI_FONT_FAMILY, DEFAULT_UI_FONT_SIZE},
    toast::{Toast, ToastId, ToastView},
};
use gpui::{
    deferred, div, px, size, App, AppContext, BorrowAppContext, Bounds, Context, DismissEvent,
    Entity, EventEmitter, FocusHandle, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, Task, TitlebarOptions, WeakEntity, Window, WindowBounds,
    WindowOptions,
};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    rc::Rc,
    slice,
    sync::Arc,
    time::{Duration, Instant},
};
use stoat::{
    buffer::{BufferId, Encoding},
    host::{
        CherryPickOutcome, ConflictedFile, GitApplyError, GitRepo, RebaseError, RebaseTodo,
        RebaseTodoOp, WatchToken,
    },
    pane::{Axis, Direction},
    rebase::{ActiveRebase, RebaseEntry, RebasePause},
    review::{ReviewFileInput, ReviewRow},
    review_apply::remove_chunks_from_buffer,
    review_session::{build_chunk_patch, ChunkStatus, ReviewSource},
};
use stoat_action::ActionKind;
use stoat_config::LineNumberMode;

/// Top-level workspace entity. Composes the structural pieces of
/// a single Stoat window: the git root, the pane tree, any docks
/// pinned to the window edges, the modal layer overlaid on top of
/// pane content, and the status bar.
///
/// `modal_layer` and `status_bar` are placeholder entities for
/// now; their full implementations land under the corresponding
/// foundation parents in `.todo-plans/TODO.md`.
pub struct Workspace {
    name: SharedString,
    uid: stoat::workspace::WorkspaceUid,
    is_fresh: bool,
    git_root: PathBuf,
    pane_tree: Entity<PaneTree>,
    buffer_registry: Entity<BufferRegistry>,
    diff_coordinator: Entity<DiffCoordinator>,
    blame_coordinator: Entity<BlameCoordinator>,
    docks: Vec<Entity<Dock>>,
    /// Workspace-level visibility of the left dock group.
    /// Independent of each individual [`Dock`]'s
    /// `DockVisibility` -- this gates whether the side renders at
    /// all. Hiding restores each dock's prior per-dock state when
    /// the side is shown again.
    left_dock_visible: bool,
    /// Workspace-level visibility of the right dock group. See
    /// [`Workspace::left_dock_visible`] for the contract.
    right_dock_visible: bool,
    /// Workspace-level visibility of the bottom dock group. See
    /// [`Workspace::left_dock_visible`] for the contract.
    bottom_dock_visible: bool,
    /// EntityId of the currently-open `DiffHunkPanel` dock item,
    /// if any. Used by `ToggleDiffHunkPanel` to find the panel
    /// across `docks` index shifts caused by other dock
    /// add/remove activity. Cleared when the panel is removed or
    /// when its dock is no longer present.
    diff_hunk_panel: Option<gpui::EntityId>,
    /// EntityId of the active pane item the last time
    /// [`Workspace::broadcast_active_pane_item`] ran. The base input
    /// mode is reasserted from the active item's kind only when this
    /// changes, so an unrelated pane event cannot clobber an
    /// in-progress submode (e.g. `line_select`) while the same item
    /// stays active.
    last_active_item_id: Option<gpui::EntityId>,
    modal_layer: Entity<ModalLayer>,
    toast_view: Entity<ToastView>,
    status_bar: Entity<StatusBar>,
    key_hint_banner: Entity<KeyHintBanner>,
    input_state_machine: Entity<InputStateMachine>,
    editor_input: Entity<EditorInput>,
    lsp_state: Entity<LspState>,
    diagnostics: Entity<DiagnosticSet>,
    workspace_label: Entity<WorkspaceLabel>,
    active_file_label: Entity<ActiveFileLabel>,
    cursor_position: Entity<CursorPosition>,
    count_prefix: Entity<CountPrefix>,
    diagnostics_badge: Entity<DiagnosticsBadge>,
    lsp_progress: Entity<LspProgress>,
    review_progress: Entity<ReviewProgress>,
    search_indicator: Entity<SearchQueryIndicator>,
    fs_watcher_driver: Entity<FsWatcherDriver>,
    fs_watch_tokens: HashMap<PathBuf, WatchToken>,
    buffer_paths: HashMap<BufferId, PathBuf>,
    registers: stoat::register::RegisterStore,
    selected_register: Option<stoat::register::Register>,
    rebase_active: Option<ActiveRebase>,
    /// Pending Claude permission prompts waiting for the active
    /// permission modal to close. FIFO so prompts surface in the
    /// order the policy emits them, matching the TUI's
    /// `permission_prompt_queue`.
    permission_prompt_queue: VecDeque<stoat::host::PermissionPrompt>,
    /// Background task that drains
    /// [`PermissionPromptHostGlobal`] on a foreground tick. Dropped
    /// when the workspace drops; absent when no global is
    /// registered (most tests, headless runs).
    _permission_prompt_poll: Option<Task<()>>,
    /// Background task that periodically writes the workspace state
    /// to its default path. Lazy-started on first [`Render`] so the
    /// workspace is fully constructed before saves begin; dropped
    /// when the workspace drops.
    _periodic_save: Option<Task<()>>,
    focus_handle: FocusHandle,
    last_window_title: Option<SharedString>,
    last_motion: Option<LastMotion>,
    /// Recently confirmed command-palette queries, oldest first.
    /// Outlives the per-open `CommandPaletteDelegate` so recall
    /// survives reopening the palette, and feeds workspace persistence.
    command_palette_history: VecDeque<String>,
    /// Monotonic id for the most recent LSP goto request the
    /// workspace has spawned. The spawn site captures the id at
    /// dispatch and re-checks it before applying the response so a
    /// late reply for an obsolete cursor cannot move the cursor
    /// after the user has moved on.
    lsp_goto_request_seq: u64,
    /// Monotonic id for the most recent LSP rename request. The
    /// rename modal captures this at confirm time and the spawned
    /// future re-checks it before applying the returned
    /// `WorkspaceEdit`, so a late reply cannot land edits over a
    /// user's fresh typing after the modal dismissed.
    lsp_rename_request_seq: u64,
    frame_timer: Rc<RefCell<FrameTimer>>,
    _active_editor_subscription: Option<Subscription>,
    _pane_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

/// Last motion dispatched through one of `Workspace`'s
/// `dispatch_move_*` helpers. Captured by intent at the top of
/// each helper so `RepeatLastMotion` (`Alt-.`) can replay it.
/// Session-local: not persisted across workspace reloads.
#[derive(Copy, Clone, Debug)]
enum LastMotion {
    Horizontal {
        delta: i32,
        extend: bool,
    },
    Vertical {
        delta: i32,
        extend: bool,
    },
    Word {
        target: crate::editor::actions::movement::WordTarget,
        extend: bool,
    },
    Page {
        dir: crate::editor::actions::movement::PageDir,
        half: bool,
    },
    ParentBound {
        bound: crate::editor::actions::treesitter::NodeBound,
        extend: bool,
    },
}

/// Period of the permission-prompt poll task. Matches
/// [`FsWatcherDriver`]'s tick so the two background drainers share
/// a uniform cadence.
const PERMISSION_PROMPT_TICK: Duration = Duration::from_millis(50);

/// Interval between periodic workspace state saves. Trades off
/// snapshot freshness against IO churn; 30 s keeps an unclean
/// shutdown's data loss bounded to half a minute of session work
/// without pounding the disk while the user types.
const PERIODIC_SAVE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceEvent {
    NameChanged,
    DockAdded { index: usize },
    DockRemoved { index: usize },
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn new(
        name: impl Into<SharedString>,
        git_root: PathBuf,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let name = name.into();
        let workspace_handle = cx.weak_entity();
        let pane_tree = {
            let workspace = workspace_handle.clone();
            cx.new(|cx| PaneTree::new(workspace, cx))
        };
        let modal_layer = {
            let weak = workspace_handle.clone();
            cx.new(|cx| ModalLayer::new(Some(weak), cx))
        };
        let toast_view = {
            let weak = workspace_handle.clone();
            cx.new(|_| ToastView::new(Some(weak)))
        };
        let status_bar = cx.new(StatusBar::new);
        let buffer_registry = cx.new(|_| BufferRegistry::new());
        let diff_coordinator = {
            let registry = buffer_registry.clone();
            let git_root = git_root.clone();
            cx.new(|cx| DiffCoordinator::new(git_root, registry, cx))
        };
        let blame_coordinator = {
            let registry = buffer_registry.clone();
            let git_root = git_root.clone();
            cx.new(|cx| BlameCoordinator::new(git_root, registry, cx))
        };
        let keymap = cx
            .try_global::<Settings>()
            .map_or_else(compile_default_keymap, compile_from_settings);
        let input_state_machine = cx.new(|_| InputStateMachine::new(workspace_handle, keymap));
        let editor_input = {
            let weak_sm = input_state_machine.downgrade();
            cx.new(|cx| EditorInput::new(weak_sm, cx))
        };
        let keystroke_subscription = cx.observe_keystrokes(|workspace, event, window, cx| {
            let sm = workspace.input_state_machine.clone();
            let keystroke = event.keystroke.clone();
            let actions = sm.update(cx, |sm, cx| sm.feed(&keystroke, window, cx));
            for action in actions {
                workspace.dispatch_action(action, window, cx);
            }
        });
        let settings_subscription = cx.observe_global::<Settings>(|workspace, cx| {
            let keymap = compile_from_settings(cx.global::<Settings>());
            workspace
                .input_state_machine
                .update(cx, |sm, _| sm.set_keymap(keymap));
        });
        cx.observe_global::<crate::theme::Theme>(|_, cx| cx.notify())
            .detach();
        let pane_tree_subscription =
            cx.subscribe(&pane_tree, |workspace, _, _: &PaneTreeEvent, cx| {
                workspace.refresh_pane_subscriptions(cx);
                workspace.broadcast_active_pane_item(cx);
                workspace.broadcast_active_editor(cx);
            });
        let initial_panes: Vec<Entity<Pane>> = {
            let tree = pane_tree.read(cx);
            tree.split_pane_ids()
                .into_iter()
                .filter_map(|id| tree.pane(id).cloned())
                .collect()
        };
        let initial_pane_subscriptions: Vec<Subscription> = initial_panes
            .into_iter()
            .map(|pane| {
                cx.subscribe(&pane, |workspace, _, _event: &PaneEvent, cx| {
                    workspace.broadcast_active_pane_item(cx);
                    workspace.broadcast_active_editor(cx);
                })
            })
            .collect();

        let lsp_state = cx.new(|_| LspState::new());
        let diagnostics = cx.new(|_| DiagnosticSet::new());
        let mode_badge = cx.new(|cx| ModeBadge::new(input_state_machine.clone(), cx));
        let key_hint_banner = cx.new(|cx| KeyHintBanner::new(input_state_machine.clone(), cx));
        let workspace_label = cx.new(|_| WorkspaceLabel::new(name.clone()));
        let active_file_label = cx.new(|_| ActiveFileLabel::new(git_root.clone()));
        let cursor_position = cx.new(|_| CursorPosition::new());
        let line_ending_item = {
            let workspace = cx.weak_entity();
            cx.new(|_| LineEndingItem::new(workspace))
        };
        let encoding_item = {
            let workspace = cx.weak_entity();
            cx.new(|_| EncodingItem::new(workspace))
        };
        let count_prefix = cx.new(|cx| CountPrefix::new(input_state_machine.clone(), cx));
        let diagnostics_badge = cx.new(|_| DiagnosticsBadge::new());
        let lsp_progress = cx.new(|cx| LspProgress::new(lsp_state.clone(), cx));
        let review_progress = cx.new(|_| ReviewProgress::new());
        let search_indicator = cx.new(|_| SearchQueryIndicator::new());
        let fs_watcher_driver = cx.new(FsWatcherDriver::new);
        let buffer_registry_subscription = cx.subscribe(
            &buffer_registry,
            |workspace, _, event: &BufferRegistryEvent, cx| {
                if let BufferRegistryEvent::BufferRemoved(id) = event {
                    workspace.release_buffer_watch(*id, cx);
                }
            },
        );
        let fs_watcher_subscription = cx.subscribe(
            &fs_watcher_driver,
            |workspace, _, event: &FsWatcherDriverEvent, cx| {
                let FsWatcherDriverEvent::ExternalEdit { path } = event;
                workspace.dispatch_review_external_edit(path.clone(), cx);
                workspace.update_project_tree(cx, |tree, cx| tree.refresh(cx));
            },
        );
        let modal_layer_subscription = cx.observe(&modal_layer, |workspace, layer, cx| {
            workspace.refresh_modal_keymap_state(&layer, cx);
            match layer.read(cx).active_text_input_editor(cx) {
                Some(editor) => workspace
                    .input_state_machine
                    .update(cx, |sm, _| sm.set_active_editor(Some(editor))),
                None => workspace.broadcast_active_editor(cx),
            }
        });
        let initial_status_item: Option<Box<dyn ItemHandle>> = {
            let tree = pane_tree.read(cx);
            let focus = tree.focus();
            tree.pane(focus)
                .and_then(|p| p.read(cx).active_item().map(ItemHandle::boxed_clone))
        };
        status_bar.update(cx, |bar, cx| {
            bar.add_left_item(mode_badge.clone(), cx);
            bar.add_left_item(workspace_label.clone(), cx);
            bar.add_left_item(active_file_label.clone(), cx);
            bar.add_right_item(cursor_position.clone(), cx);
            bar.add_right_item(line_ending_item.clone(), cx);
            bar.add_right_item(encoding_item.clone(), cx);
            bar.add_right_item(count_prefix.clone(), cx);
            bar.add_right_item(lsp_progress.clone(), cx);
            bar.add_right_item(diagnostics_badge.clone(), cx);
            bar.add_right_item(review_progress.clone(), cx);
            bar.add_right_item(search_indicator.clone(), cx);
        });
        mode_badge.update(cx, |badge, cx| {
            badge.set_active_pane_item(initial_status_item.as_deref(), cx);
        });
        active_file_label.update(cx, |label, cx| {
            label.set_active_pane_item(initial_status_item.as_deref(), cx);
        });
        cursor_position.update(cx, |item, cx| {
            item.set_active_pane_item(initial_status_item.as_deref(), cx);
        });
        line_ending_item.update(cx, |item, cx| {
            item.set_active_pane_item(initial_status_item.as_deref(), cx);
        });
        encoding_item.update(cx, |item, cx| {
            item.set_active_pane_item(initial_status_item.as_deref(), cx);
        });
        diagnostics_badge.update(cx, |badge, cx| {
            badge.set_active_pane_item(initial_status_item.as_deref(), cx);
        });
        review_progress.update(cx, |badge, cx| {
            badge.set_active_pane_item(initial_status_item.as_deref(), cx);
        });
        search_indicator.update(cx, |item, cx| {
            item.set_active_pane_item(initial_status_item.as_deref(), cx);
        });
        let uid = cx
            .try_global::<ExecutorGlobal>()
            .map(|exec| stoat::workspace::WorkspaceUid::now(&exec.0))
            .unwrap_or_default();
        Self {
            name,
            uid,
            is_fresh: true,
            git_root,
            pane_tree,
            buffer_registry,
            diff_coordinator,
            blame_coordinator,
            docks: Vec::new(),
            left_dock_visible: true,
            right_dock_visible: true,
            bottom_dock_visible: true,
            diff_hunk_panel: None,
            last_active_item_id: None,
            modal_layer,
            toast_view,
            status_bar,
            key_hint_banner,
            input_state_machine,
            editor_input,
            lsp_state,
            diagnostics,
            workspace_label,
            active_file_label,
            cursor_position,
            count_prefix,
            diagnostics_badge,
            lsp_progress,
            review_progress,
            search_indicator,
            fs_watcher_driver,
            fs_watch_tokens: HashMap::new(),
            buffer_paths: HashMap::new(),
            registers: stoat::register::RegisterStore::new(),
            selected_register: None,
            rebase_active: None,
            permission_prompt_queue: VecDeque::new(),
            _permission_prompt_poll: None,
            _periodic_save: None,
            focus_handle: cx.focus_handle(),
            last_window_title: None,
            last_motion: None,
            command_palette_history: VecDeque::new(),
            lsp_goto_request_seq: 0,
            lsp_rename_request_seq: 0,
            frame_timer: Rc::new(RefCell::new(FrameTimer::new())),
            _active_editor_subscription: None,
            _pane_subscriptions: initial_pane_subscriptions,
            _subscriptions: vec![
                keystroke_subscription,
                settings_subscription,
                pane_tree_subscription,
                buffer_registry_subscription,
                fs_watcher_subscription,
                modal_layer_subscription,
            ],
        }
    }

    /// Push a Claude permission prompt onto the workspace's modal
    /// pipeline. When no [`PermissionModal`] is currently active the
    /// prompt opens one immediately; otherwise it queues FIFO and
    /// surfaces when the active modal closes. Mirrors
    /// [`stoat::Stoat::enqueue_permission_prompt`].
    pub fn enqueue_permission_prompt(
        &mut self,
        prompt: stoat::host::PermissionPrompt,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.permission_modal_active(cx) {
            self.permission_prompt_queue.push_back(prompt);
        } else {
            self.show_permission_modal(prompt, window, cx);
        }
    }

    fn permission_modal_active(&self, cx: &App) -> bool {
        self.modal_layer
            .read(cx)
            .active_modal::<PermissionModal>()
            .is_some()
    }

    fn show_permission_modal(
        &mut self,
        prompt: stoat::host::PermissionPrompt,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let weak_workspace = cx.weak_entity();
        let modal = cx.new(|cx| PermissionModal::new(prompt, weak_workspace, cx));
        let dismiss_subscription = cx.subscribe_in(
            &modal,
            window,
            |workspace, _, _: &DismissEvent, window, cx| {
                workspace.on_permission_modal_dismissed(window, cx);
            },
        );
        self.modal_layer.update(cx, |layer, cx| {
            layer.show_modal(modal, window, cx);
        });
        self._subscriptions.push(dismiss_subscription);
    }

    fn on_permission_modal_dismissed(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(next) = self.permission_prompt_queue.pop_front() {
            self.show_permission_modal(next, window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn permission_prompt_queue_len(&self) -> usize {
        self.permission_prompt_queue.len()
    }

    /// Open the [`crate::shell_input_modal::ShellInputModal`] for
    /// `action`, replacing any active modal. Confirm flows through
    /// `ShellInputSubmit` to [`Self::run_shell_command`]; abort
    /// flows through `DismissModal`.
    pub fn show_shell_input_modal(
        &mut self,
        action: crate::editor::actions::shell::ShellAction,
        weak_workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.modal_layer.update(cx, |layer, cx| {
            let modal = cx.new(|cx| {
                crate::shell_input_modal::ShellInputModal::new(weak_workspace, action, window, cx)
            });
            layer.show_modal(modal, window, cx);
        });
    }

    /// Open a one-shot run overlay for the `Run` action's command. The
    /// overlay spawns the command, streams its output, and is dismissed
    /// when done; distinct from the persistent run pane (`OpenRun`).
    fn dispatch_run(
        &mut self,
        action: &dyn stoat_action::Action,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(run) = action.as_any().downcast_ref::<stoat_action::Run>() else {
            return;
        };
        let command = run.command.clone();
        let cwd = self.git_root.clone();
        self.modal_layer.update(cx, |layer, cx| {
            let modal = cx.new(|cx| crate::run_modal::RunModal::new(command, cwd, cx));
            layer.show_modal(modal, window, cx);
        });
    }

    /// Write a `.dump` archive of the current workspace under
    /// `<XDG_DATA_HOME>/stoat/dumps/`. The GUI has no TUI `Stoat` state,
    /// so the snapshot carries the working tree plus the current input
    /// mode; richer pane/buffer state is not captured. Logs the outcome.
    fn dispatch_dump(&mut self, action: &dyn stoat_action::Action, cx: &mut Context<'_, Self>) {
        let Some(dump) = action.as_any().downcast_ref::<stoat_action::Dump>() else {
            return;
        };
        let git_root = self.git_root.clone();
        let mode = self.input_state_machine.read(cx).mode().to_string();
        let fs = cx.global::<FsHostGlobal>().0.clone();
        match stoat::dump::save_workspace_dir(
            &git_root,
            &mode,
            &dump.name,
            time::OffsetDateTime::now_utc(),
            fs.as_ref(),
        ) {
            Ok(id) => tracing::info!(id = %id, "GUI workspace dump captured"),
            Err(err) => tracing::error!(%err, name = %dump.name, "GUI workspace dump failed"),
        }
    }

    /// Dispatch to [`crate::editor::actions::shell::apply`]. Public
    /// so the modal can call back through its weak workspace handle.
    pub fn run_shell_command(
        &mut self,
        action: crate::editor::actions::shell::ShellAction,
        cmd: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let _ = window;
        crate::editor::actions::shell::apply(self, action, cmd, cx);
    }

    pub fn name(&self) -> &SharedString {
        &self.name
    }

    pub fn git_root(&self) -> &PathBuf {
        &self.git_root
    }

    pub fn pane_tree(&self) -> &Entity<PaneTree> {
        &self.pane_tree
    }

    pub fn rebase_active(&self) -> Option<&ActiveRebase> {
        self.rebase_active.as_ref()
    }

    /// Open the file referenced by the focused tool card on the
    /// active [`crate::claude_chat::ClaudeChat`] and move the cursor
    /// to the referenced line. Mirrors the TUI's
    /// `claude_jump_to_focused_card` body in
    /// `stoat/src/action_handlers/claude.rs:409`; silent no-op when
    /// the active pane is not a chat, no card is focused, the
    /// focused card has no `file_path`, or the resolved absolute
    /// path falls outside the workspace's git root.
    pub fn jump_to_focused_claude_card(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(chat) = crate::claude_chat::focused_chat(self, cx) else {
            return;
        };
        let Some((path, line)) = chat.read(cx).focused_tool_card_location() else {
            return;
        };
        let absolute = if path.is_absolute() {
            path
        } else {
            self.git_root.join(path)
        };
        if !absolute.starts_with(&self.git_root) {
            return;
        }
        let _ = window;
        self.open_paths(slice::from_ref(&absolute), cx);
        let pane_id = self.pane_tree.read(cx).focus();
        let Some(pane) = self.pane_tree.read(cx).pane(pane_id).cloned() else {
            return;
        };
        let target_editor: Option<(usize, Entity<Editor>)> = pane
            .read(cx)
            .items()
            .iter()
            .enumerate()
            .find_map(|(idx, item)| {
                let editor = item.to_any_view().downcast::<Editor>().ok()?;
                let path = editor.read(cx).file_path()?.to_path_buf();
                (path == absolute).then_some((idx, editor))
            });
        let Some((index, editor)) = target_editor else {
            return;
        };
        pane.update(cx, |p, cx| {
            p.activate(index, cx);
        });
        if let Some(line) = line {
            editor.update(cx, |ed, cx| ed.handle_goto_line_number(Some(line), cx));
        }
    }

    /// Restore the workspace's git working tree to the state
    /// captured at `sha`. Resolves the repo via
    /// [`GitHostGlobal`] and calls
    /// [`GitRepo::restore_tree`]; failures log via `tracing::warn!`
    /// but do not propagate -- there is no surface in the GUI yet
    /// for a checkpoint-restore error toast. Mirrors the TUI
    /// checkpoint marker mouse-handler at `stoat/src/app.rs:1259`.
    pub fn restore_to_checkpoint(&mut self, sha: String, cx: &mut Context<'_, Self>) {
        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&self.git_root) else {
            tracing::warn!(
                target: "stoat_gui::claude",
                git_root = ?self.git_root,
                "checkpoint restore skipped: no git repo at workspace root",
            );
            return;
        };
        if let Err(err) = repo.restore_tree(&sha) {
            tracing::warn!(
                target: "stoat_gui::claude",
                ?err,
                %sha,
                "checkpoint restore via marker click failed",
            );
        }
    }

    /// Propagate modal-layer transitions into the keymap-state
    /// fields the [`InputStateMachine`] exposes to the keymap engine.
    /// Driven by an `observe(&modal_layer, ...)` subscription wired
    /// in [`Self::new`]; fires on every `show_modal` / `hide_modal`
    /// / `dismiss_modal_by_id` because each of those calls
    /// `cx.notify` on the modal-layer entity.
    ///
    /// When the command palette, file finder, help modal, theme
    /// picker, or global search becomes the top of the stack, captures
    /// the prior mode, flips `mode` to `prompt`, and sets the matching
    /// `palette_open` / `finder_open` / `help_open` / `theme_picker_open`
    /// / `global_search_open` flag. When that modal is no longer on top
    /// (closed or buried under another modal), restores the prior mode
    /// and clears the flag.
    fn refresh_modal_keymap_state(&self, layer: &Entity<ModalLayer>, cx: &mut Context<'_, Self>) {
        let palette_active = layer
            .read(cx)
            .active_modal::<crate::picker::Picker<crate::command_palette::CommandPaletteDelegate>>()
            .is_some();
        let finder_active = layer
            .read(cx)
            .active_modal::<crate::picker::Picker<crate::file_finder::FileFinderDelegate>>()
            .is_some();
        let help_active = layer
            .read(cx)
            .active_modal::<crate::help::HelpModal>()
            .is_some();
        let theme_picker_active = layer
            .read(cx)
            .active_modal::<crate::picker::Picker<crate::theme_picker::ThemePickerDelegate>>()
            .is_some();
        let global_search_active = layer
            .read(cx)
            .active_modal::<crate::picker::Picker<crate::global_search::GlobalSearchDelegate>>()
            .is_some();
        self.input_state_machine.update(cx, |sm, cx_sm| {
            if palette_active {
                sm.capture_prev_mode_for_modal();
                sm.set_mode("prompt", cx_sm);
                sm.set_palette_open(true, cx_sm);
            } else if sm.palette_open() {
                sm.set_palette_open(false, cx_sm);
                if let Some(prev) = sm.take_prev_mode_for_modal() {
                    sm.set_mode(prev, cx_sm);
                }
            }

            if finder_active {
                sm.capture_prev_mode_for_modal();
                sm.set_mode("prompt", cx_sm);
                sm.set_finder_open(true, cx_sm);
            } else if sm.finder_open() {
                sm.set_finder_open(false, cx_sm);
                if let Some(prev) = sm.take_prev_mode_for_modal() {
                    sm.set_mode(prev, cx_sm);
                }
            }

            if help_active {
                sm.capture_prev_mode_for_modal();
                sm.set_mode("prompt", cx_sm);
                sm.set_help_open(true, cx_sm);
            } else if sm.help_open() {
                sm.set_help_open(false, cx_sm);
                if let Some(prev) = sm.take_prev_mode_for_modal() {
                    sm.set_mode(prev, cx_sm);
                }
            }

            if theme_picker_active {
                sm.capture_prev_mode_for_modal();
                sm.set_mode("prompt", cx_sm);
                sm.set_theme_picker_open(true, cx_sm);
            } else if sm.theme_picker_open() {
                sm.set_theme_picker_open(false, cx_sm);
                if let Some(prev) = sm.take_prev_mode_for_modal() {
                    sm.set_mode(prev, cx_sm);
                }
            }

            if global_search_active {
                sm.capture_prev_mode_for_modal();
                sm.set_mode("prompt", cx_sm);
                sm.set_global_search_open(true, cx_sm);
            } else if sm.global_search_open() {
                sm.set_global_search_open(false, cx_sm);
                if let Some(prev) = sm.take_prev_mode_for_modal() {
                    sm.set_mode(prev, cx_sm);
                }
            }
        });
    }

    pub fn buffer_registry(&self) -> &Entity<BufferRegistry> {
        &self.buffer_registry
    }

    /// Allocate the next monotonic id for an LSP goto request and
    /// record it as the most recent. The spawn site captures the
    /// returned id, the response site compares it against
    /// [`Self::lsp_goto_request_id`], and drops the response when
    /// they differ.
    pub(crate) fn bump_lsp_goto_request_id(&mut self) -> u64 {
        self.lsp_goto_request_seq += 1;
        self.lsp_goto_request_seq
    }

    pub(crate) fn lsp_goto_request_id(&self) -> u64 {
        self.lsp_goto_request_seq
    }

    /// Allocate the next monotonic id for an LSP rename request.
    /// See [`Self::bump_lsp_goto_request_id`].
    pub(crate) fn bump_lsp_rename_request_id(&mut self) -> u64 {
        self.lsp_rename_request_seq += 1;
        self.lsp_rename_request_seq
    }

    pub(crate) fn lsp_rename_request_id(&self) -> u64 {
        self.lsp_rename_request_seq
    }

    /// Look up an open [`Entity<Buffer>`] by absolute path. Returns
    /// `None` for paths the workspace has not opened (or has since
    /// closed). Delegates to the path tracked by
    /// [`crate::fs_watcher_driver::FsWatcherDriver`], which the
    /// `open_paths` flow populates as buffers come online.
    pub fn buffer_for_path(&self, path: &Path, cx: &App) -> Option<Entity<Buffer>> {
        self.fs_watcher_driver.read(cx).buffer_for_path(path)
    }

    pub fn diff_coordinator(&self) -> &Entity<DiffCoordinator> {
        &self.diff_coordinator
    }

    /// Per-workspace yank/paste register store. Backs the unnamed
    /// register plus helix-style named registers; clipboard variants
    /// route through [`crate::globals::ClipboardHostGlobal`] instead.
    pub fn registers(&self) -> &stoat::register::RegisterStore {
        &self.registers
    }

    pub fn registers_mut(&mut self) -> &mut stoat::register::RegisterStore {
        &mut self.registers
    }

    /// Consume the pending [`stoat::register::Register`] target set
    /// by a prior [`SelectRegister`]-style action, falling back to
    /// [`stoat::register::Register::Unnamed`] when nothing is
    /// pending. The pending state is cleared on read; chord-style
    /// register selection is one-shot.
    pub fn consume_selected_register(&mut self) -> stoat::register::Register {
        self.selected_register
            .take()
            .unwrap_or(stoat::register::Register::Unnamed)
    }

    pub fn set_selected_register(&mut self, register: stoat::register::Register) {
        self.selected_register = Some(register);
    }

    pub fn blame_coordinator(&self) -> &Entity<BlameCoordinator> {
        &self.blame_coordinator
    }

    /// Open every path in `paths` as an [`Entity<Editor>`] hosted in
    /// the workspace's pane tree. The first path lands in the
    /// currently focused pane; each additional path triggers
    /// [`PaneTree::split`] with [`Axis::Vertical`] and the editor
    /// goes into the new pane. Empty `paths` is a no-op.
    ///
    /// Path resolution makes each relative path absolute against
    /// the current working directory. Symlinks are **not** resolved
    /// -- canonicalization needs the file to exist, and we want
    /// unsaved new-file paths to open as empty buffers under the
    /// path the user typed.
    ///
    /// Files unreadable today (missing, permission denied, etc.)
    /// open as empty buffers under their absolute path so a
    /// subsequent save writes through. The IO failure is logged at
    /// `tracing::warn`.
    pub fn open_paths(&mut self, paths: &[PathBuf], cx: &mut Context<'_, Self>) {
        if paths.is_empty() {
            return;
        }
        let cwd = std::env::current_dir().ok();
        for (index, path) in paths.iter().enumerate() {
            let absolute = absolute_path(path, cwd.as_deref());
            let pane_id = if index == 0 {
                self.pane_tree.read(cx).focus()
            } else {
                self.pane_tree
                    .update(cx, |tree, cx| tree.split(Axis::Vertical, cx))
            };
            let pane = self
                .pane_tree
                .read(cx)
                .pane(pane_id)
                .expect("pane tree returns its own pane id")
                .clone();
            // BufferRegistry dedupes the buffer; dedupe the pane item too
            // so repeat-opens activate the existing tab instead of
            // stacking duplicates over the same buffer.
            let existing = pane
                .read(cx)
                .items()
                .iter()
                .enumerate()
                .find_map(|(idx, item)| {
                    let editor = item.to_any_view().downcast::<Editor>().ok()?;
                    let path = editor.read(cx).file_path()?.to_path_buf();
                    (path == absolute).then_some(idx)
                });
            if let Some(existing) = existing {
                pane.update(cx, |p, cx| {
                    p.activate(existing, cx);
                });
                continue;
            }
            let editor = self.build_editor_for_path(&absolute, cx);
            pane.update(cx, |p, cx| {
                let index = p.add_item(Box::new(editor), cx);
                p.activate(index, cx);
            });
        }
    }

    /// Construct a fully-wired [`Entity<Editor>`] for `absolute`,
    /// opening (or reusing) the matching buffer in the registry and
    /// installing the LSP / syntax / completion managers. Does not
    /// add the editor to any pane; the caller decides where it lands.
    fn build_editor_for_path(
        &mut self,
        absolute: &Path,
        cx: &mut Context<'_, Self>,
    ) -> Entity<Editor> {
        let text = read_path_or_empty(absolute, cx);
        let (buffer_id, shared) = self
            .buffer_registry
            .update(cx, |registry, cx| registry.open(absolute, &text, cx));
        let buffer = cx.new(|_| Buffer::from_shared(shared));
        buffer.update(cx, |b, cx| {
            b.set_file_path(Some(absolute.to_path_buf()), cx)
        });
        self.register_buffer_watch(buffer_id, absolute.to_path_buf(), buffer.clone(), cx);
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let multi_buffer = {
            let buffer = buffer.clone();
            cx.new(|cx| MultiBuffer::singleton(buffer, cx))
        };
        let display_map = {
            let buffer = buffer.clone();
            cx.new(|cx| DisplayMap::new(buffer, executor, cx))
        };
        let diff_map = {
            let buffer = buffer.clone();
            cx.new(|cx| DiffMap::new(buffer, cx))
        };
        let workspace_handle = cx.weak_entity();
        let workspace_diagnostics = self.diagnostics.clone();
        let editor =
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx));
        editor.update(cx, |ed, cx| {
            ed.set_workspace(Some(workspace_handle));
            ed.set_file_path(Some(absolute.to_path_buf()), cx);
            ed.set_diagnostic_set(Some(workspace_diagnostics), cx);
            ed.install_hover_popup(cx);
            ed.install_completion_popup(cx);
            ed.install_inlay_hints(cx);
            ed.install_code_lens(cx);
            ed.install_semantic_tokens(cx);
            ed.install_signature_help(cx);
            ed.install_syntax_map_updater(cx);
        });
        if editor.read(cx).inline_blame_visible() {
            self.attach_and_refresh_blame(&editor, cx);
        }
        editor
    }

    /// Build a stand-alone preview [`Entity<Editor>`] backed by a
    /// fresh scratch [`Buffer`] in the workspace's [`BufferRegistry`].
    /// The editor carries the same `MultiBuffer` + `DisplayMap` +
    /// `DiffMap` chain a file-bound editor uses plus
    /// `install_syntax_map_updater` so updates to the buffer's
    /// content trigger tree-sitter highlighting, but skips the LSP /
    /// completion / inlay-hints / diagnostics installs the file-bound
    /// editor needs. Callers writing content into the returned
    /// buffer assign a language so the syntax pipeline picks it up.
    pub(crate) fn build_preview_editor(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> (Entity<Buffer>, Entity<Editor>) {
        let shared = self
            .buffer_registry
            .update(cx, |registry, cx| registry.new_scratch(cx).1);
        let buffer = cx.new(|_| Buffer::from_shared(shared));
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let multi_buffer = {
            let buffer = buffer.clone();
            cx.new(|cx| MultiBuffer::singleton(buffer, cx))
        };
        let display_map = {
            let buffer = buffer.clone();
            cx.new(|cx| DisplayMap::new(buffer, executor, cx))
        };
        let diff_map = {
            let buffer = buffer.clone();
            cx.new(|cx| DiffMap::new(buffer, cx))
        };
        let weak_workspace = cx.weak_entity();
        let editor =
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx));
        editor.update(cx, |ed, cx| {
            ed.set_workspace(Some(weak_workspace));
            ed.install_syntax_map_updater(cx);
        });
        (buffer, editor)
    }

    pub fn uid(&self) -> stoat::workspace::WorkspaceUid {
        self.uid
    }

    pub fn is_fresh(&self) -> bool {
        self.is_fresh
    }

    /// Mark the workspace as no longer fresh. Persistence skips
    /// fresh workspaces so abandoned sessions never write state.
    pub fn mark_dirty(&mut self) {
        self.is_fresh = false;
    }

    /// Recently confirmed command-palette queries, oldest first.
    pub fn command_palette_history(&self) -> &VecDeque<String> {
        &self.command_palette_history
    }

    /// Record a confirmed command-palette query as the most recent
    /// entry, deduplicating earlier occurrences and capping the list
    /// at [`crate::command_palette::HISTORY_LIMIT`].
    pub fn push_command_palette_query(&mut self, query: String) {
        crate::command_palette::record_query_capped(
            &mut self.command_palette_history,
            query,
            crate::command_palette::HISTORY_LIMIT,
        );
    }

    /// Build the serializable v1 snapshot of every part of the
    /// workspace this iteration knows how to round-trip.
    pub fn to_state(&self, cx: &App) -> crate::workspace_persist::WorkspaceStateV1 {
        let pane_tree = self.pane_tree.read(cx);
        let panes_inner = pane_tree.inner_clone();
        let focused_pane = pane_tree.focus();
        let mut pane_items = std::collections::BTreeMap::new();
        for id in pane_tree.split_pane_ids() {
            let Some(pane_entity) = pane_tree.pane(id) else {
                continue;
            };
            let pane_ref = pane_entity.read(cx);
            let snap = crate::workspace_persist::snapshot_pane_items(pane_ref, cx, id);
            pane_items.insert(id, snap);
        }
        let buffers = self.buffer_registry.read(cx).snapshot();
        let docks: Vec<crate::workspace_persist::DockSnapV1> = self
            .docks
            .iter()
            .enumerate()
            .map(|(idx, dock_entity)| {
                let dock = dock_entity.read(cx);
                crate::workspace_persist::snapshot_dock(dock, cx, idx)
            })
            .collect();
        crate::workspace_persist::WorkspaceStateV1 {
            uid: self.uid,
            name: self.name.to_string(),
            git_root: self.git_root.clone(),
            panes: panes_inner,
            focused_pane,
            pane_items,
            docks,
            buffers,
            command_palette_history: self.command_palette_history.clone(),
        }
    }

    /// Save the workspace's state to its canonical
    /// `<XDG_STATE_HOME>/stoat/workspaces/<git_root_hash>/<uid>.ron`
    /// path. Silent no-op when no [`FsHostGlobal`] is installed
    /// (headless tests) or [`Self::is_fresh`] is true. IO and path
    /// resolution failures are logged via `tracing::warn!` and
    /// swallowed -- this is invoked from background lifecycle
    /// triggers (periodic timer, release observer) where there is
    /// no surface to propagate an error to.
    pub fn save_state_to_default_path(&self, cx: &App) {
        if self.is_fresh {
            return;
        }
        let Some(fs) = cx.try_global::<FsHostGlobal>().map(|g| g.0.clone()) else {
            return;
        };
        let path = match crate::workspace_persist::state_path(&self.git_root, self.uid, &*fs) {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!(?err, git_root = ?self.git_root, "resolve workspace state path failed");
                return;
            },
        };
        if let Err(err) = self.save_state(&path, &*fs, cx) {
            tracing::warn!(?err, ?path, "workspace save_state failed");
        }
    }

    /// Serialize the workspace and write it atomically to `path`.
    /// No-op when [`Self::is_fresh`] is true so untouched workspaces
    /// don't leave files on disk. Parent directory is created if
    /// missing.
    pub fn save_state(
        &self,
        path: &Path,
        fs: &dyn stoat::host::FsHost,
        cx: &App,
    ) -> std::io::Result<()> {
        if self.is_fresh {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs.create_dir_all(parent)?;
        }
        let state = self.to_state(cx);
        let body = ron::ser::to_string_pretty(&state, ron::ser::PrettyConfig::default())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let tmp = path.with_extension("ron.tmp");
        fs.write(&tmp, body.as_bytes())?;
        fs.rename(&tmp, path)?;
        Ok(())
    }

    /// Replace `self`'s persistable state with `state`. Buffers
    /// rehydrate via [`stoat::buffer::TextBuffer::from_history`]; the
    /// pane tree shape rebuilds with empty panes, then per-pane
    /// editor file paths re-open via [`Self::build_editor_for_path`]
    /// which reuses the rehydrated buffers in the registry.
    pub fn apply_state(
        &mut self,
        state: crate::workspace_persist::WorkspaceStateV1,
        cx: &mut Context<'_, Self>,
    ) {
        self.uid = state.uid;
        self.git_root = state.git_root.clone();
        self.name = SharedString::from(state.name.clone());
        self.is_fresh = false;
        self.command_palette_history = state.command_palette_history;
        self.buffer_registry
            .update(cx, |registry, cx| registry.restore_from(state.buffers, cx));
        self.pane_tree
            .update(cx, |tree, cx| tree.apply_state(state.panes, cx));
        for (pane_id, items) in state.pane_items {
            let pane = match self.pane_tree.read(cx).pane(pane_id).cloned() {
                Some(p) => p,
                None => continue,
            };
            let mut materialized: usize = 0;
            for snap in &items.items {
                match snap.kind {
                    crate::item::ItemKind::Editor => {
                        let path = snap
                            .blob
                            .get("file_path")
                            .and_then(|v| v.as_str())
                            .map(PathBuf::from);
                        let Some(path) = path else {
                            tracing::info!(
                                pane_id = ?pane_id,
                                "skipping editor item with no file_path during restore"
                            );
                            continue;
                        };
                        let editor = self.build_editor_for_path(&path, cx);
                        let folds = crate::workspace_persist::folds_from_blob(&snap.blob);
                        if !folds.is_empty() {
                            let display_map = editor.read(cx).display_map().clone();
                            display_map.update(cx, |dm, dm_cx| dm.fold(folds, dm_cx));
                        }
                        pane.update(cx, |p, cx| {
                            p.add_item(Box::new(editor), cx);
                        });
                        materialized += 1;
                    },
                    other => {
                        tracing::info!(
                            pane_id = ?pane_id,
                            kind = ?other,
                            "skipping non-editor item during workspace restore v1"
                        );
                    },
                }
            }
            if materialized > 0 {
                let active = items.active_index.min(materialized - 1);
                pane.update(cx, |p, cx| {
                    p.activate(active, cx);
                });
            }
            let active_editor = pane
                .read(cx)
                .active_item()
                .and_then(|item| item.to_any_view().downcast::<Editor>().ok());
            if let Some(editor) = active_editor {
                editor.update(cx, |ed, cx| {
                    ed.set_minimap_visible(items.minimap_visible, cx)
                });
            }
        }
        self.pane_tree
            .update(cx, |tree, cx| tree.set_focus(state.focused_pane, cx));

        self.docks.clear();
        for (idx, snap) in state.docks.into_iter().enumerate() {
            if let Some(path) = snap.editor_path {
                let editor = self.build_editor_for_path(&path, cx);
                self.add_dock(Box::new(editor), snap.side, snap.default_width, cx);
            } else if let Some(tree_snap) = snap.project_tree {
                let git_root = self.git_root.clone();
                let fs = cx.global::<FsHostGlobal>().0.clone();
                let tree = cx.new(|cx| {
                    let mut tree = ProjectTree::new(git_root, fs, cx);
                    tree.set_expanded(tree_snap.expanded);
                    tree
                });
                self.add_dock(Box::new(tree), snap.side, snap.default_width, cx);
            } else {
                tracing::info!(
                    dock_index = idx,
                    "skipping dock with no restorable item during workspace restore v1"
                );
                continue;
            }
            let dock = self.docks.last().cloned().expect("just-added dock present");
            dock.update(cx, |d, cx| {
                d.set_visibility(snap.visibility, cx);
            });
        }
    }

    /// Read and apply the persisted state at `path`.
    pub fn restore_state(
        &mut self,
        path: &Path,
        fs: &dyn stoat::host::FsHost,
        cx: &mut Context<'_, Self>,
    ) -> std::io::Result<()> {
        let mut buf = Vec::new();
        fs.read(path, &mut buf)?;
        let body = String::from_utf8(buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let state: crate::workspace_persist::WorkspaceStateV1 = ron::from_str(&body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        self.apply_state(state, cx);
        Ok(())
    }

    /// Discover the most-recently-modified persisted workspace
    /// under `anchor` and apply it. Returns `Ok(true)` when a
    /// workspace was restored, `Ok(false)` when the anchor has no
    /// persisted state, and surfaces the underlying IO / parse
    /// error otherwise. Backs the `--continue` binary flag.
    pub fn restore_most_recent(
        &mut self,
        anchor: &Path,
        fs: &dyn stoat::host::FsHost,
        cx: &mut Context<'_, Self>,
    ) -> std::io::Result<bool> {
        let files = crate::workspace_persist::list_workspace_files(anchor, fs)?;
        let Some(newest) = files.into_iter().next() else {
            return Ok(false);
        };
        self.restore_state(&newest, fs, cx)?;
        Ok(true)
    }

    pub fn docks(&self) -> &[Entity<Dock>] {
        &self.docks
    }

    pub fn modal_layer(&self) -> &Entity<ModalLayer> {
        &self.modal_layer
    }

    /// Open a modal of type `V` over the workspace, or close it if a
    /// modal of the same type is already active. A different active
    /// modal is replaced. Delegates to [`ModalLayer::toggle_modal`].
    pub fn toggle_modal<V, B>(&mut self, window: &mut Window, cx: &mut App, build: B)
    where
        V: ModalView,
        B: FnOnce(&mut Window, &mut Context<'_, V>) -> V,
    {
        self.modal_layer
            .update(cx, |layer, cx| layer.toggle_modal(window, cx, build));
    }

    /// Close the currently active modal if any. Returns `false` when
    /// no modal is active or the modal's `on_before_dismiss` vetoes.
    /// Delegates to [`ModalLayer::hide_modal`].
    pub fn dismiss_modal(&mut self, window: &mut Window, cx: &mut App) -> bool {
        self.modal_layer
            .update(cx, |layer, cx| layer.hide_modal(window, cx))
    }

    /// Raise `toast` on the workspace's bottom-right toast overlay.
    /// Transient kinds auto-dismiss; errors persist until dismissed.
    pub fn show_toast(&mut self, toast: Toast, cx: &mut Context<'_, Self>) {
        self.toast_view.update(cx, |view, cx| view.push(toast, cx));
    }

    /// Dismiss the toast with `id` from the overlay, if still showing.
    pub fn dismiss_toast(&mut self, id: ToastId, cx: &mut Context<'_, Self>) {
        self.toast_view.update(cx, |view, cx| view.dismiss(id, cx));
    }

    /// Re-decode the active editor's file with `encoding`, replacing the
    /// buffer's contents. A scratch buffer (no path) records the encoding
    /// without altering content. A lossy decode raises a warning toast.
    pub fn apply_encoding_to_active_buffer(
        &mut self,
        encoding: Encoding,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(editor) = self
            .input_state_machine
            .read(cx)
            .active_editor()
            .cloned()
            .and_then(|weak| weak.upgrade())
        else {
            return;
        };
        let Some(buffer) = editor
            .read(cx)
            .multi_buffer()
            .read(cx)
            .as_singleton()
            .cloned()
        else {
            return;
        };

        let Some(path) = editor.read(cx).file_path().map(Path::to_path_buf) else {
            let current = buffer.read(cx).text();
            buffer.update(cx, |b, cx| b.set_encoding(encoding, &current, cx));
            return;
        };

        let fs = cx.global::<FsHostGlobal>().0.clone();
        let mut bytes = Vec::new();
        if let Err(err) = fs.read(&path, &mut bytes) {
            tracing::warn!(?path, %err, "encoding: re-decode read failed");
            return;
        }

        let (text, lossy) = stoat::buffer::decode(&bytes, encoding);
        buffer.update(cx, |b, cx| b.set_encoding(encoding, &text, cx));
        if lossy {
            self.show_toast(
                Toast::warning(format!(
                    "Decoded as {} with replacement characters",
                    encoding.as_str()
                )),
                cx,
            );
        }
    }

    pub fn status_bar(&self) -> &Entity<StatusBar> {
        &self.status_bar
    }

    pub fn workspace_label(&self) -> &Entity<WorkspaceLabel> {
        &self.workspace_label
    }

    pub fn active_file_label(&self) -> &Entity<ActiveFileLabel> {
        &self.active_file_label
    }

    pub fn cursor_position(&self) -> &Entity<CursorPosition> {
        &self.cursor_position
    }

    pub fn count_prefix(&self) -> &Entity<CountPrefix> {
        &self.count_prefix
    }

    pub fn diagnostics_badge(&self) -> &Entity<DiagnosticsBadge> {
        &self.diagnostics_badge
    }

    pub fn lsp_state(&self) -> &Entity<LspState> {
        &self.lsp_state
    }

    pub fn diagnostics(&self) -> &Entity<DiagnosticSet> {
        &self.diagnostics
    }

    pub fn lsp_progress(&self) -> &Entity<LspProgress> {
        &self.lsp_progress
    }

    pub fn review_progress(&self) -> &Entity<ReviewProgress> {
        &self.review_progress
    }

    pub fn search_indicator(&self) -> &Entity<SearchQueryIndicator> {
        &self.search_indicator
    }

    /// Register a status item at the left side of the status bar.
    /// Fires the item's [`StatusItemView::set_active_pane_item`]
    /// callback immediately so it picks up the workspace's current
    /// active pane item on registration.
    pub fn add_status_item_left<V>(&mut self, item: Entity<V>, cx: &mut Context<'_, Self>)
    where
        V: StatusItemView,
    {
        let initial = self.active_pane_item(cx);
        self.status_bar.update(cx, |bar, cx| {
            bar.add_left_item(item.clone(), cx);
        });
        item.update(cx, |item, cx| {
            item.set_active_pane_item(initial.as_deref(), cx);
        });
    }

    /// Register a status item at the right side of the status bar.
    /// Right-side items render in reverse-registration order so the
    /// most-recently-added item lands at the window's right edge.
    pub fn add_status_item_right<V>(&mut self, item: Entity<V>, cx: &mut Context<'_, Self>)
    where
        V: StatusItemView,
    {
        let initial = self.active_pane_item(cx);
        self.status_bar.update(cx, |bar, cx| {
            bar.add_right_item(item.clone(), cx);
        });
        item.update(cx, |item, cx| {
            item.set_active_pane_item(initial.as_deref(), cx);
        });
    }

    fn active_pane_item(&self, cx: &App) -> Option<Box<dyn ItemHandle>> {
        let tree = self.pane_tree.read(cx);
        let focus = tree.focus();
        tree.pane(focus)
            .and_then(|p| p.read(cx).active_item().map(ItemHandle::boxed_clone))
    }

    fn broadcast_active_pane_item(&mut self, cx: &mut Context<'_, Self>) {
        let active = self.active_pane_item(cx);
        let status_bar = self.status_bar.clone();
        status_bar.update(cx, |bar, cx| {
            bar.set_active_pane_item(active.as_deref(), cx);
        });

        let active_id = active.as_ref().map(|item| item.item_id());
        if active_id != self.last_active_item_id {
            self.last_active_item_id = active_id;
            let mode = match active.as_ref().map(|item| item.item_kind(cx)) {
                Some(crate::item::ItemKind::Review) => "review",
                Some(crate::item::ItemKind::Rebase) => "rebase",
                Some(crate::item::ItemKind::Conflict) => "conflict",
                _ => "normal",
            };
            self.set_input_mode(mode, cx);
        }

        self.refresh_active_editor_subscription(cx);
        cx.notify();
    }

    /// Register `path` with the workspace's fs-watch surfaces:
    /// 1. Call [`crate::host::FsWatchHost::watch`] (storing the returned [`WatchToken`]) so
    ///    external edits to the file enqueue a [`crate::host::FsWatchEvent`] on the host's queue.
    /// 2. Call [`FsWatcherDriver::track`] so the per-workspace driver routes those events to the
    ///    matching `buffer` entity.
    ///
    /// Idempotent for an already-watched `path`: the registry's
    /// own dedup keeps `buffer_id` stable, and we keep one token
    /// per path. Failed watches log at `tracing::warn`; the
    /// buffer still opens.
    fn register_buffer_watch(
        &mut self,
        buffer_id: BufferId,
        path: PathBuf,
        buffer: Entity<Buffer>,
        cx: &mut Context<'_, Self>,
    ) {
        self.buffer_paths.insert(buffer_id, path.clone());
        if !self.fs_watch_tokens.contains_key(&path) {
            let host = cx.global::<FsWatchHostGlobal>().0.clone();
            match host.watch(&path) {
                Ok(token) => {
                    self.fs_watch_tokens.insert(path.clone(), token);
                },
                Err(err) => {
                    tracing::warn!(
                        ?path,
                        %err,
                        "workspace: fs watch registration failed",
                    );
                },
            }
        }
        self.fs_watcher_driver.update(cx, |driver, _| {
            driver.track(path, buffer);
        });
    }

    /// Drop the watch surfaces for the buffer at `buffer_id`,
    /// inverse of [`Self::register_buffer_watch`]. Triggered by
    /// [`BufferRegistryEvent::BufferRemoved`].
    fn release_buffer_watch(&mut self, buffer_id: BufferId, cx: &mut Context<'_, Self>) {
        let Some(path) = self.buffer_paths.remove(&buffer_id) else {
            return;
        };
        if let Some(token) = self.fs_watch_tokens.remove(&path) {
            let host = cx.global::<FsWatchHostGlobal>().0.clone();
            host.unwatch(token);
        }
        self.fs_watcher_driver.update(cx, |driver, _| {
            driver.untrack(&path);
        });
    }

    /// Re-bind [`Workspace::_active_editor_subscription`] to the focused
    /// pane's active editor (if any). Each [`EditorEvent::Changed`]
    /// notifies the workspace so the window-title formatter picks up
    /// dirty-state transitions in the active buffer. Non-editor active
    /// items clear the subscription -- their dirty state is constant
    /// or rare enough that polling on pane-tree changes suffices.
    fn refresh_active_editor_subscription(&mut self, cx: &mut Context<'_, Self>) {
        let editor = self
            .active_pane_item(cx)
            .and_then(|item| item.to_any_view().downcast::<Editor>().ok());
        self._active_editor_subscription = editor.map(|editor| {
            cx.subscribe(&editor, |_, _, _event: &EditorEvent, cx| {
                cx.notify();
            })
        });
    }

    /// Re-bind [`Workspace::_pane_subscriptions`] to every pane in the
    /// current tree. Each pane's [`PaneEvent`] notifies the workspace
    /// so per-pane tab changes (active item, item added/removed)
    /// update the window-title formatter and the status bar without
    /// going through [`PaneTreeEvent`], which only fires on tree
    /// structure changes.
    fn refresh_pane_subscriptions(&mut self, cx: &mut Context<'_, Self>) {
        let panes: Vec<Entity<Pane>> = self
            .pane_tree
            .read(cx)
            .split_pane_ids()
            .into_iter()
            .filter_map(|id| self.pane_tree.read(cx).pane(id).cloned())
            .collect();
        self._pane_subscriptions = panes
            .into_iter()
            .map(|pane| {
                cx.subscribe(&pane, |workspace, _, _event: &PaneEvent, cx| {
                    workspace.broadcast_active_pane_item(cx);
                    workspace.broadcast_active_editor(cx);
                })
            })
            .collect();
    }

    /// Format the OS-level window title from the workspace name plus
    /// the focused pane's active item label and dirty state. Matches
    /// the tab-strip dirty marker convention at
    /// [`crate::tab_bar`] (trailing ` [+]` when dirty). Falls back to
    /// the workspace name alone when no item is active in the focused
    /// pane.
    fn compute_window_title(&self, cx: &App) -> SharedString {
        let Some(item) = self.active_pane_item(cx) else {
            return self.name.clone();
        };
        let label = item.tab_label(cx);
        let dirty = if item.is_dirty(cx) { " [+]" } else { "" };
        SharedString::from(format!("{} -- {}{}", self.name, label, dirty))
    }

    /// Push the focused pane's active editor (if any) into the
    /// [`InputStateMachine`]'s `active_editor` and
    /// `editor_focus_target` slots. The active item is either an
    /// [`Editor`] directly or a pane item that hosts an embedded
    /// editor (today: the Run pane's command line); either flavor
    /// lands in the state machine so IME commits and dispatched
    /// editor actions act on the right buffer. Non-editor items
    /// (or no active item) clear both slots. Drives the motion /
    /// save / save-selection / jump dispatch helpers on [`Workspace`]
    /// that look up the active editor through the state machine.
    fn broadcast_active_editor(&mut self, cx: &mut Context<'_, Self>) {
        let editor = self
            .active_pane_item(cx)
            .and_then(|item| Self::editor_for_pane_item(item.as_ref(), cx));
        let focus_target = editor
            .as_ref()
            .map(|_| self.editor_input.read(cx).focus_handle().clone());
        let weak_editor = editor.as_ref().map(Entity::downgrade);
        self.input_state_machine.update(cx, |sm, _| {
            sm.set_active_editor(weak_editor);
            sm.set_editor_focus_target(focus_target);
        });
    }

    /// Extract the editor a pane item exposes to the workspace's
    /// IME / action pipeline. Items that are themselves [`Editor`]s
    /// return their own handle; items that host an embedded editor
    /// command line (today: the Run pane) return that. Other items
    /// return `None`, leaving `active_editor` cleared while they
    /// are focused.
    fn editor_for_pane_item(item: &dyn ItemHandle, cx: &App) -> Option<Entity<Editor>> {
        let view = item.to_any_view();
        if let Ok(editor) = view.clone().downcast::<Editor>() {
            return Some(editor);
        }
        if let Ok(run) = view.downcast::<crate::run_pane::Run>() {
            return Some(run.read(cx).input.clone());
        }
        None
    }

    pub fn input_state_machine(&self) -> &Entity<InputStateMachine> {
        &self.input_state_machine
    }

    /// IME / typed-character bridge into [`InputStateMachine::text_input`].
    /// Editor render paths register it with the platform input handler via
    /// `Window::handle_input` against [`EditorInput::focus_handle`] so the
    /// OS text-input pipeline routes through the workspace's state machine.
    pub fn editor_input(&self) -> &Entity<EditorInput> {
        &self.editor_input
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub fn set_name(&mut self, name: impl Into<SharedString>, cx: &mut Context<'_, Self>) -> bool {
        let name = name.into();
        if self.name == name {
            return false;
        }
        self.name = name.clone();
        self.workspace_label
            .update(cx, |label, cx| label.set_name(name, cx));
        cx.emit(WorkspaceEvent::NameChanged);
        cx.notify();
        true
    }

    /// Set the working directory the workspace resolves paths against.
    /// File-finder, review-open, git discovery, and the save-state path
    /// read this root on demand, so they follow the change; the diff and
    /// blame coordinators and the active-file label captured the root at
    /// construction and are updated here so they follow it too.
    pub fn set_git_root(&mut self, git_root: impl Into<PathBuf>, cx: &mut Context<'_, Self>) {
        let git_root = git_root.into();
        self.git_root = git_root.clone();
        self.diff_coordinator
            .update(cx, |c, cx| c.set_git_root(git_root.clone(), cx));
        self.blame_coordinator
            .update(cx, |c, cx| c.set_git_root(git_root.clone(), cx));
        self.active_file_label
            .update(cx, move |l, cx| l.set_workspace_root(git_root, cx));
        cx.notify();
    }

    /// Change the active workspace's working directory to `path`. Empty
    /// paths are ignored. Reached through the command palette's
    /// argument-collection flow for the `SetCwd` action.
    fn dispatch_set_cwd(&mut self, path: &str, cx: &mut Context<'_, Self>) {
        if path.is_empty() {
            tracing::warn!("SetCwd: empty path ignored");
            return;
        }
        self.set_git_root(PathBuf::from(path), cx);
    }

    pub fn add_dock(
        &mut self,
        item: Box<dyn ItemHandle>,
        side: DockSide,
        default_extent: u16,
        cx: &mut Context<'_, Self>,
    ) -> usize {
        let dock = cx.new(|cx| Dock::new(item, side, default_extent, cx));
        let index = self.docks.len();
        self.docks.push(dock);
        cx.emit(WorkspaceEvent::DockAdded { index });
        cx.notify();
        index
    }

    /// Remove and return the dock at `index`. Out-of-range indices
    /// return None without side effects.
    pub fn remove_dock(
        &mut self,
        index: usize,
        cx: &mut Context<'_, Self>,
    ) -> Option<Entity<Dock>> {
        if index >= self.docks.len() {
            return None;
        }
        let removed = self.docks.remove(index);
        cx.emit(WorkspaceEvent::DockRemoved { index });
        cx.notify();
        Some(removed)
    }

    /// Handle the [`Quit`] action: close the focused pane, then
    /// exit the application when that pane was the last remaining
    /// one. [`PaneTree::close`] returns `false` for the last-pane
    /// case, which is how this distinguishes "closed a pane" from
    /// "refused to close the last pane".
    pub fn handle_quit(&self, cx: &mut Context<'_, Self>) {
        let closed = self.pane_tree.update(cx, |tree, cx| {
            let focus = tree.focus();
            tree.close(focus, cx)
        });
        if !closed {
            cx.quit();
        }
    }

    /// Handle `QuitAll`: quit immediately when no buffer is dirty;
    /// otherwise open the [`crate::quit_confirm::QuitConfirmModal`]
    /// and wait for the user to confirm or cancel.
    pub fn handle_quit_all(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let dirty = self.buffer_registry.read(cx).dirty_buffers();
        if dirty.is_empty() {
            cx.quit();
            return;
        }
        crate::quit_confirm::open_quit_confirm(self, &dirty, window, cx);
    }

    /// Handle `CloseWorkspace`: close the current window when no
    /// buffer is dirty; otherwise open the same
    /// [`crate::quit_confirm::QuitConfirmModal`] used by `QuitAll`,
    /// configured to close the window on confirm instead of
    /// quitting the app. The workspace's release observer persists
    /// state when the entity drops as the window closes.
    pub fn handle_close_workspace(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let dirty = self.buffer_registry.read(cx).dirty_buffers();
        if dirty.is_empty() {
            window.remove_window();
            return;
        }
        crate::quit_confirm::open_close_workspace_confirm(self, &dirty, window, cx);
    }

    /// Dispatch a Stoat action resolved by the input state machine.
    /// Routes by [`ActionKind`]: pane-targeted variants update
    /// [`Entity<PaneTree>`], root-targeted variants mutate the
    /// workspace itself. Editor- and modal-targeted variants fall
    /// through to a `tracing::trace` and are wired by the items
    /// that build their target entities (editor render, review
    /// item, modals, etc.).
    ///
    /// Active-modal routing runs first: if the top modal's
    /// [`ModalView::handle_action`] returns `true` (the picker uses
    /// this for select / confirm / dismiss), the workspace's own
    /// match is skipped. This is how `Enter` / `Ctrl-V` / `Ctrl-X`
    /// confirm the picker without consulting the editor or pane
    /// dispatch paths.
    pub fn dispatch_action(
        &mut self,
        action: Box<dyn stoat_action::Action>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let _span = tracing::trace_span!("workspace.dispatch_action").entered();
        if render_stats_enabled(cx) {
            self.frame_timer.borrow_mut().start_frame(Instant::now());
        }
        let handled_by_modal = self
            .modal_layer
            .update(cx, |layer, cx| layer.handle_action(&*action, window, cx));
        if handled_by_modal {
            return;
        }
        match action.kind() {
            ActionKind::Quit => self.handle_quit(cx),
            ActionKind::QuitAll => self.handle_quit_all(window, cx),
            ActionKind::SplitRight => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.split(Axis::Vertical, cx);
                });
            },
            ActionKind::SplitDown => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.split(Axis::Horizontal, cx);
                });
            },
            ActionKind::SplitNewRight => self.dispatch_split_new(Axis::Vertical, cx),
            ActionKind::SplitNewDown => self.dispatch_split_new(Axis::Horizontal, cx),
            ActionKind::FocusLeft => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.focus_direction(Direction::Left, cx);
                });
            },
            ActionKind::FocusRight => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.focus_direction(Direction::Right, cx);
                });
            },
            ActionKind::FocusUp => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.focus_direction(Direction::Up, cx);
                });
            },
            ActionKind::FocusDown => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.focus_direction(Direction::Down, cx);
                });
            },
            ActionKind::FocusNext => {
                self.pane_tree.update(cx, |tree, cx| tree.focus_next(cx));
            },
            ActionKind::FocusPrev => {
                self.pane_tree.update(cx, |tree, cx| tree.focus_prev(cx));
            },
            ActionKind::ClosePane => {
                self.pane_tree.update(cx, |tree, cx| {
                    let focus = tree.focus();
                    tree.close(focus, cx);
                });
            },
            ActionKind::CloseBuffer => self.close_active_buffer(cx),
            ActionKind::ToggleDockLeft => self.toggle_dock(DockSide::Left, cx),
            ActionKind::ToggleDockRight => self.toggle_dock(DockSide::Right, cx),
            ActionKind::NewWorkspace => self.dispatch_new_workspace(cx),
            ActionKind::CopyWorkspace => self.dispatch_copy_workspace(cx),
            ActionKind::CloseWorkspace => self.handle_close_workspace(window, cx),
            ActionKind::RenameWorkspace => {
                let name = action
                    .as_any()
                    .downcast_ref::<stoat_action::RenameWorkspace>()
                    .map(|a| a.name.clone())
                    .unwrap_or_default();
                if name.is_empty() {
                    crate::rename_workspace_modal::open_rename_workspace(self, window, cx);
                } else {
                    self.set_name(name, cx);
                }
            },
            ActionKind::SetCwd => {
                let path = action
                    .as_any()
                    .downcast_ref::<stoat_action::SetCwd>()
                    .map(|a| a.path.clone())
                    .unwrap_or_default();
                self.dispatch_set_cwd(&path, cx);
            },
            ActionKind::Pwd => {
                tracing::info!(working_directory = %self.git_root().display(), "pwd");
            },
            ActionKind::Env => {
                if let Some(env) = cx.try_global::<EnvHostGlobal>() {
                    let vars: Vec<(String, String)> = env
                        .0
                        .vars()
                        .into_iter()
                        .filter(|(name, _)| name.starts_with("STOAT_"))
                        .collect();
                    tracing::info!(?vars, "env");
                }
            },
            ActionKind::CloseOtherPanes => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.close_others(cx);
                });
            },
            ActionKind::SetActivePane => {
                if let Some(set) = action
                    .as_any()
                    .downcast_ref::<crate::actions::SetActivePane>()
                {
                    let id = stoat::pane::PaneId::from_ffi(set.pane_id);
                    self.pane_tree.update(cx, |tree, cx| tree.set_focus(id, cx));
                }
            },
            ActionKind::DismissModal => {
                self.dismiss_modal(window, cx);
                if let Some(editor) = self
                    .input_state_machine
                    .read(cx)
                    .active_editor()
                    .cloned()
                    .and_then(|w| w.upgrade())
                {
                    editor.update(cx, |ed, cx| ed.set_hover_position(None, cx));
                }
            },
            ActionKind::ClickAt => {
                if let Some(click) = action.as_any().downcast_ref::<crate::actions::ClickAt>() {
                    let weak_editor = self.input_state_machine.read(cx).active_editor().cloned();
                    if let Some(editor) = weak_editor.and_then(|w| w.upgrade()) {
                        let (row, col) = (click.row, click.col);
                        editor.update(cx, |ed, cx| ed.set_cursor_at_grid(row, col, cx));
                    }
                }
            },
            ActionKind::DragSelectTo => {
                if let Some(drag) = action
                    .as_any()
                    .downcast_ref::<crate::actions::DragSelectTo>()
                {
                    let weak_editor = self.input_state_machine.read(cx).active_editor().cloned();
                    if let Some(editor) = weak_editor.and_then(|w| w.upgrade()) {
                        let (row, col) = (drag.row, drag.col);
                        editor.update(cx, |ed, cx| {
                            ed.extend_primary_selection_to_grid(row, col, cx)
                        });
                    }
                }
            },
            ActionKind::HoverAt => {
                if let Some(hover) = action.as_any().downcast_ref::<crate::actions::HoverAt>() {
                    let weak_editor = self.input_state_machine.read(cx).active_editor().cloned();
                    if let Some(editor) = weak_editor.and_then(|w| w.upgrade()) {
                        let (row, col) = (hover.row, hover.col);
                        editor.update(cx, |ed, cx| ed.set_hover_position(Some((row, col)), cx));
                    }
                }
            },
            ActionKind::Hover => self.dispatch_hover(cx),
            ActionKind::AcceptCompletion => {
                let popup = self
                    .input_state_machine
                    .read(cx)
                    .active_editor()
                    .cloned()
                    .and_then(|w| w.upgrade())
                    .and_then(|editor| editor.read(cx).completion_popup().cloned())
                    .filter(|p| p.read(cx).is_visible());
                if let Some(popup) = popup {
                    popup.update(cx, |p, cx| p.accept(cx));
                } else {
                    crate::editor::actions::edit::handle_insert_newline(self, cx);
                }
            },
            ActionKind::SmartTab => {
                crate::editor::actions::smart_tab::handle_smart_tab(self, cx);
            },
            ActionKind::TriggerCompletion => {
                crate::editor::actions::smart_tab::handle_trigger_completion(self, cx);
            },
            ActionKind::Increment => {
                let count = self.take_count(cx);
                crate::editor::actions::numbers::handle_increment(self, count, cx);
            },
            ActionKind::Decrement => {
                let count = self.take_count(cx);
                crate::editor::actions::numbers::handle_decrement(self, count, cx);
            },
            ActionKind::CodeAction => self.dispatch_code_action(window, cx),
            ActionKind::FormatSelections => self.dispatch_format_selections(window, cx),
            ActionKind::GotoDefinition => {
                crate::lsp::goto::spawn_goto(
                    self,
                    crate::lsp::goto::LspGotoKind::Definition,
                    window,
                    cx,
                );
            },
            ActionKind::GotoTypeDefinition => {
                crate::lsp::goto::spawn_goto(
                    self,
                    crate::lsp::goto::LspGotoKind::TypeDefinition,
                    window,
                    cx,
                );
            },
            ActionKind::GotoImplementation => {
                crate::lsp::goto::spawn_goto(
                    self,
                    crate::lsp::goto::LspGotoKind::Implementation,
                    window,
                    cx,
                );
            },
            ActionKind::GotoReferences => self.dispatch_goto_references(window, cx),
            ActionKind::RenameSymbol => self.dispatch_rename_symbol(window, cx),
            ActionKind::RepeatLastMotion => self.dispatch_repeat_last_motion(cx),
            ActionKind::MoveLeft => self.dispatch_move_horizontal(-1, false, cx),
            ActionKind::MoveRight => self.dispatch_move_horizontal(1, false, cx),
            ActionKind::MoveUp => self.dispatch_move_vertical(-1, false, cx),
            ActionKind::MoveDown => self.dispatch_move_vertical(1, false, cx),
            ActionKind::PageUp => {
                self.dispatch_page_motion(crate::editor::actions::movement::PageDir::Up, false, cx)
            },
            ActionKind::PageDown => self.dispatch_page_motion(
                crate::editor::actions::movement::PageDir::Down,
                false,
                cx,
            ),
            ActionKind::HalfPageUp => {
                self.dispatch_page_motion(crate::editor::actions::movement::PageDir::Up, true, cx)
            },
            ActionKind::HalfPageDown => {
                self.dispatch_page_motion(crate::editor::actions::movement::PageDir::Down, true, cx)
            },
            ActionKind::MoveNextWordStart => self.dispatch_move_word(
                crate::editor::actions::movement::WordTarget::NextStart,
                false,
                cx,
            ),
            ActionKind::MoveNextWordEnd => self.dispatch_move_word(
                crate::editor::actions::movement::WordTarget::NextEnd,
                false,
                cx,
            ),
            ActionKind::MovePrevWordStart => self.dispatch_move_word(
                crate::editor::actions::movement::WordTarget::PrevStart,
                false,
                cx,
            ),
            ActionKind::MovePrevWordEnd => self.dispatch_move_word(
                crate::editor::actions::movement::WordTarget::PrevEnd,
                false,
                cx,
            ),
            ActionKind::MoveNextLongWordStart => self.dispatch_move_word(
                crate::editor::actions::movement::WordTarget::NextLongStart,
                false,
                cx,
            ),
            ActionKind::MoveNextLongWordEnd => self.dispatch_move_word(
                crate::editor::actions::movement::WordTarget::NextLongEnd,
                false,
                cx,
            ),
            ActionKind::MovePrevLongWordStart => self.dispatch_move_word(
                crate::editor::actions::movement::WordTarget::PrevLongStart,
                false,
                cx,
            ),
            ActionKind::MovePrevLongWordEnd => self.dispatch_move_word(
                crate::editor::actions::movement::WordTarget::PrevLongEnd,
                false,
                cx,
            ),
            ActionKind::GotoLineStart => self.dispatch_simple_goto(GotoKind::LineStart, false, cx),
            ActionKind::GotoLineEnd => self.dispatch_simple_goto(GotoKind::LineEnd, false, cx),
            ActionKind::GotoFirstNonwhitespace => {
                self.dispatch_simple_goto(GotoKind::FirstNonwhitespace, false, cx)
            },
            ActionKind::GotoFileStart => self.dispatch_simple_goto(GotoKind::FileStart, false, cx),
            ActionKind::GotoLastLine => self.dispatch_simple_goto(GotoKind::LastLine, false, cx),
            ActionKind::GotoLineNumber => self.dispatch_goto_line_number(cx),
            ActionKind::GotoColumn => self.dispatch_goto_column(false, cx),
            ActionKind::ExpandSelection => self.dispatch_expand_selection(cx),
            ActionKind::AlignViewTop => {
                if let Some(editor) = self.active_editor(cx) {
                    editor.update(cx, |ed, cx| {
                        ed.handle_align_view(crate::editor::actions::movement::ViewAlign::Top, cx)
                    });
                }
            },
            ActionKind::AlignViewCenter => {
                if let Some(editor) = self.active_editor(cx) {
                    editor.update(cx, |ed, cx| {
                        ed.handle_align_view(
                            crate::editor::actions::movement::ViewAlign::Center,
                            cx,
                        )
                    });
                }
            },
            ActionKind::AlignViewBottom => {
                if let Some(editor) = self.active_editor(cx) {
                    editor.update(cx, |ed, cx| {
                        ed.handle_align_view(
                            crate::editor::actions::movement::ViewAlign::Bottom,
                            cx,
                        )
                    });
                }
            },
            ActionKind::ScrollUp => {
                let count = self.take_count(cx);
                if let Some(editor) = self.active_editor(cx) {
                    editor.update(cx, |ed, cx| {
                        ed.handle_scroll_view(
                            crate::editor::actions::movement::ScrollDir::Up,
                            count,
                            cx,
                        )
                    });
                }
            },
            ActionKind::ScrollDown => {
                let count = self.take_count(cx);
                if let Some(editor) = self.active_editor(cx) {
                    editor.update(cx, |ed, cx| {
                        ed.handle_scroll_view(
                            crate::editor::actions::movement::ScrollDir::Down,
                            count,
                            cx,
                        )
                    });
                }
            },
            ActionKind::GotoWindowTop => {
                if let Some(editor) = self.active_editor(cx) {
                    editor.update(cx, |ed, cx| {
                        ed.handle_goto_window(
                            crate::editor::actions::movement::WindowPos::Top,
                            false,
                            cx,
                        )
                    });
                }
            },
            ActionKind::GotoWindowCenter => {
                if let Some(editor) = self.active_editor(cx) {
                    editor.update(cx, |ed, cx| {
                        ed.handle_goto_window(
                            crate::editor::actions::movement::WindowPos::Center,
                            false,
                            cx,
                        )
                    });
                }
            },
            ActionKind::GotoWindowBottom => {
                if let Some(editor) = self.active_editor(cx) {
                    editor.update(cx, |ed, cx| {
                        ed.handle_goto_window(
                            crate::editor::actions::movement::WindowPos::Bottom,
                            false,
                            cx,
                        )
                    });
                }
            },
            ActionKind::ExtendGotoWindowTop => {
                if let Some(editor) = self.active_editor(cx) {
                    editor.update(cx, |ed, cx| {
                        ed.handle_goto_window(
                            crate::editor::actions::movement::WindowPos::Top,
                            true,
                            cx,
                        )
                    });
                }
            },
            ActionKind::ExtendGotoWindowCenter => {
                if let Some(editor) = self.active_editor(cx) {
                    editor.update(cx, |ed, cx| {
                        ed.handle_goto_window(
                            crate::editor::actions::movement::WindowPos::Center,
                            true,
                            cx,
                        )
                    });
                }
            },
            ActionKind::ExtendGotoWindowBottom => {
                if let Some(editor) = self.active_editor(cx) {
                    editor.update(cx, |ed, cx| {
                        ed.handle_goto_window(
                            crate::editor::actions::movement::WindowPos::Bottom,
                            true,
                            cx,
                        )
                    });
                }
            },
            ActionKind::MatchBrackets => {
                if let Some(editor) = self.active_editor(cx) {
                    editor.update(cx, |ed, cx| ed.handle_match_brackets(cx));
                }
            },
            ActionKind::ShrinkSelection => self.dispatch_shrink_selection(cx),
            ActionKind::SelectNextSibling => self.dispatch_select_sibling(
                crate::editor::actions::treesitter::SiblingDir::Next,
                false,
                cx,
            ),
            ActionKind::SelectPrevSibling => self.dispatch_select_sibling(
                crate::editor::actions::treesitter::SiblingDir::Prev,
                false,
                cx,
            ),
            ActionKind::SelectAllSiblings => self.dispatch_select_all_siblings(cx),
            ActionKind::SelectAllChildren => self.dispatch_select_all_children(cx),
            ActionKind::MoveParentNodeStart => self.dispatch_move_parent_bound(
                crate::editor::actions::treesitter::NodeBound::Start,
                false,
                cx,
            ),
            ActionKind::MoveParentNodeEnd => self.dispatch_move_parent_bound(
                crate::editor::actions::treesitter::NodeBound::End,
                false,
                cx,
            ),
            ActionKind::GotoNextFunction => self.dispatch_goto_textobject(
                crate::editor::actions::treesitter::NavKind::Function,
                crate::editor::actions::treesitter::NavDirection::Next,
                cx,
            ),
            ActionKind::GotoPrevFunction => self.dispatch_goto_textobject(
                crate::editor::actions::treesitter::NavKind::Function,
                crate::editor::actions::treesitter::NavDirection::Prev,
                cx,
            ),
            ActionKind::GotoNextClass => self.dispatch_goto_textobject(
                crate::editor::actions::treesitter::NavKind::Class,
                crate::editor::actions::treesitter::NavDirection::Next,
                cx,
            ),
            ActionKind::GotoPrevClass => self.dispatch_goto_textobject(
                crate::editor::actions::treesitter::NavKind::Class,
                crate::editor::actions::treesitter::NavDirection::Prev,
                cx,
            ),
            ActionKind::GotoNextDiagnostic => {
                self.dispatch_goto_diagnostic(crate::editor::actions::goto::DiagnosticDir::Next, cx)
            },
            ActionKind::GotoPrevDiagnostic => {
                self.dispatch_goto_diagnostic(crate::editor::actions::goto::DiagnosticDir::Prev, cx)
            },
            ActionKind::GotoNextHunk => {
                self.dispatch_goto_hunk(crate::editor::actions::goto::ChangeDir::Next, cx)
            },
            ActionKind::GotoPrevHunk => {
                self.dispatch_goto_hunk(crate::editor::actions::goto::ChangeDir::Prev, cx)
            },
            ActionKind::GotoNextParagraph => self
                .dispatch_goto_paragraph(crate::editor::actions::movement::ParagraphDir::Next, cx),
            ActionKind::GotoPrevParagraph => self
                .dispatch_goto_paragraph(crate::editor::actions::movement::ParagraphDir::Prev, cx),
            ActionKind::ExtendLeft => self.dispatch_move_horizontal(-1, true, cx),
            ActionKind::ExtendRight => self.dispatch_move_horizontal(1, true, cx),
            ActionKind::ExtendUp => self.dispatch_move_vertical(-1, true, cx),
            ActionKind::ExtendDown => self.dispatch_move_vertical(1, true, cx),
            ActionKind::ExtendNextWordStart => self.dispatch_move_word(
                crate::editor::actions::movement::WordTarget::NextStart,
                true,
                cx,
            ),
            ActionKind::ExtendNextWordEnd => self.dispatch_move_word(
                crate::editor::actions::movement::WordTarget::NextEnd,
                true,
                cx,
            ),
            ActionKind::ExtendPrevWordStart => self.dispatch_move_word(
                crate::editor::actions::movement::WordTarget::PrevStart,
                true,
                cx,
            ),
            ActionKind::ExtendPrevWordEnd => self.dispatch_move_word(
                crate::editor::actions::movement::WordTarget::PrevEnd,
                true,
                cx,
            ),
            ActionKind::ExtendToLineEnd => self.dispatch_simple_goto(GotoKind::LineEnd, true, cx),
            ActionKind::ExtendToFileStart => {
                self.dispatch_simple_goto(GotoKind::FileStart, true, cx)
            },
            ActionKind::ExtendToLastLine => self.dispatch_simple_goto(GotoKind::LastLine, true, cx),
            ActionKind::ExtendGotoFirstNonwhitespace => {
                self.dispatch_simple_goto(GotoKind::FirstNonwhitespace, true, cx)
            },
            ActionKind::ExtendGotoFileStart => {
                self.dispatch_simple_goto(GotoKind::FileStart, true, cx)
            },
            ActionKind::ExtendGotoLastLine => {
                self.dispatch_simple_goto(GotoKind::LastLine, true, cx)
            },
            ActionKind::ExtendGotoColumn => self.dispatch_goto_column(true, cx),
            ActionKind::ExtendSelectNextSibling => self.dispatch_select_sibling(
                crate::editor::actions::treesitter::SiblingDir::Next,
                true,
                cx,
            ),
            ActionKind::ExtendSelectPrevSibling => self.dispatch_select_sibling(
                crate::editor::actions::treesitter::SiblingDir::Prev,
                true,
                cx,
            ),
            ActionKind::ExtendMoveParentNodeStart => self.dispatch_move_parent_bound(
                crate::editor::actions::treesitter::NodeBound::Start,
                true,
                cx,
            ),
            ActionKind::ExtendMoveParentNodeEnd => self.dispatch_move_parent_bound(
                crate::editor::actions::treesitter::NodeBound::End,
                true,
                cx,
            ),
            ActionKind::FindNextChar => self.dispatch_set_pending_find(
                crate::editor::actions::movement::FindKind::NextChar,
                false,
                cx,
            ),
            ActionKind::FindPrevChar => self.dispatch_set_pending_find(
                crate::editor::actions::movement::FindKind::PrevChar,
                false,
                cx,
            ),
            ActionKind::TillNextChar => self.dispatch_set_pending_find(
                crate::editor::actions::movement::FindKind::TillNextChar,
                false,
                cx,
            ),
            ActionKind::TillPrevChar => self.dispatch_set_pending_find(
                crate::editor::actions::movement::FindKind::TillPrevChar,
                false,
                cx,
            ),
            ActionKind::ExtendFindNextChar => self.dispatch_set_pending_find(
                crate::editor::actions::movement::FindKind::NextChar,
                true,
                cx,
            ),
            ActionKind::ExtendFindPrevChar => self.dispatch_set_pending_find(
                crate::editor::actions::movement::FindKind::PrevChar,
                true,
                cx,
            ),
            ActionKind::ExtendTillNextChar => self.dispatch_set_pending_find(
                crate::editor::actions::movement::FindKind::TillNextChar,
                true,
                cx,
            ),
            ActionKind::ExtendTillPrevChar => self.dispatch_set_pending_find(
                crate::editor::actions::movement::FindKind::TillPrevChar,
                true,
                cx,
            ),
            ActionKind::ApplyFindChar => {
                if let Some(apply) = action
                    .as_any()
                    .downcast_ref::<crate::actions::ApplyFindChar>()
                {
                    if let Some(editor) = self.active_editor(cx) {
                        let kind = apply.kind;
                        let ch = apply.ch;
                        let extend = apply.extend;
                        let count = apply.count;
                        editor.update(cx, |ed, cx| {
                            ed.handle_find_char(kind, ch, extend, count, cx)
                        });
                    }
                }
            },
            ActionKind::SetMark => {
                self.dispatch_set_pending_mark(crate::editor::actions::marks::MarkRequest::Set, cx)
            },
            ActionKind::GotoMark => self.dispatch_set_pending_mark(
                crate::editor::actions::marks::MarkRequest::GotoLine,
                cx,
            ),
            ActionKind::GotoMarkExact => self.dispatch_set_pending_mark(
                crate::editor::actions::marks::MarkRequest::GotoExact,
                cx,
            ),
            ActionKind::ApplyMarkChar => {
                if let Some(apply) = action
                    .as_any()
                    .downcast_ref::<crate::actions::ApplyMarkChar>()
                {
                    if let Some(editor) = self.active_editor(cx) {
                        let request = apply.request;
                        let ch = apply.ch;
                        editor.update(cx, |ed, cx| match request {
                            crate::editor::actions::marks::MarkRequest::Set => {
                                ed.handle_set_mark(ch, cx)
                            },
                            crate::editor::actions::marks::MarkRequest::GotoLine => {
                                ed.handle_goto_mark(ch, false, cx)
                            },
                            crate::editor::actions::marks::MarkRequest::GotoExact => {
                                ed.handle_goto_mark(ch, true, cx)
                            },
                        });
                    }
                }
            },
            ActionKind::SaveSelection => self.dispatch_save_selection(cx),
            ActionKind::SaveBuffer => self.dispatch_save_buffer(cx),
            ActionKind::ToggleBlame => self.dispatch_toggle_blame(cx),
            ActionKind::ToggleInlineBlame => self.dispatch_toggle_inline_blame(cx),
            ActionKind::ToggleMinimap => self.dispatch_toggle_minimap(cx),
            ActionKind::ToggleRelativeLineNumbers => self.dispatch_toggle_relative_line_numbers(cx),
            ActionKind::FoldAtCursor => self.dispatch_fold_at_cursor(cx),
            ActionKind::UnfoldAtCursor => self.dispatch_unfold_at_cursor(cx),
            ActionKind::FoldAll => self.dispatch_fold_all(cx),
            ActionKind::UnfoldAll => self.dispatch_unfold_all(cx),
            ActionKind::ToggleTabBar => self.dispatch_toggle_tab_bar(cx),
            ActionKind::Set => self.dispatch_set(&*action, cx),
            ActionKind::ToggleProjectTree => self.dispatch_toggle_project_tree(cx),
            ActionKind::ToggleOutlinePanel => self.dispatch_toggle_outline_panel(cx),
            ActionKind::ToggleDiagnosticsPanel => self.dispatch_toggle_diagnostics_panel(cx),
            ActionKind::OpenMarkdownPreview => self.dispatch_open_markdown_preview(cx),
            ActionKind::ProjectTreeSelectNext => {
                self.update_project_tree(cx, ProjectTree::select_next)
            },
            ActionKind::ProjectTreeSelectPrev => {
                self.update_project_tree(cx, ProjectTree::select_prev)
            },
            ActionKind::ProjectTreeCollapse => self.update_project_tree(cx, ProjectTree::collapse),
            ActionKind::ProjectTreeExpand => self.update_project_tree(cx, ProjectTree::expand),
            ActionKind::ProjectTreeConfirm => self.dispatch_project_tree_confirm(cx),
            ActionKind::ProjectTreeRefresh => self.update_project_tree(cx, ProjectTree::refresh),
            ActionKind::DeleteTreeEntry => self.dispatch_delete_tree_entry(window, cx),
            ActionKind::ToggleDiffHunkPanel => self.dispatch_toggle_diff_hunk_panel(cx),
            ActionKind::JumpBackward => self.dispatch_jump(JumpDir::Backward, cx),
            ActionKind::JumpForward => self.dispatch_jump(JumpDir::Forward, cx),
            ActionKind::ReviewNextChunk => self.dispatch_review_step(ReviewStepDir::Next, cx),
            ActionKind::ReviewPrevChunk => self.dispatch_review_step(ReviewStepDir::Prev, cx),
            ActionKind::ReviewStageChunk => {
                self.dispatch_review_set_status(ReviewStatusChange::Stage, cx)
            },
            ActionKind::ReviewUnstageChunk => {
                self.dispatch_review_set_status(ReviewStatusChange::Unstage, cx)
            },
            ActionKind::ReviewSkipChunk => {
                self.dispatch_review_set_status(ReviewStatusChange::Skip, cx)
            },
            ActionKind::ReviewToggleStage => {
                self.dispatch_review_set_status(ReviewStatusChange::Toggle, cx)
            },
            ActionKind::ReviewApproveHunk => self.dispatch_review_approve(true, cx),
            ActionKind::ReviewToggleApproval => self.dispatch_review_approve(false, cx),
            ActionKind::ReviewNextUnreviewedHunk => self.dispatch_review_next_unreviewed(cx),
            ActionKind::ReviewResetProgress => self.dispatch_review_reset_progress(cx),
            ActionKind::ReviewEnterLineSelect => self.dispatch_review_enter_line_select(cx),
            ActionKind::ReviewLineSelectCancel => self.dispatch_review_line_select_cancel(cx),
            ActionKind::ReviewLineSelectToggle => self.dispatch_review_line_select_toggle(cx),
            ActionKind::ReviewLineSelectAll => self.dispatch_review_line_select_all(cx),
            ActionKind::ReviewLineSelectStage => self.dispatch_review_line_select_stage(false, cx),
            ActionKind::ReviewLineSelectUnstage => self.dispatch_review_line_select_stage(true, cx),
            ActionKind::GitToggleStageHunk => self.dispatch_git_stage_hunk(false, cx),
            ActionKind::GitUnstageHunk => self.dispatch_git_stage_hunk(true, cx),
            ActionKind::GitToggleStageLine => self.dispatch_git_stage_line(cx),
            ActionKind::ReviewRevertHunk => self.dispatch_review_revert_hunk(cx),
            ActionKind::ReviewCycleComparisonMode => self.dispatch_review_cycle_comparison_mode(cx),
            ActionKind::ReviewToggleFollow => self.dispatch_review_toggle_follow(cx),
            ActionKind::ReviewRemoveSelected => self.dispatch_review_remove_selected(cx),
            ActionKind::ReviewApplyStaged => self.dispatch_review_apply_staged(cx),
            ActionKind::ReviewRefresh => self.dispatch_review_refresh(cx),
            ActionKind::ReviewExternalEdit => {
                if let Some(action) = action
                    .as_any()
                    .downcast_ref::<stoat_action::ReviewExternalEdit>()
                {
                    let path = action.path.clone();
                    self.dispatch_review_external_edit(path, cx);
                }
            },
            ActionKind::JumpToMoveSource => {
                self.dispatch_jump_to_move_source(JumpMoveNav::First, cx)
            },
            ActionKind::JumpToNextMoveSource => {
                self.dispatch_jump_to_move_source(JumpMoveNav::Next, cx)
            },
            ActionKind::JumpToPrevMoveSource => {
                self.dispatch_jump_to_move_source(JumpMoveNav::Prev, cx)
            },
            ActionKind::JumpToMoveTarget => self.dispatch_jump_to_move_target(cx),
            ActionKind::QueryMoveRelationships => {
                self.dispatch_query_move_relationships(window, cx)
            },
            ActionKind::OpenCommandPalette => {
                crate::command_palette::open_command_palette(self, window, cx)
            },
            ActionKind::OpenThemePicker => crate::theme_picker::open_theme_picker(self, window, cx),
            ActionKind::OpenLineEndingPicker => {
                crate::line_ending_picker::open_line_ending_picker(self, window, cx)
            },
            ActionKind::OpenEncodingPicker => {
                crate::encoding_picker::open_encoding_picker(self, window, cx)
            },
            ActionKind::OpenGotoLineModal => {
                crate::goto_line_modal::open_goto_line_modal(self, window, cx)
            },
            ActionKind::OpenHelp => crate::help::open_help(self, window, cx),
            ActionKind::OpenAbout => crate::about_modal::open_about(self, window, cx),
            ActionKind::OpenFileFinder => crate::file_finder::open_file_finder(self, window, cx),
            ActionKind::OpenFileFinderHSplit => {
                crate::file_finder::open_file_finder_split(self, Axis::Horizontal, window, cx)
            },
            ActionKind::OpenFileFinderVSplit => {
                crate::file_finder::open_file_finder_split(self, Axis::Vertical, window, cx)
            },
            ActionKind::OpenChangedFilePicker => {
                crate::file_finder::open_changed_file_finder(self, window, cx)
            },
            ActionKind::OpenBufferPicker => {
                crate::buffer_picker::open_buffer_picker(self, window, cx)
            },
            ActionKind::OpenGitStatus => {
                crate::git_status_picker::open_git_status_picker(self, window, cx)
            },
            ActionKind::OpenConflictPicker => {
                crate::conflict_picker::open_conflict_picker(self, window, cx)
            },
            ActionKind::OpenSymbolPicker => {
                crate::symbol_picker::open_symbol_picker(self, window, cx)
            },
            ActionKind::OpenWorkspaceSymbolPicker => {
                crate::workspace_symbol_picker::open_workspace_symbol_picker(self, window, cx)
            },
            ActionKind::OpenDiagnosticsPicker => {
                crate::diagnostics_picker::open_diagnostics_picker(self, window, cx)
            },
            ActionKind::OpenWorkspaceDiagnosticsPicker => {
                crate::diagnostics_picker::open_workspace_diagnostics_picker(self, window, cx)
            },
            ActionKind::OpenJumplistPicker => {
                crate::jumplist_picker::open_jumplist_picker(self, window, cx)
            },
            ActionKind::OpenWorkspacePicker => {
                crate::workspace_picker::open_workspace_picker(self, window, cx)
            },
            ActionKind::SwitchWorkspace => {
                crate::workspace_picker::open_workspace_picker(self, window, cx)
            },
            ActionKind::OpenCheckpointPicker => {
                crate::claude_checkpoint_picker::open_claude_checkpoint_picker(self, window, cx)
            },
            ActionKind::OpenGlobalSearch => {
                crate::global_search::open_global_search(self, window, cx)
            },
            ActionKind::OpenLastPicker => self.dispatch_open_last_picker(window, cx),
            ActionKind::OpenClaude => crate::claude_chat::dispatch_open_claude(self, window, cx),
            ActionKind::ClaudeSubmit => crate::claude_chat::dispatch_claude_submit(self, cx),
            ActionKind::ClaudeToPane => crate::claude_chat::dispatch_claude_to_pane(self, cx),
            ActionKind::ClaudeToDockLeft => {
                crate::claude_chat::dispatch_claude_to_dock(self, DockSide::Left, cx)
            },
            ActionKind::ClaudeToDockRight => {
                crate::claude_chat::dispatch_claude_to_dock(self, DockSide::Right, cx)
            },
            ActionKind::ClaudeToggleFollow => {
                crate::claude_chat::dispatch_claude_toggle_follow(self, cx)
            },
            ActionKind::OpenRun => crate::run_pane::dispatch_open_run(self, window, cx),
            ActionKind::OpenTerminalDock => {
                crate::run_pane::dispatch_open_terminal_dock(self, window, cx)
            },
            ActionKind::Run => self.dispatch_run(&*action, window, cx),
            ActionKind::Dump => self.dispatch_dump(&*action, cx),
            ActionKind::RunSubmit => crate::run_pane::dispatch_run_submit(self, cx),
            ActionKind::RunHistoryPrev => crate::run_pane::dispatch_run_history_prev(self, cx),
            ActionKind::RunHistoryNext => crate::run_pane::dispatch_run_history_next(self, cx),
            ActionKind::RunInterrupt => crate::run_pane::dispatch_run_interrupt(self, cx),
            ActionKind::RunClickAt => {
                if let Some(click) = action
                    .as_any()
                    .downcast_ref::<crate::run_pane::mouse::RunClickAt>()
                {
                    let (row, col) = (click.row, click.col);
                    crate::run_pane::mouse::handle_run_click_at(self, row, col, cx);
                }
            },
            ActionKind::RunDragSelectTo => {
                if let Some(drag) = action
                    .as_any()
                    .downcast_ref::<crate::run_pane::mouse::RunDragSelectTo>()
                {
                    let (row, col) = (drag.row, drag.col);
                    crate::run_pane::mouse::handle_run_drag_select_to(self, row, col, cx);
                }
            },
            ActionKind::ClaudeFocusNextToolCard => {
                crate::claude_chat::dispatch_claude_focus_next_tool_card(self, cx)
            },
            ActionKind::ClaudeFocusPrevToolCard => {
                crate::claude_chat::dispatch_claude_focus_prev_tool_card(self, cx)
            },
            ActionKind::ClaudeToggleToolCardExpand => {
                crate::claude_chat::dispatch_claude_toggle_tool_card_expand(self, cx)
            },
            ActionKind::ClaudeInterrupt => crate::claude_chat::dispatch_claude_interrupt(self, cx),
            ActionKind::ClaudeJumpToFocusedCard => {
                crate::claude_chat::dispatch_claude_jump_to_focused_card(self, window, cx)
            },
            ActionKind::OpenReview => self.dispatch_open_review(cx),
            ActionKind::OpenReviewCommit => {
                if let Some(action) = action
                    .as_any()
                    .downcast_ref::<stoat_action::OpenReviewCommit>()
                {
                    let workdir = action.workdir.clone();
                    let sha = action.sha.clone();
                    self.dispatch_open_review_commit(workdir, sha, cx);
                }
            },
            ActionKind::OpenReviewCommitRange => {
                if let Some(action) = action
                    .as_any()
                    .downcast_ref::<stoat_action::OpenReviewCommitRange>()
                {
                    let workdir = action.workdir.clone();
                    let from = action.from.clone();
                    let to = action.to.clone();
                    self.dispatch_open_review_commit_range(workdir, from, to, cx);
                }
            },
            ActionKind::OpenReviewAgentEdits => {
                if let Some(action) = action
                    .as_any()
                    .downcast_ref::<stoat_action::OpenReviewAgentEdits>()
                {
                    let edits = action.edits.clone();
                    self.dispatch_open_review_agent_edits(edits, cx);
                }
            },
            ActionKind::OpenCommits => self.dispatch_open_commits(window, cx),
            ActionKind::CommitsNext => self.dispatch_commits_step(CommitStep::Down(1), cx),
            ActionKind::CommitsPrev => self.dispatch_commits_step(CommitStep::Up(1), cx),
            ActionKind::CommitsPageDown => self.dispatch_commits_step(CommitStep::PageDown, cx),
            ActionKind::CommitsPageUp => self.dispatch_commits_step(CommitStep::PageUp, cx),
            ActionKind::CommitsFirst => self.dispatch_commits_step(CommitStep::First, cx),
            ActionKind::CommitsLast => self.dispatch_commits_step(CommitStep::Last, cx),
            ActionKind::CommitsRefresh => self.dispatch_commits_refresh(cx),
            ActionKind::CommitsOpenReview => self.dispatch_commits_open_review(cx),
            ActionKind::CloseCommits => self.dispatch_close_commits(cx),
            ActionKind::ConflictTakeOurs => {
                self.dispatch_conflict_take_side(ConflictSide::Ours, cx)
            },
            ActionKind::ConflictTakeTheirs => {
                self.dispatch_conflict_take_side(ConflictSide::Theirs, cx)
            },
            ActionKind::ConflictNextFile => self.dispatch_conflict_nav(ConflictNavDir::Next, cx),
            ActionKind::ConflictPrevFile => self.dispatch_conflict_nav(ConflictNavDir::Prev, cx),
            ActionKind::ConflictSkipEntry => self.dispatch_conflict_skip_entry(cx),
            ActionKind::ConflictApply => self.dispatch_conflict_apply(window, cx),
            ActionKind::ConflictAbort => self.dispatch_conflict_abort(cx),
            ActionKind::EnterRebase => self.dispatch_enter_rebase(cx),
            ActionKind::RebaseNext => self.dispatch_rebase_move(RebaseMoveDir::Next, cx),
            ActionKind::RebasePrev => self.dispatch_rebase_move(RebaseMoveDir::Prev, cx),
            ActionKind::RebaseMoveUp => self.dispatch_rebase_move(RebaseMoveDir::SwapUp, cx),
            ActionKind::RebaseMoveDown => self.dispatch_rebase_move(RebaseMoveDir::SwapDown, cx),
            ActionKind::SetRebaseOpPick => self.dispatch_rebase_set_op(RebaseTodoOp::Pick, cx),
            ActionKind::SetRebaseOpSquash => self.dispatch_rebase_set_op(RebaseTodoOp::Squash, cx),
            ActionKind::SetRebaseOpFixup => self.dispatch_rebase_set_op(RebaseTodoOp::Fixup, cx),
            ActionKind::SetRebaseOpDrop => self.dispatch_rebase_set_op(RebaseTodoOp::Drop, cx),
            ActionKind::SetRebaseOpReword => self.dispatch_rebase_set_op(RebaseTodoOp::Reword, cx),
            ActionKind::SetRebaseOpEdit => self.dispatch_rebase_set_op(RebaseTodoOp::Edit, cx),
            ActionKind::ExecuteRebase => self.dispatch_execute_rebase(window, cx),
            ActionKind::AbortRebase => self.dispatch_abort_rebase(cx),
            ActionKind::RebaseContinue => self.dispatch_rebase_continue(window, cx),
            ActionKind::Yank => crate::editor::actions::edit::handle_yank(self, cx),
            ActionKind::PasteAfter => crate::editor::actions::edit::handle_paste_after(self, cx),
            ActionKind::PasteBefore => crate::editor::actions::edit::handle_paste_before(self, cx),
            ActionKind::YankToClipboard => {
                crate::editor::actions::edit::handle_yank_to_clipboard(self, cx)
            },
            ActionKind::YankMainToClipboard => {
                crate::editor::actions::edit::handle_yank_main_to_clipboard(self, cx)
            },
            ActionKind::PasteClipboardAfter => {
                crate::editor::actions::edit::handle_paste_clipboard_after(self, cx)
            },
            ActionKind::PasteClipboardBefore => {
                crate::editor::actions::edit::handle_paste_clipboard_before(self, cx)
            },
            ActionKind::DeleteSelection => {
                crate::editor::actions::edit::handle_delete_selection(self, cx)
            },
            ActionKind::DeleteForward => {
                crate::editor::actions::edit::handle_delete_forward(self, cx)
            },
            ActionKind::DeleteBackward => {
                crate::editor::actions::edit::handle_delete_backward(self, cx)
            },
            ActionKind::DeleteWordForward => {
                crate::editor::actions::edit::handle_delete_word_forward(self, cx)
            },
            ActionKind::DeleteWordBackward => {
                crate::editor::actions::edit::handle_delete_word_backward(self, cx)
            },
            ActionKind::Insert => crate::editor::actions::edit::handle_insert(self, window, cx),
            ActionKind::Append => crate::editor::actions::edit::handle_append(self, window, cx),
            ActionKind::InsertNewline => {
                crate::editor::actions::edit::handle_insert_newline(self, cx)
            },
            ActionKind::OpenBelow => crate::editor::actions::edit::handle_open_below(self, cx),
            ActionKind::OpenAbove => crate::editor::actions::edit::handle_open_above(self, cx),
            ActionKind::SwitchCase => crate::editor::actions::edit::handle_switch_case(self, cx),
            ActionKind::SwitchToUppercase => {
                crate::editor::actions::edit::handle_switch_to_uppercase(self, cx)
            },
            ActionKind::SwitchToLowercase => {
                crate::editor::actions::edit::handle_switch_to_lowercase(self, cx)
            },
            ActionKind::IndentSelection => {
                let count = self.take_count(cx);
                crate::editor::actions::indent::handle_indent_selection(self, count, cx);
            },
            ActionKind::UnindentSelection => {
                let count = self.take_count(cx);
                crate::editor::actions::indent::handle_unindent_selection(self, count, cx);
            },
            ActionKind::ToggleComments => {
                crate::editor::actions::indent::handle_toggle_comments(self, cx)
            },
            ActionKind::Undo => {
                let count = self.take_count(cx);
                crate::editor::actions::undo::handle_undo(self, count, cx);
            },
            ActionKind::Redo => {
                let count = self.take_count(cx);
                crate::editor::actions::undo::handle_redo(self, count, cx);
            },
            ActionKind::CommitUndoCheckpoint => {
                crate::editor::actions::undo::handle_commit_checkpoint(self, cx)
            },
            ActionKind::CollapseSelection => {
                crate::editor::actions::multi_cursor::handle_collapse_selection(self, cx)
            },
            ActionKind::FlipSelections => {
                crate::editor::actions::multi_cursor::handle_flip_selections(self, cx)
            },
            ActionKind::SelectAll => {
                crate::editor::actions::multi_cursor::handle_select_all(self, cx)
            },
            ActionKind::SelectLineBelow => {
                let count = self.take_count(cx);
                crate::editor::actions::multi_cursor::handle_select_line_below(self, count, cx);
            },
            ActionKind::KeepPrimarySelection => {
                crate::editor::actions::multi_cursor::handle_keep_primary_selection(self, cx)
            },
            ActionKind::RemovePrimarySelection => {
                crate::editor::actions::multi_cursor::handle_remove_primary_selection(self, cx)
            },
            ActionKind::RotateSelectionsForward => {
                let count = self.take_count(cx);
                crate::editor::actions::multi_cursor::handle_rotate_selections_forward(
                    self, count, cx,
                );
            },
            ActionKind::RotateSelectionsBackward => {
                let count = self.take_count(cx);
                crate::editor::actions::multi_cursor::handle_rotate_selections_backward(
                    self, count, cx,
                );
            },
            ActionKind::TrimSelections => {
                crate::editor::actions::multi_cursor::handle_trim_selections(self, cx)
            },
            ActionKind::SplitSelectionOnNewline => {
                crate::editor::actions::multi_cursor::handle_split_selection_on_newline(self, cx)
            },
            ActionKind::AlignSelections => {
                crate::editor::actions::multi_cursor::handle_align_selections(self, cx)
            },
            ActionKind::AddSelectionBelow => {
                let count = self.take_count(cx);
                crate::editor::actions::multi_cursor::handle_add_selection_below(self, count, cx);
            },
            ActionKind::AddSelectionAbove => {
                let count = self.take_count(cx);
                crate::editor::actions::multi_cursor::handle_add_selection_above(self, count, cx);
            },
            ActionKind::SplitSelection => {
                crate::editor::actions::multi_cursor::handle_split_selection(self, window, cx)
            },
            ActionKind::KeepSelections => {
                crate::editor::actions::multi_cursor::handle_keep_selections(self, window, cx)
            },
            ActionKind::RemoveSelections => {
                crate::editor::actions::multi_cursor::handle_remove_selections(self, window, cx)
            },
            ActionKind::RecordMacro => self.dispatch_record_macro(cx),
            ActionKind::ReplayMacro => self.dispatch_replay_macro(cx),
            ActionKind::SelectRegister => self.dispatch_select_register(cx),
            ActionKind::ApplyRegisterSelectChar => {
                if let Some(apply) = action
                    .as_any()
                    .downcast_ref::<crate::actions::ApplyRegisterSelectChar>()
                {
                    self.apply_register_select_char(apply.ch);
                }
            },
            ActionKind::ReplaceChar => self.dispatch_replace_char(cx),
            ActionKind::ApplyReplaceChar => {
                if let Some(apply) = action
                    .as_any()
                    .downcast_ref::<crate::actions::ApplyReplaceChar>()
                {
                    if let Some(editor) = self.active_editor(cx) {
                        let ch = apply.ch;
                        editor.update(cx, |ed, cx| ed.replace_char_in_selections(ch, cx));
                    }
                }
            },
            ActionKind::InsertRegister => self.dispatch_insert_register(cx),
            ActionKind::ApplyInsertRegisterChar => {
                if let Some(apply) = action
                    .as_any()
                    .downcast_ref::<crate::actions::ApplyInsertRegisterChar>()
                {
                    let ch = apply.ch;
                    crate::editor::actions::edit::handle_insert_register_char(self, ch, cx);
                }
            },
            ActionKind::ApplyReplayMacroChar => {
                if let Some(apply) = action
                    .as_any()
                    .downcast_ref::<crate::actions::ApplyReplayMacroChar>()
                {
                    let ch = apply.ch;
                    self.apply_replay_macro_char(ch, window, cx);
                }
            },
            ActionKind::SurroundAdd => self.dispatch_surround_add(cx),
            ActionKind::SurroundDelete => self.dispatch_surround_delete(cx),
            ActionKind::SurroundReplace => self.dispatch_surround_replace(cx),
            ActionKind::ApplySurroundAddChar => {
                if let Some(apply) = action
                    .as_any()
                    .downcast_ref::<crate::actions::ApplySurroundAddChar>()
                {
                    if let Some(editor) = self.active_editor(cx) {
                        let ch = apply.ch;
                        editor.update(cx, |ed, cx| ed.handle_surround_add(ch, cx));
                    }
                }
            },
            ActionKind::ApplySurroundDeleteChar => {
                if let Some(apply) = action
                    .as_any()
                    .downcast_ref::<crate::actions::ApplySurroundDeleteChar>()
                {
                    if let Some(editor) = self.active_editor(cx) {
                        let ch = apply.ch;
                        editor.update(cx, |ed, cx| ed.handle_surround_delete(ch, cx));
                    }
                }
            },
            ActionKind::ApplySurroundReplaceChar => {
                if let Some(apply) = action
                    .as_any()
                    .downcast_ref::<crate::actions::ApplySurroundReplaceChar>()
                {
                    if let Some(editor) = self.active_editor(cx) {
                        let from = apply.from;
                        let to = apply.to;
                        editor.update(cx, |ed, cx| ed.handle_surround_replace(from, to, cx));
                    }
                }
            },
            ActionKind::ShellPipe => {
                crate::editor::actions::shell::handle_shell_pipe(self, window, cx)
            },
            ActionKind::ShellPipeTo => {
                crate::editor::actions::shell::handle_shell_pipe_to(self, window, cx)
            },
            ActionKind::ShellInsertOutput => {
                crate::editor::actions::shell::handle_shell_insert_output(self, window, cx)
            },
            ActionKind::ShellAppendOutput => {
                crate::editor::actions::shell::handle_shell_append_output(self, window, cx)
            },
            ActionKind::ShellKeepPipe => {
                crate::editor::actions::shell::handle_shell_keep_pipe(self, window, cx)
            },
            ActionKind::SelectTextobjectAround => {
                crate::editor::actions::textobject::handle_select_textobject_around(self, cx)
            },
            ActionKind::SelectTextobjectInner => {
                crate::editor::actions::textobject::handle_select_textobject_inner(self, cx)
            },
            ActionKind::SearchNext => self.dispatch_search_step(SearchStep::Next, cx),
            ActionKind::SearchPrev => self.dispatch_search_step(SearchStep::Prev, cx),
            ActionKind::OpenSearchInput => {
                crate::editor::regex_input_modal::handle_open_search_input(
                    self,
                    crate::editor::search::SearchDirection::Forward,
                    window,
                    cx,
                )
            },
            ActionKind::OpenReverseSearchInput => {
                crate::editor::regex_input_modal::handle_open_search_input(
                    self,
                    crate::editor::search::SearchDirection::Reverse,
                    window,
                    cx,
                )
            },
            ActionKind::GotoWord => self.dispatch_goto_word(cx),
            ActionKind::GotoWordJump => {
                if let Some(jump) = action
                    .as_any()
                    .downcast_ref::<crate::actions::GotoWordJump>()
                {
                    self.dispatch_goto_word_jump(jump.byte_offset, cx);
                }
            },
            ActionKind::ApplyTextobjectChar => {
                if let Some(apply) = action
                    .as_any()
                    .downcast_ref::<crate::actions::ApplyTextobjectChar>()
                {
                    let mode = apply.mode;
                    let ch = apply.ch;
                    crate::editor::actions::textobject::handle_apply_textobject_char(
                        self, mode, ch, cx,
                    );
                }
            },
            other => {
                tracing::trace!(target: "stoat::dispatch", "unrouted action: {other:?}");
            },
        }

        if crate::picker::is_picker_open_kind(action.kind())
            && self.modal_layer.read(cx).has_active_modal()
        {
            let name = action.def().name();
            self.input_state_machine
                .update(cx, |sm, _| sm.set_last_picker_action(Some(name)));
        }
    }

    /// Re-dispatch the most recently opened picker action. Reads
    /// the action name recorded on
    /// [`InputStateMachine::last_picker_action`], looks it up in
    /// the `stoat_action::registry`, constructs a fresh action
    /// instance, and routes it back through
    /// [`Self::dispatch_action`]. The picker rebuilds from current
    /// state -- prior query and selection are not restored. Silent
    /// no-op when no picker has been recorded or the registry
    /// lookup fails.
    fn dispatch_open_last_picker(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(name) = self.input_state_machine.read(cx).last_picker_action() else {
            return;
        };
        let Some(entry) = stoat_action::registry::lookup(name) else {
            return;
        };
        let Ok(action) = (entry.create)(&[]) else {
            return;
        };
        self.dispatch_action(action, window, cx);
    }

    fn dispatch_move_horizontal(&mut self, delta: i32, extend: bool, cx: &mut Context<'_, Self>) {
        self.last_motion = Some(LastMotion::Horizontal { delta, extend });
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| {
            ed.handle_move_horizontal(delta, count, extend, cx)
        });
    }

    fn dispatch_move_vertical(&mut self, delta: i32, extend: bool, cx: &mut Context<'_, Self>) {
        self.last_motion = Some(LastMotion::Vertical { delta, extend });
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| {
            ed.handle_move_vertical(delta, count, extend, cx)
        });
    }

    fn dispatch_page_motion(
        &mut self,
        dir: crate::editor::actions::movement::PageDir,
        half: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.last_motion = Some(LastMotion::Page { dir, half });
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| ed.handle_page_motion(dir, half, count, cx));
    }

    fn dispatch_move_word(
        &mut self,
        target: crate::editor::actions::movement::WordTarget,
        extend: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.last_motion = Some(LastMotion::Word { target, extend });
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| ed.handle_move_word(target, count, extend, cx));
    }

    fn dispatch_simple_goto(&mut self, kind: GotoKind, extend: bool, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        editor.update(cx, |ed, cx| match kind {
            GotoKind::LineStart => ed.handle_goto_line_start(extend, cx),
            GotoKind::LineEnd => ed.handle_goto_line_end(extend, cx),
            GotoKind::FirstNonwhitespace => ed.handle_goto_first_nonwhitespace(extend, cx),
            GotoKind::FileStart => ed.handle_goto_file_start(extend, cx),
            GotoKind::LastLine => ed.handle_goto_last_line(extend, cx),
        });
    }

    fn dispatch_goto_line_number(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self
            .input_state_machine
            .update(cx, |sm, _| sm.take_consumed_count());
        editor.update(cx, |ed, cx| ed.handle_goto_line_number(count, cx));
    }

    fn dispatch_goto_column(&mut self, extend: bool, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| ed.handle_goto_column(count, extend, cx));
    }

    fn dispatch_expand_selection(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| ed.handle_expand_selection(count, cx));
    }

    fn dispatch_shrink_selection(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| ed.handle_shrink_selection(count, cx));
    }

    fn dispatch_select_sibling(
        &mut self,
        dir: crate::editor::actions::treesitter::SiblingDir,
        extend: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| {
            ed.handle_select_sibling(dir, extend, count, cx)
        });
    }

    fn dispatch_select_all_siblings(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        editor.update(cx, |ed, cx| ed.handle_select_all_siblings(cx));
    }

    fn dispatch_select_all_children(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        editor.update(cx, |ed, cx| ed.handle_select_all_children(cx));
    }

    fn dispatch_move_parent_bound(
        &mut self,
        bound: crate::editor::actions::treesitter::NodeBound,
        extend: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.last_motion = Some(LastMotion::ParentBound { bound, extend });
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| {
            ed.handle_move_parent_bound(bound, extend, count, cx)
        });
    }

    fn dispatch_repeat_last_motion(&mut self, cx: &mut Context<'_, Self>) {
        let Some(motion) = self.last_motion else {
            return;
        };
        match motion {
            LastMotion::Horizontal { delta, extend } => {
                self.dispatch_move_horizontal(delta, extend, cx);
            },
            LastMotion::Vertical { delta, extend } => {
                self.dispatch_move_vertical(delta, extend, cx);
            },
            LastMotion::Word { target, extend } => {
                self.dispatch_move_word(target, extend, cx);
            },
            LastMotion::Page { dir, half } => {
                self.dispatch_page_motion(dir, half, cx);
            },
            LastMotion::ParentBound { bound, extend } => {
                self.dispatch_move_parent_bound(bound, extend, cx);
            },
        }
    }

    fn dispatch_goto_textobject(
        &mut self,
        kind: crate::editor::actions::treesitter::NavKind,
        direction: crate::editor::actions::treesitter::NavDirection,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        editor.update(cx, |ed, cx| ed.handle_goto_textobject(kind, direction, cx));
    }

    fn dispatch_goto_diagnostic(
        &mut self,
        dir: crate::editor::actions::goto::DiagnosticDir,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        editor.update(cx, |ed, cx| ed.handle_goto_diagnostic(dir, cx));
    }

    fn dispatch_goto_hunk(
        &mut self,
        dir: crate::editor::actions::goto::ChangeDir,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        editor.update(cx, |ed, cx| ed.handle_goto_hunk(dir, cx));
    }

    fn dispatch_goto_paragraph(
        &mut self,
        dir: crate::editor::actions::movement::ParagraphDir,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        editor.update(cx, |ed, cx| ed.handle_goto_paragraph(dir, cx));
    }

    fn dispatch_hover(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        editor.update(cx, |ed, cx| {
            let Some(newest) = ed
                .selections()
                .all_anchors()
                .iter()
                .max_by_key(|s| s.id)
                .cloned()
            else {
                return;
            };
            let mb_snapshot = ed.multi_buffer().read(cx).snapshot();
            let head_offset = mb_snapshot.resolve_anchor(&newest.head());
            let head_point = mb_snapshot.rope().offset_to_point(head_offset);
            let display_snapshot = ed.display_map().update(cx, |dm, _| dm.snapshot());
            let head_display = display_snapshot.buffer_to_display(head_point);
            ed.set_hover_position(Some((head_display.row, head_display.column)), cx);
        });
    }

    pub(crate) fn active_editor(&self, cx: &Context<'_, Self>) -> Option<Entity<Editor>> {
        self.input_state_machine
            .read(cx)
            .active_editor()
            .cloned()
            .and_then(|w| w.upgrade())
    }

    fn take_count(&mut self, cx: &mut Context<'_, Self>) -> u32 {
        self.input_state_machine
            .update(cx, |sm, _| sm.take_consumed_count())
            .unwrap_or(1)
    }

    fn dispatch_set_pending_find(
        &mut self,
        kind: crate::editor::actions::movement::FindKind,
        extend: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let count = self.take_count(cx);
        self.input_state_machine
            .update(cx, |sm, cx| sm.set_pending_find(kind, extend, count, cx));
    }

    fn dispatch_set_pending_mark(
        &mut self,
        request: crate::editor::actions::marks::MarkRequest,
        cx: &mut Context<'_, Self>,
    ) {
        self.input_state_machine
            .update(cx, |sm, cx| sm.set_pending_mark(request, cx));
    }

    /// `RecordMacro` toggles the input state machine's macro
    /// recording. On Off->On, the register defaults to the
    /// workspace's pending [`stoat::register::Register`] (or
    /// `Unnamed`); on On->Off, the captured sequence lands in
    /// the input state machine's macro store.
    fn dispatch_record_macro(&mut self, cx: &mut Context<'_, Self>) {
        let register = self.consume_selected_register();
        self.input_state_machine
            .update(cx, |sm, cx| sm.toggle_macro_recording(register, cx));
    }

    fn dispatch_replay_macro(&mut self, cx: &mut Context<'_, Self>) {
        self.input_state_machine
            .update(cx, |sm, cx| sm.arm_replay_macro(cx));
    }

    fn dispatch_select_register(&mut self, cx: &mut Context<'_, Self>) {
        self.input_state_machine
            .update(cx, |sm, cx| sm.arm_select_register(cx));
    }

    fn dispatch_replace_char(&mut self, cx: &mut Context<'_, Self>) {
        self.input_state_machine
            .update(cx, |sm, cx| sm.arm_replace_char(cx));
    }

    fn dispatch_insert_register(&mut self, cx: &mut Context<'_, Self>) {
        self.input_state_machine
            .update(cx, |sm, cx| sm.arm_insert_register(cx));
    }

    fn apply_register_select_char(&mut self, ch: char) {
        if let Some(register) = stoat::action_handlers::yank::register_for_char(ch) {
            self.set_selected_register(register);
        }
    }

    fn dispatch_surround_add(&mut self, cx: &mut Context<'_, Self>) {
        self.input_state_machine
            .update(cx, |sm, cx| sm.arm_surround_add(cx));
    }

    fn dispatch_surround_delete(&mut self, cx: &mut Context<'_, Self>) {
        self.input_state_machine
            .update(cx, |sm, cx| sm.arm_surround_delete(cx));
    }

    fn dispatch_surround_replace(&mut self, cx: &mut Context<'_, Self>) {
        self.input_state_machine
            .update(cx, |sm, cx| sm.arm_surround_replace(cx));
    }

    /// Re-feed each captured keystroke through the input state
    /// machine and dispatch the resulting actions. Resolves the
    /// chord-completing char to a [`stoat::register::Register`]
    /// via [`stoat::action_handlers::yank::register_for_char`];
    /// no-op when the char does not name a register or the
    /// register has no stored macro.
    ///
    /// FIXME: nested replay loops are not guarded -- a macro that
    /// arms ReplayMacro then types a register char will recurse.
    fn apply_replay_macro_char(
        &mut self,
        ch: char,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(register) = stoat::action_handlers::yank::register_for_char(ch) else {
            return;
        };
        let Some(keys) = self
            .input_state_machine
            .read(cx)
            .macro_for_register(register)
        else {
            return;
        };
        for ks in keys {
            let actions = self
                .input_state_machine
                .update(cx, |sm, cx| sm.feed(&ks, window, cx));
            for action in actions {
                self.dispatch_action(action, window, cx);
            }
        }
    }

    fn dispatch_save_selection(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        editor.update(cx, |ed, cx| ed.handle_save_selection(cx));
    }

    fn dispatch_save_buffer(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let buffer = editor
            .read(cx)
            .multi_buffer()
            .read(cx)
            .as_singleton()
            .cloned();
        let Some(buffer) = buffer else {
            return;
        };
        buffer.update(cx, |b, cx| b.save(cx));
    }

    /// Flip the active editor's blame-strip visibility. On toggle-on,
    /// ensures the editor has the workspace-shared
    /// [`crate::git::blame::BlameState`] for its buffer attached and
    /// schedules a [`stoat::host::GitRepo::blame_path`] refresh against
    /// the workspace's git host. Scratch buffers (no file path) flip
    /// the flag but skip the refresh: the host method has nothing to
    /// blame outside the workdir.
    fn dispatch_toggle_blame(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let new_visible = !editor.read(cx).blame_visible();
        editor.update(cx, |ed, cx| ed.set_blame_visible(new_visible, cx));
        if new_visible {
            self.attach_and_refresh_blame(&editor, cx);
        }
    }

    /// Flip the active editor's inline-blame visibility -- the
    /// end-of-line alternative to the gutter strip. On toggle-on,
    /// attaches the shared [`crate::git::blame::BlameState`] and
    /// schedules a refresh; scratch buffers flip the flag but skip the
    /// refresh.
    fn dispatch_toggle_inline_blame(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let new_visible = !editor.read(cx).inline_blame_visible();
        editor.update(cx, |ed, cx| ed.set_inline_blame_visible(new_visible, cx));
        if new_visible {
            self.attach_and_refresh_blame(&editor, cx);
        }
    }

    /// Attach the workspace-shared [`crate::git::blame::BlameState`] for
    /// `editor`'s singleton buffer and schedule a blame refresh against
    /// the git host. A no-op for buffers without an on-disk path
    /// (scratch, modal inputs). Shared by both blame toggles and the
    /// editor-open path when inline blame starts visible from settings.
    fn attach_and_refresh_blame(&mut self, editor: &Entity<Editor>, cx: &mut Context<'_, Self>) {
        let Some(buffer) = editor
            .read(cx)
            .multi_buffer()
            .read(cx)
            .as_singleton()
            .cloned()
        else {
            return;
        };
        let Some(path) = buffer.read(cx).file_path().map(Path::to_path_buf) else {
            return;
        };
        let buffer_id = buffer.read(cx).read(|b| b.buffer_id());

        let state = self
            .blame_coordinator
            .update(cx, |coord, cx| coord.state_for(buffer_id, buffer, cx));
        editor.update(cx, |ed, cx| ed.set_blame_state(Some(state), cx));
        self.blame_coordinator
            .update(cx, |coord, cx| coord.refresh(buffer_id, path, cx));
    }

    /// Flip the active editor's minimap visibility and repaint. The
    /// minimap is a reduced-scale mirror column painted alongside the
    /// editor by sibling work; this only toggles the visibility flag.
    /// No-op when no editor is focused.
    fn dispatch_toggle_minimap(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let new_visible = !editor.read(cx).minimap_visible();
        editor.update(cx, |ed, cx| ed.set_minimap_visible(new_visible, cx));
    }

    /// Apply a runtime `Set { key, value }` action against the
    /// `Settings` global, parsing `value` against the key's typed
    /// schema via [`stoat_config::Settings::apply_runtime`]. Unknown
    /// keys and unparseable values log a warning and leave the
    /// global untouched.
    fn dispatch_set(&mut self, action: &dyn stoat_action::Action, cx: &mut Context<'_, Self>) {
        let Some(set) = action.as_any().downcast_ref::<stoat_action::Set>() else {
            return;
        };
        let key = set.key.clone();
        let value = set.value.clone();
        if !cx.has_global::<Settings>() {
            cx.set_global(Settings::default());
        }
        let result = cx.update_global::<Settings, _>(|s, _| s.resolved.apply_runtime(&key, &value));
        if let Err(err) = result {
            tracing::warn!(
                target: "stoat_gui::settings",
                ?err,
                %key,
                %value,
                "set: apply_runtime failed",
            );
        }
    }

    /// Flip the per-pane tab bar visibility via the `Settings` global.
    /// Reads the resolved `ui_pane_show_tab_bar` (defaulting to `true`
    /// when unset) and writes the negation back. Each `Pane` observes
    /// the global, so every pane repaints on the next frame.
    fn dispatch_toggle_tab_bar(&mut self, cx: &mut Context<'_, Self>) {
        let current = cx
            .try_global::<Settings>()
            .and_then(|s| s.resolved.ui_pane_show_tab_bar)
            .unwrap_or(true);
        if !cx.has_global::<Settings>() {
            cx.set_global(Settings::default());
        }
        cx.update_global::<Settings, _>(|s, _| {
            s.resolved.ui_pane_show_tab_bar = Some(!current);
        });
    }

    /// Cycle the gutter line-number mode via the `Settings` global:
    /// absolute -> relative -> hybrid -> absolute. Each `Pane` observes
    /// the global, so every editor repaints on the next frame.
    fn dispatch_toggle_relative_line_numbers(&mut self, cx: &mut Context<'_, Self>) {
        let current = cx
            .try_global::<Settings>()
            .and_then(|s| s.resolved.ui_editor_line_numbers)
            .unwrap_or(LineNumberMode::Absolute);
        let next = match current {
            LineNumberMode::Absolute => LineNumberMode::Relative,
            LineNumberMode::Relative => LineNumberMode::Hybrid,
            LineNumberMode::Hybrid => LineNumberMode::Absolute,
        };
        if !cx.has_global::<Settings>() {
            cx.set_global(Settings::default());
        }
        cx.update_global::<Settings, _>(|s, _| {
            s.resolved.ui_editor_line_numbers = Some(next);
        });
    }

    /// Fold the smallest syntactic container enclosing the cursor.
    fn dispatch_fold_at_cursor(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let Some(range) = fold_actions::fold_container_at(editor.read(cx), cx) else {
            return;
        };
        editor.update(cx, |ed, cx| {
            ed.display_map()
                .update(cx, |dm, dm_cx| dm.fold(vec![range], dm_cx));
        });
    }

    /// Unfold the container fold enclosing the cursor.
    fn dispatch_unfold_at_cursor(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let Some(range) = fold_actions::fold_container_at(editor.read(cx), cx) else {
            return;
        };
        editor.update(cx, |ed, cx| {
            ed.display_map()
                .update(cx, |dm, dm_cx| dm.unfold(vec![range], dm_cx));
        });
    }

    /// Fold every top-level syntactic container in the active editor.
    fn dispatch_fold_all(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let ranges = fold_actions::top_level_fold_ranges(editor.read(cx), cx);
        if ranges.is_empty() {
            return;
        }
        editor.update(cx, |ed, cx| {
            ed.display_map()
                .update(cx, |dm, dm_cx| dm.fold(ranges, dm_cx));
        });
    }

    /// Clear every fold in the active editor.
    fn dispatch_unfold_all(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let range = fold_actions::whole_buffer_range(editor.read(cx), cx);
        editor.update(cx, |ed, cx| {
            ed.display_map()
                .update(cx, |dm, dm_cx| dm.unfold(vec![range], dm_cx));
        });
    }

    /// Open a new gpui window hosting a clone of the current
    /// workspace's state: same pane tree, items, docks, and
    /// buffers. Refreshes the snapshot's `uid` so the copy
    /// does not collide with the source workspace's persistence
    /// file. No-op when [`ExecutorGlobal`] is absent (the uid
    /// generator depends on it); errors from `cx.open_window`
    /// are logged.
    fn dispatch_copy_workspace(&mut self, cx: &mut Context<'_, Self>) {
        let Some(executor) = cx.try_global::<ExecutorGlobal>().map(|g| g.0.clone()) else {
            tracing::warn!("CopyWorkspace: ExecutorGlobal missing, cannot refresh uid");
            return;
        };
        let mut state = self.to_state(cx);
        let mut new_uid = stoat::workspace::WorkspaceUid::now(&executor);
        if new_uid == state.uid {
            new_uid = stoat::workspace::WorkspaceUid(new_uid.0.wrapping_add(1));
        }
        state.uid = new_uid;

        let bounds = Bounds::centered(None, size(px(1200.0), px(800.0)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Stoat")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_window, cx| cx.new(|cx| crate::stoat_app::StoatApp::new_with_state(state, cx)),
        );
        if let Err(err) = result {
            tracing::warn!(?err, "CopyWorkspace: failed to open window");
        }
    }

    /// Spawn a fresh `StoatApp` in a new gpui window. The new
    /// window is independent of the current one: it anchors at
    /// the current working directory and seeds a scratch editor.
    /// Errors from `cx.open_window` are logged but do not propagate.
    fn dispatch_new_workspace(&mut self, cx: &mut Context<'_, Self>) {
        let bounds = Bounds::centered(None, size(px(1200.0), px(800.0)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Stoat")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| {
                cx.new(|cx| {
                    crate::stoat_app::StoatApp::new(Vec::new(), crate::RestoreMode::None, cx)
                })
            },
        );
        if let Err(err) = result {
            tracing::warn!(?err, "NewWorkspace: failed to open window");
        }
    }

    /// Whether `side`'s dock group is currently rendered. Each
    /// individual dock keeps its own `DockVisibility`; this bool
    /// gates the whole side and is flipped by [`Self::toggle_dock`].
    pub fn dock_side_visible(&self, side: DockSide) -> bool {
        match side {
            DockSide::Left => self.left_dock_visible,
            DockSide::Right => self.right_dock_visible,
            DockSide::Bottom => self.bottom_dock_visible,
        }
    }

    /// Flip the workspace-level visibility of `side`'s dock group
    /// and trigger a repaint. Preserves each individual dock's
    /// `DockVisibility` across the toggle.
    fn toggle_dock(&mut self, side: DockSide, cx: &mut Context<'_, Self>) {
        let visible = match side {
            DockSide::Left => &mut self.left_dock_visible,
            DockSide::Right => &mut self.right_dock_visible,
            DockSide::Bottom => &mut self.bottom_dock_visible,
        };
        *visible = !*visible;
        cx.notify();
    }

    /// Remove the active tab from the focused pane, leaving the
    /// pane in place (even when it becomes empty). The pane's own
    /// `remove_item` clamps the active index to the remaining
    /// items. No-op when the focused pane is already empty.
    fn close_active_buffer(&mut self, cx: &mut Context<'_, Self>) {
        let focus = self.pane_tree.read(cx).focus();
        let Some(pane) = self.pane_tree.read(cx).pane(focus).cloned() else {
            return;
        };
        let index = pane.read(cx).active_index();
        pane.update(cx, |p, cx| {
            p.remove_item(index, cx);
        });
    }

    /// Split the focused pane along `axis` and open a freshly
    /// allocated scratch buffer in the new (now-focused) pane.
    fn dispatch_split_new(&mut self, axis: Axis, cx: &mut Context<'_, Self>) {
        let new_pane_id = self.pane_tree.update(cx, |tree, cx| tree.split(axis, cx));
        self.open_scratch_in_pane(new_pane_id, cx);
    }

    /// Allocate a fresh scratch buffer through the workspace's
    /// [`BufferRegistry`] and add a backing editor to the pane at
    /// `pane_id`. No-op when the pane id is unknown.
    fn open_scratch_in_pane(&mut self, pane_id: stoat::pane::PaneId, cx: &mut Context<'_, Self>) {
        let weak_workspace = cx.weak_entity();
        let (_buffer_id, shared) = self
            .buffer_registry
            .update(cx, |reg, cx| reg.new_scratch(cx));
        let buffer = cx.new(|_| Buffer::from_shared(shared));
        let multi_buffer = {
            let buffer = buffer.clone();
            cx.new(|cx| MultiBuffer::singleton(buffer, cx))
        };
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let display_map = {
            let buffer = buffer.clone();
            cx.new(|cx| DisplayMap::new(buffer, executor, cx))
        };
        let diff_map = cx.new(|cx| DiffMap::new(buffer, cx));
        let editor =
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx));
        editor.update(cx, |ed, _| ed.set_workspace(Some(weak_workspace)));

        let Some(pane) = self.pane_tree.read(cx).pane(pane_id).cloned() else {
            return;
        };
        pane.update(cx, |p, cx| {
            p.add_item(Box::new(editor), cx);
        });
    }

    /// Toggle the right-side `DiffHunkPanel`. When the panel is open,
    /// remove the dock that hosts it; otherwise create a panel for
    /// the active editor and add it as a right-side dock. No-op when
    /// no editor is active.
    fn dispatch_toggle_diff_hunk_panel(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(panel_id) = self.diff_hunk_panel {
            let existing_index = self
                .docks
                .iter()
                .position(|d| d.read(cx).item().item_id() == panel_id);
            self.diff_hunk_panel = None;
            if let Some(idx) = existing_index {
                self.remove_dock(idx, cx);
            }
            return;
        }
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let panel = cx.new(|cx| crate::diff_hunk_panel::DiffHunkPanel::new(&editor, cx));
        let panel_id = panel.entity_id();
        self.add_dock(Box::new(panel), DockSide::Right, 240, cx);
        self.diff_hunk_panel = Some(panel_id);
    }

    /// Toggle the left-side project file tree dock. When a project tree
    /// dock is open, remove it and return to normal mode; otherwise
    /// build one listing the workspace root, add it as a left-side
    /// dock, and enter `project_tree` mode so navigation keys route to
    /// the tree.
    fn dispatch_toggle_project_tree(&mut self, cx: &mut Context<'_, Self>) {
        let existing = self
            .docks
            .iter()
            .position(|d| d.read(cx).item().item_kind(cx) == crate::item::ItemKind::ProjectTree);
        if let Some(idx) = existing {
            self.remove_dock(idx, cx);
            self.set_input_mode("normal", cx);
            return;
        }
        let git_root = self.git_root().clone();
        let fs = cx.global::<FsHostGlobal>().0.clone();
        let tree = cx.new(|cx| ProjectTree::new(git_root, fs, cx));
        self.add_dock(Box::new(tree), DockSide::Left, 240, cx);
        self.set_input_mode("project_tree", cx);
    }

    fn dispatch_toggle_outline_panel(&mut self, cx: &mut Context<'_, Self>) {
        let existing = self
            .docks
            .iter()
            .position(|d| d.read(cx).item().item_kind(cx) == crate::item::ItemKind::OutlinePanel);
        if let Some(idx) = existing {
            self.remove_dock(idx, cx);
            return;
        }
        let workspace = cx.entity();
        let panel = cx.new(|cx| crate::outline_panel::OutlinePanel::new(workspace, cx));
        self.add_dock(Box::new(panel), DockSide::Right, 240, cx);
    }

    fn dispatch_toggle_diagnostics_panel(&mut self, cx: &mut Context<'_, Self>) {
        let existing = self.docks.iter().position(|d| {
            d.read(cx).item().item_kind(cx) == crate::item::ItemKind::DiagnosticsPanel
        });
        if let Some(idx) = existing {
            self.remove_dock(idx, cx);
            return;
        }
        let workspace = cx.entity();
        let diagnostics = self.diagnostics().clone();
        let git_root = self.git_root().clone();
        let panel = cx.new(|cx| {
            crate::diagnostics_panel::DiagnosticsPanel::new(workspace, diagnostics, git_root, cx)
        });
        self.add_dock(Box::new(panel), DockSide::Right, 320, cx);
    }

    fn dispatch_open_markdown_preview(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let Some(buffer) = editor
            .read(cx)
            .multi_buffer()
            .read(cx)
            .as_singleton()
            .cloned()
        else {
            return;
        };
        let new_pane_id = self
            .pane_tree
            .update(cx, |tree, cx| tree.split(Axis::Vertical, cx));
        let preview = cx.new(|cx| crate::markdown_preview::MarkdownPreview::new(buffer, cx));
        let Some(pane) = self.pane_tree.read(cx).pane(new_pane_id).cloned() else {
            return;
        };
        pane.update(cx, |p, cx| {
            p.add_item(Box::new(preview), cx);
        });
    }

    /// Resolve the project tree hosted in a left dock, if one is open.
    fn active_project_tree(&self, cx: &App) -> Option<Entity<ProjectTree>> {
        self.docks.iter().find_map(|dock| {
            let dock = dock.read(cx);
            if dock.item().item_kind(cx) != crate::item::ItemKind::ProjectTree {
                return None;
            }
            dock.item().to_any_view().downcast::<ProjectTree>().ok()
        })
    }

    /// Run `f` against the open project tree, if any. The shared body
    /// of the `ProjectTree*` navigation dispatch arms.
    fn update_project_tree(
        &self,
        cx: &mut Context<'_, Self>,
        f: impl FnOnce(&mut ProjectTree, &mut Context<'_, ProjectTree>),
    ) {
        if let Some(tree) = self.active_project_tree(cx) {
            tree.update(cx, f);
        }
    }

    /// Confirm the project tree selection: toggle a directory, or open
    /// the selected file in the focused pane and return to normal mode.
    fn dispatch_project_tree_confirm(&mut self, cx: &mut Context<'_, Self>) {
        let Some(tree) = self.active_project_tree(cx) else {
            return;
        };
        let to_open = tree.update(cx, |tree, cx| tree.confirm(cx));
        if let Some(path) = to_open {
            self.open_paths(&[path], cx);
            self.set_input_mode("normal", cx);
        }
    }

    /// Open the delete-confirm modal for the selected project-tree entry.
    /// No-op when no tree is active or the tree is empty.
    fn dispatch_delete_tree_entry(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tree) = self.active_project_tree(cx) else {
            return;
        };
        let Some((path, name, is_dir)) = tree.read(cx).selected_entry() else {
            return;
        };
        crate::delete_tree_confirm::open_delete_tree_confirm(self, path, name, is_dir, window, cx);
    }

    /// Delete `path` from disk -- recursively when `is_dir` -- then refresh
    /// the project tree and raise a toast reporting success or the IO error.
    pub(crate) fn delete_tree_path(
        &mut self,
        path: PathBuf,
        is_dir: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(fs) = cx.try_global::<FsHostGlobal>().map(|g| g.0.clone()) else {
            return;
        };
        let result = if is_dir {
            fs.remove_dir_all(&path)
        } else {
            fs.remove_file(&path)
        };
        match result {
            Ok(()) => {
                self.show_toast(Toast::success("Deleted 1 item"), cx);
                self.update_project_tree(cx, ProjectTree::refresh);
            },
            Err(err) => {
                self.show_toast(Toast::error(format!("Delete failed: {err}")), cx);
            },
        }
    }

    /// Set the input state machine's mode without focus side effects.
    fn set_input_mode(&self, mode: &str, cx: &mut Context<'_, Self>) {
        self.input_state_machine
            .update(cx, |sm, cx_sm| sm.set_mode(mode, cx_sm));
    }

    fn dispatch_jump(&mut self, dir: JumpDir, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| match dir {
            JumpDir::Backward => ed.handle_jump_backward(count, cx),
            JumpDir::Forward => ed.handle_jump_forward(count, cx),
        });
    }

    fn active_review_item(&self, cx: &App) -> Option<Entity<crate::review_item::ReviewItem>> {
        self.active_pane_item(cx).and_then(|item| {
            item.to_any_view()
                .downcast::<crate::review_item::ReviewItem>()
                .ok()
        })
    }

    fn active_conflict_item(&self, cx: &App) -> Option<Entity<ConflictItem>> {
        self.active_pane_item(cx)
            .and_then(|item| item.to_any_view().downcast::<ConflictItem>().ok())
    }

    fn active_rebase_item(&self, cx: &App) -> Option<Entity<RebaseItem>> {
        self.active_pane_item(cx)
            .and_then(|item| item.to_any_view().downcast::<RebaseItem>().ok())
    }

    fn dispatch_enter_rebase(&mut self, cx: &mut Context<'_, Self>) {
        let Some(commit_list) = self.active_commit_list(cx) else {
            return;
        };
        let plan = {
            let state = commit_list.read(cx).state().read(cx);
            let inner = state.inner();
            if inner.selected == 0 || inner.selected >= inner.commits.len() {
                tracing::warn!(
                    action = "EnterRebase",
                    selected = inner.selected,
                    "nothing to rebase: select an older commit first"
                );
                return;
            }
            let entries: Vec<RebaseEntry> = inner.commits[..inner.selected]
                .iter()
                .rev()
                .cloned()
                .map(|commit| RebaseEntry {
                    op: RebaseTodoOp::Pick,
                    commit,
                })
                .collect();
            let onto = inner.commits[inner.selected].sha.clone();
            stoat::rebase::RebaseState::new(inner.workdir.clone(), onto, entries)
        };
        let item = cx.new(|cx| RebaseItem::new(plan, cx));
        self.open_item(Box::new(item), cx);
    }

    fn dispatch_rebase_move(&mut self, dir: RebaseMoveDir, cx: &mut Context<'_, Self>) {
        let Some(rebase_item) = self.active_rebase_item(cx) else {
            return;
        };
        rebase_item.update(cx, |item, cx| item.handle_move(dir, cx));
    }

    fn dispatch_rebase_set_op(&mut self, op: RebaseTodoOp, cx: &mut Context<'_, Self>) {
        let Some(rebase_item) = self.active_rebase_item(cx) else {
            return;
        };
        rebase_item.update(cx, |item, cx| item.handle_set_op(op, cx));
    }

    fn dispatch_execute_rebase(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.rebase_active.is_some() {
            tracing::warn!("ExecuteRebase: a rebase is already in flight; ignoring");
            return;
        }
        let Some(rebase_item) = self.active_rebase_item(cx) else {
            return;
        };
        let plan = rebase_item.read(cx).take_plan(cx);
        let workdir = plan.workdir.clone();

        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&workdir) else {
            tracing::warn!("ExecuteRebase: no git repo at {}", workdir.display());
            return;
        };
        if !repo.changed_files().is_empty() {
            tracing::warn!("ExecuteRebase: working tree dirty; commit or stash first");
            return;
        }

        self.rebase_active = Some(ActiveRebase::new(plan));

        let pane_id = self.pane_tree.read(cx).focus();
        let Some(pane) = self.pane_tree.read(cx).pane(pane_id).cloned() else {
            return;
        };
        pane.update(cx, |p, cx| {
            let idx = p.items().iter().position(|item| {
                item.to_any_view()
                    .downcast::<RebaseItem>()
                    .ok()
                    .is_some_and(|entity| entity == rebase_item)
            });
            if let Some(idx) = idx {
                p.remove_item(idx, cx);
            }
        });

        self.continue_rebase_atomic(pane, window, cx);
    }

    fn dispatch_abort_rebase(&mut self, cx: &mut Context<'_, Self>) {
        let Some(rebase_item) = self.active_rebase_item(cx) else {
            return;
        };
        let pane_id = self.pane_tree.read(cx).focus();
        let Some(pane) = self.pane_tree.read(cx).pane(pane_id).cloned() else {
            return;
        };
        pane.update(cx, |p, cx| {
            let idx = p.items().iter().position(|item| {
                item.to_any_view()
                    .downcast::<RebaseItem>()
                    .ok()
                    .is_some_and(|entity| entity == rebase_item)
            });
            if let Some(idx) = idx {
                p.remove_item(idx, cx);
            }
        });
    }

    fn dispatch_rebase_continue(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(active) = self.rebase_active.as_ref() else {
            return;
        };
        if !matches!(active.pause, Some(RebasePause::Edit { .. })) {
            return;
        }
        if let Some(active) = self.rebase_active.as_mut() {
            active.pause = None;
        }

        let pane_id = self.pane_tree.read(cx).focus();
        let Some(pane) = self.pane_tree.read(cx).pane(pane_id).cloned() else {
            return;
        };
        self.continue_rebase_atomic(pane, window, cx);
    }

    /// Commit the rewritten message for an active
    /// [`RebasePause::Reword`] and resume the stepper. Mirrors the
    /// TUI's `reword_confirm` semantics: an empty (whitespace-only)
    /// message auto-aborts; on git-backend failure the pause stays
    /// installed so the modal can retry.
    pub(crate) fn commit_reword(
        &mut self,
        new_message: String,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let trimmed = new_message.trim().to_string();
        if trimmed.is_empty() {
            self.abort_reword(cx);
            return;
        }

        let (workdir, picked_sha, fallback_parent) = {
            let Some(active) = self.rebase_active.as_ref() else {
                return;
            };
            let Some(RebasePause::Reword {
                cherry_picked_commit,
                ..
            }) = active.pause.as_ref()
            else {
                return;
            };
            (
                active.workdir.clone(),
                cherry_picked_commit.clone(),
                active.current_head.clone(),
            )
        };

        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&workdir) else {
            tracing::warn!("RewordConfirm: no git repo at {}", workdir.display());
            return;
        };
        let Some(tree) = repo.commit_tree(&picked_sha) else {
            tracing::warn!("RewordConfirm: commit tree unreadable for {picked_sha}");
            return;
        };
        let parent = repo.parent_sha(&picked_sha).or(Some(fallback_parent));

        let new_sha = match repo.create_commit(
            parent.as_deref(),
            &tree,
            &trimmed,
            "stoat",
            "stoat@example.invalid",
        ) {
            Ok(sha) => sha,
            Err(GitApplyError::Backend { reason, .. }) => {
                tracing::warn!("RewordConfirm: create_commit failed: {reason}");
                return;
            },
        };

        if let Some(active) = self.rebase_active.as_mut() {
            active.current_head = new_sha.clone();
            active.last_pick_sha = Some(new_sha);
            active.last_message = Some(trimmed);
            active.pause = None;
        }

        let pane_id = self.pane_tree.read(cx).focus();
        let Some(pane) = self.pane_tree.read(cx).pane(pane_id).cloned() else {
            return;
        };
        self.continue_rebase_atomic(pane, window, cx);
    }

    /// Tear down an in-progress [`RebasePause::Reword`]: clear
    /// `rebase_active` and leave HEAD where the stepper had last
    /// advanced it. No-op when the active pause is not a Reword.
    pub(crate) fn abort_reword(&mut self, cx: &mut Context<'_, Self>) {
        let is_reword = matches!(
            self.rebase_active.as_ref().and_then(|a| a.pause.as_ref()),
            Some(RebasePause::Reword { .. })
        );
        if !is_reword {
            return;
        }
        self.rebase_active = None;
        cx.notify();
    }

    fn dispatch_conflict_take_side(&mut self, side: ConflictSide, cx: &mut Context<'_, Self>) {
        let Some(conflict_item) = self.active_conflict_item(cx) else {
            return;
        };
        conflict_item.update(cx, |item, cx| item.take_side(side, cx));
    }

    fn dispatch_conflict_skip_entry(&mut self, cx: &mut Context<'_, Self>) {
        let Some(conflict_item) = self.active_conflict_item(cx) else {
            return;
        };
        conflict_item.update(cx, |item, cx| item.skip_entry(cx));
    }

    fn dispatch_conflict_apply(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(active) = self.rebase_active.as_ref() else {
            return;
        };
        let Some(RebasePause::Conflict {
            source_sha, files, ..
        }) = active.pause.as_ref()
        else {
            return;
        };
        let source_sha = source_sha.clone();
        let conflicted_paths: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
        let workdir = active.workdir.clone();
        let current_head = active.current_head.clone();

        let pane_id = self.pane_tree.read(cx).focus();
        let Some(pane) = self.pane_tree.read(cx).pane(pane_id).cloned() else {
            return;
        };

        let resolutions: HashMap<PathBuf, String> = pane
            .read(cx)
            .items()
            .iter()
            .filter_map(|item| {
                let conflict = item.to_any_view().downcast::<ConflictItem>().ok()?;
                let conflict = conflict.read(cx);
                Some((
                    conflict.path().to_path_buf(),
                    conflict.result_buffer_text(cx),
                ))
            })
            .collect();

        if !conflicted_paths.iter().all(|p| resolutions.contains_key(p)) {
            tracing::warn!(
                "ConflictApply: not every conflicted file has an open view; aborting apply",
            );
            return;
        }

        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&workdir) else {
            tracing::warn!("ConflictApply: no git repo at {}", workdir.display());
            return;
        };
        let Some(mut tree) = repo.commit_tree(&current_head) else {
            tracing::warn!("ConflictApply: current_head {current_head} has no tree",);
            return;
        };
        for path in &conflicted_paths {
            if let Some(text) = resolutions.get(path) {
                tree.insert(path.clone(), text.clone());
            }
        }

        let message = format!("conflict-resolved {source_sha}");
        let new_sha = match repo.create_commit(
            Some(&current_head),
            &tree,
            &message,
            "stoat",
            "stoat@example.invalid",
        ) {
            Ok(sha) => sha,
            Err(GitApplyError::Backend { reason, .. }) => {
                tracing::warn!("ConflictApply: create_commit failed: {reason}");
                return;
            },
        };

        if let Some(active) = self.rebase_active.as_mut() {
            active.current_head = new_sha.clone();
            active.last_pick_sha = Some(new_sha);
            active.last_message = Some(message);
            active.pause = None;
        }

        pane.update(cx, |p, cx| {
            let conflict_indices: Vec<usize> = p
                .items()
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    item.to_any_view()
                        .downcast::<ConflictItem>()
                        .ok()
                        .map(|_| i)
                })
                .collect();
            for idx in conflict_indices.into_iter().rev() {
                p.remove_item(idx, cx);
            }
        });

        self.continue_rebase_atomic(pane, window, cx);
    }

    /// Drive the rebase forward after a `ConflictApply` resolves a
    /// pause, or kick off execution from `ExecuteRebase`. With no
    /// remaining entries, finalize: point HEAD at `current_head` and
    /// clear `rebase_active`. Otherwise pick a path:
    ///
    /// - If `remaining` contains any [`RebaseTodoOp::Reword`] or [`RebaseTodoOp::Edit`] entry,
    ///   dispatch to [`Self::drive_rebase_step`] -- one executor task per entry so we can install
    ///   pauses between them.
    /// - Otherwise spawn the atomic [`stoat::host::GitRepo::run_rebase`] call on the executor
    ///   ([`Self::apply_rebase_outcome`] for the main-thread landing) which finalizes the plan in
    ///   one shot.
    fn continue_rebase_atomic(
        &mut self,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(active) = self.rebase_active.as_ref() else {
            return;
        };
        if active.pause.is_some() {
            return;
        }

        if active
            .remaining
            .iter()
            .any(|e| matches!(e.op, RebaseTodoOp::Reword | RebaseTodoOp::Edit))
        {
            self.drive_rebase_step(pane, window, cx);
            return;
        }

        let workdir = active.workdir.clone();
        let onto = active.current_head.clone();
        let todo: Vec<RebaseTodo> = active
            .remaining
            .iter()
            .map(|e| RebaseTodo {
                op: e.op,
                sha: e.commit.sha.clone(),
                message: e.commit.summary.clone(),
            })
            .collect();

        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&workdir) else {
            tracing::warn!("rebase: no git repo at {}", workdir.display());
            self.rebase_active = None;
            return;
        };

        if todo.is_empty() {
            let _ = repo.update_head(&onto);
            self.rebase_active = None;
            return;
        }

        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let weak_pane = pane.downgrade();
        cx.spawn(async move |weak_self, cx| {
            let outcome = executor
                .spawn(async move { execute_rebase_plan(repo.as_ref(), &onto, &todo) })
                .await;
            let _ = weak_self.update(cx, |this, cx| {
                let Some(pane) = weak_pane.upgrade() else {
                    this.rebase_active = None;
                    return;
                };
                this.apply_rebase_outcome(outcome, pane, cx);
            });
        })
        .detach();
    }

    /// Stepper-driven execute path used when the plan contains
    /// [`RebaseTodoOp::Reword`] or [`RebaseTodoOp::Edit`] entries.
    /// Pops one entry, spawns an executor task running
    /// [`execute_rebase_step`], then routes the outcome through
    /// [`Self::apply_step_outcome`] which either continues the
    /// loop (recursing) or installs a pause and stops.
    ///
    /// With no remaining entries this finalizes the rebase the
    /// same way the atomic path does -- HEAD update + clear
    /// `rebase_active` -- so a stepper-driven plan finishes
    /// cleanly when its last entry was a clean Pick/Drop.
    fn drive_rebase_step(
        &mut self,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(active) = self.rebase_active.as_ref() else {
            return;
        };
        if active.pause.is_some() {
            return;
        }

        let workdir = active.workdir.clone();
        let current_head = active.current_head.clone();
        let last_pick = active.last_pick_sha.clone();
        let last_message = active.last_message.clone();

        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&workdir) else {
            tracing::warn!("rebase: no git repo at {}", workdir.display());
            self.rebase_active = None;
            return;
        };

        let Some(entry) = active.remaining.front().cloned() else {
            let _ = repo.update_head(&current_head);
            self.rebase_active = None;
            return;
        };

        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let weak_pane = pane.downgrade();
        cx.spawn_in(window, async move |weak_self, cx| {
            let outcome = executor
                .spawn(async move {
                    execute_rebase_step(
                        repo.as_ref(),
                        &entry,
                        &current_head,
                        last_pick.as_deref(),
                        last_message.as_deref(),
                    )
                })
                .await;
            let _ = weak_self.update_in(cx, |this, window, cx| {
                let Some(pane) = weak_pane.upgrade() else {
                    this.rebase_active = None;
                    return;
                };
                this.apply_step_outcome(outcome, pane, window, cx);
            });
        })
        .detach();
    }

    /// Land a [`RebaseStepOutcome`] on the main thread. `Step` and
    /// `Drop` advance the cursor and recurse via
    /// [`Self::drive_rebase_step`]; `Reword` / `Edit` install the
    /// matching [`RebasePause`] and stop (UI surfaces the pause).
    /// `Conflict` mirrors the atomic-path conflict handling
    /// (drop entries up to the failing sha, install pause, open
    /// [`ConflictItem`] views). `Aborted` warns and clears state.
    ///
    /// For [`RebasePause::Reword`] this opens a [`RewordModal`]
    /// seeded with the original commit message; the modal owns the
    /// confirm/abort handoff back into the workspace. For
    /// [`RebasePause::Edit`] this opens a [`ReviewSource::Commit`]
    /// review of the just-applied commit in `pane` -- the user can
    /// dismiss or modify it before dispatching `RebaseContinue` to
    /// resume.
    fn apply_step_outcome(
        &mut self,
        outcome: RebaseStepOutcome,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match outcome {
            RebaseStepOutcome::Step {
                new_head,
                new_message,
                op,
            } => {
                if let Some(active) = self.rebase_active.as_mut() {
                    active.remaining.pop_front();
                    active.current_head = new_head.clone();
                    if matches!(
                        op,
                        RebaseTodoOp::Pick | RebaseTodoOp::Squash | RebaseTodoOp::Fixup
                    ) {
                        active.last_pick_sha = Some(new_head);
                        active.last_message = Some(new_message);
                    }
                }
                self.drive_rebase_step(pane, window, cx);
            },
            RebaseStepOutcome::Drop => {
                if let Some(active) = self.rebase_active.as_mut() {
                    active.remaining.pop_front();
                }
                self.drive_rebase_step(pane, window, cx);
            },
            RebaseStepOutcome::Reword {
                cherry_picked_commit,
                original_message,
            } => {
                if let Some(active) = self.rebase_active.as_mut() {
                    active.remaining.pop_front();
                    active.current_head = cherry_picked_commit.clone();
                    active.last_pick_sha = Some(cherry_picked_commit.clone());
                    active.last_message = Some(original_message.clone());
                    active.pause = Some(RebasePause::Reword {
                        cherry_picked_commit,
                        original_message: original_message.clone(),
                    });
                } else {
                    return;
                }
                let weak_workspace = cx.weak_entity();
                self.toggle_modal::<crate::reword_modal::RewordModal, _>(
                    window,
                    cx,
                    |window, cx| {
                        crate::reword_modal::RewordModal::new(
                            weak_workspace,
                            &original_message,
                            window,
                            cx,
                        )
                    },
                );
                let _ = pane;
            },
            RebaseStepOutcome::Edit {
                cherry_picked_commit,
            } => {
                let workdir_for_review = self.rebase_active.as_ref().map(|a| a.workdir.clone());
                if let Some(active) = self.rebase_active.as_mut() {
                    active.remaining.pop_front();
                    active.current_head = cherry_picked_commit.clone();
                    active.last_pick_sha = Some(cherry_picked_commit.clone());
                    active.pause = Some(RebasePause::Edit {
                        cherry_picked_commit: cherry_picked_commit.clone(),
                    });
                }
                if let Some(workdir) = workdir_for_review {
                    self.open_review_source(
                        ReviewSource::Commit {
                            workdir,
                            sha: cherry_picked_commit,
                        },
                        "RebaseStepEdit",
                        cx,
                    );
                }
                let _ = pane;
            },
            RebaseStepOutcome::Conflict { at_sha, files } => {
                if let Some(active) = self.rebase_active.as_mut() {
                    let pos = active.remaining.iter().position(|e| e.commit.sha == at_sha);
                    if let Some(pos) = pos {
                        for _ in 0..=pos {
                            active.remaining.pop_front();
                        }
                    }
                    active.pause = Some(RebasePause::Conflict {
                        source_sha: at_sha,
                        files: files.clone(),
                        selected: 0,
                        resolutions: HashMap::new(),
                    });
                }
                pane.update(cx, |p, cx| {
                    for file in files {
                        let item = cx.new(|cx| ConflictItem::from_conflicted_file(file, cx));
                        let handle: Box<dyn ItemHandle> = Box::new(item);
                        p.add_item(handle, cx);
                    }
                });
            },
            RebaseStepOutcome::Aborted(reason) => {
                tracing::warn!("rebase: {reason}");
                self.rebase_active = None;
            },
        }
    }

    /// Land the result of an executor-backed [`run_rebase`] call.
    /// Mutates `rebase_active` and installs `ConflictItem` views in
    /// `pane` on conflict. Runs on the main thread, so it can touch
    /// gpui state freely.
    fn apply_rebase_outcome(
        &mut self,
        outcome: RebaseExecutionOutcome,
        pane: Entity<Pane>,
        cx: &mut Context<'_, Self>,
    ) {
        match outcome {
            RebaseExecutionOutcome::Clean { new_head } => {
                let workdir = self.rebase_active.as_ref().map(|a| a.workdir.clone());
                if let Some(workdir) = workdir {
                    let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
                    if let Some(repo) = git.discover(&workdir) {
                        let _ = repo.update_head(&new_head);
                    }
                }
                self.rebase_active = None;
            },
            RebaseExecutionOutcome::Conflict { at_sha, files } => {
                if let Some(active) = self.rebase_active.as_mut() {
                    let pos = active.remaining.iter().position(|e| e.commit.sha == at_sha);
                    if let Some(pos) = pos {
                        for _ in 0..=pos {
                            active.remaining.pop_front();
                        }
                    }
                    active.pause = Some(RebasePause::Conflict {
                        source_sha: at_sha,
                        files: files.clone(),
                        selected: 0,
                        resolutions: HashMap::new(),
                    });
                }
                pane.update(cx, |p, cx| {
                    for file in files {
                        let item = cx.new(|cx| ConflictItem::from_conflicted_file(file, cx));
                        let handle: Box<dyn ItemHandle> = Box::new(item);
                        p.add_item(handle, cx);
                    }
                });
            },
            RebaseExecutionOutcome::Aborted(reason) => {
                tracing::warn!("rebase: {reason}");
                self.rebase_active = None;
            },
        }
    }

    fn dispatch_conflict_abort(&mut self, cx: &mut Context<'_, Self>) {
        let Some(active) = self.rebase_active.as_ref() else {
            return;
        };
        let original_head = active.onto.clone();
        let workdir = active.workdir.clone();

        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        match git.discover(&workdir) {
            Some(repo) => {
                if let Err(GitApplyError::Backend { reason, .. }) = repo.update_head(&original_head)
                {
                    tracing::warn!("ConflictAbort: update_head({original_head}) failed: {reason}",);
                }
            },
            None => {
                tracing::warn!("ConflictAbort: no git repo at {}", workdir.display());
            },
        }

        self.rebase_active = None;

        let pane_id = self.pane_tree.read(cx).focus();
        let Some(pane) = self.pane_tree.read(cx).pane(pane_id).cloned() else {
            return;
        };
        pane.update(cx, |p, cx| {
            let conflict_indices: Vec<usize> = p
                .items()
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    item.to_any_view()
                        .downcast::<ConflictItem>()
                        .ok()
                        .map(|_| i)
                })
                .collect();
            for idx in conflict_indices.into_iter().rev() {
                p.remove_item(idx, cx);
            }
        });
    }

    fn dispatch_conflict_nav(&mut self, dir: ConflictNavDir, cx: &mut Context<'_, Self>) {
        let pane_id = self.pane_tree.read(cx).focus();
        let Some(pane) = self.pane_tree.read(cx).pane(pane_id).cloned() else {
            return;
        };
        pane.update(cx, |p, cx| {
            let unresolved: Vec<usize> = p
                .items()
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let conflict = item.to_any_view().downcast::<ConflictItem>().ok()?;
                    if conflict.read(cx).has_unresolved_conflicts(cx) {
                        Some(i)
                    } else {
                        None
                    }
                })
                .collect();
            if unresolved.is_empty() {
                return;
            }
            let current = p.active_index();
            let target = match dir {
                ConflictNavDir::Next => unresolved
                    .iter()
                    .find(|&&i| i > current)
                    .copied()
                    .or_else(|| unresolved.first().copied()),
                ConflictNavDir::Prev => unresolved
                    .iter()
                    .rev()
                    .find(|&&i| i < current)
                    .copied()
                    .or_else(|| unresolved.last().copied()),
            };
            if let Some(idx) = target {
                p.activate(idx, cx);
            }
        });
    }

    fn dispatch_search_step(&mut self, step: SearchStep, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        editor.update(cx, |ed, cx| match step {
            SearchStep::Next => ed.search_next(cx),
            SearchStep::Prev => ed.search_prev(cx),
        });
    }

    fn dispatch_goto_word(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let labels = editor.update(cx, |ed, cx| {
            let display_snapshot = ed.display_map().update(cx, |dm, _| dm.snapshot());
            let buffer_snapshot = display_snapshot.buffer_snapshot().clone();
            let rope = buffer_snapshot.rope();
            let scroll_row = ed.scroll_row();
            let viewport = ed.viewport_rows_for_page().max(1);
            let last_row = scroll_row.saturating_add(viewport.saturating_sub(1));
            let max_targets = stoat::goto_word::ALPHABET.len() * stoat::goto_word::ALPHABET.len();
            let targets =
                stoat::goto_word::find_word_starts(rope, scroll_row, last_row, max_targets);
            stoat::goto_word::assign_labels(&targets, stoat::goto_word::ALPHABET)
        });
        if labels.is_empty() {
            editor.update(cx, |ed, cx| ed.clear_pending_goto_word(cx));
            return;
        }
        editor.update(cx, |ed, cx| ed.arm_pending_goto_word(labels, cx));
    }

    fn dispatch_goto_word_jump(&mut self, offset: usize, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        editor.update(cx, |ed, cx| {
            ed.clear_pending_goto_word(cx);
            ed.jump_to_offset(offset, cx);
        });
    }

    fn dispatch_review_step(&mut self, dir: ReviewStepDir, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        review_item.update(cx, |item, cx| {
            let session = item.session().clone();
            let new_id = session.update(cx, |session, cx| match dir {
                ReviewStepDir::Next => session.next(cx),
                ReviewStepDir::Prev => session.prev(cx),
            });
            let Some(new_id) = new_id else { return };
            let target = session
                .read(cx)
                .inner()
                .chunks
                .get(&new_id)
                .map(|chunk| (chunk.file_index, chunk.buffer_line_range.start));
            let Some((file_index, buffer_row)) = target else {
                return;
            };
            let Some(file) = item.files().get(file_index) else {
                return;
            };
            let editor = file.editor.clone();
            editor.update(cx, |ed, cx| {
                ed.set_cursor_at_buffer_row(buffer_row, cx);
                ed.request_autoscroll(
                    crate::editor::scroll::autoscroll::AutoscrollStrategy::Center,
                    cx,
                );
            });
        });
    }

    fn dispatch_review_set_status(
        &mut self,
        change: ReviewStatusChange,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();
        let Some(id) = session.read(cx).inner().cursor.current else {
            return;
        };
        session.update(cx, |session, cx| match change {
            ReviewStatusChange::Stage => {
                session.set_status(id, ChunkStatus::Staged, cx);
            },
            ReviewStatusChange::Unstage => {
                session.set_status(id, ChunkStatus::Unstaged, cx);
            },
            ReviewStatusChange::Skip => {
                session.set_status(id, ChunkStatus::Skipped, cx);
            },
            ReviewStatusChange::Toggle => {
                session.toggle_stage(id, cx);
            },
        });
    }

    /// Clear approval and revert status to `Pending` for every chunk
    /// in the active review session, then scroll the editor to the
    /// reset cursor. No-op when no review item is focused.
    fn dispatch_review_reset_progress(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        review_item.update(cx, |item, cx| {
            let session = item.session().clone();
            session.update(cx, |session, cx| session.reset_progress(cx));
            let Some(new_id) = session.read(cx).inner().cursor.current else {
                return;
            };
            let target = session
                .read(cx)
                .inner()
                .chunks
                .get(&new_id)
                .map(|chunk| (chunk.file_index, chunk.buffer_line_range.start));
            let Some((file_index, buffer_row)) = target else {
                return;
            };
            let Some(file) = item.files().get(file_index) else {
                return;
            };
            let editor = file.editor.clone();
            editor.update(cx, |ed, cx| {
                ed.set_cursor_at_buffer_row(buffer_row, cx);
                ed.request_autoscroll(
                    crate::editor::scroll::autoscroll::AutoscrollStrategy::Center,
                    cx,
                );
            });
        });
    }

    /// Enter `line_select` mode on the chunk under the review cursor,
    /// snapshotting its rows all-selected. No-op (mode unchanged) when
    /// no review item is active or the cursor has no chunk.
    fn dispatch_review_enter_line_select(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let entered = review_item.update(cx, |item, cx| {
            let session = item.session().clone();
            session.update(cx, |session, cx| session.enter_line_select(cx))
        });
        if entered {
            self.set_input_mode("line_select", cx);
        }
    }

    /// Clear the active line selection and return to `review` mode.
    fn dispatch_review_line_select_cancel(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(review_item) = self.active_review_item(cx) {
            review_item.update(cx, |item, cx| {
                let session = item.session().clone();
                session.update(cx, |session, cx| session.cancel_line_select(cx));
            });
        }
        self.set_input_mode("review", cx);
    }

    /// Toggle the selected bit of the line under the active file's editor
    /// cursor. No-op when no review item is active, no line selection
    /// exists, or the cursor is not on a changed row.
    fn dispatch_review_line_select_toggle(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();

        let file_index = {
            let inner = session.read(cx).inner();
            inner
                .cursor
                .current
                .and_then(|id| inner.chunks.get(&id))
                .map(|chunk| chunk.file_index)
        };
        let Some(cursor_row) = file_index
            .and_then(|fi| {
                review_item
                    .read(cx)
                    .files()
                    .get(fi)
                    .map(|f| f.editor.clone())
            })
            .map(|editor| editor.read(cx).primary_cursor_buffer_row(cx))
        else {
            return;
        };

        session.update(cx, |session, cx| {
            session.toggle_line_select(cursor_row, cx);
        });
    }

    /// Select every row of the active line selection.
    fn dispatch_review_line_select_all(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();
        session.update(cx, |session, cx| {
            session.select_all_lines(cx);
        });
    }

    /// Stage (or unstage, when `unstage`) the active line selection's
    /// selected rows by applying its partial-hunk patch to the index,
    /// then clear the selection and return to `review` mode. No-op for
    /// non-WorkingTree sources or when no selection is active.
    fn dispatch_review_line_select_stage(&mut self, unstage: bool, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();

        let (workdir, id, plan) = {
            let inner = session.read(cx).inner();
            let workdir = match &inner.source {
                ReviewSource::WorkingTree { workdir } => workdir.clone(),
                _ => {
                    tracing::warn!(
                        "ReviewLineSelectStage: only WorkingTree sources stage to the index"
                    );
                    return;
                },
            };
            let Some(id) = inner.line_selection.as_ref().map(|s| s.hunk_id) else {
                return;
            };
            let Some(plan) = inner.plan_line_select_stage(unstage) else {
                return;
            };
            (workdir, id, plan)
        };

        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&workdir) else {
            tracing::warn!(
                "ReviewLineSelectStage: no git repo at {}",
                workdir.display()
            );
            return;
        };
        for patch in [plan.reverse.as_ref(), plan.forward.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Err(GitApplyError::Backend { reason, .. }) = repo.apply_to_index(patch) {
                tracing::warn!("ReviewLineSelectStage: apply_to_index failed: {reason}");
                return;
            }
        }

        session.update(cx, |session, cx| {
            session.set_chunk_staged_rows(id, plan.rows, plan.status, cx);
            session.cancel_line_select(cx);
        });
        self.set_input_mode("review", cx);
    }

    /// Advance the review cursor to the next chunk whose `approved`
    /// flag is `false`, wrapping past the end of the session. No-op
    /// when every chunk is approved or no review item is active.
    /// Scrolls the corresponding editor row into view when the cursor
    /// moves.
    fn dispatch_review_next_unreviewed(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        review_item.update(cx, |item, cx| {
            let session = item.session().clone();
            let new_id = session.update(cx, |session, cx| session.next_unreviewed(cx));
            let Some(new_id) = new_id else { return };
            let target = session
                .read(cx)
                .inner()
                .chunks
                .get(&new_id)
                .map(|chunk| (chunk.file_index, chunk.buffer_line_range.start));
            let Some((file_index, buffer_row)) = target else {
                return;
            };
            let Some(file) = item.files().get(file_index) else {
                return;
            };
            let editor = file.editor.clone();
            editor.update(cx, |ed, cx| {
                ed.set_cursor_at_buffer_row(buffer_row, cx);
                ed.request_autoscroll(
                    crate::editor::scroll::autoscroll::AutoscrollStrategy::Center,
                    cx,
                );
            });
        });
    }

    /// Approve the chunk under the review cursor. With `advance = true`
    /// (the `ReviewApproveHunk` mark-and-advance path), sets the
    /// approval flag and steps to the next chunk; the cursor jump
    /// scrolls the corresponding editor row into view. With
    /// `advance = false` (`ReviewToggleApproval`), flips the flag in
    /// place without moving the cursor.
    fn dispatch_review_approve(&mut self, advance: bool, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        review_item.update(cx, |item, cx| {
            let session = item.session().clone();
            let Some(id) = session.read(cx).inner().cursor.current else {
                return;
            };
            let new_id = session.update(cx, |session, cx| {
                if advance {
                    session.set_approved(id, true, cx);
                    session.next(cx)
                } else {
                    session.toggle_approved(id, cx);
                    None
                }
            });
            let Some(new_id) = new_id else { return };
            let target = session
                .read(cx)
                .inner()
                .chunks
                .get(&new_id)
                .map(|chunk| (chunk.file_index, chunk.buffer_line_range.start));
            let Some((file_index, buffer_row)) = target else {
                return;
            };
            let Some(file) = item.files().get(file_index) else {
                return;
            };
            let editor = file.editor.clone();
            editor.update(cx, |ed, cx| {
                ed.set_cursor_at_buffer_row(buffer_row, cx);
                ed.request_autoscroll(
                    crate::editor::scroll::autoscroll::AutoscrollStrategy::Center,
                    cx,
                );
            });
        });
    }

    fn dispatch_review_remove_selected(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();
        let session_ref = session.read(cx);
        let inner = session_ref.inner();
        if !matches!(inner.source, ReviewSource::WorkingTree { .. }) {
            return;
        }
        let Some(id) = inner.cursor.current else {
            return;
        };
        let Some(chunk) = inner.chunks.get(&id) else {
            return;
        };
        let Some(file) = inner.files.get(chunk.file_index) else {
            return;
        };
        let file_index = chunk.file_index;
        let new_buffer = remove_chunks_from_buffer(&file.base_text, &file.buffer_text, &[chunk]);

        let Some(buffer) = review_item
            .read(cx)
            .files()
            .get(file_index)
            .map(|f| f.buffer.clone())
        else {
            return;
        };
        let len = buffer.read(cx).read(|b| b.rope().len());
        buffer.update(cx, |b, cx| b.edit(0..len, &new_buffer, cx));
    }

    /// Stage or unstage the chunk under the review cursor directly
    /// against the git index, bypassing the batch apply flow. With
    /// `force_unstage` the chunk is always reversed out of the index;
    /// otherwise a currently-`Staged` chunk is unstaged and any other
    /// chunk is staged. On a successful index apply the chunk's status
    /// follows (`Staged` on stage, `Pending` on unstage). No-op unless
    /// the source is a working tree and a chunk is under the cursor.
    fn dispatch_git_stage_hunk(&mut self, force_unstage: bool, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();

        let (workdir, id, patch, next_status) = {
            let inner = session.read(cx).inner();
            let workdir = match &inner.source {
                ReviewSource::WorkingTree { workdir } => workdir.clone(),
                _ => {
                    tracing::warn!("GitStageHunk: only WorkingTree sources stage to the index");
                    return;
                },
            };
            let Some(id) = inner.cursor.current else {
                return;
            };
            let Some(chunk) = inner.chunks.get(&id) else {
                return;
            };
            let unstage = force_unstage || chunk.status == ChunkStatus::Staged;
            let next_status = if unstage {
                ChunkStatus::Pending
            } else {
                ChunkStatus::Staged
            };
            let Some(patch) = build_chunk_patch(inner, [id], unstage) else {
                return;
            };
            (workdir, id, patch, next_status)
        };

        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&workdir) else {
            tracing::warn!("GitStageHunk: no git repo at {}", workdir.display());
            return;
        };
        if let Err(GitApplyError::Backend { reason, .. }) = repo.apply_to_index(&patch) {
            tracing::warn!("GitStageHunk: apply_to_index failed: {reason}");
            return;
        }

        session.update(cx, |session, cx| {
            session.set_status(id, next_status, cx);
        });
    }

    /// Toggle the staged state of a single line of the chunk under the
    /// review cursor: add (or remove) the chunk's first changed row in its
    /// staged-row set and rebuild the index state -- reverse the old
    /// subset, apply the new -- so adjacent-line stages accumulate. Acts on
    /// the chunk's first changed row; precise per-line cursor targeting is
    /// a follow-up. `WorkingTree` sources only.
    fn dispatch_git_stage_line(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();

        // Buffer row of the active file's editor cursor, used to pick which
        // line within the chunk to toggle.
        let file_index = {
            let inner = session.read(cx).inner();
            inner
                .cursor
                .current
                .and_then(|id| inner.chunks.get(&id))
                .map(|chunk| chunk.file_index)
        };
        let cursor_row = file_index
            .and_then(|fi| {
                review_item
                    .read(cx)
                    .files()
                    .get(fi)
                    .map(|f| f.editor.clone())
            })
            .map(|editor| editor.read(cx).primary_cursor_buffer_row(cx));

        let (workdir, id, plan) = {
            let inner = session.read(cx).inner();
            let workdir = match &inner.source {
                ReviewSource::WorkingTree { workdir } => workdir.clone(),
                _ => {
                    tracing::warn!(
                        "GitToggleStageLine: only WorkingTree sources stage to the index"
                    );
                    return;
                },
            };
            let Some(id) = inner.cursor.current else {
                return;
            };
            let Some(chunk) = inner.chunks.get(&id) else {
                return;
            };
            // Stage the changed row under the editor cursor; fall back to
            // the first changed row when the cursor is not on one.
            let row = cursor_row
                .and_then(|cr| {
                    chunk.hunk.rows.iter().position(|r| {
                        matches!(
                            r,
                            ReviewRow::Changed { right: Some(side), .. }
                                if side.line_num.saturating_sub(1) == cr
                        )
                    })
                })
                .or_else(|| {
                    chunk
                        .hunk
                        .rows
                        .iter()
                        .position(|r| matches!(r, ReviewRow::Changed { .. }))
                });
            let Some(row) = row else {
                return;
            };
            let Some(plan) = inner.plan_line_stage(id, row as u32) else {
                return;
            };
            (workdir, id, plan)
        };

        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&workdir) else {
            tracing::warn!("GitToggleStageLine: no git repo at {}", workdir.display());
            return;
        };
        for patch in [plan.reverse.as_ref(), plan.forward.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Err(GitApplyError::Backend { reason, .. }) = repo.apply_to_index(patch) {
                tracing::warn!("GitToggleStageLine: apply_to_index failed: {reason}");
                return;
            }
        }

        session.update(cx, |session, cx| {
            session.set_chunk_staged_rows(id, plan.rows, plan.status, cx);
        });
    }

    /// Apply the reversed patch of the chunk under the review cursor to
    /// the working tree, undoing that change on disk. Works for any
    /// workdir-bearing source; does not change chunk status.
    fn dispatch_review_revert_hunk(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();

        let (workdir, patch) = {
            let inner = session.read(cx).inner();
            let workdir = match &inner.source {
                ReviewSource::WorkingTree { workdir }
                | ReviewSource::WorkspaceWatch { workdir }
                | ReviewSource::Commit { workdir, .. }
                | ReviewSource::CommitRange { workdir, .. } => workdir.clone(),
                _ => {
                    tracing::warn!(
                        "ReviewRevertHunk: source has no working tree to revert against"
                    );
                    return;
                },
            };
            let Some(id) = inner.cursor.current else {
                return;
            };
            let Some(patch) = build_chunk_patch(inner, [id], true) else {
                return;
            };
            (workdir, patch)
        };

        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&workdir) else {
            tracing::warn!("ReviewRevertHunk: no git repo at {}", workdir.display());
            return;
        };
        if let Err(GitApplyError::Backend { reason, .. }) = repo.apply_to_workdir(&patch) {
            tracing::warn!("ReviewRevertHunk: apply_to_workdir failed: {reason}");
        }
    }

    fn dispatch_review_apply_staged(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();
        let Some((workdir, patches)) = ({
            let inner = session.read(cx).inner();
            let workdir = match &inner.source {
                ReviewSource::WorkingTree { workdir } => workdir.clone(),
                _ => {
                    tracing::warn!(
                        "ReviewApplyStaged: only WorkingTree sources are applyable; \
                         other sources are read-only reviews"
                    );
                    return;
                },
            };
            let patches: Vec<String> = inner
                .order
                .iter()
                .filter_map(|id| {
                    let chunk = inner.chunks.get(id)?;
                    if chunk.status != ChunkStatus::Staged {
                        return None;
                    }
                    build_chunk_patch(inner, [*id], false)
                })
                .collect();
            Some((workdir, patches))
        }) else {
            return;
        };

        if patches.is_empty() {
            tracing::info!("ReviewApplyStaged: nothing staged");
            return;
        }

        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&workdir) else {
            tracing::warn!("ReviewApplyStaged: no git repo at {}", workdir.display());
            return;
        };

        let total = patches.len();
        let mut applied = 0usize;
        let mut first_failure: Option<String> = None;
        for patch in &patches {
            match repo.apply_to_index(patch) {
                Ok(()) => applied += 1,
                Err(GitApplyError::Backend { reason, .. }) => {
                    if first_failure.is_none() {
                        first_failure = Some(reason);
                    }
                },
            }
        }

        session.update(cx, |session, cx| {
            session.set_apply_result(
                ReviewApplyResult {
                    applied,
                    total,
                    first_failure,
                },
                cx,
            );
        });

        self.show_toast(Toast::success(format!("Staged {applied} chunks")), cx);
    }

    fn dispatch_review_refresh(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();
        let source = session.read(cx).inner().source.clone();
        let inputs = review_inputs_for_source(&source, cx);
        session.update(cx, |session, cx| session.refresh_files(inputs, cx));
    }

    /// Cycle the active review to the next diff-comparison source
    /// ([`ReviewSource::next_comparison`]): WorkingTree -> unstaged-only ->
    /// staged-only -> the HEAD commit -> back. Re-extracts hunks from the
    /// new source and preserves review decisions across the swap. No-op
    /// when no review is focused or the current source is outside the
    /// cycle.
    fn dispatch_review_cycle_comparison_mode(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();
        let source = session.read(cx).inner().source.clone();

        let workdir = match &source {
            ReviewSource::WorkingTree { workdir }
            | ReviewSource::WorkingTreeUnstaged { workdir }
            | ReviewSource::WorkingTreeStaged { workdir }
            | ReviewSource::Commit { workdir, .. } => workdir.clone(),
            _ => return,
        };

        let head_sha = {
            let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
            git.discover(&workdir)
                .and_then(|repo| repo.log_commits(None, 1).first().map(|c| c.sha.clone()))
        };

        let Some(next) = source.next_comparison(head_sha.as_deref()) else {
            return;
        };

        let inputs = review_inputs_for_source(&next, cx);
        session.update(cx, |session, cx| session.cycle_source(next, inputs, cx));
    }

    /// Flip follow mode on the active review session. No-op when no
    /// review item is focused.
    fn dispatch_review_toggle_follow(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();
        session.update(cx, |session, cx| session.toggle_follow(cx));
    }

    /// Move the review cursor to the first chunk of the reviewed file
    /// at `path` and scroll its editor row into view. No-op when no
    /// review is active, `path` is not one of the reviewed files, or
    /// the file has no chunks.
    fn review_jump_to_file(&mut self, path: &Path, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        review_item.update(cx, |item, cx| {
            let session = item.session().clone();
            let target = {
                let inner = session.read(cx).inner();
                inner
                    .files
                    .iter()
                    .find(|f| f.path == path)
                    .and_then(|f| f.chunks.first().copied())
                    .and_then(|id| {
                        inner
                            .chunks
                            .get(&id)
                            .map(|c| (id, c.file_index, c.buffer_line_range.start))
                    })
            };
            let Some((chunk_id, file_index, buffer_row)) = target else {
                return;
            };
            session.update(cx, |s, cx| s.set_cursor_chunk(chunk_id, cx));
            let Some(file) = item.files().get(file_index) else {
                return;
            };
            let editor = file.editor.clone();
            editor.update(cx, |ed, cx| {
                ed.set_cursor_at_buffer_row(buffer_row, cx);
                ed.request_autoscroll(
                    crate::editor::scroll::autoscroll::AutoscrollStrategy::Center,
                    cx,
                );
            });
        });
    }

    fn dispatch_review_external_edit(&mut self, path: PathBuf, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let session = review_item.read(cx).session().clone();
        let Some((rel_path, language, base_text)) = ({
            let inner = session.read(cx).inner();
            inner
                .files
                .iter()
                .find(|f| f.path == path)
                .map(|f| (f.rel_path.clone(), f.language.clone(), f.base_text.clone()))
        }) else {
            return;
        };

        let fs = cx.global::<FsHostGlobal>().0.clone();
        let buffer_text = {
            let mut buf = Vec::new();
            match fs.read(&path, &mut buf) {
                Ok(()) => match String::from_utf8(buf) {
                    Ok(text) => text,
                    Err(err) => {
                        tracing::warn!(
                            ?path,
                            %err,
                            "ReviewExternalEdit: file is not valid UTF-8, skipping refresh",
                        );
                        return;
                    },
                },
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(err) => {
                    tracing::warn!(
                        ?path,
                        %err,
                        "ReviewExternalEdit: fs read failed, skipping refresh",
                    );
                    return;
                },
            }
        };

        let new_input = ReviewFileInput {
            path: path.clone(),
            rel_path,
            language,
            base_text,
            buffer_text: Arc::new(buffer_text),
        };
        session.update(cx, |session, cx| {
            session.refresh_file(&path, new_input, cx);
        });

        let follow = session.read(cx).inner().follow;
        if follow {
            self.review_jump_to_file(&path, cx);
        }
    }

    fn dispatch_jump_to_move_source(&mut self, nav: JumpMoveNav, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        // Anchor: `First` always re-anchors on the current cursor
        // chunk; `Next` / `Prev` continue cycling against the last
        // anchored chunk so navigating to a destination file doesn't
        // break the cycle. Fall back to the current cursor chunk
        // when no prior anchor exists.
        let anchor_chunk = match nav {
            JumpMoveNav::First => None,
            JumpMoveNav::Next | JumpMoveNav::Prev => {
                review_item.read(cx).move_cursor().map(|c| c.0)
            },
        }
        .or_else(|| {
            review_item
                .read(cx)
                .session()
                .read(cx)
                .inner()
                .cursor
                .current
        });
        let Some(anchor_chunk) = anchor_chunk else {
            return;
        };
        let sources = review_item
            .read(cx)
            .session()
            .read(cx)
            .inner()
            .move_sources_in_chunk(anchor_chunk);
        if sources.is_empty() {
            return;
        }
        let next_index = review_item.update(cx, |item, _| {
            let len = sources.len();
            let current = match item.move_cursor() {
                Some((cid, idx)) if cid == anchor_chunk => idx,
                _ => 0,
            };
            let next = match nav {
                JumpMoveNav::First => 0,
                JumpMoveNav::Next => (current + 1) % len.max(1),
                JumpMoveNav::Prev => (current + len.saturating_sub(1)) % len.max(1),
            };
            item.set_move_cursor(Some((anchor_chunk, next)));
            next
        });
        let Some(prov) = sources.get(next_index).cloned() else {
            return;
        };
        navigate_to_move_provenance(&review_item, &prov, cx);
    }

    fn dispatch_jump_to_move_target(&mut self, cx: &mut Context<'_, Self>) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let Some(chunk_id) = review_item
            .read(cx)
            .session()
            .read(cx)
            .inner()
            .cursor
            .current
        else {
            return;
        };
        let targets = review_item
            .read(cx)
            .session()
            .read(cx)
            .inner()
            .move_targets_in_chunk(chunk_id);
        let Some(prov) = targets.first().cloned() else {
            return;
        };
        navigate_to_move_provenance(&review_item, &prov, cx);
    }

    /// Navigate the active [`ReviewItem`] to the target side of
    /// `relationship`. Drives the same path as
    /// [`Self::dispatch_jump_to_move_target`]; called by the
    /// [`MoveRelationshipPickerDelegate`] on confirm.
    pub fn navigate_to_move_relationship(
        &mut self,
        relationship: &stoat::review_session::MoveRelationship,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        navigate_to_move_provenance(&review_item, &relationship.target, cx);
    }

    fn dispatch_query_move_relationships(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(review_item) = self.active_review_item(cx) else {
            return;
        };
        let relationships = review_item
            .read(cx)
            .session()
            .read(cx)
            .inner()
            .collect_move_relationships();
        if relationships.is_empty() {
            return;
        }
        let workspace = cx.weak_entity();
        self.toggle_modal::<crate::picker::Picker<
            crate::review_move_picker::MoveRelationshipPickerDelegate,
        >, _>(window, cx, move |window, cx| {
            let delegate =
                crate::review_move_picker::MoveRelationshipPickerDelegate::new(relationships, workspace);
            crate::picker::Picker::new(delegate, window, cx)
        });
    }

    /// Fetch `textDocument/references` for the active editor's
    /// cursor and open a picker over the response. No-op when no
    /// editor is active, the buffer has no path, no language is
    /// registered, or the server does not advertise the references
    /// capability.
    fn dispatch_goto_references(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let weak_editor = self.input_state_machine.read(cx).active_editor().cloned();
        let Some(editor) = weak_editor.and_then(|w| w.upgrade()) else {
            return;
        };
        let Some(path) = editor.read(cx).file_path().map(Path::to_path_buf) else {
            return;
        };
        let registry = &cx.global::<crate::globals::LanguageRegistry>().0;
        let Some(language) = registry.for_path(&path) else {
            return;
        };
        let host = cx.global::<crate::globals::LspHostGlobal>().0.clone();
        let mb_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
        let rope = mb_snapshot.rope().clone();
        let Some(primary) = editor.read(cx).selections().all_anchors().first().cloned() else {
            return;
        };
        let cursor_offset = mb_snapshot.resolve_anchor(&primary.head());
        let Some(uri) = path.to_str().and_then(|s| {
            <lsp_types::Uri as std::str::FromStr>::from_str(&format!("file://{s}")).ok()
        }) else {
            return;
        };
        let workspace_root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.clone());
        let weak_workspace = cx.weak_entity();
        cx.spawn_in(window, async move |_, cx| {
            let server = match host.launch(&language, &workspace_root).await {
                Ok(s) => Arc::<dyn stoat::host::LspServer>::from(s),
                Err(err) => {
                    tracing::warn!(
                        target: "stoat_gui::lsp::references",
                        ?err,
                        "failed to launch LSP server for references"
                    );
                    return;
                },
            };
            let _ = server.initialize(Some(uri.clone())).await;
            if !server.supports_feature(stoat::host::LanguageServerFeature::GotoReference) {
                return;
            }
            let encoding = server.offset_encoding();
            let position = stoat::lsp::util::byte_offset_to_lsp_pos(&rope, cursor_offset, encoding);
            let params = lsp_types::ReferenceParams {
                text_document_position: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: lsp_types::ReferenceContext {
                    include_declaration: true,
                },
            };
            let locations = match server.references(params).await {
                Ok(Some(locs)) if !locs.is_empty() => locs,
                Ok(_) => return,
                Err(err) => {
                    tracing::warn!(
                        target: "stoat_gui::lsp::references",
                        ?err,
                        "references request failed",
                    );
                    return;
                },
            };
            let _ = weak_workspace
                .clone()
                .update_in(cx, |workspace, window, cx| {
                    let weak_workspace_inner = cx.weak_entity();
                    workspace
                    .toggle_modal::<
                        crate::picker::Picker<crate::lsp::ReferencesPickerDelegate>,
                        _,
                    >(window, cx, move |window, cx| {
                        let delegate = crate::lsp::ReferencesPickerDelegate::new(
                            locations,
                            weak_workspace_inner,
                            encoding,
                        );
                        crate::picker::Picker::new(delegate, window, cx)
                    });
                });
        })
        .detach();
    }

    fn dispatch_open_review(&mut self, cx: &mut Context<'_, Self>) {
        let workdir = self.git_root.clone();
        let source = ReviewSource::WorkingTree { workdir };
        self.open_review_source(source, "OpenReview", cx);
    }

    /// Issue an LSP `textDocument/prepareRename` for the symbol under
    /// the cursor and, on a non-null response, open
    /// [`crate::lsp::rename::RenameModal`] seeded with the symbol's
    /// placeholder text. No-op when no editor is active, the buffer
    /// has no file path, no language is registered for that path, or
    /// the server does not advertise
    /// [`stoat::host::LanguageServerFeature::RenameSymbol`].
    fn dispatch_rename_symbol(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let weak_editor = self.input_state_machine.read(cx).active_editor().cloned();
        let Some(editor) = weak_editor.and_then(|w| w.upgrade()) else {
            return;
        };
        let Some(path) = editor.read(cx).file_path().map(Path::to_path_buf) else {
            return;
        };
        let registry = &cx.global::<crate::globals::LanguageRegistry>().0;
        let Some(language) = registry.for_path(&path) else {
            return;
        };
        let host = cx.global::<crate::globals::LspHostGlobal>().0.clone();
        let mb_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
        let rope = mb_snapshot.rope().clone();
        let Some(primary) = editor.read(cx).selections().all_anchors().first().cloned() else {
            return;
        };
        let cursor_offset = mb_snapshot.resolve_anchor(&primary.head());
        let Some(uri) = path.to_str().and_then(|s| {
            <lsp_types::Uri as std::str::FromStr>::from_str(&format!("file://{s}")).ok()
        }) else {
            return;
        };
        let workspace_root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.clone());
        let weak_editor = editor.downgrade();
        let weak_workspace = cx.weak_entity();
        cx.spawn_in(window, async move |_, cx| {
            let server = match host.launch(&language, &workspace_root).await {
                Ok(s) => Arc::<dyn stoat::host::LspServer>::from(s),
                Err(err) => {
                    tracing::warn!(
                        target: "stoat_gui::lsp::rename",
                        ?err,
                        "failed to launch LSP server for rename"
                    );
                    return;
                },
            };
            let _ = server.initialize(Some(uri.clone())).await;
            if !server.supports_feature(stoat::host::LanguageServerFeature::RenameSymbol) {
                return;
            }
            let encoding = server.offset_encoding();
            let position = stoat::lsp::util::byte_offset_to_lsp_pos(&rope, cursor_offset, encoding);
            let params = lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position,
            };
            let response = match server.prepare_rename(params).await {
                Ok(Some(resp)) => resp,
                Ok(None) => return,
                Err(err) => {
                    tracing::warn!(
                        target: "stoat_gui::lsp::rename",
                        ?err,
                        "prepare_rename request failed",
                    );
                    return;
                },
            };
            let placeholder =
                crate::lsp::rename::placeholder_from_prepare(response, &rope, encoding);
            let _ = weak_workspace
                .clone()
                .update_in(cx, |workspace, window, cx| {
                    let weak_workspace_inner = cx.weak_entity();
                    workspace.toggle_modal::<crate::lsp::rename::RenameModal, _>(
                        window,
                        cx,
                        move |window, cx| {
                            crate::lsp::rename::RenameModal::new(
                                &placeholder,
                                uri,
                                position,
                                encoding,
                                server,
                                weak_editor,
                                weak_workspace_inner,
                                rope,
                                window,
                                cx,
                            )
                        },
                    );
                });
        })
        .detach();
    }

    /// Open the LSP code action picker for the active editor's
    /// selection range. Spawns a fresh LSP server through
    /// `LspHostGlobal::launch`, fetches `textDocument/codeAction`,
    /// translates the response into picker entries, and shows the
    /// modal. No-op when no editor is active, the buffer has no
    /// file path, or no language is registered for that path.
    fn dispatch_format_selections(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let weak_editor = self.input_state_machine.read(cx).active_editor().cloned();
        let Some(editor) = weak_editor.and_then(|w| w.upgrade()) else {
            return;
        };
        let Some(path) = editor.read(cx).file_path().map(Path::to_path_buf) else {
            return;
        };
        let registry = &cx.global::<crate::globals::LanguageRegistry>().0;
        let Some(language) = registry.for_path(&path) else {
            return;
        };
        let host = cx.global::<crate::globals::LspHostGlobal>().0.clone();
        let mb_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
        let rope = mb_snapshot.rope().clone();
        let Some(primary) = editor.read(cx).selections().all_anchors().first().cloned() else {
            return;
        };
        let start_offset = mb_snapshot.resolve_anchor(&primary.start);
        let end_offset = mb_snapshot.resolve_anchor(&primary.end);
        let (lo, hi) = if start_offset <= end_offset {
            (start_offset, end_offset)
        } else {
            (end_offset, start_offset)
        };
        let Some(uri) = path.to_str().and_then(|s| {
            <lsp_types::Uri as std::str::FromStr>::from_str(&format!("file://{s}")).ok()
        }) else {
            return;
        };
        let workspace_root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.clone());
        let weak_editor = editor.downgrade();
        let weak_workspace = cx.weak_entity();
        cx.spawn_in(window, async move |_, cx| {
            let server = match host.launch(&language, &workspace_root).await {
                Ok(s) => Arc::<dyn stoat::host::LspServer>::from(s),
                Err(err) => {
                    tracing::warn!(
                        target: "stoat_gui::lsp::format",
                        ?err,
                        "failed to launch LSP server for format",
                    );
                    return;
                },
            };
            let _ = server.initialize(Some(uri.clone())).await;
            if !server.supports_feature(stoat::host::LanguageServerFeature::Format) {
                return;
            }
            let encoding = server.offset_encoding();
            let range = stoat::lsp::util::byte_range_to_lsp_range(&rope, lo..hi, encoding);
            let params = lsp_types::DocumentRangeFormattingParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                range,
                options: lsp_types::FormattingOptions::default(),
                work_done_progress_params: Default::default(),
            };
            let edits = match server.range_formatting(params).await {
                Ok(Some(edits)) if !edits.is_empty() => edits,
                Ok(_) => return,
                Err(err) => {
                    tracing::warn!(
                        target: "stoat_gui::lsp::format",
                        ?err,
                        "range_formatting request failed",
                    );
                    return;
                },
            };
            #[allow(clippy::mutable_key_type)]
            let mut changes: HashMap<lsp_types::Uri, Vec<lsp_types::TextEdit>> = HashMap::new();
            changes.insert(uri.clone(), edits);
            let edit = lsp_types::WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            };
            let _ = weak_workspace.clone().update_in(cx, |_, _, cx| {
                crate::lsp::edit_apply::apply_workspace_edit_to_buffer(
                    &edit,
                    &uri,
                    &rope,
                    encoding,
                    &weak_editor,
                    &weak_workspace,
                    cx,
                );
            });
        })
        .detach();
    }

    fn dispatch_code_action(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let weak_editor = self.input_state_machine.read(cx).active_editor().cloned();
        let Some(editor) = weak_editor.and_then(|w| w.upgrade()) else {
            return;
        };
        let Some(path) = editor.read(cx).file_path().map(Path::to_path_buf) else {
            return;
        };
        let registry = &cx.global::<crate::globals::LanguageRegistry>().0;
        let Some(language) = registry.for_path(&path) else {
            return;
        };
        let host = cx.global::<crate::globals::LspHostGlobal>().0.clone();
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let mb_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
        let rope = mb_snapshot.rope().clone();
        let Some(primary) = editor.read(cx).selections().all_anchors().first().cloned() else {
            return;
        };
        let start_offset = mb_snapshot.resolve_anchor(&primary.start);
        let end_offset = mb_snapshot.resolve_anchor(&primary.end);
        let (lo, hi) = if start_offset <= end_offset {
            (start_offset, end_offset)
        } else {
            (end_offset, start_offset)
        };
        let Some(uri) = path.to_str().and_then(|s| {
            <lsp_types::Uri as std::str::FromStr>::from_str(&format!("file://{s}")).ok()
        }) else {
            return;
        };
        let workspace_root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.clone());
        let weak_editor = editor.downgrade();
        let weak_workspace = cx.weak_entity();
        cx.spawn_in(window, async move |_, cx| {
            let server = match host.launch(&language, &workspace_root).await {
                Ok(s) => Arc::<dyn stoat::host::LspServer>::from(s),
                Err(err) => {
                    tracing::warn!(
                        target: "stoat_gui::lsp::code_action",
                        ?err,
                        "failed to launch LSP server for code action"
                    );
                    return;
                },
            };
            let _ = server.initialize(Some(uri.clone())).await;
            if !server.supports_feature(stoat::host::LanguageServerFeature::CodeAction) {
                return;
            }
            let encoding = server.offset_encoding();
            let range = stoat::lsp::util::byte_range_to_lsp_range(&rope, lo..hi, encoding);
            let params = lsp_types::CodeActionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                range,
                context: lsp_types::CodeActionContext {
                    diagnostics: Vec::new(),
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let response = match server.code_action(params).await {
                Ok(Some(r)) => r,
                Ok(None) | Err(_) => return,
            };
            let entries = crate::lsp::code_action::translate_actions(response);
            if entries.is_empty() {
                return;
            }
            let _ = weak_workspace
                .clone()
                .update_in(cx, |workspace, window, cx| {
                    let weak_workspace_inner = cx.weak_entity();
                    workspace.toggle_modal::<
                    crate::picker::Picker<crate::lsp::CodeActionPickerDelegate>,
                    _,
                >(window, cx, move |window, cx| {
                    let delegate = crate::lsp::CodeActionPickerDelegate::new(
                        entries,
                        weak_editor,
                        weak_workspace_inner,
                        uri,
                        rope,
                        encoding,
                        server,
                        executor,
                    );
                    crate::picker::Picker::new(delegate, window, cx)
                });
                });
        })
        .detach();
    }

    fn dispatch_open_review_commit(
        &mut self,
        workdir: PathBuf,
        sha: String,
        cx: &mut Context<'_, Self>,
    ) {
        let source = ReviewSource::Commit { workdir, sha };
        self.open_review_source(source, "OpenReviewCommit", cx);
    }

    fn dispatch_open_review_commit_range(
        &mut self,
        workdir: PathBuf,
        from: String,
        to: String,
        cx: &mut Context<'_, Self>,
    ) {
        let source = ReviewSource::CommitRange { workdir, from, to };
        self.open_review_source(source, "OpenReviewCommitRange", cx);
    }

    fn dispatch_open_review_agent_edits(
        &mut self,
        edits: Vec<stoat_action::AgentEdit>,
        cx: &mut Context<'_, Self>,
    ) {
        let proposals: Vec<stoat::review_session::AgentEditProposal> = edits
            .into_iter()
            .map(|e| stoat::review_session::AgentEditProposal {
                path: e.path,
                base_text: e.base_text,
                proposed_text: e.proposed_text,
            })
            .collect();
        let source = ReviewSource::AgentEdits {
            edits: Arc::new(proposals),
        };
        self.open_review_source(source, "OpenReviewAgentEdits", cx);
    }

    fn active_commit_list(&self, cx: &App) -> Option<Entity<crate::commit_list::CommitListItem>> {
        self.active_pane_item(cx).and_then(|item| {
            item.to_any_view()
                .downcast::<crate::commit_list::CommitListItem>()
                .ok()
        })
    }

    fn dispatch_open_commits(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        use crate::picker::PickerDelegate;
        let git = cx.global::<crate::globals::GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&self.git_root) else {
            tracing::warn!(action = "OpenCommits", "not inside a git repository");
            return;
        };
        let Some(workdir) = repo.workdir() else {
            tracing::warn!(action = "OpenCommits", "git repo has no workdir");
            return;
        };

        let state = cx.new(|_| {
            crate::commit_list::CommitListState::new(stoat::commit_list::CommitListState::new(
                workdir,
            ))
        });
        let buffer_registry = self.buffer_registry.clone();
        let item = cx
            .new(|cx| crate::commit_list::CommitListItem::new(state, buffer_registry, window, cx));
        self.open_item(Box::new(item.clone()), cx);

        let picker = item.read(cx).picker().clone();
        picker.update(cx, |p, picker_cx| {
            p.delegate_mut()
                .update_matches(String::new(), picker_cx)
                .detach();
        });
    }

    fn dispatch_commits_step(&mut self, step: CommitStep, cx: &mut Context<'_, Self>) {
        let Some(item) = self.active_commit_list(cx) else {
            return;
        };
        let picker = item.read(cx).picker().clone();
        let count = item.read(cx).state().read(cx).inner().commits.len();
        if count == 0 {
            return;
        }
        let selected = picker.read(cx).selected_index();
        let last = count - 1;
        let new_ix = match step {
            CommitStep::Down(n) => selected.saturating_add(n).min(last),
            CommitStep::Up(n) => selected.saturating_sub(n),
            CommitStep::PageDown => selected
                .saturating_add(crate::commit_list::COMMITS_PAGE_STEP)
                .min(last),
            CommitStep::PageUp => selected.saturating_sub(crate::commit_list::COMMITS_PAGE_STEP),
            CommitStep::First => 0,
            CommitStep::Last => last,
        };
        if new_ix == selected {
            return;
        }
        picker.update(cx, |p, picker_cx| p.set_selected_index(new_ix, picker_cx));
    }

    fn dispatch_commits_refresh(&mut self, cx: &mut Context<'_, Self>) {
        let Some(item) = self.active_commit_list(cx) else {
            return;
        };
        item.update(cx, |item, cx| item.refresh(cx));
    }

    fn dispatch_commits_open_review(&mut self, cx: &mut Context<'_, Self>) {
        let Some(item) = self.active_commit_list(cx) else {
            return;
        };
        let (workdir, sha) = {
            let state = item.read(cx).state().read(cx);
            let inner = state.inner();
            let Some(sha) = inner.selected_sha().map(String::from) else {
                return;
            };
            (inner.workdir.clone(), sha)
        };
        self.dispatch_open_review_commit(workdir, sha, cx);
    }

    fn dispatch_close_commits(&mut self, cx: &mut Context<'_, Self>) {
        let pane_id = self.pane_tree.read(cx).focus();
        let Some(pane) = self.pane_tree.read(cx).pane(pane_id).cloned() else {
            return;
        };
        let active_idx = pane.read(cx).active_index();
        let is_commit_list = pane
            .read(cx)
            .active_item()
            .map(|item| {
                item.to_any_view()
                    .downcast::<crate::commit_list::CommitListItem>()
                    .is_ok()
            })
            .unwrap_or(false);
        if !is_commit_list {
            return;
        }
        pane.update(cx, |p, cx| {
            p.remove_item(active_idx, cx);
        });
    }

    /// Build a [`ReviewItem`] for `source` and add it to the
    /// focused pane. `action_label` is a human-readable name
    /// used only for warn-level diagnostics when the scan
    /// returns no inputs or extracts no hunks. Shared by every
    /// `OpenReview*` dispatch arm.
    fn open_review_source(
        &mut self,
        source: ReviewSource,
        action_label: &'static str,
        cx: &mut Context<'_, Self>,
    ) {
        let inputs = review_inputs_for_source(&source, cx);
        if inputs.is_empty() {
            tracing::warn!(
                action = action_label,
                "no inputs returned for review source"
            );
            return;
        }

        let mut inner_session = stoat::review_session::ReviewSession::new(source);
        inner_session.add_files(inputs);
        if inner_session.order.is_empty() {
            tracing::warn!(action = action_label, "no diff hunks to display");
            return;
        }

        let session = cx.new(|_| crate::review_session::ReviewSession::new(inner_session));
        let buffer_registry = self.buffer_registry.clone();
        let review_item = cx
            .new(|cx| crate::review_item::ReviewItem::from_session(session, &buffer_registry, cx));

        self.open_item(Box::new(review_item), cx);
    }

    /// Add `item` as a new tab in the focused pane and activate it.
    /// Returns the index it was inserted at. Shared by every
    /// dispatch arm that opens a fresh ItemView (`OpenReview*`,
    /// `OpenCommits`, ...).
    fn open_item(&mut self, item: Box<dyn ItemHandle>, cx: &mut Context<'_, Self>) -> usize {
        let pane_id = self.pane_tree.read(cx).focus();
        let pane = self
            .pane_tree
            .read(cx)
            .pane(pane_id)
            .expect("pane tree returns its own focused pane id")
            .clone();
        pane.update(cx, |p, cx| {
            let index = p.add_item(item, cx);
            p.activate(index, cx);
            index
        })
    }
}

/// Spawn the background poll that drains
/// [`PermissionPromptHostGlobal`] into the workspace's modal queue.
/// Returns `None` when the global is not registered (most tests,
/// headless runs) so the workspace does not park a no-op task. The
/// task uses [`gpui::Context::spawn_in`] so its update hops land in
/// window context for [`crate::modal_layer::ModalLayer::show_modal`].
/// Spawn the periodic save loop that flushes the workspace's
/// state every [`PERIODIC_SAVE_INTERVAL`]. The loop exits when the
/// workspace's weak handle no longer upgrades (i.e. the workspace
/// has been dropped); the returned task should be stored on the
/// workspace so it dies with it.
///
/// Returns `None` when the [`ExecutorGlobal`] is absent, which
/// makes the save loop a silent no-op in headless test contexts
/// that have not installed an executor.
fn spawn_periodic_save(cx: &mut Context<'_, Workspace>) -> Option<Task<()>> {
    let executor = cx.try_global::<ExecutorGlobal>().map(|g| g.0.clone())?;
    let task = cx.spawn(async move |weak_workspace, cx| loop {
        executor.timer(PERIODIC_SAVE_INTERVAL).await;
        let live = weak_workspace.read_with(cx, |workspace, app| {
            workspace.save_state_to_default_path(app);
        });
        if live.is_err() {
            break;
        }
    });
    Some(task)
}

fn spawn_permission_prompt_poll(
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) -> Option<Task<()>> {
    let host = cx
        .try_global::<PermissionPromptHostGlobal>()
        .map(|g| g.0.clone())?;
    let executor = cx.try_global::<ExecutorGlobal>().map(|g| g.0.clone())?;
    let task = cx.spawn_in(window, async move |weak_workspace, cx| loop {
        executor.timer(PERMISSION_PROMPT_TICK).await;
        let pumped = weak_workspace.update_in(cx, |workspace, window, cx| {
            while let Some(prompt) = host.try_recv() {
                workspace.enqueue_permission_prompt(prompt, window, cx);
            }
        });
        if pumped.is_err() {
            break;
        }
    });
    Some(task)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum JumpMoveNav {
    First,
    Next,
    Prev,
}

/// Direction and magnitude for a commit-list cursor step. The
/// `Down` / `Up` variants carry the row delta; `PageDown` /
/// `PageUp` use [`crate::commit_list::COMMITS_PAGE_STEP`]; `First`
/// jumps to the newest commit, `Last` to the oldest loaded commit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum CommitStep {
    Down(usize),
    Up(usize),
    PageDown,
    PageUp,
    First,
    Last,
}

/// Result of a single executor-backed
/// [`stoat::host::GitRepo::run_rebase`] invocation, normalized so the
/// main-thread handler [`Workspace::apply_rebase_outcome`] does not
/// need to touch git itself. `Aborted` collapses every host-side
/// failure that should cancel the rebase (backend error, dirty
/// worktree, or a conflict whose per-file recovery itself failed)
/// into a single warn-and-clear path.
enum RebaseExecutionOutcome {
    Clean {
        new_head: String,
    },
    Conflict {
        at_sha: String,
        files: Vec<ConflictedFile>,
    },
    Aborted(String),
}

/// Run the rebase plan on the executor thread and synthesize a
/// [`RebaseExecutionOutcome`]. On `RebaseError::Conflict`, also call
/// `cherry_pick_tree` to recover the per-file conflict list so the
/// main-thread handler can install [`ConflictItem`] views without
/// reaching back into the host.
fn execute_rebase_plan(
    repo: &dyn GitRepo,
    onto: &str,
    todo: &[RebaseTodo],
) -> RebaseExecutionOutcome {
    match repo.run_rebase(onto, todo) {
        Ok(new_head) => RebaseExecutionOutcome::Clean { new_head },
        Err(RebaseError::Conflict { at_sha, .. }) => match repo.cherry_pick_tree(&at_sha, onto) {
            Ok(CherryPickOutcome::Conflict { files }) => {
                RebaseExecutionOutcome::Conflict { at_sha, files }
            },
            Ok(CherryPickOutcome::Clean { .. }) => RebaseExecutionOutcome::Aborted(format!(
                "run_rebase reported a conflict at {at_sha} but cherry_pick_tree is clean",
            )),
            Err(GitApplyError::Backend { reason, .. }) => RebaseExecutionOutcome::Aborted(format!(
                "cherry_pick_tree({at_sha}) failed: {reason}",
            )),
        },
        Err(RebaseError::Backend { reason, .. }) => {
            RebaseExecutionOutcome::Aborted(format!("run_rebase failed: {reason}"))
        },
        Err(RebaseError::DirtyWorktree { .. }) => {
            RebaseExecutionOutcome::Aborted("run_rebase requires a clean worktree".into())
        },
    }
}

/// Per-entry result from the stepper-driven execute path. Variants
/// model the outcome of one `cherry_pick_tree` + `create_commit`
/// pair: `Step` for Pick / Squash / Fixup, `Drop` for entries that
/// skip without touching git, `Reword` / `Edit` for ops that need
/// to pause for user input, `Conflict` mid-cherry-pick, and
/// `Aborted` for backend errors that should cancel the rebase. The
/// main-thread handler [`Workspace::apply_step_outcome`] maps each
/// variant to the corresponding state mutation and UI install.
enum RebaseStepOutcome {
    Step {
        new_head: String,
        new_message: String,
        op: RebaseTodoOp,
    },
    Drop,
    Reword {
        cherry_picked_commit: String,
        original_message: String,
    },
    Edit {
        cherry_picked_commit: String,
    },
    Conflict {
        at_sha: String,
        files: Vec<ConflictedFile>,
    },
    Aborted(String),
}

/// Process a single rebase entry on the executor thread. Mirrors
/// the per-op arms of the TUI stepper
/// (`stoat/src/action_handlers/rebase.rs::drive_rebase`):
/// cherry-pick onto `current_head` for Pick/Reword/Edit, onto
/// `last_pick` for Squash/Fixup, skip for Drop. Returns a
/// [`RebaseStepOutcome`] for the main thread to apply.
fn execute_rebase_step(
    repo: &dyn GitRepo,
    entry: &RebaseEntry,
    current_head: &str,
    last_pick: Option<&str>,
    last_message: Option<&str>,
) -> RebaseStepOutcome {
    match entry.op {
        RebaseTodoOp::Drop => RebaseStepOutcome::Drop,
        RebaseTodoOp::Pick | RebaseTodoOp::Reword | RebaseTodoOp::Edit => {
            match repo.cherry_pick_tree(&entry.commit.sha, current_head) {
                Ok(CherryPickOutcome::Clean {
                    tree,
                    message,
                    author_name,
                    author_email,
                    ..
                }) => match repo.create_commit(
                    Some(current_head),
                    &tree,
                    &message,
                    &author_name,
                    &author_email,
                ) {
                    Ok(new_sha) => match entry.op {
                        RebaseTodoOp::Pick => RebaseStepOutcome::Step {
                            new_head: new_sha,
                            new_message: message,
                            op: RebaseTodoOp::Pick,
                        },
                        RebaseTodoOp::Reword => RebaseStepOutcome::Reword {
                            cherry_picked_commit: new_sha,
                            original_message: message,
                        },
                        RebaseTodoOp::Edit => RebaseStepOutcome::Edit {
                            cherry_picked_commit: new_sha,
                        },
                        _ => unreachable!(),
                    },
                    Err(GitApplyError::Backend { reason, .. }) => {
                        RebaseStepOutcome::Aborted(format!("create_commit failed: {reason}"))
                    },
                },
                Ok(CherryPickOutcome::Conflict { files }) => RebaseStepOutcome::Conflict {
                    at_sha: entry.commit.sha.clone(),
                    files,
                },
                Err(GitApplyError::Backend { reason, .. }) => {
                    RebaseStepOutcome::Aborted(format!("cherry-pick failed: {reason}"))
                },
            }
        },
        RebaseTodoOp::Squash | RebaseTodoOp::Fixup => {
            let Some(last_pick) = last_pick else {
                return RebaseStepOutcome::Aborted("squash/fixup without preceding pick".into());
            };
            let last_message = last_message.unwrap_or("");
            match repo.cherry_pick_tree(&entry.commit.sha, last_pick) {
                Ok(CherryPickOutcome::Clean {
                    tree,
                    message: source_msg,
                    author_name,
                    author_email,
                    ..
                }) => {
                    let prev_parent = repo.parent_sha(last_pick);
                    let combined = match entry.op {
                        RebaseTodoOp::Squash => {
                            format!("{}\n\n{}", last_message.trim_end(), source_msg.trim_end(),)
                        },
                        _ => last_message.to_string(),
                    };
                    match repo.create_commit(
                        prev_parent.as_deref(),
                        &tree,
                        &combined,
                        &author_name,
                        &author_email,
                    ) {
                        Ok(new_sha) => RebaseStepOutcome::Step {
                            new_head: new_sha,
                            new_message: combined,
                            op: entry.op,
                        },
                        Err(GitApplyError::Backend { reason, .. }) => RebaseStepOutcome::Aborted(
                            format!("squash/fixup commit failed: {reason}"),
                        ),
                    }
                },
                Ok(CherryPickOutcome::Conflict { files }) => RebaseStepOutcome::Conflict {
                    at_sha: entry.commit.sha.clone(),
                    files,
                },
                Err(GitApplyError::Backend { reason, .. }) => RebaseStepOutcome::Aborted(format!(
                    "squash/fixup cherry-pick failed: {reason}",
                )),
            }
        },
    }
}

/// Switch the active file's editor and editor cursor to point at
/// `prov`. The session's chunk cursor parks on the chunk in the
/// destination file containing the provenance line (falling back
/// to the file's first chunk), so subsequent navigation knows the
/// review focus has moved across files. Silent when the
/// provenance's `rel_path` is not in the session.
fn navigate_to_move_provenance(
    review_item: &Entity<crate::review_item::ReviewItem>,
    prov: &stoat::review::MoveProvenance,
    cx: &mut Context<'_, Workspace>,
) {
    let session = review_item.read(cx).session().clone();
    let Some(file_index) = session
        .read(cx)
        .inner()
        .files
        .iter()
        .position(|f| f.rel_path == prov.rel_path)
    else {
        tracing::warn!(
            target = %prov.rel_path,
            "JumpToMove*: target file not in review session, skipping",
        );
        return;
    };
    let new_chunk = session
        .read(cx)
        .inner()
        .chunk_for_buffer_line(file_index, prov.line);
    if let Some(new_chunk) = new_chunk {
        session.update(cx, |s, cx| s.set_cursor_chunk(new_chunk, cx));
    }

    let editor = review_item
        .read(cx)
        .files()
        .get(file_index)
        .map(|f| f.editor.clone());
    let Some(editor) = editor else { return };
    editor.update(cx, |ed, cx| {
        ed.set_cursor_at_buffer_row(prov.line, cx);
        ed.request_autoscroll(
            crate::editor::scroll::autoscroll::AutoscrollStrategy::Center,
            cx,
        );
    });
}

/// Build a fresh [`ReviewFileInput`] list from `source` using the
/// hosts and language registry on the app globals. Used by
/// [`Workspace::dispatch_review_refresh`] to re-extract hunks
/// against the same source the session was opened against.
///
/// Returns an empty `Vec` when the source has no usable data:
/// `InMemory` / `AgentEdits` with empty stored data, working-tree
/// without a discoverable repo, or commits that the git host
/// cannot read. Empty input is the signal `ReviewSession::refresh_files`
/// uses to clear all chunks; callers do not need to short-circuit.
fn review_inputs_for_source(source: &ReviewSource, cx: &App) -> Vec<ReviewFileInput> {
    use crate::globals::{FsHostGlobal, GitHostGlobal, LanguageRegistry};
    let langs = &cx.global::<LanguageRegistry>().0;
    match source {
        ReviewSource::InMemory { files } => files
            .iter()
            .map(|file| ReviewFileInput {
                path: file.path.clone(),
                rel_path: file.path.display().to_string(),
                language: langs.for_path(&file.path),
                base_text: file.base_text.clone(),
                buffer_text: file.buffer_text.clone(),
            })
            .collect(),
        ReviewSource::AgentEdits { edits } => edits
            .iter()
            .map(|edit| ReviewFileInput {
                path: edit.path.clone(),
                rel_path: edit.path.display().to_string(),
                language: langs.for_path(&edit.path),
                base_text: edit.base_text.clone(),
                buffer_text: edit.proposed_text.clone(),
            })
            .collect(),
        ReviewSource::WorkingTree { workdir }
        | ReviewSource::WorkingTreeUnstaged { workdir }
        | ReviewSource::WorkingTreeStaged { workdir } => {
            let staged_filter = match source {
                ReviewSource::WorkingTreeUnstaged { .. } => Some(false),
                ReviewSource::WorkingTreeStaged { .. } => Some(true),
                _ => None,
            };
            let git = cx.global::<GitHostGlobal>().0.clone();
            let fs = cx.global::<FsHostGlobal>().0.clone();
            stoat::diff::scan_working_tree(&*git, &*fs, langs, workdir, None, staged_filter)
                .map(|(_, inputs)| inputs)
                .unwrap_or_default()
        },
        ReviewSource::Commit { workdir, sha } => {
            review_inputs_from_commit_trees(workdir, sha, None, langs, cx)
        },
        ReviewSource::CommitRange { workdir, from, to } => {
            review_inputs_from_commit_trees(workdir, to, Some(from.as_str()), langs, cx)
        },
        ReviewSource::WorkspaceWatch { .. } => Vec::new(),
    }
}

fn review_inputs_from_commit_trees(
    workdir: &Path,
    head_sha: &str,
    base_sha: Option<&str>,
    langs: &stoat_language::LanguageRegistry,
    cx: &App,
) -> Vec<ReviewFileInput> {
    use crate::globals::GitHostGlobal;
    let git = cx.global::<GitHostGlobal>().0.clone();
    let Some(repo) = git.discover(workdir) else {
        return Vec::new();
    };
    let Some(workdir) = repo.workdir() else {
        return Vec::new();
    };
    let Some(new_tree) = repo.commit_tree(head_sha) else {
        return Vec::new();
    };
    let base_tree = match base_sha {
        Some(sha) => repo.commit_tree(sha).unwrap_or_default(),
        None => match repo.parent_sha(head_sha) {
            Some(parent) => repo.commit_tree(&parent).unwrap_or_default(),
            None => std::collections::BTreeMap::new(),
        },
    };
    let mut paths: std::collections::BTreeSet<&Path> = std::collections::BTreeSet::new();
    for p in base_tree.keys() {
        paths.insert(p.as_path());
    }
    for p in new_tree.keys() {
        paths.insert(p.as_path());
    }
    let mut inputs: Vec<ReviewFileInput> = Vec::new();
    for rel in paths {
        let base = base_tree.get(rel).cloned().unwrap_or_default();
        let buffer = new_tree.get(rel).cloned().unwrap_or_default();
        if base == buffer {
            continue;
        }
        let abs = workdir.join(rel);
        let lang = langs.for_path(&abs);
        inputs.push(ReviewFileInput {
            path: abs,
            rel_path: rel.display().to_string(),
            language: lang,
            base_text: Arc::new(base),
            buffer_text: Arc::new(buffer),
        });
    }
    inputs
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ReviewStepDir {
    Next,
    Prev,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SearchStep {
    Next,
    Prev,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ConflictNavDir {
    Next,
    Prev,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ReviewStatusChange {
    Stage,
    Unstage,
    Skip,
    Toggle,
}

fn absolute_path(path: &Path, cwd: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match cwd {
        Some(cwd) => cwd.join(path),
        None => path.to_path_buf(),
    }
}

fn read_path_or_empty(path: &Path, cx: &App) -> String {
    let fs = cx.global::<FsHostGlobal>().0.clone();
    let mut buf = Vec::new();
    match fs.read(path, &mut buf) {
        Ok(()) => match String::from_utf8(buf) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(?path, %err, "open_paths: file is not valid UTF-8, opening empty");
                String::new()
            },
        },
        Err(err) => {
            tracing::warn!(?path, %err, "open_paths: read failed, opening empty buffer");
            String::new()
        },
    }
}

#[derive(Copy, Clone, Debug)]
enum JumpDir {
    Backward,
    Forward,
}

#[derive(Copy, Clone, Debug)]
enum GotoKind {
    LineStart,
    LineEnd,
    FirstNonwhitespace,
    FileStart,
    LastLine,
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if self._permission_prompt_poll.is_none() {
            self._permission_prompt_poll = spawn_permission_prompt_poll(window, cx);
        }
        if self._periodic_save.is_none() {
            self._periodic_save = spawn_periodic_save(cx);
        }
        let title = self.compute_window_title(cx);
        if self.last_window_title.as_ref() != Some(&title) {
            window.set_window_title(&title);
            self.last_window_title = Some(title);
        }

        let left_docks: Vec<Entity<Dock>> = if self.left_dock_visible {
            self.docks
                .iter()
                .filter(|d| d.read(cx).side() == DockSide::Left)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let right_docks: Vec<Entity<Dock>> = if self.right_dock_visible {
            self.docks
                .iter()
                .filter(|d| d.read(cx).side() == DockSide::Right)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let bottom_docks: Vec<Entity<Dock>> = if self.bottom_dock_visible {
            self.docks
                .iter()
                .filter(|d| d.read(cx).side() == DockSide::Bottom)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let (ui_family, ui_size) = ui_font(cx);
        let body = div()
            .flex()
            .flex_row()
            .size_full()
            .bg(cx.theme().background)
            .font_family(ui_family)
            .text_size(px(ui_size))
            .track_focus(&self.focus_handle)
            .children(left_docks)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .child(
                        div()
                            .flex_1()
                            .child(self.pane_tree.clone())
                            .child(deferred(self.toast_view.clone())),
                    )
                    .children(bottom_docks)
                    .child(self.status_bar.clone()),
            )
            .children(right_docks)
            .child(self.key_hint_banner.clone())
            .child(deferred(self.modal_layer.clone()));
        if render_stats_enabled(cx) {
            body.child(deferred(
                RenderStatsOverlay::new(self.frame_timer.clone()).element(),
            ))
        } else {
            body
        }
    }
}

fn ui_font(cx: &App) -> (SharedString, f32) {
    let (family, size) = match cx.try_global::<Settings>() {
        Some(settings) => (
            settings.resolved.ui_font_family.clone(),
            settings.resolved.ui_font_size,
        ),
        None => (None, None),
    };
    (
        family
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from(DEFAULT_UI_FONT_FAMILY)),
        size.unwrap_or(DEFAULT_UI_FONT_SIZE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{DeserializeSnafu, ItemError, ItemView};
    use gpui::{
        div, px, size, Bounds, DismissEvent, Focusable, IntoElement, Point, Render, Styled,
        Subscription, TestAppContext, VisualContext, VisualTestContext, Window,
    };
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use stoat::keymap::Keymap;

    struct WorkspaceItem {
        label: SharedString,
        kind: crate::item::ItemKind,
    }

    impl Render for WorkspaceItem {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl ItemView for WorkspaceItem {
        fn tab_label(&self, _cx: &App) -> SharedString {
            self.label.clone()
        }

        fn item_kind(&self) -> crate::item::ItemKind {
            self.kind
        }

        fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
        where
            Self: Sized,
        {
            DeserializeSnafu {
                reason: "WorkspaceItem is test-only",
            }
            .fail()
        }
    }

    struct Recorder {
        _subscription: Subscription,
    }

    fn install_recorder(
        cx: &mut TestAppContext,
        ws: &Entity<Workspace>,
    ) -> (Entity<Recorder>, Arc<Mutex<Vec<WorkspaceEvent>>>) {
        let events: Arc<Mutex<Vec<WorkspaceEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let ws = ws.clone();
        let recorder = cx.update(|cx| {
            let sink = events.clone();
            cx.new(|cx| {
                let subscription = cx.subscribe(&ws, move |_, _, event: &WorkspaceEvent, _| {
                    sink.lock().expect("recorder mutex").push(event.clone());
                });
                Recorder {
                    _subscription: subscription,
                }
            })
        });
        (recorder, events)
    }

    fn drain(events: &Arc<Mutex<Vec<WorkspaceEvent>>>) -> Vec<WorkspaceEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn install_workspace_test_globals(cx: &mut TestAppContext) {
        use crate::globals::{ExecutorGlobal, FsWatchHostGlobal};
        use stoat::host::FsWatchHost;
        use stoat_host::NoopFsWatcher;
        use stoat_scheduler::{Executor, TestScheduler};
        cx.update(|cx| {
            if !cx.has_global::<ExecutorGlobal>() {
                cx.set_global(ExecutorGlobal(Executor::new(
                    Arc::new(TestScheduler::new()),
                )));
            }
            if !cx.has_global::<FsWatchHostGlobal>() {
                cx.set_global(FsWatchHostGlobal(
                    Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
                ));
            }
        });
    }

    fn new_workspace(cx: &mut TestAppContext, name: &str, root: &str) -> Entity<Workspace> {
        install_workspace_test_globals(cx);
        let name = name.to_string();
        let root = PathBuf::from(root);
        cx.update(|cx| cx.new(|cx| Workspace::new(name, root, cx)))
    }

    fn new_workspace_in_window<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        root: &str,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext) {
        install_workspace_test_globals(cx);
        let name = name.to_string();
        let root = PathBuf::from(root);
        cx.add_window_view(|_window, cx| Workspace::new(name, root, cx))
    }

    struct TestModal {
        focus_handle: FocusHandle,
        veto_dismiss: bool,
    }

    impl TestModal {
        fn new(cx: &mut Context<'_, Self>) -> Self {
            Self {
                focus_handle: cx.focus_handle(),
                veto_dismiss: false,
            }
        }

        fn vetoing(cx: &mut Context<'_, Self>) -> Self {
            Self {
                focus_handle: cx.focus_handle(),
                veto_dismiss: true,
            }
        }
    }

    impl Render for TestModal {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl Focusable for TestModal {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl EventEmitter<DismissEvent> for TestModal {}

    impl ModalView for TestModal {
        fn on_before_dismiss(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> bool {
            !self.veto_dismiss
        }
    }

    fn new_item(cx: &mut TestAppContext, label: &str) -> Box<dyn ItemHandle> {
        let label = SharedString::from(label.to_string());
        let kind = crate::item::ItemKind::Unknown;
        let entity = cx.update(|cx| cx.new(|_| WorkspaceItem { label, kind }));
        Box::new(entity)
    }

    #[test]
    fn fresh_workspace_exposes_input_state_machine_with_defaults() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let sm = ws.read_with(&cx, |w, _| w.input_state_machine().clone());
        sm.read_with(&cx, |sm, _| {
            assert_eq!(sm.mode(), "normal");
            assert!(!sm.palette_open());
            assert!(!sm.finder_open());
            assert!(!sm.help_open());
            assert!(!sm.claude_focused());
            assert_eq!(sm.pending_count(), None);
        });
    }

    #[test]
    fn show_toast_adds_to_overlay_and_dismiss_removes_it() {
        use crate::toast::Toast;

        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");

        let id = ws.update(&mut cx, |w, cx| {
            let toast = Toast::error("boom");
            let id = toast.id;
            w.show_toast(toast, cx);
            id
        });
        ws.read_with(&cx, |w, cx| {
            assert_eq!(w.toast_view.read(cx).toasts().len(), 1);
        });

        ws.update(&mut cx, |w, cx| w.dismiss_toast(id, cx));
        ws.read_with(&cx, |w, cx| {
            assert!(w.toast_view.read(cx).toasts().is_empty());
        });
    }

    #[test]
    fn apply_encoding_redecodes_active_buffer_from_disk() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/a.txt", [0x93u8, 0xFA, 0x96, 0x7B]);
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        let editor = ws.update(vcx, |w, cx| {
            w.build_editor_for_path(Path::new("/tmp/repo/a.txt"), cx)
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        ws.update(vcx, |w, cx| {
            w.apply_encoding_to_active_buffer(Encoding::ShiftJis, cx)
        });
        vcx.run_until_parked();

        let (text, encoding) = editor.read_with(vcx, |ed, cx| {
            let buffer = ed
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("singleton");
            (buffer.read(cx).text(), buffer.read(cx).encoding())
        });
        assert_eq!(text, "日本");
        assert_eq!(encoding, Encoding::ShiftJis);
    }

    #[test]
    fn apply_encoding_warns_when_decode_is_lossy() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/b.txt", [0xFFu8, 0xFE]);
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        let editor = ws.update(vcx, |w, cx| {
            w.build_editor_for_path(Path::new("/tmp/repo/b.txt"), cx)
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        ws.update(vcx, |w, cx| {
            w.apply_encoding_to_active_buffer(Encoding::Utf8, cx)
        });
        vcx.run_until_parked();

        ws.read_with(vcx, |w, cx| {
            assert_eq!(w.toast_view.read(cx).toasts().len(), 1);
        });
    }

    #[test]
    fn editor_input_getter_returns_workspace_editor_input_entity() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let (via_getter, via_field) = ws.read_with(&cx, |w, _| {
            (w.editor_input().clone(), w.editor_input.clone())
        });
        assert_eq!(via_getter.entity_id(), via_field.entity_id());
    }

    #[test]
    fn fresh_workspace_has_default_keymap() {
        use stoat::keymap::{KeymapState, StateValue};

        struct NormalState;
        impl KeymapState for NormalState {
            fn get(&self, field: &str) -> Option<&StateValue> {
                static MODE: std::sync::OnceLock<StateValue> = std::sync::OnceLock::new();
                if field == "mode" {
                    Some(MODE.get_or_init(|| StateValue::String("normal".into())))
                } else {
                    None
                }
            }
        }

        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let sm = ws.read_with(&cx, |w, _| w.input_state_machine().clone());
        let count = sm.read_with(&cx, |sm, _| sm.keymap().active_bindings(&NormalState).len());
        assert!(
            count > 0,
            "fresh workspace should have the default keymap installed"
        );
    }

    #[test]
    fn settings_change_swaps_keymap() {
        use std::collections::HashMap;
        use stoat::keymap::{KeymapState, StateValue};

        struct NormalState {
            values: HashMap<String, StateValue>,
        }

        impl KeymapState for NormalState {
            fn get(&self, field: &str) -> Option<&StateValue> {
                self.values.get(field)
            }
        }

        fn normal() -> NormalState {
            let mut values = HashMap::new();
            values.insert("mode".into(), StateValue::String("normal".into()));
            NormalState { values }
        }

        let mut cx = TestAppContext::single();
        cx.update(|cx| {
            cx.set_global(Settings::load_from_source("on key { x -> Quit(); }"));
        });
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let sm = ws.read_with(&cx, |w, _| w.input_state_machine().clone());

        let before = sm.read_with(&cx, |sm, _| {
            sm.keymap()
                .active_bindings(&normal())
                .iter()
                .map(|(label, _)| label.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(before, vec!["x".to_string()]);

        cx.update(|cx| {
            cx.set_global(Settings::load_from_source("on key { y -> Quit(); }"));
        });
        cx.run_until_parked();

        let after = sm.read_with(&cx, |sm, _| {
            sm.keymap()
                .active_bindings(&normal())
                .iter()
                .map(|(label, _)| label.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(after, vec!["y".to_string()]);
    }

    #[test]
    fn fresh_workspace_exposes_pane_tree_and_no_docks() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");

        ws.read_with(&cx, |w, _| {
            assert_eq!(w.name(), &SharedString::from("main"));
            assert_eq!(w.git_root(), &PathBuf::from("/tmp/repo"));
            assert!(w.docks().is_empty());
        });
        let pane_tree = ws.read_with(&cx, |w, _| w.pane_tree().clone());
        let pane_count = pane_tree.read_with(&cx, |t, _| t.pane_count());
        assert_eq!(pane_count, 1);
    }

    #[test]
    fn set_name_emits_only_on_change() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let (_r, events) = install_recorder(&mut cx, &ws);

        let same = ws.update(&mut cx, |w, cx| w.set_name("main", cx));
        cx.run_until_parked();
        assert!(!same);
        assert_eq!(drain(&events), Vec::<WorkspaceEvent>::new());

        let changed = ws.update(&mut cx, |w, cx| w.set_name("renamed", cx));
        cx.run_until_parked();
        assert!(changed);
        assert_eq!(drain(&events), vec![WorkspaceEvent::NameChanged]);
        assert_eq!(
            ws.read_with(&cx, |w, _| w.name().clone()),
            SharedString::from("renamed")
        );
    }

    #[test]
    fn new_registers_default_status_items() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let status_bar = ws.read_with(&cx, |w, _| w.status_bar().clone());
        let (left, right) = status_bar.read_with(&cx, |bar, _| {
            (bar.left_items().len(), bar.right_items().len())
        });
        assert_eq!(left, 3);
        assert_eq!(right, 8);
    }

    #[test]
    fn set_name_propagates_to_workspace_label() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let label = ws.read_with(&cx, |w, _| w.workspace_label().clone());

        let initial = label.read_with(&cx, |l, _| l.name().clone());
        assert_eq!(initial, SharedString::from("main"));

        ws.update(&mut cx, |w, cx| w.set_name("renamed", cx));
        cx.run_until_parked();
        let updated = label.read_with(&cx, |l, _| l.name().clone());
        assert_eq!(updated, SharedString::from("renamed"));
    }

    #[test]
    fn add_dock_emits_and_grows_docks() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let (_r, events) = install_recorder(&mut cx, &ws);
        let item = new_item(&mut cx, "outline");

        let index = ws.update(&mut cx, |w, cx| w.add_dock(item, DockSide::Left, 200, cx));
        cx.run_until_parked();

        assert_eq!(index, 0);
        assert_eq!(drain(&events), vec![WorkspaceEvent::DockAdded { index: 0 }]);
        assert_eq!(ws.read_with(&cx, |w, _| w.docks().len()), 1);
    }

    #[test]
    fn remove_dock_out_of_range_returns_none() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let (_r, events) = install_recorder(&mut cx, &ws);

        let removed = ws.update(&mut cx, |w, cx| w.remove_dock(7, cx));
        cx.run_until_parked();

        assert!(removed.is_none());
        assert_eq!(drain(&events), Vec::<WorkspaceEvent>::new());
    }

    #[test]
    fn remove_dock_in_range_emits_and_shrinks() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let item = new_item(&mut cx, "outline");
        ws.update(&mut cx, |w, cx| {
            w.add_dock(item, DockSide::Right, 240, cx);
        });
        let (_r, events) = install_recorder(&mut cx, &ws);

        let removed = ws.update(&mut cx, |w, cx| w.remove_dock(0, cx));
        cx.run_until_parked();

        assert!(removed.is_some());
        assert_eq!(
            drain(&events),
            vec![WorkspaceEvent::DockRemoved { index: 0 }]
        );
        assert_eq!(ws.read_with(&cx, |w, _| w.docks().len()), 0);
    }

    #[test]
    fn workspace_toggle_modal_opens_modal() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update_in(vcx, |w, window, cx| {
            w.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::new(cx));
        });
        vcx.run_until_parked();

        let active = ws.read_with(vcx, |w, cx| {
            w.modal_layer().read(cx).active_modal::<TestModal>()
        });
        assert!(active.is_some());
    }

    #[test]
    fn workspace_dismiss_modal_closes_active_modal() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update_in(vcx, |w, window, cx| {
            w.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::new(cx));
        });
        vcx.run_until_parked();

        let dismissed = ws.update_in(vcx, |w, window, cx| w.dismiss_modal(window, cx));
        vcx.run_until_parked();

        assert!(dismissed);
        let active = ws.read_with(vcx, |w, cx| {
            w.modal_layer().read(cx).active_modal::<TestModal>()
        });
        assert!(active.is_none());
    }

    #[test]
    fn workspace_dismiss_modal_empty_returns_false() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        let dismissed = ws.update_in(vcx, |w, window, cx| w.dismiss_modal(window, cx));
        assert!(!dismissed);
    }

    #[test]
    fn workspace_dismiss_modal_respects_veto() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update_in(vcx, |w, window, cx| {
            w.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::vetoing(cx));
        });
        vcx.run_until_parked();

        let dismissed = ws.update_in(vcx, |w, window, cx| w.dismiss_modal(window, cx));
        assert!(!dismissed);
        let active = ws.read_with(vcx, |w, cx| {
            w.modal_layer().read(cx).active_modal::<TestModal>()
        });
        assert!(active.is_some());
    }

    #[test]
    fn opening_command_palette_sets_palette_open_and_prompt_mode() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::OpenCommandPalette);
        vcx.run_until_parked();

        let (palette_open, mode) = ws.read_with(vcx, |w, cx| {
            let sm = w.input_state_machine().read(cx);
            (sm.palette_open(), sm.mode().to_string())
        });
        assert!(
            palette_open,
            "palette_open should be true while palette is the active modal"
        );
        assert_eq!(
            mode, "prompt",
            "mode should be prompt while palette is active"
        );
    }

    #[test]
    fn closing_command_palette_clears_palette_open_and_restores_mode() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::OpenCommandPalette);
        vcx.run_until_parked();

        ws.update_in(vcx, |w, window, cx| {
            w.dismiss_modal(window, cx);
        });
        vcx.run_until_parked();

        let (palette_open, mode) = ws.read_with(vcx, |w, cx| {
            let sm = w.input_state_machine().read(cx);
            (sm.palette_open(), sm.mode().to_string())
        });
        assert!(
            !palette_open,
            "palette_open should clear after the palette closes"
        );
        assert_eq!(mode, "normal", "mode should restore to the prior value");
    }

    #[test]
    fn toggling_command_palette_off_clears_palette_open() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::OpenCommandPalette);
        vcx.run_until_parked();
        dispatch(&ws, vcx, stoat_action::OpenCommandPalette);
        vcx.run_until_parked();

        let (palette_open, mode) = ws.read_with(vcx, |w, cx| {
            let sm = w.input_state_machine().read(cx);
            (sm.palette_open(), sm.mode().to_string())
        });
        assert!(
            !palette_open,
            "second OpenCommandPalette toggles the modal off"
        );
        assert_eq!(mode, "normal");
    }

    #[test]
    fn keyboard_confirmed_palette_action_dispatches_without_reentrant_panic() {
        use crate::{command_palette::CommandPaletteDelegate, picker::Picker};

        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let panes_before = ws.read_with(vcx, |w, cx| w.pane_tree().read(cx).pane_count());

        dispatch(&ws, vcx, stoat_action::OpenCommandPalette);
        vcx.run_until_parked();

        let picker = ws.read_with(vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<CommandPaletteDelegate>>()
                .expect("command palette modal active")
        });
        let buffer = picker.read_with(vcx, |p, cx| {
            p.query_editor()
                .read(cx)
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("single-line query editor has singleton buffer")
                .clone()
        });
        buffer.update(vcx, |b, cx| {
            let len = b.text().len();
            b.edit(0..len, "SplitRight", cx);
        });
        vcx.run_until_parked();

        dispatch(&ws, vcx, stoat_action::SubmitPromptInput);
        vcx.run_until_parked();

        let panes_after = ws.read_with(vcx, |w, cx| w.pane_tree().read(cx).pane_count());
        assert_eq!(
            panes_after,
            panes_before + 1,
            "keyboard-confirmed palette action must dispatch after the keystroke lease releases",
        );
    }

    #[test]
    fn opening_help_sets_help_open_and_prompt_mode() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::OpenHelp);
        vcx.run_until_parked();

        let (help_open, mode) = ws.read_with(vcx, |w, cx| {
            let sm = w.input_state_machine().read(cx);
            (sm.help_open(), sm.mode().to_string())
        });
        assert!(
            help_open,
            "help_open should be true while help is the active modal"
        );
        assert_eq!(mode, "prompt", "mode should be prompt while help is active");
    }

    #[test]
    fn closing_help_clears_help_open_and_restores_mode() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::OpenHelp);
        vcx.run_until_parked();

        ws.update_in(vcx, |w, window, cx| {
            w.dismiss_modal(window, cx);
        });
        vcx.run_until_parked();

        let (help_open, mode) = ws.read_with(vcx, |w, cx| {
            let sm = w.input_state_machine().read(cx);
            (sm.help_open(), sm.mode().to_string())
        });
        assert!(
            !help_open,
            "help_open should clear after the help modal closes"
        );
        assert_eq!(mode, "normal", "mode should restore to the prior value");
    }

    fn new_workspace_with_finder_hosts(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        use crate::globals::{FsHostGlobal, GitHostGlobal};
        use stoat::host::{fake::FakeGit, FakeFs, FsHost, GitHost};

        let fs = Arc::new(FakeFs::new());
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo").with_fs(&fs);
        cx.update(|cx| {
            cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>));
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
        });
        new_workspace_in_window(cx, "main", "/repo")
    }

    #[test]
    fn opening_file_finder_sets_finder_open_and_resolves_navigation() {
        use gpui::{Keystroke, Modifiers};

        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_with_finder_hosts(&mut cx);

        dispatch(&ws, vcx, stoat_action::OpenFileFinder);
        vcx.run_until_parked();

        let (finder_open, mode) = ws.read_with(vcx, |w, cx| {
            let sm = w.input_state_machine().read(cx);
            (sm.finder_open(), sm.mode().to_string())
        });
        assert!(
            finder_open,
            "finder_open should be true while the finder is the active modal"
        );
        assert_eq!(
            mode, "prompt",
            "mode should be prompt while the finder is active"
        );

        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        let kinds = sm.update_in(vcx, |sm, window, cx| {
            let down = Keystroke {
                modifiers: Modifiers::default(),
                key: "down".into(),
                key_char: None,
            };
            sm.feed(&down, window, cx)
                .iter()
                .map(|a| a.kind())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            kinds,
            vec![ActionKind::FileFinderSelectNext],
            "Down must resolve to the finder navigation binding while finder_open"
        );
    }

    #[test]
    fn closing_file_finder_clears_finder_open_and_restores_mode() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_with_finder_hosts(&mut cx);

        dispatch(&ws, vcx, stoat_action::OpenFileFinder);
        vcx.run_until_parked();

        ws.update_in(vcx, |w, window, cx| {
            w.dismiss_modal(window, cx);
        });
        vcx.run_until_parked();

        let (finder_open, mode) = ws.read_with(vcx, |w, cx| {
            let sm = w.input_state_machine().read(cx);
            (sm.finder_open(), sm.mode().to_string())
        });
        assert!(
            !finder_open,
            "finder_open should clear after the finder modal closes"
        );
        assert_eq!(mode, "normal", "mode should restore to the prior value");
    }

    #[test]
    fn close_help_action_dismisses_help_modal() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::OpenHelp);
        vcx.run_until_parked();
        ws.update_in(vcx, |w, window, cx| {
            w.dispatch_action(Box::new(stoat_action::CloseHelp), window, cx);
        });
        vcx.run_until_parked();

        let active = ws.read_with(vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<crate::help::HelpModal>()
                .is_some()
        });
        assert!(!active, "CloseHelp must dismiss the help modal");
    }

    #[test]
    fn dispatch_picker_open_records_last_picker_action() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::OpenCommandPalette);
        vcx.run_until_parked();

        let recorded = ws.read_with(vcx, |w, cx| {
            w.input_state_machine().read(cx).last_picker_action()
        });
        assert_eq!(recorded, Some("OpenCommandPalette"));
    }

    #[test]
    fn dispatch_picker_open_skips_recording_when_no_modal_opens() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::OpenJumplistPicker);
        vcx.run_until_parked();

        let modal_active = ws.read_with(vcx, |w, cx| w.modal_layer().read(cx).has_active_modal());
        let recorded = ws.read_with(vcx, |w, cx| {
            w.input_state_machine().read(cx).last_picker_action()
        });
        assert!(
            !modal_active,
            "OpenJumplistPicker should no-op without an active editor"
        );
        assert_eq!(
            recorded, None,
            "recording must skip when the picker did not open a modal"
        );
    }

    #[test]
    fn dispatch_open_last_picker_with_no_history_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::OpenLastPicker);
        vcx.run_until_parked();

        let modal_active = ws.read_with(vcx, |w, cx| w.modal_layer().read(cx).has_active_modal());
        let recorded = ws.read_with(vcx, |w, cx| {
            w.input_state_machine().read(cx).last_picker_action()
        });
        assert!(!modal_active);
        assert_eq!(recorded, None);
    }

    #[test]
    fn dispatch_open_last_picker_reopens_recorded_picker_after_dismiss() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::OpenCommandPalette);
        vcx.run_until_parked();
        ws.update_in(vcx, |w, window, cx| {
            w.dismiss_modal(window, cx);
        });
        vcx.run_until_parked();
        assert!(!ws.read_with(vcx, |w, cx| w.modal_layer().read(cx).has_active_modal()));

        dispatch(&ws, vcx, stoat_action::OpenLastPicker);
        vcx.run_until_parked();

        let palette_open =
            ws.read_with(vcx, |w, cx| w.input_state_machine().read(cx).palette_open());
        assert!(
            palette_open,
            "OpenLastPicker should re-dispatch the recorded OpenCommandPalette and reopen the palette"
        );
    }

    #[test]
    fn workspace_handle_quit_closes_focused_pane_when_multiple_exist() {
        use stoat::pane::Axis;
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(&cx, |w, _| w.pane_tree().clone());
        pane_tree.update(&mut cx, |t, cx| {
            t.split(Axis::Vertical, cx);
        });
        assert_eq!(pane_tree.read_with(&cx, |t, _| t.pane_count()), 2);

        ws.update(&mut cx, |w, cx| w.handle_quit(cx));
        cx.run_until_parked();

        assert_eq!(pane_tree.read_with(&cx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn workspace_handle_quit_keeps_last_pane() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(&cx, |w, _| w.pane_tree().clone());
        assert_eq!(pane_tree.read_with(&cx, |t, _| t.pane_count()), 1);

        ws.update(&mut cx, |w, cx| w.handle_quit(cx));
        cx.run_until_parked();

        assert_eq!(pane_tree.read_with(&cx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn workspace_handle_quit_all_with_no_dirty_buffers_does_not_open_modal() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update_in(vcx, |w, window, cx| w.handle_quit_all(window, cx));
        vcx.run_until_parked();

        let active = ws.read_with(vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<crate::quit_confirm::QuitConfirmModal>()
        });
        assert!(active.is_none());
    }

    #[test]
    fn workspace_handle_quit_all_with_dirty_buffer_opens_modal() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        let shared = ws.update_in(vcx, |w, _window, cx| {
            w.buffer_registry()
                .update(cx, |r, cx| r.open(Path::new("/tmp/repo/foo.rs"), "x", cx))
                .1
        });
        shared.write().expect("buffer poisoned").dirty = true;

        ws.update_in(vcx, |w, window, cx| w.handle_quit_all(window, cx));
        vcx.run_until_parked();

        let active = ws.read_with(vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<crate::quit_confirm::QuitConfirmModal>()
        });
        assert!(active.is_some());
    }

    #[test]
    fn workspace_observe_keystrokes_forwards_to_state_machine() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let window = vcx.window_handle();
        cx.simulate_keystrokes(window, "5");
        cx.run_until_parked();

        let sm = ws.read_with(&cx, |w, _| w.input_state_machine().clone());
        sm.read_with(&cx, |sm, _| assert_eq!(sm.pending_count(), Some(5)));
    }

    #[test]
    fn keystroke_space_p_chord_opens_file_finder() {
        use crate::{
            file_finder::FileFinderDelegate,
            globals::{FsHostGlobal, GitHostGlobal},
            picker::Picker,
        };
        use stoat::host::{fake::FakeGit, FakeFs, FsHost, GitHost};

        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo").with_fs(&fs);
        cx.update(|cx| {
            cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>));
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
        });
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/repo");
        let window = vcx.window_handle();
        cx.simulate_keystrokes(window, "space p");
        cx.run_until_parked();

        let finder_active = ws.read_with(&cx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<FileFinderDelegate>>()
                .is_some()
        });
        assert!(
            finder_active,
            "the 'space p' chord in normal mode should open the file finder"
        );
    }

    #[test]
    fn ime_colon_opens_command_palette() {
        use crate::{command_palette::CommandPaletteDelegate, picker::Picker};
        use gpui::EntityInputHandler;

        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor_input = ws.read_with(vcx, |w, _| w.editor_input().clone());

        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_text_in_range(None, ":", window, cx);
        });
        vcx.run_until_parked();
        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_text_in_range(None, ":", window, cx);
        });
        vcx.run_until_parked();

        let picker = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<CommandPaletteDelegate>>()
            })
            .expect("typing ':' should open the command palette");
        let query_text = picker.read_with(vcx, |p, cx| {
            p.query_editor()
                .read(cx)
                .multi_buffer()
                .read(cx)
                .snapshot()
                .text()
                .to_string()
        });
        assert_eq!(
            query_text, "",
            "the ':' that opened the palette must not leak into the query editor on the paired IME redelivery"
        );
    }

    #[test]
    fn ime_colon_then_escape_then_colon_keeps_second_palette_query_empty() {
        use crate::{command_palette::CommandPaletteDelegate, picker::Picker};
        use gpui::{EntityInputHandler, Keystroke, Modifiers};

        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor_input = ws.read_with(vcx, |w, _| w.editor_input().clone());

        for _ in 0..2 {
            editor_input.update_in(vcx, |ei, window, cx| {
                ei.replace_text_in_range(None, ":", window, cx);
            });
            vcx.run_until_parked();
        }

        let escape = Keystroke {
            modifiers: Modifiers::default(),
            key: "escape".into(),
            key_char: None,
        };
        ws.update_in(vcx, |w, window, cx| {
            let sm = w.input_state_machine().clone();
            let actions = sm.update(cx, |sm, cx| sm.feed(&escape, window, cx));
            for action in actions {
                w.dispatch_action(action, window, cx);
            }
        });
        vcx.run_until_parked();

        for _ in 0..2 {
            editor_input.update_in(vcx, |ei, window, cx| {
                ei.replace_text_in_range(None, ":", window, cx);
            });
            vcx.run_until_parked();
        }

        let picker = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<CommandPaletteDelegate>>()
            })
            .expect("the second ':' should re-open the command palette");
        let query_text = picker.read_with(vcx, |p, cx| {
            p.query_editor()
                .read(cx)
                .multi_buffer()
                .read(cx)
                .snapshot()
                .text()
                .to_string()
        });
        assert_eq!(
            query_text, "",
            "the ':' that re-opens the palette must not leak across the dismiss/re-open boundary"
        );
    }

    #[test]
    fn keystroke_colon_then_paired_text_input_keeps_palette_query_empty() {
        use crate::{command_palette::CommandPaletteDelegate, picker::Picker};
        use gpui::EntityInputHandler;

        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor_input = ws.read_with(vcx, |w, _| w.editor_input().clone());

        vcx.simulate_keystrokes(":");
        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_text_in_range(None, ":", window, cx);
        });
        vcx.run_until_parked();

        let picker = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<CommandPaletteDelegate>>()
            })
            .expect("real-typing ':' should open the command palette");
        let query_text = picker.read_with(vcx, |p, cx| {
            p.query_editor()
                .read(cx)
                .multi_buffer()
                .read(cx)
                .snapshot()
                .text()
                .to_string()
        });
        assert_eq!(
            query_text, "",
            "feed(':') via observe_keystrokes followed by text_input(':') via dispatch_input must not leak into the palette query"
        );
    }

    #[test]
    fn keystroke_colon_then_escape_then_colon_with_paired_text_inputs_keeps_palette_query_empty() {
        use crate::{command_palette::CommandPaletteDelegate, picker::Picker};
        use gpui::EntityInputHandler;

        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor_input = ws.read_with(vcx, |w, _| w.editor_input().clone());

        vcx.simulate_keystrokes(":");
        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_text_in_range(None, ":", window, cx);
        });
        vcx.run_until_parked();
        vcx.simulate_keystrokes("escape");
        vcx.simulate_keystrokes(":");
        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_text_in_range(None, ":", window, cx);
        });
        vcx.run_until_parked();

        let picker = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<CommandPaletteDelegate>>()
            })
            .expect("the second ':' should re-open the command palette");
        let query_text = picker.read_with(vcx, |p, cx| {
            p.query_editor()
                .read(cx)
                .multi_buffer()
                .read(cx)
                .snapshot()
                .text()
                .to_string()
        });
        assert_eq!(
            query_text, "",
            "real-typing ':' / Esc / ':' through the feed+text_input dual-fire must not leak the reopen char"
        );
    }

    #[test]
    fn keystroke_j_with_paired_text_input_moves_cursor_one_row() {
        use gpui::EntityInputHandler;

        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor_input = ws.read_with(vcx, |w, _| w.editor_input().clone());
        let editor = new_singleton_editor(vcx, "r0\nr1\nr2\nr3\n");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        vcx.simulate_keystrokes("j");
        vcx.run_until_parked();
        assert_eq!(
            editor_cursor_buffer_row(vcx, &editor),
            1,
            "control: feed('j') alone (no IME twin in normal mode) advances one row"
        );

        // macOS fires both paths per keypress; supply the IME twin
        // that simulate_keystrokes omits in normal mode.
        vcx.simulate_keystrokes("j");
        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_text_in_range(None, "j", window, cx);
        });
        vcx.run_until_parked();
        assert_eq!(
            editor_cursor_buffer_row(vcx, &editor),
            2,
            "feed('j') plus its text_input('j') IME twin must advance exactly one row, not two"
        );
    }

    #[test]
    fn keystroke_space_p_chord_with_paired_text_input_keeps_finder_query_empty() {
        use crate::{
            file_finder::FileFinderDelegate,
            globals::{FsHostGlobal, GitHostGlobal},
            picker::Picker,
        };
        use gpui::EntityInputHandler;
        use stoat::host::{fake::FakeGit, FakeFs, FsHost, GitHost};

        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo").with_fs(&fs);
        cx.update(|cx| {
            cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>));
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
        });
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/repo");
        let editor_input = ws.read_with(vcx, |w, _| w.editor_input().clone());

        vcx.simulate_keystrokes("space p");
        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_text_in_range(None, "p", window, cx);
        });
        vcx.run_until_parked();

        let picker = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<FileFinderDelegate>>()
            })
            .expect("the 'space p' chord should open the file finder");
        let query_text = picker.read_with(vcx, |p, cx| {
            p.query_editor()
                .read(cx)
                .multi_buffer()
                .read(cx)
                .snapshot()
                .text()
                .to_string()
        });
        assert_eq!(
            query_text, "",
            "the 'p' that completes the chord followed by its paired text_input('p') must not leak into the finder query"
        );
    }

    #[test]
    fn ime_space_p_chord_opens_file_finder() {
        use crate::{
            file_finder::FileFinderDelegate,
            globals::{FsHostGlobal, GitHostGlobal},
            picker::Picker,
        };
        use gpui::EntityInputHandler;
        use stoat::host::{fake::FakeGit, FakeFs, FsHost, GitHost};

        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo").with_fs(&fs);
        cx.update(|cx| {
            cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>));
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
        });
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/repo");
        let editor_input = ws.read_with(vcx, |w, _| w.editor_input().clone());

        for ch in [" ", "p"] {
            editor_input.update_in(vcx, |ei, window, cx| {
                ei.replace_text_in_range(None, ch, window, cx);
            });
        }
        vcx.run_until_parked();

        let finder_active = ws.read_with(vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<FileFinderDelegate>>()
                .is_some()
        });
        assert!(
            finder_active,
            "the 'space p' chord via the IME replace_text_in_range path should open the file finder"
        );
    }

    #[test]
    fn ime_space_p_chord_keeps_finder_query_empty() {
        use crate::{
            file_finder::FileFinderDelegate,
            globals::{FsHostGlobal, GitHostGlobal},
            picker::Picker,
        };
        use gpui::EntityInputHandler;
        use stoat::host::{fake::FakeGit, FakeFs, FsHost, GitHost};

        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo").with_fs(&fs);
        cx.update(|cx| {
            cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>));
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
        });
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/repo");
        let editor_input = ws.read_with(vcx, |w, _| w.editor_input().clone());

        // First half of the chord: space dispatches SetMode(space) so the
        // marker is not armed (action list comes from a SetMode handled
        // inline, not from a printable-key dispatch).
        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_text_in_range(None, " ", window, cx);
        });
        vcx.run_until_parked();
        // Second half: `p` resolves OpenFileFinder in space mode; the
        // mode-armed marker should then drop the macOS paired redelivery.
        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_text_in_range(None, "p", window, cx);
        });
        vcx.run_until_parked();
        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_text_in_range(None, "p", window, cx);
        });
        vcx.run_until_parked();

        let picker = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<FileFinderDelegate>>()
            })
            .expect("space p should open the file finder");
        let query_text = picker.read_with(vcx, |p, cx| {
            p.query_editor()
                .read(cx)
                .multi_buffer()
                .read(cx)
                .snapshot()
                .text()
                .to_string()
        });
        assert_eq!(
            query_text, "",
            "the 'p' that completed the chord must not leak into the finder query on the paired IME redelivery"
        );
    }

    fn dispatch<A: stoat_action::Action>(
        ws: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
        action: A,
    ) {
        ws.update_in(vcx, |w, window, cx| {
            w.dispatch_action(Box::new(action), window, cx);
        });
    }

    #[test]
    fn dispatch_run_opens_run_modal() {
        use stoat::host::{
            fake::terminal::{FakeTerminalHost, FakeTerminalSession},
            TerminalHost,
        };

        let mut cx = TestAppContext::single();
        let session = Arc::new(FakeTerminalSession::new());
        let terminal: Arc<dyn TerminalHost> = Arc::new(FakeTerminalHost::new(session));
        cx.update(|cx| cx.set_global(crate::globals::TerminalHostGlobal(terminal)));
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(
            &ws,
            vcx,
            stoat_action::Run {
                command: "echo hi".into(),
            },
        );
        vcx.run_until_parked();

        let opened = ws.read_with(vcx, |w, cx| {
            w.modal_layer
                .read(cx)
                .active_modal::<crate::run_modal::RunModal>()
                .is_some()
        });
        assert!(opened, "Run should open a run modal overlay");
    }

    #[test]
    fn dispatch_open_changed_file_picker_lists_changed_files() {
        use crate::{
            file_finder::FileFinderDelegate,
            globals::{FsHostGlobal, GitHostGlobal},
            picker::{Picker, PickerDelegate},
        };
        use stoat::host::{fake::FakeGit, FakeFs, FsHost, GitHost};

        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        let git = Arc::new(FakeGit::new());
        {
            let mut builder = git.add_repo("/repo").with_fs(&fs);
            builder.modified("a.rs", "v1\n", "v2\n");
            builder.modified("b.rs", "v1\n", "v2\n");
            builder.unstaged_file("c.rs", "c\n");
            builder.unstaged_file("d.rs", "d\n");
            builder.unstaged_file("e.rs", "e\n");
        }
        cx.update(|cx| {
            cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>));
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
        });
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/repo");

        dispatch(&ws, vcx, stoat_action::OpenChangedFilePicker);
        vcx.run_until_parked();

        let count = ws.read_with(vcx, |w, cx| {
            w.modal_layer
                .read(cx)
                .active_modal::<Picker<FileFinderDelegate>>()
                .map(|picker| picker.read(cx).delegate().match_count())
        });
        assert_eq!(
            count,
            Some(5),
            "changed-file picker should list the two modified and three untracked files",
        );
    }

    #[test]
    fn dispatch_file_finder_scope_toggle_switches_scope() {
        use crate::{
            file_finder::FileFinderDelegate,
            globals::{FsHostGlobal, GitHostGlobal},
            picker::{Picker, PickerDelegate},
        };
        use stoat::host::{fake::FakeGit, FakeFs, FsHost, GitHost};

        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        let git = Arc::new(FakeGit::new());
        {
            let mut builder = git.add_repo("/repo").with_fs(&fs);
            builder.modified("a.rs", "v1\n", "v2\n");
            builder.unstaged_file("b.rs", "b\n");
        }
        fs.insert_files([
            (PathBuf::from("/repo/c.rs"), b"c".as_slice()),
            (PathBuf::from("/repo/d.rs"), b"d".as_slice()),
        ]);
        cx.update(|cx| {
            cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>));
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
        });
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/repo");

        dispatch(&ws, vcx, stoat_action::OpenFileFinder);
        vcx.run_until_parked();

        let match_count = |vcx: &mut VisualTestContext| {
            ws.read_with(vcx, |w, cx| {
                w.modal_layer
                    .read(cx)
                    .active_modal::<Picker<FileFinderDelegate>>()
                    .map(|p| p.read(cx).delegate().match_count())
            })
        };

        assert_eq!(
            match_count(vcx),
            Some(4),
            "all-files scope lists every walked file"
        );

        dispatch(&ws, vcx, stoat_action::FileFinderScopeToggle);
        vcx.run_until_parked();
        assert_eq!(
            match_count(vcx),
            Some(2),
            "toggling lists only the two git-changed files",
        );

        dispatch(&ws, vcx, stoat_action::FileFinderScopeToggle);
        vcx.run_until_parked();
        assert_eq!(
            match_count(vcx),
            Some(4),
            "toggling back returns to all files"
        );
    }

    #[test]
    fn dispatch_toggle_project_tree_opens_and_closes_left_dock() {
        use crate::{globals::FsHostGlobal, item::ItemKind};
        use stoat::host::{FakeFs, FsHost};

        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/a.rs", "");
        fs.insert_dir("/repo/src");
        cx.update(|cx| cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>)));
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/repo");

        let project_tree_docks = |vcx: &mut VisualTestContext| {
            ws.read_with(vcx, |w, cx| {
                w.docks()
                    .iter()
                    .filter(|d| d.read(cx).item().item_kind(cx) == ItemKind::ProjectTree)
                    .count()
            })
        };

        dispatch(&ws, vcx, stoat_action::ToggleProjectTree);
        vcx.run_until_parked();
        assert_eq!(
            project_tree_docks(vcx),
            1,
            "toggle opens a project tree dock"
        );

        dispatch(&ws, vcx, stoat_action::ToggleProjectTree);
        vcx.run_until_parked();
        assert_eq!(project_tree_docks(vcx), 0, "toggling again removes it");
    }

    #[test]
    fn dispatch_toggle_project_tree_enters_and_exits_project_tree_mode() {
        use crate::globals::FsHostGlobal;
        use stoat::host::{FakeFs, FsHost};

        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/a.rs", "");
        cx.update(|cx| cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>)));
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/repo");

        let mode = |vcx: &mut VisualTestContext| {
            ws.read_with(vcx, |w, cx| {
                w.input_state_machine().read(cx).mode().to_string()
            })
        };

        dispatch(&ws, vcx, stoat_action::ToggleProjectTree);
        vcx.run_until_parked();
        assert_eq!(
            mode(vcx),
            "project_tree",
            "opening enters project_tree mode"
        );

        dispatch(&ws, vcx, stoat_action::ToggleProjectTree);
        vcx.run_until_parked();
        assert_eq!(mode(vcx), "normal", "closing returns to normal mode");
    }

    #[test]
    fn active_item_kind_drives_input_mode() {
        use crate::item::ItemKind;

        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane = {
            let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
            pane_tree
                .read_with(vcx, |t, _| t.pane(t.focus()).cloned())
                .expect("focused pane registered")
        };

        let mode = |vcx: &mut VisualTestContext| {
            ws.read_with(vcx, |w, cx| {
                w.input_state_machine().read(cx).mode().to_string()
            })
        };

        for (label, kind, expected) in [
            ("review", ItemKind::Review, "review"),
            ("rebase", ItemKind::Rebase, "rebase"),
            ("conflict", ItemKind::Conflict, "conflict"),
            ("editor", ItemKind::Editor, "normal"),
        ] {
            let item = workspace_item_of_kind(vcx, label, kind);
            pane.update(vcx, |p, cx| {
                let index = p.add_item(item, cx);
                p.activate(index, cx);
            });
            vcx.run_until_parked();
            assert_eq!(mode(vcx), expected, "{label} item drives {expected} mode");
        }
    }

    #[test]
    fn pane_event_preserves_submode_on_unchanged_active_item() {
        use crate::item::ItemKind;

        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane = {
            let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
            pane_tree
                .read_with(vcx, |t, _| t.pane(t.focus()).cloned())
                .expect("focused pane registered")
        };

        let mode = |vcx: &mut VisualTestContext| {
            ws.read_with(vcx, |w, cx| {
                w.input_state_machine().read(cx).mode().to_string()
            })
        };

        let item = workspace_item_of_kind(vcx, "review", ItemKind::Review);
        pane.update(vcx, |p, cx| {
            let index = p.add_item(item, cx);
            p.activate(index, cx);
        });
        vcx.run_until_parked();
        assert_eq!(mode(vcx), "review", "review item enters review mode");

        ws.update(vcx, |w, cx| w.set_input_mode("line_select", cx));
        ws.update(vcx, |w, cx| w.broadcast_active_pane_item(cx));
        vcx.run_until_parked();

        assert_eq!(
            mode(vcx),
            "line_select",
            "a pane event on the unchanged active item keeps the submode",
        );
    }

    #[test]
    fn delete_tree_path_removes_directory_from_disk() {
        use crate::globals::FsHostGlobal;
        use stoat::host::{FakeFs, FsHost};

        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        fs.insert_dir("/repo/sub");
        fs.insert_file("/repo/sub/a.rs", "a");
        cx.update(|cx| cx.set_global(FsHostGlobal(fs.clone() as Arc<dyn FsHost>)));
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/repo");

        ws.update_in(vcx, |w, _window, cx| {
            w.delete_tree_path(PathBuf::from("/repo/sub"), true, cx);
        });

        assert!(!fs.exists(Path::new("/repo/sub")));
        assert!(!fs.exists(Path::new("/repo/sub/a.rs")));
    }

    #[test]
    fn project_tree_confirm_on_file_opens_it_and_exits_mode() {
        use crate::globals::FsHostGlobal;
        use stoat::host::{FakeFs, FsHost};

        let mut cx = TestAppContext::single();
        let fs = Arc::new(FakeFs::new());
        fs.insert_dir("/repo");
        fs.insert_dir("/repo/src");
        fs.insert_file("/repo/a.rs", "hello\n");
        cx.update(|cx| cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>)));
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/repo");

        dispatch(&ws, vcx, stoat_action::ToggleProjectTree);
        vcx.run_until_parked();

        dispatch(&ws, vcx, stoat_action::ProjectTreeSelectNext);
        dispatch(&ws, vcx, stoat_action::ProjectTreeConfirm);
        vcx.run_until_parked();

        let opened = ws.read_with(vcx, |w, cx| {
            w.active_pane_item(cx)
                .and_then(|item| item.to_any_view().downcast::<Editor>().ok())
                .and_then(|ed| ed.read(cx).file_path().map(Path::to_path_buf))
        });
        assert_eq!(opened, Some(PathBuf::from("/repo/a.rs")));
        assert_eq!(
            ws.read_with(vcx, |w, cx| {
                w.input_state_machine().read(cx).mode().to_string()
            }),
            "normal",
            "opening a file exits project_tree mode"
        );
    }

    #[test]
    fn dispatch_dump_writes_archive_under_dumps_dir() {
        use stoat::host::FsHost;
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/main.rs", b"fn main() {}\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(
            &ws,
            vcx,
            stoat_action::Dump {
                name: "gui-bug".to_string(),
            },
        );
        vcx.run_until_parked();

        let dumps = stoat::dump::dumps_dir().expect("dumps dir resolves");
        let dump_files: Vec<String> = fs
            .list_dir(&dumps)
            .expect("dumps dir listed")
            .into_iter()
            .filter(|e| e.name.ends_with(".dump"))
            .map(|e| e.name.to_string())
            .collect();
        assert_eq!(
            dump_files.len(),
            1,
            "exactly one dump written, got {dump_files:?}"
        );
        assert!(
            dump_files[0].ends_with("_gui-bug.dump"),
            "dump filename carries the sanitized name: {}",
            dump_files[0]
        );
    }

    #[test]
    fn dispatch_split_right_grows_pane_tree() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitRight);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 2);
    }

    #[test]
    fn dispatch_split_down_grows_pane_tree() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitDown);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 2);
    }

    #[test]
    fn dispatch_split_new_right_adds_pane_with_fresh_scratch_editor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        let registry_size_before = ws.read_with(vcx, |w, cx| w.buffer_registry().read(cx).len());

        dispatch(&ws, vcx, stoat_action::SplitNewRight);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 2);
        assert_eq!(
            ws.read_with(vcx, |w, cx| w.buffer_registry().read(cx).len()),
            registry_size_before + 1,
            "a new scratch buffer should be registered",
        );
        let focused_id = pane_tree.read_with(vcx, |t, _| t.focus());
        let focused_pane = pane_tree
            .read_with(vcx, |t, _| t.pane(focused_id).cloned())
            .expect("focused pane registered");
        assert_eq!(
            focused_pane.read_with(vcx, |p, _| p.items().len()),
            1,
            "new pane should contain exactly one scratch editor",
        );
    }

    #[test]
    fn dispatch_open_markdown_preview_splits_and_adds_preview() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "# Title\n\nbody\n");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::OpenMarkdownPreview);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 2);
        let has_preview = pane_tree.read_with(vcx, |t, cx| {
            t.split_pane_ids().iter().any(|id| {
                t.pane(*id).is_some_and(|p| {
                    p.read(cx)
                        .items()
                        .iter()
                        .any(|it| it.item_kind(cx) == crate::item::ItemKind::MarkdownPreview)
                })
            })
        });
        assert!(has_preview, "a pane should host the markdown preview");
    }

    #[test]
    fn dispatch_split_new_down_adds_pane_with_fresh_scratch_editor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitNewDown);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 2);
        let focused_id = pane_tree.read_with(vcx, |t, _| t.focus());
        let focused_pane = pane_tree
            .read_with(vcx, |t, _| t.pane(focused_id).cloned())
            .expect("focused pane registered");
        assert_eq!(
            focused_pane.read_with(vcx, |p, _| p.items().len()),
            1,
            "new pane should contain exactly one scratch editor",
        );
    }

    fn workspace_item(vcx: &mut VisualTestContext, label: &str) -> Box<dyn ItemHandle> {
        workspace_item_of_kind(vcx, label, crate::item::ItemKind::Unknown)
    }

    fn workspace_item_of_kind(
        vcx: &mut VisualTestContext,
        label: &str,
        kind: crate::item::ItemKind,
    ) -> Box<dyn ItemHandle> {
        let label = SharedString::from(label.to_string());
        let entity = vcx.update(|_, cx| cx.new(|_| WorkspaceItem { label, kind }));
        Box::new(entity)
    }

    #[test]
    fn dispatch_copy_workspace_opens_window_with_matching_state() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/a.rs", b"alpha");
        fs.insert_file("/tmp/repo/b.rs", b"beta");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(
                &[
                    PathBuf::from("/tmp/repo/a.rs"),
                    PathBuf::from("/tmp/repo/b.rs"),
                ],
                cx,
            );
        });
        vcx.run_until_parked();

        let source_uid = ws.read_with(vcx, |w, _| w.uid());
        let source_pane_count = ws.read_with(vcx, |w, cx| w.pane_tree().read(cx).pane_count());
        assert_eq!(source_pane_count, 2, "source has two panes");

        dispatch(&ws, vcx, stoat_action::CopyWorkspace);
        vcx.run_until_parked();

        assert_eq!(
            vcx.update(|_, cx| cx.windows().len()),
            2,
            "CopyWorkspace opens a second window",
        );

        let new_handle = vcx
            .update(|_, cx| {
                cx.windows()
                    .into_iter()
                    .find_map(|h| h.downcast::<crate::stoat_app::StoatApp>())
            })
            .expect("copied StoatApp window present");

        let new_workspace = new_handle
            .read_with(vcx, |app, _| app.workspace().clone())
            .expect("copy window's workspace entity");
        let (new_pane_count, new_uid) =
            new_workspace.read_with(vcx, |w, cx| (w.pane_tree().read(cx).pane_count(), w.uid()));
        assert_eq!(
            new_pane_count, source_pane_count,
            "copy preserves pane tree shape",
        );
        assert_ne!(new_uid, source_uid, "copy gets a fresh uid",);
    }

    #[test]
    fn dispatch_close_workspace_removes_window_when_clean() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        assert_eq!(vcx.update(|_, cx| cx.windows().len()), 1);

        dispatch(&ws, vcx, stoat_action::CloseWorkspace);
        vcx.run_until_parked();

        let remaining = vcx.cx.read(|app| app.windows().len());
        assert_eq!(
            remaining, 0,
            "clean CloseWorkspace removes the hosting window",
        );
    }

    #[test]
    fn dispatch_close_workspace_with_dirty_buffer_opens_confirm_modal() {
        use crate::quit_confirm::QuitConfirmModal;
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/a.rs", b"alpha");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/a.rs")], cx);
        });
        vcx.run_until_parked();
        ws.update(vcx, |w, cx| {
            let registry = w.buffer_registry().clone();
            registry.update(cx, |r, _| {
                let id = r.ids().next().expect("a.rs registered");
                let shared = r.get(id).expect("buffer shared").clone();
                shared.write().expect("buffer lock").edit(0..0, "x");
            });
        });
        vcx.run_until_parked();

        dispatch(&ws, vcx, stoat_action::CloseWorkspace);
        vcx.run_until_parked();

        let modal_active = ws.read_with(vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<QuitConfirmModal>()
                .is_some()
        });
        assert!(
            modal_active,
            "dirty CloseWorkspace should open the quit-confirm modal",
        );
        assert_eq!(
            vcx.update(|_, cx| cx.windows().len()),
            1,
            "window stays open while the modal is up",
        );
    }

    #[test]
    fn dispatch_rename_workspace_with_name_sets_name_directly() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(
            &ws,
            vcx,
            stoat_action::RenameWorkspace { name: "foo".into() },
        );
        vcx.run_until_parked();

        let name = ws.read_with(vcx, |w, _| w.name().clone());
        assert_eq!(name.as_ref(), "foo");
        let label_name = ws.read_with(vcx, |w, cx| w.workspace_label().read(cx).name().clone());
        assert_eq!(label_name.as_ref(), "foo");
    }

    #[test]
    fn dispatch_rename_workspace_with_empty_name_opens_modal() {
        use crate::rename_workspace_modal::RenameWorkspaceModal;
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::RenameWorkspace { name: "".into() });
        vcx.run_until_parked();

        let modal_active = ws.read_with(vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<RenameWorkspaceModal>()
                .is_some()
        });
        assert!(modal_active, "empty-name RenameWorkspace opens the modal");
        let name = ws.read_with(vcx, |w, _| w.name().clone());
        assert_eq!(
            name.as_ref(),
            "main",
            "workspace name should remain unchanged"
        );
    }

    #[test]
    fn dispatch_set_cwd_changes_git_root() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(
            &ws,
            vcx,
            stoat_action::SetCwd {
                path: "/tmp/elsewhere".into(),
            },
        );
        vcx.run_until_parked();

        let root = ws.read_with(vcx, |w, _| w.git_root().clone());
        assert_eq!(root, PathBuf::from("/tmp/elsewhere"));
    }

    #[test]
    fn set_git_root_propagates_to_blame_coordinator() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| w.set_git_root("/tmp/elsewhere", cx));

        let root = ws.read_with(vcx, |w, cx| {
            w.blame_coordinator().read(cx).git_root().to_path_buf()
        });
        assert_eq!(root, PathBuf::from("/tmp/elsewhere"));
    }

    #[test]
    fn dispatch_switch_workspace_opens_picker_modal() {
        use crate::workspace_picker::WorkspacePickerDelegate;
        use stoat::host::FsHost;
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        let fs_dyn: Arc<dyn FsHost> = fs.clone();
        let dir = stoat::workspace::persist::workspace_dir_for(Path::new("/tmp/repo"), &*fs_dyn)
            .expect("workspace dir");
        let current_uid = ws.read_with(vcx, |w, _| w.uid());
        let other_uid = stoat::workspace::WorkspaceUid(current_uid.0.wrapping_add(1));
        let mut sibling_state = ws.read_with(vcx, |w, cx| w.to_state(cx));
        sibling_state.uid = other_uid;
        sibling_state.name = "sibling".to_string();
        let body = ron::ser::to_string_pretty(&sibling_state, ron::ser::PrettyConfig::default())
            .expect("serialize");
        fs.create_dir_all(&dir).expect("create state dir");
        fs.write(&dir.join(format!("{other_uid}.ron")), body.as_bytes())
            .expect("write state file");

        dispatch(&ws, vcx, stoat_action::SwitchWorkspace);
        vcx.run_until_parked();

        let has_modal = ws.read_with(vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<crate::picker::Picker<WorkspacePickerDelegate>>()
                .is_some()
        });
        assert!(
            has_modal,
            "SwitchWorkspace should open the workspace picker modal"
        );
    }

    #[test]
    fn dispatch_new_workspace_opens_additional_gpui_window() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let before = vcx.update(|_, cx| cx.windows().len());
        assert_eq!(before, 1, "harness opens one window before dispatch");

        dispatch(&ws, vcx, stoat_action::NewWorkspace);
        vcx.run_until_parked();

        let after = vcx.update(|_, cx| cx.windows().len());
        assert_eq!(after, 2, "NewWorkspace opens a second gpui window");
    }

    #[test]
    fn fresh_workspace_reports_both_dock_sides_visible() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        ws.read_with(&cx, |w, _| {
            assert!(w.dock_side_visible(DockSide::Left));
            assert!(w.dock_side_visible(DockSide::Right));
        });
    }

    #[test]
    fn fresh_workspace_reports_bottom_dock_visible() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        ws.read_with(&cx, |w, _| {
            assert!(w.dock_side_visible(DockSide::Bottom));
        });
    }

    #[test]
    fn dispatch_toggle_dock_left_flips_left_only() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::ToggleDockLeft);
        vcx.run_until_parked();

        ws.read_with(vcx, |w, _| {
            assert!(!w.dock_side_visible(DockSide::Left));
            assert!(w.dock_side_visible(DockSide::Right));
        });
    }

    #[test]
    fn dispatch_toggle_dock_right_flips_right_only() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::ToggleDockRight);
        vcx.run_until_parked();

        ws.read_with(vcx, |w, _| {
            assert!(w.dock_side_visible(DockSide::Left));
            assert!(!w.dock_side_visible(DockSide::Right));
        });
    }

    #[test]
    fn dispatch_toggle_dock_left_round_trips() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::ToggleDockLeft);
        vcx.run_until_parked();
        dispatch(&ws, vcx, stoat_action::ToggleDockLeft);
        vcx.run_until_parked();

        ws.read_with(vcx, |w, _| {
            assert!(w.dock_side_visible(DockSide::Left));
        });
    }

    #[test]
    fn dispatch_close_buffer_removes_active_tab_keeping_pane() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        let pane = pane_tree
            .read_with(vcx, |t, _| t.pane(t.focus()).cloned())
            .expect("focused pane registered");

        let alpha = workspace_item(vcx, "alpha");
        let beta = workspace_item(vcx, "beta");
        pane.update(vcx, |p, cx| {
            p.add_item(alpha, cx);
            p.add_item(beta, cx);
        });
        assert_eq!(pane.read_with(vcx, |p, _| p.items().len()), 2);

        dispatch(&ws, vcx, stoat_action::CloseBuffer);
        vcx.run_until_parked();

        assert_eq!(pane.read_with(vcx, |p, _| p.items().len()), 1);
        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn dispatch_close_buffer_on_single_item_pane_empties_pane() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        let pane = pane_tree
            .read_with(vcx, |t, _| t.pane(t.focus()).cloned())
            .expect("focused pane registered");

        let alpha = workspace_item(vcx, "alpha");
        pane.update(vcx, |p, cx| {
            p.add_item(alpha, cx);
        });

        dispatch(&ws, vcx, stoat_action::CloseBuffer);
        vcx.run_until_parked();

        assert_eq!(pane.read_with(vcx, |p, _| p.items().len()), 0);
        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn dispatch_close_buffer_on_empty_pane_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::CloseBuffer);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn dispatch_close_pane_after_split_returns_to_single() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitRight);
        dispatch(&ws, vcx, stoat_action::ClosePane);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn dispatch_close_other_panes_collapses_to_focused() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitRight);
        dispatch(&ws, vcx, stoat_action::SplitDown);
        dispatch(&ws, vcx, stoat_action::SplitRight);
        dispatch(&ws, vcx, stoat_action::defs::pane::CloseOtherPanes);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn dispatch_focus_direction_changes_focused_pane() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitRight);
        let after_split = pane_tree.read_with(vcx, |t, _| t.focus());

        dispatch(&ws, vcx, stoat_action::FocusLeft);
        vcx.run_until_parked();

        let after_focus_left = pane_tree.read_with(vcx, |t, _| t.focus());
        assert_ne!(after_focus_left, after_split);
    }

    #[test]
    fn dispatch_focus_next_cycles_through_panes() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitRight);
        let after_split = pane_tree.read_with(vcx, |t, _| t.focus());

        dispatch(&ws, vcx, stoat_action::FocusNext);
        vcx.run_until_parked();
        let after_next = pane_tree.read_with(vcx, |t, _| t.focus());
        assert_ne!(after_next, after_split);

        dispatch(&ws, vcx, stoat_action::FocusNext);
        vcx.run_until_parked();
        let after_wrap = pane_tree.read_with(vcx, |t, _| t.focus());
        assert_eq!(after_wrap, after_split);
    }

    #[test]
    fn dispatch_quit_closes_focused_pane_when_multiple_exist() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        dispatch(&ws, vcx, stoat_action::SplitRight);
        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 2);

        dispatch(&ws, vcx, stoat_action::Quit);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn dispatch_unknown_action_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        let before = pane_tree.read_with(vcx, |t, _| (t.pane_count(), t.focus()));

        dispatch(&ws, vcx, stoat_action::MoveLeft);
        vcx.run_until_parked();

        let after = pane_tree.read_with(vcx, |t, _| (t.pane_count(), t.focus()));
        assert_eq!(before, after);
    }

    #[test]
    fn dispatch_set_active_pane_changes_focus() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitRight);
        let original_id = pane_tree.read_with(vcx, |t, _| t.focus());
        dispatch(&ws, vcx, stoat_action::FocusLeft);
        let other_id = pane_tree.read_with(vcx, |t, _| t.focus());
        assert_ne!(original_id, other_id);

        dispatch(
            &ws,
            vcx,
            crate::actions::SetActivePane {
                pane_id: original_id.as_ffi(),
            },
        );
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.focus()), original_id);
    }

    #[test]
    fn dispatch_dismiss_modal_closes_active_modal() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update_in(vcx, |w, window, cx| {
            w.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::new(cx));
        });
        vcx.run_until_parked();

        dispatch(&ws, vcx, stoat_action::DismissModal);
        vcx.run_until_parked();

        let active = ws.read_with(vcx, |w, cx| {
            w.modal_layer().read(cx).active_modal::<TestModal>()
        });
        assert!(active.is_none());
    }

    fn new_singleton_editor(vcx: &mut VisualTestContext, text: &str) -> Entity<Editor> {
        use crate::{
            buffer::Buffer,
            diff_map::DiffMap,
            display_map::DisplayMap,
            editor::{Editor, EditorMode},
            multi_buffer::MultiBuffer,
        };
        use stoat::buffer::BufferId;
        use stoat_scheduler::{Executor, TestScheduler};

        let buffer = vcx.update(|_, cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let multi_buffer = {
            let buffer = buffer.clone();
            vcx.update(|_, cx| cx.new(|cx| MultiBuffer::singleton(buffer, cx)))
        };
        let display_map = {
            let buffer = buffer.clone();
            vcx.update(|_, cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        let diff_map = vcx.update(|_, cx| cx.new(|cx| DiffMap::new(buffer, cx)));
        vcx.update(|_, cx| {
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        })
    }

    fn cursor_offsets(vcx: &mut VisualTestContext, editor: &Entity<Editor>) -> Vec<usize> {
        editor.update(vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            ed.selections()
                .all_anchors()
                .iter()
                .map(|s| snapshot.resolve_anchor(&s.start))
                .collect()
        })
    }

    #[test]
    fn dispatch_click_at_moves_active_editor_cursor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, crate::actions::ClickAt { row: 0, col: 6 });
        vcx.run_until_parked();

        assert_eq!(cursor_offsets(vcx, &editor), vec![6]);
    }

    #[test]
    fn dispatch_click_at_without_active_editor_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        let before = pane_tree.read_with(vcx, |t, _| (t.pane_count(), t.focus()));

        dispatch(&ws, vcx, crate::actions::ClickAt { row: 3, col: 7 });
        vcx.run_until_parked();

        let after = pane_tree.read_with(vcx, |t, _| (t.pane_count(), t.focus()));
        assert_eq!(before, after);
    }

    #[test]
    fn dispatch_click_at_with_dropped_editor_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let weak = {
            let editor = new_singleton_editor(vcx, "hello");
            editor.downgrade()
        };
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(weak)));
        vcx.run_until_parked();

        dispatch(&ws, vcx, crate::actions::ClickAt { row: 0, col: 2 });
        vcx.run_until_parked();
    }

    #[test]
    fn dispatch_click_at_from_editor_listener_path_moves_cursor_without_panic() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        editor.update(vcx, |ed, cx| {
            ed.set_workspace(Some(ws.downgrade()));
            ed.set_cell_size(size(px(10.0), px(20.0)), cx);
            ed.set_text_region_bounds(
                Bounds {
                    origin: Point::default(),
                    size: size(px(800.0), px(600.0)),
                },
                cx,
            );
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        editor.update_in(vcx, |ed, window, cx| {
            ed.dispatch_click_at(Point::new(px(60.0), px(0.0)), window, cx);
        });
        vcx.run_until_parked();

        assert_eq!(cursor_offsets(vcx, &editor), vec![6]);
    }

    #[test]
    fn pane_add_item_broadcasts_active_editor_to_input_state_machine() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "");

        ws.update(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            let pane = w
                .pane_tree
                .read(cx)
                .pane(focus)
                .expect("focused pane")
                .clone();
            pane.update(cx, |p, cx| {
                p.add_item(Box::new(editor.clone()), cx);
            });
        });
        vcx.run_until_parked();

        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        let (focus_target_present, active_editor_id) = sm.read_with(vcx, |sm, _| {
            (
                sm.editor_focus_target().is_some(),
                sm.active_editor()
                    .and_then(|weak| weak.upgrade())
                    .map(|e| e.entity_id()),
            )
        });
        assert!(
            focus_target_present,
            "editor_focus_target must populate after pane.add_item(editor) so insert mode can focus the editor input handler"
        );
        assert_eq!(active_editor_id, Some(editor.entity_id()));
    }

    #[test]
    fn backspace_in_insert_mode_shrinks_active_editor_buffer() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "ab");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, crate::actions::ClickAt { row: 0, col: 2 });
        vcx.run_until_parked();

        let window = vcx.window_handle();
        cx.simulate_keystrokes(window, "i");
        cx.simulate_keystrokes(window, "backspace");
        cx.run_until_parked();

        let text = editor.read_with(&cx, |ed, cx| {
            ed.multi_buffer().read(cx).snapshot().text().to_string()
        });
        assert_eq!(text, "a");
    }

    #[test]
    fn enter_in_insert_mode_inserts_newline_when_no_completion_popup() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "ab");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, crate::actions::ClickAt { row: 0, col: 2 });
        vcx.run_until_parked();

        let window = vcx.window_handle();
        cx.simulate_keystrokes(window, "i");
        cx.simulate_keystrokes(window, "enter");
        cx.run_until_parked();

        let text = editor.read_with(&cx, |ed, cx| {
            ed.multi_buffer().read(cx).snapshot().text().to_string()
        });
        assert_eq!(text, "ab\n");
    }

    fn selection_offsets(
        vcx: &mut VisualTestContext,
        editor: &Entity<Editor>,
    ) -> Vec<(usize, usize)> {
        editor.update(vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            ed.selections()
                .all_anchors()
                .iter()
                .map(|s| {
                    (
                        snapshot.resolve_anchor(&s.start),
                        snapshot.resolve_anchor(&s.end),
                    )
                })
                .collect()
        })
    }

    #[test]
    fn dispatch_drag_select_to_extends_primary_selection() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        dispatch(&ws, vcx, crate::actions::ClickAt { row: 0, col: 2 });
        vcx.run_until_parked();

        dispatch(&ws, vcx, crate::actions::DragSelectTo { row: 0, col: 7 });
        vcx.run_until_parked();

        assert_eq!(selection_offsets(vcx, &editor), vec![(2, 7)]);
    }

    #[test]
    fn dispatch_drag_select_to_without_active_editor_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        let before = pane_tree.read_with(vcx, |t, _| (t.pane_count(), t.focus()));

        dispatch(&ws, vcx, crate::actions::DragSelectTo { row: 1, col: 4 });
        vcx.run_until_parked();

        let after = pane_tree.read_with(vcx, |t, _| (t.pane_count(), t.focus()));
        assert_eq!(before, after);
    }

    #[test]
    fn dispatch_drag_select_to_from_editor_listener_path_extends_selection_without_panic() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        editor.update(vcx, |ed, cx| {
            ed.set_workspace(Some(ws.downgrade()));
            ed.set_cell_size(size(px(10.0), px(20.0)), cx);
            ed.set_text_region_bounds(
                Bounds {
                    origin: Point::default(),
                    size: size(px(800.0), px(600.0)),
                },
                cx,
            );
            ed.set_cursor_at_grid(0, 2, cx);
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        editor.update_in(vcx, |ed, window, cx| {
            ed.dispatch_drag_select_to(Point::new(px(70.0), px(0.0)), window, cx);
        });
        vcx.run_until_parked();

        assert_eq!(selection_offsets(vcx, &editor), vec![(2, 7)]);
    }

    #[test]
    fn dispatch_hover_at_sets_position_on_active_editor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, crate::actions::HoverAt { row: 0, col: 4 });
        vcx.run_until_parked();

        assert_eq!(
            editor.read_with(vcx, |ed, _| ed.hover_position()),
            Some((0, 4))
        );
    }

    #[test]
    fn dispatch_hover_sets_position_at_primary_cursor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        editor.update(vcx, |ed, cx| ed.set_cursor_at_grid(0, 6, cx));
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::Hover);
        vcx.run_until_parked();

        assert_eq!(
            editor.read_with(vcx, |ed, _| ed.hover_position()),
            Some((0, 6))
        );
    }

    #[test]
    fn dispatch_hover_without_active_editor_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::Hover);
        vcx.run_until_parked();
    }

    #[test]
    fn dispatch_repeat_last_motion_replays_word_advance() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "foo bar baz");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::MoveNextWordStart);
        vcx.run_until_parked();
        assert_eq!(selection_offsets(vcx, &editor), vec![(0, 4)]);

        dispatch(&ws, vcx, stoat_action::RepeatLastMotion);
        vcx.run_until_parked();
        assert_eq!(selection_offsets(vcx, &editor), vec![(4, 8)]);
    }

    #[test]
    fn dispatch_repeat_last_motion_with_no_prior_motion_is_noop() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "foo");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::RepeatLastMotion);
        vcx.run_until_parked();

        assert_eq!(cursor_offsets(vcx, &editor), vec![0]);
    }

    #[test]
    fn dispatch_format_selections_applies_range_edits_from_lsp() {
        use crate::globals::{FsHostGlobal, FsWatchHostGlobal, LspHostGlobal};
        use stoat::host::{
            fake::{FakeLsp, FakeLspHost},
            LspHost,
        };
        use stoat_host::NoopFsWatcher;
        use stoat_scheduler::{Executor, TestScheduler};

        let mut cx = TestAppContext::single();
        let lsp = Arc::new(FakeLsp::new());
        lsp.set_capabilities(lsp_types::ServerCapabilities {
            document_formatting_provider: Some(lsp_types::OneOf::Left(true)),
            document_range_formatting_provider: Some(lsp_types::OneOf::Left(true)),
            ..Default::default()
        });
        lsp.set_range_formatting(
            "/repo/main.rs",
            vec![lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 1,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 4,
                    },
                },
                new_text: String::new(),
            }],
        );

        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/repo/main.rs", b"fn main() {\n    let x = 1;\n}\n");
        cx.update(|cx| {
            cx.set_global(FsHostGlobal(fs.clone() as Arc<dyn stoat::host::FsHost>));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn stoat::host::FsWatchHost>
            ));
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
            cx.set_global(LspHostGlobal(
                Arc::new(FakeLspHost::new(lsp.clone())) as Arc<dyn LspHost>
            ));
            cx.set_global(crate::globals::LanguageRegistry(
                stoat_language::LanguageRegistry::standard(),
            ));
        });
        let (ws, vcx) =
            cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/repo"), cx));
        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/repo/main.rs")], cx)
        });
        vcx.run_until_parked();

        let editor = ws
            .read_with(vcx, |w, cx| {
                w.buffer_for_path(Path::new("/repo/main.rs"), cx)
                    .and_then(|buffer| {
                        let target = buffer.entity_id();
                        let pane_tree = w.pane_tree().read(cx);
                        for pane_id in pane_tree.split_pane_ids() {
                            let pane = pane_tree.pane(pane_id)?;
                            for item in pane.read(cx).items() {
                                if let Ok(editor) = item.to_any_view().downcast::<Editor>() {
                                    let singleton = editor
                                        .read(cx)
                                        .multi_buffer()
                                        .read(cx)
                                        .as_singleton()
                                        .cloned();
                                    if singleton.as_ref().map(Entity::entity_id) == Some(target) {
                                        return Some(editor);
                                    }
                                }
                            }
                        }
                        None
                    })
            })
            .expect("editor for opened file");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::FormatSelections);
        vcx.run_until_parked();

        let text = editor.read_with(vcx, |ed, cx| {
            ed.multi_buffer().read(cx).snapshot().text().to_string()
        });
        assert_eq!(text, "fn main() {\nlet x = 1;\n}\n");
    }

    #[test]
    fn dispatch_switch_case_toggles_case_in_selection() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "Foo Bar");
        editor.update(vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = stoat_text::Selection {
                id: 600,
                start: snapshot.anchor_at(0, stoat_text::Bias::Left),
                end: snapshot.anchor_at(7, stoat_text::Bias::Right),
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![sel], &snapshot);
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::SwitchCase);
        vcx.run_until_parked();

        let text = editor.read_with(vcx, |ed, cx| {
            ed.multi_buffer().read(cx).snapshot().text().to_string()
        });
        assert_eq!(text, "fOO bAR");
    }

    #[test]
    fn dispatch_open_search_input_opens_regex_input_modal() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "foo bar foo");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::OpenSearchInput);
        vcx.run_until_parked();

        let active = ws.read_with(vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<crate::editor::regex_input_modal::RegexInputModal>()
                .is_some()
        });
        assert!(active, "OpenSearchInput should open the regex input modal");
    }

    #[test]
    fn dispatch_open_reverse_search_input_opens_regex_input_modal() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "foo bar foo");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::OpenReverseSearchInput);
        vcx.run_until_parked();

        let active = ws.read_with(vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<crate::editor::regex_input_modal::RegexInputModal>()
                .is_some()
        });
        assert!(
            active,
            "OpenReverseSearchInput should open the regex input modal"
        );
    }

    #[test]
    fn dispatch_open_search_input_confirm_sets_search_state_and_jumps() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "foo bar foo");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::OpenSearchInput);
        vcx.run_until_parked();

        let modal = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<crate::editor::regex_input_modal::RegexInputModal>()
            })
            .expect("modal opened");
        let input = modal.read_with(vcx, |m, _| m.input_editor_for_test());
        input.update(vcx, |ed, cx| ed.apply_text_to_all_cursors("foo", cx));
        vcx.run_until_parked();

        modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::PickerConfirm, window, cx);
        });
        vcx.run_until_parked();

        let state = editor.read_with(vcx, |ed, _| ed.search_state().cloned());
        let state = state.expect("search_state set after confirm");
        assert_eq!(state.query(), "foo");
        assert_eq!(
            state.direction(),
            crate::editor::search::SearchDirection::Forward
        );
        assert_eq!(cursor_offsets(vcx, &editor), vec![8]);
    }

    #[test]
    fn dispatch_goto_window_center_moves_cursor_to_viewport_midpoint() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let body = (0..30)
            .map(|i| format!("row{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let editor = new_singleton_editor(vcx, &body);
        editor.update(vcx, |ed, cx| {
            ed.set_cell_size(size(px(8.0), px(16.0)), cx);
            ed.set_text_region_bounds(
                Bounds {
                    origin: Point::new(px(0.0), px(0.0)),
                    size: size(px(160.0), px(160.0)),
                },
                cx,
            );
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::GotoWindowCenter);
        vcx.run_until_parked();

        // Buffer rows 0-9 = 5 bytes each (row0..row9 + \n).
        let expected = 5 * 5;
        assert_eq!(cursor_offsets(vcx, &editor), vec![expected]);
    }

    #[test]
    fn dispatch_scroll_down_advances_scroll_row_without_moving_cursor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let body = (0..30)
            .map(|i| format!("row{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let editor = new_singleton_editor(vcx, &body);
        editor.update(vcx, |ed, cx| {
            ed.set_cell_size(size(px(8.0), px(16.0)), cx);
            ed.set_text_region_bounds(
                Bounds {
                    origin: Point::new(px(0.0), px(0.0)),
                    size: size(px(160.0), px(160.0)),
                },
                cx,
            );
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::ScrollDown);
        vcx.run_until_parked();

        assert_eq!(editor.read_with(vcx, |ed, _| ed.scroll_row()), 1);
        assert_eq!(cursor_offsets(vcx, &editor), vec![0]);
    }

    #[test]
    fn dispatch_align_view_center_sets_scroll_row_to_centered_position() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let body = (0..30)
            .map(|i| format!("row{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let editor = new_singleton_editor(vcx, &body);
        editor.update(vcx, |ed, cx| {
            ed.set_cell_size(size(px(8.0), px(16.0)), cx);
            ed.set_text_region_bounds(
                Bounds {
                    origin: Point::new(px(0.0), px(0.0)),
                    size: size(px(160.0), px(160.0)),
                },
                cx,
            );
            ed.set_cursor_at_buffer_row(15, cx);
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::AlignViewCenter);
        vcx.run_until_parked();

        assert_eq!(editor.read_with(vcx, |ed, _| ed.scroll_row()), 10);
    }

    #[test]
    fn dispatch_match_brackets_jumps_to_matching_bracket() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "foo ( bar )");
        editor.update(vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let anchor = snapshot.anchor_at(4, stoat_text::Bias::Left);
            let sel = stoat_text::Selection {
                id: 800,
                start: anchor,
                end: anchor,
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![sel], &snapshot);
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::MatchBrackets);
        vcx.run_until_parked();

        assert_eq!(cursor_offsets(vcx, &editor), vec![10]);
    }

    #[test]
    fn dispatch_insert_register_then_apply_inserts_named_register_text() {
        use stoat::register::Register;

        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "bar");
        editor.update(vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = stoat_text::Selection {
                id: 700,
                start: snapshot.anchor_at(3, stoat_text::Bias::Left),
                end: snapshot.anchor_at(3, stoat_text::Bias::Left),
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![sel], &snapshot);
        });
        ws.update(vcx, |w, _| {
            w.registers_mut()
                .write(Register::Named('a'), "foo".to_string())
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::InsertRegister);
        vcx.run_until_parked();
        assert!(sm.read_with(vcx, |sm, _| sm.pending_insert_register()));

        dispatch(
            &ws,
            vcx,
            crate::actions::ApplyInsertRegisterChar { ch: 'a' },
        );
        vcx.run_until_parked();

        let text = editor.read_with(vcx, |ed, cx| {
            ed.multi_buffer().read(cx).snapshot().text().to_string()
        });
        assert_eq!(text, "barfoo");
    }

    #[test]
    fn dispatch_replace_char_then_apply_replaces_selection_chars() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "abcdef");
        editor.update(vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = stoat_text::Selection {
                id: 400,
                start: snapshot.anchor_at(0, stoat_text::Bias::Left),
                end: snapshot.anchor_at(3, stoat_text::Bias::Right),
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![sel], &snapshot);
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::ReplaceChar);
        vcx.run_until_parked();
        assert!(sm.read_with(vcx, |sm, _| sm.pending_replace()));

        dispatch(&ws, vcx, crate::actions::ApplyReplaceChar { ch: 'X' });
        vcx.run_until_parked();

        let text = editor.read_with(vcx, |ed, cx| {
            ed.multi_buffer().read(cx).snapshot().text().to_string()
        });
        assert_eq!(text, "XXXdef");
    }

    #[test]
    fn dispatch_open_below_inserts_blank_line_after_cursor_row_on_active_editor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "line0\nline1\n");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::OpenBelow);
        vcx.run_until_parked();

        let text = editor.read_with(vcx, |ed, cx| {
            ed.multi_buffer().read(cx).snapshot().text().to_string()
        });
        assert_eq!(text, "line0\n\nline1\n");
        assert_eq!(cursor_offsets(vcx, &editor), vec![6]);
    }

    #[test]
    fn schedule_hover_debounce_from_editor_listener_path_sets_hover_position_without_panic() {
        let mut cx = TestAppContext::single();
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        cx.update(|cx| cx.set_global(ExecutorGlobal(scheduler.executor())));
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        editor.update(vcx, |ed, cx| {
            ed.set_workspace(Some(ws.downgrade()));
            ed.set_cell_size(size(px(10.0), px(20.0)), cx);
            ed.set_text_region_bounds(
                Bounds {
                    origin: Point::default(),
                    size: size(px(800.0), px(600.0)),
                },
                cx,
            );
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        editor.update_in(vcx, |ed, window, cx| {
            ed.schedule_hover_debounce(3, 5, window, cx);
        });
        vcx.run_until_parked();
        scheduler.advance_clock(Duration::from_millis(60));
        vcx.run_until_parked();

        assert_eq!(
            editor.read_with(vcx, |ed, _| ed.hover_position()),
            Some((3, 5))
        );
    }

    #[test]
    fn dispatch_hover_at_without_active_editor_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        let before = pane_tree.read_with(vcx, |t, _| (t.pane_count(), t.focus()));

        dispatch(&ws, vcx, crate::actions::HoverAt { row: 1, col: 2 });
        vcx.run_until_parked();

        let after = pane_tree.read_with(vcx, |t, _| (t.pane_count(), t.focus()));
        assert_eq!(before, after);
    }

    #[test]
    fn dispatch_dismiss_modal_clears_active_editors_hover_position() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        editor.update(vcx, |ed, cx| ed.set_hover_position(Some((0, 3)), cx));
        vcx.run_until_parked();
        assert_eq!(
            editor.read_with(vcx, |ed, _| ed.hover_position()),
            Some((0, 3))
        );

        dispatch(&ws, vcx, stoat_action::DismissModal);
        vcx.run_until_parked();

        assert!(editor.read_with(vcx, |ed, _| ed.hover_position()).is_none());
    }

    #[test]
    fn dispatch_move_right_advances_cursor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::MoveRight);
        vcx.run_until_parked();

        assert_eq!(cursor_offsets(vcx, &editor), vec![1]);
    }

    #[test]
    fn dispatch_move_consumes_count_from_state_machine() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| {
            sm.set_active_editor(Some(editor.downgrade()));
            sm.set_consumed_count_for_test(Some(4));
        });

        dispatch(&ws, vcx, stoat_action::MoveRight);
        vcx.run_until_parked();

        assert_eq!(cursor_offsets(vcx, &editor), vec![4]);
        let leftover = sm.read_with(vcx, |sm, _| sm.consumed_count());
        assert_eq!(leftover, None);
    }

    #[test]
    fn dispatch_move_down_advances_display_row() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "abc\ndef\nghi");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::MoveDown);
        vcx.run_until_parked();

        let row = editor.update(vcx, |ed, cx| {
            let snapshot = ed.display_map().update(cx, |dm, _| dm.snapshot());
            let buffer_snap = ed.multi_buffer().read(cx).snapshot();
            let head_anchor = ed.selections().all_anchors()[0].head();
            let head_point = buffer_snap.point_for_anchor(&head_anchor);
            snapshot.buffer_to_display(head_point).row
        });
        assert_eq!(row, 1);
    }

    #[test]
    fn dispatch_move_without_active_editor_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        let before = pane_tree.read_with(vcx, |t, _| (t.pane_count(), t.focus()));

        dispatch(&ws, vcx, stoat_action::MoveRight);
        vcx.run_until_parked();

        let after = pane_tree.read_with(vcx, |t, _| (t.pane_count(), t.focus()));
        assert_eq!(before, after);
    }

    #[test]
    fn dispatch_move_next_word_start_selects_first_word() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "foo bar baz");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::MoveNextWordStart);
        vcx.run_until_parked();

        assert_eq!(selection_offsets(vcx, &editor), vec![(0, 4)]);
    }

    #[test]
    fn dispatch_goto_line_number_consumes_count() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "alpha\nbeta\ngamma");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| {
            sm.set_active_editor(Some(editor.downgrade()));
            sm.set_consumed_count_for_test(Some(2));
        });

        dispatch(&ws, vcx, stoat_action::GotoLineNumber);
        vcx.run_until_parked();

        assert_eq!(cursor_offsets(vcx, &editor), vec![6]);
    }

    #[test]
    fn dispatch_goto_line_number_without_count_jumps_to_last_line() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "alpha\nbeta\ngamma");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::GotoLineNumber);
        vcx.run_until_parked();

        assert_eq!(cursor_offsets(vcx, &editor), vec![11]);
    }

    fn goto_line_modal_active(ws: &Entity<Workspace>, vcx: &mut VisualTestContext) -> bool {
        ws.read_with(vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<crate::goto_line_modal::GotoLineModal>()
                .is_some()
        })
    }

    #[test]
    fn dispatch_open_goto_line_modal_opens_modal_over_active_editor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "alpha\nbeta\ngamma");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::OpenGotoLineModal);
        vcx.run_until_parked();

        assert!(goto_line_modal_active(&ws, vcx));
    }

    #[test]
    fn dispatch_open_goto_line_modal_without_active_editor_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::OpenGotoLineModal);
        vcx.run_until_parked();

        assert!(!goto_line_modal_active(&ws, vcx));
    }

    fn outline_dock_count(ws: &Entity<Workspace>, vcx: &mut VisualTestContext) -> usize {
        ws.read_with(vcx, |w, cx| {
            w.docks()
                .iter()
                .filter(|d| d.read(cx).item().item_kind(cx) == crate::item::ItemKind::OutlinePanel)
                .count()
        })
    }

    #[test]
    fn dispatch_toggle_outline_panel_adds_then_removes_right_dock() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::ToggleOutlinePanel);
        vcx.run_until_parked();
        assert_eq!(
            outline_dock_count(&ws, vcx),
            1,
            "toggle opens the outline dock"
        );

        dispatch(&ws, vcx, stoat_action::ToggleOutlinePanel);
        vcx.run_until_parked();
        assert_eq!(outline_dock_count(&ws, vcx), 0, "toggle again closes it");
    }

    #[test]
    fn dispatch_toggle_diagnostics_panel_adds_then_removes_right_dock() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let count = |ws: &Entity<Workspace>, vcx: &mut VisualTestContext| {
            ws.read_with(vcx, |w, cx| {
                w.docks()
                    .iter()
                    .filter(|d| {
                        d.read(cx).item().item_kind(cx) == crate::item::ItemKind::DiagnosticsPanel
                    })
                    .count()
            })
        };

        dispatch(&ws, vcx, stoat_action::ToggleDiagnosticsPanel);
        vcx.run_until_parked();
        assert_eq!(count(&ws, vcx), 1, "toggle opens the diagnostics dock");

        dispatch(&ws, vcx, stoat_action::ToggleDiagnosticsPanel);
        vcx.run_until_parked();
        assert_eq!(count(&ws, vcx), 0, "toggle again closes it");
    }

    #[test]
    fn dispatch_goto_line_start_jumps_to_column_zero() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        dispatch(&ws, vcx, crate::actions::ClickAt { row: 0, col: 6 });
        vcx.run_until_parked();
        assert_eq!(cursor_offsets(vcx, &editor), vec![6]);

        dispatch(&ws, vcx, stoat_action::GotoLineStart);
        vcx.run_until_parked();

        assert_eq!(cursor_offsets(vcx, &editor), vec![0]);
    }

    #[test]
    fn dispatch_move_word_consumes_count() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "foo bar baz qux");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| {
            sm.set_active_editor(Some(editor.downgrade()));
            sm.set_consumed_count_for_test(Some(2));
        });

        dispatch(&ws, vcx, stoat_action::MoveNextWordStart);
        vcx.run_until_parked();

        assert_eq!(selection_offsets(vcx, &editor), vec![(0, 8)]);
    }

    fn seed_primary_offset(vcx: &mut VisualTestContext, editor: &Entity<Editor>, offset: usize) {
        use stoat_text::{Bias, Selection, SelectionGoal};
        editor.update(vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let anchor = snapshot.anchor_at(offset, Bias::Left);
            ed.selections_mut().replace_with(
                vec![Selection {
                    id: 1,
                    start: anchor,
                    end: anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &snapshot,
            );
        });
    }

    #[test]
    fn dispatch_extend_right_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "abcdef");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 2);

        dispatch(&ws, vcx, stoat_action::ExtendRight);
        dispatch(&ws, vcx, stoat_action::ExtendRight);
        vcx.run_until_parked();

        assert_eq!(selection_offsets(vcx, &editor), vec![(2, 4)]);
    }

    #[test]
    fn dispatch_extend_left_walks_anchor_backward() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "abcdef");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 4);

        dispatch(&ws, vcx, stoat_action::ExtendLeft);
        dispatch(&ws, vcx, stoat_action::ExtendLeft);
        vcx.run_until_parked();

        let sel = selection_offsets(vcx, &editor);
        assert_eq!(sel, vec![(2, 4)]);
        let reversed = editor.read_with(vcx, |ed, _| ed.selections().all_anchors()[0].reversed);
        assert!(reversed);
    }

    #[test]
    fn dispatch_extend_down_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "abc\ndef\nghi");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 1);

        dispatch(&ws, vcx, stoat_action::ExtendDown);
        vcx.run_until_parked();

        let sel = selection_offsets(vcx, &editor);
        assert_eq!(sel, vec![(1, 5)]);
    }

    #[test]
    fn dispatch_extend_next_word_start_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "foo bar baz");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 1);

        dispatch(&ws, vcx, stoat_action::ExtendNextWordStart);
        vcx.run_until_parked();

        let sel = selection_offsets(vcx, &editor);
        assert_eq!(sel, vec![(1, 4)]);
    }

    #[test]
    fn dispatch_extend_to_line_end_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world\nnext");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 2);

        dispatch(&ws, vcx, stoat_action::ExtendToLineEnd);
        vcx.run_until_parked();

        let sel = selection_offsets(vcx, &editor);
        assert_eq!(sel, vec![(2, 11)]);
    }

    #[test]
    fn dispatch_extend_to_file_start_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "alpha\nbeta\ngamma");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 7);

        dispatch(&ws, vcx, stoat_action::ExtendToFileStart);
        vcx.run_until_parked();

        let sel = selection_offsets(vcx, &editor);
        assert_eq!(sel, vec![(0, 7)]);
        let reversed = editor.read_with(vcx, |ed, _| ed.selections().all_anchors()[0].reversed);
        assert!(reversed);
    }

    #[test]
    fn dispatch_extend_goto_first_nonwhitespace_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "  foo bar");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 0);

        dispatch(&ws, vcx, stoat_action::ExtendGotoFirstNonwhitespace);
        vcx.run_until_parked();

        assert_eq!(selection_offsets(vcx, &editor), vec![(0, 2)]);
    }

    #[test]
    fn dispatch_extend_goto_file_start_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "alpha\nbeta\ngamma");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 7);

        dispatch(&ws, vcx, stoat_action::ExtendGotoFileStart);
        vcx.run_until_parked();

        assert_eq!(selection_offsets(vcx, &editor), vec![(0, 7)]);
        let reversed = editor.read_with(vcx, |ed, _| ed.selections().all_anchors()[0].reversed);
        assert!(reversed);
    }

    #[test]
    fn dispatch_extend_goto_last_line_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "row0\nrow1\nrow2");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 0);

        dispatch(&ws, vcx, stoat_action::ExtendGotoLastLine);
        vcx.run_until_parked();

        assert_eq!(selection_offsets(vcx, &editor), vec![(0, 10)]);
    }

    #[test]
    fn dispatch_extend_goto_column_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 0);
        sm.update(vcx, |sm, _| sm.set_consumed_count_for_test(Some(5)));

        dispatch(&ws, vcx, stoat_action::ExtendGotoColumn);
        vcx.run_until_parked();

        assert_eq!(selection_offsets(vcx, &editor), vec![(0, 4)]);
    }

    #[test]
    fn dispatch_find_next_char_arms_chord_and_executes_on_char() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::FindNextChar);
        vcx.run_until_parked();
        sm.read_with(vcx, |sm, _| {
            assert!(
                sm.pending_find().is_some(),
                "chord armed after FindNextChar"
            )
        });

        vcx.simulate_keystrokes("o");

        assert_eq!(cursor_offsets(vcx, &editor), vec![4]);
        sm.read_with(vcx, |sm, _| assert!(sm.pending_find().is_none()));
    }

    #[test]
    fn record_macro_starts_and_stops_recording() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::RecordMacro);
        sm.read_with(vcx, |sm, _| {
            assert!(sm.macro_recording().is_some(), "recording armed");
        });

        dispatch(&ws, vcx, stoat_action::RecordMacro);
        sm.read_with(vcx, |sm, _| {
            assert!(sm.macro_recording().is_none(), "recording stopped");
            assert!(sm
                .macros()
                .contains_key(&stoat::register::Register::Unnamed));
        });
    }

    #[test]
    fn record_then_replay_repeats_movement() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::RecordMacro);
        vcx.simulate_keystrokes("l l l");
        dispatch(&ws, vcx, stoat_action::RecordMacro);
        assert_eq!(cursor_offsets(vcx, &editor), vec![3]);

        dispatch(&ws, vcx, stoat_action::ReplayMacro);
        sm.read_with(vcx, |sm, _| assert!(sm.pending_macro_replay()));
        vcx.simulate_keystrokes("\"");
        sm.read_with(vcx, |sm, _| assert!(!sm.pending_macro_replay()));
        assert_eq!(cursor_offsets(vcx, &editor), vec![6]);
    }

    #[test]
    fn record_macro_excludes_record_keystroke() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::RecordMacro);
        vcx.simulate_keystrokes("l");
        dispatch(&ws, vcx, stoat_action::RecordMacro);

        sm.read_with(vcx, |sm, _| {
            let stored = sm
                .macros()
                .get(&stoat::register::Register::Unnamed)
                .expect("macro stored");
            assert_eq!(
                stored.len(),
                1,
                "only the l keystroke captured, not the dispatched RecordMacro",
            );
        });
    }

    #[test]
    fn select_register_chord_sets_named_register() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::SelectRegister);
        sm.read_with(vcx, |sm, _| assert!(sm.pending_register_select()));
        vcx.simulate_keystrokes("a");
        sm.read_with(vcx, |sm, _| assert!(!sm.pending_register_select()));
        ws.read_with(vcx, |w, _| {
            // consume_selected_register is &mut so we re-read by calling once via update.
            let _ = w;
        });
        ws.update(vcx, |w, _| {
            assert_eq!(
                w.consume_selected_register(),
                stoat::register::Register::Named('a'),
            );
        });
    }

    #[test]
    fn replay_with_empty_register_is_noop() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 1);

        dispatch(&ws, vcx, stoat_action::ReplayMacro);
        vcx.simulate_keystrokes("a");
        sm.read_with(vcx, |sm, _| assert!(!sm.pending_macro_replay()));
        assert_eq!(cursor_offsets(vcx, &editor), vec![1]);
    }

    #[test]
    fn replay_chord_cleared_by_non_char_key() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::ReplayMacro);
        sm.read_with(vcx, |sm, _| assert!(sm.pending_macro_replay()));
        vcx.simulate_keystrokes("escape");
        sm.read_with(vcx, |sm, _| assert!(!sm.pending_macro_replay()));
    }

    #[test]
    fn increment_dispatch_adds_one_to_number_under_cursor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "42");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 0);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::Increment);
        vcx.run_until_parked();

        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "43");
    }

    #[test]
    fn decrement_dispatch_subtracts_one_from_number() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "100");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 1);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::Decrement);
        vcx.run_until_parked();

        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "99");
    }

    #[test]
    fn indent_selection_inserts_tab_on_touched_line() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 2);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::IndentSelection);
        vcx.run_until_parked();

        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "\thello");
    }

    #[test]
    fn unindent_selection_removes_leading_tab() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "\thello");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 2);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::UnindentSelection);
        vcx.run_until_parked();

        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "hello");
    }

    #[test]
    fn unindent_selection_consumes_one_space_group() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "    hello");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 4);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::UnindentSelection);
        vcx.run_until_parked();

        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "hello");
    }

    #[test]
    fn toggle_comments_inserts_prefix_for_rust_path() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        vcx.update(|_, cx| cx.set_global(crate::globals::LanguageRegistry::standard()));
        let editor = new_singleton_editor(vcx, "hello");
        editor.update(vcx, |ed, cx| {
            ed.set_file_path(Some(PathBuf::from("/tmp/repo/main.rs")), cx)
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 0);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::ToggleComments);
        vcx.run_until_parked();

        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "// hello");
    }

    #[test]
    fn toggle_comments_removes_prefix_when_present() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        vcx.update(|_, cx| cx.set_global(crate::globals::LanguageRegistry::standard()));
        let editor = new_singleton_editor(vcx, "// hello");
        editor.update(vcx, |ed, cx| {
            ed.set_file_path(Some(PathBuf::from("/tmp/repo/main.rs")), cx)
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 0);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::ToggleComments);
        vcx.run_until_parked();

        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "hello");
    }

    #[test]
    fn toggle_comments_without_language_global_is_noop() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello");
        editor.update(vcx, |ed, cx| {
            ed.set_file_path(Some(PathBuf::from("/tmp/repo/main.rs")), cx)
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 0);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::ToggleComments);
        vcx.run_until_parked();

        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "hello");
    }

    #[test]
    fn smart_tab_inserts_tab_at_column_zero() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 0);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::SmartTab);
        vcx.run_until_parked();

        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "\thello");
    }

    #[test]
    fn smart_tab_no_op_after_text_without_popup() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 3);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::SmartTab);
        vcx.run_until_parked();

        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "hello");
    }

    #[test]
    fn smart_tab_inserts_tab_after_existing_indent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "\thello");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 1);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::SmartTab);
        vcx.run_until_parked();

        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "\t\thello");
    }

    #[test]
    fn surround_add_chord_wraps_selection() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");
        editor.update(vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let start = snapshot.anchor_at(0, stoat_text::Bias::Right);
            let end = snapshot.anchor_at(5, stoat_text::Bias::Left);
            ed.selections_mut().replace_with(
                vec![stoat_text::Selection {
                    id: 1,
                    start,
                    end,
                    reversed: false,
                    goal: stoat_text::SelectionGoal::None,
                }],
                &snapshot,
            );
        });

        dispatch(&ws, vcx, stoat_action::SurroundAdd);
        sm.read_with(vcx, |sm, _| assert!(sm.pending_surround_add()));
        vcx.simulate_keystrokes("(");
        sm.read_with(vcx, |sm, _| assert!(!sm.pending_surround_add()));
        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "(hello) world");
    }

    #[test]
    fn surround_delete_chord_removes_pair() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "a(hello)b");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 4);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::SurroundDelete);
        sm.read_with(vcx, |sm, _| assert!(sm.pending_surround_delete()));
        vcx.simulate_keystrokes("(");
        sm.read_with(vcx, |sm, _| assert!(!sm.pending_surround_delete()));
        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "ahellob");
    }

    #[test]
    fn surround_replace_two_stage_chord_swaps_pair() {
        use stoat::action_handlers::surround::SurroundReplaceStage;
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "a(hello)b");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 4);

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton");

        dispatch(&ws, vcx, stoat_action::SurroundReplace);
        sm.read_with(vcx, |sm, _| {
            assert_eq!(
                sm.pending_surround_replace(),
                SurroundReplaceStage::AwaitFrom
            );
        });
        vcx.simulate_keystrokes("(");
        sm.read_with(vcx, |sm, _| {
            assert_eq!(
                sm.pending_surround_replace(),
                SurroundReplaceStage::AwaitTo('('),
            );
        });
        vcx.simulate_keystrokes("[");
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.pending_surround_replace(), SurroundReplaceStage::Idle);
        });
        assert_eq!(buffer.read_with(vcx, |b, _| b.text()), "a[hello]b");
    }

    #[test]
    fn surround_replace_non_char_clears_chord() {
        use stoat::action_handlers::surround::SurroundReplaceStage;
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "abc");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        dispatch(&ws, vcx, stoat_action::SurroundReplace);
        sm.read_with(vcx, |sm, _| {
            assert_eq!(
                sm.pending_surround_replace(),
                SurroundReplaceStage::AwaitFrom
            );
        });
        vcx.simulate_keystrokes("escape");
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.pending_surround_replace(), SurroundReplaceStage::Idle);
        });
    }

    #[test]
    fn dispatch_set_mark_then_goto_mark_round_trips() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "abcdef");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 4);

        dispatch(&ws, vcx, stoat_action::SetMark);
        vcx.simulate_keystrokes("a");

        seed_primary_offset(vcx, &editor, 0);
        dispatch(&ws, vcx, stoat_action::GotoMarkExact);
        vcx.simulate_keystrokes("a");

        assert_eq!(cursor_offsets(vcx, &editor), vec![4]);
        sm.read_with(vcx, |sm, _| assert!(sm.pending_mark().is_none()));
    }

    #[test]
    fn dispatch_jump_backward_returns_to_saved_position() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "abcdef");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 3);
        dispatch(&ws, vcx, stoat_action::SaveSelection);

        seed_primary_offset(vcx, &editor, 0);
        dispatch(&ws, vcx, stoat_action::JumpBackward);
        vcx.run_until_parked();

        assert_eq!(cursor_offsets(vcx, &editor), vec![3]);
    }

    #[test]
    fn dispatch_extend_till_prev_char_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "hello world");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 9);

        dispatch(&ws, vcx, stoat_action::ExtendTillPrevChar);
        vcx.simulate_keystrokes("h");

        let sel = selection_offsets(vcx, &editor);
        assert_eq!(sel, vec![(1, 9)]);
        let reversed = editor.read_with(vcx, |ed, _| ed.selections().all_anchors()[0].reversed);
        assert!(reversed);
    }

    #[test]
    fn dispatch_extend_to_last_line_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "alpha\nbeta\ngamma");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        seed_primary_offset(vcx, &editor, 2);

        dispatch(&ws, vcx, stoat_action::ExtendToLastLine);
        vcx.run_until_parked();

        let sel = selection_offsets(vcx, &editor);
        assert_eq!(sel, vec![(2, 11)]);
    }

    #[test]
    fn keystroke_routes_split_right_through_state_machine() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (config, errors) = stoat_config::parse("on key { s -> SplitRight(); }");
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let keymap = Keymap::compile(&config.expect("config"));
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_keymap(keymap));

        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        let window = vcx.window_handle();
        cx.simulate_keystrokes(window, "s");
        cx.run_until_parked();

        assert_eq!(pane_tree.read_with(&cx, |t, _| t.pane_count()), 2);
    }

    #[test]
    fn keystroke_sequence_dispatches_each_action_in_order() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (config, errors) = stoat_config::parse("on key { s -> [SplitRight(), SplitRight()]; }");
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let keymap = Keymap::compile(&config.expect("config"));
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_keymap(keymap));

        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        let window = vcx.window_handle();
        cx.simulate_keystrokes(window, "s");
        cx.run_until_parked();

        assert_eq!(pane_tree.read_with(&cx, |t, _| t.pane_count()), 3);
    }

    #[test]
    fn dispatch_action_routes_picker_select_next_to_active_modal() {
        use crate::{
            globals::ExecutorGlobal,
            picker::{Picker, PickerDelegate, PickerSecondary},
        };
        use gpui::{AnyElement, Task};
        use stoat_scheduler::{Executor, TestScheduler};

        struct StubDelegate {
            count: usize,
            selected: usize,
        }

        impl PickerDelegate for StubDelegate {
            fn match_count(&self) -> usize {
                self.count
            }
            fn selected_index(&self) -> usize {
                self.selected
            }
            fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
                self.selected = ix;
            }
            fn update_matches(
                &mut self,
                _query: String,
                _cx: &mut Context<'_, Picker<Self>>,
            ) -> Task<()> {
                Task::ready(())
            }
            fn confirm(
                &mut self,
                _secondary: Option<PickerSecondary>,
                _window: &mut Window,
                _cx: &mut Context<'_, Picker<Self>>,
            ) {
            }
            fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}
            fn render_match(
                &self,
                _ix: usize,
                _selected: bool,
                _cx: &mut Context<'_, Picker<Self>>,
            ) -> AnyElement {
                div().into_any_element()
            }
        }

        let mut cx = TestAppContext::single();
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
        });
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update_in(vcx, |w, window, cx| {
            w.toggle_modal::<Picker<StubDelegate>, _>(window, cx, |window, cx| {
                Picker::new(
                    StubDelegate {
                        count: 3,
                        selected: 0,
                    },
                    window,
                    cx,
                )
            });
        });
        vcx.run_until_parked();

        ws.update_in(vcx, |w, window, cx| {
            w.dispatch_action(Box::new(stoat_action::PickerSelectNext), window, cx);
        });

        let picker = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<StubDelegate>>()
            })
            .expect("picker active");
        assert_eq!(picker.read_with(vcx, |p, _| p.selected_index()), 1);
    }

    #[test]
    fn add_status_item_left_invokes_initial_callback_and_responds_to_pane_changes() {
        use crate::status_bar::StatusItemView;
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        struct Probe {
            observed: Arc<Mutex<Vec<usize>>>,
        }
        impl gpui::Render for Probe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut Context<'_, Self>,
            ) -> impl IntoElement {
                div().size_full()
            }
        }
        impl StatusItemView for Probe {
            fn set_active_pane_item(
                &mut self,
                _: Option<&dyn ItemHandle>,
                _cx: &mut Context<'_, Self>,
            ) {
                *self
                    .observed
                    .lock()
                    .expect("probe mutex")
                    .last_mut()
                    .unwrap_or(&mut 0) += 1;
                self.observed.lock().expect("probe mutex").push(0);
            }
        }

        let observed: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(vec![0]));
        let probe = {
            let observed = observed.clone();
            vcx.update(|_, cx| cx.new(|_| Probe { observed }))
        };
        ws.update(vcx, |w, cx| {
            w.add_status_item_left(probe.clone(), cx);
        });
        vcx.run_until_parked();
        assert!(
            observed.lock().expect("probe mutex").len() >= 2,
            "registration should fire initial set_active_pane_item",
        );

        let probe_calls_before = observed.lock().expect("probe mutex").len();
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        pane_tree.update(vcx, |tree, cx| {
            tree.split(Axis::Vertical, cx);
        });
        vcx.run_until_parked();

        let probe_calls_after = observed.lock().expect("probe mutex").len();
        assert!(
            probe_calls_after > probe_calls_before,
            "pane-tree change should re-fire set_active_pane_item",
        );
    }

    #[test]
    fn render_pane_tree_with_split_does_not_panic_with_border_styling() {
        use stoat::pane::Axis;
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        pane_tree.update(vcx, |tree, cx| {
            tree.split(Axis::Vertical, cx);
        });
        vcx.run_until_parked();
        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 2);
    }

    #[test]
    fn render_composes_docks_pane_area_and_modal_overlay_without_panic() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        let left_item = new_item(&mut vcx.cx, "outline");
        let right_item = new_item(&mut vcx.cx, "agent");
        ws.update(vcx, |w, cx| {
            w.add_dock(left_item, DockSide::Left, 200, cx);
            w.add_dock(right_item, DockSide::Right, 240, cx);
        });
        ws.update_in(vcx, |w, window, cx| {
            w.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::new(cx));
        });
        vcx.run_until_parked();

        ws.read_with(vcx, |w, cx| {
            assert_eq!(w.docks().len(), 2);
            assert!(w
                .modal_layer()
                .read(cx)
                .active_modal::<TestModal>()
                .is_some());
        });
    }

    fn install_globals_with_fs(cx: &mut TestAppContext, fs: Arc<stoat::host::FakeFs>) {
        install_globals_with_fs_and_watcher(cx, fs, Arc::new(stoat_host::FakeFsWatcher::new()));
    }

    fn install_globals_with_fs_and_watcher(
        cx: &mut TestAppContext,
        fs: Arc<stoat::host::FakeFs>,
        watcher: Arc<stoat_host::FakeFsWatcher>,
    ) {
        use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal};
        use stoat_scheduler::{Executor, TestScheduler};
        cx.update(|cx| {
            cx.set_global(FsHostGlobal(fs as Arc<dyn stoat::host::FsHost>));
            cx.set_global(FsWatchHostGlobal(
                watcher as Arc<dyn stoat::host::FsWatchHost>,
            ));
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
        });
    }

    #[test]
    fn open_paths_with_no_files_is_noop() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| w.open_paths(&[], cx));

        ws.read_with(vcx, |w, cx| {
            assert_eq!(w.buffer_registry().read(cx).len(), 0);
            assert_eq!(w.pane_tree().read(cx).pane_count(), 1);
        });
    }

    #[test]
    fn open_paths_single_file_opens_into_focused_pane() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hello stoat\n");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx)
        });

        ws.read_with(vcx, |w, cx| {
            assert_eq!(w.buffer_registry().read(cx).len(), 1);
            assert_eq!(w.pane_tree().read(cx).pane_count(), 1);
            let focus = w.pane_tree().read(cx).focus();
            let pane = w
                .pane_tree()
                .read(cx)
                .pane(focus)
                .expect("focused pane present")
                .read(cx);
            assert_eq!(pane.len(), 1);
            assert!(pane.active_item().is_some());
        });
    }

    #[test]
    fn open_paths_activates_the_newly_added_item() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"foo\n");
        fs.insert_file("/tmp/repo/bar.rs", b"bar\n");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx);
        });
        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/bar.rs")], cx);
        });

        ws.read_with(vcx, |w, cx| {
            let focus = w.pane_tree().read(cx).focus();
            let pane = w
                .pane_tree()
                .read(cx)
                .pane(focus)
                .expect("focused pane present")
                .read(cx);
            assert_eq!(pane.len(), 2);
            assert_eq!(
                pane.active_index(),
                1,
                "second open_paths must activate the just-added tab",
            );
        });
    }

    #[test]
    fn bump_lsp_goto_request_id_increments_and_records_latest() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");

        let first = ws.update(&mut cx, |w, _| w.bump_lsp_goto_request_id());
        let second = ws.update(&mut cx, |w, _| w.bump_lsp_goto_request_id());

        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(
            ws.read_with(&cx, |w, _| w.lsp_goto_request_id()),
            2,
            "lsp_goto_request_id must track the most recent bump",
        );
    }

    #[test]
    fn bump_lsp_rename_request_id_increments_and_records_latest() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");

        let first = ws.update(&mut cx, |w, _| w.bump_lsp_rename_request_id());
        let second = ws.update(&mut cx, |w, _| w.bump_lsp_rename_request_id());
        let third = ws.update(&mut cx, |w, _| w.bump_lsp_rename_request_id());

        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(third, 3);
        assert_eq!(
            ws.read_with(&cx, |w, _| w.lsp_rename_request_id()),
            3,
            "lsp_rename_request_id must track the most recent bump",
        );
    }

    #[test]
    fn opening_a_file_updates_active_file_label() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hello stoat\n");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx)
        });
        vcx.run_until_parked();

        let label = ws.read_with(vcx, |w, _| w.active_file_label().clone());
        let filename = label.read_with(vcx, |l, _| l.filename().cloned());
        assert_eq!(filename, Some(SharedString::from("foo.rs")));
    }

    #[test]
    fn pending_count_propagates_to_count_prefix() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let sm = ws.read_with(&cx, |w, _| w.input_state_machine().clone());
        let count_prefix = ws.read_with(&cx, |w, _| w.count_prefix().clone());

        let initial = count_prefix.read_with(&cx, |_, cx| sm.read(cx).pending_count());
        assert_eq!(initial, None);

        sm.update(&mut cx, |sm, cx| sm.set_pending_count_for_test(Some(7), cx));
        cx.run_until_parked();
        let after = count_prefix.read_with(&cx, |_, cx| sm.read(cx).pending_count());
        assert_eq!(after, Some(7));
    }

    #[test]
    fn opening_a_file_populates_cursor_position() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hello stoat\n");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx)
        });
        vcx.run_until_parked();

        let item = ws.read_with(vcx, |w, _| w.cursor_position().clone());
        let position = item.read_with(vcx, |c, _| c.position());
        assert_eq!(position, Some((1, 1)));
    }

    #[test]
    fn opening_a_file_without_diagnostics_leaves_badge_empty() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hello stoat\n");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx)
        });
        vcx.run_until_parked();

        let badge = ws.read_with(vcx, |w, _| w.diagnostics_badge().clone());
        badge.read_with(vcx, |b, _| assert!(b.summary().is_none()));
    }

    #[test]
    fn open_paths_multiple_files_split_per_extra_path() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/a.rs", b"a");
        fs.insert_file("/tmp/repo/b.rs", b"b");
        fs.insert_file("/tmp/repo/c.rs", b"c");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(
                &[
                    PathBuf::from("/tmp/repo/a.rs"),
                    PathBuf::from("/tmp/repo/b.rs"),
                    PathBuf::from("/tmp/repo/c.rs"),
                ],
                cx,
            )
        });

        ws.read_with(vcx, |w, cx| {
            assert_eq!(w.buffer_registry().read(cx).len(), 3);
            assert_eq!(w.pane_tree().read(cx).pane_count(), 3);
        });
    }

    #[test]
    fn open_paths_unreadable_path_opens_empty_buffer() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/new.rs")], cx)
        });

        ws.read_with(vcx, |w, cx| {
            assert_eq!(w.buffer_registry().read(cx).len(), 1);
            let id = w
                .buffer_registry()
                .read(cx)
                .id_for_path(Path::new("/tmp/repo/new.rs"))
                .expect("path registered under its absolute form");
            let shared = w
                .buffer_registry()
                .read(cx)
                .get(id)
                .expect("buffer present");
            assert_eq!(
                shared.read().expect("buffer lock").rope().to_string(),
                String::new()
            );
        });
    }

    #[test]
    fn open_paths_watches_each_opened_file() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hello");
        let watcher = Arc::new(stoat_host::FakeFsWatcher::new());
        install_globals_with_fs_and_watcher(&mut cx, fs, watcher.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx)
        });

        assert!(watcher.is_watching(Path::new("/tmp/repo/foo.rs")));
    }

    #[test]
    fn open_paths_registers_buffer_with_watcher_driver() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hello");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx)
        });

        ws.read_with(vcx, |w, cx| {
            assert_eq!(w.fs_watcher_driver.read(cx).tracked_count(), 1);
        });
    }

    #[test]
    fn open_paths_is_idempotent_for_known_path() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hello");
        let watcher = Arc::new(stoat_host::FakeFsWatcher::new());
        install_globals_with_fs_and_watcher(&mut cx, fs, watcher.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx);
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx);
        });

        assert_eq!(watcher.watched_paths().len(), 1);
        ws.read_with(vcx, |w, cx| {
            assert_eq!(w.fs_watch_tokens.len(), 1);
            let pane_id = w.pane_tree().read(cx).focus();
            let pane = w
                .pane_tree()
                .read(cx)
                .pane(pane_id)
                .expect("focused pane")
                .read(cx);
            assert_eq!(
                pane.len(),
                1,
                "second open_paths must not stack a duplicate tab"
            );
            assert_eq!(pane.active_index(), 0);
        });
    }

    #[test]
    fn removing_buffer_unwatches_and_untracks() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hello");
        let watcher = Arc::new(stoat_host::FakeFsWatcher::new());
        install_globals_with_fs_and_watcher(&mut cx, fs, watcher.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx)
        });
        assert!(watcher.is_watching(Path::new("/tmp/repo/foo.rs")));

        let buffer_id = ws.read_with(vcx, |w, cx| {
            w.buffer_registry()
                .read(cx)
                .id_for_path(Path::new("/tmp/repo/foo.rs"))
                .expect("path registered")
        });
        ws.update(vcx, |w, cx| {
            w.buffer_registry()
                .update(cx, |r, cx| r.remove(buffer_id, cx));
        });
        vcx.run_until_parked();

        assert!(!watcher.is_watching(Path::new("/tmp/repo/foo.rs")));
        ws.read_with(vcx, |w, cx| {
            assert_eq!(w.fs_watch_tokens.len(), 0);
            assert_eq!(w.fs_watcher_driver.read(cx).tracked_count(), 0);
        });
    }

    #[test]
    fn dispatch_save_buffer_writes_active_editor_buffer() {
        use stoat::host::FsHost;
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/save.rs", b"before");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/save.rs")], cx)
        });
        vcx.run_until_parked();

        let editor = ws.read_with(vcx, |w, cx| {
            let pane_id = w.pane_tree().read(cx).focus();
            let pane = w
                .pane_tree()
                .read(cx)
                .pane(pane_id)
                .expect("focused pane")
                .clone();
            pane.read(cx)
                .active_item()
                .expect("editor active in pane")
                .to_any_view()
                .downcast::<Editor>()
                .expect("active item is Editor")
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        let buffer_entity = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton buffer in editor");
        buffer_entity.update(vcx, |b, cx| b.edit(6..6, " after", cx));
        vcx.run_until_parked();
        assert!(buffer_entity.read_with(vcx, |b, _| b.is_dirty()));

        ws.update_in(vcx, |w, window, cx| {
            w.dispatch_action(Box::new(stoat_action::SaveBuffer), window, cx);
        });
        vcx.run_until_parked();

        assert!(!buffer_entity.read_with(vcx, |b, _| b.is_dirty()));
        let mut on_disk = Vec::new();
        (*fs)
            .read(Path::new("/tmp/repo/save.rs"), &mut on_disk)
            .expect("save wrote through");
        assert_eq!(String::from_utf8(on_disk).expect("utf8"), "before after");
    }

    #[test]
    fn pane_focus_change_broadcasts_active_editor_to_state_machine() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hello");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx)
        });

        let pane_a = ws.read_with(vcx, |w, cx| w.pane_tree().read(cx).focus());

        ws.update(vcx, |w, cx| {
            w.pane_tree()
                .update(cx, |tree, cx| tree.split(Axis::Horizontal, cx));
        });
        vcx.run_until_parked();

        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.read_with(vcx, |sm, _| {
            assert!(sm.active_editor().is_none());
            assert!(sm.editor_focus_target().is_none());
        });

        ws.update(vcx, |w, cx| {
            w.pane_tree()
                .update(cx, |tree, cx| tree.set_focus(pane_a, cx));
        });
        vcx.run_until_parked();

        let expected_editor = ws.read_with(vcx, |w, cx| {
            w.pane_tree()
                .read(cx)
                .pane(pane_a)
                .expect("pane a present")
                .read(cx)
                .active_item()
                .expect("editor active in pane a")
                .to_any_view()
                .downcast::<Editor>()
                .expect("active item is Editor")
        });
        let expected_handle =
            ws.read_with(vcx, |w, cx| w.editor_input.read(cx).focus_handle().clone());

        sm.read_with(vcx, |sm, _| {
            let active = sm.active_editor().expect("active editor set");
            assert_eq!(
                active.upgrade().expect("editor live").entity_id(),
                expected_editor.entity_id(),
            );
            assert_eq!(sm.editor_focus_target(), Some(&expected_handle));
        });
    }

    #[test]
    fn pane_focus_change_clears_state_machine_when_no_editor_active() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.pane_tree()
                .update(cx, |tree, cx| tree.split(Axis::Horizontal, cx));
        });
        vcx.run_until_parked();

        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.read_with(vcx, |sm, _| {
            assert!(sm.active_editor().is_none());
            assert!(sm.editor_focus_target().is_none());
        });
    }

    #[test]
    fn fresh_workspace_registers_mode_badge_as_left_status_item() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        ws.read_with(&cx, |w, cx| {
            let bar = w.status_bar().read(cx);
            assert!(bar.left_items()[0].to_any().downcast::<ModeBadge>().is_ok());
        });
    }

    #[test]
    fn window_title_falls_back_to_workspace_name_when_no_active_item() {
        let mut cx = TestAppContext::single();
        let (_ws, vcx) = new_workspace_in_window(&mut cx, "demo", "/tmp/repo");
        vcx.run_until_parked();
        assert_eq!(vcx.window_title().as_deref(), Some("demo"));
    }

    #[test]
    fn window_title_includes_active_item_tab_label() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "demo", "/tmp/repo");
        let item = new_item(&mut vcx.cx, "main.rs");
        ws.update(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            let pane = w
                .pane_tree
                .read(cx)
                .pane(focus)
                .expect("focused pane")
                .clone();
            pane.update(cx, |p, cx| {
                p.add_item(item, cx);
            });
        });
        vcx.run_until_parked();
        assert_eq!(vcx.window_title().as_deref(), Some("demo -- main.rs"));
    }

    #[test]
    fn window_title_appends_dirty_marker_when_active_item_is_dirty() {
        struct DirtyItem {
            label: SharedString,
        }
        impl Render for DirtyItem {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<'_, Self>,
            ) -> impl IntoElement {
                div().size_full()
            }
        }
        impl ItemView for DirtyItem {
            fn tab_label(&self, _cx: &App) -> SharedString {
                self.label.clone()
            }
            fn is_dirty(&self, _cx: &App) -> bool {
                true
            }
            fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
            where
                Self: Sized,
            {
                DeserializeSnafu {
                    reason: "DirtyItem is test-only",
                }
                .fail()
            }
        }

        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "demo", "/tmp/repo");
        let item: Box<dyn ItemHandle> = Box::new(vcx.update(|_, cx| {
            cx.new(|_| DirtyItem {
                label: SharedString::from("draft.txt"),
            })
        }));
        ws.update(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            let pane = w
                .pane_tree
                .read(cx)
                .pane(focus)
                .expect("focused pane")
                .clone();
            pane.update(cx, |p, cx| {
                p.add_item(item, cx);
            });
        });
        vcx.run_until_parked();
        assert_eq!(vcx.window_title().as_deref(), Some("demo -- draft.txt [+]"));
    }

    #[test]
    fn window_title_updates_when_active_item_changes() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "demo", "/tmp/repo");

        let item_a = new_item(&mut vcx.cx, "a.rs");
        let pane_a = ws.update(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            let pane = w
                .pane_tree
                .read(cx)
                .pane(focus)
                .expect("focused pane")
                .clone();
            pane.update(cx, |p, cx| {
                p.add_item(item_a, cx);
            });
            focus
        });
        let pane_b = ws.update(vcx, |w, cx| {
            w.pane_tree
                .update(cx, |tree, cx| tree.split(Axis::Vertical, cx))
        });
        let item_b = new_item(&mut vcx.cx, "b.rs");
        ws.update(vcx, |w, cx| {
            let pane = w
                .pane_tree
                .read(cx)
                .pane(pane_b)
                .expect("split pane")
                .clone();
            pane.update(cx, |p, cx| {
                p.add_item(item_b, cx);
            });
        });
        vcx.run_until_parked();
        assert_eq!(vcx.window_title().as_deref(), Some("demo -- b.rs"));

        ws.update(vcx, |w, cx| {
            w.pane_tree
                .update(cx, |tree, cx| tree.set_focus(pane_a, cx));
        });
        vcx.run_until_parked();
        assert_eq!(vcx.window_title().as_deref(), Some("demo -- a.rs"));
    }

    #[test]
    fn window_title_updates_when_active_editor_buffer_dirties() {
        use std::ops::Range;
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hello");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "demo", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx)
        });
        vcx.run_until_parked();
        assert_eq!(vcx.window_title().as_deref(), Some("demo -- foo.rs"));

        let editor = ws.read_with(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            w.pane_tree
                .read(cx)
                .pane(focus)
                .expect("focused pane")
                .read(cx)
                .active_item()
                .expect("active item")
                .to_any_view()
                .downcast::<Editor>()
                .expect("active item is Editor")
        });
        let buffer = editor.read_with(vcx, |ed, cx| {
            ed.multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("singleton")
                .clone()
        });
        buffer.update(vcx, |b, cx| {
            b.edit(Range { start: 5, end: 5 }, " world", cx);
        });
        vcx.run_until_parked();
        assert_eq!(vcx.window_title().as_deref(), Some("demo -- foo.rs [+]"));
    }

    #[test]
    fn open_paths_resolves_relative_paths_against_cwd() {
        let mut cx = TestAppContext::single();
        let cwd = std::env::current_dir().expect("cwd");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        let abs = cwd.join("relative.rs");
        fs.insert_file(&abs, b"relative");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("relative.rs")], cx)
        });

        ws.read_with(vcx, |w, cx| {
            let registered = w.buffer_registry().read(cx).id_for_path(&abs);
            assert!(
                registered.is_some(),
                "relative path should be registered under its absolute form"
            );
        });
    }

    fn new_two_chunk_review_session(
        vcx: &mut VisualTestContext,
    ) -> Entity<crate::review_session::ReviewSession> {
        vcx.update(|_, cx| {
            cx.new(|_| {
                let mut inner = stoat::review_session::ReviewSession::new(ReviewSource::InMemory {
                    files: Arc::new(Vec::new()),
                });
                let base = (0..14)
                    .map(|i| format!("L{i}"))
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n";
                let buffer = {
                    let mut lines: Vec<String> = (0..14).map(|i| format!("L{i}")).collect();
                    lines[0] = "L0_NEW".to_string();
                    lines[13] = "L13_NEW".to_string();
                    lines.join("\n") + "\n"
                };
                inner.add_files(vec![ReviewFileInput {
                    path: PathBuf::from("a.txt"),
                    rel_path: "a.txt".to_string(),
                    language: None,
                    base_text: Arc::new(base),
                    buffer_text: Arc::new(buffer),
                }]);
                assert_eq!(
                    inner.order.len(),
                    2,
                    "test fixture requires a two-chunk diff; \
                     got {} chunks",
                    inner.order.len(),
                );
                crate::review_session::ReviewSession::new(inner)
            })
        })
    }

    fn open_review_item_in_focused_pane(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
        session: Entity<crate::review_session::ReviewSession>,
    ) -> Entity<crate::review_item::ReviewItem> {
        let registry = ws.read_with(vcx, |w, _| w.buffer_registry().clone());
        let item = vcx.update(|_, cx| {
            cx.new(|cx| crate::review_item::ReviewItem::from_session(session, &registry, cx))
        });
        ws.update(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            let pane = w
                .pane_tree
                .read(cx)
                .pane(focus)
                .expect("focused pane")
                .clone();
            let handle: Box<dyn ItemHandle> = Box::new(item.clone());
            pane.update(cx, |p, cx| {
                p.add_item(handle, cx);
            });
        });
        vcx.run_until_parked();
        item
    }

    fn session_cursor(
        vcx: &mut VisualTestContext,
        session: &Entity<crate::review_session::ReviewSession>,
    ) -> Option<usize> {
        session.read_with(vcx, |s, _| {
            let inner = s.inner();
            inner
                .cursor
                .current
                .and_then(|id| inner.order.iter().position(|x| *x == id))
        })
    }

    fn editor_for_file(
        vcx: &mut VisualTestContext,
        review_item: &Entity<crate::review_item::ReviewItem>,
        file_index: usize,
    ) -> Entity<Editor> {
        review_item.read_with(vcx, |item, _| item.files()[file_index].editor.clone())
    }

    #[test]
    fn dispatch_review_next_chunk_advances_session_cursor_and_moves_editor_cursor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        assert_eq!(session_cursor(vcx, &session), Some(0), "starts at chunk 0");
        let target_row = session.read_with(vcx, |s, _| {
            let inner = s.inner();
            let id = inner.order[1];
            inner.chunks[&id].buffer_line_range.start
        });

        dispatch(&ws, vcx, stoat_action::ReviewNextChunk);
        vcx.run_until_parked();

        assert_eq!(
            session_cursor(vcx, &session),
            Some(1),
            "cursor advanced to chunk 1"
        );
        let editor = editor_for_file(vcx, &item, 0);
        let cursor_row = editor_cursor_buffer_row(vcx, &editor);
        assert_eq!(cursor_row, target_row);
    }

    #[test]
    fn dispatch_review_next_chunk_at_last_chunk_is_noop() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        session.update(vcx, |s, cx| {
            s.next(cx);
        });
        vcx.run_until_parked();
        assert_eq!(session_cursor(vcx, &session), Some(1));
        let editor = editor_for_file(vcx, &item, 0);
        let cursor_before = editor_cursor_buffer_row(vcx, &editor);

        dispatch(&ws, vcx, stoat_action::ReviewNextChunk);
        vcx.run_until_parked();

        assert_eq!(session_cursor(vcx, &session), Some(1), "cursor stays put");
        let cursor_after = editor_cursor_buffer_row(vcx, &editor);
        assert_eq!(
            cursor_after, cursor_before,
            "editor cursor untouched when clamping"
        );
    }

    #[test]
    fn dispatch_review_prev_chunk_steps_backward_and_moves_editor_cursor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        session.update(vcx, |s, cx| {
            s.next(cx);
        });
        vcx.run_until_parked();
        assert_eq!(session_cursor(vcx, &session), Some(1));
        let target_row = session.read_with(vcx, |s, _| {
            let inner = s.inner();
            let id = inner.order[0];
            inner.chunks[&id].buffer_line_range.start
        });

        dispatch(&ws, vcx, stoat_action::ReviewPrevChunk);
        vcx.run_until_parked();

        assert_eq!(session_cursor(vcx, &session), Some(0));
        let editor = editor_for_file(vcx, &item, 0);
        let cursor_row = editor_cursor_buffer_row(vcx, &editor);
        assert_eq!(cursor_row, target_row);
    }

    #[test]
    fn dispatch_review_prev_chunk_at_first_chunk_is_noop() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        assert_eq!(session_cursor(vcx, &session), Some(0));

        dispatch(&ws, vcx, stoat_action::ReviewPrevChunk);
        vcx.run_until_parked();

        assert_eq!(session_cursor(vcx, &session), Some(0));
    }

    #[test]
    fn dispatch_review_next_chunk_without_review_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        // Active pane has no item; dispatch must not panic.

        dispatch(&ws, vcx, stoat_action::ReviewNextChunk);
        vcx.run_until_parked();
    }

    #[test]
    fn dispatch_review_next_chunk_across_files_targets_new_file_editor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = vcx.update(|_, cx| {
            cx.new(|_| {
                let mut inner = stoat::review_session::ReviewSession::new(ReviewSource::InMemory {
                    files: Arc::new(Vec::new()),
                });
                inner.add_files(vec![
                    ReviewFileInput {
                        path: PathBuf::from("a.txt"),
                        rel_path: "a.txt".to_string(),
                        language: None,
                        base_text: Arc::new("a_old\n".to_string()),
                        buffer_text: Arc::new("a_new\n".to_string()),
                    },
                    ReviewFileInput {
                        path: PathBuf::from("b.txt"),
                        rel_path: "b.txt".to_string(),
                        language: None,
                        base_text: Arc::new("b_old\n".to_string()),
                        buffer_text: Arc::new("b_new\n".to_string()),
                    },
                ]);
                assert_eq!(inner.order.len(), 2, "one chunk per file expected");
                crate::review_session::ReviewSession::new(inner)
            })
        });
        let item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        assert_eq!(session_cursor(vcx, &session), Some(0));
        let editor_a = editor_for_file(vcx, &item, 0);
        let editor_b = editor_for_file(vcx, &item, 1);
        let cursor_a_before = editor_cursor_buffer_row(vcx, &editor_a);
        let cursor_b_before = editor_cursor_buffer_row(vcx, &editor_b);
        assert_eq!(cursor_b_before, 0, "fixture starts editor b at row 0");

        dispatch(&ws, vcx, stoat_action::ReviewNextChunk);
        vcx.run_until_parked();

        assert_eq!(session_cursor(vcx, &session), Some(1));
        let target_row_b = session.read_with(vcx, |s, _| {
            let inner = s.inner();
            inner.chunks[&inner.order[1]].buffer_line_range.start
        });
        assert_eq!(
            editor_cursor_buffer_row(vcx, &editor_b),
            target_row_b,
            "the new file's editor receives the cursor"
        );
        assert_eq!(
            editor_cursor_buffer_row(vcx, &editor_a),
            cursor_a_before,
            "the previously-active file's editor is not retargeted"
        );
    }

    fn editor_cursor_buffer_row(vcx: &mut VisualTestContext, editor: &Entity<Editor>) -> u32 {
        editor.update(vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let offset = snapshot.resolve_anchor(&ed.selections().all_anchors()[0].start);
            snapshot.rope().offset_to_point(offset).row
        })
    }

    fn cursor_chunk_status(
        vcx: &mut VisualTestContext,
        session: &Entity<crate::review_session::ReviewSession>,
    ) -> Option<ChunkStatus> {
        session.read_with(vcx, |s, _| {
            let inner = s.inner();
            let id = inner.cursor.current?;
            Some(inner.chunks[&id].status)
        })
    }

    #[test]
    fn dispatch_review_stage_chunk_sets_cursor_chunk_to_staged() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::Pending)
        );

        dispatch(&ws, vcx, stoat_action::ReviewStageChunk);
        vcx.run_until_parked();

        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::Staged)
        );
    }

    #[test]
    fn dispatch_review_unstage_chunk_sets_cursor_chunk_to_unstaged() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        dispatch(&ws, vcx, stoat_action::ReviewUnstageChunk);
        vcx.run_until_parked();

        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::Unstaged)
        );
    }

    #[test]
    fn dispatch_review_skip_chunk_sets_cursor_chunk_to_skipped() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        dispatch(&ws, vcx, stoat_action::ReviewSkipChunk);
        vcx.run_until_parked();

        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::Skipped)
        );
    }

    #[test]
    fn dispatch_review_toggle_stage_flips_between_staged_and_unstaged() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        dispatch(&ws, vcx, stoat_action::ReviewToggleStage);
        vcx.run_until_parked();
        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::Staged),
            "pending -> staged on first toggle",
        );

        dispatch(&ws, vcx, stoat_action::ReviewToggleStage);
        vcx.run_until_parked();
        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::Unstaged),
            "staged -> unstaged on second toggle",
        );

        dispatch(&ws, vcx, stoat_action::ReviewToggleStage);
        vcx.run_until_parked();
        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::Staged),
            "unstaged -> staged on third toggle",
        );
    }

    #[test]
    fn dispatch_review_toggle_stage_from_skipped_promotes_to_staged() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        dispatch(&ws, vcx, stoat_action::ReviewSkipChunk);
        vcx.run_until_parked();
        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::Skipped)
        );

        dispatch(&ws, vcx, stoat_action::ReviewToggleStage);
        vcx.run_until_parked();

        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::Staged)
        );
    }

    fn cursor_chunk_approved(
        vcx: &mut VisualTestContext,
        session: &Entity<crate::review_session::ReviewSession>,
    ) -> Option<bool> {
        session.read_with(vcx, |s, _| {
            let inner = s.inner();
            let id = inner.cursor.current?;
            Some(inner.chunks[&id].approved)
        })
    }

    fn cursor_chunk_index(
        vcx: &mut VisualTestContext,
        session: &Entity<crate::review_session::ReviewSession>,
    ) -> Option<usize> {
        session.read_with(vcx, |s, _| {
            let inner = s.inner();
            let id = inner.cursor.current?;
            inner.order.iter().position(|x| *x == id)
        })
    }

    #[test]
    fn dispatch_review_approve_hunk_sets_approved_and_advances_cursor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        assert_eq!(cursor_chunk_approved(vcx, &session), Some(false));
        assert_eq!(cursor_chunk_index(vcx, &session), Some(0));

        dispatch(&ws, vcx, stoat_action::ReviewApproveHunk);
        vcx.run_until_parked();

        let first = session.read_with(vcx, |s, _| s.inner().order[0]);
        let first_approved = session.read_with(vcx, |s, _| s.inner().chunks[&first].approved);
        assert!(first_approved, "first chunk approved after dispatch");
        assert_eq!(
            cursor_chunk_index(vcx, &session),
            Some(1),
            "cursor advanced to next chunk",
        );
    }

    #[test]
    fn dispatch_review_toggle_approval_flips_without_moving_cursor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        assert_eq!(cursor_chunk_approved(vcx, &session), Some(false));
        assert_eq!(cursor_chunk_index(vcx, &session), Some(0));

        dispatch(&ws, vcx, stoat_action::ReviewToggleApproval);
        vcx.run_until_parked();
        assert_eq!(cursor_chunk_approved(vcx, &session), Some(true));
        assert_eq!(cursor_chunk_index(vcx, &session), Some(0));

        dispatch(&ws, vcx, stoat_action::ReviewToggleApproval);
        vcx.run_until_parked();
        assert_eq!(cursor_chunk_approved(vcx, &session), Some(false));
        assert_eq!(cursor_chunk_index(vcx, &session), Some(0));
    }

    #[test]
    fn dispatch_review_next_unreviewed_advances_to_next_unapproved() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        assert_eq!(cursor_chunk_index(vcx, &session), Some(0));

        dispatch(&ws, vcx, stoat_action::ReviewNextUnreviewedHunk);
        vcx.run_until_parked();
        assert_eq!(
            cursor_chunk_index(vcx, &session),
            Some(1),
            "cursor advanced to next unapproved chunk",
        );
    }

    #[test]
    fn dispatch_review_next_unreviewed_wraps_past_end() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        dispatch(&ws, vcx, stoat_action::ReviewNextChunk);
        vcx.run_until_parked();
        assert_eq!(
            cursor_chunk_index(vcx, &session),
            Some(1),
            "cursor seeded at last chunk",
        );

        dispatch(&ws, vcx, stoat_action::ReviewNextUnreviewedHunk);
        vcx.run_until_parked();
        assert_eq!(
            cursor_chunk_index(vcx, &session),
            Some(0),
            "wraps back to first unapproved chunk",
        );
    }

    #[test]
    fn dispatch_review_reset_progress_clears_state_and_resets_cursor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        let ids = session.read_with(vcx, |s, _| s.inner().order.clone());
        session.update(vcx, |s, cx| {
            for id in &ids {
                s.set_status(*id, ChunkStatus::Staged, cx);
                s.set_approved(*id, true, cx);
            }
        });
        dispatch(&ws, vcx, stoat_action::ReviewNextChunk);
        vcx.run_until_parked();
        assert_eq!(
            cursor_chunk_index(vcx, &session),
            Some(1),
            "cursor advanced before reset",
        );

        dispatch(&ws, vcx, stoat_action::ReviewResetProgress);
        vcx.run_until_parked();

        session.read_with(vcx, |s, _| {
            for id in &ids {
                assert_eq!(s.inner().chunks[id].status, ChunkStatus::Pending);
                assert!(!s.inner().chunks[id].approved);
            }
        });
        assert_eq!(
            cursor_chunk_index(vcx, &session),
            Some(0),
            "cursor snaps back to first chunk",
        );
    }

    #[test]
    fn dispatch_review_reset_progress_without_review_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        dispatch(&ws, vcx, stoat_action::ReviewResetProgress);
        vcx.run_until_parked();
    }

    #[test]
    fn dispatch_review_next_unreviewed_no_op_when_all_approved() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        let ids = session.read_with(vcx, |s, _| s.inner().order.clone());
        session.update(vcx, |s, cx| {
            for id in &ids {
                s.set_approved(*id, true, cx);
            }
        });
        let cursor_before = cursor_chunk_index(vcx, &session);

        dispatch(&ws, vcx, stoat_action::ReviewNextUnreviewedHunk);
        vcx.run_until_parked();
        assert_eq!(
            cursor_chunk_index(vcx, &session),
            cursor_before,
            "cursor unchanged when every chunk is approved",
        );
    }

    #[test]
    fn dispatch_review_approve_hunk_without_review_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::ReviewApproveHunk);
        vcx.run_until_parked();
    }

    #[test]
    fn dispatch_review_stage_chunk_without_review_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::ReviewStageChunk);
        vcx.run_until_parked();
    }

    #[test]
    fn dispatch_review_stage_chunk_with_no_session_cursor_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = vcx.update(|_, cx| {
            cx.new(|_| {
                let inner = stoat::review_session::ReviewSession::new(ReviewSource::InMemory {
                    files: Arc::new(Vec::new()),
                });
                crate::review_session::ReviewSession::new(inner)
            })
        });
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        assert_eq!(
            cursor_chunk_status(vcx, &session),
            None,
            "empty session has no cursor"
        );

        dispatch(&ws, vcx, stoat_action::ReviewStageChunk);
        vcx.run_until_parked();

        assert_eq!(cursor_chunk_status(vcx, &session), None);
    }

    fn new_two_chunk_working_tree_session(
        vcx: &mut VisualTestContext,
        workdir: &str,
        rel_path: &str,
    ) -> (Entity<crate::review_session::ReviewSession>, String, String) {
        let base = (0..14)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let buffer = {
            let mut lines: Vec<String> = (0..14).map(|i| format!("L{i}")).collect();
            lines[0] = "L0_NEW".to_string();
            lines[13] = "L13_NEW".to_string();
            lines.join("\n") + "\n"
        };
        let base_clone = base.clone();
        let buffer_clone = buffer.clone();
        let session = vcx.update(|_, cx| {
            cx.new(|_| {
                let mut inner =
                    stoat::review_session::ReviewSession::new(ReviewSource::WorkingTree {
                        workdir: PathBuf::from(workdir),
                    });
                inner.add_files(vec![ReviewFileInput {
                    path: PathBuf::from(workdir).join(rel_path),
                    rel_path: rel_path.to_string(),
                    language: None,
                    base_text: Arc::new(base_clone),
                    buffer_text: Arc::new(buffer_clone),
                }]);
                assert_eq!(
                    inner.order.len(),
                    2,
                    "test fixture requires a two-chunk diff; got {} chunks",
                    inner.order.len(),
                );
                crate::review_session::ReviewSession::new(inner)
            })
        });
        (session, base, buffer)
    }

    fn buffer_text_at_file(
        vcx: &mut VisualTestContext,
        item: &Entity<crate::review_item::ReviewItem>,
        file_index: usize,
    ) -> String {
        let buffer = item.read_with(vcx, |item, _| item.files()[file_index].buffer.clone());
        buffer.read_with(vcx, |b, _| b.text())
    }

    #[test]
    fn dispatch_review_remove_selected_reverts_cursor_chunk_in_working_tree() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (session, base, buffer) = new_two_chunk_working_tree_session(vcx, "/tmp/repo", "a.txt");
        let item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        assert_eq!(buffer_text_at_file(vcx, &item, 0), buffer);

        let expected = session.read_with(vcx, |s, _| {
            let inner = s.inner();
            let chunk = &inner.chunks[&inner.cursor.current.expect("cursor set on add_files")];
            remove_chunks_from_buffer(&base, &buffer, &[chunk])
        });

        dispatch(&ws, vcx, stoat_action::ReviewRemoveSelected);
        vcx.run_until_parked();

        assert_eq!(buffer_text_at_file(vcx, &item, 0), expected);
        assert_ne!(
            buffer_text_at_file(vcx, &item, 0),
            buffer,
            "buffer must differ from pre-revert state",
        );
    }

    #[test]
    fn dispatch_review_remove_selected_is_noop_for_in_memory_source() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        let before = buffer_text_at_file(vcx, &item, 0);

        dispatch(&ws, vcx, stoat_action::ReviewRemoveSelected);
        vcx.run_until_parked();

        assert_eq!(
            buffer_text_at_file(vcx, &item, 0),
            before,
            "non-WorkingTree source must leave the buffer untouched",
        );
    }

    #[test]
    fn dispatch_review_remove_selected_without_review_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::ReviewRemoveSelected);
        vcx.run_until_parked();
    }

    #[test]
    fn dispatch_review_remove_selected_with_no_session_cursor_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = vcx.update(|_, cx| {
            cx.new(|_| {
                let inner = stoat::review_session::ReviewSession::new(ReviewSource::WorkingTree {
                    workdir: PathBuf::from("/tmp/repo"),
                });
                crate::review_session::ReviewSession::new(inner)
            })
        });
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        dispatch(&ws, vcx, stoat_action::ReviewRemoveSelected);
        vcx.run_until_parked();
    }

    fn install_git_host_global(vcx: &mut VisualTestContext, git: Arc<stoat::host::fake::FakeGit>) {
        use crate::globals::GitHostGlobal;
        vcx.update(|_, cx| {
            cx.set_global(GitHostGlobal(git as Arc<dyn stoat::host::GitHost>));
        });
    }

    fn session_apply_result(
        vcx: &mut VisualTestContext,
        session: &Entity<crate::review_session::ReviewSession>,
    ) -> Option<ReviewApplyResult> {
        session.read_with(vcx, |s, _| s.last_apply_result().cloned())
    }

    fn stage_all(
        vcx: &mut VisualTestContext,
        session: &Entity<crate::review_session::ReviewSession>,
    ) {
        let ids: Vec<_> = session.read_with(vcx, |s, _| s.inner().order.clone());
        session.update(vcx, |s, cx| {
            for id in ids {
                s.set_status(id, ChunkStatus::Staged, cx);
            }
        });
    }

    #[test]
    fn dispatch_git_toggle_stage_hunk_stages_then_unstages_via_index() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo");
        install_git_host_global(vcx, git.clone());
        let (session, _, _) = new_two_chunk_working_tree_session(vcx, "/tmp/repo", "a.txt");
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        dispatch(&ws, vcx, stoat_action::GitToggleStageHunk);
        vcx.run_until_parked();
        let staged = git.applied_patches(Path::new("/tmp/repo"));
        assert_eq!(staged.len(), 1, "stage applies the forward patch once");
        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::Staged)
        );

        dispatch(&ws, vcx, stoat_action::GitToggleStageHunk);
        vcx.run_until_parked();
        let patches = git.applied_patches(Path::new("/tmp/repo"));
        assert_eq!(patches.len(), 2, "second toggle applies the reversed patch");
        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::Pending)
        );

        // The unstage patch is the exact inverse of the stage patch.
        let body = |patch: &str, prefix: char| -> Vec<String> {
            patch
                .lines()
                .filter(|l| l.starts_with(prefix) && !l.starts_with("---") && !l.starts_with("+++"))
                .map(|l| l[1..].to_string())
                .collect()
        };
        assert_eq!(body(&patches[0], '+'), body(&patches[1], '-'));
        assert_eq!(body(&patches[0], '-'), body(&patches[1], '+'));
    }

    #[test]
    fn dispatch_git_stage_line_targets_the_cursor_row() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo");
        install_git_host_global(vcx, git.clone());

        // One chunk with three adjacent changed lines (L1/L2/L3 -> M1/M2/M3).
        let base = "a\nL1\nL2\nL3\nz\n";
        let buffer = "a\nM1\nM2\nM3\nz\n";
        let session = vcx.update(|_, cx| {
            cx.new(|_| {
                let mut inner =
                    stoat::review_session::ReviewSession::new(ReviewSource::WorkingTree {
                        workdir: PathBuf::from("/tmp/repo"),
                    });
                inner.add_files(vec![ReviewFileInput {
                    path: PathBuf::from("/tmp/repo/a.txt"),
                    rel_path: "a.txt".to_string(),
                    language: None,
                    base_text: Arc::new(base.to_string()),
                    buffer_text: Arc::new(buffer.to_string()),
                }]);
                assert_eq!(inner.order.len(), 1, "fixture must be a single chunk");
                crate::review_session::ReviewSession::new(inner)
            })
        });
        let item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        // Park the editor cursor on the middle changed line (M2, buffer row 2).
        let editor = item.read_with(vcx, |item, _| item.files()[0].editor.clone());
        editor.update(vcx, |ed, cx| ed.set_cursor_at_buffer_row(2, cx));

        dispatch(&ws, vcx, stoat_action::GitToggleStageLine);
        vcx.run_until_parked();

        let patches = git.applied_patches(Path::new("/tmp/repo"));
        assert_eq!(patches.len(), 1, "one line stages one patch: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-L2\n") && patch.contains("+M2\n"),
            "must stage the line under the cursor (L2 -> M2), not the first: {patch}"
        );
        assert!(
            !patch.contains("-L1\n") && !patch.contains("-L3\n"),
            "must not stage the other changed lines: {patch}"
        );
        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::PartiallyStaged)
        );
    }

    #[test]
    fn dispatch_git_unstage_hunk_forces_unstage_on_pending_chunk() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo");
        install_git_host_global(vcx, git.clone());
        let (session, _, _) = new_two_chunk_working_tree_session(vcx, "/tmp/repo", "a.txt");
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        dispatch(&ws, vcx, stoat_action::GitUnstageHunk);
        vcx.run_until_parked();
        assert_eq!(git.applied_patches(Path::new("/tmp/repo")).len(), 1);
        assert_eq!(
            cursor_chunk_status(vcx, &session),
            Some(ChunkStatus::Pending),
            "GitUnstageHunk forces the unstage path and never stages a pending chunk",
        );
    }

    #[test]
    fn dispatch_review_apply_staged_applies_each_staged_chunk_via_git_host() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo");
        install_git_host_global(vcx, git.clone());
        let (session, _, _) = new_two_chunk_working_tree_session(vcx, "/tmp/repo", "a.txt");
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        stage_all(vcx, &session);

        dispatch(&ws, vcx, stoat_action::ReviewApplyStaged);
        vcx.run_until_parked();

        assert_eq!(git.applied_patches(Path::new("/tmp/repo")).len(), 2);
        assert_eq!(
            session_apply_result(vcx, &session),
            Some(ReviewApplyResult {
                applied: 2,
                total: 2,
                first_failure: None,
            }),
        );
    }

    #[test]
    fn dispatch_review_apply_staged_records_partial_failure() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo").fail_apply_with("disk full");
        install_git_host_global(vcx, git.clone());
        let (session, _, _) = new_two_chunk_working_tree_session(vcx, "/tmp/repo", "a.txt");
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        stage_all(vcx, &session);

        dispatch(&ws, vcx, stoat_action::ReviewApplyStaged);
        vcx.run_until_parked();

        assert_eq!(
            git.applied_patches(Path::new("/tmp/repo")).len(),
            2,
            "every staged chunk reaches apply_to_index even on failure",
        );
        assert_eq!(
            session_apply_result(vcx, &session),
            Some(ReviewApplyResult {
                applied: 0,
                total: 2,
                first_failure: Some("disk full".to_string()),
            }),
        );
    }

    #[test]
    fn dispatch_review_apply_staged_with_nothing_staged_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo");
        install_git_host_global(vcx, git.clone());
        let (session, _, _) = new_two_chunk_working_tree_session(vcx, "/tmp/repo", "a.txt");
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        dispatch(&ws, vcx, stoat_action::ReviewApplyStaged);
        vcx.run_until_parked();

        assert!(git.applied_patches(Path::new("/tmp/repo")).is_empty());
        assert_eq!(session_apply_result(vcx, &session), None);
    }

    #[test]
    fn dispatch_review_apply_staged_is_noop_for_in_memory_source() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo");
        install_git_host_global(vcx, git.clone());
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        stage_all(vcx, &session);

        dispatch(&ws, vcx, stoat_action::ReviewApplyStaged);
        vcx.run_until_parked();

        assert!(git.applied_patches(Path::new("/tmp/repo")).is_empty());
        assert_eq!(session_apply_result(vcx, &session), None);
    }

    #[test]
    fn dispatch_review_apply_staged_without_review_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        install_git_host_global(vcx, git.clone());

        dispatch(&ws, vcx, stoat_action::ReviewApplyStaged);
        vcx.run_until_parked();

        assert!(git.applied_patches(Path::new("/tmp/repo")).is_empty());
    }

    fn install_language_registry_global(vcx: &mut VisualTestContext) {
        use crate::globals::LanguageRegistry;
        vcx.update(|_, cx| {
            if !cx.has_global::<LanguageRegistry>() {
                cx.set_global(LanguageRegistry::standard());
            }
        });
    }

    fn in_memory_session_with_files(
        vcx: &mut VisualTestContext,
        files: Vec<stoat::review_session::InMemoryFile>,
    ) -> Entity<crate::review_session::ReviewSession> {
        let stored = Arc::new(files);
        let inputs_for_session = stored.clone();
        vcx.update(|_, cx| {
            cx.new(|_| {
                let mut inner = stoat::review_session::ReviewSession::new(ReviewSource::InMemory {
                    files: stored.clone(),
                });
                let inputs: Vec<ReviewFileInput> = inputs_for_session
                    .iter()
                    .map(|f| ReviewFileInput {
                        path: f.path.clone(),
                        rel_path: f.path.display().to_string(),
                        language: None,
                        base_text: f.base_text.clone(),
                        buffer_text: f.buffer_text.clone(),
                    })
                    .collect();
                inner.add_files(inputs);
                crate::review_session::ReviewSession::new(inner)
            })
        })
    }

    #[test]
    fn review_follow_jumps_cursor_to_edited_file_only_when_enabled() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/a.txt", b"a\nNEW\nc\n");
        fs.insert_file("/tmp/repo/b.txt", b"x\nNEW\nz\n");
        install_fs_host_global(vcx, fs.clone());
        let session = in_memory_session_with_files(
            vcx,
            vec![
                stoat::review_session::InMemoryFile {
                    path: PathBuf::from("/tmp/repo/a.txt"),
                    base_text: Arc::new("a\nOLD\nc\n".to_string()),
                    buffer_text: Arc::new("a\nNEW\nc\n".to_string()),
                },
                stoat::review_session::InMemoryFile {
                    path: PathBuf::from("/tmp/repo/b.txt"),
                    base_text: Arc::new("x\nOLD\nz\n".to_string()),
                    buffer_text: Arc::new("x\nNEW\nz\n".to_string()),
                },
            ],
        );
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        // Park the cursor on file b's chunk so a jump back to file a is
        // observable.
        dispatch(&ws, vcx, stoat_action::ReviewNextChunk);
        vcx.run_until_parked();
        assert_eq!(cursor_chunk_index(vcx, &session), Some(1));

        // Follow on: an external edit to a.txt snaps the cursor to a's
        // first chunk.
        dispatch(&ws, vcx, stoat_action::ReviewToggleFollow);
        vcx.run_until_parked();
        dispatch(
            &ws,
            vcx,
            stoat_action::ReviewExternalEdit {
                path: PathBuf::from("/tmp/repo/a.txt"),
            },
        );
        vcx.run_until_parked();
        assert_eq!(cursor_chunk_index(vcx, &session), Some(0));

        // Follow off: an external edit to b.txt leaves the cursor put.
        dispatch(&ws, vcx, stoat_action::ReviewToggleFollow);
        vcx.run_until_parked();
        dispatch(
            &ws,
            vcx,
            stoat_action::ReviewExternalEdit {
                path: PathBuf::from("/tmp/repo/b.txt"),
            },
        );
        vcx.run_until_parked();
        assert_eq!(
            cursor_chunk_index(vcx, &session),
            Some(0),
            "follow off must not move the cursor on an external edit",
        );
    }

    #[test]
    fn dispatch_review_refresh_re_extracts_in_memory_session() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        install_language_registry_global(vcx);
        let session = in_memory_session_with_files(
            vcx,
            vec![stoat::review_session::InMemoryFile {
                path: PathBuf::from("a.txt"),
                base_text: Arc::new("a\nOLD\nc\n".to_string()),
                buffer_text: Arc::new("a\nNEW\nc\n".to_string()),
            }],
        );
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        let version_before = session.read_with(vcx, |s, _| s.inner().version);

        dispatch(&ws, vcx, stoat_action::ReviewRefresh);
        vcx.run_until_parked();

        let version_after = session.read_with(vcx, |s, _| s.inner().version);
        assert!(
            version_after > version_before,
            "refresh must bump the session version (before={version_before}, after={version_after})",
        );
        session.read_with(vcx, |s, _| {
            assert_eq!(s.inner().files.len(), 1);
            assert_eq!(s.inner().order.len(), 1);
        });
    }

    #[test]
    fn dispatch_review_refresh_carries_decided_statuses() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        install_language_registry_global(vcx);
        let session = in_memory_session_with_files(
            vcx,
            vec![stoat::review_session::InMemoryFile {
                path: PathBuf::from("a.txt"),
                base_text: Arc::new("a\nOLD\nc\n".to_string()),
                buffer_text: Arc::new("a\nNEW\nc\n".to_string()),
            }],
        );
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        let id_before = session.read_with(vcx, |s, _| s.inner().order[0]);
        session.update(vcx, |s, cx| {
            s.set_status(id_before, ChunkStatus::Staged, cx);
        });

        dispatch(&ws, vcx, stoat_action::ReviewRefresh);
        vcx.run_until_parked();

        session.read_with(vcx, |s, _| {
            let id = s.inner().order[0];
            assert_eq!(s.inner().chunks[&id].status, ChunkStatus::Staged);
        });
    }

    #[test]
    fn dispatch_review_refresh_without_review_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::ReviewRefresh);
        vcx.run_until_parked();
    }

    fn install_fs_host_global(vcx: &mut VisualTestContext, fs: Arc<stoat::host::FakeFs>) {
        use crate::globals::FsHostGlobal;
        vcx.update(|_, cx| {
            cx.set_global(FsHostGlobal(fs as Arc<dyn stoat::host::FsHost>));
        });
    }

    #[test]
    fn dispatch_review_external_edit_refreshes_named_file() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/a.txt", b"a\nNEW\nc\n");
        install_fs_host_global(vcx, fs.clone());
        let session = in_memory_session_with_files(
            vcx,
            vec![stoat::review_session::InMemoryFile {
                path: PathBuf::from("/tmp/repo/a.txt"),
                base_text: Arc::new("a\nOLD\nc\n".to_string()),
                buffer_text: Arc::new("a\nNEW\nc\n".to_string()),
            }],
        );
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        fs.insert_file("/tmp/repo/a.txt", b"a\nNEWER\nc\n");

        dispatch(
            &ws,
            vcx,
            stoat_action::ReviewExternalEdit {
                path: PathBuf::from("/tmp/repo/a.txt"),
            },
        );
        vcx.run_until_parked();

        session.read_with(vcx, |s, _| {
            assert_eq!(s.inner().files[0].buffer_text.as_str(), "a\nNEWER\nc\n");
        });
    }

    #[test]
    fn dispatch_review_external_edit_with_unknown_path_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        install_fs_host_global(vcx, fs.clone());
        let session = in_memory_session_with_files(
            vcx,
            vec![stoat::review_session::InMemoryFile {
                path: PathBuf::from("/tmp/repo/a.txt"),
                base_text: Arc::new("a\nOLD\nc\n".to_string()),
                buffer_text: Arc::new("a\nNEW\nc\n".to_string()),
            }],
        );
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        let version_before = session.read_with(vcx, |s, _| s.inner().version);

        dispatch(
            &ws,
            vcx,
            stoat_action::ReviewExternalEdit {
                path: PathBuf::from("/tmp/repo/elsewhere.txt"),
            },
        );
        vcx.run_until_parked();

        let version_after = session.read_with(vcx, |s, _| s.inner().version);
        assert_eq!(
            version_after, version_before,
            "unknown path must not bump the session version",
        );
    }

    #[test]
    fn dispatch_review_external_edit_without_review_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        install_fs_host_global(vcx, fs);

        dispatch(
            &ws,
            vcx,
            stoat_action::ReviewExternalEdit {
                path: PathBuf::from("/tmp/repo/a.txt"),
            },
        );
        vcx.run_until_parked();
    }

    fn build_inner_session_with_provenances(
        cursor_file_rel: &str,
        target_file_rel: &str,
        extra_provenances: &[stoat::review::MoveProvenance],
    ) -> stoat::review_session::ReviewSession {
        use stoat::review::ReviewSide;
        let mut inner = stoat::review_session::ReviewSession::new(ReviewSource::InMemory {
            files: Arc::new(Vec::new()),
        });
        inner.add_files(vec![
            ReviewFileInput {
                path: PathBuf::from(cursor_file_rel),
                rel_path: cursor_file_rel.to_string(),
                language: None,
                base_text: Arc::new("a\nOLD\nc\n".to_string()),
                buffer_text: Arc::new("a\nNEW\nc\n".to_string()),
            },
            ReviewFileInput {
                path: PathBuf::from(target_file_rel),
                rel_path: target_file_rel.to_string(),
                language: None,
                base_text: Arc::new("x\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n".to_string()),
                buffer_text: Arc::new("X\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n".to_string()),
            },
        ]);
        let cursor_id = inner.order[0];
        let chunk = inner.chunks.get_mut(&cursor_id).expect("cursor chunk");
        // Replace the chunk's rows with one synthetic Changed row
        // per provenance entry; tests rely on these provenance
        // values directly.
        chunk.hunk.rows.clear();
        for (i, prov) in extra_provenances.iter().enumerate() {
            chunk.hunk.rows.push(ReviewRow::Changed {
                left: None,
                right: Some(ReviewSide {
                    text: String::new(),
                    line_num: (i as u32) + 1,
                    change_spans: Vec::new(),
                    moved_spans: Vec::new(),
                    move_provenance: Some(prov.clone()),
                }),
            });
        }
        inner
    }

    fn session_with_inner(
        vcx: &mut VisualTestContext,
        inner: stoat::review_session::ReviewSession,
    ) -> Entity<crate::review_session::ReviewSession> {
        vcx.update(|_, cx| cx.new(|_| crate::review_session::ReviewSession::new(inner)))
    }

    fn active_file_index(
        vcx: &mut VisualTestContext,
        item: &Entity<crate::review_item::ReviewItem>,
    ) -> Option<usize> {
        item.read_with(vcx, |item, app| item.active_file_index(app))
    }

    #[test]
    fn dispatch_jump_to_move_source_switches_file_and_scrolls() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let target = stoat::review::MoveProvenance {
            rel_path: "b.txt".to_string(),
            line: 0,
        };
        let inner = build_inner_session_with_provenances("a.txt", "b.txt", &[target]);
        let session = session_with_inner(vcx, inner);
        let item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        assert_eq!(active_file_index(vcx, &item), Some(0));

        dispatch(&ws, vcx, stoat_action::JumpToMoveSource);
        vcx.run_until_parked();

        assert_eq!(
            active_file_index(vcx, &item),
            Some(1),
            "session cursor must move to a chunk in the target file",
        );
    }

    #[test]
    fn dispatch_jump_to_move_source_without_provenance_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        let active_before = active_file_index(vcx, &item);

        dispatch(&ws, vcx, stoat_action::JumpToMoveSource);
        vcx.run_until_parked();

        assert_eq!(active_file_index(vcx, &item), active_before);
    }

    #[test]
    fn dispatch_jump_to_next_move_source_cycles_through_distinct_sources() {
        use stoat::review::MoveProvenance;
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let prov_b = MoveProvenance {
            rel_path: "b.txt".to_string(),
            line: 0,
        };
        let prov_a = MoveProvenance {
            rel_path: "a.txt".to_string(),
            line: 0,
        };
        let inner = build_inner_session_with_provenances("a.txt", "b.txt", &[prov_b, prov_a]);
        let session = session_with_inner(vcx, inner);
        let item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        dispatch(&ws, vcx, stoat_action::JumpToMoveSource);
        vcx.run_until_parked();
        assert_eq!(active_file_index(vcx, &item), Some(1));

        dispatch(&ws, vcx, stoat_action::JumpToNextMoveSource);
        vcx.run_until_parked();
        assert_eq!(
            active_file_index(vcx, &item),
            Some(0),
            "next cycles to the second distinct provenance (a.txt)",
        );
    }

    #[test]
    fn dispatch_jump_to_prev_move_source_cycles_backward() {
        use stoat::review::MoveProvenance;
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let prov_b = MoveProvenance {
            rel_path: "b.txt".to_string(),
            line: 0,
        };
        let prov_a = MoveProvenance {
            rel_path: "a.txt".to_string(),
            line: 0,
        };
        let inner = build_inner_session_with_provenances("a.txt", "b.txt", &[prov_b, prov_a]);
        let session = session_with_inner(vcx, inner);
        let item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        dispatch(&ws, vcx, stoat_action::JumpToMoveSource);
        vcx.run_until_parked();
        assert_eq!(active_file_index(vcx, &item), Some(1));

        dispatch(&ws, vcx, stoat_action::JumpToPrevMoveSource);
        vcx.run_until_parked();
        assert_eq!(
            active_file_index(vcx, &item),
            Some(0),
            "prev wraps to the last distinct provenance (a.txt)",
        );
    }

    #[test]
    fn dispatch_jump_to_move_target_uses_lhs_only_provenance() {
        use stoat::review::{MoveProvenance, ReviewSide};
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let target = MoveProvenance {
            rel_path: "b.txt".to_string(),
            line: 0,
        };
        let mut inner = build_inner_session_with_provenances("a.txt", "b.txt", &[]);
        let cursor_id = inner.cursor.current.expect("cursor set");
        let chunk = inner.chunks.get_mut(&cursor_id).expect("cursor chunk");
        chunk.hunk.rows.clear();
        chunk.hunk.rows.push(ReviewRow::Changed {
            left: Some(ReviewSide {
                text: String::new(),
                line_num: 1,
                change_spans: Vec::new(),
                moved_spans: Vec::new(),
                move_provenance: Some(target.clone()),
            }),
            right: None,
        });
        let session = session_with_inner(vcx, inner);
        let item = open_review_item_in_focused_pane(vcx, &ws, session.clone());

        dispatch(&ws, vcx, stoat_action::JumpToMoveTarget);
        vcx.run_until_parked();

        assert_eq!(
            active_file_index(vcx, &item),
            Some(1),
            "JumpToMoveTarget must switch the active file using the LHS-only provenance",
        );
    }

    #[test]
    fn dispatch_jump_to_move_source_without_review_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::JumpToMoveSource);
        vcx.run_until_parked();
    }

    fn modal_count(vcx: &mut VisualTestContext, ws: &Entity<Workspace>) -> bool {
        ws.read_with(vcx, |w, cx| w.modal_layer().read(cx).has_active_modal())
    }

    #[test]
    fn dispatch_query_move_relationships_opens_picker_when_moves_exist() {
        use stoat::review::MoveProvenance;
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let target = MoveProvenance {
            rel_path: "b.txt".to_string(),
            line: 0,
        };
        let inner = build_inner_session_with_provenances("a.txt", "b.txt", &[target]);
        let session = session_with_inner(vcx, inner);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session.clone());
        assert!(!modal_count(vcx, &ws));

        dispatch(&ws, vcx, stoat_action::QueryMoveRelationships);
        vcx.run_until_parked();

        assert!(
            modal_count(vcx, &ws),
            "QueryMoveRelationships must push a picker modal when the session has cross-file moves",
        );
    }

    #[test]
    fn dispatch_query_move_relationships_with_no_moves_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let session = new_two_chunk_review_session(vcx);
        let _item = open_review_item_in_focused_pane(vcx, &ws, session);

        dispatch(&ws, vcx, stoat_action::QueryMoveRelationships);
        vcx.run_until_parked();

        assert!(!modal_count(vcx, &ws));
    }

    #[test]
    fn dispatch_query_move_relationships_without_review_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::QueryMoveRelationships);
        vcx.run_until_parked();

        assert!(!modal_count(vcx, &ws));
    }

    fn active_pane_review_item(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
    ) -> Option<Entity<crate::review_item::ReviewItem>> {
        ws.read_with(vcx, |w, cx| w.active_pane_item(cx))
            .and_then(|handle| {
                handle
                    .to_any_view()
                    .downcast::<crate::review_item::ReviewItem>()
                    .ok()
            })
    }

    fn install_full_globals(
        vcx: &mut VisualTestContext,
        fs: Arc<stoat::host::FakeFs>,
        git: Arc<stoat::host::fake::FakeGit>,
    ) {
        use crate::globals::{FsHostGlobal, GitHostGlobal, LanguageRegistry};
        vcx.update(|_, cx| {
            cx.set_global(FsHostGlobal(fs as Arc<dyn stoat::host::FsHost>));
            cx.set_global(GitHostGlobal(git as Arc<dyn stoat::host::GitHost>));
            if !cx.has_global::<LanguageRegistry>() {
                cx.set_global(LanguageRegistry::standard());
            }
        });
    }

    #[test]
    fn dispatch_open_review_opens_review_item_for_working_tree() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/a.txt", b"a\nNEW\nc\n");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .with_fs(&fs)
            .modified("a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        install_full_globals(vcx, fs, git);

        dispatch(&ws, vcx, stoat_action::OpenReview);
        vcx.run_until_parked();

        let item = active_pane_review_item(vcx, &ws).expect("review item in focused pane");
        item.read_with(vcx, |item, cx| {
            assert_eq!(item.files().len(), 1);
            assert_eq!(item.files()[0].rel_path, "a.txt");
            assert!(!item.session().read(cx).inner().order.is_empty());
        });
    }

    #[test]
    fn dispatch_open_review_with_no_changed_files_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo");
        install_full_globals(vcx, fs, git);

        dispatch(&ws, vcx, stoat_action::OpenReview);
        vcx.run_until_parked();

        assert!(active_pane_review_item(vcx, &ws).is_none());
    }

    #[test]
    fn dispatch_open_review_without_git_repo_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        install_full_globals(vcx, fs, git);

        dispatch(&ws, vcx, stoat_action::OpenReview);
        vcx.run_until_parked();

        assert!(active_pane_review_item(vcx, &ws).is_none());
    }

    #[test]
    fn dispatch_open_review_commit_opens_review_item_for_commit() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("p1", &[("a.txt", "a\nOLD\nc\n")])
            .commit_with_parent("c1", "p1", &[("a.txt", "a\nNEW\nc\n")]);
        install_full_globals(vcx, fs, git);

        dispatch(
            &ws,
            vcx,
            stoat_action::OpenReviewCommit {
                workdir: PathBuf::from("/tmp/repo"),
                sha: "c1".to_string(),
            },
        );
        vcx.run_until_parked();

        let item = active_pane_review_item(vcx, &ws).expect("review item in focused pane");
        item.read_with(vcx, |item, cx| {
            assert_eq!(item.files().len(), 1);
            assert_eq!(item.files()[0].rel_path, "a.txt");
            assert!(matches!(
                item.session().read(cx).inner().source,
                ReviewSource::Commit { ref sha, .. } if sha == "c1"
            ));
        });
    }

    #[test]
    fn dispatch_open_review_commit_with_unknown_sha_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo");
        install_full_globals(vcx, fs, git);

        dispatch(
            &ws,
            vcx,
            stoat_action::OpenReviewCommit {
                workdir: PathBuf::from("/tmp/repo"),
                sha: "ghost".to_string(),
            },
        );
        vcx.run_until_parked();

        assert!(active_pane_review_item(vcx, &ws).is_none());
    }

    #[test]
    fn dispatch_open_review_commit_for_root_commit_diffs_against_empty_tree() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("root", &[("a.txt", "alpha\nbeta\n")]);
        install_full_globals(vcx, fs, git);

        dispatch(
            &ws,
            vcx,
            stoat_action::OpenReviewCommit {
                workdir: PathBuf::from("/tmp/repo"),
                sha: "root".to_string(),
            },
        );
        vcx.run_until_parked();

        let item = active_pane_review_item(vcx, &ws).expect("review item in focused pane");
        item.read_with(vcx, |item, _| {
            assert_eq!(item.files().len(), 1);
            assert_eq!(item.files()[0].rel_path, "a.txt");
        });
    }

    #[test]
    fn dispatch_open_review_commit_range_opens_review_item() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("from", &[("a.txt", "a\nOLD\nc\n")])
            .commit_with_parent("to", "from", &[("a.txt", "a\nNEW\nc\n")]);
        install_full_globals(vcx, fs, git);

        dispatch(
            &ws,
            vcx,
            stoat_action::OpenReviewCommitRange {
                workdir: PathBuf::from("/tmp/repo"),
                from: "from".to_string(),
                to: "to".to_string(),
            },
        );
        vcx.run_until_parked();

        let item = active_pane_review_item(vcx, &ws).expect("review item in focused pane");
        item.read_with(vcx, |item, cx| {
            assert_eq!(item.files().len(), 1);
            assert_eq!(item.files()[0].rel_path, "a.txt");
            assert!(matches!(
                item.session().read(cx).inner().source,
                ReviewSource::CommitRange {
                    ref from, ref to, ..
                } if from == "from" && to == "to"
            ));
        });
    }

    #[test]
    fn dispatch_open_review_commit_range_with_unknown_to_sha_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("from", &[("a.txt", "a\nOLD\nc\n")]);
        install_full_globals(vcx, fs, git);

        dispatch(
            &ws,
            vcx,
            stoat_action::OpenReviewCommitRange {
                workdir: PathBuf::from("/tmp/repo"),
                from: "from".to_string(),
                to: "ghost".to_string(),
            },
        );
        vcx.run_until_parked();

        assert!(active_pane_review_item(vcx, &ws).is_none());
    }

    #[test]
    fn dispatch_open_review_agent_edits_opens_review_item() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        install_language_registry_global(vcx);

        dispatch(
            &ws,
            vcx,
            stoat_action::OpenReviewAgentEdits {
                edits: vec![stoat_action::AgentEdit {
                    path: PathBuf::from("agent/a.rs"),
                    base_text: Arc::new("a\nOLD\nc\n".to_string()),
                    proposed_text: Arc::new("a\nNEW\nc\n".to_string()),
                }],
            },
        );
        vcx.run_until_parked();

        let item = active_pane_review_item(vcx, &ws).expect("review item in focused pane");
        item.read_with(vcx, |item, cx| {
            assert_eq!(item.files().len(), 1);
            assert!(matches!(
                item.session().read(cx).inner().source,
                ReviewSource::AgentEdits { .. },
            ));
        });
    }

    #[test]
    fn dispatch_open_review_agent_edits_with_empty_payload_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        install_language_registry_global(vcx);

        dispatch(
            &ws,
            vcx,
            stoat_action::OpenReviewAgentEdits { edits: Vec::new() },
        );
        vcx.run_until_parked();

        assert!(active_pane_review_item(vcx, &ws).is_none());
    }

    fn install_commit_list_globals(
        vcx: &mut VisualTestContext,
        git: Arc<stoat::host::fake::FakeGit>,
    ) -> Arc<stoat_scheduler::TestScheduler> {
        use crate::globals::{ExecutorGlobal, GitHostGlobal, LanguageRegistry};
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let executor = scheduler.executor();
        vcx.update(|_, cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(GitHostGlobal(git as Arc<dyn stoat::host::GitHost>));
            if !cx.has_global::<LanguageRegistry>() {
                cx.set_global(LanguageRegistry::standard());
            }
        });
        scheduler
    }

    fn seed_commits(git: &Arc<stoat::host::fake::FakeGit>, workdir: &str, count: usize) {
        let mut builder = git.add_repo(workdir);
        let mut prev: Option<String> = None;
        for i in 0..count {
            let sha = format!("c{:04}", i);
            let body = format!("fn a() {{ /* {i} */ }}\n");
            match prev.as_deref() {
                None => {
                    builder.commit_with_message(
                        &sha,
                        &format!("commit {sha}"),
                        &[("a.rs", body.as_str())],
                    );
                },
                Some(parent) => {
                    builder.commit_with_parent_message(
                        &sha,
                        parent,
                        &format!("commit {sha}"),
                        &[("a.rs", body.as_str())],
                    );
                },
            };
            prev = Some(sha);
        }
    }

    fn install_commit_list_in_focused_pane(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
        workdir: &str,
        commits: usize,
    ) -> (
        Entity<crate::commit_list::CommitListItem>,
        Entity<crate::commit_list::CommitListState>,
        Arc<stoat::host::fake::FakeGit>,
        Arc<stoat_scheduler::TestScheduler>,
    ) {
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        seed_commits(&git, workdir, commits);
        let scheduler = install_commit_list_globals(vcx, git.clone());

        let state = vcx.update(|_, cx| {
            cx.new(|_| {
                let inner = stoat::commit_list::CommitListState::new(PathBuf::from(workdir));
                crate::commit_list::CommitListState::new(inner)
            })
        });
        let registry = ws.read_with(vcx, |w, _| w.buffer_registry().clone());
        let item = vcx.update(|window, cx| {
            let state = state.clone();
            let registry = registry.clone();
            cx.new(|cx| crate::commit_list::CommitListItem::new(state, registry, window, cx))
        });
        ws.update(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            let pane = w
                .pane_tree
                .read(cx)
                .pane(focus)
                .expect("focused pane")
                .clone();
            let handle: Box<dyn ItemHandle> = Box::new(item.clone());
            pane.update(cx, |p, cx| {
                p.add_item(handle, cx);
            });
        });
        (item, state, git, scheduler)
    }

    fn settle_commits(
        scheduler: &Arc<stoat_scheduler::TestScheduler>,
        vcx: &mut VisualTestContext,
    ) {
        for _ in 0..4 {
            scheduler.run_until_parked();
            vcx.run_until_parked();
        }
    }

    fn trigger_initial_commits_load(
        item: &Entity<crate::commit_list::CommitListItem>,
        vcx: &mut VisualTestContext,
    ) {
        use crate::picker::PickerDelegate;
        let picker = item.read_with(vcx, |item, _| item.picker().clone());
        picker.update(vcx, |p, cx| {
            p.delegate_mut().update_matches(String::new(), cx).detach();
        });
    }

    fn delegate_selected(
        item: &Entity<crate::commit_list::CommitListItem>,
        vcx: &mut VisualTestContext,
    ) -> usize {
        let picker = item.read_with(vcx, |item, _| item.picker().clone());
        picker.read_with(vcx, |p, _| p.selected_index())
    }

    #[test]
    fn dispatch_commits_next_advances_selection() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (item, _state, _git, scheduler) =
            install_commit_list_in_focused_pane(vcx, &ws, "/tmp/repo", 3);
        trigger_initial_commits_load(&item, vcx);
        settle_commits(&scheduler, vcx);

        dispatch(&ws, vcx, stoat_action::CommitsNext);
        settle_commits(&scheduler, vcx);

        assert_eq!(delegate_selected(&item, vcx), 1);
    }

    #[test]
    fn dispatch_commits_prev_clamps_at_zero() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (item, _state, _git, scheduler) =
            install_commit_list_in_focused_pane(vcx, &ws, "/tmp/repo", 3);
        trigger_initial_commits_load(&item, vcx);
        settle_commits(&scheduler, vcx);

        dispatch(&ws, vcx, stoat_action::CommitsPrev);
        settle_commits(&scheduler, vcx);

        assert_eq!(delegate_selected(&item, vcx), 0);
    }

    #[test]
    fn dispatch_commits_page_down_advances_by_step() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (item, _state, _git, scheduler) =
            install_commit_list_in_focused_pane(vcx, &ws, "/tmp/repo", 32);
        trigger_initial_commits_load(&item, vcx);
        settle_commits(&scheduler, vcx);

        dispatch(&ws, vcx, stoat_action::CommitsPageDown);
        settle_commits(&scheduler, vcx);

        assert_eq!(
            delegate_selected(&item, vcx),
            crate::commit_list::COMMITS_PAGE_STEP,
        );
    }

    #[test]
    fn dispatch_commits_first_returns_to_head() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (item, _state, _git, scheduler) =
            install_commit_list_in_focused_pane(vcx, &ws, "/tmp/repo", 5);
        trigger_initial_commits_load(&item, vcx);
        settle_commits(&scheduler, vcx);
        // Move off zero first.
        dispatch(&ws, vcx, stoat_action::CommitsNext);
        dispatch(&ws, vcx, stoat_action::CommitsNext);
        settle_commits(&scheduler, vcx);
        assert_eq!(delegate_selected(&item, vcx), 2);

        dispatch(&ws, vcx, stoat_action::CommitsFirst);
        settle_commits(&scheduler, vcx);

        assert_eq!(delegate_selected(&item, vcx), 0);
    }

    #[test]
    fn dispatch_commits_last_jumps_to_loaded_tail() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (item, _state, _git, scheduler) =
            install_commit_list_in_focused_pane(vcx, &ws, "/tmp/repo", 5);
        trigger_initial_commits_load(&item, vcx);
        settle_commits(&scheduler, vcx);

        dispatch(&ws, vcx, stoat_action::CommitsLast);
        settle_commits(&scheduler, vcx);

        assert_eq!(delegate_selected(&item, vcx), 4);
    }

    #[test]
    fn dispatch_commits_refresh_clears_caches() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (item, state, _git, scheduler) =
            install_commit_list_in_focused_pane(vcx, &ws, "/tmp/repo", 3);
        trigger_initial_commits_load(&item, vcx);
        settle_commits(&scheduler, vcx);

        // After the initial load, state.commits is non-empty.
        let before = state.read_with(vcx, |s, _| s.inner().commits.len());
        assert_eq!(before, 3);

        dispatch(&ws, vcx, stoat_action::CommitsRefresh);
        settle_commits(&scheduler, vcx);

        // Refresh clears + reloads; final state should still have 3 commits
        // but the preview cache and loading sets must have been cleared.
        let after = state.read_with(vcx, |s, _| s.inner().commits.len());
        assert_eq!(after, 3, "refresh reloads the same first page");
        let preview_count = state.read_with(vcx, |s, _| s.inner().preview_sessions.len());
        let summary_count = state.read_with(vcx, |s, _| s.inner().summaries.len());
        let item_previews = item.read_with(vcx, |item, _| item.preview_items().len());
        assert_eq!(
            (preview_count, summary_count, item_previews),
            (1, 1, 1),
            "refresh rebuilds caches for the current selection",
        );
    }

    #[test]
    fn dispatch_commits_open_review_opens_review_item() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (item, _state, _git, scheduler) =
            install_commit_list_in_focused_pane(vcx, &ws, "/tmp/repo", 3);
        trigger_initial_commits_load(&item, vcx);
        settle_commits(&scheduler, vcx);

        dispatch(&ws, vcx, stoat_action::CommitsOpenReview);
        settle_commits(&scheduler, vcx);

        let review = active_pane_review_item(vcx, &ws).expect("review item in focused pane");
        review.read_with(vcx, |review, cx| {
            assert!(matches!(
                review.session().read(cx).inner().source,
                ReviewSource::Commit { .. },
            ));
        });
    }

    fn active_pane_rebase_item(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
    ) -> Option<Entity<RebaseItem>> {
        ws.read_with(vcx, |w, cx| w.active_pane_item(cx))
            .and_then(|handle| handle.to_any_view().downcast::<RebaseItem>().ok())
    }

    #[test]
    fn dispatch_enter_rebase_opens_rebase_item_with_plan() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (item, _state, _git, scheduler) =
            install_commit_list_in_focused_pane(vcx, &ws, "/tmp/repo", 3);
        trigger_initial_commits_load(&item, vcx);
        settle_commits(&scheduler, vcx);

        dispatch(&ws, vcx, stoat_action::CommitsNext);
        settle_commits(&scheduler, vcx);

        dispatch(&ws, vcx, stoat_action::EnterRebase);
        settle_commits(&scheduler, vcx);

        let rebase = active_pane_rebase_item(vcx, &ws).expect("rebase item in focused pane");
        rebase.read_with(vcx, |rebase, cx| {
            let state = rebase.state().read(cx);
            assert_eq!(state.onto, "c0001", "cursor commit becomes onto");
            assert_eq!(state.todo.len(), 1, "only the newer commit is above");
            assert_eq!(state.todo[0].commit.sha, "c0002");
            assert_eq!(state.todo[0].op, RebaseTodoOp::Pick);
        });
    }

    #[test]
    fn dispatch_enter_rebase_oldest_loaded_commit_builds_full_plan_oldest_first() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (item, _state, _git, scheduler) =
            install_commit_list_in_focused_pane(vcx, &ws, "/tmp/repo", 3);
        trigger_initial_commits_load(&item, vcx);
        settle_commits(&scheduler, vcx);

        dispatch(&ws, vcx, stoat_action::CommitsLast);
        settle_commits(&scheduler, vcx);

        dispatch(&ws, vcx, stoat_action::EnterRebase);
        settle_commits(&scheduler, vcx);

        let rebase = active_pane_rebase_item(vcx, &ws).expect("rebase item in focused pane");
        rebase.read_with(vcx, |rebase, cx| {
            let state = rebase.state().read(cx);
            assert_eq!(state.onto, "c0000");
            let shas: Vec<_> = state.todo.iter().map(|e| e.commit.sha.clone()).collect();
            assert_eq!(
                shas,
                vec!["c0001".to_string(), "c0002".to_string()],
                "todo is oldest-first above onto",
            );
        });
    }

    #[test]
    fn dispatch_enter_rebase_at_head_is_noop() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (item, _state, _git, scheduler) =
            install_commit_list_in_focused_pane(vcx, &ws, "/tmp/repo", 3);
        trigger_initial_commits_load(&item, vcx);
        settle_commits(&scheduler, vcx);

        dispatch(&ws, vcx, stoat_action::EnterRebase);
        settle_commits(&scheduler, vcx);

        assert!(
            active_pane_rebase_item(vcx, &ws).is_none(),
            "EnterRebase at HEAD must not install a RebaseItem",
        );
        assert!(
            active_pane_commit_list(vcx, &ws).is_some(),
            "commit list remains the active item",
        );
    }

    #[test]
    fn dispatch_enter_rebase_without_commit_list_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        let _scheduler = install_commit_list_globals(vcx, git);

        dispatch(&ws, vcx, stoat_action::EnterRebase);
        vcx.run_until_parked();

        assert!(active_pane_rebase_item(vcx, &ws).is_none());
    }

    #[test]
    fn dispatch_close_commits_removes_commit_list() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (_item, _state, _git, scheduler) =
            install_commit_list_in_focused_pane(vcx, &ws, "/tmp/repo", 3);
        settle_commits(&scheduler, vcx);

        // Confirm the commit list is the active item before close.
        let active_before = ws.read_with(vcx, |w, cx| {
            w.active_pane_item(cx)
                .map(|item| {
                    item.to_any_view()
                        .downcast::<crate::commit_list::CommitListItem>()
                        .is_ok()
                })
                .unwrap_or(false)
        });
        assert!(
            active_before,
            "commit list must be the active item before close"
        );

        dispatch(&ws, vcx, stoat_action::CloseCommits);
        settle_commits(&scheduler, vcx);

        let active_after = ws.read_with(vcx, |w, cx| {
            w.active_pane_item(cx)
                .map(|item| {
                    item.to_any_view()
                        .downcast::<crate::commit_list::CommitListItem>()
                        .is_ok()
                })
                .unwrap_or(false)
        });
        assert!(
            !active_after,
            "CloseCommits must remove the commit-list item"
        );
    }

    fn active_pane_commit_list(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
    ) -> Option<Entity<crate::commit_list::CommitListItem>> {
        ws.read_with(vcx, |w, cx| w.active_pane_item(cx))
            .and_then(|handle| {
                handle
                    .to_any_view()
                    .downcast::<crate::commit_list::CommitListItem>()
                    .ok()
            })
    }

    #[test]
    fn dispatch_open_commits_installs_commit_list_item() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        seed_commits(&git, "/tmp/repo", 3);
        let _scheduler = install_commit_list_globals(vcx, git);

        dispatch(&ws, vcx, stoat_action::OpenCommits);
        vcx.run_until_parked();

        assert!(
            active_pane_commit_list(vcx, &ws).is_some(),
            "OpenCommits must install a CommitListItem in the focused pane",
        );
    }

    #[test]
    fn dispatch_open_commits_triggers_initial_page_load() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        seed_commits(&git, "/tmp/repo", 3);
        let scheduler = install_commit_list_globals(vcx, git);

        dispatch(&ws, vcx, stoat_action::OpenCommits);
        settle_commits(&scheduler, vcx);

        let item =
            active_pane_commit_list(vcx, &ws).expect("commit list installed in focused pane");
        let count = item.read_with(vcx, |item, cx| item.state().read(cx).inner().commits.len());
        assert_eq!(
            count, 3,
            "initial page load must populate state.commits after settle",
        );
    }

    #[test]
    fn dispatch_open_commits_without_git_repo_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        // FakeGit with no repo registered: discover returns None.
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        let _scheduler = install_commit_list_globals(vcx, git);

        dispatch(&ws, vcx, stoat_action::OpenCommits);
        vcx.run_until_parked();

        assert!(
            active_pane_commit_list(vcx, &ws).is_none(),
            "OpenCommits must skip when the workdir is not a git repo",
        );
    }

    fn open_conflict_item_in_focused_pane(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
        file: ConflictedFile,
    ) -> Entity<ConflictItem> {
        let item = vcx.update(|_, cx| cx.new(|cx| ConflictItem::from_conflicted_file(file, cx)));
        ws.update(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            let pane = w
                .pane_tree
                .read(cx)
                .pane(focus)
                .expect("focused pane")
                .clone();
            let handle: Box<dyn ItemHandle> = Box::new(item.clone());
            pane.update(cx, |p, cx| {
                p.add_item(handle, cx);
            });
        });
        vcx.run_until_parked();
        item
    }

    fn conflicted_file(path: &str, ours: &str, theirs: &str) -> ConflictedFile {
        ConflictedFile {
            path: PathBuf::from(path),
            ancestor: None,
            ours: Some(ours.to_string()),
            theirs: Some(theirs.to_string()),
        }
    }

    #[test]
    fn dispatch_conflict_take_ours_replaces_result_buffer() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let item = open_conflict_item_in_focused_pane(
            vcx,
            &ws,
            conflicted_file("a.txt", "alpha\n", "beta\n"),
        );

        dispatch(&ws, vcx, stoat_action::ConflictTakeOurs);
        vcx.run_until_parked();

        let text = item.read_with(vcx, |item, cx| item.result_buffer_text(cx));
        assert_eq!(text, "alpha\n");
    }

    #[test]
    fn dispatch_conflict_take_theirs_replaces_result_buffer() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let item = open_conflict_item_in_focused_pane(
            vcx,
            &ws,
            conflicted_file("a.txt", "alpha\n", "beta\n"),
        );

        dispatch(&ws, vcx, stoat_action::ConflictTakeTheirs);
        vcx.run_until_parked();

        let text = item.read_with(vcx, |item, cx| item.result_buffer_text(cx));
        assert_eq!(text, "beta\n");
    }

    #[test]
    fn dispatch_conflict_take_side_without_conflict_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::ConflictTakeOurs);
        vcx.run_until_parked();
        // The focused pane has no ConflictItem; the dispatch is a no-op.
    }

    fn activate_focused_pane_index(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
        index: usize,
    ) {
        ws.update(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            let pane = w
                .pane_tree
                .read(cx)
                .pane(focus)
                .expect("focused pane")
                .clone();
            pane.update(cx, |p, cx| {
                p.activate(index, cx);
            });
        });
        vcx.run_until_parked();
    }

    fn focused_pane_active_index(vcx: &mut VisualTestContext, ws: &Entity<Workspace>) -> usize {
        ws.read_with(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            w.pane_tree
                .read(cx)
                .pane(focus)
                .map(|p| p.read(cx).active_index())
                .unwrap_or(0)
        })
    }

    fn resolve_conflict_item_at(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
        target_index: usize,
    ) {
        let original = focused_pane_active_index(vcx, ws);
        activate_focused_pane_index(vcx, ws, target_index);
        dispatch(ws, vcx, stoat_action::ConflictTakeOurs);
        vcx.run_until_parked();
        activate_focused_pane_index(vcx, ws, original);
    }

    #[test]
    fn dispatch_conflict_next_file_activates_next_unresolved() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let _a =
            open_conflict_item_in_focused_pane(vcx, &ws, conflicted_file("a.txt", "a1\n", "a2\n"));
        let _b =
            open_conflict_item_in_focused_pane(vcx, &ws, conflicted_file("b.txt", "b1\n", "b2\n"));
        let _c =
            open_conflict_item_in_focused_pane(vcx, &ws, conflicted_file("c.txt", "c1\n", "c2\n"));
        activate_focused_pane_index(vcx, &ws, 0);

        dispatch(&ws, vcx, stoat_action::ConflictNextFile);
        vcx.run_until_parked();

        assert_eq!(focused_pane_active_index(vcx, &ws), 1);
    }

    #[test]
    fn dispatch_conflict_next_file_wraps_to_first() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let _a =
            open_conflict_item_in_focused_pane(vcx, &ws, conflicted_file("a.txt", "a1\n", "a2\n"));
        let _b =
            open_conflict_item_in_focused_pane(vcx, &ws, conflicted_file("b.txt", "b1\n", "b2\n"));
        let _c =
            open_conflict_item_in_focused_pane(vcx, &ws, conflicted_file("c.txt", "c1\n", "c2\n"));
        activate_focused_pane_index(vcx, &ws, 2);

        dispatch(&ws, vcx, stoat_action::ConflictNextFile);
        vcx.run_until_parked();

        assert_eq!(focused_pane_active_index(vcx, &ws), 0);
    }

    #[test]
    fn dispatch_conflict_next_file_skips_resolved() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let _a =
            open_conflict_item_in_focused_pane(vcx, &ws, conflicted_file("a.txt", "a1\n", "a2\n"));
        let _b =
            open_conflict_item_in_focused_pane(vcx, &ws, conflicted_file("b.txt", "b1\n", "b2\n"));
        let _c =
            open_conflict_item_in_focused_pane(vcx, &ws, conflicted_file("c.txt", "c1\n", "c2\n"));
        resolve_conflict_item_at(vcx, &ws, 1);
        activate_focused_pane_index(vcx, &ws, 0);

        dispatch(&ws, vcx, stoat_action::ConflictNextFile);
        vcx.run_until_parked();

        assert_eq!(focused_pane_active_index(vcx, &ws), 2);
    }

    #[test]
    fn dispatch_conflict_prev_file_activates_previous_unresolved() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let _a =
            open_conflict_item_in_focused_pane(vcx, &ws, conflicted_file("a.txt", "a1\n", "a2\n"));
        let _b =
            open_conflict_item_in_focused_pane(vcx, &ws, conflicted_file("b.txt", "b1\n", "b2\n"));
        let _c =
            open_conflict_item_in_focused_pane(vcx, &ws, conflicted_file("c.txt", "c1\n", "c2\n"));
        activate_focused_pane_index(vcx, &ws, 2);

        dispatch(&ws, vcx, stoat_action::ConflictPrevFile);
        vcx.run_until_parked();

        assert_eq!(focused_pane_active_index(vcx, &ws), 1);
    }

    #[test]
    fn dispatch_conflict_next_file_without_any_conflict_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::ConflictNextFile);
        vcx.run_until_parked();
        // The focused pane has no ConflictItems; the dispatch is a no-op.
    }

    #[test]
    fn dispatch_conflict_skip_entry_resolves_with_ancestor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let file = ConflictedFile {
            path: PathBuf::from("a.txt"),
            ancestor: Some("base\n".to_string()),
            ours: Some("alpha\n".to_string()),
            theirs: Some("beta\n".to_string()),
        };
        let item = open_conflict_item_in_focused_pane(vcx, &ws, file);

        dispatch(&ws, vcx, stoat_action::ConflictSkipEntry);
        vcx.run_until_parked();

        let text = item.read_with(vcx, |item, cx| item.result_buffer_text(cx));
        assert_eq!(text, "base\n");
    }

    fn rebase_entry(sha: &str, summary: &str) -> RebaseEntry {
        RebaseEntry {
            op: RebaseTodoOp::Pick,
            commit: stoat::host::CommitInfo {
                sha: sha.to_string(),
                short_sha: sha.chars().take(7).collect(),
                summary: summary.to_string(),
                author_name: "stoat".to_string(),
                author_email: "stoat@example.invalid".to_string(),
                time: 1_700_000_000,
                parent_count: 1,
            },
        }
    }

    fn install_conflict_rebase(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
        workdir: &str,
        source_sha: &str,
        current_head: &str,
        files: Vec<ConflictedFile>,
        remaining: Vec<RebaseEntry>,
    ) {
        ws.update(vcx, |w, _| {
            w.rebase_active = Some(ActiveRebase {
                workdir: PathBuf::from(workdir),
                onto: current_head.to_string(),
                remaining: remaining.into(),
                current_head: current_head.to_string(),
                last_pick_sha: None,
                last_message: None,
                pause: Some(RebasePause::Conflict {
                    source_sha: source_sha.to_string(),
                    files,
                    selected: 0,
                    resolutions: HashMap::new(),
                }),
            });
        });
    }

    fn focused_pane_conflict_item_count(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
    ) -> usize {
        ws.read_with(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            let Some(pane) = w.pane_tree.read(cx).pane(focus) else {
                return 0;
            };
            pane.read(cx)
                .items()
                .iter()
                .filter(|item| item.to_any_view().downcast::<ConflictItem>().is_ok())
                .count()
        })
    }

    fn head_sha(git: &Arc<stoat::host::fake::FakeGit>, workdir: &str) -> Option<String> {
        use stoat::host::GitHost;
        let repo = git.discover(Path::new(workdir))?;
        repo.log_commits(None, 1).first().map(|c| c.sha.clone())
    }

    #[test]
    fn dispatch_conflict_apply_creates_commit_with_resolved_tree() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("head1", &[("a.txt", "ours-content\n")]);
        install_git_host_global(vcx, git.clone());

        let item = open_conflict_item_in_focused_pane(
            vcx,
            &ws,
            conflicted_file("a.txt", "ours-content\n", "theirs-content\n"),
        );
        item.update(vcx, |item, cx| {
            item.set_result_buffer_text_for_test("resolved-text\n", cx);
        });
        install_conflict_rebase(
            vcx,
            &ws,
            "/tmp/repo",
            "src1",
            "head1",
            vec![conflicted_file(
                "a.txt",
                "ours-content\n",
                "theirs-content\n",
            )],
            Vec::new(),
        );

        dispatch(&ws, vcx, stoat_action::ConflictApply);
        vcx.run_until_parked();

        let new_head = head_sha(&git, "/tmp/repo").expect("HEAD points at the new commit");
        assert_ne!(new_head, "head1", "HEAD must advance off the parent");
        let message = git.commit_message(Path::new("/tmp/repo"), &new_head);
        assert_eq!(message, Some("conflict-resolved src1".to_string()));

        use stoat::host::GitHost;
        let repo = git
            .discover(Path::new("/tmp/repo"))
            .expect("repo discoverable at /tmp/repo");
        let new_tree = repo.commit_tree(&new_head).expect("new commit has a tree");
        assert_eq!(
            new_tree.get(&PathBuf::from("a.txt")).map(String::as_str),
            Some("resolved-text\n"),
        );
    }

    #[test]
    fn dispatch_conflict_apply_clears_rebase_active_when_remaining_empty() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("head1", &[("a.txt", "ours-content\n")]);
        install_git_host_global(vcx, git.clone());

        let item = open_conflict_item_in_focused_pane(
            vcx,
            &ws,
            conflicted_file("a.txt", "ours-content\n", "theirs-content\n"),
        );
        item.update(vcx, |item, cx| {
            item.set_result_buffer_text_for_test("resolved-text\n", cx);
        });
        install_conflict_rebase(
            vcx,
            &ws,
            "/tmp/repo",
            "src1",
            "head1",
            vec![conflicted_file(
                "a.txt",
                "ours-content\n",
                "theirs-content\n",
            )],
            Vec::new(),
        );

        dispatch(&ws, vcx, stoat_action::ConflictApply);
        vcx.run_until_parked();

        let still_active = ws.read_with(vcx, |w, _| w.rebase_active.is_some());
        assert!(
            !still_active,
            "rebase_active must clear when remaining is empty after apply",
        );
        assert_eq!(
            focused_pane_conflict_item_count(vcx, &ws),
            0,
            "ConflictItem views must be closed after a successful apply",
        );
    }

    #[test]
    fn dispatch_conflict_apply_with_no_rebase_active_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("head1", &[("a.txt", "ours-content\n")]);
        install_git_host_global(vcx, git.clone());

        let _ = open_conflict_item_in_focused_pane(
            vcx,
            &ws,
            conflicted_file("a.txt", "ours-content\n", "theirs-content\n"),
        );

        dispatch(&ws, vcx, stoat_action::ConflictApply);
        vcx.run_until_parked();

        let head = head_sha(&git, "/tmp/repo").expect("seeded HEAD remains");
        assert_eq!(
            head, "head1",
            "no rebase active: dispatch must not write any new commit",
        );
        assert_eq!(focused_pane_conflict_item_count(vcx, &ws), 1);
    }

    #[test]
    fn dispatch_conflict_apply_with_missing_view_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("head1", &[("a.txt", "x\n"), ("b.txt", "y\n")]);
        install_git_host_global(vcx, git.clone());

        let _ = open_conflict_item_in_focused_pane(
            vcx,
            &ws,
            conflicted_file("a.txt", "a-ours\n", "a-theirs\n"),
        );
        install_conflict_rebase(
            vcx,
            &ws,
            "/tmp/repo",
            "src1",
            "head1",
            vec![
                conflicted_file("a.txt", "a-ours\n", "a-theirs\n"),
                conflicted_file("b.txt", "b-ours\n", "b-theirs\n"),
            ],
            Vec::new(),
        );

        dispatch(&ws, vcx, stoat_action::ConflictApply);
        vcx.run_until_parked();

        let head = head_sha(&git, "/tmp/repo").expect("seeded HEAD remains");
        assert_eq!(
            head, "head1",
            "missing conflict view: dispatch must not write any new commit",
        );
        let still_active = ws.read_with(vcx, |w, _| w.rebase_active.is_some());
        assert!(still_active, "rebase_active must remain installed");
    }

    #[test]
    fn dispatch_conflict_apply_continues_with_run_rebase() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("head1", &[("a.txt", "ours-content\n")])
            .commit_with_parent("c2", "head1", &[("a.txt", "c2-content\n")]);
        install_git_host_global(vcx, git.clone());
        let scheduler = install_executor_for_rebase(vcx);

        let item = open_conflict_item_in_focused_pane(
            vcx,
            &ws,
            conflicted_file("a.txt", "ours-content\n", "theirs-content\n"),
        );
        item.update(vcx, |item, cx| {
            item.set_result_buffer_text_for_test("resolved-text\n", cx);
        });
        install_conflict_rebase(
            vcx,
            &ws,
            "/tmp/repo",
            "src1",
            "head1",
            vec![conflicted_file(
                "a.txt",
                "ours-content\n",
                "theirs-content\n",
            )],
            vec![rebase_entry("c2", "c2 summary")],
        );

        dispatch(&ws, vcx, stoat_action::ConflictApply);
        settle_rebase(&scheduler, vcx);

        let still_active = ws.read_with(vcx, |w, _| w.rebase_active.is_some());
        assert!(
            !still_active,
            "rebase_active must clear after the remaining plan completes cleanly",
        );
        let rebases = git.applied_rebases(Path::new("/tmp/repo"));
        assert_eq!(rebases.len(), 1, "exactly one run_rebase invocation");
        assert_eq!(
            rebases[0].todo,
            vec![RebaseTodo {
                op: RebaseTodoOp::Pick,
                sha: "c2".to_string(),
                message: "c2 summary".to_string(),
            }],
        );
    }

    #[test]
    fn dispatch_conflict_apply_opens_new_conflict_view_on_further_conflict() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("head1", &[("a.txt", "ours-content\n")])
            .commit_with_parent("c2", "head1", &[("b.txt", "c2-b-content\n")])
            .simulate_conflict_at("c2");
        install_git_host_global(vcx, git.clone());
        let scheduler = install_executor_for_rebase(vcx);

        let item = open_conflict_item_in_focused_pane(
            vcx,
            &ws,
            conflicted_file("a.txt", "ours-content\n", "theirs-content\n"),
        );
        item.update(vcx, |item, cx| {
            item.set_result_buffer_text_for_test("resolved-text\n", cx);
        });
        install_conflict_rebase(
            vcx,
            &ws,
            "/tmp/repo",
            "src1",
            "head1",
            vec![conflicted_file(
                "a.txt",
                "ours-content\n",
                "theirs-content\n",
            )],
            vec![rebase_entry("c2", "c2 summary")],
        );

        dispatch(&ws, vcx, stoat_action::ConflictApply);
        settle_rebase(&scheduler, vcx);

        let pause_sha = ws.read_with(vcx, |w, _| {
            match w.rebase_active.as_ref()?.pause.as_ref()? {
                RebasePause::Conflict { source_sha, .. } => Some(source_sha.clone()),
                _ => None,
            }
        });
        assert_eq!(
            pause_sha,
            Some("c2".to_string()),
            "a fresh conflict at c2 must install a new Conflict pause",
        );
        let remaining_len = ws.read_with(vcx, |w, _| {
            w.rebase_active
                .as_ref()
                .expect("rebase active")
                .remaining
                .len()
        });
        assert_eq!(
            remaining_len, 0,
            "c2 is dropped from remaining when its conflict becomes the active pause",
        );
        assert!(
            focused_pane_conflict_item_count(vcx, &ws) >= 1,
            "ConflictItem views for c2's conflicted files must be opened",
        );
    }

    #[test]
    fn dispatch_conflict_abort_resets_head_to_original_onto() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("head1", &[("a.txt", "ours-content\n")])
            .commit_with_parent("dangling", "head1", &[("a.txt", "dangling-content\n")])
            .set_head("dangling");
        install_git_host_global(vcx, git.clone());

        let _ = open_conflict_item_in_focused_pane(
            vcx,
            &ws,
            conflicted_file("a.txt", "ours-content\n", "theirs-content\n"),
        );
        install_conflict_rebase(
            vcx,
            &ws,
            "/tmp/repo",
            "src1",
            "head1",
            vec![conflicted_file(
                "a.txt",
                "ours-content\n",
                "theirs-content\n",
            )],
            Vec::new(),
        );

        dispatch(&ws, vcx, stoat_action::ConflictAbort);
        vcx.run_until_parked();

        let head = head_sha(&git, "/tmp/repo").expect("repo has HEAD");
        assert_eq!(
            head, "head1",
            "ConflictAbort must restore HEAD to the rebase's original onto",
        );
    }

    #[test]
    fn dispatch_conflict_abort_clears_rebase_active_and_closes_views() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("head1", &[("a.txt", "x\n"), ("b.txt", "y\n")]);
        install_git_host_global(vcx, git.clone());

        let _ = open_conflict_item_in_focused_pane(
            vcx,
            &ws,
            conflicted_file("a.txt", "a-ours\n", "a-theirs\n"),
        );
        let _ = open_conflict_item_in_focused_pane(
            vcx,
            &ws,
            conflicted_file("b.txt", "b-ours\n", "b-theirs\n"),
        );
        install_conflict_rebase(
            vcx,
            &ws,
            "/tmp/repo",
            "src1",
            "head1",
            vec![
                conflicted_file("a.txt", "a-ours\n", "a-theirs\n"),
                conflicted_file("b.txt", "b-ours\n", "b-theirs\n"),
            ],
            Vec::new(),
        );

        dispatch(&ws, vcx, stoat_action::ConflictAbort);
        vcx.run_until_parked();

        let still_active = ws.read_with(vcx, |w, _| w.rebase_active.is_some());
        assert!(!still_active, "rebase_active must clear on abort");
        assert_eq!(
            focused_pane_conflict_item_count(vcx, &ws),
            0,
            "every open ConflictItem view must close on abort",
        );
    }

    #[test]
    fn dispatch_conflict_abort_with_no_rebase_active_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("head1", &[("a.txt", "x\n")]);
        install_git_host_global(vcx, git.clone());

        dispatch(&ws, vcx, stoat_action::ConflictAbort);
        vcx.run_until_parked();

        let head = head_sha(&git, "/tmp/repo").expect("seeded HEAD remains");
        assert_eq!(
            head, "head1",
            "no rebase active: dispatch must not call update_head",
        );
    }

    fn install_executor_for_rebase(
        vcx: &mut VisualTestContext,
    ) -> Arc<stoat_scheduler::TestScheduler> {
        use crate::globals::ExecutorGlobal;
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let executor = scheduler.executor();
        vcx.update(|_, cx| {
            cx.set_global(ExecutorGlobal(executor));
        });
        scheduler
    }

    fn settle_rebase(scheduler: &Arc<stoat_scheduler::TestScheduler>, vcx: &mut VisualTestContext) {
        for _ in 0..4 {
            scheduler.run_until_parked();
            vcx.run_until_parked();
        }
    }

    fn open_rebase_item_in_focused_pane(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
        state: stoat::rebase::RebaseState,
    ) -> Entity<RebaseItem> {
        let item = vcx.update(|_, cx| cx.new(|cx| RebaseItem::new(state, cx)));
        ws.update(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            let pane = w
                .pane_tree
                .read(cx)
                .pane(focus)
                .expect("focused pane")
                .clone();
            let handle: Box<dyn ItemHandle> = Box::new(item.clone());
            pane.update(cx, |p, cx| {
                p.add_item(handle, cx);
            });
        });
        vcx.run_until_parked();
        item
    }

    fn rebase_state_with(entries: Vec<RebaseEntry>) -> stoat::rebase::RebaseState {
        stoat::rebase::RebaseState::new(PathBuf::from("/tmp/repo"), "onto1".to_string(), entries)
    }

    fn replace_reword_editor_text(
        vcx: &mut VisualTestContext,
        modal: &Entity<crate::reword_modal::RewordModal>,
        text: &str,
    ) {
        let buffer = modal.read_with(vcx, |m, cx| {
            m.editor()
                .read(cx)
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("auto-height editor singleton buffer")
                .clone()
        });
        buffer.update(vcx, |b, cx| {
            let len = b.text().len();
            b.edit(0..len, text, cx);
        });
        vcx.run_until_parked();
    }

    fn pick_entry(sha: &str, summary: &str) -> RebaseEntry {
        rebase_entry_with_op(sha, summary, RebaseTodoOp::Pick)
    }

    fn rebase_entry_with_op(sha: &str, summary: &str, op: RebaseTodoOp) -> RebaseEntry {
        RebaseEntry {
            op,
            commit: stoat::host::CommitInfo {
                sha: sha.to_string(),
                short_sha: sha.chars().take(7).collect(),
                summary: summary.to_string(),
                author_name: "Alice".to_string(),
                author_email: "alice@example.invalid".to_string(),
                time: 1_700_000_000,
                parent_count: 1,
            },
        }
    }

    fn focused_pane_rebase_item_count(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
    ) -> usize {
        ws.read_with(vcx, |w, cx| {
            let focus = w.pane_tree.read(cx).focus();
            let Some(pane) = w.pane_tree.read(cx).pane(focus) else {
                return 0;
            };
            pane.read(cx)
                .items()
                .iter()
                .filter(|item| item.to_any_view().downcast::<RebaseItem>().is_ok())
                .count()
        })
    }

    fn read_rebase_selected(vcx: &mut VisualTestContext, item: &Entity<RebaseItem>) -> usize {
        item.read_with(vcx, |item, cx| item.state().read(cx).selected)
    }

    fn read_rebase_todo_shas(
        vcx: &mut VisualTestContext,
        item: &Entity<RebaseItem>,
    ) -> Vec<String> {
        item.read_with(vcx, |item, cx| {
            item.state()
                .read(cx)
                .todo
                .iter()
                .map(|e| e.commit.sha.clone())
                .collect()
        })
    }

    fn read_rebase_op_at(
        vcx: &mut VisualTestContext,
        item: &Entity<RebaseItem>,
        index: usize,
    ) -> RebaseTodoOp {
        item.read_with(vcx, |item, cx| item.state().read(cx).todo[index].op)
    }

    #[test]
    fn dispatch_rebase_next_advances_selected() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let item = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![
                pick_entry("c1", "one"),
                pick_entry("c2", "two"),
                pick_entry("c3", "three"),
            ]),
        );

        dispatch(&ws, vcx, stoat_action::RebaseNext);
        vcx.run_until_parked();

        assert_eq!(read_rebase_selected(vcx, &item), 1);
    }

    #[test]
    fn dispatch_rebase_prev_decrements_selected() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let item = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![pick_entry("c1", "one"), pick_entry("c2", "two")]),
        );
        dispatch(&ws, vcx, stoat_action::RebaseNext);
        vcx.run_until_parked();
        assert_eq!(read_rebase_selected(vcx, &item), 1);

        dispatch(&ws, vcx, stoat_action::RebasePrev);
        vcx.run_until_parked();

        assert_eq!(read_rebase_selected(vcx, &item), 0);
    }

    #[test]
    fn dispatch_rebase_move_up_swaps_with_predecessor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let item = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![pick_entry("c1", "one"), pick_entry("c2", "two")]),
        );
        dispatch(&ws, vcx, stoat_action::RebaseNext);
        vcx.run_until_parked();

        dispatch(&ws, vcx, stoat_action::RebaseMoveUp);
        vcx.run_until_parked();

        assert_eq!(
            read_rebase_todo_shas(vcx, &item),
            vec!["c2".to_string(), "c1".to_string()],
        );
        assert_eq!(read_rebase_selected(vcx, &item), 0);
    }

    #[test]
    fn dispatch_rebase_move_down_swaps_with_successor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let item = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![pick_entry("c1", "one"), pick_entry("c2", "two")]),
        );

        dispatch(&ws, vcx, stoat_action::RebaseMoveDown);
        vcx.run_until_parked();

        assert_eq!(
            read_rebase_todo_shas(vcx, &item),
            vec!["c2".to_string(), "c1".to_string()],
        );
        assert_eq!(read_rebase_selected(vcx, &item), 1);
    }

    #[test]
    fn dispatch_set_rebase_op_changes_op() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let item = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![pick_entry("c1", "one")]),
        );

        dispatch(&ws, vcx, stoat_action::SetRebaseOpDrop);
        vcx.run_until_parked();

        assert_eq!(read_rebase_op_at(vcx, &item, 0), RebaseTodoOp::Drop);
    }

    #[test]
    fn dispatch_execute_rebase_runs_clean_plan() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("onto1", &[("a.txt", "base\n")])
            .commit_with_parent("c1", "onto1", &[("a.txt", "c1\n")]);
        install_git_host_global(vcx, git.clone());
        let scheduler = install_executor_for_rebase(vcx);

        let _ = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![pick_entry("c1", "c1 summary")]),
        );

        dispatch(&ws, vcx, stoat_action::ExecuteRebase);
        settle_rebase(&scheduler, vcx);

        let rebases = git.applied_rebases(Path::new("/tmp/repo"));
        assert_eq!(rebases.len(), 1);
        assert_eq!(rebases[0].onto, "onto1");
        let active = ws.read_with(vcx, |w, _| w.rebase_active.is_some());
        assert!(!active, "rebase_active must clear on clean completion");
        assert_eq!(
            focused_pane_rebase_item_count(vcx, &ws),
            0,
            "RebaseItem must close after execute consumes the plan",
        );
    }

    #[test]
    fn dispatch_execute_rebase_defers_run_rebase_to_executor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("onto1", &[("a.txt", "base\n")])
            .commit_with_parent("c1", "onto1", &[("a.txt", "c1\n")]);
        install_git_host_global(vcx, git.clone());
        let scheduler = install_executor_for_rebase(vcx);

        let _ = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![pick_entry("c1", "c1 summary")]),
        );

        dispatch(&ws, vcx, stoat_action::ExecuteRebase);
        vcx.run_until_parked();

        assert!(
            ws.read_with(vcx, |w, _| w.rebase_active.is_some()),
            "rebase_active stays installed until the executor task completes",
        );
        assert!(
            git.applied_rebases(Path::new("/tmp/repo")).is_empty(),
            "run_rebase has not run yet -- it's queued on the executor scheduler",
        );

        settle_rebase(&scheduler, vcx);

        assert!(
            !ws.read_with(vcx, |w, _| w.rebase_active.is_some()),
            "rebase_active clears once the executor task lands its outcome",
        );
        assert_eq!(git.applied_rebases(Path::new("/tmp/repo")).len(), 1);
    }

    #[test]
    fn dispatch_execute_rebase_opens_conflict_views_on_conflict() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("onto1", &[("a.txt", "base\n")])
            .commit_with_parent("c1", "onto1", &[("a.txt", "c1\n")])
            .simulate_conflict_at("c1");
        install_git_host_global(vcx, git.clone());
        let scheduler = install_executor_for_rebase(vcx);

        let _ = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![pick_entry("c1", "c1 summary")]),
        );

        dispatch(&ws, vcx, stoat_action::ExecuteRebase);
        settle_rebase(&scheduler, vcx);

        let pause_sha = ws.read_with(vcx, |w, _| {
            match w.rebase_active.as_ref()?.pause.as_ref()? {
                RebasePause::Conflict { source_sha, .. } => Some(source_sha.clone()),
                _ => None,
            }
        });
        assert_eq!(pause_sha, Some("c1".to_string()));
        assert!(
            focused_pane_conflict_item_count(vcx, &ws) >= 1,
            "conflict views must be opened on conflict outcome",
        );
    }

    #[test]
    fn dispatch_execute_rebase_stepper_handles_reword_pause() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit_with_message("onto1", "onto1 msg", &[("a.txt", "base\n")])
            .commit_with_parent_message("c1", "onto1", "c1 msg", &[("a.txt", "c1\n")]);
        install_git_host_global(vcx, git.clone());
        let scheduler = install_executor_for_rebase(vcx);

        let _ = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![rebase_entry_with_op(
                "c1",
                "c1 summary",
                RebaseTodoOp::Reword,
            )]),
        );

        dispatch(&ws, vcx, stoat_action::ExecuteRebase);
        settle_rebase(&scheduler, vcx);

        ws.read_with(vcx, |w, cx| {
            let active = w.rebase_active.as_ref().expect("rebase_active set");
            assert!(matches!(
                active.pause.as_ref(),
                Some(RebasePause::Reword { .. })
            ));
            assert!(
                active.remaining.is_empty(),
                "Reword entry consumed before installing the pause",
            );
            assert!(
                w.modal_layer()
                    .read(cx)
                    .active_modal::<crate::reword_modal::RewordModal>()
                    .is_some(),
                "Reword pause must open a RewordModal",
            );
        });
    }

    #[test]
    fn dispatch_reword_confirm_creates_commit_and_advances_stepper() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit_with_message("onto1", "onto1 msg", &[("a.txt", "base\n")])
            .commit_with_parent_message("c1", "onto1", "c1 msg", &[("a.txt", "c1\n")]);
        install_git_host_global(vcx, git.clone());
        let scheduler = install_executor_for_rebase(vcx);

        let _ = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![rebase_entry_with_op(
                "c1",
                "c1 summary",
                RebaseTodoOp::Reword,
            )]),
        );

        dispatch(&ws, vcx, stoat_action::ExecuteRebase);
        settle_rebase(&scheduler, vcx);

        let modal = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<crate::reword_modal::RewordModal>()
            })
            .expect("RewordModal open after pause");
        replace_reword_editor_text(vcx, &modal, "new message");

        dispatch(&ws, vcx, stoat_action::RewordConfirm);
        settle_rebase(&scheduler, vcx);

        ws.read_with(vcx, |w, cx| {
            assert!(
                w.rebase_active.is_none(),
                "rebase_active must clear after a clean reword confirm",
            );
            assert!(
                w.modal_layer()
                    .read(cx)
                    .active_modal::<crate::reword_modal::RewordModal>()
                    .is_none(),
                "RewordModal must dismiss after confirm",
            );
        });

        let new_head = head_sha(&git, "/tmp/repo").expect("HEAD points at the rewritten commit");
        let message = git.commit_message(Path::new("/tmp/repo"), &new_head);
        assert_eq!(message, Some("new message".to_string()));
    }

    #[test]
    fn dispatch_reword_confirm_with_empty_message_aborts() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit_with_message("onto1", "onto1 msg", &[("a.txt", "base\n")])
            .commit_with_parent_message("c1", "onto1", "c1 msg", &[("a.txt", "c1\n")]);
        install_git_host_global(vcx, git.clone());
        let scheduler = install_executor_for_rebase(vcx);

        let _ = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![rebase_entry_with_op(
                "c1",
                "c1 summary",
                RebaseTodoOp::Reword,
            )]),
        );

        dispatch(&ws, vcx, stoat_action::ExecuteRebase);
        settle_rebase(&scheduler, vcx);

        let modal = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<crate::reword_modal::RewordModal>()
            })
            .expect("RewordModal open after pause");
        replace_reword_editor_text(vcx, &modal, "   \n  \t  ");

        dispatch(&ws, vcx, stoat_action::RewordConfirm);
        vcx.run_until_parked();

        ws.read_with(vcx, |w, cx| {
            assert!(
                w.rebase_active.is_none(),
                "whitespace-only confirm must auto-abort the rebase",
            );
            assert!(
                w.modal_layer()
                    .read(cx)
                    .active_modal::<crate::reword_modal::RewordModal>()
                    .is_none(),
                "RewordModal must dismiss after auto-abort",
            );
        });
    }

    #[test]
    fn dispatch_reword_abort_clears_rebase_active() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit_with_message("onto1", "onto1 msg", &[("a.txt", "base\n")])
            .commit_with_parent_message("c1", "onto1", "c1 msg", &[("a.txt", "c1\n")]);
        install_git_host_global(vcx, git.clone());
        let scheduler = install_executor_for_rebase(vcx);

        let _ = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![rebase_entry_with_op(
                "c1",
                "c1 summary",
                RebaseTodoOp::Reword,
            )]),
        );

        dispatch(&ws, vcx, stoat_action::ExecuteRebase);
        settle_rebase(&scheduler, vcx);

        dispatch(&ws, vcx, stoat_action::RewordAbort);
        vcx.run_until_parked();

        ws.read_with(vcx, |w, cx| {
            assert!(
                w.rebase_active.is_none(),
                "RewordAbort must clear rebase_active",
            );
            assert!(
                w.modal_layer()
                    .read(cx)
                    .active_modal::<crate::reword_modal::RewordModal>()
                    .is_none(),
                "RewordAbort must dismiss the modal",
            );
        });
    }

    #[test]
    fn dispatch_execute_rebase_stepper_handles_edit_pause() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit_with_message("onto1", "onto1 msg", &[("a.txt", "base\n")])
            .commit_with_parent_message("c1", "onto1", "c1 msg", &[("a.txt", "c1\n")]);
        install_git_host_global(vcx, git.clone());
        install_full_globals(vcx, Arc::new(stoat::host::FakeFs::new()), git.clone());
        let scheduler = install_executor_for_rebase(vcx);

        let _ = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![rebase_entry_with_op(
                "c1",
                "c1 summary",
                RebaseTodoOp::Edit,
            )]),
        );

        dispatch(&ws, vcx, stoat_action::ExecuteRebase);
        settle_rebase(&scheduler, vcx);

        ws.read_with(vcx, |w, _| {
            let active = w.rebase_active.as_ref().expect("rebase_active set");
            assert!(matches!(
                active.pause.as_ref(),
                Some(RebasePause::Edit { .. })
            ));
        });
    }

    #[test]
    fn dispatch_execute_rebase_stepper_drops_then_reword_pauses() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit_with_message("onto1", "onto1 msg", &[("a.txt", "base\n")])
            .commit_with_parent_message("c1", "onto1", "c1 msg", &[("a.txt", "c1\n")])
            .commit_with_parent_message("c2", "c1", "c2 msg", &[("a.txt", "c2\n")]);
        install_git_host_global(vcx, git.clone());
        let scheduler = install_executor_for_rebase(vcx);

        let _ = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![
                rebase_entry_with_op("c1", "c1 summary", RebaseTodoOp::Drop),
                rebase_entry_with_op("c2", "c2 summary", RebaseTodoOp::Reword),
            ]),
        );

        dispatch(&ws, vcx, stoat_action::ExecuteRebase);
        settle_rebase(&scheduler, vcx);

        ws.read_with(vcx, |w, _| {
            let active = w.rebase_active.as_ref().expect("rebase_active set");
            assert!(matches!(
                active.pause.as_ref(),
                Some(RebasePause::Reword { .. }),
            ));
            assert!(
                active.remaining.is_empty(),
                "Drop + Reword consumed everything",
            );
        });
    }

    #[test]
    fn dispatch_execute_rebase_stepper_handles_conflict_mid_plan() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit_with_message("onto1", "onto1 msg", &[("a.txt", "base\n")])
            .commit_with_parent_message("c1", "onto1", "c1 msg", &[("a.txt", "c1\n")])
            .commit_with_parent_message("c2", "c1", "c2 msg", &[("a.txt", "c2\n")])
            .simulate_conflict_at("c2");
        install_git_host_global(vcx, git.clone());
        let scheduler = install_executor_for_rebase(vcx);

        let _ = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![
                rebase_entry_with_op("c1", "c1 summary", RebaseTodoOp::Pick),
                rebase_entry_with_op("c2", "c2 summary", RebaseTodoOp::Edit),
            ]),
        );

        dispatch(&ws, vcx, stoat_action::ExecuteRebase);
        settle_rebase(&scheduler, vcx);

        let pause_sha = ws.read_with(vcx, |w, _| {
            match w.rebase_active.as_ref()?.pause.as_ref()? {
                RebasePause::Conflict { source_sha, .. } => Some(source_sha.clone()),
                _ => None,
            }
        });
        assert_eq!(pause_sha, Some("c2".to_string()));
        assert!(
            focused_pane_conflict_item_count(vcx, &ws) >= 1,
            "conflict views must be opened when the stepper hits a conflict",
        );
    }

    #[test]
    fn dispatch_execute_rebase_rejects_dirty_worktree() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("onto1", &[("a.txt", "base\n")])
            .modified("a.txt", "base\n", "dirty\n");
        install_git_host_global(vcx, git.clone());

        let _ = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![pick_entry("c1", "c1 summary")]),
        );

        dispatch(&ws, vcx, stoat_action::ExecuteRebase);
        vcx.run_until_parked();

        assert!(
            git.applied_rebases(Path::new("/tmp/repo")).is_empty(),
            "dirty worktree must reject the rebase before running",
        );
        let active = ws.read_with(vcx, |w, _| w.rebase_active.is_some());
        assert!(
            !active,
            "rebase_active must not be installed on dirty reject"
        );
        assert_eq!(
            focused_pane_rebase_item_count(vcx, &ws),
            1,
            "RebaseItem must remain open when execute is rejected",
        );
    }

    #[test]
    fn dispatch_execute_rebase_with_no_rebase_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("onto1", &[("a.txt", "base\n")]);
        install_git_host_global(vcx, git.clone());

        dispatch(&ws, vcx, stoat_action::ExecuteRebase);
        vcx.run_until_parked();

        assert!(git.applied_rebases(Path::new("/tmp/repo")).is_empty());
        let active = ws.read_with(vcx, |w, _| w.rebase_active.is_some());
        assert!(!active);
    }

    #[test]
    fn dispatch_abort_rebase_closes_active_rebase_item() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let _ = open_rebase_item_in_focused_pane(
            vcx,
            &ws,
            rebase_state_with(vec![pick_entry("c1", "one")]),
        );

        dispatch(&ws, vcx, stoat_action::AbortRebase);
        vcx.run_until_parked();

        assert_eq!(focused_pane_rebase_item_count(vcx, &ws), 0);
    }

    #[test]
    fn dispatch_abort_rebase_with_no_rebase_item_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::AbortRebase);
        vcx.run_until_parked();
    }

    fn install_edit_pause(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
        workdir: &str,
        current_head: &str,
        cherry_picked: &str,
        remaining: Vec<RebaseEntry>,
    ) {
        ws.update(vcx, |w, _| {
            w.rebase_active = Some(ActiveRebase {
                workdir: PathBuf::from(workdir),
                onto: current_head.to_string(),
                remaining: remaining.into(),
                current_head: current_head.to_string(),
                last_pick_sha: None,
                last_message: None,
                pause: Some(RebasePause::Edit {
                    cherry_picked_commit: cherry_picked.to_string(),
                }),
            });
        });
    }

    #[test]
    fn dispatch_rebase_continue_with_edit_pause_drives_forward() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("head1", &[("a.txt", "x\n")])
            .commit_with_parent("c2", "head1", &[("a.txt", "y\n")]);
        install_git_host_global(vcx, git.clone());
        let scheduler = install_executor_for_rebase(vcx);
        install_edit_pause(
            vcx,
            &ws,
            "/tmp/repo",
            "head1",
            "edited-c1",
            vec![pick_entry("c2", "c2")],
        );

        dispatch(&ws, vcx, stoat_action::RebaseContinue);
        settle_rebase(&scheduler, vcx);

        let active = ws.read_with(vcx, |w, _| w.rebase_active.is_some());
        assert!(!active, "rebase_active must clear after clean continuation");
        assert_eq!(
            git.applied_rebases(Path::new("/tmp/repo")).len(),
            1,
            "exactly one run_rebase invocation",
        );
    }

    #[test]
    fn dispatch_rebase_continue_without_pause_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("head1", &[("a.txt", "x\n")]);
        install_git_host_global(vcx, git.clone());

        dispatch(&ws, vcx, stoat_action::RebaseContinue);
        vcx.run_until_parked();

        assert!(git.applied_rebases(Path::new("/tmp/repo")).is_empty());
    }

    #[test]
    fn dispatch_rebase_continue_with_conflict_pause_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("head1", &[("a.txt", "x\n")]);
        install_git_host_global(vcx, git.clone());
        install_conflict_rebase(
            vcx,
            &ws,
            "/tmp/repo",
            "src1",
            "head1",
            vec![conflicted_file("a.txt", "ours\n", "theirs\n")],
            Vec::new(),
        );

        dispatch(&ws, vcx, stoat_action::RebaseContinue);
        vcx.run_until_parked();

        let pause_still_conflict = ws.read_with(vcx, |w, _| {
            matches!(
                w.rebase_active.as_ref().and_then(|a| a.pause.as_ref()),
                Some(RebasePause::Conflict { .. }),
            )
        });
        assert!(
            pause_still_conflict,
            "Conflict pause is resumed via ConflictApply, not RebaseContinue",
        );
        assert!(git.applied_rebases(Path::new("/tmp/repo")).is_empty());
    }

    fn open_editor_in_focused_pane(
        vcx: &mut VisualTestContext,
        ws: &Entity<Workspace>,
        path: &Path,
    ) -> Entity<Editor> {
        ws.update(vcx, |w, cx| w.open_paths(&[path.to_path_buf()], cx));
        vcx.run_until_parked();
        let editor = ws.read_with(vcx, |w, cx| {
            let pane_id = w.pane_tree().read(cx).focus();
            let pane = w
                .pane_tree()
                .read(cx)
                .pane(pane_id)
                .expect("focused pane")
                .clone();
            pane.read(cx)
                .active_item()
                .expect("editor active in pane")
                .to_any_view()
                .downcast::<Editor>()
                .expect("active item is Editor")
        });
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        editor
    }

    #[test]
    fn dispatch_toggle_blame_flips_visibility_on_active_editor() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/main.rs", b"hi\n");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo").head_file("main.rs", "hi\n");
        install_git_host_global(vcx, git);
        let editor = open_editor_in_focused_pane(vcx, &ws, Path::new("/tmp/repo/main.rs"));

        editor.read_with(vcx, |ed, _| assert!(!ed.blame_visible()));
        dispatch(&ws, vcx, stoat_action::ToggleBlame);
        vcx.run_until_parked();
        editor.read_with(vcx, |ed, _| assert!(ed.blame_visible()));

        dispatch(&ws, vcx, stoat_action::ToggleBlame);
        vcx.run_until_parked();
        editor.read_with(vcx, |ed, _| assert!(!ed.blame_visible()));
    }

    #[test]
    fn dispatch_toggle_inline_blame_flips_visibility_on_active_editor() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/main.rs", b"hi\n");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo").head_file("main.rs", "hi\n");
        install_git_host_global(vcx, git);
        let editor = open_editor_in_focused_pane(vcx, &ws, Path::new("/tmp/repo/main.rs"));

        editor.read_with(vcx, |ed, _| assert!(!ed.inline_blame_visible()));
        dispatch(&ws, vcx, stoat_action::ToggleInlineBlame);
        vcx.run_until_parked();
        editor.read_with(vcx, |ed, _| assert!(ed.inline_blame_visible()));

        dispatch(&ws, vcx, stoat_action::ToggleInlineBlame);
        vcx.run_until_parked();
        editor.read_with(vcx, |ed, _| assert!(!ed.inline_blame_visible()));
    }

    #[test]
    fn dispatch_set_applies_runtime_setting_via_settings_global() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        vcx.update(|_, cx| cx.set_global(Settings::default()));

        dispatch(
            &ws,
            vcx,
            stoat_action::Set {
                key: "ui.pane.show_tab_bar".into(),
                value: "false".into(),
            },
        );
        vcx.run_until_parked();

        let after = vcx.read(|cx| cx.global::<Settings>().resolved.ui_pane_show_tab_bar);
        assert_eq!(after, Some(false));
    }

    #[test]
    fn dispatch_set_unknown_key_leaves_settings_untouched() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        vcx.update(|_, cx| cx.set_global(Settings::default()));

        dispatch(
            &ws,
            vcx,
            stoat_action::Set {
                key: "nope.bad.path".into(),
                value: "true".into(),
            },
        );
        vcx.run_until_parked();

        let after = vcx.read(|cx| cx.global::<Settings>().resolved.ui_pane_show_tab_bar);
        assert_eq!(after, None);
    }

    #[test]
    fn dispatch_toggle_tab_bar_cycles_settings_global() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        vcx.update(|_, cx| cx.set_global(Settings::default()));

        let before = vcx.read(|cx| cx.global::<Settings>().resolved.ui_pane_show_tab_bar);
        assert_eq!(before, None);

        dispatch(&ws, vcx, stoat_action::ToggleTabBar);
        vcx.run_until_parked();
        let after_first = vcx.read(|cx| cx.global::<Settings>().resolved.ui_pane_show_tab_bar);
        assert_eq!(after_first, Some(false));

        dispatch(&ws, vcx, stoat_action::ToggleTabBar);
        vcx.run_until_parked();
        let after_second = vcx.read(|cx| cx.global::<Settings>().resolved.ui_pane_show_tab_bar);
        assert_eq!(after_second, Some(true));
    }

    #[test]
    fn dispatch_toggle_relative_line_numbers_cycles_settings_global() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        vcx.update(|_, cx| cx.set_global(Settings::default()));

        let before = vcx.read(|cx| cx.global::<Settings>().resolved.ui_editor_line_numbers);
        assert_eq!(before, None);

        dispatch(&ws, vcx, stoat_action::ToggleRelativeLineNumbers);
        vcx.run_until_parked();
        let first = vcx.read(|cx| cx.global::<Settings>().resolved.ui_editor_line_numbers);
        assert_eq!(first, Some(LineNumberMode::Relative));

        dispatch(&ws, vcx, stoat_action::ToggleRelativeLineNumbers);
        vcx.run_until_parked();
        let second = vcx.read(|cx| cx.global::<Settings>().resolved.ui_editor_line_numbers);
        assert_eq!(second, Some(LineNumberMode::Hybrid));

        dispatch(&ws, vcx, stoat_action::ToggleRelativeLineNumbers);
        vcx.run_until_parked();
        let third = vcx.read(|cx| cx.global::<Settings>().resolved.ui_editor_line_numbers);
        assert_eq!(third, Some(LineNumberMode::Absolute));
    }

    #[test]
    fn dispatch_toggle_minimap_flips_visibility_on_active_editor() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "alpha\nbeta\ngamma");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        editor.read_with(vcx, |ed, _| assert!(!ed.minimap_visible()));
        dispatch(&ws, vcx, stoat_action::ToggleMinimap);
        vcx.run_until_parked();
        editor.read_with(vcx, |ed, _| assert!(ed.minimap_visible()));

        dispatch(&ws, vcx, stoat_action::ToggleMinimap);
        vcx.run_until_parked();
        editor.read_with(vcx, |ed, _| assert!(!ed.minimap_visible()));
    }

    #[test]
    fn toggle_minimap_constructs_and_drops_minimap_child() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "alpha\nbeta\ngamma");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        editor.read_with(vcx, |ed, _| assert!(ed.minimap().is_none()));

        dispatch(&ws, vcx, stoat_action::ToggleMinimap);
        vcx.run_until_parked();
        let minimap = editor
            .read_with(vcx, |ed, _| ed.minimap().cloned())
            .expect("minimap child constructed on toggle-on");
        minimap.read_with(vcx, |mm, _| assert!(mm.mode().is_minimap()));

        dispatch(&ws, vcx, stoat_action::ToggleMinimap);
        vcx.run_until_parked();
        editor.read_with(vcx, |ed, _| assert!(ed.minimap().is_none()));
    }

    #[test]
    fn dispatch_toggle_diff_hunk_panel_adds_then_removes_dock() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let editor = new_singleton_editor(vcx, "alpha\nbeta\ngamma\ndelta");
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        editor.update(vcx, |ed, cx| {
            let hunks = vec![stoat::diff_map::DiffHunk {
                status: stoat::diff_map::DiffHunkStatus::Added,
                staged: false,
                buffer_start_line: 1,
                buffer_line_range: 1..2,
                base_byte_range: 0..0,
                anchor_range: None,
                token_detail: None,
            }];
            let new = stoat::DiffMap::from_hunks(hunks, None);
            ed.diff_map().update(cx, |dm, cx| dm.set_diff(new, cx));
        });
        vcx.run_until_parked();

        assert_eq!(ws.read_with(vcx, |w, _| w.docks().len()), 0);

        dispatch(&ws, vcx, stoat_action::ToggleDiffHunkPanel);
        vcx.run_until_parked();
        assert_eq!(ws.read_with(vcx, |w, _| w.docks().len()), 1);
        let docks = ws.read_with(vcx, |w, _| w.docks().to_vec());
        assert_eq!(docks[0].read_with(vcx, |d, _| d.side()), DockSide::Right);

        dispatch(&ws, vcx, stoat_action::ToggleDiffHunkPanel);
        vcx.run_until_parked();
        assert_eq!(ws.read_with(vcx, |w, _| w.docks().len()), 0);
    }

    #[test]
    fn dispatch_toggle_diff_hunk_panel_without_editor_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::ToggleDiffHunkPanel);
        vcx.run_until_parked();

        assert_eq!(ws.read_with(vcx, |w, _| w.docks().len()), 0);
    }

    #[test]
    fn space_shift_g_b_chord_toggles_blame_via_keymap() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/main.rs", b"hi\n");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo").head_file("main.rs", "hi\n");
        install_git_host_global(vcx, git);
        let editor = open_editor_in_focused_pane(vcx, &ws, Path::new("/tmp/repo/main.rs"));

        editor.read_with(vcx, |ed, _| assert!(!ed.blame_visible()));
        vcx.simulate_keystrokes("space shift-g b");
        vcx.run_until_parked();

        editor.read_with(vcx, |ed, _| assert!(ed.blame_visible()));
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.mode(), "normal");
        });
    }

    #[test]
    fn dispatch_toggle_blame_populates_blame_state_on_toggle_on() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/a.rs", b"l1\nl2\nl3\n");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let scheduler = install_executor_for_rebase(vcx);
        let git = Arc::new(stoat::host::fake::FakeGit::new());
        git.add_repo("/tmp/repo")
            .commit("c1", &[("a.rs", "l1\nl2\nl3\n")]);
        install_git_host_global(vcx, git);
        let editor = open_editor_in_focused_pane(vcx, &ws, Path::new("/tmp/repo/a.rs"));

        dispatch(&ws, vcx, stoat_action::ToggleBlame);
        settle_rebase(&scheduler, vcx);

        let state = editor
            .read_with(vcx, |ed, _| ed.blame_state().cloned())
            .expect("BlameState attached on toggle-on");
        let entries = state.read_with(vcx, |s, _| s.blame().to_vec());
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].line, 0);
        assert_eq!(entries[2].line, 2);
    }

    #[test]
    fn dispatch_toggle_blame_with_no_file_path_flips_flag_but_skips_refresh() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/a.rs", b"hi\n");
        install_globals_with_fs(&mut cx, fs);
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        install_git_host_global(vcx, Arc::new(stoat::host::fake::FakeGit::new()));
        let editor = open_editor_in_focused_pane(vcx, &ws, Path::new("/tmp/repo/a.rs"));

        let buffer = editor
            .read_with(vcx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton buffer");
        buffer.update(vcx, |b, cx| b.set_file_path(None, cx));
        vcx.run_until_parked();

        dispatch(&ws, vcx, stoat_action::ToggleBlame);
        vcx.run_until_parked();

        editor.read_with(vcx, |ed, _| {
            assert!(ed.blame_visible());
            assert!(ed.blame_state().is_none());
        });
    }

    fn make_permission_prompt(
        tool: &str,
    ) -> (
        stoat::host::PermissionPrompt,
        tokio::sync::oneshot::Receiver<stoat::host::ApprovalDecision>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let prompt = stoat::host::PermissionPrompt {
            tool: tool.to_string(),
            input: "{}".to_string(),
            response_tx: tx,
        };
        (prompt, rx)
    }

    fn active_permission_modal(
        ws: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
    ) -> Option<Entity<PermissionModal>> {
        ws.read_with(vcx, |w, cx| {
            w.modal_layer.read(cx).active_modal::<PermissionModal>()
        })
    }

    #[test]
    fn enqueue_permission_prompt_opens_modal_when_idle() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "perm-test", "/tmp/perm");
        let (prompt, _rx) = make_permission_prompt("Bash");

        ws.update_in(vcx, |w, window, cx| {
            w.enqueue_permission_prompt(prompt, window, cx);
        });
        vcx.run_until_parked();

        assert!(
            active_permission_modal(&ws, vcx).is_some(),
            "first enqueue opens modal immediately"
        );
        let len = ws.read_with(vcx, |w, _| w.permission_prompt_queue_len());
        assert_eq!(len, 0, "no queue entries when modal opens directly");
    }

    #[test]
    fn enqueue_permission_prompt_queues_when_modal_open() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "perm-test", "/tmp/perm");
        let (first, _rx1) = make_permission_prompt("Bash");
        let (second, _rx2) = make_permission_prompt("Read");

        ws.update_in(vcx, |w, window, cx| {
            w.enqueue_permission_prompt(first, window, cx);
            w.enqueue_permission_prompt(second, window, cx);
        });
        vcx.run_until_parked();

        let modal_active = active_permission_modal(&ws, vcx).is_some();
        let queue_len = ws.read_with(vcx, |w, _| w.permission_prompt_queue_len());
        assert!(modal_active, "first prompt opens the modal");
        assert_eq!(queue_len, 1, "second prompt waits behind the active modal");
    }

    #[test]
    fn dismissing_active_modal_opens_next_from_queue() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "perm-test", "/tmp/perm");
        let (first, _rx1) = make_permission_prompt("Bash");
        let (second, _rx2) = make_permission_prompt("Read");

        ws.update_in(vcx, |w, window, cx| {
            w.enqueue_permission_prompt(first, window, cx);
            w.enqueue_permission_prompt(second, window, cx);
        });
        vcx.run_until_parked();

        let active = active_permission_modal(&ws, vcx).expect("first modal active");
        active.update(vcx, |_, cx| cx.emit(DismissEvent));
        vcx.run_until_parked();

        assert!(
            active_permission_modal(&ws, vcx).is_some(),
            "next queued prompt opens after dismiss"
        );
        let queue_len = ws.read_with(vcx, |w, _| w.permission_prompt_queue_len());
        assert_eq!(queue_len, 0, "queue drained after dismiss promotes next");
    }

    #[test]
    fn queue_drains_in_fifo_order() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "perm-test", "/tmp/perm");
        let (first, _rx1) = make_permission_prompt("First");
        let (second, _rx2) = make_permission_prompt("Second");
        let (third, _rx3) = make_permission_prompt("Third");

        ws.update_in(vcx, |w, window, cx| {
            w.enqueue_permission_prompt(first, window, cx);
            w.enqueue_permission_prompt(second, window, cx);
            w.enqueue_permission_prompt(third, window, cx);
        });
        vcx.run_until_parked();

        let initial_tool = active_permission_modal(&ws, vcx)
            .expect("first modal")
            .read_with(vcx, |m, _| m.tool().to_string());
        assert_eq!(initial_tool, "First");

        active_permission_modal(&ws, vcx)
            .expect("first modal")
            .update(vcx, |_, cx| cx.emit(DismissEvent));
        vcx.run_until_parked();
        let second_tool = active_permission_modal(&ws, vcx)
            .expect("second modal")
            .read_with(vcx, |m, _| m.tool().to_string());
        assert_eq!(second_tool, "Second");

        active_permission_modal(&ws, vcx)
            .expect("second modal")
            .update(vcx, |_, cx| cx.emit(DismissEvent));
        vcx.run_until_parked();
        let third_tool = active_permission_modal(&ws, vcx)
            .expect("third modal")
            .read_with(vcx, |m, _| m.tool().to_string());
        assert_eq!(third_tool, "Third");
    }

    #[test]
    fn fresh_workspace_does_not_write_state() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        let path = PathBuf::from("/tmp/state/fresh.ron");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.read_with(vcx, |w, cx| {
            w.save_state(&path, &*fs_dyn, cx).expect("save");
        });
        assert!(
            !fs_dyn.exists(&path),
            "fresh workspace must not write state"
        );
    }

    #[test]
    fn save_then_restore_round_trips_pane_tree_and_editor_paths() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hello foo\n");
        fs.insert_file("/tmp/repo/bar.rs", b"hello bar\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update(vcx, |w, cx| {
            w.open_paths(
                &[
                    PathBuf::from("/tmp/repo/foo.rs"),
                    PathBuf::from("/tmp/repo/bar.rs"),
                ],
                cx,
            );
            w.mark_dirty();
        });
        vcx.run_until_parked();
        let pane_count_before = ws.read_with(vcx, |w, cx| w.pane_tree().read(cx).pane_count());
        let path = PathBuf::from("/tmp/state/round-trip.ron");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.read_with(vcx, |w, cx| {
            w.save_state(&path, &*fs_dyn, cx).expect("save");
        });
        assert!(fs_dyn.exists(&path), "dirty workspace must write");

        let (fresh_ws, vcx2) = new_workspace_in_window(&mut cx, "other", "/elsewhere");
        fresh_ws.update(vcx2, |w, cx| {
            w.restore_state(&path, &*fs_dyn, cx).expect("restore");
        });
        vcx2.run_until_parked();
        fresh_ws.read_with(vcx2, |w, cx| {
            assert_eq!(w.git_root(), Path::new("/tmp/repo"));
            assert_eq!(w.pane_tree().read(cx).pane_count(), pane_count_before);
            let mut paths_seen: Vec<PathBuf> = Vec::new();
            for id in w.pane_tree().read(cx).split_pane_ids() {
                let pane = w
                    .pane_tree()
                    .read(cx)
                    .pane(id)
                    .expect("pane present")
                    .read(cx);
                for item in pane.items() {
                    if let Ok(editor) = item.to_any_view().downcast::<Editor>() {
                        if let Some(p) = editor.read(cx).file_path() {
                            paths_seen.push(p.to_path_buf());
                        }
                    }
                }
            }
            paths_seen.sort();
            assert_eq!(
                paths_seen,
                vec![
                    PathBuf::from("/tmp/repo/bar.rs"),
                    PathBuf::from("/tmp/repo/foo.rs"),
                ]
            );
        });
    }

    #[test]
    fn save_then_restore_round_trips_minimap_visibility() {
        fn focused_editor(
            ws: &Entity<Workspace>,
            vcx: &mut VisualTestContext,
        ) -> Option<Entity<Editor>> {
            ws.read_with(vcx, |w, cx| {
                let tree = w.pane_tree().read(cx);
                let pane = tree.pane(tree.focus()).expect("focused pane").read(cx);
                pane.active_item()
                    .and_then(|item| item.to_any_view().downcast::<Editor>().ok())
            })
        }

        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hello foo\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx);
            w.mark_dirty();
        });
        vcx.run_until_parked();

        focused_editor(&ws, vcx)
            .expect("active editor")
            .update(vcx, |ed, cx| ed.set_minimap_visible(true, cx));

        let path = PathBuf::from("/tmp/state/minimap.ron");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.read_with(vcx, |w, cx| {
            w.save_state(&path, &*fs_dyn, cx).expect("save");
        });

        let (fresh_ws, vcx2) = new_workspace_in_window(&mut cx, "other", "/elsewhere");
        fresh_ws.update(vcx2, |w, cx| {
            w.restore_state(&path, &*fs_dyn, cx).expect("restore");
        });
        vcx2.run_until_parked();

        let restored = focused_editor(&fresh_ws, vcx2).expect("restored editor");
        assert!(
            restored.read_with(vcx2, |ed, _| ed.minimap_visible()),
            "minimap visibility must round-trip as visible"
        );
    }

    #[test]
    fn save_then_restore_round_trips_command_palette_history() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update(vcx, |w, _| {
            w.mark_dirty();
            w.push_command_palette_query("open file".to_string());
            w.push_command_palette_query("quit".to_string());
        });

        let fs: Arc<dyn stoat::host::FsHost> = Arc::new(stoat::host::FakeFs::new());
        let path = PathBuf::from("/tmp/state/palette_history.ron");
        ws.read_with(vcx, |w, cx| {
            w.save_state(&path, &*fs, cx).expect("save");
        });

        let (fresh_ws, vcx2) = new_workspace_in_window(&mut cx, "other", "/elsewhere");
        fresh_ws.update(vcx2, |w, cx| {
            w.restore_state(&path, &*fs, cx).expect("restore");
        });
        vcx2.run_until_parked();

        assert_eq!(
            fresh_ws.read_with(vcx2, |w, _| w.command_palette_history().clone()),
            VecDeque::from(vec!["open file".to_string(), "quit".to_string()]),
            "confirmed query history must round-trip oldest-first",
        );
    }

    #[test]
    fn restore_carries_uid_across_save_load() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"x\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx);
            w.mark_dirty();
        });
        vcx.run_until_parked();
        let original_uid = ws.read_with(vcx, |w, _| w.uid());
        let path = PathBuf::from("/tmp/state/uid.ron");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.read_with(vcx, |w, cx| {
            w.save_state(&path, &*fs_dyn, cx).expect("save");
        });

        let (fresh_ws, vcx2) = new_workspace_in_window(&mut cx, "other", "/elsewhere");
        fresh_ws.update(vcx2, |w, cx| {
            w.restore_state(&path, &*fs_dyn, cx).expect("restore");
        });
        let restored_uid = fresh_ws.read_with(vcx2, |w, _| w.uid());
        assert_eq!(restored_uid, original_uid);
    }

    #[test]
    fn restore_preserves_editor_folds() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/fold.rs", b"fn main() {\n    body;\n}\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/fold.rs")], cx);
        });
        vcx.run_until_parked();
        ws.update(vcx, |w, cx| {
            let editor = w.active_editor(cx).expect("active editor");
            let display_map = editor.read(cx).display_map().clone();
            display_map.update(cx, |dm, dm_cx| {
                dm.fold(
                    vec![stoat_text::Point::new(0, 11)..stoat_text::Point::new(2, 0)],
                    dm_cx,
                )
            });
            w.mark_dirty();
        });
        vcx.run_until_parked();

        let path = PathBuf::from("/tmp/state/folds.ron");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.read_with(vcx, |w, cx| {
            w.save_state(&path, &*fs_dyn, cx).expect("save");
        });

        let (fresh_ws, vcx2) = new_workspace_in_window(&mut cx, "other", "/tmp/repo");
        fresh_ws.update(vcx2, |w, cx| {
            w.restore_state(&path, &*fs_dyn, cx).expect("restore");
        });
        vcx2.run_until_parked();

        let folded = fresh_ws.update(vcx2, |w, cx| {
            let editor = w.active_editor(cx).expect("restored editor");
            let display_map = editor.read(cx).display_map().clone();
            display_map
                .update(cx, |dm, _| dm.snapshot())
                .is_line_folded(1)
        });
        assert!(folded, "restored editor keeps the persisted fold");
    }

    #[test]
    fn restore_rebuilds_dock_with_editor_item_at_saved_side() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/outline.rs", b"// outline\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/outline.rs")], cx);
            let dock_editor = w.build_editor_for_path(Path::new("/tmp/repo/outline.rs"), cx);
            w.add_dock(Box::new(dock_editor), DockSide::Left, 220, cx);
            w.mark_dirty();
        });
        vcx.run_until_parked();
        let path = PathBuf::from("/tmp/state/dock-round-trip.ron");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.read_with(vcx, |w, cx| {
            w.save_state(&path, &*fs_dyn, cx).expect("save");
        });

        let (fresh_ws, vcx2) = new_workspace_in_window(&mut cx, "other", "/elsewhere");
        fresh_ws.update(vcx2, |w, cx| {
            w.restore_state(&path, &*fs_dyn, cx).expect("restore");
        });
        vcx2.run_until_parked();
        fresh_ws.read_with(vcx2, |w, cx| {
            assert_eq!(w.docks().len(), 1);
            let dock = w.docks()[0].read(cx);
            assert_eq!(dock.side(), DockSide::Left);
            assert_eq!(dock.default_extent(), 220);
            let editor = dock
                .item()
                .to_any_view()
                .downcast::<Editor>()
                .expect("dock holds an Editor");
            let path = editor.read(cx).file_path().map(Path::to_path_buf);
            assert_eq!(path, Some(PathBuf::from("/tmp/repo/outline.rs")));
        });
    }

    #[test]
    fn restore_rebuilds_bottom_dock_at_saved_side() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/term.rs", b"// term\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update(vcx, |w, cx| {
            let dock_editor = w.build_editor_for_path(Path::new("/tmp/repo/term.rs"), cx);
            w.add_dock(Box::new(dock_editor), DockSide::Bottom, 200, cx);
            w.mark_dirty();
        });
        vcx.run_until_parked();
        let path = PathBuf::from("/tmp/state/bottom-dock.ron");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.read_with(vcx, |w, cx| {
            w.save_state(&path, &*fs_dyn, cx).expect("save");
        });

        let (fresh_ws, vcx2) = new_workspace_in_window(&mut cx, "other", "/elsewhere");
        fresh_ws.update(vcx2, |w, cx| {
            w.restore_state(&path, &*fs_dyn, cx).expect("restore");
        });
        vcx2.run_until_parked();
        fresh_ws.read_with(vcx2, |w, cx| {
            assert_eq!(w.docks().len(), 1);
            let dock = w.docks()[0].read(cx);
            assert_eq!(dock.side(), DockSide::Bottom);
            assert_eq!(dock.default_extent(), 200);
        });
    }

    #[test]
    fn restore_carries_dock_visibility() {
        use crate::dock::DockVisibility;

        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/agent.rs", b"x\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update(vcx, |w, cx| {
            let item = w.build_editor_for_path(Path::new("/tmp/repo/agent.rs"), cx);
            w.add_dock(Box::new(item), DockSide::Right, 240, cx);
            w.docks()[0].update(cx, |d, cx| {
                d.set_visibility(DockVisibility::Minimized, cx);
            });
            w.mark_dirty();
        });
        vcx.run_until_parked();
        let path = PathBuf::from("/tmp/state/dock-visibility.ron");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.read_with(vcx, |w, cx| {
            w.save_state(&path, &*fs_dyn, cx).expect("save");
        });

        let (fresh_ws, vcx2) = new_workspace_in_window(&mut cx, "other", "/elsewhere");
        fresh_ws.update(vcx2, |w, cx| {
            w.restore_state(&path, &*fs_dyn, cx).expect("restore");
        });
        vcx2.run_until_parked();
        fresh_ws.read_with(vcx2, |w, cx| {
            let dock = w.docks()[0].read(cx);
            assert_eq!(dock.visibility(), DockVisibility::Minimized);
        });
    }

    #[test]
    fn restore_preserves_multi_dock_order_and_sides() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/left.rs", b"l\n");
        fs.insert_file("/tmp/repo/right.rs", b"r\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update(vcx, |w, cx| {
            let left = w.build_editor_for_path(Path::new("/tmp/repo/left.rs"), cx);
            w.add_dock(Box::new(left), DockSide::Left, 200, cx);
            let right = w.build_editor_for_path(Path::new("/tmp/repo/right.rs"), cx);
            w.add_dock(Box::new(right), DockSide::Right, 240, cx);
            w.mark_dirty();
        });
        vcx.run_until_parked();
        let path = PathBuf::from("/tmp/state/multi-dock.ron");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.read_with(vcx, |w, cx| {
            w.save_state(&path, &*fs_dyn, cx).expect("save");
        });

        let (fresh_ws, vcx2) = new_workspace_in_window(&mut cx, "other", "/elsewhere");
        fresh_ws.update(vcx2, |w, cx| {
            w.restore_state(&path, &*fs_dyn, cx).expect("restore");
        });
        vcx2.run_until_parked();
        fresh_ws.read_with(vcx2, |w, cx| {
            let sides: Vec<DockSide> = w.docks().iter().map(|d| d.read(cx).side()).collect();
            assert_eq!(sides, vec![DockSide::Left, DockSide::Right]);
        });
    }

    #[test]
    fn restore_rebuilds_project_tree_dock_with_expanded_dirs() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_dir("/tmp/repo");
        fs.insert_dir("/tmp/repo/src");
        fs.insert_file("/tmp/repo/src/main.rs", b"fn main() {}\n");
        fs.insert_file("/tmp/repo/a.rs", b"x\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        dispatch(&ws, vcx, stoat_action::ToggleProjectTree);
        dispatch(&ws, vcx, stoat_action::ProjectTreeExpand);
        ws.update(vcx, |w, _| w.mark_dirty());
        vcx.run_until_parked();

        let path = PathBuf::from("/tmp/state/project-tree.ron");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.read_with(vcx, |w, cx| {
            w.save_state(&path, &*fs_dyn, cx).expect("save");
        });

        let (fresh_ws, vcx2) = new_workspace_in_window(&mut cx, "other", "/elsewhere");
        fresh_ws.update(vcx2, |w, cx| {
            w.restore_state(&path, &*fs_dyn, cx).expect("restore");
        });
        vcx2.run_until_parked();

        fresh_ws.read_with(vcx2, |w, cx| {
            assert_eq!(w.docks().len(), 1);
            let dock = w.docks()[0].read(cx);
            assert_eq!(dock.side(), DockSide::Left);
            let tree = dock
                .item()
                .to_any_view()
                .downcast::<ProjectTree>()
                .expect("dock holds a ProjectTree");
            assert_eq!(
                tree.read(cx).expanded_paths(),
                vec![PathBuf::from("/tmp/repo/src")]
            );
        });
    }

    #[test]
    fn to_state_records_non_editor_item_kinds() {
        use crate::{item::ItemKind, rebase_item::RebaseItem};
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hi\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx);
            let rebase_state =
                stoat::rebase::RebaseState::new(PathBuf::from("/tmp/repo"), "HEAD".into(), vec![]);
            let rebase = cx.new(|cx| RebaseItem::new(rebase_state, cx));
            let focus = w.pane_tree().read(cx).focus();
            let pane = w
                .pane_tree()
                .read(cx)
                .pane(focus)
                .cloned()
                .expect("focused pane");
            pane.update(cx, |p, cx| {
                p.add_item(Box::new(rebase), cx);
            });
            w.mark_dirty();
        });
        vcx.run_until_parked();
        let state = ws.read_with(vcx, |w, cx| w.to_state(cx));
        let focus = ws.read_with(vcx, |w, cx| w.pane_tree().read(cx).focus());
        let pane_items = state.pane_items.get(&focus).expect("focused pane snapshot");
        let kinds: Vec<ItemKind> = pane_items.items.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, vec![ItemKind::Editor, ItemKind::Rebase]);
    }

    #[test]
    fn restore_drops_non_editor_items_but_preserves_editor_neighbors() {
        use crate::rebase_item::RebaseItem;
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hi\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx);
            let rebase_state =
                stoat::rebase::RebaseState::new(PathBuf::from("/tmp/repo"), "HEAD".into(), vec![]);
            let rebase = cx.new(|cx| RebaseItem::new(rebase_state, cx));
            let focus = w.pane_tree().read(cx).focus();
            let pane = w
                .pane_tree()
                .read(cx)
                .pane(focus)
                .cloned()
                .expect("focused pane");
            pane.update(cx, |p, cx| {
                p.add_item(Box::new(rebase), cx);
            });
            w.mark_dirty();
        });
        vcx.run_until_parked();
        let path = PathBuf::from("/tmp/state/mixed-items.ron");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.read_with(vcx, |w, cx| {
            w.save_state(&path, &*fs_dyn, cx).expect("save");
        });

        let (fresh_ws, vcx2) = new_workspace_in_window(&mut cx, "other", "/elsewhere");
        fresh_ws.update(vcx2, |w, cx| {
            w.restore_state(&path, &*fs_dyn, cx).expect("restore");
        });
        vcx2.run_until_parked();
        fresh_ws.read_with(vcx2, |w, cx| {
            let focus = w.pane_tree().read(cx).focus();
            let pane = w
                .pane_tree()
                .read(cx)
                .pane(focus)
                .expect("focused pane")
                .read(cx);
            assert_eq!(pane.len(), 1, "only the editor materializes on restore");
            let item = pane.active_item().expect("active item");
            let editor = item
                .to_any_view()
                .downcast::<Editor>()
                .expect("editor item");
            let editor_path = editor.read(cx).file_path().map(Path::to_path_buf);
            assert_eq!(editor_path, Some(PathBuf::from("/tmp/repo/foo.rs")));
        });
    }

    #[test]
    fn restore_most_recent_with_no_persisted_state_returns_false() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();

        let restored = ws.update(vcx, |w, cx| {
            w.restore_most_recent(Path::new("/tmp/repo"), &*fs_dyn, cx)
                .expect("restore")
        });
        assert!(!restored);
    }

    #[test]
    fn save_state_to_default_path_writes_to_canonical_path() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.update(vcx, |w, _| w.mark_dirty());

        let uid = ws.read_with(vcx, |w, _| w.uid());
        let expected_path =
            stoat::workspace::persist::state_path_for(Path::new("/tmp/repo"), uid, &*fs_dyn)
                .expect("state path");

        ws.read_with(vcx, |w, cx| w.save_state_to_default_path(cx));

        assert!(
            stoat::host::FsHost::exists(&*fs, &expected_path),
            "state file should exist",
        );
    }

    #[test]
    fn save_state_to_default_path_no_op_when_fresh() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        let uid = ws.read_with(vcx, |w, _| w.uid());
        let expected_path =
            stoat::workspace::persist::state_path_for(Path::new("/tmp/repo"), uid, &*fs_dyn)
                .expect("state path");

        ws.read_with(vcx, |w, cx| w.save_state_to_default_path(cx));

        assert!(
            !stoat::host::FsHost::exists(&*fs, &expected_path),
            "fresh workspace should not produce a state file",
        );
    }

    #[test]
    fn save_state_to_default_path_no_op_without_fs_host() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update(vcx, |w, _| w.mark_dirty());

        ws.read_with(vcx, |w, cx| w.save_state_to_default_path(cx));
    }

    #[test]
    fn periodic_save_writes_state_after_interval() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        cx.update(|cx| {
            cx.set_global(FsHostGlobal(fs.clone() as Arc<dyn stoat::host::FsHost>));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(stoat_host::NoopFsWatcher::new()) as Arc<dyn stoat::host::FsWatchHost>,
            ));
            cx.set_global(ExecutorGlobal(scheduler.executor()));
        });
        let (ws, vcx) = cx
            .add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx));
        ws.update(vcx, |w, _| w.mark_dirty());
        let uid = ws.read_with(vcx, |w, _| w.uid());
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        let expected_path =
            stoat::workspace::persist::state_path_for(Path::new("/tmp/repo"), uid, &*fs_dyn)
                .expect("state path");

        let task = ws.update(vcx, |_, cx| spawn_periodic_save(cx));
        assert!(task.is_some(), "expected task when ExecutorGlobal is set");
        ws.update(vcx, |w, _| w._periodic_save = task);
        vcx.run_until_parked();

        scheduler.advance_clock(PERIODIC_SAVE_INTERVAL);
        vcx.run_until_parked();

        assert!(
            stoat::host::FsHost::exists(&*fs, &expected_path),
            "periodic save should have written the state file",
        );
    }

    #[test]
    fn restore_most_recent_picks_newest_persisted_file() {
        let mut cx = TestAppContext::single();
        let fs: Arc<stoat::host::FakeFs> = Arc::new(stoat::host::FakeFs::new());
        fs.insert_file("/tmp/repo/foo.rs", b"hi\n");
        install_globals_with_fs(&mut cx, fs.clone());
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let fs_dyn: Arc<dyn stoat::host::FsHost> = fs.clone();
        ws.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/tmp/repo/foo.rs")], cx);
            w.mark_dirty();
        });
        vcx.run_until_parked();
        let state_dir =
            stoat::workspace::persist::workspace_dir_for(Path::new("/tmp/repo"), &*fs_dyn)
                .expect("state dir");
        let uid = ws.read_with(vcx, |w, _| w.uid());
        let state_path = state_dir.join(format!("{uid}.ron"));
        ws.read_with(vcx, |w, cx| {
            w.save_state(&state_path, &*fs_dyn, cx).expect("save");
        });

        let (fresh_ws, vcx2) = new_workspace_in_window(&mut cx, "other", "/elsewhere");
        let restored = fresh_ws.update(vcx2, |w, cx| {
            w.restore_most_recent(Path::new("/tmp/repo"), &*fs_dyn, cx)
                .expect("restore")
        });
        assert!(restored);
        fresh_ws.read_with(vcx2, |w, _| {
            assert_eq!(w.uid(), uid);
        });
    }
}
