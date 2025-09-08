//! GPUI action definitions for the editor

use gpui::{actions, Action};

// Define basic movement actions
actions!(
    editor,
    [
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        MoveWordForward,
        MoveWordBackward,
        MoveLineStart,
        MoveLineEnd,
        MoveFileStart,
        MoveFileEnd,
        PageUp,
        PageDown,
    ]
);

// Define editing actions
actions!(
    editor,
    [
        EnterInsertMode,
        EnterInsertModeAfter,
        EnterInsertModeLineStart,
        EnterInsertModeLineEnd,
        EnterVisualMode,
        EnterVisualLineMode,
        EnterVisualBlockMode,
        EnterCommandMode,
        Escape,
        Delete,
        DeleteLine,
        DeleteToEndOfLine,
        DeleteWord,
        Change,
        ChangeLine,
        ChangeToEndOfLine,
        ChangeWord,
        Yank,
        YankLine,
        Paste,
        PasteBefore,
        Undo,
        Redo,
        Indent,
        Outdent,
        JoinLines,
    ]
);

// Define search and replace actions
actions!(
    editor,
    [
        Search,
        SearchBackward,
        NextMatch,
        PreviousMatch,
        Replace,
        ReplaceAll,
    ]
);

// Define file operations
actions!(
    editor,
    [
        Save,
        SaveAs,
        Open,
        NewFile,
        CloseFile,
        Quit,
        ForceQuit,
        SaveAndQuit,
    ]
);

// Define window and view actions
actions!(
    editor,
    [
        SplitHorizontal,
        SplitVertical,
        NextWindow,
        PreviousWindow,
        CloseWindow,
        ZoomIn,
        ZoomOut,
        ResetZoom,
    ]
);

// Define selection actions
actions!(
    editor,
    [
        SelectAll,
        SelectLine,
        SelectWord,
        SelectToLineStart,
        SelectToLineEnd,
        ExpandSelection,
        ShrinkSelection,
    ]
);

// Define clipboard actions
actions!(
    editor,
    [Copy, Cut, PasteFromSystemClipboard, CopyToSystemClipboard,]
);

// Define macro actions
actions!(
    editor,
    [StartRecordingMacro, StopRecordingMacro, PlayMacro,]
);
