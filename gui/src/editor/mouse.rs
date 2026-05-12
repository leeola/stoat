use gpui::{Pixels, Point, Size};

/// Translate a pixel point inside the editor's text region to a
/// `(row, col)` grid position. Used by the editor's
/// `on_mouse_down` handler (lands with the editor render item) to
/// build a [`crate::actions::ClickAt`] action.
///
/// Cells are addressed in monospace grid space: `cell_size.height`
/// is the line height and `cell_size.width` is the advance width
/// of one column. Negative or out-of-bounds points clamp to `(0,
/// 0)` -- the surrounding handler is responsible for filtering
/// pixel events that fell outside the text region before calling
/// in.
pub fn point_to_grid(point: Point<Pixels>, cell_size: Size<Pixels>) -> (u32, u32) {
    let cell_w: f32 = cell_size.width.into();
    let cell_h: f32 = cell_size.height.into();
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return (0, 0);
    }
    let x: f32 = point.x.into();
    let y: f32 = point.y.into();
    let col = (x.max(0.0) / cell_w) as u32;
    let row = (y.max(0.0) / cell_h) as u32;
    (row, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{px, size};

    #[test]
    fn origin_maps_to_zero_zero() {
        let cell = size(px(8.0), px(16.0));
        let result = point_to_grid(Point::new(px(0.0), px(0.0)), cell);
        assert_eq!(result, (0, 0));
    }

    #[test]
    fn integer_cells_round_down() {
        let cell = size(px(10.0), px(20.0));
        assert_eq!(point_to_grid(Point::new(px(15.0), px(45.0)), cell), (2, 1));
    }

    #[test]
    fn fractional_pixels_round_down() {
        let cell = size(px(8.0), px(16.0));
        assert_eq!(point_to_grid(Point::new(px(7.9), px(15.9)), cell), (0, 0));
        assert_eq!(point_to_grid(Point::new(px(8.1), px(16.1)), cell), (1, 1));
    }

    #[test]
    fn negative_pixels_clamp_to_zero() {
        let cell = size(px(8.0), px(16.0));
        assert_eq!(point_to_grid(Point::new(px(-5.0), px(-2.0)), cell), (0, 0));
    }

    #[test]
    fn zero_cell_size_returns_zero_zero() {
        let cell = size(px(0.0), px(0.0));
        assert_eq!(
            point_to_grid(Point::new(px(100.0), px(100.0)), cell),
            (0, 0)
        );
    }
}
