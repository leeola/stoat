//! The lower layer of the editor, which the app layer in `stoat` builds on.
//!
//! It holds the hosts over the file system, git, language servers, and
//! terminals, the text buffers with their diff and display maps, and the
//! keymap. Nothing here reaches up into the app layer, and it is its own
//! crate so that an edit to the app layer leaves it compiled. `stoat` names
//! each of its modules through an alias of the same name, so `stoat` code
//! reaches a module here at `crate::<module>`.

use stoat_theme as theme;

pub mod buffer;
pub mod diff_map;
pub mod display_map;
pub mod host;
pub mod keymap;
pub mod multi_buffer;
pub mod selection;
pub mod syntax_parse;
pub mod wake;

/// A redraw handle for tests, where nothing listens for the wake.
///
/// Constructors take one so no production site can be built without a live
/// handle. A test that only inspects state has no run loop to wake, so it
/// passes a fresh one rather than plumbing the app's.
#[cfg(test)]
pub(crate) fn test_notify() -> std::sync::Arc<tokio::sync::Notify> {
    std::sync::Arc::new(tokio::sync::Notify::new())
}
