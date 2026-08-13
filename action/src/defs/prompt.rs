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
    PaletteSelectPrevDef,
    PaletteSelectPrev,
    "PaletteSelectPrev",
    ActionKind::PaletteSelectPrev,
    "select previous palette entry",
    "Move the palette selection up by one row. Bound by default to Up and \
     Ctrl-P while the command palette is open.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    PaletteSelectNextDef,
    PaletteSelectNext,
    "PaletteSelectNext",
    ActionKind::PaletteSelectNext,
    "select next palette entry",
    "Move the palette selection down by one row. Bound by default to Down \
     and Ctrl-N while the command palette is open.",
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
    PaletteCompleteDef,
    PaletteComplete,
    "PaletteComplete",
    ActionKind::PaletteComplete,
    "complete selected palette entry",
    "Complete the highlighted candidate into the palette input. From the \
     command list this is the selected command, completed with a trailing \
     space when it takes arguments so the argument picker opens. From an \
     argument list it is the selected row, for every picker-backed argument \
     such as a file, buffer, theme, or directory. Bound by default to Tab; \
     a no-op when the list is empty.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    PalettePageUpDef,
    PalettePageUp,
    "PalettePageUp",
    ActionKind::PalettePageUp,
    "page palette up",
    "Move the palette selection up by half the visible list height. \
     Bound by default to Ctrl-B while the command palette is open.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    PalettePageDownDef,
    PalettePageDown,
    "PalettePageDown",
    ActionKind::PalettePageDown,
    "page palette down",
    "Move the palette selection down by half the visible list height. \
     Bound by default to Ctrl-F while the command palette is open.",
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
