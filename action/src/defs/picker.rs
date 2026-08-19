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
    PickerFirstDef,
    PickerFirst,
    "PickerFirst",
    ActionKind::PickerFirst,
    "first picker row",
    "Move the open list modal's selection to the first row. Bound only where the modal takes normal-mode keys, since a prompt in insert mode reads g as text.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    PickerLastDef,
    PickerLast,
    "PickerLast",
    ActionKind::PickerLast,
    "last picker row",
    "Move the open list modal's selection to the last row, the counterpart to PickerFirst.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    PickerDetailDownDef,
    PickerDetailDown,
    "PickerDetailDown",
    ActionKind::PickerDetailDown,
    "scroll the picker preview down",
    "Scroll the open list modal's preview pane down by half its rows, leaving the selection where it is. A modal with no preview does nothing.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    PickerDetailUpDef,
    PickerDetailUp,
    "PickerDetailUp",
    ActionKind::PickerDetailUp,
    "scroll the picker preview up",
    "Scroll the open list modal's preview pane up by half its rows, the counterpart to PickerDetailDown.",
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
