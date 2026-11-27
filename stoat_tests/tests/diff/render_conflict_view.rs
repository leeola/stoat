use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};
use std::{path::PathBuf, sync::Arc};
use stoat::{render::conflict_view::ConflictColumns, test_harness::TestHarness};
use stoat_widgets::ApcScene;
use stoatty_protocol::command::{decode_stream, Command};

/// Open the conflict view on one three-stage file in a `width`-wide harness.
///
/// The harness paints for a terminal with no scene, so the view keeps the
/// cell layout these tests read.
fn conflict_harness(width: u16, ancestor: &str, ours: &str, theirs: &str) -> TestHarness {
    let mut h = TestHarness::with_size(width, 24);
    h.stoat.stoatty = false;
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git()
        .add_repo("/repo")
        .with_fs(h.fake_fs())
        .conflicted_file("f.txt", Some(ancestor), Some(ours), Some(theirs));
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::Conflict);
    h.settle();
    h
}

/// The MergeGrid fills a side with no line on the row with the absent fill
/// and stripes each such run of cells with one hatched bar in the absent
/// colors. No other cell takes a hatch.
///
/// The theme gives the absent fill a color nothing else paints, so the
/// cells that take it are exactly the runs the bars must cover.
#[test]
fn the_merge_grid_hatches_each_side_with_no_line() {
    let mut h = conflict_harness(150, "a\nb\nc\n", "a\nB\nc\n", "a\nX\nc\n");
    let theme = {
        let src = format!(
            "{}\ntheme hatched inherits ermine_night {{ \
             diff.absent = {{ fg: \"#040506\", bg: \"#010203\" }}; }}",
            stoat::app::DEFAULT_KEYMAP,
        );
        let (config, _) = stoat_config::parse(&src);
        stoat::theme::Theme::from_config(&config.expect("the config parses"), "hatched")
            .expect("the theme loads")
    };
    let inner = Rect::new(0, 0, 150, 20);
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("center editor");
    let mut buf = Buffer::empty(inner);
    let mut scene = ApcScene::new();
    stoat::render::conflict_view::render_conflict_view(
        editor,
        inner,
        Style::default(),
        &theme,
        &mut buf,
        Some(&mut scene),
        true,
    );

    let absent = Color::Rgb(1, 2, 3);
    let mut filled = Vec::new();
    for y in inner.y..inner.bottom() {
        let mut x = inner.x;
        while x < inner.right() {
            let start = x;
            while x < inner.right() && buf[(x, y)].bg == absent {
                x += 1;
            }
            match x > start {
                true => filled.push((Rect::new(start, y, x - start, 1), [1, 2, 3], [4, 5, 6])),
                false => x += 1,
            }
        }
    }
    let hatched: Vec<_> = decode_stream(scene.buffer())
        .into_iter()
        .filter_map(|command| match command {
            Command::Bar(bar) => bar.hatch.map(|hatch| {
                let cells = Rect::new(
                    (bar.x / 16) as u16,
                    (bar.y / 16) as u16,
                    bar.width / 16,
                    bar.height / 16,
                );
                (cells, bar.color, hatch)
            }),
            _ => None,
        })
        .collect();

    assert!(!filled.is_empty(), "the marker rows leave a side empty");
    assert_eq!(hatched, filled);
}

/// The center has no horizontal scroll, so a cursor past its column is
/// hidden rather than drawn over the rule or the theirs column.
#[test]
fn a_cursor_past_the_center_column_paints_nothing() {
    let shared = "x".repeat(40);
    let mut h = conflict_harness(
        90,
        &format!("{shared}\nbase\n"),
        &format!("{shared}\nOURS\n"),
        &format!("{shared}\nTHEIRS\n"),
    );
    h.type_keys("k");
    h.type_keys("g l");
    let head = h.head_offsets();

    let theme = stoat::theme::Theme::empty();
    let inner = Rect::new(0, 0, 90, 20);
    let cols = ConflictColumns::compute(inner);
    let center_width = usize::from(cols.sep2_x - cols.center_text_x);
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("center editor");
    let mut buf = Buffer::empty(inner);
    editor.cursor_screen_cell = None;
    stoat::render::conflict_view::render_conflict_view(
        editor,
        inner,
        Style::default(),
        &theme,
        &mut buf,
        Some(&mut ApcScene::new()),
        true,
    );
    let delegated = editor.cursor_screen_cell;
    stoat::render::conflict_view::render_conflict_view(
        editor,
        inner,
        Style::default(),
        &theme,
        &mut buf,
        None,
        false,
    );
    let painted_past = (cols.sep2_x..inner.right())
        .flat_map(|x| (inner.top()..inner.bottom()).map(move |y| (x, y)))
        .any(|cell| buf[cell].style().add_modifier.contains(Modifier::REVERSED));

    assert_eq!(
        (head[0] >= center_width, delegated, painted_past),
        (true, None, false),
        "the cursor sits past the center column, and neither path draws it there"
    );
}

