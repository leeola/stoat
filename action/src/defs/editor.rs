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
    "Increment the decimal number at or after each cursor on its current line by one. When a cursor is on a digit the run of digits there is the target; otherwise the search walks forward to the first digit on the same line and uses the run that begins there. The scan never crosses a line ending. A leading `-` is included only when the dash is preceded by whitespace, line start, or non-word punctuation. Two cursors that find the same number share a single edit.",
    ActionPriority::Rare
);

define_action!(
    DecrementDef,
    Decrement,
    "Decrement",
    ActionKind::Decrement,
    "decrement number under cursor",
    "Decrement the decimal number at or after each cursor on its current line by one. When a cursor is on a digit the run of digits there is the target; otherwise the search walks forward to the first digit on the same line and uses the run that begins there. The scan never crosses a line ending. A leading `-` is included only when the dash is preceded by whitespace, line start, or non-word punctuation. Two cursors that find the same number share a single edit.",
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
    CommitUndoCheckpointDef,
    CommitUndoCheckpoint,
    "CommitUndoCheckpoint",
    ActionKind::CommitUndoCheckpoint,
    "place undo checkpoint",
    "Place a named checkpoint marker at the current position on the focused buffer's op log. Subsequent checkpoint-navigation actions can target this marker. Stoat treats every edit as its own undo unit, so this records a label rather than committing in-progress changes.",
    ActionPriority::Rare
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
    ToggleCommentsDef,
    ToggleComments,
    "ToggleComments",
    ActionKind::ToggleComments,
    "toggle line comments",
    "Toggle the line-comment prefix on every line touched by any selection. The prefix is the language's `line_comment` (e.g. `//` for rust, `#` for toml); buffers whose language has none are a no-op. Each line decides independently: if its first non-whitespace run is the prefix (followed by a space or end-of-content), the prefix and one trailing space are removed; otherwise the prefix and a single space are inserted at the first non-whitespace position. Empty / whitespace-only lines are skipped.",
    ActionPriority::Rare
);

define_action!(
    ToggleBlameDef,
    ToggleBlame,
    "ToggleBlame",
    ActionKind::ToggleBlame,
    "toggle blame strip",
    "Toggle the blame strip on the active editor's gutter. When visible, the strip shows the short sha, first-name of the author, and short relative age for the commit that last touched each source line. Toggling on triggers a refresh against the workspace's git host; toggling off hides the strip but keeps the cached entries until the next edit invalidates them. Buffers without an on-disk path (scratch, modal inputs) are a no-op.",
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
    SelectAllSiblingsDef,
    SelectAllSiblings,
    "SelectAllSiblings",
    ActionKind::SelectAllSiblings,
    "select every named sibling of the current node",
    "Replace each selection with one selection per named tree-sitter sibling under the deepest ancestor that has more than one child. Single-child wrapper nodes are skipped so trivial AST shells do not collapse the result. No-op when the buffer has no syntax tree.",
    ActionPriority::Rare
);

define_action!(
    SelectAllChildrenDef,
    SelectAllChildren,
    "SelectAllChildren",
    ActionKind::SelectAllChildren,
    "select every named child of the current node",
    "Replace each selection with one selection per named tree-sitter child of the smallest node containing the selection. Anonymous tokens (punctuation, keywords) are skipped. No-op when the buffer has no syntax tree, and selections over childless nodes are left untouched.",
    ActionPriority::Rare
);

define_action!(
    ExtendSelectNextSiblingDef,
    ExtendSelectNextSibling,
    "ExtendSelectNextSibling",
    ActionKind::ExtendSelectNextSibling,
    "extend to next syntax sibling",
    "Like `SelectNextSibling` but extends the primary selection rather than collapsing it. The selection's tail stays put while the head moves to the next named sibling's range.",
    ActionPriority::Rare
);

