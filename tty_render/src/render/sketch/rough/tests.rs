//! Tests for the hand-drawn geometry generator.

use super::*;
use stoatty_protocol::command::{
    SketchEasing, SketchFill, SketchFillStyle, SketchPhase, SketchTiming,
};

fn options() -> Options {
    Options::new(1.0)
}

/// The generator is transcribed, not invented, so its first draws are
/// checked against the reference sequence rather than against itself.
#[test]
fn the_stream_matches_the_reference_sequence() {
    let mut random = Random::new(1, 0);
    let seeds: Vec<i32> = (0..4)
        .map(|_| {
            random.next();
            random.seed
        })
        .collect();

    assert_eq!(seeds, [48271, 182605793, 1291342511, 1533981633]);
}

/// A zero seed means the emitter had no opinion, and the terminal picks one
/// from the id. A clock instead redraws the mark differently on every
/// zoom step.
#[test]
fn a_zero_seed_is_derived_from_the_id() {
    let mut from_id = Random::new(0, 7);
    let mut again = Random::new(0, 7);
    let mut other = Random::new(0, 8);

    assert_eq!(from_id.next(), again.next(), "the same id gives one stream");
    assert_ne!(
        Random::new(0, 7).next(),
        other.next(),
        "another id gives another",
    );
}

#[test]
fn one_seed_reproduces_and_two_seeds_differ() {
    let shape = |seed: u32| {
        let mut random = Random::new(seed, 0);
        let mut opts = options();
        flatten(&ellipse(50.0, 50.0, 80.0, 40.0, &mut opts, &mut random))
    };

    assert_eq!(shape(11), shape(11), "one seed draws one mark");
    assert_ne!(shape(11), shape(12), "another seed draws another");
}

/// The wobble is bounded, so a mark stays inside the box that declared it
/// rather than wandering across the screen.
#[test]
fn an_ellipse_stays_near_its_center() {
    let mut random = Random::new(3, 0);
    let mut opts = options();
    let strokes = flatten(&ellipse(100.0, 100.0, 80.0, 60.0, &mut opts, &mut random));

    let (rx, ry) = (40.0_f32, 30.0_f32);
    let slack = 3.0 * opts.roughness as f32 + opts.max_randomness_offset as f32;
    for stroke in &strokes {
        for point in &stroke.points {
            let dx = (point[0] - 100.0).abs();
            let dy = (point[1] - 100.0).abs();
            assert!(
                dx <= rx + slack && dy <= ry + slack,
                "point {point:?} escaped the declared box",
            );
        }
    }
}

/// With vertices preserved, a box's sides meet at the corners it was asked
/// for. Without that, low-roughness boxes show gaps where the sides miss.
#[test]
fn a_preserved_rect_starts_and_ends_on_its_corners() {
    let mut random = Random::new(5, 0);
    let mut opts = options();
    assert!(opts.preserve_vertices, "roughness 1 preserves vertices");

    let strokes = flatten(&rect(10.0, 20.0, 60.0, 40.0, 0.0, &mut opts, &mut random));
    let corners = [[10.0, 20.0], [70.0, 20.0], [70.0, 60.0], [10.0, 60.0]];

    for stroke in &strokes {
        let first = stroke.points[0];
        let last = stroke.points[stroke.points.len() - 1];
        for end in [first, last] {
            let near = corners
                .iter()
                .any(|c: &[f32; 2]| (c[0] - end[0]).abs() < 0.01 && (c[1] - end[1]).abs() < 0.01);
            assert!(near, "stroke end {end:?} is not on a corner");
        }
    }
}

/// The reveal reads these as a distance axis, so they must rise and the
/// last must be the whole length. Drifted sums reveal at the wrong rate.
#[test]
fn prefix_lengths_rise_to_the_summed_length() {
    let mut random = Random::new(9, 0);
    let mut opts = options();
    let strokes = flatten(&ellipse(0.0, 0.0, 120.0, 90.0, &mut opts, &mut random));
    assert!(!strokes.is_empty(), "an ellipse draws something");

    for stroke in &strokes {
        assert_eq!(stroke.lengths.len(), stroke.points.len());
        assert_eq!(stroke.lengths[0], 0.0, "the first point is at distance 0");
        assert!(
            stroke.lengths.windows(2).all(|w| w[1] >= w[0]),
            "distances never go backwards",
        );

        let summed: f32 = stroke
            .points
            .windows(2)
            .map(|w| ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2)).sqrt())
            .sum();
        let last = stroke.lengths[stroke.lengths.len() - 1];
        assert!(
            (last - summed).abs() < 0.01,
            "the last distance is the whole length, got {last} against {summed}",
        );
    }
}

