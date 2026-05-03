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

define_action!(
    GotoImplementationDef,
    GotoImplementation,
    "GotoImplementation",
    ActionKind::GotoImplementation,
    "jump to symbol implementation",
    "Move the primary cursor to an implementation of the symbol under the \
     cursor by issuing an LSP `textDocument/implementation` request. \
     Multi-file targets open the destination file in the focused pane before \
     jumping. Multiple candidates jump to the first; no-op when the server \
     returns nothing or does not advertise the capability.",
    ActionPriority::Common
);

define_action!(
    HoverDef,
    Hover,
    "Hover",
    ActionKind::Hover,
    "show hover documentation",
    "Issue an LSP `textDocument/hover` request for the symbol under the \
     focused editor's primary cursor and show the response as a \
     cursor-anchored popup. The popup persists until the next motion or \
     action; no-op when the server does not advertise the capability or \
     returns nothing.",
    ActionPriority::Common
);

define_action!(
    CodeActionDef,
    CodeAction,
    "CodeAction",
    ActionKind::CodeAction,
    "show code actions for selection",
    "Issue an LSP `textDocument/codeAction` request for the focused \
     editor's primary selection and present the response as a numbered \
     popup. Number keys 1-9 select an action; on select the resulting \
     `WorkspaceEdit` is applied to the workspace, calling \
     `codeAction/resolve` first when the action's edit is deferred. \
     No-op when the server does not advertise the capability or returns \
     no actionable items.",
    ActionPriority::Common
);

define_action!(
    RenameSymbolDef,
    RenameSymbol,
    "RenameSymbol",
    ActionKind::RenameSymbol,
    "rename symbol under cursor",
    "Issue an LSP `textDocument/prepareRename` for the symbol under the \
     cursor and open a one-line input modal seeded with the symbol's \
     text. Submitting the input fires `textDocument/rename` and applies \
     the resulting `WorkspaceEdit`. Escape cancels without applying. \
     No-op when the server does not advertise the capability.",
    ActionPriority::Common
);
