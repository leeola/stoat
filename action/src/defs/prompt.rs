use crate::{Action, ActionDef, ActionKind, ParamDef};
use std::any::Any;

#[derive(Debug)]
pub struct SubmitPromptInputDef;

impl ActionDef for SubmitPromptInputDef {
    fn name(&self) -> &'static str {
        "SubmitPromptInput"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::SubmitPromptInput
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "submit prompt input"
    }

    fn long_desc(&self) -> &'static str {
        "Submit the currently focused prompt input (command palette, help search, \
         reword, etc.). Routes to the owning consumer based on focus."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct SubmitPromptInput;

impl SubmitPromptInput {
    pub const DEF: &SubmitPromptInputDef = &SubmitPromptInputDef;
}

impl Action for SubmitPromptInput {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct CancelPromptInputDef;

impl ActionDef for CancelPromptInputDef {
    fn name(&self) -> &'static str {
        "CancelPromptInput"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::CancelPromptInput
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "cancel prompt input"
    }

    fn long_desc(&self) -> &'static str {
        "Cancel the currently focused prompt input, closing the modal or \
         discarding the draft as appropriate for its owning consumer."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct CancelPromptInput;

impl CancelPromptInput {
    pub const DEF: &CancelPromptInputDef = &CancelPromptInputDef;
}

impl Action for CancelPromptInput {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PromptInsertNewlineDef;

impl ActionDef for PromptInsertNewlineDef {
    fn name(&self) -> &'static str {
        "PromptInsertNewline"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PromptInsertNewline
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "insert newline in prompt"
    }

    fn long_desc(&self) -> &'static str {
        "Insert a literal newline at the cursor without submitting. Typically \
         bound to Shift-Enter or Alt-Enter in prompt mode so Enter stays reserved \
         for submission."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct PromptInsertNewline;

impl PromptInsertNewline {
    pub const DEF: &PromptInsertNewlineDef = &PromptInsertNewlineDef;
}

impl Action for PromptInsertNewline {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PaletteSelectPrevDef;

impl ActionDef for PaletteSelectPrevDef {
    fn name(&self) -> &'static str {
        "PaletteSelectPrev"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PaletteSelectPrev
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "select previous palette entry"
    }

    fn long_desc(&self) -> &'static str {
        "Move the palette selection up by one row. Bound by default to Up and \
         Ctrl-P while the command palette is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct PaletteSelectPrev;

impl PaletteSelectPrev {
    pub const DEF: &PaletteSelectPrevDef = &PaletteSelectPrevDef;
}

impl Action for PaletteSelectPrev {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PaletteSelectNextDef;

impl ActionDef for PaletteSelectNextDef {
    fn name(&self) -> &'static str {
        "PaletteSelectNext"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PaletteSelectNext
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "select next palette entry"
    }

    fn long_desc(&self) -> &'static str {
        "Move the palette selection down by one row. Bound by default to Down \
         and Ctrl-N while the command palette is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct PaletteSelectNext;

impl PaletteSelectNext {
    pub const DEF: &PaletteSelectNextDef = &PaletteSelectNextDef;
}

impl Action for PaletteSelectNext {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PaletteHistoryPrevDef;

impl ActionDef for PaletteHistoryPrevDef {
    fn name(&self) -> &'static str {
        "PaletteHistoryPrev"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PaletteHistoryPrev
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "recall older palette history"
    }

    fn long_desc(&self) -> &'static str {
        "Recall the previous command from palette history, fish-style: the \
         already-typed text is a substring needle that filters matches. Bound \
         by default to Up while the command palette is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct PaletteHistoryPrev;

impl PaletteHistoryPrev {
    pub const DEF: &PaletteHistoryPrevDef = &PaletteHistoryPrevDef;
}

impl Action for PaletteHistoryPrev {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PaletteHistoryNextDef;

impl ActionDef for PaletteHistoryNextDef {
    fn name(&self) -> &'static str {
        "PaletteHistoryNext"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PaletteHistoryNext
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "recall newer palette history"
    }

    fn long_desc(&self) -> &'static str {
        "Recall the next command toward the newest in palette history, under \
         the same substring needle. Stepping past the newest restores the \
         originally-typed text. Bound by default to Down while the command \
         palette is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct PaletteHistoryNext;

impl PaletteHistoryNext {
    pub const DEF: &PaletteHistoryNextDef = &PaletteHistoryNextDef;
}

impl Action for PaletteHistoryNext {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PaletteCompleteDef;

impl ActionDef for PaletteCompleteDef {
    fn name(&self) -> &'static str {
        "PaletteComplete"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PaletteComplete
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "complete selected palette entry"
    }

    fn long_desc(&self) -> &'static str {
        "Complete the highlighted candidate into the palette input. From the \
         command list this is the selected command, completed with a trailing \
         space when it takes arguments so the argument picker opens. From an \
         argument list it is the selected row, for every picker-backed argument \
         such as a file, buffer, theme, or directory. Bound by default to Tab; \
         a no-op when the list is empty."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct PaletteComplete;

impl PaletteComplete {
    pub const DEF: &PaletteCompleteDef = &PaletteCompleteDef;
}

impl Action for PaletteComplete {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PalettePageUpDef;

impl ActionDef for PalettePageUpDef {
    fn name(&self) -> &'static str {
        "PalettePageUp"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PalettePageUp
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "page palette up"
    }

    fn long_desc(&self) -> &'static str {
        "Move the palette selection up by half the visible list height. \
         Bound by default to Ctrl-B while the command palette is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct PalettePageUp;

impl PalettePageUp {
    pub const DEF: &PalettePageUpDef = &PalettePageUpDef;
}

impl Action for PalettePageUp {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PalettePageDownDef;

impl ActionDef for PalettePageDownDef {
    fn name(&self) -> &'static str {
        "PalettePageDown"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PalettePageDown
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "page palette down"
    }

    fn long_desc(&self) -> &'static str {
        "Move the palette selection down by half the visible list height. \
         Bound by default to Ctrl-F while the command palette is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct PalettePageDown;

impl PalettePageDown {
    pub const DEF: &PalettePageDownDef = &PalettePageDownDef;
}

impl Action for PalettePageDown {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct PaletteScopeToggleDef;

impl ActionDef for PaletteScopeToggleDef {
    fn name(&self) -> &'static str {
        "PaletteScopeToggle"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::PaletteScopeToggle
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "toggle command palette scope"
    }

    fn long_desc(&self) -> &'static str {
        "Flip the command palette between its default Active scope (actions \
         applicable to the current UI/user state) and All scope (every \
         palette-visible action). Bound by default to Shift-Tab while the \
         palette is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct PaletteScopeToggle;

impl PaletteScopeToggle {
    pub const DEF: &PaletteScopeToggleDef = &PaletteScopeToggleDef;
}

impl Action for PaletteScopeToggle {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
