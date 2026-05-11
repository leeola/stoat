use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    OpenClaudeDef,
    OpenClaude,
    "OpenClaude",
    ActionKind::OpenClaude,
    "open claude chat",
    "Open a Claude Code chat panel. Placement is controlled by the `claude.default_placement` setting (defaults to a split pane).",
    ActionPriority::Common
);

define_action!(
    ClaudeToPaneDef,
    ClaudeToPane,
    "ClaudeToPane",
    ActionKind::ClaudeToPane,
    "move claude to pane",
    "Move the active Claude chat into a new split pane. Reuses the existing session.",
    ActionPriority::Rare
);

define_action!(
    ClaudeToDockLeftDef,
    ClaudeToDockLeft,
    "ClaudeToDockLeft",
    ActionKind::ClaudeToDockLeft,
    "move claude to left dock",
    "Move the active Claude chat to the left dock. Reuses the existing session.",
    ActionPriority::Rare
);

define_action!(
    ClaudeToDockRightDef,
    ClaudeToDockRight,
    "ClaudeToDockRight",
    ActionKind::ClaudeToDockRight,
    "move claude to right dock",
    "Move the active Claude chat to the right dock. Reuses the existing session.",
    ActionPriority::Rare
);

define_action!(
    ClaudeSubmitDef,
    ClaudeSubmit,
    "ClaudeSubmit",
    ActionKind::ClaudeSubmit,
    "send to claude",
    "Send the current input to Claude.",
    ActionPriority::Rare
);

define_action!(
    ClaudeToggleFollowDef,
    ClaudeToggleFollow,
    "ClaudeToggleFollow",
    ActionKind::ClaudeToggleFollow,
    "toggle claude follow",
    "Toggle Claude follow mode. When on, file-oriented tool calls open their target file in an editor pane and move the cursor to the line Claude is touching.",
    ActionPriority::Rare
);

define_action!(
    OpenCheckpointPickerDef,
    OpenCheckpointPicker,
    "OpenCheckpointPicker",
    ActionKind::OpenCheckpointPicker,
    "claude restore",
    "Open a picker listing every per-message checkpoint captured for the active Claude chat. Selecting an entry restores the working tree to the state captured when the user submitted that message.",
    ActionPriority::Common
);

define_action!(
    ClaudeInterruptDef,
    ClaudeInterrupt,
    "ClaudeInterrupt",
    ActionKind::ClaudeInterrupt,
    "interrupt claude",
    "Cancel the in-flight Claude turn. Sends interrupt to the agent over the control protocol and marks any pending tool calls as cancelled in the chat scrollback.",
    ActionPriority::Common
);

define_action!(
    ClaudeFocusNextToolCardDef,
    ClaudeFocusNextToolCard,
    "ClaudeFocusNextToolCard",
    ActionKind::ClaudeFocusNextToolCard,
    "focus next tool card",
    "Move keyboard focus to the next tool-call card in the chat scrollback (older direction). Engages card focus when none is set.",
    ActionPriority::Common
);

define_action!(
    ClaudeFocusPrevToolCardDef,
    ClaudeFocusPrevToolCard,
    "ClaudeFocusPrevToolCard",
    ActionKind::ClaudeFocusPrevToolCard,
    "focus previous tool card",
    "Move keyboard focus to the previous tool-call card in the chat scrollback (newer direction).",
    ActionPriority::Common
);

define_action!(
    ClaudeToggleToolCardExpandDef,
    ClaudeToggleToolCardExpand,
    "ClaudeToggleToolCardExpand",
    ActionKind::ClaudeToggleToolCardExpand,
    "toggle tool card",
    "Expand or collapse the tool-call card currently focused for keyboard navigation.",
    ActionPriority::Common
);

define_action!(
    ClaudeJumpToFocusedCardDef,
    ClaudeJumpToFocusedCard,
    "ClaudeJumpToFocusedCard",
    ActionKind::ClaudeJumpToFocusedCard,
    "jump to focused tool card",
    "Open the file referenced by the focused tool-call card in an editor pane and move the cursor to the referenced line. No-op when the focused card has no file path in its tool input.",
    ActionPriority::Common
);

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
