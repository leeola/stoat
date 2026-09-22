//! The sketch commands stroke hand-drawn marks over the cell grid.
//!
//! A sketch is declared, not drawn: the emitter says what shape it wants and
//! how rough and how fast, and the terminal generates the wobbling geometry
//! itself. That split is what lets a mark stay hand-drawn at every font size,
//! since the roughness is applied in pixels against the live cell metrics, and
//! what lets the stroke animate at the display refresh rate without the emitter
//! sending a frame per step.
//!
//! One head serves four shapes. An ellipse circles a subject, a rectangle
//! boxes a block, and a line connects one to a label. A path is a connector
//! that turns more than once. They exist for the walkthrough player, but
//! nothing here knows about walkthroughs.

use crate::frame;

/// A hand-drawn mark the terminal generates and animates.
///
/// [`Self::id`] is the mark's identity across frames, chosen by the emitter the
/// way a minimap strip id is. The terminal latches [`Self::timing`] when an id
/// first appears, so an emitter that re-declares its whole decoration set every
/// frame does not restart the animation. A re-declaration changes geometry and
/// style. Only a new id starts a new draw.
#[derive(Clone, PartialEq, Debug)]
pub struct SketchCommand {
    pub id: u32,
    pub style: SketchStyle,
    pub timing: SketchTiming,
    pub shape: SketchShape,
    /// Pool this mark rides, and the pool's top row, so the mark glides with a
    /// scrolling pane instead of staying pinned to the screen. `None` leaves it
    /// screen-fixed.
    pub anchor: Option<(u32, f32)>,
}

/// How a mark is stroked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SketchStyle {
    pub color: [u8; 3],
    /// Stroke opacity, 255 being opaque.
    pub alpha: u8,
    /// Stroke thickness in **256ths of the cell width**, so 64 is a quarter of
    /// a cell and a mark tracks live font zoom.
    pub width: u16,
    /// How far the stroke wanders, where 64 is rough.js roughness 1.0.
    pub roughness: u8,
    /// Seed for the wander, so a mark redrawn at another size wobbles the same
    /// way. Zero asks the terminal to derive one from [`SketchCommand::id`],
    /// which is what an emitter with no opinion sends.
    pub seed: u32,
}

impl SketchStyle {
    /// A marker's stroke: one cell-quarter wide, rough.js roughness 1.0, opaque,
    /// and seeded from the mark's own id.
    ///
    /// The five fields are tuned together, and a mark that spells them out
    /// per call drifts from the ones beside it. This names the one combination
    /// an annotation wants.
    pub fn marker(color: [u8; 3]) -> SketchStyle {
        SketchStyle {
            color,
            alpha: 255,
            width: 64,
            roughness: 64,
            seed: 0,
        }
    }
}

/// When a mark draws itself, and in which direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SketchTiming {
    /// Wait this long after the mark first appears before drawing starts.
    pub delay_ms: u16,
    /// How long the stroke takes to draw itself end to end.
    pub duration_ms: u16,
    pub easing: SketchEasing,
    pub phase: SketchPhase,
}

impl SketchTiming {
    /// A stroke that waits `delay_ms`, then draws itself over `duration_ms`.
    ///
    /// Smoothstep and the enter phase, which is what a mark being drawn on
    /// wants. Staggering several marks is a matter of their delays alone.
    pub fn after(delay_ms: u16, duration_ms: u16) -> SketchTiming {
        SketchTiming {
            delay_ms,
            duration_ms,
            easing: SketchEasing::Smoothstep,
            phase: SketchPhase::Enter,
        }
    }
}

/// The curve a mark's reveal follows over its duration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SketchEasing {
    Linear,
    Smoothstep,
    EaseOutCubic,
}

/// Whether a mark draws itself on or wipes itself off.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SketchPhase {
    Enter,
    Exit,
}

