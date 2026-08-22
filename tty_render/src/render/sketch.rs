//! The hand-drawn marks that annotate the grid.
//!
//! A sketch arrives as a declaration rather than as points, so the geometry is
//! generated here against the live cell metrics. That is what keeps a mark
//! hand-drawn at every font size, and what lets the stroke reveal itself at the
//! display refresh rate without the emitter sending a frame per step.

// FIXME: no draw pass consumes this geometry yet.
#[allow(dead_code)]
pub(crate) mod rough;
