//! Hand-drawn geometry, generated from a shape declaration.
//!
//! A sketch arrives as a shape and a roughness, never as points. This turns one
//! into flattened polylines in physical pixels, so the wobble is computed
//! against the live cell metrics and a mark redrawn at another font size looks
//! hand-drawn at that size rather than scaled.
//!
//! The reference states every jitter in logical pixels, so the generation runs
//! on a cell divided by the display density and scales its points back out. A
//! mark therefore wanders the same visible distance on a 1x display and on a 2x
//! one.
//!
//! Nothing here reads a clock or a GPU. The same seed yields the same points
//! every call, which is what lets a reveal walk arc length instead of time and
//! what lets a mark be regenerated on a zoom step without appearing to redraw
//! itself.
//!
//! The math is transcribed from rough.js, whose wobble is a chain of draws from
//! one shared pseudo-random stream. The *order* of those draws is part of the
//! output, so the transcription follows the reference's call order rather than
//! the order that reads most naturally.

use crate::render::CellMetrics;
use std::f64::consts::PI;
use stoatty_protocol::command::{
    SketchBounds, SketchCommand, SketchEnd, SketchFill, SketchFillStyle, SketchShape, SketchSide,
};

/// How far a flattened chord may sit from the curve it replaces, in logical
/// pixels.
///
/// A quarter pixel is under what a stroke's own anti-aliased edge resolves, so
/// the facets a coarser flattening leaves are gone rather than merely small.
const BEZIER_TOLERANCE: f64 = 0.25;

/// Sixteenths of a cell, which is the unit the protocol states bounds in.
const CELL_FRACTION: f64 = 16.0;

/// 256ths of a cell width, which is the unit the protocol states stroke widths
/// in.
const WIDTH_FRACTION: f32 = 256.0;

/// The roughness byte that means rough.js roughness 1.0.
const ROUGHNESS_UNIT: f64 = 64.0;

/// 64ths of the chord, which is the unit the protocol states a bend in.
const BEND_FRACTION: f64 = 64.0;

/// How far a connector's control point reaches along its side's normal, as a
/// fraction of the distance the other end lies that way.
///
/// A half reach leaves each box facing outward for long enough to read as
/// leaving it, and short enough that the pen is turning by the time it is out.
const FORWARD_REACH: f64 = 0.5;

/// How far the control point reaches where the other end lies behind the side,
/// as a multiple of the root of that distance in pixels.
///
/// The root is what keeps a target just behind the side from throwing the pen
/// as far as a distant one does. A reach following the distance signed would
/// send the pen away from the target and back.
const HOOK_REACH: f64 = 6.25;

/// How far along a side a connector may meet it, as a fraction of the side's
/// length held back from either end.
///
/// A point near a corner reads as missing the box rather than arriving at it,
/// and a rounded corner has no outline there to arrive on.
const SIDE_INSET: f64 = 0.2;

/// How far outside a mark's outline a connector's end sits, in pixels.
pub(crate) const COMPONENT_GAP: f64 = 4.0;

/// The box a connector points at, and how its outline rounds inside it.
///
/// A connector meets the outline rather than the box. A ring's own curve lies
/// inside its bounds everywhere but the four midpoints, so a point taken off
/// the box floats outside the mark, and a rounded corner has no outline at the
/// corner of its box at all.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct ComponentBox {
    pub(crate) bounds: [f32; 4],
    pub(crate) rounding: Rounding,
}

/// How a component's outline rounds inside its bounds.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Rounding {
    /// The ellipse those bounds inscribe.
    Ellipse,
    /// A rectangle whose corners round by `radius_px`.
    Rect { radius_px: f32 },
}

impl ComponentBox {
    /// The same box in the logical frame the generation runs in.
    fn logical(self, scale: f32) -> ComponentBox {
        ComponentBox {
            bounds: self.bounds.map(|edge| edge / scale),
            rounding: match self.rounding {
                Rounding::Ellipse => Rounding::Ellipse,
                Rounding::Rect { radius_px } => Rounding::Rect {
                    radius_px: radius_px / scale,
                },
            },
        }
    }
}

/// Flattened geometry for one mark, ready to stroke.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Geometry {
    /// Base and overlay strokes, adjacent in pairs, so a reveal that advances
    /// stroke `i` advances the overlay that doubles it in the same step.
    pub(crate) strokes: Vec<Stroke>,
    /// The quadrilateral a filled box paints under its stroke, or `None` for an
    /// open shape.
    pub(crate) fill: Option<[[f32; 2]; 4]>,
}

/// One continuous pen-down path, in physical pixels.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Stroke {
    pub(crate) points: Vec<[f32; 2]>,
    /// Distance along the path at each point, so `lengths[i]` is the arc length
    /// from the start to `points[i]` and the last entry is the whole length.
    ///
    /// Carried rather than recomputed because the reveal asks "where is the pen
    /// at fraction t" every frame, and a prefix sum answers by binary search.
    pub(crate) lengths: Vec<f32>,
    /// Multiplier on the mark's stroke weight.
    ///
    /// One for an outline. A hatch line is half as heavy, which is the weight
    /// the reference fills at, so the fill reads as shading rather than as a
    /// second outline.
    pub(crate) weight: f32,
    /// Straight color and alpha to draw in, or `None` for the mark's own.
    ///
    /// A hatch line takes the fill's color, so one mark's strokes can carry two
    /// colors while still blending as one shape.
    pub(crate) color: Option<[u8; 4]>,
}

impl Stroke {
    fn new(points: Vec<[f32; 2]>) -> Stroke {
        Stroke::weighted(points, 1.0, None)
    }

    fn weighted(points: Vec<[f32; 2]>, weight: f32, color: Option<[u8; 4]>) -> Stroke {
        let mut lengths = Vec::with_capacity(points.len());
        let mut total = 0.0;
        for (index, point) in points.iter().enumerate() {
            if index > 0 {
                let previous = points[index - 1];
                total +=
                    ((point[0] - previous[0]).powi(2) + (point[1] - previous[1]).powi(2)).sqrt();
            }
            lengths.push(total);
        }
        Stroke {
            points,
            lengths,
            weight,
            color,
        }
    }
}

/// rough.js's pseudo-random stream.
///
/// A Lehmer generator, transcribed rather than swapped for a better one: the
/// wobble it produces is what makes a mark look hand-drawn, and another
/// generator's distribution changes the look.
struct Random {
    seed: i32,
}