/// Which mark to draw, and the geometry it needs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SketchShape {
    Ellipse {
        bounds: SketchBounds,
        /// Painted inside the stroke. `None` leaves the ring open.
        ///
        /// Only a hatched style draws. A solid ellipse stays open, because the
        /// renderer resolves a solid fill as a convex quad and an ellipse has
        /// no quad to give it.
        fill: Option<SketchFill>,
    },
    Rect {
        bounds: SketchBounds,
        /// Corner rounding in sixteenths of a cell.
        radius: u8,
        /// Painted inside the stroke. `None` leaves the box open.
        fill: Option<SketchFill>,
    },
    Line {
        from: SketchEnd,
        to: SketchEnd,
        /// How far the curve bows off the straight chord, in 64ths of the
        /// chord's length, the sign picking the side. Zero asks the terminal
        /// for an S-curve when both ends name components, and draws straight
        /// otherwise.
        bend: i8,
        /// Bit 0 puts an arrowhead at [`Self::Line::from`], bit 1 at
        /// [`Self::Line::to`].
        heads: u8,
    },
    /// A curve through every one of [`Self::Path::points`], for a connector
    /// that turns more than once.
    ///
    /// The terminal draws the Catmull-Rom spline through the points, with the
    /// first and last doubled so the curve reaches them. That is the curve a
    /// bent line draws through its bowed midpoint. Fewer than two points draw
    /// nothing, and one frame carries at most 255 points.
    Path { points: Vec<SketchPoint> },
}

/// A mark's box, in **sixteenths of a cell** so it tracks live font zoom.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SketchBounds {
    pub x: i16,
    pub y: i16,
    pub w: u16,
    pub h: u16,
}

/// A spot a path passes through, in **sixteenths of a cell** so it tracks live
/// font zoom.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SketchPoint {
    pub x: i16,
    pub y: i16,
}

/// What fills a rectangle behind its stroke.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SketchFill {
    pub color: [u8; 3],
    pub alpha: u8,
    pub style: SketchFillStyle,
}

/// How a fill is laid down.
///
/// [`Self::Solid`] paints one flat translucent body. The hatched styles lay
/// pen strokes across it instead, which is the reference's default look and
/// leaves the cells under a mark legible in a way a solid body does not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SketchFillStyle {
    Solid,
    /// Parallel pen strokes at a fixed angle.
    Hachure,
    /// [`Self::Hachure`] crossed with a second pass at a right angle to it.
    CrossHatch,
}

/// Where one end of a line sits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SketchEnd {
    /// A fixed spot, in sixteenths of a cell.
    Point { x: i16, y: i16 },
    /// Another decoration's edge, so a connector tracks whatever it points at
    /// as that thing moves.
    Component { id: u32, side: SketchSide },
}

/// Which edge of a component a line meets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SketchSide {
    /// Let the terminal pick the edge facing the line's other end.
    Auto,
    Left,
    Right,
    Top,
    Bottom,
}

/// Bytes every sketch payload spends before its shape body.
const SKETCH_HEAD: usize = 21;

/// Bytes a line's end spends, whichever kind it is.
const END_LEN: usize = 7;

/// The most points one `sketch_path` frame carries, since one byte counts them.
const MAX_PATH_POINTS: usize = 255;

/// Encode a [`SketchCommand`] as a full `Gstoatty;sketch_*` frame for an
/// emitter.
pub fn encode_sketch(command: &SketchCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_sketch_into(&mut out, command);
    out
}

