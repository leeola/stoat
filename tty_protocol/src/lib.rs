//! Stoatty's APC sub-protocol: the `Gstoatty` frame grammar and the
//! typed command surface emitting programs link against to drive
//! renderer features.
//!
//! Kept dependency-light -- no GPU, windowing, or terminal-state deps --
//! so a program needs only this crate to emit stoatty bytes, and the
//! frames degrade to ignorable escape sequences in any other terminal.

pub mod command;
pub mod frame;
