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

define_action!(
    BlamePreviewDef,
    BlamePreview,
    "BlamePreview",
    ActionKind::BlamePreview,
    "preview the commit's patch",
    "Open the Commit Preview on the commit that last changed the cursor line \
     of a blamed pane. The preview is a read-only overlay with the commit's \
     header and its patch against its first parent, built from git objects, \
     so the working tree, HEAD, and the buffers stay as they were. An \
     uncommitted line opens the working-tree diff instead.",
    ActionPriority::Rare,
    palette_visible = false
);

define_action!(
    BlameCheckoutDef,
    BlameCheckout,
    "BlameCheckout",
    ActionKind::BlameCheckout,
    "check the commit out",
    "Check out the commit that last changed the cursor line of a blamed pane, \
     detached, and open the changes view on that line against the commit's \
     parent. The checkout is a review walk of one commit, so :done returns. \
     Over uncommitted changes it asks first and offers to stash them. Unsaved \
     buffers, an open walk, and a rebase refuse it.",
    ActionPriority::Rare,
    palette_visible = false
);

define_action!(
    BlameParentDef,
    BlameParent,
    "BlameParent",
    ActionKind::BlameParent,
    "blame the file at the commit's parent",
    "Show the file of the cursor line's commit as it stood at the commit's \
     first parent, read-only, with the cursor on the line that matches. The \
     pane keeps its blame, so a second press goes one commit further, and the \
     jump back returns. From the Commit Preview it closes the preview first. A \
     root commit and a file the parent does not hold set the status instead.",
    ActionPriority::Rare,
    palette_visible = false
);

define_action!(
    CheckoutConfirmAcceptDef,
    CheckoutConfirmAccept,
    "CheckoutConfirmAccept",
    ActionKind::CheckoutConfirmAccept,
    "stash and check out",
    "Stash the working tree's changes, the untracked files with them, and \
     check out the commit the checkout confirm names. A checkout that fails \
     puts the changes back.",
    ActionPriority::Rare,
    palette_visible = false
);

define_action!(
    CheckoutConfirmCancelDef,
    CheckoutConfirmCancel,
    "CheckoutConfirmCancel",
    ActionKind::CheckoutConfirmCancel,
    "cancel the checkout",
    "Close the checkout confirm with no checkout and no stash, and bring back \
     the Commit Preview it opened over.",
    ActionPriority::Rare,
    palette_visible = false
);

define_action!(
    CommitPreviewCloseDef,
    CommitPreviewClose,
    "CommitPreviewClose",
    ActionKind::CommitPreviewClose,
    "close the Commit Preview",
    "Close the Commit Preview and return to the blamed pane it opened from.",
    ActionPriority::Rare,
    palette_visible = false
);

define_action!(
    CommitPreviewLineDownDef,
    CommitPreviewLineDown,
    "CommitPreviewLineDown",
    ActionKind::CommitPreviewLineDown,
    "scroll the Commit Preview down",
    "Scroll the patch of the Commit Preview down one row. The scroll stops \
     where the last row of the patch reaches the bottom of the box.",
    ActionPriority::Rare,
    palette_visible = false
);

define_action!(
    CommitPreviewLineUpDef,
    CommitPreviewLineUp,
    "CommitPreviewLineUp",
    ActionKind::CommitPreviewLineUp,
    "scroll the Commit Preview up",
    "Scroll the patch of the Commit Preview up one row. The scroll stops at \
     the first row of the patch.",
    ActionPriority::Rare,
    palette_visible = false
);

define_action!(
    CommitPreviewNextFileDef,
    CommitPreviewNextFile,
    "CommitPreviewNextFile",
    ActionKind::CommitPreviewNextFile,
    "go to the next file in the Commit Preview",
    "Scroll the patch of the Commit Preview to the header of the next file. \
     At the last file the scroll stays, and the status says so.",
    ActionPriority::Rare,
    palette_visible = false
);

define_action!(
    CommitPreviewPrevFileDef,
    CommitPreviewPrevFile,
    "CommitPreviewPrevFile",
    ActionKind::CommitPreviewPrevFile,
    "go to the previous file in the Commit Preview",
    "Scroll the patch of the Commit Preview to the nearest file header above \
     the top row. At the first file the scroll stays, and the status says so.",
    ActionPriority::Rare,
    palette_visible = false
);

define_action!(
    CommitPreviewParentDef,
    CommitPreviewParent,
    "CommitPreviewParent",
    ActionKind::CommitPreviewParent,
    "preview against the next parent",
    "Show the patch of the Commit Preview against the next parent of a merge, \
     and go back to the first parent after the last. A commit with one parent \
     or none keeps its patch, and the status says so.",
    ActionPriority::Rare,
    palette_visible = false
);
