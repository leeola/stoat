//! Draw the current walkthrough stop as hand-drawn marks over the code.
//!
//! One stop becomes a mark around its focus, a connector to the narration card,
//! and a connector and label box per annotation the reader has reached. The
//! annotation the reader is on shows its code in its marker color while the
//! rest of the pane dims. The geometry comes from [`crate::walkthrough::slide`],
//! which is pure. This pass measures the screen for it and emits what it
//! returns.
//!
//! Under a terminal that draws no marks, only the highlight applies, since it is
//! plain cell color. The pinned card and the status line already carry the stop
//! there, and a cell fallback for the marks covers the code the tour is about.
//!
//! Ids are derived from the stop and the part rather than allocated. The
//! terminal latches a mark's timing when its id first appears, so a scene
//! re-emitted every frame has to come out with the same ids or every frame
//! restarts every stroke.

use crate::{
    app::Stoat,
    buffer_lifecycle,
    editor_state::EditorId,
    pane::View,
    render::{screen, TypeScale},
    theme::{scope, TypeRole},
    walkthrough::{
        run::{part, WalkthroughRun},
        slide::{
            self, AnnotationCells, CellRange, Emphasis, Link, Mark, Route, SixteenthRect, Slide,
            SlideInput, LINK_CORNER,
        },
        Range,
    },
};
use ratatui::{buffer::Buffer, layout::Rect, style::Color};
use std::{
    collections::HashMap,
    ops::RangeInclusive,
    time::{Duration, Instant},
};
use stoat_text::{Point, Rope};
use stoat_widgets::ApcScene;
use stoatty_protocol::command::{
    self, SketchBounds, SketchCommand, SketchEasing, SketchEnd, SketchFill, SketchFillStyle,
    SketchPhase, SketchPoint, SketchShape, SketchSide, SketchStyle, SketchTiming, TextRunCommand,
};

/// The protocol version that decodes a sketch. An older stoatty ignores the
/// frames, so nothing is emitted rather than sending what it drops.
const SKETCH_PROTOCOL: u32 = 3;

/// The protocol version that draws a sketch path. An older stoatty ignores the
/// frame whole, so a connector that bends goes to it as a straight line
/// between the same ends rather than vanishing.
const PATH_PROTOCOL: u32 = 6;

/// The protocol version that draws a sketch elbow. An older stoatty ignores the
/// frame whole, so an elbow goes to it as a straight line between the same
/// ends rather than vanishing.
const ELBOW_PROTOCOL: u32 = 7;

/// Columns a label is wrapped at, chosen so a box stays narrower than the code
/// it sits beside.
const LABEL_WRAP: usize = 38;

/// Stroke widths in 256ths of a cell, by emphasis.
///
/// A plain stroke is an eighth of a cell, about one pixel at a typical cell
/// width. That reads as a pen line rather than as a border over the glyphs
/// beside it. The rough pass draws each stroke twice, which thickens a wide one
/// further.
///
/// The current annotation draws heavier as well as brighter, at about a sixth
/// of a cell. Brightness alone reads as a color change on a busy screen, where
/// weight reads as attention.
const WIDTH_PLAIN: u16 = 32;
const WIDTH_CURRENT: u16 = 44;

/// Stroke opacity by emphasis. A dimmed mark stays legible, since the reader
/// still has to see what else the stop calls out.
const ALPHA_PLAIN: u8 = 255;
const ALPHA_DIMMED: u8 = 110;

/// How far the code outside the current annotation dims toward the background.
///
/// Twice the default dim of an unfocused pane. An unfocused pane has to stay
/// readable, where code the reader is not on has to recede.
pub const SPOTLIGHT_DIM: f32 = 0.5;

/// A connector is under a tenth of a cell, thinner than the marks it joins, so
/// it reads as a pointer rather than as another annotation.
const LINK_WIDTH: u16 = 24;

/// Where a sketched box stops rounding further, in sixteenths of a cell.
///
/// The reference caps at 32 pixels, which is about three and a half cells wide
/// at a typical terminal font size.
const CORNER_RADIUS_CAP: u16 = 56;

