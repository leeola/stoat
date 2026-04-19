use crate::{action::define_action, ActionKind};

define_action!(
    OpenCommitsDef,
    OpenCommits,
    "OpenCommits",
    ActionKind::OpenCommits,
    "browse commit history",
    "Open the commit-list view: a left pane of commits on the current \
     branch with a right-pane preview of the selected commit's changes."
);

define_action!(
    CloseCommitsDef,
    CloseCommits,
    "CloseCommits",
    ActionKind::CloseCommits,
    "close the commit-list view",
    "Drop the active commit-list state and return to normal mode."
);

define_action!(
    CommitsNextDef,
    CommitsNext,
    "CommitsNext",
    ActionKind::CommitsNext,
    "select the next commit",
    "Move the selection cursor one row down in the commit list. \
     Triggers a lazy page-load when the cursor approaches the tail of \
     the currently loaded window."
);

define_action!(
    CommitsPrevDef,
    CommitsPrev,
    "CommitsPrev",
    ActionKind::CommitsPrev,
    "select the previous commit",
    "Move the selection cursor one row up in the commit list."
);

define_action!(
    CommitsPageDownDef,
    CommitsPageDown,
    "CommitsPageDown",
    ActionKind::CommitsPageDown,
    "scroll the commit list down a page",
    "Advance the commit-list selection by one viewport height."
);

define_action!(
    CommitsPageUpDef,
    CommitsPageUp,
    "CommitsPageUp",
    ActionKind::CommitsPageUp,
    "scroll the commit list up a page",
    "Retreat the commit-list selection by one viewport height."
);

define_action!(
    CommitsFirstDef,
    CommitsFirst,
    "CommitsFirst",
    ActionKind::CommitsFirst,
    "jump to HEAD in the commit list",
    "Move the selection to the newest commit (top of the list)."
);

define_action!(
    CommitsLastDef,
    CommitsLast,
    "CommitsLast",
    ActionKind::CommitsLast,
    "jump to the oldest loaded commit",
    "Move the selection to the oldest commit currently in the loaded \
     window. Does not force a full walk of history."
);

define_action!(
    CommitsRefreshDef,
    CommitsRefresh,
    "CommitsRefresh",
    ActionKind::CommitsRefresh,
    "rescan the commit list",
    "Discard the loaded commits and preview caches and reload the \
     first page from HEAD. Use after external branch changes."
);

define_action!(
    CommitsOpenReviewDef,
    CommitsOpenReview,
    "CommitsOpenReview",
    ActionKind::CommitsOpenReview,
    "open the selected commit in review",
    "Open a review session over the currently selected commit. The \
     session is read-only (ReviewApplyStaged is a no-op for commit \
     sources); use the separate `ReviewRemoveSelected` action to \
     actually remove staged hunks from the commit. Closing the review \
     returns to commits mode."
);
