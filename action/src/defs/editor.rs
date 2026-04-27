use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    AddSelectionBelowDef,
    AddSelectionBelow,
    "AddSelectionBelow",
    ActionKind::AddSelectionBelow,
    "add cursor below",
    "Add a new cursor on the line below the newest cursor.",
    ActionPriority::Rare
);

define_action!(
    AddSelectionAboveDef,
    AddSelectionAbove,
    "AddSelectionAbove",
    ActionKind::AddSelectionAbove,
    "add cursor above",
    "Add a new cursor on the line above the newest cursor.",
    ActionPriority::Rare
);

define_action!(
    SplitSelectionOnNewlineDef,
    SplitSelectionOnNewline,
    "SplitSelectionOnNewline",
    ActionKind::SplitSelectionOnNewline,
    "split selections on newlines",
    "Split each multi-line selection at newline boundaries so every covered line becomes its own selection. Selections without newlines and empty selections are kept as-is.",
    ActionPriority::Rare
);

define_action!(
    AlignSelectionsDef,
    AlignSelections,
    "AlignSelections",
    ActionKind::AlignSelections,
    "align selections in column",
    "Insert spaces at the start of each selection so every selection's head sits in the same display column. Selections that span multiple display rows are rejected and the action is a no-op. When several selections live on the same line, the n-th selection on each line aligns with the n-th selection on every other line.",
    ActionPriority::Rare
);

define_action!(
    IncrementDef,
    Increment,
    "Increment",
    ActionKind::Increment,
    "increment number under cursor",
    "Increment the decimal number under the primary cursor by one. The cursor must rest on a digit; a leading `-` is included only when the dash is preceded by whitespace, line start, or non-word punctuation. Operates on the primary cursor only.",
    ActionPriority::Rare
);

define_action!(
    DecrementDef,
    Decrement,
    "Decrement",
    ActionKind::Decrement,
    "decrement number under cursor",
    "Decrement the decimal number under the primary cursor by one. The cursor must rest on a digit; a leading `-` is included only when the dash is preceded by whitespace, line start, or non-word punctuation. Operates on the primary cursor only.",
    ActionPriority::Rare
);

define_action!(
    DeleteSelectionDef,
    DeleteSelection,
    "DeleteSelection",
    ActionKind::DeleteSelection,
    "delete selected text",
    "Delete the contents of every non-empty selection and collapse each to a cursor at the deletion start. Cursor-only selections (empty ranges) are left as-is.",
    ActionPriority::Rare
);

define_action!(
    UndoDef,
    Undo,
    "Undo",
    ActionKind::Undo,
    "undo last edit",
    "Reverse the most recent edit on the focused buffer. Repeat to walk further back through edit history; no-ops once history is empty. Anchor-based selections re-validate against the post-undo snapshot.",
    ActionPriority::Common
);

define_action!(
    RedoDef,
    Redo,
    "Redo",
    ActionKind::Redo,
    "redo last undone edit",
    "Re-apply the most recently undone edit on the focused buffer. Repeat to walk forward through the redo stack; no-ops once it is empty. Any new edit clears the redo stack per standard undo/redo semantics.",
    ActionPriority::Common
);

define_action!(
    IndentSelectionDef,
    IndentSelection,
    "IndentSelection",
    ActionKind::IndentSelection,
    "indent selected lines",
    "Insert a tab character at the start of every line covered by any selection. Multi-cursor scope: each distinct row receives the indent at most once.",
    ActionPriority::Rare
);

define_action!(
    UnindentSelectionDef,
    UnindentSelection,
    "UnindentSelection",
    ActionKind::UnindentSelection,
    "unindent selected lines",
    "Remove one indent level from the start of every line covered by any selection. Removes a leading tab if present, otherwise up to four leading spaces. Lines without leading whitespace are left untouched.",
    ActionPriority::Rare
);

define_action!(
    MoveLeftDef,
    MoveLeft,
    "MoveLeft",
    ActionKind::MoveLeft,
    "move cursor left",
    "Move every cursor one grapheme to the left and collapse any selection.",
    ActionPriority::Rare
);

