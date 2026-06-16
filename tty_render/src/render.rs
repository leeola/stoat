//! The grid render passes that draw [`stoatty_term`]'s cells.

pub mod background;
pub mod decoration;
pub mod overlay;
pub mod text;

/// Cell size in physical pixels used to lay out the grid.
///
/// A fixed placeholder until cell metrics are derived from the font; the grid
/// passes only need a consistent cell rectangle, and the background and text
/// passes must agree on it so glyphs land on their cells.
pub(crate) const CELL_WIDTH: f32 = 18.0;
pub(crate) const CELL_HEIGHT: f32 = 36.0;
