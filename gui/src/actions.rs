use crate::editor::actions::{marks::MarkRequest, movement::FindKind, textobject::TextobjectMode};
use std::any::Any;
use stoat_action::{Action, ActionDef, ActionKind, ActionPriority, ParamDef};

/// Activate the pane identified by `pane_id` in the workspace's
/// pane tree. Constructed by `Pane::render`'s mouse-down handler;
/// the dispatch arm in `Workspace::dispatch_action` converts the
/// `u64` back into a `stoat::pane::PaneId` via
/// `slotmap::KeyData::from_ffi`. Not registered with the keymap
/// registry -- this action only flows through code, never through
/// `keymap::lookup`.
#[derive(Debug, Clone, Copy)]
pub struct SetActivePane {
    pub pane_id: u64,
}

#[derive(Debug)]
pub struct SetActivePaneDef;

impl ActionDef for SetActivePaneDef {
    fn name(&self) -> &'static str {
        "SetActivePane"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::SetActivePane
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "activate a pane"
    }

    fn long_desc(&self) -> &'static str {
        "Make the pane identified by `pane_id` the workspace's focused pane. Dispatched by mouse-down on a pane element; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl SetActivePane {
    pub const DEF: &SetActivePaneDef = &SetActivePaneDef;
}

impl Action for SetActivePane {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Click at a grid position inside the active editor's text
/// region. Constructed by the editor's mouse-down handler with
/// `(row, col)` translated from pixel coordinates. Not registered
/// with the keymap registry.
#[derive(Debug, Clone, Copy)]
pub struct ClickAt {
    pub row: u32,
    pub col: u32,
}

#[derive(Debug)]
pub struct ClickAtDef;

impl ActionDef for ClickAtDef {
    fn name(&self) -> &'static str {
        "ClickAt"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ClickAt
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "click at editor grid position"
    }

    fn long_desc(&self) -> &'static str {
        "Place the cursor (or extend the selection) at the row/column inside the active editor's text region. Dispatched by mouse-down on the editor's rendered text area; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl ClickAt {
    pub const DEF: &ClickAtDef = &ClickAtDef;
}

impl Action for ClickAt {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Extend the active editor's primary selection head to the grid
/// position `(row, col)`. Constructed by the editor's mouse-move
/// handler while the left button is held; the anchor stays put and
/// the head moves under the cursor. Not registered with the keymap
/// registry.
#[derive(Debug, Clone, Copy)]
pub struct DragSelectTo {
    pub row: u32,
    pub col: u32,
}

#[derive(Debug)]
pub struct DragSelectToDef;

impl ActionDef for DragSelectToDef {
    fn name(&self) -> &'static str {
        "DragSelectTo"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::DragSelectTo
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "drag-select to editor grid position"
    }

    fn long_desc(&self) -> &'static str {
        "Extend the active editor's primary selection so its anchor stays put and its head moves to the row/column inside the editor's text region. Dispatched by mouse-drag on the editor's rendered text area; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl DragSelectTo {
    pub const DEF: &DragSelectToDef = &DragSelectToDef;
}

impl Action for DragSelectTo {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Record a hover position inside the active editor's text region.
/// Constructed by the editor's mouse-move handler after the 50ms
/// hover debounce fires; the LSP hover popup observes this position
/// to compute the hover request. Not registered with the keymap
/// registry.
#[derive(Debug, Clone, Copy)]
pub struct HoverAt {
    pub row: u32,
    pub col: u32,
}

#[derive(Debug)]
pub struct HoverAtDef;

impl ActionDef for HoverAtDef {
    fn name(&self) -> &'static str {
        "HoverAt"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::HoverAt
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "hover at editor grid position"
    }

    fn long_desc(&self) -> &'static str {
        "Record the hover position inside the active editor's text region so the LSP hover popup can query the server. Dispatched after a 50ms debounce on the editor's mouse-move handler; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl HoverAt {
    pub const DEF: &HoverAtDef = &HoverAtDef;
}

impl Action for HoverAt {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Chord-completing action for the find/till character pipeline.
/// Synthesized by [`crate::input_state_machine::InputStateMachine::feed`]
/// when a `KeyCode::Char` keystroke lands while
/// [`crate::input_state_machine::InputStateMachine::pending_find`] is
/// set; carries the resolved char plus the priming-side flags so
/// `Workspace::dispatch_action` can route the lookup to the active
/// editor's `handle_find_char` without re-consulting state. Not
/// keymap-bindable -- only the input pipeline constructs it.
#[derive(Debug, Clone, Copy)]
pub struct ApplyFindChar {
    pub kind: FindKind,
    pub ch: char,
    pub extend: bool,
    pub count: u32,
}

#[derive(Debug)]
pub struct ApplyFindCharDef;

impl ActionDef for ApplyFindCharDef {
    fn name(&self) -> &'static str {
        "ApplyFindChar"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ApplyFindChar
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "apply pending find/till chord"
    }

