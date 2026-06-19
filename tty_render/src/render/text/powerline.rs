//! Procedural geometry for the powerline separators.
//!
//! The powerline range carries separators designed to fill a whole cell:
//! solid arrows, chevrons, rounded caps, slant triangles, diagonals, and
//! trapezoid dividers. Scaling a font's rasterized bitmap to the cell can never
//! reproduce them. The horizontal extent stays at the bitmap width, so an arrow
//! never reaches the cell's right edge and adjacent separators leave a seam.
//! Instead the geometric subset is drawn directly into a cell-sized coverage
//! mask here, so each shape fills its exact cell rect.
//!
//! Only the geometric subset (U+E0B0..=U+E0BF, U+E0D2, U+E0D4) is generated.
//! The stylized symbols (flames, ice, pixelated dividers) are left to the font,
//! matching rio's and ghostty's built-in sprite sets.

use std::f32::consts::FRAC_PI_2;

/// Subsample grid edge length per pixel. Coverage is the fraction of the
/// `SUBSAMPLES`x`SUBSAMPLES` samples inside the shape, giving cheap analytic AA;
/// the result is cached in the atlas, so the cost is paid once per size.
const SUBSAMPLES: u32 = 4;

/// Line segments per quarter arc when approximating the rounded caps.
const ARC_STEPS: u32 = 16;

/// Rasterize the powerline glyph `cp` into a `width`x`height` R8 coverage mask,
/// or `None` when `cp` is outside the geometric subset this module draws.
///
/// The mask fills the whole cell box, so callers place it at the exact, pixel-
/// snapped cell rect and separators abut with no seam.
pub(super) fn rasterize(cp: u32, width: u32, height: u32) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }

    let w = width as f32;
    let h = height as f32;
    let thick = thickness(height);
    let mut canvas = Canvas::new(width, height);

    match cp {
        0xE0B0 => canvas.fill_polygon(&[(0.0, 0.0), (w, h / 2.0), (0.0, h)]),
        0xE0B2 => canvas.fill_polygon(&[(w, 0.0), (0.0, h / 2.0), (w, h)]),

        0xE0B1 => canvas.stroke_polyline(&[(0.0, 0.0), (w, h / 2.0), (0.0, h)], thick),
        0xE0B3 => canvas.stroke_polyline(&[(w, 0.0), (0.0, h / 2.0), (w, h)], thick),

        0xE0B4 => canvas.fill_polygon(&rounded_cap(w, h)),
        0xE0B5 => canvas.stroke_polyline(&rounded_cap(w, h), thick),
        0xE0B6 => {
            canvas.fill_polygon(&rounded_cap(w, h));
            canvas.flip_horizontal();
        },
        0xE0B7 => {
            canvas.stroke_polyline(&rounded_cap(w, h), thick);
            canvas.flip_horizontal();
        },

        0xE0B8 => canvas.fill_polygon(&[(0.0, 0.0), (w, h), (0.0, h)]),
        0xE0BA => canvas.fill_polygon(&[(w, 0.0), (w, h), (0.0, h)]),
        0xE0BC => canvas.fill_polygon(&[(0.0, 0.0), (w, 0.0), (0.0, h)]),
        0xE0BE => canvas.fill_polygon(&[(0.0, 0.0), (w, 0.0), (w, h)]),

        0xE0B9 | 0xE0BF => canvas.stroke_polyline(&diagonal_ul_lr(w, h), thick),
        0xE0BB | 0xE0BD => canvas.stroke_polyline(&diagonal_ur_ll(w, h), thick),

        0xE0D2 => trapezoids(&mut canvas, w, h, thick),
        0xE0D4 => {
            trapezoids(&mut canvas, w, h, thick);
            canvas.flip_horizontal();
        },

        _ => return None,
    }

    Some(canvas.into_bytes())
}

/// Whether `cp` is a powerline separator this module draws procedurally, as
/// opposed to a stylized symbol (flame, ice, pixelated divider) left to the
/// font. Lets callers skip the rasterize attempt for the common cell-fill
/// codepoints that are not powerline geometry.
pub(super) fn is_geometric(cp: u32) -> bool {
    matches!(cp, 0xE0B0..=0xE0BF | 0xE0D2 | 0xE0D4)
}

/// Stroke thickness in pixels for the hollow separators, tied to the cell height
/// so it matches the body underline weight (`height / 16`, floored at one pixel).
fn thickness(height: u32) -> f32 {
    (height as f32 / 16.0).round().max(1.0)
}

