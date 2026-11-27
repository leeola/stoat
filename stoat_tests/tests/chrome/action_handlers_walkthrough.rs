use codegraph::{Confidence, Edge, EdgeKind, FileId, FileShard, Symbol, SymbolKey, Target};
use std::{
    ops::Range as ByteRange,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{
    action_handlers,
    action_handlers::walkthrough::{
        backward, done, forward, next, next_annotation, open, prev, prev_annotation,
        show_description, show_narration_again, CARD_MAX_WIDTH, CARD_MIN_WIDTH,
    },
    app::Stoat,
    badge::BadgeSource,
    code_index::{build, nav},
    git_jobs,
    host::FakeFs,
    keymap::{KeymapState, StateValue},
    keymap_state::StoatKeymapState,
    render::hover::HoverFrame,
    review_walk::ReturnRef,
    test_harness::{draw_type_at_full_cell, TestHarness},
    walkthrough::{Location, Point, Range, Walkthrough},
    workspace::diff::DiffBase,
};
use stoat_config::Settings;
use stoat_language::SymbolKind;
use stoat_scheduler::TestScheduler;

const FIRST: &str = "fn one() {}\nfn two() {}\n";

const SECOND: &str = "fn three() {}\n";

/// [`FIRST`] with `fn two` renamed, the working-tree edit a diff tour
/// explains.
const RENAMED: &str = "fn one() {}\nfn deux() {}\n";

/// Stop 1's narration. Stop 2 has none, so one tour covers both cases.
const NARRATION: &str = "The **entry** point.";

/// Annotation a1's narration. a2 has none, so one walk covers both cases.
const ANNOTATION_NARRATION: &str = "Where the call **lands**.";

/// The description of the tour in [`harness_with_tour`]. The tour in
/// [`stoat_with_tour`] has none, so the two cover both cases.
const DESCRIPTION: &str = "From the **entry** to the exit.";

fn range_of(line: u32, cols: (u32, u32)) -> Range {
    Range {
        start: Point { line, col: cols.0 },
        end: Point { line, col: cols.1 },
    }
}

fn location(path: &str, line: u32, cols: (u32, u32), snippet: &str) -> Location {
    Location {
        path: PathBuf::from(path),
        range: range_of(line, cols),
        snippet: snippet.to_owned(),
    }
}

/// A workspace holding a two-stop tour over `a.rs` and `b.rs`, with `first`
/// as the content of `a.rs`, which is how a test puts a stop out of date.
fn stoat_with_tour(first: &str) -> Stoat {
    draw_type_at_full_cell();
    let scheduler = Arc::new(TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;

    let mut walkthrough = Walkthrough::new("tour".to_owned(), "Tour".to_owned(), None);
    walkthrough
        .add_stop(
            Some("first".to_owned()),
            NARRATION.to_owned(),
            location("a.rs", 2, (1, 11), "fn two() {}"),
            None,
            None,
        )
        .expect("append");
    walkthrough
        .add_stop(
            Some("second".to_owned()),
            String::new(),
            location("b.rs", 1, (1, 13), "fn three() {}"),
            None,
            None,
        )
        .expect("append");

    // Stop 1 calls out the callee it reaches in b.rs, then a neighbor in
    // its own file, so one walk covers a cross-file hop and a same-file one.
    // Only the callee narrates, so the walk covers both card branches too.
    walkthrough
        .add_annotation(
            "s1",
            Some(PathBuf::from("b.rs")),
            range_of(1, (1, 13)),
            "fn three() {}".to_owned(),
            "the callee".to_owned(),
            ANNOTATION_NARRATION.to_owned(),
        )
        .expect("s1 exists");
    walkthrough
        .add_annotation(
            "s1",
            None,
            range_of(1, (1, 11)),
            "fn one() {}".to_owned(),
            "the neighbor".to_owned(),
            String::new(),
        )
        .expect("s1 exists");

    let fs = Arc::new(FakeFs::new());
    fs.insert_file("/repo/a.rs", first);
    fs.insert_file("/repo/b.rs", SECOND);
    fs.insert_file(
        "/repo/.stoat/walkthroughs/tour.json",
        serde_json::to_string(&walkthrough).expect("serialize"),
    );
    stoat.set_fs_host(fs);
    stoat
}

/// Where the cursor sits, as `(file, offset)`.
fn cursor(stoat: &mut Stoat) -> (String, usize) {
    let (buffer_id, offset) = {
        let editor = action_handlers::focused_editor_mut(stoat).expect("a focused editor");
        let snapshot = editor.display_map.snapshot();
        let buffer = snapshot.buffer_snapshot();

        let selection = editor.selections.newest_anchor();
        let tail = buffer.resolve_anchor(&selection.tail());
        let head = buffer.resolve_anchor(&selection.head());

        (
            editor.buffer_id,
            stoat_text::cursor_offset(buffer.rope(), tail, head),
        )
    };

    let path = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    (path, offset)
}

/// The two symbols the tour's stops focus on, `two` in `a.rs` calling
/// `three` in `b.rs`.
fn indexed_symbols() -> [(u8, FileId, &'static str, ByteRange<usize>); 2] {
    [
        (1, build::file_id("a.rs"), "two", 12..23),
        (2, build::file_id("b.rs"), "three", 0..13),
    ]
}

/// Index both stops' symbols, with the call edge between them only when
/// `calls` is set, which is how a test picks a related or unrelated pair.
fn index_the_tour(stoat: &mut Stoat, calls: bool) {
    let symbols = indexed_symbols();
    let keys: Vec<SymbolKey> = symbols
        .iter()
        .map(|(id, ..)| SymbolKey([*id; 16]))
        .collect();

    for (index, (id, file, name, def_range)) in symbols.into_iter().enumerate() {
        let edges = match calls && index == 0 {
            true => vec![Edge {
                from: keys[0],
                to: Target::Sym(keys[1]),
                kind: EdgeKind::Calls,
                site_range: def_range.clone(),
                confidence: Confidence::Resolved,
            }],
            false => Vec::new(),
        };

        stoat
            .active_workspace_mut()
            .code_graph
            .insert_shard(FileShard {
                content_hash: [0u8; 32],
                symbols: vec![Symbol {
                    key: SymbolKey([id; 16]),
                    file,
                    name: name.to_owned(),
                    kind: SymbolKind::Function,
                    container: vec![],
                    def_range,
                    name_range: 0..1,
                    body_hash: [0u8; 32],
                }],
                edges,
            });
    }
}

/// Where the active trail sits and how long it is, or `None` for no trail.
fn trail_progress(stoat: &Stoat) -> Option<(usize, usize)> {
    stoat
        .active_workspace()
        .trail
        .as_ref()
        .and_then(|trail| trail.progress())
}

/// The popup's text, one string per line, with the styling dropped.
fn popup_lines(stoat: &Stoat) -> Vec<String> {
    stoat
        .pending_hover
        .as_ref()
        .map(|popup| {
            popup
                .lines
                .iter()
                .map(|line| {
                    line.iter()
                        .map(|(text, _)| text.as_str())
                        .collect::<String>()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn open_lands_on_the_first_stops_focus() {
    let mut stoat = stoat_with_tour(FIRST);
    open(&mut stoat, "tour");

    assert_eq!(
        cursor(&mut stoat),
        ("/repo/a.rs".to_owned(), 12),
        "stop 1 focuses line 2 of a.rs, which starts at byte 12",
    );
    assert_eq!(stoat.pending_message.as_deref(), Some("1/2: first"));
}

#[test]
fn stepping_walks_the_stops_and_clamps() {
    let mut stoat = stoat_with_tour(FIRST);
    open(&mut stoat, "tour");

    next(&mut stoat, 1);
    assert_eq!(cursor(&mut stoat), ("/repo/b.rs".to_owned(), 0));
    assert_eq!(stoat.pending_message.as_deref(), Some("2/2: second"));

    next(&mut stoat, 1);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("already on the last stop"),
        "there is nothing past the end to jump to",
    );

    prev(&mut stoat, 1);
    assert_eq!(cursor(&mut stoat), ("/repo/a.rs".to_owned(), 12));

    prev(&mut stoat, 1);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("already on the first stop")
    );
}

#[test]
fn arriving_shows_the_narration_under_a_title_line() {
    let mut stoat = stoat_with_tour(FIRST);
    open(&mut stoat, "tour");

    assert_eq!(
        popup_lines(&stoat),
        ["first - 1/2", "The entry point."],
        "the title line names the stop and where it sits, then the markdown",
    );

    let popup = stoat.pending_hover.as_ref().expect("a popup");
    assert_eq!(
        popup.anchor_offset, 12,
        "the popup sits at the focus the cursor landed on",
    );
}

/// Every popup on screen describes the stop the reader is on, so a stop
/// with nothing to say takes the previous one's down.
#[test]
fn a_stop_with_no_narration_shows_no_popup() {
    let mut stoat = stoat_with_tour(FIRST);
    open(&mut stoat, "tour");
    next(&mut stoat, 1);

    assert!(stoat.pending_hover.is_none());
}

/// A hover goes on the next key press, which is what makes it a hover. A
/// narration card is the point of the screen while a tour plays, so an
/// incidental key leaves it up.
#[test]
fn an_ordinary_key_leaves_the_card_up() {
    let mut h = harness_with_tour();
    open(&mut h.stoat, "tour");
    assert!(
        h.stoat
            .pending_hover
            .as_ref()
            .is_some_and(|popup| popup.pinned),
        "the card is pinned",
    );

    h.type_keys("j");
    assert_eq!(
        popup_lines(&h.stoat),
        ["first - 1/2", "The entry point."],
        "and a movement key does not take it down",
    );
}

/// A card that cannot be dismissed is worse than one that vanishes, so the
/// deliberate dismissals go on working.
#[test]
fn esc_takes_the_card_down() {
    let mut h = harness_with_tour();
    open(&mut h.stoat, "tour");

    h.type_keys("escape");
    assert!(h.stoat.pending_hover.is_none());
}

/// The card stays until it is dismissed, so one key both hides it when it
/// is in the way and brings it back.
/// Walkthrough mode binds Esc to an action, so it never reaches the plain
/// key branch while that mode is active. Without the action path knowing,
/// the card survives the one key meant to take it down.
#[test]
fn esc_takes_the_card_down_from_inside_walkthrough_mode() {
    let mut h = harness_with_tour();
    open(&mut h.stoat, "tour");

    h.type_keys("space W");
    assert_eq!(h.stoat.focused_mode(), "walkthrough", "the mode is entered");
    assert!(
        h.stoat.pending_hover.is_some(),
        "and the card survived getting there",
    );

    h.type_keys("escape");
    assert!(h.stoat.pending_hover.is_none(), "Esc takes it down");
}

#[test]
fn asking_again_toggles_the_card() {
    let mut stoat = stoat_with_tour(FIRST);
    open(&mut stoat, "tour");

    show_narration_again(&mut stoat);
    assert!(stoat.pending_hover.is_none(), "a card that is up goes down");

    show_narration_again(&mut stoat);
    assert_eq!(
        popup_lines(&stoat),
        ["first - 1/2", "The entry point."],
        "and asking once more brings it back",
    );
}

/// A plain terminal draws no marks, so the card keeps the modal border and
/// the narration reads the same there.
#[test]
fn a_plain_terminal_keeps_the_modal_frame() {
    let mut stoat = stoat_with_tour(FIRST);
    open(&mut stoat, "tour");

    assert_eq!(
        stoat.pending_hover.as_ref().map(|popup| popup.frame),
        Some(HoverFrame::Modal),
        "the harness leaves the protocol at zero, which is the older-terminal case",
    );
}

/// The card goes where the layout puts it rather than beside the cursor,
/// which is what keeps it clear of the code the stop is about.
#[test]
fn the_card_carries_its_own_placement() {
    let mut h = harness_with_tour();
    open(&mut h.stoat, "tour");

    let frame = h.stoat.size();
    let popup = h.stoat.pending_hover.as_ref().expect("a popup");
    let placement = popup.placement.expect("the card places itself");

    // The floor wins on a narrow frame, so the ceiling is stated the way
    // the sizing states it rather than as a bare half-frame bound.
    let ceiling = CARD_MAX_WIDTH.min((frame.width / 2).max(CARD_MIN_WIDTH));
    assert!(
        (CARD_MIN_WIDTH..=ceiling).contains(&placement.width),
        "the card is between {CARD_MIN_WIDTH} and {ceiling}, got {placement:?}",
    );
    assert!(
        placement.x + placement.width <= frame.x + frame.width,
        "and stays inside the frame, got {placement:?} in {frame:?}",
    );
}

#[test]
fn the_narration_shows_again_after_a_dismissal() {
    let mut stoat = stoat_with_tour(FIRST);
    open(&mut stoat, "tour");
    stoat.pending_hover = None;

    show_narration_again(&mut stoat);
    assert_eq!(popup_lines(&stoat), ["first - 1/2", "The entry point."]);

    next(&mut stoat, 1);
    show_narration_again(&mut stoat);
    assert!(stoat.pending_hover.is_none());
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("this stop has no narration"),
        "a stop with nothing to re-show says so",
    );
}

/// The reader asked for a different card than the narration on screen, so
/// the description replaces it rather than taking it down.
#[test]
fn the_description_card_names_the_tour() {
    let mut h = harness_with_tour();
    open(&mut h.stoat, "tour");

    show_description(&mut h.stoat);
    assert_eq!(
        popup_lines(&h.stoat),
        ["Tour", "From the entry to the exit."],
    );
}

#[test]
fn a_tour_with_no_description_says_so() {
    let mut stoat = stoat_with_tour(FIRST);
    open(&mut stoat, "tour");

    show_description(&mut stoat);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("walkthrough 'tour' has no description"),
    );
    assert_eq!(
        popup_lines(&stoat),
        ["first - 1/2", "The entry point."],
        "the narration card stays up",
    );
}

/// Two stops inside one definition resolve to the same symbol, so the path
/// between them is that symbol alone. That is ordinary rather than an
/// edge, and "1 stops" tells the reader the count is generated and leaves
/// them wondering what it miscounted.
#[test]
fn a_trail_within_one_definition_reads_as_one_stop() {
    draw_type_at_full_cell();
    let scheduler = Arc::new(TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;

    // Both stops sit inside `fn two`, which is the whole point: they
    // resolve to one symbol and the walk between them has one stop.
    let mut walkthrough = Walkthrough::new("tour".to_owned(), "Tour".to_owned(), None);
    for (id, cols, snippet) in [("first", (1, 6), "fn two"), ("second", (7, 11), "() {}")] {
        walkthrough
            .add_stop(
                Some(id.to_owned()),
                String::new(),
                location("a.rs", 2, cols, snippet),
                None,
                None,
            )
            .expect("append");
    }

    let fs = Arc::new(FakeFs::new());
    fs.insert_file("/repo/a.rs", FIRST);
    fs.insert_file(
        "/repo/.stoat/walkthroughs/tour.json",
        serde_json::to_string(&walkthrough).expect("serialize"),
    );
    stoat.set_fs_host(fs);

    stoat
        .active_workspace_mut()
        .code_graph
        .insert_shard(FileShard {
            content_hash: [0u8; 32],
            symbols: vec![Symbol {
                key: SymbolKey([1u8; 16]),
                file: build::file_id("a.rs"),
                name: "two".to_owned(),
                kind: SymbolKind::Function,
                container: vec![],
                def_range: 12..23,
                name_range: 15..18,
                body_hash: [0u8; 32],
            }],
            edges: Vec::new(),
        });
    stoat
        .active_workspace_mut()
        .file_paths
        .insert(build::file_id("a.rs"), PathBuf::from("a.rs"));

    open(&mut stoat, "tour");
    next(&mut stoat, 1);

    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("2/2: second (trail: 1 stop)"),
        "one stop is one stop",
    );
}

#[test]
fn stepping_between_related_stops_lays_the_trail() {
    let mut stoat = stoat_with_tour(FIRST);
    index_the_tour(&mut stoat, true);
    open(&mut stoat, "tour");

    assert_eq!(trail_progress(&stoat), None, "the first stop follows none");

    next(&mut stoat, 1);
    assert_eq!(
        trail_progress(&stoat),
        Some((1, 2)),
        "the trail runs from the caller to the callee, sitting on the first",
    );
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("2/2: second (trail: 2 stops)"),
        "the stop the reader landed on comes first, then what connects it",
    );
}

/// A trail between the last pair of stops says nothing true about this
/// pair, so an unrelated step takes it down rather than leaving it up.
#[test]
fn stepping_between_unrelated_stops_clears_the_trail() {
    let mut stoat = stoat_with_tour(FIRST);
    index_the_tour(&mut stoat, false);
    open(&mut stoat, "tour");
    nav::install_trail(&mut stoat, &[SymbolKey([9u8; 16]), SymbolKey([8u8; 16])]);

    next(&mut stoat, 1);
    assert_eq!(trail_progress(&stoat), None);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("2/2: second"),
        "with no trail to report, the stop status stands alone",
    );
}

#[test]
fn annotation_stepping_walks_out_from_the_focus_and_back() {
    let mut stoat = stoat_with_tour(FIRST);
    index_the_tour(&mut stoat, true);
    open(&mut stoat, "tour");

    prev_annotation(&mut stoat, 1);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("already on the stop"),
        "the focus heads the walk, so nothing precedes it",
    );

    next_annotation(&mut stoat, 1);
    assert_eq!(
        cursor(&mut stoat),
        ("/repo/b.rs".to_owned(), 0),
        "the first annotation names a file of its own",
    );
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("a1 1/2: the callee (trail: 2 stops)"),
        "the cross-file hop lays the trail between the two symbols",
    );
    assert_eq!(trail_progress(&stoat), Some((1, 2)));

    next_annotation(&mut stoat, 1);
    assert_eq!(cursor(&mut stoat), ("/repo/a.rs".to_owned(), 0));
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("a2 2/2: the neighbor"),
        "an unindexed end lays no trail, so the status stands alone",
    );
    assert_eq!(trail_progress(&stoat), None);

    next_annotation(&mut stoat, 1);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("already on the last annotation")
    );

    prev_annotation(&mut stoat, 1);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("a1 1/2: the callee"),
        "stepping back off an unindexed annotation lays no trail either",
    );

    prev_annotation(&mut stoat, 1);
    assert_eq!(cursor(&mut stoat), ("/repo/a.rs".to_owned(), 12));
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("1/2: first (trail: 2 stops)"),
        "back past the first annotation reads as the stop, still related",
    );
}

