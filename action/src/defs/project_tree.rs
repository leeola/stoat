use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    ProjectTreeSelectNextDef,
    ProjectTreeSelectNext,
    "ProjectTreeSelectNext",
    ActionKind::ProjectTreeSelectNext,
    "select the next tree row",
    "Move the project tree selection one visible row down.",
    ActionPriority::Rare
);

define_action!(
    ProjectTreeSelectPrevDef,
    ProjectTreeSelectPrev,
    "ProjectTreeSelectPrev",
    ActionKind::ProjectTreeSelectPrev,
    "select the previous tree row",
    "Move the project tree selection one visible row up.",
    ActionPriority::Rare
);

define_action!(
    ProjectTreeCollapseDef,
    ProjectTreeCollapse,
    "ProjectTreeCollapse",
    ActionKind::ProjectTreeCollapse,
    "collapse the selected directory",
    "Collapse the selected directory if it is expanded. No-op when the \
     selected row is a file or an already-collapsed directory.",
    ActionPriority::Rare
);

define_action!(
    ProjectTreeExpandDef,
    ProjectTreeExpand,
    "ProjectTreeExpand",
    ActionKind::ProjectTreeExpand,
    "expand the selected directory",
    "Expand the selected directory if it is collapsed, listing its \
     contents inline. No-op when the selected row is a file or an \
     already-expanded directory.",
    ActionPriority::Rare
);

define_action!(
    ProjectTreeConfirmDef,
    ProjectTreeConfirm,
    "ProjectTreeConfirm",
    ActionKind::ProjectTreeConfirm,
    "open the selected tree row",
    "Toggle expansion when the selected row is a directory, or open the \
     file in the focused pane when it is a file.",
    ActionPriority::Rare
);

define_action!(
    ProjectTreeRefreshDef,
    ProjectTreeRefresh,
    "ProjectTreeRefresh",
    ActionKind::ProjectTreeRefresh,
    "rescan the project tree",
    "Re-read the workspace directory contents from disk, preserving the \
     set of expanded directories.",
    ActionPriority::Rare
);

define_action!(
    DeleteTreeEntryDef,
    DeleteTreeEntry,
    "DeleteTreeEntry",
    ActionKind::DeleteTreeEntry,
    "delete the selected tree entry",
    "Delete the selected project-tree file or directory from disk after \
     confirmation. Directories are removed recursively.",
    ActionPriority::Rare
);

define_action!(
    RenameTreeEntryDef,
    RenameTreeEntry,
    "RenameTreeEntry",
    ActionKind::RenameTreeEntry,
    "rename the selected tree entry",
    "Edit the selected project-tree entry's name inline, pre-filled with \
     its current name. Enter renames the file or directory on disk; \
     Escape cancels.",
    ActionPriority::Rare
);

define_action!(
    NewFileInTreeDef,
    NewFileInTree,
    "NewFileInTree",
    ActionKind::NewFileInTree,
    "create a new file in the tree",
    "Open an inline input for a new file in the selected directory, or in \
     the selected entry's parent. Enter creates an empty file on disk; \
     Escape cancels.",
    ActionPriority::Rare
);

define_action!(
    NewFolderInTreeDef,
    NewFolderInTree,
    "NewFolderInTree",
    ActionKind::NewFolderInTree,
    "create a new folder in the tree",
    "Open an inline input for a new directory in the selected directory, or \
     in the selected entry's parent. Enter creates the directory on disk; \
     Escape cancels.",
    ActionPriority::Rare
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;

    #[test]
    fn kinds_and_names_match() {
        assert_eq!(
            ProjectTreeSelectNext.kind(),
            ActionKind::ProjectTreeSelectNext
        );
        assert_eq!(ProjectTreeSelectNext.def().name(), "ProjectTreeSelectNext");
        assert_eq!(ProjectTreeCollapse.kind(), ActionKind::ProjectTreeCollapse);
        assert_eq!(ProjectTreeConfirm.def().name(), "ProjectTreeConfirm");
        assert_eq!(ProjectTreeRefresh.kind(), ActionKind::ProjectTreeRefresh);
    }
}
