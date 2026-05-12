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
    fn neither_action_is_palette_visible() {
        assert!(!SetActivePane::DEF.palette_visible());
        assert!(!ClickAt::DEF.palette_visible());
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
