use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    OpenGotoLineModalDef,
    OpenGotoLineModal,
    "goto-line",
    ActionKind::OpenGotoLineModal,
    "open go-to-line modal",
    "Open a modal that takes a line number and previews the destination \
     row live as you type. Enter keeps the previewed cursor; Escape \
     restores the original cursor row.",
    ActionPriority::Common
);