/// How long a retiring slide takes to un-draw itself.
///
/// Short enough that a reader stepping quickly is not waiting on it, long
/// enough that the marks read as being taken back rather than cut.
pub const EXIT_MS: u16 = 140;

/// How long a part stays out of the frames before the terminal forgets it.
///
/// The mirror of stoatty's `SKETCH_GRACE`. A mark the terminal has not seen for
/// that long has lost its clock, and it draws on the next timing it is sent.
const REDECLARE_GRACE: Duration = Duration::from_millis(250);

/// Light the current annotation's code, and emit the current stop's marks,
/// connectors, and label boxes.
///
/// A no-op with no walkthrough playing or when the focused pane is not an
/// editor. Under a terminal that draws no marks, only the code is lit. Returns
/// whether it lit the code.
pub(crate) fn render_slide(stoat: &mut Stoat, buf: &mut Buffer, scene: &mut ApcScene) -> bool {
    let Some(content) = focused_content_area(stoat) else {
        return false;
    };
    let Some(input) = measure(stoat) else {
        return false;
    };
    let colors = Colors::of(stoat);
    spotlight(buf, &input, &colors, content);

    if !scene.live() || !stoat.stoatty || stoat.stoatty_protocol < SKETCH_PROTOCOL {
        return true;
    }

    // A retiring slide holds the screen while it goes, and the arriving one
    // waits that out rather than drawing over it.
    let retiring = retire(stoat, scene);
    let input = SlideInput {
        start_offset_ms: input.start_offset_ms + if retiring { EXIT_MS } else { 0 },
        ..input
    };

    let slide = match stoat.active_workspace_mut().walkthrough.as_mut() {
        Some(run) => run.slide_for(&input),
        None => return true,
    };

    // The card is part of the slide, so it goes where the slide puts it. The
    // popup paints from its placement on the line after this one, and the size
    // it was pinned with is what `measure` fed the placement, so moving it
    // here settles at the same rect on every later frame.
    if let Some(card) = slide.card
        && let Some(popup) = stoat.pending_hover.as_mut().filter(|popup| popup.pinned)
    {
        popup.placement = Some(card);
    }

    // Taken rather than read, so the painter owns the record while it schedules
    // and hands it back below.
    let last_declared = match stoat.active_workspace_mut().walkthrough.as_mut() {
        Some(run) => std::mem::take(&mut run.last_declared),
        None => return true,
    };
    let now = stoat.executor.now();
    let Some(run) = stoat.active_workspace().walkthrough.as_ref() else {
        return true;
    };
    let painter = Painter {
        ids: SlideIds::of(run),
        colors,
        labels: input
            .annotations
            .iter()
            .map(|annotation| (annotation.key, annotation.label_lines.clone()))
            .collect(),
        anchor: pool_anchor(stoat),
        label_scale: stoat.theme.type_scale(TypeRole::Ui),
        protocol: stoat.stoatty_protocol,
        now,
        opening: last_declared.is_empty(),
        last_declared,
        declared: SlideParts::default(),
    };

    let (declared, last_declared) = {
        let mut painter = painter;
        painter.focus(&slide, buf, scene);
        painter.callouts(&slide, buf, scene);
        // Last, so the card's seq is the highest and it occludes the marks it
        // covers rather than being drawn through by them.
        painter.card(&slide, scene);
        (painter.declared, painter.last_declared)
    };

    if let Some(run) = stoat.active_workspace_mut().walkthrough.as_mut() {
        run.last_parts = declared;
        run.last_declared = last_declared;
    }
    true
}