/// Below the thresholds a wobble sized for a large shape reads as noise, so
/// it is damped rather than left to swamp the mark.
#[test]
fn roughness_is_damped_only_for_a_small_shape() {
    assert_eq!(
        damped_roughness(2.0, 100.0, 60.0),
        2.0,
        "a large box keeps it"
    );
    assert_eq!(
        damped_roughness(2.0, 30.0, 15.0),
        1.0,
        "a short side halves it"
    );
    assert_eq!(damped_roughness(3.0, 8.0, 8.0), 1.0, "a tiny box thirds it");
    assert_eq!(
        damped_roughness(9.0, 30.0, 15.0),
        2.5,
        "the damped value is capped",
    );
}

/// A single move draws nothing, so it must not become a stroke with one
/// point and a zero-length distance axis.
#[test]
fn a_lone_move_makes_no_stroke() {
    assert_eq!(flatten(&[Op::Move([1.0, 2.0])]), Vec::new());
}

/// Every bezier flattens to the same count, so a mark regenerated at
/// another size keeps each point at the same place along its stroke.
#[test]
fn a_bezier_flattens_to_a_fixed_count() {
    let strokes = flatten(&[
        Op::Move([0.0, 0.0]),
        Op::Curve([1.0, 1.0, 2.0, 1.0, 3.0, 0.0]),
    ]);

    let [stroke] = strokes.as_slice() else {
        panic!("one move and one curve make one stroke, got {strokes:?}");
    };
    assert_eq!(stroke.points.len(), BEZIER_SEGMENTS + 1);
}

fn metrics() -> CellMetrics {
    CellMetrics {
        font_size: 16.0,
        width: 10.0,
        height: 20.0,
        scale_factor: 1.0,
    }
}

/// Roughness zero makes the wobble vanish, so a geometric claim can be
/// checked exactly instead of against a tolerance.
fn command(shape: SketchShape, roughness: u8) -> SketchCommand {
    SketchCommand {
        id: 1,
        style: SketchStyle {
            color: [255, 0, 0],
            alpha: 255,
            width: 64,
            roughness,
            seed: 7,
        },
        timing: SketchTiming {
            delay_ms: 0,
            duration_ms: 400,
            easing: SketchEasing::Linear,
            phase: SketchPhase::Enter,
        },
        shape,
        anchor: None,
    }
}

fn nothing_resolves(_: u32) -> Option<[f32; 4]> {
    None
}

/// A box is declared in cell fractions so it covers the same cells at every
/// font size. A cell is taller than it is wide, so the two axes scale by
/// different numbers and a single scalar puts the mark in the wrong place.
#[test]
fn a_declared_box_lands_where_the_metrics_put_it() {
    let shape = SketchShape::Rect {
        bounds: SketchBounds {
            x: 16,
            y: 16,
            w: 32,
            h: 32,
        },
        radius: 0,
        fill: Some(SketchFill {
            color: [0, 0, 255],
            alpha: 128,
            style: SketchFillStyle::Solid,
        }),
    };
    let geometry = geometry(&command(shape, 0), metrics(), &nothing_resolves);

    assert_eq!(
        geometry.fill,
        Some([[10.0, 20.0], [30.0, 20.0], [30.0, 60.0], [10.0, 60.0]]),
        "one cell across is a cell width, one cell down is a cell height",
    );
}

#[test]
fn an_open_box_carries_no_fill() {
    let shape = SketchShape::Rect {
        bounds: SketchBounds {
            x: 0,
            y: 0,
            w: 32,
            h: 32,
        },
        radius: 0,
        fill: None,
    };
    let geometry = geometry(&command(shape, 64), metrics(), &nothing_resolves);

    assert_eq!(geometry.fill, None);
    assert!(!geometry.strokes.is_empty(), "the box is still stroked");
}

/// A connector names what it points at, and the caller resolves the name.
/// An unresolvable end draws nothing, because a connector left dangling at
/// the origin points at the wrong thing rather than at nothing.
#[test]
fn a_connector_to_a_missing_component_draws_nothing() {
    let shape = SketchShape::Line {
        from: SketchEnd::Point { x: 0, y: 0 },
        to: SketchEnd::Component {
            id: 99,
            side: SketchSide::Auto,
        },
        bend: 0,
        heads: 0,
    };
    let geometry = geometry(&command(shape, 64), metrics(), &nothing_resolves);

    assert_eq!(geometry.strokes, Vec::new());
}

#[test]
fn a_zero_length_connector_draws_nothing() {
    let shape = SketchShape::Line {
        from: SketchEnd::Point { x: 8, y: 8 },
        to: SketchEnd::Point { x: 8, y: 8 },
        bend: 0,
        heads: 0,
    };
    let geometry = geometry(&command(shape, 64), metrics(), &nothing_resolves);

    assert_eq!(geometry.strokes, Vec::new());
}