/// Split each rendered row on the two column rules into its trimmed ours,
/// center, and theirs text.
///
/// Reading each column on its own is what makes these tests meaningful. A
/// whole-frame substring check passes on a painting center even when both
/// sides are blank, which is exactly the failure being pinned. Rows are read
/// off the rules rather than off [`ConflictColumns`] so the assertions
/// describe what reached the screen, not what the geometry intended.
fn split_columns(frame: &str) -> Vec<(String, String, String)> {
    frame
        .lines()
        .map(|line| {
            let mut parts = line.split('│');
            let ours = parts.next().unwrap_or_default().trim().to_string();
            let center = parts.next().unwrap_or_default().trim().to_string();
            let theirs = parts.next().unwrap_or_default().trim().to_string();
            (ours, center, theirs)
        })
        .collect()
}

/// The focused editor's conflict state paired with a snapshot of its center
/// buffer, which is what `derived` is keyed on.
fn view_and_snapshot(
    h: &mut TestHarness,
) -> (
    stoat::conflict_session::ConflictViewState,
    stoat::display_map::DisplaySnapshot,
) {
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("center editor");
    let view = editor.conflict_view.clone().expect("conflict view");
    (
        Arc::try_unwrap(view).unwrap_or_else(|shared| (*shared).clone()),
        editor.display_map.snapshot(),
    )
}

/// A rebuild allocates a fresh plan, so a stable pointer is proof the cached
/// one was handed back rather than recomputed.
fn plan_ptr(
    state: &mut stoat::conflict_session::ConflictViewState,
    snap: &stoat::display_map::DisplaySnapshot,
) -> *const stoat::merge_view::AlignRow {
    state.derived(snap).1.plan.as_ptr()
}

#[test]
fn derived_layout_is_reused_while_the_buffer_holds() {
    let mut h = conflict_harness(150, "a\nbase\nz\n", "a\nOURS\nz\n", "a\nTHEIRS\nz\n");
    let (mut state, snap) = view_and_snapshot(&mut h);

    let first = plan_ptr(&mut state, &snap);
    assert_eq!(
        plan_ptr(&mut state, &snap),
        first,
        "a second paint against the same snapshot reuses the derived layout",
    );
}

#[test]
fn an_edit_to_the_center_forces_a_recompute() {
    let mut h = conflict_harness(150, "a\nbase\nz\n", "a\nOURS\nz\n", "a\nTHEIRS\nz\n");
    let (mut state, before) = view_and_snapshot(&mut h);
    let stale = plan_ptr(&mut state, &before);

    h.type_keys("i");
    h.type_text("edited");
    h.type_keys("escape");

    let (_, after) = view_and_snapshot(&mut h);
    assert_ne!(
        plan_ptr(&mut state, &after),
        stale,
        "a changed buffer version rebuilds rather than reusing",
    );

    let mut scratch = state.clone();
    scratch.invalidate_derived();
    assert_eq!(
        state.derived(&after).1.states,
        scratch.derived(&after).1.states,
        "the rebuilt states match a from-scratch classification",
    );
}

#[test]
fn a_pick_forces_a_recompute() {
    let mut h = conflict_harness(150, "a\nbase\nz\n", "a\nOURS\nz\n", "a\nTHEIRS\nz\n");
    let (mut state, snap) = view_and_snapshot(&mut h);
    let before = state.derived(&snap).1.states.clone();

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ConflictPickOurs);
    h.settle();

    let (mut picked, after) = view_and_snapshot(&mut h);
    assert_ne!(
        picked.derived(&after).1.states,
        before,
        "the pick's new chunk state reaches the painter rather than a stale one",
    );
}

#[test]
fn side_columns_carry_the_ours_and_theirs_content() {
    let mut h = conflict_harness(150, "a\nbase\nz\n", "a\nOURS\nz\n", "a\nTHEIRS\nz\n");
    let rows = split_columns(&h.snapshot().content);

    assert_eq!(
        rows[0],
        ("1 a".into(), "1 a".into(), "1 a".into()),
        "context above the conflict reads across all three columns, each \
         numbered from its own stage",
    );
    assert_eq!(
        (rows[1].0.as_str(), rows[1].2.as_str()),
        ("2 OURS", "2 THEIRS"),
        "each side's stage content sits on the band's first row",
    );
    assert_eq!(
        rows[1].1, "?   2 <<<<<<< ours",
        "and lines up with the center's marker band",
    );
}

#[test]
fn side_columns_stay_aligned_while_scrolled() {
    let stage = |edit: &str| -> String {
        (0..200)
            .map(|i| match i {
                120 => format!("{edit}\n"),
                _ => format!("line {i:03}\n"),
            })
            .collect()
    };
    let base: String = (0..200).map(|i| format!("line {i:03}\n")).collect();

    let mut h = conflict_harness(150, &base, &stage("OURS EDIT"), &stage("THEIRS EDIT"));
    // Park the marker band one row below the top so it renders clear of the
    // conflict hint box, which the view always overlays bottom-right.
    stoat::action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .scroll_row = 119;

    let rows = split_columns(&h.snapshot().content);

    assert_eq!(
        (rows[0].0.as_str(), rows[0].2.as_str()),
        ("120 line 119", "120 line 119"),
        "the sides start at the scrolled row, so the align plan is indexed \
         by absolute display row rather than by screen row",
    );
    assert_eq!(
        (rows[1].0.as_str(), rows[1].2.as_str()),
        ("121 OURS EDIT", "121 THEIRS EDIT"),
        "and the scrolled conflict band still carries both sides",
    );
}