/// Light the current annotation's code in its marker color, and dim the rest of
/// the pane's content toward the background.
///
/// The dim covers all of `dim`, gutter included, so the line numbers recede
/// with the code.
///
/// A no-op while the reader is on the focus, or when the current annotation's
/// code is off screen or in another file. A theme with no RGB background gets
/// the color without the dim.
fn spotlight(buf: &mut Buffer, input: &SlideInput, colors: &Colors, dim: Rect) {
    let Some(annotation) = input.current.and_then(|key| {
        input
            .annotations
            .iter()
            .find(|annotation| annotation.key == key)
    }) else {
        return;
    };
    let range = &annotation.range;
    let (Some(&first), Some(&last)) = (range.rows.first(), range.rows.last()) else {
        return;
    };

    if let Some(bg) = colors.background {
        crate::render::pane::dim_pane_content(buf, dim, bg, SPOTLIGHT_DIM);
    }

    let line_end = |row: u16| {
        input
            .line_ends
            .iter()
            .find(|(at, _)| *at == row)
            .map_or(input.pane.right(), |(_, end)| *end)
    };
    let [r, g, b] = colors.marker(annotation.key);
    for &row in &range.rows {
        let start = if row == first {
            range.start_x
        } else {
            input.pane.x
        };
        let end = if row == last {
            range.end_x + 1
        } else {
            line_end(row)
        };
        for x in start..end.min(input.pane.right()) {
            buf[(x, row)].set_fg(Color::Rgb(r, g, b));
        }
    }
}

/// What [`spotlight`] paints on the live grid, carried to the focused editor's
/// pooled pages so a glide composites the same colors.
#[derive(Clone, PartialEq, Debug)]
pub struct Spotlight {
    /// The buffer offsets of the current annotation's code, first and last
    /// character.
    pub range: RangeInclusive<usize>,
    /// The annotation's marker color.
    pub color: [u8; 3],
    /// How far the rest of the page dims toward the background, zero when the
    /// theme gives no RGB background.
    pub dim: f32,
}

/// The focused pane's editor and the spotlight the live grid paints on it for
/// the frame just drawn, or `None` when it paints none.
///
/// It paints none while the reader is on the focus, for an annotation in
/// another file, for one whose code is off screen, and while the focused pane
/// shows a file other than the stop's. A pooled page carries the same answer,
/// because a page lit where the settled grid is not flashes a dim that the
/// grid never shows once the glide ends.
pub fn spotlight_of(stoat: &mut Stoat) -> Option<(EditorId, Spotlight)> {
    let (range, key) = {
        let run = stoat.active_workspace().walkthrough.as_ref()?;
        let annotation = run
            .current_annotation()
            .filter(|annotation| annotation.path.is_none())?;
        let key = run.annotation_progress()?.0 - 1;
        (key < run.drawable_annotations()).then_some((annotation.range, key))?
    };
    let colors = Colors::of(stoat);

    let ws = stoat.active_workspace();
    let View::Editor(editor_id) = ws.panes.pane(ws.panes.focus()).view else {
        return None;
    };
    if !shows_stop_file(stoat, editor_id) {
        return None;
    }
    let editor = stoat.active_workspace_mut().editors.get_mut(editor_id)?;
    let offsets = {
        let snapshot = editor.display_map.snapshot();
        range_offsets(snapshot.buffer_snapshot().rope(), &range)
    };
    screen::visible_span(editor, offsets.clone())?;

    Some((
        editor_id,
        Spotlight {
            range: offsets,
            color: colors.marker(key),
            dim: match colors.background {
                Some(_) => SPOTLIGHT_DIM,
                None => 0.0,
            },
        },
    ))
}

