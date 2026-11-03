use crate::{
    action::{define_action, define_action_def},
    Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind, ValueSource,
};
use std::any::Any;

define_action!(
    OpenCommitsDef,
    OpenCommits,
    "OpenCommits",
    ActionKind::OpenCommits,
    "browse commit history",
    "Open the commit-list view: a left pane of commits on the current \
     branch with a right-pane preview of the selected commit's changes. \
     The list takes the full editor width while it is open. A second run \
     closes it.",
    ActionPriority::Common,
    command_name = "commits"
);

define_action!(
    CloseCommitsDef,
    CloseCommits,
    "CloseCommits",
    ActionKind::CloseCommits,
    "close the commit-list view",
    "Drop the active commit-list state, restore the pane layout the list \
     widened, and return to normal mode."
);

define_action!(
    CommitsNextDef,
    CommitsNext,
    "CommitsNext",
    ActionKind::CommitsNext,
    "select the next commit",
    "Move the selection cursor one row down in the commit list. \
     Triggers a lazy page-load when the cursor approaches the tail of \
     the currently loaded window.",
    ActionPriority::Rare
);

define_action!(
    CommitsPrevDef,
    CommitsPrev,
    "CommitsPrev",
    ActionKind::CommitsPrev,
    "select the previous commit",
    "Move the selection cursor one row up in the commit list.",
    ActionPriority::Rare
);

define_action!(
    CommitsPageDownDef,
    CommitsPageDown,
    "CommitsPageDown",
    ActionKind::CommitsPageDown,
    "scroll the commit list down a page",
    "Advance the commit-list selection by one viewport height.",
    ActionPriority::Rare
);

define_action!(
    CommitsPageUpDef,
    CommitsPageUp,
    "CommitsPageUp",
    ActionKind::CommitsPageUp,
    "scroll the commit list up a page",
    "Retreat the commit-list selection by one viewport height.",
    ActionPriority::Rare
);

define_action!(
    CommitsDetailDownDef,
    CommitsDetailDown,
    "CommitsDetailDown",
    ActionKind::CommitsDetailDown,
    "scroll the commit diff down half a pane",
    "Advance the commits screen's diff preview by half the detail pane's height.",
    ActionPriority::Rare
);

define_action!(
    CommitsDetailUpDef,
    CommitsDetailUp,
    "CommitsDetailUp",
    ActionKind::CommitsDetailUp,
    "scroll the commit diff up half a pane",
    "Retreat the commits screen's diff preview by half the detail pane's height.",
    ActionPriority::Rare
);

define_action!(
    CommitsFirstDef,
    CommitsFirst,
    "CommitsFirst",
    ActionKind::CommitsFirst,
    "jump to HEAD in the commit list",
    "Move the selection to the newest commit (top of the list).",
    ActionPriority::Rare
);

define_action!(
    CommitsLastDef,
    CommitsLast,
    "CommitsLast",
    ActionKind::CommitsLast,
    "jump to the oldest loaded commit",
    "Move the selection to the oldest commit currently in the loaded \
     window. Does not force a full walk of history.",
    ActionPriority::Rare
);

define_action!(
    CommitsRefreshDef,
    CommitsRefresh,
    "CommitsRefresh",
    ActionKind::CommitsRefresh,
    "rescan the commit list",
    "Discard the loaded commits and preview caches and reload the \
     first page from HEAD. Use after external branch changes.",
    ActionPriority::Rare
);

const GIT_LS_PARAMS: &[ParamDef] = &[ParamDef {
    name: "rev",
    kind: ParamKind::String,
    value_source: ValueSource::Branches,
    required: false,
    description: "Branch, tag, sha, or revspec to list. Defaults to HEAD.",
}];

define_action_def!(
    GitLsDef,
    "GitLs",
    ActionKind::GitLs,
    "browse commit history",
    "Open a read-only picker over a ref's first-parent history, with a \
     fuzzy filter and a diff preview of the highlighted commit. Lists the \
     current branch when given no revision. Selecting a row dismisses the \
     picker without changing the working tree.",
    ActionPriority::Rare,
    aliases = &["git ls"],
    command_name = "git log",
    params = GIT_LS_PARAMS
);

#[derive(Debug)]
pub struct GitLs {
    /// Revision to list, or `None` to list HEAD.
    pub rev: Option<String>,
}

impl GitLs {
    pub const DEF: &GitLsDef = &GitLsDef;
}

