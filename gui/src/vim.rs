//! Vim mode implementation for modal editing

use gpui::KeyContext;

/// Vim editing modes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    Normal,
    Insert,
    Visual,
    Command,
}

impl VimMode {
    /// Get the key context string for this mode
    pub fn key_context(&self) -> &'static str {
        match self {
            VimMode::Normal => "vim_mode == normal",
            VimMode::Insert => "vim_mode == insert",
            VimMode::Visual => "vim_mode == visual",
            VimMode::Command => "vim_mode == command",
        }
    }

    /// Check if this mode allows text insertion
    pub fn allows_insertion(&self) -> bool {
        matches!(self, VimMode::Insert | VimMode::Command)
    }

    /// Check if this mode shows selection
    pub fn has_selection(&self) -> bool {
        matches!(self, VimMode::Visual)
    }
}

impl Default for VimMode {
    fn default() -> Self {
        VimMode::Normal
    }
}

/// Vim motion types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMotion {
    Left,
    Right,
    Up,
    Down,
    WordForward,
    WordBackward,
    LineStart,
    LineEnd,
    FileStart,
    FileEnd,
    NextChar(char),
    PrevChar(char),
}

/// Vim operator types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimOperator {
    Delete,
    Change,
    Yank,
    Indent,
    Outdent,
}

/// Vim text object types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimTextObject {
    Word,
    BigWord,
    Line,
    Paragraph,
    InnerWord,
    InnerBigWord,
    InnerParen,
    InnerBrace,
    InnerBracket,
    InnerQuote,
    InnerDoubleQuote,
}

/// Vim command state for complex operations
#[derive(Debug, Clone)]
pub struct VimCommandState {
    /// Optional count prefix (e.g., "3" in "3dd")
    pub count: Option<usize>,
    /// Pending operator (e.g., "d" waiting for motion)
    pub operator: Option<VimOperator>,
    /// Register for yank/paste operations
    pub register: Option<char>,
    /// Last executed command for dot repeat
    pub last_command: Option<VimCommand>,
}

impl Default for VimCommandState {
    fn default() -> Self {
        Self {
            count: None,
            operator: None,
            register: None,
            last_command: None,
        }
    }
}

/// A complete vim command
#[derive(Debug, Clone)]
pub struct VimCommand {
    pub count: Option<usize>,
    pub operator: Option<VimOperator>,
    pub motion: Option<VimMotion>,
    pub text_object: Option<VimTextObject>,
}

impl VimCommand {
    /// Execute this command with the given count multiplier
    pub fn with_count(&self, count: usize) -> Self {
        let mut cmd = self.clone();
        cmd.count = Some(count * self.count.unwrap_or(1));
        cmd
    }
}
