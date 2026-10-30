// Shared row placement, prepended to every pass that turns a cell row into a
// pixel y. WGSL has no include, and the passes must agree on where the last
// row paints, or the chrome on that row parts from the cells it marks.
//
// Every row before last_row keeps the uniform pitch, and the last row hangs
// from bottom_row_y. A window whose height is not a whole number of cells
// anchors that row to its bottom edge, so it covers the bottom of the row above
// it. A pool composite carries a last_row that no row reaches, which keeps all
// of its rows on the uniform pitch.

// The clip y that cuts nothing.
const NO_CLIP_Y: f32 = 1e9;

// The pixel y of row coordinate `row` in a grid of rows `cell_h` pixels tall
// whose last row, `last_row`, paints at `bottom_row_y`. A row past last_row
// hangs from bottom_row_y too.
fn place_row(row: f32, cell_h: f32, bottom_row_y: f32, last_row: u32) -> f32 {
    let last = f32(last_row);
    if row < last {
        return row * cell_h;
    }
    return bottom_row_y + (row - last) * cell_h;
}

// The pixel y of row coordinate `row` in this draw.
fn row_y(row: f32) -> f32 {
    return place_row(row, globals.cell_size.y, globals.bottom_row_y, globals.last_row);
}

// The pixel point of grid coordinate `at` in this draw, with its row placed by
// row_y.
fn cell_px(at: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(at.x * globals.cell_size.x, row_y(at.y));
}

// The pixel y, before any scroll, at and below which an element whose rows end
// at `end_row` paints nothing.
//
// An element that ends before the last row stops where the last row starts, so
// it never paints under that row. An element that reaches the last row draws
// whole. While bottom_row_y sits on the uniform pitch, the last row overlaps no
// row, and nothing is cut.
fn row_clip_y(end_row: f32) -> f32 {
    let last = f32(globals.last_row);
    let overlapped = globals.bottom_row_y < last * globals.cell_size.y;
    if overlapped && end_row <= last {
        return globals.bottom_row_y;
    }
    return NO_CLIP_Y;
}

// The bottom edge of a quad from `top` down `height` pixels, held at `clip_y`
// and never above the top, and the fraction of the height that edge keeps.
//
// A quad of one row ends at the last row this way in its vertex stage, with its
// texture or local coordinate scaled by the fraction, so no fragment pays for
// the cut.
fn clamp_bottom(top: f32, height: f32, clip_y: f32) -> vec2<f32> {
    let bottom = max(top, min(top + height, clip_y));
    let kept = select(1.0, (bottom - top) / height, height > 0.0);
    return vec2<f32>(bottom, kept);
}
