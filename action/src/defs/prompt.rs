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
    "Recall the previous entry from the open prompt's history, fish-style. \
     An entry matches when it holds every word of the already-typed text, in \
     any order, with ASCII case ignored. A prompt that keeps no history does \
     nothing. Bound by default to Alt-Up, and in the command palette to \
     Ctrl-Up as well.",
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
     under the same needle. Stepping past the newest restores the \
     originally-typed text. Bound by default to Alt-Down, and in the command \
     palette to Ctrl-Down as well.",
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
     palette-visible action). In the list of a file argument, step the file \
     scope as the File Finder steps it. Bound by default to Shift-Tab while \
     the palette is open.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    PaletteUpDef,
    PaletteUp,
    "PaletteUp",
    ActionKind::PaletteUp,
    "move up or recall palette history",
    "Move the command palette's selection up one row. On the first row of \
     the command list or of the argument list, and over a free-typed \
     argument, recall the palette's history instead. The typed text is the \
     needle, and the newest line that holds each of its words goes into the \
     input. While the history walks, recall the next older match. Bound by \
     default to Up while the palette is open.",
    ActionPriority::Normal,
    palette_visible = false
);
