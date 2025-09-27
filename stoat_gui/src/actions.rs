//! Legacy actions module
//!
//! This module contains some legacy action definitions that are not part of the
//! main command system. Most actions have been moved to the commands module.

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