/// An annotation is a sub-step with documentation of its own, so the card
/// follows the reader down into it and back up when it has nothing to add.
#[test]
fn a_narrated_annotation_takes_the_card_and_an_unnarrated_one_leaves_it() {
    let mut stoat = stoat_with_tour(FIRST);
    open(&mut stoat, "tour");

    next_annotation(&mut stoat, 1);
    assert_eq!(
        popup_lines(&stoat),
        ["the callee - 1/2", "Where the call lands."],
        "the annotation speaks for itself, under its own heading",
    );

    next_annotation(&mut stoat, 1);
    assert_eq!(
        popup_lines(&stoat),
        ["first - 1/2", "The entry point."],
        "an annotation with nothing to add leaves the stop's card up",
    );
}

#[test]
fn asking_again_on_an_annotation_brings_its_own_card_back() {
    let mut stoat = stoat_with_tour(FIRST);
    open(&mut stoat, "tour");
    next_annotation(&mut stoat, 1);

    show_narration_again(&mut stoat);
    assert!(stoat.pending_hover.is_none(), "a card that is up goes down");

    show_narration_again(&mut stoat);
    assert_eq!(
        popup_lines(&stoat),
        ["the callee - 1/2", "Where the call lands."],
        "and asking once more brings the annotation's own card back",
    );
}