define_action!(
    MoveRightDef,
    MoveRight,
    "MoveRight",
    ActionKind::MoveRight,
    "move cursor right",
    "Move every cursor one grapheme to the right and collapse any selection.",
    ActionPriority::Rare
);

define_action!(
    MoveUpDef,
    MoveUp,
    "MoveUp",
    ActionKind::MoveUp,
    "move cursor up",
    "Move every cursor one display line up, preserving the goal column.",
    ActionPriority::Rare
);

define_action!(
    MoveDownDef,
    MoveDown,
    "MoveDown",
    ActionKind::MoveDown,
    "move cursor down",
    "Move every cursor one display line down, preserving the goal column.",
    ActionPriority::Rare
);

define_action!(
    PageUpDef,
    PageUp,
    "PageUp",
    ActionKind::PageUp,
    "move cursor up one page",
    "Move the cursor up by the focused editor's viewport height and scroll the view by the same amount, keeping the cursor at the same relative screen row.",
    ActionPriority::Rare
);

define_action!(
    PageDownDef,
    PageDown,
    "PageDown",
    ActionKind::PageDown,
    "move cursor down one page",
    "Move the cursor down by the focused editor's viewport height and scroll the view by the same amount, keeping the cursor at the same relative screen row.",
    ActionPriority::Rare
);

define_action!(
    HalfPageUpDef,
    HalfPageUp,
    "HalfPageUp",
    ActionKind::HalfPageUp,
    "move cursor up half a page",
    "Move the cursor up by half the focused editor's viewport height (rounded up) and scroll the view by the same amount.",
    ActionPriority::Rare
);

define_action!(
    HalfPageDownDef,
    HalfPageDown,
    "HalfPageDown",
    ActionKind::HalfPageDown,
    "move cursor down half a page",
    "Move the cursor down by half the focused editor's viewport height (rounded up) and scroll the view by the same amount.",
    ActionPriority::Rare
);

define_action!(
    MoveNextWordStartDef,
    MoveNextWordStart,
    "MoveNextWordStart",
    ActionKind::MoveNextWordStart,
    "select to next word start",
    "Select from each cursor head to the start of the next word.",
    ActionPriority::Rare
);

define_action!(
    MoveNextWordEndDef,
    MoveNextWordEnd,
    "MoveNextWordEnd",
    ActionKind::MoveNextWordEnd,
    "select to next word end",
    "Select from each cursor head to the end of the next word.",
    ActionPriority::Rare
);

define_action!(
    MovePrevWordStartDef,
    MovePrevWordStart,
    "MovePrevWordStart",
    ActionKind::MovePrevWordStart,
    "select to previous word start",
    "Select backward from each cursor head to the start of the previous word.",
    ActionPriority::Rare
);

define_action!(
    MovePrevWordEndDef,
    MovePrevWordEnd,
    "MovePrevWordEnd",
    ActionKind::MovePrevWordEnd,
    "select to previous word end",
    "Select backward from each cursor head to the end of the previous word.",
    ActionPriority::Rare
);

define_action!(
    MoveNextLongWordStartDef,
    MoveNextLongWordStart,
    "MoveNextLongWordStart",
    ActionKind::MoveNextLongWordStart,
    "select to next long-word start",
    "Select from each cursor head to the start of the next long word. Long words are runs of non-whitespace characters; punctuation does not split them.",
    ActionPriority::Rare
);

define_action!(
    MoveNextLongWordEndDef,
    MoveNextLongWordEnd,
    "MoveNextLongWordEnd",
    ActionKind::MoveNextLongWordEnd,
    "select to next long-word end",
    "Select from each cursor head to the end of the next long word. Long words are runs of non-whitespace characters; punctuation does not split them.",
    ActionPriority::Rare
);

define_action!(
    MovePrevLongWordStartDef,
    MovePrevLongWordStart,
    "MovePrevLongWordStart",
    ActionKind::MovePrevLongWordStart,
    "select to previous long-word start",
    "Select backward from each cursor head to the start of the previous long word. Long words are runs of non-whitespace characters; punctuation does not split them.",
    ActionPriority::Rare
);

define_action!(
    MovePrevLongWordEndDef,
    MovePrevLongWordEnd,
    "MovePrevLongWordEnd",
    ActionKind::MovePrevLongWordEnd,
    "select to previous long-word end",
    "Select backward from each cursor head to the end of the previous long word. Long words are runs of non-whitespace characters; punctuation does not split them.",
    ActionPriority::Rare
);