/// The right-facing rounded cap outline, from the top-left corner down the
/// bulging right side to the bottom-left corner.
///
/// Returned open so [`Canvas::stroke_polyline`] traces only the curve while
/// [`Canvas::fill_polygon`] closes it across the flat left edge. The radius is
/// `min(width, height / 2)`, so a typical cell (width is height / 2) yields a
/// true semicircle and a wider cell a stadium with straight sides.
fn rounded_cap(w: f32, h: f32) -> Vec<(f32, f32)> {
    let r = w.min(h / 2.0);
    let mut points = Vec::with_capacity((ARC_STEPS * 2 + 2) as usize);

    for step in 0..=ARC_STEPS {
        let angle = -FRAC_PI_2 + (step as f32 / ARC_STEPS as f32) * FRAC_PI_2;
        points.push((r * angle.cos(), r + r * angle.sin()));
    }
    for step in 0..=ARC_STEPS {
        let angle = (step as f32 / ARC_STEPS as f32) * FRAC_PI_2;
        points.push((r * angle.cos(), (h - r) + r * angle.sin()));
    }

    points
}

/// Endpoints of the upper-left to lower-right diagonal, overshooting the cell
/// box slightly so the strokes of adjacent cells join without a gap.
fn diagonal_ul_lr(w: f32, h: f32) -> [(f32, f32); 2] {
    let (sx, sy) = diagonal_overshoot(w, h);
    [(-sx, -sy), (w + sx, h + sy)]
}

/// Endpoints of the upper-right to lower-left diagonal, overshooting as in
/// [`diagonal_ul_lr`].
fn diagonal_ur_ll(w: f32, h: f32) -> [(f32, f32); 2] {
    let (sx, sy) = diagonal_overshoot(w, h);
    [(w + sx, -sy), (-sx, h + sy)]
}

fn diagonal_overshoot(w: f32, h: f32) -> (f32, f32) {
    (0.5 * (w / h).min(1.0), 0.5 * (h / w).min(1.0))
}

/// Draw the two trapezoid halves of the slanted divider, split by a gap of one
/// stroke thickness across the cell's vertical middle.
fn trapezoids(canvas: &mut Canvas, w: f32, h: f32, thick: f32) {
    let gap = thick / 2.0;
    canvas.fill_polygon(&[
        (0.0, 0.0),
        (w, 0.0),
        (w / 2.0, h / 2.0 - gap),
        (0.0, h / 2.0 - gap),
    ]);
    canvas.fill_polygon(&[
        (0.0, h),
        (w, h),
        (w / 2.0, h / 2.0 + gap),
        (0.0, h / 2.0 + gap),
    ]);
}