/// One gesture reads the tour as a single sequence, so it carries the
/// reader off a focus, through the stop's annotations, and on to the next.
#[test]
fn the_linear_walk_runs_a_focus_its_annotations_then_the_next_stop() {
    let mut stoat = stoat_with_tour(FIRST);
    index_the_tour(&mut stoat, true);
    open(&mut stoat, "tour");

    backward(&mut stoat, 1);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("already at the start of the tour"),
    );

    forward(&mut stoat, 1);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("a1 1/2: the callee (trail: 2 stops)"),
        "the first point past a focus is the stop's first annotation",
    );

    forward(&mut stoat, 1);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("a2 2/2: the neighbor"),
    );

    forward(&mut stoat, 1);
    assert_eq!(cursor(&mut stoat), ("/repo/b.rs".to_owned(), 0));
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("2/2: second"),
        "past the last annotation the walk reaches the next stop's focus",
    );

    forward(&mut stoat, 1);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("already at the end of the tour"),
    );
}

/// Backward inverts forward exactly, so a reader who overshoots gets the
/// point they came from rather than the top of the stop they left.
#[test]
fn the_linear_walk_back_lands_on_the_previous_stops_last_annotation() {
    let mut stoat = stoat_with_tour(FIRST);
    index_the_tour(&mut stoat, true);
    open(&mut stoat, "tour");
    next(&mut stoat, 1);

    backward(&mut stoat, 1);
    assert_eq!(cursor(&mut stoat), ("/repo/a.rs".to_owned(), 0));
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("a2 2/2: the neighbor"),
    );
}

