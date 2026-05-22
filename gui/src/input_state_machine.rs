use crate::{
    actions::{
        ApplyFindChar, ApplyMarkChar, ApplyRegisterSelectChar, ApplyReplayMacroChar,
        ApplySurroundAddChar, ApplySurroundDeleteChar, ApplySurroundReplaceChar,
        ApplyTextobjectChar, GotoWordJump,
    },
    editor::{
        actions::{marks::MarkRequest, movement::FindKind, textobject::TextobjectMode},
        Editor,
    },
    workspace::Workspace,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gpui::{Context, FocusHandle, Keystroke, WeakEntity, Window};
use std::{collections::HashMap, ops::Range};
use stoat::{
    action_handlers::surround::SurroundReplaceStage,
    keymap::{Keymap, KeymapState, StateValue},
    keymap_state::{arg_as_str, normalize_shift_event, resolve_action},
    register::Register,
};
use stoat_config::KeyPart;

/// Operator-pending state for multi-stage chords. Variants land
/// with the actions that need them (e.g. textobject selection,
/// surround edits). Empty today; the field is kept to give those
/// follow-ups a place to plug in without reshuffling
/// [`InputStateMachine`].
#[derive(Debug)]
pub enum Operator {}

/// Workspace-hosted entity that owns every per-keystroke piece of
/// state the GUI input pipeline needs. Predicate-visible fields
/// mirror `StoatKeymapState` so the same `stoat::keymap` engine
/// drives both surfaces; matcher state holds the in-progress
/// chord and count between keystrokes; the workspace handle and
/// owned [`Keymap`] are the dispatch hooks the surrounding
/// machinery (observe_keystrokes, dispatch_action, sequence
/// lowering) will reach for in subsequent items.
///
/// The five predicate-visible fields are stored as [`StateValue`]
/// carriers so [`KeymapState::get`] can hand out borrows directly,
/// matching `StoatKeymapState`'s storage layout.
pub struct InputStateMachine {
    mode: StateValue,
    palette_open: StateValue,
    finder_open: StateValue,
    help_open: StateValue,
    claude_focused: StateValue,
    pending_count: Option<u32>,
    /// Count carried over for dispatch after [`feed`] consumes a
    /// keystroke. `feed` moves `pending_count` into this slot once
    /// the keystroke resolves to at least one action, so the
    /// downstream `Workspace::dispatch_action` arms (motion
    /// handlers, etc.) can observe the count between the resolve
    /// and the next keystroke.
    consumed_count: Option<u32>,
    pending_chord: Vec<KeyPart>,
    pending_operator: Option<Operator>,
    prev_focused: Option<FocusHandle>,
    marked_text: Option<String>,
    marked_range: Option<Range<usize>>,
    /// Most recently accepted IME commit, kept here as the
    /// observation point for `EditorInput`'s forwarding tests.
    /// The downstream dispatch into the active editor's buffer
    /// lands with the editor edit-action item; until then this
    /// field is the only place a committed IME insert surfaces.
    last_text_input: Option<String>,
    /// One-shot marker set by [`text_input`] when a single-char
    /// IME commit lands in an allowed mode. The next [`feed`] call
    /// consumes it; if the raw key event matches the committed
    /// char with no modifiers and we are still in an allowed mode,
    /// the keystroke is dropped as a macOS IME duplicate.
    pending_duplicate_char: Option<char>,
    /// Active editor that [`text_input`] dispatches IME commits
    /// into via [`Editor::apply_text_to_all_cursors`]. Production
    /// callers update this when an editor becomes the workspace's
    /// focused item; tests stage it directly.
    active_editor: Option<WeakEntity<Editor>>,
    /// Focus handle to focus when [`transition_mode`] enters an
    /// input mode (insert / reword_insert / prompt / run).
    /// External callers register this when an editor input target
    /// becomes active. `None` means the state machine has nothing
    /// to focus on input-mode entry.
    editor_focus_target: Option<FocusHandle>,
    /// Active after-key chord set by the Find/Till priming
    /// actions. The next keystroke in normal/select mode that
    /// resolves to `KeyCode::Char` is consumed as the chord-target
    /// character and dispatched through
    /// [`crate::actions::ApplyFindChar`]; any other keystroke
    /// clears the chord and falls through to the normal keymap
    /// path.
    pending_find: Option<PendingFind>,
    /// Active after-key chord set by the
    /// `SetMark`/`GotoMark`/`GotoMarkExact` priming actions. The
    /// next keystroke in normal mode that resolves to
    /// `KeyCode::Char` is consumed as the mark name and dispatched
    /// through [`crate::actions::ApplyMarkChar`]; any other
    /// keystroke clears the chord and falls through to the normal
    /// keymap path.
    pending_mark: Option<MarkRequest>,
    /// Active after-key chord set by the [`stoat_action::SelectRegister`]
    /// action. The next chord-completing char keystroke is consumed
    /// and dispatched through [`ApplyRegisterSelectChar`]; any
    /// other keystroke clears the chord.
    pending_register_select: bool,
    /// Active after-key chord set by the [`stoat_action::ReplayMacro`]
    /// action. The next chord-completing char keystroke is consumed
    /// and dispatched through [`ApplyReplayMacroChar`].
    pending_macro_replay: bool,
    /// In-progress recording started by
    /// [`stoat_action::RecordMacro`]. When `Some`, every keystroke
    /// that does NOT resolve to `RecordMacro` is appended to
    /// `keys`; toggling `RecordMacro` again moves the captured
    /// sequence into `macros` keyed by `register`.
    macro_recording: Option<MacroRecording>,
    /// In-process macro store keyed by [`Register`]. Populated by
    /// [`toggle_macro_recording`] when a recording ends; consumed
    /// by [`ApplyReplayMacroChar`] dispatch.
    macros: HashMap<Register, Vec<Keystroke>>,
    /// Active after-key chord set by the [`stoat_action::SurroundAdd`]
    /// action. The next chord-completing char keystroke is consumed
    /// and dispatched through [`ApplySurroundAddChar`].
    pending_surround_add: bool,
    /// Active after-key chord set by the [`stoat_action::SurroundDelete`]
    /// action. The next chord-completing char keystroke is consumed
    /// and dispatched through [`ApplySurroundDeleteChar`].
    pending_surround_delete: bool,
    /// Two-stage chord state set by the
    /// [`stoat_action::SurroundReplace`] action. `AwaitFrom`
    /// captures the from-char and transitions to
    /// `AwaitTo(from)`; `AwaitTo(from)` captures the to-char and
    /// dispatches [`ApplySurroundReplaceChar { from, to }`].
    pending_surround_replace: SurroundReplaceStage,
    /// Active after-key chord set by the
    /// [`stoat_action::SelectTextobjectAround`] /
    /// [`stoat_action::SelectTextobjectInner`] actions. The next
    /// chord-completing char keystroke is consumed and dispatched
    /// through [`ApplyTextobjectChar`] with the captured mode.
    pending_textobject_select: Option<TextobjectMode>,
    /// Mode captured before a modal opened, restored when the modal
    /// closes. Populated by
    /// [`Self::capture_prev_mode_for_modal`] from the workspace's
    /// modal-layer observer; drained by
    /// [`Self::take_prev_mode_for_modal`] on the closing edge.
    prev_mode_for_modal: Option<String>,
    /// Name of the most recently opened picker action, recorded by
    /// [`Workspace::dispatch_action`]'s tail when a picker-open
    /// action successfully toggles a modal. Drives
    /// [`stoat_action::OpenLastPicker`] recall: the handler looks
    /// the name up in the action registry and re-dispatches a
    /// fresh action instance, rebuilding the picker from current
    /// state. `None` until the first picker opens.
    last_picker_action: Option<&'static str>,
    workspace: WeakEntity<Workspace>,
    keymap: Keymap,
}

/// In-progress macro recording. `keys` grows on every keystroke
/// captured by [`InputStateMachine::feed`] that does not resolve
/// to the [`stoat_action::RecordMacro`] toggle.
#[derive(Debug)]
pub struct MacroRecording {
    pub register: Register,
    pub keys: Vec<Keystroke>,
}

/// Chord state armed by the Find/Till priming actions
/// ([`crate::input_state_machine::InputStateMachine::set_pending_find`])
/// and consumed by the next chord-completing keystroke.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PendingFind {
    pub kind: FindKind,
    pub extend: bool,
    pub count: u32,
}