/// The bend is what separates a connector that arcs around a subject from
/// one that cuts through it, so it must actually leave the chord.
#[test]
fn a_bend_carries_the_connector_off_the_chord() {
    let line = |bend: i8| {
        let shape = SketchShape::Line {
            from: SketchEnd::Point { x: 0, y: 0 },
            to: SketchEnd::Point { x: 96, y: 0 },
            bend,
            heads: 0,
        };
        geometry(&command(shape, 0), metrics(), &nothing_resolves)
    };

    let straight = line(0);
    assert!(
        straight
            .strokes
            .iter()
            .flat_map(|s| &s.points)
            .all(|p| p[1].abs() < 0.001),
        "an unbent connector stays on the chord",
    );

    let bent = line(32);
    let low = bent
        .strokes
        .iter()
        .flat_map(|s| &s.points)
        .fold(0.0_f32, |acc, p| acc.max(p[1]));
    assert!(
        low > 1.0,
        "a bent connector leaves the chord, reached {low}"
    );
}

/// Both bits of the mask are read, so a connector can point at one end, the
/// other, or both.
#[test]
fn each_head_bit_adds_its_arrow() {
    let count = |heads: u8| {
        let shape = SketchShape::Line {
            from: SketchEnd::Point { x: 0, y: 0 },
            to: SketchEnd::Point { x: 96, y: 0 },
            bend: 0,
            heads,
        };
        geometry(&command(shape, 64), metrics(), &nothing_resolves)
            .strokes
            .len()
    };

    let bare = count(0);
    let one = count(1);
    assert!(one > bare, "one head adds strokes, {bare} to {one}");
    assert_eq!(count(2) - bare, one - bare, "either end costs the same");
    assert_eq!(count(3) - bare, 2 * (one - bare), "both ends cost twice");
}

/// An emitter that re-declares its whole decoration set every frame must
/// get the same mark back, or the sketch shimmers.
#[test]
fn one_command_regenerates_the_same_geometry() {
    let shape = SketchShape::Ellipse(SketchBounds {
        x: 4,
        y: 4,
        w: 48,
        h: 24,
    });
    let command = command(shape, 96);
    let draw = || geometry(&command, metrics(), &nothing_resolves);

    assert_eq!(draw(), draw());
}

/// A weight in pixels thins as the font grows. Stating it against the cell
/// keeps a mark's apparent weight through a zoom.
#[test]
fn stroke_weight_tracks_the_cell_width() {
    let style = SketchStyle {
        color: [0, 0, 0],
        alpha: 255,
        width: 64,
        roughness: 64,
        seed: 1,
    };
    assert_eq!(
        stroke_width(&style, metrics()),
        2.5,
        "64/256 of a 10px cell"
    );

    let mut wide = metrics();
    wide.width = 20.0;
    assert_eq!(stroke_width(&style, wide), 5.0, "a doubled cell doubles it");
}

/// The wobble is a chain of draws from one shared stream, so the order of
/// the draws is part of the output. Reordering them, or dropping one, moves
/// every point after it while every other test here still passes.
///
/// This pins the transcription in place against that drift. It says nothing
/// about whether the transcription matched the reference to begin with,
/// which only reading the reference establishes.
#[test]
fn the_draw_order_is_pinned_to_its_transcription() {
    let shape = SketchShape::Rect {
        bounds: SketchBounds {
            x: 0,
            y: 0,
            w: 64,
            h: 32,
        },
        radius: 0,
        fill: None,
    };
    let geometry = geometry(&command(shape, 64), metrics(), &nothing_resolves);

    assert_eq!(geometry.strokes.len(), 8, "four sides, each stroked twice");

    let head: Vec<[f32; 2]> = geometry.strokes[0]
        .points
        .iter()
        .take(4)
        .map(|p| [(p[0] * 1e4).round() / 1e4, (p[1] * 1e4).round() / 1e4])
        .collect();
    assert_eq!(
        head,
        [
            [0.0, 0.0],
            [2.7809, -0.1388],
            [5.9531, -0.1889],
            [9.638, -0.1748],
        ],
    );

    let tail = &geometry.strokes[7];
    assert_eq!(
        (tail.lengths[tail.lengths.len() - 1] * 1e4).round() / 1e4,
        40.0081,
        "the last stroke sits downstream of every draw in the run",
    );
}