/// The annotations belong to the stop, so leaving it leaves them.
#[test]
fn a_stop_step_returns_to_the_focus() {
    let mut stoat = stoat_with_tour(FIRST);
    index_the_tour(&mut stoat, true);
    open(&mut stoat, "tour");
    next_annotation(&mut stoat, 1);

    next(&mut stoat, 1);
    prev(&mut stoat, 1);
    assert_eq!(cursor(&mut stoat), ("/repo/a.rs".to_owned(), 12));
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("1/2: first (trail: 2 stops)"),
        "the stop reads as an arrival, not as the annotation it left from",
    );
}

#[test]
fn a_stop_with_no_annotations_says_so() {
    let mut stoat = stoat_with_tour(FIRST);
    open(&mut stoat, "tour");
    next(&mut stoat, 1);

    next_annotation(&mut stoat, 1);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("this stop has no annotations")
    );
}

#[test]
fn done_ends_the_walkthrough() {
    let mut stoat = stoat_with_tour(FIRST);
    index_the_tour(&mut stoat, true);
    open(&mut stoat, "tour");
    next(&mut stoat, 1);
    done(&mut stoat);

    assert!(stoat.active_workspace().walkthrough.is_none());
    assert_eq!(
        trail_progress(&stoat),
        None,
        "the trail belonged to the tour that just ended",
    );
    assert_eq!(stoat.pending_message.as_deref(), Some("walkthrough closed"));

    next(&mut stoat, 1);
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("no walkthrough is playing"),
        "the actions say so rather than doing nothing at all",
    );
}

#[test]
fn an_unknown_slug_installs_nothing() {
    let mut stoat = stoat_with_tour(FIRST);
    open(&mut stoat, "missing");

    assert!(stoat.active_workspace().walkthrough.is_none());
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("no walkthrough 'missing'")
    );
}

/// The stop that drifted is the one most worth looking at, so the jump
/// still happens and the report rides along with it.
#[test]
fn a_drifted_stop_still_jumps() {
    let mut stoat = stoat_with_tour("fn one() {}\nfn TWO() {}\n");
    open(&mut stoat, "tour");

    assert_eq!(cursor(&mut stoat), ("/repo/a.rs".to_owned(), 12));
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("stop s1 drifted from its capture")
    );
}

/// The tour of [`stoat_with_tour`] over a [`TestHarness`], so a test can
/// reach it the way a reader does, through the keymap rather than by
/// calling the handler. Both stops narrate here, because the popup is what
/// these tests watch.
fn harness_with_tour() -> TestHarness {
    let mut h = Stoat::test();
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");

    let mut walkthrough = Walkthrough::new("tour".to_owned(), "Tour".to_owned(), None);
    walkthrough.description = DESCRIPTION.to_owned();
    walkthrough
        .add_stop(
            Some("first".to_owned()),
            NARRATION.to_owned(),
            location("a.rs", 2, (1, 11), "fn two() {}"),
            None,
            None,
        )
        .expect("append");
    walkthrough
        .add_stop(
            Some("second".to_owned()),
            "The **exit**.".to_owned(),
            location("b.rs", 1, (1, 13), "fn three() {}"),
            None,
            None,
        )
        .expect("append");
    // Only stop 1 carries an annotation, so one tour reaches both annotation
    // branches. A step onto it clamps there, and stop 2 has none to step onto.
    walkthrough
        .add_annotation(
            "s1",
            None,
            range_of(1, (1, 11)),
            "fn one() {}".to_owned(),
            "the neighbor".to_owned(),
            String::new(),
        )
        .expect("s1 exists");

    h.fake_fs().insert_file("/repo/a.rs", FIRST);
    h.fake_fs().insert_file("/repo/b.rs", SECOND);
    h.fake_fs().insert_file(
        "/repo/.stoat/walkthroughs/tour.json",
        serde_json::to_string(&walkthrough).expect("serialize"),
    );
    h
}

