use gpui::actions;

/// Trait for all actions that can be dispatched in the editor
pub trait Action: std::fmt::Debug + Send + Sync + 'static {
    fn boxed_clone(&self) -> Box<dyn Action>;
    fn as_any(&self) -> &dyn std::any::Any;
}

impl<T> Action for T
where
    T: Clone + std::fmt::Debug + Send + Sync + 'static,
{
    fn boxed_clone(&self) -> Box<dyn Action> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// Core movement actions
actions!(
    movement,
    [
        MoveUp,
        MoveDown,
        MoveLeft,
        MoveRight,
        MoveWordLeft,
        MoveWordRight,
        MoveToLineStart,
        MoveToLineEnd,
        MoveToFileStart,
        MoveToFileEnd,
        PageUp,
        PageDown
    ]
);

// Text editing actions
actions!(
    edit,
    [
        InsertChar,
        DeleteLeft,
        DeleteRight,
        DeleteWord,
        DeleteLine,
        NewLine,
        Indent,
        Outdent
    ]
);

// File operations
actions!(file, [Save, SaveAs, Quit, ForceQuit]);

// History actions
actions!(history, [Undo, Redo]);

// Mode switching actions for modal editing
actions!(
    mode,
    [
        EnterInsertMode,
        EnterNormalMode,
        EnterVisualMode,
        EnterCommandMode
    ]
);

// Selection actions
actions!(
    selection,
    [
        SelectUp,
        SelectDown,
        SelectLeft,
        SelectRight,
        SelectWordLeft,
        SelectWordRight,
        SelectToLineStart,
        SelectToLineEnd,
        SelectAll,
        ClearSelection
    ]
);

// Clipboard actions
actions!(clipboard, [Copy, Cut, Paste]);

// Search actions
actions!(search, [Find, FindNext, FindPrevious, Replace]);

/// Action that inserts a specific character
#[derive(Debug, Clone)]
pub struct InsertCharacter {
    pub character: char,
}

/// Action that finds a specific character on the current line
#[derive(Debug, Clone)]
pub struct FindCharacter {
    pub character: char,
    pub direction: Direction,
}

/// Direction for movement and search actions
#[derive(Debug, Clone)]
pub enum Direction {
    Forward,
    Backward,
}

/// Action that repeats the last action with an optional count
#[derive(Debug, Clone)]
pub struct Repeat {
    pub count: Option<usize>,
}

/// Action that applies a count to another action
#[derive(Debug, Clone)]
pub struct CountedAction {
    pub count: usize,
    pub action: String, // Action name to be resolved later
}