impl InputStateMachine {
    pub fn new(workspace: WeakEntity<Workspace>, keymap: Keymap) -> Self {
        Self {
            mode: StateValue::String("normal".into()),
            palette_open: StateValue::Bool(false),
            finder_open: StateValue::Bool(false),
            help_open: StateValue::Bool(false),
            claude_focused: StateValue::Bool(false),
            pending_count: None,
            consumed_count: None,
            pending_chord: Vec::new(),
            pending_operator: None,
            prev_focused: None,
            marked_text: None,
            marked_range: None,
            last_text_input: None,
            pending_duplicate_char: None,
            active_editor: None,
            editor_focus_target: None,
            pending_find: None,
            pending_mark: None,
            pending_register_select: false,
            pending_macro_replay: false,
            macro_recording: None,
            macros: HashMap::new(),
            pending_surround_add: false,
            pending_surround_delete: false,
            pending_surround_replace: SurroundReplaceStage::Idle,
            pending_textobject_select: None,
            prev_mode_for_modal: None,
            last_picker_action: None,
            workspace,
            keymap,
        }
    }

    pub fn mode(&self) -> &str {
        match &self.mode {
            StateValue::String(s) => s.as_str(),
            _ => "",
        }
    }

    pub fn palette_open(&self) -> bool {
        matches!(self.palette_open, StateValue::Bool(true))
    }

    pub fn finder_open(&self) -> bool {
        matches!(self.finder_open, StateValue::Bool(true))
    }

    pub fn help_open(&self) -> bool {
        matches!(self.help_open, StateValue::Bool(true))
    }

    pub fn claude_focused(&self) -> bool {
        matches!(self.claude_focused, StateValue::Bool(true))
    }

    pub fn pending_count(&self) -> Option<u32> {
        self.pending_count
    }

    pub fn consumed_count(&self) -> Option<u32> {
        self.consumed_count
    }

    /// Return the count consumed by the most recent
    /// [`feed`]-resolved action and clear the slot. Dispatch arms
    /// for count-aware actions (motion handlers, etc.) call this
    /// once to read the count and prevent the next keystroke
    /// from observing a stale value.
    pub fn take_consumed_count(&mut self) -> Option<u32> {
        self.consumed_count.take()
    }

    #[cfg(test)]
    pub(crate) fn set_consumed_count_for_test(&mut self, count: Option<u32>) {
        self.consumed_count = count;
    }

    #[cfg(test)]
    pub(crate) fn set_pending_count_for_test(
        &mut self,
        count: Option<u32>,
        cx: &mut Context<'_, Self>,
    ) {
        self.pending_count = count;
        cx.notify();
    }

    pub fn pending_chord(&self) -> &[KeyPart] {
        &self.pending_chord
    }

    pub fn pending_operator(&self) -> Option<&Operator> {
        self.pending_operator.as_ref()
    }

    pub fn prev_focused(&self) -> Option<&FocusHandle> {
        self.prev_focused.as_ref()
    }

    pub fn marked_text(&self) -> Option<&str> {
        self.marked_text.as_deref()
    }

    pub fn marked_range(&self) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    pub fn last_text_input(&self) -> Option<&str> {
        self.last_text_input.as_deref()
    }

    pub fn active_editor(&self) -> Option<&WeakEntity<Editor>> {
        self.active_editor.as_ref()
    }

    /// Set the editor that [`text_input`] commits dispatch into.
    /// Production code calls this when the workspace's focused
    /// item changes; passing `None` detaches dispatch and leaves
    /// `text_input` to fall back to recording on `last_text_input`
    /// only.
    pub fn set_active_editor(&mut self, editor: Option<WeakEntity<Editor>>) {
        self.active_editor = editor;
    }

    pub fn editor_focus_target(&self) -> Option<&FocusHandle> {
        self.editor_focus_target.as_ref()
    }

    /// Register the focus handle that [`transition_mode`] focuses
    /// when entering an input mode. External callers update this
    /// when an editor input target becomes active. `None` detaches
    /// the slot, leaving entry-into-input-mode without a focus
    /// side effect.
    pub fn set_editor_focus_target(&mut self, handle: Option<FocusHandle>) {
        self.editor_focus_target = handle;
    }

    /// Update the active mode and produce focus output. When the
    /// transition crosses from a non-input mode into an input mode
    /// (insert / reword_insert / prompt / run) and an
    /// `editor_focus_target` is registered, the target is focused
    /// via `window.focus`. Transitions between input modes do not
    /// refocus -- the input target is already focused; transitions
    /// into a non-input mode produce no focus output (the next
    /// action focuses whatever it intends to focus).
    pub fn transition_mode(
        &mut self,
        mode: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let was_input = self.text_input_allowed();
        self.mode = StateValue::String(mode.into().into());
        let now_input = self.text_input_allowed();
        if !was_input && now_input {
            if let Some(handle) = self.editor_focus_target.as_ref() {
                window.focus(handle);
            }
        }
        cx.notify();
    }

    /// Set the mode without focus side effects. Used by modal
    /// lifecycle wiring that needs to update the keymap-state mode
    /// flag (so predicates like `mode == prompt && palette_open`
    /// fire) while the modal layer's own focus machinery handles
    /// the focus transition independently.
    pub fn set_mode(&mut self, mode: impl Into<String>, cx: &mut Context<'_, Self>) {
        let new = StateValue::String(mode.into().into());
        if self.mode != new {
            self.mode = new;
            cx.notify();
        }
    }