impl Random {
    /// A stream for `seed`, deriving one from `id` when the emitter had no
    /// opinion.
    ///
    /// A zero seed is what an emitter sends to mean "you pick". Deriving from
    /// the id rather than from a clock keeps the mark stable across redraws,
    /// which is the whole reason the field exists.
    fn new(seed: u32, id: u32) -> Random {
        let seed = match seed {
            0 => id.wrapping_mul(0x9E37_79B9) | 1,
            other => other,
        };
        Random {
            seed: seed as i32 & 0x7FFF_FFFF,
        }
    }

    fn next(&mut self) -> f64 {
        self.seed = self.seed.wrapping_mul(48271) & 0x7FFF_FFFF;
        f64::from(self.seed) / 2_147_483_648.0
    }
}

/// The knobs rough.js reads while it wobbles a path.
///
/// Every field but [`Self::roughness`], [`Self::preserve_vertices`], and an
/// ellipse's [`Self::curve_fitting`] keeps the reference's default, because
/// those defaults are tuned together and one changed in isolation stops looking
/// hand-drawn.
struct Options {
    roughness: f64,
    bowing: f64,
    max_randomness_offset: f64,
    curve_tightness: f64,
    /// How closely an ellipse's radii hug the ones asked for, the shortfall
    /// being how far they wander. Only [`ellipse`] reads it.
    curve_fitting: f64,
    curve_step_count: f64,
    /// Pin a segment's endpoints to where they were asked for, so a box's
    /// corners meet. rough.js leaves them free, which reads as sketchier and
    /// leaves visible gaps at low roughness.
    preserve_vertices: bool,
    /// Set per segment from its length, so a long line wobbles less per pixel
    /// than a short one.
    gain: f64,
}

impl Options {
    fn new(roughness: f64) -> Options {
        Options {
            roughness,
            bowing: 1.0,
            max_randomness_offset: 2.0,
            curve_tightness: 0.0,
            curve_fitting: 0.95,
            curve_step_count: 9.0,
            preserve_vertices: roughness < 2.0,
            gain: 1.0,
        }
    }

    fn offset(&self, min: f64, max: f64, random: &mut Random) -> f64 {
        self.roughness * self.gain * (random.next() * (max - min) + min)
    }

    fn offset_opt(&self, x: f64, random: &mut Random) -> f64 {
        self.offset(-x, x, random)
    }
}

/// One drawing instruction, as rough.js emits them.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Op {
    Move([f64; 2]),
    /// A cubic bezier's two control points and its endpoint.
    Curve([f64; 6]),
}

/// The ellipse one pass rides, after the curve fitting has nudged its radii.
struct Ring {
    increment: f64,
    cx: f64,
    cy: f64,
    rx: f64,
    ry: f64,
}

/// A connector's declaration, unpacked from its shape variant.
struct Connector {
    from: SketchEnd,
    to: SketchEnd,
    bend: i8,
    heads: u8,
}

/// Which roughness exemptions a shape qualifies for.
///
/// Only the shape's own type decides this, so [`rough_kind`] reads it once
/// where the shape is already at hand rather than [`damped_roughness`]
/// inspecting a shape it otherwise has no use for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RoughKind {
    /// A connector, which passes its two deltas as its sides.
    Linear,
    /// A box with rounded corners.
    Round,
    Other,
}

/// Generate one mark's flattened geometry at the current cell size.
///
/// The shape arrives in cell fractions and leaves in physical pixels. The
/// wobble must be applied against the cell, because a mark generated once and
/// then scaled carries a wobble that grows with the font and stops looking
/// hand-drawn.
///
/// Every wobble constant is stated in logical pixels. The generation therefore
/// runs on a cell divided by the display density, and the points scale back to
/// physical at the end. A roughness-1 mark then wanders the same visible
/// distance on a 1x display and on a 2x one.
///
/// The same command and metrics always yield the same geometry. Nothing here
/// reads a clock, so a mark regenerated on a zoom step does not appear to
/// redraw itself.
pub(crate) fn geometry<Resolve>(
    command: &SketchCommand,
    metrics: CellMetrics,
    resolve: &Resolve,
) -> Geometry
where
    Resolve: Fn(u32) -> Option<ComponentBox>,
{
    let scale = f64::from(metrics.scale_factor);
    let (cw, ch) = (
        f64::from(metrics.width) / scale,
        f64::from(metrics.height) / scale,
    );
    // A component's bounds arrive in physical pixels, so they join the logical
    // frame the rest of the generation runs in.
    let resolve = |id: u32| resolve(id).map(|component| component.logical(metrics.scale_factor));
    let mut random = Random::new(command.style.seed, command.id);

    match command.shape {
        SketchShape::Ellipse { bounds, fill } => {
            let (x, y, w, h) = pixel_bounds(bounds, cw, ch);
            let mut options = shape_options(command, w, h);
            // An ellipse alone pins its fitting, so its radii do not wander and
            // the ring hugs the box it was declared with. A focus ring that
            // wanders 5% cuts into the word it circles. The radii draws still
            // consume their two stream values, so every later draw stays put.
            options.curve_fitting = 1.0;

            // The reference lays a fill before the outline it sits under, and
            // both draw from one stream, so the hatch has to run first.
            let mut strokes = hatch_strokes(
                &ellipse_polygon(x, y, w, h),
                fill,
                hatch_gap(command, cw),
                scale,
                &mut options,
                &mut random,
            );
            let ops = ellipse(x + w / 2.0, y + h / 2.0, w, h, &mut options, &mut random);
            strokes.extend(flatten(&ops, scale));

            Geometry {
                strokes,
                fill: None,
            }
        },
        SketchShape::Rect {
            bounds,
            radius,
            fill,
        } => {
            let (x, y, w, h) = pixel_bounds(bounds, cw, ch);
            let mut options = shape_options(command, w, h);

            let mut strokes = hatch_strokes(
                &[[x, y], [x + w, y], [x + w, y + h], [x, y + h]],
                fill,
                hatch_gap(command, cw),
                scale,
                &mut options,
                &mut random,
            );
            let ops = rect(
                x,
                y,
                w,
                h,
                f64::from(radius) / CELL_FRACTION * cw,
                &mut options,
                &mut random,
            );
            strokes.extend(flatten(&ops, scale));

            Geometry {
                strokes,
                // Drawn after the stroke so the stroke's own geometry does not
                // move when a box gains or loses its fill. A hatched box has no
                // quad: its fill is the strokes above.
                fill: fill
                    .filter(|fill| fill.style == SketchFillStyle::Solid)
                    .map(|_| {
                        jittered_quad(x, y, w, h, &options, &mut random)
                            .map(|corner| corner.map(|edge| edge * metrics.scale_factor))
                    }),
            }
        },
        SketchShape::Line {
            from,
            to,
            bend,
            heads,
        } => {
            let connector = Connector {
                from,
                to,
                bend,
                heads,
            };
            line_geometry(command, connector, cw, ch, scale, &resolve, &mut random)
        },
    }
}

