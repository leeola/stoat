use crate::{
    action::define_action, Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind,
};
use serde::Deserialize;
use std::any::Any;

define_action!(
    NewWorkspaceDef,
    NewWorkspace,
    "workspace-new",
    ActionKind::NewWorkspace,
    "create a new workspace",
    "Create a fresh workspace at the current workspace's git root. The new workspace has default panes and no inherited buffers, Claude session, or rebase state. The current workspace is saved and kept in the background; the new workspace becomes active.",
    ActionPriority::Common
);

define_action!(
    CopyWorkspaceDef,
    CopyWorkspace,
    "workspace-copy",
    ActionKind::CopyWorkspace,
    "duplicate this workspace",
    "Create a new workspace that clones the current workspace's panes, editors, buffers, and layout. The copy sits at the same git root as the source but does not inherit live Claude session or in-flight rebase state. The current workspace is saved; the copy becomes active.",
    ActionPriority::Normal
);

define_action!(
    SwitchWorkspaceDef,
    SwitchWorkspace,
    "workspace-switch",
    ActionKind::SwitchWorkspace,
    "switch workspaces",
    "Open the workspace picker to choose another open workspace to switch to. The current workspace is saved before the switch; its panes, buffers, and selections are preserved for the return trip.",
    ActionPriority::Common
);

define_action!(
    CloseWorkspaceDef,
    CloseWorkspace,
    "workspace-close",
    ActionKind::CloseWorkspace,
    "close this workspace",
    "Close the active workspace, delete its persisted state, and switch to a sibling workspace. Refuses when only one workspace is open.",
    ActionPriority::Normal
);

define_action!(
    OpenWorkspacePickerDef,
    OpenWorkspacePicker,
    "workspaces",
    ActionKind::OpenWorkspacePicker,
    "resume a saved workspace",
    "Open a picker listing every persisted workspace under the current git root. Confirm rehydrates the chosen workspace into the active window, replacing the current panes, buffers, docks, and Claude session.",
    ActionPriority::Common
);

define_action!(
    ToggleProjectTreeDef,
    ToggleProjectTree,
    "project-tree",
    ActionKind::ToggleProjectTree,
    "toggle the project file tree",
    "Open the project file tree in a left dock listing the workspace directory contents, or close it if already open.",
    ActionPriority::Common
);

define_action!(
    ToggleOutlinePanelDef,
    ToggleOutlinePanel,
    "outline-panel",
    ActionKind::ToggleOutlinePanel,
    "toggle the symbol outline panel",
    "Open a right dock showing the active editor's symbol tree, tracking the cursor and refreshing as the buffer changes, or close it if already open.",
    ActionPriority::Common
);

define_action!(
    ToggleDiagnosticsPanelDef,
    ToggleDiagnosticsPanel,
    "diagnostics-panel",
    ActionKind::ToggleDiagnosticsPanel,
    "toggle the diagnostics panel",
    "Open a right dock listing every diagnostic across open buffers grouped by file, or close it if already open.",
    ActionPriority::Common
);

define_action!(
    OpenMarkdownPreviewDef,
    OpenMarkdownPreview,
    "markdown-preview",
    ActionKind::OpenMarkdownPreview,
    "open markdown preview",
    "Split the active pane and open a live-updating rendered preview of the active markdown buffer alongside it.",
    ActionPriority::Common
);

const RENAME_WORKSPACE_PARAMS: &[ParamDef] = &[ParamDef {
    name: "name",
    kind: ParamKind::String,
    required: false,
    description: "New display name for the active workspace. Empty string re-engages the default basename fallback. Parameterless invocation routes through a name-input modal in the GUI.",
}];

#[derive(Debug)]
pub struct RenameWorkspaceDef;