#[test]
fn walkthrough_reads_whether_a_tour_is_open() {
    let mut h = harness_with_tour();
    let open_tour = |h: &TestHarness| {
        StoatKeymapState::from_stoat(&h.stoat)
            .get("walkthrough")
            .cloned()
    };
    let before = open_tour(&h);

    open(&mut h.stoat, "tour");
    let during = open_tour(&h);

    done(&mut h.stoat);
    assert_eq!(
        [before, during, open_tour(&h)],
        [false, true, false].map(|on| Some(StateValue::Bool(on))),
        "no tour, then the tour open, then the tour done",
    );
}

#[test]
fn the_space_box_offers_the_picker_without_a_tour_and_the_mode_with_one() {
    let mut h = harness_with_tour();
    let w_row = |h: &mut TestHarness| {
        h.type_keys("space");
        h.render_composited();
        let action = h
            .stoat
            .hints_cache
            .as_ref()
            .expect("the space chord laid out its box")
            .rows
            .iter()
            .find(|row| row.keys == "W")
            .map(|row| row.action.clone());
        h.type_keys("escape");
        action
    };
    let before = w_row(&mut h);

    open(&mut h.stoat, "tour");
    assert_eq!(
        [before, w_row(&mut h)],
        [Some("walkthroughs"), Some("+walkthrough")].map(|action| action.map(str::to_owned)),
        "W picks a tour with none open, and enters the mode with one",
    );
}

/// The walkthrough box lists only the mode's own keys, and not the Escape
/// that leaves the mode, so it covers little of the code the reader reads
/// through the tour.
#[test]
fn the_walkthrough_box_lists_the_modes_own_keys() {
    let mut h = harness_with_tour();
    open(&mut h.stoat, "tour");
    h.type_keys("space W");
    h.render_composited();

    let rows: Vec<(String, String)> = h
        .stoat
        .hints_cache
        .as_ref()
        .expect("the walkthrough mode laid out its box")
        .rows
        .iter()
        .map(|row| (row.keys.clone(), row.action.clone()))
        .collect();
    let expected = [
        ("n, Dn", "go to the next walkthrough stop"),
        ("p, Up", "go to the previous walkthrough stop"),
        ("a", "go to the next annotation of this stop"),
        ("A", "go to the previous annotation of this stop"),
        ("s", "show the narration for this point again"),
        ("S", "show the walkthrough's description"),
        ("d", "end the walkthrough"),
        ("o", "open the walkthrough picker"),
        ("Rt", "go to the next point of the walkthrough"),
        ("Lt", "go to the previous point of the walkthrough"),
    ]
    .map(|(keys, action)| (keys.to_owned(), action.to_owned()));
    assert_eq!(
        rows, expected,
        "the box names the tour keys and nothing else"
    );
}

/// The picker opens over the tour that plays, so the reader swaps tours
/// without closing the open one first.
#[test]
fn o_in_the_mode_opens_the_picker_over_the_playing_tour() {
    let mut h = harness_with_tour();
    open(&mut h.stoat, "tour");

    h.type_keys("space W o");
    assert_eq!(
        (
            h.stoat
                .walkthrough_picker
                .as_ref()
                .map(|picker| picker.playing_idx()),
            h.stoat.active_workspace().walkthrough.is_some(),
        ),
        (Some(Some(0)), true),
    );
}

/// The narration shares the hover popup, which any key takes down, and the
/// chord that raises it is itself a key press. A popup this dispatch put up
/// is the one case that clear must leave alone, or no reader ever sees it.
#[test]
fn the_chord_that_raises_the_narration_leaves_it_up() {
    let mut h = harness_with_tour();
    open(&mut h.stoat, "tour");
    h.stoat.pending_hover = None;

    h.type_keys("space W s");

    assert_eq!(popup_lines(&h.stoat), ["first - 1/2", "The entry point."]);
}

#[test]
fn stepping_by_chord_shows_the_stop_it_lands_on() {
    let mut h = harness_with_tour();
    open(&mut h.stoat, "tour");

    h.type_keys("space W n");

    assert_eq!(popup_lines(&h.stoat), ["second - 2/2", "The exit."]);
}

/// A step off the end moves nobody, so nothing raises a popup and the
/// reader would lose the narration of the stop they are still on.
#[test]
fn a_clamped_step_keeps_the_narration_of_the_stop_it_stays_on() {
    let mut h = harness_with_tour();
    open(&mut h.stoat, "tour");
    h.type_keys("space W n");

    h.type_keys("space W n");

    assert_eq!(popup_lines(&h.stoat), ["second - 2/2", "The exit."]);
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("already on the last stop"),
    );
}

/// The same hold covers the annotation walk, whose two no-move branches
/// leave the reader on a stop whose narration they still need.
#[test]
fn a_no_move_annotation_step_keeps_the_narration_up() {
    for (keys, status, popup) in [
        (
            "space W a space W a",
            "already on the last annotation",
            ["first - 1/2", "The entry point."],
        ),
        (
            "space W n space W a",
            "this stop has no annotations",
            ["second - 2/2", "The exit."],
        ),
    ] {
        let mut h = harness_with_tour();
        open(&mut h.stoat, "tour");

        h.type_keys(keys);

        assert_eq!(
            popup_lines(&h.stoat),
            popup,
            "{keys} leaves the reader on a narrated stop",
        );
        assert_eq!(h.stoat.pending_message.as_deref(), Some(status));
    }
}

/// A tour is read a step at a time, so the mode has to survive each step
/// for the next one to take a bare key.
#[test]
fn the_walkthrough_mode_holds_until_escape() {
    let mut h = harness_with_tour();
    open(&mut h.stoat, "tour");

    h.type_keys("space W n");
    assert_eq!(h.stoat.focused_mode(), "walkthrough");
    assert_eq!(popup_lines(&h.stoat), ["second - 2/2", "The exit."]);

    h.type_keys("S");
    assert_eq!(
        popup_lines(&h.stoat),
        ["Tour", "From the entry to the exit."],
        "a bare S raises the tour's description",
    );

    h.type_keys("p");
    assert_eq!(
        popup_lines(&h.stoat),
        ["first - 1/2", "The entry point."],
        "a bare p walks back, taking no space W prefix",
    );

    h.type_keys("escape");
    assert_eq!(h.stoat.focused_mode(), "normal");
    assert!(
        h.stoat.active_workspace().walkthrough.is_some(),
        "escape leaves the mode, not the tour, so space W returns to it",
    );
}

