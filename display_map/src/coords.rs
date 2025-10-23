///! Coordinate types for each layer in the DisplayMap transformation pipeline.
///!
///! Each coordinate type represents a distinct transformation layer, preventing
///! accidental mixing of coordinate spaces through compile-time type safety.

/// Coordinate after applying inlay hints.
///
/// [`InlayMap`](crate::InlayMap) transforms buffer [`text::Point`] to [`InlayPoint`] by
/// inserting visual-only text (type annotations, parameter names) that doesn't exist
/// in the actual buffer.
///
/// # Example
///
/// ```text
/// Buffer:     let x = compute(42);
/// Display:    let x: String = compute(value: 42);
///                    ^^^^^^             ^^^^^^
///                    inlay hints (shift column positions)
/// ```
///
/// The column position at `compute` in the buffer is different from its visual column
/// in the display due to the `: String` inlay hint inserted before it.
///
/// # Layer Position
///
/// First layer in the pipeline: `Point -> InlayPoint`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct InlayPoint {
    pub row: u32,
    pub column: u32,
}

/// Coordinate after applying code folding.
///
/// [`FoldMap`](crate::FoldMap) transforms [`InlayPoint`] to [`FoldPoint`] by hiding
/// folded regions, reducing the number of visible rows.
///
/// # Example
///
/// ```text
/// Buffer/Inlay:              Fold Display:
/// fn example() {             fn example() { ... }
///     line 1                 fn another() {
///     line 2
///     line 3
/// }
/// fn another() {
/// ```
///
/// Rows 1-4 in the buffer are hidden, so `fn another()` at buffer row 5 appears
/// at fold row 1.
///
/// # Layer Position
///
/// Second layer: `InlayPoint -> FoldPoint`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct FoldPoint {
    pub row: u32,
    pub column: u32,
}

/// Coordinate after expanding tab characters.
///
/// [`TabMap`](crate::TabMap) transforms [`FoldPoint`] to [`TabPoint`] by expanding
/// `\t` characters into the appropriate number of spaces based on tab width settings.
///
/// # Example
///
/// ```text
/// Buffer (tab_width=4):     Display:
/// "text"                    "    text"   (tab expands to 4 spaces)
/// "ab cd"                   "ab  cd"     (tab expands to 2 spaces to align at column 4)
/// ```
///
/// A tab character always extends to the next multiple of `tab_width`, so its visual
/// width depends on the current column position.
///
/// # Layer Position
///
/// Third layer: `FoldPoint -> TabPoint`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct TabPoint {
    pub row: u32,
    pub column: u32,
}

/// Coordinate after soft-wrapping long lines.
///
/// [`WrapMap`](crate::WrapMap) transforms [`TabPoint`] to [`WrapPoint`] by breaking
/// long lines that exceed the editor width into multiple display rows.
///
/// # Example
///
/// ```text
/// Buffer (one line):
/// "This is a very long line that exceeds the wrap width"
///
/// Display (wrap at column 20):
/// Row 0: "This is a very long "
/// Row 1: "line that exceeds "
/// Row 2: "the wrap width"
/// ```
///
/// A single buffer line becomes multiple display rows, increasing the total row count
/// without modifying the buffer.
///
/// # Layer Position
///
/// Fourth layer: `TabPoint -> WrapPoint`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct WrapPoint {
    pub row: u32,
    pub column: u32,
}

/// Coordinate after inserting block decorations.
///
/// [`BlockMap`](crate::BlockMap) transforms [`WrapPoint`] to [`BlockPoint`] by
/// inserting visual blocks between buffer lines (diagnostics, git blame, etc.).
///
/// # Example
///
/// ```text
/// Buffer/Wrap:              Block Display:
/// Row 0: fn example()       Row 0: fn example()
///                           Row 1: Warning: unused variable
/// Row 1: let x = 42         Row 2: let x = 42
/// ```
///
/// Block decorations add extra display rows that don't correspond to any buffer
/// content, pushing subsequent lines down visually.
///
/// # Layer Position
///
/// Fifth layer: `WrapPoint -> BlockPoint`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct BlockPoint {
    pub row: u32,
    pub column: u32,
}

/// Final display coordinate after all transformations.
///
/// [`DisplayPoint`] represents the actual visual position on screen after all
/// transformation layers have been applied. This is what the editor uses for:
/// - Cursor positioning
/// - Selection rendering
/// - Scrolling calculations
/// - Mouse click handling
///
/// # Transformation Pipeline
///
/// ```text
/// Point (buffer)
///   | InlayMap
/// InlayPoint
///   | FoldMap
/// FoldPoint
///   | TabMap
/// TabPoint
///   | WrapMap
/// WrapPoint
///   | BlockMap
/// DisplayPoint (final)
/// ```
///
/// # Usage
///
/// All visual operations should use [`DisplayPoint`], while buffer operations use
/// [`text::Point`]. The [`DisplayMap`](crate::DisplayMap) provides conversion between
/// these coordinate spaces.
///
/// # Related
///
/// - Use [`DisplayMap::to_display_point`](crate::DisplayMap::to_display_point) to convert from
///   buffer coordinates
/// - Use [`DisplayMap::to_buffer_point`](crate::DisplayMap::to_buffer_point) to convert back to
///   buffer coordinates
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct DisplayPoint {
    pub row: u32,
    pub column: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinate_types_have_correct_defaults() {
        assert_eq!(InlayPoint::default(), InlayPoint { row: 0, column: 0 });
        assert_eq!(FoldPoint::default(), FoldPoint { row: 0, column: 0 });
        assert_eq!(TabPoint::default(), TabPoint { row: 0, column: 0 });
        assert_eq!(WrapPoint::default(), WrapPoint { row: 0, column: 0 });
        assert_eq!(BlockPoint::default(), BlockPoint { row: 0, column: 0 });
        assert_eq!(DisplayPoint::default(), DisplayPoint { row: 0, column: 0 });
    }

    #[test]
    fn coordinate_types_support_equality() {
        let p1 = InlayPoint { row: 5, column: 10 };
        let p2 = InlayPoint { row: 5, column: 10 };
        let p3 = InlayPoint { row: 5, column: 11 };

        assert_eq!(p1, p2);
        assert_ne!(p1, p3);
    }

    #[test]
    fn coordinate_types_support_ordering() {
        let p1 = FoldPoint { row: 5, column: 10 };
        let p2 = FoldPoint { row: 5, column: 11 };
        let p3 = FoldPoint { row: 6, column: 5 };

        assert!(p1 < p2);
        assert!(p2 < p3);
        assert!(p1 < p3);
    }

    #[test]
    fn coordinate_types_can_be_cloned_and_copied() {
        let p1 = TabPoint {
            row: 10,
            column: 20,
        };
        let p2 = p1; // Copy
        let p3 = p1.clone(); // Clone

        assert_eq!(p1, p2);
        assert_eq!(p1, p3);
    }

    #[test]
    fn coordinate_types_can_be_used_in_collections() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(WrapPoint { row: 1, column: 2 });
        set.insert(WrapPoint { row: 1, column: 2 }); // Duplicate
        set.insert(WrapPoint { row: 2, column: 3 });

        assert_eq!(set.len(), 2); // Duplicates removed
    }
}
