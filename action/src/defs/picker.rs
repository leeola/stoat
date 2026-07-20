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
