//! Tests for the hand-drawn mark pass.

use super::*;
use crate::{gpu::headless_device, render::AnchoredPanel};
use stoatty_protocol::command::{
    SketchBounds, SketchCommand, SketchEasing, SketchEnd, SketchFill, SketchPhase, SketchSide,
    SketchStyle, SketchTiming,
};
use wgpu::{
    naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    },
    Color, CommandEncoderDescriptor, Extent3d, LoadOp, MapMode, Operations, Origin3d, PollType,
    RenderPassColorAttachment, RenderPassDescriptor, StoreOp, TexelCopyBufferInfo,
    TexelCopyBufferLayout, TexelCopyTextureInfo, TextureAspect, TextureDescriptor,
    TextureDimension, TextureFormat, TextureUsages, TextureViewDescriptor,
};

/// A reserved WGSL keyword, a type mismatch, or a stale binding fails at
/// pipeline creation rather than at compile time, so nothing catches one until
/// a machine with an adapter runs the readback tests. This catches it
/// everywhere.
#[test]
fn shader_is_valid_wgsl() {
    let module = wgsl::parse_str(&crate::render::with_occlusion(include_str!(
        "../../../shaders/sketch.wgsl"
    )))
    .expect("parse sketch");
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .expect("validate sketch");
}

/// The readback target's edge, in pixels. A row must be a multiple of 256
/// bytes for the copy to the readback buffer, which four bytes a texel makes 64.
const TARGET: u32 = 128;

fn metrics() -> CellMetrics {
    CellMetrics {
        font_size: 16.0,
        width: 8.0,
        height: 16.0,
        scale_factor: 1.0,
    }
}

fn sketch(id: u32, shape: SketchShape) -> Sketch {
    Sketch {
        command: SketchCommand {
            id,
            style: SketchStyle {
                color: [255, 0, 0],
                alpha: 255,
                width: 128,
                roughness: 64,
                seed: 5,
            },
            timing: SketchTiming {
                delay_ms: 0,
                duration_ms: 400,
                easing: SketchEasing::Linear,
                phase: SketchPhase::Enter,
            },
            shape,
            anchor: None,
        },
        seq: id,
    }
}

fn boxed(x: i16, y: i16, w: u16, h: u16) -> SketchBounds {
    SketchBounds { x, y, w, h }
}

/// The strokes and their shared points for one mark list at one cell size.
fn marks(sketches: &[Sketch]) -> (Vec<[f32; 2]>, Vec<MarkGeometry>) {
    let mut geometry = Vec::new();
    let points = generate_marks(sketches, metrics(), &mut geometry);
    (points, geometry)
}

fn instances(sketches: &[Sketch], progress: &[f32]) -> Vec<SketchInstance> {
    let (_, geometry) = marks(sketches);
    let (mut built, mut riding) = (Vec::new(), Vec::new());
    build_instances(
        sketches,
        &geometry,
        progress,
        &[],
        metrics(),
        &mut built,
        &mut riding,
    );
    built
}

/// Every stroke's points land in the shared arena, and each span names its own
/// slice of it. An overlapping span draws another stroke's points.
#[test]
fn every_stroke_names_its_own_span_of_the_shared_points() {
    let list = [sketch(1, SketchShape::Ellipse(boxed(0, 0, 64, 32)))];
    let (points, geometry) = marks(&list);

    let [mark] = geometry.as_slice() else {
        panic!("one sketch generates one mark, got {}", geometry.len());
    };
    assert!(!mark.strokes.is_empty(), "an ellipse strokes something");

    let mut next = 0;
    for stroke in &mark.strokes {
        assert_eq!(stroke.point_offset, next, "spans are packed end to end");
        next += stroke.count;
    }
    assert_eq!(next as usize, points.len(), "the spans cover every point");
}

/// A fill's four corners share the arena with the strokes, so the shader reads
/// both through one binding.
#[test]
fn a_filled_box_puts_its_corners_in_the_shared_points() {
    let list = [sketch(
        1,
        SketchShape::Rect {
            bounds: boxed(0, 0, 32, 32),
            radius: 0,
            fill: Some(SketchFill {
                color: [0, 0, 255],
                alpha: 200,
                style: SketchFillStyle::Solid,
            }),
        },
    )];
    let (points, geometry) = marks(&list);

    let (offset, _) = geometry[0].fill.expect("a filled box carries a quad");
    assert_eq!(
        points.len() - offset as usize,
        4,
        "the quad is four points at the end of the arena",
    );
}