/// The four corners of a filled box, each nudged like the stroke around it.
///
/// A crisp rectangle behind a wobbling outline reads machine-cut, which is the
/// one place a fill gives the drawing away.
///
/// The nudge is capped at a quarter of the shorter side, because the shader
/// resolves the fill as a convex quad and a corner that crossed its neighbour
/// turns the shape inside out.
fn jittered_quad(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    options: &Options,
    random: &mut Random,
) -> [[f32; 2]; 4] {
    let reach = options
        .max_randomness_offset
        .min(w.abs().min(h.abs()) / 4.0);
    let mut corner = |cx: f64, cy: f64| {
        [
            (cx + options.offset_opt(reach, random)) as f32,
            (cy + options.offset_opt(reach, random)) as f32,
        ]
    };

    [
        corner(x, y),
        corner(x + w, y),
        corner(x + w, y + h),
        corner(x, y + h),
    ]
}

/// Turn a mark's declared box into a pixel rectangle.
///
/// A cell is taller than it is wide, so a horizontal fraction and a vertical
/// one scale by different numbers. A mark spanning three cells across and three
/// down covers the same cells at every font size, which is what the unit is
/// for.
fn pixel_bounds(bounds: SketchBounds, cw: f64, ch: f64) -> (f64, f64, f64, f64) {
    (
        f64::from(bounds.x) / CELL_FRACTION * cw,
        f64::from(bounds.y) / CELL_FRACTION * ch,
        f64::from(bounds.w) / CELL_FRACTION * cw,
        f64::from(bounds.h) / CELL_FRACTION * ch,
    )
}

/// The knobs for one shape, with its roughness already damped for its size.
fn shape_options(command: &SketchCommand, w: f64, h: f64) -> Options {
    let declared = f64::from(command.style.roughness) / ROUGHNESS_UNIT;
    Options::new(damped_roughness(declared, w, h, rough_kind(&command.shape)))
}

/// Which of the roughness exemptions `shape` qualifies for.
///
/// An ellipse is not round in the reference's sense. Its `canChangeRoundness`
/// covers rectangles, lines, diamonds, and images, so an ellipse damps by size
/// alone.
fn rough_kind(shape: &SketchShape) -> RoughKind {
    match shape {
        SketchShape::Line { .. } => RoughKind::Linear,
        SketchShape::Rect { radius, .. } if *radius > 0 => RoughKind::Round,
        _ => RoughKind::Other,
    }
}

/// Stroke thickness in physical pixels, from a weight in 256ths of a cell.
///
/// Stated against the cell width rather than in pixels, so a mark keeps its
/// apparent weight through a font-size change instead of thinning as the text
/// grows.
///
/// Takes the weight rather than the style, because a re-declared mark eases
/// toward its new weight and draws at a fractional one on the way.
pub(crate) fn stroke_width(width_256ths: f32, metrics: CellMetrics) -> f32 {
    width_256ths / WIDTH_FRACTION * metrics.width
}

/// A connector, with the arrowheads its `heads` mask asks for.
fn line_geometry<Resolve>(
    command: &SketchCommand,
    connector: Connector,
    cw: f64,
    ch: f64,
    scale: f64,
    resolve: &Resolve,
    random: &mut Random,
) -> Geometry
where
    Resolve: Fn(u32) -> Option<ComponentBox>,
{
    let Connector {
        from,
        to,
        bend,
        heads,
    } = connector;
    let empty = Geometry {
        strokes: Vec::new(),
        fill: None,
    };
    // Each end's attachment needs the other end's position, and an end whose
    // side is Auto has no position until that side is picked. The two centers
    // break the circle: they are known before either side is.
    let (Some(from_center), Some(to_center)) = (
        end_center(from, cw, ch, resolve),
        end_center(to, cw, ch, resolve),
    ) else {
        return empty;
    };
    let (Some((start, from_side)), Some((end, to_side))) = (
        attach(from, cw, ch, to_center, resolve),
        attach(to, cw, ch, from_center, resolve),
    ) else {
        return empty;
    };

    let chord = ((end[0] - start[0]).powi(2) + (end[1] - start[1]).powi(2)).sqrt();
    if chord <= f64::EPSILON {
        return empty;
    }

    let both_components =
        matches!(from, SketchEnd::Component { .. }) && matches!(to, SketchEnd::Component { .. });
    let mut options = shape_options(command, end[0] - start[0], end[1] - start[1]);

    let mut ops = match (bend, both_components) {
        (0, false) => double_line(start[0], start[1], end[0], end[1], &mut options, random),
        (0, true) => {
            let [first, second] = s_curve_controls(start, end, from_side, to_side);
            cubic(start, first, second, end, &options, random)
        },
        (bend, _) => {
            let bow = f64::from(bend) / BEND_FRACTION * chord;
            let normal = [-(end[1] - start[1]) / chord, (end[0] - start[0]) / chord];
            let mid = [
                (start[0] + end[0]) / 2.0 + normal[0] * bow,
                (start[1] + end[1]) / 2.0 + normal[1] * bow,
            ];
            curve(&[start, mid, end], &options, random)
        },
    };

    if heads & 1 != 0 {
        let angle = (start[1] - end[1]).atan2(start[0] - end[0]);
        ops.extend(arrow_head(start, angle, chord, &mut options, random));
    }
    if heads & 2 != 0 {
        let angle = (end[1] - start[1]).atan2(end[0] - start[0]);
        ops.extend(arrow_head(end, angle, chord, &mut options, random));
    }

    Geometry {
        strokes: flatten(&ops, scale),
        fill: None,
    }
}

