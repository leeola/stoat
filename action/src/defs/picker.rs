use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    PickerNextDef,
    PickerNext,
    "PickerNext",
    ActionKind::PickerNext,
    "next picker row",
    "Move the open list modal's selection to the next row. Each modal binds this in its own keymap block, so one verb serves them all.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    PickerPrevDef,
    PickerPrev,
    "PickerPrev",
    ActionKind::PickerPrev,
    "previous picker row",
    "Move the open list modal's selection to the previous row, the counterpart to PickerNext.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    PickerPageDownDef,
    PickerPageDown,
    "PickerPageDown",
    ActionKind::PickerPageDown,
    "page the picker down",
    "Move the open list modal's selection down by half the visible rows.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    PickerPageUpDef,
    PickerPageUp,
    "PickerPageUp",
    ActionKind::PickerPageUp,
    "page the picker up",
    "Move the open list modal's selection up by half the visible rows, the counterpart to PickerPageDown.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    PickerCompleteDef,
    PickerComplete,
    "PickerComplete",
    ActionKind::PickerComplete,
    "complete from the picker",
    "Fill the open list modal's prompt from its selected row. A modal with nothing to complete does nothing.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CommitPickerNextDef,
    CommitPickerNext,
    "CommitPickerNext",
    ActionKind::CommitPickerNext,
    "next commit row",
    "Move the commit picker's selection to the next row.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CommitPickerPageDownDef,
    CommitPickerPageDown,
    "CommitPickerPageDown",
    ActionKind::CommitPickerPageDown,
    "page the commit list down",
    "Move the commit picker's selection down by half the visible rows, matching how every other modal list pages.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CommitPickerPageUpDef,
    CommitPickerPageUp,
    "CommitPickerPageUp",
    ActionKind::CommitPickerPageUp,
    "page the commit list up",
    "Move the commit picker's selection up by half the visible rows, the counterpart to CommitPickerPageDown.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CommitPickerNextBranchDef,
    CommitPickerNextBranch,
    "CommitPickerNextBranch",
    ActionKind::CommitPickerNextBranch,
    "jump to the next branch tip",
    "Move the commit picker's selection down the list to the nearest commit a local branch points at, skipping the ordinary commits between.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CommitPickerPrevBranchDef,
    CommitPickerPrevBranch,
    "CommitPickerPrevBranch",
    ActionKind::CommitPickerPrevBranch,
    "jump to the previous branch tip",
    "Move the commit picker's selection up the list to the nearest commit a local branch points at, the counterpart to CommitPickerNextBranch.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CommitPickerColumnCycleDef,
    CommitPickerColumnCycle,
    "CommitPickerColumnCycle",
    ActionKind::CommitPickerColumnCycle,
    "cycle the filtered commit column",
    "Advance which column of the commit table the query filters, cycling through every column and back to searching all of them.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CommitPickerDrillInDef,
    CommitPickerDrillIn,
    "CommitPickerDrillIn",
    ActionKind::CommitPickerDrillIn,
    "drill into the selected merge",
    "Re-scope the commit list to the commits the selected merge brought in, leaving out the mainline it merged into. Reports a badge on a row that is not a merge.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CommitPickerBackDef,
    CommitPickerBack,
    "CommitPickerBack",
    ActionKind::CommitPickerBack,
    "leave the drilled commit scope",
    "Return to the commit list a drill re-scoped away from, restoring its selection and query. Does nothing at the outermost scope.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CommitPickerPrevDef,
    CommitPickerPrev,
    "CommitPickerPrev",
    ActionKind::CommitPickerPrev,
    "previous commit row",
    "Move the commit picker's selection to the previous row.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CommitPickerSelectDef,
    CommitPickerSelect,
    "CommitPickerSelect",
    ActionKind::CommitPickerSelect,
    "review from the selected commit",
    "Take the commit under the picker's selection as the review base.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CommitPickerCloseDef,
    CommitPickerClose,
    "CommitPickerClose",
    ActionKind::CommitPickerClose,
    "close commit picker",
    "Dismiss the commit picker without starting a review.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CodeSearchNextDef,
    CodeSearchNext,
    "CodeSearchNext",
    ActionKind::CodeSearchNext,
    "next code-search result",
    "Move the code-search modal's selection to the next match.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CodeSearchPrevDef,
    CodeSearchPrev,
    "CodeSearchPrev",
    ActionKind::CodeSearchPrev,
    "previous code-search result",
    "Move the code-search modal's selection to the previous match.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CodeSearchPageDownDef,
    CodeSearchPageDown,
    "CodeSearchPageDown",
    ActionKind::CodeSearchPageDown,
    "page code search down",
    "Move the code-search modal's selection down by half the visible list \
     height. Bound by default to Ctrl-F while the modal is open.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CodeSearchPageUpDef,
    CodeSearchPageUp,
    "CodeSearchPageUp",
    ActionKind::CodeSearchPageUp,
    "page code search up",
    "Move the code-search modal's selection up by half the visible list \
     height. Bound by default to Ctrl-B while the modal is open.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CodeSearchSelectDef,
    CodeSearchSelect,
    "CodeSearchSelect",
    ActionKind::CodeSearchSelect,
    "open selected code-search match",
    "Open the file under the code-search selection and jump to the match.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CodeSearchCloseDef,
    CodeSearchClose,
    "CodeSearchClose",
    ActionKind::CodeSearchClose,
    "close code search",
    "Dismiss the code-search modal without jumping.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    CodeSearchModeToggleDef,
    CodeSearchModeToggle,
    "CodeSearchModeToggle",
    ActionKind::CodeSearchModeToggle,
    "toggle code search mode",
    "Switch the code-search modal between regex and quick AST-pattern search. AST mode matches ast-grep patterns against files of the focused buffer's language.",
    ActionPriority::Common,
    palette_visible = false
);