    fn long_desc(&self) -> &'static str {
        "Run the pending Find/Till chord against the active editor with the chord-completing character. Synthesized by the input pipeline after a `FindNextChar`/`FindPrevChar`/`TillNextChar`/`TillPrevChar` action arms the chord; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl ApplyFindChar {
    pub const DEF: &ApplyFindCharDef = &ApplyFindCharDef;
}

impl Action for ApplyFindChar {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Chord-completing action for the mark pipeline. Synthesized by
/// [`crate::input_state_machine::InputStateMachine::feed`] when a
/// `KeyCode::Char` keystroke lands while
/// [`crate::input_state_machine::InputStateMachine::pending_mark`]
/// is set; carries the chord-target character plus the priming
/// request so `Workspace::dispatch_action` can route the lookup to
/// the active editor's `handle_set_mark` / `handle_goto_mark`. Not
/// keymap-bindable -- only the input pipeline constructs it.
#[derive(Debug, Clone, Copy)]
pub struct ApplyMarkChar {
    pub request: MarkRequest,
    pub ch: char,
}

#[derive(Debug)]
pub struct ApplyMarkCharDef;

impl ActionDef for ApplyMarkCharDef {
    fn name(&self) -> &'static str {
        "ApplyMarkChar"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ApplyMarkChar
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "apply pending mark chord"
    }

    fn long_desc(&self) -> &'static str {
        "Run the pending Set/GotoMark/GotoMarkExact chord against the active editor with the chord-completing character. Synthesized by the input pipeline after a `SetMark`/`GotoMark`/`GotoMarkExact` action arms the chord; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl ApplyMarkChar {
    pub const DEF: &ApplyMarkCharDef = &ApplyMarkCharDef;
}

impl Action for ApplyMarkChar {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Chord-completion action synthesized by
/// [`crate::input_state_machine::InputStateMachine::feed`] after a
/// [`stoat_action::SelectRegister`] action arms the chord. Carries
/// the chord-completing character; the workspace dispatch resolves
/// it through [`stoat::register::register_for_char`]
/// and stores the matching [`stoat::register::Register`] in
/// `Workspace::selected_register` for the next yank/paste.
#[derive(Debug)]
pub struct ApplyRegisterSelectChar {
    pub ch: char,
}

#[derive(Debug)]
pub struct ApplyRegisterSelectCharDef;

impl ActionDef for ApplyRegisterSelectCharDef {
    fn name(&self) -> &'static str {
        "ApplyRegisterSelectChar"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ApplyRegisterSelectChar
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "apply pending register-select chord"
    }

    fn long_desc(&self) -> &'static str {
        "Run the pending SelectRegister chord with the chord-completing character; sets the workspace's pending register for the next yank/paste. Synthesized by the input pipeline after a `SelectRegister` action arms the chord; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl ApplyRegisterSelectChar {
    pub const DEF: &ApplyRegisterSelectCharDef = &ApplyRegisterSelectCharDef;
}

impl Action for ApplyRegisterSelectChar {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Chord-completion action synthesized by
/// [`crate::input_state_machine::InputStateMachine::feed`] after a
/// [`stoat_action::ReplaceChar`] action arms the chord. Carries
/// the chord-completing character; the workspace dispatch routes
/// it to the active editor's
/// [`crate::editor::Editor::replace_char_in_selections`] which
/// replaces every char in each non-empty selection with `ch`.
#[derive(Debug)]
pub struct ApplyReplaceChar {
    pub ch: char,
}

#[derive(Debug)]
pub struct ApplyReplaceCharDef;

impl ActionDef for ApplyReplaceCharDef {
    fn name(&self) -> &'static str {
        "ApplyReplaceChar"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ApplyReplaceChar
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "apply pending replace-char chord"
    }

    fn long_desc(&self) -> &'static str {
        "Run the pending ReplaceChar chord with the chord-completing character; replaces every char of each non-empty selection on the active editor with that character. Synthesized by the input pipeline after a `ReplaceChar` action arms the chord; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl ApplyReplaceChar {
    pub const DEF: &ApplyReplaceCharDef = &ApplyReplaceCharDef;
}

impl Action for ApplyReplaceChar {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Chord-completion action synthesized by
/// [`crate::input_state_machine::InputStateMachine::feed`] after a
/// [`stoat_action::InsertRegister`] action arms the chord in
/// insert mode. Carries the chord-completing character; the
/// workspace dispatch resolves the matching
/// [`stoat::register::Register`] via
/// [`stoat::register::register_for_char`] and
/// inserts the register's content at every cursor on the active
/// editor.
#[derive(Debug)]
pub struct ApplyInsertRegisterChar {
    pub ch: char,
}

#[derive(Debug)]
pub struct ApplyInsertRegisterCharDef;

impl ActionDef for ApplyInsertRegisterCharDef {
    fn name(&self) -> &'static str {
        "ApplyInsertRegisterChar"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ApplyInsertRegisterChar
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "apply pending insert-register chord"
    }

