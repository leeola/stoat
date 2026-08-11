//! Stoatty's APC sub-protocol: the `Gstoatty` frame grammar and the
//! typed command surface emitting programs link against to drive
//! renderer features.
//!
//! Kept dependency-light -- no GPU, windowing, or terminal-state deps --
//! so a program needs only this crate to emit stoatty bytes, and the
//! frames degrade to ignorable escape sequences in any other terminal.
//!
//! # Evolving a command
//!
//! A command grows only by appending. A new field goes at the end of its
//! fixed head, or arrives as an argument after the ones already defined.
//! Never reorder, resize, or repurpose what is already there.
//!
//! The rule exists because the two sides of a session are versioned
//! separately. A program may be newer than the terminal it is talking to,
//! over ssh or simply because one of them was updated first. Every decoder
//! reads the prefix it knows and ignores what follows, so a frame carrying
//! a field an older terminal has never heard of still delivers everything
//! that terminal does understand. An unknown enum code degrades to a member
//! it knows rather than dropping the command around it.
//!
//! Anything a decoder cannot express as an append needs a new sub-command
//! instead, which an older terminal ignores whole rather than misreading.

pub mod command;
pub mod frame;
pub mod window_ipc;