/// A connector's two control points when both ends name components.
///
/// Each pushes out along the side its end meets, by half the distance the other
/// end lies along that normal, so the curve leaves one box and arrives at the
/// other head-on.
///
/// A target behind the side reaches by the root of that distance instead. A
/// reach that followed it signed would send the pen away from the target and
/// back, which is the loop a ring draws around its own label when the label
/// sits beside it rather than below.
fn s_curve_controls(
    start: [f64; 2],
    end: [f64; 2],
    from_side: SketchSide,
    to_side: SketchSide,
) -> [[f64; 2]; 2] {
    let control = |point: [f64; 2], side: SketchSide, other: [f64; 2]| {
        let normal = match side {
            SketchSide::Left => [-1.0, 0.0],
            SketchSide::Right => [1.0, 0.0],
            SketchSide::Top => [0.0, -1.0],
            SketchSide::Bottom => [0.0, 1.0],
            SketchSide::Auto => {
                let (dx, dy) = (other[0] - point[0], other[1] - point[1]);
                match dx.abs() >= dy.abs() {
                    true => [dx.signum(), 0.0],
                    false => [0.0, dy.signum()],
                }
            },
        };
        let along = (other[0] - point[0]) * normal[0] + (other[1] - point[1]) * normal[1];
        let reach = match along >= 0.0 {
            true => along * FORWARD_REACH,
            false => (-along).sqrt() * HOOK_REACH,
        };
        [point[0] + normal[0] * reach, point[1] + normal[1] * reach]
    };

    [control(start, from_side, end), control(end, to_side, start)]
}

/// Where one end sits before its side is chosen: a fixed point as itself, a
/// component as the center of its box.
fn end_center<Resolve>(end: SketchEnd, cw: f64, ch: f64, resolve: &Resolve) -> Option<[f64; 2]>
where
    Resolve: Fn(u32) -> Option<ComponentBox>,
{
    match end {
        SketchEnd::Point { x, y } => Some(point_px(x, y, cw, ch)),
        SketchEnd::Component { id, .. } => {
            let [min_x, min_y, max_x, max_y] = resolve(id)?.bounds.map(f64::from);
            Some([(min_x + max_x) / 2.0, (min_y + max_y) / 2.0])
        },
    }
}

/// Where one end of a connector actually meets, and which side it met on.
///
/// `toward` is the other end's center, which is what an `Auto` side is chosen
/// against. A fixed point reports `Auto`, since it has no box and so no side.
fn attach<Resolve>(
    end: SketchEnd,
    cw: f64,
    ch: f64,
    toward: [f64; 2],
    resolve: &Resolve,
) -> Option<([f64; 2], SketchSide)>
where
    Resolve: Fn(u32) -> Option<ComponentBox>,
{
    match end {
        SketchEnd::Point { x, y } => Some((point_px(x, y, cw, ch), SketchSide::Auto)),
        SketchEnd::Component { id, side } => {
            let component = resolve(id)?;
            let side = match side {
                SketchSide::Auto => facing_side(component.bounds.map(f64::from), toward),
                named => named,
            };
            Some((side_point(&component, side, toward), side))
        },
    }
}

/// The side of `bounds` that faces `toward`.
///
/// Compared as a fraction of each half-extent rather than in raw pixels, so a
/// wide box does not claim its long sides for directions that clearly point at
/// a short one.
fn facing_side(bounds: [f64; 4], toward: [f64; 2]) -> SketchSide {
    let [min_x, min_y, max_x, max_y] = bounds;
    let dx = toward[0] - (min_x + max_x) / 2.0;
    let dy = toward[1] - (min_y + max_y) / 2.0;
    let half_w = ((max_x - min_x) / 2.0).max(f64::EPSILON);
    let half_h = ((max_y - min_y) / 2.0).max(f64::EPSILON);

    match (dx / half_w).abs() >= (dy / half_h).abs() {
        true if dx >= 0.0 => SketchSide::Right,
        true => SketchSide::Left,
        false if dy >= 0.0 => SketchSide::Bottom,
        false => SketchSide::Top,
    }
}

/// Where a connector meets `component` on `side`, nearest `toward`.
///
/// The along-side coordinate follows the other end, so a link to a card eight
/// rows tall meets it level with the mark it comes from rather than at the
/// card's middle, and a label is met beside the word its mark circles.
///
/// It is held inset from the side's ends by the larger of [`SIDE_INSET`] of the
/// side and the corner's own radius, and never crosses the middle. A point in a
/// corner reads as missing the box rather than arriving at it.
///
/// A ring's outline lies inside its bounds everywhere but the four midpoints,
/// so the across coordinate follows the ellipse at that position. The point
/// then pushes out by [`COMPONENT_GAP`] along the side's axis, which is what
/// keeps a connector clear of the stroke it points at.
fn side_point(component: &ComponentBox, side: SketchSide, toward: [f64; 2]) -> [f64; 2] {
    let [min_x, min_y, max_x, max_y] = component.bounds.map(f64::from);
    let center = [(min_x + max_x) / 2.0, (min_y + max_y) / 2.0];
    let (half_w, half_h) = ((max_x - min_x) / 2.0, (max_y - min_y) / 2.0);
    let corner = match component.rounding {
        Rounding::Ellipse => 0.0,
        Rounding::Rect { radius_px } => f64::from(radius_px),
    };

    // Half the side is the floor, which is the middle: a side shorter than two
    // insets has no straight run to meet on.
    let along = |half: f64, at: f64, want: f64| {
        let inset = (half * 2.0 * SIDE_INSET).max(corner).min(half);
        want.clamp(at - half + inset, at + half - inset)
    };
    // The ellipse's own half-extent across the side at `t`, which is the
    // position along it as a fraction of the half-extent there.
    let curve = |half: f64, t: f64| half * (1.0 - t * t).max(0.0).sqrt();

    match side {
        SketchSide::Left | SketchSide::Right => {
            let y = along(half_h, center[1], toward[1]);
            let out = match component.rounding {
                Rounding::Ellipse => curve(half_w, (y - center[1]) / half_h.max(f64::EPSILON)),
                Rounding::Rect { .. } => half_w,
            } + COMPONENT_GAP;
            match side {
                SketchSide::Left => [center[0] - out, y],
                _ => [center[0] + out, y],
            }
        },
        SketchSide::Top | SketchSide::Bottom | SketchSide::Auto => {
            let x = along(half_w, center[0], toward[0]);
            let out = match component.rounding {
                Rounding::Ellipse => curve(half_h, (x - center[0]) / half_w.max(f64::EPSILON)),
                Rounding::Rect { .. } => half_h,
            } + COMPONENT_GAP;
            match side {
                SketchSide::Top => [x, center[1] - out],
                _ => [x, center[1] + out],
            }
        },
    }
}