/// Append a `Gstoatty;sketch_*` frame for `command` to `out` without
/// allocating.
///
/// The shape picks the sub-command name, so a terminal that predates one shape
/// ignores those frames whole and still draws the shapes it knows.
pub fn encode_sketch_into(out: &mut Vec<u8>, command: &SketchCommand) {
    frame::begin(out, sub_command(&command.shape));
    frame::push_arg(out, |w| {
        w.write_all(&command.id.to_be_bytes())?;
        w.write_all(&command.style.color)?;
        w.write_all(&[command.style.alpha])?;
        w.write_all(&command.style.width.to_be_bytes())?;
        w.write_all(&[command.style.roughness])?;
        w.write_all(&command.style.seed.to_be_bytes())?;
        w.write_all(&command.timing.delay_ms.to_be_bytes())?;
        w.write_all(&command.timing.duration_ms.to_be_bytes())?;
        w.write_all(&[easing_code(command.timing.easing)])?;
        w.write_all(&[phase_code(command.timing.phase)])?;

        match &command.shape {
            SketchShape::Ellipse { bounds, fill } => {
                write_bounds(w, bounds)?;
                write_fill(w, *fill)?;
            },
            SketchShape::Rect {
                bounds,
                radius,
                fill,
            } => {
                write_bounds(w, bounds)?;
                w.write_all(&[*radius])?;
                write_fill(w, *fill)?;
            },
            SketchShape::Line {
                from,
                to,
                bend,
                heads,
            } => {
                write_end(w, from)?;
                write_end(w, to)?;
                w.write_all(&bend.to_be_bytes())?;
                w.write_all(&[*heads])?;
            },
            SketchShape::Path { points } => {
                let points = &points[..points.len().min(MAX_PATH_POINTS)];
                w.write_all(&[points.len() as u8])?;
                for point in points {
                    w.write_all(&point.x.to_be_bytes())?;
                    w.write_all(&point.y.to_be_bytes())?;
                }
            },
        }

        // Trailing and optional, so a receiver built before the anchor existed
        // reads the frame whole and treats the mark as screen-fixed.
        if let Some((host, top_rows)) = command.anchor {
            w.write_all(&host.to_be_bytes())?;
            w.write_all(&top_rows.to_be_bytes())?;
        }
        Ok(())
    });
    frame::end(out);
}

pub(super) fn decode_sketch(sub: &str, args: &[Vec<u8>]) -> Option<SketchCommand> {
    let arg = args.first()?;
    let body = arg.get(SKETCH_HEAD..)?;

    let (shape, used) = match sub {
        "sketch_ellipse" => {
            let bounds = read_bounds(body)?;
            // A frame that stops after the bounds predates the fill, so it
            // reads as an open ring rather than being dropped.
            match read_fill(body, 8) {
                Some(fill) => (SketchShape::Ellipse { bounds, fill }, 14),
                None => (SketchShape::Ellipse { bounds, fill: None }, 8),
            }
        },
        "sketch_rect" => {
            let bounds = read_bounds(body)?;
            let radius = *body.get(8)?;
            let fill = read_fill(body, 9)?;
            (
                SketchShape::Rect {
                    bounds,
                    radius,
                    fill,
                },
                15,
            )
        },
        "sketch_line" => {
            let from = read_end(body.get(..END_LEN)?)?;
            let to = read_end(body.get(END_LEN..END_LEN * 2)?)?;
            let bend = *body.get(END_LEN * 2)? as i8;
            let heads = *body.get(END_LEN * 2 + 1)?;
            (
                SketchShape::Line {
                    from,
                    to,
                    bend,
                    heads,
                },
                END_LEN * 2 + 2,
            )
        },
        "sketch_path" => {
            let count = usize::from(*body.first()?);
            let points = body
                .get(1..1 + 4 * count)?
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&[x0, x1, y0, y1]| SketchPoint {
                    x: i16::from_be_bytes([x0, x1]),
                    y: i16::from_be_bytes([y0, y1]),
                })
                .collect();
            (SketchShape::Path { points }, 1 + 4 * count)
        },
        _ => return None,
    };

    Some(SketchCommand {
        id: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        style: SketchStyle {
            color: [arg[4], arg[5], arg[6]],
            alpha: arg[7],
            width: u16::from_be_bytes([arg[8], arg[9]]),
            roughness: arg[10],
            seed: u32::from_be_bytes([arg[11], arg[12], arg[13], arg[14]]),
        },
        timing: SketchTiming {
            delay_ms: u16::from_be_bytes([arg[15], arg[16]]),
            duration_ms: u16::from_be_bytes([arg[17], arg[18]]),
            easing: decode_easing(arg[19]),
            phase: decode_phase(arg[20]),
        },
        shape,
        anchor: body.get(used..used + 8).map(|tail| {
            (
                u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]),
                f32::from_be_bytes([tail[4], tail[5], tail[6], tail[7]]),
            )
        }),
    })
}

