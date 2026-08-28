//! Draw the current walkthrough stop as hand-drawn marks over the code.
//!
//! One stop becomes a mark around its focus, a connector to the narration card,
//! and a mark, connector, and label box per annotation. The geometry comes from
//! [`crate::walkthrough::slide`], which is pure. This pass measures the screen
//! for it and emits what it returns.
//!
//! Nothing is emitted under a terminal that draws no marks. The pinned card and
//! the status line already carry the stop there, and a cell fallback covers the
//! code the tour is about.
//!
//! Ids are derived from the stop and the part rather than allocated. The
//! terminal latches a mark's timing when its id first appears, so a scene
//! re-emitted every frame has to come out with the same ids or every frame
//! restarts every stroke.

use crate::{
    app::Stoat,
    pane::View,
    render::TEXT_SCALE_POPUP,
    theme::scope,
    walkthrough::{
        run::{part, WalkthroughRun},
        slide::{
            self, AnnotationCells, CellRange, Emphasis, Mark, SixteenthRect, Slide, SlideInput,
        },
    },
};
use ratatui::{buffer::Buffer, layout::Rect};
use std::{collections::HashMap, time::Duration};
use stoat_widgets::ApcScene;
use stoatty_protocol::command::{
    self, SketchBounds, SketchCommand, SketchEasing, SketchEnd, SketchFill, SketchFillStyle,
    SketchPhase, SketchShape, SketchSide, SketchStyle, SketchTiming, TextRunCommand,
};

/// The protocol version that decodes a sketch. An older stoatty ignores the
/// frames, so nothing is emitted rather than sending what it drops.
const SKETCH_PROTOCOL: u32 = 3;

/// Columns a label is wrapped at, chosen so a box stays narrower than the code
/// it sits beside.
const LABEL_WRAP: usize = 38;

/// Stroke widths in 256ths of a cell, by emphasis.
///
/// The current annotation draws heavier as well as brighter. Brightness alone
/// reads as a color change on a busy screen, where weight reads as attention.
const WIDTH_PLAIN: u16 = 64;
const WIDTH_CURRENT: u16 = 88;

/// Stroke opacity by emphasis. A dimmed mark stays legible, since the reader
/// still has to see what else the stop calls out.
const ALPHA_PLAIN: u8 = 255;
const ALPHA_DIMMED: u8 = 110;

/// A connector is thinner than the marks it joins, so it reads as a pointer
/// rather than as another annotation.
const LINK_WIDTH: u16 = 48;

/// Corner rounding of a label box, in sixteenths of a cell.
const LABEL_RADIUS: u8 = 4;

/// How long a retiring slide takes to un-draw itself.
///
/// Short enough that a reader stepping quickly is not waiting on it, long
/// enough that the marks read as being taken back rather than cut.
pub(crate) const EXIT_MS: u16 = 140;

/// Emit the current stop's marks, connectors, and label boxes.
///
/// A no-op with no walkthrough playing, under a terminal that draws no marks,
/// or when the focused pane is not an editor.
pub(crate) fn render_slide(stoat: &mut Stoat, buf: &mut Buffer, scene: &mut ApcScene) {
    if !scene.live() || !stoat.stoatty || stoat.stoatty_protocol < SKETCH_PROTOCOL {
        return;
    }
    let Some(input) = measure(stoat) else {
        return;
    };

    // A retiring slide holds the screen while it goes, and the arriving one
    // waits that out rather than drawing over it.
    let retiring = retire(stoat, scene);
    let input = SlideInput {
        start_offset_ms: input.start_offset_ms + if retiring { EXIT_MS } else { 0 },
        ..input
    };

    let slide = slide::layout(&input);
    let Some(run) = stoat.active_workspace().walkthrough.as_ref() else {
        return;
    };
    let painter = Painter {
        ids: SlideIds::of(run),
        colors: Colors::of(stoat),
        labels: input
            .annotations
            .iter()
            .map(|annotation| (annotation.key, annotation.label_lines.clone()))
            .collect(),
        anchor: pool_anchor(stoat),
        theme: &stoat.theme,
        declared: SlideParts::default(),
    };

    // The painter borrows the theme, so it is scoped to the emit and hands back
    // only what the run has to keep.
    let declared = {
        let mut painter = painter;
        painter.focus(&slide, buf, scene);
        painter.callouts(&slide, buf, scene);
        painter.declared
    };

    if let Some(run) = stoat.active_workspace_mut().walkthrough.as_mut() {
        run.last_parts = declared;
    }
}