fn point_px(x: i16, y: i16, cw: f64, ch: f64) -> [f64; 2] {
    [
        f64::from(x) / CELL_FRACTION * cw,
        f64::from(y) / CELL_FRACTION * ch,
    ]
}

/// Wobble one segment, as the base pass or as the overlay that doubles it.
///
/// The two passes differ only in how far they jitter, and every draw here comes
/// off the shared stream in the reference's order.
fn line(
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    options: &mut Options,
    overlay: bool,
    random: &mut Random,
) -> Vec<Op> {
    let len_sq = (x1 - x2).powi(2) + (y1 - y2).powi(2);
    let len = len_sq.sqrt();

    options.gain = if len < 200.0 {
        1.0
    } else if len > 500.0 {
        0.4
    } else {
        -0.001_666_8 * len + 1.233_334
    };

    let mut offset = options.max_randomness_offset;
    if offset * offset * 100.0 > len_sq {
        offset = len / 10.0;
    }
    let half = offset / 2.0;
    let jitter = if overlay { half } else { offset };

    let diverge = 0.2 + 0.2 * random.next();
    let mut mid_x = options.bowing * options.max_randomness_offset * (y2 - y1) / 200.0;
    let mut mid_y = options.bowing * options.max_randomness_offset * (x1 - x2) / 200.0;
    mid_x = options.offset_opt(mid_x, random);
    mid_y = options.offset_opt(mid_y, random);

    let mut ops = Vec::with_capacity(2);
    if options.preserve_vertices {
        ops.push(Op::Move([x1, y1]));
    } else {
        let dx = options.offset_opt(jitter, random);
        let dy = options.offset_opt(jitter, random);
        ops.push(Op::Move([x1 + dx, y1 + dy]));
    }

    // Each coordinate draws its own jitter, so the six are six draws.
    let c1x = mid_x + x1 + (x2 - x1) * diverge + options.offset_opt(jitter, random);
    let c1y = mid_y + y1 + (y2 - y1) * diverge + options.offset_opt(jitter, random);
    let c2x = mid_x + x1 + 2.0 * (x2 - x1) * diverge + options.offset_opt(jitter, random);
    let c2y = mid_y + y1 + 2.0 * (y2 - y1) * diverge + options.offset_opt(jitter, random);
    let (ex, ey) = match options.preserve_vertices {
        true => {
            // The draws still happen, so the stream stays in step with a run
            // that does move the endpoint.
            let _ = options.offset_opt(jitter, random);
            let _ = options.offset_opt(jitter, random);
            (x2, y2)
        },
        false => (
            x2 + options.offset_opt(jitter, random),
            y2 + options.offset_opt(jitter, random),
        ),
    };
    ops.push(Op::Curve([c1x, c1y, c2x, c2y, ex, ey]));
    ops
}

/// A segment stroked twice, which is what reads as pen-over-pen.
fn double_line(
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    options: &mut Options,
    random: &mut Random,
) -> Vec<Op> {
    let mut ops = line(x1, y1, x2, y2, options, false, random);
    ops.extend(line(x1, y1, x2, y2, options, true, random));
    ops
}

/// Catmull-Rom through `points`, as cubic beziers.
///
/// Starts at `points[1]` and stops two from the end, so the first and last
/// points are phantoms that only shape the tangents. A caller wanting the curve
/// to reach its true ends duplicates them.
fn curve_ops(points: &[[f64; 2]], options: &Options) -> Vec<Op> {
    if points.len() < 3 {
        return Vec::new();
    }
    if points.len() == 3 {
        return vec![
            Op::Move(points[1]),
            Op::Curve([
                points[1][0],
                points[1][1],
                points[2][0],
                points[2][1],
                points[2][0],
                points[2][1],
            ]),
        ];
    }

    let s = 1.0 - options.curve_tightness;
    let mut ops = vec![Op::Move(points[1])];
    for i in 1..points.len() - 2 {
        let c1 = [
            points[i][0] + (s * points[i + 1][0] - s * points[i - 1][0]) / 6.0,
            points[i][1] + (s * points[i + 1][1] - s * points[i - 1][1]) / 6.0,
        ];
        let c2 = [
            points[i + 1][0] + (s * points[i][0] - s * points[i + 2][0]) / 6.0,
            points[i + 1][1] + (s * points[i][1] - s * points[i + 2][1]) / 6.0,
        ];
        ops.push(Op::Curve([
            c1[0],
            c1[1],
            c2[0],
            c2[1],
            points[i + 1][0],
            points[i + 1][1],
        ]));
    }
    ops
}

/// The same points, each nudged, then curved.
fn curve_with_offset(
    points: &[[f64; 2]],
    amount: f64,
    options: &Options,
    random: &mut Random,
) -> Vec<Op> {
    let mut nudged = Vec::with_capacity(points.len() + 2);
    // The first point twice and the last twice, so the phantoms curve_ops drops
    // are copies rather than real vertices.
    nudged.push([
        points[0][0] + options.offset_opt(amount, random),
        points[0][1] + options.offset_opt(amount, random),
    ]);
    nudged.push([
        points[0][0] + options.offset_opt(amount, random),
        points[0][1] + options.offset_opt(amount, random),
    ]);
    for (index, point) in points.iter().enumerate().skip(1) {
        nudged.push([
            point[0] + options.offset_opt(amount, random),
            point[1] + options.offset_opt(amount, random),
        ]);
        if index == points.len() - 1 {
            nudged.push([
                point[0] + options.offset_opt(amount, random),
                point[1] + options.offset_opt(amount, random),
            ]);
        }
    }
    curve_ops(&nudged, options)
}

