///! DisplayMap coordinate transformation system for Stoat.
///!
///! DisplayMap is a layered pipeline that transforms raw buffer coordinates ([`text::Point`])
///! into visual display coordinates ([`DisplayPoint`]), handling:
///!
///! - **Inlay hints**: Type annotations shown inline (InlayMap)
///! - **Code folding**: Hidden regions like collapsed function bodies (FoldMap)
///! - **Tab expansion**: Tab characters rendered as multiple spaces (TabMap)
///! - **Soft wrapping**: Long lines wrapped to fit editor width (WrapMap)
///! - **Block decorations**: Visual elements between lines like diagnostics (BlockMap)
///!
///! # Architecture
///!
///! The transformation pipeline consists of six layers, each with its own coordinate space:
///!
///! ```text
///! Point (buffer)
///!   | InlayMap
///! InlayPoint
///!   | FoldMap
///! FoldPoint
///!   | TabMap
///! TabPoint
///!   | WrapMap
///! WrapPoint
///!   | BlockMap
///! BlockPoint (display)
///! ```
///!
///! Each layer:
///! - Maintains its own coordinate space with type-safe coordinate types
///! - Provides bidirectional conversion to adjacent layers
///! - Handles buffer edits incrementally using [`sum_tree::SumTree`]
///! - Achieves O(log n) coordinate conversions
///!
///! # Type Safety
///!
///! Coordinate types are distinct to prevent mixing coordinate spaces. The compiler
///! enforces correct usage:
///!
///! ```compile_fail
///! let inlay_point = InlayPoint { row: 10, column: 5 };
///! let fold_point = FoldPoint { row: 10, column: 5 };
///! // This won't compile - different types!
///! assert_eq!(inlay_point, fold_point);
///! ```
///!
///! # Usage
///!
///! Each layer implements [`CoordinateTransform`] for bidirectional conversion:
///!
///! ```ignore
///! // TabMap transforms FoldPoint <-> TabPoint
///! let tab_map = TabMap::new(fold_map, tab_width);
///! let tab_point = tab_map.to_coords(fold_point);
///! let back = tab_map.from_coords(tab_point);
///! ```
///!
///! # Related
///!
///! - See `.claude/DISPLAY_MAP.md` for full implementation plan
///! - Based on Zed's editor DisplayMap architecture
///! - Uses [`sum_tree::SumTree`] for efficient coordinate queries
mod block_map;
mod buffer_utils;
mod coords;
mod crease_map;
mod dimensions;
mod display_map;
mod fold_map;
mod inlay_map;
mod tab_map;
mod traits;
mod transform;
mod wrap_map;

// Re-export text crate types for convenience
pub use block_map::{
    BlockMap, BlockPlacement, BlockProperties, BlockSnapshot, BlockStyle, CustomBlock,
    CustomBlockId,
};
pub use coords::{BlockPoint, DisplayPoint, FoldPoint, InlayPoint, TabPoint, WrapPoint};
pub use crease_map::{Crease, CreaseId, CreaseMap, CreaseSnapshot};
pub use dimensions::{BlockOffset, BufferOffset, FoldOffset, InlayOffset, TabOffset, WrapOffset};
pub use display_map::{DisplayMap, DisplaySnapshot};
pub use fold_map::{Fold, FoldMap, FoldSnapshot};
pub use inlay_map::{InlayId, InlayMap, InlaySnapshot};
pub use sum_tree::Bias;
pub use tab_map::{TabMap, TabSnapshot};
pub use text::{BufferSnapshot, Point};
pub use traits::{CoordinateTransform, EditableLayer};
pub use transform::{Isomorphic, TransformSummary};
pub use wrap_map::{WrapMap, WrapSnapshot};

/// Edit operations on the buffer using Point coordinates
pub type BufferEdit = text::Edit<Point>;
