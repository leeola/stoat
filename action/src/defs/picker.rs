use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    JumplistPickerNextDef,
    JumplistPickerNext,
    "JumplistPickerNext",
    ActionKind::JumplistPickerNext,
    "next jumplist row",
    "Move the jumplist picker's selection to the next row.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    JumplistPickerPrevDef,
    JumplistPickerPrev,
    "JumplistPickerPrev",
    ActionKind::JumplistPickerPrev,
    "previous jumplist row",
    "Move the jumplist picker's selection to the previous row.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    JumplistPickerPageDownDef,
    JumplistPickerPageDown,
    "JumplistPickerPageDown",
    ActionKind::JumplistPickerPageDown,
    "page the jumplist down",
    "Move the jumplist picker's selection down by half the visible rows, matching how every other modal list pages.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    JumplistPickerPageUpDef,
    JumplistPickerPageUp,
    "JumplistPickerPageUp",
    ActionKind::JumplistPickerPageUp,
    "page the jumplist up",
    "Move the jumplist picker's selection up by half the visible rows, the counterpart to JumplistPickerPageDown.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    JumplistPickerSelectDef,
    JumplistPickerSelect,
    "JumplistPickerSelect",
    ActionKind::JumplistPickerSelect,
    "jump to selected row",
    "Jump the focused editor to the location under the jumplist picker's selection.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    JumplistPickerCloseDef,
    JumplistPickerClose,
    "JumplistPickerClose",
    ActionKind::JumplistPickerClose,
    "close jumplist picker",
    "Dismiss the jumplist picker without jumping.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    DiagnosticsPickerNextDef,
    DiagnosticsPickerNext,
    "DiagnosticsPickerNext",
    ActionKind::DiagnosticsPickerNext,
    "next diagnostic row",
    "Move the diagnostics picker's selection to the next row.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    DiagnosticsPickerPrevDef,
    DiagnosticsPickerPrev,
    "DiagnosticsPickerPrev",
    ActionKind::DiagnosticsPickerPrev,
    "previous diagnostic row",
    "Move the diagnostics picker's selection to the previous row.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    DiagnosticsPickerPageDownDef,
    DiagnosticsPickerPageDown,
    "DiagnosticsPickerPageDown",
    ActionKind::DiagnosticsPickerPageDown,
    "page the diagnostics list down",
    "Move the diagnostics picker's selection down by half the visible rows, matching how every other modal list pages.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    DiagnosticsPickerPageUpDef,
    DiagnosticsPickerPageUp,
    "DiagnosticsPickerPageUp",
    ActionKind::DiagnosticsPickerPageUp,
    "page the diagnostics list up",
    "Move the diagnostics picker's selection up by half the visible rows, the counterpart to DiagnosticsPickerPageDown.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    DiagnosticsPickerSelectDef,
    DiagnosticsPickerSelect,
    "DiagnosticsPickerSelect",
    ActionKind::DiagnosticsPickerSelect,
    "jump to selected diagnostic",
    "Jump the focused editor to the diagnostic under the picker's selection, opening its file first for workspace-scope entries.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    DiagnosticsPickerCloseDef,
    DiagnosticsPickerClose,
    "DiagnosticsPickerClose",
    ActionKind::DiagnosticsPickerClose,
    "close diagnostics picker",
    "Dismiss the diagnostics picker without jumping.",
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
    LocationPickerNextDef,
    LocationPickerNext,
    "LocationPickerNext",
    ActionKind::LocationPickerNext,
    "next location row",
    "Move the goto-location picker's selection to the next row.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    LocationPickerPrevDef,
    LocationPickerPrev,
    "LocationPickerPrev",
    ActionKind::LocationPickerPrev,
    "previous location row",
    "Move the goto-location picker's selection to the previous row.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    LocationPickerPageDownDef,
    LocationPickerPageDown,
    "LocationPickerPageDown",
    ActionKind::LocationPickerPageDown,
    "page the location list down",
    "Move the goto-location picker's selection down by half the visible rows, matching how every other modal list pages.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    LocationPickerPageUpDef,
    LocationPickerPageUp,
    "LocationPickerPageUp",
    ActionKind::LocationPickerPageUp,
    "page the location list up",
    "Move the goto-location picker's selection up by half the visible rows, the counterpart to LocationPickerPageDown.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    LocationPickerSelectDef,
    LocationPickerSelect,
    "LocationPickerSelect",
    ActionKind::LocationPickerSelect,
    "jump to selected location",
    "Jump the focused editor to the goto candidate under the picker's selection.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    LocationPickerCloseDef,
    LocationPickerClose,
    "LocationPickerClose",
    ActionKind::LocationPickerClose,
    "close location picker",
    "Dismiss the goto-location picker without jumping.",
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