/// A connector between two components leaves each box facing outward before
/// it turns, so it reads as joining them rather than cutting the corner off
/// whichever one it starts at.
#[test]
fn two_components_are_joined_by_an_s_curve() {
    let ends = |from, to| {
        let shape = SketchShape::Line {
            from,
            to,
            bend: 0,
            heads: 0,
        };
        // Two small boxes on one horizontal line, so an auto side has an
        // unambiguous facing edge.
        let resolve = |id: u32| match id {
            1 => Some([0.0, 0.0, 8.0, 8.0]),
            _ => Some([100.0, 0.0, 108.0, 8.0]),
        };
        geometry(&command(shape, 0), metrics(), &resolve)
    };

    let straight = ends(
        SketchEnd::Point { x: 0, y: 0 },
        SketchEnd::Point { x: 160, y: 0 },
    );
    assert!(
        straight
            .strokes
            .iter()
            .flat_map(|s| &s.points)
            .all(|p| p[1].abs() < 0.001),
        "two fixed points are joined by a straight line",
    );

    let curved = ends(
        SketchEnd::Component {
            id: 1,
            side: SketchSide::Bottom,
        },
        SketchEnd::Component {
            id: 2,
            side: SketchSide::Top,
        },
    );
    let reach = curved
        .strokes
        .iter()
        .flat_map(|s| &s.points)
        .fold(0.0_f32, |acc, p| acc.max(p[1].abs()));
    assert!(reach > 1.0, "the curve leaves the chord, reached {reach}");
}

/// A rounded box strokes its corners as well as its sides, and the corners
/// stay inside the box the emitter declared.
#[test]
fn a_rounded_box_strokes_its_corners_inside_its_bounds() {
    let box_at = |radius: u8| {
        let shape = SketchShape::Rect {
            bounds: SketchBounds {
                x: 0,
                y: 0,
                w: 64,
                h: 64,
            },
            radius,
            fill: None,
        };
        geometry(&command(shape, 0), metrics(), &nothing_resolves)
    };

    let square = box_at(0);
    let rounded = box_at(8);
    assert_eq!(square.strokes.len(), 8, "four sides, each stroked twice");
    assert_eq!(rounded.strokes.len(), 16, "four corners join them");

    for point in rounded.strokes.iter().flat_map(|s| &s.points) {
        assert!(
            (0.0..=40.0).contains(&point[0]) && (0.0..=80.0).contains(&point[1]),
            "point {point:?} left the declared box",
        );
    }
}

/// A crisp rectangle behind a wobbling outline reads machine-cut, so the fill's
/// corners are nudged like the stroke around them.
///
/// The shader resolves the fill as a convex quad, so a corner that crossed its
/// neighbour would turn the shape inside out. The nudge stays inside the box's
/// own quarter for that reason.
#[test]
fn a_fill_quad_is_nudged_and_stays_convex() {
    let seeded = |w: u16, h: u16, seed: u32| {
        let shape = SketchShape::Rect {
            bounds: SketchBounds { x: 0, y: 0, w, h },
            radius: 0,
            fill: Some(SketchFill {
                color: [0, 0, 255],
                alpha: 255,
                style: SketchFillStyle::Solid,
            }),
        };
        let mut command = command(shape, 64);
        command.style.seed = seed;
        geometry(&command, metrics(), &nothing_resolves)
            .fill
            .expect("a filled box carries a quad")
    };
    let filled = |w: u16, h: u16| seeded(w, h, 7);

    let quad = filled(64, 64);
    let crisp = [[0.0, 0.0], [40.0, 0.0], [40.0, 80.0], [0.0, 80.0]];
    assert_ne!(quad, crisp, "the corners do not sit on the declared box");

    for quad in [quad, filled(4, 4), filled(2, 8)] {
        let cross = |at: usize| {
            let (a, b, c) = (quad[at], quad[(at + 1) % 4], quad[(at + 2) % 4]);
            (b[0] - a[0]) * (c[1] - b[1]) - (b[1] - a[1]) * (c[0] - b[0])
        };
        let signs: Vec<bool> = (0..4).map(|at| cross(at) > 0.0).collect();
        assert!(
            signs.iter().all(|&sign| sign == signs[0]),
            "every turn goes the same way, so the quad stays convex: {quad:?}",
        );
    }

    // The cap is what keeps a box smaller than the nudge convex, so it is
    // checked on its own. One seed proves little here, because an uncapped
    // nudge only sometimes draws far enough to cross; a fixed spread of them
    // makes the difference certain while staying reproducible.
    let (w, h) = (4.0 / 16.0 * metrics().width, 4.0 / 16.0 * metrics().height);
    let quarter = w.min(h) / 4.0;
    let declared = [[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]];
    for seed in 1..=16 {
        let tiny = seeded(4, 4, seed);
        for (corner, declared) in tiny.iter().zip(&declared) {
            assert!(
                (corner[0] - declared[0]).abs() <= quarter + 1e-4
                    && (corner[1] - declared[1]).abs() <= quarter + 1e-4,
                "at seed {seed} corner {corner:?} strayed past {quarter} from {declared:?}",
            );
        }
    }
}