/// A repository of two commits with a three-stop tour over them. `s1` reads
/// `a1b2c3d4` over `a.rs` line 2, and `s2` and `s3` read `b2c3d4e5` over
/// `b.rs` line 1 and `a.rs` line 1.
///
/// The fake host moves HEAD on a checkout without writing the tree, so the
/// tree is seeded once at the tip, where every stop reads what it captured.
fn commit_tour_harness() -> TestHarness {
    let mut h = Stoat::test();
    h.seed_linear_history(
        "/repo",
        &[
            ("a1b2c3d4", "feat: add a.rs", &[("a.rs", FIRST)]),
            (
                "b2c3d4e5",
                "feat: add b.rs",
                &[("a.rs", FIRST), ("b.rs", SECOND)],
            ),
        ],
    );
    h.fake_git()
        .add_repo("/repo")
        .branch("main", "b2c3d4e5")
        .set_head_branch("main");
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_fs().insert_file("/repo/a.rs", FIRST);
    h.fake_fs().insert_file("/repo/b.rs", SECOND);

    store_tour(
        &h,
        "tour",
        &[
            (
                "first",
                Some("a1b2c3d4"),
                location("a.rs", 2, (1, 11), "fn two() {}"),
            ),
            (
                "second",
                Some("b2c3d4e5"),
                location("b.rs", 1, (1, 13), "fn three() {}"),
            ),
            (
                "third",
                Some("b2c3d4e5"),
                location("a.rs", 1, (1, 11), "fn one() {}"),
            ),
        ],
    );
    h
}

/// Store the tour `slug`, whose stops are `(title, commit, focus)`.
fn store_tour(h: &TestHarness, slug: &str, stops: &[(&str, Option<&str>, Location)]) {
    let mut walkthrough = Walkthrough::new(slug.to_owned(), "Tour".to_owned(), None);
    for (title, commit, focus) in stops {
        walkthrough
            .add_stop(
                Some((*title).to_owned()),
                String::new(),
                focus.clone(),
                commit.map(str::to_owned),
                None,
            )
            .expect("append");
    }
    h.fake_fs().insert_file(
        format!("/repo/.stoat/walkthroughs/{slug}.json"),
        serde_json::to_string(&walkthrough).expect("serialize"),
    );
}

/// A repository whose working tree renames `fn two` in `a.rs`, and a tour
/// whose `s1` is a diff stop over the renamed line and whose `s2` is a plain
/// stop in `b.rs`, which the edit leaves alone.
fn diff_tour_harness() -> TestHarness {
    let mut h = Stoat::test();
    h.stage_review_scenario("/repo", &[("a.rs", FIRST, RENAMED)]);
    h.fake_fs().insert_file("/repo/b.rs", SECOND);

    let mut walkthrough = Walkthrough::new("tour".to_owned(), "Tour".to_owned(), None);
    for (title, focus) in [
        ("renamed", location("a.rs", 2, (1, 12), "fn deux() {}")),
        ("plain", location("b.rs", 1, (1, 13), "fn three() {}")),
    ] {
        walkthrough
            .add_stop(Some(title.to_owned()), String::new(), focus, None, None)
            .expect("append");
    }
    walkthrough.stops[0].diff = true;
    h.fake_fs().insert_file(
        "/repo/.stoat/walkthroughs/tour.json",
        serde_json::to_string(&walkthrough).expect("serialize"),
    );
    h
}

/// Whether the focused editor shows its file as a diff.
fn diff_shown(h: &TestHarness) -> bool {
    let ws = h.stoat.active_workspace();
    h.stoat
        .focused_editor_ids()
        .and_then(|(editor, _)| ws.editors.get(editor))
        .is_some_and(|editor| editor.diff_view)
}

#[test]
fn a_diff_stop_latches_the_diff_view() {
    let mut h = diff_tour_harness();
    open(&mut h.stoat, "tour");
    h.settle();

    assert_eq!(
        (open_path(&h), latched(&h), diff_shown(&h)),
        (Some(PathBuf::from("/repo/a.rs")), true, true),
        "the stop's file shows as a diff in a latched pane",
    );
}

/// The flag turns the view on and never off. A tour of diff stops then
/// needs no flag on every stop to stay in the view, and a reader who
/// opened the view by hand keeps it through a plain tour.
#[test]
fn a_plain_stop_leaves_the_view_as_it_was() {
    let mut plain = harness_with_tour();
    open(&mut plain.stoat, "tour");

    let mut h = diff_tour_harness();
    open(&mut h.stoat, "tour");
    h.settle();
    next(&mut h.stoat, 1);
    h.settle();

    assert_eq!(
        (latched(&plain), open_path(&h), latched(&h)),
        (false, Some(PathBuf::from("/repo/b.rs")), true),
        "a plain stop latches nothing, and keeps the latch a diff stop set",
    );
}

fn checkouts(h: &TestHarness) -> Vec<String> {
    h.fake_git().checkouts(Path::new("/repo"))
}

