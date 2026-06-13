use crate::{Action, ActionDef, ActionKind, ActionPriority, ParamDef};
use std::any::Any;

#[derive(Debug)]
pub struct OpenFileFinderDef;

impl ActionDef for OpenFileFinderDef {
    fn name(&self) -> &'static str {
        "files"
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
        "files-hsplit"
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
        "files-vsplit"
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
        "changed-files"
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
pub struct OpenGitStatusDef;

impl ActionDef for OpenGitStatusDef {
    fn name(&self) -> &'static str {
        "git-status"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenGitStatus
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "open git status picker"
    }

    fn long_desc(&self) -> &'static str {
        "Open a workspace-level picker listing tracked, untracked, and modified \
         files in the active git repository. Each entry shows the file's git \
         status; selecting one opens that file in the focused pane."
    }

    fn palette_visible(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub struct OpenGitStatus;

impl OpenGitStatus {
    pub const DEF: &OpenGitStatusDef = &OpenGitStatusDef;
}

impl Action for OpenGitStatus {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct OpenConflictPickerDef;

impl ActionDef for OpenConflictPickerDef {
    fn name(&self) -> &'static str {
        "conflicts"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenConflictPicker
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "open conflict picker"
    }

    fn long_desc(&self) -> &'static str {
        "Open a workspace-level picker listing every file in the active git \
         repository with unresolved merge conflicts. Selecting one opens that \
         file in the focused pane, where its conflict regions are highlighted."
    }

    fn palette_visible(&self) -> bool {
        true
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug)]
pub struct OpenConflictPicker;

impl OpenConflictPicker {
    pub const DEF: &OpenConflictPickerDef = &OpenConflictPickerDef;
}

impl Action for OpenConflictPicker {
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
        "buffers"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_names() {
        assert_eq!(OpenFileFinder.kind(), ActionKind::OpenFileFinder);
        assert_eq!(OpenFileFinder.def().name(), "files");
        assert!(OpenFileFinder.def().params().is_empty());
        assert!(OpenFileFinder.def().palette_visible());

        assert_eq!(
            OpenFileFinderHSplit.kind(),
            ActionKind::OpenFileFinderHSplit
        );
        assert_eq!(OpenFileFinderHSplit.def().name(), "files-hsplit");
        assert!(OpenFileFinderHSplit.def().palette_visible());

        assert_eq!(
            OpenFileFinderVSplit.kind(),
            ActionKind::OpenFileFinderVSplit
        );
        assert_eq!(OpenFileFinderVSplit.def().name(), "files-vsplit");
        assert!(OpenFileFinderVSplit.def().palette_visible());

        assert_eq!(
            OpenChangedFilePicker.kind(),
            ActionKind::OpenChangedFilePicker
        );
        assert_eq!(OpenChangedFilePicker.def().name(), "changed-files");
        assert!(OpenChangedFilePicker.def().palette_visible());

        assert_eq!(OpenBufferPicker.kind(), ActionKind::OpenBufferPicker);
        assert_eq!(OpenBufferPicker.def().name(), "buffers");
        assert!(OpenBufferPicker.def().palette_visible());

        assert_eq!(
            FileFinderSelectPrev.kind(),
            ActionKind::FileFinderSelectPrev
        );
        assert_eq!(
            FileFinderSelectNext.kind(),
            ActionKind::FileFinderSelectNext
        );
        assert_eq!(
            FileFinderScopeToggle.kind(),
            ActionKind::FileFinderScopeToggle
        );

        for def in [
            FileFinderSelectPrev.def(),
            FileFinderSelectNext.def(),
            FileFinderScopeToggle.def(),
        ] {
            assert!(!def.palette_visible());
        }
    }
}
