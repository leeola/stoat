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
    OpenDiagnosticsPickerDef,
    OpenDiagnosticsPicker,
    "OpenDiagnosticsPicker",
    ActionKind::OpenDiagnosticsPicker,
    "open the diagnostics picker for the focused buffer",
    "Open a modal listing every diagnostic in the focused buffer's diagnostic set. \
     Each row shows the diagnostic's line:column, severity glyph, and a message snippet; \
     selecting an entry collapses the cursor at that diagnostic's range start. \
     No-op when the focused pane is not an editor or the buffer has no diagnostics.",
    ActionPriority::Common
);

define_action!(
    OpenWorkspaceDiagnosticsPickerDef,
    OpenWorkspaceDiagnosticsPicker,
    "OpenWorkspaceDiagnosticsPicker",
    ActionKind::OpenWorkspaceDiagnosticsPicker,
    "open the diagnostics picker for the entire workspace",
    "Open a modal listing every (path, diagnostic) pair currently known to the workspace. \
     Each row shows the path, line:column, severity glyph, and a message snippet; \
     selecting an entry opens the target file in the focused pane and collapses the cursor \
     at the diagnostic's range start. No-op when no diagnostics are loaded.",
    ActionPriority::Common
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
    GotoDeclarationDef,
    GotoDeclaration,
    "GotoDeclaration",
    ActionKind::GotoDeclaration,
    "jump to symbol declaration",
    "Move the primary cursor to the declaration of the symbol under the cursor \
     by issuing an LSP `textDocument/declaration` request. Multi-file targets \
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

define_action!(
    OpenSymbolPickerDef,
    OpenSymbolPicker,
    "OpenSymbolPicker",
    ActionKind::OpenSymbolPicker,
    "show document symbols",
    "Issue an LSP `textDocument/documentSymbol` request for the focused \
     buffer and present the response as a numbered popup. Number keys \
     1-9 select a symbol; on select the cursor jumps to the symbol's \
     definition. No-op when the server does not advertise the \
     capability or returns no symbols.",
    ActionPriority::Common
);

define_action!(
    OpenWorkspaceSymbolPickerDef,
    OpenWorkspaceSymbolPicker,
    "OpenWorkspaceSymbolPicker",
    ActionKind::OpenWorkspaceSymbolPicker,
    "search workspace symbols",
    "Open a one-line input modal for a workspace-symbol query. \
     Submitting fires `workspace/symbol` and presents the response as \
     a numbered popup. Number keys 1-9 select a symbol; on select the \
     cursor opens the target file at the symbol's location. No-op when \
     the server does not advertise the capability.",
    ActionPriority::Common
);

define_action!(
    FormatSelectionsDef,
    FormatSelections,
    "FormatSelections",
    ActionKind::FormatSelections,
    "format selection via LSP",
    "Issue an LSP `textDocument/rangeFormatting` request for the \
     focused editor's primary selection and apply the returned text \
     edits to the buffer. No-op when the server does not advertise \
     the formatting capability or returns no edits.",
    ActionPriority::Common
);

define_action!(
    FormatDef,
    Format,
    "Format",
    ActionKind::Format,
    "format document via LSP",
    "Issue an LSP `textDocument/formatting` request for the whole \
     focused document and apply the returned text edits to the \
     buffer. No-op when the server does not advertise the formatting \
     capability or returns no edits.",
    ActionPriority::Common
);
