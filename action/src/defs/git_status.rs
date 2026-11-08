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

define_action!(
    GitStatusOpenDef,
    GitStatusOpen,
    "GitStatusOpen",
    ActionKind::GitStatusOpen,
    "open the selected git status file",
    "Close the git status panel and open its selected file. A conflicted file \
     opens in the conflict view, a staged or unstaged file opens in the \
     per-file diff view, and an untracked file opens as it is.",
    ActionPriority::Rare
);

define_action!(
    GitStatusStageDef,
    GitStatusStage,
    "GitStatusStage",
    ActionKind::GitStatusStage,
    "stage the selected git status file",
    "Stage the git status panel's selected file as the working tree holds it, \
     as git add does. An unstaged file moves to staged, an untracked file is \
     added, and a file gone from disk stages its deletion. A conflicted file \
     takes a side with o or t instead.",
    ActionPriority::Rare
);

define_action!(
    GitStatusUnstageDef,
    GitStatusUnstage,
    "GitStatusUnstage",
    ActionKind::GitStatusUnstage,
    "unstage the selected git status file",
    "Take the git status panel's selected staged file out of the index. Its \
     index entry returns to HEAD's, or leaves the index for a file HEAD lacks, \
     and the working tree keeps its content.",
    ActionPriority::Rare
);

define_action!(
    GitStatusTakeOursDef,
    GitStatusTakeOurs,
    "GitStatusTakeOurs",
    ActionKind::GitStatusTakeOurs,
    "resolve the selected conflict to our side",
    "Resolve the git status panel's selected conflicted file whole to our \
     side. The file takes our version and is staged, and its row notes that \
     it took ours.",
    ActionPriority::Rare
);

define_action!(
    GitStatusTakeTheirsDef,
    GitStatusTakeTheirs,
    "GitStatusTakeTheirs",
    ActionKind::GitStatusTakeTheirs,
    "resolve the selected conflict to their side",
    "Resolve the git status panel's selected conflicted file whole to their \
     side. The file takes their version and is staged, and its row notes that \
     it took theirs.",
    ActionPriority::Rare
);

define_action!(
    GitStatusCommitDef,
    GitStatusCommit,
    "GitStatusCommit",
    ActionKind::GitStatusCommit,
    "commit from the git status panel",
    "Close the git status panel and commit. During a merge it commits the \
     merge, during a rebase it continues the rebase, and otherwise it commits \
     the index, each through the commit message buffer where one applies.",
    ActionPriority::Rare
);

define_action!(
    GitStatusAbortDef,
    GitStatusAbort,
    "GitStatusAbort",
    ActionKind::GitStatusAbort,
    "abort the merge or the rebase",
    "Abort the merge or the rebase that the git status panel's banner names, \
     and read the panel again once the abort lands. With no operation in \
     progress, a badge says so and nothing changes.",
    ActionPriority::Rare
);

define_action!(
    GitStatusSkipDef,
    GitStatusSkip,
    "GitStatusSkip",
    ActionKind::GitStatusSkip,
    "skip the rebase entry stopped on a conflict",
    "Skip the rebase entry that stopped on a conflict, as the git status \
     panel's banner names it, and read the panel again. The rebase goes on \
     without that commit. With no rebase stopped on a conflict, a badge says \
     so and nothing changes.",
    ActionPriority::Rare
);
