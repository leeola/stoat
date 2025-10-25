//! Transform pattern infrastructure for DisplayMap layers.
//!
//! The Transform pattern is the core architectural element used by all DisplayMap
//! layers (InlayMap, FoldMap, TabMap, WrapMap, BlockMap). Each layer uses a
//! `SumTree<Transform>` where Transform is an enum representing either:
//!
//! - **Isomorphic**: 1:1 mapping (no transformation)
//! - **Layer-specific variant**: The actual transformation (Inlay, Fold, Wrap, etc.)
//!
//! This pattern enables efficient O(log n) coordinate conversions by explicitly
//! representing both transformed and untransformed regions.
//!
//! # Example: InlayMap
//!
//! ```ignore
//! enum Transform {
//!     Isomorphic(TextSummary),  // No inlay, coordinates map 1:1
//!     Inlay(Inlay),             // Inlay inserted, adds columns
//! }
//!
//! struct TransformSummary {
//!     input: TextSummary,   // Coordinates before inlay
//!     output: TextSummary,  // Coordinates after inlay
//! }
//! ```
//!
//! # Why This Pattern?
//!
//! **Alternative**: Store items directly (`SumTree<Inlay>`)
//! - Problem: How to represent unchanged regions efficiently?
//! - Cursor seeks need to know total extent, not just where items are
//!
//! **Transform Pattern**: Explicit Isomorphic regions
//! - Cursor can efficiently seek by input OR output coordinates
//! - Adjacent Isomorphic transforms merge automatically
//! - Summaries aggregate correctly for O(log n) queries
//!
//! # Architecture
//!
//! Each layer implements:
//! 1. `enum Transform { Isomorphic(TextSummary), LayerSpecific(...) }`
//! 2. `struct TransformSummary { input: TextSummary, output: TextSummary }`
//! 3. `impl Item for Transform` with Summary = TransformSummary
//! 4. Coordinate conversion using `Cursor` with custom dimensions
//!
//! # Related
//!
//! - [`text::TextSummary`] - Aggregated text metadata from text crate
//! - [`sum_tree::SumTree`] - B-tree with summarization
//! - [`sum_tree::Cursor`] - Efficient seeking using dimensions
//! - See DISPLAY_MAP.md for comprehensive architecture documentation
use text::TextSummary;

/// Marker trait for transformation summaries.
///
/// Each layer's TransformSummary implements this trait to enable generic
/// handling of transform trees. The summary tracks both input coordinates
/// (before transformation) and output coordinates (after transformation).
///
/// # Type Parameters
///
/// - `Input`: The dimension type for input coordinates (e.g., Point, InlayPoint)
/// - `Output`: The dimension type for output coordinates (e.g., InlayPoint, FoldPoint)
///
/// # Example
///
/// ```ignore
/// struct InlayTransformSummary {
///     input: TextSummary,   // Buffer space (Point)
///     output: TextSummary,  // After inlays (InlayPoint)
/// }
///
/// impl TransformSummary for InlayTransformSummary {
///     type Input = TextSummary;
///     type Output = TextSummary;
/// }
/// ```
pub trait TransformSummary: Clone + Default {
    /// Input dimension type (coordinates before transformation)
    type Input: Clone + Default;

    /// Output dimension type (coordinates after transformation)
    type Output: Clone + Default;

    /// Get the input summary
    fn input(&self) -> &Self::Input;

    /// Get the output summary
    fn output(&self) -> &Self::Output;
}

/// Base type for isomorphic transforms across all layers.
///
/// Isomorphic transforms represent regions where coordinates map 1:1 with
/// no transformation applied. These are used to fill gaps between actual
/// transformations (inlays, folds, wraps, etc.).
///
/// # Properties
///
/// - Input summary equals output summary (1:1 mapping)
/// - Adjacent isomorphic transforms should be merged
/// - Efficiently represents large unchanged regions
///
/// # Usage
///
/// Each layer's Transform enum includes an Isomorphic variant:
///
/// ```ignore
/// enum InlayTransform {
///     Isomorphic(TextSummary),
///     Inlay(Inlay),
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Isomorphic(pub TextSummary);

impl Isomorphic {
    /// Create a new isomorphic transform with the given summary.
    pub fn new(summary: TextSummary) -> Self {
        Self(summary)
    }

    /// Get the text summary for this isomorphic region.
    pub fn summary(&self) -> &TextSummary {
        &self.0
    }
}
