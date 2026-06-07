use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    ToggleDockRightDef,
    ToggleDockRight,
    "ToggleDockRight",
    ActionKind::ToggleDockRight,
    "toggle right dock",
    "Cycle the right dock panel through visible, minimized, and hidden states.",
    ActionPriority::Rare
);

define_action!(
    ToggleDockLeftDef,
    ToggleDockLeft,
    "ToggleDockLeft",
    ActionKind::ToggleDockLeft,
    "toggle left dock",
    "Cycle the left dock panel through visible, minimized, and hidden states.",
    ActionPriority::Rare
);
