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
    PaletteHistoryPrevDef,
    PaletteHistoryPrev,
    "PaletteHistoryPrev",
    ActionKind::PaletteHistoryPrev,
    "recall older palette history",
    "Recall the previous command from palette history, fish-style: the \
     already-typed text is a substring needle that filters matches. Bound \
     by default to Up while the command palette is open.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    PaletteHistoryNextDef,
    PaletteHistoryNext,
    "PaletteHistoryNext",
    ActionKind::PaletteHistoryNext,
    "recall newer palette history",
    "Recall the next command toward the newest in palette history, under \
     the same substring needle. Stepping past the newest restores the \
     originally-typed text. Bound by default to Down while the command \
     palette is open.",
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