impl ActionDef for RenameWorkspaceDef {
    fn name(&self) -> &'static str {
        "workspace-rename"
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

const SET_CWD_PARAMS: &[ParamDef] = &[ParamDef {
    name: "path",
    kind: ParamKind::String,
    required: true,
    description: "Filesystem path to set as the active workspace's working directory.",
}];

#[derive(Debug)]
pub struct SetCwdDef;

impl ActionDef for SetCwdDef {
    fn name(&self) -> &'static str {
        "cd"
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
        "Set the active workspace's working directory to the given path. Subsequent file-finder, review, and git operations resolve against the new root."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

const SCREENSHOT_PARAMS: &[ParamDef] = &[ParamDef {
    name: "path",
    kind: ParamKind::String,
    required: false,
    description: "Output file for the capture. Empty writes a timestamped screenshot-<millis>.png under the workspace root; a relative path resolves under that root.",
}];
#[derive(Debug)]
pub struct ScreenshotDef;

impl ActionDef for ScreenshotDef {
    fn name(&self) -> &'static str {
        "screenshot"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::Screenshot
    }

    fn params(&self) -> &'static [ParamDef] {
        SCREENSHOT_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "capture the window to an image"
    }

    fn long_desc(&self) -> &'static str {
        "Capture the rendered editor window to a PNG. With no path, writes a timestamped file under the workspace root and logs its location. macOS only; the capture needs Screen Recording permission."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Screenshot {
    pub path: String,
}
impl Screenshot {
    pub const DEF: &ScreenshotDef = &ScreenshotDef;
}
impl Action for Screenshot {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

define_action!(
    PwdDef,
    Pwd,
    "pwd",
    ActionKind::Pwd,
    "report the working directory",
    "Write the active workspace's working directory to the log at info level. The working directory is the workspace git root that the file finder, review, and git operations resolve against.",
    ActionPriority::Common
);

define_action!(
    EnvDef,
    Env,
    "env",
    ActionKind::Env,
    "report STOAT_ environment variables",
    "Write every environment variable whose name begins with STOAT_ to the log at info level as name/value pairs, reporting which Stoat-specific overrides are active in the current process.",
    ActionPriority::Common
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_names() {
        assert_eq!(NewWorkspace.kind(), ActionKind::NewWorkspace);
        assert_eq!(NewWorkspace.def().name(), "workspace-new");
        assert_eq!(CopyWorkspace.kind(), ActionKind::CopyWorkspace);
        assert_eq!(CopyWorkspace.def().name(), "workspace-copy");
        assert_eq!(SwitchWorkspace.kind(), ActionKind::SwitchWorkspace);
        assert_eq!(SwitchWorkspace.def().name(), "workspace-switch");
        assert_eq!(CloseWorkspace.kind(), ActionKind::CloseWorkspace);
        assert_eq!(CloseWorkspace.def().name(), "workspace-close");
        assert_eq!(OpenWorkspacePicker.kind(), ActionKind::OpenWorkspacePicker);
        assert_eq!(OpenWorkspacePicker.def().name(), "workspaces");
        assert_eq!(ToggleProjectTree.kind(), ActionKind::ToggleProjectTree);
        assert_eq!(ToggleProjectTree.def().name(), "project-tree");
        assert_eq!(Pwd.kind(), ActionKind::Pwd);
        assert_eq!(Pwd.def().name(), "pwd");
        assert_eq!(Env.kind(), ActionKind::Env);
        assert_eq!(Env.def().name(), "env");
        let rename = RenameWorkspace {
            name: "x".to_string(),
        };
        assert_eq!(rename.kind(), ActionKind::RenameWorkspace);
        assert_eq!(rename.def().name(), "workspace-rename");
    }

    #[test]
    fn zero_arg_actions_have_no_params() {
        assert!(NewWorkspace.def().params().is_empty());
        assert!(CopyWorkspace.def().params().is_empty());
        assert!(SwitchWorkspace.def().params().is_empty());
        assert!(CloseWorkspace.def().params().is_empty());
        assert!(OpenWorkspacePicker.def().params().is_empty());
        assert!(Pwd.def().params().is_empty());
        assert!(Env.def().params().is_empty());
    }

    #[test]
    fn rename_workspace_takes_name_param() {
        let params = RenameWorkspaceDef.params();
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "name");
        assert_eq!(params[0].kind, ParamKind::String);
        assert!(!params[0].required);
    }

    #[test]
    fn set_cwd_takes_path_param() {
        let set = SetCwd {
            path: "/tmp".to_string(),
        };
        assert_eq!(set.kind(), ActionKind::SetCwd);
        assert_eq!(set.def().name(), "cd");
        let params = SetCwdDef.params();
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "path");
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
