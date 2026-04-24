use crate::{Action, ActionDef, ActionKind, ParamDef};
use std::any::Any;

#[derive(Debug)]
pub struct OpenFileFinderDef;

impl ActionDef for OpenFileFinderDef {
    fn name(&self) -> &'static str {
        "OpenFileFinder"
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
        assert_eq!(OpenFileFinder.def().name(), "OpenFileFinder");
        assert!(OpenFileFinder.def().params().is_empty());
        assert!(OpenFileFinder.def().palette_visible());

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