/// The ring of points one ellipse pass rides, and how far it overshoots.
fn compute_ellipse_points(
    ring: &Ring,
    offset: f64,
    overlap: f64,
    options: &Options,
    random: &mut Random,
) -> Vec<[f64; 2]> {
    let &Ring {
        increment,
        cx,
        cy,
        rx,
        ry,
    } = ring;
    let rad_offset = options.offset_opt(0.5, random) - PI / 2.0;
    let mut points = Vec::new();

    // A lead-in one increment before the start, pulled inward, so the pen is
    // already moving when it reaches the ring.
    points.push([
        options.offset_opt(offset, random) + cx + 0.9 * rx * (rad_offset - increment).cos(),
        options.offset_opt(offset, random) + cy + 0.9 * ry * (rad_offset - increment).sin(),
    ]);

    let mut angle = rad_offset;
    while angle < 2.0 * PI + rad_offset - 0.01 {
        points.push([
            options.offset_opt(offset, random) + cx + rx * angle.cos(),
            options.offset_opt(offset, random) + cy + ry * angle.sin(),
        ]);
        angle += increment;
    }

    // Three tail points carry the pen past the start, so the ring closes
    // over itself instead of stopping short of where it began.
    points.push([
        options.offset_opt(offset, random)
            + cx
            + rx * (rad_offset + 2.0 * PI + 0.5 * overlap).cos(),
        options.offset_opt(offset, random)
            + cy
            + ry * (rad_offset + 2.0 * PI + 0.5 * overlap).sin(),
    ]);
    points.push([
        options.offset_opt(offset, random) + cx + 0.98 * rx * (rad_offset + overlap).cos(),
        options.offset_opt(offset, random) + cy + 0.98 * ry * (rad_offset + overlap).sin(),
    ]);
    points.push([
        options.offset_opt(offset, random) + cx + 0.9 * rx * (rad_offset + 0.5 * overlap).cos(),
        options.offset_opt(offset, random) + cy + 0.9 * ry * (rad_offset + 0.5 * overlap).sin(),
    ]);

    points
}

/// A hand-drawn ellipse inscribed in the box at `(cx, cy)` of size `w` by `h`.
fn ellipse(
    cx: f64,
    cy: f64,
    w: f64,
    h: f64,
    options: &mut Options,
    random: &mut Random,
) -> Vec<Op> {
    let psq = (2.0 * PI * (((w / 2.0).powi(2) + (h / 2.0).powi(2)) / 2.0).sqrt()).sqrt();
    let step_count = options
        .curve_step_count
        .max(options.curve_step_count / 200.0_f64.sqrt() * psq);
    let increment = 2.0 * PI / step_count;

    let mut rx = (w / 2.0).abs();
    let mut ry = (h / 2.0).abs();
    let fitting = 1.0 - options.curve_fitting;
    rx += options.offset_opt(rx * fitting, random);
    ry += options.offset_opt(ry * fitting, random);

    let overlap = increment * options.offset(0.1, options.offset(0.4, 1.0, random), random);
    let ring = Ring {
        increment,
        cx,
        cy,
        rx,
        ry,
    };
    let first = compute_ellipse_points(&ring, 1.0, overlap, options, random);
    let second = compute_ellipse_points(&ring, 1.5, 0.0, options, random);

    let mut ops = curve_ops(&first, options);
    ops.extend(curve_ops(&second, options));
    ops
}

/// A hand-drawn box, square-cornered or rounded.
fn rect(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    radius: f64,
    options: &mut Options,
    random: &mut Random,
) -> Vec<Op> {
    let radius = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    if radius <= 0.0 {
        let corners = [[x, y], [x + w, y], [x + w, y + h], [x, y + h]];
        let mut ops = Vec::new();
        for pair in corners.windows(2) {
            ops.extend(double_line(
                pair[0][0], pair[0][1], pair[1][0], pair[1][1], options, random,
            ));
        }
        ops.extend(double_line(
            corners[3][0],
            corners[3][1],
            corners[0][0],
            corners[0][1],
            options,
            random,
        ));
        return ops;
    }

    // Straight sides between the tangent points, with a quadratic through each
    // corner, matching the rounded-rectangle path excalidraw hands rough.js.
    let (r, x2, y2) = (radius, x + w, y + h);
    let mut ops = Vec::new();
    let sides = [
        ([x + r, y], [x2 - r, y]),
        ([x2, y + r], [x2, y2 - r]),
        ([x2 - r, y2], [x + r, y2]),
        ([x, y2 - r], [x, y + r]),
    ];
    let corners = [
        ([x2 - r, y], [x2, y], [x2, y + r]),
        ([x2, y2 - r], [x2, y2], [x2 - r, y2]),
        ([x + r, y2], [x, y2], [x, y2 - r]),
        ([x, y + r], [x, y], [x + r, y]),
    ];

    for (index, (from, to)) in sides.iter().enumerate() {
        ops.extend(double_line(from[0], from[1], to[0], to[1], options, random));

        // `line` leaves the gain it computed for the side in place, and
        // `offset_opt` multiplies by it, so a corner would otherwise wobble by
        // the length of the side before it.
        options.gain = 1.0;
        let (start, control, end) = corners[index];
        ops.extend(quadratic(start, control, end, options, random));
    }
    ops
}

/// One rounded corner, stroked twice like a side.
///
/// The quadratic is raised to a cubic so it flattens through the same path as
/// every other curve, and so one transcription serves both a corner and a
/// connector.
fn quadratic(
    start: [f64; 2],
    control: [f64; 2],
    end: [f64; 2],
    options: &Options,
    random: &mut Random,
) -> Vec<Op> {
    let raise = |from: [f64; 2]| {
        [
            from[0] + 2.0 / 3.0 * (control[0] - from[0]),
            from[1] + 2.0 / 3.0 * (control[1] - from[1]),
        ]
    };
    cubic(start, raise(start), raise(end), end, options, random)
}

