// NOTE: This module is a super rough outline. Need to reference some prior art, as
// i expect this is a solved problem. Perhaps even expose an existing crate type.

// TODO: Rename to be more Key/User oriented. This app has lots of inputs, so this is conflated.
//
/// Raw input from some Stoat UI, to be later mapped to underlying commands/modes/nodes/etc.
///
/// The resulting output from [`Output`](crate::output::Output).
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum UserInput {
    // NIT: multiple chars?
    Keyboard(char),
    // Mouse(Mouse),
}

// NIT: Not sure what i want to do about mouse. Mouse input is useful but it has some association to
// the graphical environment. It's difficult to abstract that correctly. However Stoat being Canvas
// based means it has a coordinate space.. so perhaps there is a good overlap somewhere.
// Plus of course it can scroll in text windows, possibly reveal canvas space, etc.
// #[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
// pub enum Mouse {
//     Click { x: usize, y: usize },
//     // NIT: Velocity? Note sure what data we get for mouse scroll.
//     Scroll { up: bool },
// }
