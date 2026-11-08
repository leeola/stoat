use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    GitStatusDef,
    GitStatus,
    "GitStatus",
    ActionKind::GitStatus,
    "show the git status panel",
    "Open the git status panel at the window's top right. The panel names the \
     branch and how far it has moved from its upstream, or from main when it \
     has no upstream. A banner names a merge or a rebase in progress. Then the \
     changed files list in the sections conflicted, staged, unstaged, and \
     untracked. Outside a git repository the panel does not open.",
    ActionPriority::Common,
    aliases = &["git st"],
    command_name = "git status"
);

define_action!(
    CloseGitStatusDef,
    CloseGitStatus,
    "CloseGitStatus",
    ActionKind::CloseGitStatus,
    "close the git status panel",
    "Close the git status panel and return to the pane under it.",
    ActionPriority::Rare
);

define_action!(
    GitStatusNextDef,
    GitStatusNext,
    "GitStatusNext",
    ActionKind::GitStatusNext,
    "select the next git status file",
    "Move the git status panel's selection down one file. The selection walks \
     the files of every section in the order the panel lists them, and stops \
     at the last file.",
    ActionPriority::Rare
);

define_action!(
    GitStatusPrevDef,
    GitStatusPrev,
    "GitStatusPrev",
    ActionKind::GitStatusPrev,
    "select the previous git status file",
    "Move the git status panel's selection up one file. The selection walks \
     the files of every section in the order the panel lists them, and stops \
     at the first file.",
    ActionPriority::Rare
);