/// Whether `editor_id` shows the current stop's file.
///
/// A stop's ranges name lines of its own file, so a slide over another file
/// marks code the stop never described. The stop's path resolves the way the
/// jump opens it.
fn shows_stop_file(stoat: &Stoat, editor_id: EditorId) -> bool {
    let ws = stoat.active_workspace();
    let Some(run) = ws.walkthrough.as_ref() else {
        return false;
    };
    let stop_path = buffer_lifecycle::absolute_path(stoat, &run.current_stop().focus.path);
    ws.editors
        .get(editor_id)
        .and_then(|editor| ws.buffers.path_for(editor.buffer_id))
        .is_some_and(|path| path == stop_path)
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
    /// The connector and label of each annotation, in order.
    annotations: Vec<(u32, u32)>,
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
    /// The narration card's outline, which is the one part with a color of its
    /// own rather than its annotation's.
    card_stroke: [u8; 3],
    /// The theme background, which the code outside a lit annotation dims
    /// toward. `None` when the theme gives no RGB background.
    background: Option<[u8; 3]>,
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
            card_stroke: crate::render::paint::style_rgb(card.fg).unwrap_or([255, 255, 255]),
            background: crate::render::paint::style_rgb(stoat.theme.get(scope::UI_BACKGROUND).bg),
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
struct Painter {
    ids: SlideIds,
    colors: Colors,
    /// Each annotation's wrapped label lines, by key.
    labels: HashMap<usize, Vec<String>>,
    /// The pool the marks ride, so they glide with the pane rather than
    /// staying pinned to the screen.
    anchor: Option<(u32, f32)>,
    /// The scale the label text draws at, the theme's [`TypeRole::Ui`] size.
    label_scale: u16,
    /// The terminal's protocol version, which decides whether a connector
    /// that bends goes out as the shape its route names or as a straight line.
    protocol: u32,
    /// The scheduler clock this frame reads, so every part measures its absence
    /// against the same instant.
    now: Instant,
    /// Whether this frame opens the slide, where every part takes the slide's
    /// own schedule.
    opening: bool,
    /// When each part id last went out in a frame. Taken from the run and
    /// handed back with the rest.
    last_declared: HashMap<u32, Instant>,
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
pub struct SlideParts {
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

impl Painter {
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

    /// The timing that part `id` goes out with on this frame.
    ///
    /// The slide's own schedule, unless the part comes back after at least
    /// [`REDECLARE_GRACE`] out of the frames. The terminal has dropped that
    /// part's clock and latches the next timing it is sent. On the schedule the
    /// part waits out the stagger again, so it draws at once instead.
    ///
    /// The gap runs from the last frame this side put the part in, and the
    /// terminal holds the scene between frames. A part that stays on screen
    /// through an idle spell as long as the grace therefore also goes out with
    /// no delay. The terminal still holds that part's clock and ignores the
    /// timing, so the screen does not change.
    fn schedule(&mut self, id: u32, scheduled: SketchTiming) -> SketchTiming {
        self.schedule_with(id, scheduled, scheduled.delay_ms)
    }

    /// [`Self::schedule`], but a part the terminal does not hold starts at its
    /// offset from `group_start` in the slide's schedule, rather than at once.
    ///
    /// A callout that a step reveals draws as one gesture. Its mark goes at
    /// once, and its connector and label follow at their offsets from the mark.
    fn schedule_with(
        &mut self,
        id: u32,
        scheduled: SketchTiming,
        group_start: u16,
    ) -> SketchTiming {
        let last = self.last_declared.insert(id, self.now);
        let seen = last.is_some_and(|at| self.now.saturating_duration_since(at) < REDECLARE_GRACE);
        if self.opening || seen {
            return scheduled;
        }
        SketchTiming::after(
            scheduled.delay_ms.saturating_sub(group_start),
            scheduled.duration_ms,
        )
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
            timing: self.schedule(self.ids.focus, timing_of(slide, Some(slide::Part::Focus))),
            fill: None,
        };
        self.mark(mark, stroke, buf, scene);

        if !slide.focus_link {
            return;
        }
        let timing = self.schedule(
            self.ids.focus_link,
            timing_of(slide, Some(slide::Part::FocusLink)),
        );
        self.link(
            Stroke {
                id: self.ids.focus_link,
                timing,
                ..stroke
            },
            SketchShape::Line {
                from: SketchEnd::Component {
                    id: self.ids.focus,
                    side: SketchSide::Auto,
                },
                to: SketchEnd::Component {
                    id: self.ids.card,
                    side: SketchSide::Auto,
                },
                bend: 0,
                heads: 0,
            },
            buf,
            scene,
        );
    }

    /// Emit the narration card's box.
    ///
    /// The card is one of the slide's parts, so it draws on the slide's
    /// schedule and is recorded for the exit with everything else. The body
    /// text is written by the hover render, which runs after this and appends
    /// its runs to the same record.
    fn card(&mut self, slide: &Slide, scene: &mut ApcScene) {
        let Some(rect) = slide.card else {
            return;
        };
        let timing = self.schedule(self.ids.card, timing_of(slide, Some(slide::Part::Card)));
        self.declare(
            SketchCommand {
                id: self.ids.card,
                style: card_style(self.colors.card_stroke),
                timing,
                shape: SketchShape::Rect {
                    bounds: SketchBounds {
                        x: rect.x as i16 * 16,
                        y: rect.y as i16 * 16,
                        w: rect.width * 16,
                        h: rect.height * 16,
                    },
                    radius: sketch_corner_radius(rect.width, rect.height),
                    fill: Some(SketchFill {
                        color: self.colors.fill,
                        alpha: 255,
                        style: SketchFillStyle::Solid,
                    }),
                },
                anchor: self.anchor,
            },
            scene,
        );
    }

    /// Emit each annotation's label box and the connector to it.
    fn callouts(&mut self, slide: &Slide, buf: &mut Buffer, scene: &mut ApcScene) {
        for callout in &slide.callouts {
            let Some(&(link_id, label_id)) = self.ids.annotations.get(callout.key) else {
                continue;
            };

            // A label this slide never drew belongs to an annotation a step just
            // reached, so its callout draws from this frame on, starting with
            // its first part. A part back in view after the grace was drawn
            // before, and draws at once.
            let label_scheduled = timing_of(slide, Some(slide::Part::Label(callout.key)));
            let link_scheduled = callout
                .link
                .is_some()
                .then(|| timing_of(slide, Some(slide::Part::Link(callout.key))));
            let group_start = (!self.opening && !self.last_declared.contains_key(&label_id))
                .then(|| link_scheduled.unwrap_or(label_scheduled).delay_ms);
            let start = |scheduled: SketchTiming| group_start.unwrap_or(scheduled.delay_ms);

            // The box before its connector, so the line has something to
            // arrive at by the time it is drawn.
            let lines = self.labels.get(&callout.key).cloned().unwrap_or_default();
            let stroke = Stroke {
                id: label_id,
                color: self.colors.marker(callout.key),
                emphasis: slide.emphasis(callout.key),
                timing: self.schedule_with(label_id, label_scheduled, start(label_scheduled)),
                fill: Some(self.colors.fill),
            };
            self.label(callout.label, stroke, &lines, buf, scene);

            if let Some(scheduled) = link_scheduled
                && let Some(link) = &callout.link
            {
                let timing = self.schedule_with(link_id, scheduled, start(scheduled));
                self.link(
                    Stroke {
                        id: link_id,
                        timing,
                        fill: None,
                        ..stroke
                    },
                    link_shape(link, self.protocol),
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
            Mark::Ellipse(rect) => SketchShape::Ellipse {
                bounds: bounds_of(rect),
                fill: None,
            },
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

    /// Emit a connector as `shape`, a line or a path.
    ///
    /// The focus connector names the card, so the terminal meets the card on
    /// the side facing the mark. A label connector carries every point from the
    /// layout, which alone knows what lies between the code and the box.
    fn link(
        &mut self,
        stroke: Stroke,
        shape: SketchShape,
        _buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
        self.declare(
            SketchCommand {
                id: stroke.id,
                style: SketchStyle {
                    width: LINK_WIDTH,
                    ..mark_style(stroke)
                },
                timing: stroke.timing,
                shape,
                anchor: self.anchor,
            },
            scene,
        );
    }

    /// Emit one label box and the text inside it.
    ///
    /// The cells under the box keep their code. The terminal draws the opaque
    /// fill over the grid, so the fill hides that code. A clear blanks whole
    /// cells, and so also blanks the code outside the rounded corners.
    fn label(
        &mut self,
        box_: Rect,
        stroke: Stroke,
        lines: &[String],
        _buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
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
                    radius: sketch_corner_radius(box_.width, box_.height),
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
                    scale: self.label_scale,
                    color: stroke.color,
                    bg: None,
                    text: line.clone(),
                    bold: false,
                    italic: false,
                    follow: stroke.id,
                    anchor: self.anchor,
                },
                scene,
            );
        }
    }
}

/// The shape a label connector goes out as.
///
/// A straight route is a straight line. A bowed one is a path through its
/// midpoint, and an elbow is a polyline through its corners, each rounded by
/// [`LINK_CORNER`], when `protocol` says the terminal draws that shape. A
/// terminal that does not ignores the frame whole, so there the connector is a
/// straight line between the same ends. A pointer that crosses something still
/// says which code the label names.
pub fn link_shape(link: &Link, protocol: u32) -> SketchShape {
    let points = |between: &[(i16, i16)]| {
        [&[link.from][..], between, &[link.to]]
            .concat()
            .into_iter()
            .map(|(x, y)| SketchPoint { x, y })
            .collect()
    };

    match &link.route {
        Route::Bowed(mid) if protocol >= PATH_PROTOCOL => SketchShape::Path {
            points: points(&[*mid]),
        },
        Route::Elbow(corners) if protocol >= ELBOW_PROTOCOL => SketchShape::Elbow {
            points: points(corners),
            radius: LINK_CORNER as u8,
        },
        Route::Straight | Route::Bowed(_) | Route::Elbow(_) => SketchShape::Line {
            from: SketchEnd::Point {
                x: link.from.0,
                y: link.from.1,
            },
            to: SketchEnd::Point {
                x: link.to.0,
                y: link.to.1,
            },
            bend: 0,
            heads: 0,
        },
    }
}

/// How far a sketched box rounds its corners, in sixteenths of a cell.
///
/// A quarter of the box's shorter side, capped at [`CORNER_RADIUS_CAP`], which
/// is what the reference does in pixels. A fixed rounding reads machine-square
/// beside a hand-drawn stroke, and the larger the box the more it does.
///
/// Stoat states bounds in cells and never sees the pixel size, so the shorter
/// side is found under the typical two-to-one cell aspect. A cell is twice as
/// tall as it is wide, so a height in cells counts double against a width.
///
/// The focus mark passes no radius through here. It hugs its text with a
/// four-sixteenth pad, so rounding pulls the stroke across the first and last
/// characters it circles.
pub(crate) fn sketch_corner_radius(width_cells: u16, height_cells: u16) -> u8 {
    let shorter = width_cells.min(height_cells.saturating_mul(2));
    let radius = shorter.saturating_mul(4).min(CORNER_RADIUS_CAP);
    radius as u8
}

/// The stroke the narration card draws with.
///
/// The slide draws the card when it places it, and the hover render draws it
/// when the slide does not. Both take this, so the card has one weight
/// whichever path draws it.
pub(crate) fn card_style(stroke: [u8; 3]) -> SketchStyle {
    SketchStyle {
        width: WIDTH_PLAIN,
        ..SketchStyle::marker(stroke)
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

/// The focused pane's content area, gutter included.
///
/// `None` when the focused pane is not an editor.
pub fn focused_content_area(stoat: &Stoat) -> Option<Rect> {
    let ws = stoat.active_workspace();
    let pane = ws.panes.pane(ws.panes.focus());
    match pane.view {
        View::Editor(_) => Some(pane.area),
        _ => None,
    }
}

/// Measure the screen into the layout's input.
///
/// `None` when no tour plays, the focused pane is not an editor, that editor
/// shows a file other than the stop's, or the render has not painted it yet.
pub fn measure(stoat: &mut Stoat) -> Option<SlideInput> {
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

    let ws = stoat.active_workspace();
    let pane = ws.panes.pane(ws.panes.focus());
    let View::Editor(editor_id) = pane.view else {
        return None;
    };
    if !shows_stop_file(stoat, editor_id) {
        return None;
    }
    let pane_rect = ws.editors.get(editor_id)?.text_rect?;

    let focus = measure_range(stoat, editor_id, &focus_range);
    let annotations = annotations
        .into_iter()
        .filter_map(|(key, range, label)| {
            let range = measure_range(stoat, editor_id, &range)?;
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
        cell_pixels: stoat.cell_pixels,
        card,
        current,
        card_hidden,
        start_offset_ms: 0,
    })
}

/// Turn a stored range into the cells it covers on screen.
///
/// A range that crosses the view's edge is cut at that edge. `None` when no row
/// of it is on screen, which the layout turns into no mark.
fn measure_range(stoat: &mut Stoat, editor_id: EditorId, range: &Range) -> Option<CellRange> {
    let ws = stoat.active_workspace_mut();
    let editor = ws.editors.get_mut(editor_id)?;
    let offsets = {
        let snapshot = editor.display_map.snapshot();
        range_offsets(snapshot.buffer_snapshot().rope(), range)
    };

    let span = screen::visible_span(editor, offsets)?;
    Some(CellRange {
        rows: span.rows.collect(),
        start_x: span.start_x,
        end_x: span.end_x,
    })
}

/// The buffer offsets of a stored range's first and last characters.
fn range_offsets(rope: &Rope, range: &Range) -> RangeInclusive<usize> {
    // Stored ranges are one-based, and a Point is zero-based.
    let offset = |line: u32, col: u32| {
        rope.point_to_offset(Point::new(line.saturating_sub(1), col.saturating_sub(1)))
    };
    offset(range.start.line, range.start.col)..=offset(range.end.line, range.end.col)
}

/// Where the text ends on each visible row, so a box lands past it rather than
/// over it.
fn line_ends(stoat: &mut Stoat, editor_id: EditorId, pane: Rect) -> Vec<(u16, u16)> {
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
    use super::sketch_corner_radius;
    use crate::walkthrough::slide::{Link, Route};
    use stoatty_protocol::command::{SketchEnd, SketchPoint, SketchShape};

    /// A rounding that does not grow with the box reads machine-square beside
    /// the hand-drawn stroke around it, so it tracks the shorter side.
    #[test]
    fn a_sketched_box_rounds_by_its_shorter_side() {
        assert_eq!(
            sketch_corner_radius(20, 3),
            24,
            "a wide label rounds by its height, which counts double",
        );
        assert_eq!(
            sketch_corner_radius(3, 20),
            12,
            "a tall one rounds by its width",
        );
        assert_eq!(sketch_corner_radius(40, 10), 56, "a large card caps");
        assert_eq!(sketch_corner_radius(1, 1), 4, "a single cell barely rounds");
    }

    /// A connector that bends goes out as the shape its route names, where the
    /// terminal draws that shape. A bowed route is a path through its midpoint,
    /// and an elbow is a polyline through its corners. An older terminal ignores
    /// either frame whole, so there the connector is a straight line between
    /// the same ends.
    #[test]
    fn a_bent_connector_takes_a_shape_the_terminal_draws() {
        let bowed = Link {
            from: (0, 16),
            to: (160, 48),
            route: Route::Bowed((80, 0)),
        };
        let straight = Link {
            route: Route::Straight,
            ..bowed.clone()
        };
        let elbow = Link {
            route: Route::Elbow(vec![(80, 16), (80, 48)]),
            ..bowed.clone()
        };
        let line = SketchShape::Line {
            from: SketchEnd::Point { x: 0, y: 16 },
            to: SketchEnd::Point { x: 160, y: 48 },
            bend: 0,
            heads: 0,
        };

        assert_eq!(
            [
                super::link_shape(&bowed, 6),
                super::link_shape(&bowed, 5),
                super::link_shape(&straight, 6),
                super::link_shape(&elbow, 7),
                super::link_shape(&elbow, 6),
            ],
            [
                SketchShape::Path {
                    points: vec![
                        SketchPoint { x: 0, y: 16 },
                        SketchPoint { x: 80, y: 0 },
                        SketchPoint { x: 160, y: 48 },
                    ],
                },
                line.clone(),
                line.clone(),
                SketchShape::Elbow {
                    points: vec![
                        SketchPoint { x: 0, y: 16 },
                        SketchPoint { x: 80, y: 16 },
                        SketchPoint { x: 80, y: 48 },
                        SketchPoint { x: 160, y: 48 },
                    ],
                    radius: 16,
                },
                line,
            ],
            "a path or an elbow where the terminal draws one, else a straight \
             line, and a straight line for a straight route",
        );
    }
}
