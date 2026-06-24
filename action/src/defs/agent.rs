use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    SpawnClaudeDef,
    SpawnClaude,
    "SpawnClaude",
    ActionKind::SpawnClaude,
    "spawn claude",
    "Launch a Claude agent session in the focused pane.",
    ActionPriority::Common
);