/// A reveal walks arc length, so a fraction lands on the segment holding that
/// distance rather than on that fraction of the point count. A stroke whose
/// points crowd where it wobbles most then draws at an even speed.
#[test]
fn the_reveal_lands_on_the_segment_holding_its_distance() {
    let stroke = StrokeSpan {
        point_offset: 0,
        count: 5,
        // A long first segment, then three short ones, which is what separates
        // an arc-length search from an index one.
        prefix: vec![0.0, 70.0, 80.0, 90.0, 100.0],
        total: 100.0,
    };

    assert_eq!(reveal_at(&stroke, 0.0), (0, 0.0), "nothing at the start");
    assert_eq!(reveal_at(&stroke, 1.0), (5, 0.0), "everything at the end");

    let (count, t) = reveal_at(&stroke, 0.5);
    assert_eq!(
        count, 1,
        "half the length is still inside the first segment"
    );
    assert!(
        (t - 50.0 / 70.0).abs() < 1e-5,
        "the pen sits partway along it, got {t}",
    );

    let (count, t) = reveal_at(&stroke, 0.75);
    assert_eq!(count, 2, "three quarters has passed the first segment");
    assert!((t - 0.5).abs() < 1e-5, "and is halfway through the next");
}

/// How far the pen has come through each stroke at `progress`, in declaration
/// order.
///
/// An entry is `None` before the pen reaches that stroke, `Some(false)` while
/// it draws, and `Some(true)` once the stroke is whole.
fn stroke_progress(sketches: &[Sketch], progress: f32) -> Vec<Option<bool>> {
    let (_, geometry) = marks(sketches);
    let built = instances(sketches, &[progress]);
    geometry[0]
        .strokes
        .iter()
        .map(|stroke| {
            built
                .iter()
                .find(|instance| {
                    instance.kind == KIND_STROKE && instance.point_offset == stroke.point_offset
                })
                .map(|instance| instance.reveal_count == stroke.count)
        })
        .collect()
}

/// The pen walks the mark's units in order, so a box draws around its perimeter
/// rather than growing all four sides at once.
#[test]
fn a_boxs_sides_draw_one_after_another() {
    let list = [sketch(
        1,
        SketchShape::Rect {
            // Two cells wide and two tall, so the four sides are the same
            // length and a quarter of the reveal covers each.
            bounds: boxed(0, 0, 64, 32),
            radius: 0,
            fill: None,
        },
    )];
    let (_, geometry) = marks(&list);
    assert_eq!(
        geometry[0].strokes.len(),
        8,
        "four sides, each a base stroke and its overlay",
    );

    assert_eq!(
        stroke_progress(&list, 0.3),
        [
            Some(true),
            Some(true),
            Some(false),
            Some(false),
            None,
            None,
            None,
            None,
        ],
        "the first side is whole, the second is under way, and the rest wait",
    );
}

/// A base stroke and its overlay are one unit, so the doubling never draws its
/// second pass after the first. A single-pair mark keeps the look it had.
#[test]
fn an_ellipses_two_strokes_advance_together() {
    let list = [sketch(1, SketchShape::Ellipse(boxed(0, 0, 64, 32)))];

    assert_eq!(
        stroke_progress(&list, 0.5),
        [Some(false), Some(false)],
        "both passes are partway through at the halfway point",
    );
}

/// A stroke the reveal has not reached contributes no instance, rather than an
/// empty one the GPU still rasterizes.
#[test]
fn an_unreached_stroke_builds_no_instance() {
    let list = [sketch(1, SketchShape::Ellipse(boxed(0, 0, 64, 32)))];

    assert_eq!(instances(&list, &[0.0]), Vec::new(), "nothing at zero");
    assert!(!instances(&list, &[1.0]).is_empty(), "every stroke at one",);
}

/// A caller with no clock passes an empty slice, and every mark draws whole.
#[test]
fn a_missing_progress_entry_draws_the_mark_complete() {
    let list = [sketch(1, SketchShape::Ellipse(boxed(0, 0, 64, 32)))];
    assert_eq!(instances(&list, &[]), instances(&list, &[1.0]));
}

