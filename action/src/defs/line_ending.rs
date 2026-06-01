use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    OpenLineEndingPickerDef,
    OpenLineEndingPicker,
    "OpenLineEndingPicker",
    ActionKind::OpenLineEndingPicker,
    "open line-ending picker",
    "Open a picker to change the active buffer's line endings between \
     LF, CRLF, and CR. Confirm rewrites every line ending in the buffer.",
    ActionPriority::Common
);
