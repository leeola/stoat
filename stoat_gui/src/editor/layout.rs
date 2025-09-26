use gpui::{Bounds, Pixels, ShapedLine};
use smallvec::SmallVec;

/// Layout state computed in prepaint
pub struct EditorLayout {
    /// The shaped lines ready to paint
    pub lines: SmallVec<[PositionedLine; 32]>,
    /// Total bounds of the editor
    pub bounds: Bounds<Pixels>,
    /// Content area (excluding padding)
    pub _content_bounds: Bounds<Pixels>,
    /// Line height for positioning
    pub _line_height: Pixels,
}

/// A shaped line with its rendering position
pub struct PositionedLine {
    pub shaped: ShapedLine,
    pub position: gpui::Point<Pixels>,
}
