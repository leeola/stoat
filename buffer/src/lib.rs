//! A text buffer and the display stack that turns it into rows.
//!
//! [`buffer::TextBuffer`] holds a file's text and its edit history.
//! [`multi_buffer::MultiBuffer`] hands out the snapshots the display layers
//! read, and [`diff_map::DiffMap`] holds a buffer's hunks against its base
//! text. [`display_map::DisplayMap`] layers inlays, folds, tabs, wraps, and
//! blocks over a buffer to give its display rows.
//!
//! The four name each other, as a buffer holds its diff map and a diff map
//! builds display blocks, so they share one crate.

pub mod buffer;
pub mod diff_map;
pub mod display_map;
pub mod multi_buffer;

/// A redraw handle for tests, where nothing listens for the wake.
///
/// Constructors take one so no production site can be built without a live
/// handle. A test that only inspects state has no run loop to wake, so it
/// passes a fresh one rather than plumbing the app's.
#[cfg(test)]
pub(crate) fn test_notify() -> std::sync::Arc<tokio::sync::Notify> {
    std::sync::Arc::new(tokio::sync::Notify::new())
}
