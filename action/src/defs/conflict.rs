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

define_action!(
    ConflictPickOursDef,
    ConflictPickOurs,
    "ConflictPickOurs",
    ActionKind::ConflictPickOurs,
    "resolve the current chunk by taking ours",
    "Replace the conflict chunk under the cursor with its whole ours side. \
     Re-picking a chunk already hand-edited to text no pick produces first \
     warns, then overwrites on an immediate repeat.",
    ActionPriority::Rare
);

define_action!(
    ConflictPickTheirsDef,
    ConflictPickTheirs,
    "ConflictPickTheirs",
    ActionKind::ConflictPickTheirs,
    "resolve the current chunk by taking theirs",
    "Replace the conflict chunk under the cursor with its whole theirs side. \
     Re-picking a chunk already hand-edited to text no pick produces first \
     warns, then overwrites on an immediate repeat.",
    ActionPriority::Rare
);

define_action!(
    ConflictPickBothDef,
    ConflictPickBoth,
    "ConflictPickBoth",
    ActionKind::ConflictPickBoth,
    "resolve the current chunk by taking both sides",
    "Replace the conflict chunk under the cursor with both sides, ours before \
     theirs on each row. Re-picking a chunk already hand-edited to text no pick \
     produces first warns, then overwrites on an immediate repeat.",
    ActionPriority::Rare
);

define_action!(
    ConflictResetChunkDef,
    ConflictResetChunk,
    "ConflictResetChunk",
    ActionKind::ConflictResetChunk,
    "reset the current chunk to its conflict markers",
    "Restore the conflict chunk under the cursor to its raw marker block, \
     discarding any pick or hand edit in that region.",
    ActionPriority::Rare
);
