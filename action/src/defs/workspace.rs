use crate::{
    action::{define_action, impl_gpui_action},
    Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind,
};
use serde::Deserialize;
use std::any::Any;

define_action!(
    NewWorkspaceDef,
    NewWorkspace,
    "NewWorkspace",
    ActionKind::NewWorkspace,
    "create a new workspace",
    "Create a fresh workspace at the current workspace's git root. The new workspace has default panes and no inherited buffers, Claude session, or rebase state. The current workspace is saved and kept in the background; the new workspace becomes active.",
    ActionPriority::Common
);

define_action!(
    CopyWorkspaceDef,
    CopyWorkspace,
    "CopyWorkspace",
    ActionKind::CopyWorkspace,
    "duplicate this workspace",
    "Create a new workspace that clones the current workspace's panes, editors, buffers, and layout. The copy sits at the same git root as the source but does not inherit live Claude session or in-flight rebase state. The current workspace is saved; the copy becomes active.",
    ActionPriority::Normal
);

define_action!(
    SwitchWorkspaceDef,
    SwitchWorkspace,
    "SwitchWorkspace",
    ActionKind::SwitchWorkspace,
    "switch workspaces",
    "Open the workspace picker to choose another open workspace to switch to. The current workspace is saved before the switch; its panes, buffers, and selections are preserved for the return trip.",
    ActionPriority::Common
);

define_action!(
    CloseWorkspaceDef,
    CloseWorkspace,
    "CloseWorkspace",
    ActionKind::CloseWorkspace,
    "close this workspace",
    "Close the active workspace, delete its persisted state, and switch to a sibling workspace. Refuses when only one workspace is open.",
    ActionPriority::Normal
);

const RENAME_WORKSPACE_PARAMS: &[ParamDef] = &[ParamDef {
    name: "name",
    kind: ParamKind::String,
    required: true,
    description: "New display name for the active workspace. Empty string re-engages the default basename fallback.",
}];

#[derive(Debug)]
pub struct RenameWorkspaceDef;

impl ActionDef for RenameWorkspaceDef {
    fn name(&self) -> &'static str {
        "RenameWorkspace"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::RenameWorkspace
    }

    fn params(&self) -> &'static [ParamDef] {
        RENAME_WORKSPACE_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "rename this workspace"
    }

    fn long_desc(&self) -> &'static str {
        "Set a user-facing display name for the active workspace. The name is persisted with the workspace and shown in the workspace picker and pane footer. Pass an empty string to revert to the default `git_root.file_name()` fallback."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RenameWorkspace {
    pub name: String,
}

impl RenameWorkspace {
    pub const DEF: &RenameWorkspaceDef = &RenameWorkspaceDef;
}

impl Action for RenameWorkspace {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl_gpui_action!(RenameWorkspace, "RenameWorkspace");

#[cfg(test)]
mod tests {
    use super::*;

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
        let rename = RenameWorkspace {
            name: "x".to_string(),
        };
        assert_eq!(rename.kind(), ActionKind::RenameWorkspace);
        assert_eq!(rename.def().name(), "RenameWorkspace");
    }

    #[test]
    fn zero_arg_actions_have_no_params() {
        assert!(NewWorkspace.def().params().is_empty());
        assert!(CopyWorkspace.def().params().is_empty());
        assert!(SwitchWorkspace.def().params().is_empty());
        assert!(CloseWorkspace.def().params().is_empty());
    }

    #[test]
    fn rename_workspace_takes_name_param() {
        let params = RenameWorkspaceDef.params();
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "name");
        assert_eq!(params[0].kind, ParamKind::String);
        assert!(params[0].required);
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(NewWorkspace);
        assert!(action.as_any().downcast_ref::<NewWorkspace>().is_some());

        let rename: Box<dyn Action> = Box::new(RenameWorkspace {
            name: "alpha".to_string(),
        });
        let recovered = rename
            .as_any()
            .downcast_ref::<RenameWorkspace>()
            .expect("downcast");
        assert_eq!(recovered.name, "alpha");
    }
}