define_action!(
    ExtendLeftDef,
    ExtendLeft,
    "ExtendLeft",
    ActionKind::ExtendLeft,
    "extend selection left",
    "Move every cursor head one grapheme left, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    ExtendRightDef,
    ExtendRight,
    "ExtendRight",
    ActionKind::ExtendRight,
    "extend selection right",
    "Move every cursor head one grapheme right, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    ExtendUpDef,
    ExtendUp,
    "ExtendUp",
    ActionKind::ExtendUp,
    "extend selection up",
    "Move every cursor head one display line up, keeping the tail fixed and preserving the goal column.",
    ActionPriority::Rare
);

define_action!(
    ExtendDownDef,
    ExtendDown,
    "ExtendDown",
    ActionKind::ExtendDown,
    "extend selection down",
    "Move every cursor head one display line down, keeping the tail fixed and preserving the goal column.",
    ActionPriority::Rare
);

define_action!(
    ExtendNextWordStartDef,
    ExtendNextWordStart,
    "ExtendNextWordStart",
    ActionKind::ExtendNextWordStart,
    "extend selection to next word start",
    "Extend each selection's head to the start of the next word, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    ExtendNextWordEndDef,
    ExtendNextWordEnd,
    "ExtendNextWordEnd",
    ActionKind::ExtendNextWordEnd,
    "extend selection to next word end",
    "Extend each selection's head to the end of the next word, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    ExpandSelectionDef,
    ExpandSelection,
    "ExpandSelection",
    ActionKind::ExpandSelection,
    "expand selection to enclosing syntax node",
    "Expand the primary selection to the smallest tree-sitter node that strictly contains it. If the selection already equals that node, walk to the parent. No-op when the buffer has no syntax tree (plain text or unparseable file types). Primary cursor only; root syntax layer only.",
    ActionPriority::Rare
);

define_action!(
    ShrinkSelectionDef,
    ShrinkSelection,
    "ShrinkSelection",
    ActionKind::ShrinkSelection,
    "shrink selection along the expand chain",
    "Pop the most recent expand step and restore the selection to its prior range. No-op when no expand has run since the chain was last cleared. The chain clears when the user moves the selection off the expand path (next expand starts a fresh stack).",
    ActionPriority::Rare
);

define_action!(
    SelectNextSiblingDef,
    SelectNextSibling,
    "SelectNextSibling",
    ActionKind::SelectNextSibling,
    "select next syntax sibling",
    "Set the primary selection to the next named tree-sitter sibling of the smallest node containing the current selection. Anonymous tokens (punctuation, keywords) are skipped. No-op when the buffer has no syntax tree or the current node has no next sibling.",
    ActionPriority::Rare
);

define_action!(
    SelectPrevSiblingDef,
    SelectPrevSibling,
    "SelectPrevSibling",
    ActionKind::SelectPrevSibling,
    "select previous syntax sibling",
    "Set the primary selection to the previous named tree-sitter sibling of the smallest node containing the current selection. Anonymous tokens (punctuation, keywords) are skipped. No-op when the buffer has no syntax tree or the current node has no previous sibling.",
    ActionPriority::Rare
);

define_action!(
    MoveParentNodeStartDef,
    MoveParentNodeStart,
    "MoveParentNodeStart",
    ActionKind::MoveParentNodeStart,
    "move cursor to parent node start",
    "Collapse the primary selection to a cursor at the start byte of the enclosing tree-sitter node's parent. No-op when the buffer has no syntax tree or the current node is at the root.",
    ActionPriority::Rare
);

define_action!(
    MoveParentNodeEndDef,
    MoveParentNodeEnd,
    "MoveParentNodeEnd",
    ActionKind::MoveParentNodeEnd,
    "move cursor to parent node end",
    "Collapse the primary selection to a cursor at the end byte of the enclosing tree-sitter node's parent. No-op when the buffer has no syntax tree or the current node is at the root.",
    ActionPriority::Rare
);

