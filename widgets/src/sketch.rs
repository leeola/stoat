//! Hand-drawn marks that annotate what the cells below them show.
//!
//! A mark is declared, not drawn: the widget says what shape it wants and how
//! rough and how fast, and the terminal generates the wobbling stroke and
//! animates it. That split is what keeps a mark hand-drawn at every font size
//! and lets it draw itself at the display refresh rate with no frame per step
//! from here.
//!
//! Coordinates are in **sixteenths of a cell** (16 = one cell) relative to the
//! render area's top-left, like [`crate::polyline::Polyline`], so a mark tracks
//! live font zoom.
//!
//! There is no cell fallback. A wobbling stroke is inherently sub-cell, so a
//! terminal that does not know the command draws nothing and the cells beneath
//! read on their own. A caller wanting the annotation to survive that writes
//! glyphs instead.

use crate::{cells, ApcScene};
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
use stoatty_protocol::command::{
    self, SketchBounds, SketchCommand, SketchEnd, SketchFill, SketchShape, SketchStyle,
    SketchTiming,
};

/// An ellipse circling a subject.
///
/// The shape an annotation reaches for first. A circle around a word says
/// "this" without covering what it points at, which a filled box does.
pub struct SketchEllipse {
    /// The mark's identity across frames, chosen by the caller.
    ///
    /// The terminal latches the timing when an id first appears, so an emitter
    /// that re-declares its whole scene every frame does not restart the
    /// stroke. A new id starts a new draw, which is how a scene replays.
    pub id: u32,
    pub style: SketchStyle,
    pub timing: SketchTiming,
    /// The box the ellipse is inscribed in, in sixteenths from the area's
    /// top-left.
    pub bounds: SketchBounds,
    /// Pool this mark rides, and that pool's top row, so it glides with a
    /// scrolling pane. `None` leaves it fixed to the screen.
    pub anchor: Option<(u32, f32)>,
}

/// A rectangle boxing a block, optionally filled.
pub struct SketchRect {
    pub id: u32,
    pub style: SketchStyle,
    pub timing: SketchTiming,
    /// The box itself, in sixteenths from the area's top-left.
    pub bounds: SketchBounds,
    /// Corner rounding in sixteenths of a cell. Zero draws square corners.
    pub radius: u8,
    /// Painted inside the stroke, fading in behind it. `None` leaves the box
    /// open, which is what keeps the cells inside readable.
    pub fill: Option<SketchFill>,
    pub anchor: Option<(u32, f32)>,
}

/// A line or curve connecting one thing to another.
///
/// An end naming a component tracks that mark as it moves, which is what lets a
/// connector point at a circle without repeating its coordinates.
pub struct SketchLine {
    pub id: u32,
    pub style: SketchStyle,
    pub timing: SketchTiming,
    pub from: SketchEnd,
    pub to: SketchEnd,
    /// How far the curve bows off the straight chord, in 64ths of the chord's
    /// length, the sign picking the side. Zero asks for an S-curve when both
    /// ends name components, and draws straight otherwise.
    pub bend: i8,
    /// Bit 0 puts an arrowhead at [`Self::from`], bit 1 at [`Self::to`].
    pub heads: u8,
    pub anchor: Option<(u32, f32)>,
}

impl StatefulWidget for SketchEllipse {
    type State = ApcScene;

    fn render(self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        emit(
            scene,
            self.id,
            self.style,
            self.timing,
            SketchShape::Ellipse(absolute_bounds(area, self.bounds)),
            self.anchor,
        );
    }
}

impl StatefulWidget for SketchRect {
    type State = ApcScene;

    fn render(self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        emit(
            scene,
            self.id,
            self.style,
            self.timing,
            SketchShape::Rect {
                bounds: absolute_bounds(area, self.bounds),
                radius: self.radius,
                fill: self.fill,
            },
            self.anchor,
        );
    }
}

impl StatefulWidget for SketchLine {
    type State = ApcScene;

    fn render(self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        emit(
            scene,
            self.id,
            self.style,
            self.timing,
            SketchShape::Line {
                from: absolute_end(area, self.from),
                to: absolute_end(area, self.to),
                bend: self.bend,
                heads: self.heads,
            },
            self.anchor,
        );
    }
}

