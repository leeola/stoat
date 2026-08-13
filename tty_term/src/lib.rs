//! Stoatty's terminal core: a pure bytes-to-grid model.
//!
//! Holds the superset cell grid and the driver that projects a parsed
//! VT byte stream onto it, applying decoded [`stoatty_protocol`]
//! commands. No IO lives here, so the model stays testable; the app
//! crate feeds it bytes.

pub mod grid;
pub mod term;
pub mod theme;

/// Re-exported so a consumer of [`term::PoolView`] reads the id range without
/// depending on the protocol crate.
///
/// The constant is part of what a pool snapshot means rather than a wire
/// detail. It splits editor-pane pools from box pools, which is a z-order the
/// renderer composites by.
pub use stoatty_protocol::command::NON_PANE_POOL_BASE;