define_action!(
    ExtendSelectPrevSiblingDef,
    ExtendSelectPrevSibling,
    "ExtendSelectPrevSibling",
    ActionKind::ExtendSelectPrevSibling,
    "extend to previous syntax sibling",
    "Like `SelectPrevSibling` but extends the primary selection rather than collapsing it. The selection's tail stays put while the head moves to the previous named sibling's range.",
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
    ExtendMoveParentNodeStartDef,
    ExtendMoveParentNodeStart,
    "ExtendMoveParentNodeStart",
    ActionKind::ExtendMoveParentNodeStart,
    "extend to parent node start",
    "Like `MoveParentNodeStart` but extends the primary selection rather than collapsing it. The selection's tail stays put while the head moves to the parent node's start byte.",
    ActionPriority::Rare
);

define_action!(
    ExtendMoveParentNodeEndDef,
    ExtendMoveParentNodeEnd,
    "ExtendMoveParentNodeEnd",
    ActionKind::ExtendMoveParentNodeEnd,
    "extend to parent node end",
    "Like `MoveParentNodeEnd` but extends the primary selection rather than collapsing it. The selection's tail stays put while the head moves to the parent node's end byte.",
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
    OpenJumplistPickerDef,
    OpenJumplistPicker,
    "OpenJumplistPicker",
    ActionKind::OpenJumplistPicker,
    "open jumplist picker",
    "Open a modal listing every entry in the focused editor's jumplist with line:column and a snippet of the line content. Enter jumps to the selected entry; Esc cancels. No-op when the jumplist is empty.",
    ActionPriority::Common
);

define_action!(
    OpenLastPickerDef,
    OpenLastPicker,
    "OpenLastPicker",
    ActionKind::OpenLastPicker,
    "reopen the most recently opened picker",
    "Re-fire the action that last opened a picker (file finder, command palette, jumplist, diagnostics, etc.) so the user can resume browsing without remembering the original chord. The picker rebuilds fresh from current state -- prior query and selection are not restored. No-op when no picker has been opened in this session.",
    ActionPriority::Common
);

define_action!(
    OpenGlobalSearchDef,
    OpenGlobalSearch,
    "OpenGlobalSearch",
    ActionKind::OpenGlobalSearch,
    "open workspace regex search",
    "Open an input modal for a regex pattern. On submit, scan every workspace file for matches and present them as a picker listing path:line:column with a snippet of the matched line. Enter opens the selected match's file at the match offset; Esc cancels.",
    ActionPriority::Common
);

define_action!(
    SplitSelectionDef,
    SplitSelection,
    "SplitSelection",
    ActionKind::SplitSelection,
    "split each selection on regex matches",
    "Open an input modal for a regex pattern. On submit, split every existing selection at every match of the pattern: matches are removed and the parts between them become new sub-selections. Cursor (zero-width) selections pass through unchanged. Invalid regex is a silent no-op.",
    ActionPriority::Common
);

define_action!(
    KeepSelectionsDef,
    KeepSelections,
    "KeepSelections",
    ActionKind::KeepSelections,
    "keep selections matching regex",
    "Open an input modal for a regex pattern. On submit, keep only the selections whose contents match the pattern; drop the rest. If the filter would empty the selection set, leave selections unchanged. Invalid regex is a silent no-op.",
    ActionPriority::Common
);

define_action!(
    RemoveSelectionsDef,
    RemoveSelections,
    "RemoveSelections",
    ActionKind::RemoveSelections,
    "remove selections matching regex",
    "Open an input modal for a regex pattern. On submit, drop every selection whose contents match the pattern; keep the rest. If the filter would empty the selection set, leave selections unchanged. Invalid regex is a silent no-op.",
    ActionPriority::Common
);

define_action!(
    RecordMacroDef,
    RecordMacro,
    "RecordMacro",
    ActionKind::RecordMacro,
    "toggle macro recording",
    "First press starts recording every subsequent keypress into a macro register; second press stops and stores the captured keystrokes. The target register is the most recent SelectRegister chord, defaulting to the unnamed register.",
    ActionPriority::Common
);

define_action!(
    ReplayMacroDef,
    ReplayMacro,
    "ReplayMacro",
    ActionKind::ReplayMacro,
    "replay macro from register",
    "Arm a chord; the next char keypress names a register and replays the keystroke sequence stored there. No-op when the register has no recorded macro.",
    ActionPriority::Common
);