/// One cubic segment, stroked twice.
///
/// The two passes take the offsets the reference uses for a path's `C`
/// segments. Those offsets rise with the roughness and not with the segment's
/// size, so a generously rounded corner stays as clean as the sides it joins
/// and a long connector as clean as a short one.
///
/// With vertices preserved the ends are pinned, as a side's are, so a corner
/// meets the straights it joins instead of starting beside them. Only the
/// control points move, and each moves on its own, which is what gives the
/// curve its slack rather than sliding it whole.
fn cubic(
    start: [f64; 2],
    first: [f64; 2],
    second: [f64; 2],
    end: [f64; 2],
    options: &Options,
    random: &mut Random,
) -> Vec<Op> {
    let mut ops = Vec::new();
    let rough = options.roughness;
    for amount in [1.0 * (1.0 + 0.2 * rough), 1.5 * (1.0 + 0.22 * rough)] {
        let jitter = |random: &mut Random| options.offset_opt(amount, random);

        // The draws still happen when an end is pinned, so the stream stays in
        // step with a run that does move it.
        let (sx, sy) = match options.preserve_vertices {
            true => {
                let _ = jitter(random);
                let _ = jitter(random);
                (start[0], start[1])
            },
            false => (start[0] + jitter(random), start[1] + jitter(random)),
        };
        let c1x = first[0] + jitter(random);
        let c1y = first[1] + jitter(random);
        let c2x = second[0] + jitter(random);
        let c2y = second[1] + jitter(random);
        let (ex, ey) = match options.preserve_vertices {
            true => {
                let _ = jitter(random);
                let _ = jitter(random);
                (end[0], end[1])
            },
            false => (end[0] + jitter(random), end[1] + jitter(random)),
        };

        ops.push(Op::Move([sx, sy]));
        ops.push(Op::Curve([c1x, c1y, c2x, c2y, ex, ey]));
    }
    ops
}

/// A hand-drawn curve through `points`, stroked twice.
fn curve(points: &[[f64; 2]], options: &Options, random: &mut Random) -> Vec<Op> {
    if points.len() < 2 {
        return Vec::new();
    }
    let mut ops = curve_with_offset(
        points,
        1.0 * (1.0 + 0.2 * options.roughness),
        options,
        random,
    );
    ops.extend(curve_with_offset(
        points,
        1.5 * (1.0 + 0.22 * options.roughness),
        options,
        random,
    ));
    ops
}

/// Two strokes meeting at `tip`, angled back along `direction`.
///
/// Sized against the segment it caps, so a short connector does not sprout a
/// head longer than itself.
fn arrow_head(
    tip: [f64; 2],
    direction: f64,
    segment_len: f64,
    options: &mut Options,
    random: &mut Random,
) -> Vec<Op> {
    const SIZE: f64 = 25.0;
    const SPREAD: f64 = 20.0 * PI / 180.0;

    let size = SIZE.min(segment_len / 2.0);
    let mut ops = Vec::new();
    for side in [-1.0, 1.0] {
        let angle = direction + side * SPREAD;
        ops.extend(double_line(
            tip[0] - size * angle.cos(),
            tip[1] - size * angle.sin(),
            tip[0],
            tip[1],
            options,
            random,
        ));
    }
    ops
}

/// Walk the ops into flattened strokes, one per pen-down run.
///
/// The ops are in logical pixels and `scale` carries them to physical, so the
/// arc lengths the reveal walks are measured in the pixels it draws.
///
/// Each bezier takes the chords its own curvature needs. The count is free to
/// vary because the reveal walks arc length through the prefix sums rather than
/// counting points, so a mark regenerated at another size reveals at the same
/// rate through a different number of chords.
fn flatten(ops: &[Op], scale: f64) -> Vec<Stroke> {
    let mut strokes = Vec::new();
    let mut current: Vec<[f32; 2]> = Vec::new();
    let mut pen = [0.0_f64; 2];

    for op in ops {
        match op {
            Op::Move(to) => {
                if current.len() > 1 {
                    strokes.push(Stroke::new(std::mem::take(&mut current)));
                } else {
                    current.clear();
                }
                pen = *to;
                current.push([(to[0] * scale) as f32, (to[1] * scale) as f32]);
            },
            Op::Curve(c) => {
                let (p0, p1, p2, p3) = (pen, [c[0], c[1]], [c[2], c[3]], [c[4], c[5]]);
                let count = segment_count(p0, p1, p2, p3);
                for step in 1..=count {
                    let t = step as f64 / count as f64;
                    let point = cubic_at(p0, p1, p2, p3, t);
                    current.push([(point[0] * scale) as f32, (point[1] * scale) as f32]);
                }
                pen = p3;
            },
        }
    }
    if current.len() > 1 {
        strokes.push(Stroke::new(current));
    }
    strokes
}

/// Chords a cubic needs to stay within [`BEZIER_TOLERANCE`] of its curve.
///
/// Wang's bound. A cubic's deviation from its chords is governed by its second
/// differences, so the larger of the two bounds the whole curve, and the count
/// falls out of the square root. A straight cubic asks for the floor of two,
/// which is the one chord its endpoints need.
///
/// The ceiling of 64 bounds what a degenerate control net can ask for. At the
/// tolerance here only a curve hundreds of pixels across comes near it.
fn segment_count(p0: [f64; 2], p1: [f64; 2], p2: [f64; 2], p3: [f64; 2]) -> usize {
    let second = |a: [f64; 2], b: [f64; 2], c: [f64; 2]| {
        let (x, y) = (a[0] - 2.0 * b[0] + c[0], a[1] - 2.0 * b[1] + c[1]);
        (x * x + y * y).sqrt()
    };
    let deviation = second(p0, p1, p2).max(second(p1, p2, p3));

    ((0.75 * deviation / BEZIER_TOLERANCE).sqrt().ceil() as usize).clamp(2, 64)
}

/// The point at `t` along the cubic through the four control points.
fn cubic_at(p0: [f64; 2], p1: [f64; 2], p2: [f64; 2], p3: [f64; 2], t: f64) -> [f64; 2] {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    [
        a * p0[0] + b * p1[0] + c * p2[0] + d * p3[0],
        a * p0[1] + b * p1[1] + c * p2[1] + d * p3[1],
    ]
}

/// The gap between hatch lines, in the logical pixels generation runs in.
///
/// Taken from the declared weight rather than the eased one, so a restyle
/// changes how heavy the fill draws without moving where its lines sit.
fn hatch_gap(command: &SketchCommand, cw: f64) -> f64 {
    HACHURE_GAP * f64::from(command.style.width) / f64::from(WIDTH_FRACTION) * cw
}

/// The ellipse inscribed in the box, as a polygon the hatch fills.
///
/// Sampled rather than taken from the stroked ring, so the fill consumes no
/// draws and the outline's wobble stays where it was.
fn ellipse_polygon(x: f64, y: f64, w: f64, h: f64) -> Vec<[f64; 2]> {
    const STEPS: usize = 32;
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (rx, ry) = ((w / 2.0).abs(), (h / 2.0).abs());

    (0..STEPS)
        .map(|step| {
            let angle = 2.0 * PI * step as f64 / STEPS as f64;
            [cx + rx * angle.cos(), cy + ry * angle.sin()]
        })
        .collect()
}