    /// Set the `palette_open` keymap-state flag. No-ops on unchanged
    /// value so observers don't see redundant notifications.
    pub fn set_palette_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        let new = StateValue::Bool(open);
        if self.palette_open != new {
            self.palette_open = new;
            cx.notify();
        }
    }

    /// Set the `help_open` keymap-state flag. No-ops on unchanged
    /// value so observers don't see redundant notifications.
    pub fn set_help_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        let new = StateValue::Bool(open);
        if self.help_open != new {
            self.help_open = new;
            cx.notify();
        }
    }

    /// Capture the current mode for restoration when a modal closes.
    /// Idempotent: a subsequent call while the slot is still
    /// occupied is a no-op, so nested-modal scenarios restore the
    /// mode that existed before the *first* modal opened.
    pub fn capture_prev_mode_for_modal(&mut self) {
        if self.prev_mode_for_modal.is_none() {
            self.prev_mode_for_modal = Some(self.mode().to_string());
        }
    }

    /// Drain the captured prev-mode slot. Called when a modal
    /// closes; returns the mode to restore via [`Self::set_mode`],
    /// or `None` when nothing was captured.
    pub fn take_prev_mode_for_modal(&mut self) -> Option<String> {
        self.prev_mode_for_modal.take()
    }

    /// Name of the most recently opened picker action, or `None`
    /// when no picker has opened since the workspace started.
    /// Consumed by [`stoat_action::OpenLastPicker`].
    pub fn last_picker_action(&self) -> Option<&'static str> {
        self.last_picker_action
    }

    /// Record `name` as the most recently opened picker action.
    /// Called from [`Workspace::dispatch_action`]'s post-dispatch
    /// tail when a picker-open action toggles a modal; `None`
    /// clears the slot.
    pub fn set_last_picker_action(&mut self, name: Option<&'static str>) {
        self.last_picker_action = name;
    }

    /// Save `handle` for later restoration via
    /// [`restore_prev_focus`]. Callers that own a transition into
    /// a focus-grabbing surface (entering a fullscreen view,
    /// opening a non-modal sub-pane) capture the previously
    /// focused handle here so the symmetric exit can restore it.
    pub fn capture_prev_focus(&mut self, handle: Option<FocusHandle>) {
        self.prev_focused = handle;
    }

    /// Restore focus to the handle saved by [`capture_prev_focus`]
    /// and clear the slot. No-op when no previous handle is
    /// captured. Single-slot semantics; nested capture/restore
    /// pairs that need stacking should track stacks externally
    /// (see `ModalLayer::previous_focus_handle` for the modal case).
    pub fn restore_prev_focus(&mut self, window: &mut Window) {
        if let Some(handle) = self.prev_focused.take() {
            window.focus(&handle);
        }
    }

    /// Modes in which IME / direct text input is accepted. Outside
    /// these modes, focus output keeps the input target unfocused
    /// in production so the OS does not route IME there; the gate
    /// here enforces the same contract for tests that bypass the
    /// OS path.
    pub fn text_input_allowed(&self) -> bool {
        matches!(self.mode(), "insert" | "reword_insert" | "prompt" | "run")
    }

    /// Apply a committed IME insert (`insertText` from
    /// NSTextInputClient). When [`text_input_allowed`] is false the
    /// commit is dropped silently. Otherwise the text is recorded
    /// as the most recent input and any in-flight composition state
    /// is cleared.
    ///
    /// `range` is forwarded for the eventual buffer-level dispatch
    /// but unused today; the field is the temporary observation
    /// point until the editor edit-action item lands.
    ///
    /// Single-char commits also arm a one-shot duplicate-drop
    /// marker that the next [`feed`] call honours, so a paired raw
    /// key event from macOS does not double-insert. Multi-char
    /// commits leave the marker cleared because the originating
    /// keystrokes were consumed by the IME and have no raw twin.
    ///
    /// When [`active_editor`] holds a live handle, the commit is
    /// dispatched through [`Editor::apply_text_to_all_cursors`] so
    /// the text lands at every cursor; the IME side stays unaware
    /// of multi-cursor. The fallback (`active_editor` is `None` or
    /// the weak handle is dead) only records on `last_text_input`.
    pub fn text_input(
        &mut self,
        text: &str,
        _range: Option<Range<usize>>,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.text_input_allowed() {
            return;
        }
        self.last_text_input = Some(text.to_string());
        self.marked_text = None;
        self.marked_range = None;
        self.pending_duplicate_char = {
            let mut chars = text.chars();
            chars.next().filter(|_| chars.next().is_none())
        };
        if let Some(editor) = self.active_editor.as_ref().and_then(WeakEntity::upgrade) {
            let text = text.to_string();
            editor.update(cx, |ed, cx| ed.apply_text_to_all_cursors(&text, cx));
        }
        cx.notify();
    }

    /// Apply an IME composition update (`setMarkedText`). When
    /// [`text_input_allowed`] is false the update is dropped
    /// silently. Otherwise the marked text and its UTF-16 range
    /// are recorded so [`marked_text`] / [`marked_range`] reflect
    /// the in-flight composition. Any pending duplicate-drop
    /// marker is cleared -- composition transitions do not pair
    /// with raw key duplicates.
    pub fn composition_update(
        &mut self,
        text: &str,
        range: Option<Range<usize>>,
        _selected: Option<Range<usize>>,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.text_input_allowed() {
            return;
        }
        self.marked_text = Some(text.to_string());
        self.marked_range = range;
        self.pending_duplicate_char = None;
        cx.notify();
    }

    /// Clear any in-flight IME composition (`unmarkText`).
    /// Unconditional so a mode change mid-composition still leaves
    /// the state machine with a clean composition slot. Also
    /// clears any pending duplicate-drop marker so a stale entry
    /// from a prior commit does not survive across composition
    /// boundaries.
    pub fn composition_commit(&mut self, cx: &mut Context<'_, Self>) {
        self.marked_text = None;
        self.marked_range = None;
        self.pending_duplicate_char = None;
        cx.notify();
    }

    /// Active after-key chord state, if a Find/Till priming action
    /// armed one and the consuming keystroke has not arrived yet.
    pub fn pending_find(&self) -> Option<&PendingFind> {
        self.pending_find.as_ref()
    }

    /// Arm the after-key Find/Till chord. The next keystroke that
    /// resolves to `KeyCode::Char` in normal/select mode is consumed
    /// as the chord target; any other keystroke clears the chord.
    pub fn set_pending_find(
        &mut self,
        kind: FindKind,
        extend: bool,
        count: u32,
        cx: &mut Context<'_, Self>,
    ) {
        self.pending_find = Some(PendingFind {
            kind,
            extend,
            count,
        });
        cx.notify();
    }

    /// Active after-key chord state, if a mark priming action
    /// armed one and the consuming keystroke has not arrived yet.
    pub fn pending_mark(&self) -> Option<MarkRequest> {
        self.pending_mark
    }

    /// Arm the after-key mark chord. The next keystroke that
    /// resolves to `KeyCode::Char` in normal mode is consumed as
    /// the mark name; any other keystroke clears the chord.
    pub fn set_pending_mark(&mut self, request: MarkRequest, cx: &mut Context<'_, Self>) {
        self.pending_mark = Some(request);
        cx.notify();
    }

    pub fn pending_register_select(&self) -> bool {
        self.pending_register_select
    }

    /// Arm the after-key register-select chord. The next
    /// chord-completing char keystroke produces a
    /// [`crate::actions::ApplyRegisterSelectChar`] action.
    pub fn arm_select_register(&mut self, cx: &mut Context<'_, Self>) {
        self.pending_register_select = true;
        cx.notify();
    }

    pub fn pending_macro_replay(&self) -> bool {
        self.pending_macro_replay
    }

    /// Arm the after-key macro-replay chord. The next
    /// chord-completing char keystroke produces a
    /// [`crate::actions::ApplyReplayMacroChar`] action.
    pub fn arm_replay_macro(&mut self, cx: &mut Context<'_, Self>) {
        self.pending_macro_replay = true;
        cx.notify();
    }

    pub fn macro_recording(&self) -> Option<&MacroRecording> {
        self.macro_recording.as_ref()
    }

    pub fn macros(&self) -> &HashMap<Register, Vec<Keystroke>> {
        &self.macros
    }

    /// Toggle macro recording. Off -> start recording into
    /// `register`. On -> stop, move the captured keystroke
    /// sequence into `macros[recording.register]` (the register
    /// chosen when recording started, not the argument here).
    pub fn toggle_macro_recording(&mut self, register: Register, cx: &mut Context<'_, Self>) {
        if let Some(rec) = self.macro_recording.take() {
            self.macros.insert(rec.register, rec.keys);
        } else {
            self.macro_recording = Some(MacroRecording {
                register,
                keys: Vec::new(),
            });
        }
        cx.notify();
    }

    /// Look up a stored macro by `register`, returning a cloned
    /// keystroke vector (or `None` when no macro is stored for
    /// that register). Used by the replay-chord dispatch.
    pub fn macro_for_register(&self, register: Register) -> Option<Vec<Keystroke>> {
        self.macros.get(&register).cloned()
    }

    pub fn pending_surround_add(&self) -> bool {
        self.pending_surround_add
    }

    pub fn pending_surround_delete(&self) -> bool {
        self.pending_surround_delete
    }

    pub fn pending_surround_replace(&self) -> SurroundReplaceStage {
        self.pending_surround_replace
    }

    pub fn arm_surround_add(&mut self, cx: &mut Context<'_, Self>) {
        self.pending_surround_add = true;
        cx.notify();
    }

    pub fn arm_surround_delete(&mut self, cx: &mut Context<'_, Self>) {
        self.pending_surround_delete = true;
        cx.notify();
    }

    pub fn arm_surround_replace(&mut self, cx: &mut Context<'_, Self>) {
        self.pending_surround_replace = SurroundReplaceStage::AwaitFrom;
        cx.notify();
    }

    pub fn pending_textobject_select(&self) -> Option<TextobjectMode> {
        self.pending_textobject_select
    }

    pub fn arm_textobject_select(&mut self, mode: TextobjectMode, cx: &mut Context<'_, Self>) {
        self.pending_textobject_select = Some(mode);
        cx.notify();
    }

    pub fn workspace(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// Replace the active keymap. Used by the keymap-loader item to
    /// hot-reload bindings when settings change, and by tests to
    /// stage a known binding before driving keystrokes through the
    /// pipeline.
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
    }

    /// Stage the predicate-visible `mode` field directly. Used by
    /// tests to drive scenarios where the state machine would
    /// normally arrive at a given mode through action dispatch
    /// (action wiring is a later item); production code transitions
    /// modes through action handlers, never this method.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_mode_for_test(&mut self, mode: StateValue) {
        self.mode = mode;
    }

    /// Drive one platform keystroke through the input pipeline:
    /// translate it to the crossterm shape the keymap engine matches
    /// against, fold an ASCII digit into the pending count when one
    /// is in flight (normal/select modes only), look up bindings
    /// against `self` as the [`KeymapState`], and resolve each
    /// match into a [`stoat_action::Action`] via [`resolve_action`].
    /// Returns the resolved actions for the caller to dispatch.
    ///
    /// A sequence binding (e.g. `C-k -> [SelectLine(), Comment()];`)
    /// surfaces as one entry per child in source order; there is no
    /// composite Action type, so the caller dispatches each child
    /// individually via `Workspace::dispatch_action`.
    ///
    /// `SetMode` bindings are handled inline by calling
    /// [`Self::transition_mode`] on `self` and do not appear in the
    /// returned action list. `SetMode` is intentionally absent from
    /// the action registry (see `stoat_action::registry`), so the
    /// downstream `resolve_action` path cannot dispatch it.
    ///
    /// On macOS, a single keypress in a text-input mode can fire
    /// both an IME commit (via [`text_input`]) and a raw key event.
    /// `feed` consumes the duplicate-drop marker armed by the prior
    /// `text_input` call: when the marker is set, the current mode
    /// still allows text input, and the raw event is the unmodified
    /// `KeyCode::Char` matching the committed character, the
    /// keystroke is dropped. The marker is one-shot regardless of
    /// match so a non-matching raw event also clears it.
    ///
    /// Returning the action list rather than dispatching inline
    /// keeps this method off the workspace's update path; the
    /// `cx.observe_keystrokes` callback already holds a `&mut
    /// Workspace`, and re-entering [`Entity::update`] from
    /// underneath that borrow would panic.
    ///
    /// Keystrokes the crossterm shape cannot represent (modifier-only
    /// events, unknown named keys) are silently dropped. Unknown
    /// action names and bad arg shapes are dropped after a
    /// `tracing::warn` inside [`resolve_action`].
    pub fn feed(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Vec<Box<dyn stoat_action::Action>> {
        let Some(event) = keystroke_to_key_event(keystroke) else {
            return Vec::new();
        };
        let event = normalize_shift_event(event);

        if let Some(dup) = self.pending_duplicate_char.take() {
            if self.text_input_allowed()
                && event.modifiers.is_empty()
                && event.code == KeyCode::Char(dup)
            {
                return Vec::new();
            }
        }

        let count_active_mode = self.mode() == "normal" || self.mode() == "select";

        if count_active_mode && self.pending_find.is_some() {
            if let KeyCode::Char(ch) = event.code {
                let pf = self.pending_find.take().expect("checked above");
                cx.notify();
                return vec![Box::new(ApplyFindChar {
                    kind: pf.kind,
                    ch,
                    extend: pf.extend,
                    count: pf.count,
                })];
            }
            self.pending_find = None;
            cx.notify();
        }

        if self.mode() == "normal" && self.pending_mark.is_some() {
            if let KeyCode::Char(ch) = event.code {
                let request = self.pending_mark.take().expect("checked above");
                cx.notify();
                return vec![Box::new(ApplyMarkChar { request, ch })];
            }
            self.pending_mark = None;
            cx.notify();
        }

        if count_active_mode && self.pending_register_select {
            if let KeyCode::Char(ch) = event.code {
                self.pending_register_select = false;
                cx.notify();
                return vec![Box::new(ApplyRegisterSelectChar { ch })];
            }
            self.pending_register_select = false;
            cx.notify();
        }

        if count_active_mode && self.pending_macro_replay {
            if let KeyCode::Char(ch) = event.code {
                self.pending_macro_replay = false;
                cx.notify();
                return vec![Box::new(ApplyReplayMacroChar { ch })];
            }
            self.pending_macro_replay = false;
            cx.notify();
        }

        if count_active_mode && self.pending_surround_add {
            if let KeyCode::Char(ch) = event.code {
                self.pending_surround_add = false;
                cx.notify();
                return vec![Box::new(ApplySurroundAddChar { ch })];
            }
            self.pending_surround_add = false;
            cx.notify();
        }

        if count_active_mode && self.pending_surround_delete {
            if let KeyCode::Char(ch) = event.code {
                self.pending_surround_delete = false;
                cx.notify();
                return vec![Box::new(ApplySurroundDeleteChar { ch })];
            }
            self.pending_surround_delete = false;
            cx.notify();
        }

        if count_active_mode {
            match self.pending_surround_replace {
                SurroundReplaceStage::AwaitFrom => {
                    if let KeyCode::Char(ch) = event.code {
                        self.pending_surround_replace = SurroundReplaceStage::AwaitTo(ch);
                        cx.notify();
                        return Vec::new();
                    }
                    self.pending_surround_replace = SurroundReplaceStage::Idle;
                    cx.notify();
                },
                SurroundReplaceStage::AwaitTo(from) => {
                    if let KeyCode::Char(to) = event.code {
                        self.pending_surround_replace = SurroundReplaceStage::Idle;
                        cx.notify();
                        return vec![Box::new(ApplySurroundReplaceChar { from, to })];
                    }
                    self.pending_surround_replace = SurroundReplaceStage::Idle;
                    cx.notify();
                },
                SurroundReplaceStage::Idle => {},
            }
        }

        if count_active_mode {
            if let Some(mode) = self.pending_textobject_select {
                if let KeyCode::Char(ch) = event.code {
                    self.pending_textobject_select = None;
                    cx.notify();
                    return vec![Box::new(ApplyTextobjectChar { mode, ch })];
                }
                self.pending_textobject_select = None;
                cx.notify();
            }
        }

        if count_active_mode {
            if let Some(editor) = self.active_editor.as_ref().and_then(WeakEntity::upgrade) {
                let has_labels = editor.read(cx).pending_goto_word_labels().is_some();
                if has_labels {
                    if let KeyCode::Char(ch) = event.code {
                        let step = editor.read(cx).pending_goto_word_labels().map(|labels| {
                            stoat::goto_word::step_jump(
                                labels,
                                editor.read(cx).pending_goto_word_input(),
                                ch,
                            )
                        });
                        match step {
                            Some(stoat::goto_word::JumpStep::Jump(offset)) => {
                                editor.update(cx, |ed, cx| ed.clear_pending_goto_word(cx));
                                cx.notify();
                                return vec![Box::new(GotoWordJump {
                                    byte_offset: offset,
                                })];
                            },
                            Some(stoat::goto_word::JumpStep::Continue) => {
                                editor.update(cx, |ed, cx| ed.push_pending_goto_word_input(ch, cx));
                                cx.notify();
                                return Vec::new();
                            },
                            Some(stoat::goto_word::JumpStep::Cancel) | None => {
                                editor.update(cx, |ed, cx| ed.clear_pending_goto_word(cx));
                                cx.notify();
                                return Vec::new();
                            },
                        }
                    }
                    editor.update(cx, |ed, cx| ed.clear_pending_goto_word(cx));
                    cx.notify();
                }
            }
        }

        let digit = unmodified_ascii_digit(&event);

        if count_active_mode && self.pending_count.is_some() {
            if let Some(d) = digit {
                let new_count = self
                    .pending_count
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(d);
                self.pending_count = Some(new_count);
                cx.notify();
                return Vec::new();
            }
        }

        let resolved = self
            .keymap
            .lookup(self, &event)
            .map(<[_]>::to_vec)
            .unwrap_or_default();

        if resolved.is_empty() {
            if count_active_mode {
                if let Some(d) = digit {
                    self.pending_count = Some(d);
                    cx.notify();
                }
            }
            return Vec::new();
        }

        for ra in &resolved {
            if ra.name == "SetMode" {
                if let Some(mode_name) = ra.args.first().and_then(arg_as_str) {
                    self.transition_mode(mode_name, window, cx);
                }
            }
        }

        let actions: Vec<Box<dyn stoat_action::Action>> = resolved
            .iter()
            .filter_map(|ra| resolve_action(&ra.name, &ra.args))
            .collect();

        if self.macro_recording.is_some() {
            let is_record_toggle = resolved.iter().any(|ra| ra.name == "RecordMacro");
            if !is_record_toggle {
                if let Some(rec) = self.macro_recording.as_mut() {
                    rec.keys.push(keystroke.clone());
                }
            }
        }

        if !actions.is_empty() && self.pending_count.is_some() {
            self.consumed_count = self.pending_count.take();
            cx.notify();
        }

        actions
    }
}