/// A single-channel coverage buffer the powerline shapes draw into.
struct Canvas {
    width: u32,
    height: u32,
    /// R8 coverage, row-major, length `width * height`.
    data: Vec<u8>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Canvas {
        Canvas {
            width,
            height,
            data: vec![0u8; width as usize * height as usize],
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    /// Fill a simple polygon (even-odd rule), max-compositing antialiased
    /// coverage. The points are treated as closed: the last connects to the
    /// first.
    fn fill_polygon(&mut self, points: &[(f32, f32)]) {
        self.composite(|x, y| point_in_polygon(x, y, points));
    }

    /// Stroke an open polyline `thickness` pixels wide, max-compositing
    /// antialiased coverage.
    fn stroke_polyline(&mut self, points: &[(f32, f32)], thickness: f32) {
        let half = thickness / 2.0;
        self.composite(|x, y| distance_to_polyline(x, y, points) <= half);
    }

    /// Mirror the buffer left-to-right, turning a right-facing shape into its
    /// left-facing variant.
    fn flip_horizontal(&mut self) {
        for row in self.data.chunks_mut(self.width as usize) {
            row.reverse();
        }
    }

    /// Supersample `inside` over each pixel and max-composite the coverage so
    /// successive draws into one canvas accumulate rather than overwrite.
    fn composite(&mut self, inside: impl Fn(f32, f32) -> bool) {
        let total = SUBSAMPLES * SUBSAMPLES;
        for py in 0..self.height {
            for px in 0..self.width {
                let mut hits = 0;
                for sy in 0..SUBSAMPLES {
                    for sx in 0..SUBSAMPLES {
                        let x = px as f32 + (sx as f32 + 0.5) / SUBSAMPLES as f32;
                        let y = py as f32 + (sy as f32 + 0.5) / SUBSAMPLES as f32;
                        if inside(x, y) {
                            hits += 1;
                        }
                    }
                }

                if hits == 0 {
                    continue;
                }

                let coverage = ((hits * 255 + total / 2) / total) as u8;
                let index = (py * self.width + px) as usize;
                if coverage > self.data[index] {
                    self.data[index] = coverage;
                }
            }
        }
    }
}

/// Whether `(x, y)` lies inside the polygon, by the even-odd ray-cast rule.
fn point_in_polygon(x: f32, y: f32, points: &[(f32, f32)]) -> bool {
    let n = points.len();
    if n < 3 {
        return false;
    }

    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = points[i];
        let (xj, yj) = points[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Shortest distance from `(x, y)` to any segment of the open polyline, or
/// infinity for a polyline with no segments.
fn distance_to_polyline(x: f32, y: f32, points: &[(f32, f32)]) -> f32 {
    points
        .windows(2)
        .map(|segment| distance_to_segment(x, y, segment[0], segment[1]))
        .fold(f32::INFINITY, f32::min)
}

fn distance_to_segment(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let len_sq = dx * dx + dy * dy;
    if len_sq == 0.0 {
        return ((x - a.0).powi(2) + (y - a.1).powi(2)).sqrt();
    }

    let t = (((x - a.0) * dx + (y - a.1) * dy) / len_sq).clamp(0.0, 1.0);
    let cx = a.0 + t * dx;
    let cy = a.1 + t * dy;
    ((x - cx).powi(2) + (y - cy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::{is_geometric, rasterize};

    /// Coverage byte at pixel `(col, row)` of a `width`-wide mask.
    fn at(mask: &[u8], width: u32, col: u32, row: u32) -> u8 {
        mask[(row * width + col) as usize]
    }

    #[test]
    fn non_geometric_codepoints_are_left_to_the_font() {
        assert!(rasterize(0xE0C0, 16, 32).is_none(), "flame");
        assert!(rasterize(0xE0D3, 16, 32).is_none(), "stylized divider");
        assert!(rasterize(u32::from('A'), 16, 32).is_none(), "letter");
        assert!(rasterize(0xE0B0, 0, 32).is_none(), "zero width");
    }

    #[test]
    fn is_geometric_agrees_with_rasterize_across_the_range() {
        for cp in 0xE0B0..=0xE0D4 {
            assert_eq!(
                is_geometric(cp),
                rasterize(cp, 16, 32).is_some(),
                "{cp:#06x} routing must match what rasterize draws"
            );
        }
    }

    #[test]
    fn right_arrow_fills_left_edge_and_reaches_the_right_edge() {
        let (w, h) = (16, 32);
        let mask = rasterize(0xE0B0, w, h).expect("E0B0 is geometric");

        // The full-height left edge is the arrow's base: solid top to bottom.
        assert!(at(&mask, w, 0, 8) > 200, "left edge upper covered");
        assert!(at(&mask, w, 0, 16) > 200, "left edge middle covered");
        assert!(at(&mask, w, 0, 24) > 200, "left edge lower covered");

        // The apex reaches the cell's right edge at mid-height; this is the seam
        // the old vertical-only scaling left behind.
        assert!(at(&mask, w, w - 2, 16) > 200, "apex reaches right edge");

        // Above the upper edge the cell stays empty, so the arrow is a triangle.
        assert!(at(&mask, w, w - 2, 1) < 40, "top-right empty");
    }

    #[test]
    fn left_arrow_mirrors_the_right_arrow() {
        let (w, h) = (16, 32);
        let mask = rasterize(0xE0B2, w, h).expect("E0B2 is geometric");

        assert!(at(&mask, w, w - 1, 8) > 200, "right edge is the base");
        assert!(at(&mask, w, 1, 16) > 200, "apex reaches the left edge");
        assert!(at(&mask, w, 1, 1) < 40, "top-left empty");
    }

    #[test]
    fn rounded_caps_face_opposite_directions() {
        let (w, h) = (16, 32);
        let right = rasterize(0xE0B4, w, h).expect("E0B4 is geometric");
        let left = rasterize(0xE0B6, w, h).expect("E0B6 is geometric");

        // The right cap's flat side is the full-height left edge; the left cap is
        // its mirror, so its left edge is the bulge tip and empty near the top.
        assert!(
            at(&right, w, 0, 4) > 200,
            "right cap flat left edge covered"
        );
        assert!(at(&left, w, 0, 4) < 40, "left cap left edge empty near top");
        assert!(
            at(&left, w, w - 1, 4) > 200,
            "left cap flat right edge covered"
        );
    }
}
