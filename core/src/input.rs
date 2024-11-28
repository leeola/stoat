// NOTE: This module is a super rough outline. Need to reference some prior art, as
// i expect this is a solved problem. Perhaps even expose an existing crate type.

/// Raw input from some Stoat UI, to be later mapped to underlying commands/modes/nodes/etc.
///
/// The resulting output from [`Output`](crate::output::Output).
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Input {
    // NIT: multiple chars?
    Keyboard(char),
    Mouse(Mouse),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mouse {
    Click { x: usize, y: usize },
    // NIT: Velocity? Note sure what data we get for mouse scroll.
    Scroll { up: bool },
}