/// Re-declare the retiring slide with the exit phase, and report whether one is
/// still going.
///
/// The terminal restarts a mark's clock when its phase changes and only then,
/// so the same ids draw themselves off without new ones and without the two
/// runs interfering.
fn retire(stoat: &mut Stoat, scene: &mut ApcScene) -> bool {
    let now = stoat.executor.now();
    let Some((parts, since)) = stoat.active_workspace().walkthrough_exit.clone() else {
        return false;
    };

    if now.saturating_duration_since(since) >= Duration::from_millis(u64::from(EXIT_MS)) {
        stoat.active_workspace_mut().walkthrough_exit = None;
        return false;
    }

    let timing = SketchTiming {
        delay_ms: 0,
        duration_ms: EXIT_MS,
        easing: SketchEasing::Smoothstep,
        phase: SketchPhase::Exit,
    };
    for mark in &parts.marks {
        command::encode_sketch_into(
            scene.buffer(),
            &SketchCommand {
                timing,
                ..mark.clone()
            },
        );
    }
    // The label text follows its box, so it fades with the box rather than
    // vanishing the moment the box starts to go. It has to be re-declared for
    // that, or it is simply absent from the frame.
    for run in &parts.runs {
        command::encode_text_run_into(scene.buffer(), run);
    }

    true
}

/// The ids one stop's parts draw under.
struct SlideIds {
    focus: u32,
    card: u32,
    focus_link: u32,
    /// The mark, connector, and label of each annotation, in order.
    annotations: Vec<(u32, u32, u32)>,
}

impl SlideIds {
    fn of(run: &WalkthroughRun) -> SlideIds {
        let drawable = run
            .current_stop()
            .annotations
            .len()
            .min(run.drawable_annotations());
        SlideIds {
            focus: run.part_id(part::FOCUS_MARK),
            card: run.part_id(part::CARD),
            focus_link: run.part_id(part::FOCUS_LINK),
            annotations: (0..drawable).map(|at| run.annotation_ids(at)).collect(),
        }
    }
}

/// The colors a slide draws in, resolved once rather than per mark.
struct Colors {
    focus: [u8; 3],
    markers: [[u8; 3]; 6],
    /// The label box's fill, shared with the narration card so the two read as
    /// one set of chrome.
    fill: [u8; 3],
}

impl Colors {
    fn of(stoat: &Stoat) -> Colors {
        let rgb = |name: &str, fallback: [u8; 3]| {
            crate::render::paint::style_rgb(stoat.theme.get(name).fg).unwrap_or(fallback)
        };
        let card = stoat.theme.get(scope::UI_WALKTHROUGH_CARD);

        Colors {
            focus: rgb(scope::UI_WALKTHROUGH_FOCUS, [229, 96, 96]),
            markers: std::array::from_fn(|at| {
                rgb(scope::UI_WALKTHROUGH_MARKERS[at], [97, 175, 239])
            }),
            fill: crate::render::paint::style_rgb(card.bg).unwrap_or([40, 44, 52]),
        }
    }

    /// Annotation `key`'s color, cycling through the six markers.
    fn marker(&self, key: usize) -> [u8; 3] {
        self.markers[key % self.markers.len()]
    }
}

/// Everything constant across one slide's parts.
///
/// Bundled rather than threaded through every emit, because the ids, the
/// colors, the labels, the pool anchor, and the pane never change within a
/// frame and passing all five to every function makes each one unreadable.
struct Painter<'a> {
    ids: SlideIds,
    colors: Colors,
    /// Each annotation's wrapped label lines, by key.
    labels: HashMap<usize, Vec<String>>,
    /// The pool the marks ride, so they glide with the pane rather than
    /// staying pinned to the screen.
    anchor: Option<(u32, f32)>,
    theme: &'a crate::theme::Theme,
    /// What this frame declared, kept so a step can send it back with the exit
    /// phase.
    ///
    /// A retiring mark cannot be recomputed. Its geometry was measured against
    /// the old scroll position, and after a jump to another file against
    /// another buffer, so the emitted form is the only honest record.
    declared: SlideParts,
}

/// One slide's emitted parts, as they went on the wire.
#[derive(Clone, Default, PartialEq, Debug)]
pub(crate) struct SlideParts {
    pub(crate) marks: Vec<SketchCommand>,
    /// The label text, which fades with the box it follows rather than
    /// vanishing the moment the box starts to go.
    pub(crate) runs: Vec<TextRunCommand>,
}

/// One part's stroke: which mark it is, in what color, at what weight, and
/// when it draws.
#[derive(Clone, Copy)]
struct Stroke {
    id: u32,
    color: [u8; 3],
    emphasis: Emphasis,
    timing: SketchTiming,
    /// What a box paints inside its outline, or `None` for an open mark.
    fill: Option<[u8; 3]>,
}

