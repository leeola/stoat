use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    GotoNextDiagnosticDef,
    GotoNextDiagnostic,
    "GotoNextDiagnostic",
    ActionKind::GotoNextDiagnostic,
    "jump to the next diagnostic",
    "Move the primary cursor to the next LSP diagnostic in the focused buffer. \
     Searches forward from the cursor's current byte offset; no-op when no \
     diagnostic lies after the cursor. Does not wrap.",
    ActionPriority::Rare
);

define_action!(
    GotoPrevDiagnosticDef,
    GotoPrevDiagnostic,
    "GotoPrevDiagnostic",
    ActionKind::GotoPrevDiagnostic,
    "jump to the previous diagnostic",
    "Move the primary cursor to the previous LSP diagnostic in the focused \
     buffer. Searches backward from the cursor's current byte offset; no-op \
     when no diagnostic lies before the cursor. Does not wrap.",
    ActionPriority::Rare
);

define_action!(
    GotoDefinitionDef,
    GotoDefinition,
    "GotoDefinition",
    ActionKind::GotoDefinition,
    "jump to symbol definition",
    "Move the primary cursor to the definition of the symbol under the cursor \
     by issuing an LSP `textDocument/definition` request. Multi-file targets \
     open the destination file in the focused pane before jumping. Multiple \
     candidates jump to the first; no-op when the server returns nothing or \
     does not advertise the capability.",
    ActionPriority::Common
);

define_action!(
    GotoTypeDefinitionDef,
    GotoTypeDefinition,
    "GotoTypeDefinition",
    ActionKind::GotoTypeDefinition,
    "jump to type definition",
    "Move the primary cursor to the type definition of the symbol under the \
     cursor by issuing an LSP `textDocument/typeDefinition` request. \
     Multi-file targets open the destination file in the focused pane before \
     jumping. Multiple candidates jump to the first; no-op when the server \
     returns nothing or does not advertise the capability.",
    ActionPriority::Common
);