define_action!(
    ShellPipeDef,
    ShellPipe,
    "ShellPipe",
    ActionKind::ShellPipe,
    "pipe selections through a shell command",
    "Open an input modal for a shell command. On submit, run the command once per selection with the selection's text as stdin and replace each selection with the command's stdout. Empty selections pass through unchanged.",
    ActionPriority::Common
);

define_action!(
    ShellPipeToDef,
    ShellPipeTo,
    "ShellPipeTo",
    ActionKind::ShellPipeTo,
    "pipe selections through a shell command and discard output",
    "Open an input modal for a shell command. On submit, run the command once per selection with the selection's text as stdin; ignore the output. Selections are unchanged. Used for side-effect commands.",
    ActionPriority::Common
);

define_action!(
    ShellInsertOutputDef,
    ShellInsertOutput,
    "ShellInsertOutput",
    ActionKind::ShellInsertOutput,
    "insert shell command output at every cursor",
    "Open an input modal for a shell command. On submit, run the command once with empty stdin and insert its stdout at every selection's cursor.",
    ActionPriority::Common
);

define_action!(
    ShellAppendOutputDef,
    ShellAppendOutput,
    "ShellAppendOutput",
    ActionKind::ShellAppendOutput,
    "append shell command output after every selection",
    "Open an input modal for a shell command. On submit, run the command once with empty stdin and append its stdout after the end of every selection.",
    ActionPriority::Common
);

define_action!(
    ShellKeepPipeDef,
    ShellKeepPipe,
    "ShellKeepPipe",
    ActionKind::ShellKeepPipe,
    "keep selections whose shell command exits zero",
    "Open an input modal for a shell command. On submit, run the command once per selection with that selection's text as stdin; keep only selections whose exit code is zero. Empty result leaves selections unchanged.",
    ActionPriority::Common
);

define_action!(
    SaveBufferDef,
    SaveBuffer,
    "SaveBuffer",
    ActionKind::SaveBuffer,
    "save the focused buffer to disk",
    "Write the focused buffer's rope text to its backing file via FsHost, clear the buffer's dirty flag, and notify the LSP server via did_save. No-op for scratch buffers (no path).",
    ActionPriority::Common
);

define_action!(
    CloseBufferDef,
    CloseBuffer,
    "CloseBuffer",
    ActionKind::CloseBuffer,
    "close the focused buffer",
    "Drop the focused buffer from the workspace's BufferRegistry and notify the LSP server via did_close. Editor panes that displayed the closed buffer are rebound to fresh scratch buffers so the pane layout stays coherent. Refuses to close when the buffer is dirty; save first.",
    ActionPriority::Common
);

define_action!(
    AcceptCompletionDef,
    AcceptCompletion,
    "AcceptCompletion",
    ActionKind::AcceptCompletion,
    "accept the highlighted completion popup item",
    "Replace the highlighted item's replace_range in the focused buffer with its insert_text and place the primary cursor at the inserted end. Clears the completion popup and the in-flight request. No-op when no popup is showing.",
    ActionPriority::Common
);

define_action!(
    SmartTabDef,
    SmartTab,
    "SmartTab",
    ActionKind::SmartTab,
    "smart Tab in insert mode",
    "Arbitrate the Tab key in insert mode: advance the active snippet placeholder if one is in flight, accept the highlighted completion popup item if the popup is open, otherwise insert a tab character when the cursor follows only whitespace on the current line. No-op when none of those conditions hold.",
    ActionPriority::Common
);

define_action!(
    TriggerCompletionDef,
    TriggerCompletion,
    "TriggerCompletion",
    ActionKind::TriggerCompletion,
    "manually trigger the completion popup",
    "Force a completion request even when the buffer signature has not changed since the last fetch. Bypasses the dedup guard that suppresses redundant triggers during typing. No-op outside insert mode in an editor pane.",
    ActionPriority::Common
);

define_action!(
    FindNextCharDef,
    FindNextChar,
    "FindNextChar",
    ActionKind::FindNextChar,
    "find next char on line",
    "Wait for the next char keypress, then jump the primary cursor forward to the next occurrence of that char on the current line. Cursor lands on the matched char. No-op when the char does not appear after the cursor.",
    ActionPriority::Rare
);

