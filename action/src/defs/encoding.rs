use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    OpenEncodingPickerDef,
    OpenEncodingPicker,
    "encoding",
    ActionKind::OpenEncodingPicker,
    "open encoding picker",
    "Open a picker to re-decode the active buffer with a different \
     character encoding. Confirm re-reads the file and replaces its \
     contents; a lossy decode is flagged with a warning.",
    ActionPriority::Common
);
