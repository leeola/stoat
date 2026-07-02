//! Stoatty's GPU renderer: the wgpu context, glyph atlas, and the
//! instanced background/text passes that draw [`stoatty_term`]'s cell
//! grid.
//!
//! Windowing-toolkit-agnostic: the app passes in a raw window handle
//! for surface creation, so this crate never depends on the windowing
//! library.

pub mod atlas;
pub mod gpu;
pub mod perf;
pub mod render;