/// The hatch strokes filling `polygon`, or none for a solid or absent fill.
///
/// Each chord runs through the same double-line wobble an outline takes, so a
/// hatch line reads as a pen stroke rather than as a ruled one.
fn hatch_strokes(
    polygon: &[[f64; 2]],
    fill: Option<SketchFill>,
    gap: f64,
    scale: f64,
    options: &mut Options,
    random: &mut Random,
) -> Vec<Stroke> {
    let Some(fill) = fill else {
        return Vec::new();
    };
    let angles: &[f64] = match fill.style {
        SketchFillStyle::Solid => return Vec::new(),
        SketchFillStyle::Hachure => &[HACHURE_ANGLE],
        SketchFillStyle::CrossHatch => &[HACHURE_ANGLE, HACHURE_ANGLE + PI / 2.0],
    };

    let mut ops = Vec::new();
    for &angle in angles {
        for [from, to] in hachure_lines(polygon, gap, angle) {
            ops.extend(double_line(from[0], from[1], to[0], to[1], options, random));
        }
    }

    let color = [fill.color[0], fill.color[1], fill.color[2], fill.alpha];
    flatten(&ops, scale)
        .into_iter()
        .map(|stroke| Stroke {
            weight: HACHURE_WEIGHT,
            color: Some(color),
            ..stroke
        })
        .collect()
}
/// The angle hachure runs at, in radians.
///
/// The reference's default. Off the horizontal and off the vertical, so a hatch
/// line crosses the box's own sides rather than running along one.
const HACHURE_ANGLE: f64 = -41.0 * PI / 180.0;

/// Gap between hatch lines, as a multiple of the mark's stroke weight.
const HACHURE_GAP: f64 = 4.0;

/// Most scanlines one hatch angle lays over a shape.
///
/// The gap comes from the declared stroke weight, and a weight arrives as a
/// `u16` of cell-width 256ths, so a light one puts the lines a fraction of a
/// pixel apart. A declared box reaches 4,095 cells, so the two together ask for
/// half a million chords over a shape the screen cannot show, each of which the
/// pass rebuilds every frame.
///
/// The ceiling sits above what any on-screen shape asks for. A 40-cell box at a
/// quarter-cell weight asks 320. Past the ceiling the gap widens to fit, which
/// costs no detail. The lines there already sit closer together than the stroke
/// drawn along them.
const MAX_HACHURE_LINES: usize = 512;

/// A hatch line's weight, as a fraction of the mark's stroke weight.
pub(crate) const HACHURE_WEIGHT: f32 = 0.5;

/// The chords that hatch `polygon`, each `gap` apart at `angle`.
///
/// Scanline fill runs in the rotated frame. The polygon turns so the hatch runs
/// horizontal, y walks in `gap` steps, each scanline's crossings with the edges
/// are paired, and each pair turns back. Pairing sorted crossings is what keeps
/// a concave shape hatched only where it is solid.
///
/// The first scanline sits half a gap in, so a hatch never lands exactly on an
/// edge, where a crossing count is ambiguous.
///
/// `gap` widens if it would ask for more than [`MAX_HACHURE_LINES`] scanlines.
/// A convex shape lays one chord per scanline, so the ceiling bounds the result.
/// A concave one lays one chord per pair of crossings, which its edge count
/// bounds.
fn hachure_lines(polygon: &[[f64; 2]], gap: f64, angle: f64) -> Vec<[[f64; 2]; 2]> {
    if polygon.len() < 3 || gap <= 0.0 {
        return Vec::new();
    }

    let (sin, cos) = (-angle).sin_cos();
    let rotate =
        |p: [f64; 2], sin: f64, cos: f64| [p[0] * cos - p[1] * sin, p[0] * sin + p[1] * cos];
    let turned: Vec<[f64; 2]> = polygon.iter().map(|&p| rotate(p, sin, cos)).collect();

    let (mut top, mut bottom) = (f64::MAX, f64::MIN);
    for point in &turned {
        top = top.min(point[1]);
        bottom = bottom.max(point[1]);
    }

    // The declared weight sets the gap, and nothing about the weight bounds it
    // against the shape it fills, so the sweep gets the ceiling instead.
    let gap = gap.max((bottom - top) / MAX_HACHURE_LINES as f64);

    let (back_sin, back_cos) = angle.sin_cos();
    let mut lines = Vec::new();
    let mut y = top + gap / 2.0;
    let mut crossings = Vec::new();

    while y < bottom {
        crossings.clear();
        for (index, from) in turned.iter().enumerate() {
            let to = turned[(index + 1) % turned.len()];
            let (low, high) = (from[1].min(to[1]), from[1].max(to[1]));
            if y < low || y >= high {
                continue;
            }
            let t = (y - from[1]) / (to[1] - from[1]);
            crossings.push(from[0] + (to[0] - from[0]) * t);
        }

        crossings.sort_by(|a, b| a.total_cmp(b));
        for &[left, right] in crossings.as_chunks::<2>().0 {
            lines.push([
                rotate([left, y], back_sin, back_cos),
                rotate([right, y], back_sin, back_cos),
            ]);
        }
        y += gap;
    }

    lines
}
/// Roughness damped for a shape too small to carry it.
///
/// A wobble sized for a large box swamps a small one, so the same roughness
/// reads as noise below about twenty pixels. The thresholds and the exemptions
/// are excalidraw's.
///
/// A connector and a rounded box each escape the both-sides rule on their own
/// terms. A connector passes its two deltas as the sides, so a near-axis one is
/// long on one axis and a pixel or two on the other, which the both-sides rule
/// reads as a small shape. A rounded box carries a wobble at a smaller size
/// than a square-cornered one does, because its corners are already curved.
fn damped_roughness(roughness: f64, w: f64, h: f64, kind: RoughKind) -> f64 {
    let (min_side, max_side) = (w.abs().min(h.abs()), w.abs().max(h.abs()));

    let exempt = match kind {
        RoughKind::Linear => max_side >= 50.0,
        RoughKind::Round => min_side >= 15.0,
        RoughKind::Other => min_side >= 20.0 && max_side >= 50.0,
    };
    if exempt {
        return roughness;
    }

    let divisor = if max_side < 10.0 { 3.0 } else { 2.0 };
    (roughness / divisor).min(2.5)
}

#[cfg(test)]
mod tests;
