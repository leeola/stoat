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
