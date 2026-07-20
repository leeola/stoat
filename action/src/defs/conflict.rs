use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    ConflictDef,
    Conflict,
    "Conflict",
    ActionKind::Conflict,
    "resolve merge conflicts in a three-way view",
    "Open the current merge, rebase, or cherry-pick's conflicted files in a \
     three-column view of ours, an editable result, and theirs, built from the \
     on-disk index stages. Re-dispatch to close the view and return to the \
     plain file.",
    ActionPriority::Common
);

define_action!(
    CloseConflictDef,
    CloseConflict,
    "CloseConflict",
    ActionKind::CloseConflict,
    "close the conflict resolve view",
    "Close the three-way conflict view and return the focused pane to the plain \
     file. Any unwritten resolution in the center is discarded; the working file \
     and index stay untouched until an explicit apply."
);
