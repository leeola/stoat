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
         Claude chat, reword, etc.). Routes to the owning consumer based on focus."
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
pub struct ShellInputSubmitDef;

impl ActionDef for ShellInputSubmitDef {
    fn name(&self) -> &'static str {
        "ShellInputSubmit"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ShellInputSubmit
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "submit shell input modal"
    }

    fn long_desc(&self) -> &'static str {
        "Run the typed shell command for the active ShellInputModal (Pipe, \
         PipeTo, InsertOutput, AppendOutput, or KeepPipe) and dismiss the \
         modal. Bypassed when no shell input modal is open."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct ShellInputSubmit;

impl ShellInputSubmit {
    pub const DEF: &ShellInputSubmitDef = &ShellInputSubmitDef;
}

impl Action for ShellInputSubmit {
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
