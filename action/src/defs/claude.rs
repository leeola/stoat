use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    ToggleDockRightDef,
    ToggleDockRight,
    "dock-right",
    ActionKind::ToggleDockRight,
    "toggle right dock",
    "Cycle the right dock panel through visible, minimized, and hidden states.",
    ActionPriority::Rare
);

define_action!(
    ToggleDockLeftDef,
    ToggleDockLeft,
    "dock-left",
    ActionKind::ToggleDockLeft,
    "toggle left dock",
    "Cycle the left dock panel through visible, minimized, and hidden states.",
    ActionPriority::Rare
);
