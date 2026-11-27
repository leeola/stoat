use ratatui::{buffer::Buffer, layout::Rect};
use stoat::{
    render::{
        chrome::{ISLAND_BOTTOM, ISLAND_GAP, ISLAND_SIDE, ISLAND_TOP},
        help::{help_content_rows, help_layout, paint_help_list_rows, render_help},
    },
    test_fixture::{column_rules, plate_islands},
};
use stoatty_protocol::command::{decode_stream, Command};

/// The footer row ends in its note, which names the mode help opened from and
/// then the context it captured there.
#[test]
fn the_footer_ends_in_the_mode_and_context_note() {
    let mut h = stoat::test_harness::TestHarness::with_size(200, 40);
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
    let help = h.stoat.help.take().expect("help is open");
    let theme = h.stoat.theme.clone();
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let area = Rect::new(0, 0, 200, 40);
    let mut buf = Buffer::empty(area);
    render_help(
        &help,
        "insert",
        h.stoat.active_workspace_mut(),
        &theme,
        &chrome,
        area,
        0,
        &mut buf,
        &mut stoat_widgets::ApcScene::new(),
    );

    let footer = help_layout(area, help_content_rows(&help), 0)
        .expect("help fits")
        .footer;
    h.stoat.help = Some(help);
    let text: String = (footer.x..footer.x + footer.width)
        .map(|x| buf[(x, footer.y)].symbol())
        .collect();
    assert!(
        text.ends_with("normal \u{00B7} view file \u{00B7} pane editor "),
        "the footer reads {text:?}"
    );
}

/// The live list draws its selected first row as the rounded box with no
/// bar. A pooled page captures no panel, so the page of the same rows keeps
/// the accent bar at the row's first cell and draws no panel.
#[test]
fn the_help_list_rounds_its_live_row_and_keeps_the_bar_on_its_page() {
    let mut h = stoat::test_harness::TestHarness::with_size(200, 40);
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
    let help = h.stoat.help.take().expect("help is open");
    let theme = h.stoat.theme.clone();
    let rows = Rect::new(0, 0, 40, 4);
    let mut live = stoat_widgets::ApcScene::new();
    paint_help_list_rows(
        &help,
        rows,
        0,
        &theme,
        true,
        &mut Buffer::empty(rows),
        &mut live,
    );
    let page = stoat::smooth_scroll::render_help_list_page(&help, 0, &theme, 40, 4);

    let marks = |bytes: &[u8]| -> Vec<(char, i32, i32)> {
        decode_stream(bytes)
            .into_iter()
            .filter_map(|command| match command {
                Command::Panel(panel) => Some(('p', panel.left.into(), panel.top.into())),
                Command::Bar(bar) => Some(('b', bar.x.into(), bar.y.into())),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        (marks(live.bytes()), marks(&page)),
        (vec![('p', 0, 0)], vec![('b', 0, 0)]),
    );
}

/// Under the plate look the list and the detail draw as Islands that face
/// each other across the separator column, which draws no rule.
#[test]
fn under_the_plate_the_list_and_the_detail_draw_as_islands() {
    let mut h = stoat::test_harness::TestHarness::with_size(200, 40);
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
    let help = h.stoat.help.take().expect("help is open");
    let theme = h.stoat.theme.clone();
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let area = Rect::new(0, 0, 200, 40);
    let mut buf = Buffer::empty(area);
    let mut scene = stoat_widgets::ApcScene::new();
    scene.set_nudge_protocol(true);
    render_help(
        &help,
        "insert",
        h.stoat.active_workspace_mut(),
        &theme,
        &chrome,
        area,
        0,
        &mut buf,
        &mut scene,
    );

    let layout = help_layout(area, help_content_rows(&help), 0).expect("help fits");
    let (list, detail) = (layout.list, layout.detail);
    assert_eq!(
        (
            plate_islands(&theme, &scene),
            column_rules(&scene, list.right())
        ),
        (
            vec![
                (list, [ISLAND_TOP, ISLAND_GAP, ISLAND_BOTTOM, ISLAND_SIDE]),
                (detail, [ISLAND_TOP, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_GAP]),
            ],
            0,
        ),
    );
}
