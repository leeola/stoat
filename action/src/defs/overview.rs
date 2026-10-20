use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    DiffOverviewDef,
    DiffOverview,
    "DiffOverview",
    ActionKind::DiffOverview,
    "show every change as one hunk list",
    "Open an overview of every working-tree change against the diff base. \
     The overview shows the hunks of each changed file side by side, in one \
     list that scrolls. From the per-file diff it opens on the hunk under the \
     cursor. Run on the overview, it opens the per-file diff at the cursor's \
     hunk, so one key flips between the two layouts.",
    ActionPriority::Common,
    command_name = "diff-overview"
);

define_action!(
    CloseOverviewDef,
    CloseOverview,
    "CloseOverview",
    ActionKind::CloseOverview,
    "close the diff overview",
    "Close the overview and put back the view it covered. An overview opened \
     from the per-file diff returns to that diff.",
    ActionPriority::Rare
);

define_action!(
    OverviewNextDef,
    OverviewNext,
    "OverviewNext",
    ActionKind::OverviewNext,
    "move to the next overview row",
    "Move the overview cursor down one row, or down by the count typed \
     before the key.",
    ActionPriority::Rare
);

define_action!(
    OverviewPrevDef,
    OverviewPrev,
    "OverviewPrev",
    ActionKind::OverviewPrev,
    "move to the previous overview row",
    "Move the overview cursor up one row, or up by the count typed before \
     the key.",
    ActionPriority::Rare
);

define_action!(
    OverviewNextChunkDef,
    OverviewNextChunk,
    "OverviewNextChunk",
    ActionKind::OverviewNextChunk,
    "move to the next hunk in the overview",
    "Move the overview cursor to the header of the next hunk. At the last \
     hunk the cursor stays, and the status says \"no more changes\".",
    ActionPriority::Rare
);

define_action!(
    OverviewPrevChunkDef,
    OverviewPrevChunk,
    "OverviewPrevChunk",
    ActionKind::OverviewPrevChunk,
    "move to the previous hunk in the overview",
    "Move the overview cursor to the header of the previous hunk. Inside a \
     hunk, the first step goes to the header of that hunk. At the first hunk \
     the cursor stays, and the status says \"no more changes\".",
    ActionPriority::Rare
);

define_action!(
    OverviewPageDownDef,
    OverviewPageDown,
    "OverviewPageDown",
    ActionKind::OverviewPageDown,
    "scroll the overview down a page",
    "Move the overview cursor down by the height of the overview.",
    ActionPriority::Rare
);

define_action!(
    OverviewPageUpDef,
    OverviewPageUp,
    "OverviewPageUp",
    ActionKind::OverviewPageUp,
    "scroll the overview up a page",
    "Move the overview cursor up by the height of the overview.",
    ActionPriority::Rare
);

define_action!(
    OverviewHalfPageDownDef,
    OverviewHalfPageDown,
    "OverviewHalfPageDown",
    ActionKind::OverviewHalfPageDown,
    "scroll the overview down half a page",
    "Move the overview cursor down by half the height of the overview.",
    ActionPriority::Rare
);

define_action!(
    OverviewHalfPageUpDef,
    OverviewHalfPageUp,
    "OverviewHalfPageUp",
    ActionKind::OverviewHalfPageUp,
    "scroll the overview up half a page",
    "Move the overview cursor up by half the height of the overview.",
    ActionPriority::Rare
);

define_action!(
    OverviewFirstDef,
    OverviewFirst,
    "OverviewFirst",
    ActionKind::OverviewFirst,
    "jump to the first overview row",
    "Move the overview cursor to the first row.",
    ActionPriority::Rare
);

define_action!(
    OverviewLastDef,
    OverviewLast,
    "OverviewLast",
    ActionKind::OverviewLast,
    "jump to the last overview row",
    "Move the overview cursor to the last row.",
    ActionPriority::Rare
);

define_action!(
    OverviewOpenDef,
    OverviewOpen,
    "OverviewOpen",
    ActionKind::OverviewOpen,
    "open the per-file diff at the cursor's hunk",
    "Close the overview and open the file under the cursor in the per-file \
     diff, with the cursor on the line that the overview row shows. A row \
     with no file under it keeps the overview open.",
    ActionPriority::Rare
);
