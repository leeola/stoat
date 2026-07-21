use crate::{Action, ActionDef, ActionKind, ParamDef};
use std::any::Any;

#[derive(Debug)]
pub struct OpenFileFinderDef;

impl ActionDef for OpenFileFinderDef {
    fn name(&self) -> &'static str {
        "OpenFileFinder"
    }

    fn command_name(&self) -> Option<&'static str> {
        Some("files")
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenFileFinder
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "open the file finder"
    }

    fn long_desc(&self) -> &'static str {
        "Open the file finder modal. Type to fuzzy-filter files in the current \
         workspace; Enter opens the selected file in the focused pane; Shift-Tab \
         switches between all tracked files and git-modified files."
    }

    fn palette_visible(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub struct OpenFileFinder;

impl OpenFileFinder {
    pub const DEF: &OpenFileFinderDef = &OpenFileFinderDef;
}

impl Action for OpenFileFinder {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct OpenFileFinderHSplitDef;

impl ActionDef for OpenFileFinderHSplitDef {
    fn name(&self) -> &'static str {
        "OpenFileFinderHSplit"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenFileFinderHSplit
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "open file finder, split horizontally on select"
    }

    fn long_desc(&self) -> &'static str {
        "Open the file finder modal. When the user submits a file, split the \
         focused pane horizontally and open the selected file in the new pane \
         below."
    }

    fn palette_visible(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub struct OpenFileFinderHSplit;

impl OpenFileFinderHSplit {
    pub const DEF: &OpenFileFinderHSplitDef = &OpenFileFinderHSplitDef;
}

impl Action for OpenFileFinderHSplit {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct OpenFileFinderVSplitDef;

impl ActionDef for OpenFileFinderVSplitDef {
    fn name(&self) -> &'static str {
        "OpenFileFinderVSplit"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenFileFinderVSplit
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "open file finder, split vertically on select"
    }

    fn long_desc(&self) -> &'static str {
        "Open the file finder modal. When the user submits a file, split the \
         focused pane vertically and open the selected file in the new pane to \
         the right."
    }

    fn palette_visible(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub struct OpenFileFinderVSplit;

impl OpenFileFinderVSplit {
    pub const DEF: &OpenFileFinderVSplitDef = &OpenFileFinderVSplitDef;
}

impl Action for OpenFileFinderVSplit {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct OpenChangedFilePickerDef;

impl ActionDef for OpenChangedFilePickerDef {
    fn name(&self) -> &'static str {
        "OpenChangedFilePicker"
    }

    fn command_name(&self) -> Option<&'static str> {
        Some("changed-files")
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenChangedFilePicker
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "open the changed-file picker"
    }

    fn long_desc(&self) -> &'static str {
        "Open the file finder modal pre-filtered to files with uncommitted git \
         changes. Shift-Tab flips back to the All scope (every tracked file in \
         the workspace)."
    }

    fn palette_visible(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub struct OpenChangedFilePicker;

impl OpenChangedFilePicker {
    pub const DEF: &OpenChangedFilePickerDef = &OpenChangedFilePickerDef;
}

impl Action for OpenChangedFilePicker {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct OpenBufferPickerDef;

impl ActionDef for OpenBufferPickerDef {
    fn name(&self) -> &'static str {
        "OpenBufferPicker"
    }

    fn command_name(&self) -> Option<&'static str> {
        Some("buffers")
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenBufferPicker
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "open the buffer picker"
    }

    fn long_desc(&self) -> &'static str {
        "Open the file finder modal scoped to currently-open buffers. \
         Selecting a row switches the focused pane to that buffer. \
         Shift-Tab flips to the All scope (every tracked file in the workspace)."
    }

    fn palette_visible(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub struct OpenBufferPicker;

impl OpenBufferPicker {
    pub const DEF: &OpenBufferPickerDef = &OpenBufferPickerDef;
}

impl Action for OpenBufferPicker {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct OpenWorkspaceFileFinderDef;

impl ActionDef for OpenWorkspaceFileFinderDef {
    fn name(&self) -> &'static str {
        "OpenWorkspaceFileFinder"
    }

    fn command_name(&self) -> Option<&'static str> {
        Some("workspace-files")
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenWorkspaceFileFinder
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "open the cross-workspace file finder"
    }

    fn long_desc(&self) -> &'static str {
        "Open the file finder modal scoped across every known workspace -- the \
         open workspaces plus the on-disk registry. Rows show each file under \
         its owning workspace, and selecting one opens it in the current \
         workspace. Shift-Tab flips to the All scope."
    }

    fn palette_visible(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub struct OpenWorkspaceFileFinder;

impl OpenWorkspaceFileFinder {
    pub const DEF: &OpenWorkspaceFileFinderDef = &OpenWorkspaceFileFinderDef;
}

impl Action for OpenWorkspaceFileFinder {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct FileFinderSelectPrevDef;

impl ActionDef for FileFinderSelectPrevDef {
    fn name(&self) -> &'static str {
        "FileFinderSelectPrev"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::FileFinderSelectPrev
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "select previous file"
    }

    fn long_desc(&self) -> &'static str {
        "Move the file finder selection up by one row. Bound by default to Up \
         and Ctrl-P while the file finder is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct FileFinderSelectPrev;

impl FileFinderSelectPrev {
    pub const DEF: &FileFinderSelectPrevDef = &FileFinderSelectPrevDef;
}

impl Action for FileFinderSelectPrev {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct FileFinderSelectNextDef;

impl ActionDef for FileFinderSelectNextDef {
    fn name(&self) -> &'static str {
        "FileFinderSelectNext"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::FileFinderSelectNext
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "select next file"
    }

    fn long_desc(&self) -> &'static str {
        "Move the file finder selection down by one row. Bound by default to \
         Down and Ctrl-N while the file finder is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct FileFinderSelectNext;

impl FileFinderSelectNext {
    pub const DEF: &FileFinderSelectNextDef = &FileFinderSelectNextDef;
}

impl Action for FileFinderSelectNext {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct FileFinderScopeToggleDef;

impl ActionDef for FileFinderScopeToggleDef {
    fn name(&self) -> &'static str {
        "FileFinderScopeToggle"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::FileFinderScopeToggle
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "toggle file finder scope"
    }

    fn long_desc(&self) -> &'static str {
        "Flip the file finder between All scope (every tracked file in the \
         workspace) and Modified scope (files with uncommitted git changes). \
         Bound by default to Shift-Tab while the finder is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct FileFinderScopeToggle;

impl FileFinderScopeToggle {
    pub const DEF: &FileFinderScopeToggleDef = &FileFinderScopeToggleDef;
}

impl Action for FileFinderScopeToggle {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct FileFinderCompleteDef;

impl ActionDef for FileFinderCompleteDef {
    fn name(&self) -> &'static str {
        "FileFinderComplete"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::FileFinderComplete
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "complete selected file"
    }

    fn long_desc(&self) -> &'static str {
        "Complete the highlighted row into the file finder query, replacing \
         what was typed with the row's path. Browsing a directory completes \
         the typed prefix plus the row's name, so a following Enter opens that \
         entry. Bound by default to Tab while the finder is open; a no-op when \
         the list is empty."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct FileFinderComplete;

impl FileFinderComplete {
    pub const DEF: &FileFinderCompleteDef = &FileFinderCompleteDef;
}

impl Action for FileFinderComplete {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct FileFinderPageUpDef;

impl ActionDef for FileFinderPageUpDef {
    fn name(&self) -> &'static str {
        "FileFinderPageUp"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::FileFinderPageUp
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "page file finder up"
    }

    fn long_desc(&self) -> &'static str {
        "Move the file finder selection up by half the visible list height. \
         Bound by default to Ctrl-B while the file finder is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct FileFinderPageUp;

impl FileFinderPageUp {
    pub const DEF: &FileFinderPageUpDef = &FileFinderPageUpDef;
}

impl Action for FileFinderPageUp {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct FileFinderPageDownDef;

impl ActionDef for FileFinderPageDownDef {
    fn name(&self) -> &'static str {
        "FileFinderPageDown"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::FileFinderPageDown
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "page file finder down"
    }

    fn long_desc(&self) -> &'static str {
        "Move the file finder selection down by half the visible list height. \
         Bound by default to Ctrl-F while the file finder is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct FileFinderPageDown;

impl FileFinderPageDown {
    pub const DEF: &FileFinderPageDownDef = &FileFinderPageDownDef;
}

impl Action for FileFinderPageDown {
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
            FileFinderSelectPrev.kind(),
            ActionKind::FileFinderSelectPrev
        );
        assert_eq!(
            FileFinderSelectNext.kind(),
            ActionKind::FileFinderSelectNext
        );
        assert_eq!(FileFinderPageUp.kind(), ActionKind::FileFinderPageUp);
        assert_eq!(FileFinderPageDown.kind(), ActionKind::FileFinderPageDown);
        assert_eq!(
            FileFinderScopeToggle.kind(),
            ActionKind::FileFinderScopeToggle
        );
        assert_eq!(FileFinderComplete.kind(), ActionKind::FileFinderComplete);
        assert_eq!(FileFinderComplete.def().name(), "FileFinderComplete");
        assert!(FileFinderComplete.def().params().is_empty());

        for def in [
            FileFinderSelectPrev.def(),
            FileFinderSelectNext.def(),
            FileFinderPageUp.def(),
            FileFinderPageDown.def(),
            FileFinderScopeToggle.def(),
            FileFinderComplete.def(),
        ] {
            assert!(!def.palette_visible());
        }
    }
}