/// The frame name a shape rides, so a terminal that knows only some of them
/// ignores the rest whole.
fn sub_command(shape: &SketchShape) -> &'static str {
    match shape {
        SketchShape::Ellipse { .. } => "sketch_ellipse",
        SketchShape::Rect { .. } => "sketch_rect",
        SketchShape::Line { .. } => "sketch_line",
        SketchShape::Path { .. } => "sketch_path",
    }
}

fn write_bounds(
    w: &mut (impl std::io::Write + ?Sized),
    bounds: &SketchBounds,
) -> std::io::Result<()> {
    w.write_all(&bounds.x.to_be_bytes())?;
    w.write_all(&bounds.y.to_be_bytes())?;
    w.write_all(&bounds.w.to_be_bytes())?;
    w.write_all(&bounds.h.to_be_bytes())
}

fn read_bounds(body: &[u8]) -> Option<SketchBounds> {
    let b = body.get(..8)?;
    Some(SketchBounds {
        x: i16::from_be_bytes([b[0], b[1]]),
        y: i16::from_be_bytes([b[2], b[3]]),
        w: u16::from_be_bytes([b[4], b[5]]),
        h: u16::from_be_bytes([b[6], b[7]]),
    })
}

/// An end is a fixed-width 7 bytes whichever kind it is, so the second one
/// starts at a known offset without reading the first.
fn write_end(w: &mut (impl std::io::Write + ?Sized), end: &SketchEnd) -> std::io::Result<()> {
    match end {
        SketchEnd::Point { x, y } => {
            w.write_all(&[0])?;
            w.write_all(&x.to_be_bytes())?;
            w.write_all(&y.to_be_bytes())?;
            w.write_all(&0u16.to_be_bytes())
        },
        SketchEnd::Component { id, side } => {
            w.write_all(&[1])?;
            w.write_all(&id.to_be_bytes())?;
            w.write_all(&[side_code(*side)])?;
            w.write_all(&[0])
        },
    }
}

fn read_end(bytes: &[u8]) -> Option<SketchEnd> {
    let b = bytes.get(..END_LEN)?;
    Some(match b[0] {
        1 => SketchEnd::Component {
            id: u32::from_be_bytes([b[1], b[2], b[3], b[4]]),
            side: decode_side(b[5]),
        },
        // An unknown kind reads as a point, which draws somewhere rather than
        // dropping the whole line.
        _ => SketchEnd::Point {
            x: i16::from_be_bytes([b[1], b[2]]),
            y: i16::from_be_bytes([b[3], b[4]]),
        },
    })
}

fn easing_code(easing: SketchEasing) -> u8 {
    match easing {
        SketchEasing::Linear => 0,
        SketchEasing::Smoothstep => 1,
        SketchEasing::EaseOutCubic => 2,
    }
}

fn decode_easing(code: u8) -> SketchEasing {
    match code {
        0 => SketchEasing::Linear,
        2 => SketchEasing::EaseOutCubic,
        _ => SketchEasing::Smoothstep,
    }
}

fn phase_code(phase: SketchPhase) -> u8 {
    match phase {
        SketchPhase::Enter => 0,
        SketchPhase::Exit => 1,
    }
}

fn decode_phase(code: u8) -> SketchPhase {
    match code {
        1 => SketchPhase::Exit,
        _ => SketchPhase::Enter,
    }
}

fn fill_style_code(style: SketchFillStyle) -> u8 {
    match style {
        SketchFillStyle::Solid => 0,
        SketchFillStyle::Hachure => 1,
        SketchFillStyle::CrossHatch => 2,
    }
}

/// An unknown code reads as [`SketchFillStyle::Solid`], so a mark from a newer
/// emitter still fills rather than dropping the frame.
fn decode_fill_style(code: u8) -> SketchFillStyle {
    match code {
        1 => SketchFillStyle::Hachure,
        2 => SketchFillStyle::CrossHatch,
        _ => SketchFillStyle::Solid,
    }
}

