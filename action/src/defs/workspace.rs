use crate::{
    action::define_action, Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind,
    ValueSource,
};
use std::any::Any;

define_action!(
    NewWorkspaceDef,
    NewWorkspace,
    "NewWorkspace",
    ActionKind::NewWorkspace,
    "create a new workspace",
    "Create a fresh workspace at the current workspace's git root. The new workspace has default panes and no inherited buffers or rebase state. The current workspace is saved and kept in the background; the new workspace becomes active.",
    ActionPriority::Common
);

define_action!(
    CopyWorkspaceDef,
    CopyWorkspace,
    "CopyWorkspace",
    ActionKind::CopyWorkspace,
    "duplicate this workspace",
    "Create a new workspace that clones the current workspace's panes, editors, buffers, and layout. The copy sits at the same git root as the source but does not inherit in-flight rebase state. The current workspace is saved; the copy becomes active."
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
    WorkspacePickerNextDef,
    WorkspacePickerNext,
    "WorkspacePickerNext",
    ActionKind::WorkspacePickerNext,
    "next workspace row",
    "Move the workspace picker's selection to the next row.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    WorkspacePickerCompleteDef,
    WorkspacePickerComplete,
    "WorkspacePickerComplete",
    ActionKind::WorkspacePickerComplete,
    "complete selected workspace",
    "Complete the highlighted workspace's name into the picker's filter input, \
     replacing what was typed. The completed workspace stays selected, so a \
     following Enter switches to it. Bound by default to Tab while the picker \
     is open; a no-op when the list is empty.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    WorkspacePickerPrevDef,
    WorkspacePickerPrev,
    "WorkspacePickerPrev",
    ActionKind::WorkspacePickerPrev,
    "previous workspace row",
    "Move the workspace picker's selection to the previous row.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    WorkspacePickerSelectDef,
    WorkspacePickerSelect,
    "WorkspacePickerSelect",
    ActionKind::WorkspacePickerSelect,
    "open selected workspace",
    "Switch to the workspace under the picker's selection, saving the current one first.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    WorkspacePickerCloseDef,
    WorkspacePickerClose,
    "WorkspacePickerClose",
    ActionKind::WorkspacePickerClose,
    "close workspace picker",
    "Dismiss the workspace picker without switching.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CloseWorkspaceDef,
    CloseWorkspace,
    "CloseWorkspace",
    ActionKind::CloseWorkspace,
    "close this workspace",
    "Close the active workspace, delete its persisted state, and switch to a sibling workspace. Refuses when only one workspace is open."
);

const RENAME_WORKSPACE_PARAMS: &[ParamDef] = &[ParamDef {
    name: "name",
    kind: ParamKind::String,
    value_source: ValueSource::None,
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

#[derive(Debug)]
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

const SET_CWD_PARAMS: &[ParamDef] = &[ParamDef {
    name: "path",
    kind: ParamKind::String,
    value_source: ValueSource::Directories,
    required: true,
    description: "Directory to set as the active workspace's working directory. A relative path resolves against the current root.",
}];

#[derive(Debug)]
pub struct SetCwdDef;

impl ActionDef for SetCwdDef {
    fn name(&self) -> &'static str {
        "SetCwd"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::SetCwd
    }

    fn params(&self) -> &'static [ParamDef] {
        SET_CWD_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "change the working directory"
    }

    fn long_desc(&self) -> &'static str {
        "Set the active workspace's working directory to the given path. Subsequent file-finder, diff, and review operations resolve against the new root."
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["cd"]
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug)]
pub struct SetCwd {
    pub path: String,
}

impl SetCwd {
    pub const DEF: &SetCwdDef = &SetCwdDef;
}

impl Action for SetCwd {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct ShowCwdDef;

impl ActionDef for ShowCwdDef {
    fn name(&self) -> &'static str {
        "ShowCwd"
    }

    fn command_name(&self) -> Option<&'static str> {
        Some("pwd")
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ShowCwd
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "show the working directory"
    }

    fn long_desc(&self) -> &'static str {
        "Report the active workspace's working directory as a status message. This is the root the file finder, diff, and review resolve against, not the process working directory."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug)]
pub struct ShowCwd;

impl ShowCwd {
    pub const DEF: &ShowCwdDef = &ShowCwdDef;
}

impl Action for ShowCwd {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct ReloadEnvDef;

impl ActionDef for ReloadEnvDef {
    fn name(&self) -> &'static str {
        "ReloadEnv"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ReloadEnv
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "reload the project environment"
    }

    fn long_desc(&self) -> &'static str {
        "Re-run direnv against the active workspace's root and replace its stored environment diff. Runs even when `direnv.load` is disabled, since invoking it is explicit intent. Child processes spawned after the reload pick up the new environment; already-running ones keep theirs."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug)]
pub struct ReloadEnv;

impl ReloadEnv {
    pub const DEF: &ReloadEnvDef = &ReloadEnvDef;
}

impl Action for ReloadEnv {
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
        let set_cwd = SetCwd {
            path: "/x".to_string(),
        };
        assert_eq!(set_cwd.kind(), ActionKind::SetCwd);
        assert_eq!(set_cwd.def().name(), "SetCwd");
        assert_eq!(set_cwd.def().aliases(), &["cd"]);
        assert_eq!(ShowCwd.kind(), ActionKind::ShowCwd);
        assert_eq!(ShowCwd.def().name(), "ShowCwd");
        assert_eq!(ShowCwd.def().command_name(), Some("pwd"));
        assert_eq!(ReloadEnv.kind(), ActionKind::ReloadEnv);
        assert_eq!(ReloadEnv.def().name(), "ReloadEnv");
        assert_eq!(
            ReloadEnv.def().command_name(),
            None,
            "reload-env is the derived name"
        );
    }

    #[test]
    fn set_cwd_takes_path_param() {
        let params = SetCwdDef.params();
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "path");
        assert_eq!(params[0].kind, ParamKind::String);
        assert!(params[0].required);
    }

    #[test]
    fn zero_arg_actions_have_no_params() {
        assert!(NewWorkspace.def().params().is_empty());
        assert!(CopyWorkspace.def().params().is_empty());
        assert!(SwitchWorkspace.def().params().is_empty());
        assert!(CloseWorkspace.def().params().is_empty());
        assert!(ShowCwd.def().params().is_empty());
        assert!(ReloadEnv.def().params().is_empty());
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