impl Action for GitLs {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

const GIT_CHECKOUT_PARAMS: &[ParamDef] = &[ParamDef {
    name: "reference",
    kind: ParamKind::String,
    value_source: ValueSource::Branches,
    required: true,
    description: "Local branch to attach HEAD to, or a commit to detach at.",
}];

define_action_def!(
    GitCheckoutDef,
    "GitCheckout",
    ActionKind::GitCheckout,
    "check out a branch or commit",
    "Check out a local branch and attach HEAD to it, or check out any other \
     revision and detach HEAD at its commit. The open buffers, the diffs, \
     and the commits lists follow the new tree. Refuses while a review walk \
     or a rebase holds HEAD, and when the checkout has to overwrite a \
     tracked file's local edit.",
    ActionPriority::Common,
    aliases = &["git co"],
    command_name = "git checkout",
    params = GIT_CHECKOUT_PARAMS
);

#[derive(Debug)]
pub struct GitCheckout {
    /// Local branch to attach HEAD to, or any other revision to detach at.
    pub reference: String,
}

impl GitCheckout {
    pub const DEF: &GitCheckoutDef = &GitCheckoutDef;
}

impl Action for GitCheckout {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

define_action!(
    GitBranchDef,
    GitBranch,
    "GitBranch",
    ActionKind::GitBranch,
    "list the local branches",
    "Open the command palette holding `git checkout `, so the local branches \
     list with the branch HEAD is on first. Enter checks the chosen branch \
     out, and Escape changes nothing.",
    ActionPriority::Common,
    aliases = &["git br"],
    command_name = "git branch"
);

define_action!(
    GitCommitDef,
    GitCommit,
    "GitCommit",
    ActionKind::GitCommit,
    "commit the staged changes",
    "Write git's message template to the repository's COMMIT_EDITMSG, open \
     it as a buffer in a new split, and hold it. The commit lands with the \
     cleaned message when the written buffer closes, as with `:wq`. A write \
     alone commits nothing, and a close without a write or with only comment \
     lines commits nothing. Refuses while a review walk or a rebase holds \
     HEAD, and when nothing is staged.",
    ActionPriority::Common,
    aliases = &["git ci"],
    command_name = "git commit"
);

define_action!(
    GitAmendDef,
    GitAmend,
    "GitAmend",
    ActionKind::GitAmend,
    "amend HEAD with the staged changes",
    "Write HEAD's message above git's template to the repository's \
     COMMIT_EDITMSG, open it as a buffer in a new split, and hold it. When \
     the buffer closes with no unsaved edits, HEAD is replaced with a commit \
     of the index and the cleaned message, as `git commit --amend` does. A \
     write alone amends nothing, and a message of only comment lines amends \
     nothing. With nothing staged, the amend changes the message alone. \
     Refuses while a review walk or a rebase holds HEAD.",
    ActionPriority::Common,
    aliases = &["git cia"],
    command_name = "git amend"
);

define_action!(
    GitAddDef,
    GitAdd,
    "GitAdd",
    ActionKind::GitAdd,
    "stage every tracked change",
    "Stage every change to a tracked file, as `git add -u` does, so edits and \
     deletions reach the index. Untracked files stay out of it. Refuses while \
     a review walk or a rebase holds HEAD.",
    ActionPriority::Common,
    aliases = &["git au"],
    command_name = "git add"
);

define_action!(
    GitStashDef,
    GitStash,
    "GitStash",
    ActionKind::GitStash,
    "stash the working-tree changes",
    "Stash every change to a tracked file, staged or not, under git's default \
     message, as `git stash` does. Untracked files stay. Refuses while a \
     review walk or a rebase holds HEAD, and badges when there is nothing to \
     stash.",
    ActionPriority::Common,
    command_name = "git stash"
);

const GIT_STASH_PUSH_PARAMS: &[ParamDef] = &[ParamDef {
    name: "message",
    kind: ParamKind::String,
    value_source: ValueSource::None,
    required: false,
    description: "Message for the stash. Omit it for git's default message.",
}];

define_action_def!(
    GitStashPushDef,
    "GitStashPush",
    ActionKind::GitStashPush,
    "stash with a message",
    "Stash every change to a tracked file, staged or not, under the given \
     message, as `git stash push -m` does. Untracked files stay. Refuses while \
     a review walk or a rebase holds HEAD, and badges when there is nothing \
     to stash.",
    ActionPriority::Common,
    command_name = "git stash push",
    params = GIT_STASH_PUSH_PARAMS
);

#[derive(Debug)]
pub struct GitStashPush {
    /// The stash message, or `None` for git's default message.
    pub message: Option<String>,
}

impl GitStashPush {
    pub const DEF: &GitStashPushDef = &GitStashPushDef;
}

impl Action for GitStashPush {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

define_action!(
    GitStashPopDef,
    GitStashPop,
    "GitStashPop",
    ActionKind::GitStashPop,
    "apply and drop the newest stash",
    "Apply the newest stash to the working tree and drop it, as `git stash \
     pop` does. A pop that conflicts keeps the stash and badges, and with no \
     stash it badges and changes nothing. Refuses while a review walk or a \
     rebase holds HEAD.",
    ActionPriority::Common,
    aliases = &["git spo"],
    command_name = "git stash pop"
);

define_action!(
    GitStashListDef,
    GitStashList,
    "GitStashList",
    ActionKind::GitStashList,
    "list the stashes",
    "Open a picker over the stashes, newest first, with a diff preview of \
     the highlighted stash. Enter pops the highlighted stash, and Escape \
     closes the picker with every stash kept. The pop refuses while a review \
     walk or a rebase holds HEAD.",
    ActionPriority::Common,
    aliases = &["git sls"],
    command_name = "git stash list"
);

define_action!(
    CommitsOpenReviewDef,
    CommitsOpenReview,
    "CommitsOpenReview",
    ActionKind::CommitsOpenReview,
    "check the selected commit out and diff it",
    "Check the selected commit out and open a diff of it against its \
     parent, so the files on disk are the commit and the diff on screen \
     is what it changed. A working tree with uncommitted changes refuses, \
     since the checkout would have to overwrite it. Run `:done` to put \
     HEAD back where it was.",
    ActionPriority::Rare
);