impl KeymapState for InputStateMachine {
    fn get(&self, field: &str) -> Option<&StateValue> {
        match field {
            "mode" => Some(&self.mode),
            "palette_open" => Some(&self.palette_open),
            "finder_open" => Some(&self.finder_open),
            "help_open" => Some(&self.help_open),
            "claude_focused" => Some(&self.claude_focused),
            _ => None,
        }
    }
}

fn keystroke_to_key_event(keystroke: &Keystroke) -> Option<KeyEvent> {
    let mut modifiers = KeyModifiers::empty();
    if keystroke.modifiers.control {
        modifiers |= KeyModifiers::CONTROL;
    }
    if keystroke.modifiers.alt {
        modifiers |= KeyModifiers::ALT;
    }
    if keystroke.modifiers.shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    if keystroke.modifiers.platform {
        modifiers |= KeyModifiers::SUPER;
    }

    let code = match keystroke.key.as_str() {
        "space" => KeyCode::Char(' '),
        "enter" => KeyCode::Enter,
        "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        s if function_key_index(s).is_some() => KeyCode::F(function_key_index(s)?),
        s => {
            let mut chars = s.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(first)
        },
    };

    Some(KeyEvent::new(code, modifiers))
}

fn function_key_index(key: &str) -> Option<u8> {
    let rest = key.strip_prefix('f')?;
    if rest.is_empty() {
        return None;
    }
    rest.parse().ok()
}

