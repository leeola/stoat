use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    OpenFileFinderDef,
    OpenFileFinder,
    "OpenFileFinder",
    ActionKind::OpenFileFinder,
    "open the file finder",
    "Open the file finder modal. Type to fuzzy-filter files in the current \
     workspace; Enter opens the selected file in the focused pane; Shift-Tab \
     switches between all tracked files and git-modified files.",
    ActionPriority::Normal,
    command_name = "files"
);

define_action!(
    OpenFileFinderHSplitDef,
    OpenFileFinderHSplit,
    "OpenFileFinderHSplit",
    ActionKind::OpenFileFinderHSplit,
    "open file finder, split horizontally on select",
    "Open the file finder modal. When the user submits a file, split the \
     focused pane horizontally and open the selected file in the new pane \
     below."
);

define_action!(
    OpenFileFinderVSplitDef,
    OpenFileFinderVSplit,
    "OpenFileFinderVSplit",
    ActionKind::OpenFileFinderVSplit,
    "open file finder, split vertically on select",
    "Open the file finder modal. When the user submits a file, split the \
     focused pane vertically and open the selected file in the new pane to \
     the right."
);

define_action!(
    OpenChangedFilePickerDef,
    OpenChangedFilePicker,
    "OpenChangedFilePicker",
    ActionKind::OpenChangedFilePicker,
    "open the changed-file picker",
    "Open the file finder modal pre-filtered to files with uncommitted git \
     changes. Shift-Tab flips back to the All scope (every tracked file in \
     the workspace).",
    ActionPriority::Normal,
    command_name = "changed-files"
);

define_action!(
    OpenBufferPickerDef,
    OpenBufferPicker,
    "OpenBufferPicker",
    ActionKind::OpenBufferPicker,
    "open the buffer picker",
    "Open the file finder modal scoped to currently-open buffers. \
     Selecting a row switches the focused pane to that buffer. \
     Shift-Tab flips to the All scope (every tracked file in the workspace).",
    ActionPriority::Normal,
    command_name = "buffers"
);

define_action!(
    OpenWorkspaceFileFinderDef,
    OpenWorkspaceFileFinder,
    "OpenWorkspaceFileFinder",
    ActionKind::OpenWorkspaceFileFinder,
    "open the cross-workspace file finder",
    "Open the file finder modal scoped across every known workspace -- the \
     open workspaces plus the on-disk registry. Rows show each file under \
     its owning workspace, and selecting one opens it in the current \
     workspace. Shift-Tab flips to the All scope.",
    ActionPriority::Normal,
    command_name = "workspace-files"
);

define_action!(
    FileFinderScopeToggleDef,
    FileFinderScopeToggle,
    "FileFinderScopeToggle",
    ActionKind::FileFinderScopeToggle,
    "toggle file finder scope",
    "Flip the file finder between All scope (every tracked file in the \
     workspace) and Modified scope (files with uncommitted git changes). \
     Bound by default to Shift-Tab while the finder is open.",
    ActionPriority::Normal,
    palette_visible = false
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn kinds_and_names() {
        assert_eq!(OpenFileFinder.kind(), ActionKind::OpenFileFinder);
        assert_eq!(OpenFileFinder.def().name(), "OpenFileFinder");
        assert!(OpenFileFinder.def().params().is_empty());
        assert!(OpenFileFinder.def().palette_visible());

        assert_eq!(
            OpenFileFinderHSplit.kind(),
            ActionKind::OpenFileFinderHSplit
        );
        assert_eq!(OpenFileFinderHSplit.def().name(), "OpenFileFinderHSplit");
        assert!(OpenFileFinderHSplit.def().palette_visible());

        assert_eq!(
            OpenFileFinderVSplit.kind(),
            ActionKind::OpenFileFinderVSplit
        );
        assert_eq!(OpenFileFinderVSplit.def().name(), "OpenFileFinderVSplit");
        assert!(OpenFileFinderVSplit.def().palette_visible());

        assert_eq!(
            OpenChangedFilePicker.kind(),
            ActionKind::OpenChangedFilePicker
        );
        assert_eq!(OpenChangedFilePicker.def().name(), "OpenChangedFilePicker");
        assert!(OpenChangedFilePicker.def().palette_visible());

        assert_eq!(OpenBufferPicker.kind(), ActionKind::OpenBufferPicker);
        assert_eq!(OpenBufferPicker.def().name(), "OpenBufferPicker");
        assert!(OpenBufferPicker.def().palette_visible());

        assert_eq!(
            OpenWorkspaceFileFinder.kind(),
            ActionKind::OpenWorkspaceFileFinder
        );
        assert_eq!(
            OpenWorkspaceFileFinder.def().name(),
            "OpenWorkspaceFileFinder"
        );
        assert!(OpenWorkspaceFileFinder.def().palette_visible());

        assert_eq!(
            FileFinderScopeToggle.kind(),
            ActionKind::FileFinderScopeToggle
        );
        assert_eq!(FileFinderScopeToggle.def().name(), "FileFinderScopeToggle");
        assert!(FileFinderScopeToggle.def().params().is_empty());
        assert!(!FileFinderScopeToggle.def().palette_visible());
    }
}
