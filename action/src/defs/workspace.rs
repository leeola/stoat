use crate::{action::define_action, ActionKind};

define_action!(
    NewWorkspaceDef,
    NewWorkspace,
    "NewWorkspace",
    ActionKind::NewWorkspace,
    "create a new workspace",
    "Create a fresh workspace at the current workspace's git root. The new workspace has default panes and no inherited buffers, Claude session, or rebase state. The current workspace is saved and kept in the background; the new workspace becomes active."
);

define_action!(
    CopyWorkspaceDef,
    CopyWorkspace,
    "CopyWorkspace",
    ActionKind::CopyWorkspace,
    "duplicate this workspace",
    "Create a new workspace that clones the current workspace's panes, editors, buffers, and layout. The copy sits at the same git root as the source but does not inherit live Claude session or in-flight rebase state. The current workspace is saved; the copy becomes active."
);

define_action!(
    SwitchWorkspaceDef,
    SwitchWorkspace,
    "SwitchWorkspace",
    ActionKind::SwitchWorkspace,
    "switch workspaces",
    "Open the workspace picker to choose another open workspace to switch to. The current workspace is saved before the switch; its panes, buffers, and selections are preserved for the return trip."
);

define_action!(
    CloseWorkspaceDef,
    CloseWorkspace,
    "CloseWorkspace",
    ActionKind::CloseWorkspace,
    "close this workspace",
    "Close the active workspace, delete its persisted state, and switch to a sibling workspace. Refuses when only one workspace is open."
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn kinds_and_names() {
        assert_eq!(NewWorkspace.kind(), ActionKind::NewWorkspace);
        assert_eq!(NewWorkspace.def().name(), "NewWorkspace");
        assert_eq!(CopyWorkspace.kind(), ActionKind::CopyWorkspace);
        assert_eq!(CopyWorkspace.def().name(), "CopyWorkspace");
        assert_eq!(SwitchWorkspace.kind(), ActionKind::SwitchWorkspace);
        assert_eq!(SwitchWorkspace.def().name(), "SwitchWorkspace");
        assert_eq!(CloseWorkspace.kind(), ActionKind::CloseWorkspace);
        assert_eq!(CloseWorkspace.def().name(), "CloseWorkspace");
    }

    #[test]
    fn all_zero_arg() {
        assert!(NewWorkspace.def().params().is_empty());
        assert!(CopyWorkspace.def().params().is_empty());
        assert!(SwitchWorkspace.def().params().is_empty());
        assert!(CloseWorkspace.def().params().is_empty());
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(NewWorkspace);
        assert!(action.as_any().downcast_ref::<NewWorkspace>().is_some());
    }
}
