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
