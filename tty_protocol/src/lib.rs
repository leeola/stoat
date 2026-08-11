//! Stoatty's APC sub-protocol: the `Gstoatty` frame grammar and the
//! typed command surface emitting programs link against to drive
//! renderer features.
//!
//! Kept dependency-light -- no GPU, windowing, or terminal-state deps --
//! so a program needs only this crate to emit stoatty bytes.
//!
//! # What degrades, and what does not
//!
//! A frame degrades to an ignorable escape sequence in any other terminal,
//! since an APC string is consumed and never drawn. That covers every
//! command in [`command`].
//!
//! Streamed content does not degrade. A popover's text, a text run's
//! characters, and a page fill's cells travel outside the frame wrapper as
//! ordinary bytes, so a terminal that never opened the capture prints them
//! over whatever is on screen. Settle which terminal answers with
//! [`detect`] before you emit any of it.
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

/// The revision of this protocol each side announces in the handshake.
///
/// A peer reports what it can render, so an emitter can hold back a command the
/// other end predates rather than sending bytes it will print raw. Bumped
/// whenever a command is added or an existing one grows a field.
///
/// Zero is reserved for a peer whose handshake carries no version at all, which
/// is every build from before the field existed.
pub const PROTOCOL_VERSION: u32 = 1;

pub mod command;
pub mod detect;
pub mod frame;
pub mod window_ipc;
