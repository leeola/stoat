/// Default vertical scroll margin (in display rows) applied by
/// `Fit`, `Newest`, and `Focused` strategies. Matches the default
/// `vertical_scroll_margin` Zed exposes through its
/// `editor_settings`; wiring this through `stoat_config::Settings`
/// is deferred to its own item.
pub const DEFAULT_VERTICAL_SCROLL_MARGIN: f64 = 3.0;

/// Vertical autoscroll strategies. Callers request one via
/// [`crate::editor::Editor::request_autoscroll`]; the next layout
/// pass consumes the request and snaps `scroll_position.y` toward
/// the target row.
///
/// - `Fit`: scroll the minimum amount so the cursor span fits the viewport; no-op when already
///   inside.
/// - `Newest`: like `Fit` but always targets the newest cursor's row.
/// - `Center`: centre the cursor in the viewport.
/// - `Focused`: place the cursor near the top, capped by the vertical scroll margin.
/// - `Top`: cursor at the top.
/// - `Bottom`: cursor at the bottom.
/// - `TopRelative(n)`: cursor `n` rows below the top edge.
/// - `BottomRelative(n)`: cursor `n` rows above the bottom edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AutoscrollStrategy {
    Fit,
    Newest,
    #[default]
    Center,
    Focused,
    Top,
    Bottom,
    TopRelative(u32),
    BottomRelative(u32),
}

/// Compute the new vertical scroll position (in display rows) for a
/// pending autoscroll request. Inputs are all in display-row units;
/// the result is clamped to `[0.0, max_scroll_top]`.
///
/// - `current_y`: the editor's current `scroll_position.y`.
/// - `target_top_row` / `target_bottom_row`: the row range to bring into view. For a single cursor
///   at row `r`, pass `(r, r + 1)`.
/// - `visible_rows`: viewport height divided by line height.
/// - `max_scroll_top`: largest valid scroll top (typically `total_rows - visible_rows`, clamped at
///   zero).
/// - `vertical_margin`: padding kept between the cursor and the viewport edge for `Fit` / `Newest`
///   / `Focused`; ignored by the absolute strategies.
pub fn compute_autoscroll_y(
    strategy: AutoscrollStrategy,
    current_y: f64,
    target_top_row: f64,
    target_bottom_row: f64,
    visible_rows: f64,
    max_scroll_top: f64,
    vertical_margin: f64,
) -> f64 {
    let centered_margin = ((visible_rows - (target_bottom_row - target_top_row)) / 2.0).floor();
    let raw = match strategy {
        AutoscrollStrategy::Fit | AutoscrollStrategy::Newest => {
            let margin = centered_margin.min(vertical_margin).max(0.0);
            let target_top = (target_top_row - margin).max(0.0);
            let target_bottom = target_bottom_row + margin;
            let start_row = current_y;
            let end_row = start_row + visible_rows;
            let needs_scroll_up = target_top < start_row;
            let needs_scroll_down = target_bottom >= end_row;
            if needs_scroll_up && !needs_scroll_down {
                target_top
            } else if !needs_scroll_up && needs_scroll_down {
                target_bottom - visible_rows
            } else {
                current_y
            }
        },
        AutoscrollStrategy::Center => (target_top_row - centered_margin.max(0.0)).max(0.0),
        AutoscrollStrategy::Focused => {
            let margin = centered_margin.min(vertical_margin).max(0.0);
            (target_top_row - margin).max(0.0)
        },
        AutoscrollStrategy::Top => target_top_row.max(0.0),
        AutoscrollStrategy::Bottom => (target_bottom_row - visible_rows).max(0.0),
        AutoscrollStrategy::TopRelative(offset) => (target_top_row - offset as f64).max(0.0),
        AutoscrollStrategy::BottomRelative(offset) => {
            (target_bottom_row + offset as f64 - visible_rows).max(0.0)
        },
    };
    raw.clamp(0.0, max_scroll_top.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARGIN: f64 = DEFAULT_VERTICAL_SCROLL_MARGIN;

    #[test]
    fn strategy_default_is_center() {
        assert_eq!(AutoscrollStrategy::default(), AutoscrollStrategy::Center);
    }

    #[test]
    fn center_places_target_in_middle() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::Center,
            0.0,
            50.0,
            51.0,
            20.0,
            100.0,
            MARGIN,
        );
        assert_eq!(y, 41.0);
    }

    #[test]
    fn center_clamps_to_zero_when_target_above_viewport_center() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::Center,
            10.0,
            2.0,
            3.0,
            20.0,
            100.0,
            MARGIN,
        );
        assert_eq!(y, 0.0);
    }

    #[test]
    fn center_clamps_to_max_when_target_near_end() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::Center,
            0.0,
            98.0,
            99.0,
            20.0,
            100.0,
            MARGIN,
        );
        assert_eq!(y, 89.0);
    }

    #[test]
    fn top_places_target_at_zero_offset() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::Top,
            0.0,
            42.0,
            43.0,
            20.0,
            100.0,
            MARGIN,
        );
        assert_eq!(y, 42.0);
    }

    #[test]
    fn bottom_places_target_at_end_of_viewport() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::Bottom,
            0.0,
            42.0,
            43.0,
            20.0,
            100.0,
            MARGIN,
        );
        assert_eq!(y, 23.0);
    }

    #[test]
    fn top_relative_offsets_by_n_rows() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::TopRelative(5),
            0.0,
            42.0,
            43.0,
            20.0,
            100.0,
            MARGIN,
        );
        assert_eq!(y, 37.0);
    }

    #[test]
    fn bottom_relative_offsets_by_n_rows() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::BottomRelative(2),
            0.0,
            42.0,
            43.0,
            20.0,
            100.0,
            MARGIN,
        );
        assert_eq!(y, 25.0);
    }

    #[test]
    fn focused_uses_minimum_margin_above_target() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::Focused,
            0.0,
            42.0,
            43.0,
            20.0,
            100.0,
            MARGIN,
        );
        assert_eq!(y, 39.0);
    }

    #[test]
    fn fit_noop_when_target_already_visible() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::Fit,
            20.0,
            30.0,
            31.0,
            20.0,
            100.0,
            MARGIN,
        );
        assert_eq!(y, 20.0);
    }

    #[test]
    fn fit_scrolls_up_when_target_above_viewport() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::Fit,
            50.0,
            10.0,
            11.0,
            20.0,
            100.0,
            MARGIN,
        );
        assert_eq!(y, 7.0);
    }

    #[test]
    fn fit_scrolls_down_when_target_below_viewport() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::Fit,
            10.0,
            60.0,
            61.0,
            20.0,
            100.0,
            MARGIN,
        );
        assert_eq!(y, 44.0);
    }

    #[test]
    fn newest_scrolls_when_target_offscreen() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::Newest,
            0.0,
            50.0,
            51.0,
            20.0,
            100.0,
            MARGIN,
        );
        assert_eq!(y, 34.0);
    }

    #[test]
    fn clamps_result_to_max_scroll_top() {
        let y = compute_autoscroll_y(
            AutoscrollStrategy::Top,
            0.0,
            150.0,
            151.0,
            20.0,
            80.0,
            MARGIN,
        );
        assert_eq!(y, 80.0);
    }

    #[test]
    fn negative_max_scroll_top_clamps_to_zero() {
        let y = compute_autoscroll_y(AutoscrollStrategy::Top, 0.0, 5.0, 6.0, 20.0, -5.0, MARGIN);
        assert_eq!(y, 0.0);
    }
}