/// The fill eases in over the back half of the reveal, so the box fills behind
/// the stroke rather than ahead of it.
#[test]
fn a_fill_stays_clear_through_the_first_half_of_the_reveal() {
    let list = [sketch(
        1,
        SketchShape::Rect {
            bounds: boxed(0, 0, 32, 32),
            radius: 0,
            fill: Some(SketchFill {
                color: [0, 0, 255],
                alpha: 255,
                style: SketchFillStyle::Solid,
            }),
        },
    )];
    let alpha_at = |progress: f32| {
        instances(&list, &[progress])
            .iter()
            .find(|instance| instance.kind == KIND_FILL)
            .map(|instance| instance.color[3])
            .expect("a filled box always builds its fill instance")
    };

    assert_eq!(alpha_at(0.5), 0.0, "clear at the halfway point");
    assert!(alpha_at(0.75) > 0.0, "rising past it");
    assert!(alpha_at(0.75) < 1.0, "and not yet full");
    assert_eq!(alpha_at(1.0), 1.0, "opaque at the end");
}

/// A connector names the mark it points at, and the end lands outside that
/// mark's outline so the line stops short of the stroke rather than crossing
/// it.
#[test]
fn a_connector_ends_outside_the_component_it_names() {
    let list = [
        sketch(1, SketchShape::Ellipse(boxed(0, 0, 32, 32))),
        sketch(
            2,
            SketchShape::Line {
                from: SketchEnd::Point { x: 160, y: 8 },
                to: SketchEnd::Component {
                    id: 1,
                    side: SketchSide::Right,
                },
                bend: 0,
                heads: 0,
            },
        ),
    ];
    let (_, geometry) = marks(&list);
    assert!(
        !geometry[1].strokes.is_empty(),
        "a resolvable connector draws",
    );

    let target = shape_bounds(&list[0].command.shape, metrics()).expect("an ellipse has bounds");
    assert!(
        geometry[1].bounds[0] >= target[2],
        "the connector starts clear of the mark's right edge at {}",
        target[2],
    );
}

/// A connector to an id nothing answers for draws nothing, because a line left
/// at the origin points at the wrong thing.
#[test]
fn a_connector_to_a_missing_component_draws_nothing() {
    let list = [sketch(
        1,
        SketchShape::Line {
            from: SketchEnd::Point { x: 0, y: 0 },
            to: SketchEnd::Component {
                id: 99,
                side: SketchSide::Auto,
            },
            bend: 0,
            heads: 0,
        },
    )];
    let (points, geometry) = marks(&list);

    assert_eq!(points, Vec::<[f32; 2]>::new());
    assert!(geometry[0].strokes.is_empty());
}

/// A mark anchored to a compositing pool is held back from the base draw and
/// carries its host's shift, or the composite paints over it.
#[test]
fn a_riding_mark_is_shifted_and_held_back() {
    let mut list = [sketch(1, SketchShape::Ellipse(boxed(0, 0, 64, 32)))];
    list[0].command.anchor = Some((3, 0.0));

    let (_, geometry) = marks(&list);
    let (mut built, mut riding) = (Vec::new(), Vec::new());
    let anchored = [AnchoredPanel {
        host: 3,
        dy_px: -12.0,
        scissor: [0, 0, 40, 40],
    }];
    build_instances(
        &list,
        &geometry,
        &[1.0],
        &anchored,
        metrics(),
        &mut built,
        &mut riding,
    );

    assert_eq!(riding.len(), built.len(), "every instance of it rides");
    assert!(
        built.iter().all(|instance| instance.dy == -12.0),
        "each carries the host's shift",
    );
    assert!(
        riding.iter().all(|&(_, scissor)| scissor == [0, 0, 40, 40]),
        "and its host's scissor",
    );
}

/// A mark anchored to a pool that is not compositing this frame draws with the
/// rest, unshifted.
#[test]
fn a_mark_whose_host_is_still_does_not_ride() {
    let mut list = [sketch(1, SketchShape::Ellipse(boxed(0, 0, 64, 32)))];
    list[0].command.anchor = Some((3, 0.0));

    let (_, geometry) = marks(&list);
    let (mut built, mut riding) = (Vec::new(), Vec::new());
    build_instances(
        &list,
        &geometry,
        &[1.0],
        &[],
        metrics(),
        &mut built,
        &mut riding,
    );

    assert_eq!(riding, Vec::new(), "nothing is held back");
    assert!(built.iter().all(|instance| instance.dy == 0.0));
}