fn walk_shas(h: &TestHarness) -> Vec<String> {
    h.stoat
        .active_workspace()
        .review_walk
        .as_ref()
        .map(|walk| {
            walk.commits
                .iter()
                .map(|commit| commit.sha.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// The sha a revision base names, or `None` for the working tree's own base.
fn diff_base(h: &TestHarness) -> Option<Option<String>> {
    match h.stoat.active_workspace().diff_base() {
        Some(DiffBase::Rev { sha, .. }) => Some(sha.clone()),
        _ => None,
    }
}

fn latched(h: &TestHarness) -> bool {
    let panes = &h.stoat.active_workspace().panes;
    panes.pane(panes.focus()).diff_mode
}

fn open_path(h: &TestHarness) -> Option<PathBuf> {
    let id = h.stoat.focused_editor_ids()?.1;
    h.stoat
        .active_workspace()
        .buffers
        .path_for(id)
        .map(Path::to_path_buf)
}

fn status(h: &TestHarness) -> Option<&str> {
    h.stoat.pending_message.as_deref()
}

fn tour_slug(h: &TestHarness) -> Option<&str> {
    h.stoat
        .active_workspace()
        .walkthrough
        .as_ref()
        .map(|run| run.walkthrough.slug.as_str())
}

fn review_badge(h: &TestHarness) -> Option<String> {
    let ws = h.stoat.active_workspace();
    ws.badges
        .find_by_source(BadgeSource::Review)
        .and_then(|id| ws.badges.get(id))
        .map(|badge| badge.label.clone())
}

#[test]
fn opening_a_commit_tour_checks_out_the_first_stops_commit() {
    let mut h = commit_tour_harness();
    open(&mut h.stoat, "tour");
    h.settle();

    assert_eq!(
        (checkouts(&h), walk_shas(&h), diff_base(&h)),
        (
            vec!["detached:a1b2c3d4".to_owned()],
            vec!["a1b2c3d4".to_owned(), "b2c3d4e5".to_owned()],
            Some(None),
        ),
        "one walk over the tour's two commits, standing on the first",
    );
    assert_eq!(
        (open_path(&h), latched(&h), status(&h)),
        (
            Some(PathBuf::from("/repo/a.rs")),
            true,
            Some("1/3: first @ a1b2c3d")
        ),
    );
}

/// The checkout runs off the loop, so the jump waits for its landing rather
/// than showing the stop over the last commit's tree.
#[test]
fn a_step_onto_another_commit_lands_after_its_checkout() {
    let mut h = commit_tour_harness();
    open(&mut h.stoat, "tour");
    h.settle();

    next(&mut h.stoat, 1);
    assert_eq!(
        (open_path(&h), status(&h)),
        (
            Some(PathBuf::from("/repo/a.rs")),
            Some("checking out b2c3d4e")
        ),
        "nothing jumps before the landing",
    );

    h.settle();
    assert_eq!(
        (
            checkouts(&h).last().cloned(),
            diff_base(&h),
            open_path(&h),
            status(&h)
        ),
        (
            Some("detached:b2c3d4e5".to_owned()),
            Some(Some("a1b2c3d4".to_owned())),
            Some(PathBuf::from("/repo/b.rs")),
            Some("2/3: second @ b2c3d4e"),
        ),
    );
}

/// A landing arrives with no key press behind it, so it follows the cursor
/// itself, and a stop far down its file lands on screen.
#[test]
fn a_landing_scrolls_a_distant_stop_into_view() {
    let long: String = (1..=80).map(|n| format!("fn f{n}() {{}}\n")).collect();
    let mut h = Stoat::test();
    h.seed_linear_history(
        "/repo",
        &[
            ("a1b2c3d4", "feat: add long.rs", &[("long.rs", &long)]),
            (
                "b2c3d4e5",
                "feat: add b.rs",
                &[("long.rs", &long), ("b.rs", SECOND)],
            ),
        ],
    );
    h.fake_git()
        .add_repo("/repo")
        .branch("main", "b2c3d4e5")
        .set_head_branch("main");
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_fs().insert_file("/repo/long.rs", &long);
    h.fake_fs().insert_file("/repo/b.rs", SECOND);
    store_tour(
        &h,
        "tour",
        &[
            (
                "top",
                Some("a1b2c3d4"),
                location("long.rs", 1, (1, 10), "fn f1() {}"),
            ),
            (
                "bottom",
                Some("b2c3d4e5"),
                location("long.rs", 70, (1, 11), "fn f70() {}"),
            ),
        ],
    );
    open(&mut h.stoat, "tour");
    h.settle();

    next(&mut h.stoat, 1);
    h.settle();
    h.snapshot();
    assert!(
        h.rendered_text().contains("fn f70() {}"),
        "the landed stop is on screen, got:\n{}",
        h.rendered_text(),
    );
}

/// The point-by-point walk crosses commits the same way the stop walk does.
#[test]
fn a_linear_step_onto_another_commit_checks_it_out() {
    let mut h = commit_tour_harness();
    open(&mut h.stoat, "tour");
    h.settle();

    forward(&mut h.stoat, 1);
    h.settle();
    assert_eq!(
        (checkouts(&h), open_path(&h), status(&h)),
        (
            vec![
                "detached:a1b2c3d4".to_owned(),
                "detached:b2c3d4e5".to_owned()
            ],
            Some(PathBuf::from("/repo/b.rs")),
            Some("2/3: second @ b2c3d4e"),
        ),
    );
}

#[test]
fn a_step_within_one_commit_checks_out_nothing() {
    let mut h = commit_tour_harness();
    open(&mut h.stoat, "tour");
    h.settle();
    next(&mut h.stoat, 1);
    h.settle();

    next(&mut h.stoat, 1);
    h.settle();
    assert_eq!(
        (checkouts(&h), open_path(&h), status(&h)),
        (
            vec![
                "detached:a1b2c3d4".to_owned(),
                "detached:b2c3d4e5".to_owned()
            ],
            Some(PathBuf::from("/repo/a.rs")),
            Some("3/3: third @ b2c3d4e"),
        ),
    );
}

#[test]
fn stepping_back_across_commits_checks_the_earlier_one_out_again() {
    let mut h = commit_tour_harness();
    open(&mut h.stoat, "tour");
    h.settle();
    next(&mut h.stoat, 1);
    h.settle();

    prev(&mut h.stoat, 1);
    h.settle();
    assert_eq!(
        (checkouts(&h), diff_base(&h), open_path(&h), status(&h)),
        (
            vec![
                "detached:a1b2c3d4".to_owned(),
                "detached:b2c3d4e5".to_owned(),
                "detached:a1b2c3d4".to_owned(),
            ],
            Some(None),
            Some(PathBuf::from("/repo/a.rs")),
            Some("1/3: first @ a1b2c3d"),
        ),
    );
}

#[test]
fn done_returns_head_and_closes_the_tour() {
    let mut h = commit_tour_harness();
    open(&mut h.stoat, "tour");
    h.settle();

    done(&mut h.stoat);
    h.settle();
    assert_eq!(
        (
            checkouts(&h).last().cloned(),
            walk_shas(&h),
            tour_slug(&h),
            diff_base(&h),
            latched(&h)
        ),
        (Some("ref:main".to_owned()), Vec::new(), None, None, false),
    );
}

/// `:done` during a commit tour closes the tour with the walk, and the
/// status that the return leaves names the branch.
#[test]
fn review_done_closes_a_commit_tour() {
    let mut h = commit_tour_harness();
    open(&mut h.stoat, "tour");
    h.settle();

    action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    h.settle();
    assert_eq!(
        (checkouts(&h).last().cloned(), tour_slug(&h), status(&h)),
        (Some("ref:main".to_owned()), None, Some("returned to main")),
    );
}

/// A refused walk sends no landing, so a tour left to wait for one never
/// moves.
#[test]
fn a_dirty_tree_refuses_a_commit_tour() {
    let mut h = commit_tour_harness();
    h.fake_git()
        .add_repo("/repo")
        .modified("a.rs", FIRST, "fn edited() {}\n");

    open(&mut h.stoat, "tour");
    h.settle();
    assert_eq!(
        (checkouts(&h), tour_slug(&h), review_badge(&h)),
        (Vec::new(), None, Some("uncommitted changes".to_owned())),
    );
}

#[test]
fn a_tour_naming_an_unknown_commit_keeps_the_playing_tour() {
    let mut h = commit_tour_harness();
    open(&mut h.stoat, "tour");
    h.settle();
    store_tour(
        &h,
        "ghost",
        &[(
            "gone",
            Some("deadbeef00"),
            location("a.rs", 1, (1, 11), "fn one() {}"),
        )],
    );

    open(&mut h.stoat, "ghost");
    h.settle();
    assert_eq!(
        (
            tour_slug(&h),
            walk_shas(&h).len(),
            checkouts(&h),
            status(&h)
        ),
        (
            Some("tour"),
            2,
            vec!["detached:a1b2c3d4".to_owned()],
            Some("commit deadbee is not in this repository"),
        ),
    );
}

#[test]
fn a_plain_tour_starts_no_walk() {
    let mut h = commit_tour_harness();
    store_tour(
        &h,
        "plain",
        &[("here", None, location("a.rs", 2, (1, 11), "fn two() {}"))],
    );

    open(&mut h.stoat, "plain");
    h.settle();
    assert_eq!(
        (checkouts(&h), walk_shas(&h), open_path(&h), status(&h)),
        (
            Vec::new(),
            Vec::new(),
            Some(PathBuf::from("/repo/a.rs")),
            Some("1/1: here")
        ),
    );
}

/// The tour's walk waits behind another git job here, the way it waits
/// behind a slow return, so the tour closes before the walk's turn.
#[test]
fn a_tour_closed_before_its_checkout_starts_no_walk() {
    let mut h = commit_tour_harness();
    git_jobs::enqueue(&mut h.stoat, git_jobs::idle_job(None));
    open(&mut h.stoat, "tour");
    done(&mut h.stoat);
    h.settle();

    assert_eq!(
        (checkouts(&h), walk_shas(&h), tour_slug(&h)),
        (Vec::new(), Vec::new(), None),
    );
}

/// A step taken before the walk starts has no walk to seek. The first
/// landing finds the tour on another commit and follows it there, so the
/// reader arrives once, on the stop's own commit.
#[test]
fn a_step_before_the_first_checkout_lands_on_the_stops_commit() {
    let mut h = commit_tour_harness();
    git_jobs::enqueue(&mut h.stoat, git_jobs::idle_job(None));
    open(&mut h.stoat, "tour");
    next(&mut h.stoat, 1);
    h.settle();

    assert_eq!(
        (checkouts(&h), diff_base(&h), open_path(&h), status(&h)),
        (
            vec![
                "detached:a1b2c3d4".to_owned(),
                "detached:b2c3d4e5".to_owned()
            ],
            Some(Some("a1b2c3d4".to_owned())),
            Some(PathBuf::from("/repo/b.rs")),
            Some("2/3: second @ b2c3d4e"),
        ),
    );
}

/// A stop that names no commit reads whatever the walk has checked out, and
/// a step from it back onto that commit still lands, with nothing to check
/// out again.
#[test]
fn a_working_tree_stop_in_a_commit_tour_jumps_without_a_checkout() {
    let mut h = commit_tour_harness();
    store_tour(
        &h,
        "mixed",
        &[
            (
                "first",
                Some("a1b2c3d4"),
                location("a.rs", 2, (1, 11), "fn two() {}"),
            ),
            (
                "second",
                None,
                location("b.rs", 1, (1, 13), "fn three() {}"),
            ),
            (
                "third",
                Some("a1b2c3d4"),
                location("a.rs", 1, (1, 11), "fn one() {}"),
            ),
        ],
    );
    open(&mut h.stoat, "mixed");
    h.settle();

    next(&mut h.stoat, 1);
    let second = (open_path(&h), status(&h).map(str::to_owned));
    next(&mut h.stoat, 1);
    h.settle();

    assert_eq!(
        (second, checkouts(&h), open_path(&h), status(&h)),
        (
            (
                Some(PathBuf::from("/repo/b.rs")),
                Some("2/3: second".to_owned())
            ),
            vec!["detached:a1b2c3d4".to_owned()],
            Some(PathBuf::from("/repo/a.rs")),
            Some("3/3: third @ a1b2c3d"),
        ),
    );
}

/// The first tour's walk hands HEAD back before the second starts, so the
/// second records the branch rather than the first tour's commit.
#[test]
fn opening_a_commit_tour_over_another_returns_the_first() {
    let mut h = commit_tour_harness();
    open(&mut h.stoat, "tour");
    h.settle();
    store_tour(
        &h,
        "later",
        &[(
            "again",
            Some("a1b2c3d4"),
            location("a.rs", 1, (1, 11), "fn one() {}"),
        )],
    );

    open(&mut h.stoat, "later");
    h.settle();
    let return_ref = h
        .stoat
        .active_workspace()
        .review_walk
        .as_ref()
        .map(|walk| walk.return_ref.clone());
    assert_eq!(
        (checkouts(&h), tour_slug(&h), return_ref),
        (
            vec![
                "detached:a1b2c3d4".to_owned(),
                "ref:main".to_owned(),
                "detached:a1b2c3d4".to_owned(),
            ],
            Some("later"),
            Some(ReturnRef::Branch("main".to_owned())),
        ),
    );
}
