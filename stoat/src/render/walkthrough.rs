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
use ratatui::{buffer::Buffer, layout::Rect, style::Color};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
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
const SPOTLIGHT_DIM: f32 = 0.5;

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
pub(crate) const EXIT_MS: u16 = 140;

/// How long a part stays out of the frames before the terminal forgets it.
///
/// The mirror of stoatty's `SKETCH_GRACE`. A mark the terminal has not seen for
/// that long has lost its clock, and it draws on the next timing it is sent.
const REDECLARE_GRACE: Duration = Duration::from_millis(250);

/// Light the current annotation's code, and emit the current stop's marks,
/// connectors, and label boxes.
///
/// A no-op with no walkthrough playing or when the focused pane is not an
/// editor. Under a terminal that draws no marks, only the code is lit.
pub(crate) fn render_slide(stoat: &mut Stoat, buf: &mut Buffer, scene: &mut ApcScene) {
    let Some(input) = measure(stoat) else {
        return;
    };
    let colors = Colors::of(stoat);
    spotlight(buf, &input, &colors);

    if !scene.live() || !stoat.stoatty || stoat.stoatty_protocol < SKETCH_PROTOCOL {
        return;
    }

    // A retiring slide holds the screen while it goes, and the arriving one
    // waits that out rather than drawing over it.
    let retiring = retire(stoat, scene);
    let input = SlideInput {
        start_offset_ms: input.start_offset_ms + if retiring { EXIT_MS } else { 0 },
        ..input
    };

    let slide = slide::layout(&input);

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
        None => return,
    };
    let now = stoat.executor.now();
    let Some(run) = stoat.active_workspace().walkthrough.as_ref() else {
        return;
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
}