/// Render `sketches` at `progress` into a square target and read back the red
/// channel of every texel.
///
/// `anchored` names the compositing pools, so a mark riding one is held back
/// from the base draw and recorded by the riding pass instead. A caller with no
/// ride passes `&[]`.
fn render_red(
    device: &Device,
    queue: &Queue,
    sketches: &[Sketch],
    progress: &[f32],
    anchored: &[AnchoredPanel],
) -> Option<Vec<u8>> {
    let mut grid = Grid::new(16, 12);
    grid.set_sketches(sketches.to_vec());

    let mut pass = SketchPass::new(device, TextureFormat::Rgba8Unorm, metrics());
    pass.prepare(
        device,
        queue,
        &grid,
        progress,
        anchored,
        &[],
        [TARGET as f32, TARGET as f32],
    );

    let size = Extent3d {
        width: TARGET,
        height: TARGET,
        depth_or_array_layers: 1,
    };
    let target = device.create_texture(&TextureDescriptor {
        label: Some("sketch target"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8Unorm,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&TextureViewDescriptor::default());
    let readback = device.create_buffer(&BufferDescriptor {
        label: Some("sketch readback"),
        size: u64::from(TARGET) * u64::from(TARGET) * 4,
        usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
    {
        let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("sketch"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Clear(Color::BLACK),
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.draw(&mut render_pass);
        pass.draw_riding(&mut render_pass);
    }
    encoder.copy_texture_to_buffer(
        TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect: TextureAspect::All,
        },
        TexelCopyBufferInfo {
            buffer: &readback,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(TARGET * 4),
                rows_per_image: None,
            },
        },
        size,
    );
    queue.submit(Some(encoder.finish()));

    readback.slice(..).map_async(MapMode::Read, |_| {});
    device.poll(PollType::wait_indefinitely()).ok()?;
    let rgba = readback.slice(..).get_mapped_range().to_vec();

    Some(rgba.chunks_exact(4).map(|texel| texel[0]).collect())
}

/// A ridden mark paints where its host carried it, not where it was generated.
///
/// The vertex stage moves the quad and the fragment stage measures the distance
/// fields, so a shift in one and not the other leaves the ink behind while the
/// box slides off it. Only a rendered image separates the two.
#[test]
fn a_ridden_mark_paints_at_its_hosts_shift() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("sketch ride test: no wgpu adapter, skipping");
        return;
    };
    const SHIFT: usize = 8;

    let filled = SketchShape::Rect {
        bounds: boxed(16, 16, 32, 16),
        radius: 0,
        fill: Some(SketchFill {
            color: [255, 0, 0],
            alpha: 255,
            style: SketchFillStyle::Solid,
        }),
    };
    let plain = [sketch(1, filled)];
    let mut ridden = plain.clone();
    ridden[0].command.anchor = Some((3, 0.0));

    let rest = render_red(&device, &queue, &plain, &[1.0], &[]).expect("readback");
    let carried = render_red(
        &device,
        &queue,
        &ridden,
        &[1.0],
        &[AnchoredPanel {
            host: 3,
            dy_px: SHIFT as f32,
            scissor: [0, 0, 64, 64],
        }],
    )
    .expect("readback");

    assert!(rest.iter().any(|&byte| byte > 0), "the mark paints at rest");

    let row = TARGET as usize;
    let mut expected = vec![0u8; rest.len()];
    expected[SHIFT * row..].copy_from_slice(&rest[..rest.len() - SHIFT * row]);

    let differs = carried
        .iter()
        .zip(&expected)
        .position(|(carried, expected)| carried != expected)
        .map(|at| (at as u32 % TARGET, at as u32 / TARGET));
    assert_eq!(
        differs, None,
        "the ridden mark is the resting one moved down, first differing (x, y)",
    );
}

/// A mark at full progress paints, and the same mark at zero paints nothing.
/// Without that the reveal is decorative rather than real.
#[test]
fn a_reveal_of_zero_paints_nothing_and_one_paints_the_mark() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("sketch reveal test: no wgpu adapter, skipping");
        return;
    };
    let list = [sketch(1, SketchShape::Ellipse(boxed(16, 16, 96, 64)))];

    let whole = render_red(&device, &queue, &list, &[1.0], &[]).expect("readback");
    let none = render_red(&device, &queue, &list, &[0.0], &[]).expect("readback");

    assert!(whole.iter().any(|&byte| byte > 0), "a full reveal paints");
    assert!(
        none.iter().all(|&byte| byte == 0),
        "an empty one paints nothing",
    );
}

