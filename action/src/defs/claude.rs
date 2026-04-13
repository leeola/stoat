use crate::{action::define_action, ActionKind};

define_action!(
    OpenClaudeDef,
    OpenClaude,
    "OpenClaude",
    ActionKind::OpenClaude,
    "open claude chat",
    "Open a Claude Code chat panel in the right dock."
);

define_action!(
    ClaudeSubmitDef,
    ClaudeSubmit,
    "ClaudeSubmit",
    ActionKind::ClaudeSubmit,
    "send to claude",
    "Send the current input to Claude."
);

define_action!(
    ToggleDockRightDef,
    ToggleDockRight,
    "ToggleDockRight",
    ActionKind::ToggleDockRight,
    "toggle right dock",
    "Cycle the right dock panel through visible, minimized, and hidden states."
);

define_action!(
    ToggleDockLeftDef,
    ToggleDockLeft,
    "ToggleDockLeft",
    ActionKind::ToggleDockLeft,
    "toggle left dock",
    "Cycle the left dock panel through visible, minimized, and hidden states."
);
