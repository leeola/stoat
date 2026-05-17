use crate::editor::actions::{marks::MarkRequest, movement::FindKind};
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
/// it through [`stoat::action_handlers::yank::register_for_char`]
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