/// Light the current annotation's code in its marker color, and dim the rest of
/// the pane's content toward the background.
///
/// A no-op while the reader is on the focus, or when the current annotation's
/// code is off screen or in another file. A theme with no RGB background gets
/// the color without the dim.
fn spotlight(buf: &mut Buffer, input: &SlideInput, colors: &Colors) {
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
        crate::render::pane::dim_pane_content(buf, input.pane, bg, SPOTLIGHT_DIM);
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
            SketchEnd::Component {
                id: self.ids.focus,
                side: SketchSide::Auto,
            },
            self.ids.card,
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

            if let Some(scheduled) = link_scheduled {
                let timing = self.schedule_with(link_id, scheduled, start(scheduled));
                // The line leaves the code just past its last cell, level with
                // its first row, which the label's placement is measured from.
                let code_end = SketchEnd::Point {
                    x: (callout.range.end_x as i16 + 1) * 16 + 4,
                    y: callout.range.rows[0] as i16 * 16 + 8,
                };
                self.link(
                    Stroke {
                        id: link_id,
                        timing,
                        fill: None,
                        ..stroke
                    },
                    code_end,
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

    /// Emit a connector from `from` to the mark `to`.
    ///
    /// The `to` end names a mark, so the connector tracks it as it moves and
    /// meets it on the side facing `from`.
    fn link(
        &mut self,
        stroke: Stroke,
        from: SketchEnd,
        to: u32,
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
                shape: SketchShape::Line {
                    from,
                    to: SketchEnd::Component {
                        id: to,
                        side: SketchSide::Auto,
                    },
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
    use super::{sketch_corner_radius, EXIT_MS, SPOTLIGHT_DIM};
    use crate::{
        action_handlers::walkthrough::open,
        app::Stoat,
        render::paint,
        test_harness::TestHarness,
        theme::scope,
        walkthrough::{
            run::{part, ID_SPACE, STOP_ID_STRIDE},
            slide, Location, Point, Range, Walkthrough,
        },
    };
    use ratatui::style::Color;
    use std::{path::PathBuf, time::Duration};
    use stoatty_protocol::command::{
        self, Command, SketchCommand, SketchEnd, SketchPhase, SketchShape, SketchTiming,
    };

    const CODE: &str = "fn one() {}\nfn two() {}\nfn three() {}\n";

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
        harness_over(annotations, CODE)
    }

    /// The same tour over `code`, for a test that needs the marks to land on
    /// lines long enough to read under them.
    fn harness_over(annotations: &[(u32, &str)], code: &str) -> TestHarness {
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

        h.fake_fs().insert_file("/repo/a.rs", code);
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

    /// The rect the narration card occupies after a frame has placed it.
    fn card_rect(h: &mut TestHarness) -> ratatui::layout::Rect {
        h.stoat.render();
        h.stoat
            .pending_hover
            .as_ref()
            .expect("the stop narrates")
            .placement
            .expect("the card is placed")
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

    /// The connector and label ids of annotation `at`.
    fn annotation_ids(h: &TestHarness, at: usize) -> (u32, u32) {
        let run = h
            .stoat
            .active_workspace()
            .walkthrough
            .as_ref()
            .expect("a tour is playing");
        run.annotation_ids(at)
    }

    /// Step onto the next `count` annotations, as a reader pressing `a` does.
    fn reach(h: &mut TestHarness, count: usize) {
        for _ in 0..count {
            crate::action_handlers::walkthrough::next_annotation(&mut h.stoat);
        }
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
            matches!(
                focus.shape,
                SketchShape::Ellipse {
                    bounds: _,
                    fill: None,
                }
            ),
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

    /// Every annotation draws a label box under its own id, so a stop with two
    /// points reads as two rather than one. None draws a mark of its own, so
    /// the focus is the only enclosure on screen.
    #[test]
    fn each_annotation_draws_only_its_label() {
        let mut h = harness(&[(1, "one"), (3, "two")]);
        open(&mut h.stoat, "tour");
        reach(&mut h, 2);

        let emitted = sketches(&mut h);
        for at in 0..2 {
            let (_, label) = annotation_ids(&h, at);
            assert!(
                emitted.iter().any(|sketch| sketch.id == label),
                "annotation {at} draws its label box",
            );
        }

        let enclosures: Vec<u32> = emitted
            .iter()
            .filter(|sketch| {
                matches!(
                    sketch.shape,
                    SketchShape::Ellipse { .. } | SketchShape::Rect { fill: None, .. }
                )
            })
            .map(|sketch| sketch.id)
            .collect();
        assert_eq!(
            enclosures,
            [part_id(&h, part::FOCUS_MARK)],
            "the focus is the only enclosure",
        );
    }

    /// The card and a label the reader has left behind draw at the focus mark's
    /// weight, so no box reads as heavier chrome than the rest. Only the label
    /// the reader is on draws heavier.
    #[test]
    fn only_the_current_label_draws_heavier_than_the_focus() {
        let mut h = harness(&[(1, "one"), (3, "two")]);
        open(&mut h.stoat, "tour");
        reach(&mut h, 2);

        let emitted = sketches(&mut h);
        let width = |id: u32| {
            emitted
                .iter()
                .find(|sketch| sketch.id == id)
                .map(|sketch| sketch.style.width)
                .expect("the part draws")
        };
        let plain = width(part_id(&h, part::FOCUS_MARK));

        assert_eq!(
            [
                width(part_id(&h, part::CARD)),
                width(annotation_ids(&h, 0).1)
            ],
            [plain, plain],
            "the card and the label left behind take the focus mark's weight",
        );
        assert!(
            width(annotation_ids(&h, 1).1) > plain,
            "the current label draws heavier",
        );
    }

    /// An annotation in another file names a range of that file. The same line
    /// numbers in the file on screen hold other code, so it draws nothing here,
    /// even once reached.
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
        walkthrough
            .add_annotation(
                "s1",
                None,
                range_of(1, (1, 11)),
                "fn one() {}".to_owned(),
                "right here".to_owned(),
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
        // On past it to the annotation in this file, so it is reached while
        // this file is on screen.
        reach(&mut h, 2);
        let mark = {
            let run = h
                .stoat
                .active_workspace()
                .walkthrough
                .as_ref()
                .expect("playing");
            run.annotation_ids(0).1
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
        reach(&mut h, 2);

        let expected: Vec<[u8; 3]> = (0..2)
            .map(|at| {
                let style = h.stoat.theme.get(scope::UI_WALKTHROUGH_MARKERS[at]);
                paint::style_rgb(style.fg).expect("the theme names a color")
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
            [run.annotation_ids(0).1, run.annotation_ids(1).1]
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
    /// walking onto an annotation draws it bright and dims the ones before it.
    #[test]
    fn walking_onto_an_annotation_dims_the_others() {
        let mut h = harness(&[(1, "one"), (3, "two")]);
        open(&mut h.stoat, "tour");

        let alphas = |h: &mut TestHarness| -> [Option<u8>; 2] {
            let run_ids = {
                let run = h
                    .stoat
                    .active_workspace()
                    .walkthrough
                    .as_ref()
                    .expect("playing");
                [run.annotation_ids(0).1, run.annotation_ids(1).1]
            };
            let emitted = sketches(h);
            run_ids.map(|id| {
                emitted
                    .iter()
                    .find(|sketch| sketch.id == id)
                    .map(|sketch| sketch.style.alpha)
            })
        };

        assert_eq!(alphas(&mut h), [None, None], "the stop opens on its focus");

        reach(&mut h, 1);
        assert_eq!(
            alphas(&mut h),
            [Some(255), None],
            "a step draws the one it lands on",
        );

        reach(&mut h, 1);
        assert_eq!(
            alphas(&mut h),
            [Some(110), Some(255)],
            "the one walked onto is bright and the one before it recedes",
        );
    }

    /// A step onto an annotation draws its callout then and there, connector
    /// first. On the slide's own schedule it waits out the focus, the card, and
    /// every callout before it.
    #[test]
    fn a_stepped_onto_annotation_draws_its_callout_at_once() {
        // On its own row the second label overlaps the first, so it moves off
        // that row and takes a connector.
        let mut h = harness(&[(1, "one"), (3, "two")]);
        open(&mut h.stoat, "tour");
        reach(&mut h, 1);
        sketches(&mut h);

        reach(&mut h, 1);
        let emitted = sketches(&mut h);
        let timing = |id: u32| {
            emitted
                .iter()
                .find(|sketch| sketch.id == id)
                .map(|sketch| sketch.timing)
        };
        let (link, label) = annotation_ids(&h, 1);
        let (link_at, link_ms) = slide_timing(&mut h.stoat, slide::Part::Link(1));
        let (label_at, label_ms) = slide_timing(&mut h.stoat, slide::Part::Label(1));

        assert_eq!(timing(link), Some(SketchTiming::after(0, link_ms)));
        assert_eq!(
            timing(label),
            Some(SketchTiming::after(label_at - link_at, label_ms)),
            "the label follows its connector as the slide choreographs it",
        );
    }

    /// A connector leaves the annotation's code just past its last cell, so it
    /// reads as pointing from the code to the label.
    #[test]
    fn a_labels_connector_starts_at_its_code() {
        let mut h = harness(&[(1, "one"), (3, "two")]);
        open(&mut h.stoat, "tour");
        reach(&mut h, 2);

        let emitted = sketches(&mut h);
        let input = super::measure(&mut h.stoat).expect("the pane measures");
        let range = &input.annotations[1].range;
        let link = emitted
            .iter()
            .find(|sketch| sketch.id == annotation_ids(&h, 1).0)
            .expect("the label that moved has a connector");

        let SketchShape::Line { from, .. } = &link.shape else {
            panic!("a connector is a line, got {:?}", link.shape);
        };
        assert_eq!(
            *from,
            SketchEnd::Point {
                x: (range.end_x as i16 + 1) * 16 + 4,
                y: range.rows[0] as i16 * 16 + 8,
            },
        );
    }

    /// The annotation the reader is on shows its code in its own marker color,
    /// and the code around it recedes toward the background.
    #[test]
    fn the_current_annotations_code_takes_its_marker_color() {
        assert_spotlit(3);
    }

    /// The highlight is plain cell color, so a terminal that draws no marks
    /// still shows which code the reader is on.
    #[test]
    fn a_plain_terminal_still_highlights_the_annotation() {
        assert_spotlit(0);
    }

    /// Step onto the one annotation of a tour under `protocol`, then check that
    /// its first cell takes its marker color and a cell two rows down dims.
    fn assert_spotlit(protocol: u32) {
        let mut h = harness(&[(1, "one")]);
        h.stoat.stoatty_protocol = protocol;
        open(&mut h.stoat, "tour");
        h.snapshot();
        let pane = super::measure(&mut h.stoat)
            .expect("the pane measures")
            .pane;
        let outside = (pane.x, pane.y + 2);
        let Color::Rgb(r, g, b) = h.rendered_buffer()[outside].fg else {
            panic!("the code is painted in RGB");
        };

        reach(&mut h, 1);
        h.snapshot();
        let range = super::measure(&mut h.stoat)
            .expect("the pane measures")
            .annotations[0]
            .range
            .clone();
        let rgb = |color| paint::style_rgb(color).expect("the theme names an RGB color");
        let marker = rgb(h.stoat.theme.get(scope::UI_WALKTHROUGH_MARKERS[0]).fg);
        let background = rgb(h.stoat.theme.get(scope::UI_BACKGROUND).bg);
        let dimmed = paint::dim_rgb([r, g, b], background, SPOTLIGHT_DIM);

        let fg = |cell: (u16, u16)| h.rendered_buffer()[cell].fg;
        assert_eq!(
            (fg((range.start_x, range.rows[0])), fg(outside)),
            (
                Color::Rgb(marker[0], marker[1], marker[2]),
                Color::Rgb(dimmed[0], dimmed[1], dimmed[2]),
            ),
            "the annotation's code lights and the code around it dims",
        );
    }

    /// On the focus nothing is lit, so the code reads as it does with no tour
    /// open. The focus mark alone says what the stop is about.
    #[test]
    fn the_focus_alone_dims_nothing() {
        let mut h = harness(&[(1, "one")]);
        open(&mut h.stoat, "tour");
        h.snapshot();
        let pane = super::measure(&mut h.stoat)
            .expect("the pane measures")
            .pane;
        let cell = (pane.x, pane.y + 2);
        let on_the_focus = h.rendered_buffer()[cell].fg;

        crate::action_handlers::walkthrough::done(&mut h.stoat);
        h.snapshot();
        assert_eq!(
            h.rendered_buffer()[cell].fg,
            on_the_focus,
            "the code keeps its color",
        );
    }

    /// The card is one of the slide's parts, so it opens when the slide says
    /// rather than at once. Drawn on its own clock it arrives over the slide
    /// being retired, and the link that points at it reaches a card that
    /// finished long before.
    #[test]
    fn the_card_draws_on_its_slides_schedule() {
        let mut h = harness(&[]);
        open(&mut h.stoat, "tour");

        let card_id = part_id(&h, part::CARD);
        let emitted = sketches(&mut h);
        let cards: Vec<&SketchCommand> = emitted
            .iter()
            .filter(|sketch| sketch.id == card_id)
            .collect();
        assert_eq!(
            cards.len(),
            1,
            "the card is declared once, so two clocks do not fight over it",
        );
        let card = cards[0];

        let scheduled = slide_timing(&mut h.stoat, slide::Part::Card);
        assert_eq!(
            (card.timing.delay_ms, card.timing.duration_ms),
            scheduled,
            "the card takes the table's own entry",
        );

        let focus = emitted
            .iter()
            .find(|sketch| sketch.id == part_id(&h, part::FOCUS_MARK))
            .expect("the focus draws");
        assert!(
            card.timing.delay_ms >= focus.timing.delay_ms + focus.timing.duration_ms,
            "and waits for the focus it is linked from, {} against {}",
            card.timing.delay_ms,
            focus.timing.delay_ms + focus.timing.duration_ms,
        );
    }

    /// The start and duration of `part`, as the slide's own table holds them.
    fn slide_timing(stoat: &mut Stoat, part: slide::Part) -> (u16, u16) {
        let input = super::measure(stoat).expect("the pane measures");
        let slide = slide::layout(&input);
        slide
            .timing
            .iter()
            .find(|(at, ..)| *at == part)
            .map(|(_, start, duration)| (*start, *duration))
            .expect("the part is scheduled")
    }

    /// The terminal drops the clock of a mark it has not seen for a quarter
    /// second, then latches the next timing it is sent. On the slide's
    /// schedule, a part scrolled back into view waits out the whole stagger
    /// again.
    #[test]
    fn a_part_scrolled_back_into_view_draws_at_once() {
        let (opening, returned) = label_timing_around(Duration::from_millis(300));
        assert_eq!(returned, SketchTiming::after(0, opening.duration_ms));
    }

    /// The terminal still holds the clock of a part gone for less than that, so
    /// the part keeps the timing it opened with.
    #[test]
    fn a_short_absence_keeps_the_schedule() {
        let (opening, returned) = label_timing_around(Duration::from_millis(100));
        assert_eq!(returned, opening);
    }

    /// An annotation label's timing on the slide's opening frame, and on the
    /// frame it comes back on after `absence` out of view.
    fn label_timing_around(absence: Duration) -> (SketchTiming, SketchTiming) {
        let filler: String = (4..=60).map(|n| format!("fn line_{n}() {{}}\n")).collect();
        let mut h = harness_over(&[(1, "one")], &format!("{CODE}{filler}"));
        open(&mut h.stoat, "tour");
        reach(&mut h, 1);

        let label = annotation_ids(&h, 0).1;
        let timing = |h: &mut TestHarness| {
            sketches(h)
                .into_iter()
                .find(|sketch| sketch.id == label)
                .map(|sketch| sketch.timing)
        };
        let scroll_to = |h: &mut TestHarness, row: u32| {
            let (editor, _) = h.stoat.focused_editor_ids().expect("an editor has focus");
            h.stoat.active_workspace_mut().editors[editor].scroll_row = row;
        };

        let opening = timing(&mut h).expect("the label draws");
        assert_ne!(opening.delay_ms, 0, "the label waits for its mark");

        scroll_to(&mut h, 30);
        assert_eq!(timing(&mut h), None, "the label is out of view");

        h.advance_clock(absence);
        scroll_to(&mut h, 0);
        (opening, timing(&mut h).expect("the label is back in view"))
    }

    /// A card that vanished on the step frame left the screen while every mark
    /// around it was still running its stroke back.
    #[test]
    fn a_retiring_cards_text_goes_with_it() {
        let mut h = harness(&[]);
        open(&mut h.stoat, "tour");
        let leaving = part_id(&h, part::CARD);
        frame(&mut h);

        crate::action_handlers::walkthrough::next(&mut h.stoat);
        let emitted = frame(&mut h);

        let card = emitted
            .iter()
            .find_map(|command| match command {
                Command::Sketch(sketch) if sketch.id == leaving => Some(sketch),
                _ => None,
            })
            .expect("the card of the stop being left is re-declared");
        assert_eq!(
            card.timing.phase,
            SketchPhase::Exit,
            "running its stroke back to nothing",
        );

        assert!(
            emitted.iter().any(|command| match command {
                Command::TextRun(run) => run.follow == leaving,
                _ => false,
            }),
            "and its narration goes with it",
        );
    }

    /// A run paints its backing one cell tall and as wide as its text, above the
    /// card's fill. A backing in any other color bands every line of the
    /// narration.
    #[test]
    fn the_cards_body_sits_on_the_cards_fill() {
        let mut h = harness(&[]);
        open(&mut h.stoat, "tour");
        let card = part_id(&h, part::CARD);

        let fill = paint::style_rgb(h.stoat.theme.get(scope::UI_WALKTHROUGH_CARD).bg);
        let background = paint::style_rgb(h.stoat.theme.get(scope::UI_BACKGROUND).bg);
        assert_ne!(fill, background, "the theme sets the card apart");

        let backings: Vec<Option<[u8; 3]>> = frame(&mut h)
            .into_iter()
            .filter_map(|command| match command {
                Command::TextRun(run) if run.follow == card => Some(run.bg),
                _ => None,
            })
            .collect();
        assert!(!backings.is_empty(), "the card has a body");
        assert_eq!(
            backings,
            vec![fill; backings.len()],
            "every run backs onto the card's fill",
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

        h.advance_clock(Duration::from_millis(u64::from(EXIT_MS) + 10));
        assert!(
            !sketches(&mut h).iter().any(|sketch| sketch.id == leaving),
            "and a later frame has let it go",
        );
        assert!(
            h.stoat.active_workspace().walkthrough_exit.is_none(),
            "with nothing left holding it",
        );
    }

    /// A step forward within one file leaves the marks already up on the same
    /// code. Un-drawing them only to draw them again reads as a flicker.
    #[test]
    fn stepping_forward_within_a_file_retires_nothing() {
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

    /// A step back takes the callout it leaves with it, so the annotations up
    /// are always the ones the reader has reached.
    #[test]
    fn stepping_back_retires_the_annotation_left() {
        let mut h = harness(&[(1, "one"), (3, "two")]);
        open(&mut h.stoat, "tour");
        reach(&mut h, 2);
        sketches(&mut h);

        crate::action_handlers::walkthrough::prev_annotation(&mut h.stoat);
        let emitted = sketches(&mut h);
        let declared = |id: u32| -> Vec<(SketchPhase, u8)> {
            emitted
                .iter()
                .filter(|sketch| sketch.id == id)
                .map(|sketch| (sketch.timing.phase, sketch.style.alpha))
                .collect()
        };

        assert_eq!(
            declared(annotation_ids(&h, 1).1),
            [(SketchPhase::Exit, 255)],
            "the label left runs its stroke back off as it last drew",
        );
        assert_eq!(
            declared(annotation_ids(&h, 0).1),
            [(SketchPhase::Enter, 255)],
            "and the one landed on is current again",
        );
        assert!(
            h.stoat.active_workspace().walkthrough_exit.is_some(),
            "with the exit holding it",
        );
    }

    /// A callout reached again while it still runs off draws under the same
    /// ids. Declared in both phases, it restarts its stroke on every frame of
    /// what is left of the exit.
    #[test]
    fn a_callout_reached_again_mid_exit_is_declared_once() {
        let mut h = harness(&[(1, "one"), (3, "two")]);
        open(&mut h.stoat, "tour");
        reach(&mut h, 2);
        sketches(&mut h);
        crate::action_handlers::walkthrough::prev_annotation(&mut h.stoat);
        sketches(&mut h);

        reach(&mut h, 1);
        let label = annotation_ids(&h, 1).1;
        let phases: Vec<SketchPhase> = sketches(&mut h)
            .iter()
            .filter(|sketch| sketch.id == label)
            .map(|sketch| sketch.timing.phase)
            .collect();
        assert_eq!(phases, [SketchPhase::Enter], "drawn back on, not also off");
    }

    /// A label is declared to fade in with the box it sits in. Dropped from the
    /// exit frame it disappears at once, leaving the box to run its stroke back
    /// around where the text was.
    #[test]
    fn a_retiring_label_goes_with_its_box() {
        let mut h = harness(&[(1, "one")]);
        open(&mut h.stoat, "tour");
        reach(&mut h, 1);
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
            let (link, label) = run.annotation_ids(at);
            ids.extend([link, label]);
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
        reach(&mut h, 1);

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

    /// The card is placed against the focus and the code beside it, so it has
    /// to arrive where the slide put it. Pinned top right at every stop, it
    /// covers the same corner whatever the stop is about, and the connector
    /// the slide draws to it reaches a card that is not there.
    #[test]
    fn the_card_lands_where_the_slide_placed_it() {
        let mut h = harness(&[]);
        open(&mut h.stoat, "tour");

        let first = card_rect(&mut h);
        crate::action_handlers::walkthrough::next(&mut h.stoat);
        let second = card_rect(&mut h);

        assert_eq!(
            second.y,
            first.y + 1,
            "the card follows the focus down a row, rather than holding one",
        );
        assert_eq!(second.width, first.width, "at the width it was pinned with");
        assert_eq!(second.height, first.height, "and the height");
    }

    /// A foreign terminal draws no chrome at all, so it gets no marks either.
    #[test]
    fn a_foreign_terminal_gets_no_marks() {
        let mut h = harness(&[(1, "one")]);
        h.stoat.stoatty = false;
        open(&mut h.stoat, "tour");

        assert_eq!(sketches(&mut h), Vec::new());
    }

    /// The terminal draws a box's opaque fill over the code, so the fill hides
    /// what it covers. A clear blanks whole cells, and so also blanks the code
    /// outside the rounded corners. The cells under the card keep their code
    /// before the pen reaches the card and after.
    #[test]
    fn the_code_under_a_box_is_never_cleared() {
        let long: String = (0..12)
            .map(|n| format!("fn name_{n}(value: u32) -> u32 {{ value + {n} }} // padding\n"))
            .collect();
        let mut h = harness_over(&[], &long);
        open(&mut h.stoat, "tour");

        let card = card_rect(&mut h);
        // The card's last row, past the gutter, which its own body does not
        // reach.
        let row = card.y + card.height - 1;
        let under = |h: &TestHarness| -> String {
            (card.x + 4..card.x + card.width)
                .map(|col| {
                    h.rendered_buffer()[(col, row)]
                        .symbol()
                        .chars()
                        .next()
                        .unwrap_or(' ')
                })
                .collect()
        };

        h.snapshot();
        let code = under(&h);
        assert!(!code.trim().is_empty(), "the card sits over code: {code:?}");

        h.advance_clock(Duration::from_millis(600));
        h.snapshot();
        assert_eq!(
            under(&h),
            code,
            "the code stays once the pen has reached the card"
        );
    }
}
