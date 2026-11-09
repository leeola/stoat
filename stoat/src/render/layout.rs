use ratatui::layout::Rect;

/// Partition a pane's area into its content region and its status rect.
///
/// A pane's status shows in its window's status row, below the pane tree, so
/// the whole area is content and the status rect is empty. The empty rect sits
/// along the area's bottom edge, which is where a card anchored on a pane's
/// status lands above the window row.
pub(crate) fn split_pane_status(area: Rect) -> (Rect, Rect) {
    let status = Rect {
        x: area.x,
        y: area.y + area.height,
        width: area.width,
        height: 0,
    };
    (area, status)
}