impl Painter<'_> {
    /// Record and encode one mark.
    fn declare(&mut self, command: SketchCommand, scene: &mut ApcScene) {
        command::encode_sketch_into(scene.buffer(), &command);
        self.declared.marks.push(command);
    }

    /// Record and encode one label run.
    fn declare_run(&mut self, command: TextRunCommand, scene: &mut ApcScene) {
        command::encode_text_run_into(scene.buffer(), &command);
        self.declared.runs.push(command);
    }

    /// Emit the focus mark and the connector to the card.
    fn focus(&mut self, slide: &Slide, buf: &mut Buffer, scene: &mut ApcScene) {
        let Some(mark) = slide.focus else {
            return;
        };
        let stroke = Stroke {
            id: self.ids.focus,
            color: self.colors.focus,
            emphasis: Emphasis::Plain,
            timing: timing_of(slide, Some(slide::Part::Focus)),
            fill: None,
        };
        self.mark(mark, stroke, buf, scene);

        if !slide.focus_link {
            return;
        }
        self.link(
            Stroke {
                id: self.ids.focus_link,
                timing: timing_of(slide, Some(slide::Part::FocusLink)),
                ..stroke
            },
            self.ids.focus,
            self.ids.card,
            buf,
            scene,
        );
    }

    /// Emit each annotation's mark, connector, and label box.
    fn callouts(&mut self, slide: &Slide, buf: &mut Buffer, scene: &mut ApcScene) {
        for callout in &slide.callouts {
            let Some(&(mark_id, link_id, label_id)) = self.ids.annotations.get(callout.key) else {
                continue;
            };
            let stroke = Stroke {
                id: mark_id,
                color: self.colors.marker(callout.key),
                emphasis: slide.emphasis(callout.key),
                timing: timing_of(slide, Some(slide::Part::Mark(callout.key))),
                fill: None,
            };
            self.mark(callout.mark, stroke, buf, scene);

            // The box before its connector, so the line has something to
            // arrive at by the time it is drawn.
            let lines = self.labels.get(&callout.key).cloned().unwrap_or_default();
            self.label(
                callout.label,
                Stroke {
                    id: label_id,
                    timing: timing_of(slide, Some(slide::Part::Label(callout.key))),
                    fill: Some(self.colors.fill),
                    ..stroke
                },
                &lines,
                buf,
                scene,
            );

            if callout.link {
                self.link(
                    Stroke {
                        id: link_id,
                        timing: timing_of(slide, Some(slide::Part::Link(callout.key))),
                        ..stroke
                    },
                    mark_id,
                    label_id,
                    buf,
                    scene,
                );
            }
        }
    }

    /// Emit one mark, as the ring or box its shape says.
    ///
    /// The layout already works in surface coordinates, so the shape goes on
    /// the wire as it comes out.
    fn mark(&mut self, mark: Mark, stroke: Stroke, _buf: &mut Buffer, scene: &mut ApcScene) {
        let shape = match mark {
            Mark::Ellipse(rect) => SketchShape::Ellipse(bounds_of(rect)),
            Mark::Rect(rect) => SketchShape::Rect {
                bounds: bounds_of(rect),
                radius: 0,
                fill: None,
            },
        };
        self.declare(
            SketchCommand {
                id: stroke.id,
                style: mark_style(stroke),
                timing: stroke.timing,
                shape,
                anchor: self.anchor,
            },
            scene,
        );
    }

    /// Emit a connector between two marks.
    ///
    /// Both ends name a mark, so the connector tracks them as they move and
    /// leaves each on the side facing the other.
    fn link(
        &mut self,
        stroke: Stroke,
        from: u32,
        to: u32,
        _buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
        let end = |id| SketchEnd::Component {
            id,
            side: SketchSide::Auto,
        };
        self.declare(
            SketchCommand {
                id: stroke.id,
                style: SketchStyle {
                    width: LINK_WIDTH,
                    ..mark_style(stroke)
                },
                timing: stroke.timing,
                shape: SketchShape::Line {
                    from: end(from),
                    to: end(to),
                    bend: 0,
                    heads: 0,
                },
                anchor: self.anchor,
            },
            scene,
        );
    }

    /// Emit one label box and the text inside it.
    ///
    /// The cells under the box are cleared first. A hand-drawn box is stroke
    /// and fill, both drawn by the terminal. Without the clear the code beneath
    /// shows through wherever the fill is not opaque.
    fn label(
        &mut self,
        box_: Rect,
        stroke: Stroke,
        lines: &[String],
        buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
        crate::render::clear_themed(box_, buf, self.theme);

        self.declare(
            SketchCommand {
                id: stroke.id,
                style: mark_style(stroke),
                timing: stroke.timing,
                shape: SketchShape::Rect {
                    bounds: SketchBounds {
                        x: box_.x as i16 * 16,
                        y: box_.y as i16 * 16,
                        w: box_.width * 16,
                        h: box_.height * 16,
                    },
                    radius: LABEL_RADIUS,
                    fill: stroke.fill.map(|color| SketchFill {
                        color,
                        alpha: 255,
                        style: SketchFillStyle::Solid,
                    }),
                },
                anchor: self.anchor,
            },
            scene,
        );

        self.label_text(box_, stroke, lines, scene);
    }

    /// A label's own text, drawn inside its box.
    ///
    /// The runs carry the box's id, so a label fades in as the box that holds
    /// it closes rather than sitting there while the pen draws.
    fn label_text(&mut self, box_: Rect, stroke: Stroke, lines: &[String], scene: &mut ApcScene) {
        for (offset, line) in lines.iter().enumerate() {
            let row = box_.y + 1 + offset as u16;
            if row + 1 >= box_.y + box_.height {
                break;
            }
            self.declare_run(
                TextRunCommand {
                    col: (box_.x as i16 + 1) * 16,
                    row: row as i16 * 16,
                    scale: TEXT_SCALE_POPUP,
                    color: stroke.color,
                    bg: None,
                    text: line.clone(),
                    follow: stroke.id,
                    anchor: self.anchor,
                },
                scene,
            );
        }
    }
}