define_action!(
    FindPrevCharDef,
    FindPrevChar,
    "FindPrevChar",
    ActionKind::FindPrevChar,
    "find previous char on line",
    "Wait for the next char keypress, then jump the primary cursor backward to the previous occurrence of that char on the current line. Cursor lands on the matched char. No-op when the char does not appear before the cursor.",
    ActionPriority::Rare
);

define_action!(
    TillNextCharDef,
    TillNextChar,
    "TillNextChar",
    ActionKind::TillNextChar,
    "till next char on line",
    "Wait for the next char keypress, then jump the primary cursor forward to one position before the next occurrence of that char on the current line. No-op when the char does not appear after the cursor.",
    ActionPriority::Rare
);

define_action!(
    TillPrevCharDef,
    TillPrevChar,
    "TillPrevChar",
    ActionKind::TillPrevChar,
    "till previous char on line",
    "Wait for the next char keypress, then jump the primary cursor backward to one position after the previous occurrence of that char on the current line. No-op when the char does not appear before the cursor.",
    ActionPriority::Rare
);

define_action!(
    ExtendFindNextCharDef,
    ExtendFindNextChar,
    "ExtendFindNextChar",
    ActionKind::ExtendFindNextChar,
    "extend to next char on line",
    "Like `FindNextChar` but extends the primary selection rather than collapsing it. The selection's tail stays put while the head moves to the matched char.",
    ActionPriority::Rare
);

define_action!(
    ExtendFindPrevCharDef,
    ExtendFindPrevChar,
    "ExtendFindPrevChar",
    ActionKind::ExtendFindPrevChar,
    "extend to previous char on line",
    "Like `FindPrevChar` but extends the primary selection rather than collapsing it.",
    ActionPriority::Rare
);

define_action!(
    ExtendTillNextCharDef,
    ExtendTillNextChar,
    "ExtendTillNextChar",
    ActionKind::ExtendTillNextChar,
    "extend till next char on line",
    "Like `TillNextChar` but extends the primary selection rather than collapsing it.",
    ActionPriority::Rare
);

define_action!(
    ExtendTillPrevCharDef,
    ExtendTillPrevChar,
    "ExtendTillPrevChar",
    ActionKind::ExtendTillPrevChar,
    "extend till previous char on line",
    "Like `TillPrevChar` but extends the primary selection rather than collapsing it.",
    ActionPriority::Rare
);

define_action!(
    SetMarkDef,
    SetMark,
    "SetMark",
    ActionKind::SetMark,
    "set mark at cursor",
    "Wait for the next char keypress, then store the primary cursor's byte offset under that name in the focused buffer's mark table. Subsequent `GotoMark`/`GotoMarkExact` calls with the same char jump back to the stored position. Marks are buffer-local; later edits do not move the stored offset.",
    ActionPriority::Rare
);

define_action!(
    GotoMarkDef,
    GotoMark,
    "GotoMark",
    ActionKind::GotoMark,
    "goto mark line",
    "Wait for the next char keypress, then jump the primary cursor to the start of the line containing the named mark in the focused buffer. No-op when no mark with that name has been set in the focused buffer.",
    ActionPriority::Rare
);

define_action!(
    GotoMarkExactDef,
    GotoMarkExact,
    "GotoMarkExact",
    ActionKind::GotoMarkExact,
    "goto mark exact offset",
    "Wait for the next char keypress, then jump the primary cursor to the exact byte offset stored under the named mark in the focused buffer. No-op when no mark with that name has been set in the focused buffer.",
    ActionPriority::Rare
);

define_action!(
    SurroundAddDef,
    SurroundAdd,
    "SurroundAdd",
    ActionKind::SurroundAdd,
    "surround selection with pair",
    "Wait for the next char keypress, then wrap every non-empty selection with the matching pair: bracket-like opens/closes (`(`/`)`, `[`/`]`, `{`/`}`, `<`/`>`) wrap with the canonical open and close; quote-like chars (`\"`, `'`, `` ` ``) wrap with the same char on both sides; any other printable char wraps with that char on both sides. Empty (collapsed) selections are skipped.",
    ActionPriority::Rare
);

