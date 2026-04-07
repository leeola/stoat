pub mod action_handlers;
pub mod app;
pub mod buffer;
mod buffer_registry;
pub mod command_palette;
pub mod diff_map;
pub mod display_map;
mod editor_state;
pub mod git;
pub mod host;
pub mod keymap;
pub mod multi_buffer;
pub mod pane;
pub mod ui;

pub use app::{Stoat, UpdateEffect};
#[cfg(test)]
mod test_harness;
pub use buffer::{BufferId, SharedBuffer, TextBuffer, TextBufferSnapshot};
pub use diff_map::{ChangeKind, ChangeSpan, DiffHunk, DiffHunkStatus, DiffMap, TokenDetail};
pub use display_map::{
    BlockMap, BlockPoint, BlockRow, BlockRowKind, BlockSnapshot, Chunk, ChunkRenderer,
    ChunkRendererId, ChunkReplacement, Crease, CreaseId, CreaseMap, CreaseSnapshot, DisplayMap,
    DisplayMapId, DisplayPoint, DisplayRow, DisplaySnapshot, FoldMap, FoldPlaceholder, FoldPoint,
    FoldSnapshot, HighlightKey, HighlightStyle, HighlightStyleId, HighlightStyleInterner,
    HighlightedChunk, Highlights, InlayHighlight, InlayHighlights, InlayId, InlayKind, InlayMap,
    InlayOffset, InlayPoint, InlaySnapshot, SemanticTokenHighlight, SemanticTokensHighlights,
    TabMap, TabPoint, TabRow, TabSnapshot, TextHighlights, WrapMap, WrapPoint, WrapSnapshot,
};
pub use git::DiffStatus;
pub use multi_buffer::{
    ExcerptBoundary, ExcerptId, ExcerptInfo, MultiBuffer, MultiBufferAnchor, MultiBufferPoint,
    MultiBufferRow, MultiBufferSnapshot,
};
pub use pane::{Axis, Direction, Pane, PaneId, PaneTree, Placement, View};
pub use stoat_log as log;
