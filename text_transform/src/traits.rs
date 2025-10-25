///! Core traits for DisplayMap layer implementations.
///!
///! These traits define the interfaces that all transformation layers must implement,
///! enabling composition of the six-layer pipeline.
use text::Point;

/// Edit operations on the buffer using Point coordinates
pub type BufferEdit = text::Edit<Point>;

/// Trait for coordinate transformation between adjacent layers.
///
/// Each layer in the DisplayMap pipeline implements this trait to provide bidirectional
/// conversion between its input and output coordinate spaces.
///
/// # Type Parameters
///
/// - `From`: The input coordinate type (e.g., [`FoldPoint`](crate::FoldPoint))
/// - `To`: The output coordinate type (e.g., [`TabPoint`](crate::TabPoint))
///
/// # Invariants
///
/// Implementations must maintain these invariants:
///
/// 1. **Clamping**: If a coordinate is invalid (e.g., inside a fold, inside a tab expansion),
///    conversion should clamp to the nearest valid position rather than panicking.
///
/// 2. **Consistency**: For any valid coordinate `c`, converting forward then backward should return
///    an equivalent coordinate (accounting for clamping): ```text let c2 = to_coords(c); let c3 =
///    from_coords(c2); // c3 should be equivalent to c (or clamped if c was invalid) ```
///
/// 3. **Ordering preservation**: If `a < b` in buffer space, then their transformed coordinates
///    should maintain relative ordering (accounting for hidden regions like folds).
///
/// # Example Implementation
///
/// ```ignore
/// impl CoordinateTransform<FoldPoint, TabPoint> for TabMap {
///     fn to_coords(&self, fold_point: FoldPoint) -> TabPoint {
///         // Transform FoldPoint to TabPoint by expanding tabs
///         let buffer_point = self.fold_map.to_buffer_point(fold_point);
///         let line = self.buffer.line(buffer_point.row);
///
///         let mut display_col = 0;
///         for (idx, ch) in line.chars().enumerate() {
///             if idx >= fold_point.column as usize {
///                 break;
///             }
///             if ch == '\t' {
///                 display_col = (display_col / self.tab_width + 1) * self.tab_width;
///             } else {
///                 display_col += 1;
///             }
///         }
///
///         TabPoint {
///             row: fold_point.row,
///             column: display_col,
///         }
///     }
///
///     fn from_coords(&self, tab_point: TabPoint) -> FoldPoint {
///         // Reverse transformation: TabPoint back to FoldPoint
///         // ...
///     }
/// }
/// ```
///
/// # Related
///
/// - [`EditableLayer`]: Trait for handling buffer edits
/// - Each layer type implements this for its specific coordinate pair
pub trait CoordinateTransform<From, To> {
    /// Convert from input coordinate to output coordinate.
    ///
    /// Transforms a coordinate from the layer's input space to its output space.
    /// If the input coordinate is invalid (e.g., inside a folded region), this should
    /// clamp to the nearest valid position.
    ///
    /// # Performance
    ///
    /// Implementations should achieve O(log n) performance using [`sum_tree::SumTree`]
    /// for querying transformation state.
    fn to_coords(&self, point: From) -> To;

    /// Convert from output coordinate back to input coordinate.
    ///
    /// Reverse transformation from the layer's output space back to its input space.
    /// This is the inverse of [`to_coords`](Self::to_coords).
    ///
    /// If the output coordinate maps to an invalid input position (e.g., cursor inside
    /// a tab expansion), this should clamp to the start of the special region.
    ///
    /// # Performance
    ///
    /// Implementations should achieve O(log n) performance using [`sum_tree::SumTree`].
    fn from_coords(&self, point: To) -> From;
}

/// Trait for layers that respond to buffer edits.
///
/// All transformation layers must implement this trait to handle incremental updates
/// when the buffer changes. This ensures that coordinate transformations remain correct
/// as the buffer is edited.
///
/// # Edit Handling Strategy
///
/// Layers typically use one of two strategies:
///
/// 1. **Anchor-based tracking**: For layers that insert/remove visual elements at fixed buffer
///    positions (InlayMap, FoldMap, BlockMap). Elements are anchored to buffer positions and
///    automatically adjust when the buffer changes.
///
/// 2. **Recalculation**: For layers that depend on buffer content (TabMap, WrapMap). Affected
///    regions are recalculated when the buffer changes.
///
/// # Example Implementation
///
/// ```ignore
/// impl EditableLayer for TabMap {
///     fn apply_edit(&mut self, edit: &BufferEdit) {
///         // Invalidate cached tab expansions for edited rows
///         for row in edit.old.start.row..=edit.old.end.row {
///             self.tab_cache.remove(&row);
///         }
///
///         self.version += 1;
///     }
///
///     fn version(&self) -> usize {
///         self.version
///     }
/// }
/// ```
///
/// # Related
///
/// - [`CoordinateTransform`]: Trait for coordinate conversion
/// - [`text::BufferEdit`]: Describes a buffer edit operation
pub trait EditableLayer {
    /// Handle a buffer edit by updating internal state.
    ///
    /// This method is called when the buffer changes. Implementations should:
    /// 1. Update any internal data structures to reflect the edit
    /// 2. Invalidate any cached state for affected regions
    /// 3. Increment the version counter
    /// 4. Notify any subscribers of the change (if applicable)
    ///
    /// # Incremental Updates
    ///
    /// Implementations should only update the affected region, not recalculate the entire
    /// layer state. This is critical for performance with large files.
    ///
    /// # Anchor Handling
    ///
    /// If the layer uses [`text::Anchor`] for tracking positions, these will automatically
    /// adjust when the buffer changes. The layer should recompute any derived state based
    /// on the new anchor positions.
    fn apply_edit(&mut self, edit: &BufferEdit);

    /// Get the current version of this layer.
    ///
    /// The version is incremented each time the layer state changes. This enables
    /// caching strategies - cached values can be invalidated by comparing version
    /// numbers.
    ///
    /// # Example
    ///
    /// ```ignore
    /// if cache.version != layer.version() {
    ///     cache.clear();
    ///     cache.version = layer.version();
    /// }
    /// ```
    fn version(&self) -> usize;
}
