use crate::{action::define_action, ActionKind};

define_action!(
    EnterRebaseDef,
    EnterRebase,
    "EnterRebase",
    ActionKind::EnterRebase,
    "enter interactive rebase over the commit list",
    "Seed a rebase todo list from the currently loaded commits (oldest \
     first, all marked Pick) and switch to rebase mode for editing."
);

define_action!(
    AbortRebaseDef,
    AbortRebase,
    "AbortRebase",
    ActionKind::AbortRebase,
    "discard the current rebase plan",
    "Drop the in-progress todo list without executing it and return to \
     commits mode. No commits are rewritten."
);

define_action!(
    ExecuteRebaseDef,
    ExecuteRebase,
    "ExecuteRebase",
    ActionKind::ExecuteRebase,
    "run the current rebase plan",
    "Apply the todo list to the repository: pick, squash/fixup, and \
     drop entries in order. On success the commit list is refreshed \
     and focus returns there. A conflict anywhere in the plan aborts \
     atomically with an error badge."
);

define_action!(
    RebaseNextDef,
    RebaseNext,
    "RebaseNext",
    ActionKind::RebaseNext,
    "select the next entry in the rebase plan",
    "Move the rebase-mode cursor down by one todo entry."
);

define_action!(
    RebasePrevDef,
    RebasePrev,
    "RebasePrev",
    ActionKind::RebasePrev,
    "select the previous entry in the rebase plan",
    "Move the rebase-mode cursor up by one todo entry."
);

define_action!(
    RebaseMoveUpDef,
    RebaseMoveUp,
    "RebaseMoveUp",
    ActionKind::RebaseMoveUp,
    "swap selected rebase entry with the one above",
    "Reorder the rebase todo list: move the selected entry one row up."
);

define_action!(
    RebaseMoveDownDef,
    RebaseMoveDown,
    "RebaseMoveDown",
    ActionKind::RebaseMoveDown,
    "swap selected rebase entry with the one below",
    "Reorder the rebase todo list: move the selected entry one row down."
);

define_action!(
    SetRebaseOpPickDef,
    SetRebaseOpPick,
    "SetRebaseOpPick",
    ActionKind::SetRebaseOpPick,
    "mark selected rebase entry as Pick",
    "Set the selected todo entry's operation to Pick (apply the commit \
     as-is on top of the running HEAD)."
);

define_action!(
    SetRebaseOpSquashDef,
    SetRebaseOpSquash,
    "SetRebaseOpSquash",
    ActionKind::SetRebaseOpSquash,
    "mark selected rebase entry as Squash",
    "Set the selected todo entry's operation to Squash: merge this \
     commit into the previous one in the plan, combining both messages."
);

define_action!(
    SetRebaseOpFixupDef,
    SetRebaseOpFixup,
    "SetRebaseOpFixup",
    ActionKind::SetRebaseOpFixup,
    "mark selected rebase entry as Fixup",
    "Set the selected todo entry's operation to Fixup: like Squash but \
     discard this commit's message."
);

define_action!(
    SetRebaseOpDropDef,
    SetRebaseOpDrop,
    "SetRebaseOpDrop",
    ActionKind::SetRebaseOpDrop,
    "mark selected rebase entry as Drop",
    "Set the selected todo entry's operation to Drop: skip this commit \
     entirely during execution."
);
