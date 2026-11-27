use ratatui::style::Color;
use std::{path::PathBuf, time::Duration};
use stoat::{
    action_handlers::walkthrough::open,
    apc_emit, app,
    app::Stoat,
    render::{
        paint,
        walkthrough::{Spotlight, EXIT_MS, SPOTLIGHT_DIM},
    },
    test_fixture::drain_apc,
    test_harness::TestHarness,
    theme::scope,
    walkthrough::{
        run::{part, ID_SPACE, STOP_ID_STRIDE},
        slide, Location, Point, Range, Walkthrough,
    },
};
use stoat_action::OpenFile;
use stoat_config::LineNumbers;
use stoatty_protocol::{
    command,
    command::{
        Command, FillCommand, PoolRegionCommand, SketchCommand, SketchPhase, SketchShape,
        SketchTiming, NON_PANE_POOL_BASE,
    },
};

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
    harness_over(annotations, CODE)
}

/// The same tour over `code`, for a test that needs the marks to land on
/// lines long enough to read under them.
fn harness_over(annotations: &[(u32, &str)], code: &str) -> TestHarness {
    harness_with_stops(annotations, code, &[])
}

/// The same tour with a stop on each of `extra` past its two, for a test
/// that walks far enough to tell a count from a single step.
fn harness_with_stops(annotations: &[(u32, &str)], code: &str, extra: &[Location]) -> TestHarness {
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
            None,
        )
        .expect("append");
    walkthrough
        .add_stop(
            Some("second".to_owned()),
            "The **exit**.".to_owned(),
            location(3, (1, 13), "fn three() {}"),
            None,
            None,
        )
        .expect("append");
    for focus in extra {
        walkthrough
            .add_stop(None, "A detour.".to_owned(), focus.clone(), None, None)
            .expect("append");
    }
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
    let bytes = h.stoat.apc_scene.all_bytes();

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
        stoat::action_handlers::walkthrough::next_annotation(&mut h.stoat, 1);
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

    stoat::action_handlers::walkthrough::next(&mut h.stoat, 1);
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
    let mut h = cross_file_tour();
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

/// A stop's marks name lines of its own file, so the slide draws nothing
/// while the focused pane shows another file, whether a step to an
/// annotation there or an open put it on screen.
#[test]
fn a_slide_draws_nothing_over_another_file() {
    let mut h = cross_file_tour();
    let (focus_mark, focus_link) = (part_id(&h, part::FOCUS_MARK), part_id(&h, part::FOCUS_LINK));
    let drawn = |h: &mut TestHarness| -> Vec<u32> {
        sketches(h).into_iter().map(|sketch| sketch.id).collect()
    };

    reach(&mut h, 1);
    let after_the_step = drawn(&mut h);

    reach(&mut h, 1);
    stoat::action_handlers::dispatch(
        &mut h.stoat,
        &OpenFile {
            path: PathBuf::from("/repo/elsewhere.rs"),
        },
    );
    let after_the_open = drawn(&mut h);

    assert_eq!(
        (
            after_the_step
                .iter()
                .any(|&id| id == focus_mark || id == focus_link),
            after_the_open.contains(&focus_mark),
            stoat::render::walkthrough::spotlight_of(&mut h.stoat).is_some(),
        ),
        (false, false, false),
        "neither the step nor the open draws the stop's marks or spotlight there"
    );
}

/// A one-stop tour in `a.rs` whose first annotation names a range of
/// `elsewhere.rs` and whose second names one of `a.rs`, opened on its stop.
fn cross_file_tour() -> TestHarness {
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
    h
}

