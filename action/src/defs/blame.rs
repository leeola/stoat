use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    ToggleBlameDef,
    ToggleBlame,
    "ToggleBlame",
    ActionKind::ToggleBlame,
    "show or hide git blame",
    "Show or hide the Blame Column of the focused pane. The column names the \
     commit that last changed each line, and a line that differs from HEAD \
     reads uncommitted. The pane keeps the column across the files it opens.",
    ActionPriority::Common,
    command_name = "git blame"
);