fn emit(
    scene: &mut ApcScene,
    id: u32,
    style: SketchStyle,
    timing: SketchTiming,
    shape: SketchShape,
    anchor: Option<(u32, f32)>,
) {
    command::encode_sketch_into(
        scene.buffer(),
        &SketchCommand {
            id,
            style,
            timing,
            shape,
            anchor,
        },
    );
}

/// Shift a declared box from the area's top-left to the surface's.
fn absolute_bounds(area: Rect, bounds: SketchBounds) -> SketchBounds {
    SketchBounds {
        x: cells::to_sixteenths(area.x, bounds.x),
        y: cells::to_sixteenths(area.y, bounds.y),
        ..bounds
    }
}

/// Shift a fixed line end into surface coordinates.
///
/// A component end passes through unchanged. It names a mark rather than a
/// place, and that mark's own position is already absolute.
fn absolute_end(area: Rect, end: SketchEnd) -> SketchEnd {
    match end {
        SketchEnd::Point { x, y } => SketchEnd::Point {
            x: cells::to_sixteenths(area.x, x),
            y: cells::to_sixteenths(area.y, y),
        },
        component => component,
    }
}

#[cfg(test)]
mod tests {
    use super::{SketchEllipse, SketchLine, SketchRect};
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{
        self, encode_sketch, Command, SketchBounds, SketchCommand, SketchEnd, SketchFill,
        SketchFillStyle, SketchShape, SketchSide, SketchStyle, SketchTiming,
    };

    fn scene_and_buf() -> (ApcScene, Buffer) {
        (ApcScene::new(), Buffer::empty(Rect::new(0, 0, 80, 24)))
    }

    /// The area's origin, two cells across and one down, is 32 and 16
    /// sixteenths.
    fn area() -> Rect {
        Rect::new(2, 1, 10, 4)
    }

    fn expected(shape: SketchShape) -> Vec<u8> {
        encode_sketch(&SketchCommand {
            id: 3,
            style: SketchStyle::marker([255, 0, 0]),
            timing: SketchTiming::after(100, 400),
            shape,
            anchor: None,
        })
    }

    /// The widgets encode; the terminal decodes. A scene that encodes cleanly
    /// but decodes to something else reaches the window silently wrong, and
    /// comparing against the encoder alone never sees it.
    #[test]
    fn a_scene_of_marks_decodes_back_to_what_was_declared() {
        let (mut scene, mut buf) = scene_and_buf();
        let style = SketchStyle::marker([229, 192, 123]);
        let timing = SketchTiming::after(200, 700);

        SketchEllipse {
            id: 1,
            style,
            timing,
            bounds: SketchBounds {
                x: 0,
                y: 0,
                w: 96,
                h: 28,
            },
            anchor: None,
        }
        .render(area(), &mut buf, &mut scene);

        SketchLine {
            id: 2,
            style,
            timing,
            from: SketchEnd::Component {
                id: 1,
                side: SketchSide::Auto,
            },
            to: SketchEnd::Point { x: 320, y: 64 },
            bend: 0,
            heads: 2,
            anchor: Some((4, 3.5)),
        }
        .render(area(), &mut buf, &mut scene);

        let decoded: Vec<SketchCommand> = command::decode_stream(scene.buffer())
            .into_iter()
            .filter_map(|command| match command {
                Command::Sketch(sketch) => Some(sketch),
                _ => None,
            })
            .collect();

        assert_eq!(
            decoded,
            vec![
                SketchCommand {
                    id: 1,
                    style,
                    timing,
                    shape: SketchShape::Ellipse(SketchBounds {
                        x: 2 * 16,
                        y: 16,
                        w: 96,
                        h: 28,
                    }),
                    anchor: None,
                },
                SketchCommand {
                    id: 2,
                    style,
                    timing,
                    shape: SketchShape::Line {
                        from: SketchEnd::Component {
                            id: 1,
                            side: SketchSide::Auto,
                        },
                        to: SketchEnd::Point {
                            x: 2 * 16 + 320,
                            y: 16 + 64,
                        },
                        bend: 0,
                        heads: 2,
                    },
                    anchor: Some((4, 3.5)),
                },
            ],
            "every field survives the round trip, in declaration order",
        );
    }