    fn long_desc(&self) -> &'static str {
        "Run the pending InsertRegister chord with the chord-completing character; inserts the resolved register's text at every cursor on the active editor. Synthesized by the input pipeline after an `InsertRegister` action arms the chord in insert mode; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl ApplyInsertRegisterChar {
    pub const DEF: &ApplyInsertRegisterCharDef = &ApplyInsertRegisterCharDef;
}

impl Action for ApplyInsertRegisterChar {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Chord-completion action synthesized by
/// [`crate::input_state_machine::InputStateMachine::feed`] after a
/// [`stoat_action::ReplayMacro`] action arms the chord. Carries the
/// chord-completing character; workspace dispatch resolves the
/// register and re-feeds the stored keystroke sequence through the
/// input pipeline.
#[derive(Debug)]
pub struct ApplyReplayMacroChar {
    pub ch: char,
}

#[derive(Debug)]
pub struct ApplyReplayMacroCharDef;

impl ActionDef for ApplyReplayMacroCharDef {
    fn name(&self) -> &'static str {
        "ApplyReplayMacroChar"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ApplyReplayMacroChar
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "apply pending replay-macro chord"
    }

    fn long_desc(&self) -> &'static str {
        "Run the pending ReplayMacro chord with the chord-completing character; resolves the register, looks up its stored macro, and re-feeds each captured keystroke through the input pipeline. Synthesized by the input pipeline after a `ReplayMacro` action arms the chord; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl ApplyReplayMacroChar {
    pub const DEF: &ApplyReplayMacroCharDef = &ApplyReplayMacroCharDef;
}

impl Action for ApplyReplayMacroChar {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Chord-completion action synthesized by
/// [`crate::input_state_machine::InputStateMachine::feed`] after a
/// [`stoat_action::SurroundAdd`] action arms the chord. Carries the
/// chord-completing character; workspace dispatch resolves the
/// canonical [`open`, `close`] pair via
/// [`stoat_language::surround::surround_pair_for`] and
/// wraps every non-empty selection.
#[derive(Debug)]
pub struct ApplySurroundAddChar {
    pub ch: char,
}

#[derive(Debug)]
pub struct ApplySurroundAddCharDef;

impl ActionDef for ApplySurroundAddCharDef {
    fn name(&self) -> &'static str {
        "ApplySurroundAddChar"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ApplySurroundAddChar
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "apply pending surround-add chord"
    }

    fn long_desc(&self) -> &'static str {
        "Run the pending SurroundAdd chord with the chord-completing character; wraps every non-empty selection with the resolved pair. Synthesized by the input pipeline after a `SurroundAdd` action arms the chord; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl ApplySurroundAddChar {
    pub const DEF: &ApplySurroundAddCharDef = &ApplySurroundAddCharDef;
}

impl Action for ApplySurroundAddChar {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Chord-completion action synthesized after a
/// [`stoat_action::SurroundDelete`] arms the chord. Carries the
/// chord-completing character; workspace dispatch finds the
/// nearest enclosing pair for each cursor and removes both ends.
#[derive(Debug)]
pub struct ApplySurroundDeleteChar {
    pub ch: char,
}

#[derive(Debug)]
pub struct ApplySurroundDeleteCharDef;

impl ActionDef for ApplySurroundDeleteCharDef {
    fn name(&self) -> &'static str {
        "ApplySurroundDeleteChar"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ApplySurroundDeleteChar
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "apply pending surround-delete chord"
    }

    fn long_desc(&self) -> &'static str {
        "Run the pending SurroundDelete chord with the chord-completing character; removes the nearest enclosing pair around each cursor. Synthesized by the input pipeline after a `SurroundDelete` action arms the chord; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl ApplySurroundDeleteChar {
    pub const DEF: &ApplySurroundDeleteCharDef = &ApplySurroundDeleteCharDef;
}

impl Action for ApplySurroundDeleteChar {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Chord-completion action synthesized after the two-stage
/// [`stoat_action::SurroundReplace`] chord captures both
/// chars. Workspace dispatch resolves the existing pair for
/// `from` and replaces it with the canonical pair for `to`.
#[derive(Debug)]
pub struct ApplySurroundReplaceChar {
    pub from: char,
    pub to: char,
}

#[derive(Debug)]
pub struct ApplySurroundReplaceCharDef;

impl ActionDef for ApplySurroundReplaceCharDef {
    fn name(&self) -> &'static str {
        "ApplySurroundReplaceChar"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ApplySurroundReplaceChar
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "apply pending surround-replace chord"
    }