/// Write a fill as the presence byte, the color, the alpha, and the style code.
fn write_fill(
    w: &mut (impl std::io::Write + ?Sized),
    fill: Option<SketchFill>,
) -> std::io::Result<()> {
    w.write_all(&[fill.is_some() as u8])?;
    let fill = fill.unwrap_or(SketchFill {
        color: [0, 0, 0],
        alpha: 0,
        style: SketchFillStyle::Solid,
    });
    w.write_all(&fill.color)?;
    w.write_all(&[fill.alpha])?;
    w.write_all(&[fill_style_code(fill.style)])
}

/// Read the six fill bytes at `at`, or `None` when the frame stops before them.
///
/// An ellipse gained its fill after the shape shipped, so a frame from an
/// emitter that predates it ends after the bounds. The outer `None` is what
/// lets that frame read as an unfilled ellipse rather than being dropped.
fn read_fill(body: &[u8], at: usize) -> Option<Option<SketchFill>> {
    let present = *body.get(at)?;
    let color = [*body.get(at + 1)?, *body.get(at + 2)?, *body.get(at + 3)?];
    let alpha = *body.get(at + 4)?;
    let style = decode_fill_style(*body.get(at + 5)?);
    Some((present != 0).then_some(SketchFill {
        color,
        alpha,
        style,
    }))
}

fn side_code(side: SketchSide) -> u8 {
    match side {
        SketchSide::Auto => 0,
        SketchSide::Left => 1,
        SketchSide::Right => 2,
        SketchSide::Top => 3,
        SketchSide::Bottom => 4,
    }
}

