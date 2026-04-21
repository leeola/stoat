use ratatui::layout::Rect;

/// Partition a pane's area into its content region and the 1-row status bar
/// at the bottom. For panes shorter than 2 rows there is no room for a
/// status bar, so the full area is returned as content.
pub(crate) fn split_pane_status(area: Rect) -> (Rect, Rect) {
    if area.height < 2 {
        return (
            area,
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 0,
            },
        );
    }
    let content = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height - 1,
    };
    let status = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    (content, status)
}