define_action!(
    SurroundReplaceDef,
    SurroundReplace,
    "SurroundReplace",
    ActionKind::SurroundReplace,
    "replace surrounding pair",
    "Wait for two char keypresses (`from` then `to`), then for every selection's cursor find the nearest enclosing `from` pair and replace its open/close with the canonical pair for `to`. Bracket-like chars use canonical opens/closes; symmetric chars (quotes, etc.) use the same char on both sides. No-op when the cursor is not enclosed by a `from` pair.",
    ActionPriority::Rare
);

define_action!(
    SurroundDeleteDef,
    SurroundDelete,
    "SurroundDelete",
    ActionKind::SurroundDelete,
    "delete surrounding pair",
    "Wait for the next char keypress, then for every selection's cursor find the nearest enclosing pair for that char and delete the open/close chars, leaving the inner content. No-op when the cursor is not enclosed by such a pair.",
    ActionPriority::Rare
);

define_action!(
    SelectTextobjectAroundDef,
    SelectTextobjectAround,
    "SelectTextobjectAround",
    ActionKind::SelectTextobjectAround,
    "select around textobject",
    "Wait for the next char keypress (`f` function, `t` class, `p` paragraph, `a` parameter, `c` comment), then expand the primary selection to enclose the textobject containing the cursor. `Around` includes surrounding context (e.g. function signature plus body, or paragraph plus trailing blank line). Tree-sitter-driven for `f`/`t`/`a`/`c` (no-op for languages without a `textobjects.scm`); line-based walk for `p`. No-op when no matching textobject contains the cursor.",
    ActionPriority::Rare
);

define_action!(
    SelectTextobjectInnerDef,
    SelectTextobjectInner,
    "SelectTextobjectInner",
    ActionKind::SelectTextobjectInner,
    "select inside textobject",
    "Wait for the next char keypress (`f` function, `t` class, `p` paragraph, `a` parameter, `c` comment), then collapse the primary selection onto the textobject's inner content (e.g. function body without the signature, or paragraph without trailing blank lines). Tree-sitter-driven for `f`/`t`/`a`/`c` (no-op for languages without a `textobjects.scm`); line-based walk for `p`. No-op when no matching textobject contains the cursor.",
    ActionPriority::Rare
);

define_action!(
    GotoNextFunctionDef,
    GotoNextFunction,
    "GotoNextFunction",
    ActionKind::GotoNextFunction,
    "goto next function",
    "Move the primary cursor to the start of the next function definition in the buffer, looking up `function.around` captures via the language's `textobjects.scm`. No-op for languages without a textobjects query (json, markdown) or when no function lies after the cursor.",
    ActionPriority::Rare
);

define_action!(
    GotoPrevFunctionDef,
    GotoPrevFunction,
    "GotoPrevFunction",
    ActionKind::GotoPrevFunction,
    "goto previous function",
    "Move the primary cursor to the start of the previous function definition in the buffer, looking up `function.around` captures via the language's `textobjects.scm`. No-op for languages without a textobjects query (json, markdown) or when no function lies before the cursor.",
    ActionPriority::Rare
);

define_action!(
    GotoNextClassDef,
    GotoNextClass,
    "GotoNextClass",
    ActionKind::GotoNextClass,
    "goto next class or type",
    "Move the primary cursor to the start of the next class / struct / enum / trait / impl definition in the buffer, looking up `class.around` captures via the language's `textobjects.scm`. No-op for languages without a textobjects query (json, markdown) or when no class lies after the cursor.",
    ActionPriority::Rare
);

define_action!(
    GotoPrevClassDef,
    GotoPrevClass,
    "GotoPrevClass",
    ActionKind::GotoPrevClass,
    "goto previous class or type",
    "Move the primary cursor to the start of the previous class / struct / enum / trait / impl definition in the buffer, looking up `class.around` captures via the language's `textobjects.scm`. No-op for languages without a textobjects query (json, markdown) or when no class lies before the cursor.",
    ActionPriority::Rare
);

define_action!(
    OpenSearchInputDef,
    OpenSearchInput,
    "OpenSearchInput",
    ActionKind::OpenSearchInput,
    "open forward search input",
    "Open a one-line input modal for forward in-buffer search. On submit jumps to the first substring match at or after the cursor, wrapping if no match exists ahead. Stores the query and direction for later `SearchNext` / `SearchPrev` to repeat.",
    ActionPriority::Common
);