fn decode_side(code: u8) -> SketchSide {
    match code {
        1 => SketchSide::Left,
        2 => SketchSide::Right,
        3 => SketchSide::Top,
        4 => SketchSide::Bottom,
        _ => SketchSide::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, Command};

    fn head() -> (SketchStyle, SketchTiming) {
        (
            SketchStyle {
                color: [220, 50, 47],
                alpha: 200,
                width: 64,
                roughness: 64,
                seed: 0xABCD_1234,
            },
            SketchTiming {
                delay_ms: 120,
                duration_ms: 480,
                easing: SketchEasing::EaseOutCubic,
                phase: SketchPhase::Enter,
            },
        )
    }

    fn sketch(shape: SketchShape, anchor: Option<(u32, f32)>) -> SketchCommand {
        let (style, timing) = head();
        SketchCommand {
            id: 7,
            style,
            timing,
            shape,
            anchor,
        }
    }

    fn bounds() -> SketchBounds {
        SketchBounds {
            x: -16,
            y: 32,
            w: 240,
            h: 48,
        }
    }

    fn path(points: &[(i16, i16)]) -> SketchShape {
        SketchShape::Path {
            points: points.iter().map(|&(x, y)| SketchPoint { x, y }).collect(),
        }
    }

    /// The payload lengths the protocol doc publishes, checked against what the
    /// encoder writes. A doc that drifts from the wire is worse than none: the
    /// terminal reads by offset, so a wrong length there sends the next
    /// implementer to the wrong bytes.
    #[test]
    fn each_shape_writes_the_documented_payload_length() {
        let cases = [
            (
                SketchShape::Ellipse {
                    bounds: bounds(),
                    fill: None,
                },
                35,
            ),
            (
                SketchShape::Rect {
                    bounds: bounds(),
                    radius: 8,
                    fill: None,
                },
                36,
            ),
            (
                SketchShape::Line {
                    from: SketchEnd::Point { x: 0, y: 0 },
                    to: SketchEnd::Point { x: 1, y: 1 },
                    bend: 0,
                    heads: 0,
                },
                37,
            ),
            (path(&[(0, 0), (16, -8), (32, 40)]), 34),
        ];

        for (shape, expected) in cases {
            for (anchor, extra) in [(None, 0), (Some((1, 2.0)), 8)] {
                let encoded = encode_sketch(&sketch(shape.clone(), anchor));
                let payload = decoded_arg_len(&encoded);
                assert_eq!(
                    payload,
                    expected + extra,
                    "{shape:?} with anchor {anchor:?} writes its documented length",
                );
            }
        }
    }

    /// The decoded length of a one-argument frame's payload, read back through
    /// the decoder rather than computed from the frame's base64 width.
    fn decoded_arg_len(encoded: &[u8]) -> usize {
        let text = std::str::from_utf8(encoded).expect("frames are ascii");
        let arg = text
            .rsplit(';')
            .next()
            .expect("a frame has an argument")
            .trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '=');
        // base64 without padding: four characters carry three bytes.
        let padding = arg.chars().filter(|c| *c == '=').count();
        arg.len() / 4 * 3 - padding
    }

    #[test]
    fn every_shape_round_trips_without_an_anchor() {
        for shape in [
            SketchShape::Ellipse {
                bounds: bounds(),
                fill: None,
            },
            SketchShape::Rect {
                bounds: bounds(),
                radius: 8,
                fill: Some(SketchFill {
                    color: [1, 2, 3],
                    alpha: 64,
                    style: SketchFillStyle::Solid,
                }),
            },
            SketchShape::Line {
                from: SketchEnd::Point { x: -4, y: 40 },
                to: SketchEnd::Component {
                    id: 99,
                    side: SketchSide::Left,
                },
                bend: -24,
                heads: 0b10,
            },
            path(&[(-4, 40), (96, 0), (96, 96)]),
        ] {
            let command = sketch(shape.clone(), None);
            assert_eq!(
                decode(&encode_sketch(&command)),
                Some(Command::Sketch(command.clone())),
                "{shape:?} round-trips",
            );
        }
    }

    #[test]
    fn every_shape_round_trips_with_an_anchor() {
        for shape in [
            SketchShape::Ellipse {
                bounds: bounds(),
                fill: None,
            },
            SketchShape::Rect {
                bounds: bounds(),
                radius: 0,
                fill: None,
            },
            SketchShape::Line {
                from: SketchEnd::Component {
                    id: 1,
                    side: SketchSide::Auto,
                },
                to: SketchEnd::Component {
                    id: 2,
                    side: SketchSide::Bottom,
                },
                bend: 0,
                heads: 0,
            },
            path(&[(0, 0), (-32, 8)]),
        ] {
            let command = sketch(shape.clone(), Some((11, 3.5)));
            assert_eq!(
                decode(&encode_sketch(&command)),
                Some(Command::Sketch(command.clone())),
                "{shape:?} round-trips carrying its anchor",
            );
        }
    }

    /// One byte counts a path's points, so a longer path ships its first 255
    /// rather than a count that wraps and misreads the points after it.
    #[test]
    fn a_path_over_255_points_is_cut_to_255() {
        let points: Vec<(i16, i16)> = (0..300).map(|at| (at, -at)).collect();

        let Some(Command::Sketch(decoded)) = decode(&encode_sketch(&sketch(path(&points), None)))
        else {
            panic!("a path decodes");
        };
        assert_eq!(decoded.shape, path(&points[..255]), "the first 255 points");
    }

    /// An open box and a filled one differ only by the presence byte. A decoder
    /// that ignores it reads the padding as a black fill.
    #[test]
    fn a_rect_without_a_fill_stays_open() {
        let command = sketch(
            SketchShape::Rect {
                bounds: bounds(),
                radius: 4,
                fill: None,
            },
            None,
        );

        let Some(Command::Sketch(decoded)) = decode(&encode_sketch(&command)) else {
            panic!("a rect decodes");
        };
        assert_eq!(decoded.shape, command.shape);
    }

    /// A hatched fill is a code on the wire, not a new sub-command, so both
    /// styles have to survive the round trip under either shape.
    #[test]
    fn a_hatched_fill_round_trips_under_either_shape() {
        for style in [SketchFillStyle::Hachure, SketchFillStyle::CrossHatch] {
            let fill = Some(SketchFill {
                color: [9, 8, 7],
                alpha: 128,
                style,
            });
            for shape in [
                SketchShape::Ellipse {
                    bounds: bounds(),
                    fill,
                },
                SketchShape::Rect {
                    bounds: bounds(),
                    radius: 4,
                    fill,
                },
            ] {
                let command = sketch(shape.clone(), None);
                assert_eq!(
                    decode(&encode_sketch(&command)),
                    Some(Command::Sketch(command.clone())),
                    "{shape:?} round-trips",
                );
            }
        }
    }

    /// The ellipse gained its fill after the shape shipped, so a body that
    /// stops after the bounds reads as an open ring rather than dropping the
    /// frame or reading the absent bytes as an anchor.
    #[test]
    fn an_ellipse_without_its_fill_bytes_stays_open() {
        // The head the decoder reads by index, then the bounds and nothing
        // else, which is every byte such an emitter writes.
        let mut arg = vec![0u8; SKETCH_HEAD];
        arg.extend_from_slice(&[0, 16, 0, 32, 0, 48, 0, 64]);

        let decoded = decode_sketch("sketch_ellipse", &[arg]).expect("a short ellipse decodes");

        assert_eq!(
            (decoded.shape, decoded.anchor),
            (
                SketchShape::Ellipse {
                    bounds: SketchBounds {
                        x: 16,
                        y: 32,
                        w: 48,
                        h: 64,
                    },
                    fill: None,
                },
                None,
            ),
            "the ring draws, and nothing reads an anchor out of the gap",
        );
    }

    #[test]
    fn a_payload_shorter_than_the_head_decodes_to_nothing() {
        // Twenty bytes of head, one short, with no body at all.
        let short = [0u8; SKETCH_HEAD - 1];
        assert_eq!(decode_sketch("sketch_ellipse", &[short.to_vec()]), None);
    }

    #[test]
    fn a_head_without_its_body_decodes_to_nothing() {
        let head_only = [0u8; SKETCH_HEAD];
        for sub in ["sketch_ellipse", "sketch_rect", "sketch_line"] {
            assert_eq!(
                decode_sketch(sub, &[head_only.to_vec()]),
                None,
                "{sub} needs its body",
            );
        }
    }

    #[test]
    fn an_unknown_sub_command_decodes_to_nothing() {
        let arg = [0u8; SKETCH_HEAD + 8];
        assert_eq!(decode_sketch("sketch_spiral", &[arg.to_vec()]), None);
    }

    /// The protocol's rule is that an unknown code degrades to a member the
    /// decoder knows rather than dropping the command around it. These are the
    /// documented landing spots.
    #[test]
    fn unknown_codes_degrade_to_their_documented_defaults() {
        assert_eq!(decode_easing(200), SketchEasing::Smoothstep);
        assert_eq!(decode_phase(200), SketchPhase::Enter);
        assert_eq!(decode_side(200), SketchSide::Auto);
        assert_eq!(decode_fill_style(200), SketchFillStyle::Solid);
    }

    /// An end is fixed-width whichever kind it is, so an unknown kind byte must
    /// still consume seven bytes or the second end reads from the wrong place.
    #[test]
    fn an_unknown_end_kind_reads_as_a_point() {
        let bytes = [9, 0x01, 0x02, 0x03, 0x04, 0, 0];
        assert_eq!(
            read_end(&bytes),
            Some(SketchEnd::Point {
                x: 0x0102,
                y: 0x0304
            }),
        );
    }

    /// A zero seed is the emitter saying it has no opinion, which the terminal
    /// resolves. It has to survive the wire as zero rather than being replaced
    /// here.
    #[test]
    fn a_zero_seed_survives_the_wire() {
        let (mut style, timing) = head();
        style.seed = 0;
        let command = SketchCommand {
            id: 3,
            style,
            timing,
            shape: SketchShape::Ellipse {
                bounds: bounds(),
                fill: None,
            },
            anchor: None,
        };

        let Some(Command::Sketch(decoded)) = decode(&encode_sketch(&command)) else {
            panic!("an ellipse decodes");
        };
        assert_eq!(decoded.style.seed, 0);
    }
}
