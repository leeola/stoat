use stoat::{
    agent_status::{AgentHookEvent, AgentStatus},
    badge::{Anchor, Badge, BadgeSource},
    test_fixture::corner_badge,
    Stoat,
};
use stoat_config::TabBarMode;
use stoatty_protocol::{command, command::Command, NUDGE_PROTOCOL};

#[test]
fn snapshot_agent_badge_active() {
    let mut h = Stoat::test();
    let mut status = AgentStatus::new();
    status.apply(AgentHookEvent::PreToolUse {
        tool: "Bash".into(),
    });
    h.stoat.active_workspace_mut().agent = Some(status);
    h.assert_snapshot("agent_badge_active");
}

/// With the tab bar up, the agent's top-right chip sits in the bar's row
/// rather than on the first pane row under it.
#[test]
fn the_agent_chip_sits_in_the_tab_bar_row() {
    let mut h = stoat::test_harness::TestHarness::with_size(80, 24);
    h.type_action("NewTab()");
    let mut status = AgentStatus::new();
    status.apply(AgentHookEvent::PreToolUse {
        tool: "Bash".into(),
    });
    h.stoat.active_workspace_mut().agent = Some(status);

    let buf = h.render_composited();
    let chip_rows: Vec<u16> = (0..buf.area.height)
        .filter(|&y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .contains("claude")
        })
        .collect();
    assert_eq!((h.stoat.tab_bar_visible(), chip_rows), (true, vec![0]));
}

/// From the nudge protocol on, a chip on a chrome band draws at the band's
/// nudge, as the band's own elements do. A bottom-right chip draws up by
/// the status row's pad, and a top-right chip down by the tab bar's pad
/// while the bar shows. The overlay lane ends on a zero nudge, so nothing
/// after the chips moves. A hidden bar, or a stoatty before the nudge,
/// leaves a chip on its row.
#[test]
fn a_chip_on_a_chrome_band_takes_the_band_nudge() {
    let nudges = |protocol: u32, bar: TabBarMode| {
        let mut h = stoat::test_harness::TestHarness::with_size(160, 50);
        h.stoat.stoatty_protocol = protocol;
        h.stoat.settings.ui_tab_bar = Some(bar);
        h.stoat.badges.insert(Badge {
            source: BadgeSource::Agent,
            anchor: Anchor::TopRight,
            ..corner_badge("top")
        });
        h.stoat.badges.insert(corner_badge("bottom"));
        h.settle();
        h.stoat.render();

        let lane =
            command::decode_stream(h.stoat.apc_scene.lane_bytes(stoat::render::OVERLAY_LANE));
        let in_effect = |at: usize| {
            lane[..at]
                .iter()
                .rev()
                .find_map(|command| match command {
                    Command::Nudge(nudge) => Some(*nudge),
                    _ => None,
                })
                .unwrap_or([0, 0])
        };
        let chip = |label: &str| {
            let at = lane
                .iter()
                .position(|command| matches!(command, Command::TextRun(run) if run.text == label))
                .expect("the chip's text run");
            (matches!(lane[at - 1], Command::Panel(_)), in_effect(at - 1))
        };
        (chip("top"), chip("bottom"), in_effect(lane.len()))
    };

    assert_eq!(
        [
            nudges(NUDGE_PROTOCOL, TabBarMode::Always),
            nudges(NUDGE_PROTOCOL, TabBarMode::Never),
            nudges(NUDGE_PROTOCOL - 1, TabBarMode::Always),
        ],
        [
            ((true, [0, 7]), (true, [0, -3]), [0, 0]),
            ((true, [0, 0]), (true, [0, -3]), [0, 0]),
            ((true, [0, 0]), (true, [0, 0]), [0, 0]),
        ],
        "whether a panel leads each chip's text and the nudge on it, top then bottom, and the \
             nudge the lane ends on",
    );
}

#[test]
fn pinned_chord_paints_the_badge_and_escape_clears_it() {
    let mut h = stoat::test_harness::TestHarness::with_size(160, 60);
    h.type_keys("space g g");
    let held = shows_pinned(&mut h);
    h.type_keys("escape");
    assert_eq!(
        (held, shows_pinned(&mut h)),
        (true, false),
        "the badge shows while the chord holds"
    );
}

/// A config-named `*_pin` mode holds with no `PinMode` press, and the
/// badge marks it all the same.
#[test]
fn a_pin_named_mode_paints_the_badge() {
    let mut h = stoat::test_harness::TestHarness::with_size(160, 60);
    h.stoat.set_focused_mode("goto_pin".into());
    assert!(shows_pinned(&mut h), "the mode's name marks the hold");
}

/// Whether the composited frame holds the word `pinned` on any row.
fn shows_pinned(h: &mut stoat::test_harness::TestHarness) -> bool {
    let buf = h.render_composited();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .contains("pinned")
}