define_action!(
    OpenReverseSearchInputDef,
    OpenReverseSearchInput,
    "OpenReverseSearchInput",
    ActionKind::OpenReverseSearchInput,
    "open reverse search input",
    "Open a one-line input modal for reverse in-buffer search. On submit jumps to the first substring match before the cursor, wrapping if no match exists behind. Stores the query and direction for later `SearchNext` / `SearchPrev` to repeat.",
    ActionPriority::Common
);

define_action!(
    SearchNextDef,
    SearchNext,
    "SearchNext",
    ActionKind::SearchNext,
    "jump to next match",
    "Jump every cursor to the next match of the most recently submitted search query, in the direction that search was opened with (`/` -> forward, `?` -> reverse). Wraps around buffer ends. No-op when no search has been run in this session.",
    ActionPriority::Common
);

define_action!(
    SearchPrevDef,
    SearchPrev,
    "SearchPrev",
    ActionKind::SearchPrev,
    "jump to previous match",
    "Jump every cursor to the next match in the direction opposite to the last submitted search (`/` makes `N` go backward; `?` makes `N` go forward). Wraps around buffer ends. No-op when no search has been run in this session.",
    ActionPriority::Common
);

define_action!(
    YankDef,
    Yank,
    "Yank",
    ActionKind::Yank,
    "yank primary selection",
    "Copy the focused editor's primary selection content into the unnamed register (`\"`). Selections themselves are not changed. Subsequent `PasteAfter` / `PasteBefore` paste the same content.",
    ActionPriority::Common
);

define_action!(
    PasteAfterDef,
    PasteAfter,
    "PasteAfter",
    ActionKind::PasteAfter,
    "paste register after selection",
    "Insert the unnamed register's content immediately after every selection's end offset. Each affected selection collapses to a cursor at the end of the inserted text. No-op when the register is empty.",
    ActionPriority::Common
);

define_action!(
    PasteBeforeDef,
    PasteBefore,
    "PasteBefore",
    ActionKind::PasteBefore,
    "paste register before selection",
    "Insert the unnamed register's content immediately before every selection's start offset. Each affected selection collapses to a cursor at the end of the inserted text. No-op when the register is empty.",
    ActionPriority::Common
);

define_action!(
    YankToClipboardDef,
    YankToClipboard,
    "YankToClipboard",
    ActionKind::YankToClipboard,
    "yank selections to system clipboard",
    "Gather every non-collapsed selection's content (joined by newlines in start-offset order) and write it to the system clipboard via the active `ClipboardHost`. Falls back to a logged warning when the platform clipboard is unavailable.",
    ActionPriority::Common
);

define_action!(
    YankMainToClipboardDef,
    YankMainToClipboard,
    "YankMainToClipboard",
    ActionKind::YankMainToClipboard,
    "yank primary selection to system clipboard",
    "Write only the primary selection's content to the system clipboard via the active `ClipboardHost`. Useful when multi-selection yank would join unrelated regions.",
    ActionPriority::Common
);

define_action!(
    PasteClipboardAfterDef,
    PasteClipboardAfter,
    "PasteClipboardAfter",
    ActionKind::PasteClipboardAfter,
    "paste system clipboard after selection",
    "Read the current system clipboard contents through the active `ClipboardHost` and insert them at every selection's end offset. Line-aware: when the clipboard has exactly one line per selection, paste line K into selection K.",
    ActionPriority::Common
);

define_action!(
    PasteClipboardBeforeDef,
    PasteClipboardBefore,
    "PasteClipboardBefore",
    ActionKind::PasteClipboardBefore,
    "paste system clipboard before selection",
    "Read the current system clipboard contents through the active `ClipboardHost` and insert them at every selection's start offset. Line-aware: when the clipboard has exactly one line per selection, paste line K into selection K.",
    ActionPriority::Common
);