    fn long_desc(&self) -> &'static str {
        "Run the pending SurroundReplace chord with the two chord-completing characters (from then to); replaces the nearest enclosing pair around each cursor. Synthesized by the input pipeline after a `SurroundReplace` action arms the chord and both chars are captured; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl ApplySurroundReplaceChar {
    pub const DEF: &ApplySurroundReplaceCharDef = &ApplySurroundReplaceCharDef;
}

impl Action for ApplySurroundReplaceChar {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Chord-completion action synthesized after a
/// [`stoat_action::SelectTextobjectAround`] or
/// [`stoat_action::SelectTextobjectInner`] arms the chord. Carries
/// the captured mode plus the chord-completing character (the
/// textobject type: `p`/`f`/`t`/`a`/`c`); workspace dispatch
/// resolves the matching range and installs it as the primary
/// selection.
#[derive(Debug)]
pub struct ApplyTextobjectChar {
    pub mode: TextobjectMode,
    pub ch: char,
}

#[derive(Debug)]
pub struct ApplyTextobjectCharDef;

impl ActionDef for ApplyTextobjectCharDef {
    fn name(&self) -> &'static str {
        "ApplyTextobjectChar"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ApplyTextobjectChar
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "apply pending textobject-select chord"
    }

    fn long_desc(&self) -> &'static str {
        "Run the pending SelectTextobject chord with the chord-completing character; replaces every selection with the textobject range. Synthesized by the input pipeline after a `SelectTextobjectAround` / `SelectTextobjectInner` action arms the chord; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl ApplyTextobjectChar {
    pub const DEF: &ApplyTextobjectCharDef = &ApplyTextobjectCharDef;
}

impl Action for ApplyTextobjectChar {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Synthesized by [`crate::input_state_machine::InputStateMachine::feed`]
/// when an in-progress `goto_word` chord resolves to a unique label.
/// Carries the absolute byte offset of the labelled word's start;
/// workspace dispatch routes it to the active editor's primary
/// cursor.
#[derive(Debug)]
pub struct GotoWordJump {
    pub byte_offset: usize,
}

#[derive(Debug)]
pub struct GotoWordJumpDef;

impl ActionDef for GotoWordJumpDef {
    fn name(&self) -> &'static str {
        "GotoWordJump"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::GotoWordJump
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "jump cursor to labelled goto_word target"
    }

    fn long_desc(&self) -> &'static str {
        "Resolve the pending `GotoWord` chord by jumping every selection's primary cursor to the matched label's byte offset. Synthesized by the input pipeline after the typed label fully matches; not user-bindable."
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Rare
    }
}

impl GotoWordJump {
    pub const DEF: &GotoWordJumpDef = &GotoWordJumpDef;
}

impl Action for GotoWordJump {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_active_pane_carries_id() {
        let action = SetActivePane { pane_id: 42 };
        assert_eq!(action.kind(), ActionKind::SetActivePane);
        assert_eq!(action.def().name(), "SetActivePane");
        assert_eq!(action.pane_id, 42);
    }

    #[test]
    fn click_at_carries_grid_position() {
        let action = ClickAt { row: 5, col: 12 };
        assert_eq!(action.kind(), ActionKind::ClickAt);
        assert_eq!(action.def().name(), "ClickAt");
        assert_eq!((action.row, action.col), (5, 12));
    }

    #[test]
    fn drag_select_to_carries_grid_position() {
        let action = DragSelectTo { row: 7, col: 3 };
        assert_eq!(action.kind(), ActionKind::DragSelectTo);
        assert_eq!(action.def().name(), "DragSelectTo");
        assert_eq!((action.row, action.col), (7, 3));
    }

    #[test]
    fn hover_at_carries_grid_position() {
        let action = HoverAt { row: 2, col: 9 };
        assert_eq!(action.kind(), ActionKind::HoverAt);
        assert_eq!(action.def().name(), "HoverAt");
        assert_eq!((action.row, action.col), (2, 9));
    }

    #[test]
    fn no_mouse_action_is_palette_visible() {
        assert!(!SetActivePane::DEF.palette_visible());
        assert!(!ClickAt::DEF.palette_visible());
        assert!(!DragSelectTo::DEF.palette_visible());
        assert!(!HoverAt::DEF.palette_visible());
    }

    #[test]
    fn downcast_round_trip() {
        let boxed: Box<dyn Action> = Box::new(SetActivePane { pane_id: 7 });
        let recovered = boxed
            .as_any()
            .downcast_ref::<SetActivePane>()
            .expect("downcast");
        assert_eq!(recovered.pane_id, 7);
    }
}
