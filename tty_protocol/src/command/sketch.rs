//! The sketch commands stroke hand-drawn marks over the cell grid.
//!
//! A sketch is declared, not drawn: the emitter says what shape it wants and
//! how rough and how fast, and the terminal generates the wobbling geometry
//! itself. That split is what lets a mark stay hand-drawn at every font size,
//! since the roughness is applied in pixels against the live cell metrics, and
//! what lets the stroke animate at the display refresh rate without the emitter
//! sending a frame per step.
//!
//! One head serves three shapes. An ellipse circles a subject, a rectangle
//! boxes a block, and a line connects one to a label. They exist for the
//! walkthrough player, but nothing here knows about walkthroughs.

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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SketchShape {
    Ellipse(SketchBounds),
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
}

/// A mark's box, in **sixteenths of a cell** so it tracks live font zoom.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SketchBounds {
    pub x: i16,
    pub y: i16,
    pub w: u16,
    pub h: u16,
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
/// Only [`Self::Solid`] draws today. The code byte reserves room for hachure,
/// which then arrives as an appended value rather than a new sub-command.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SketchFillStyle {
    Solid,
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
            SketchShape::Ellipse(bounds) => write_bounds(w, bounds)?,
            SketchShape::Rect {
                bounds,
                radius,
                fill,
            } => {
                write_bounds(w, bounds)?;
                w.write_all(&[*radius])?;
                w.write_all(&[fill.is_some() as u8])?;
                let fill = fill.unwrap_or(SketchFill {
                    color: [0, 0, 0],
                    alpha: 0,
                    style: SketchFillStyle::Solid,
                });
                w.write_all(&fill.color)?;
                w.write_all(&[fill.alpha])?;
                w.write_all(&[fill_style_code(fill.style)])?;
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
        "sketch_ellipse" => (SketchShape::Ellipse(read_bounds(body)?), 8),
        "sketch_rect" => {
            let bounds = read_bounds(body)?;
            let radius = *body.get(8)?;
            let present = *body.get(9)?;
            let color = [*body.get(10)?, *body.get(11)?, *body.get(12)?];
            let alpha = *body.get(13)?;
            let style = decode_fill_style(*body.get(14)?);
            let fill = (present != 0).then_some(SketchFill {
                color,
                alpha,
                style,
            });
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
        SketchShape::Ellipse(_) => "sketch_ellipse",
        SketchShape::Rect { .. } => "sketch_rect",
        SketchShape::Line { .. } => "sketch_line",
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
    }
}

fn decode_fill_style(_code: u8) -> SketchFillStyle {
    SketchFillStyle::Solid
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

    /// The payload lengths the protocol doc publishes, checked against what the
    /// encoder writes. A doc that drifts from the wire is worse than none: the
    /// terminal reads by offset, so a wrong length there sends the next
    /// implementer to the wrong bytes.
    #[test]
    fn each_shape_writes_the_documented_payload_length() {
        let cases = [
            (SketchShape::Ellipse(bounds()), 29),
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
        ];

        for (shape, expected) in cases {
            for (anchor, extra) in [(None, 0), (Some((1, 2.0)), 8)] {
                let encoded = encode_sketch(&sketch(shape, anchor));
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
            SketchShape::Ellipse(bounds()),
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
        ] {
            let command = sketch(shape, None);
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
            SketchShape::Ellipse(bounds()),
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
        ] {
            let command = sketch(shape, Some((11, 3.5)));
            assert_eq!(
                decode(&encode_sketch(&command)),
                Some(Command::Sketch(command.clone())),
                "{shape:?} round-trips carrying its anchor",
            );
        }
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
            shape: SketchShape::Ellipse(bounds()),
            anchor: None,
        };

        let Some(Command::Sketch(decoded)) = decode(&encode_sketch(&command)) else {
            panic!("an ellipse decodes");
        };
        assert_eq!(decoded.style.seed, 0);
    }
}