define_action!(
    SelectRegisterDef,
    SelectRegister,
    "SelectRegister",
    ActionKind::SelectRegister,
    "select register for next yank/paste",
    "Wait for the next char keypress; the captured letter selects a named register (`a-z`) for the next `Yank` / `PasteAfter` / `PasteBefore` operation. Typing `\"` selects the unnamed register explicitly. The selection is consumed by the next yank or paste and reverts to the unnamed register.",
    ActionPriority::Rare
);

define_action!(
    InsertRegisterDef,
    InsertRegister,
    "InsertRegister",
    ActionKind::InsertRegister,
    "insert register at cursor",
    "Wait for the next char keypress in insert mode; the captured letter (`a-z`) names a register whose contents are inserted at the focused editor's cursor. Typing `\"` reads the unnamed register. No-op when the register is empty or unset.",
    ActionPriority::Rare
);

define_action!(
    RepeatLastMotionDef,
    RepeatLastMotion,
    "RepeatLastMotion",
    ActionKind::RepeatLastMotion,
    "repeat last find motion",
    "Replay the most recent f/F/t/T find against the same target char. No-op when no find has been executed in this session.",
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
    OpenBelowDef,
    OpenBelow,
    "OpenBelow",
    ActionKind::OpenBelow,
    "open new line below",
    "Insert a blank line directly below the line containing each primary cursor and place the cursor at column 0 of the new line. Multiple cursors on the same row produce a single newline insertion. Typically chained with `SetMode(insert)` to enter insert mode on the new line.",
    ActionPriority::Rare
);

define_action!(
    OpenAboveDef,
    OpenAbove,
    "OpenAbove",
    ActionKind::OpenAbove,
    "open new line above",
    "Insert a blank line directly above the line containing each primary cursor and place the cursor at column 0 of the new line. Multiple cursors on the same row produce a single newline insertion. Typically chained with `SetMode(insert)` to enter insert mode on the new line.",
    ActionPriority::Rare
);

define_action!(
    ReplaceCharDef,
    ReplaceChar,
    "ReplaceChar",
    ActionKind::ReplaceChar,
    "replace selected chars with next typed char",
    "Arms a one-shot prompt for the next character keypress; once a printable char arrives, every character in every non-empty selection is replaced with that char and the selection is preserved over the replaced text. Empty selections are left untouched. Mirrors Helix's `r` binding.",
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
    GotoColumnDef,
    GotoColumn,
    "GotoColumn",
    ActionKind::GotoColumn,
    "goto column from count",
    "Move the primary cursor to the n-th character of its current line, where n is the pending count prefix (1-indexed). With no pending count, lands at column 1 (line start). Counts beyond the line's character count clamp to the line end. Walks by `char`, so UTF-8 multibyte characters count as one column.",
    ActionPriority::Rare
);

define_action!(
    ExtendGotoColumnDef,
    ExtendGotoColumn,
    "ExtendGotoColumn",
    ActionKind::ExtendGotoColumn,
    "extend to column from count",
    "Like `GotoColumn` but extends the primary selection rather than collapsing it. The selection's tail stays put while the head moves to the column.",
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
    GotoNextParagraphDef,
    GotoNextParagraph,
    "GotoNextParagraph",
    ActionKind::GotoNextParagraph,
    "goto next paragraph",
    "Move the primary cursor to the start of the next paragraph. A paragraph is a run of lines whose byte length is non-zero; lines with zero bytes (purely a line ending) are paragraph separators. Walks forward over the rest of the current paragraph, then over any empty lines, landing at the first non-empty row that follows. No-op when no further paragraph exists in the buffer. Primary-cursor only.",
    ActionPriority::Rare
);

define_action!(
    GotoPrevParagraphDef,
    GotoPrevParagraph,
    "GotoPrevParagraph",
    ActionKind::GotoPrevParagraph,
    "goto previous paragraph",
    "Move the primary cursor to the start of the previous paragraph. From the row above the cursor, walks backward over any empty lines, then over the run of non-empty lines, landing at the row after the empty separator (or row 0 when the buffer begins with the run). No-op when the cursor is already at row 0. Primary-cursor only.",
    ActionPriority::Rare
);