define_action!(
    SaveSelectionDef,
    SaveSelection,
    "SaveSelection",
    ActionKind::SaveSelection,
    "save current position to jumplist",
    "Push the primary selection's start byte offset onto the focused editor's jumplist. Truncates any forward history (anything reachable via JumpForward) before pushing.",
    ActionPriority::Rare
);

define_action!(
    JumpBackwardDef,
    JumpBackward,
    "JumpBackward",
    ActionKind::JumpBackward,
    "jump backward in jumplist",
    "Walk one entry backward through the focused editor's jumplist and collapse the primary selection to a cursor at that byte offset. No-op when at the start of the list.",
    ActionPriority::Rare
);

define_action!(
    JumpForwardDef,
    JumpForward,
    "JumpForward",
    ActionKind::JumpForward,
    "jump forward in jumplist",
    "Walk one entry forward through the focused editor's jumplist and collapse the primary selection to a cursor at that byte offset. No-op when at the end of the list.",
    ActionPriority::Rare
);

define_action!(
    ExtendPrevWordStartDef,
    ExtendPrevWordStart,
    "ExtendPrevWordStart",
    ActionKind::ExtendPrevWordStart,
    "extend selection to previous word start",
    "Extend each selection's head backward to the start of the previous word, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    ExtendPrevWordEndDef,
    ExtendPrevWordEnd,
    "ExtendPrevWordEnd",
    ActionKind::ExtendPrevWordEnd,
    "extend selection to previous word end",
    "Extend each selection's head backward to the end of the previous word, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    GotoLineStartDef,
    GotoLineStart,
    "GotoLineStart",
    ActionKind::GotoLineStart,
    "goto line start",
    "Collapse every selection to column 0 of the line containing its cursor head.",
    ActionPriority::Rare
);

define_action!(
    GotoLineEndDef,
    GotoLineEnd,
    "GotoLineEnd",
    ActionKind::GotoLineEnd,
    "goto line end",
    "Collapse every selection to the end of the line containing its cursor head (just before the trailing newline).",
    ActionPriority::Rare
);

define_action!(
    GotoFirstNonwhitespaceDef,
    GotoFirstNonwhitespace,
    "GotoFirstNonwhitespace",
    ActionKind::GotoFirstNonwhitespace,
    "goto first nonwhitespace",
    "Collapse every selection to the first non-whitespace column of the line containing its cursor head; leaves the selection unchanged if the line is empty or only whitespace.",
    ActionPriority::Rare
);

define_action!(
    GotoFileStartDef,
    GotoFileStart,
    "GotoFileStart",
    ActionKind::GotoFileStart,
    "goto file start",
    "Collapse every selection to offset 0 of the focused buffer.",
    ActionPriority::Rare
);

define_action!(
    GotoLastLineDef,
    GotoLastLine,
    "GotoLastLine",
    ActionKind::GotoLastLine,
    "goto last line",
    "Collapse every selection to column 0 of the buffer's last line (falling back to the second-to-last line when the buffer ends with a trailing newline).",
    ActionPriority::Rare
);

define_action!(
    GotoLineNumberDef,
    GotoLineNumber,
    "GotoLineNumber",
    ActionKind::GotoLineNumber,
    "goto line number from count",
    "Jump to the start of the line numbered by the pending count prefix (1-indexed); falls back to the last line when no count is pending. Counts beyond the buffer length clamp to the last visible row.",
    ActionPriority::Rare
);

define_action!(
    GotoNextChangeDef,
    GotoNextChange,
    "GotoNextChange",
    ActionKind::GotoNextChange,
    "goto next change",
    "Move the primary cursor to the start line of the next diff hunk strictly after the cursor's row. No-op when the buffer has no diff map or no hunk lies after the cursor. Primary-cursor only.",
    ActionPriority::Rare
);

define_action!(
    GotoPrevChangeDef,
    GotoPrevChange,
    "GotoPrevChange",
    ActionKind::GotoPrevChange,
    "goto previous change",
    "Move the primary cursor to the start line of the previous diff hunk strictly before the cursor's row. No-op when the buffer has no diff map or no hunk lies before the cursor. Primary-cursor only.",
    ActionPriority::Rare
);