/// The stroke a mark draws with, at the weight and opacity its emphasis says.
fn mark_style(stroke: Stroke) -> SketchStyle {
    let (width, alpha) = match stroke.emphasis {
        Emphasis::Current => (WIDTH_CURRENT, ALPHA_PLAIN),
        Emphasis::Dimmed => (WIDTH_PLAIN, ALPHA_DIMMED),
        Emphasis::Plain => (WIDTH_PLAIN, ALPHA_PLAIN),
    };
    SketchStyle {
        width,
        alpha,
        // Seed zero asks the terminal to derive one from the id, which keeps a
        // mark wobbling the same way across every redraw without this side
        // choosing a number.
        seed: 0,
        ..SketchStyle::marker(stroke.color)
    }
}

/// A layout rectangle as the widget's own bounds.
fn bounds_of(rect: SixteenthRect) -> SketchBounds {
    SketchBounds {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    }
}

/// The timing the layout scheduled for `part`, or an immediate stroke.
///
/// A part the table does not name draws at once rather than not at all, since a
/// missing schedule is a layout gap rather than a reason to leave a mark off.
fn timing_of(slide: &Slide, part: Option<slide::Part>) -> SketchTiming {
    let scheduled = part.and_then(|part| {
        slide
            .timing
            .iter()
            .find(|(at, ..)| *at == part)
            .map(|(_, start, duration)| SketchTiming::after(*start, *duration))
    });
    scheduled.unwrap_or_else(|| SketchTiming::after(0, 260))
}

/// The pool the marks ride, so they glide with the pane rather than staying
/// pinned to the screen.
///
/// `None` when the focused pane is not an editor, which leaves the marks
/// screen-fixed. Nothing is drawn in that case anyway, since the measurement
/// below returns nothing.
fn pool_anchor(stoat: &Stoat) -> Option<(u32, f32)> {
    let ws = stoat.active_workspace();
    let pane = ws.panes.pane(ws.panes.focus());
    let View::Editor(editor_id) = pane.view else {
        return None;
    };
    let editor = ws.editors.get(editor_id)?;
    Some((pane.index, editor.scroll_row as f32))
}

/// Measure the screen into the layout's input.
///
/// `None` when no tour plays or the focused pane is not an editor.
fn measure(stoat: &mut Stoat) -> Option<SlideInput> {
    let pane_area = {
        let ws = stoat.active_workspace();
        let pane = ws.panes.pane(ws.panes.focus());
        match pane.view {
            View::Editor(_) => pane.area,
            _ => return None,
        }
    };

    let (focus_range, annotations, card, current, card_hidden) = {
        let run = stoat.active_workspace().walkthrough.as_ref()?;
        let stop = run.current_stop();
        let drawable = run.drawable_annotations();
        (
            stop.focus.range,
            stop.annotations
                .iter()
                .take(drawable)
                .enumerate()
                .filter(|(_, annotation)| annotation.path.is_none())
                .map(|(key, annotation)| (key, annotation.range, annotation.label.clone()))
                .collect::<Vec<_>>(),
            stoat
                .pending_hover
                .as_ref()
                .filter(|popup| popup.pinned)
                .and_then(|popup| popup.placement)
                .map(|rect| (rect.width, rect.height)),
            run.annotation_progress()
                .map(|(at, _)| at.saturating_sub(1)),
            stoat
                .pending_hover
                .as_ref()
                .is_none_or(|popup| !popup.pinned),
        )
    };

    // The minimap strip is not code, so a mark placed over it points at
    // nothing. The card and the labels are clamped into what is left.
    let (content, _) = crate::render::layout::split_pane_status(pane_area);
    let ws = stoat.active_workspace();
    let pane = ws.panes.pane(ws.panes.focus());
    let View::Editor(editor_id) = pane.view else {
        return None;
    };
    let strip_cols = ws
        .editors
        .get(editor_id)
        .and_then(|editor| editor.minimap_rect)
        .map_or(0, |rect| rect.width);
    let pane_rect = Rect {
        width: content.width.saturating_sub(strip_cols),
        ..content
    };

    let focus = measure_range(stoat, editor_id, pane_rect, &focus_range);
    let annotations = annotations
        .into_iter()
        .filter_map(|(key, range, label)| {
            let range = measure_range(stoat, editor_id, pane_rect, &range)?;
            Some(AnnotationCells {
                key,
                range,
                label_lines: crate::render::text::wrap_text(&label, LABEL_WRAP),
            })
        })
        .collect();

    Some(SlideInput {
        pane: pane_rect,
        focus,
        annotations,
        line_ends: line_ends(stoat, editor_id, pane_rect),
        card,
        current,
        card_hidden,
        start_offset_ms: 0,
    })
}