    /// A mark is declared against the widget's own area, so every fixed
    /// coordinate shifts by that area's origin before it goes on the wire.
    #[test]
    fn an_ellipse_lands_at_absolute_sixteenths() {
        let (mut scene, mut buf) = scene_and_buf();

        SketchEllipse {
            id: 3,
            style: SketchStyle::marker([255, 0, 0]),
            timing: SketchTiming::after(100, 400),
            bounds: SketchBounds {
                x: 4,
                y: 8,
                w: 64,
                h: 32,
            },
            anchor: None,
        }
        .render(area(), &mut buf, &mut scene);

        assert_eq!(
            scene.buffer(),
            &expected(SketchShape::Ellipse(SketchBounds {
                x: 2 * 16 + 4,
                y: 16 + 8,
                w: 64,
                h: 32,
            })),
            "the origin shifts, and the size does not",
        );
    }

    #[test]
    fn a_rect_carries_its_rounding_and_fill() {
        let (mut scene, mut buf) = scene_and_buf();
        let fill = SketchFill {
            color: [0, 0, 255],
            alpha: 128,
            style: SketchFillStyle::Solid,
        };

        SketchRect {
            id: 3,
            style: SketchStyle::marker([255, 0, 0]),
            timing: SketchTiming::after(100, 400),
            bounds: SketchBounds {
                x: 0,
                y: 0,
                w: 48,
                h: 48,
            },
            radius: 6,
            fill: Some(fill),
            anchor: None,
        }
        .render(area(), &mut buf, &mut scene);

        assert_eq!(
            scene.buffer(),
            &expected(SketchShape::Rect {
                bounds: SketchBounds {
                    x: 2 * 16,
                    y: 16,
                    w: 48,
                    h: 48,
                },
                radius: 6,
                fill: Some(fill),
            }),
        );
    }

    /// A component end names a mark rather than a place, and that mark's own
    /// position is already absolute. Shifting it by the area moves the
    /// connector off whatever it points at.
    #[test]
    fn a_component_end_passes_through_while_a_point_shifts() {
        let (mut scene, mut buf) = scene_and_buf();
        let component = SketchEnd::Component {
            id: 9,
            side: SketchSide::Auto,
        };

        SketchLine {
            id: 3,
            style: SketchStyle::marker([255, 0, 0]),
            timing: SketchTiming::after(100, 400),
            from: SketchEnd::Point { x: 4, y: 8 },
            to: component,
            bend: -12,
            heads: 2,
            anchor: None,
        }
        .render(area(), &mut buf, &mut scene);

        assert_eq!(
            scene.buffer(),
            &expected(SketchShape::Line {
                from: SketchEnd::Point {
                    x: 2 * 16 + 4,
                    y: 16 + 8,
                },
                to: component,
                bend: -12,
                heads: 2,
            }),
        );
    }

    /// An anchored mark rides a pool, and the pool it names is the terminal's
    /// own id rather than anything area-relative.
    #[test]
    fn an_anchor_reaches_the_wire_unchanged() {
        let (mut scene, mut buf) = scene_and_buf();

        SketchEllipse {
            id: 3,
            style: SketchStyle::marker([255, 0, 0]),
            timing: SketchTiming::after(100, 400),
            bounds: SketchBounds {
                x: 0,
                y: 0,
                w: 16,
                h: 16,
            },
            anchor: Some((7, 12.5)),
        }
        .render(area(), &mut buf, &mut scene);

        assert_eq!(
            scene.buffer(),
            &encode_sketch(&SketchCommand {
                id: 3,
                style: SketchStyle::marker([255, 0, 0]),
                timing: SketchTiming::after(100, 400),
                shape: SketchShape::Ellipse(SketchBounds {
                    x: 2 * 16,
                    y: 16,
                    w: 16,
                    h: 16,
                }),
                anchor: Some((7, 12.5)),
            }),
        );
    }
}
