///! Dimension types for SumTree seeking in DisplayMap layers.
///!
///! Each DisplayMap layer needs custom offset types to represent positions in
///! its coordinate space. These offset types are used with [`sum_tree::Cursor`]
///! to efficiently seek through transform trees.
///!
///! # Offset Type Pattern
///!
///! Each layer defines its own offset type wrapping `usize`:
///!
///! ```ignore
///! #[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
///! pub struct InlayOffset(pub usize);
///! ```
///!
///! These types must implement `Add`, `Sub`, `AddAssign`, `SubAssign` to work
///! with SumTree's dimension system.
///!
///! # Coordinate Spaces
///!
///! - **Point**: Buffer coordinates (row, column) - already exists
///! - **InlayOffset**: Byte offset in buffer space
///! - **InlayPoint**: Point after inlay hints applied
///! - **FoldOffset**: Byte offset after folding
///! - **FoldPoint**: Point after folding
///! - And so on through the layer stack
///!
///! # Usage with SumTree
///!
///! ```ignore
///! let mut cursor = transforms.cursor::<InlayOffset>();
///! cursor.seek(&target_offset, Bias::Left, &());
///! ```
///!
///! The cursor uses these offset types as dimensions to efficiently navigate
///! the transform tree.
use std::ops::{Add, AddAssign, Sub, SubAssign};

/// Byte offset in buffer space (before any transformations).
///
/// This is the input coordinate space for InlayMap. It represents positions
/// as byte offsets from the start of the buffer.
#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq, Hash)]
pub struct BufferOffset(pub usize);

impl Add for BufferOffset {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for BufferOffset {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl AddAssign for BufferOffset {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl SubAssign for BufferOffset {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

/// Byte offset after inlay hints applied.
///
/// This is the output coordinate space for InlayMap and input space for FoldMap.
/// Inlays add visual text, increasing offsets for positions after the inlay.
#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq, Hash)]
pub struct InlayOffset(pub usize);

impl Add for InlayOffset {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for InlayOffset {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl AddAssign for InlayOffset {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl SubAssign for InlayOffset {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

/// Byte offset after code folding applied.
///
/// This is the output coordinate space for FoldMap and input space for TabMap.
/// Folded regions are hidden, reducing offsets for positions after folds.
#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq, Hash)]
pub struct FoldOffset(pub usize);

impl Add for FoldOffset {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for FoldOffset {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl AddAssign for FoldOffset {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl SubAssign for FoldOffset {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

/// Byte offset after tab expansion applied.
///
/// This is the output coordinate space for TabMap and input space for WrapMap.
/// Tabs expand to multiple spaces, increasing offsets.
#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq, Hash)]
pub struct TabOffset(pub usize);

impl Add for TabOffset {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for TabOffset {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl AddAssign for TabOffset {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl SubAssign for TabOffset {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

/// Byte offset after soft wrapping applied.
///
/// This is the output coordinate space for WrapMap and input space for BlockMap.
/// Wrapping doesn't change byte offsets, but changes visual row positions.
#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq, Hash)]
pub struct WrapOffset(pub usize);

impl Add for WrapOffset {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for WrapOffset {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl AddAssign for WrapOffset {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl SubAssign for WrapOffset {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

/// Byte offset after block decorations applied.
///
/// This is the output coordinate space for BlockMap - the final display offset.
/// Blocks insert visual rows between lines, changing row positions but not byte offsets.
#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq, Hash)]
pub struct BlockOffset(pub usize);

impl Add for BlockOffset {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for BlockOffset {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl AddAssign for BlockOffset {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl SubAssign for BlockOffset {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}