/// Turn a stored range into the cells it covers on screen.
///
/// `None` when neither end is visible, which the layout turns into no mark. A
/// range clamped into view puts a box around whatever scrolled into its place.
fn measure_range(
    stoat: &mut Stoat,
    editor_id: crate::editor_state::EditorId,
    pane: Rect,
    range: &crate::walkthrough::Range,
) -> Option<CellRange> {
    let ws = stoat.active_workspace_mut();
    let editor = ws.editors.get_mut(editor_id)?;
    let offsets = {
        let snapshot = editor.display_map.snapshot();
        let rope = snapshot.buffer_snapshot().rope();
        // Stored ranges are one-based, and a Point is zero-based.
        let point = |line: u32, col: u32| {
            rope.point_to_offset(stoat_text::Point::new(
                line.saturating_sub(1),
                col.saturating_sub(1),
            ))
        };
        (
            point(range.start.line, range.start.col),
            point(range.end.line, range.end.col),
        )
    };

    let start = crate::render::hover::cursor_screen_position(editor, pane, offsets.0)?;
    let end = crate::render::hover::cursor_screen_position(editor, pane, offsets.1)?;

    Some(CellRange {
        rows: (start.1..=end.1).collect(),
        start_x: start.0,
        end_x: end.0,
    })
}

