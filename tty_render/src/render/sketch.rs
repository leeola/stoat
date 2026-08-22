//! The hand-drawn marks that annotate the grid.
//!
//! A sketch arrives as a declaration rather than as points, so the geometry is
//! generated here against the live cell metrics. That is what keeps a mark
//! hand-drawn at every font size, and what lets the stroke reveal itself at the
//! display refresh rate without the emitter sending a frame per step.
//!
//! [`rough`] turns one declaration into flattened polylines. [`pass`] uploads
//! them and draws the part a reveal has reached.

pub(crate) mod pass;
pub(crate) mod rough;

pub use pass::SketchPass;
