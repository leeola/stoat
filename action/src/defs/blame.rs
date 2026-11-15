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

define_action!(
    BlameCardDef,
    BlameCard,
    "BlameCard",
    ActionKind::BlameCard,
    "show the commit of the cursor line",
    "Open the Blame Card under the cursor line of a blamed pane. The card \
     names the commit that last changed the line: its sha and subject, its \
     author, date, parent, and change size, the first paragraph of its \
     message, and the keys that act on it. An uncommitted line has no card.",
    ActionPriority::Rare,
    palette_visible = false
);

define_action!(
    BlameCardCloseDef,
    BlameCardClose,
    "BlameCardClose",
    ActionKind::BlameCardClose,
    "close the Blame Card",
    "Close the Blame Card. The card also closes when the cursor leaves its \
     line, the pane stops blaming, the focus moves, or a modal opens.",
    ActionPriority::Rare,
    palette_visible = false
);

define_action!(
    BlameCopyShaDef,
    BlameCopySha,
    "BlameCopySha",
    ActionKind::BlameCopySha,
    "copy the commit's sha",
    "Copy the full sha of the commit that last changed the cursor line of a \
     blamed pane to the system clipboard. The registers keep what they hold. \
     An uncommitted line has no commit to copy.",
    ActionPriority::Rare,
    palette_visible = false
);
