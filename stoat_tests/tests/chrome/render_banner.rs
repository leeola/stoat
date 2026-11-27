use stoat::{
    action_handlers,
    review_walk::{ReturnRef, ReviewWalk, WalkStash},
    test_harness::TestHarness,
};
use stoat_action::NewTab;
use stoatty_protocol::{
    command::{
        decode_stream, BorderStyle, Command, Edge, PanelCommand, PanelShadow, TextRunCommand,
    },
    PROTOCOL_VERSION,
};

/// The ermine banner's tint, which its panel and its runs fill with.
const TINT: [u8; 3] = [0x2e, 0x2a, 0x1e];

/// Without chrome bands the banner is the cells of the row under the tab
/// bar, with stoatty and without it.
///
/// The commit's texts start a cell in, and a stash adds its count and its
/// branch. The return ends a cell short of the row's end, and a walk that
/// started detached returns to its short sha. In a row too narrow for
/// both, the commit's texts clip and the return drops.
#[test]
fn the_cell_banner_names_the_commit_and_the_return() {
    let rows = |stoatty: bool| {
        let row = |width: u16, return_ref: ReturnRef, changes: Option<usize>| {
            let mut h = banner_harness(width, true, return_ref, changes);
            h.stoat.stoatty = stoatty;
            let buf = h.stoat.render();
            (0..width).map(|x| buf[(x, 1)].symbol()).collect::<String>()
        };
        [
            row(120, on_main(), Some(2)),
            row(80, ReturnRef::Detached("c0ffee1234".to_owned()), None),
            row(60, on_main(), Some(1)),
        ]
    };

    let want = [
        format!(
            " detached at b2c3d4e checked out from blame \u{b7} 2 changes stashed from \
                 main{:22}Space h c return to main ",
            ""
        ),
        format!(
            " detached at b2c3d4e checked out from blame{:9}Space h c return to c0ffee1 ",
            ""
        ),
        " detached at b2c3d4e checked out from blame \u{b7} 1 change stas ".to_owned(),
    ];
    assert_eq!([rows(false), rows(true)], [want.clone(), want]);
}

/// Under chrome bands the banner is a panel with radius 6 that stands 4 px
/// outside its row at the top and the bottom and 1 px outside it at each
/// side, with its texts on it as runs.
///
/// Under the tab bar's band the banner draws 2 px down. With the bar
/// hidden it draws 4 px down from the window's first row, and at 60 cells
/// the stash text clips and the return drops. The island under the banner
/// reaches into the band's second row both times.
#[test]
fn the_band_banner_draws_its_panel_and_runs() {
    let under_bar = band_banner(banner_harness(120, true, on_main(), Some(2)));
    let alone = band_banner(banner_harness(60, false, on_main(), Some(1)));

    let (warning, dim) = ([0xe8, 0xbd, 0x6a], [0xa3, 0xa9, 0xb5]);
    assert_eq!(
        [under_bar, alone],
        [
            (
                vec![
                    Command::Nudge([0, 2]),
                    panel(2, 118),
                    run(32, 32, "detached at b2c3d4e", warning, true),
                    run(
                        352,
                        32,
                        "checked out from blame \u{b7} 2 changes stashed from main",
                        dim,
                        false,
                    ),
                    run(1504, 32, "Space h c", [0x6c, 0xc7, 0xd6], false),
                    run(1648, 32, " return to main", [0x83, 0x8a, 0x98], false),
                    Command::Nudge([0, 0]),
                ],
                Some(Edge::Margin(14)),
            ),
            (
                vec![
                    Command::Nudge([0, 4]),
                    panel(0, 58),
                    run(32, 0, "detached at b2c3d4e", warning, true),
                    run(
                        352,
                        0,
                        "checked out from blame \u{b7} 1 change st",
                        dim,
                        false
                    ),
                    Command::Nudge([0, 0]),
                ],
                Some(Edge::Margin(14)),
            ),
        ],
    );
}

/// A harness `width` cells wide whose workspace holds a blame checkout's
/// walk at b2c3d4e back to `return_ref`, with a stash of `changes` files
/// where it holds one. A second tab shows the tab bar where `tab_bar`.
fn banner_harness(
    width: u16,
    tab_bar: bool,
    return_ref: ReturnRef,
    changes: Option<usize>,
) -> TestHarness {
    let mut h = TestHarness::with_size(width, 12);
    if tab_bar {
        action_handlers::dispatch(&mut h.stoat, &NewTab);
    }
    let stash = changes.map(|changes| WalkStash {
        sha: "s1".to_owned(),
        changes,
    });
    h.stoat.active_workspace_mut().review_walk =
        Some(ReviewWalk::blame_checkout("b2c3d4e", return_ref, stash));
    h
}

/// The commands of the banner in a frame of `h` under chrome bands, from
/// the nudge before its panel to the nudge after its runs, and the top edge
/// of the frame's first island.
fn band_banner(mut h: TestHarness) -> (Vec<Command>, Option<Edge>) {
    h.stoat.stoatty_protocol = PROTOCOL_VERSION;
    h.stoat.render();
    let commands = decode_stream(&h.stoat.apc_scene.all_bytes());
    let panel = commands
        .iter()
        .position(|command| matches!(command, Command::Panel(panel) if panel.fill == Some(TINT)))
        .expect("the banner's panel");

    let mut banner = Vec::new();
    for command in commands[panel - 1..]
        .iter()
        .filter(|command| **command != Command::TextRunEnd)
    {
        banner.push(command.clone());
        if *command == Command::Nudge([0, 0]) {
            break;
        }
    }
    let island = commands.iter().find_map(|command| match command {
        Command::Panel(panel) if panel.backdrop => Some(panel.edges[0]),
        _ => None,
    });
    (banner, island)
}

/// The banner's panel on the row at `top`, one column in and `width` wide.
fn panel(top: u16, width: u16) -> Command {
    Command::Panel(PanelCommand {
        top,
        left: 1,
        width,
        height: 1,
        style: BorderStyle::None_,
        border: TINT,
        corner_radius: 6,
        fill: Some(TINT),
        shadow: PanelShadow::None_,
        edges: [
            Edge::Outset(4),
            Edge::Outset(1),
            Edge::Outset(4),
            Edge::Outset(1),
        ],
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: false,
        backdrop: false,
        anchor: None,
    })
}

/// A banner run of `text` at `col` and `row` in sixteenths, on the tint.
fn run(col: i16, row: i16, text: &str, color: [u8; 3], bold: bool) -> Command {
    Command::TextRun(TextRunCommand {
        col,
        row,
        scale: 256,
        color,
        bg: Some(TINT),
        bold,
        italic: false,
        follow: 0,
        anchor: None,
        text: text.to_owned(),
    })
}

fn on_main() -> ReturnRef {
    ReturnRef::Branch("main".to_owned())
}