/// A half reveal paints strictly less than a whole one and nothing the whole
/// one does not, because a partial stroke is a prefix of the complete one.
#[test]
fn a_half_reveal_paints_a_prefix_of_the_whole() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("sketch prefix test: no wgpu adapter, skipping");
        return;
    };
    let list = [sketch(1, SketchShape::Ellipse(boxed(16, 16, 96, 64)))];

    let whole = render_red(&device, &queue, &list, &[1.0], &[]).expect("readback");
    let half = render_red(&device, &queue, &list, &[0.5], &[]).expect("readback");

    let lit = |ink: &[u8]| ink.iter().filter(|&&byte| byte > 0).count();
    assert!(lit(&half) > 0, "half a reveal paints something");
    assert!(
        lit(&half) < lit(&whole),
        "and less than the whole, {} against {}",
        lit(&half),
        lit(&whole),
    );

    let outside = half
        .iter()
        .zip(&whole)
        .position(|(half, whole)| *half > 0 && *whole == 0)
        .map(|at| (at as u32 % TARGET, at as u32 / TARGET));
    assert_eq!(
        outside, None,
        "the partial stroke paints no texel the whole one leaves clear, at (x, y)",
    );
}

/// The pen tip is the same shape the finished stroke already covers, so a
/// growing mark only ever adds ink.
///
/// A tip drawn as its own instance overlaps the run it follows and composites
/// that overlap twice, which reads brighter mid-draw than the same place reads
/// when the stroke is done. Resolving the tip inside the stroke's
/// own distance field is what prevents it, and per-texel monotonic coverage is
/// what shows it holds.
#[test]
fn a_growing_stroke_never_outpaints_the_finished_one() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("sketch joint test: no wgpu adapter, skipping");
        return;
    };
    let list = [sketch(1, SketchShape::Ellipse(boxed(16, 16, 96, 64)))];
    let whole = render_red(&device, &queue, &list, &[1.0], &[]).expect("readback");

    assert!(
        whole.iter().any(|&byte| byte > 0 && byte < 255),
        "the mark paints a partly covered fringe, where a doubled blend shows",
    );

    for step in 1..8 {
        let progress = step as f32 / 8.0;
        let partial = render_red(&device, &queue, &list, &[progress], &[]).expect("readback");

        let brighter = partial
            .iter()
            .zip(&whole)
            .position(|(partial, whole)| partial > whole)
            .map(|at| {
                let index = at as u32;
                (index % TARGET, index / TARGET, partial[at], whole[at])
            });
        assert_eq!(
            brighter, None,
            "at progress {progress} a texel outpaints the finished stroke, \
             at (x, y, partial, whole)",
        );
    }
}