define_action!(
    MatchBracketsDef,
    MatchBrackets,
    "MatchBrackets",
    ActionKind::MatchBrackets,
    "match brackets",
    "Move the primary cursor to the bracket that matches the one under the cursor. Supports `()`, `[]`, and `{}`; `<>` is excluded due to ambiguity with comparison operators. Scans forward from an open bracket or backward from a close bracket, tracking nesting depth to find the pair. No-op when the cursor is not on a recognised bracket or when no balanced match exists in the buffer. Naive scan -- a future tree-sitter-aware variant could exclude brackets inside strings and comments. Primary-cursor only.",
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
    GotoWordDef,
    GotoWord,
    "GotoWord",
    ActionKind::GotoWord,
    "goto labelled word in viewport",
    "Label every word start visible in the focused editor's viewport with a one- or two-character tag (single-char when there are <= 26 candidates, otherwise two-char). The next character keystrokes narrow to a unique label and jump the cursor to that word. Mirrors Helix's two-character interactive label jump.",
    ActionPriority::Rare
);

define_action!(
    ExtendGotoFirstNonwhitespaceDef,
    ExtendGotoFirstNonwhitespace,
    "ExtendGotoFirstNonwhitespace",
    ActionKind::ExtendGotoFirstNonwhitespace,
    "extend to first non-whitespace",
    "Like `GotoFirstNonwhitespace` but extends the primary selection rather than collapsing it.",
    ActionPriority::Rare
);

define_action!(
    ExtendGotoFileStartDef,
    ExtendGotoFileStart,
    "ExtendGotoFileStart",
    ActionKind::ExtendGotoFileStart,
    "extend to file start",
    "Like `GotoFileStart` but extends the primary selection rather than collapsing it.",
    ActionPriority::Rare
);

define_action!(
    ExtendGotoLastLineDef,
    ExtendGotoLastLine,
    "ExtendGotoLastLine",
    ActionKind::ExtendGotoLastLine,
    "extend to last line",
    "Like `GotoLastLine` but extends the primary selection rather than collapsing it.",
    ActionPriority::Rare
);

define_action!(
    ExtendGotoWindowTopDef,
    ExtendGotoWindowTop,
    "ExtendGotoWindowTop",
    ActionKind::ExtendGotoWindowTop,
    "extend to window top",
    "Like `GotoWindowTop` but extends the primary selection rather than collapsing it.",
    ActionPriority::Rare
);

define_action!(
    ExtendGotoWindowCenterDef,
    ExtendGotoWindowCenter,
    "ExtendGotoWindowCenter",
    ActionKind::ExtendGotoWindowCenter,
    "extend to window center",
    "Like `GotoWindowCenter` but extends the primary selection rather than collapsing it.",
    ActionPriority::Rare
);

define_action!(
    ExtendGotoWindowBottomDef,
    ExtendGotoWindowBottom,
    "ExtendGotoWindowBottom",
    ActionKind::ExtendGotoWindowBottom,
    "extend to window bottom",
    "Like `GotoWindowBottom` but extends the primary selection rather than collapsing it.",
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
    "Toggle the case of every character in each selection: uppercase becomes lowercase and vice versa. Digits, punctuation, and non-letter characters pass through unchanged.",
    ActionPriority::Rare
);

define_action!(
    SwitchToUppercaseDef,
    SwitchToUppercase,
    "SwitchToUppercase",
    ActionKind::SwitchToUppercase,
    "uppercase selection",
    "Uppercase every character in each selection. Already-uppercase and non-letter characters pass through unchanged.",
    ActionPriority::Rare
);

define_action!(
    SwitchToLowercaseDef,
    SwitchToLowercase,
    "SwitchToLowercase",
    ActionKind::SwitchToLowercase,
    "lowercase selection",
    "Lowercase every character in each selection. Already-lowercase and non-letter characters pass through unchanged.",
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
    RemovePrimarySelectionDef,
    RemovePrimarySelection,
    "RemovePrimarySelection",
    ActionKind::RemovePrimarySelection,
    "remove primary selection",
    "Drop the newest (primary) selection while retaining all others. No-op when only one selection exists.",
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
            RemovePrimarySelection.kind(),
            ActionKind::RemovePrimarySelection
        );
        assert_eq!(
            RemovePrimarySelection.def().name(),
            "RemovePrimarySelection"
        );
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
