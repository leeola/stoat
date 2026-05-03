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
