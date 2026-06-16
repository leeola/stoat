//! The stoatty terminal application: owns the window and event loop,
//! spawns the shell over a PTY, and drives [`stoatty_render`] over the
//! grid that [`stoatty_term`] builds from the PTY byte stream.
//!
//! IO lives here -- the window, the reader thread, the event loop --
//! keeping the renderer and terminal-core crates pure.

pub mod app;
pub mod config;
pub mod pty;