/// Where the text ends on each visible row, so a box lands past it rather than
/// over it.
fn line_ends(
    stoat: &mut Stoat,
    editor_id: crate::editor_state::EditorId,
    pane: Rect,
) -> Vec<(u16, u16)> {
    let Some(editor) = stoat.active_workspace_mut().editors.get_mut(editor_id) else {
        return Vec::new();
    };
    let scroll = editor.scroll_row;
    let snapshot = editor.display_map.snapshot();

    (0..pane.height)
        .map(|offset| {
            let row = scroll + u32::from(offset);
            let width = snapshot.line_len(row).min(u32::from(u16::MAX)) as u16;
            (pane.y + offset, pane.x + width)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::EXIT_MS;
    use crate::{
        action_handlers::walkthrough::open,
        app::Stoat,
        test_harness::TestHarness,
        theme::scope,
        walkthrough::{
            run::{part, ID_SPACE, STOP_ID_STRIDE},
            Location, Point, Range, Walkthrough,
        },
    };
    use std::path::PathBuf;
    use stoatty_protocol::command::{self, Command, SketchCommand, SketchPhase, SketchShape};

    const CODE: &str = "fn one() {}\nfn two() {}\nfn three() {}\n";

    fn range_of(line: u32, cols: (u32, u32)) -> Range {
        Range {
            start: Point { line, col: cols.0 },
            end: Point { line, col: cols.1 },
        }
    }

    fn location(line: u32, cols: (u32, u32), snippet: &str) -> Location {
        Location {
            path: PathBuf::from("a.rs"),
            range: range_of(line, cols),
            snippet: snippet.to_owned(),
        }
    }

    /// A one-stop tour over a visible range, with `annotations` labeled ranges
    /// on the lines above it.
    fn harness(annotations: &[(u32, &str)]) -> TestHarness {
        let mut h = Stoat::test();
        // Protocol 3 is what decodes a sketch. The harness sets `stoatty` but
        // leaves the version at zero, which is the older-terminal case.
        h.stoat.stoatty_protocol = 3;
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");

        let mut walkthrough = Walkthrough::new("tour".to_owned(), "Tour".to_owned(), None);
        walkthrough
            .add_stop(
                Some("first".to_owned()),
                "The **entry** point.".to_owned(),
                location(2, (1, 11), "fn two() {}"),
                None,
            )
            .expect("append");
        walkthrough
            .add_stop(
                Some("second".to_owned()),
                "The **exit**.".to_owned(),
                location(3, (1, 13), "fn three() {}"),
                None,
            )
            .expect("append");
        for (line, label) in annotations {
            walkthrough
                .add_annotation(
                    "s1",
                    None,
                    range_of(*line, (1, 11)),
                    "fn one() {}".to_owned(),
                    (*label).to_owned(),
                    String::new(),
                )
                .expect("s1 exists");
        }

        h.fake_fs().insert_file("/repo/a.rs", CODE);
        h.fake_fs().insert_file(
            "/repo/.stoat/walkthroughs/tour.json",
            serde_json::to_string(&walkthrough).expect("serialize"),
        );
        h
    }

    /// Render one frame and return the sketch commands it emitted.
    ///
    /// The frame runs [`render_slide`] itself, so this reads what it wrote
    /// rather than calling it again, which would emit every part twice.
    fn frame(h: &mut TestHarness) -> Vec<Command> {
        h.stoat.render();
        let bytes = h.stoat.apc_scene.bytes().to_vec();

        command::decode_stream(&bytes)
    }

    fn sketches(h: &mut TestHarness) -> Vec<SketchCommand> {
        frame(h)
            .into_iter()
            .filter_map(|command| match command {
                Command::Sketch(sketch) => Some(sketch),
                _ => None,
            })
            .collect()
    }

    /// The id a run's part draws under, computed the way a reader's session
    /// does rather than hard-coded, so the test survives a change of base.
    fn part_id(h: &TestHarness, part: u32) -> u32 {
        let run = h
            .stoat
            .active_workspace()
            .walkthrough
            .as_ref()
            .expect("a tour is playing");
        run.part_id(part)
    }

    /// A stop's focus gets a ring around the line it names, under the first id
    /// of the run.
    #[test]
    fn a_focus_draws_a_ring_under_its_own_id() {
        let mut h = harness(&[]);
        open(&mut h.stoat, "tour");

        let emitted = sketches(&mut h);
        let focus = emitted
            .iter()
            .find(|sketch| sketch.id == part_id(&h, part::FOCUS_MARK))
            .expect("the focus draws");

        assert!(
            matches!(focus.shape, SketchShape::Ellipse(_)),
            "one row is circled, got {:?}",
            focus.shape,
        );
        assert_eq!(focus.style.seed, 0, "the terminal derives the seed");
    }

    /// A stop's ids sit in a block of its own, so stepping to the next one
    /// declares marks the terminal has not seen and each draws from nothing.
    /// Shared ids would have the new stop's marks read as the old one's
    /// mid-draw.
    #[test]
    fn stepping_to_the_next_stop_moves_every_id() {
        let mut h = harness(&[]);
        open(&mut h.stoat, "tour");
        let first = part_id(&h, part::FOCUS_MARK);

        crate::action_handlers::walkthrough::next(&mut h.stoat);
        let second = part_id(&h, part::FOCUS_MARK);

        assert_eq!(
            second - first,
            STOP_ID_STRIDE,
            "the next stop starts one block along",
        );
    }

    /// Every annotation draws a mark and a label box, each under its own id, so
    /// a stop with two points reads as two rather than one.
    #[test]
    fn each_annotation_draws_its_mark_and_its_label() {
        let mut h = harness(&[(1, "one"), (3, "two")]);
        open(&mut h.stoat, "tour");

        let emitted = sketches(&mut h);
        let run = h
            .stoat
            .active_workspace()
            .walkthrough
            .as_ref()
            .expect("playing");

        for at in 0..2 {
            let (mark, _, label) = run.annotation_ids(at);
            assert!(
                emitted.iter().any(|sketch| sketch.id == mark),
                "annotation {at} draws its mark",
            );
            assert!(
                emitted.iter().any(|sketch| sketch.id == label),
                "annotation {at} draws its label box",
            );
        }
    }

    /// An annotation in another file names a range of that file. Measured
    /// against the one on screen it would mark whatever lines happen to sit at
    /// those numbers here, so it draws nothing until the reader opens it.
    #[test]
    fn a_cross_file_annotation_draws_nothing_here() {
        let mut h = Stoat::test();
        h.stoat.stoatty_protocol = 3;
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");

        let mut walkthrough = Walkthrough::new("tour".to_owned(), "Tour".to_owned(), None);
        walkthrough
            .add_stop(
                Some("first".to_owned()),
                "The **entry** point.".to_owned(),
                location(2, (1, 11), "fn two() {}"),
                None,
            )
            .expect("append");
        walkthrough
            .add_annotation(
                "s1",
                Some(PathBuf::from("elsewhere.rs")),
                range_of(1, (1, 11)),
                "fn other() {}".to_owned(),
                "over there".to_owned(),
                String::new(),
            )
            .expect("s1 exists");

        h.fake_fs().insert_file("/repo/a.rs", CODE);
        h.fake_fs().insert_file("/repo/elsewhere.rs", CODE);
        h.fake_fs().insert_file(
            "/repo/.stoat/walkthroughs/tour.json",
            serde_json::to_string(&walkthrough).expect("serialize"),
        );

        open(&mut h.stoat, "tour");
        let mark = {
            let run = h
                .stoat
                .active_workspace()
                .walkthrough
                .as_ref()
                .expect("playing");
            run.annotation_ids(0).0
        };

        let emitted = sketches(&mut h);
        assert!(
            !emitted.iter().any(|sketch| sketch.id == mark),
            "no mark for it in this file, got {:?}",
            emitted.iter().map(|sketch| sketch.id).collect::<Vec<_>>(),
        );
        assert!(
            emitted
                .iter()
                .any(|sketch| sketch.id == part_id(&h, part::FOCUS_MARK)),
            "and the focus still draws",
        );
    }

    /// Annotations cycle through the marker scopes, so two adjacent ones draw
    /// in different colors. Sharing one would read as a single annotation in
    /// two places.
    #[test]
    fn adjacent_annotations_take_different_marker_colors() {
        let mut h = harness(&[(1, "one"), (3, "two")]);
        open(&mut h.stoat, "tour");

        let expected: Vec<[u8; 3]> = (0..2)
            .map(|at| {
                let style = h.stoat.theme.get(scope::UI_WALKTHROUGH_MARKERS[at]);
                crate::render::paint::style_rgb(style.fg).expect("the theme names a color")
            })
            .collect();
        assert_ne!(expected[0], expected[1], "the theme gives them apart");

        let ids = {
            let run = h
                .stoat
                .active_workspace()
                .walkthrough
                .as_ref()
                .expect("playing");
            [run.annotation_ids(0).0, run.annotation_ids(1).0]
        };
        let emitted = sketches(&mut h);
        let drawn: Vec<[u8; 3]> = ids
            .iter()
            .map(|id| {
                emitted
                    .iter()
                    .find(|sketch| sketch.id == *id)
                    .map(|sketch| sketch.style.color)
                    .expect("the mark draws")
            })
            .collect();

        assert_eq!(drawn, expected, "each takes its own scope's color");
    }

    /// A stop with six marks still has to read as being about one of them, so
    /// walking onto an annotation brightens it and dims the rest.
    #[test]
    fn walking_onto_an_annotation_dims_the_others() {
        let mut h = harness(&[(1, "one"), (3, "two")]);
        open(&mut h.stoat, "tour");

        let alphas = |h: &mut TestHarness| {
            let run_ids = {
                let run = h
                    .stoat
                    .active_workspace()
                    .walkthrough
                    .as_ref()
                    .expect("playing");
                [run.annotation_ids(0).0, run.annotation_ids(1).0]
            };
            let emitted = sketches(h);
            run_ids.map(|id| {
                emitted
                    .iter()
                    .find(|sketch| sketch.id == id)
                    .map(|sketch| sketch.style.alpha)
                    .expect("the mark draws")
            })
        };

        assert_eq!(alphas(&mut h), [255, 255], "nothing is singled out yet");

        crate::action_handlers::walkthrough::next_annotation(&mut h.stoat);
        assert_eq!(
            alphas(&mut h),
            [255, 110],
            "the one walked onto stays bright and the other recedes",
        );
    }

    /// A slide that simply stopped being re-declared would vanish between two
    /// frames, since the scene's leading reset clears whatever a frame leaves
    /// out. Stepping sends the old parts back with the exit phase instead.
    #[test]
    fn stepping_un_draws_the_stop_it_leaves() {
        let mut h = harness(&[]);
        open(&mut h.stoat, "tour");

        let leaving = part_id(&h, part::FOCUS_MARK);
        // The first frame is what records the parts a step then retires.
        sketches(&mut h);

        crate::action_handlers::walkthrough::next(&mut h.stoat);
        let arriving = part_id(&h, part::FOCUS_MARK);
        let emitted = sketches(&mut h);

        let retiring = emitted
            .iter()
            .find(|sketch| sketch.id == leaving)
            .expect("the stop being left is re-declared");
        assert_eq!(
            retiring.timing.phase,
            SketchPhase::Exit,
            "running its stroke back to nothing",
        );
        assert_eq!(retiring.timing.duration_ms, EXIT_MS);

        let entering = emitted
            .iter()
            .find(|sketch| sketch.id == arriving)
            .expect("and the stop being arrived at draws");
        assert_eq!(
            entering.timing.phase,
            SketchPhase::Enter,
            "the new stop draws itself on",
        );
        assert!(
            entering.timing.delay_ms >= EXIT_MS,
            "after the old one is gone, got {}",
            entering.timing.delay_ms,
        );
    }

    /// The exit is over after its own duration, and the parts go with it. Left
    /// declared they would sit there half-drawn.
    #[test]
    fn a_retired_slide_is_gone_once_its_exit_runs_out() {
        let mut h = harness(&[]);
        open(&mut h.stoat, "tour");
        let leaving = part_id(&h, part::FOCUS_MARK);
        sketches(&mut h);

        crate::action_handlers::walkthrough::next(&mut h.stoat);
        assert!(
            sketches(&mut h).iter().any(|sketch| sketch.id == leaving),
            "the frame right after the step still un-draws it",
        );

        h.advance_clock(std::time::Duration::from_millis(u64::from(EXIT_MS) + 10));
        assert!(
            !sketches(&mut h).iter().any(|sketch| sketch.id == leaving),
            "and a later frame has let it go",
        );
        assert!(
            h.stoat.active_workspace().walkthrough_exit.is_none(),
            "with nothing left holding it",
        );
    }

    /// An annotation step within one file leaves the same marks on the same
    /// code. Un-drawing them only to draw them again reads as a flicker.
    #[test]
    fn walking_within_a_file_retires_nothing() {
        let mut h = harness(&[(1, "one")]);
        open(&mut h.stoat, "tour");
        sketches(&mut h);

        crate::action_handlers::walkthrough::next_annotation(&mut h.stoat);
        assert!(
            h.stoat.active_workspace().walkthrough_exit.is_none(),
            "nothing retires",
        );
        assert!(
            sketches(&mut h)
                .iter()
                .all(|sketch| sketch.timing.phase == SketchPhase::Enter),
            "and every mark still draws itself on",
        );
    }

    /// A label is declared to fade in with the box it sits in. Dropped from the
    /// exit frame it disappears at once, leaving the box to run its stroke back
    /// around where the text was.
    #[test]
    fn a_retiring_label_goes_with_its_box() {
        let mut h = harness(&[(1, "one")]);
        open(&mut h.stoat, "tour");
        sketches(&mut h);

        crate::action_handlers::walkthrough::next(&mut h.stoat);
        let emitted = frame(&mut h);

        let exiting: Vec<u32> = emitted
            .iter()
            .filter_map(|command| match command {
                Command::Sketch(sketch) if sketch.timing.phase == SketchPhase::Exit => {
                    Some(sketch.id)
                },
                _ => None,
            })
            .collect();
        let label = emitted
            .iter()
            .find_map(|command| match command {
                Command::TextRun(run) if run.text == "one" => Some(run),
                _ => None,
            })
            .expect("the label of the stop being left is re-declared");

        assert!(
            exiting.contains(&label.follow),
            "fading out with the box it sits in, got follow {} of exiting {exiting:?}",
            label.follow,
        );
    }

    /// A second run must not reuse the first's ids, or the terminal reads its
    /// marks as the old ones still mid-draw.
    #[test]
    fn a_second_tour_draws_under_different_ids() {
        let mut h = harness(&[]);
        open(&mut h.stoat, "tour");
        let first = part_id(&h, part::FOCUS_MARK);

        open(&mut h.stoat, "tour");
        let second = part_id(&h, part::FOCUS_MARK);

        assert_ne!(first, second, "each run takes its own id space");
        assert!(first >= ID_SPACE && second >= ID_SPACE);
    }

    /// A stop's parts are contiguous and a later stop's start past them, so no
    /// two parts of one tour ever collide.
    #[test]
    fn every_part_of_a_stop_takes_a_distinct_id() {
        let mut h = harness(&[(1, "one"), (3, "two")]);
        open(&mut h.stoat, "tour");

        let run = h
            .stoat
            .active_workspace()
            .walkthrough
            .as_ref()
            .expect("playing");
        let mut ids = vec![
            run.part_id(part::FOCUS_MARK),
            run.part_id(part::CARD),
            run.part_id(part::FOCUS_LINK),
        ];
        for at in 0..2 {
            let (mark, link, label) = run.annotation_ids(at);
            ids.extend([mark, link, label]);
        }

        let declared = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), declared, "no two parts share an id");
        assert!(
            ids.iter().all(|id| *id < run.part_id(0) + STOP_ID_STRIDE),
            "and every one stays inside the stop's own block",
        );
    }

    /// The scene is re-emitted every frame, and the terminal latches a mark's
    /// timing on the id it first sees. Ids that moved between frames would
    /// restart every stroke on every frame.
    #[test]
    fn a_second_frame_emits_the_same_ids() {
        let mut h = harness(&[(1, "one")]);
        open(&mut h.stoat, "tour");

        let ids = |h: &mut TestHarness| -> Vec<u32> {
            let mut ids: Vec<u32> = sketches(h).iter().map(|sketch| sketch.id).collect();
            ids.sort_unstable();
            ids
        };
        let first = ids(&mut h);
        assert!(!first.is_empty(), "the first frame draws something");
        assert_eq!(ids(&mut h), first, "and the second draws the same parts");
    }

    /// A terminal that decodes no sketch gets none. The pinned card and the
    /// status line carry the stop there instead.
    #[test]
    fn an_older_terminal_gets_no_marks() {
        let mut h = harness(&[(1, "one")]);
        h.stoat.stoatty_protocol = 2;
        open(&mut h.stoat, "tour");

        assert_eq!(sketches(&mut h), Vec::new());
        assert!(
            h.stoat.pending_hover.is_some(),
            "and the narration is still up",
        );
    }

    /// A foreign terminal draws no chrome at all, so it gets no marks either.
    #[test]
    fn a_foreign_terminal_gets_no_marks() {
        let mut h = harness(&[(1, "one")]);
        h.stoat.stoatty = false;
        open(&mut h.stoat, "tour");

        assert_eq!(sketches(&mut h), Vec::new());
    }
}
