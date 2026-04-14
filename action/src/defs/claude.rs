use crate::{action::define_action, ActionKind};

define_action!(
    OpenClaudeDef,
    OpenClaude,
    "OpenClaude",
    ActionKind::OpenClaude,
    "open claude chat",
    "Open a Claude Code chat panel. Placement is controlled by the `claude.default_placement` setting (defaults to a split pane)."
);

define_action!(
    ClaudeToPaneDef,
    ClaudeToPane,
    "ClaudeToPane",
    ActionKind::ClaudeToPane,
    "move claude to pane",
    "Move the active Claude chat into a new split pane. Reuses the existing session."
);

define_action!(
    ClaudeToDockLeftDef,
    ClaudeToDockLeft,
    "ClaudeToDockLeft",
    ActionKind::ClaudeToDockLeft,
    "move claude to left dock",
    "Move the active Claude chat to the left dock. Reuses the existing session."
);

define_action!(
    ClaudeToDockRightDef,
    ClaudeToDockRight,
    "ClaudeToDockRight",
    ActionKind::ClaudeToDockRight,
    "move claude to right dock",
    "Move the active Claude chat to the right dock. Reuses the existing session."
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