define_action!(
    GotoWindowTopDef,
    GotoWindowTop,
    "GotoWindowTop",
    ActionKind::GotoWindowTop,
    "goto window top",
    "Collapse every selection to column 0 of the topmost row currently visible in the focused editor's viewport. Does not scroll the view.",
    ActionPriority::Rare
);

define_action!(
    GotoWindowCenterDef,
    GotoWindowCenter,
    "GotoWindowCenter",
    ActionKind::GotoWindowCenter,
    "goto window center",
    "Collapse every selection to column 0 of the row at the vertical midpoint of the focused editor's viewport. Does not scroll the view.",
    ActionPriority::Rare
);

define_action!(
    GotoWindowBottomDef,
    GotoWindowBottom,
    "GotoWindowBottom",
    ActionKind::GotoWindowBottom,
    "goto window bottom",
    "Collapse every selection to column 0 of the bottommost row currently visible in the focused editor's viewport. Does not scroll the view.",
    ActionPriority::Rare
);

define_action!(
    AlignViewTopDef,
    AlignViewTop,
    "AlignViewTop",
    ActionKind::AlignViewTop,
    "align view top",
    "Scroll the focused editor so the cursor's row sits at the top of the viewport. Cursor position is unchanged.",
    ActionPriority::Rare
);

define_action!(
    AlignViewCenterDef,
    AlignViewCenter,
    "AlignViewCenter",
    ActionKind::AlignViewCenter,
    "align view center",
    "Scroll the focused editor so the cursor's row sits at the vertical midpoint of the viewport. Cursor position is unchanged.",
    ActionPriority::Rare
);

define_action!(
    AlignViewBottomDef,
    AlignViewBottom,
    "AlignViewBottom",
    ActionKind::AlignViewBottom,
    "align view bottom",
    "Scroll the focused editor so the cursor's row sits at the bottom of the viewport. Cursor position is unchanged.",
    ActionPriority::Rare
);

define_action!(
    ScrollUpDef,
    ScrollUp,
    "ScrollUp",
    ActionKind::ScrollUp,
    "scroll view up",
    "Scroll the focused editor up by one line. The cursor stays at its buffer position; pressing again brings the view back over it.",
    ActionPriority::Rare
);

define_action!(
    ScrollDownDef,
    ScrollDown,
    "ScrollDown",
    ActionKind::ScrollDown,
    "scroll view down",
    "Scroll the focused editor down by one line. The cursor stays at its buffer position; pressing again brings the view back over it.",
    ActionPriority::Rare
);

define_action!(
    SwitchCaseDef,
    SwitchCase,
    "SwitchCase",
    ActionKind::SwitchCase,
    "toggle case",
    "Toggle the case of every character in the primary selection: uppercase becomes lowercase and vice versa. Digits, punctuation, and non-letter characters pass through unchanged. Operates on the primary cursor only.",
    ActionPriority::Rare
);

define_action!(
    SwitchToUppercaseDef,
    SwitchToUppercase,
    "SwitchToUppercase",
    ActionKind::SwitchToUppercase,
    "uppercase selection",
    "Uppercase every character in the primary selection. Already-uppercase and non-letter characters pass through unchanged. Operates on the primary cursor only.",
    ActionPriority::Rare
);

define_action!(
    SwitchToLowercaseDef,
    SwitchToLowercase,
    "SwitchToLowercase",
    ActionKind::SwitchToLowercase,
    "lowercase selection",
    "Lowercase every character in the primary selection. Already-lowercase and non-letter characters pass through unchanged. Operates on the primary cursor only.",
    ActionPriority::Rare
);

define_action!(
    ExtendToLineStartDef,
    ExtendToLineStart,
    "ExtendToLineStart",
    ActionKind::ExtendToLineStart,
    "extend selection to line start",
    "Extend each selection's head to column 0 of the line containing its cursor head, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    ExtendToLineEndDef,
    ExtendToLineEnd,
    "ExtendToLineEnd",
    ActionKind::ExtendToLineEnd,
    "extend selection to line end",
    "Extend each selection's head to the end of the line containing its cursor head (just before the trailing newline), keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    ExtendToFileStartDef,
    ExtendToFileStart,
    "ExtendToFileStart",
    ActionKind::ExtendToFileStart,
    "extend selection to file start",
    "Extend each selection's head to offset 0 of the focused buffer, keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    ExtendToLastLineDef,
    ExtendToLastLine,
    "ExtendToLastLine",
    ActionKind::ExtendToLastLine,
    "extend selection to last line",
    "Extend each selection's head to column 0 of the buffer's last line (falling back to the second-to-last line when the buffer ends with a trailing newline), keeping the tail fixed.",
    ActionPriority::Rare
);