fn unmodified_ascii_digit(event: &KeyEvent) -> Option<u32> {
    if !event.modifiers.is_empty() {
        return None;
    }
    match event.code {
        KeyCode::Char(ch) if ch.is_ascii_digit() => ch.to_digit(10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};
    use std::path::PathBuf;
    use stoat_config::Config;

    fn empty_keymap() -> Keymap {
        Keymap::compile(&Config {
            blocks: Vec::new(),
            themes: Vec::new(),
        })
    }

    fn compile_keymap(src: &str) -> Keymap {
        let (config, errors) = stoat_config::parse(src);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        Keymap::compile(&config.expect("expected config"))
    }

    fn new_state_machine_with_keymap(
        cx: &mut TestAppContext,
        keymap: Keymap,
    ) -> (Entity<InputStateMachine>, &mut VisualTestContext) {
        let (workspace, vcx) =
            cx.add_window_view(|_, cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx));
        let weak = workspace.downgrade();
        let sm = vcx.update(|_, cx| cx.new(|_| InputStateMachine::new(weak, keymap)));
        (sm, vcx)
    }

    fn new_state_machine(
        cx: &mut TestAppContext,
    ) -> (Entity<InputStateMachine>, &mut VisualTestContext) {
        new_state_machine_with_keymap(cx, empty_keymap())
    }

    fn key(name: &str) -> Keystroke {
        Keystroke {
            modifiers: Modifiers::default(),
            key: name.into(),
            key_char: None,
        }
    }

    fn key_with(name: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: name.into(),
            key_char: None,
        }
    }

    fn feed_in_app<R>(
        vcx: &mut VisualTestContext,
        sm: &Entity<InputStateMachine>,
        f: impl FnOnce(&mut InputStateMachine, &mut Window, &mut Context<'_, InputStateMachine>) -> R,
    ) -> R {
        sm.update_in(vcx, f)
    }

    #[test]
    fn defaults() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.mode(), "normal");
            assert!(!sm.palette_open());
            assert!(!sm.finder_open());
            assert!(!sm.help_open());
            assert!(!sm.claude_focused());
            assert_eq!(sm.pending_count(), None);
            assert!(sm.pending_chord().is_empty());
            assert!(sm.pending_operator().is_none());
            assert!(sm.prev_focused().is_none());
            assert_eq!(sm.last_picker_action(), None);
        });
    }

    #[test]
    fn set_last_picker_action_round_trips() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        sm.update(vcx, |sm, _| {
            sm.set_last_picker_action(Some("OpenJumplistPicker"))
        });
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.last_picker_action(), Some("OpenJumplistPicker"));
        });
        sm.update(vcx, |sm, _| sm.set_last_picker_action(None));
        sm.read_with(vcx, |sm, _| assert_eq!(sm.last_picker_action(), None));
    }

    #[test]
    fn keymap_state_get_returns_predicate_fields() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.get("mode"), Some(&StateValue::String("normal".into())));
            assert_eq!(sm.get("palette_open"), Some(&StateValue::Bool(false)));
            assert_eq!(sm.get("finder_open"), Some(&StateValue::Bool(false)));
            assert_eq!(sm.get("help_open"), Some(&StateValue::Bool(false)));
            assert_eq!(sm.get("claude_focused"), Some(&StateValue::Bool(false)));
            assert_eq!(sm.get("unknown"), None);
        });
    }

    #[test]
    fn feed_digit_seeds_pending_count() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        let stroke = key("5");
        feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.feed(&stroke, window, cx);
        });
        sm.read_with(vcx, |sm, _| assert_eq!(sm.pending_count(), Some(5)));
    }

    #[test]
    fn feed_digit_extends_pending_count() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        let first = key("5");
        let second = key("2");
        feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.feed(&first, window, cx);
            sm.feed(&second, window, cx);
        });
        sm.read_with(vcx, |sm, _| assert_eq!(sm.pending_count(), Some(52)));
    }

    #[test]
    fn feed_digit_in_insert_mode_does_not_seed_count() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        sm.update(vcx, |sm, _| {
            sm.mode = StateValue::String("insert".into());
        });
        let stroke = key("5");
        feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.feed(&stroke, window, cx);
        });
        sm.read_with(vcx, |sm, _| assert_eq!(sm.pending_count(), None));
    }

    #[test]
    fn feed_modified_digit_does_not_seed_count() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        let stroke = key_with(
            "5",
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
        );
        feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.feed(&stroke, window, cx);
        });
        sm.read_with(vcx, |sm, _| assert_eq!(sm.pending_count(), None));
    }

    #[test]
    fn feed_unmapped_key_is_no_op() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        let stroke = key("q");
        feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.feed(&stroke, window, cx);
        });
        sm.read_with(vcx, |sm, _| assert_eq!(sm.pending_count(), None));
    }

    #[test]
    fn feed_matched_action_clears_pending_count() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { q -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        sm.update(vcx, |sm, _| sm.pending_count = Some(3));
        let stroke = key("q");
        feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.feed(&stroke, window, cx);
        });
        sm.read_with(vcx, |sm, _| assert_eq!(sm.pending_count(), None));
    }

    #[test]
    fn feed_matched_action_moves_pending_count_into_consumed() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { q -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        sm.update(vcx, |sm, _| sm.pending_count = Some(5));
        let stroke = key("q");
        feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.feed(&stroke, window, cx);
        });
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.pending_count(), None);
            assert_eq!(sm.consumed_count(), Some(5));
        });
    }

    #[test]
    fn take_consumed_count_returns_and_clears() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        sm.update(vcx, |sm, _| sm.consumed_count = Some(7));
        let taken = sm.update(vcx, |sm, _| sm.take_consumed_count());
        assert_eq!(taken, Some(7));
        sm.read_with(vcx, |sm, _| assert_eq!(sm.consumed_count(), None));
    }

    #[test]
    fn feed_uppercase_letter_normalizes_shift() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { G -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        sm.update(vcx, |sm, _| sm.pending_count = Some(3));
        let stroke = key_with(
            "g",
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        );
        feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.feed(&stroke, window, cx);
        });
        sm.read_with(vcx, |sm, _| assert_eq!(sm.pending_count(), None));
    }

    #[test]
    fn feed_lowers_sequence_binding_in_order() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { s -> [SplitRight(), Quit()]; }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        let stroke = key("s");
        let kinds = feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.feed(&stroke, window, cx)
                .iter()
                .map(|a| a.kind())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            kinds,
            vec![
                stoat_action::ActionKind::SplitRight,
                stoat_action::ActionKind::Quit,
            ]
        );
    }

    fn set_mode(vcx: &mut VisualTestContext, sm: &Entity<InputStateMachine>, mode: &str) {
        let mode = mode.to_string();
        sm.update(vcx, |sm, _| {
            sm.mode = StateValue::String(mode.into());
        });
    }

    #[test]
    fn text_input_in_insert_mode_records_input() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        set_mode(vcx, &sm, "insert");
        feed_in_app(vcx, &sm, |sm, _window, cx| sm.text_input("hi", None, cx));
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.last_text_input(), Some("hi"));
            assert_eq!(sm.marked_text(), None);
            assert_eq!(sm.marked_range(), None);
        });
    }

    #[test]
    fn text_input_in_normal_mode_is_dropped() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        feed_in_app(vcx, &sm, |sm, _window, cx| sm.text_input("hi", None, cx));
        sm.read_with(vcx, |sm, _| assert_eq!(sm.last_text_input(), None));
    }

    #[test]
    fn text_input_allowed_for_each_mode() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        for mode in ["insert", "reword_insert", "prompt", "run"] {
            set_mode(vcx, &sm, mode);
            sm.read_with(vcx, |sm, _| {
                assert!(sm.text_input_allowed(), "expected {mode} to allow input");
            });
        }
        for mode in ["normal", "select"] {
            set_mode(vcx, &sm, mode);
            sm.read_with(vcx, |sm, _| {
                assert!(!sm.text_input_allowed(), "expected {mode} to drop input");
            });
        }
    }

    #[test]
    fn composition_update_sets_marked_state_in_insert() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        set_mode(vcx, &sm, "insert");
        feed_in_app(vcx, &sm, |sm, _window, cx| {
            sm.composition_update("ka", Some(0..2), None, cx)
        });
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.marked_text(), Some("ka"));
            assert_eq!(sm.marked_range(), Some(0..2));
        });
    }

    #[test]
    fn composition_update_dropped_in_normal() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        feed_in_app(vcx, &sm, |sm, _window, cx| {
            sm.composition_update("ka", Some(0..2), None, cx)
        });
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.marked_text(), None);
            assert_eq!(sm.marked_range(), None);
        });
    }

    #[test]
    fn composition_commit_clears_marked_state() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        set_mode(vcx, &sm, "insert");
        feed_in_app(vcx, &sm, |sm, _window, cx| {
            sm.composition_update("ka", Some(0..2), None, cx);
            sm.composition_commit(cx);
        });
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.marked_text(), None);
            assert_eq!(sm.marked_range(), None);
        });
    }

    #[test]
    fn composition_commit_clears_even_in_normal_mode() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        set_mode(vcx, &sm, "insert");
        feed_in_app(vcx, &sm, |sm, _window, cx| {
            sm.composition_update("ka", Some(0..2), None, cx);
        });
        set_mode(vcx, &sm, "normal");
        feed_in_app(vcx, &sm, |sm, _window, cx| sm.composition_commit(cx));
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.marked_text(), None);
            assert_eq!(sm.marked_range(), None);
        });
    }

    #[test]
    fn text_input_clears_marked_state() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        set_mode(vcx, &sm, "insert");
        feed_in_app(vcx, &sm, |sm, _window, cx| {
            sm.composition_update("ka", Some(0..2), None, cx);
            sm.text_input("か", None, cx);
        });
        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.last_text_input(), Some("か"));
            assert_eq!(sm.marked_text(), None);
            assert_eq!(sm.marked_range(), None);
        });
    }

    fn quit_kinds(actions: Vec<Box<dyn stoat_action::Action>>) -> Vec<stoat_action::ActionKind> {
        actions.iter().map(|a| a.kind()).collect()
    }

    #[test]
    fn text_input_then_feed_drops_raw_duplicate_in_insert() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { a -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        set_mode(vcx, &sm, "insert");
        let stroke = key("a");
        let kinds = feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.text_input("a", None, cx);
            quit_kinds(sm.feed(&stroke, window, cx))
        });
        assert!(kinds.is_empty(), "raw duplicate should drop, got {kinds:?}");
    }

    #[test]
    fn feed_after_text_input_only_drops_one_event() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { a -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        set_mode(vcx, &sm, "insert");
        let stroke = key("a");
        let (first, second) = feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.text_input("a", None, cx);
            let first = quit_kinds(sm.feed(&stroke, window, cx));
            let second = quit_kinds(sm.feed(&stroke, window, cx));
            (first, second)
        });
        assert!(first.is_empty(), "first feed should drop duplicate");
        assert_eq!(second, vec![stoat_action::ActionKind::Quit]);
    }

    #[test]
    fn text_input_in_normal_mode_does_not_set_marker() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { a -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        let stroke = key("a");
        let kinds = feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.text_input("a", None, cx);
            quit_kinds(sm.feed(&stroke, window, cx))
        });
        assert_eq!(kinds, vec![stoat_action::ActionKind::Quit]);
    }

    #[test]
    fn mode_change_after_text_input_clears_drop() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { a -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        set_mode(vcx, &sm, "insert");
        sm.update(vcx, |sm, cx| sm.text_input("a", None, cx));
        set_mode(vcx, &sm, "normal");
        let stroke = key("a");
        let kinds = feed_in_app(vcx, &sm, |sm, window, cx| {
            quit_kinds(sm.feed(&stroke, window, cx))
        });
        assert_eq!(kinds, vec![stoat_action::ActionKind::Quit]);
    }

    #[test]
    fn text_input_with_multi_char_does_not_set_marker() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { a -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        set_mode(vcx, &sm, "insert");
        let stroke = key("a");
        let kinds = feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.text_input("ka", None, cx);
            quit_kinds(sm.feed(&stroke, window, cx))
        });
        assert_eq!(kinds, vec![stoat_action::ActionKind::Quit]);
    }

    #[test]
    fn non_matching_feed_clears_marker() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { a -> Quit(); b -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        set_mode(vcx, &sm, "insert");
        let a = key("a");
        let b = key("b");
        let (first, second) = feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.text_input("a", None, cx);
            let first = quit_kinds(sm.feed(&b, window, cx));
            let second = quit_kinds(sm.feed(&a, window, cx));
            (first, second)
        });
        assert_eq!(
            first,
            vec![stoat_action::ActionKind::Quit],
            "non-matching feed processes"
        );
        assert_eq!(
            second,
            vec![stoat_action::ActionKind::Quit],
            "marker was cleared by prior feed"
        );
    }

    #[test]
    fn composition_update_clears_pending_duplicate() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { a -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        set_mode(vcx, &sm, "insert");
        let stroke = key("a");
        let kinds = feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.text_input("a", None, cx);
            sm.composition_update("k", Some(0..1), None, cx);
            quit_kinds(sm.feed(&stroke, window, cx))
        });
        assert_eq!(kinds, vec![stoat_action::ActionKind::Quit]);
    }

    #[test]
    fn composition_commit_clears_pending_duplicate() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { a -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        set_mode(vcx, &sm, "insert");
        let stroke = key("a");
        let kinds = feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.text_input("a", None, cx);
            sm.composition_commit(cx);
            quit_kinds(sm.feed(&stroke, window, cx))
        });
        assert_eq!(kinds, vec![stoat_action::ActionKind::Quit]);
    }

    #[test]
    fn feed_with_modifier_is_not_a_duplicate() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { C-a -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        set_mode(vcx, &sm, "insert");
        let stroke = key_with(
            "a",
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
        );
        let kinds = feed_in_app(vcx, &sm, |sm, window, cx| {
            sm.text_input("a", None, cx);
            quit_kinds(sm.feed(&stroke, window, cx))
        });
        assert_eq!(kinds, vec![stoat_action::ActionKind::Quit]);
    }

    fn new_singleton_editor(vcx: &mut VisualTestContext, text: &str) -> Entity<Editor> {
        use crate::{
            buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
            multi_buffer::MultiBuffer,
        };
        use std::sync::Arc;
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

    fn editor_text(vcx: &mut VisualTestContext, editor: &Entity<Editor>) -> String {
        editor.update(vcx, |ed, cx| {
            ed.multi_buffer().read(cx).snapshot().text().to_string()
        })
    }

    #[test]
    fn text_input_with_active_editor_dispatches_to_apply() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        set_mode(vcx, &sm, "insert");
        let editor = new_singleton_editor(vcx, "hello");
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        feed_in_app(vcx, &sm, |sm, _window, cx| sm.text_input("a", None, cx));
        vcx.run_until_parked();

        assert_eq!(editor_text(vcx, &editor), "ahello");
        sm.read_with(vcx, |sm, _| assert_eq!(sm.last_text_input(), Some("a")));
    }

    #[test]
    fn text_input_without_active_editor_only_records() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        set_mode(vcx, &sm, "insert");

        feed_in_app(vcx, &sm, |sm, _window, cx| sm.text_input("a", None, cx));

        sm.read_with(vcx, |sm, _| assert_eq!(sm.last_text_input(), Some("a")));
    }

    #[test]
    fn composition_update_does_not_dispatch_to_editor() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        set_mode(vcx, &sm, "insert");
        let editor = new_singleton_editor(vcx, "hello");
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));

        feed_in_app(vcx, &sm, |sm, _window, cx| {
            sm.composition_update("k", Some(0..1), None, cx)
        });
        vcx.run_until_parked();

        assert_eq!(editor_text(vcx, &editor), "hello");
    }

    #[test]
    fn keystroke_to_key_event_handles_named_keys() {
        for (name, expected_code) in [
            ("space", KeyCode::Char(' ')),
            ("enter", KeyCode::Enter),
            ("escape", KeyCode::Esc),
            ("tab", KeyCode::Tab),
            ("backspace", KeyCode::Backspace),
            ("delete", KeyCode::Delete),
            ("up", KeyCode::Up),
            ("down", KeyCode::Down),
            ("left", KeyCode::Left),
            ("right", KeyCode::Right),
            ("f1", KeyCode::F(1)),
            ("f12", KeyCode::F(12)),
        ] {
            let stroke = key(name);
            let event = keystroke_to_key_event(&stroke).expect(name);
            assert_eq!(event.code, expected_code, "code for {name}");
            assert_eq!(event.modifiers, KeyModifiers::empty(), "mods for {name}");
        }
    }

    #[test]
    fn keystroke_to_key_event_maps_all_modifiers() {
        let stroke = key_with(
            "a",
            Modifiers {
                control: true,
                alt: true,
                shift: true,
                platform: true,
                function: false,
            },
        );
        let event = keystroke_to_key_event(&stroke).expect("translate");
        assert_eq!(event.code, KeyCode::Char('a'));
        assert_eq!(
            event.modifiers,
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER
        );
    }

    #[test]
    fn keystroke_to_key_event_rejects_multi_char_unknown() {
        let stroke = key("noodle");
        assert!(keystroke_to_key_event(&stroke).is_none());
    }

    /// Anchor entity that owns its own focus handle. Used to seed
    /// focus targets in the focus-output tests so we can verify
    /// `transition_mode` and `restore_prev_focus` move focus to the
    /// expected handle.
    struct FocusAnchor {
        handle: FocusHandle,
    }

    impl FocusAnchor {
        fn new(cx: &mut Context<'_, Self>) -> Self {
            Self {
                handle: cx.focus_handle(),
            }
        }
    }

    impl gpui::Render for FocusAnchor {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    fn make_focus_handle(vcx: &mut VisualTestContext) -> FocusHandle {
        let entity = vcx.update(|_, cx| cx.new(FocusAnchor::new));
        entity.read_with(vcx, |a, _| a.handle.clone())
    }

    #[test]
    fn transition_mode_into_insert_focuses_editor_target() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        let target = make_focus_handle(vcx);
        sm.update(vcx, |sm, _| {
            sm.set_editor_focus_target(Some(target.clone()));
        });

        sm.update_in(vcx, |sm, window, cx| {
            sm.transition_mode("insert", window, cx);
        });

        let focused = vcx.update(|window, _| target.is_focused(window));
        assert!(
            focused,
            "editor target should be focused after entering insert"
        );
    }

    #[test]
    fn transition_mode_into_insert_without_target_is_no_op() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);

        sm.update_in(vcx, |sm, window, cx| {
            sm.transition_mode("insert", window, cx);
        });

        sm.read_with(vcx, |sm, _| assert_eq!(sm.mode(), "insert"));
    }

    #[test]
    fn transition_between_input_modes_does_not_refocus() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        let editor_target = make_focus_handle(vcx);
        let other_handle = make_focus_handle(vcx);
        sm.update(vcx, |sm, _| {
            sm.set_editor_focus_target(Some(editor_target.clone()));
        });

        sm.update_in(vcx, |sm, window, cx| {
            sm.transition_mode("insert", window, cx);
        });
        // Some other code subsequently focuses a different handle.
        vcx.update(|window, _| window.focus(&other_handle));

        sm.update_in(vcx, |sm, window, cx| {
            sm.transition_mode("prompt", window, cx);
        });

        let other_still_focused = vcx.update(|window, _| other_handle.is_focused(window));
        assert!(
            other_still_focused,
            "transition between input modes should not steal focus from existing target"
        );
    }

    #[test]
    fn transition_into_non_input_mode_does_not_focus_editor() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        let editor_target = make_focus_handle(vcx);
        let other_handle = make_focus_handle(vcx);
        sm.update(vcx, |sm, _| {
            sm.set_editor_focus_target(Some(editor_target.clone()));
        });
        vcx.update(|window, _| window.focus(&other_handle));

        sm.update_in(vcx, |sm, window, cx| {
            sm.transition_mode("select", window, cx);
        });

        let editor_focused = vcx.update(|window, _| editor_target.is_focused(window));
        assert!(
            !editor_focused,
            "select is not an input mode; editor target stays unfocused"
        );
    }

    #[test]
    fn set_mode_dispatches_set_mode_binding_to_transition_mode() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { i -> SetMode(insert); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        let stroke = key("i");

        let actions = feed_in_app(vcx, &sm, |sm, window, cx| sm.feed(&stroke, window, cx));

        assert!(actions.is_empty(), "SetMode is intercepted, not dispatched");
        sm.read_with(vcx, |sm, _| assert_eq!(sm.mode(), "insert"));
    }

    #[test]
    fn set_mode_focuses_editor_target_on_entry() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { i -> SetMode(insert); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        let target = make_focus_handle(vcx);
        sm.update(vcx, |sm, _| {
            sm.set_editor_focus_target(Some(target.clone()));
        });
        let stroke = key("i");

        feed_in_app(vcx, &sm, |sm, window, cx| sm.feed(&stroke, window, cx));

        let focused = vcx.update(|window, _| target.is_focused(window));
        assert!(
            focused,
            "editor target should be focused after SetMode(insert) fires"
        );
    }

    #[test]
    fn set_mode_from_set_mode_does_not_appear_in_dispatched_actions() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap(
            "on key {\
                mode == insert { Escape -> SetMode(normal); }\
            }",
        );
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        set_mode(vcx, &sm, "insert");
        let stroke = key("escape");

        let actions = feed_in_app(vcx, &sm, |sm, window, cx| sm.feed(&stroke, window, cx));

        assert!(
            actions.is_empty(),
            "SetMode(normal) should not surface as a dispatched action"
        );
        sm.read_with(vcx, |sm, _| assert_eq!(sm.mode(), "normal"));
    }

    #[test]
    fn capture_then_restore_round_trips() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        let original = make_focus_handle(vcx);
        let other = make_focus_handle(vcx);
        vcx.update(|window, _| window.focus(&original));

        sm.update(vcx, |sm, _| sm.capture_prev_focus(Some(original.clone())));
        vcx.update(|window, _| window.focus(&other));
        sm.update_in(vcx, |sm, window, _cx| sm.restore_prev_focus(window));

        let original_focused = vcx.update(|window, _| original.is_focused(window));
        assert!(
            original_focused,
            "restore_prev_focus should bring captured handle back"
        );
        sm.read_with(vcx, |sm, _| {
            assert!(sm.prev_focused().is_none(), "restore should clear the slot");
        });
    }

    #[test]
    fn restore_prev_focus_without_capture_is_no_op() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        let original = make_focus_handle(vcx);
        vcx.update(|window, _| window.focus(&original));

        sm.update_in(vcx, |sm, window, _cx| sm.restore_prev_focus(window));

        let still_focused = vcx.update(|window, _| original.is_focused(window));
        assert!(still_focused, "no-op restore should leave focus unchanged");
    }

    #[test]
    fn set_pending_find_arms_chord() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        set_mode(vcx, &sm, "normal");

        sm.update(vcx, |sm, cx| {
            sm.set_pending_find(FindKind::NextChar, false, 1, cx)
        });

        sm.read_with(vcx, |sm, _| {
            assert_eq!(
                sm.pending_find().copied(),
                Some(PendingFind {
                    kind: FindKind::NextChar,
                    extend: false,
                    count: 1,
                }),
            );
        });
    }

    #[test]
    fn char_keystroke_consumes_chord_and_emits_apply_action() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        set_mode(vcx, &sm, "normal");
        sm.update(vcx, |sm, cx| {
            sm.set_pending_find(FindKind::TillNextChar, true, 3, cx)
        });

        let stroke = key("x");
        let actions = feed_in_app(vcx, &sm, |sm, window, cx| sm.feed(&stroke, window, cx));

        assert_eq!(actions.len(), 1);
        let apply = actions[0]
            .as_any()
            .downcast_ref::<ApplyFindChar>()
            .expect("ApplyFindChar");
        assert_eq!(apply.kind, FindKind::TillNextChar);
        assert_eq!(apply.ch, 'x');
        assert!(apply.extend);
        assert_eq!(apply.count, 3);
        sm.read_with(vcx, |sm, _| assert!(sm.pending_find().is_none()));
    }

    #[test]
    fn non_char_keystroke_clears_chord_and_falls_through() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { Escape -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        set_mode(vcx, &sm, "normal");
        sm.update(vcx, |sm, cx| {
            sm.set_pending_find(FindKind::NextChar, false, 1, cx)
        });

        let stroke = key("escape");
        let kinds = feed_in_app(vcx, &sm, |sm, window, cx| {
            quit_kinds(sm.feed(&stroke, window, cx))
        });

        assert_eq!(kinds, vec![stoat_action::ActionKind::Quit]);
        sm.read_with(vcx, |sm, _| assert!(sm.pending_find().is_none()));
    }

    #[test]
    fn set_pending_mark_arms_chord() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        set_mode(vcx, &sm, "normal");

        sm.update(vcx, |sm, cx| sm.set_pending_mark(MarkRequest::Set, cx));

        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.pending_mark(), Some(MarkRequest::Set));
        });
    }

    #[test]
    fn char_keystroke_consumes_mark_chord_and_emits_apply_action() {
        let mut cx = TestAppContext::single();
        let (sm, vcx) = new_state_machine(&mut cx);
        set_mode(vcx, &sm, "normal");
        sm.update(vcx, |sm, cx| {
            sm.set_pending_mark(MarkRequest::GotoExact, cx)
        });

        let stroke = key("m");
        let actions = feed_in_app(vcx, &sm, |sm, window, cx| sm.feed(&stroke, window, cx));

        assert_eq!(actions.len(), 1);
        let apply = actions[0]
            .as_any()
            .downcast_ref::<ApplyMarkChar>()
            .expect("ApplyMarkChar");
        assert_eq!(apply.request, MarkRequest::GotoExact);
        assert_eq!(apply.ch, 'm');
        sm.read_with(vcx, |sm, _| assert!(sm.pending_mark().is_none()));
    }

    #[test]
    fn non_char_keystroke_clears_mark_chord_and_falls_through() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { Escape -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        set_mode(vcx, &sm, "normal");
        sm.update(vcx, |sm, cx| sm.set_pending_mark(MarkRequest::Set, cx));

        let stroke = key("escape");
        let kinds = feed_in_app(vcx, &sm, |sm, window, cx| {
            quit_kinds(sm.feed(&stroke, window, cx))
        });

        assert_eq!(kinds, vec![stoat_action::ActionKind::Quit]);
        sm.read_with(vcx, |sm, _| assert!(sm.pending_mark().is_none()));
    }

    #[test]
    fn pending_find_outside_normal_or_select_is_inert() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { a -> Quit(); }");
        let (sm, vcx) = new_state_machine_with_keymap(&mut cx, keymap);
        set_mode(vcx, &sm, "insert");
        sm.update(vcx, |sm, cx| {
            sm.set_pending_find(FindKind::NextChar, false, 1, cx)
        });

        let stroke = key("a");
        let kinds = feed_in_app(vcx, &sm, |sm, window, cx| {
            quit_kinds(sm.feed(&stroke, window, cx))
        });

        assert_eq!(kinds, vec![stoat_action::ActionKind::Quit]);
        sm.read_with(vcx, |sm, _| {
            assert_eq!(
                sm.pending_find().copied(),
                Some(PendingFind {
                    kind: FindKind::NextChar,
                    extend: false,
                    count: 1,
                }),
            );
        });
    }
}