/// The pen tip sits partway along the segment after the revealed run, so a
/// stroke grows smoothly instead of jumping point to point.
///
/// Two progress values inside the same segment reveal the same whole points and
/// differ only in the fraction. A shader that ignores the fraction paints them
/// identically, which is the jump this rules out.
#[test]
fn the_pen_tip_advances_inside_one_segment() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("sketch pen-tip test: no wgpu adapter, skipping");
        return;
    };
    // A long straight connector flattens to eight even segments, so a pair of
    // fractions an eighth apart share one. A generated curve's segments are far
    // too short to tell the fraction from the count.
    let list = [sketch(
        1,
        SketchShape::Line {
            from: SketchEnd::Point { x: 16, y: 64 },
            to: SketchEnd::Point { x: 240, y: 64 },
            bend: 0,
            heads: 0,
        },
    )];

    let counts = |progress: f32| {
        instances(&list, &[progress])
            .iter()
            .map(|instance| (instance.reveal_count, instance.reveal_t))
            .collect::<Vec<_>>()
    };
    // The flattened segments are not evenly long, so the widest span sharing
    // one is found rather than assumed. A narrow span moves the tip by less
    // than a texel and proves nothing.
    const STEPS: u32 = 400;
    let whole_points = |progress: f32| {
        counts(progress)
            .into_iter()
            .map(|(count, _)| count)
            .collect::<Vec<_>>()
    };
    let (mut best, mut run_start) = ((0, 0), 0);
    for step in 1..=STEPS {
        if whole_points(step as f32 / STEPS as f32) != whole_points(run_start as f32 / STEPS as f32)
        {
            run_start = step;
        } else if step - run_start > best.1 - best.0 {
            best = (run_start, step);
        }
    }
    let (early, late) = (best.0 as f32 / STEPS as f32, best.1 as f32 / STEPS as f32);
    assert!(late > early, "some segment spans more than one step");

    let (before, after) = (counts(early), counts(late));
    assert!(
        !before.is_empty()
            && before
                .iter()
                .zip(&after)
                .all(|(before, after)| before.0 == after.0 && before.1 < after.1),
        "the span holds the count and moves only the fraction, {before:?} to {after:?}",
    );

    let lit = |progress: f32| {
        render_red(&device, &queue, &list, &[progress], &[])
            .expect("readback")
            .iter()
            .filter(|&&byte| byte > 0)
            .count()
    };
    assert!(
        lit(late) > lit(early),
        "the tip carries the stroke further, {} against {}",
        lit(late),
        lit(early),
    );
}

/// An `auto` side picks the edge facing the connector's other end, so a line
/// leaves the box on the side it is heading toward rather than always the same
/// one.
#[test]
fn an_auto_side_faces_the_connector_s_other_end() {
    let target = sketch(1, SketchShape::Ellipse(boxed(64, 32, 32, 32)));
    let box_px = shape_bounds(&target.command.shape, metrics()).expect("an ellipse has bounds");

    // The connector preserves its vertices at this roughness, so a stroke's last
    // point is exactly where the line met the box.
    let meeting = |x: i16, y: i16| {
        let list = [
            target.clone(),
            sketch(
                2,
                SketchShape::Line {
                    from: SketchEnd::Point { x, y },
                    to: SketchEnd::Component {
                        id: 1,
                        side: SketchSide::Auto,
                    },
                    bend: 0,
                    heads: 0,
                },
            ),
        ];
        let (points, geometry) = marks(&list);
        let stroke = &geometry[1].strokes[0];
        points[(stroke.point_offset + stroke.count - 1) as usize]
    };
    let [min_x, min_y, max_x, max_y] = box_px;
    let center = [(min_x + max_x) / 2.0, (min_y + max_y) / 2.0];

    let left = meeting(0, 48);
    assert!(
        left[0] < min_x && (left[1] - center[1]).abs() < 0.01,
        "a line from the left meets the left edge's midpoint, got {left:?} \
         for a box at {box_px:?}",
    );

    let right = meeting(400, 48);
    assert!(
        right[0] > max_x && (right[1] - center[1]).abs() < 0.01,
        "a line from the right meets the right edge's midpoint, got {right:?}",
    );

    let above = meeting(80, 0);
    assert!(
        above[1] < min_y && (above[0] - center[0]).abs() < 0.01,
        "a line from above meets the top edge's midpoint, got {above:?}",
    );

    let below = meeting(80, 200);
    assert!(
        below[1] > max_y && (below[0] - center[0]).abs() < 0.01,
        "a line from below meets the bottom edge's midpoint, got {below:?}",
    );

    // A cell is twice as tall as it is wide, so this box is too. The side is
    // chosen as a fraction of each half-extent rather than in raw pixels, and
    // this direction is the one those two answers disagree on: it is nearer the
    // vertical axis in pixels but well past the narrow side in proportion.
    let (half_w, half_h) = ((max_x - min_x) / 2.0, (max_y - min_y) / 2.0);
    let diagonal = [center[0] + half_w * 1.25, center[1] + half_h * 0.75];
    assert!(
        diagonal[0] - center[0] < diagonal[1] - center[1],
        "the fixture leans vertical in raw pixels, {diagonal:?} from {center:?}",
    );

    let corner = meeting(
        (diagonal[0] / metrics().width * 16.0) as i16,
        (diagonal[1] / metrics().height * 16.0) as i16,
    );
    assert!(
        corner[0] > max_x && (corner[1] - center[1]).abs() < 0.01,
        "proportion wins, so it meets the right edge, got {corner:?}",
    );
}
