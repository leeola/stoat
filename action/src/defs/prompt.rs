use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    SubmitPromptInputDef,
    SubmitPromptInput,
    "SubmitPromptInput",
    ActionKind::SubmitPromptInput,
    "submit prompt input",
    "Submit the currently focused prompt input (command palette, help search, \
     reword, etc.). Routes to the owning consumer based on focus.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    CancelPromptInputDef,
    CancelPromptInput,
    "CancelPromptInput",
    ActionKind::CancelPromptInput,
    "cancel prompt input",
    "Cancel the currently focused prompt input, closing the modal or \
     discarding the draft as appropriate for its owning consumer.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    PromptInsertNewlineDef,
    PromptInsertNewline,
    "PromptInsertNewline",
    ActionKind::PromptInsertNewline,
    "insert newline in prompt",
    "Insert a literal newline at the cursor without submitting. Typically \
     bound to Shift-Enter or Alt-Enter in prompt mode so Enter stays reserved \
     for submission.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    PromptHistoryPrevDef,
    PromptHistoryPrev,
    "PromptHistoryPrev",
    ActionKind::PromptHistoryPrev,
    "recall older prompt history",
    "Recall the previous entry from the open prompt's history, fish-style: \
     the already-typed text is a substring needle that filters matches. A \
     prompt that keeps no history does nothing. Bound by default to Alt-Up.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    PromptHistoryNextDef,
    PromptHistoryNext,
    "PromptHistoryNext",
    ActionKind::PromptHistoryNext,
    "recall newer prompt history",
    "Recall the next entry toward the newest in the open prompt's history, \
     under the same substring needle. Stepping past the newest restores the \
     originally-typed text. Bound by default to Alt-Down.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    PaletteScopeToggleDef,
    PaletteScopeToggle,
    "PaletteScopeToggle",
    ActionKind::PaletteScopeToggle,
    "toggle command palette scope",
    "Flip the command palette between its default Active scope (actions \
     applicable to the current UI/user state) and All scope (every \
     palette-visible action). Bound by default to Shift-Tab while the \
     palette is open.",
    ActionPriority::Normal,
    palette_visible = false
);
