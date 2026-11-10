use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    DiffChangesDef,
    DiffChanges,
    "DiffChanges",
    ActionKind::DiffChanges,
    "show every change as one hunk list",
    "Open a changes view of every working-tree change against the diff base. \
     The changes view shows the hunks of each changed file side by side, in one \
     list that scrolls. From the per-file diff it opens on the hunk under the \
     cursor. Run on the changes view, it closes the view and lands on the \
     cursor's file at the cursor, with the selections kept, so one key opens \
     and closes the changes view. Ctrl-t and DiffFiles open the per-file diff \
     at the cursor's hunk from it.",
    ActionPriority::Common,
    command_name = "diff-changes"
);

define_action!(
    CloseDiffChangesDef,
    CloseDiffChanges,
    "CloseDiffChanges",
    ActionKind::CloseDiffChanges,
    "close the changes view",
    "Close the changes view and land on the cursor's file at the cursor, with the \
     selections kept. A changes view opened from the per-file diff returns to the \
     diff of that file.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesNextDef,
    DiffChangesNext,
    "DiffChangesNext",
    ActionKind::DiffChangesNext,
    "move to the next changes view row",
    "Move the changes view cursor down one row, or down by the count typed \
     before the key. A step past the last row of a file goes on to the first \
     row of the next file.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesPrevDef,
    DiffChangesPrev,
    "DiffChangesPrev",
    ActionKind::DiffChangesPrev,
    "move to the previous changes view row",
    "Move the changes view cursor up one row, or up by the count typed before \
     the key. A step past the first row of a file goes on to the last row of \
     the previous file.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesPageDownDef,
    DiffChangesPageDown,
    "DiffChangesPageDown",
    ActionKind::DiffChangesPageDown,
    "scroll the changes view down a page",
    "Move the changes view cursor down by the height of the changes view.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesPageUpDef,
    DiffChangesPageUp,
    "DiffChangesPageUp",
    ActionKind::DiffChangesPageUp,
    "scroll the changes view up a page",
    "Move the changes view cursor up by the height of the changes view.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesHalfPageDownDef,
    DiffChangesHalfPageDown,
    "DiffChangesHalfPageDown",
    ActionKind::DiffChangesHalfPageDown,
    "scroll the changes view down half a page",
    "Move the changes view cursor down by half the height of the changes view.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesHalfPageUpDef,
    DiffChangesHalfPageUp,
    "DiffChangesHalfPageUp",
    ActionKind::DiffChangesHalfPageUp,
    "scroll the changes view up half a page",
    "Move the changes view cursor up by half the height of the changes view.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesFirstDef,
    DiffChangesFirst,
    "DiffChangesFirst",
    ActionKind::DiffChangesFirst,
    "jump to the first changes view row",
    "Move the changes view cursor to the first row.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesLastDef,
    DiffChangesLast,
    "DiffChangesLast",
    ActionKind::DiffChangesLast,
    "jump to the last changes view row",
    "Move the changes view cursor to the last row.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesOpenDef,
    DiffChangesOpen,
    "DiffChangesOpen",
    ActionKind::DiffChangesOpen,
    "open the per-file diff at the cursor's hunk",
    "Close the changes view and open the file under the cursor in the per-file \
     diff, with the cursor on the line that the changes view row shows. A row \
     with no file under it keeps the changes view open.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesExpandUpDef,
    DiffChangesExpandUp,
    "DiffChangesExpandUp",
    ActionKind::DiffChangesExpandUp,
    "open ten unchanged lines above the cursor's hunk",
    "Open ten unchanged lines of the nearest fold above the cursor, from the \
     fold's bottom edge, so the hunk below the fold grows up. On a fold row it \
     opens ten lines of that fold the same way. A fold of ten lines or fewer \
     opens whole, and the fold below a file's last hunk only grows down. An \
     expand stays inside the cursor's file, and with no fold above the cursor \
     the status row says so. Opened lines stay open through every edit until \
     DiffChangesCollapse folds them.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesExpandDownDef,
    DiffChangesExpandDown,
    "DiffChangesExpandDown",
    ActionKind::DiffChangesExpandDown,
    "open ten unchanged lines below the cursor's hunk",
    "Open ten unchanged lines of the nearest fold below the cursor, from the \
     fold's top edge, so the hunk above the fold grows down. On a fold row it \
     opens ten lines of that fold the same way. A fold of ten lines or fewer \
     opens whole, and the fold above a file's first hunk only grows up. An \
     expand stays inside the cursor's file, and with no fold below the cursor \
     the status row says so. Opened lines stay open through every edit until \
     DiffChangesCollapse folds them.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesExpandAllDef,
    DiffChangesExpandAll,
    "DiffChangesExpandAll",
    ActionKind::DiffChangesExpandAll,
    "open the whole fold at the cursor",
    "Open the whole fold of the fold row under the cursor. On any other row it \
     opens the nearest fold above and the nearest fold below the cursor, each \
     whole. An expand stays inside the cursor's file, and with no fold to open \
     the status row says so. Opened lines stay open through every edit until \
     DiffChangesCollapse folds them.",
    ActionPriority::Rare
);

define_action!(
    DiffChangesCollapseDef,
    DiffChangesCollapse,
    "DiffChangesCollapse",
    ActionKind::DiffChangesCollapse,
    "refold the revealed lines around the cursor",
    "Fold again the lines that an expand opened around the cursor. On an opened \
     row it folds every opened span of the run of shown rows that holds the \
     cursor. On a fold row it folds the opened spans that touch that fold. A \
     cursor that the new fold hides moves to the fold row.",
    ActionPriority::Rare
);
