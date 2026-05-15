use crate::{
    buffer::Buffer,
    buffer_registry::BufferRegistry,
    diff_map::DiffMap,
    display_map::DisplayMap,
    dock::{Dock, DockSide},
    editor::{Editor, EditorEvent, EditorMode},
    editor_input::EditorInput,
    globals::{ExecutorGlobal, FsHostGlobal},
    input_state_machine::InputStateMachine,
    item::ItemHandle,
    keymap_loader::{compile_default_keymap, compile_from_settings},
    modal_layer::{ModalLayer, ModalView},
    multi_buffer::MultiBuffer,
    pane::{Pane, PaneEvent},
    pane_tree::{PaneTree, PaneTreeEvent},
    settings::Settings,
    status_bar::{
        active_file::ActiveFileLabel, count_prefix::CountPrefix, cursor_position::CursorPosition,
        mode_badge::ModeBadge, workspace_label::WorkspaceLabel, StatusBar, StatusItemView,
    },
    theme::{background_color, DEFAULT_UI_FONT_FAMILY, DEFAULT_UI_FONT_SIZE},
};
use gpui::{
    deferred, div, px, App, AppContext, Context, Entity, EventEmitter, FocusHandle,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    Window,
};
use std::path::{Path, PathBuf};
use stoat::pane::{Axis, Direction};
use stoat_action::ActionKind;

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
    git_root: PathBuf,
    pane_tree: Entity<PaneTree>,
    buffer_registry: Entity<BufferRegistry>,
    docks: Vec<Entity<Dock>>,
    modal_layer: Entity<ModalLayer>,
    status_bar: Entity<StatusBar>,
    input_state_machine: Entity<InputStateMachine>,
    editor_input: Entity<EditorInput>,
    workspace_label: Entity<WorkspaceLabel>,
    active_file_label: Entity<ActiveFileLabel>,
    cursor_position: Entity<CursorPosition>,
    count_prefix: Entity<CountPrefix>,
    focus_handle: FocusHandle,
    last_window_title: Option<SharedString>,
    _active_editor_subscription: Option<Subscription>,
    _pane_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

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
        let status_bar = cx.new(StatusBar::new);
        let buffer_registry = cx.new(|_| BufferRegistry::new());
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
            let actions = sm.update(cx, |sm, cx| sm.feed(&keystroke, cx));
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
                })
            })
            .collect();

        let mode_badge = cx.new(|cx| ModeBadge::new(input_state_machine.clone(), cx));
        let workspace_label = cx.new(|_| WorkspaceLabel::new(name.clone()));
        let active_file_label = cx.new(|_| ActiveFileLabel::new(git_root.clone()));
        let cursor_position = cx.new(|_| CursorPosition::new());
        let count_prefix = cx.new(|cx| CountPrefix::new(input_state_machine.clone(), cx));
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
            bar.add_right_item(count_prefix.clone(), cx);
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
        Self {
            name,
            git_root,
            pane_tree,
            buffer_registry,
            docks: Vec::new(),
            modal_layer,
            status_bar,
            input_state_machine,
            editor_input,
            workspace_label,
            active_file_label,
            cursor_position,
            count_prefix,
            focus_handle: cx.focus_handle(),
            last_window_title: None,
            _active_editor_subscription: None,
            _pane_subscriptions: initial_pane_subscriptions,
            _subscriptions: vec![
                keystroke_subscription,
                settings_subscription,
                pane_tree_subscription,
            ],
        }
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

    pub fn buffer_registry(&self) -> &Entity<BufferRegistry> {
        &self.buffer_registry
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
            let text = read_path_or_empty(&absolute, cx);
            let (_, shared) = self
                .buffer_registry
                .update(cx, |registry, cx| registry.open(&absolute, &text, cx));
            let buffer = cx.new(|_| Buffer::from_shared(shared));
            buffer.update(cx, |b, cx| b.set_file_path(Some(absolute.clone()), cx));
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
            let editor = cx
                .new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx));
            editor.update(cx, |ed, cx| {
                ed.set_workspace(Some(workspace_handle));
                ed.set_file_path(Some(absolute.clone()), cx);
            });
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
            pane.update(cx, |p, cx| {
                p.add_item(Box::new(editor), cx);
            });
        }
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
        self.refresh_active_editor_subscription(cx);
        cx.notify();
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
    /// `editor_focus_target` slots. Non-editor active items (or no
    /// active item) clear both slots. Drives the motion / save /
    /// save-selection / jump dispatch helpers on [`Workspace`] that
    /// look up the active editor through the state machine.
    fn broadcast_active_editor(&mut self, cx: &mut Context<'_, Self>) {
        let editor = self
            .active_pane_item(cx)
            .and_then(|item| item.to_any_view().downcast::<Editor>().ok());
        let focus_target = editor
            .as_ref()
            .map(|_| self.editor_input.read(cx).focus_handle().clone());
        let weak_editor = editor.as_ref().map(Entity::downgrade);
        self.input_state_machine.update(cx, |sm, _| {
            sm.set_active_editor(weak_editor);
            sm.set_editor_focus_target(focus_target);
        });
    }

    pub fn input_state_machine(&self) -> &Entity<InputStateMachine> {
        &self.input_state_machine
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

    pub fn add_dock(
        &mut self,
        item: Box<dyn ItemHandle>,
        side: DockSide,
        default_width: u16,
        cx: &mut Context<'_, Self>,
    ) -> usize {
        let dock = cx.new(|cx| Dock::new(item, side, default_width, cx));
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
        let handled_by_modal = self
            .modal_layer
            .update(cx, |layer, cx| layer.handle_action(&*action, window, cx));
        if handled_by_modal {
            return;
        }
        match action.kind() {
            ActionKind::Quit => self.handle_quit(cx),
            ActionKind::QuitAll => cx.quit(),
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
            ActionKind::GotoColumn => self.dispatch_goto_column(cx),
            ActionKind::ExpandSelection => self.dispatch_expand_selection(cx),
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
            ActionKind::JumpBackward => self.dispatch_jump(JumpDir::Backward, cx),
            ActionKind::JumpForward => self.dispatch_jump(JumpDir::Forward, cx),
            other => {
                tracing::trace!(target: "stoat::dispatch", "unrouted action: {other:?}");
            },
        }
    }

    fn dispatch_move_horizontal(&mut self, delta: i32, extend: bool, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| {
            ed.handle_move_horizontal(delta, count, extend, cx)
        });
    }

    fn dispatch_move_vertical(&mut self, delta: i32, extend: bool, cx: &mut Context<'_, Self>) {
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

    fn dispatch_goto_column(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| ed.handle_goto_column(count, false, cx));
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
        let Some(editor) = self.active_editor(cx) else {
            return;
        };
        let count = self.take_count(cx);
        editor.update(cx, |ed, cx| {
            ed.handle_move_parent_bound(bound, extend, count, cx)
        });
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

    fn active_editor(&self, cx: &Context<'_, Self>) -> Option<Entity<crate::editor::Editor>> {
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
        let title = self.compute_window_title(cx);
        if self.last_window_title.as_ref() != Some(&title) {
            window.set_window_title(&title);
            self.last_window_title = Some(title);
        }

        let left_docks: Vec<Entity<Dock>> = self
            .docks
            .iter()
            .filter(|d| d.read(cx).side() == DockSide::Left)
            .cloned()
            .collect();
        let right_docks: Vec<Entity<Dock>> = self
            .docks
            .iter()
            .filter(|d| d.read(cx).side() == DockSide::Right)
            .cloned()
            .collect();
        let (ui_family, ui_size) = ui_font(cx);
        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(background_color(cx))
            .font_family(ui_family)
            .text_size(px(ui_size))
            .track_focus(&self.focus_handle)
            .children(left_docks)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .child(div().flex_1().child(self.pane_tree.clone()))
                    .child(self.status_bar.clone()),
            )
            .children(right_docks)
            .child(deferred(self.modal_layer.clone()))
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
        div, DismissEvent, Focusable, IntoElement, Render, Styled, Subscription, TestAppContext,
        VisualContext, VisualTestContext, Window,
    };
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use stoat::keymap::Keymap;

    struct WorkspaceItem {
        label: SharedString,
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

    fn new_workspace(cx: &mut TestAppContext, name: &str, root: &str) -> Entity<Workspace> {
        let name = name.to_string();
        let root = PathBuf::from(root);
        cx.update(|cx| cx.new(|cx| Workspace::new(name, root, cx)))
    }

    fn new_workspace_in_window<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        root: &str,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext) {
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
        let entity = cx.update(|cx| cx.new(|_| WorkspaceItem { label }));
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
        assert_eq!(right, 2);
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
    fn workspace_observe_keystrokes_forwards_to_state_machine() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let window = vcx.window_handle();
        cx.simulate_keystrokes(window, "5");
        cx.run_until_parked();

        let sm = ws.read_with(&cx, |w, _| w.input_state_machine().clone());
        sm.read_with(&cx, |sm, _| assert_eq!(sm.pending_count(), Some(5)));
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

    fn new_singleton_editor(
        vcx: &mut VisualTestContext,
        text: &str,
    ) -> Entity<crate::editor::Editor> {
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

    fn cursor_offsets(
        vcx: &mut VisualTestContext,
        editor: &Entity<crate::editor::Editor>,
    ) -> Vec<usize> {
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

    fn selection_offsets(
        vcx: &mut VisualTestContext,
        editor: &Entity<crate::editor::Editor>,
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

        assert_eq!(selection_offsets(vcx, &editor), vec![(0, 3)]);
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

        assert_eq!(selection_offsets(vcx, &editor), vec![(0, 7)]);
    }

    fn seed_primary_offset(
        vcx: &mut VisualTestContext,
        editor: &Entity<crate::editor::Editor>,
        offset: usize,
    ) {
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
        assert_eq!(sel, vec![(1, 3)]);
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
            tree.split(stoat::pane::Axis::Vertical, cx);
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
        use crate::globals::{ExecutorGlobal, FsHostGlobal};
        use stoat_scheduler::{Executor, TestScheduler};
        cx.update(|cx| {
            cx.set_global(FsHostGlobal(fs as Arc<dyn stoat::host::FsHost>));
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
}