define_action!(
    CollapseSelectionDef,
    CollapseSelection,
    "CollapseSelection",
    ActionKind::CollapseSelection,
    "collapse selection",
    "Collapse every selection to its cursor head, leaving the cursor position unchanged.",
    ActionPriority::Rare
);

define_action!(
    FlipSelectionsDef,
    FlipSelections,
    "FlipSelections",
    ActionKind::FlipSelections,
    "flip selection anchors",
    "Swap head and anchor for every non-empty selection, keeping the range fixed while moving the cursor to the opposite end.",
    ActionPriority::Rare
);

define_action!(
    SelectAllDef,
    SelectAll,
    "SelectAll",
    ActionKind::SelectAll,
    "select all",
    "Replace every selection with a single selection spanning the entire focused buffer.",
    ActionPriority::Rare
);

define_action!(
    SelectLineBelowDef,
    SelectLineBelow,
    "SelectLineBelow",
    ActionKind::SelectLineBelow,
    "select line below",
    "Snap every selection to its containing lines; extend one line downward when the selection is already line-shaped.",
    ActionPriority::Rare
);

define_action!(
    KeepPrimarySelectionDef,
    KeepPrimarySelection,
    "KeepPrimarySelection",
    ActionKind::KeepPrimarySelection,
    "keep primary selection",
    "Discard every selection except the newest (primary) one.",
    ActionPriority::Rare
);

define_action!(
    RotateSelectionsForwardDef,
    RotateSelectionsForward,
    "RotateSelectionsForward",
    ActionKind::RotateSelectionsForward,
    "rotate primary selection forward",
    "Make the next selection (in offset order, wrapping at the end) the primary.",
    ActionPriority::Rare
);

define_action!(
    RotateSelectionsBackwardDef,
    RotateSelectionsBackward,
    "RotateSelectionsBackward",
    ActionKind::RotateSelectionsBackward,
    "rotate primary selection backward",
    "Make the previous selection (in offset order, wrapping at the start) the primary.",
    ActionPriority::Rare
);

