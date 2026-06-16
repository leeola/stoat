//! Stoatty's terminal core: a pure bytes-to-grid model.
//!
//! Holds the superset cell grid and the driver that projects a parsed
//! VT byte stream onto it, applying decoded [`stoatty_protocol`]
//! commands. No IO lives here, so the model stays testable; the app
//! crate feeds it bytes.

pub mod grid;
pub mod term {}