/// A landing frames the stop's lines, so a focus that fits the pane lands
/// whole and draws its ring. A focus taller than the pane rings the part
/// that shows.
#[test]
fn a_landed_stop_shows_its_whole_focus() {
    let code: String = (1..=100)
        .map(|line| format!("fn f{line}() {{}}\n"))
        .collect();
    let lines = |first: u32, last: u32| Location {
        path: PathBuf::from("a.rs"),
        range: Range {
            start: Point {
                line: first,
                col: 1,
            },
            end: Point { line: last, col: 1 },
        },
        snippet: String::new(),
    };
    let mut h = harness_with_stops(&[], &code, &[lines(60, 66), lines(10, 90)]);
    open(&mut h.stoat, "tour");
    h.stoat.render();
    let rings = |h: &mut TestHarness| {
        let emitted = sketches(h);
        let focus = part_id(h, part::FOCUS_MARK);
        emitted.iter().any(|sketch| sketch.id == focus)
    };

    stoat::action_handlers::walkthrough::next(&mut h.stoat, 2);
    let short_rings = rings(&mut h);
    let (scroll_row, viewport) = {
        let editor =
            stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        (editor.scroll_row, editor.viewport_rows.expect("rendered"))
    };

    stoat::action_handlers::walkthrough::next(&mut h.stoat, 1);
    let tall_rings = rings(&mut h);

    assert_eq!(
        (
            short_rings,
            scroll_row <= 59,
            65 < scroll_row + viewport,
            tall_rings
        ),
        (true, true, true, true),
        "the short focus lands whole and ringed, and the tall one rings what shows"
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
    // Every label takes a connector. On its own row the second label
    // overlaps the first, so it moves off that row and its line turns.
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

/// A connector is the line the layout planned, every point of it, since the
/// layout alone knows what lies between the code and the box.
#[test]
fn a_labels_connector_is_the_line_the_layout_planned() {
    let mut h = harness(&[(1, "one"), (3, "two")]);
    open(&mut h.stoat, "tour");
    reach(&mut h, 2);

    let emitted = sketches(&mut h);
    let input = stoat::render::walkthrough::measure(&mut h.stoat).expect("the pane measures");
    let planned = slide::layout(&input)
        .callouts
        .into_iter()
        .find(|callout| callout.key == 1)
        .and_then(|callout| callout.link)
        .expect("the label that moved has a connector");
    let link = emitted
        .iter()
        .find(|sketch| sketch.id == annotation_ids(&h, 1).0)
        .expect("the connector is emitted");

    assert_eq!(
        link.shape,
        stoat::render::walkthrough::link_shape(&planned, h.stoat.stoatty_protocol)
    );
}

/// A scroll glides the pane under the slide, so every mark, label, and card
/// run rides the pane's pool from the top row it was laid out at. A part
/// that rides nothing stays on the screen while the code under it moves.
#[test]
fn every_part_of_a_slide_rides_the_focused_pane() {
    let mut h = harness(&[(1, "one"), (3, "two")]);
    open(&mut h.stoat, "tour");
    reach(&mut h, 1);

    let emitted = frame(&mut h);
    let pane_top = {
        let ws = h.stoat.active_workspace();
        let pane = ws.panes.pane(ws.panes.focus());
        let (editor_id, _) = h.stoat.focused_editor_ids().expect("an editor has focus");
        (pane.index, ws.editors[editor_id].scroll_row as f32)
    };
    // The slide's marks take the stop's ids, and its label and card runs
    // follow one of them. The chrome around the pane takes neither.
    let stop = {
        let first = part_id(&h, part::FOCUS_MARK);
        first..first + STOP_ID_STRIDE
    };
    let anchors = |marks: bool| -> Vec<Option<(u32, f32)>> {
        emitted
            .iter()
            .filter_map(|command| match command {
                Command::Sketch(sketch) if marks && stop.contains(&sketch.id) => {
                    Some(sketch.anchor)
                },
                Command::TextRun(run) if !marks && stop.contains(&run.follow) => Some(run.anchor),
                _ => None,
            })
            .collect()
    };
    let (marks, runs) = (anchors(true), anchors(false));

    assert!(
        !marks.is_empty() && !runs.is_empty(),
        "the slide draws marks and runs, got {marks:?} and {runs:?}",
    );
    let riding = |count: usize| vec![Some(pane_top); count];
    assert_eq!(
        (&marks, &runs),
        (&riding(marks.len()), &riding(runs.len())),
        "every mark and text run of the slide rides the pane",
    );
}

/// The focus mark starts and ends on the glyphs of `fn two() {}`, and the
/// pane the marks clamp into starts where the code does, past the gutter.
#[test]
fn a_mark_encloses_the_code_it_names() {
    let mut h = harness(&[]);
    let (input, x) = painted_focus(&mut h);
    let focus = input.focus.expect("the focus is on screen");
    let content =
        stoat::render::walkthrough::focused_content_area(&h.stoat).expect("an editor has focus");
    let (editor_id, _) = h.stoat.focused_editor_ids().expect("focused editor");
    let gutter = h.stoat.active_workspace().editors[editor_id].gutter_width;

    assert!(gutter > 0, "the relative line numbers take a gutter");
    assert_eq!(x, content.x + gutter, "the code starts past the gutter");
    assert_eq!(
        (focus.start_x, focus.end_x, input.pane.x),
        (x, x + 10, x),
        "the mark covers columns 1 to 11, and the pane starts at the code",
    );
}

/// The diff view paints the buffer in its right column, beside the base's
/// copy of each line. The slide measures from the column the render
/// recorded, so a mark starts on the glyph it names, not on the base's copy.
#[test]
fn a_mark_in_the_diff_view_starts_at_the_right_columns_glyph() {
    let mut h = harness(&[]);
    // Wide enough for the two-column diff layout.
    h.resize(120, 24);
    open(&mut h.stoat, "tour");
    h.seed_current_diff_map("fn ONE() {}\nfn two() {}\nfn three() {}\n");
    let (editor_id, _) = h.stoat.focused_editor_ids().expect("an editor has focus");
    h.stoat.active_workspace_mut().editors[editor_id].set_diff_view(true);
    h.snapshot();

    let input = stoat::render::walkthrough::measure(&mut h.stoat).expect("the pane measures");
    let focus = input.focus.expect("the focus is on screen");
    let text = h.stoat.active_workspace().editors[editor_id]
        .text_rect
        .expect("the render records the text rect");
    let buf = h.rendered_buffer();
    let x = (text.x..buf.area.width)
        .find(|&x| buf[(x, focus.rows[0])].symbol() == "f")
        .expect("the focus line paints in the right column");

    assert_eq!(
        (focus.start_x, focus.end_x),
        (x, x + 10),
        "the mark covers columns 1 to 11 of the right column",
    );
}

/// A tour over the diff view leaves the view's hints box to the toggle, so
/// the toggle forces a box on there as it does over a plain editor.
#[test]
fn the_toggle_forces_the_box_on_over_a_toured_diff_view() {
    let mut h = harness(&[]);
    open(&mut h.stoat, "tour");
    let (editor_id, _) = h.stoat.focused_editor_ids().expect("an editor has focus");
    h.stoat.active_workspace_mut().editors[editor_id].set_diff_view(true);
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleKeyHints);

    assert_eq!(
        (h.stoat.key_hints_visible, h.stoat.screen_hints_hidden),
        (true, false),
    );
}

/// A line ends one cell past its last glyph, so a box placed at the end
/// sits beside the code rather than over it.
#[test]
fn a_line_end_lands_past_the_text() {
    let mut h = harness(&[]);
    let (input, x) = painted_focus(&mut h);
    let row = input.focus.as_ref().expect("the focus is on screen").rows[0];
    assert_eq!(
        input.line_ends.iter().find(|(at, _)| *at == row),
        Some(&(row, x + 11)),
    );
}

/// Open the tour and paint it, then return the measured input and the
/// column where the first glyph of the focus line paints.
fn painted_focus(h: &mut TestHarness) -> (slide::SlideInput, u16) {
    open(&mut h.stoat, "tour");
    h.snapshot();
    let input = stoat::render::walkthrough::measure(&mut h.stoat).expect("the pane measures");
    let row = input.focus.as_ref().expect("the focus is on screen").rows[0];
    let buf = h.rendered_buffer();
    let x = (0..buf.area.width)
        .find(|&x| buf[(x, row)].symbol() == "f")
        .expect("the focus line is painted");
    (input, x)
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
    let pane = stoat::render::walkthrough::measure(&mut h.stoat)
        .expect("the pane measures")
        .pane;
    let outside = (pane.x, pane.y + 2);
    let Color::Rgb(r, g, b) = h.rendered_buffer()[outside].fg else {
        panic!("the code is painted in RGB");
    };

    reach(&mut h, 1);
    h.snapshot();
    let range = stoat::render::walkthrough::measure(&mut h.stoat)
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
    let pane = stoat::render::walkthrough::measure(&mut h.stoat)
        .expect("the pane measures")
        .pane;
    let cell = (pane.x, pane.y + 2);
    let on_the_focus = h.rendered_buffer()[cell].fg;

    stoat::action_handlers::walkthrough::done(&mut h.stoat);
    h.snapshot();
    assert_eq!(
        h.rendered_buffer()[cell].fg,
        on_the_focus,
        "the code keeps its color",
    );
}

/// The pooled pages carry the spotlight the live grid paints and no other.
/// An annotation scrolled off the pane lights nothing on the grid, so its
/// pages stay unlit too. A lit page there flashes a dim through the glide
/// that the settled grid never shows.
#[test]
fn the_pooled_spotlight_is_the_one_the_grid_paints() {
    let filler: String = (4..=60).map(|n| format!("fn line_{n}() {{}}\n")).collect();
    let mut h = harness_over(&[(1, "one")], &format!("{CODE}{filler}"));
    open(&mut h.stoat, "tour");
    reach(&mut h, 1);
    h.stoat.render();

    let (editor_id, _) = h.stoat.focused_editor_ids().expect("an editor has focus");
    let marker = paint::style_rgb(h.stoat.theme.get(scope::UI_WALKTHROUGH_MARKERS[0]).fg)
        .expect("the theme names an RGB color");
    assert_eq!(
        stoat::render::walkthrough::spotlight_of(&mut h.stoat),
        Some((
            editor_id,
            Spotlight {
                range: 0..=10,
                color: marker,
                dim: SPOTLIGHT_DIM,
            },
        )),
        "an annotation on screen lights the pages as it lights the grid",
    );

    h.stoat.active_workspace_mut().editors[editor_id].scroll_row = 30;
    h.stoat.render();
    assert_eq!(
        stoat::render::walkthrough::spotlight_of(&mut h.stoat),
        None,
        "and scrolled off the pane it lights neither",
    );
}

/// A step between two annotations takes the spotlight off one page and puts
/// it on another, and changes nothing else a page paints. The emit that moves
/// the target refills those two pages and keeps the rest the terminal holds.
///
/// Absolute numbers keep the cursor's landing out of the gutter, so the
/// spotlight is the only change.
#[test]
fn stepping_between_annotations_refills_only_the_pages_they_light() {
    let filler: String = (4..=400).map(|n| format!("fn line_{n}() {{}}\n")).collect();
    // Line 2 lies on page 0, and line 51 (row 50) on page 2 of any page 17
    // to 25 rows tall.
    let mut h = harness_over(&[(2, "near"), (51, "far")], &format!("{CODE}{filler}"));
    h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
    h.stoat.settings.editor_line_numbers = Some(LineNumbers::Absolute);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    open(&mut h.stoat, "tour");
    // Stepped by key, so the view follows the cursor to each annotation the
    // way it does for a reader.
    h.type_keys("space W");
    h.type_keys("a");
    h.stoat.render();
    apc_emit::emit_smooth_scroll(&mut h.stoat);
    h.settle();
    let height = drain_apc(&mut rx)
        .into_iter()
        .find_map(|command| match command {
            Command::PoolRegion(PoolRegionCommand { pool, height, .. })
                if pool < NON_PANE_POOL_BASE =>
            {
                Some(u32::from(height))
            },
            _ => None,
        })
        .expect("the editor declares its pool");
    assert!(
        (2 * height..3 * height).contains(&50),
        "row 50 lies on page 2 of a {height}-row page",
    );

    // The step's first emit holds, since its glide has not moved the target
    // yet, and a held emit compares no page.
    h.type_keys("a");
    h.stoat.render();
    apc_emit::emit_smooth_scroll(&mut h.stoat);
    h.settle();
    let _ = drain_apc(&mut rx);

    app::tick_animation(&mut h.stoat, 0.016);
    apc_emit::emit_smooth_scroll(&mut h.stoat);
    h.settle();
    let filled: Vec<u64> = drain_apc(&mut rx)
        .into_iter()
        .filter_map(|command| match command {
            Command::Fill(FillCommand { pool, index, .. }) if pool < NON_PANE_POOL_BASE => {
                Some(index)
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        filled,
        vec![0, 2],
        "only the pages the two annotations light refill",
    );
}

/// A paint that changes nothing in the stop reuses the layout of the paint
/// before it, and the paint after a step lays the stop out again.
#[test]
fn a_quiet_paint_lays_the_stop_out_once() {
    let mut h = harness(&[(1, "one"), (3, "three")]);
    open(&mut h.stoat, "tour");
    let layouts = |h: &TestHarness| {
        h.stoat
            .active_workspace()
            .walkthrough
            .as_ref()
            .expect("a tour plays")
            .layouts
    };
    // The pinned card settles into the size the next paint measures.
    frame(&mut h);
    frame(&mut h);

    let settled = layouts(&h);
    frame(&mut h);
    frame(&mut h);
    let quiet = layouts(&h);
    reach(&mut h, 1);
    frame(&mut h);
    assert_eq!(
        (quiet - settled, layouts(&h) - quiet),
        (0, 1),
        "(layouts over two quiet paints, layouts over the paint after a step)",
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
    let input = stoat::render::walkthrough::measure(stoat).expect("the pane measures");
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

    stoat::action_handlers::walkthrough::next(&mut h.stoat, 1);
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

    stoat::action_handlers::walkthrough::next(&mut h.stoat, 1);
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

    stoat::action_handlers::walkthrough::next(&mut h.stoat, 1);
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

    stoat::action_handlers::walkthrough::next_annotation(&mut h.stoat, 1);
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

    stoat::action_handlers::walkthrough::prev_annotation(&mut h.stoat, 1);
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
    stoat::action_handlers::walkthrough::prev_annotation(&mut h.stoat, 1);
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

    stoat::action_handlers::walkthrough::next(&mut h.stoat, 1);
    let emitted = frame(&mut h);

    let exiting: Vec<u32> = emitted
        .iter()
        .filter_map(|command| match command {
            Command::Sketch(sketch) if sketch.timing.phase == SketchPhase::Exit => Some(sketch.id),
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

/// A count typed in the walkthrough mode steps that many stops, and a count
/// past either end lands on the stop at that end.
#[test]
fn a_count_steps_that_many_stops_and_holds_at_the_ends() {
    let mut h = harness_with_stops(&[], CODE, &[location(1, (1, 11), "fn one() {}")]);
    open(&mut h.stoat, "tour");
    h.type_keys("space W");
    let progress = |h: &TestHarness| {
        h.stoat
            .active_workspace()
            .walkthrough
            .as_ref()
            .expect("a tour is playing")
            .progress()
    };

    let mut walked = vec![progress(&h)];
    for keys in ["2 n", "9 p"] {
        h.type_keys(keys);
        walked.push(progress(&h));
    }

    assert_eq!(
        (walked, h.stoat.pending_count, h.stoat.focused_mode()),
        (vec![(1, 3), (3, 3), (1, 3)], None, "walkthrough"),
        "two stops on from the first, then nine back holds on the first",
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
    stoat::action_handlers::walkthrough::next(&mut h.stoat, 1);
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
    // The card's last row, which its own body does not reach.
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