define_action!(
    TrimSelectionsDef,
    TrimSelections,
    "TrimSelections",
    ActionKind::TrimSelections,
    "trim whitespace from selections",
    "Strip leading and trailing whitespace from every selection. Selections that become empty (or were entirely whitespace) are dropped; if all selections drop, collapse the primary to its head.",
    ActionPriority::Rare
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn kind_and_name() {
        assert_eq!(AddSelectionBelow.kind(), ActionKind::AddSelectionBelow);
        assert_eq!(AddSelectionBelow.def().name(), "AddSelectionBelow");
    }

    #[test]
    fn move_kinds_and_names() {
        assert_eq!(MoveLeft.kind(), ActionKind::MoveLeft);
        assert_eq!(MoveLeft.def().name(), "MoveLeft");
        assert_eq!(MoveRight.kind(), ActionKind::MoveRight);
        assert_eq!(MoveRight.def().name(), "MoveRight");
        assert_eq!(MoveUp.kind(), ActionKind::MoveUp);
        assert_eq!(MoveUp.def().name(), "MoveUp");
        assert_eq!(MoveDown.kind(), ActionKind::MoveDown);
        assert_eq!(MoveDown.def().name(), "MoveDown");
        assert_eq!(MoveNextWordStart.kind(), ActionKind::MoveNextWordStart);
        assert_eq!(MoveNextWordStart.def().name(), "MoveNextWordStart");
        assert_eq!(MoveNextWordEnd.kind(), ActionKind::MoveNextWordEnd);
        assert_eq!(MoveNextWordEnd.def().name(), "MoveNextWordEnd");
        assert_eq!(MovePrevWordStart.kind(), ActionKind::MovePrevWordStart);
        assert_eq!(MovePrevWordStart.def().name(), "MovePrevWordStart");
        assert_eq!(MovePrevWordEnd.kind(), ActionKind::MovePrevWordEnd);
        assert_eq!(MovePrevWordEnd.def().name(), "MovePrevWordEnd");
    }

    #[test]
    fn extend_kinds_and_names() {
        assert_eq!(ExtendLeft.kind(), ActionKind::ExtendLeft);
        assert_eq!(ExtendLeft.def().name(), "ExtendLeft");
        assert_eq!(ExtendRight.kind(), ActionKind::ExtendRight);
        assert_eq!(ExtendRight.def().name(), "ExtendRight");
        assert_eq!(ExtendUp.kind(), ActionKind::ExtendUp);
        assert_eq!(ExtendUp.def().name(), "ExtendUp");
        assert_eq!(ExtendDown.kind(), ActionKind::ExtendDown);
        assert_eq!(ExtendDown.def().name(), "ExtendDown");
        assert_eq!(ExtendNextWordStart.kind(), ActionKind::ExtendNextWordStart);
        assert_eq!(ExtendNextWordStart.def().name(), "ExtendNextWordStart");
        assert_eq!(ExtendNextWordEnd.kind(), ActionKind::ExtendNextWordEnd);
        assert_eq!(ExtendNextWordEnd.def().name(), "ExtendNextWordEnd");
        assert_eq!(ExtendPrevWordStart.kind(), ActionKind::ExtendPrevWordStart);
        assert_eq!(ExtendPrevWordStart.def().name(), "ExtendPrevWordStart");
        assert_eq!(ExtendPrevWordEnd.kind(), ActionKind::ExtendPrevWordEnd);
        assert_eq!(ExtendPrevWordEnd.def().name(), "ExtendPrevWordEnd");
    }

    #[test]
    fn goto_kinds_and_names() {
        assert_eq!(GotoLineStart.kind(), ActionKind::GotoLineStart);
        assert_eq!(GotoLineStart.def().name(), "GotoLineStart");
        assert_eq!(GotoLineEnd.kind(), ActionKind::GotoLineEnd);
        assert_eq!(GotoLineEnd.def().name(), "GotoLineEnd");
    }

    #[test]
    fn selection_primitive_kinds_and_names() {
        assert_eq!(CollapseSelection.kind(), ActionKind::CollapseSelection);
        assert_eq!(CollapseSelection.def().name(), "CollapseSelection");
        assert_eq!(FlipSelections.kind(), ActionKind::FlipSelections);
        assert_eq!(FlipSelections.def().name(), "FlipSelections");
        assert_eq!(SelectAll.kind(), ActionKind::SelectAll);
        assert_eq!(SelectAll.def().name(), "SelectAll");
        assert_eq!(SelectLineBelow.kind(), ActionKind::SelectLineBelow);
        assert_eq!(SelectLineBelow.def().name(), "SelectLineBelow");
        assert_eq!(
            KeepPrimarySelection.kind(),
            ActionKind::KeepPrimarySelection
        );
        assert_eq!(KeepPrimarySelection.def().name(), "KeepPrimarySelection");
        assert_eq!(
            RotateSelectionsForward.kind(),
            ActionKind::RotateSelectionsForward
        );
        assert_eq!(
            RotateSelectionsForward.def().name(),
            "RotateSelectionsForward"
        );
        assert_eq!(
            RotateSelectionsBackward.kind(),
            ActionKind::RotateSelectionsBackward
        );
        assert_eq!(
            RotateSelectionsBackward.def().name(),
            "RotateSelectionsBackward"
        );
        assert_eq!(TrimSelections.kind(), ActionKind::TrimSelections);
        assert_eq!(TrimSelections.def().name(), "TrimSelections");
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(AddSelectionBelow);
        assert!(action
            .as_any()
            .downcast_ref::<AddSelectionBelow>()
            .is_some());
        let action: Box<dyn Action> = Box::new(MoveLeft);
        assert!(action.as_any().downcast_ref::<MoveLeft>().is_some());
        let action: Box<dyn Action> = Box::new(ExtendLeft);
        assert!(action.as_any().downcast_ref::<ExtendLeft>().is_some());
    }
}
