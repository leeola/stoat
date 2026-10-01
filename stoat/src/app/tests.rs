use super::*;
use crate::{
    action_handlers::lsp::RenameInputState,
    agent_status::AgentHookEvent,
    apc_emit::{
        display_map_stamp, editor_page_content_version, osc_default_colors, window_content_version,
    },
    badge::{Anchor as BadgeAnchor, Badge, BadgeSource, BadgeState},
    debounce::{FS_WATCH_DEBOUNCE, INDEX_EDIT_DEBOUNCE},
    display_map::{DisplayPoint, PaintVersion},
    host::FsEventKind,
    input_parse::{self, InputStep},
    input_view::{InputView, SubmitTarget},
    render::{review::DiffDials, walkthrough::Spotlight},
    run::GridSelection,
    term_session::{TermSelection, TermSession},
    test_fixture::{
        finder_layout, focused_editor_pane_area, mouse_event, open_indent_buffer,
        open_scratch_file, open_with_minimap_strip, palette_sizing,
    },
};
use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::style::Modifier;
use std::{
    ops::Range,
    path::{Path, PathBuf},
};
use stoat_config::LineNumbers;
use stoatty_protocol::command::{self, PoolRegionCommand};

fn stoat_with_detached_pane(window: u32) -> (Stoat, PaneId) {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;
    let ws = stoat.active_workspace_mut();
    let detached = ws.panes.split(crate::pane::Axis::Vertical);
    assert!(ws.panes.detach(detached, window));
    (stoat, detached)
}

/// A launch resolves the mouse capture policy before the UI thread starts,
/// then builds the settings, keymap and theme pool, and a config reload
/// builds them again. Each reads the 55 KB source, so each parsing it
/// separately puts milliseconds between the launch and its first frame.
#[test]
fn every_caller_of_the_embedded_config_reads_one_parse() {
    assert!(
        std::ptr::eq(embedded_config(), embedded_config()),
        "the defaults are parsed once and shared from there on",
    );
}

#[test]
fn window_ipc_resize_sets_detached_pane_area() {
    let (mut stoat, detached) = stoat_with_detached_pane(3);
    stoat.handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Resized {
        window: 3,
        cols: 50,
        rows: 20,
    }));
    assert_eq!(
        stoat.active_workspace().panes.pane(detached).area,
        Rect::new(0, 0, 50, 20),
    );
    assert!(
        stoat.pool_settle.is_some(),
        "an aux window's resize moves its pool's rectangle, so it holds the \
             fills back the way the terminal's does",
    );
}

/// Travel worth a fraction of a line reaches the editor whole, which is
/// what lets the view rest between two rows.
#[test]
fn window_ipc_wheel_moves_the_editor_target_by_its_fraction() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 12);
    let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
    let path = h.write_file("long.rs", &body);
    h.open_file(&path);

    let target = |h: &mut crate::test_harness::TestHarness| {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        editor.scroll_row as f32 + editor.scroll_frac
    };

    // Three rows to a line, so a fifth of a line is 0.6 of a row.
    h.stoat.handle_window_ipc(wheel_ipc(0.2));
    assert!(
        (target(&mut h) - 0.6).abs() < 1e-5,
        "one fraction of travel moves a fraction of a row",
    );

    h.stoat.handle_window_ipc(wheel_ipc(0.2));
    assert!(
        (target(&mut h) - 1.2).abs() < 1e-5,
        "and the next accrues on it rather than restarting",
    );
}

/// A list steps by whole notches, so sub-notch travel over one accrues
/// until it makes a step rather than being thrown away or falling through
/// to the pane underneath.
#[test]
fn window_ipc_wheel_steps_a_picker_once_per_notch() {
    use stoat_action::OpenFileFinder;

    let mut h = crate::test_harness::TestHarness::with_size(80, 24);
    let root = std::path::PathBuf::from("/wheel-ipc-finder");
    for name in ["a.rs", "b.rs", "c.rs"] {
        h.fake_fs().insert_file(root.join(name), b"x\n");
    }
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();

    let selected = |h: &crate::test_harness::TestHarness| {
        h.stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .active_core_ref()
            .picklist
            .selected
    };

    for _ in 0..4 {
        h.stoat.handle_window_ipc(wheel_ipc(0.2));
    }
    assert_eq!(
        selected(&h),
        0,
        "four fifths of a line is short of a notch, so the list holds",
    );

    h.stoat.handle_window_ipc(wheel_ipc(0.2));
    assert_eq!(
        selected(&h),
        1,
        "and the fifth completes it, stepping exactly once",
    );
}

/// A notch is one line of travel, so the precision path and the notch path
/// land the same target for the same distance.
#[test]
fn window_ipc_wheel_lands_where_the_same_notches_land() {
    let landed = |precision: bool| {
        let mut h = crate::test_harness::TestHarness::with_size(40, 12);
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);
        for _ in 0..3 {
            match precision {
                true => {
                    h.stoat.handle_window_ipc(wheel_ipc(1.0));
                },
                false => {
                    h.stoat.update(Event::Mouse(MouseEvent {
                        kind: MouseEventKind::ScrollDown,
                        column: 1,
                        row: 1,
                        modifiers: KeyModifiers::NONE,
                    }));
                },
            }
        }
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        (editor.scroll_row, editor.scroll_frac)
    };

    assert_eq!(
        landed(true),
        landed(false),
        "three lines of travel and three notches reach one place",
    );
}

#[test]
fn window_ipc_closed_reattaches_pane() {
    let (mut stoat, detached) = stoat_with_detached_pane(3);
    stoat.handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Closed { window: 3 }));
    let panes = &stoat.active_workspace().panes;
    assert_eq!(panes.pane(detached).placement, Placement::Split);
    assert!(panes.split_pane_ids().contains(&detached));
}

/// Nothing may assume a stoatty is listening before the handshake says so,
/// since the rich protocol splatters raw payload over a foreign terminal.
#[test]
fn a_fresh_stoat_assumes_no_stoatty_is_listening() {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );

    assert!(!stoat.stoatty, "the default is the safe one");
}

/// The first frames go out before the handshake can answer, so confirming a
/// listener has to repaint them rather than only affect later frames.
#[test]
fn a_confirmed_stoatty_sets_the_flag_and_repaints() {
    let mut h = crate::test_harness::TestHarness::with_size(80, 24);
    h.stoat.stoatty = false;

    assert_eq!(
        h.stoat
            .handle_stoatty_present(Some(stoatty_protocol::PROTOCOL_VERSION)),
        UpdateEffect::Redraw
    );
    assert!(h.stoat.stoatty, "and the flag stays set");
    assert_eq!(
        h.stoat.stoatty_protocol,
        stoatty_protocol::PROTOCOL_VERSION,
        "the peer's version is kept for gating what may be emitted"
    );
}

/// A stoatty from before the version field answers the handshake without
/// one, and is still a stoatty. Reading the report as a bare presence flag
/// would lose the distinction the version exists to carry.
#[test]
fn a_stoatty_predating_the_version_field_still_confirms_at_version_zero() {
    let mut h = crate::test_harness::TestHarness::with_size(80, 24);
    h.stoat.stoatty = false;

    assert_eq!(
        h.stoat.handle_stoatty_present(Some(0)),
        UpdateEffect::Redraw
    );
    assert!(h.stoat.stoatty, "a version-less reply is still a stoatty");
    assert_eq!(h.stoat.stoatty_protocol, 0);
}

/// The bin layer wires the app up before the run loop drains the handshake,
/// so the theme defaults cannot go out there and ride confirmation instead.
///
/// The zoom claim rides with them, asking for the press in band. That is
/// what makes the combo work over a link: the handshake and the presses
/// both cross it, where the window socket the claim used to wait for names
/// a path on the wrong machine.
#[test]
fn confirming_a_stoatty_sends_the_theme_defaults_and_claims_the_zoom() {
    let mut h = crate::test_harness::TestHarness::with_size(80, 24);
    h.stoat.stoatty = false;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    h.stoat
        .handle_stoatty_present(Some(stoatty_protocol::PROTOCOL_VERSION));

    let sent: Vec<u8> = std::iter::from_fn(|| rx.try_recv().ok())
        .flatten()
        .collect();
    let mut expected = osc_default_colors(&h.stoat.theme);
    assert!(
        expected.starts_with(b"\x1b]10;"),
        "the harness theme defines the default colors this covers"
    );
    command::encode_zoom_capture_into(&mut expected, true, true);
    assert_eq!(
        sent, expected,
        "confirmation pushes the theme defaults and an inband zoom claim, \
             and nothing else"
    );
}

/// The zoom claim goes out on the handshake and stays out.
///
/// The presses come back down the pty, so the handshake is the whole round
/// trip and the window socket is no part of it. That is what makes the claim
/// hold over a link, where the socket names a path on the wrong machine and
/// never connects at all.
///
/// Nothing here releases it either. A terminal drops the claim when this
/// process leaves the alternate screen, which it can see for itself.
#[test]
fn the_zoom_claim_rides_the_handshake_alone() {
    let claim = |on: bool| {
        let mut out = Vec::new();
        command::encode_zoom_capture_into(&mut out, on, on);
        out
    };

    let mut h = crate::test_harness::TestHarness::with_size(80, 24);
    h.stoat.stoatty = false;
    h.stoat.window_ipc_connected = false;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    // Every zoom frame, not a count of the claims, so a release sent before
    // anything was claimed shows up here too.
    let zoom_frames = |rx: &mut UnboundedReceiver<Vec<u8>>| {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|frame| *frame == claim(true) || *frame == claim(false))
            .collect::<Vec<_>>()
    };

    h.stoat
        .handle_stoatty_present(Some(stoatty_protocol::PROTOCOL_VERSION));
    assert_eq!(
        zoom_frames(&mut rx),
        vec![claim(true)],
        "the handshake alone claims the combo, with no socket in sight",
    );

    h.stoat.handle_window_ipc(WindowIpc::Connected);
    h.stoat.handle_window_ipc(WindowIpc::Disconnected);
    assert_eq!(
        zoom_frames(&mut rx),
        Vec::<Vec<u8>>::new(),
        "and the socket coming and going says nothing about it",
    );
}

#[test]
fn an_unanswered_handshake_leaves_the_session_foreign() {
    let mut h = crate::test_harness::TestHarness::with_size(80, 24);
    h.stoat.stoatty = false;

    assert_eq!(h.stoat.handle_stoatty_present(None), UpdateEffect::None);
    assert!(!h.stoat.stoatty, "a silent terminal is a foreign one");
}

fn zoom(delta: i32) -> WindowIpc {
    WindowIpc::Event(WindowIpcEvent::Zoom { window: 0, delta })
}

fn chord(ch: char) -> WindowIpc {
    WindowIpc::Event(WindowIpcEvent::Chord { window: 0, ch })
}

/// Precision wheel travel at the primary window's first cell.
fn wheel_ipc(lines: f32) -> WindowIpc {
    WindowIpc::Event(WindowIpcEvent::Wheel {
        window: 0,
        col: 1,
        row: 1,
        mods: 0,
        lines,
    })
}

/// A width-120 harness over one `.rs` file whose second line changed,
/// rendered in the diff view. The first line is unchanged, so it paints as
/// a context row on both sides, and the removed line paints as a block row
/// on the left.
fn diff_syntax_harness() -> crate::test_harness::TestHarness {
    let mut h = crate::test_harness::TestHarness::with_size(120, 20);
    // The column scans below address fixed side-by-side columns, so the
    // pane must span the full width rather than share it with a minimap.
    h.stoat.minimap_override = Some(false);
    h.stage_review_scenario(
        "/repo",
        &[(
            "a.rs",
            "fn keep() {}\nfn old() {}\n",
            "fn keep() {}\nfn new() {}\n",
        )],
    );
    h.stoat.set_diff_warm_auto(true);
    h.open_file(std::path::Path::new("/repo/a.rs"));
    h.settle_diff_jobs();
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);
    h.snapshot();
    h
}

/// The foreground of the first `glyph` cell on the row holding `text`.
fn glyph_fg(h: &crate::test_harness::TestHarness, text: &str, glyph: &str) -> String {
    let buf = h.rendered_buffer();
    let line = |y: u16| -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    };
    let row = (0..buf.area.height)
        .find(|&y| line(y).contains(text))
        .unwrap_or_else(|| panic!("{text:?} rendered"));
    let x = (0..buf.area.width)
        .find(|&x| buf[(x, row)].symbol() == glyph)
        .unwrap_or_else(|| panic!("{glyph:?} on the {text:?} row"));
    format!("{:?}", buf[(x, row)].style().fg)
}

/// The distinct foregrounds of the non-blank cells in `cols` on the row
/// holding `text`, which is one color exactly where nothing colors tokens.
fn row_colors(h: &crate::test_harness::TestHarness, cols: Range<u16>, text: &str) -> usize {
    let buf = h.rendered_buffer();
    let row = row_holding(buf, cols.clone(), text);
    cols.filter(|&x| !buf[(x, row)].symbol().trim().is_empty())
        .map(|x| format!("{:?}", buf[(x, row)].style().fg))
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

/// The glyphs of the cells in `cols` that carry `modifier`, on the row holding
/// `text`.
fn glyphs_with(
    h: &crate::test_harness::TestHarness,
    cols: Range<u16>,
    text: &str,
    modifier: Modifier,
) -> String {
    let buf = h.rendered_buffer();
    let row = row_holding(buf, cols.clone(), text);
    cols.filter(|&x| buf[(x, row)].modifier.contains(modifier))
        .map(|x| buf[(x, row)].symbol().to_string())
        .collect()
}

/// The first row whose glyphs in `cols` contain `text`.
fn row_holding(buf: &Buffer, cols: Range<u16>, text: &str) -> u16 {
    let line = |y: u16| -> String {
        cols.clone()
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    };
    (0..buf.area.height)
        .find(|&y| line(y).contains(text))
        .unwrap_or_else(|| panic!("{text:?} rendered in cols {cols:?}"))
}

/// With nothing over the panes the combo is a pane resize, so the focused
/// pane takes room from its neighbor.
#[test]
fn a_zoom_step_with_no_modal_open_resizes_the_focused_pane() {
    let mut h = crate::test_harness::TestHarness::with_size(101, 40);
    let left = h.stoat.active_workspace().panes.focus();
    let right = h
        .stoat
        .active_workspace_mut()
        .panes
        .split(crate::pane::Axis::Vertical);
    h.stoat.active_workspace_mut().panes.set_focus(left);
    let width = |h: &crate::test_harness::TestHarness, id| {
        h.stoat.active_workspace().panes.pane(id).area.width
    };
    assert_eq!((width(&h, left), width(&h, right)), (50, 50));

    h.stoat.handle_window_ipc(zoom(1));

    assert_eq!(
        (width(&h, left), width(&h, right)),
        (60, 40),
        "the focused pane grew a step against its neighbor"
    );
}

/// Inside the diff view there is no pane the reader is looking at to
/// resize, so the combo turns the one dial that screen has instead. What it
/// zooms there is the changed code, so the shrink key recedes the unchanged
/// context further and the grow key brings it back.
#[test]
fn a_zoom_step_in_the_diff_view_tunes_the_soften_and_leaves_the_panes_alone() {
    let mut h = crate::test_harness::TestHarness::with_size(101, 40);
    let left = h.stoat.active_workspace().panes.focus();
    let right = h
        .stoat
        .active_workspace_mut()
        .panes
        .split(crate::pane::Axis::Vertical);
    h.stoat.active_workspace_mut().panes.set_focus(left);
    let widths = |h: &crate::test_harness::TestHarness| {
        (
            h.stoat.active_workspace().panes.pane(left).area.width,
            h.stoat.active_workspace().panes.pane(right).area.width,
        )
    };
    let before = widths(&h);

    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);
    h.stoat.handle_window_ipc(zoom(1));

    assert_eq!(
        (h.stoat.diff_soften, widths(&h)),
        (-1, before),
        "the grow key brings the unchanged context back and moves no pane"
    );

    h.stoat.handle_window_ipc(zoom(-1));
    h.stoat.handle_window_ipc(zoom(-1));
    assert_eq!(
        h.stoat.diff_soften, 1,
        "the shrink key recedes it further, walking back past zero"
    );

    for _ in 0..8 {
        h.stoat.handle_window_ipc(zoom(-1));
    }
    assert_eq!(
        h.stoat.diff_soften,
        crate::render::review::DIFF_SOFTEN_MAX,
        "and stops where the receding stops"
    );

    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(false);
    let level = h.stoat.diff_soften;
    h.stoat.handle_window_ipc(zoom(1));

    assert_eq!(
        (h.stoat.diff_soften, widths(&h) == before),
        (level, false),
        "a plain pane takes the step as a resize again"
    );
}

/// The commits overlay paints over the pane grid, so a resize there moves
/// a pane nobody sees. The combo tunes the soften instead, which is the
/// contrast the reader is actually looking at.
#[test]
fn a_zoom_step_on_the_commits_screen_tunes_the_soften() {
    let mut h = crate::test_harness::TestHarness::with_size(101, 40);
    let left = h.stoat.active_workspace().panes.focus();
    let right = h
        .stoat
        .active_workspace_mut()
        .panes
        .split(crate::pane::Axis::Vertical);
    h.stoat.active_workspace_mut().panes.set_focus(left);
    let widths = |h: &crate::test_harness::TestHarness| {
        (
            h.stoat.active_workspace().panes.pane(left).area.width,
            h.stoat.active_workspace().panes.pane(right).area.width,
        )
    };
    let before = widths(&h);

    h.seed_linear_history("/repo", &[("c1", "first", &[("a.rs", "fn a() {}\n")])]);
    h.open_commits("/repo");
    h.stoat.handle_window_ipc(zoom(-1));

    assert_eq!(
        (h.stoat.diff_soften, widths(&h)),
        (1, before),
        "the shrink key recedes the preview's context and moves no pane"
    );
}

/// Syntax color competes with the diff's own marking, so the chord drops it
/// from both columns and leaves the soften and the bold to speak alone.
#[test]
fn the_syntax_chord_drops_both_diff_columns_to_one_color() {
    let mut h = diff_syntax_harness();
    assert!(
        h.stoat.diff_syntax,
        "the diff view opens with its syntax color on"
    );
    assert!(
        row_colors(&h, 68..120, "fn keep") > 1
            && row_colors(&h, 8..59, "fn keep") > 1
            && row_colors(&h, 8..59, "fn old") > 1,
        "every row starts out carrying token colors"
    );

    assert_eq!(h.stoat.handle_window_ipc(chord('8')), UpdateEffect::Redraw);
    h.snapshot();
    assert_eq!(
        (
            h.stoat.diff_syntax,
            row_colors(&h, 68..120, "fn keep"),
            row_colors(&h, 8..59, "fn keep"),
            row_colors(&h, 8..59, "fn old"),
        ),
        (false, 1, 1, 2),
        "every row loses its tokens, and the two left on the removed row are \
             the receded gaps against the changed word, which is the diff's own \
             marking rather than syntax"
    );

    h.stoat.handle_window_ipc(chord('8'));
    h.snapshot();
    assert!(
        h.stoat.diff_syntax && row_colors(&h, 68..120, "fn keep") > 1,
        "a second chord brings the color back"
    );
}

/// The tint is the diff view's own dial, so it answers only there, and each
/// end holds rather than wrapping or running away.
#[test]
fn the_nine_and_zero_chords_step_the_tint_dial() {
    let mut h = diff_syntax_harness();
    assert_eq!(h.stoat.diff_tint, 0, "a session opens with the tint off");

    for _ in 0..5 {
        assert_eq!(h.stoat.handle_window_ipc(chord('0')), UpdateEffect::Redraw);
    }
    assert_eq!(
        h.stoat.diff_tint,
        crate::render::review::DIFF_TINT_MAX,
        "two presses reach the top and the rest hold there",
    );

    for _ in 0..5 {
        assert_eq!(h.stoat.handle_window_ipc(chord('9')), UpdateEffect::Redraw);
    }
    assert_eq!(
        h.stoat.diff_tint, 0,
        "the same count back down lands on off"
    );

    h.stoat.diff_tint = 2;
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(false);
    assert_eq!(
        (
            h.stoat.handle_window_ipc(chord('0')),
            h.stoat.handle_window_ipc(chord('9')),
            h.stoat.diff_tint,
        ),
        (UpdateEffect::None, UpdateEffect::None, 2),
        "off the diff view both chords leave the level alone",
    );
}

/// A commit preview paints its rows through the diff view's own painter,
/// so the dial that colors them answers on that screen too. One key, one
/// meaning, wherever a diff is in front of the reader.
#[test]
fn the_tint_and_syntax_chords_answer_on_the_commits_screen() {
    let mut h = crate::test_harness::TestHarness::with_size(101, 40);
    h.seed_linear_history("/repo", &[("c1", "first", &[("a.rs", "fn a() {}\n")])]);
    h.open_commits("/repo");

    assert_eq!(
        (h.stoat.diff_tint, h.stoat.diff_syntax),
        (0, true),
        "a session opens with the tint off and syntax on",
    );

    assert_eq!(h.stoat.handle_window_ipc(chord('0')), UpdateEffect::Redraw);
    assert_eq!(h.stoat.diff_tint, 1, "ctrl-0 steps the dial up");
    assert_eq!(h.stoat.handle_window_ipc(chord('9')), UpdateEffect::Redraw);
    assert_eq!(h.stoat.diff_tint, 0, "ctrl-9 steps it back down");

    assert_eq!(h.stoat.handle_window_ipc(chord('8')), UpdateEffect::Redraw);
    assert!(!h.stoat.diff_syntax, "ctrl-8 flips the syntax coloring");
}

/// An in-band claim spells a chord as super plus the digit down the pty
/// rather than as a socket event, so the two deliveries have to land the
/// same handlers.
#[test]
fn the_in_band_super_digits_step_the_tint_dial() {
    let mut h = diff_syntax_harness();
    assert_eq!(
        (h.stoat.diff_tint, h.stoat.diff_syntax),
        (0, true),
        "a session opens with the tint off and syntax on",
    );

    h.stoat.update(inband_chord('0'));
    assert_eq!(h.stoat.diff_tint, 1, "super-0 steps the dial up");
    h.stoat.update(inband_chord('9'));
    assert_eq!(h.stoat.diff_tint, 0, "super-9 steps it back down");
    h.stoat.update(inband_chord('8'));
    assert!(!h.stoat.diff_syntax, "super-8 flips the syntax coloring");
    h.stoat.update(inband_chord('7'));
    assert!(h.stoat.diff_bold, "super-7 flips the bold");
    h.stoat.update(inband_chord('6'));
    assert!(h.stoat.diff_underline, "super-6 flips the underline");

    h.stoat.diff_tint = 2;
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(false);
    h.stoat.update(inband_chord('0'));
    h.stoat.update(inband_chord('9'));
    h.stoat.update(inband_chord('8'));
    h.stoat.update(inband_chord('7'));
    h.stoat.update(inband_chord('6'));

    assert_eq!(
        (
            h.stoat.diff_tint,
            h.stoat.diff_syntax,
            h.stoat.diff_bold,
            h.stoat.diff_underline,
        ),
        (2, false, true, true),
        "off the diff view the in-band digits leave every dial alone",
    );
}

/// A digit chord as it arrives over the pty, which is exactly super on the
/// digit. The same press over the window socket is [`chord`].
fn inband_chord(ch: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::SUPER))
}

/// The terminal forwards the chord on the zoom claim rather than on what is
/// on screen, so the flag has to defend its own scope.
#[test]
fn the_syntax_chord_outside_the_diff_view_changes_nothing() {
    let mut h = diff_syntax_harness();
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(false);

    assert_eq!(
        (h.stoat.handle_window_ipc(chord('8')), h.stoat.diff_syntax),
        (UpdateEffect::None, true),
        "a chord over a plain pane leaves the flag and the frame alone"
    );
}

/// Weight marks a change span without spending the color that the tint and
/// the syntax use, so the chord bolds every span of any kind in both columns
/// and nothing around them.
#[test]
fn the_seven_chord_bolds_every_change_span() {
    let mut h = diff_syntax_harness();
    assert_eq!(
        (
            h.stoat.diff_bold,
            glyphs_with(&h, 68..120, "fn new", Modifier::BOLD)
        ),
        (false, String::new()),
        "a session opens with the bold off, and a code change carries none"
    );

    assert_eq!(h.stoat.handle_window_ipc(chord('7')), UpdateEffect::Redraw);
    h.snapshot();
    assert_eq!(
        (
            glyphs_with(&h, 68..120, "fn new", Modifier::BOLD),
            glyphs_with(&h, 8..59, "fn old", Modifier::BOLD),
            glyphs_with(&h, 68..120, "fn keep", Modifier::BOLD),
        ),
        ("new".to_string(), "old".to_string(), String::new()),
        "each column bolds its changed word, and the context row stays plain"
    );

    h.stoat.handle_window_ipc(chord('7'));
    h.snapshot();
    assert_eq!(
        (
            h.stoat.diff_bold,
            glyphs_with(&h, 68..120, "fn new", Modifier::BOLD)
        ),
        (false, String::new()),
        "a second chord clears it"
    );
}

/// The terminal forwards the chord on the zoom claim rather than on what is
/// on screen, so the bold flag defends its own scope as the syntax flag does.
#[test]
fn the_bold_chord_outside_the_diff_view_changes_nothing() {
    let mut h = diff_syntax_harness();
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(false);

    assert_eq!(
        (h.stoat.handle_window_ipc(chord('7')), h.stoat.diff_bold),
        (UpdateEffect::None, false),
        "a chord over a plain pane leaves the flag and the frame alone"
    );
}

/// An underline marks a change span on every theme, whether it blends or not,
/// so the chord underlines every span in both columns and nothing around them.
#[test]
fn the_six_chord_underlines_every_change_span() {
    let mut h = diff_syntax_harness();
    assert_eq!(
        (
            h.stoat.diff_underline,
            glyphs_with(&h, 68..120, "fn new", Modifier::UNDERLINED),
        ),
        (false, String::new()),
        "a session opens with the underline off, and a blending theme draws none"
    );

    assert_eq!(h.stoat.handle_window_ipc(chord('6')), UpdateEffect::Redraw);
    h.snapshot();
    assert_eq!(
        (
            glyphs_with(&h, 68..120, "fn new", Modifier::UNDERLINED),
            glyphs_with(&h, 8..59, "fn old", Modifier::UNDERLINED),
            glyphs_with(&h, 68..120, "fn keep", Modifier::UNDERLINED),
        ),
        ("new".to_string(), "old".to_string(), String::new()),
        "each column underlines its changed word, and the context row stays plain"
    );

    h.stoat.handle_window_ipc(chord('6'));
    h.snapshot();
    assert_eq!(
        (
            h.stoat.diff_underline,
            glyphs_with(&h, 68..120, "fn new", Modifier::UNDERLINED),
        ),
        (false, String::new()),
        "a second chord clears it"
    );
}

/// The terminal forwards the chord on the zoom claim rather than on what is
/// on screen, so the underline flag defends its own scope as the syntax flag
/// does.
#[test]
fn the_underline_chord_outside_the_diff_view_changes_nothing() {
    let mut h = diff_syntax_harness();
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(false);

    assert_eq!(
        (
            h.stoat.handle_window_ipc(chord('6')),
            h.stoat.diff_underline
        ),
        (UpdateEffect::None, false),
        "a chord over a plain pane leaves the flag and the frame alone"
    );
}

/// The flag subtracts from the diff view alone, so a plain pane keeps the
/// color the session toggle gave it.
#[test]
fn a_plain_pane_keeps_its_color_while_the_diff_toggle_is_off() {
    let mut h = diff_syntax_harness();
    h.stoat.handle_window_ipc(chord('8'));
    h.snapshot();
    assert!(!h.stoat.diff_syntax, "the toggle is off");

    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(false);
    h.snapshot();

    // The keyword and the name it declares carry different scopes, so their
    // colors part exactly where something is coloring tokens. Reading two
    // glyphs rather than a column range keeps the gutter out of the answer.
    let keyword = glyph_fg(&h, "fn keep", "f");
    let name = glyph_fg(&h, "fn keep", "k");
    assert_ne!(
        keyword, name,
        "the same file outside the diff view still paints its tokens"
    );
}

/// A digit the diff view does not claim is a chord the terminal forwarded
/// on the claim that nothing here answers, so it must not redraw or move
/// any state.
#[test]
fn an_unclaimed_digit_chord_is_a_no_op() {
    let mut h = diff_syntax_harness();

    assert_eq!(
        (h.stoat.handle_window_ipc(chord('5')), h.stoat.diff_syntax),
        (UpdateEffect::None, true),
        "an unspoken-for digit changes nothing"
    );
}

/// An open modal owns the combo, so the panes behind it must not move.
#[test]
fn a_zoom_step_with_a_modal_open_zooms_it_and_leaves_the_panes_alone() {
    use stoat_action::OpenFileFinder;

    let mut h = crate::test_harness::TestHarness::with_size(101, 40);
    let left = h.stoat.active_workspace().panes.focus();
    h.stoat
        .active_workspace_mut()
        .panes
        .split(crate::pane::Axis::Vertical);
    h.stoat.active_workspace_mut().panes.set_focus(left);
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();

    h.stoat.handle_window_ipc(zoom(1));

    assert_eq!(
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
        1,
        "the open finder took the step"
    );
    assert_eq!(
        h.stoat.active_workspace().panes.pane(left).area.width,
        50,
        "and the pane behind it kept its share"
    );
}

/// The zoom press as it arrives over the pty, which is exactly super on `=`
/// or `-`. The same press over the window socket is [`zoom`].
fn inband_zoom(delta: i32) -> Event {
    let ch = if delta > 0 { '=' } else { '-' };
    Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::SUPER))
}

/// The press has to mean the same thing whichever transport carried it, so
/// the two are run against separate harnesses and compared rather than
/// checked against a number written here.
#[test]
fn an_inband_zoom_press_steps_a_modal_like_the_socket_event_does() {
    let mut over_pty = zoomable_finder();
    let mut over_socket = zoomable_finder();

    over_pty.stoat.update(inband_zoom(1));
    over_socket.stoat.handle_window_ipc(zoom(1));

    assert_eq!(
        (
            over_pty.stoat.modal_zoom.clone(),
            finder_box(&over_pty),
            over_pty.stoat.modal_zoom.is_empty(),
        ),
        (
            over_socket.stoat.modal_zoom.clone(),
            finder_box(&over_socket),
            false,
        ),
        "the press moved the modal, and moved it the same way over both",
    );
}

/// With nothing over the panes the press is a pane resize, the same as the
/// socket event is. Reaching that means passing every reader below it: the
/// keymap alone would type the character instead.
#[test]
fn an_inband_zoom_press_with_no_modal_open_resizes_the_focused_pane() {
    let mut h = crate::test_harness::TestHarness::with_size(101, 40);
    let left = h.stoat.active_workspace().panes.focus();
    let right = h
        .stoat
        .active_workspace_mut()
        .panes
        .split(crate::pane::Axis::Vertical);
    h.stoat.active_workspace_mut().panes.set_focus(left);
    let width = |h: &crate::test_harness::TestHarness, id| {
        h.stoat.active_workspace().panes.pane(id).area.width
    };
    assert_eq!((width(&h, left), width(&h, right)), (50, 50));

    h.stoat.update(inband_zoom(-1));

    assert_eq!(
        (width(&h, left), width(&h, right)),
        (40, 60),
        "the focused pane gave a step to its neighbor"
    );

    // Exactly super is what the host writes. A press carrying more is some
    // other chord, and taking it here would shadow whatever the keymap has
    // for it.
    h.stoat.update(Event::Key(KeyEvent::new(
        KeyCode::Char('-'),
        KeyModifiers::SUPER | KeyModifiers::CONTROL,
    )));

    assert_eq!(
        (width(&h, left), width(&h, right)),
        (40, 60),
        "a press with more than super held is not the zoom combo"
    );
}

/// A macro replays what the user typed, and over the socket the press is
/// never typing at all. Capturing it here would make a replay zoom where the
/// recording did not.
#[test]
fn a_macro_recorded_around_an_inband_zoom_press_does_not_carry_it() {
    use stoat_action as action;

    let mut h = crate::test_harness::TestHarness::with_size(101, 40);
    action_handlers::dispatch(&mut h.stoat, &action::RecordMacro);

    h.stoat.update(inband_zoom(1));
    h.stoat.update(Event::Key(KeyEvent::new(
        KeyCode::Char('j'),
        KeyModifiers::NONE,
    )));

    assert_eq!(
        h.stoat
            .macro_recording
            .as_ref()
            .expect("recording")
            .keys
            .iter()
            .map(|key| key.code)
            .collect::<Vec<_>>(),
        [KeyCode::Char('j')],
        "the ordinary key was recorded and the zoom press was not",
    );
}

/// A finder over a terminal and a content size that pin its box arithmetic:
/// `modal_box` gives it width `(56 + 6z).clamp(40, 58)` and height
/// `(32 + 7z).clamp(12, 68)`, so the box stops growing at level 6 and stops
/// shrinking at level -3, both well inside the ledger's own range.
fn zoomable_finder() -> crate::test_harness::TestHarness {
    use stoat_action::OpenFileFinder;

    let mut h = crate::test_harness::TestHarness::with_size(60, 70);
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();
    h.stoat
        .file_finder
        .as_mut()
        .expect("finder open")
        .content_size = (120, 8);
    h
}

/// The box the finder draws at the level the ledger currently holds for it.
fn finder_box(h: &crate::test_harness::TestHarness) -> Option<Rect> {
    mouse::open_modal_box(
        &h.stoat,
        ModalKind::FileFinder,
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
    )
}

/// A modal saturates against the screen long before the ledger runs out, and
/// steps the box cannot take must not be counted -- otherwise the user has
/// to unwind invisible levels before the modal moves again.
#[test]
fn zoom_steps_stop_where_the_modal_box_stops_growing() {
    let mut h = zoomable_finder();

    for _ in 0..20 {
        h.stoat.handle_window_ipc(zoom(1));
    }
    assert_eq!(
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
        6,
        "growing stops at the last level that moves the box, not at MODAL_ZOOM_MAX"
    );
    assert_eq!(
        finder_box(&h),
        Some(Rect::new(1, 1, 58, 68)),
        "which is the largest box the area allows"
    );

    h.stoat.handle_window_ipc(zoom(-1));

    assert_eq!(
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
        5,
        "so the next step back lands one level down"
    );
    assert_eq!(
        finder_box(&h),
        Some(Rect::new(1, 1, 58, 67)),
        "and shrinks the modal on that first press"
    );
}

#[test]
fn zoom_steps_stop_where_the_modal_box_stops_shrinking() {
    let mut h = zoomable_finder();

    for _ in 0..20 {
        h.stoat.handle_window_ipc(zoom(-1));
    }
    assert_eq!(
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
        -3,
        "shrinking stops at the last level that moves the box, not at MODAL_ZOOM_MIN"
    );
    assert_eq!(
        finder_box(&h),
        Some(Rect::new(10, 29, 40, 12)),
        "which is the smallest box the modal allows"
    );

    h.stoat.handle_window_ipc(zoom(1));

    assert_eq!(
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
        -2,
        "so the next step forward lands one level up"
    );
    assert_eq!(
        finder_box(&h),
        Some(Rect::new(8, 26, 44, 18)),
        "and grows the modal on that first press"
    );
}

/// Nothing rewrites the ledger when the terminal shrinks, so a level left
/// over from a larger one sits past what the box can take. The first press
/// has to re-enter the range rather than spend itself on a counter.
#[test]
fn a_stale_zoom_level_moves_the_modal_on_its_first_step() {
    let mut h = zoomable_finder();
    h.stoat
        .modal_zoom
        .insert(ModalKind::FileFinder, MODAL_ZOOM_MAX);

    h.stoat.handle_window_ipc(zoom(-1));

    assert_eq!(
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
        5,
        "the stale level clamps into range before the step applies"
    );
    assert_eq!(
        finder_box(&h),
        Some(Rect::new(1, 1, 58, 67)),
        "so the box is a row shorter than the one the stale level drew"
    );
}

/// An area too small to host the modal has no box to measure, leaving the
/// ledger range as the only bound on a step.
#[test]
fn modal_zoom_steps_clamp_at_both_ends() {
    use stoat_action::OpenFileFinder;

    let mut h = crate::test_harness::TestHarness::with_size(30, 20);
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();
    assert_eq!(
        mouse::open_modal_box(&h.stoat, ModalKind::FileFinder, 0),
        None,
        "the finder does not fit a terminal this small"
    );

    for _ in 0..20 {
        h.stoat.handle_window_ipc(zoom(1));
    }
    assert_eq!(
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
        MODAL_ZOOM_MAX,
        "growing stops at the ceiling"
    );

    for _ in 0..40 {
        h.stoat.handle_window_ipc(zoom(-1));
    }
    assert_eq!(
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
        MODAL_ZOOM_MIN,
        "and shrinking stops at the floor"
    );
}

/// Kinds read their share independently, so one modal's dragged separator
/// must not move another's, and a kind never dragged keeps the layout's own
/// default.
#[test]
fn a_dragged_share_is_read_back_per_kind() {
    let mut h = crate::test_harness::TestHarness::with_size(101, 40);

    assert_eq!(
        modal_split_percent(&h.stoat.modal_split, ModalKind::FileFinder),
        crate::render::picker::DEFAULT_LIST_PERCENT,
        "an untouched kind sits at the default"
    );

    h.stoat.modal_split.insert(ModalKind::FileFinder, 65);

    assert_eq!(
        modal_split_percent(&h.stoat.modal_split, ModalKind::FileFinder),
        65,
        "the stored share reads back"
    );
    assert_eq!(
        modal_split_percent(&h.stoat.modal_split, ModalKind::CommitPicker),
        crate::render::picker::DEFAULT_LIST_PERCENT,
        "and its sibling kinds are untouched"
    );
}

/// A modal already sized to its content has nothing to zoom, but it still
/// owns the combo, so the step must not fall through to the panes it hides.
#[test]
fn a_zoomless_modal_swallows_the_step() {
    let mut h = crate::test_harness::TestHarness::with_size(101, 40);
    let left = h.stoat.active_workspace().panes.focus();
    h.stoat
        .active_workspace_mut()
        .panes
        .split(crate::pane::Axis::Vertical);
    h.stoat.active_workspace_mut().panes.set_focus(left);
    h.stoat.quit_all_confirm = Some(QuitAllConfirm::new(&[], std::path::Path::new("/")));

    assert_eq!(
        h.stoat.handle_window_ipc(zoom(1)),
        UpdateEffect::None,
        "nothing changed, so no redraw is owed"
    );
    assert_eq!(
        h.stoat.active_workspace().panes.pane(left).area.width,
        50,
        "the panes behind the picker kept their shares"
    );
}

#[test]
fn window_ipc_focused_moves_focus_to_and_from_the_windowed_pane() {
    let (mut stoat, detached) = stoat_with_detached_pane(3);
    let split = stoat.active_workspace().panes.split_pane_ids()[0];
    stoat.active_workspace_mut().panes.set_focus(split);

    stoat.handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Focused { window: 3 }));
    assert_eq!(
        stoat.active_workspace().panes.focus(),
        detached,
        "focused(n) focuses the windowed pane"
    );

    stoat.handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Focused { window: 0 }));
    assert_eq!(
        stoat.active_workspace().panes.focus(),
        split,
        "focused(0) returns focus to the split layout"
    );
}

/// The buffer shown in the focused editor.
fn focused_buffer(h: &crate::test_harness::TestHarness) -> BufferId {
    let (editor_id, _) = h.stoat.focused_editor_ids().expect("focused editor");
    h.stoat
        .active_workspace()
        .editors
        .get(editor_id)
        .expect("editor exists")
        .buffer_id
}

#[test]
fn window_ipc_side_buttons_walk_the_jumplist() {
    use crate::test_harness::TestHarness;
    use stoatty_protocol::window_ipc::MouseButton as IpcMouseButton;

    let mut h = TestHarness::with_size(40, 6);
    let a = h.write_file("a.rs", "aaaa\nbbbb\n");
    let b = h.write_file("b.rs", "xxxx\nyyyy\n");

    h.open_file(&a);
    h.type_keys("l");
    let a_buffer = focused_buffer(&h);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.open_file(&b);
    let b_buffer = focused_buffer(&h);
    h.type_keys("l");

    let side_event = |kind| {
        WindowIpc::Event(WindowIpcEvent::Mouse {
            window: 0,
            kind,
            col: 0,
            row: 0,
            mods: 0,
        })
    };

    h.stoat
        .handle_window_ipc(side_event(MouseKind::Press(IpcMouseButton::Back)));
    assert_eq!(
        (focused_buffer(&h), h.primary_head_offset()),
        (a_buffer, 1),
        "back re-shows a.rs at the saved offset"
    );

    h.stoat
        .handle_window_ipc(side_event(MouseKind::Press(IpcMouseButton::Forward)));
    assert_eq!(
        (focused_buffer(&h), h.primary_head_offset()),
        (b_buffer, 1),
        "forward returns to where the jump left b.rs"
    );

    h.stoat
        .handle_window_ipc(side_event(MouseKind::Release(IpcMouseButton::Back)));
    assert_eq!(
        (focused_buffer(&h), h.primary_head_offset()),
        (b_buffer, 1),
        "a release moves nothing, so one click walks one entry"
    );
}

/// The buttons resolve the keymap before the jumplist, so a mode that
/// binds them speaks for the press. The pinned goto chord is the shipped
/// case, where they walk hunks back as its n arm and forward as its p arm.
#[test]
fn window_ipc_side_buttons_walk_changes_in_a_pinned_goto_chord() {
    use crate::test_harness::TestHarness;
    use stoatty_protocol::window_ipc::MouseButton as IpcMouseButton;

    let mut h = TestHarness::with_size(40, 20);
    let workdir = std::path::PathBuf::from("/repo");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\ne\n", "a\nX\nc\nd\nY\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("a.rs"));
    h.settle_diff_jobs();

    h.type_keys("space g g");

    let press = |h: &mut TestHarness, button| {
        h.stoat
            .handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Mouse {
                window: 0,
                kind: MouseKind::Press(button),
                col: 0,
                row: 0,
                mods: 0,
            }));
        h.settle();
        (h.primary_head_offset(), h.stoat.focused_mode().to_string())
    };

    assert_eq!(
        (
            press(&mut h, IpcMouseButton::Back),
            press(&mut h, IpcMouseButton::Back),
            press(&mut h, IpcMouseButton::Forward),
        ),
        (
            (2, "space_goto".to_string()),
            (8, "space_goto".to_string()),
            (2, "space_goto".to_string()),
        ),
        "back walks down to each hunk and forward returns up, with the pin holding throughout"
    );
}

#[test]
fn a_side_button_jump_glides_the_view_back_to_the_entry() {
    use crate::{editor_state::ScrollGlide, test_harness::TestHarness};
    use stoatty_protocol::window_ipc::MouseButton as IpcMouseButton;

    let mut h = TestHarness::with_size(40, 12);
    let body: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
    let path = h.write_file("long.rs", &body);
    h.open_file(&path);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);

    // Strand the view far down the file, then clear the glide that put it
    // there so the assertions can only see what the jump itself arms.
    action_handlers::movement::jump_to_offset(&mut h.stoat, body.len());
    let away = {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
        action_handlers::view::ensure_cursor_in_view(editor, 3);
        editor.scroll_glide = ScrollGlide::None;
        editor.scroll_row
    };
    assert!(away > 20, "precondition: the view left the origin");

    h.stoat
        .handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Mouse {
            window: 0,
            kind: MouseKind::Press(IpcMouseButton::Back),
            col: 0,
            row: 0,
            mods: 0,
        }));

    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    assert_eq!(
        editor.scroll_row, 0,
        "back pulls the view to the recorded entry"
    );
    assert_eq!(
        editor.scroll_glide,
        ScrollGlide::Page,
        "the jump arms the glide that ships the cursor anchor"
    );
    assert_eq!(
        editor.scroll_offset, away as f32,
        "the glide starts where the view was, so the cursor arrives from below"
    );
}

#[test]
fn scroll_anim_tick_eases_offset_then_settles() {
    use crate::test_harness::TestHarness;

    let mut h = TestHarness::with_size(40, 12);
    let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
    let path = h.write_file("glide.rs", &body);
    h.open_file(&path);

    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
        action_handlers::view::wheel_scroll(editor, true);
    }
    assert!(
        h.stoat.is_animating(),
        "an armed wheel glide makes the editor animate"
    );

    h.stoat.tick_scroll_anim(0.016);
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert!(
            editor.scroll_offset > 0.0 && editor.scroll_offset < editor.scroll_row as f32,
            "the tick eases the offset up toward the fixed target"
        );
    }

    for _ in 0..1000 {
        if !h.stoat.is_animating() {
            break;
        }
        h.stoat.tick_scroll_anim(0.016);
    }
    assert!(!h.stoat.is_animating(), "repeated ticks settle to rest");
    assert_eq!(
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .scroll_offset,
        3.0,
        "the offset settles on the wheel target"
    );
}

#[test]
fn wheel_coast_drags_cursor_into_view_no_key_snapback() {
    use crate::test_harness::TestHarness;

    let mut h = TestHarness::with_size(40, 12);
    let body: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
    let path = h.write_file("long.rs", &body);
    h.open_file(&path);

    // The cursor starts at the top. Wheel-flick the view downward and let
    // it settle.
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
        for _ in 0..4 {
            action_handlers::view::wheel_scroll(editor, true);
        }
    }
    for _ in 0..1000 {
        if !h.stoat.is_animating() {
            break;
        }
        h.stoat.tick_scroll_anim(0.016);
    }

    let (coasted, row) = {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        let offset = buffer_snapshot.resolve_anchor(&head);
        let row = buffer_snapshot.rope().offset_to_point(offset).row;
        (editor.scroll_row, row)
    };
    assert!(coasted > 3, "the wheel coast advanced the view");
    assert!(
        row >= coasted + 3 && row < coasted + 10,
        "the coast dragged the cursor into the scrolloff band \
             (scroll_row {coasted}, cursor_row {row})",
    );

    // A later cursor motion follows normally. The view does not snap back to
    // where the cursor used to be.
    h.type_keys("k");
    let after = action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .scroll_row;
    assert!(
        after + 2 >= coasted,
        "the view stays at the coasted position rather than snapping back \
             (coasted {coasted}, after {after})",
    );
}

#[test]
fn wheel_glide_keeps_cursor_planted_then_a_mid_glide_key_clamps_it() {
    use crate::test_harness::TestHarness;

    let mut h = TestHarness::with_size(40, 12);
    let body: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
    let path = h.write_file("long.rs", &body);
    h.open_file(&path);

    let head_row = |h: &mut TestHarness| -> u32 {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        let offset = buffer_snapshot.resolve_anchor(&head);
        buffer_snapshot.rope().offset_to_point(offset).row
    };

    let cursor_before = head_row(&mut h);
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
        for _ in 0..4 {
            action_handlers::view::wheel_scroll(editor, true);
        }
    }
    // One tick keeps the glide in flight. The selection has not moved.
    h.stoat.tick_scroll_anim(0.016);
    assert!(h.stoat.is_animating(), "the wheel glide is still in flight");
    assert_eq!(
        head_row(&mut h),
        cursor_before,
        "mid-glide the selection stays anchored to its original line"
    );

    // A key pressed mid-glide clamps the cursor into the landing band without
    // snapping the view back up to the stranded cursor.
    let scroll_before = action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .scroll_row;
    h.type_keys("k");
    let scroll_after = action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .scroll_row;
    assert!(
        scroll_after + 2 >= scroll_before,
        "the mid-glide key does not snap the view backward \
             (before {scroll_before}, after {scroll_after})",
    );
    assert!(
        head_row(&mut h) > cursor_before,
        "the mid-glide key clamped the cursor down into the landing viewport"
    );
}

#[test]
fn wheel_glide_rehomes_the_cursor_when_velocity_drops() {
    use crate::test_harness::TestHarness;

    let mut h = TestHarness::with_size(40, 12);
    let body: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
    let path = h.write_file("long.rs", &body);
    h.open_file(&path);

    let head_row = |h: &mut TestHarness| -> u32 {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        let offset = buffer_snapshot.resolve_anchor(&head);
        buffer_snapshot.rope().offset_to_point(offset).row
    };

    let origin = head_row(&mut h);
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
        for _ in 0..4 {
            action_handlers::view::wheel_scroll(editor, true);
        }
    }

    // The first tick is fast, so the cursor stays anchored to its origin line.
    h.stoat.tick_scroll_anim(0.016);
    assert!(h.stoat.is_animating(), "the wheel glide is still in flight");
    assert_eq!(
        head_row(&mut h),
        origin,
        "at high velocity the cursor stays anchored to its origin line"
    );

    // As the glide slows below the re-home velocity the cursor lands in the
    // scrolloff band while the glide is still in flight.
    let mut ticks = 0;
    while head_row(&mut h) == origin {
        assert!(ticks < 100, "the cursor re-homes before the glide ends");
        h.stoat.tick_scroll_anim(0.016);
        ticks += 1;
    }
    assert!(
        h.stoat.is_animating(),
        "the re-home beat the settle, so the glide is still gliding"
    );
    let landed = head_row(&mut h);
    let band_top = action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .scroll_row;
    assert!(
        landed >= band_top + 3 && landed < band_top + 10,
        "the re-home lands the cursor in the scrolloff band \
             (scroll_row {band_top}, cursor_row {landed})"
    );

    // A further notch drifts the viewport on, so the cursor re-homes a second
    // time mid-glide rather than freezing until the settle.
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        action_handlers::view::wheel_scroll(editor, true);
    }
    let mut ticks = 0;
    while head_row(&mut h) == landed {
        assert!(
            ticks < 100,
            "the cursor re-homes a second time on the slow crawl"
        );
        h.stoat.tick_scroll_anim(0.016);
        ticks += 1;
    }
    assert!(
        h.stoat.is_animating(),
        "the second re-home also lands mid-glide"
    );
    assert!(
        head_row(&mut h) > landed,
        "the second re-home advanced the cursor further down the band"
    );
}

fn pane_scroll_state(h: &mut crate::test_harness::TestHarness) -> (u32, f32) {
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    (editor.scroll_row, editor.scroll_offset)
}

#[test]
fn wheel_moves_file_finder_selection_not_the_pane() {
    use crate::test_harness::TestHarness;
    use stoat_action::OpenFileFinder;

    let mut h = TestHarness::with_size(80, 24);
    let root = std::path::PathBuf::from("/finder-wheel");
    for name in ["a.rs", "b.rs", "c.rs"] {
        h.fake_fs().insert_file(root.join(name), b"x\n");
    }
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();
    let before = pane_scroll_state(&mut h);

    h.stoat
        .update(mouse_event(MouseEventKind::ScrollDown, 10, 10));

    assert_eq!(
        h.stoat
            .file_finder
            .as_ref()
            .expect("finder open")
            .active_core_ref()
            .picklist
            .selected,
        1,
        "a wheel notch moves the finder selection down",
    );
    assert_eq!(
        pane_scroll_state(&mut h),
        before,
        "the pane beneath does not scroll",
    );
}

#[test]
fn wheel_moves_palette_command_selection_not_the_pane() {
    use crate::test_harness::TestHarness;
    use stoat_action::OpenCommandPalette;

    let mut h = TestHarness::with_size(80, 24);
    let path = h.write_file("f.rs", "x\n");
    h.open_file(&path);
    action_handlers::dispatch(&mut h.stoat, &OpenCommandPalette);
    h.settle();
    let before = pane_scroll_state(&mut h);

    h.stoat
        .update(mouse_event(MouseEventKind::ScrollDown, 10, 10));

    assert_eq!(
        h.stoat
            .command_palette
            .as_ref()
            .expect("palette open")
            .selected,
        1,
        "a wheel notch moves the palette command selection down",
    );
    assert_eq!(
        pane_scroll_state(&mut h),
        before,
        "the pane beneath does not scroll"
    );
}

#[test]
fn wheel_moves_palette_arg_picker_selection() {
    use crate::test_harness::TestHarness;

    let mut h = TestHarness::with_size(80, 24);
    let root = std::path::PathBuf::from("/arg-wheel");
    for name in ["a.rs", "b.rs", "c.rs"] {
        h.fake_fs().insert_file(root.join(name), b"x\n");
    }
    h.stoat.active_workspace_mut().git_root = root;
    h.type_text(":o ");
    h.settle();

    h.stoat
        .update(mouse_event(MouseEventKind::ScrollDown, 10, 10));

    let selected = h
        .stoat
        .command_palette
        .as_ref()
        .expect("palette open")
        .arg_picker
        .as_ref()
        .expect("arg picker active")
        .core
        .picklist
        .selected;
    assert_eq!(
        selected, 1,
        "a wheel notch moves the arg picker selection down"
    );
}

/// Open a 40-line document, then a file finder over a four-entry workspace,
/// and return the finder's list rect. The document beneath stays focused so
/// callers can assert a swallowed click never disturbs its cursor.
fn open_finder_with_four(h: &mut crate::test_harness::TestHarness) -> Rect {
    use stoat_action::OpenFileFinder;

    let doc = h.seed_long_file("under.rs", 40);
    h.open_file(&doc);

    let root = std::path::PathBuf::from("/click-finder");
    // Each file previews long enough to scroll, so a wheel over the preview
    // has an observable effect whichever entry is selected.
    let long: String = (0..80).map(|i| format!("line {i}\n")).collect();
    for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        h.fake_fs().insert_file(root.join(name), long.as_bytes());
    }
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();

    finder_layout(h).list
}

fn finder_selected(h: &crate::test_harness::TestHarness) -> usize {
    h.stoat
        .file_finder
        .as_ref()
        .expect("finder open")
        .active_core_ref()
        .picklist
        .selected
}

/// The box sizes to the whole candidate list, so narrowing the query must
/// not resize it out from under the user still typing that query.
#[test]
fn the_finder_box_sizes_to_its_base_list_and_holds_while_filtering() {
    use stoat_action::OpenFileFinder;

    let mut h = crate::test_harness::TestHarness::with_size(140, 60);
    let root = std::path::PathBuf::from("/sized-finder");
    for i in 0..50 {
        h.fake_fs()
            .insert_file(root.join(format!("f{i}.rs")), b"fn main() {}");
    }
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();
    h.snapshot();

    let opened = finder_layout(&h).modal;
    assert_eq!(
        opened.height, 54,
        "fifty rows plus four chrome rows outgrow the recommended 32"
    );

    h.type_text("f1");
    h.settle();
    h.snapshot();

    assert!(
        finder_filtered_len(&h) < 50,
        "the filter has to actually narrow the list for the box to be held over anything"
    );
    assert_eq!(
        finder_layout(&h).modal,
        opened,
        "but the box stays exactly where it opened"
    );
}

fn finder_filtered_len(h: &crate::test_harness::TestHarness) -> usize {
    h.stoat
        .file_finder
        .as_ref()
        .expect("finder open")
        .active_core_ref()
        .picklist
        .filtered
        .len()
}

#[test]
fn click_finder_row_moves_selection_not_the_pane() {
    use crossterm::event::MouseButton;

    let mut h = crate::test_harness::TestHarness::with_size(80, 24);
    let list = open_finder_with_four(&mut h);
    let before = h.stoat.focused_cursor_pos();

    // The third visible row is two below the list top.
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        list.x + 1,
        list.y + 2,
    ));

    assert_eq!(finder_selected(&h), 2, "clicking the third row selects it");
    assert_eq!(
        h.stoat.focused_cursor_pos(),
        before,
        "the click never reaches the buffer beneath",
    );
}

#[test]
fn click_outside_modal_is_swallowed() {
    use crossterm::event::MouseButton;

    let mut h = crate::test_harness::TestHarness::with_size(80, 24);
    open_finder_with_four(&mut h);
    let before = h.stoat.focused_cursor_pos();

    // Row 0 sits above the centered modal.
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0));

    assert!(
        h.stoat.file_finder.is_some(),
        "an outside click does not dismiss the finder"
    );
    assert_eq!(finder_selected(&h), 0, "the selection is unchanged");
    assert_eq!(
        h.stoat.focused_cursor_pos(),
        before,
        "the buffer is untouched"
    );
}

#[test]
fn click_empty_row_below_last_item_is_swallowed() {
    use crossterm::event::MouseButton;

    let mut h = crate::test_harness::TestHarness::with_size(80, 24);
    let list = open_finder_with_four(&mut h);

    // Only four items are listed, so the sixth row is empty.
    assert!(list.height > 5, "the list is tall enough for an empty row");
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        list.x + 1,
        list.y + 5,
    ));

    assert_eq!(
        finder_selected(&h),
        0,
        "a click on an empty row moves nothing"
    );
    assert!(
        h.stoat.file_finder.is_some(),
        "and does not dismiss the finder"
    );
}

fn finder_preview_id(h: &crate::test_harness::TestHarness) -> EditorId {
    h.stoat
        .file_finder
        .as_ref()
        .expect("finder open")
        .active_core_ref()
        .preview
        .editor
}

#[test]
fn wheel_over_finder_preview_scrolls_it_not_the_list() {
    let mut h = crate::test_harness::TestHarness::with_size(100, 30);
    open_finder_with_four(&mut h);
    action_handlers::sync_file_finder_preview(&mut h.stoat);
    h.settle();

    let preview = finder_layout(&h)
        .preview
        .expect("the preview pane is present at this width");
    let preview_id = finder_preview_id(&h);

    h.stoat.update(mouse_event(
        MouseEventKind::ScrollDown,
        preview.x + preview.width / 2,
        preview.y + preview.height / 2,
    ));

    assert_eq!(
        finder_selected(&h),
        0,
        "a wheel over the preview leaves the list selection put"
    );
    let scroll_row = h
        .stoat
        .active_workspace()
        .editors
        .get(preview_id)
        .expect("preview editor")
        .scroll_row;
    assert!(scroll_row > 0, "the wheel scrolls the preview down");
}

#[test]
fn wheel_over_palette_arg_preview_scrolls_it_not_the_list() {
    let mut h = crate::test_harness::TestHarness::with_size(100, 30);
    let root = std::path::PathBuf::from("/arg-preview");
    let long: String = (0..80).map(|i| format!("line {i}\n")).collect();
    h.fake_fs().insert_file(root.join("a.rs"), long.as_bytes());
    for name in ["b.rs", "c.rs"] {
        h.fake_fs().insert_file(root.join(name), b"x\n");
    }
    h.stoat.active_workspace_mut().git_root = root;
    h.type_text(":o ");
    h.settle();

    let (rows, zoom) = palette_sizing(&h);
    let preview = crate::render::command_palette::palette_arg_body(h.stoat.size(), rows, zoom)
        .and_then(|(_, preview)| preview)
        .expect("the arg preview pane is present at this width");
    let preview_id = h
        .stoat
        .command_palette
        .as_ref()
        .expect("palette open")
        .arg_picker
        .as_ref()
        .expect("arg picker active")
        .active_core_ref()
        .preview
        .editor;

    h.stoat.update(mouse_event(
        MouseEventKind::ScrollDown,
        preview.x + preview.width / 2,
        preview.y + preview.height / 2,
    ));

    let selected = h
        .stoat
        .command_palette
        .as_ref()
        .expect("palette open")
        .arg_picker
        .as_ref()
        .expect("arg picker active")
        .active_core_ref()
        .picklist
        .selected;
    assert_eq!(
        selected, 0,
        "a wheel over the preview leaves the arg selection put"
    );
    let scroll_row = h
        .stoat
        .active_workspace()
        .editors
        .get(preview_id)
        .expect("preview editor")
        .scroll_row;
    assert!(scroll_row > 0, "the wheel scrolls the preview down");
}

#[test]
fn preview_scroll_resets_on_selection_change() {
    let mut h = crate::test_harness::TestHarness::with_size(100, 30);
    open_finder_with_four(&mut h);
    action_handlers::sync_file_finder_preview(&mut h.stoat);
    let preview_id = finder_preview_id(&h);

    {
        let editor = h
            .stoat
            .active_workspace_mut()
            .editors
            .get_mut(preview_id)
            .expect("preview editor");
        editor.scroll_offset = 5.0;
        editor.scroll_row = 5;
        editor.scroll_glide = ScrollGlide::Wheel;
    }

    action_handlers::file_finder_move_selection(&mut h.stoat, 1);
    action_handlers::sync_file_finder_preview(&mut h.stoat);

    let editor = h
        .stoat
        .active_workspace()
        .editors
        .get(preview_id)
        .expect("preview editor");
    assert_eq!(
        (editor.scroll_row, editor.scroll_offset, editor.scroll_glide,),
        (0, 0.0, ScrollGlide::None),
        "a new selection resets the preview scroll to the top",
    );
}

#[test]
fn glide_tick_eases_offset_to_target_and_clears_glide() {
    use crate::test_harness::TestHarness;

    let mut h = TestHarness::with_size(40, 12);
    let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
    let path = h.write_file("glide.rs", &body);
    h.open_file(&path);
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
        editor.scroll_row = 10;
        editor.scroll_offset = 0.0;
        editor.scroll_glide = ScrollGlide::Page;
    }
    assert!(h.stoat.is_animating(), "a page glide animates");

    h.stoat.tick_scroll_anim(0.016);
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert!(
            editor.scroll_offset > 0.0 && editor.scroll_offset < 10.0,
            "tick eases the offset toward the target"
        );
        assert_eq!(editor.scroll_row, 10, "scroll_row stays the fixed target");
    }

    for _ in 0..1000 {
        if !h.stoat.is_animating() {
            break;
        }
        h.stoat.tick_scroll_anim(0.016);
    }
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    assert_eq!(
        editor.scroll_glide,
        ScrollGlide::None,
        "the glide clears on settle"
    );
    assert_eq!(
        editor.scroll_offset, 10.0,
        "the offset settles on the target"
    );
}

/// A wheel rest between two rows is a target like any other, so the glide
/// eases onto it and stops rather than rounding to the row grid.
#[test]
fn glide_tick_settles_on_a_fractional_target() {
    use crate::test_harness::TestHarness;

    let mut h = TestHarness::with_size(40, 12);
    let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
    let path = h.write_file("glide.rs", &body);
    h.open_file(&path);
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
        editor.scroll_row = 10;
        editor.scroll_frac = 0.5;
        editor.scroll_offset = 0.0;
        editor.scroll_glide = ScrollGlide::Wheel;
    }

    for _ in 0..1000 {
        if !h.stoat.is_animating() {
            break;
        }
        h.stoat.tick_scroll_anim(0.016);
    }

    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    assert_eq!(
        (editor.scroll_offset, editor.scroll_glide),
        (10.5, ScrollGlide::None),
        "the offset rests half a row down and the glide is over",
    );
    assert!(!h.stoat.is_animating(), "so nothing asks for another tick");
}

#[test]
fn glide_tick_snaps_a_gap_wider_than_three_viewports() {
    use crate::test_harness::TestHarness;

    let mut h = TestHarness::with_size(40, 12);
    let body: String = (0..500).map(|i| format!("line {i}\n")).collect();
    let path = h.write_file("glide.rs", &body);
    h.open_file(&path);
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
        editor.scroll_row = 100;
        editor.scroll_offset = 0.0;
        editor.scroll_glide = ScrollGlide::Page;
    }

    h.stoat.tick_scroll_anim(0.016);

    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    assert_eq!(
        editor.scroll_offset, 100.0,
        "a gap wider than three viewports snaps straight to the target"
    );
    assert_eq!(
        editor.scroll_glide,
        ScrollGlide::None,
        "and clears the glide"
    );
}

#[test]
fn cold_build_shard_merges_into_the_workspace_graph() {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;

    let workspace = stoat.active_workspace;
    let shard = codegraph::FileShard {
        content_hash: [0u8; 32],
        symbols: vec![codegraph::Symbol {
            key: codegraph::SymbolKey([1u8; 16]),
            file: codegraph::FileId(0),
            name: "foo".to_string(),
            kind: stoat_language::SymbolKind::Function,
            container: vec![],
            def_range: 0..11,
            name_range: 3..6,
            body_hash: [0u8; 32],
        }],
        edges: vec![],
    };
    stoat
        .index_update_tx
        .send(IndexUpdate::Shard {
            workspace,
            rel_path: "a.rs".to_string(),
            shard,
        })
        .unwrap();

    stoat.drain_index_updates();

    let ws = stoat.active_workspace();
    assert_eq!(ws.index_generation, 1);
    assert_eq!(
        ws.code_graph.symbol_at(codegraph::FileId(0), 5),
        Some(codegraph::SymbolKey([1u8; 16]))
    );
}

#[test]
fn non_repo_root_skips_the_index_build() {
    use crate::host::{FakeFs, FakeGit};

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/scratch"),
    );
    stoat.persistence_disabled = true;

    let fs = Arc::new(FakeFs::new());
    fs.insert_file("/scratch/a.rs", "fn foo() {}\n");
    stoat.set_fs_host(fs);
    stoat.set_git_host(Arc::new(FakeGit::new()));

    stoat.start_index_build();
    scheduler.run_until_parked();
    stoat.drain_index_updates();

    let ws = stoat.active_workspace();
    assert_eq!(
        ws.index_generation, 0,
        "a non-repo root builds no index shards",
    );
    assert_eq!(
        ws.code_graph
            .symbol_at(crate::code_index::build::file_id("a.rs"), 5),
        None,
        "no symbol is indexed when the workspace root is not a repo",
    );
}

#[test]
fn index_build_watches_the_repo_root_off_the_render_thread() {
    use crate::host::{FakeFs, FakeFsWatcher, FakeGit};

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );

    let fs = Arc::new(FakeFs::new());
    fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
    stoat.set_fs_host(fs);
    let git = FakeGit::new();
    git.add_repo("/repo");
    stoat.set_git_host(Arc::new(git));
    let watcher = Arc::new(FakeFsWatcher::new());
    stoat.set_fs_watch_host(watcher.clone());

    let git_root = stoat.active_workspace().git_root.clone();
    stoat.start_index_build();
    scheduler.run_until_parked();

    assert!(
        watcher.is_watching(&git_root),
        "the repo root is watched once the blocking registration runs"
    );
}

#[test]
fn a_persistence_disabled_index_build_still_watches_the_repo_root() {
    use crate::host::{FakeFs, FakeFsWatcher, FakeGit};

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;

    let fs = Arc::new(FakeFs::new());
    fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
    stoat.set_fs_host(fs);
    let git = FakeGit::new();
    git.add_repo("/repo");
    stoat.set_git_host(Arc::new(git));
    let watcher = Arc::new(FakeFsWatcher::new());
    stoat.set_fs_watch_host(watcher.clone());

    let git_root = stoat.active_workspace().git_root.clone();
    stoat.start_index_build();
    scheduler.run_until_parked();

    assert!(
        watcher.is_watching(&git_root),
        "the watch reads no state, so disabled persistence keeps it"
    );
}

#[test]
fn batched_reindex_drain_cross_links_like_sequential() {
    let file_a = codegraph::FileId(1);
    let file_b = codegraph::FileId(2);
    let caller = codegraph::SymbolKey([1u8; 16]);
    let callee = codegraph::SymbolKey([2u8; 16]);

    let callees_after = |drain_between: bool| -> Vec<codegraph::SymbolKey> {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;
        let workspace = stoat.active_workspace;

        let symbol = |key, file, name: &str| codegraph::Symbol {
            key,
            file,
            name: name.to_string(),
            kind: stoat_language::SymbolKind::Function,
            container: vec![],
            def_range: 0..10,
            name_range: 3..6,
            body_hash: [0u8; 32],
        };
        let reindex = |file, rel_path: &str, symbols, edges| IndexUpdate::Reindex {
            workspace,
            file,
            rel_path: rel_path.to_string(),
            shard: codegraph::FileShard {
                content_hash: [0u8; 32],
                symbols,
                edges,
            },
            persist: false,
        };

        stoat
            .index_update_tx
            .send(reindex(
                file_a,
                "a.rs",
                vec![symbol(caller, file_a, "caller")],
                vec![codegraph::Edge {
                    from: caller,
                    to: codegraph::Target::Unresolved {
                        name: "callee".to_string(),
                        kind: stoat_language::RefKind::Call,
                    },
                    kind: codegraph::EdgeKind::Calls,
                    site_range: 0..6,
                    confidence: codegraph::Confidence::NameMatch,
                }],
            ))
            .unwrap();
        if drain_between {
            stoat.drain_index_updates();
        }
        stoat
            .index_update_tx
            .send(reindex(
                file_b,
                "b.rs",
                vec![symbol(callee, file_b, "callee")],
                vec![],
            ))
            .unwrap();
        stoat.drain_index_updates();

        stoat.active_workspace().code_graph.step(
            caller,
            codegraph::EdgeKind::Calls,
            codegraph::Dir::Down,
        )
    };

    assert_eq!(
        callees_after(false),
        vec![callee],
        "one batched drain resolves file A's call to file B's definition",
    );
    assert_eq!(
        callees_after(true),
        callees_after(false),
        "batching two reindexes into one drain matches draining them one at a time",
    );
}

/// A cold build queues every shard at once, so the count never engages and
/// one turn merges the whole thing. The time bound is what ends that turn,
/// and it has to end it on its own rather than only alongside the count.
///
/// The bound is read here rather than driven through a drain, since a wall
/// clock does not advance on command, and eight milliseconds of real
/// merging ties the answer to how fast the machine is.
#[test]
fn an_index_turn_ends_on_either_bound() {
    let under = INDEX_DRAIN_BUDGET - std::time::Duration::from_millis(1);

    assert!(
        !index_turn_spent(INDEX_DRAIN_CAP - 1, under),
        "inside both bounds the turn continues",
    );
    assert!(
        index_turn_spent(INDEX_DRAIN_CAP, under),
        "the count ends a turn of updates too cheap for the clock to see",
    );
    assert!(
        index_turn_spent(1, INDEX_DRAIN_BUDGET),
        "and the time ends a turn of one update the count would let run on",
    );
}

#[test]
fn a_capped_drain_leaves_the_remainder_for_the_next_tick() {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;
    let workspace = stoat.active_workspace;

    let total = INDEX_DRAIN_CAP + 1;
    for i in 0..total {
        stoat
            .index_update_tx
            .send(IndexUpdate::Shard {
                workspace,
                rel_path: format!("f{i}.rs"),
                shard: codegraph::FileShard {
                    content_hash: [0u8; 32],
                    symbols: vec![],
                    edges: vec![],
                },
            })
            .unwrap();
    }

    stoat.drain_index_updates();
    assert_eq!(
        stoat.active_workspace().index_generation,
        INDEX_DRAIN_CAP as u64,
        "the drain caps its work and leaves the remainder queued",
    );

    stoat.drain_index_updates();
    assert_eq!(
        stoat.active_workspace().index_generation,
        total as u64,
        "the next drain completes the queued remainder",
    );
}

#[test]
fn reindex_replaces_a_files_symbols_in_the_graph() {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;
    let workspace = stoat.active_workspace;
    let file = codegraph::FileId(7);

    let symbol = |key: u8, name: &str| codegraph::Symbol {
        key: codegraph::SymbolKey([key; 16]),
        file,
        name: name.to_string(),
        kind: stoat_language::SymbolKind::Function,
        container: vec![],
        def_range: 0..11,
        name_range: 3..6,
        body_hash: [0u8; 32],
    };

    stoat
        .index_update_tx
        .send(IndexUpdate::Shard {
            workspace,
            rel_path: "a.rs".to_string(),
            shard: codegraph::FileShard {
                content_hash: [0u8; 32],
                symbols: vec![symbol(1, "foo")],
                edges: vec![],
            },
        })
        .unwrap();
    stoat.drain_index_updates();
    assert_eq!(
        stoat.active_workspace().code_graph.symbol_at(file, 5),
        Some(codegraph::SymbolKey([1u8; 16]))
    );

    stoat
        .index_update_tx
        .send(IndexUpdate::Reindex {
            workspace,
            file,
            rel_path: "a.rs".to_string(),
            shard: codegraph::FileShard {
                content_hash: [9u8; 32],
                symbols: vec![symbol(2, "bar")],
                edges: vec![],
            },
            persist: false,
        })
        .unwrap();
    stoat.drain_index_updates();

    let ws = stoat.active_workspace();
    assert_eq!(
        ws.code_graph.symbol_at(file, 5),
        Some(codegraph::SymbolKey([2u8; 16])),
        "reindex evicts the old symbol and inserts the new one"
    );
    assert_eq!(ws.index_generation, 2);
}

#[test]
fn external_change_reindexes_and_remove_evicts() {
    use crate::host::{FakeFs, FakeFsWatcher, FsEventKind};

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;

    let fs = Arc::new(FakeFs::new());
    fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
    stoat.set_fs_host(fs.clone());
    let watcher = Arc::new(FakeFsWatcher::new());
    stoat.set_fs_watch_host(watcher.clone());

    let path = PathBuf::from("/repo/src/a.rs");
    let file = crate::code_index::build::file_id("src/a.rs");

    let drive = |stoat: &mut Stoat, kind: FsEventKind| {
        watcher.inject(&path, kind);
        debounce::drain_fs_watch_events(stoat);
        scheduler.advance_clock(FS_WATCH_DEBOUNCE);
        debounce::drain_pending_index_edits(stoat);
        scheduler.run_until_parked();
        stoat.drain_index_updates();
    };

    drive(&mut stoat, FsEventKind::Modified);
    assert!(
        stoat
            .active_workspace()
            .code_graph
            .symbol_at(file, 4)
            .is_some(),
        "an external modify indexes the file",
    );

    fs.remove_file(&path).unwrap();
    drive(&mut stoat, FsEventKind::Removed);
    assert_eq!(
        stoat.active_workspace().code_graph.symbol_at(file, 4),
        None,
        "an external remove evicts the file",
    );
}

/// Watches are registered per directory, so which directories get one is
/// the whole question. A recursive watch on the root covered `target/`,
/// `node_modules/`, and the object store, which on a built repo is most of
/// the tree and enough to exhaust the platform's watch limit.
///
/// Stated as the watch set rather than as events, since whether a write
/// under an unwatched directory reports anything is the platform's
/// behavior rather than this code's.
#[test]
fn workspace_watches_cover_the_source_tree_and_the_git_refs() {
    use crate::host::{FakeFs, FakeFsWatcher};

    let fs = FakeFs::new();
    fs.insert_files([
        ("/repo/src/a.rs", "fn a() {}".as_bytes()),
        ("/repo/src/deep/b.rs", "fn b() {}".as_bytes()),
        ("/repo/target/debug/gen.rs", "gen".as_bytes()),
        ("/repo/node_modules/pkg/i.js", "js".as_bytes()),
        ("/repo/.git/refs/heads/main", "sha".as_bytes()),
        ("/repo/.git/objects/ab/cdef", "obj".as_bytes()),
    ]);
    let watcher = FakeFsWatcher::new();

    watch_workspace_dirs(&fs, &watcher, Path::new("/repo"));

    assert_eq!(
        watcher.watched_paths(),
        [
            PathBuf::from("/repo"),
            PathBuf::from("/repo/.git"),
            PathBuf::from("/repo/.git/refs"),
            PathBuf::from("/repo/.git/refs/heads"),
            PathBuf::from("/repo/src"),
            PathBuf::from("/repo/src/deep"),
        ],
        "the source tree plus the three .git directories that carry HEAD, \
             the index, and the branch tips, and nothing from a built tree or \
             the object store",
    );
}

/// A directory created after startup has no watch of its own, which leaves
/// files written into it untracked for the rest of the session. Its create
/// event is the only notice the editor gets.
#[test]
fn a_directory_created_after_startup_gets_its_own_watch() {
    use crate::host::{FakeFs, FakeFsWatcher, FsEventKind};

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;

    let fs = Arc::new(FakeFs::new());
    fs.insert_file("/repo/src/a.rs", "fn a() {}\n");
    stoat.set_fs_host(fs.clone());
    let watcher = Arc::new(FakeFsWatcher::new());
    stoat.set_fs_watch_host(watcher.clone());

    let fresh = PathBuf::from("/repo/src/fresh");
    fs.insert_dir(&fresh);
    watcher.inject(&fresh, FsEventKind::Created);
    debounce::drain_fs_watch_events(&mut stoat);
    assert!(
        watcher.is_watching(&fresh),
        "the new directory is watched from its create event",
    );

    let file = PathBuf::from("/repo/src/a.rs");
    watcher.inject(&file, FsEventKind::Created);
    debounce::drain_fs_watch_events(&mut stoat);
    assert!(
        !watcher.is_watching(&file),
        "a created file needs no watch of its own, its directory has one",
    );
}

/// One external change reads the file once. Deciding whether the change is
/// stale means reading it, and extracting means reading it, so a gate
/// standing outside the job read every changed file twice. A checkout or a
/// formatter run pays that per file.
///
/// The gate moving into the job is also what takes the read off the run
/// loop, but the test scheduler runs blocking work inline, so the count is
/// what is observable here rather than which thread it happened on.
#[test]
fn an_external_change_reads_the_file_once() {
    use crate::host::{FakeFs, FakeFsWatcher, FsEventKind};

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;

    let fs = Arc::new(FakeFs::new());
    fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
    stoat.set_fs_host(fs.clone());
    let watcher = Arc::new(FakeFsWatcher::new());
    stoat.set_fs_watch_host(watcher.clone());

    let path = PathBuf::from("/repo/src/a.rs");
    let reads = || {
        fs.ops()
            .iter()
            .filter(|op| matches!(op, crate::host::FakeFsOp::Read { path: p } if *p == path))
            .count()
    };
    let drive = |stoat: &mut Stoat| {
        watcher.inject(&path, FsEventKind::Modified);
        debounce::drain_fs_watch_events(stoat);
        scheduler.advance_clock(FS_WATCH_DEBOUNCE);
        debounce::drain_pending_index_edits(stoat);
        scheduler.run_until_parked();
        stoat.drain_index_updates();
    };

    // Index it once, so the rounds below have a recorded hash to differ
    // from and to match.
    drive(&mut stoat);
    let file = crate::code_index::build::file_id("src/a.rs");
    assert!(
        stoat
            .active_workspace()
            .code_graph
            .content_hash(file)
            .is_some(),
        "the first round indexed the file",
    );

    let before = reads();
    fs.insert_file("/repo/src/a.rs", "fn foo() {}\nfn bar() {}\n");
    drive(&mut stoat);
    assert_eq!(
        reads(),
        before + 1,
        "a changed file is fingerprinted and extracted from one read",
    );
    assert!(
        stoat
            .active_workspace()
            .code_graph
            .symbol_at(file, 17)
            .is_some(),
        "and the change reached the graph, so the single read did both jobs",
    );

    // An event for a file nothing touched, which is what the watch echo of
    // the editor's own save looks like.
    let before = reads();
    let generation = stoat.active_workspace().index_generation;
    drive(&mut stoat);
    assert_eq!(
        reads(),
        before + 1,
        "an unchanged file is read once to find that out",
    );
    assert_eq!(
        stoat.active_workspace().index_generation,
        generation,
        "and the matching fingerprint stops it before it reindexes",
    );
}

/// One window covers a burst. A checkout naming thousands of files must not
/// allocate a timer for each, so the second path here joins the window the
/// first opened rather than starting its own.
///
/// The clock is what shows it. Both paths drain one window after the burst
/// started, a point at which a timer per path leaves the later path's own
/// window still open.
#[test]
fn a_burst_of_external_changes_shares_one_debounce_window() {
    use crate::host::{FakeFs, FakeFsWatcher, FsEventKind};

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;

    let fs = Arc::new(FakeFs::new());
    fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
    fs.insert_file("/repo/src/b.rs", "fn bar() {}\n");
    stoat.set_fs_host(fs);
    let watcher = Arc::new(FakeFsWatcher::new());
    stoat.set_fs_watch_host(watcher.clone());

    let (a, b) = (
        PathBuf::from("/repo/src/a.rs"),
        PathBuf::from("/repo/src/b.rs"),
    );

    watcher.inject(&a, FsEventKind::Modified);
    debounce::drain_fs_watch_events(&mut stoat);

    // Most of the window elapses, then the second path arrives inside it.
    scheduler.advance_clock(FS_WATCH_DEBOUNCE / 2);
    watcher.inject(&b, FsEventKind::Modified);
    debounce::drain_fs_watch_events(&mut stoat);
    assert_eq!(
        stoat.index_pending_external_edits.len(),
        2,
        "both paths wait on the same window",
    );

    scheduler.advance_clock(FS_WATCH_DEBOUNCE);
    assert!(
        debounce::drain_pending_index_edits(&mut stoat),
        "the window the first path opened closed and carried both",
    );
    assert!(
        stoat.index_pending_external_edits.is_empty(),
        "one drain took the whole burst",
    );

    scheduler.run_until_parked();
    stoat.drain_index_updates();
    let graph = &stoat.active_workspace().code_graph;
    assert!(
        graph
            .content_hash(crate::code_index::build::file_id("src/a.rs"))
            .is_some()
            && graph
                .content_hash(crate::code_index::build::file_id("src/b.rs"))
                .is_some(),
        "both files of the burst reached the graph",
    );
}

#[test]
fn editing_a_buffer_live_reindexes_a_new_calls_edge() {
    use crate::host::FakeFs;

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;
    let fs = Arc::new(FakeFs::new());
    fs.insert_file("/repo/src/a.rs", "fn caller() {}\nfn callee() {}\n");
    stoat.set_fs_host(fs);

    let pane = stoat.active_workspace().panes.focus();
    let buffer_id =
        crate::buffer_lifecycle::open_file_in_pane(&mut stoat, pane, Path::new("/repo/src/a.rs"))
            .expect("open the buffer");

    // A parse arms the index debounce rather than extracting, so the
    // extract lands on a later pass once the buffer has gone quiet. The
    // first two drives spawn the parse and poll its output in, since it is
    // the poll that arms the debounce and the clock has to advance after
    // the window opens rather than before.
    let drive = |stoat: &mut Stoat| {
        stoat.drive_parse_jobs();
        scheduler.run_until_parked();
        stoat.drive_parse_jobs();
        scheduler.run_until_parked();
        scheduler.advance_clock(INDEX_EDIT_DEBOUNCE);
        scheduler.run_until_parked();
        stoat.drive_parse_jobs();
        scheduler.run_until_parked();
        stoat.drain_index_updates();
    };

    drive(&mut stoat);
    let file = crate::code_index::build::file_id("src/a.rs");
    let caller = stoat
        .active_workspace()
        .code_graph
        .symbol_at(file, 5)
        .expect("caller indexed");
    assert!(
        stoat
            .active_workspace()
            .code_graph
            .step(caller, codegraph::EdgeKind::Calls, codegraph::Dir::Down)
            .is_empty(),
        "caller has no callee edge before the edit",
    );

    {
        let ws = stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        buffer.write().expect("poisoned").edit(13..13, "callee();");
    }

    drive(&mut stoat);
    let ws = stoat.active_workspace();
    let caller = ws.code_graph.symbol_at(file, 5).expect("caller reindexed");
    let callee = ws.code_graph.symbol_at(file, 27).expect("callee reindexed");
    assert_eq!(
        ws.code_graph
            .step(caller, codegraph::EdgeKind::Calls, codegraph::Dir::Down),
        vec![callee],
        "the edit's new call appears as a Calls edge in the graph",
    );
}

/// A save is the moment the file on disk matches the buffer, so it is the
/// save's reindex that writes the shard a later open warm-loads. An edit's
/// reindex must not write one, since the disk still disagrees with the
/// buffer it extracted from.
///
/// Read off the update channel rather than off the index directory. The
/// drain resolves that directory through the XDG state dir, and a test
/// whose result turns on the environment resolving one is not repeatable.
#[test]
fn a_save_reindexes_with_persist_where_an_edit_does_not() {
    use crate::host::FakeFs;
    use stoat_action::SaveBuffer;

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    let fs = Arc::new(FakeFs::new());
    fs.insert_file("/repo/src/a.rs", "fn caller() {}\n");
    stoat.set_fs_host(fs);

    let pane = stoat.active_workspace().panes.focus();
    let buffer_id =
        crate::buffer_lifecycle::open_file_in_pane(&mut stoat, pane, Path::new("/repo/src/a.rs"))
            .expect("open the buffer");

    // Nothing drains here, so the updates queue up and each phase reads the
    // ones it produced. A drain resolves the index directory, which is the
    // environment dependency this test exists without.
    let persists = |stoat: &mut Stoat| {
        let mut seen = Vec::new();
        while let Ok(update) = stoat.index_update_rx.try_recv() {
            if let IndexUpdate::Reindex { persist, .. } = update {
                seen.push(persist);
            }
        }
        seen
    };
    let settle = |stoat: &mut Stoat| {
        stoat.drive_parse_jobs();
        scheduler.run_until_parked();
        stoat.drive_parse_jobs();
        scheduler.run_until_parked();
        scheduler.advance_clock(INDEX_EDIT_DEBOUNCE);
        scheduler.run_until_parked();
        stoat.drive_parse_jobs();
        scheduler.run_until_parked();
    };

    {
        let ws = stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        buffer
            .write()
            .expect("poisoned")
            .edit(14..14, "\nfn one() {}");
    }
    settle(&mut stoat);
    assert_eq!(
        persists(&mut stoat),
        [false],
        "the edit's reindex updates the graph and writes nothing"
    );

    assert_eq!(
        action_handlers::dispatch(&mut stoat, &SaveBuffer),
        UpdateEffect::Redraw,
        "the save reached a handler rather than falling through"
    );
    scheduler.run_until_parked();
    // The write streams off the run loop, so the reindex it drives waits on
    // the pump that lands it.
    stoat.drive_pumps();
    // A refused or failed write returns before the enqueue, so a reindex
    // arriving at all is what says the bytes reached disk first.
    assert_eq!(
        persists(&mut stoat),
        [true],
        "the save's reindex carries the shard and manifest entry to disk"
    );
}

#[test]
fn two_parses_inside_the_debounce_window_extract_once() {
    use crate::host::FakeFs;

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );
    stoat.persistence_disabled = true;
    let fs = Arc::new(FakeFs::new());
    fs.insert_file("/repo/src/a.rs", "fn caller() {}\n");
    stoat.set_fs_host(fs);

    let pane = stoat.active_workspace().panes.focus();
    let buffer_id =
        crate::buffer_lifecycle::open_file_in_pane(&mut stoat, pane, Path::new("/repo/src/a.rs"))
            .expect("open the buffer");

    let parse = |stoat: &mut Stoat| {
        stoat.drive_parse_jobs();
        scheduler.run_until_parked();
    };
    let settle = |stoat: &mut Stoat| {
        scheduler.advance_clock(INDEX_EDIT_DEBOUNCE);
        scheduler.run_until_parked();
        stoat.drive_parse_jobs();
        scheduler.run_until_parked();
        stoat.drain_index_updates();
    };
    let edit = |stoat: &mut Stoat, text: &str| {
        let ws = stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        buffer.write().expect("poisoned").edit(14..14, text);
    };

    parse(&mut stoat);
    settle(&mut stoat);
    let before = stoat.active_workspace().index_generation;

    edit(&mut stoat, "\nfn one() {}");
    parse(&mut stoat);
    edit(&mut stoat, "\nfn two() {}");
    parse(&mut stoat);

    assert_eq!(
        stoat.active_workspace().index_generation,
        before,
        "neither parse extracts while the buffer is still being typed in",
    );

    settle(&mut stoat);
    assert_eq!(
        stoat.active_workspace().index_generation,
        before + 1,
        "the two parses collapse into a single extract",
    );

    let file = crate::code_index::build::file_id("src/a.rs");
    let ws = stoat.active_workspace();
    assert!(
        ws.code_graph.symbol_at(file, 19).is_some() && ws.code_graph.symbol_at(file, 31).is_some(),
        "the one extract sees both edits, having read the rope when it fired",
    );
}

#[test]
fn agent_output_feeds_emulator() {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());

    let session: Arc<dyn crate::host::TerminalSession> =
        Arc::new(crate::host::FakeTerminalSession::new());
    let agent_id = stoat.active_workspace_mut().terms.insert(TermSession::new(
        crate::term_screen::TermScreen::new(24, 80),
        session,
        TermSession::next_token(),
    ));
    // Show the agent in the focused pane so its output marks the frame dirty.
    let pane = stoat.active_workspace().panes.focus();
    stoat.active_workspace_mut().panes.pane_mut(pane).view = View::Agent(agent_id);

    let effect = stoat.handle_pty_notification(PtyNotification::TermOutput {
        agent_id,
        data: b"hello".to_vec(),
    });

    assert_eq!(
        effect,
        UpdateEffect::None,
        "a visible agent paces its repaint to the frame tick",
    );
    assert!(stoat.pty_dirty, "the output marked the frame dirty");
    let term = &stoat.active_workspace().terms[agent_id].term;
    let row: String = term.row(0).iter().map(|cell| cell.ch).collect();
    assert!(row.starts_with("hello"), "row: {row:?}");
}

/// Fills the pty channel the way the reader thread does, from a thread and
/// against a visible pane.
///
/// A task on this runtime only runs where the loop awaits, and a select arm
/// that is ready never awaits, so a task could not keep the channel full.
/// Only output the screen shows marks the frame dirty, and only a dirty
/// frame arms the timer arm at all.
///
/// The thread ends when the caller drops the receiver, which is why nothing
/// joins it: a send parked on a full channel has no other way out.
fn flood_pty(stoat: &mut Stoat) {
    let session: Arc<dyn crate::host::TerminalSession> =
        Arc::new(crate::host::FakeTerminalSession::new());
    let agent_id = stoat.active_workspace_mut().terms.insert(TermSession::new(
        crate::term_screen::TermScreen::new(24, 80),
        session,
        TermSession::next_token(),
    ));

    let pane = stoat.active_workspace().panes.focus();
    stoat.active_workspace_mut().panes.pane_mut(pane).view = View::Agent(agent_id);

    let pty_tx = stoat.pty_tx.clone();
    let chunk = move || PtyNotification::TermOutput {
        agent_id,
        data: vec![b'x'; 64 * 1024],
    };

    // Filled before the loop starts. A thread that allocates every chunk
    // cannot outrun the parse from a standing start, so the channel would
    // be empty at the first poll and no arm below the pty one would wait.
    while pty_tx.try_send(chunk()).is_ok() {}

    std::thread::spawn(move || while pty_tx.blocking_send(chunk()).is_ok() {});
}

/// The fs-watch drain runs from one select arm and from nowhere else, so a
/// flood holding an arm above it stops the editor seeing the files the
/// flooding command writes. A build inside a terminal pane is both at once.
#[tokio::test]
async fn a_pty_flood_starves_no_other_arm() {
    let mut h = Stoat::test();
    let watcher = h.fake_fs_watcher().clone();
    flood_pty(&mut h.stoat);

    let (events_tx, events) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let (render, _frames) = watch::channel(None);

    let drained = tokio::select! {
        _ = h.stoat.run(events, render) => false,
        landed = async {
            // Injected once the loop is running, so the wake it fires has
            // to reach an arm the flood sits above.
            watcher.inject("/repo/src/a.rs", FsEventKind::Modified);
            loop {
                if watcher.pending() == 0 {
                    return true;
                }
                tokio::time::sleep(SCROLL_FRAME).await;
            }
        } => landed,
        _ = tokio::time::sleep(SCROLL_FRAME * 60) => false,
    };

    drop(events_tx);
    assert!(drained, "the flood starved the fs-watch drain");
}

/// A biased select returns at its first ready arm, so an arm ahead of the
/// frame timer that is never empty starves it. Terminal output sets a dirty
/// flag and paints nothing itself, so the timer is the only arm that turns a
/// flood into a frame.
#[tokio::test]
async fn a_pty_flood_still_paints() {
    let mut h = Stoat::test();
    flood_pty(&mut h.stoat);

    let (events_tx, events) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let (render, mut frames) = watch::channel(None);

    let painted = tokio::select! {
        _ = h.stoat.run(events, render) => false,
        changed = frames.changed() => changed.is_ok(),
        _ = tokio::time::sleep(SCROLL_FRAME * 60) => false,
    };

    drop(events_tx);
    assert!(painted, "the flood starved the frame timer");
}

/// A command flooding its pty must not hold the run loop for as long as it
/// floods. The parse runs on the app thread, so bytes drained in one turn
/// are milliseconds the frame timer and the key reader do not get.
///
/// The notifications name no run, so the handler returns at once and what
/// is measured is the budget rather than the parse behind it.
#[test]
fn drain_pending_leaves_pty_output_past_its_turn_budget_queued() {
    let mut h = Stoat::test();
    let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();

    let chunks = 32;
    let chunk = 64 * 1024;
    for _ in 0..chunks {
        h.stoat
            .pty_tx
            .try_send(PtyNotification::Output {
                run_id: RunId::default(),
                data: vec![b'x'; chunk],
            })
            .expect("the channel holds 256 chunks");
    }

    let (_, first) = h.stoat.drain_pending(&mut rx);
    assert_eq!(
        first,
        PTY_TURN_BUDGET_BYTES / chunk,
        "one turn drains its budget and no more",
    );

    let (_, second) = h.stoat.drain_pending(&mut rx);
    assert_eq!(
        second,
        chunks - first,
        "the rest waits for the next turn rather than being dropped",
    );
}

#[test]
fn term_pane_osc52_forwards_to_clipboard() {
    let mut h = Stoat::test();
    let session: Arc<dyn crate::host::TerminalSession> =
        Arc::new(crate::host::FakeTerminalSession::new());
    let agent_id = h
        .stoat
        .active_workspace_mut()
        .terms
        .insert(TermSession::new(
            crate::term_screen::TermScreen::new(24, 80),
            session,
            TermSession::next_token(),
        ));

    // OSC 52 set-clipboard with the base64 of "hi", BEL-terminated.
    h.stoat
        .handle_pty_notification(PtyNotification::TermOutput {
            agent_id,
            data: b"\x1b]52;c;aGk=\x07".to_vec(),
        });

    assert_eq!(
        h.fake_clipboard().writes(),
        vec!["hi".to_string()],
        "an OSC 52 write from a term pane reaches the system clipboard"
    );
}

#[test]
fn async_session_restore_installs_into_a_fresh_workspace() {
    let mut h = Stoat::test();
    let file = h.write_file("restored.txt", "alpha\nbeta\n");
    h.open_file(&file);
    h.settle();

    let state_path = PathBuf::from("/state/session.ron");
    h.stoat
        .active_workspace()
        .save_state(&state_path, &*h.stoat.fs_host)
        .expect("save state");

    let target = h.create_workspace();
    h.set_active_workspace(target);
    assert!(h.stoat.active_workspace().is_fresh());

    h.stoat.spawn_workspace_restore(target, state_path);
    h.settle();
    h.stoat.drive_background();

    let restored: Vec<PathBuf> = {
        let ws = h.stoat.active_workspace();
        ws.editors
            .values()
            .filter_map(|e| ws.buffers.path_for(e.buffer_id).map(|p| p.to_path_buf()))
            .collect()
    };
    assert!(
        restored.contains(&file),
        "the restore installs the saved file buffer: {restored:?}"
    );
    assert!(
        h.stoat
            .active_workspace()
            .badges
            .find_by_source(BadgeSource::SessionRestore)
            .is_none(),
        "the restoring-session badge clears after the restore installs"
    );
}

#[test]
fn async_session_restore_drops_when_the_target_was_edited() {
    let mut h = Stoat::test();
    let file = h.write_file("restored.txt", "alpha\n");
    h.open_file(&file);
    h.settle();

    let state_path = PathBuf::from("/state/session.ron");
    h.stoat
        .active_workspace()
        .save_state(&state_path, &*h.stoat.fs_host)
        .expect("save state");

    let target = h.create_workspace();
    h.set_active_workspace(target);
    let other = h.write_file("other.txt", "live\n");
    h.open_file(&other);
    assert!(!h.stoat.active_workspace().is_fresh());

    h.stoat.spawn_workspace_restore(target, state_path);
    h.settle();
    h.stoat.drive_background();

    let paths: Vec<PathBuf> = {
        let ws = h.stoat.active_workspace();
        ws.editors
            .values()
            .filter_map(|e| ws.buffers.path_for(e.buffer_id).map(|p| p.to_path_buf()))
            .collect()
    };
    assert!(paths.contains(&other), "keeps the live buffer: {paths:?}");
    assert!(
        !paths.contains(&file),
        "does not clobber the live workspace with the saved restore"
    );
    assert!(
        h.stoat
            .active_workspace()
            .badges
            .find_by_source(BadgeSource::SessionRestore)
            .is_none(),
        "the badge clears even when the restore is dropped"
    );
}

#[test]
fn term_query_reply_writes_back_to_pty() {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());

    let fake = Arc::new(crate::host::FakeTerminalSession::new());
    let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
    let agent_id = stoat.active_workspace_mut().terms.insert(TermSession::new(
        crate::term_screen::TermScreen::new(24, 80),
        session,
        TermSession::next_token(),
    ));

    // A DSR cursor-position query in the PTY output must be answered back
    // to the PTY. A fresh screen reports the cursor at row 1, column 1.
    stoat.handle_pty_notification(PtyNotification::TermOutput {
        agent_id,
        data: b"\x1b[6n".to_vec(),
    });

    assert_eq!(fake.sent_bytes(), vec![b"\x1b[1;1R".to_vec()]);
}

#[test]
fn layout_fits_agent_emulator_and_pty_to_pane() {
    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    ws.panes.split(crate::pane::Axis::Vertical);
    let focused = ws.panes.focus();

    let fake = Arc::new(crate::host::FakeTerminalSession::new());
    let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
    let agent_id = ws.terms.insert(TermSession::new(
        crate::term_screen::TermScreen::new(24, 80),
        session,
        TermSession::next_token(),
    ));
    ws.panes.pane_mut(focused).view = View::Agent(agent_id);

    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    let ws = h.stoat.active_workspace();
    let (content, _) = crate::render::layout::split_pane_status(ws.panes.pane(focused).area);
    let term = &ws.terms[agent_id].term;
    assert_eq!(
        (term.rows(), term.cols()),
        (content.height as usize, content.width as usize),
        "emulator fits the pane content area",
    );
    assert_eq!(
        fake.last_size(),
        Some((content.height, content.width)),
        "pty resized to the pane content area",
    );
}

#[test]
fn closing_term_pane_kills_pty_child() {
    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    ws.panes.split(crate::pane::Axis::Vertical);
    let focused = ws.panes.focus();

    let fake = Arc::new(crate::host::FakeTerminalSession::new());
    let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
    let agent_id = ws.terms.insert(TermSession::new(
        crate::term_screen::TermScreen::new(24, 80),
        session,
        TermSession::next_token(),
    ));
    ws.panes.pane_mut(focused).view = View::Agent(agent_id);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::ClosePane);
    h.settle();

    assert!(
        fake.was_killed(),
        "closing the agent pane kills its PTY child"
    );
    assert!(
        !h.stoat.active_workspace().terms.contains_key(agent_id),
        "closing the agent pane drops its session",
    );
}

#[test]
fn closing_terminal_pane_kills_pty_child() {
    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    ws.panes.split(crate::pane::Axis::Vertical);
    let focused = ws.panes.focus();

    let fake = Arc::new(crate::host::FakeTerminalSession::new());
    let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
    let term_id = ws.terms.insert(TermSession::new(
        crate::term_screen::TermScreen::new(24, 80),
        session,
        TermSession::next_token(),
    ));
    ws.panes.pane_mut(focused).view = View::Terminal(term_id);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::ClosePane);
    h.settle();

    assert!(
        fake.was_killed(),
        "closing the terminal pane kills its PTY child"
    );
    assert!(
        !h.stoat.active_workspace().terms.contains_key(term_id),
        "closing the terminal pane drops its session",
    );
}

fn insert_term_session(ws: &mut Workspace) -> TermId {
    let session: Arc<dyn crate::host::TerminalSession> =
        Arc::new(crate::host::FakeTerminalSession::new());
    ws.terms.insert(TermSession::new(
        crate::term_screen::TermScreen::new(24, 80),
        session,
        TermSession::next_token(),
    ))
}

#[test]
fn terminal_pane_closes_when_shell_exits() {
    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    let editor_pane = ws.panes.focus();
    let term_pane = ws.panes.split(crate::pane::Axis::Vertical);
    let term_id = insert_term_session(ws);
    ws.panes.pane_mut(term_pane).view = View::Terminal(term_id);
    h.stoat.transition_mode("insert".to_string());

    let effect = h
        .stoat
        .handle_pty_notification(PtyNotification::TermExited { term_id });

    assert_eq!(effect, UpdateEffect::Redraw);
    let ws = h.stoat.active_workspace();
    assert!(!ws.terms.contains_key(term_id), "session dropped on exit");
    assert_eq!(
        ws.panes.split_pane_ids(),
        vec![editor_pane],
        "terminal pane closed, editor remains",
    );
    assert_eq!(ws.panes.focus(), editor_pane, "focus moved to the sibling");
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "focused terminal exit leaves insert mode",
    );
}

#[test]
fn last_terminal_pane_restores_scratch_when_no_prev_view() {
    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    let only_pane = ws.panes.focus();
    let term_id = insert_term_session(ws);
    ws.panes.pane_mut(only_pane).view = View::Terminal(term_id);
    h.stoat.transition_mode("insert".to_string());

    h.stoat
        .handle_pty_notification(PtyNotification::TermExited { term_id });

    let ws = h.stoat.active_workspace();
    assert!(!ws.terms.contains_key(term_id), "session dropped on exit");
    assert_eq!(
        ws.panes.split_pane_ids(),
        vec![only_pane],
        "the last split pane is not closed",
    );
    let View::Editor(editor_id) = ws.panes.pane(only_pane).view else {
        panic!("last pane restores a scratch editor with no prev view");
    };
    let buffer_id = ws.editors.get(editor_id).expect("editor is live").buffer_id;
    let buffer = ws.buffers.get(buffer_id).expect("scratch buffer is live");
    assert_eq!(
        buffer.read().expect("buffer lock").rope().to_string(),
        "",
        "a restored scratch holds an empty rope",
    );
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "focused terminal exit leaves insert mode",
    );
}

#[test]
fn last_terminal_pane_restores_previous_view_on_exit() {
    let mut h = Stoat::test();
    let fake = Arc::new(crate::host::FakeTerminalSession::new());
    h.stoat.terminal_host = Arc::new(crate::host::FakeTerminalHost::new(fake));
    h.allow_host_swap();

    let pane = h.stoat.active_workspace().panes.focus();
    let View::Editor(original) = h.stoat.active_workspace().panes.pane(pane).view else {
        panic!("initial pane holds an editor");
    };

    action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);

    let View::Terminal(term_id) = h.stoat.active_workspace().panes.pane(pane).view else {
        panic!("terminal action points the pane at a terminal");
    };
    h.stoat.transition_mode("insert".to_string());

    h.stoat
        .handle_pty_notification(PtyNotification::TermExited { term_id });

    let ws = h.stoat.active_workspace();
    let View::Editor(restored) = ws.panes.pane(pane).view else {
        panic!("exited terminal restores the previous editor view");
    };
    assert_eq!(
        restored, original,
        "pane restored to its pre-terminal editor"
    );
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "focused terminal exit leaves insert mode",
    );
}

#[test]
fn last_terminal_pane_falls_back_to_scratch_when_prev_view_dangles() {
    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    let only_pane = ws.panes.focus();
    let View::Editor(stale) = ws.panes.pane(only_pane).view else {
        panic!("initial pane holds an editor");
    };
    let term_id = insert_term_session(ws);
    let pane = ws.panes.pane_mut(only_pane);
    pane.prev_view = Some(View::Editor(stale));
    pane.view = View::Terminal(term_id);
    ws.editors.remove(stale);
    h.stoat.transition_mode("insert".to_string());

    h.stoat
        .handle_pty_notification(PtyNotification::TermExited { term_id });

    let ws = h.stoat.active_workspace();
    let View::Editor(restored) = ws.panes.pane(only_pane).view else {
        panic!("dangling prev view falls back to a scratch editor");
    };
    assert_ne!(
        restored, stale,
        "fell back to a fresh editor, not the dead one"
    );
    assert!(ws.editors.contains_key(restored), "scratch editor is live");
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "focused terminal exit leaves insert mode",
    );
}

#[test]
fn terminal_exit_keeps_insert_mode_when_pane_not_focused() {
    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    let editor_pane = ws.panes.focus();
    let term_pane = ws.panes.split(crate::pane::Axis::Vertical);
    let term_id = insert_term_session(ws);
    ws.panes.pane_mut(term_pane).view = View::Terminal(term_id);
    ws.panes.set_focus(editor_pane);
    h.stoat.transition_mode("insert".to_string());

    h.stoat
        .handle_pty_notification(PtyNotification::TermExited { term_id });

    assert_eq!(
        h.stoat.focused_mode(),
        "insert",
        "an unfocused terminal exit leaves the mode untouched",
    );
}

#[test]
fn agent_pane_survives_shell_exit() {
    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    let only_pane = ws.panes.focus();
    let term_id = insert_term_session(ws);
    ws.panes.pane_mut(only_pane).view = View::Agent(term_id);

    h.stoat
        .handle_pty_notification(PtyNotification::TermExited { term_id });

    let ws = h.stoat.active_workspace();
    assert!(
        ws.terms.contains_key(term_id),
        "agent session retained on exit",
    );
    assert!(
        matches!(ws.panes.pane(only_pane).view, View::Agent(id) if id == term_id),
        "agent pane view unchanged",
    );
}

// The server task needs a live reactor, so these never settle the
// scheduler. They count what was enqueued rather than run any of it, and
// the directory is a path nothing binds either way.
#[test]
fn agent_sockets_stay_unserved_without_the_production_flag() {
    let mut h = Stoat::test();
    h.stoat
        .set_agent_socket_dir("/stoat-test-never-served".into());
    let uid = h.stoat.active_workspace().uid();
    let idle = h.pending_runnables();

    assert!(h.stoat.serve_term_session(uid).is_ok());
    assert!(h.stoat.serve_term_session(uid).is_ok());

    assert_eq!(
        h.pending_runnables(),
        idle,
        "a socket directory alone names a socket, it does not bind one",
    );
    assert!(h.stoat.served_agent_sockets.is_empty());
}

#[test]
fn a_workspaces_agent_socket_is_served_once() {
    let mut h = Stoat::test();
    h.stoat
        .set_agent_socket_dir("/stoat-test-never-served".into());
    h.stoat.set_serve_agent_sockets(true);
    let uid = h.stoat.active_workspace().uid();
    let idle = h.pending_runnables();

    assert!(h.stoat.serve_term_session(uid).is_ok());
    assert_eq!(h.pending_runnables(), idle + 1, "the socket is served");
    assert!(h.stoat.serve_term_session(uid).is_ok());

    assert_eq!(
        h.pending_runnables(),
        idle + 1,
        "every spawn calls this, so a repeat must not stack a second listener",
    );
    assert_eq!(
        h.stoat.served_agent_sockets.iter().collect::<Vec<_>>(),
        vec![&uid],
    );
}

#[test]
fn a_dock_held_agent_survives_shell_exit() {
    use crate::pane::{DockPanel, DockSide, DockVisibility};

    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    let term_id = insert_term_session(ws);
    ws.docks.insert(DockPanel {
        view: View::Agent(term_id),
        side: DockSide::Right,
        visibility: DockVisibility::Hidden,
        default_width: 30,
        area: Default::default(),
    });

    h.stoat
        .handle_pty_notification(PtyNotification::TermExited { term_id });

    assert!(
        h.stoat.active_workspace().terms.contains_key(term_id),
        "an agent keeps its last frame wherever it is shown",
    );
}

#[test]
fn a_terminal_hidden_behind_a_buffer_retires_on_exit() {
    let mut h = Stoat::test();
    let (pane, term_id) = {
        let ws = h.stoat.active_workspace_mut();
        let pane = ws.panes.focus();
        let term_id = insert_term_session(ws);
        // An open-in-term request leaves this state. The buffer sits in
        // front, and nothing shows the shell recorded behind it.
        ws.panes.pane_mut(pane).prev_view = Some(View::Terminal(term_id));
        (pane, term_id)
    };

    let effect = h
        .stoat
        .handle_pty_notification(PtyNotification::TermExited { term_id });

    assert_eq!(
        effect,
        UpdateEffect::None,
        "nothing showed the shell, so nothing repaints",
    );
    let ws = h.stoat.active_workspace();
    assert!(
        !ws.terms.contains_key(term_id),
        "a hidden shell's session is not left behind",
    );
    assert!(
        ws.panes.pane(pane).prev_view.is_none(),
        "a dead shell is nothing to return to",
    );
}

#[test]
fn hidden_term_output_advances_state_without_a_repaint() {
    use crate::pane::{DockPanel, DockSide, DockVisibility};

    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    let term_id = insert_term_session(ws);
    // Only a hidden dock shows the term, so no visible surface has it.
    ws.docks.insert(DockPanel {
        view: View::Terminal(term_id),
        side: DockSide::Right,
        visibility: DockVisibility::Hidden,
        default_width: 30,
        area: Rect::new(0, 0, 0, 0),
    });

    let effect = h
        .stoat
        .handle_pty_notification(PtyNotification::TermOutput {
            agent_id: term_id,
            data: b"abc".to_vec(),
        });
    assert_eq!(
        effect,
        UpdateEffect::None,
        "a hidden term drives no repaint"
    );

    let cursor = h
        .stoat
        .active_workspace()
        .terms
        .get(term_id)
        .expect("term session")
        .term
        .cursor();
    assert_eq!(
        cursor.map(|c| c.col),
        Some(3),
        "the term still fed its bytes while hidden",
    );
}

#[test]
fn visible_term_output_paces_a_repaint_to_the_tick() {
    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    let pane = ws.panes.focus();
    let term_id = insert_term_session(ws);
    ws.panes.pane_mut(pane).view = View::Terminal(term_id);

    // Rapid bursts each mark the frame dirty and repaint nothing on their own.
    for _ in 0..2 {
        let effect = h
            .stoat
            .handle_pty_notification(PtyNotification::TermOutput {
                agent_id: term_id,
                data: b"x".to_vec(),
            });
        assert_eq!(
            effect,
            UpdateEffect::None,
            "a visible term does not repaint per PTY chunk",
        );
    }
    assert!(h.stoat.pty_dirty, "the bursts marked the frame dirty");

    // The next tick coalesces them into one repaint and clears the flag.
    assert_eq!(
        h.stoat.frame_tick(0.016),
        UpdateEffect::Redraw,
        "the frame tick paints the accumulated output once",
    );
    assert!(!h.stoat.pty_dirty, "the tick cleared the dirty flag");
}

#[test]
fn an_idle_frame_tick_repaints_nothing() {
    let mut h = Stoat::test();
    assert_eq!(
        h.stoat.frame_tick(0.016),
        UpdateEffect::None,
        "a tick with no glide, build, or pty output repaints nothing",
    );
}

fn stoat_with_focused_term(
    make_view: fn(TermId) -> View,
) -> (Stoat, TermId, Arc<crate::host::FakeTerminalSession>) {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());

    let fake = Arc::new(crate::host::FakeTerminalSession::new());
    let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let term_id = ws.terms.insert(TermSession::new(
        crate::term_screen::TermScreen::new(24, 80),
        session,
        TermSession::next_token(),
    ));
    ws.panes.pane_mut(focused).view = make_view(term_id);
    (stoat, term_id, fake)
}

fn stoat_with_focused_agent() -> (Stoat, TermId, Arc<crate::host::FakeTerminalSession>) {
    stoat_with_focused_term(View::Agent)
}

fn bare(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn compile_keymap(src: &str) -> Keymap {
    let (config, errors) = stoat_config::parse(src);
    assert!(errors.is_empty(), "parse errors: {errors:?}");
    Keymap::compile(&config.expect("config"))
}

#[test]
fn set_var_gates_a_binding() {
    let mut h = Stoat::test();
    h.stoat.keymap = compile_keymap(
        r#"on key {
                x -> SetVar(sidebar, on);
                sidebar == "on" { j -> SetVar(pressed, yes); }
            }"#,
    );
    // The hints cache is keyed on state under a fixed keymap, so a new
    // keymap invalidates it.
    h.stoat.hints_cache = None;

    // `j` is inert until `x` sets the variable.
    h.stoat.handle_key(bare(KeyCode::Char('j')));
    assert!(!h.stoat.user_vars.contains_key("pressed"));

    h.stoat.handle_key(bare(KeyCode::Char('x')));
    h.stoat.handle_key(bare(KeyCode::Char('j')));
    assert_eq!(
        h.stoat.user_vars.get("pressed"),
        Some(&StateValue::String("yes".into()))
    );
}

#[test]
fn set_var_collision_with_builtin_is_ignored() {
    let mut h = Stoat::test();
    h.stoat.keymap = compile_keymap("on key { x -> SetVar(mode, hacked); }");
    h.stoat.hints_cache = None;

    h.stoat.handle_key(bare(KeyCode::Char('x')));
    assert!(!h.stoat.user_vars.contains_key("mode"));
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn hints_cache_reuses_rows_across_unchanged_frames() {
    let mut h = Stoat::test();
    // The `?` toggle forces the hints box in normal mode, so the main arm
    // populates the cache.
    h.stoat.key_hints_visible = true;
    let mut buf = Buffer::empty(h.stoat.size());

    h.stoat.paint_into(&mut buf);
    let key = h.stoat.hints_cache.as_ref().expect("cache populated").key;

    // A rebuild would drop this sentinel row. Reuse keeps it.
    h.stoat
        .hints_cache
        .as_mut()
        .unwrap()
        .rows
        .push(("SENTINEL".into(), "SENTINEL".into()));

    h.stoat.paint_into(&mut buf);
    let cache = h.stoat.hints_cache.as_ref().expect("cache retained");
    assert_eq!(cache.key, key, "unchanged state keeps the same cache key");
    assert!(
        cache.rows.iter().any(|(k, _)| k == "SENTINEL"),
        "an unchanged frame reuses the cached rows instead of rewalking",
    );
}

/// Paint a whole frame and return the APC scene it built.
///
/// Read before any flush, so the decoration lane still holds this frame
/// rather than the one before it. Handed back as text because the scene is
/// ASCII throughout, and a mismatch between two of them reads as commands
/// rather than as a pair of byte arrays.
fn frame_scene(stoat: &mut Stoat) -> String {
    let mut buf = Buffer::empty(stoat.size());
    stoat.paint_into(&mut buf);
    String::from_utf8_lossy(stoat.apc_scene.bytes()).into_owned()
}

/// The same paint with every spliceable frame dropped, which is the scene a
/// full encode writes with nothing to splice.
fn cold_frame_scene(stoat: &mut Stoat) -> String {
    for editor in stoat.active_workspace_mut().editors.values_mut() {
        editor.gutter_geometry_cache = None;
        editor.status_scene_cache = Default::default();
    }
    frame_scene(stoat)
}

/// The gutter and the status bar each splice the APC frame they last
/// emitted, so a repaint is only correct while the spliced bytes are the
/// bytes a full encode writes. Checked at rest, where every frame splices,
/// and after a cursor move, which re-encodes both.
#[test]
fn a_repaint_splices_the_scene_a_full_encode_writes() {
    let mut h = Stoat::test();
    h.stoat.stoatty = true;

    let root = std::path::PathBuf::from("/scene-memo");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    frame_scene(&mut h.stoat);
    let spliced = frame_scene(&mut h.stoat);

    assert!(!spliced.is_empty(), "a live scene carries the frame");
    assert_eq!(
        spliced,
        cold_frame_scene(&mut h.stoat),
        "an unchanged repaint splices the scene a full encode writes",
    );

    action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveDown);
    let moved = frame_scene(&mut h.stoat);

    assert_ne!(moved, spliced, "the moved cursor changes the frame");
    assert_eq!(
        moved,
        cold_frame_scene(&mut h.stoat),
        "a cursor move re-encodes instead of splicing the frame it cached",
    );
}

#[test]
fn workspace_picker_binding_is_rebindable() {
    let mut h = Stoat::test();
    h.stoat.keymap =
        compile_keymap("on key { modal == workspace_picker { Ctrl-x -> CancelPromptInput(); } }");
    h.stoat.hints_cache = None;

    action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchWorkspace);
    assert!(h.stoat.workspace_picker.is_some());

    // The picker's filter input is in insert mode, so a printable key would
    // type rather than route. `Ctrl-x` is non-printable and not a default
    // picker binding, so closing on it proves the `modal == workspace_picker`
    // block drives the picker, not hardcoded dispatch.
    h.stoat.handle_key(ctrl('x'));
    assert!(h.stoat.workspace_picker.is_none());
}

/// The picker-level completion logic is unit-tested in `workspace_picker`.
/// This covers the wiring that unit test cannot see. Tab has to route
/// through the default keymap to the handler at all, rather than falling
/// through to the insert-mode `SmartTab` and indenting the filter input.
#[test]
fn tab_completes_the_highlighted_workspace_into_the_filter() {
    let mut h = Stoat::test();
    h.stoat.active_workspace_mut().name = "alpha".into();

    action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchWorkspace);
    let _ = h.snapshot();

    h.type_keys("tab");
    let _ = h.snapshot();

    let picker = h.stoat.workspace_picker.as_ref().expect("picker open");
    assert_eq!(
        picker.input.text(h.stoat.active_workspace()),
        "alpha",
        "Tab completes the highlighted workspace name into the filter input"
    );
}

#[test]
fn modal_over_a_target_keeps_the_target_mode() {
    let mut h = Stoat::test();
    h.stoat.set_focused_mode("select".into());
    assert_eq!(h.stoat.focused_mode(), "select");

    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
    assert_eq!(
        h.stoat.focused_mode(),
        "insert",
        "the palette input carries its own mode, not the target's"
    );

    h.stoat.handle_key(ctrl('c'));
    assert_eq!(
        h.stoat.focused_mode(),
        "select",
        "closing the modal leaves the underlying target's mode untouched"
    );
}

#[test]
fn editor_pane_modes_are_independent_across_focus() {
    let mut h = Stoat::test();
    h.type_action("SplitRight()");
    h.stoat.set_focused_mode("insert".into());
    assert_eq!(h.stoat.focused_mode(), "insert");

    action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "the other pane keeps its own mode across the focus switch"
    );

    action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusRight);
    assert_eq!(
        h.stoat.focused_mode(),
        "insert",
        "returning focus restores the pane's own mode"
    );
}

#[test]
fn encode_key_to_pty_covers_agent_keys() {
    let enc = |k: KeyEvent| encode_key_to_pty(&k);
    assert_eq!(enc(bare(KeyCode::Char('a'))), Some(b"a".to_vec()));
    assert_eq!(enc(bare(KeyCode::Char('Z'))), Some(b"Z".to_vec()));
    assert_eq!(enc(ctrl('c')), Some(vec![0x03]));
    assert_eq!(enc(ctrl('a')), Some(vec![0x01]));
    assert_eq!(enc(bare(KeyCode::Enter)), Some(vec![b'\r']));
    assert_eq!(enc(bare(KeyCode::Tab)), Some(vec![b'\t']));
    assert_eq!(enc(bare(KeyCode::Backspace)), Some(vec![0x7f]));
    assert_eq!(enc(bare(KeyCode::Esc)), Some(vec![0x1b]));
    assert_eq!(enc(bare(KeyCode::Up)), Some(b"\x1b[A".to_vec()));
    assert_eq!(enc(bare(KeyCode::Down)), Some(b"\x1b[B".to_vec()));
    assert_eq!(enc(bare(KeyCode::Right)), Some(b"\x1b[C".to_vec()));
    assert_eq!(enc(bare(KeyCode::Left)), Some(b"\x1b[D".to_vec()));
    assert_eq!(enc(bare(KeyCode::F(1))), None);
}

#[test]
fn focused_term_pane_routes_keys_to_pty() {
    let (mut stoat, _id, fake) = stoat_with_focused_agent();

    assert_eq!(
        stoat.handle_key(bare(KeyCode::Char('h'))),
        UpdateEffect::None
    );
    stoat.handle_key(bare(KeyCode::Char('i')));
    stoat.handle_key(bare(KeyCode::Enter));
    stoat.handle_key(ctrl('d'));
    stoat.handle_key(ctrl('w'));

    assert_eq!(
        fake.sent_bytes(),
        vec![
            b"h".to_vec(),
            b"i".to_vec(),
            vec![b'\r'],
            vec![0x04],
            vec![0x17]
        ],
    );
    assert_eq!(
        stoat.focused_mode(),
        "normal",
        "Ctrl-W passes through and switches no mode"
    );
}

#[test]
fn focused_terminal_pane_routes_keys_to_pty() {
    let (mut stoat, _id, fake) = stoat_with_focused_term(View::Terminal);

    stoat.handle_key(bare(KeyCode::Char('l')));
    stoat.handle_key(bare(KeyCode::Char('s')));
    stoat.handle_key(bare(KeyCode::Enter));

    assert_eq!(
        fake.sent_bytes(),
        vec![b"l".to_vec(), b"s".to_vec(), vec![b'\r']],
    );
    assert_eq!(stoat.focused_mode(), "normal");
}

#[test]
fn focused_term_pane_sends_interrupt_on_ctrl_c() {
    let (mut stoat, _id, fake) = stoat_with_focused_agent();

    let effect = stoat.handle_key(ctrl('c'));

    assert_eq!(effect, UpdateEffect::None);
    assert_eq!(stoat.focused_mode(), "normal");
    assert_eq!(fake.sent_bytes(), vec![vec![0x03]]);
}

#[test]
fn ctrl_a_in_a_terminal_enters_the_prefix_and_sends_nothing() {
    for make_view in [View::Terminal, View::Agent] {
        let (mut stoat, _id, fake) = stoat_with_focused_term(make_view);

        stoat.handle_key(ctrl('a'));

        assert_eq!(
            (stoat.focused_mode().to_string(), fake.sent_bytes()),
            ("prefix".to_string(), Vec::<Vec<u8>>::new()),
            "Ctrl-a opens the tab prefix and never reaches the child",
        );
    }
}

#[test]
fn a_terminal_sends_the_keys_that_normal_mode_binds_to_its_child() {
    let (mut stoat, _id, fake) = stoat_with_focused_term(View::Terminal);

    stoat.handle_key(bare(KeyCode::Tab));
    stoat.handle_key(ctrl('u'));
    stoat.handle_key(ctrl('s'));

    assert_eq!(
        (stoat.focused_mode(), fake.sent_bytes()),
        ("normal", vec![vec![b'\t'], vec![0x15], vec![0x13]]),
        "an editor binding of the same key does not take it from the child",
    );
}

#[test]
fn ctrl_a_escape_returns_a_terminal_to_rest() {
    for make_view in [View::Terminal, View::Agent] {
        let (mut stoat, _id, fake) = stoat_with_focused_term(make_view);

        stoat.handle_key(ctrl('a'));
        stoat.handle_key(bare(KeyCode::Esc));
        stoat.handle_key(bare(KeyCode::Char('x')));

        assert_eq!(
            (stoat.focused_mode(), fake.sent_bytes()),
            ("normal", vec![b"x".to_vec()]),
            "Escape leaves the prefix, and the next key reaches the child",
        );
    }
}

#[test]
fn ctrl_a_ctrl_a_from_a_terminal_toggles_the_tab_and_back() {
    let mut h = Stoat::test();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);

    h.stoat.update(Event::Key(ctrl('a')));
    h.stoat.update(Event::Key(ctrl('a')));
    assert_eq!(
        (
            h.stoat.active_workspace().active_tab,
            h.fake_terminal().sent_bytes()
        ),
        (0, Vec::<Vec<u8>>::new()),
        "Ctrl-a Ctrl-a leaves the terminal's tab and sends it nothing",
    );

    h.stoat.update(Event::Key(ctrl('a')));
    h.stoat.update(Event::Key(ctrl('a')));
    h.stoat.update(Event::Key(bare(KeyCode::Char('x'))));
    assert_eq!(
        (
            h.stoat.active_workspace().active_tab,
            h.fake_terminal().sent_bytes()
        ),
        (1, vec![b"x".to_vec()]),
        "the toggle back lands on the terminal, which takes the next key",
    );
}

#[test]
fn the_quit_prompt_keeps_the_keys_from_the_terminal_behind_it() {
    let (mut stoat, _id, fake) = stoat_with_focused_term(View::Terminal);
    stoat.quit_all_confirm = Some(QuitAllConfirm::new(&[], Path::new("/")));

    stoat.handle_key(bare(KeyCode::Char('x')));

    assert!(
        fake.sent_bytes().is_empty(),
        "the open prompt takes the key, not the terminal behind it",
    );
}

#[test]
fn escape_in_a_terminal_or_agent_pane_reaches_the_child() {
    for make_view in [View::Terminal, View::Agent] {
        let (mut stoat, _id, fake) = stoat_with_focused_term(make_view);

        let effect = stoat.handle_key(bare(KeyCode::Esc));

        assert_eq!(
            (effect, stoat.focused_mode().to_string(), fake.sent_bytes()),
            (UpdateEffect::None, "normal".to_string(), vec![vec![0x1b]]),
            "Escape is a key like any other for the child",
        );
    }
}

#[test]
fn a_terminal_opens_with_its_keys_on_the_child() {
    let mut h = Stoat::test();

    action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "an opened terminal rests in normal mode",
    );

    h.stoat.update(Event::Key(bare(KeyCode::Char('x'))));
    assert_eq!(
        h.fake_terminal().sent_bytes(),
        vec![b"x".to_vec()],
        "the first keystroke reaches the shell without pressing i",
    );
}

#[test]
fn a_terminal_takes_keys_again_when_focus_returns() {
    let mut h = Stoat::test();
    let (_, term_pane) = split_editor_and_terminal(&mut h);
    assert_eq!(term_mode(&h.stoat, term_pane), "normal");

    h.type_action("FocusRight()");
    h.stoat.update(Event::Key(bare(KeyCode::Char('x'))));

    assert_eq!(
        (
            h.stoat.active_workspace().panes.focus(),
            h.fake_terminal().sent_bytes()
        ),
        (term_pane, vec![b"x".to_vec()]),
        "the terminal takes the first key after focus comes back",
    );
}

/// The input mode of the terminal shown in `pane`.
fn term_mode(stoat: &Stoat, pane: PaneId) -> String {
    let ws = stoat.active_workspace();
    match ws.panes.pane(pane).view {
        View::Terminal(id) => ws.terms[id].mode.clone(),
        ref other => panic!("pane shows {other:?}, not a terminal"),
    }
}

/// A two-pane split with an editor on the left and a terminal on the right,
/// focus left on the editor. Returns the two pane ids.
fn split_editor_and_terminal(h: &mut crate::test_harness::TestHarness) -> (PaneId, PaneId) {
    h.type_action("SplitRight()");
    action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
    let term_pane = h.stoat.active_workspace().panes.focus();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
    (h.stoat.active_workspace().panes.focus(), term_pane)
}

#[test]
fn a_click_into_a_terminal_or_agent_pane_gives_it_the_keys() {
    for make_view in [View::Terminal, View::Agent] {
        let mut h = Stoat::test();
        let (term_pane, term_id) = {
            let ws = h.stoat.active_workspace_mut();
            let editor_pane = ws.panes.focus();
            let term_pane = ws.panes.split(crate::pane::Axis::Vertical);
            let term_id = insert_term_session(ws);
            ws.panes.pane_mut(term_pane).view = make_view(term_id);
            ws.panes.set_focus(editor_pane);
            ws.panes.pane_mut(editor_pane).area = Rect::new(0, 0, 40, 24);
            ws.panes.pane_mut(term_pane).area = Rect::new(40, 0, 40, 24);
            (term_pane, term_id)
        };

        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 50, 5));

        assert_eq!(
            (
                h.stoat.active_workspace().panes.focus(),
                h.stoat.term_input_target()
            ),
            (term_pane, Some(term_id)),
            "the click focuses the pane, and its keys go to the child",
        );
    }
}

fn focused_terminal_pane(h: &mut crate::test_harness::TestHarness, content: &[u8]) -> TermId {
    let term_id = {
        let ws = h.stoat.active_workspace_mut();
        let pane = ws.panes.focus();
        let term_id = insert_term_session(ws);
        ws.panes.pane_mut(pane).view = View::Terminal(term_id);
        term_id
    };
    // A render fits the emulator to the focused pane, so feed the content
    // afterward to land it in the final grid.
    let _ = h.stoat.render();
    h.stoat.active_workspace_mut().terms[term_id]
        .term
        .feed(content);
    term_id
}

#[test]
fn dragging_over_a_terminal_pane_selects_and_copies() {
    use crossterm::event::MouseButton;

    let mut h = Stoat::test();
    let term_id = focused_terminal_pane(&mut h, b"hello world");

    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0));
    h.stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 4, 0));
    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 4, 0));

    assert_eq!(h.fake_clipboard().writes(), vec!["hello"]);
    assert!(
        h.stoat.active_workspace().terms[term_id]
            .selection
            .is_some(),
        "the selection stays highlighted after release",
    );
    assert!(
        h.stoat.terminal_drag.is_none(),
        "the drag clears on release"
    );
}

/// The terminal arm drops the same repeats the editor arm does, and for the
/// same reason. A release after them still copies, so the dedupe costs the
/// selection nothing.
#[test]
fn a_repeated_terminal_drag_on_the_settled_cell_costs_no_frame() {
    use crossterm::event::MouseButton;

    let mut h = Stoat::test();
    let _ = focused_terminal_pane(&mut h, b"hello world");

    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0));
    let moved = h
        .stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 4, 0));
    let repeat = h
        .stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 4, 0));
    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 4, 0));

    assert_eq!(moved, UpdateEffect::Redraw, "the head moved, so repaint");
    assert_eq!(repeat, UpdateEffect::None, "nothing moved, so no repaint");
    assert_eq!(h.fake_clipboard().writes(), vec!["hello"]);
}

#[test]
fn a_keystroke_clears_the_terminal_selection() {
    use crossterm::event::MouseButton;

    let mut h = Stoat::test();
    let term_id = focused_terminal_pane(&mut h, b"hello world");

    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0));
    h.stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 4, 0));
    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 4, 0));
    assert!(h.stoat.active_workspace().terms[term_id]
        .selection
        .is_some());

    h.stoat.update(Event::Key(bare(KeyCode::Char('x'))));
    assert!(
        h.stoat.active_workspace().terms[term_id]
            .selection
            .is_none(),
        "typing into the terminal clears the selection",
    );
}

#[test]
fn a_click_without_drag_leaves_no_terminal_selection() {
    use crossterm::event::MouseButton;

    let mut h = Stoat::test();
    let term_id = focused_terminal_pane(&mut h, b"hello world");

    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 0));
    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 0));

    assert!(
        h.stoat.active_workspace().terms[term_id]
            .selection
            .is_none(),
        "a plain click leaves no selection",
    );
    assert!(
        h.fake_clipboard().writes().is_empty(),
        "a plain click copies nothing",
    );
}

#[test]
fn palette_over_a_terminal_routes_typing_to_the_palette() {
    let mut h = Stoat::test();
    let fake = Arc::new(crate::host::FakeTerminalSession::new());
    {
        let session: Arc<dyn crate::host::TerminalSession> = fake.clone();
        let ws = h.stoat.active_workspace_mut();
        let pane = ws.panes.focus();
        let term_id = ws.terms.insert(TermSession::new(
            crate::term_screen::TermScreen::new(24, 80),
            session,
            TermSession::next_token(),
        ));
        ws.panes.pane_mut(pane).view = View::Terminal(term_id);
    }

    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
    assert!(
        h.stoat.command_palette.is_some(),
        "the command palette opens over a terminal pane",
    );

    for ch in "qui".chars() {
        h.stoat.update(Event::Key(bare(KeyCode::Char(ch))));
    }

    let text = {
        let ws = h.stoat.active_workspace();
        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        palette.focused_input().expect("palette input").text(ws)
    };
    assert_eq!(
        text, "qui",
        "typing filters the palette rather than the terminal behind it",
    );
    assert!(
        fake.sent_bytes().is_empty(),
        "the terminal PTY receives nothing while the palette owns typing",
    );

    h.stoat.update(Event::Key(bare(KeyCode::Esc)));
    assert!(h.stoat.command_palette.is_none(), "Esc closes the palette");
    h.stoat.update(Event::Key(bare(KeyCode::Char('x'))));
    assert_eq!(
        fake.sent_bytes(),
        vec![b"x".to_vec()],
        "the terminal takes its keys again after the palette closes",
    );
}

#[test]
fn a_respawned_focused_terminal_takes_keys() {
    let mut h = Stoat::test();
    let pane = {
        let ws = h.stoat.active_workspace_mut();
        let pane = ws.panes.focus();
        ws.panes.pane_mut(pane).view = View::Terminal(TermId::default());
        pane
    };
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "a dead terminal reads the fallback mode",
    );

    action_handlers::respawn_terminal_panes(&mut h.stoat);

    let View::Terminal(new_id) = h.stoat.active_workspace().panes.pane(pane).view else {
        panic!("the dead terminal pane is respawned as a terminal");
    };
    assert!(
        h.stoat.active_workspace().terms.contains_key(new_id),
        "respawned session is live",
    );
    h.stoat.update(Event::Key(bare(KeyCode::Char('x'))));
    assert_eq!(
        h.fake_terminal().sent_bytes(),
        vec![b"x".to_vec()],
        "a respawned focused terminal takes the next key",
    );
}

#[test]
fn a_pane_in_a_chord_mode_sends_no_key_to_its_child() {
    let (mut stoat, _id, fake) = stoat_with_focused_agent();
    stoat.set_focused_mode("space_pane_display".to_string());

    stoat.handle_key(bare(KeyCode::Char('x')));

    assert!(
        fake.sent_bytes().is_empty(),
        "a chord in progress holds the keys back from the child"
    );
}

#[test]
fn agent_input_requires_agent_focus() {
    let (mut stoat, _id, fake) = stoat_with_focused_agent();
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    ws.panes.pane_mut(focused).view = View::Label("scratch".to_string());

    stoat.handle_key(bare(KeyCode::Char('x')));

    assert!(
        fake.sent_bytes().is_empty(),
        "non-agent focus must not route"
    );
}

/// Every cursor a multi-cursor delete moves is carried through the same
/// merged range list, so the answer is a binary search over a prefix sum
/// rather than a walk per cursor. The four positions a target can occupy
/// relative to the ranges are what that search has to get right.
#[test]
fn an_offset_moves_back_by_the_deletions_before_it() {
    // Deleting 2..5 and 10..14 removes three bytes then four.
    let ranges = [(2usize, 5usize), (10, 14)];
    let deleted_before = [0usize, 3, 7];
    let moved = |target| Stoat::offset_after_deletions(target, &ranges, &deleted_before);

    assert_eq!(moved(1), 1, "ahead of every range, nothing shifts it");
    assert_eq!(moved(3), 2, "inside a range, it collapses to that start");
    assert_eq!(moved(7), 4, "between ranges, only the first has passed");
    assert_eq!(moved(20), 13, "past both, both have passed");

    assert_eq!(moved(2), 2, "a target on a range's start is not inside it");
    assert_eq!(moved(5), 2, "a target on a range's end sits after it");
}

/// A two-pane split with the second file focused, painted once so both
/// panes' caches are warm.
///
/// The unfocused pane's buffer carries a diagnostic, so its paint reaches
/// all three channels. Without one it produces no undercurl span at all,
/// and a replay that dropped them would pass unnoticed.
fn split_pair(h: &mut crate::test_harness::TestHarness) {
    h.stoat.stoatty = true;
    h.resize(120, 16);
    let a = h.write_file("a.txt", "alpha\nbravo\ncharlie\n");
    let b = h.write_file("b.txt", "delta\necho\nfoxtrot\n");
    h.open_file(&a);
    publish_one_diagnostic(h, &a);
    h.type_action("SplitRight()");
    h.open_file(&b);
    h.settle();
    let _ = h.stoat.render();
}

/// An error over the first word of `path`'s first line, anchored against the
/// buffer as it stands.
fn publish_one_diagnostic(h: &mut crate::test_harness::TestHarness, path: &std::path::Path) {
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range as LspRange};

    let snapshot = {
        let ws = h.stoat.active_workspace();
        let id = ws.buffers.id_for_path(path).expect("buffer registered");
        ws.buffers
            .get(id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .snapshot
            .clone()
    };
    let anchors = crate::diagnostics::PublishedSpan {
        anchors: Some((
            snapshot.anchors_at_batch(&[0], Bias::Right)[0],
            snapshot.anchors_at_batch(&[5], Bias::Left)[0],
        )),
    };
    h.stoat.diagnostics.replace_from_server(
        path.to_path_buf(),
        "test".into(),
        vec![Diagnostic {
            range: LspRange {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            message: String::new(),
            ..Default::default()
        }],
        vec![anchors],
    );
}

/// One frame's three output channels, for comparing a replayed frame
/// against a repainted one.
type FrameOutput = (Buffer, Vec<u8>, Vec<(u16, u16, u16, [u8; 3])>);

fn frame_output(h: &mut crate::test_harness::TestHarness) -> FrameOutput {
    let buf = h.stoat.render();
    let scene = h.stoat.apc_scene.bytes().to_vec();
    let spans = h
        .stoat
        .pending_undercurls
        .spans()
        .iter()
        .map(|span| (span.x, span.y, span.len, span.color))
        .collect();
    (buf, scene, spans)
}

/// Replaying an unfocused pane produces the frame repainting it would have,
/// while the focused pane alone is painted.
///
/// A cache that skipped work but changed the output would be worse than no
/// cache, so the comparison is against the same frame with the cache
/// emptied rather than against a recorded expectation. All three channels
/// are compared, since cells alone would pass while the rich gutter, the
/// minimap strip, and the undercurls were dropped.
#[test]
fn a_replayed_pane_paints_what_repainting_it_would() {
    let mut h = Stoat::test();
    split_pair(&mut h);

    h.type_keys("i");
    h.type_text("X");
    h.settle();

    let painted_before = h.stoat.pane_paints;
    let replayed = frame_output(&mut h);
    assert_eq!(
        h.stoat.pane_paints - painted_before,
        1,
        "the edited pane painted and the other replayed",
    );

    h.stoat.pane_cache.clear();
    let painted_before = h.stoat.pane_paints;
    let repainted = frame_output(&mut h);
    assert_eq!(
        h.stoat.pane_paints - painted_before,
        2,
        "and with the cache emptied both panes paint",
    );

    assert_eq!(replayed.0, repainted.0, "the cells match");
    assert_eq!(replayed.1, repainted.1, "the scene bytes match");
    assert_eq!(replayed.2, repainted.2, "the undercurl spans match");
}

/// A badge raised over an unfocused pane's status row moves that row's
/// segments out from under the box, so the pane repaints rather than
/// replaying a row whose segments the box now covers.
#[test]
fn a_badge_over_an_unfocused_status_row_repaints_it() {
    let mut h = Stoat::test();
    split_pair(&mut h);
    h.type_action("FocusLeft()");
    let _ = h.stoat.render();

    let painted_before = h.stoat.pane_paints;
    let _ = h.stoat.render();
    assert_eq!(
        h.stoat.pane_paints - painted_before,
        1,
        "with nothing moved, the unfocused right pane replays",
    );

    h.stoat.active_workspace_mut().badges.insert(Badge {
        source: BadgeSource::Review,
        anchor: BadgeAnchor::BottomRight,
        state: BadgeState::Complete,
        label: "reviewing 1/3".to_owned(),
        detail: None,
    });
    let painted_before = h.stoat.pane_paints;
    let raised = frame_output(&mut h);
    assert_eq!(
        h.stoat.pane_paints - painted_before,
        2,
        "the badge over its status row repaints the unfocused pane",
    );

    h.stoat.pane_cache.clear();
    let repainted = frame_output(&mut h);
    assert_eq!(raised.0, repainted.0, "the cells match");
    assert_eq!(raised.1, repainted.1, "the scene bytes match");
}

/// A frame driven by background activity alone paints only the focused
/// pane.
/// The key guards used to resolve the mode once each, and resolving it
/// walks the modal stack and clones a pane-tree view. Reading it once for
/// the whole chain took a movement key from twenty-two resolutions to nine.
///
/// A bound rather than a count, because the nine that remain belong to
/// callers this does not touch, and one of them arriving or leaving should
/// not fail a test about the guards. Anything near twenty means the chain
/// went back to asking per guard.
#[test]
fn the_key_guards_resolve_the_mode_once_between_them() {
    use crate::test_harness::TestHarness;

    let mut h = TestHarness::with_size(40, 6);
    let file = h.write_file("a.rs", "hello world\nsecond line\n");
    h.open_file(&file);

    let before = h.stoat.focused_mode_reads.get();
    h.type_keys("l");
    let movement = h.stoat.focused_mode_reads.get() - before;
    assert!(
        movement <= 12,
        "a movement key resolved the mode {movement} times"
    );

    let before = h.stoat.focused_mode_reads.get();
    h.type_keys("i");
    let entering = h.stoat.focused_mode_reads.get() - before;
    assert!(
        entering <= 12,
        "entering insert resolved the mode {entering} times"
    );
}

/// An action brackets itself in an undo group, and a group that takes no
/// edit is discarded on sealing, so the selections the seal would record are
/// never read. Gathering them copies the whole selection set, which at
/// multi-cursor scale is the cost that matters.
///
/// The editing action still records both ends, which is what says the count
/// is measuring something.
#[test]
fn a_non_editing_action_captures_no_post_selections() {
    use crate::test_harness::TestHarness;

    let mut h = TestHarness::with_size(40, 6);
    let file = h.write_file("a.rs", "hello world\nsecond line\n");
    h.open_file(&file);

    let before = h.stoat.selection_snapshots.get();
    h.type_keys("l");
    assert_eq!(
        h.stoat.selection_snapshots.get() - before,
        1,
        "a movement captures the pre-action set and nothing after it"
    );

    // `x` selects the line, which edits nothing either.
    let before = h.stoat.selection_snapshots.get();
    h.type_keys("x");
    assert_eq!(
        h.stoat.selection_snapshots.get() - before,
        1,
        "selecting a line is not an edit"
    );

    // `d` deletes it, so the group materializes and the seal records where
    // the selections ended up.
    let before = h.stoat.selection_snapshots.get();
    h.type_keys("d");
    assert_eq!(
        h.stoat.selection_snapshots.get() - before,
        2,
        "an edit captures both ends of its undo group"
    );
}

/// Typing in insert mode is the busiest thing the editor does, and a
/// printable character never consults the keymap, so it must not pay to
/// derive the lookup. Nothing else would notice if it started to: the
/// derivation only costs time.
///
/// The keys that do read a binding still derive one, which is what says the
/// counter is measuring something.
#[test]
fn typing_a_printable_character_never_derives_a_keymap_lookup() {
    use crate::test_harness::TestHarness;

    let mut h = TestHarness::with_size(40, 6);
    let file = h.write_file("a.rs", "hello\n");
    h.open_file(&file);

    // `i` is a normal-mode binding and reads its own lookup.
    h.type_keys("i");
    let entering_insert = h.stoat.keymap_lookups.get();
    assert!(
        entering_insert > 0,
        "entering insert resolves through the keymap"
    );

    h.type_text("abcdef");
    assert_eq!(
        h.stoat.keymap_lookups.get(),
        entering_insert,
        "six printable characters must derive no lookup between them"
    );

    // Escape is non-printable, so it falls through to the keymap.
    h.type_keys("esc");
    assert!(
        h.stoat.keymap_lookups.get() > entering_insert,
        "leaving insert resolves through the keymap"
    );
}

#[test]
fn typing_into_a_terminal_derives_a_keymap_lookup_only_for_a_key_that_types_nothing() {
    let (mut stoat, _id, _fake) = stoat_with_focused_term(View::Terminal);

    let before = stoat.keymap_lookups.get();
    stoat.handle_key(bare(KeyCode::Char('a')));
    stoat.handle_key(KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT));
    let typed = stoat.keymap_lookups.get();
    stoat.handle_key(bare(KeyCode::Enter));

    assert_eq!(
        (typed - before, stoat.keymap_lookups.get() - typed),
        (0, 1),
        "two typed characters derive no lookup, and Enter derives one",
    );
}

/// This is the case the cache exists for. A spinner tick redraws at its own
/// rate while nothing a pane reads has moved, and every visible pane used to
/// repaint in full for it. The focused one still paints, since its
/// selections and cursor are outside the key, so the saving is every pane
/// but that one.
#[test]
fn a_background_tick_paints_only_the_focused_pane() {
    let mut h = Stoat::test();
    split_pair(&mut h);

    let before = h.stoat.pane_paints;
    h.stoat.spinner_clock += 1.0;
    let _ = h.stoat.render();

    assert_eq!(
        h.stoat.pane_paints - before,
        1,
        "the spinner moved nothing the unfocused pane reads",
    );
}

/// Every input a pane paints from reaches its key.
///
/// A key that ignored one of these would leave a pane showing stale content
/// with nothing to say it had, which is why each is moved through the real
/// session rather than by building two keys by hand.
///
/// The assertion is on the key rather than on a paint count, because the
/// harness renders a frame per keystroke and the search case is several of
/// them. A key that moved is a replay refused, since the lookup compares
/// keys for equality before it hands anything back.
#[test]
fn each_input_that_moves_reaches_the_key() {
    type Case = (&'static str, fn(&mut crate::test_harness::TestHarness));
    let cases: [Case; 6] = [
        ("theme", |h| {
            action_handlers::dispatch(
                &mut h.stoat,
                &stoat_action::SetTheme {
                    name: "gruvbox-light".to_string(),
                },
            );
        }),
        ("search", |h| {
            h.type_keys("/");
            h.type_text("alpha");
            h.type_keys("enter");
        }),
        ("inactive dim", |h| {
            h.stoat.settings.ui_inactive_dim = Some(0.6);
        }),
        ("line numbers", |h| {
            h.stoat.settings.editor_line_numbers = Some(LineNumbers::Off);
        }),
        // The path the unfocused pane holds, since a key watches only its
        // own path's diagnostics.
        ("diagnostics", |h| {
            h.stoat.diagnostics.replace_from_server(
                PathBuf::from("/test/a.txt"),
                "test".into(),
                Vec::new(),
                Vec::new(),
            );
        }),
        ("the unfocused pane's own text", |h| {
            let ws = h.stoat.active_workspace_mut();
            let first = ws.panes.split_pane_ids()[0];
            let editor_id = match ws.panes.pane(first).view {
                View::Editor(id) => id,
                _ => panic!("the first pane holds an editor"),
            };
            let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            buffer.write().expect("poisoned").edit(0..0, "Z");
        }),
    ];

    for (what, apply) in cases {
        let mut h = Stoat::test();
        split_pair(&mut h);
        let _ = h.stoat.render();

        let unfocused = {
            let ws = h.stoat.active_workspace();
            let focused = ws.panes.focus();
            ws.panes
                .split_pane_ids()
                .into_iter()
                .find(|id| *id != focused)
                .expect("the split has an unfocused pane")
        };
        let before = h
            .stoat
            .pane_cache
            .get(&(h.stoat.active_workspace, unfocused))
            .expect("the unfocused pane cached its paint")
            .key;

        apply(&mut h);
        h.settle();
        let _ = h.stoat.render();

        let after = h
            .stoat
            .pane_cache
            .get(&(h.stoat.active_workspace, unfocused))
            .expect("and cached it again")
            .key;
        assert_ne!(
            before, after,
            "{what} has to reach the unfocused pane's key"
        );
    }
}

/// A publish for a file a pane does not show leaves that pane's key alone.
///
/// A check run publishes every file in the workspace one at a time. Under a
/// key that watched the whole set, each of those publishes threw away every
/// pane's cached paint, so one run repainted the screen once per file.
#[test]
fn a_publish_for_another_file_leaves_a_panes_key_alone() {
    let mut h = Stoat::test();
    split_pair(&mut h);

    let pane = {
        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        ws.panes
            .split_pane_ids()
            .into_iter()
            .find(|id| *id != focused)
            .expect("the split has an unfocused pane")
    };
    let before = h
        .stoat
        .pane_cache
        .get(&(h.stoat.active_workspace, pane))
        .expect("cached paint")
        .key;

    h.stoat.diagnostics.replace_from_server(
        PathBuf::from("/test/elsewhere.rs"),
        "test".into(),
        Vec::new(),
        Vec::new(),
    );
    h.settle();
    let _ = h.stoat.render();

    assert_eq!(
        before,
        h.stoat
            .pane_cache
            .get(&(h.stoat.active_workspace, pane))
            .expect("cached again")
            .key,
        "a file this pane does not show cannot invalidate its paint",
    );
}

/// A pane id never comes back, so an entry a close leaves behind holds one
/// pane's cells for the process.
#[test]
fn closing_a_split_drops_its_cached_paint() {
    let mut h = Stoat::test();
    split_pair(&mut h);

    let (workspace, unfocused) = {
        let workspace = h.stoat.active_workspace;
        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let unfocused = ws
            .panes
            .split_pane_ids()
            .into_iter()
            .find(|id| *id != focused)
            .expect("the split has an unfocused pane");
        (workspace, unfocused)
    };
    assert!(
        h.stoat.pane_cache.contains_key(&(workspace, unfocused)),
        "the unfocused pane cached its paint",
    );

    // The cached pane is the one to close, so focus moves onto it first.
    h.type_action("FocusLeft()");
    h.type_action("ClosePane()");
    h.settle();
    let _ = h.stoat.render();

    assert!(
        !h.stoat
            .active_workspace()
            .panes
            .split_pane_ids()
            .contains(&unfocused),
        "the close reached the cached pane",
    );

    assert!(
        !h.stoat.pane_cache.contains_key(&(workspace, unfocused)),
        "and the close takes it rather than leaving it for the process",
    );
}

#[test]
fn snapshot_initial_plain() {
    let mut h = Stoat::test();
    h.assert_snapshot("initial_plain");
}

#[test]
fn snapshot_initial_styled() {
    let mut h = Stoat::test();
    h.assert_snapshot("initial");
}

#[test]
fn snapshot_space_mode() {
    let mut h = Stoat::test();
    h.type_keys("space");
    h.assert_snapshot("space_mode");
}

fn focused_buffer_string(h: &crate::test_harness::TestHarness) -> String {
    let ws = h.stoat.active_workspace();
    let View::Editor(editor_id) = ws.panes.pane(ws.panes.focus()).view else {
        panic!("focused pane is not an editor");
    };
    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
    ws.buffers
        .get(buffer_id)
        .expect("buffer")
        .read()
        .expect("poisoned")
        .rope()
        .to_string()
}

fn focused_buffer_version(h: &crate::test_harness::TestHarness) -> u64 {
    let ws = h.stoat.active_workspace();
    let View::Editor(editor_id) = ws.panes.pane(ws.panes.focus()).view else {
        panic!("focused pane is not an editor");
    };
    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
    ws.buffers
        .get(buffer_id)
        .expect("buffer")
        .read()
        .expect("poisoned")
        .version()
}

/// A paste arrives whole, so it costs one edit across every cursor rather
/// than the whole keystroke pipeline per character. Landing as one edit is
/// also what makes it one thing to undo.
#[test]
fn a_paste_lands_as_one_edit_every_cursor_shares() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "note.txt", b"a\nb\n");
    h.type_keys("C");
    let before = focused_buffer_version(&h);

    // Carries a CRLF, which a terminal forwards as the clipboard held it.
    h.stoat.update(Event::Paste("X\r\nY".to_string()));

    // The version counts edit records, and a multi-cursor insert is one per
    // cursor however it arrived. What the batch buys is that the count does
    // not also multiply by the pasted length, which is what a character at a
    // time would have cost.
    assert_eq!(
        focused_buffer_version(&h),
        before + 2,
        "one record per cursor, not one per cursor per pasted character",
    );
    assert_eq!(focused_buffer_string(&h), "X\nYa\nX\nYb\n");

    action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);
    assert_eq!(focused_buffer_string(&h), "a\nb\n", "and one thing to undo",);
}

/// The characters of a paste are text, never keys. Pasting in normal mode
/// used to run what it spelled, so text carrying `d` or `x` edited the
/// buffer on its way in.
#[test]
fn a_normal_mode_paste_inserts_rather_than_running_its_characters() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "note.txt", b"keep\n");
    assert_eq!(h.snapshot().mode, "normal");

    h.stoat.update(Event::Paste("dd".to_string()));

    assert_eq!(focused_buffer_string(&h), "ddkeep\n");
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "and the paste leaves the mode where it found it",
    );
}

/// A modal's input is where typing goes while it is open, so a paste goes
/// there too rather than into the buffer behind it.
#[test]
fn a_paste_with_a_modal_open_lands_in_its_input() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "note.txt", b"keep\n");
    h.type_keys("space p");
    assert!(h.stoat.file_finder.is_some(), "the finder is open");

    h.stoat.update(Event::Paste("note".to_string()));

    let ws = h.stoat.active_workspace();
    let finder = h.stoat.file_finder.as_ref().expect("finder open");
    assert_eq!(finder.input.text(ws), "note");
    assert_eq!(
        focused_buffer_string(&h),
        "keep\n",
        "and the buffer behind it is untouched",
    );
}

/// A modal's input is painted as a single row, so a pasted line break has
/// nowhere to go.
///
/// The break would put the cursor on a row the region never draws, and the
/// query would carry a newline no filter expects. Every character of the
/// paste still arrives, with a space standing in for each break.
#[test]
fn a_multi_line_paste_into_a_modal_lands_on_one_row() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "note.txt", b"keep\n");
    h.type_keys("space p");
    assert!(h.stoat.file_finder.is_some(), "the finder is open");

    h.stoat.update(Event::Paste("a\r\nb\nc".to_string()));

    let ws = h.stoat.active_workspace();
    let finder = h.stoat.file_finder.as_ref().expect("finder open");
    assert_eq!(finder.input.text(ws), "a b c");

    let rows = ws
        .buffers
        .get(finder.input.buffer_id)
        .expect("input buffer")
        .read()
        .expect("poisoned")
        .rope()
        .max_point()
        .row;
    assert_eq!(rows, 0, "and the input is still the one row it paints");
}

/// Pasting a command into a terminal pane sends it to the child.
///
/// Typing there already goes to the child, and paste has no reason to
/// differ. Newlines arrive as carriage returns because that is what the
/// Enter key sends, so a pasted command line runs the way a typed one does.
#[test]
fn a_paste_into_a_terminal_reaches_the_child() {
    let mut h = Stoat::test();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);

    let effect = h.stoat.update(Event::Paste("echo hi\nls\r\n".to_string()));

    assert_eq!(
        h.fake_terminal().sent_bytes(),
        vec![b"echo hi\rls\r".to_vec()],
        "the paste reaches the child with its newlines as carriage returns",
    );
    assert_eq!(
        effect,
        UpdateEffect::None,
        "and asks for no frame of its own, the child's echo doing that",
    );
}

/// A child that asked for bracketed paste gets the guards.
///
/// A shell or editor sets DECSET 2004 so it can tell a paste from typing
/// and hold it back rather than running each line as it arrives. An
/// embedded end guard is dropped, since text that closed the bracket early
/// would have the rest of itself run as keystrokes.
#[test]
fn a_bracketed_child_gets_a_guarded_paste() {
    let mut h = Stoat::test();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
    let term_id = h
        .stoat
        .focused_term_id()
        .expect("the terminal action focuses a term");
    h.stoat
        .active_workspace_mut()
        .terms
        .get_mut(term_id)
        .expect("term session")
        .term
        .feed(b"\x1b[?2004h");

    h.stoat
        .update(Event::Paste("rm -rf\x1b[201~ /".to_string()));

    assert_eq!(
        h.fake_terminal().sent_bytes(),
        vec![b"\x1b[200~rm -rf /\x1b[201~".to_vec()],
        "the payload is wrapped and cannot close the bracket itself",
    );
}

/// A modal takes the paste even over a focused terminal.
///
/// The overlay is where typing goes while it is open, and the terminal
/// underneath keeps the focus that would otherwise claim it.
#[test]
fn a_paste_over_a_terminal_still_lands_in_an_open_modal() {
    let mut h = Stoat::test();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
    assert!(h.stoat.file_finder.is_some(), "the finder is open");

    h.stoat.update(Event::Paste("note".to_string()));

    let ws = h.stoat.active_workspace();
    let finder = h.stoat.file_finder.as_ref().expect("finder open");
    assert_eq!(finder.input.text(ws), "note");
    assert!(
        h.fake_terminal().sent_bytes().is_empty(),
        "and nothing reached the terminal behind it",
    );
}

#[test]
fn encode_paste_to_pty_normalizes_newlines_when_unbracketed() {
    assert_eq!(encode_paste_to_pty("a\r\nb\nc", false), b"a\rb\rc".to_vec());
}

#[test]
fn enter_after_open_brace_auto_indents() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"fn a() {\n}\n");
    h.type_keys("A");
    h.type_keys("enter");
    h.settle();
    assert_eq!(focused_buffer_string(&h), "fn a() {\n\t\n}\n");
}

/// A buffer written in spaces indents by its own unit, not by a tab.
///
/// The base is copied from the row and only the delta comes from the
/// indent style, so getting the delta wrong glues a tab onto spaces and the
/// new line reads a level deeper than it is. The two-space increase on the
/// second row is what the buffer's detector votes on.
#[test]
fn enter_indents_a_space_buffer_by_its_own_unit() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.json", b"{\n  \"a\": {\n  }\n}\n");
    h.type_keys("j");
    h.type_keys("A");
    h.type_keys("enter");
    h.settle();
    assert_eq!(
        focused_buffer_string(&h),
        "{\n  \"a\": {\n    \n  }\n}\n",
        "the opener's row is indented two, so the new line is indented four",
    );
}

/// Opening a line below reads the same unit as Enter does.
#[test]
fn open_below_indents_a_space_buffer_by_its_own_unit() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.json", b"{\n  \"a\": {\n  }\n}\n");
    h.type_keys("j");
    h.type_keys("o");
    h.type_text("1");
    assert_eq!(focused_buffer_string(&h), "{\n  \"a\": {\n    1\n  }\n}\n");
}

/// Opening a line above reads the indents query too, so the two directions
/// agree about the same position in the same block.
///
/// Copying the current line's leading whitespace instead makes `O` on a
/// block's closing line open flush left, where `o` on its opening line
/// opens a level in. The query is asked about the line the new one
/// follows, which for an upward open is the line before the current one.
#[test]
fn open_above_indents_by_the_query_like_open_below() {
    let opened = |dir_key: &str, down: usize| {
        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.rs", b"fn a() {\n}\n");
        for _ in 0..down {
            h.type_keys("j");
        }
        h.type_keys(dir_key);
        h.type_text("x");
        focused_buffer_string(&h)
    };

    assert_eq!(
        opened("O", 1),
        "fn a() {\n\tx\n}\n",
        "O on the closing line opens one level in",
    );
    assert_eq!(
        opened("o", 0),
        "fn a() {\n\tx\n}\n",
        "which is where o on the opening line lands too",
    );
}

/// Re-indenting an existing row reads it too, which is the other entry
/// point and the other query.
#[test]
fn shift_i_indents_a_space_buffer_by_its_own_unit() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.json", b"{\n  \"a\": {\n\n  }\n}\n");
    h.type_keys("j");
    h.type_keys("j");
    h.type_keys("I");
    h.type_text("1");
    assert_eq!(focused_buffer_string(&h), "{\n  \"a\": {\n    1\n  }\n}\n");
}

#[test]
fn enter_plaintext_copies_leading_whitespace() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "note.txt", b"\thello\n");
    h.type_keys("A");
    h.type_keys("enter");
    h.settle();
    assert_eq!(focused_buffer_string(&h), "\thello\n\t\n");
}

#[test]
fn enter_indents_each_cursor_by_its_own_line() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "note.txt", b"\tfoo\nbar\n");
    h.type_keys("C");
    h.type_keys("A");
    h.type_keys("enter");
    h.settle();
    assert_eq!(focused_buffer_string(&h), "\tfoo\n\t\nbar\n\n");
}

#[test]
fn each_cursor_lands_after_its_own_continuation() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "note.txt", b"\tfoo\nbar\n");
    h.type_keys("C");
    h.type_keys("A");
    h.type_keys("enter");
    // Typing next is what reveals where each cursor actually landed, the
    // continuations differing in length so a uniform shift misplaces one.
    h.type_keys("x");
    h.settle();
    assert_eq!(focused_buffer_string(&h), "\tfoo\n\tx\nbar\nx\n");
}

#[test]
fn enter_continues_a_comment_only_on_the_comment_line() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"let a = 1;\n// note\n");
    h.type_keys("C");
    h.type_keys("A");
    h.type_keys("enter");
    h.settle();
    assert_eq!(focused_buffer_string(&h), "let a = 1;\n\n// note\n// \n");
}

/// Lay out a hover of `num_lines` lines each `line_width` wide in a
/// `width` x `height` window, anchored at the start of line `anchor_line` of a
/// sixty-line file. Returns the popup and inner rects, and the cursor cell the
/// popup anchors to.
fn hover_layout_at(
    width: u16,
    height: u16,
    num_lines: usize,
    line_width: usize,
    anchor_line: usize,
) -> (Rect, Rect, (u16, u16)) {
    use crate::{render::hover::HoverPopup, test_harness::TestHarness};
    use ratatui::style::Style;

    let mut h = TestHarness::with_size(width, height);
    let root = std::path::PathBuf::from("/hover");
    let path = root.join("a.txt");
    let content: String = (0..60).map(|i| format!("line{i}\n")).collect();
    h.fake_fs().insert_file(&path, content.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    h.stoat.render();

    let offset: usize = content
        .lines()
        .take(anchor_line)
        .map(|line| line.len() + 1)
        .sum();
    let text = "x".repeat(line_width);
    let lines = (0..num_lines)
        .map(|_| vec![(text.clone(), Style::default())])
        .collect();
    let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
    h.stoat.pending_hover = Some(HoverPopup::new(lines, offset, editor_id));

    let (_, cursor) = crate::render::cursor_popup::focused_editor_popup_ctx(&mut h.stoat, offset)
        .expect("the anchor is on screen");
    let (popup, inner) =
        crate::render::hover::hover_popup_layout(&mut h.stoat).expect("hover layout");
    (popup, inner, cursor)
}

/// [`hover_layout_at`] anchored at the top of the file.
fn hover_layout(width: u16, height: u16, num_lines: usize, line_width: usize) -> (Rect, Rect) {
    let (popup, inner, _) = hover_layout_at(width, height, num_lines, line_width, 0);
    (popup, inner)
}

/// A hover low on the page opens above the cursor at the full capped height,
/// rather than as a cramped box in the seven rows below it.
#[test]
fn a_hover_low_on_the_page_opens_above_at_full_height() {
    let (popup, _, cursor) = hover_layout_at(60, 30, 20, 20, 22);
    assert_eq!(
        (popup.y + popup.height, popup.height),
        (cursor.1, 15),
        "half of 30 rows is the cap, and the box ends on the row above the cursor",
    );
}

/// A hover that fits below keeps its place there, low on the page or not.
#[test]
fn a_short_hover_low_on_the_page_still_opens_below() {
    let (popup, _, cursor) = hover_layout_at(60, 30, 2, 20, 24);
    assert_eq!((popup.y, popup.height), (cursor.1 + 1, 4));
}

/// A hover too tall for either side at full height takes the side with more
/// room, shrunk to the cap.
#[test]
fn a_hover_that_fits_neither_side_takes_the_larger() {
    let (popup, _, cursor) = hover_layout_at(60, 30, 20, 20, 10);
    let rows_below = 30 - (cursor.1 + 1);
    assert_eq!(
        (popup.y, popup.height),
        (cursor.1 + 1, rows_below.min(15)),
        "the rows below outnumber the rows above",
    );
}

#[test]
fn hover_popup_stays_compact_on_a_small_window() {
    // Thirty lines of hover in a 12-row window used to fill nearly the pane.
    let (popup, _) = hover_layout(40, 12, 30, 20);
    assert!(
        (3..=6).contains(&popup.height),
        "a tall hover on a small window caps near half the pane, got {}",
        popup.height,
    );
}

#[test]
fn hover_popup_caps_at_helix_absolute_limits() {
    // On a large window the absolute caps bound the popup before half-pane.
    let (popup, _) = hover_layout(200, 60, 40, 130);
    assert_eq!(popup.height, 26, "tall content caps at MAX_HEIGHT");
    assert_eq!(popup.width, 120, "wide content caps at MAX_WIDTH");
}

/// A hover popup at a fixed area (`9,1 22x7`) with interior (`10,2 20x5`),
/// `lines` as single unstyled spans and the given scroll offset.
fn hover_sel_popup(lines: &[&str], scroll_half_pages: usize) -> crate::render::hover::HoverPopup {
    use ratatui::style::Style;
    let mut popup = crate::render::hover::HoverPopup::new(
        lines
            .iter()
            .map(|l| vec![(l.to_string(), Style::default())])
            .collect(),
        0,
        EditorId::default(),
    );
    popup.scroll_half_pages = scroll_half_pages;
    popup.area = Rect {
        x: 9,
        y: 1,
        width: 22,
        height: 7,
    };
    popup.inner = Rect {
        x: 10,
        y: 2,
        width: 20,
        height: 5,
    };
    popup
}

#[test]
fn hover_drag_copies_and_leaves_the_editor_untouched() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "buffer text\n");
    h.stoat.pending_hover = Some(hover_sel_popup(&["hello world", "second line"], 0));

    // Down at inner (10,2) resolves to (line 0, col 0). The drag to (13,2) is
    // three cells in, which the 0.85x popover scale maps to char column 4, so
    // the copied span is "hell".
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 2));
    h.stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 13, 2));
    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 13, 2));

    assert_eq!(h.fake_clipboard().writes(), vec!["hell"]);
    assert!(
        h.stoat.editor_drag.is_none(),
        "a hover drag never arms the editor selection",
    );
    assert!(
        h.stoat.pending_hover.as_ref().unwrap().selection.is_some(),
        "the selection stays live after release",
    );
}

/// The key path closes the popup on any key it does not scroll with, then
/// dispatches. A click outside owes the same, so the press still lands
/// rather than being spent on the dismissal.
#[test]
fn a_press_outside_the_hover_closes_it_and_still_lands() {
    for button in [MouseButton::Left, MouseButton::Middle] {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, "buffer text\nsecond line\n");
        h.stoat.pending_hover = Some(hover_sel_popup(&["hello world"], 0));

        // Column 2 of row 0 sits left of the popup's rect, which starts at
        // x 9, so the press is outside it and inside the editor.
        h.stoat
            .update(mouse_event(MouseEventKind::Down(button), 2, 0));

        assert!(
            h.stoat.pending_hover.is_none() && h.stoat.pending_hover_request.is_none(),
            "{button:?} outside the popup closes it and its in-flight request"
        );
        assert_eq!(
            h.primary_head_offset(),
            2,
            "{button:?} still places the cursor where it was aimed"
        );
    }
}

#[test]
fn unplaceable_hover_popup_stops_consuming_mouse_input() {
    use crate::render::hover::HoverPopup;
    use ratatui::style::Style;

    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "alpha beta gamma\n");
    let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
    h.stoat.pending_hover = Some(HoverPopup::new(
        vec![vec![("hover".to_string(), Style::default())]],
        0,
        editor_id,
    ));

    // The first render stamps the popup's real screen rect.
    let _ = h.stoat.render();
    let rendered = h.stoat.pending_hover.as_ref().unwrap().area;
    assert_ne!(rendered, Rect::default(), "the popup renders a rect");

    // Make the anchor unplaceable (past the rope), then render again.
    h.stoat.pending_hover.as_mut().unwrap().anchor_offset = 10_000;
    let _ = h.stoat.render();
    assert_eq!(
        h.stoat.pending_hover.as_ref().unwrap().area,
        Rect::default(),
        "an unplaceable popup resets its stored rect",
    );

    // A Down inside the previously rendered rect falls through to the pane
    // instead of the stale area swallowing it as a hover selection. The
    // reset rect puts every point outside the popup, so the press also
    // dismisses it on the way through.
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        rendered.x + 1,
        rendered.y + 1,
    ));
    assert!(
        h.stoat.pending_hover.is_none(),
        "the stale rect takes the click as a dismissal, not as a selection",
    );
}

#[test]
fn hover_drag_outside_the_rect_clamps_into_the_popup() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "x\n");
    h.stoat.pending_hover = Some(hover_sel_popup(&["hello world"], 0));

    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 12, 2));
    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        200,
        200,
    ));

    let sel = h.stoat.pending_hover.as_ref().unwrap().selection.unwrap();
    assert_eq!(sel.anchor, (0, 2));
    assert_eq!(
        sel.head,
        (0, 11),
        "a drag past the rect clamps to the last line and its char count",
    );
}

#[test]
fn hover_selection_maps_through_the_scroll_offset() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "x\n");
    let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    // Interior height 5 => half_page 2; scroll 3 => scroll = min(15, 6) = 6.
    h.stoat.pending_hover = Some(hover_sel_popup(&refs, 3));

    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 2));

    let sel = h.stoat.pending_hover.as_ref().unwrap().selection.unwrap();
    assert_eq!(
        sel.anchor.0, 6,
        "the top row maps to the first scrolled line"
    );
}

#[test]
fn hover_hit_test_inverts_the_stoatty_scale() {
    use crate::render::hover::HoverPopup;
    use ratatui::style::Style;

    let mut popup = HoverPopup::new(
        vec![vec![("x".repeat(60), Style::default())]],
        0,
        EditorId::default(),
    );
    popup.inner = Rect {
        x: 0,
        y: 0,
        width: 50,
        height: 3,
    };
    for cell in 0..40u16 {
        let (line, col) = crate::render::hover::hover_hit_test(&popup, cell, 0);
        assert_eq!(line, 0);
        assert_eq!(col, (cell as usize * 256 + 128) / 218);
    }
}

#[test]
fn hover_grid_highlight_paints_the_selection_bg() {
    use crate::{
        render::hover::{HoverPopup, HoverSelection},
        test_harness::TestHarness,
    };
    use ratatui::style::Style;

    let mut h = TestHarness::with_size(60, 20);
    let root = std::path::PathBuf::from("/hover");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
    let mut popup = HoverPopup::new(
        vec![vec![("hello world".to_string(), Style::default())]],
        0,
        editor_id,
    );
    popup.selection = Some(HoverSelection {
        anchor: (0, 0),
        head: (0, 4),
        dragging: false,
    });
    h.stoat.pending_hover = Some(popup);

    let buf = h.stoat.render();
    let inner = h.stoat.pending_hover.as_ref().unwrap().inner;
    let sel_bg = h
        .stoat
        .theme
        .get(crate::theme::scope::UI_SELECTION)
        .bg
        .expect("theme has a selection background");

    for c in 0..4u16 {
        assert_eq!(
            buf[(inner.x + c, inner.y)].bg,
            sel_bg,
            "selected cell {c} carries the selection background",
        );
    }
    assert_ne!(
        buf[(inner.x + 5, inner.y)].bg,
        sel_bg,
        "a cell past the selection keeps the modal background",
    );
}

#[test]
fn hover_y_yanks_the_live_selection() {
    use crate::{register::Register, render::hover::HoverSelection};

    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "x\n");
    h.stoat.pending_hover = Some(hover_sel_popup(&["hello world"], 0));
    if let Some(popup) = h.stoat.pending_hover.as_mut() {
        popup.selection = Some(HoverSelection {
            anchor: (0, 0),
            head: (0, 5),
            dragging: false,
        });
    }

    h.type_keys("y");

    assert_eq!(
        h.stoat.registers.read(Register::Unnamed),
        Some(["hello".to_string()].as_slice()),
        "y yanks the selected text into the register",
    );
    assert!(
        h.stoat.pending_hover.is_some(),
        "the popup and selection stay open after a yank",
    );
}

/// The hover yank reads no register a command before it already spent.
///
/// It is intercepted ahead of the dispatch that spends the selection for
/// every other command, so without spending its own it reads whatever the
/// last command left behind.
#[test]
fn hover_y_does_not_reuse_a_spent_register() {
    use crate::{register::Register, render::hover::HoverSelection};

    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "x\n");
    h.stoat.selected_register = Some(Register::Named('a'));
    action_handlers::dispatch(&mut h.stoat, &stoat_action::FlipSelections);

    h.stoat.pending_hover = Some(hover_sel_popup(&["hello world"], 0));
    if let Some(popup) = h.stoat.pending_hover.as_mut() {
        popup.selection = Some(HoverSelection {
            anchor: (0, 0),
            head: (0, 5),
            dragging: false,
        });
    }

    h.type_keys("y");

    assert_eq!(
        h.stoat.registers.read(Register::Named('a')),
        None,
        "the command between them spent the selection",
    );
    assert_eq!(
        h.stoat.registers.read(Register::Unnamed),
        Some(["hello".to_string()].as_slice()),
    );
}

#[test]
fn hover_y_without_a_selection_closes_the_popup() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "x\n");
    h.stoat.pending_hover = Some(hover_sel_popup(&["hello world"], 0));

    h.type_keys("y");

    assert!(
        h.stoat.pending_hover.is_none(),
        "y with no selection closes the popup like any other key",
    );
}

#[test]
fn hover_drag_under_stoatty_maps_through_the_apc_scale() {
    let mut h = Stoat::test();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    let _ = open_scratch_file(&mut h, "x\n");
    let long = "x".repeat(40);
    h.stoat.pending_hover = Some(hover_sel_popup(&[&long], 0));

    // inner.x is 10. A pointer 10 cells in maps through the 0.85x scale.
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 2));
    h.stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 20, 2));

    let sel = h.stoat.pending_hover.as_ref().unwrap().selection.unwrap();
    assert_eq!(sel.anchor, (0, 0));
    assert_eq!(
        sel.head.1,
        (10 * 256 + 128) / 218,
        "the drag column maps through the stoatty 256/218 inverse",
    );
}

#[test]
fn space_a_z_widens_and_restores_the_focused_pane() {
    let mut h = Stoat::test();
    let full_width = {
        let panes = &h.stoat.active_workspace().panes;
        panes.pane(panes.focus()).area.width
    };

    h.type_keys("space a s");
    assert_eq!(h.stoat.active_workspace().panes.pane_count(), 2);

    h.type_keys("space a z");
    {
        let panes = &h.stoat.active_workspace().panes;
        let focused = panes.focus();
        assert_eq!(panes.widened(), Some(focused));
        assert_eq!(
            panes.pane(focused).area.width,
            full_width,
            "the widened pane spans full width"
        );
    }
    assert_eq!(h.stoat.pending_message.as_deref(), Some("pane widened"));

    h.type_keys("space a z");
    assert_eq!(
        h.stoat.active_workspace().panes.widened(),
        None,
        "toggling again restores the layout"
    );
    assert_eq!(h.stoat.pending_message.as_deref(), Some("pane widen off"));
}

#[test]
fn space_a_z_reports_when_the_layout_blocks_widen() {
    let mut h = Stoat::test();
    h.type_keys("space a s");
    h.type_keys("space a v");
    h.type_keys("space a k");
    h.type_keys("space a z");

    assert_eq!(h.stoat.active_workspace().panes.widened(), None);
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("cannot widen: pane edges don't align"),
    );
}

#[test]
fn detached_focus_parks_no_primary_cursor() {
    let mut h = Stoat::test();
    h.stoat.window_ipc_connected = true;

    let path = std::path::PathBuf::from("/w/a.txt");
    h.fake_fs().insert_file(&path, b"hello\n");
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    h.resize(80, 24);
    h.type_action("SplitRight()");
    h.settle();

    // Stand in for the live paint's recorded cursor cell so the split-pane
    // baseline is Some. The windowed check must return None regardless of it.
    let (editor_id, _) = h.stoat.focused_editor_ids().expect("focused editor");
    h.stoat
        .active_workspace_mut()
        .editors
        .get_mut(editor_id)
        .unwrap()
        .cursor_screen_cell = Some((7, 3));
    assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((7, 3)));

    h.type_action("DetachPane()");
    assert_eq!(
        h.stoat.primary_cursor_screen_pos(),
        None,
        "a detached focus draws its cursor in its window, not the primary"
    );
}

fn window_region() -> PoolRegionCommand {
    PoolRegionCommand {
        pool: 1,
        top: 0,
        left: 0,
        width: 80,
        height: 23,
        window: 2,
    }
}

/// Every input the terminal render reads has to reach the version, because
/// the version is what decides whether the render runs at all. A field left
/// out shows the user a stale pane until something else happens to move.
#[test]
fn a_terminal_pane_versions_on_everything_its_render_reads() {
    let mut h = Stoat::test();
    let session: Arc<dyn crate::host::TerminalSession> =
        Arc::new(crate::host::FakeTerminalSession::new());
    let term_id = h
        .stoat
        .active_workspace_mut()
        .terms
        .insert(TermSession::new(
            crate::term_screen::TermScreen::new(24, 80),
            session,
            TermSession::next_token(),
        ));

    let view = View::Terminal(term_id);
    let region = window_region();
    let version = |ws: &Workspace, focused, epoch, region| {
        window_content_version(&view, region, focused, epoch, ws).expect("a terminal is versioned")
    };

    let ws = h.stoat.active_workspace();
    let base = version(ws, true, 0, region);
    assert_eq!(
        base,
        version(ws, true, 0, region),
        "an untouched terminal holds still"
    );
    assert_ne!(
        base,
        version(ws, false, 0, region),
        "focus draws the cursor cell"
    );
    assert_ne!(
        base,
        version(ws, true, 1, region),
        "the theme recolors every cell"
    );
    assert_ne!(
        base,
        version(
            ws,
            true,
            0,
            PoolRegionCommand {
                height: 12,
                ..region
            }
        ),
        "a resized pane must re-declare its region"
    );

    h.stoat.active_workspace_mut().terms[term_id].selection = Some(TermSelection::new(1, 1));
    let selected = version(h.stoat.active_workspace(), true, 0, region);
    assert_ne!(base, selected, "a selection tints the cells it covers");

    h.stoat.active_workspace_mut().terms[term_id]
        .term
        .feed(b"output");
    assert_ne!(
        selected,
        version(h.stoat.active_workspace(), true, 0, region),
        "output repaints the screen"
    );
}

/// Every input the run render reads has to reach the version, on the same
/// terms as the terminal sibling above.
#[test]
fn a_run_pane_versions_on_everything_its_render_reads() {
    let mut h = Stoat::test();
    let exec = h.stoat.executor.clone();
    let run_id = {
        let ws = h.stoat.active_workspace_mut();
        let state = crate::run::RunState::new(PathBuf::from("/work"), ws, exec);
        ws.runs.insert(state)
    };

    let view = View::Run(run_id);
    let region = window_region();
    let version = |ws: &Workspace| {
        window_content_version(&view, region, true, 0, ws).expect("a run pane is versioned")
    };

    let base = version(h.stoat.active_workspace());
    assert_eq!(
        base,
        version(h.stoat.active_workspace()),
        "an idle run pane holds still"
    );

    h.stoat.active_workspace_mut().runs[run_id]
        .blocks
        .push(crate::run::OutputBlock::new(
            "ls".into(),
            PathBuf::from("/work"),
            80,
        ));
    let submitted = version(h.stoat.active_workspace());
    assert_ne!(base, submitted, "a submitted command adds a prompt line");

    h.stoat.active_workspace_mut().runs[run_id].blocks[0].feed(b"a.txt\n");
    let fed = version(h.stoat.active_workspace());
    assert_ne!(submitted, fed, "output fills the block's grid");

    h.stoat.active_workspace_mut().runs[run_id].blocks[0].exit_status = Some(1);
    let exited = version(h.stoat.active_workspace());
    assert_ne!(fed, exited, "the exit code flags the next prompt");

    h.stoat.active_workspace_mut().runs[run_id].scroll_offset = 3;
    let scrolled = version(h.stoat.active_workspace());
    assert_ne!(exited, scrolled, "scrolling picks different output lines");

    let input_editor = h.stoat.active_workspace().runs[run_id].input.editor_id;
    h.stoat.active_workspace_mut().editors[input_editor].scroll_row = 1;
    assert_ne!(
        scrolled,
        version(h.stoat.active_workspace()),
        "the input line scrolls under a long command"
    );
}

/// A view whose sources are untracked has to say so, since the caller reads
/// `None` as "paint it and hash what came out" rather than "unchanged".
#[test]
fn an_untracked_view_kind_reports_no_input_version() {
    let h = Stoat::test();
    assert_eq!(
        window_content_version(
            &View::Label("scratch".into()),
            window_region(),
            true,
            0,
            h.stoat.active_workspace(),
        ),
        None
    );
}

fn stoat_with_detached_editor(lines: usize) -> (crate::test_harness::TestHarness, PaneId, u32) {
    let mut h = Stoat::test();
    h.stoat.window_ipc_connected = true;
    let root = PathBuf::from("/aux-mouse");
    let path = root.join("a.txt");
    let body = (0..lines)
        .map(|i| format!("line {i}\n"))
        .collect::<String>();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    h.resize(80, 24);
    h.type_action("SplitRight()");
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);
    h.type_action("DetachPane()");
    h.settle();
    let (detached, window) = h.stoat.active_workspace().panes.windowed_panes()[0];
    (h, detached, window)
}

#[test]
fn aux_click_lands_in_the_bound_pane_not_a_primary() {
    let (mut h, detached, window) = stoat_with_detached_editor(40);

    // Focus a primary split pane, so the click can only reach the detached
    // pane by resolving the window binding, never the grid hit-test.
    action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusPane { index: 1 });
    assert_ne!(
        h.stoat.active_workspace().panes.focus(),
        detached,
        "a primary pane holds focus before the aux click"
    );

    h.stoat
        .handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Mouse {
            window,
            kind: MouseKind::Press(IpcMouseButton::Left),
            col: 3,
            row: 2,
            mods: 0,
        }));

    assert_eq!(
        h.stoat.active_workspace().panes.focus(),
        detached,
        "the aux click resolves the bound pane, not a primary pane whose rect overlaps"
    );
    assert!(
        h.stoat.editor_drag.is_some(),
        "the click placed a block cursor in the detached editor and armed drag"
    );
}

#[test]
fn aux_wheel_scrolls_the_bound_editor() {
    let (mut h, detached, window) = stoat_with_detached_editor(200);
    let View::Editor(editor_id) = h.stoat.active_workspace().panes.pane(detached).view else {
        panic!("detached pane is an editor");
    };
    let before = h
        .stoat
        .active_workspace()
        .editors
        .get(editor_id)
        .unwrap()
        .scroll_row;

    h.stoat
        .handle_window_ipc(WindowIpc::Event(WindowIpcEvent::Mouse {
            window,
            kind: MouseKind::WheelDown,
            col: 3,
            row: 2,
            mods: 0,
        }));

    let after = h
        .stoat
        .active_workspace()
        .editors
        .get(editor_id)
        .unwrap()
        .scroll_row;
    assert!(
        after > before,
        "the aux wheel advances the detached editor's scroll target"
    );
}

/// A wheel notch moves the scroll row, which the inlay-hint request keys
/// on, so a flick used to arm and cancel a request per notch and throw
/// every one away. Only the viewport the glide lands on is worth asking
/// about, and the settle is where it asks, since a frame tick never reaches
/// the trigger epilogue at the end of `update`.
#[test]
fn a_wheel_glide_requests_inlay_hints_once_at_the_settle() {
    use lsp_types::{OneOf, ServerCapabilities};

    let mut h = Stoat::test();
    h.fake_lsp().set_capabilities(ServerCapabilities {
        inlay_hint_provider: Some(OneOf::Left(true)),
        ..Default::default()
    });

    let root = PathBuf::from("/glide-hints");
    let path = root.join("a.rs");
    let body: String = (0..400).map(|i| format!("let x{i} = 1\n")).collect();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    h.type_keys("space l h");
    h.advance_clock(std::time::Duration::from_millis(150));
    let resting = h
        .stoat
        .last_inlay_hint_key
        .expect("enabling hints requested the resting viewport");

    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
        for _ in 0..5 {
            action_handlers::view::wheel_scroll(editor, true);
        }
    }
    // The trigger every one of those notches ran through, had the glide not
    // held it back.
    action_handlers::lsp::inlay_hints_trigger(&mut h.stoat);
    assert_eq!(
        h.stoat.last_inlay_hint_key,
        Some(resting),
        "a glide in flight requests nothing, however far the rows moved",
    );

    for _ in 0..1000 {
        let animating = h.stoat.is_animating();
        h.stoat.frame_tick(0.016);
        if !animating {
            break;
        }
    }
    assert!(!h.stoat.is_animating(), "the glide settles");

    let landed = h
        .stoat
        .last_inlay_hint_key
        .expect("the settle requested once");
    assert_ne!(landed, resting, "the landed viewport is what finally asks",);
    assert_eq!(
        landed.2,
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .scroll_row,
        "and it asks about the row the glide landed on",
    );
}

#[test]
fn spinner_phase_advances_and_wraps() {
    assert_eq!(spinner_phase(0.0), 0);
    assert_eq!(spinner_phase(0.05), 0, "within the first frame window");
    assert_eq!(spinner_phase(0.15), 1, "second frame");
    assert_eq!(spinner_phase(0.95), 9, "last frame of the cycle");
    assert_eq!(spinner_phase(1.05), 0, "wraps to the first frame");
    assert_eq!(spinner_phase(1.15), 1);
}

#[test]
fn frame_tick_repaints_the_spinner_only_when_the_phase_advances() {
    use crate::host::LspNotification;
    use lsp_types::{NumberOrString, WorkDoneProgress, WorkDoneProgressBegin};
    let mut h = Stoat::test();
    h.fake_lsp().push_notification(LspNotification::Progress {
        token: NumberOrString::Number(1),
        value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
            title: "indexing".into(),
            cancellable: None,
            message: None,
            percentage: None,
        }),
    });
    h.drain_lsp();
    assert!(h.stoat.lsp_progress.current().is_some(), "progress is live");

    assert_eq!(
        h.stoat.frame_tick(0.1),
        UpdateEffect::Redraw,
        "a full frame interval advances the phase and repaints"
    );

    h.stoat.spinner_clock = 0.0;
    assert_eq!(
        h.stoat.frame_tick(0.01),
        UpdateEffect::None,
        "a sub-frame tick leaves the phase put, so no repaint"
    );
}

#[test]
fn snapshot_lsp_progress_indexing() {
    use crate::{action_handlers::dispatch, host::LspNotification};
    use lsp_types::{NumberOrString, WorkDoneProgress, WorkDoneProgressBegin};
    let mut h = Stoat::test();
    h.fake_lsp().push_notification(LspNotification::Progress {
        token: NumberOrString::Number(1),
        value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
            title: "indexing".into(),
            cancellable: None,
            message: None,
            percentage: Some(25),
        }),
    });
    h.drain_lsp();
    dispatch(&mut h.stoat, &stoat_action::ToggleLspStatus);
    h.assert_snapshot("lsp_progress_indexing");
}

#[test]
fn error_show_message_wraps_into_a_popout_above_the_bar() {
    use lsp_types::MessageType;
    let mut h = Stoat::test();
    let msg = "rust-analyzer failed to load the workspace: Cargo.toml is malformed and could not be parsed, so diagnostics are unavailable";
    h.stoat.lsp_message = Some((MessageType::ERROR, msg.to_string()));

    let buf = h.render_composited();
    let rows: Vec<String> = (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
        .collect();
    let bar = rows.len() - 1;

    assert!(
        !rows[bar].contains("rust-analyzer"),
        "the bar row no longer carries the error text"
    );
    let head = rows
        .iter()
        .position(|r| r.contains("rust-analyzer"))
        .expect("error head painted");
    let tail = rows
        .iter()
        .position(|r| r.contains("diagnostics"))
        .expect("error tail painted");
    assert!(tail < bar, "the popout sits above the bar");
    assert!(tail > head, "the long message wrapped onto a second row");
}

#[test]
fn warning_show_message_still_paints_in_the_bar() {
    use lsp_types::MessageType;
    let mut h = Stoat::test();
    h.stoat.lsp_message = Some((MessageType::WARNING, "cargo check is slow".to_string()));

    let buf = h.render_composited();
    let bar = buf.area.height - 1;
    let bar_row: String = (0..buf.area.width)
        .map(|x| buf[(x, bar)].symbol())
        .collect();

    assert!(
        bar_row.replace('─', " ").contains("cargo check is slow"),
        "a warning keeps painting in the status bar:\n{bar_row}"
    );
}

#[test]
fn snapshot_lsp_show_message_error() {
    use crate::host::LspNotification;
    use lsp_types::MessageType;
    let mut h = Stoat::test();
    h.fake_lsp()
        .push_notification(LspNotification::ShowMessage {
            typ: MessageType::ERROR,
            message: "rust-analyzer failed to load".to_string(),
        });
    h.drain_lsp();
    h.assert_snapshot("lsp_show_message_error");
}

#[test]
fn user_config_overrides_embedded_setting() {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let stoat = Stoat::new_with_user_config(
        scheduler.executor(),
        Settings::default(),
        PathBuf::new(),
        Some("on init { format_on_save = true; }".to_string()),
        Vec::new(),
        None,
    );

    assert_eq!(stoat.settings.format_on_save, Some(true));
    assert_eq!(
        stoat.pending_message, None,
        "a clean parse shows no message"
    );
}

fn stoat_with_user_config(source: &str) -> Stoat {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    Stoat::new_with_user_config(
        scheduler.executor(),
        Settings::default(),
        PathBuf::new(),
        Some(source.to_string()),
        Vec::new(),
        None,
    )
}

/// A user config names the keys that user cares about, so every key it
/// leaves alone must keep the binding stoat ships.
#[test]
fn a_user_keymap_keeps_every_default_binding() {
    let stoat = stoat_with_user_config("on key { !modal && mode == normal { q -> Quit(); } }");
    let state = StoatKeymapState::new("normal");
    let bound = |code| {
        stoat
            .keymap
            .lookup(&state, &KeyEvent::new(code, KeyModifiers::NONE))
            .map(|actions| actions[0].name.clone())
    };

    assert_eq!(
        bound(KeyCode::Char('q')).as_deref(),
        Some("Quit"),
        "the user's own binding wins its key"
    );
    assert_eq!(
        bound(KeyCode::Char('i')).as_deref(),
        Some("EnterInsertMode"),
        "a key the user never bound keeps the shipped binding"
    );
}

/// The shipped `q` sits under a `!modal` guard, and a user who wants `q`
/// back writes the block they care about rather than copying that guard.
/// A negation scores no specificity, so the two tie and the user's layer
/// wins on source order. A scoring change that ranked the guard would take
/// the key back without failing anything else.
#[test]
fn a_bare_user_binding_beats_the_guarded_default_for_its_key() {
    let stoat = stoat_with_user_config("on key { mode == normal { q -> Quit(); } }");

    let actions = stoat
        .keymap
        .lookup(
            &StoatKeymapState::new("normal"),
            &KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .expect("q stays bound");
    assert_eq!(actions[0].name, "Quit");
}

/// The counterpart to the tie rule: a block naming its context positively
/// carries more atoms than a broad user block and must keep outranking it.
/// Without that, one `mode == insert` binding hijacks the same key in every
/// picker, which is the dead-picker class layering exists to end.
#[test]
fn a_positively_scoped_default_still_beats_a_broader_user_binding() {
    let stoat = stoat_with_user_config("on key { mode == insert { Down -> MoveDown(); } }");

    let state = StoatKeymapState::new("insert").with_modal("workspace_picker");
    let actions = stoat
        .keymap
        .lookup(&state, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .expect("the picker keeps its Down");
    assert_eq!(actions[0].name, "PickerNext");
}

/// The layer drops a user binding naming a renamed action, so the shipped
/// binding takes the key back rather than leaving it dead. The badge still
/// counts it, because the count is taken before layering.
#[test]
fn a_stale_user_binding_falls_back_to_the_default() {
    let stoat = stoat_with_user_config(
        "on key { modal == workspace_picker { Down -> WorkspacePickerNext(); } }",
    );

    let state = StoatKeymapState::new("insert").with_modal("workspace_picker");
    let actions = stoat
        .keymap
        .lookup(&state, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .expect("the shipped binding takes the key back");
    assert_eq!(actions[0].name, "PickerNext");

    let id = stoat
        .badges
        .find_by_source(BadgeSource::ConfigActions)
        .expect("the badge still names the stale binding");
    assert_eq!(
        stoat.badges.get(id).expect("badge").label,
        "config binds 1 unknown action",
    );
}

#[test]
fn user_theme_block_layers_over_embedded_base() {
    use crate::theme::scope::{UI_MODAL_PALETTE, UI_TEXT};
    use ratatui::style::Color;

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let base = Stoat::new_with_user_config(
        scheduler.executor(),
        Settings::default(),
        PathBuf::new(),
        None,
        Vec::new(),
        None,
    );
    let layered = Stoat::new_with_user_config(
        scheduler.executor(),
        Settings::default(),
        PathBuf::new(),
        Some("theme default_dark { ui.modal.palette.fg = \"#ff0000\"; }".to_string()),
        Vec::new(),
        None,
    );

    let red = Some(Color::Rgb(255, 0, 0));
    assert_eq!(
        layered.theme.get(UI_MODAL_PALETTE).fg,
        red,
        "the user override recolors the modal palette border",
    );
    assert_ne!(
        base.theme.get(UI_MODAL_PALETTE).fg,
        red,
        "the embedded base is not already red on its own",
    );
    assert_eq!(
        layered.theme.get(UI_TEXT).fg,
        base.theme.get(UI_TEXT).fg,
        "a scope the user did not touch keeps the embedded value",
    );
}

#[test]
fn user_only_theme_name_resolves() {
    use crate::theme::scope::UI_TEXT;
    use ratatui::style::Color;

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let stoat = Stoat::new_with_user_config(
        scheduler.executor(),
        Settings::default(),
        PathBuf::new(),
        Some("theme mine { ui.text.fg = \"#00ff00\"; }\non init { theme = mine; }".to_string()),
        Vec::new(),
        None,
    );

    assert_eq!(stoat.theme.name, "mine", "the user-only theme activates");
    assert_eq!(
        stoat.theme.get(UI_TEXT).fg,
        Some(Color::Rgb(0, 255, 0)),
        "its scopes resolve without an embedded base of the same name",
    );
}

#[test]
fn broken_user_config_falls_back_to_embedded_with_status() {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let stoat = Stoat::new_with_user_config(
        scheduler.executor(),
        Settings::default(),
        PathBuf::new(),
        Some("on init { format_on_save = ".to_string()),
        Vec::new(),
        None,
    );

    assert_eq!(
        stoat.settings.format_on_save,
        Some(false),
        "the embedded default survives a broken user config"
    );
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("user config parse failed; using built-in defaults")
    );
}

/// A config naming an action the registry dropped parses clean, so nothing
/// but this badge tells the user why the key does nothing.
#[test]
fn a_startup_config_binding_an_unknown_action_raises_a_badge() {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let stoat = Stoat::new_with_user_config(
        scheduler.executor(),
        Settings::default(),
        PathBuf::new(),
        Some("on key { Down -> WorkspacePickerNext(); }".to_string()),
        Vec::new(),
        None,
    );

    let id = stoat
        .badges
        .find_by_source(BadgeSource::ConfigActions)
        .expect("a stale binding raises a badge");
    assert_eq!(
        stoat.badges.get(id).expect("badge").label,
        "config binds 1 unknown action",
    );
}

#[test]
fn reloading_a_repaired_config_clears_the_unknown_action_badge() {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let mut stoat = Stoat::new(
        scheduler.executor(),
        Settings::default(),
        PathBuf::from("/repo"),
    );

    stoat.reload_user_config(
        r#"on key {
                modal == "workspace_picker" {
                    Down -> WorkspacePickerNext();
                    Tab -> WorkspacePickerComplete();
                }
            }"#,
    );
    let id = stoat
        .badges
        .find_by_source(BadgeSource::ConfigActions)
        .expect("a stale binding raises a badge");
    assert_eq!(
        stoat.badges.get(id).expect("badge").label,
        "config binds 2 unknown actions",
    );

    stoat.reload_user_config(DEFAULT_KEYMAP);
    assert_eq!(
        stoat.badges.find_by_source(BadgeSource::ConfigActions),
        None,
        "a repaired config retires the badge",
    );
}

#[test]
fn user_vscode_theme_joins_the_pool() {
    use ratatui::style::Color;

    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let user_themes = vec![(
            "my-gruvbox".to_string(),
            r##"{ "name": "my-gruvbox", "type": "dark", "colors": { "editor.background": "#282828" } }"##
                .to_string(),
        )];
    let stoat = Stoat::new_with_user_config(
        scheduler.executor(),
        Settings::default(),
        PathBuf::new(),
        None,
        user_themes,
        None,
    );

    assert!(
        stoat.theme_pool.contains("my-gruvbox"),
        "the user theme joins the pool",
    );
    assert!(
        stoat.theme_pool.contains("gruvbox-dark"),
        "the built-in themes join the pool too",
    );

    let theme = stoat
        .theme_pool
        .resolve("my-gruvbox")
        .expect("theme resolves");
    assert_eq!(
        theme.get("ui.background").bg,
        Some(Color::Rgb(0x28, 0x28, 0x28))
    );
}

#[test]
fn only_the_resolved_theme_converts() {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let user_themes = vec![(
        "unused".to_string(),
        r##"{ "name": "unused", "colors": { "editor.background": "#282828" } }"##.to_string(),
    )];
    let mut stoat = Stoat::new_with_user_config(
        scheduler.executor(),
        Settings::default(),
        PathBuf::new(),
        None,
        user_themes,
        None,
    );

    assert_eq!(
        stoat.theme.name, "default_dark",
        "an embedded theme is active"
    );
    assert!(
        stoat.imported_themes.iter().all(|t| !t.is_converted()),
        "startup resolves an embedded theme, so no VSCode theme is converted",
    );

    action_handlers::dispatch(
        &mut stoat,
        &stoat_action::SetTheme {
            name: "unused".to_string(),
        },
    );
    let converted: Vec<&str> = stoat
        .imported_themes
        .iter()
        .filter(|t| t.is_converted())
        .map(|t| t.name())
        .collect();
    assert_eq!(
        converted,
        ["unused"],
        "selecting a theme converts that theme and no other",
    );
}

#[test]
fn broken_user_theme_surfaces_when_selected() {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    let user_themes = vec![("bad".to_string(), "{ not json".to_string())];
    let mut stoat = Stoat::new_with_user_config(
        scheduler.executor(),
        Settings::default(),
        PathBuf::new(),
        None,
        user_themes,
        None,
    );

    assert_eq!(
        stoat.pending_message, None,
        "an unselected theme is never read, so startup reports nothing",
    );
    assert!(
        stoat.theme_pool.contains("bad"),
        "the theme is listed by file stem, which reading it cannot change",
    );

    let before = stoat.theme.name.clone();
    action_handlers::dispatch(
        &mut stoat,
        &stoat_action::SetTheme {
            name: "bad".to_string(),
        },
    );
    assert!(
        stoat
            .pending_message
            .as_deref()
            .unwrap_or_default()
            .contains("theme bad failed"),
        "selecting it surfaces the failure in the transient status: {:?}",
        stoat.pending_message,
    );
    assert_eq!(stoat.theme.name, before, "the active theme is kept");
}

fn stoat_with_env_theme(user_config: Option<&str>, cli: Settings, env: &str) -> Stoat {
    let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
    Stoat::new_with_user_config(
        scheduler.executor(),
        cli,
        PathBuf::new(),
        user_config.map(str::to_string),
        Vec::new(),
        Some(env.to_string()),
    )
}

#[test]
fn env_theme_beats_the_embedded_default() {
    let stoat = stoat_with_env_theme(None, Settings::default(), "gruvbox-dark");

    assert_eq!(
        stoat.theme.name, "gruvbox-dark",
        "with nothing explicit set, the environment names the theme"
    );
}

#[test]
fn env_theme_applies_when_the_user_config_names_no_theme() {
    let stoat = stoat_with_env_theme(
        Some("on init { format_on_save = true; }"),
        Settings::default(),
        "gruvbox-dark",
    );

    assert_eq!(
        stoat.theme.name, "gruvbox-dark",
        "a user config that sets other settings does not claim the theme"
    );
}

#[test]
fn user_config_theme_beats_the_env_theme() {
    let stoat = stoat_with_env_theme(
        Some("theme mine { ui.text.fg = \"#00ff00\"; }\non init { theme = mine; }"),
        Settings::default(),
        "gruvbox-dark",
    );

    assert_eq!(
        stoat.theme.name, "mine",
        "an explicit user-config theme outranks the environment"
    );
}

#[test]
fn cli_theme_beats_the_env_theme() {
    let cli = Settings {
        theme: Some("one-dark".to_string()),
        ..Settings::default()
    };
    let stoat = stoat_with_env_theme(None, cli, "gruvbox-dark");

    assert_eq!(
        stoat.theme.name, "one-dark",
        "an explicit CLI theme outranks the environment"
    );
}

#[test]
fn unknown_env_theme_keeps_the_embedded_default() {
    let stoat = stoat_with_env_theme(None, Settings::default(), "no-such-theme");

    assert_eq!(
        stoat.theme.name, "default_dark",
        "an unresolvable env theme is ignored rather than blanking the theme"
    );
    assert!(
        stoat.theme.try_get("ui.cursor").is_some(),
        "the default theme's caret style survives an unresolvable env theme"
    );
}

#[test]
fn lsp_message_clears_on_key() {
    use crate::host::LspNotification;
    use lsp_types::MessageType;
    let mut h = Stoat::test();
    h.fake_lsp()
        .push_notification(LspNotification::ShowMessage {
            typ: MessageType::INFO,
            message: "checking".to_string(),
        });
    h.drain_lsp();
    assert_eq!(
        h.stoat.lsp_message,
        Some((MessageType::INFO, "default: checking".to_string())),
        "the stored message is attributed to the reporting server",
    );
    h.type_keys("<Esc>");
    assert!(h.stoat.lsp_message.is_none(), "any key retires the message");
}

#[test]
fn status_message_survives_a_later_keypress() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 12);
    h.stoat.set_status("saved");
    assert_eq!(h.stoat.pending_message.as_deref(), Some("saved"));

    h.type_keys("<Esc>");

    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("saved"),
        "input no longer clears the status message",
    );
}

#[test]
fn status_message_expires_after_its_ttl() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 12);
    h.stoat.set_status("saved");

    h.stoat.render();
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("saved"),
        "the message stays visible before its ttl elapses",
    );

    h.advance_clock(STATUS_MESSAGE_TTL);
    h.stoat.render();

    assert_eq!(
        h.stoat.pending_message, None,
        "the message retires once its ttl elapses and a frame renders",
    );
}

#[test]
fn diagnostics_notification_updates_store() {
    use crate::host::LspNotification;
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Uri};
    use std::{path::PathBuf, str::FromStr};
    let mut h = Stoat::test();
    let path = PathBuf::from("/ws/a.rs");
    let uri = Uri::from_str(&format!("file://{}", path.display())).unwrap();
    let diag = Diagnostic {
        range: Range::new(Position::new(0, 0), Position::new(0, 5)),
        severity: Some(DiagnosticSeverity::ERROR),
        code: None,
        code_description: None,
        source: None,
        message: "boom".into(),
        related_information: None,
        tags: None,
        data: None,
    };
    h.fake_lsp()
        .push_notification(LspNotification::Diagnostics {
            uri,
            diagnostics: vec![diag],
            version: None,
        });
    h.drain_lsp();
    let summary = h.stoat.diagnostics.summarize(&path);
    assert_eq!(summary.error, 1);
    assert_eq!(summary.worst, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn update_effect_merge_keeps_most_urgent() {
    let none = UpdateEffect::None;
    let redraw = UpdateEffect::Redraw;
    let quit = UpdateEffect::Quit;
    assert_eq!(none.merge(redraw), redraw);
    assert_eq!(redraw.merge(none), redraw);
    assert_eq!(redraw.merge(quit), quit);
    assert_eq!(quit.merge(redraw), quit);
    assert_eq!(none.merge(none), none);
}

#[test]
fn drain_pending_applies_every_queued_event() {
    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    for size in [(80u16, 24u16), (100, 30), (120, 40)] {
        tx.send(Event::Resize(size.0, size.1)).unwrap();
    }
    let (effect, coalesced) = h.stoat.drain_pending(&mut rx);
    assert_eq!(effect, UpdateEffect::Redraw);
    assert_eq!(coalesced, 3, "all three queued events counted");
    assert_eq!(h.stoat.size(), Rect::new(0, 0, 120, 40));
    assert!(rx.try_recv().is_err(), "drain must empty the channel");
}

/// A backlog deeper than any queue bound still lands in one drain, with
/// every send returning rather than waiting for room.
///
/// The sender is the UI thread, inside the same loop that flushes frames
/// and polls stdin. A send that waited would park that loop, and with it
/// the only reader of fd 0, so the backpressure would land in the kernel's
/// tty buffer where an overflow tears escape sequences into garbage. Mouse
/// capture makes a depth like this ordinary rather than exotic, since
/// pointer motion alone produces hundreds of events a second.
#[test]
fn a_backlog_past_any_bound_queues_without_waiting() {
    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();

    // Well past the 64 the channel used to hold, and past any bound that
    // would replace it.
    let sizes: Vec<(u16, u16)> = (0..500).map(|i| (80 + i % 40, 24 + i % 20)).collect();
    for &(width, rows) in &sizes {
        tx.send(Event::Resize(width, rows))
            .expect("a queued send never waits for room");
    }

    let (effect, coalesced) = h.stoat.drain_pending(&mut rx);
    assert_eq!(effect, UpdateEffect::Redraw);
    assert_eq!(coalesced, sizes.len(), "the whole backlog applies at once");

    let (width, rows) = *sizes.last().expect("fixture");
    assert_eq!(
        h.stoat.size(),
        Rect::new(0, 0, width, rows),
        "and the last one queued is the one left standing",
    );
}

#[test]
fn open_run_spawns_shell_with_echo_disabled() {
    let mut h = Stoat::test();
    let run_id = h.open_run();

    assert_eq!(
        h.fake_terminal().sent_bytes().first().map(Vec::as_slice),
        Some(b"stty -echo\n".as_slice()),
        "eager spawn disables tty echo before anything else",
    );

    let input = h
        .stoat
        .active_workspace()
        .runs
        .get(run_id)
        .expect("run state exists")
        .input
        .clone();
    input.replace_text(h.stoat.active_workspace_mut(), "ls");
    action_handlers::dispatch(&mut h.stoat, &stoat_action::RunSubmit);

    let sent = h.fake_terminal().sent_strings();
    assert!(
        sent.get(1).is_some_and(|s| s.starts_with("ls\n")),
        "submit reuses the eager shell to send the command, got {sent:?}",
    );
}

#[test]
fn osc7_updates_run_cwd() {
    let mut h = Stoat::test();
    let run_id = h.open_run();
    h.submit_run("cd /tmp");
    h.inject_run_output(run_id, b"\x1b]7;file:///tmp\x07");

    assert_eq!(
        h.stoat
            .active_workspace()
            .runs
            .get(run_id)
            .expect("run state")
            .cwd,
        std::path::PathBuf::from("/tmp"),
        "an OSC 7 report updates the run pane's cwd",
    );
}

#[test]
fn snapshot_run_pane_prompt_blocks() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 12);
    let run_id = h.open_run();
    h.stoat
        .active_workspace_mut()
        .runs
        .get_mut(run_id)
        .expect("run state")
        .cwd = std::path::PathBuf::from("/work/proj");

    h.submit_run("true");
    h.inject_run_output(run_id, b"ok\n");
    h.inject_run_done(run_id, 0);

    h.submit_run("false");
    h.inject_run_output(run_id, b"boom\n");
    h.inject_run_done(run_id, 5);

    // The unfinished follow-up leaves both its prompt and the input row
    // showing the previous nonzero exit as a red [5].
    h.submit_run("retry");

    h.assert_snapshot("run_pane_prompt_blocks");
}

#[test]
fn run_pane_abbreviates_cwd_under_home() {
    let paint = |home: Option<&str>| {
        let mut h = crate::test_harness::TestHarness::with_size(40, 12);
        if let Some(home) = home {
            h.fake_env().set("HOME", home);
            // The cached home is resolved when the env host is set, so
            // re-inject it now that HOME is populated.
            h.stoat.set_env_host(h.fake_env().clone());
        }
        let run_id = h.open_run();
        h.stoat
            .active_workspace_mut()
            .runs
            .get_mut(run_id)
            .expect("run state")
            .cwd = std::path::PathBuf::from("/home/tester/proj");
        h.submit_run("ls");
        h.rendered_text()
    };

    assert!(
        paint(Some("/home/tester")).contains("~/proj"),
        "a cwd under $HOME (resolved through EnvHost) paints the ~-abbreviated path",
    );

    let full = paint(None);
    assert!(
        full.contains("/h/t/proj"),
        "with no $HOME the prompt paints the plain path with ancestors abbreviated: {full:?}",
    );
    assert!(
        !full.contains("~/proj"),
        "with no $HOME the prompt does not ~-abbreviate",
    );
}

#[test]
fn open_run_lands_in_insert_mode() {
    let mut h = Stoat::test();
    h.open_run();
    assert_eq!(
        h.stoat.focused_mode(),
        "insert",
        "opening a run pane enters insert mode"
    );
}

#[test]
fn run_pane_enter_binds_run_submit_through_keymap() {
    let mut h = Stoat::test();
    h.open_run();
    let state = StoatKeymapState::from_stoat(&h.stoat);
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
    let actions = h
        .stoat
        .keymap
        .lookup(&state, &enter)
        .expect("Enter is bound in a run pane");
    assert!(
        actions.iter().any(|a| a.name == "RunSubmit"),
        "run-pane Enter resolves to RunSubmit, got {actions:?}"
    );
}

#[test]
fn editor_enter_is_unbound_so_it_inserts() {
    let mut h = Stoat::test();
    h.stoat.set_focused_mode("insert".into());
    let state = StoatKeymapState::from_stoat(&h.stoat);
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
    assert!(
        h.stoat.keymap.lookup(&state, &enter).is_none(),
        "editor Enter has no keymap binding, so it falls to the insert newline"
    );
}

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

/// Ctrl-C is unbound outside a run pane, so a modal missing from the close
/// cascade falls through to the quit that ends the session.
#[test]
fn ctrl_c_closes_code_search_rather_than_quitting() {
    let mut h = Stoat::test();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
    assert!(h.stoat.code_search.is_some(), "the modal opened");

    let effect = h.stoat.handle_key(ctrl_c());

    assert!(
        !matches!(effect, UpdateEffect::Quit),
        "closing a modal must not take the session with it"
    );
    assert!(h.stoat.code_search.is_none(), "and the modal is closed");
}

/// A picker owning an input has to be disposed, not dropped: its scratch
/// editor otherwise stays in the workspace for the rest of the session.
#[test]
fn ctrl_c_disposes_the_workspace_pickers_input() {
    let mut h = Stoat::test();
    let before = h.stoat.active_workspace().editors.len();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchWorkspace);
    assert!(h.stoat.workspace_picker.is_some(), "the picker opened");
    assert!(
        h.stoat.active_workspace().editors.len() > before,
        "which took an editor for its input"
    );

    h.stoat.handle_key(ctrl_c());

    assert!(h.stoat.workspace_picker.is_none(), "the picker closed");
    assert_eq!(
        h.stoat.active_workspace().editors.len(),
        before,
        "and gave its editor back"
    );
}

/// The transient text inputs carry no Ctrl-C binding of their own, so an
/// input missing from the cascade quits the session out from under an edit
/// in progress.
#[test]
fn ctrl_c_cancels_a_rename_rather_than_quitting() {
    use lsp_types::{Position, Uri};
    use std::str::FromStr;

    let mut h = Stoat::test();
    let before = h.stoat.active_workspace().editors.len();
    let executor = h.stoat.executor.clone();
    let input = InputView::create(
        h.stoat.active_workspace_mut(),
        executor,
        SubmitTarget::RenameSymbol,
        "old_name",
        "insert",
        1,
    );
    let buffer_id = input.buffer_id;
    h.stoat.rename_input = Some(RenameInputState {
        input,
        source_uri: Uri::from_str("file:///src/lib.rs").expect("valid uri"),
        symbol_position: Position::new(0, 0),
        anchor_offset: 0,
        server: None,
        buffer_id,
    });

    let effect = h.stoat.handle_key(ctrl_c());

    assert!(
        !matches!(effect, UpdateEffect::Quit),
        "cancelling a rename must not take the session with it"
    );
    assert!(h.stoat.rename_input.is_none(), "the rename is cancelled");
    assert_eq!(
        h.stoat.active_workspace().editors.len(),
        before,
        "and gave its editor back"
    );
}

#[test]
fn ctrl_c_cancels_the_search_input_rather_than_quitting() {
    let mut h = Stoat::test();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSearchInput);
    assert!(h.stoat.search_input.is_some(), "the input opened");

    let effect = h.stoat.handle_key(ctrl_c());

    assert!(
        !matches!(effect, UpdateEffect::Quit),
        "cancelling a search must not take the session with it"
    );
    assert!(h.stoat.search_input.is_none(), "the input is cancelled");
}

/// Quitting on an unbound Ctrl-C is the behavior the cascade arms carve out
/// of, so it has to survive them.
///
/// A pane holding no view binds nothing, which is what leaves the fallback
/// reachable. Editor and run panes both bind the key, so neither reaches
/// here.
#[test]
fn ctrl_c_with_no_modal_open_still_quits() {
    let mut h = Stoat::test();
    {
        let ws = h.stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        ws.panes.pane_mut(focused).view = View::Label("nothing".into());
    }

    assert!(matches!(h.stoat.handle_key(ctrl_c()), UpdateEffect::Quit));
}

fn resolves(h: &crate::test_harness::TestHarness, key: &KeyEvent) -> bool {
    let state = StoatKeymapState::from_stoat(&h.stoat);
    h.stoat.keymap.lookup(&state, key).is_some()
}

fn space() -> KeyEvent {
    KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty())
}

/// A normal-mode modal binds only the keys it handles, so the editor's own
/// normal-mode block has to stop applying while one is open. Otherwise Space
/// opens a second modal over the picker and Ctrl-d scrolls the editor hidden
/// behind it, leaving render painting one modal while keys route to another.
#[test]
fn normal_mode_bindings_stop_at_the_location_picker() {
    use crate::location_picker::LocationEntry;

    let mut h = Stoat::test();
    let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert!(resolves(&h, &space()), "Space starts a chord in the editor");
    assert!(resolves(&h, &ctrl_d), "and Ctrl-d scrolls it");

    h.stoat.location_picker = Some(action_handlers::lsp::open_location_picker(
        &mut h.stoat,
        vec![LocationEntry {
            path: PathBuf::from("/repo/a.rs"),
            offset: 0,
            line: 1,
            column: 1,
            text: "candidate".to_owned(),
        }],
    ));

    assert!(
        !resolves(&h, &space()),
        "Space starts no chord over the picker"
    );

    // The picker prompts in insert mode and binds Ctrl-d itself, so the key
    // reaches its preview rather than the editor's half-page scroll.
    let bound = {
        let state = StoatKeymapState::from_stoat(&h.stoat);
        h.stoat
            .keymap
            .lookup(&state, &ctrl_d)
            .map(|actions| actions[0].name.clone())
    };
    assert_eq!(
        bound.as_deref(),
        Some("PickerDetailDown"),
        "Ctrl-d scrolls the picker's preview, not the editor behind it"
    );
}

#[test]
fn space_chord_never_starts_over_the_quit_confirm() {
    let mut h = Stoat::test();
    h.stoat.quit_all_confirm = Some(QuitAllConfirm::new(&[], Path::new("/")));

    assert!(
        !resolves(&h, &space()),
        "without a chord start, `space a s` cannot split behind the prompt"
    );
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "the editor is still in normal mode, so only the guard suppressed it"
    );
}

/// The guard must narrow when the block applies without changing how it
/// ranks, or it ties the equally-specific view blocks and beats them on
/// source order.
#[test]
fn a_view_block_still_outranks_the_guarded_normal_block() {
    let mut h = Stoat::test();
    h.seed_linear_history("/repo", &[("c1", "first", &[("a.rs", "fn a() {}\n")])]);
    h.open_commits("/repo");

    let state = StoatKeymapState::from_stoat(&h.stoat);
    let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty());
    let actions = h
        .stoat
        .keymap
        .lookup(&state, &j)
        .expect("j is bound on the commits screen");
    assert!(
        actions.iter().any(|a| a.name == "CommitsNext"),
        "the commits screen keeps j, got {actions:?}"
    );
}

#[test]
fn run_pane_ctrl_c_interrupts_instead_of_quitting() {
    let mut h = Stoat::test();
    h.open_run();

    let state = StoatKeymapState::from_stoat(&h.stoat);
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let actions = h
        .stoat
        .keymap
        .lookup(&state, &ctrl_c)
        .expect("Ctrl-C is bound in a run pane");
    assert!(
        actions.iter().any(|a| a.name == "RunInterrupt"),
        "run-pane Ctrl-C resolves to RunInterrupt, got {actions:?}"
    );

    let effect = h.stoat.handle_key(ctrl_c);
    assert!(
        !matches!(effect, UpdateEffect::Quit),
        "a bound Ctrl-C routes to the keymap rather than quitting"
    );
}

/// A run pane in normal mode keeps its interrupt.
///
/// Bindings resolve by predicate-atom count, and a run pane in normal mode
/// satisfies `mode == normal` as readily as an editor does. An editor-only
/// Ctrl-c is what keeps the two apart, since a `mode == normal` one
/// outranks `pane == run` and swallows the interrupt. The sibling test
/// above leaves the run pane in insert mode, where the question never
/// arises.
#[test]
fn a_run_pane_in_normal_mode_still_interrupts_on_ctrl_c() {
    let mut h = Stoat::test();
    h.open_run();
    h.stoat
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(h.stoat.focused_mode(), "normal", "the run pane left insert");

    let state = StoatKeymapState::from_stoat(&h.stoat);
    let actions = h
        .stoat
        .keymap
        .lookup(
            &state,
            &KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        )
        .expect("Ctrl-C is bound in a run pane");
    assert!(
        actions.iter().any(|a| a.name == "RunInterrupt"),
        "run-pane Ctrl-C stays the interrupt in normal mode, got {actions:?}"
    );
}

/// Ctrl-C in an editor comments the selection. Unbound it quit the editor,
/// which is the worst answer available for a key a user reaches for while
/// editing.
#[test]
fn ctrl_c_in_an_editor_does_not_quit() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "let x = 1;\n");

    let effect = h
        .stoat
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(
        !matches!(effect, UpdateEffect::Quit),
        "Ctrl-C is bound in an editor now, so it never reaches the quit fallback",
    );
}

#[test]
fn finished_modal_run_escape_dismisses_via_keymap() {
    let mut h = Stoat::test();
    let executor = h.stoat.executor.clone();
    let run_id = {
        let ws = h.stoat.active_workspace_mut();
        let run = crate::run::RunState::new(std::path::PathBuf::from("/tmp"), ws, executor);
        ws.runs.insert(run)
    };
    h.stoat.modal_run = Some(run_id);

    // A fresh run has no in-flight block, so it reads as finished.
    h.stoat
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

    assert!(
        h.stoat.modal_run.is_none(),
        "Escape on a finished modal run dismisses it"
    );
    assert!(
        h.stoat.active_workspace().runs.get(run_id).is_none(),
        "the dismissed run is removed from the registry"
    );
}

#[test]
fn run_enter_submits_from_insert_and_normal() {
    let mut h = Stoat::test();
    let fake = h.fake_terminal().clone();
    h.open_run();

    h.type_text("ls");
    h.type_keys("enter");
    assert!(
        fake.sent_strings().iter().any(|s| s.starts_with("ls\n")),
        "insert-mode Enter submits, sent {:?}",
        fake.sent_strings(),
    );

    h.type_text("pwd");
    h.type_keys("esc");
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "Escape leaves insert mode"
    );
    h.type_keys("enter");
    assert!(
        fake.sent_strings().iter().any(|s| s.starts_with("pwd\n")),
        "normal-mode Enter submits, sent {:?}",
        fake.sent_strings(),
    );
}

#[test]
fn run_up_recalls_history() {
    let mut h = Stoat::test();
    let run_id = h.open_run();

    h.type_text("ls");
    h.type_keys("enter");
    h.type_keys("up");

    let ws = h.stoat.active_workspace();
    let run_state = ws.runs.get(run_id).expect("run state exists");
    assert_eq!(
        run_state.input.text(ws),
        "ls",
        "Up recalls the last command"
    );
}

#[test]
fn run_wheel_scrolls_output_and_clamps() {
    let mut h = Stoat::test();
    // 15 output rows in a 10-row pane (9 visible): output_line_total is 16
    // (prompt + 15 rows), so the top is reachable at offset 16 - 9 = 7.
    let output: Vec<u8> = (0..15)
        .flat_map(|i| format!("line{i}\n").into_bytes())
        .collect();
    let run_id = open_run_with_output(&mut h, &output);
    // Pin a short pane (the captures inside the helper re-layout it to the
    // full terminal) so the 16 output rows overflow the 9 visible rows.
    let pane_id = h.stoat.active_workspace().panes.focus();
    h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(0, 0, 40, 10);
    let offset = |h: &crate::test_harness::TestHarness| {
        h.stoat
            .active_workspace()
            .runs
            .get(run_id)
            .unwrap()
            .scroll_offset
    };

    h.stoat.update(mouse_event(MouseEventKind::ScrollUp, 1, 1));
    h.stoat.update(mouse_event(MouseEventKind::ScrollUp, 1, 1));
    h.stoat.update(mouse_event(MouseEventKind::ScrollUp, 1, 1));
    assert_eq!(offset(&h), 7, "scroll up steps by 3 and clamps at the top");

    h.stoat
        .update(mouse_event(MouseEventKind::ScrollDown, 1, 1));
    h.stoat
        .update(mouse_event(MouseEventKind::ScrollDown, 1, 1));
    assert_eq!(offset(&h), 1, "scroll down steps by 3");

    h.stoat
        .update(mouse_event(MouseEventKind::ScrollDown, 1, 1));
    assert_eq!(offset(&h), 0, "scroll down floors at the tail");
}

#[test]
fn run_submit_resets_scroll_offset() {
    let mut h = Stoat::test();
    let output: Vec<u8> = (0..15)
        .flat_map(|i| format!("line{i}\n").into_bytes())
        .collect();
    let run_id = open_run_with_output(&mut h, &output);
    let pane_id = h.stoat.active_workspace().panes.focus();
    h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(0, 0, 40, 10);

    h.stoat.update(mouse_event(MouseEventKind::ScrollUp, 1, 1));
    assert!(
        h.stoat
            .active_workspace()
            .runs
            .get(run_id)
            .unwrap()
            .scroll_offset
            > 0,
        "precondition: scrolled up off the tail",
    );

    let input = h
        .stoat
        .active_workspace()
        .runs
        .get(run_id)
        .unwrap()
        .input
        .clone();
    input.replace_text(h.stoat.active_workspace_mut(), "pwd");
    action_handlers::dispatch(&mut h.stoat, &stoat_action::RunSubmit);

    assert_eq!(
        h.stoat
            .active_workspace()
            .runs
            .get(run_id)
            .unwrap()
            .scroll_offset,
        0,
        "submitting snaps the output back to the prompt",
    );
}

/// Three 4,000-line commands pass the 10,512-row cap on the third output.
/// Output that fills the pane to the cap exactly drops nothing, and the
/// next submit passes the cap by the new block's own row.
#[test]
fn a_run_pane_keeps_one_scrollback_across_its_blocks() {
    let mut h = Stoat::test();
    let run_id = h.open_run();
    let lines = |count: usize| "x\r\n".repeat(count).into_bytes();
    let commands = |stoat: &Stoat| -> Vec<String> {
        stoat.active_workspace().runs[run_id]
            .blocks
            .iter()
            .map(|block| block.command.clone())
            .collect()
    };

    for command in ["a", "b", "c"] {
        h.type_text(command);
        h.type_keys("enter");
        h.inject_run_output(run_id, &lines(4_000));
    }
    assert_eq!(
        commands(&h.stoat),
        ["b", "c"],
        "the third output passes the cap"
    );

    h.inject_run_output(run_id, &lines(2_510));
    assert_eq!(
        commands(&h.stoat),
        ["b", "c"],
        "output up to the cap exactly drops nothing"
    );

    h.type_text("d");
    h.type_keys("enter");
    assert_eq!(
        commands(&h.stoat),
        ["c", "d"],
        "the new block's row passes the cap"
    );
}

fn open_run_with_output(h: &mut crate::test_harness::TestHarness, output: &[u8]) -> RunId {
    let run_id = h.open_run();
    let pane_id = h.stoat.active_workspace().panes.focus();
    h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(0, 0, 40, 10);
    h.submit_run("ls");
    h.inject_run_output(run_id, output);
    run_id
}

#[test]
fn opening_diff_view_jumps_cursor_to_the_first_hunk() {
    let mut h = Stoat::test();
    open_scratch_file(&mut h, "keep\nnew\ntail\n");

    let buffer_id = {
        let ws = h.stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => ws.editors[id].buffer_id,
            _ => panic!("focused pane is not an editor"),
        }
    };
    {
        let base = "keep\nold\ntail\n";
        let text = "keep\nnew\ntail\n";
        let dm = crate::diff_map::DiffMap::from_structural_changes(
            stoat_language::structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
        );
        h.stoat
            .active_workspace_mut()
            .install_test_diff_map(buffer_id, dm);
    }

    let cursor_row = |stoat: &mut Stoat| {
        let (buffer_id, offset) = stoat.focused_cursor_pos().expect("focused cursor");
        let ws = stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        guard.rope().offset_to_point(offset).row
    };

    assert_eq!(cursor_row(&mut h.stoat), 0, "cursor starts at the top");

    h.stoat.toggle_diff_view();
    assert_eq!(
        cursor_row(&mut h.stoat),
        1,
        "opening the diff view lands the cursor on the first hunk",
    );

    h.stoat.toggle_diff_view();
    assert_eq!(
        cursor_row(&mut h.stoat),
        1,
        "toggling the view off leaves the cursor in place",
    );
}

/// Opens a scratch buffer with a small HEAD-vs-buffer diff installed, so
/// `toggle_diff_view` finds a ready map and skips the on-demand compute.
fn open_scratch_with_diff(h: &mut crate::test_harness::TestHarness) {
    open_scratch_file(h, "keep\nnew\ntail\n");
    let buffer_id = {
        let ws = h.stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => ws.editors[id].buffer_id,
            _ => panic!("focused pane is not an editor"),
        }
    };
    let base = "keep\nold\ntail\n";
    let text = "keep\nnew\ntail\n";
    let dm = crate::diff_map::DiffMap::from_structural_changes(
        stoat_language::structural_diff::diff(base, text),
        Arc::new(base.to_string()),
        text,
    );
    h.stoat
        .active_workspace_mut()
        .install_test_diff_map(buffer_id, dm);
}

#[test]
fn opening_diff_view_widens_the_focused_pane() {
    let mut h = Stoat::test();
    open_scratch_with_diff(&mut h);
    h.type_keys("space a s");

    let (focused, other, focused_area, other_area) = {
        let panes = &h.stoat.active_workspace().panes;
        let focused = panes.focus();
        let other = panes
            .split_pane_ids()
            .into_iter()
            .find(|&id| id != focused)
            .expect("a second pane");
        (
            focused,
            other,
            panes.pane(focused).area,
            panes.pane(other).area,
        )
    };

    h.stoat.toggle_diff_view();
    {
        let panes = &h.stoat.active_workspace().panes;
        assert_eq!(
            panes.widened(),
            Some(focused),
            "opening the diff widens the focused pane"
        );
        assert!(
            panes.pane(focused).area.width > focused_area.width,
            "the widened pane grows past its split width"
        );
    }

    h.stoat.toggle_diff_view();
    {
        let panes = &h.stoat.active_workspace().panes;
        assert_eq!(panes.widened(), None, "closing the diff unwidens");
        assert_eq!(
            panes.pane(focused).area,
            focused_area,
            "the focused pane is restored"
        );
        assert_eq!(
            panes.pane(other).area,
            other_area,
            "the other pane is restored"
        );
    }
}

#[test]
fn opening_diff_view_leaves_an_unwidenable_layout_put() {
    let mut h = Stoat::test();
    open_scratch_with_diff(&mut h);
    h.type_keys("space a s");
    h.type_keys("space a v");
    h.type_keys("space a k");

    let focused = h.stoat.active_workspace().panes.focus();
    h.stoat.toggle_diff_view();

    let ws = h.stoat.active_workspace();
    assert_eq!(
        ws.panes.widened(),
        None,
        "a layout with no clean cover is left unwidened"
    );
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    assert!(
        ws.editors[editor_id].diff_view,
        "the diff still opens even when the pane cannot widen"
    );
}

#[test]
fn focusing_away_from_an_open_diff_unwidens_and_keeps_the_diff() {
    let mut h = Stoat::test();
    open_scratch_with_diff(&mut h);
    h.type_keys("space a s");

    let (focused, other, editor_id) = {
        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let other = ws
            .panes
            .split_pane_ids()
            .into_iter()
            .find(|&id| id != focused)
            .expect("a second pane");
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        (focused, other, editor_id)
    };

    h.stoat.toggle_diff_view();
    assert_eq!(h.stoat.active_workspace().panes.widened(), Some(focused));

    h.stoat.active_workspace_mut().panes.set_focus(other);

    let ws = h.stoat.active_workspace();
    assert_eq!(
        ws.panes.widened(),
        None,
        "focusing another pane restores the layout"
    );
    assert!(
        ws.editors[editor_id].diff_view,
        "the diff view stays open on the original editor"
    );
}

#[test]
fn opening_diff_view_scrolls_a_far_first_hunk_into_the_viewport() {
    let mut h = Stoat::test();
    let base: String = (0..30).map(|i| format!("line {i:02}\n")).collect();
    let text: String = (0..30)
        .map(|i| {
            if i == 20 {
                "changed\n".to_string()
            } else {
                format!("line {i:02}\n")
            }
        })
        .collect();
    open_scratch_file(&mut h, &text);

    let buffer_id = {
        let ws = h.stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => ws.editors[id].buffer_id,
            _ => panic!("focused pane is not an editor"),
        }
    };
    {
        let dm = crate::diff_map::DiffMap::from_structural_changes(
            stoat_language::structural_diff::diff(&base, &text),
            Arc::new(base.to_string()),
            &text,
        );
        h.stoat
            .active_workspace_mut()
            .install_test_diff_map(buffer_id, dm);
    }

    // A ten-row viewport with the first hunk (buffer row 20) far below it.
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        editor.viewport_rows = Some(10);
        editor.scroll_row = 0;
    }

    h.stoat.toggle_diff_view();

    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    let scroll_row = editor.scroll_row;
    let cursor_row = action_handlers::view::cursor_display_row(editor);
    assert!(
        scroll_row > 0,
        "opening the diff view scrolled away from the top"
    );
    assert!(
        (scroll_row..scroll_row + 10).contains(&cursor_row),
        "the first hunk's display row {cursor_row} sits inside the viewport [{scroll_row}, {})",
        scroll_row + 10,
    );
}

#[test]
fn stoat_review_opens_the_first_changed_file_on_its_first_hunk() {
    let mut h = Stoat::test();
    let workdir = PathBuf::from("/repo");
    h.stage_review_scenario(&workdir, &[("changed.rs", "a\nb\nc\n", "a\nX\nc\n")]);
    h.stoat.set_diff_warm_auto(true);

    // Mirrors the `stoat review` startup, where the diff view opens on the
    // pathless scratch and then crosses into the sole changed file.
    h.stoat.open_working_tree_diff();
    h.settle();

    let (_, buffer_id) = h.stoat.focused_editor_ids().expect("focused editor");
    let path = {
        let ws = h.stoat.active_workspace();
        ws.buffers.path_for(buffer_id).map(|p| p.to_path_buf())
    };
    assert_eq!(
        path,
        Some(workdir.join("changed.rs")),
        "opened the first changed file",
    );
    assert!(
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .diff_view,
        "the diff view is on for the opened file",
    );

    let (cursor_buffer, offset) = h.stoat.focused_cursor_pos().expect("focused cursor");
    let cursor_row = {
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(cursor_buffer).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        guard.rope().offset_to_point(offset).row
    };
    assert_eq!(cursor_row, 1, "the cursor sits on the file's first hunk");
}

#[test]
fn diff_from_an_unchanged_file_crosses_into_the_first_changed_file() {
    let mut h = Stoat::test();
    let workdir = PathBuf::from("/repo");
    h.stage_review_scenario(&workdir, &[("changed.rs", "a\nb\nc\n", "a\nX\nc\n")]);
    // plain.rs is tracked but unchanged. Its HEAD content and working-tree
    // copy are identical, so it never appears in the changed list.
    h.fake_git()
        .add_repo(workdir.clone())
        .with_fs(h.fake_fs())
        .head_file("plain.rs", "one\ntwo\nthree\n");
    h.fake_fs()
        .insert_file(workdir.join("plain.rs"), b"one\ntwo\nthree\n");
    h.stoat.set_diff_warm_auto(true);

    action_handlers::dispatch(
        &mut h.stoat,
        &OpenFile {
            path: workdir.join("plain.rs"),
        },
    );
    h.settle();

    h.stoat.toggle_diff_view();
    h.settle();

    let (_, buffer_id) = h.stoat.focused_editor_ids().expect("focused editor");
    let path = {
        let ws = h.stoat.active_workspace();
        ws.buffers.path_for(buffer_id).map(|p| p.to_path_buf())
    };
    assert_eq!(
        path,
        Some(workdir.join("changed.rs")),
        "diff from an unchanged file crosses into the first changed file",
    );
    assert!(
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .diff_view,
        "the diff view is on for the crossed-into file",
    );

    let (cursor_buffer, offset) = h.stoat.focused_cursor_pos().expect("focused cursor");
    let cursor_row = {
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(cursor_buffer).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        guard.rope().offset_to_point(offset).row
    };
    assert_eq!(
        cursor_row, 1,
        "the cursor lands on the changed file's first hunk"
    );
}

#[test]
fn stoat_review_with_no_changes_stays_on_the_scratch() {
    let mut h = Stoat::test();
    let workdir = PathBuf::from("/repo");
    h.stage_review_scenario(&workdir, &[]);
    h.stoat.set_diff_warm_auto(true);

    let scratch = h.stoat.focused_editor_ids().expect("editor").1;

    h.stoat.open_working_tree_diff();
    h.settle();

    assert_eq!(
        h.stoat.focused_editor_ids().expect("editor").1,
        scratch,
        "focus stays on the startup scratch when nothing changed",
    );
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("no more changes"),
        "the status reports that there are no changes",
    );
}

#[test]
fn a_git_write_stales_open_diffs_with_no_precompute() {
    let mut h = Stoat::test();
    h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    h.settle_diff_jobs();

    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    assert!(
        h.stoat.active_workspace().diff_map_current(buffer_id),
        "the settled job leaves the buffer's diff current",
    );

    // The gate that used to arm the diff-refresh debounce does not apply
    // here, because precompute is off.
    h.stoat.set_diff_warm_auto(false);

    h.fake_fs_watcher()
        .inject(Path::new("/repo/.git/HEAD"), FsEventKind::Modified);
    debounce::drain_fs_watch_events(&mut h.stoat);
    h.advance_clock(FS_WATCH_DEBOUNCE);

    assert!(
        !h.stoat.active_workspace().diff_map_current(buffer_id),
        "a .git write stales the open buffer's diff map even with no precompute",
    );
}

#[test]
fn minimap_click_scrolls_to_the_proportional_line() {
    let mut h = Stoat::test();
    let editor_id = open_with_minimap_strip(&mut h);

    // Strip cell row 5 of a fits-file (60 <= 10*8) points at line 5*8+4 = 44,
    // centered in the 20-row viewport -> scroll 34.
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 74, 5));

    let editor = &h.stoat.active_workspace().editors[editor_id];
    assert_eq!(
        editor.scroll_row, 34,
        "the click eases to the centered proportional row"
    );
    assert_eq!(
        editor.scroll_glide,
        ScrollGlide::Page,
        "the scrub glides like a page motion"
    );
    assert_eq!(
        h.stoat.minimap_drag,
        Some(editor_id),
        "the press arms the scrub"
    );
}

/// The strip draws one row per buffer line, so a click resolves to a buffer
/// line and must be converted before it drives the display-row scroll. With
/// every line wrapped in two, an unconverted target lands halfway up the
/// file from the block the pointer was over.
#[test]
fn minimap_click_targets_the_clicked_buffer_line_when_wrapped() {
    let mut h = Stoat::test();
    h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
    let body: String = (0..60)
        .map(|_| "w".repeat(100))
        .collect::<Vec<_>>()
        .join("\n");
    open_scratch_file(&mut h, &body);
    let editor_id = h.stoat.focused_editor_ids().expect("editor").0;
    let display_rows = {
        let editor = &mut h.stoat.active_workspace_mut().editors[editor_id];
        editor.minimap_rect = Some(Rect::new(72, 0, 8, 10));
        editor.viewport_rows = Some(20);
        editor.display_map.set_wrap_width(Some(60));
        editor.display_map.snapshot().line_count()
    };
    assert_eq!(display_rows, 120, "every line must wrap into two rows");

    // The 60-line file fits the strip's 10 * LINES_PER_CELL rows, so cell
    // row 5 points at buffer line 5*8+4.
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 74, 5));

    let editor = &mut h.stoat.active_workspace_mut().editors[editor_id];
    let scroll_row = editor.scroll_row;
    let centered = editor
        .display_map
        .snapshot()
        .display_to_buffer(DisplayPoint::new(scroll_row + 10, 0))
        .expect("a text row")
        .row;
    assert_eq!(
        (scroll_row, centered),
        (78, 44),
        "the click centers the buffer line under the pointer, not the display row"
    );
}

#[test]
fn minimap_leaves_text_clicks_to_the_cursor() {
    let mut h = Stoat::test();
    h.stoat
        .active_workspace_mut()
        .panes
        .resize(Rect::new(0, 0, 80, 24));
    let editor_id = open_with_minimap_strip(&mut h);

    // A press in the text area, left of the strip, never arms the scrub.
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 4));

    assert_eq!(
        h.stoat.minimap_drag, None,
        "a text press does not arm the scrub"
    );
    assert_eq!(
        h.stoat.active_workspace().editors[editor_id].scroll_row,
        0,
        "a text press does not scroll the pane"
    );
    assert!(
        h.stoat
            .newest_cursor_offset(editor_id)
            .is_some_and(|o| o > 0),
        "the text press still moves the cursor off the buffer start"
    );
}

#[test]
fn minimap_drag_scrolls_monotonically() {
    let mut h = Stoat::test();
    let editor_id = open_with_minimap_strip(&mut h);

    let mut rows = Vec::new();
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 74, 1));
    rows.push(h.stoat.active_workspace().editors[editor_id].scroll_row);
    for row in [3u16, 5, 7, 9] {
        h.stoat.update(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            74,
            row,
        ));
        rows.push(h.stoat.active_workspace().editors[editor_id].scroll_row);
    }
    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 74, 9));

    assert!(
        rows.windows(2).all(|w| w[1] >= w[0]),
        "dragging down the strip scrolls monotonically down: {rows:?}"
    );
    assert!(rows[4] > rows[0], "the drag moved the viewport: {rows:?}");
    assert_eq!(h.stoat.minimap_drag, None, "releasing clears the scrub");
}

#[test]
fn single_band_click_scrubs_the_focused_editor_not_the_pane_under_it() {
    use stoat_config::MinimapMode;

    let mut h = Stoat::test();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
    h.resize(200, 24);

    let long: String = (0..200).map(|i| format!("line {i}\n")).collect();
    let a = h.write_file("a.txt", &long);
    let b = h.write_file("b.txt", "short\n");
    h.open_file(&a);
    h.type_action("SplitRight()");
    h.open_file(&b);
    // Focus the left pane (the long buffer) while the band overlays the right.
    h.type_action("FocusLeft()");
    h.settle();
    let _ = h.stoat.render();

    let focused = h.stoat.focused_editor_ids().expect("focused editor").0;
    let band = h
        .stoat
        .single_minimap_rect
        .expect("single mode reserves a band");
    let col = band.x + band.width / 2;
    let row = band.y + band.height / 2;

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        col,
        row,
    ));
    assert_eq!(
        h.stoat.minimap_drag,
        Some(focused),
        "the band press arms a scrub of the focused editor"
    );
    let after_click = h.stoat.active_workspace().editors[focused].scroll_row;
    assert!(
        after_click > 0,
        "the band click scrolls the focused editor toward the clicked line"
    );

    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        col,
        row + 2,
    ));
    let after_drag = h.stoat.active_workspace().editors[focused].scroll_row;
    assert!(
        after_drag >= after_click,
        "dragging down the band re-scrubs monotonically: {after_click} -> {after_drag}"
    );
    h.stoat.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        col,
        row + 2,
    ));
    assert_eq!(h.stoat.minimap_drag, None, "releasing clears the scrub");

    // In per-pane mode the same right-edge coordinates belong to the right
    // pane, so they never scrub the focused-left editor.
    h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
    {
        let editor = h
            .stoat
            .active_workspace_mut()
            .editors
            .get_mut(focused)
            .expect("focused editor");
        editor.scroll_row = 0;
        editor.scroll_offset = 0.0;
    }
    let _ = h.stoat.render();
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        col,
        row,
    ));
    assert_eq!(
        h.stoat.active_workspace().editors[focused].scroll_row,
        0,
        "in per-pane mode the right-edge click leaves the focused-left editor"
    );
}

#[test]
fn divider_drag_resizes_the_split() {
    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    let left = ws.panes.focus();
    let right = ws.panes.split(crate::pane::Axis::Vertical);
    ws.panes.resize(Rect::new(0, 0, 101, 24));

    let la = h.stoat.active_workspace().panes.pane(left).area;
    let divider_col = la.x + la.width;
    let left_w0 = la.width;
    let focus0 = h.stoat.active_workspace().panes.focus();

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        divider_col,
        5,
    ));
    assert!(
        h.stoat.divider_drag.is_some(),
        "clicking a divider arms a drag"
    );
    assert_eq!(
        h.stoat.active_workspace().panes.focus(),
        focus0,
        "a divider click does not move focus"
    );

    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        divider_col + 2,
        5,
    ));
    assert_eq!(
        h.stoat.active_workspace().panes.pane(left).area.width,
        left_w0 + 2,
        "dragging the divider right widens the left pane"
    );
    assert_eq!(
        h.stoat.active_workspace().panes.pane(right).area.width,
        98 - left_w0,
        "the right pane shrinks by the same delta"
    );

    h.stoat.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        divider_col + 2,
        5,
    ));
    assert!(h.stoat.divider_drag.is_none(), "releasing clears the drag");
}

/// A finder-family modal wide enough for two panes, with the list and preview
/// rects the renderer would paint.
fn side_by_side_layout(h: &crate::test_harness::TestHarness, kind: ModalKind) -> (Rect, Rect) {
    let (open_kind, _) =
        mouse::open_modal_separator(&h.stoat).expect("the modal shows a preview beside its list");
    assert_eq!(open_kind, kind, "the expected modal is the open one");
    let content = match kind {
        ModalKind::SymbolFinder => {
            let finder = h.stoat.symbol_finder.as_ref().expect("open");
            let (_, _, list, preview) = crate::render::symbol_finder::symbol_finder_layout(
                h.stoat.size(),
                finder.content_rows,
                modal_zoom_steps(&h.stoat.modal_zoom, kind),
                modal_split_percent(&h.stoat.modal_split, kind),
            )
            .expect("fits");
            (list, preview)
        },
        _ => {
            let size = h.stoat.size();
            let declared = match kind {
                ModalKind::FileFinder => h.stoat.file_finder.as_ref().expect("open").content_size,
                _ => (u16::MAX, u16::MAX),
            };
            let layout = crate::render::file_finder::file_finder_layout(
                size,
                declared,
                modal_zoom_steps(&h.stoat.modal_zoom, kind),
                modal_split_percent(&h.stoat.modal_split, kind),
            )
            .expect("fits");
            (layout.list, layout.preview)
        },
    };
    (content.0, content.1.expect("the preview pane is present"))
}

/// A harness with the file finder open over a terminal wide enough that the
/// modal splits into a list and a preview.
fn wide_finder_harness() -> crate::test_harness::TestHarness {
    let mut h = crate::test_harness::TestHarness::with_size(140, 40);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
    h.settle();
    h
}

/// Dragging the column between a finder's list and its preview is how the
/// user gives one of them more room, so the drag has to land the line where
/// the pointer is.
#[test]
fn dragging_the_finder_vline_widens_the_list() {
    let mut h = wide_finder_harness();
    let (list, preview) = side_by_side_layout(&h, ModalKind::FileFinder);
    let vline = list.x + list.width;

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        vline,
        list.y + 1,
    ));
    assert_eq!(
        h.stoat.modal_separator_drag,
        Some(ModalKind::FileFinder),
        "a press on the vline arms the drag"
    );

    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        vline + 6,
        list.y + 1,
    ));
    let (widened, narrowed) = side_by_side_layout(&h, ModalKind::FileFinder);
    assert_eq!(
        widened.width,
        list.width + 6,
        "the list grows to exactly where the pointer left the line"
    );
    assert_eq!(
        narrowed.width,
        preview.width - 6,
        "and the preview gives up the same columns"
    );

    h.stoat.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        vline + 6,
        list.y + 1,
    ));
    assert_eq!(
        h.stoat.modal_separator_drag, None,
        "releasing clears the arm"
    );
}

/// The commits screen paints into a pane rather than a boxed modal, so its
/// separator is armed after the divider arm instead of inside the modal
/// pointer path. The share it writes holds for the session.
#[test]
fn dragging_the_commits_vline_widens_the_list() {
    use crate::render::commits::{commits_list_rect, MIN_LIST_COLUMNS};

    let mut h = crate::test_harness::TestHarness::with_size(140, 40);
    h.seed_linear_history("/repo", &[("c1", "first", &[("a.rs", "fn a() {}\n")])]);
    h.open_commits("/repo");

    let pane_area = {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).area
    };
    let list = commits_list_rect(pane_area, None).expect("the list fits this terminal");
    let vline = list.x + list.width;
    let row = list.y + 1;

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        vline,
        row,
    ));
    assert!(
        h.stoat.commits_separator_drag,
        "a press on the vline arms the drag"
    );

    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        vline + 6,
        row,
    ));
    assert_eq!(
        commits_list_rect(pane_area, h.stoat.commits_split)
            .expect("the list still fits")
            .width,
        list.width + 6,
        "the list grows to exactly where the pointer left the line"
    );

    h.stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 0, row));
    assert_eq!(
        commits_list_rect(pane_area, h.stoat.commits_split)
            .expect("the list still fits")
            .width,
        MIN_LIST_COLUMNS,
        "a drag past the left edge floors at the list minimum"
    );

    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 0, row));
    assert!(!h.stoat.commits_separator_drag, "releasing clears the arm");
}

#[test]
fn a_vline_drag_clamps_at_both_edges() {
    use crate::render::picker::MIN_PANE_COLUMNS;

    let mut h = wide_finder_harness();
    let (list, preview) = side_by_side_layout(&h, ModalKind::FileFinder);
    let row = list.y + 1;
    let vline = list.x + list.width;
    // The two panes plus the one-column line are the whole body whatever the
    // share, so this is what a clamped split still has to add up to.
    let body = list.width + preview.width + 1;

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        vline,
        row,
    ));

    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        139,
        row,
    ));
    let (widest, thinnest) = side_by_side_layout(&h, ModalKind::FileFinder);
    assert_eq!(
        thinnest.width, MIN_PANE_COLUMNS,
        "dragged to the screen edge the preview keeps its floor"
    );
    assert_eq!(
        widest.width + thinnest.width + 1,
        body,
        "and the list takes the rest rather than overflowing the body"
    );

    h.stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 0, row));
    let (narrowest, _) = side_by_side_layout(&h, ModalKind::FileFinder);
    assert_eq!(
        narrowest.width, MIN_PANE_COLUMNS,
        "and dragged the other way the list keeps its own floor"
    );
}

/// Each kind stores its own share, so dragging one finder's line must not
/// move another's.
#[test]
fn each_finder_kind_drags_its_own_separator() {
    let mut h = crate::test_harness::TestHarness::with_size(140, 40);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
    h.settle();

    let (list, _) = side_by_side_layout(&h, ModalKind::CodeSearch);
    let row = list.y + 1;
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        list.x + list.width,
        row,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        list.x + list.width + 5,
        row,
    ));

    assert_eq!(
        side_by_side_layout(&h, ModalKind::CodeSearch).0.width,
        list.width + 5,
        "code search's own line moves"
    );
    assert_eq!(
        h.stoat.modal_split.get(&ModalKind::FileFinder),
        None,
        "and the file finder's share is untouched"
    );
}

#[test]
fn the_symbol_finder_vline_drags_too() {
    let mut h = crate::test_harness::TestHarness::with_size(140, 40);
    h.stoat.symbol_finder = {
        let executor = h.stoat.executor.clone();
        let mut finder = SymbolFinder::new(
            h.stoat.active_workspace_mut(),
            executor,
            BufferId::new(0),
            crate::symbol_finder::SymbolFinderScope::Document,
            Vec::new(),
        );
        finder.content_rows = 12;
        Some(finder)
    };

    let (list, _) = side_by_side_layout(&h, ModalKind::SymbolFinder);
    let row = list.y + 1;
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        list.x + list.width,
        row,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        list.x + list.width + 4,
        row,
    ));

    assert_eq!(
        side_by_side_layout(&h, ModalKind::SymbolFinder).0.width,
        list.width + 4,
        "the symbol list widens with its line"
    );
}

/// Code search covers a pane like every other modal, so a press over it must
/// not reach the editor beneath.
#[test]
fn a_press_over_code_search_never_reaches_the_buffer() {
    let mut h = crate::test_harness::TestHarness::with_size(140, 40);
    h.seed_focused_buffer(&"line\n".repeat(200));
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
    h.settle();
    let head = h.primary_head_offset();
    let (list, _) = side_by_side_layout(&h, ModalKind::CodeSearch);

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        list.x + 4,
        list.y + 2,
    ));

    assert_eq!(
        h.primary_head_offset(),
        head,
        "the cursor in the covered editor stays where it was"
    );
}

#[test]
fn mouse_down_anchors_run_pane_selection() {
    let mut h = Stoat::test();
    let run_id = open_run_with_output(&mut h, b"hello\n");
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 1));
    let block = h
        .stoat
        .active_workspace()
        .runs
        .get(run_id)
        .expect("run state exists")
        .active_block()
        .expect("active block exists");
    assert_eq!(
        block.selection,
        Some(GridSelection {
            anchor: (2, 0),
            head: (2, 0),
        }),
    );
}

#[test]
fn mouse_drag_updates_run_pane_selection_head() {
    let mut h = Stoat::test();
    // Two real output rows so the row-1 drag target lands inside the grid
    // (the trailing blank row after the final newline is not rendered).
    let run_id = open_run_with_output(&mut h, b"hello\nworld\n");
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1));
    h.stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 4, 2));
    let block = h
        .stoat
        .active_workspace()
        .runs
        .get(run_id)
        .expect("run state exists")
        .active_block()
        .expect("active block exists");
    assert_eq!(
        block.selection,
        Some(GridSelection {
            anchor: (1, 0),
            head: (4, 1),
        }),
    );
}

#[test]
fn mouse_up_leaves_run_pane_selection_in_place() {
    let mut h = Stoat::test();
    let run_id = open_run_with_output(&mut h, b"hello\n");
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 1));
    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 3, 1));
    let block = h
        .stoat
        .active_workspace()
        .runs
        .get(run_id)
        .expect("run state exists")
        .active_block()
        .expect("active block exists");
    assert_eq!(
        block.selection,
        Some(GridSelection {
            anchor: (3, 0),
            head: (3, 0),
        }),
    );
}

#[test]
fn mouse_down_outside_active_block_does_not_select() {
    let mut h = Stoat::test();
    let run_id = open_run_with_output(&mut h, b"hello\n");
    for (col, row) in [(2u16, 0u16), (2, 3), (2, 9), (50, 1)] {
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            col,
            row,
        ));
        let block = h
            .stoat
            .active_workspace()
            .runs
            .get(run_id)
            .expect("run state exists")
            .active_block()
            .expect("active block exists");
        assert_eq!(
            block.selection, None,
            "click at ({col},{row}) should not anchor",
        );
    }
}

#[test]
fn mouse_drag_without_prior_down_is_noop() {
    let mut h = Stoat::test();
    let run_id = open_run_with_output(&mut h, b"hello\n");
    h.stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 2, 1));
    let block = h
        .stoat
        .active_workspace()
        .runs
        .get(run_id)
        .expect("run state exists")
        .active_block()
        .expect("active block exists");
    assert_eq!(block.selection, None);
}

#[test]
fn mouse_on_view_without_handler_is_noop() {
    let mut h = Stoat::test();
    let pane_id = h.stoat.active_workspace().panes.focus();
    let pane = h.stoat.active_workspace_mut().panes.pane_mut(pane_id);
    pane.view = View::Label("dummy".into());
    pane.area = Rect::new(0, 0, 40, 10);
    let effect = h
        .stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    assert_eq!(effect, UpdateEffect::None);
}

#[test]
fn mouse_up_after_drag_writes_selection_to_clipboard() {
    let mut h = Stoat::test();
    let _ = open_run_with_output(&mut h, b"hello\n");
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1));
    h.stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 3, 1));
    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 3, 1));
    assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
}

#[test]
fn mouse_up_without_drag_skips_clipboard() {
    let mut h = Stoat::test();
    let _ = open_run_with_output(&mut h, b"hello\n");
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 1));
    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 1));
    assert!(h.fake_clipboard().writes().is_empty());
}

#[test]
fn mouse_up_with_no_selection_skips_clipboard() {
    let mut h = Stoat::test();
    let _ = open_run_with_output(&mut h, b"hello\n");
    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 1));
    assert!(h.fake_clipboard().writes().is_empty());
}

#[test]
fn mouse_up_multi_row_drag_writes_joined_lines() {
    let mut h = Stoat::test();
    let _ = open_run_with_output(&mut h, b"foo\nbar\n");
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1));
    h.stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 1, 2));
    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 1, 2));
    assert_eq!(h.fake_clipboard().writes(), vec!["oo\nba"]);
}

fn drag_select_ell_in_hello(h: &mut crate::test_harness::TestHarness) {
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 1));
    h.stoat
        .update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 3, 1));
    h.stoat
        .update(mouse_event(MouseEventKind::Up(MouseButton::Left), 3, 1));
}

#[test]
fn osc52_emit_fires_in_ssh_without_mux() {
    let mut h = Stoat::test();
    h.fake_env().set("SSH_CONNECTION", "1.2.3.4 22 5.6.7.8 22");
    let _ = open_run_with_output(&mut h, b"hello\n");
    drag_select_ell_in_hello(&mut h);
    assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
    assert_eq!(h.fake_clipboard().osc52_emits(), vec!["ell"]);
}

#[test]
fn osc52_emit_skipped_inside_tmux() {
    let mut h = Stoat::test();
    h.fake_env().set("SSH_CONNECTION", "1.2.3.4 22 5.6.7.8 22");
    h.fake_env().set("TMUX", "/tmp/tmux-1000/default,1234,0");
    let _ = open_run_with_output(&mut h, b"hello\n");
    drag_select_ell_in_hello(&mut h);
    assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
    assert!(h.fake_clipboard().osc52_emits().is_empty());
}

#[test]
fn osc52_emit_skipped_inside_zellij() {
    let mut h = Stoat::test();
    h.fake_env().set("SSH_TTY", "/dev/pts/0");
    h.fake_env().set("ZELLIJ", "0");
    let _ = open_run_with_output(&mut h, b"hello\n");
    drag_select_ell_in_hello(&mut h);
    assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
    assert!(h.fake_clipboard().osc52_emits().is_empty());
}

#[test]
fn osc52_emit_skipped_locally() {
    let mut h = Stoat::test();
    let _ = open_run_with_output(&mut h, b"hello\n");
    drag_select_ell_in_hello(&mut h);
    assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
    assert!(h.fake_clipboard().osc52_emits().is_empty());
}

fn buffer_text(h: &crate::test_harness::TestHarness, path: &Path) -> String {
    let ws = h.stoat.active_workspace();
    let id = ws.buffers.id_for_path(path).expect("buffer registered");
    let buf = ws.buffers.get(id).expect("buffer present");
    let guard = buf.read().expect("buffer lock");
    guard.rope().to_string()
}

fn select_forward(h: &mut crate::test_harness::TestHarness, start: usize, end: usize) {
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    let (start, end) = {
        let snapshot = editor.display_map.snapshot();
        let buf = snapshot.buffer_snapshot();
        (
            buf.anchor_at(start, Bias::Right),
            buf.anchor_at(end, Bias::Right),
        )
    };
    editor
        .selections
        .set_single_range(start, end, false, SelectionGoal::None);
}

/// Add a second 1-wide block cursor at `offset` in the focused editor,
/// for building same-line multi-cursor states no keybinding produces.
fn insert_cursor_at(h: &mut crate::test_harness::TestHarness, offset: usize) {
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    let snapshot = editor.display_map.snapshot();
    let buf = snapshot.buffer_snapshot();
    let head = buf.anchor_at(offset, Bias::Right);
    editor
        .selections
        .insert_cursor(head, SelectionGoal::None, buf);
}

#[test]
fn driven_input_sequence_types_text_into_the_buffer() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");

    // The `--inputs` driver injects plain `Event::Key`s into the same
    // channel real keystrokes use, so feed the parsed sequence through
    // `update` directly rather than the double-firing keystroke helper.
    for step in input_parse::parse_input_sequence("ifoo<Esc>").expect("parse") {
        if let InputStep::Key(key) = step {
            h.stoat.update(Event::Key(key));
        }
    }

    assert_eq!(buffer_text(&h, &path), "foo");
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn shutdown_notify_quits_the_run_loop() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let mut h = Stoat::test();
        h.stoat.persistence_disabled = true;

        // Pre-firing the notify stores a permit, so the shutdown arm
        // fires on the loop's first poll. This mirrors a `--timeout`
        // timer that elapses before the loop starts.
        let shutdown = h.stoat.shutdown_handle();
        shutdown.notify_one();

        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let (render_tx, render_rx) = watch::channel(None);
        // Hold the event sender and render receiver so a closed channel
        // cannot end the loop. Only the shutdown notify can.
        let _keep = (event_tx, render_rx);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            h.stoat.run(event_rx, render_tx),
        )
        .await;

        assert!(
            matches!(result, Ok(Ok(()))),
            "run must quit after shutdown notify, got {result:?}"
        );
    });
}

#[cfg(feature = "perf")]
#[test]
fn input_driven_frame_carries_an_input_timestamp() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let mut h = Stoat::test();
        h.stoat.persistence_disabled = true;

        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let (render_tx, render_rx) = watch::channel(None);
        // Queue one input event and drop the sender. The loop drains the
        // event (publishing its frame), then breaks on the closed channel
        // before any background redraw can supersede it in the watch.
        event_tx.send(Event::Resize(80, 24)).expect("send");
        drop(event_tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            h.stoat.run(event_rx, render_tx),
        )
        .await
        .expect("run should quit")
        .expect("run ok");

        let frame = render_rx.borrow();
        let frame = frame.as_ref().expect("a frame was published");
        assert!(
            frame.input_time.is_some(),
            "an events.recv()-driven frame carries the input timestamp"
        );
    });
}

#[cfg(feature = "perf")]
#[test]
fn notify_driven_frame_has_no_input_timestamp() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let mut h = Stoat::test();
        h.stoat.persistence_disabled = true;
        // Give the frame a real size without routing through the event
        // channel, so no input timestamp is captured.
        h.stoat.update(Event::Resize(80, 24));
        let shutdown = h.stoat.shutdown_handle();

        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let (render_tx, render_rx) = watch::channel(None);
        // A redraw-notify wakes a frame with no input behind it. The biased
        // loop takes the redraw arm, publishing a frame, then quits.
        h.stoat.redraw_notify.notify_one();
        shutdown.notify_one();
        let _keep = event_tx;

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            h.stoat.run(event_rx, render_tx),
        )
        .await
        .expect("run should quit")
        .expect("run ok");

        let frame = render_rx.borrow();
        let frame = frame.as_ref().expect("a frame was published");
        assert!(
            frame.input_time.is_none(),
            "a redraw-notify frame carries no input timestamp"
        );
    });
}

#[test]
fn ctrl_w_in_insert_mode_deletes_previous_word() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_text("foo bar baz");
    h.type_keys("ctrl-w");
    assert_eq!(buffer_text(&h, &path), "foo bar ");
    h.type_keys("ctrl-w");
    assert_eq!(buffer_text(&h, &path), "foo ");
}

#[test]
fn ctrl_w_at_buffer_start_is_noop() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_keys("ctrl-w");
    assert_eq!(buffer_text(&h, &path), "");
}

#[test]
fn alt_backspace_in_insert_mode_deletes_previous_word() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("alpha beta gamma");
    h.type_keys("alt-backspace");
    assert_eq!(buffer_text(&h, &path), "alpha beta ");
}

#[test]
fn backspace_in_insert_mode_deletes_previous_char() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("abc");
    h.type_keys("backspace");
    assert_eq!(buffer_text(&h, &path), "ab");
    h.type_keys("backspace");
    assert_eq!(buffer_text(&h, &path), "a");
}

#[test]
fn end_in_insert_mode_lands_past_the_last_character() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\ndef\n");
    h.type_keys("i");
    h.type_keys("end");
    h.type_text("X");
    assert_eq!(
        buffer_text(&h, &path),
        "abcX\ndef\n",
        "the caret appends after the last character, not before it",
    );
}

#[test]
fn home_in_insert_mode_lands_at_column_zero() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\ndef\n");
    h.type_keys("l l i");
    h.type_keys("home");
    h.type_text("X");
    assert_eq!(buffer_text(&h, &path), "Xabc\ndef\n");
}

#[test]
fn cursor_keys_move_inside_the_palette_input() {
    let mut h = Stoat::test();
    open_scratch_file(&mut h, "abc\n");
    h.type_keys(":");
    h.type_text("abc");
    h.type_keys("left");
    h.type_text("X");

    assert_eq!(
        h.stoat
            .command_palette
            .as_ref()
            .expect("palette")
            .focused_input()
            .expect("input")
            .text(h.stoat.active_workspace()),
        "abXc"
    );
}

/// Measured against the actions themselves rather than against a row this
/// pin works out for itself, since the scroll arithmetic and the scrolloff
/// band are the motion's business and not this binding's. Both legs page
/// from insert mode, because the viewport a page spans is only known once a
/// frame has been drawn and entering insert mode draws one.
#[test]
fn page_keys_in_insert_mode_run_the_page_motions() {
    let rows: String = (0..80).map(|n| format!("line {n}\n")).collect();

    let expected = {
        let mut h = Stoat::test();
        open_scratch_file(&mut h, &rows);
        h.type_keys("i");
        action_handlers::dispatch(&mut h.stoat, &stoat_action::PageDown);
        let down = h.primary_head_offset();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::PageUp);
        (down, h.primary_head_offset())
    };
    assert_ne!(
        expected.0, 0,
        "a page motion that moves nothing pins nothing"
    );

    let mut h = Stoat::test();
    open_scratch_file(&mut h, &rows);
    h.type_keys("i");
    h.type_keys("pagedown");
    let down = h.primary_head_offset();
    h.type_keys("pageup");
    assert_eq!((down, h.primary_head_offset()), expected);
}

#[test]
fn typing_open_paren_inserts_the_pair() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("(");
    assert_eq!(buffer_text(&h, &path), "()");

    h.type_text("x");
    assert_eq!(
        buffer_text(&h, &path),
        "(x)",
        "the cursor is left between the halves",
    );
}

#[test]
fn pair_not_inserted_before_a_word_char() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "word");
    h.type_keys("i");
    h.type_text("(");
    assert_eq!(
        buffer_text(&h, &path),
        "(word",
        "a closer here would trap the word inside the pair",
    );
}

#[test]
fn typing_the_closer_skips_over_it() {
    // The closer is seeded rather than typed, so the pin fails against a
    // plain insert instead of reading the same either way.
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "()");
    h.type_keys("l i");
    h.type_text(")");
    assert_eq!(
        buffer_text(&h, &path),
        "()",
        "the typed closer is the one already there"
    );

    h.type_text("x");
    assert_eq!(buffer_text(&h, &path), "()x", "and the cursor is past it");
}

#[test]
fn quote_pair_requires_non_word_on_both_sides() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("don't");
    assert_eq!(
        buffer_text(&h, &path),
        "don't",
        "an apostrophe after a letter closes nothing",
    );

    h.type_text(" 'a");
    assert_eq!(
        buffer_text(&h, &path),
        "don't 'a'",
        "after a space it pairs, and the letter lands inside",
    );
}

#[test]
fn backspace_between_a_pair_deletes_both() {
    // Seeded rather than typed, since typing one bracket and removing it
    // reads the same whether or not the closer was ever written.
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "a()b");
    h.type_keys("l l i");
    h.type_keys("backspace");
    assert_eq!(
        buffer_text(&h, &path),
        "ab",
        "the closer leaves with the opener"
    );
}

#[test]
fn multi_cursor_pairs_land_per_cursor() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "\n\n");
    h.type_keys("C");
    h.type_keys("i");
    h.type_text("(");
    assert_eq!(buffer_text(&h, &path), "()\n()\n");
    assert_eq!(
        h.head_offsets(),
        vec![1, 4],
        "each cursor sits inside the pair it wrote",
    );
}

#[test]
fn a_closing_colon_swaps_its_shortcode_for_the_emoji() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("hi :smile:");

    assert_eq!(buffer_text(&h, &path), "hi \u{1f604}");
    assert_eq!(
        h.head_offsets(),
        vec!["hi \u{1f604}".len()],
        "the cursor sits after the glyph it typed",
    );
}

#[test]
fn a_colon_with_no_shortcode_to_close_is_typed_as_written() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("use std::mem");
    assert_eq!(
        buffer_text(&h, &path),
        "use std::mem",
        "a path is untouched"
    );

    h.type_text(" x:y: :notaname:");
    assert_eq!(
        buffer_text(&h, &path),
        "use std::mem x:y: :notaname:",
        "a colon after a letter and an unknown name both type as written",
    );
}

#[test]
fn a_multi_codepoint_emoji_lands_whole_with_the_cursor_past_it() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text(":+1:");

    let text = buffer_text(&h, &path);
    assert_eq!(text, "\u{1f44d}");
    assert_eq!(h.head_offsets(), vec![text.len()]);
}

#[test]
fn a_swap_and_a_plain_colon_land_together_across_cursors() {
    // The first cursor closes a shortcode and the second does not, so one
    // batch has to carry both an emoji and a literal colon.
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, ":smile\nstd\n");
    h.type_keys("i");
    select_forward(&mut h, 6, 6);
    insert_cursor_at(&mut h, 10);
    h.type_text(":");

    assert_eq!(buffer_text(&h, &path), "\u{1f604}\nstd:\n");
}

#[test]
fn emoji_expansion_off_types_every_colon_as_written() {
    let mut h = Stoat::test();
    h.stoat.settings.editor_emoji_expansion = Some(false);
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text(":smile:");

    assert_eq!(buffer_text(&h, &path), ":smile:");
}

#[test]
fn auto_pairs_off_types_every_character_as_written() {
    let mut h = Stoat::test();
    h.stoat.settings.editor_auto_pairs = Some(false);
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("(");
    assert_eq!(buffer_text(&h, &path), "(");

    h.type_keys("backspace");
    h.type_text("'");
    assert_eq!(buffer_text(&h, &path), "'", "quotes are left alone too");
}

#[test]
fn backspace_at_buffer_start_in_insert_mode_is_noop() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_keys("backspace");
    assert_eq!(buffer_text(&h, &path), "");
}

#[test]
fn delete_in_insert_mode_deletes_next_char() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abcdef");
    h.type_keys("l l i");
    h.type_keys("delete");
    assert_eq!(buffer_text(&h, &path), "abdef");
    h.type_keys("delete");
    assert_eq!(buffer_text(&h, &path), "abef");
}

/// A father-mother-daughter ZWJ sequence. Three 4-byte emoji joined by two
/// 3-byte zero-width joiners, so 18 bytes rendering as one cell.
const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";

#[test]
fn backspace_in_insert_mode_removes_a_whole_cluster() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("xe\u{301}");
    h.type_keys("backspace");
    assert_eq!(
        buffer_text(&h, &path),
        "x",
        "the combining acute leaves with the e it sits on",
    );
}

#[test]
fn delete_in_insert_mode_removes_a_whole_cluster() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, &format!("a{FAMILY}b"));
    h.type_keys("l i");
    h.type_keys("delete");
    assert_eq!(
        buffer_text(&h, &path),
        "ab",
        "the whole joined sequence goes, not its first emoji",
    );
}

#[test]
fn a_word_motion_selects_whole_clusters() {
    // A combining mark is its own codepoint and categorizes apart from the
    // letter it sits on, so the motion stops between them. What the
    // selection covers has to be whole characters even when the motion that
    // proposed it did not think so, or deleting the word leaves the mark
    // behind on whatever follows it.
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "cafe\u{301} bar");
    h.type_keys("w d");
    assert_eq!(
        buffer_text(&h, &path),
        " bar",
        "the accent leaves with the e it sits on",
    );
}

#[test]
fn goto_line_end_lands_on_a_whole_final_character() {
    // The step back off the line end has to be by character, and the
    // selection it lands has to cover a whole one. Both mechanisms are
    // needed for this gesture, and it fails if either regresses.
    for (source, remaining) in [
        ("ab cafe\u{301}", "ab caf"),
        (&format!("ab x{FAMILY}"), "ab x"),
    ] {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, source);
        h.type_keys("g l d");
        assert_eq!(
            buffer_text(&h, &path),
            remaining,
            "the whole last character goes, from {source:?}",
        );
    }
}

#[test]
fn replace_gives_one_character_per_character() {
    // The count is per character, not per codepoint it is written with. A
    // decomposed letter is two codepoints and a joined emoji is five, and
    // each is one character on the screen and under the cursor.
    for source in ["e\u{301}b", &format!("{FAMILY}b")] {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, source);
        h.type_keys("r x");
        assert_eq!(
            buffer_text(&h, &path),
            "xb",
            "one x for the character replaced, from {source:?}",
        );
    }
}

#[test]
fn replace_gives_one_character_for_each_one_selected() {
    // Three accented letters written with six codepoints, so what is being
    // asserted is the count rather than the single-character case. The
    // line selection takes the newline too, and that is a character like
    // any other here, so it is replaced rather than kept.
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "e\u{301}a\u{302}i\u{303}\n");
    h.type_keys("x r z");
    assert_eq!(buffer_text(&h, &path), "zzzz", "one z per character");
}

/// The block cursor sits on a cluster start at every step of an `l`/`h`
/// walk, never on a byte inside the joined sequence.
#[test]
fn horizontal_motion_never_lands_inside_a_cluster() {
    let mut h = Stoat::test();
    open_scratch_file(&mut h, &format!("a{FAMILY}b"));

    assert_eq!(h.head_offsets(), vec![0], "starts on the a");
    h.type_keys("l");
    assert_eq!(
        h.head_offsets(),
        vec![1],
        "l lands on the family's first byte"
    );
    h.type_keys("l");
    assert_eq!(h.head_offsets(), vec![19], "l clears all 18 bytes of it");
    h.type_keys("h");
    assert_eq!(h.head_offsets(), vec![1], "h returns over it whole");
    h.type_keys("h");
    assert_eq!(h.head_offsets(), vec![0]);
}

/// The step back off `a` measures a grapheme, not a byte.
///
/// The delete builds the fixture. It clears the line's tail and leaves a
/// zero-width cursor at the rope end, which is the one place `a` has nothing
/// to reach over. Its selection stays empty through the insert, so leaving
/// insert steps the whole cell back rather than shrinking a range. A step
/// measured in bytes lands inside the typed sequence.
#[test]
fn esc_from_append_steps_back_onto_a_cluster_start() {
    let mut h = Stoat::test();
    open_scratch_file(&mut h, "abc");
    h.type_keys("l v l d");
    h.type_keys("a");
    h.type_text(FAMILY);
    h.type_keys("escape");
    assert_eq!(
        h.selection_spans(),
        vec![(1, 19, false)],
        "the cursor lands on the sequence's first byte, not inside it",
    );
}

#[test]
fn delete_at_buffer_end_in_insert_mode_is_noop() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc");
    h.type_keys("A");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_keys("delete");
    assert_eq!(buffer_text(&h, &path), "abc");
}

#[test]
fn backspace_applies_at_every_cursor() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "xa\nxb\n");
    h.type_keys("l");
    h.type_keys("C");
    h.type_keys("i");
    h.type_keys("backspace");
    assert_eq!(buffer_text(&h, &path), "a\nb\n");
    assert_eq!(h.head_offsets(), vec![0, 2]);
    // The sorted heads above are the same set however the cursors are
    // permuted, so only a named cursor shows each landed at its own delete.
    assert_eq!(h.primary_head_offset(), 2, "cursor added below");
}

#[test]
fn delete_applies_at_every_cursor() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "ax\nbx\n");
    h.type_keys("C");
    h.type_keys("i");
    h.type_keys("delete");
    assert_eq!(buffer_text(&h, &path), "x\nx\n");
    assert_eq!(h.head_offsets(), vec![0, 2]);
}

#[test]
fn alt_backspace_applies_at_every_cursor() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "foo\nbar\n");
    h.type_keys("l l");
    h.type_keys("C");
    h.type_keys("a");
    h.type_keys("alt-backspace");
    assert_eq!(buffer_text(&h, &path), "\n\n");
    assert_eq!(h.head_offsets(), vec![0, 1]);
}

#[test]
fn alt_backspace_merges_overlapping_word_deletes() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "hello\n");
    h.type_keys("l l");
    insert_cursor_at(&mut h, 4);
    h.type_keys("i");
    h.type_keys("alt-backspace");
    assert_eq!(buffer_text(&h, &path), "o\n");
    assert_eq!(h.head_offsets(), vec![0]);
}

/// A word deletion never cuts a grapheme cluster in half.
///
/// Word motions stop mid-cluster on purpose, deferring the snap to wherever
/// their answer is written. Writing a selection snaps, and splicing the rope
/// does not, so a deletion driven straight off a motion has to snap for
/// itself or it orphans a combining mark onto whatever text survives.
#[test]
fn a_word_delete_forward_keeps_a_combining_mark_with_its_base() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "cafe\u{301} bar");
    h.type_keys("i");
    h.type_keys("alt-d");
    assert_eq!(
        buffer_text(&h, &path),
        " bar",
        "the acute goes with the e it sits on, not onto the space",
    );
}

/// The backward sibling, where the motion's endpoint lands inside a cluster
/// rather than after one.
///
/// `prev_word_start` stops between the `e` and its acute, so the snap grows
/// the deletion out to the cluster's start and takes both. Leaving the base
/// behind without its mark, which is what an unsnapped splice does, would
/// silently rewrite the surviving character.
#[test]
fn a_word_delete_backward_keeps_a_combining_mark_with_its_base() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "cafe\u{301}");
    h.type_keys("A");
    h.type_keys("alt-backspace");
    assert_eq!(
        buffer_text(&h, &path),
        "caf",
        "the accented e goes whole rather than leaving a bare e behind",
    );
}

#[test]
fn ctrl_u_kills_to_first_non_whitespace_then_line_start() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "  foo bar");
    h.type_keys("A");
    h.type_keys("ctrl-u");
    assert_eq!(
        buffer_text(&h, &path),
        "  ",
        "first kill preserves the indent"
    );
    h.type_keys("ctrl-u");
    assert_eq!(buffer_text(&h, &path), "", "second kill removes the indent");
}

#[test]
fn ctrl_u_inside_indent_kills_to_line_start() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "  foo");
    h.type_keys("l i");
    h.type_keys("ctrl-u");
    assert_eq!(buffer_text(&h, &path), " foo");
}

#[test]
fn ctrl_u_at_line_start_joins_previous_line() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "ab\ncd");
    h.type_keys("j i");
    h.type_keys("ctrl-u");
    assert_eq!(buffer_text(&h, &path), "abcd");
}

#[test]
fn ctrl_u_at_buffer_start_is_noop() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc");
    h.type_keys("i");
    h.type_keys("ctrl-u");
    assert_eq!(buffer_text(&h, &path), "abc");
}

#[test]
fn ctrl_k_kills_to_line_end() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "ab cd\nxy");
    h.type_keys("l l i");
    h.type_keys("ctrl-k");
    assert_eq!(buffer_text(&h, &path), "ab\nxy");
}

#[test]
fn ctrl_k_at_line_end_deletes_line_separator() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "ab\ncd");
    h.type_keys("A");
    h.type_keys("ctrl-k");
    assert_eq!(buffer_text(&h, &path), "abcd");
}

#[test]
fn ctrl_k_at_buffer_end_is_noop() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc");
    h.type_keys("A");
    h.type_keys("ctrl-k");
    assert_eq!(buffer_text(&h, &path), "abc");
}

#[test]
fn ctrl_k_applies_at_every_cursor() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "ax\nbx\n");
    h.type_keys("C");
    h.type_keys("i");
    h.type_keys("ctrl-k");
    assert_eq!(buffer_text(&h, &path), "\n\n");
    assert_eq!(h.head_offsets(), vec![0, 1]);
}

#[test]
fn alt_d_deletes_next_word() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "foo bar");
    h.type_keys("i");
    h.type_keys("alt-d");
    assert_eq!(buffer_text(&h, &path), " bar");
    h.type_keys("alt-d");
    assert_eq!(buffer_text(&h, &path), "");
}

#[test]
fn alt_d_at_buffer_end_is_noop() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc");
    h.type_keys("A");
    h.type_keys("alt-d");
    assert_eq!(buffer_text(&h, &path), "abc");
}

/// Alt-Delete is the forward alias of Alt-Backspace, so it takes the whole
/// word rather than the one character a bare Delete takes.
#[test]
fn alt_delete_deletes_word_forward() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "foo bar");
    h.type_keys("i");
    h.type_keys("alt-delete");
    assert_eq!(buffer_text(&h, &path), " bar");
}

#[test]
fn bare_delete_still_deletes_one_char() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "foo bar");
    h.type_keys("i");
    h.type_keys("delete");
    assert_eq!(buffer_text(&h, &path), "oo bar");
}

#[test]
fn ctrl_h_deletes_previous_char() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc");
    h.type_keys("A");
    h.type_keys("ctrl-h");
    assert_eq!(buffer_text(&h, &path), "ab");
}

#[test]
fn ctrl_d_deletes_next_char() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abcdef");
    h.type_keys("l l i");
    h.type_keys("ctrl-d");
    assert_eq!(buffer_text(&h, &path), "abdef");
}

#[test]
fn ctrl_j_inserts_newline_with_continued_indent() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "  ab");
    h.type_keys("A");
    h.type_keys("ctrl-j");
    assert_eq!(buffer_text(&h, &path), "  ab\n  ");
}

#[test]
fn insert_session_undoes_and_redoes_as_one_step() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("hello");
    h.type_keys("esc");
    assert_eq!(buffer_text(&h, &path), "hello");
    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "",
        "one undo clears the whole insert session"
    );
    h.type_keys("U");
    assert_eq!(
        buffer_text(&h, &path),
        "hello",
        "one redo restores the whole session"
    );
}

/// A change and the text typed into it are one revision, so one undo takes
/// back both.
///
/// The two arrive as one chord, `[ChangeSelection(), SetMode(insert)]`, and
/// the user made one change. Sealing between the halves leaves the delete
/// standing after an undo, which is a state they never asked for.
#[test]
fn change_selection_and_typing_undo_as_one_step() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "foo bar\n");
    h.type_keys("w");
    let before = h.selection_spans();

    h.type_keys("c");
    h.type_text("X");
    h.type_keys("esc");
    assert_eq!(buffer_text(&h, &path), "Xbar\n");

    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "foo bar\n",
        "one undo takes back the change and the typing together",
    );
    assert_eq!(
        h.selection_spans(),
        before,
        "and restores the selection the change was made from",
    );

    h.type_keys("U");
    assert_eq!(buffer_text(&h, &path), "Xbar\n", "one redo puts both back");
}

/// A server edit inside an insert session is its own step, and the typing
/// after it stays one.
///
/// A format-on-save or a completion's extra edits land mid-session. Sealing
/// the session for them and leaving it sealed drops the session on the
/// floor, so every later keystroke becomes a revision of its own and undo
/// deletes one character at a time for the rest of the session.
#[test]
fn a_server_edit_mid_session_leaves_the_typing_after_it_one_step() {
    use crate::host::OffsetEncoding;
    use lsp_types::{Position, Range as LspRange, TextEdit};

    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "foo\n");

    h.type_keys("i");
    h.type_text("A");

    crate::lsp::edit_apply::apply_text_edits_to_buffer(
        &mut h.stoat,
        &path,
        vec![TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 0)),
            new_text: "S".to_string(),
        }],
        OffsetEncoding::Utf16,
    )
    .expect("the server edit applies");

    h.type_text("BC");
    h.type_keys("esc");
    assert_eq!(buffer_text(&h, &path), "SABCfoo\n");

    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "SAfoo\n",
        "one undo takes back every key typed after the server edit",
    );

    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "Afoo\n",
        "and the next takes back the server's edit on its own",
    );
}

/// Opening a line and typing into it are likewise one revision.
#[test]
fn open_below_and_typing_undo_as_one_step() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "foo\n");
    h.type_keys("o");
    h.type_text("bar");
    h.type_keys("esc");
    assert_eq!(buffer_text(&h, &path), "foo\nbar\n");

    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "foo\n",
        "the opened line goes with the text typed into it",
    );

    h.type_keys("U");
    assert_eq!(buffer_text(&h, &path), "foo\nbar\n");
}

#[test]
fn a_mid_session_motion_keeps_the_insert_session_one_undo_step() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("abc");
    h.type_keys("left");
    h.type_text("x");
    h.type_keys("esc");
    assert_eq!(buffer_text(&h, &path), "abxc");
    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "",
        "one undo reverts the whole session despite the mid-session motion"
    );
    h.type_keys("U");
    assert_eq!(
        buffer_text(&h, &path),
        "abxc",
        "one redo restores the whole session"
    );
}

#[test]
fn a_mid_session_dispatched_action_joins_the_insert_session() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("a");
    action_handlers::dispatch(&mut h.stoat, &stoat_action::SmartTab);
    h.type_text("b");
    h.type_keys("esc");
    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "",
        "a mid-session dispatched action leaves the session's single undo step intact"
    );
}

#[test]
fn delete_undoes_both_cursors_and_restores_selections() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "ab\nab\n");
    h.type_keys("l");
    h.type_keys("C");
    let before = h.head_offsets();
    h.type_keys("d");
    assert_eq!(buffer_text(&h, &path), "a\na\n");
    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "ab\nab\n",
        "one undo restores both cursors' deletions"
    );
    assert_eq!(h.head_offsets(), before, "undo restores both selections");
}

/// One undo reverts a whole multi-cursor insert session, and redo restores it.
///
/// The delete sibling above pins that side. An insert session is grouped
/// differently. Its group opens on entering insert mode and seals on leaving
/// it, rather than being wrapped around one action, so the two paths can fail
/// independently.
#[test]
fn an_insert_session_undoes_and_redoes_at_every_cursor() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "ab\nab\n");
    h.type_keys("l");
    h.type_keys("C");
    let before = h.head_offsets();

    h.type_keys("i");
    h.type_text("XY");
    h.type_keys("esc");
    assert_eq!(buffer_text(&h, &path), "aXYb\naXYb\n");

    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "ab\nab\n",
        "one undo reverts the session at both cursors",
    );
    assert_eq!(h.head_offsets(), before, "and restores both selections");

    h.type_keys("shift-U");
    assert_eq!(
        buffer_text(&h, &path),
        "aXYb\naXYb\n",
        "one redo reapplies the session at both cursors",
    );
}

#[test]
fn ctrl_s_splits_the_insert_session_into_two_undo_steps() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("hello");
    h.type_keys("ctrl-s");
    h.type_text("world");
    h.type_keys("esc");
    assert_eq!(buffer_text(&h, &path), "helloworld");
    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "hello",
        "the first undo reverts only the post-checkpoint edits"
    );
    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "",
        "the second undo reverts the pre-checkpoint edits"
    );
}

/// A checkpoint taken in normal mode does not swallow later edits.
///
/// Ctrl-s exists to split an insert session in two, so outside one there is
/// nothing to split and it is a plain checkpoint. It runs without the
/// action-group wrapper, so a group it opened in normal mode is one nothing
/// would close, and every later edit would keep joining it -- including
/// across an undo, which is what makes those edits reachable only as part of
/// a step they do not belong to.
#[test]
fn a_normal_mode_checkpoint_leaves_later_edits_undoable() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abcdef\n");

    h.type_keys("ctrl-s");
    h.type_keys("d");
    assert_eq!(buffer_text(&h, &path), "bcdef\n", "d deletes one character");
    h.type_keys("u");
    assert_eq!(buffer_text(&h, &path), "abcdef\n", "and undoes");

    // A second edit, since x is ExtendLineBelow here and would not make one.
    h.type_keys("d");
    assert_eq!(
        buffer_text(&h, &path),
        "bcdef\n",
        "the post-undo edit lands"
    );

    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "abcdef\n",
        "the edit made after the undo undoes on its own",
    );
}

/// Edits after a normal-mode checkpoint stay separate undo steps.
///
/// The action-group wrapper leaves an already-open group alone so a
/// mid-session action joins the insert step it sits in. A group Ctrl-s
/// opened in normal mode has no session to belong to and nothing to seal it,
/// so every later action would keep joining it and the whole run would
/// collapse into one step.
#[test]
fn edits_after_a_normal_mode_checkpoint_undo_separately() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abcdef\n");

    h.type_keys("ctrl-s");
    h.type_keys("d");
    h.type_keys("d");
    assert_eq!(buffer_text(&h, &path), "cdef\n", "two characters deleted");

    h.type_keys("u");
    assert_eq!(
        buffer_text(&h, &path),
        "bcdef\n",
        "one undo reverts only the second delete",
    );
}

#[test]
fn insert_types_at_every_cursor() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "aa\nbb\n");
    h.type_keys("C");
    h.type_keys("i");
    h.type_text("XY");
    assert_eq!(buffer_text(&h, &path), "XYaa\nXYbb\n");
    assert_eq!(h.head_offsets(), vec![2, 7]);
}

/// Two cursors on one line each land after their own inserted text.
///
/// The landing arithmetic and the batch's descending order are exercised by
/// the one-cursor-per-line tests too, since both work in offsets and a later
/// cursor carries the earlier insertions wherever it sits. What is untested
/// is the shape itself. Nothing else drives two cursors into a single line, so
/// a change that started treating rows separately would go unnoticed.
#[test]
fn same_line_cursors_each_land_after_their_own_insert() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc");
    h.type_keys("l");
    insert_cursor_at(&mut h, 2);
    h.type_keys("i");
    h.type_text("X");
    assert_eq!(buffer_text(&h, &path), "aXbXc");
    assert_eq!(
        h.head_offsets(),
        vec![2, 4],
        "the later cursor carries the earlier insertion as well as its own",
    );
}

/// The insert paths reverse this list into `edit_batch`, which takes its
/// ranges sorted descending by start. Reversing gives descending only
/// because this answers ascending, and cursors are added in whatever order
/// the reader made them, so the sort is doing real work.
///
/// `edit_batch` checks the order with a `debug_assert`, so a release build
/// has nothing but this holding it.
#[test]
fn cursor_offsets_come_back_in_ascending_order() {
    let mut h = Stoat::test();
    open_scratch_file(&mut h, "abcdefgh");

    for offset in [6, 2, 4] {
        insert_cursor_at(&mut h, offset);
    }

    let (editor_id, _) = h.stoat.focused_editor_ids().expect("editor");
    let cursors = h.stoat.editor_cursor_offsets(editor_id);
    let offsets: Vec<usize> = cursors.iter().map(|(_, offset)| *offset).collect();

    assert_eq!(
        offsets,
        vec![0, 2, 4, 6],
        "the cursors come back in offset order, not the order they were made",
    );
}

/// Adjacent cursors backspacing collapse onto one deletion.
///
/// Their delete ranges touch end to start, which no other end-to-end test
/// produces. `merge_overlapping_spans` leaves them alone, since touching is not
/// overlapping, and `edit_batch` takes them as two ranges. Both cursors then
/// land on the same offset and reduce to one.
#[test]
fn adjacent_cursors_backspacing_merge_into_one() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abcd");
    h.type_keys("l");
    insert_cursor_at(&mut h, 2);
    h.type_keys("i");
    h.type_keys("backspace");
    assert_eq!(buffer_text(&h, &path), "cd", "both leading characters go");
    assert_eq!(
        h.head_offsets(),
        vec![0],
        "the two cursors land on the same offset and dedupe",
    );
}

#[test]
fn insert_single_cursor_advances_past_text() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc");
    h.type_keys("i");
    h.type_text("X");
    assert_eq!(buffer_text(&h, &path), "Xabc");
    assert_eq!(h.head_offsets(), vec![1]);
}

#[test]
fn insert_with_forward_selection_types_at_block_cursor() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abcdef");
    select_forward(&mut h, 0, 3);
    h.stoat.transition_mode("insert".to_string());
    h.type_text("X");
    assert_eq!(buffer_text(&h, &path), "abXcdef");
}

#[test]
fn backspace_with_forward_selection_acts_at_block_cursor() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abcdef");
    select_forward(&mut h, 0, 3);
    h.stoat.transition_mode("insert".to_string());
    h.type_keys("backspace");
    assert_eq!(buffer_text(&h, &path), "acdef");
}

#[test]
fn enter_in_insert_mode_inserts_newline_in_file_buffer() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("abc");
    h.type_keys("enter");
    h.type_text("xyz");
    assert_eq!(buffer_text(&h, &path), "abc\nxyz");
}

#[test]
fn enter_after_trailing_spaces_trims_them() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_text("foo   ");
    h.type_keys("enter");
    assert_eq!(
        buffer_text(&h, &path),
        "foo\n",
        "the line the break leaves behind keeps no trailing whitespace",
    );
}

#[test]
fn enter_inside_leading_indent_pushes_the_line_down() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "    foo\n");
    h.type_keys("l l i");
    h.type_keys("enter");
    assert_eq!(
        buffer_text(&h, &path),
        "\n    foo\n",
        "the whole line moves down with its indent rather than splitting",
    );

    h.type_text("X");
    assert_eq!(
        buffer_text(&h, &path),
        "\n  X  foo\n",
        "the cursor keeps its column on the line it followed down",
    );
}

#[test]
fn append_advances_one_char_then_inserts() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\n");
    h.type_keys("a");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_text("X");
    assert_eq!(buffer_text(&h, &path), "aXbc\n");
}

#[test]
fn shift_i_jumps_to_first_nonwhitespace_then_inserts() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "    code\n");
    h.type_keys("l");
    h.type_keys("I");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_text("X");
    assert_eq!(buffer_text(&h, &path), "    Xcode\n");
}

#[test]
fn shift_a_jumps_to_line_end_then_inserts() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\nxyz\n");
    h.type_keys("A");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_text("Z");
    assert_eq!(buffer_text(&h, &path), "abcZ\nxyz\n");
}

#[test]
fn shift_i_on_empty_line_auto_indents() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"fn a() {\n\n}\n");
    h.type_keys("j");
    h.type_keys("I");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_text("x");
    assert_eq!(focused_buffer_string(&h), "fn a() {\n\tx\n}\n");
}

/// Each cursor lands past every indent inserted ahead of it, not only its
/// own. The two blank lines sit at different depths so their indents differ
/// in length, and the line between them takes the non-blank path, whose
/// landing shifts by the insert above it.
#[test]
fn shift_i_at_nested_blank_lines_lands_each_past_the_indents_ahead() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"fn a() {\n\n\tif b {\n\n\t}\n}\n");
    h.type_keys("j");
    h.type_keys("2 C");
    assert_eq!(
        h.selection_spans().len(),
        3,
        "a cursor on each of three rows"
    );

    h.type_keys("I");
    h.type_text("x");
    assert_eq!(
        focused_buffer_string(&h),
        "fn a() {\n\tx\n\txif b {\n\t\tx\n\t}\n}\n",
        "every cursor types where it landed, none inside another's indent",
    );
}

#[test]
fn shift_i_on_whitespace_line_falls_back_to_line_start() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\n    \ndef\n");
    h.type_keys("j");
    h.type_keys("l");
    h.type_keys("I");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_text("X");
    assert_eq!(buffer_text(&h, &path), "abc\nX    \ndef\n");
}

#[test]
fn shift_a_on_empty_line_auto_indents() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"fn a() {\n\n}\n");
    h.type_keys("j");
    h.type_keys("A");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_text("x");
    assert_eq!(focused_buffer_string(&h), "fn a() {\n\tx\n}\n");
}

#[test]
fn open_below_then_escape_strips_untouched_auto_indent() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"fn a() {\n}\n");
    h.type_keys("o");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_keys("escape");
    assert_eq!(focused_buffer_string(&h), "fn a() {\n\n}\n");
}

/// One edit covers every stripped line, so a second cursor's line has to
/// come out as clean as the first, and at the coordinates it had before any
/// of them ran.
///
/// Three of them, so the batch carries more than the pair that a single
/// off-by-one in its ordering still happens to get right.
#[test]
fn open_below_at_three_cursors_then_escape_strips_every_indent() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"fn a() {\n\tx\n\ty\n\tz\n}\n");
    h.type_keys("j");
    h.type_keys("2 C");
    assert_eq!(h.selection_spans().len(), 3, "a cursor on each inner line");

    h.type_keys("o");
    assert_eq!(
        focused_buffer_string(&h),
        "fn a() {\n\tx\n\t\n\ty\n\t\n\tz\n\t\n}\n",
        "every opened line carries the indent while insert mode is open",
    );

    h.type_keys("escape");
    assert_eq!(
        focused_buffer_string(&h),
        "fn a() {\n\tx\n\n\ty\n\n\tz\n\n}\n",
        "and every one loses it again untouched",
    );
}

#[test]
fn open_below_then_type_then_escape_keeps_indent() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"fn a() {\n}\n");
    h.type_keys("o");
    h.type_text("x");
    h.type_keys("escape");
    assert_eq!(focused_buffer_string(&h), "fn a() {\n\tx\n}\n");
}

#[test]
fn shift_i_on_empty_line_then_escape_strips_indent() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"fn a() {\n\n}\n");
    h.type_keys("j");
    h.type_keys("I");
    h.type_keys("escape");
    assert_eq!(focused_buffer_string(&h), "fn a() {\n\n}\n");
}

#[test]
fn shift_a_on_empty_line_then_escape_strips_indent() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"fn a() {\n\n}\n");
    h.type_keys("j");
    h.type_keys("A");
    h.type_keys("escape");
    assert_eq!(focused_buffer_string(&h), "fn a() {\n\n}\n");
    assert_eq!(h.selection_spans(), vec![(9, 10, false)]);
}

#[test]
fn insert_on_whitespace_line_then_escape_keeps_whitespace() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\n    \ndef\n");
    h.type_keys("j");
    h.type_keys("i");
    h.type_keys("escape");
    assert_eq!(buffer_text(&h, &path), "abc\n    \ndef\n");
}

#[test]
fn count_open_below_opens_that_many_lines() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"fn a() {\n}\n");
    h.type_keys("3 o");
    h.type_text("x");
    assert_eq!(focused_buffer_string(&h), "fn a() {\n\tx\n\tx\n\tx\n}\n");
}

#[test]
fn open_below_opens_per_selection_without_row_dedup() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abcdef\n");
    insert_cursor_at(&mut h, 3);
    h.type_keys("o");
    assert_eq!(h.selection_spans().len(), 2);
    h.type_text("X");
    assert_eq!(buffer_text(&h, &path), "abcdef\nX\nX\n");
}

#[test]
fn open_below_continues_line_comment() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"// foo\n");
    h.type_keys("o");
    h.type_text("bar");
    assert_eq!(focused_buffer_string(&h), "// foo\n// bar\n");
}

#[test]
fn open_above_continues_line_comment() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"// foo\n");
    h.type_keys("O");
    h.type_text("bar");
    assert_eq!(focused_buffer_string(&h), "// bar\n// foo\n");
}

/// Changing a commented line opens a bare one, where o and O continue the
/// comment.
///
/// The two share the open, but they mean different things. `o` writes a
/// second line of a comment, while `c` deleted the commented line already
/// and opens its replacement, so a token there prefixes text the user
/// never asked to comment.
#[test]
fn change_opens_a_line_without_continuing_the_comment() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"// foo\n// bar\n");
    h.type_keys("x");
    h.type_keys("c");
    h.type_text("baz");
    assert_eq!(focused_buffer_string(&h), "baz\n// bar\n");
}

#[test]
fn insert_enter_continues_line_comment() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"// foo\n");
    h.type_keys("A");
    h.type_keys("enter");
    h.type_text("bar");
    assert_eq!(focused_buffer_string(&h), "// foo\n// bar\n");
}

#[test]
fn insert_enter_continues_doc_comment_token() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"/// foo\n");
    h.type_keys("A");
    h.type_keys("enter");
    h.type_text("bar");
    assert_eq!(focused_buffer_string(&h), "/// foo\n/// bar\n");
}

#[test]
fn insert_enter_continues_inner_doc_comment_token() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"//! foo\n");
    h.type_keys("A");
    h.type_keys("enter");
    h.type_text("bar");
    assert_eq!(focused_buffer_string(&h), "//! foo\n//! bar\n");
}

#[test]
fn insert_enter_before_the_comment_token_carries_no_token() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"// foo\n");
    h.type_keys("i");
    h.type_keys("enter");
    assert_eq!(focused_buffer_string(&h), "\n// foo\n");
}

#[test]
fn enter_between_braces_opens_an_indented_line() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"fn f() {}\n");
    h.type_keys("l l l l l l l l i");
    h.type_keys("enter");
    assert_eq!(
        focused_buffer_string(&h),
        "fn f() {\n\t\n}\n",
        "the braces part onto their own lines with a line between them",
    );

    h.type_text("x");
    assert_eq!(
        focused_buffer_string(&h),
        "fn f() {\n\tx\n}\n",
        "and the cursor is on that line, one level in",
    );
}

#[test]
fn insert_enter_inside_a_comment_indent_carries_no_token() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"    // foo\n");
    h.type_keys("l l i");
    h.type_keys("enter");
    assert_eq!(
        focused_buffer_string(&h),
        "\n    // foo\n",
        "the indent moves down whole, carrying neither a token nor a re-indent",
    );
}

#[test]
fn open_below_continues_doc_comment_token() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"/// foo\n");
    h.type_keys("o");
    h.type_text("bar");
    assert_eq!(focused_buffer_string(&h), "/// foo\n/// bar\n");
}

#[test]
fn open_below_inserts_blank_line_after_current_row() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\ndef\n");
    h.type_keys("o");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_text("X");
    assert_eq!(buffer_text(&h, &path), "abc\nX\ndef\n");
}

#[test]
fn open_below_after_open_brace_auto_indents() {
    let mut h = Stoat::test();
    open_indent_buffer(&mut h, "a.rs", b"fn a() {\n}\n");
    h.type_keys("o");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_text("x");
    assert_eq!(focused_buffer_string(&h), "fn a() {\n\tx\n}\n");
}

#[test]
fn open_above_inserts_blank_line_before_current_row() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\ndef\n");
    h.type_keys("o");
    h.type_keys("escape");
    h.type_keys("O");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_text("Y");
    assert_eq!(buffer_text(&h, &path), "abc\nY\n\ndef\n");
}

#[test]
fn open_below_at_last_line_appends_at_eof() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc");
    h.type_keys("o");
    h.type_text("X");
    assert_eq!(buffer_text(&h, &path), "abc\nX");
}

#[test]
fn open_above_at_first_line_inserts_at_offset_zero() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\n");
    h.type_keys("O");
    h.type_text("Z");
    assert_eq!(buffer_text(&h, &path), "Z\nabc\n");
}

#[test]
fn change_selection_deletes_then_enters_insert() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abcdef");
    h.type_keys("v l l l");
    h.type_keys("c");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.type_text("XYZ");
    assert_eq!(buffer_text(&h, &path), "XYZef");
}

/// Alt-c is change, not delete, so it opens the replacement line.
///
/// Both keys delete the selection and enter insert. Only change opens a
/// line above a whole-line deletion, so a binding built out of a delete
/// types the replacement onto the line that followed instead.
#[test]
fn a_no_yank_change_of_whole_lines_opens_a_line_to_type_on() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "aaa\nbbb\n");
    h.type_keys("x");
    h.type_keys("alt-c");
    h.type_text("Z");
    assert_eq!(buffer_text(&h, &path), "Z\nbbb\n");
}

/// Alt-c keeps the deleted text out of every register, the way Alt-d does.
#[test]
fn a_no_yank_change_leaves_the_register_alone() {
    use crate::register::Register;

    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "aaa\nbbb\n");
    h.type_keys("y");
    h.type_keys("x");
    h.type_keys("alt-c");
    assert_eq!(
        h.stoat.registers.read(Register::Unnamed),
        Some(["a".to_string()].as_slice()),
        "the earlier yank survives, since the change writes no register",
    );
}

#[test]
fn replace_char_replaces_each_char_in_selection() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abcdef");
    h.type_keys("v l l l");
    h.type_keys("r");
    h.type_keys("X");
    assert_eq!(buffer_text(&h, &path), "XXXXef");
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "a replacement that lands hands back normal mode"
    );
}

/// The chord arms while select mode is on and the mode ends with the edit,
/// so a chord that never applies leaves the mode where it found it.
#[test]
fn a_cancelled_replace_char_keeps_select_mode() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abcdef");
    h.type_keys("v l l l");

    h.type_keys("r");
    assert!(h.stoat.pending_replace, "chord armed");

    h.type_keys("escape");
    assert!(!h.stoat.pending_replace, "chord dropped");
    assert_eq!(h.stoat.focused_mode(), "select");
    assert_eq!(buffer_text(&h, &path), "abcdef");
}

#[test]
fn replace_char_on_bare_cursor_replaces_char() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc");
    h.type_keys("r");
    h.type_keys("X");
    assert_eq!(buffer_text(&h, &path), "Xbc");
    assert_eq!(h.stoat.focused_mode(), "normal");
    assert!(!h.stoat.pending_replace);
}

/// Enter is the only way to replace a run with a line break, so it names
/// one rather than cancelling the chord.
#[test]
fn replace_char_with_enter_inserts_line_ending() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc");
    h.type_keys("r");
    h.type_keys("enter");
    assert_eq!(buffer_text(&h, &path), "\nbc");
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn replace_char_with_tab_inserts_tab() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc");
    h.type_keys("r");
    h.type_keys("tab");
    assert_eq!(buffer_text(&h, &path), "\tbc");
}

/// The replacement repeats once per character covered, so a run becomes a
/// run of line breaks rather than a single one.
#[test]
fn replace_char_with_enter_repeats_over_the_selection() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abcdef");
    h.type_keys("v l l");
    h.type_keys("r");
    h.type_keys("enter");
    assert_eq!(buffer_text(&h, &path), "\n\n\ndef");
}

#[test]
fn replace_char_with_multibyte_input_grows_buffer() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc");
    h.type_keys("v l l");
    h.type_keys("r");
    h.type_text("é");
    assert_eq!(buffer_text(&h, &path), "ééé");
}

#[test]
fn tab_at_line_start_inserts_tab() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\n");
    h.type_keys("i");
    h.type_keys("tab");
    assert_eq!(buffer_text(&h, &path), "\tabc\n");
}

#[test]
fn i_on_selection_inserts_before_it() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "foo\n");
    h.type_keys("%");
    h.type_keys("i");
    h.type_keys("X");
    assert_eq!(buffer_text(&h, &path), "Xfoo\n");
}

/// A leaves the cursor where the insert ended, past the last typed
/// character.
///
/// Only `a` steps a cursor back on the way out, since only `a` reached one
/// out in the first place. `A` moves a cursor to the line end without
/// extending it, so nothing is owed back and it stays where it was left.
#[test]
fn shift_a_then_escape_rests_past_the_last_typed_char() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\n");
    h.type_keys("A");
    h.type_keys("X");
    h.type_keys("escape");
    assert_eq!(buffer_text(&h, &path), "abcX\n");
    assert_eq!(h.selection_spans(), vec![(4, 5, false)]);
}

#[test]
fn tab_after_whitespace_inserts_indent_unit() {
    let mut h = Stoat::test();
    // The 2-space indent makes the buffer space-styled, so Tab inserts it.
    let path = open_scratch_file(&mut h, "  abc\n");
    h.type_keys("l l i");
    h.type_keys("tab");
    assert_eq!(buffer_text(&h, &path), "    abc\n");
}

#[test]
fn tab_after_nonwhitespace_is_noop() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\n");
    h.type_keys("l l l i");
    h.type_keys("tab");
    assert_eq!(buffer_text(&h, &path), "abc\n");
}

#[test]
fn backtab_inserts_indent_unit_unconditionally() {
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "abc\n");
    h.type_keys("l l i");
    h.type_keys("backtab");
    assert_eq!(buffer_text(&h, &path), "ab\tc\n");
}

#[test]
fn backspace_on_leading_indent_removes_one_width() {
    let mut h = Stoat::test();
    // The 4-space indent makes the buffer a 4-space style.
    let path = open_scratch_file(&mut h, "    abc\n");
    h.type_keys("l l l l i");
    h.type_keys("backspace");
    assert_eq!(buffer_text(&h, &path), "abc\n");
}

#[test]
fn pending_completion_defaults_to_none() {
    let h = Stoat::test();
    assert_eq!(h.stoat.pending_completion, None);
}

#[test]
fn esc_in_insert_with_open_popup_clears_popup_and_exits_to_normal() {
    use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
    let mut h = Stoat::test();
    let _path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    assert_eq!(h.stoat.focused_mode(), "insert");
    h.stoat.pending_completion = Some(CompletionPopup::showing(vec![CompletionItem {
        label: "foo".into(),
        source: CompletionSource::Lsp,
        kind: None,
        detail: None,
        replace_range: crate::completion::unused_replace_range(),
        insert_text: "foo".into(),
        is_snippet: false,
        documentation: None,
        lsp_item: None,
        server: None,
    }]));
    h.type_keys("escape");
    assert_eq!(h.stoat.pending_completion, None);
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "one escape closes the popup and leaves insert mode",
    );
}

#[test]
fn esc_in_insert_with_no_popup_exits_to_normal() {
    let mut h = Stoat::test();
    let _path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    assert_eq!(h.stoat.focused_mode(), "insert");
    assert_eq!(h.stoat.pending_completion, None);
    h.type_keys("escape");
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn tab_with_no_popup_smart_indents_after_whitespace() {
    let mut h = Stoat::test();
    // The 2-space indent makes the buffer space-styled.
    let path = open_scratch_file(&mut h, "  abc\n");
    h.type_keys("l l i");
    assert!(h.stoat.pending_completion.is_none());
    h.type_keys("tab");
    assert_eq!(buffer_text(&h, &path), "    abc\n");
}

#[test]
fn tab_with_popup_open_invokes_acceptance() {
    use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_keys("f o o");
    let replace_range = crate::completion::anchor_range_in_focused(&h.stoat, 0..3);
    h.stoat.pending_completion = Some(CompletionPopup {
        prefix_range: 0..3,
        ..CompletionPopup::showing(vec![CompletionItem {
            label: "foobar".into(),
            source: CompletionSource::Word,
            kind: None,
            detail: None,
            replace_range,
            insert_text: "foobar".into(),
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        }])
    });

    h.type_keys("tab");

    assert_eq!(buffer_text(&h, &path), "foobar");
    assert!(h.stoat.pending_completion.is_none());
}

#[test]
fn up_and_down_arrows_navigate_popup_without_moving_cursor() {
    use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
    let mut h = Stoat::test();
    let _path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_keys("f");
    let cursor_before = focused_primary_offsets(&mut h);
    assert_eq!(cursor_before.0, 1);

    let popup = || CompletionPopup {
        prefix_range: 0..1,
        ..CompletionPopup::showing(vec![
            CompletionItem {
                label: "foo".into(),
                source: CompletionSource::Word,
                kind: None,
                detail: None,
                replace_range: crate::completion::unused_replace_range(),
                insert_text: "foo".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
                server: None,
            },
            CompletionItem {
                label: "foobar".into(),
                source: CompletionSource::Word,
                kind: None,
                detail: None,
                replace_range: crate::completion::unused_replace_range(),
                insert_text: "foobar".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
                server: None,
            },
            CompletionItem {
                label: "foobaz".into(),
                source: CompletionSource::Word,
                kind: None,
                detail: None,
                replace_range: crate::completion::unused_replace_range(),
                insert_text: "foobaz".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
                server: None,
            },
        ])
    };
    h.stoat.pending_completion = Some(popup());

    h.type_keys("down");
    assert_eq!(h.stoat.pending_completion.as_ref().unwrap().selected_idx, 1,);
    h.type_keys("down");
    assert_eq!(h.stoat.pending_completion.as_ref().unwrap().selected_idx, 2,);
    // Clamps at last index.
    h.type_keys("down");
    assert_eq!(h.stoat.pending_completion.as_ref().unwrap().selected_idx, 2,);

    h.type_keys("up");
    assert_eq!(h.stoat.pending_completion.as_ref().unwrap().selected_idx, 1,);
    h.type_keys("up");
    assert_eq!(h.stoat.pending_completion.as_ref().unwrap().selected_idx, 0,);
    // Saturates at zero.
    h.type_keys("up");
    assert_eq!(h.stoat.pending_completion.as_ref().unwrap().selected_idx, 0,);

    let cursor_after = focused_primary_offsets(&mut h);
    assert_eq!(cursor_before, cursor_after);
}

#[test]
fn up_and_down_with_no_popup_move_cursor() {
    let mut h = Stoat::test();
    let _path = open_scratch_file(&mut h, "first\nsecond\n");
    h.type_keys("i");
    let (start, _) = focused_primary_offsets(&mut h);
    assert_eq!(start, 0);
    h.type_keys("down");
    let (after_down, _) = focused_primary_offsets(&mut h);
    assert!(after_down > 0, "down arrow should advance cursor");
}

#[test]
fn snippet_tabstop_wins_mid_line_only() {
    use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_keys("p r i");
    let replace_range = crate::completion::anchor_range_in_focused(&h.stoat, 0..3);
    h.stoat.pending_completion = Some(CompletionPopup {
        prefix_range: 0..3,
        ..CompletionPopup::showing(vec![CompletionItem {
            label: "fn".into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range,
            insert_text: "${1:name}(${2:arg})$0".into(),
            is_snippet: true,
            documentation: None,
            lsp_item: None,
            server: None,
        }])
    });
    h.type_keys("tab");
    assert_eq!(buffer_text(&h, &path), "name(arg)");
    assert!(h.stoat.active_snippet.is_some(), "a snippet is in flight");

    h.type_keys("home");
    h.type_keys("tab");
    assert_eq!(
        buffer_text(&h, &path),
        "\tname(arg)",
        "in an indent the key indents, snippet or no snippet",
    );
    assert!(
        h.stoat.active_snippet.is_some(),
        "and the snippet is still there to advance",
    );

    h.type_keys("end");
    h.type_keys("tab");
    assert_eq!(
        focused_primary_offsets(&mut h),
        (6, 9),
        "away from the indent the key advances the snippet, arg shifted by the tab",
    );
}

#[test]
fn tab_advances_active_snippet_to_next_tabstop() {
    use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
    let mut h = Stoat::test();
    let path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_keys("p r i");
    let replace_range = crate::completion::anchor_range_in_focused(&h.stoat, 0..3);
    h.stoat.pending_completion = Some(CompletionPopup {
        prefix_range: 0..3,
        ..CompletionPopup::showing(vec![CompletionItem {
            label: "fn".into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range,
            insert_text: "${1:name}(${2:arg})$0".into(),
            is_snippet: true,
            documentation: None,
            lsp_item: None,
            server: None,
        }])
    });
    h.type_keys("tab");
    assert_eq!(buffer_text(&h, &path), "name(arg)");
    let (start, end) = focused_primary_offsets(&mut h);
    assert_eq!((start, end), (0, 4), "first tabstop");
    assert!(h.stoat.active_snippet.is_some());

    h.type_keys("tab");
    let (start, end) = focused_primary_offsets(&mut h);
    assert_eq!((start, end), (5, 8), "second tabstop");
    assert!(h.stoat.active_snippet.is_some());

    h.type_keys("tab");
    let (start, end) = focused_primary_offsets(&mut h);
    assert_eq!((start, end), (9, 9), "exit landed at $0");
    assert!(
        h.stoat.active_snippet.is_none(),
        "snippet exits after final tab",
    );
}

#[test]
fn leaving_insert_mode_clears_active_snippet() {
    use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
    let mut h = Stoat::test();
    let _path = open_scratch_file(&mut h, "");
    h.type_keys("i");
    h.type_keys("f");
    let replace_range = crate::completion::anchor_range_in_focused(&h.stoat, 0..1);
    h.stoat.pending_completion = Some(CompletionPopup {
        prefix_range: 0..1,
        ..CompletionPopup::showing(vec![CompletionItem {
            label: "snippet".into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range,
            insert_text: "${1:a} ${2:b}".into(),
            is_snippet: true,
            documentation: None,
            lsp_item: None,
            server: None,
        }])
    });
    h.type_keys("tab");
    assert!(h.stoat.active_snippet.is_some());

    h.type_keys("escape");
    h.type_keys("escape");
    assert_eq!(h.stoat.focused_mode(), "normal");
    assert!(h.stoat.active_snippet.is_none());
}

fn focused_primary_offsets(h: &mut crate::test_harness::TestHarness) -> (usize, usize) {
    let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
    let ws = h.stoat.active_workspace_mut();
    let editor = ws.editors.get_mut(editor_id).expect("editor exists");
    let snap = editor.display_map.snapshot();
    let buf_snap = snap.buffer_snapshot();
    let sel = editor.selections.newest_anchor();
    (
        buf_snap.resolve_anchor(&sel.start),
        buf_snap.resolve_anchor(&sel.end),
    )
}

fn focused_gutter_width(h: &crate::test_harness::TestHarness) -> u16 {
    let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
    h.stoat
        .active_workspace()
        .editors
        .get(editor_id)
        .expect("editor exists")
        .gutter_width
}

#[test]
fn line_numbers_setting_toggles_the_gutter() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/ln-toggle");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    h.stoat.settings.editor_line_numbers = Some(LineNumbers::Relative);
    h.stoat.render();
    let with_numbers = focused_gutter_width(&h);

    h.stoat.settings.editor_line_numbers = Some(LineNumbers::Off);
    h.stoat.render();
    let without = focused_gutter_width(&h);

    assert!(
        with_numbers > without,
        "line numbers widen the gutter ({with_numbers}) past the \
             diagnostic-only column ({without})"
    );
    assert_eq!(
        without, 0,
        "with no diagnostics and no line numbers there is no gutter"
    );
}

#[test]
fn a_theme_defining_no_colors_pushes_nothing() {
    assert!(
        osc_default_colors(&crate::theme::Theme::empty()).is_empty(),
        "with nothing to say, the terminal's own defaults stand",
    );
}

#[test]
fn editor_page_content_version_tracks_the_cursor_line() {
    let paint = PaintVersion::default();
    let base = editor_page_content_version(
        true,
        3,
        None,
        Some(10),
        0,
        false,
        0,
        0.0,
        DiffDials::shipped(),
        0,
        paint,
        0,
        None,
    );
    assert_eq!(
        base,
        editor_page_content_version(
            true,
            3,
            None,
            Some(10),
            0,
            false,
            0,
            0.0,
            DiffDials::shipped(),
            0,
            paint,
            0,
            None
        ),
        "identical inputs keep a buffered page cached"
    );
    assert_ne!(
        base,
        editor_page_content_version(
            true,
            3,
            None,
            Some(11),
            0,
            false,
            0,
            0.0,
            DiffDials::shipped(),
            0,
            paint,
            0,
            None
        ),
        "a cursor-line move refills buffered pages"
    );
    assert_ne!(
        base,
        editor_page_content_version(
            true,
            3,
            None,
            None,
            0,
            false,
            0,
            0.0,
            DiffDials::shipped(),
            0,
            paint,
            0,
            None
        ),
        "switching to absolute numbering refills"
    );
    assert_ne!(
        base,
        editor_page_content_version(
            true,
            3,
            Some(72),
            Some(10),
            0,
            false,
            0,
            0.0,
            DiffDials::shipped(),
            0,
            paint,
            0,
            None
        ),
        "a wrap-width change refills buffered pages"
    );
    assert_ne!(
        base,
        editor_page_content_version(
            true,
            3,
            None,
            Some(10),
            0,
            true,
            7,
            0.0,
            DiffDials::shipped(),
            0,
            paint,
            0,
            None
        ),
        "a diff-view hunk change refills buffered pages"
    );
    assert_ne!(
        base,
        editor_page_content_version(
            true,
            3,
            None,
            Some(10),
            0,
            false,
            0,
            0.0,
            DiffDials {
                soften_scale: 1.5,
                ..DiffDials::shipped()
            },
            0,
            paint,
            0,
            None
        ),
        "a soften step refills buffered pages rather than gliding stale colors"
    );
    assert_ne!(
        base,
        editor_page_content_version(
            true,
            3,
            None,
            Some(10),
            0,
            false,
            0,
            0.25,
            DiffDials::shipped(),
            0,
            paint,
            0,
            None
        ),
        "a focus change to a dimmed pane refills buffered pages"
    );
    assert_ne!(
        base,
        editor_page_content_version(
            true,
            3,
            None,
            Some(10),
            0,
            false,
            0,
            0.0,
            DiffDials::shipped(),
            1,
            paint,
            0,
            None
        ),
        "a buffer edit refills buffered pages"
    );
    assert_ne!(
        base,
        editor_page_content_version(
            true,
            3,
            None,
            Some(10),
            0,
            false,
            0,
            0.0,
            DiffDials::shipped(),
            0,
            paint,
            1,
            None
        ),
        "a theme switch refills buffered pages"
    );

    let lit = |start: usize| {
        let spotlight = Spotlight {
            range: start..=start + 4,
            color: [1, 2, 3],
            dim: 0.5,
        };
        editor_page_content_version(
            true,
            3,
            None,
            Some(10),
            0,
            false,
            0,
            0.0,
            DiffDials::shipped(),
            0,
            paint,
            0,
            Some(&spotlight),
        )
    };
    assert_ne!(
        base,
        lit(6),
        "a walkthrough spotlight coming on refills buffered pages"
    );
    assert_eq!(
        lit(6),
        lit(12),
        "one moving to another annotation leaves the pool's version, since each \
             page carries where it lights in its own"
    );
}

/// A pooled page and the minimap thumb both hold cached work against the
/// display map, so anything that relaid the rows has to move what they key
/// on.
///
/// The file is deliberately plain, outside git and with no diagnostics, so
/// the diff and severity versions the page hash also carries stay at rest.
/// That leaves the mapping as the only input left to catch the change,
/// which is the case a glide over an edited file composited stale text in.
#[test]
fn an_edit_or_a_fold_refills_pooled_pages_and_the_minimap_window() {
    use crate::{display_map::DisplayMap, test_harness::TestHarness};
    use stoat_text::Point;

    fn focused_display_map(h: &mut TestHarness) -> &mut DisplayMap {
        let (editor_id, _) = h.stoat.focused_editor_ids().expect("focused editor");
        &mut h
            .stoat
            .active_workspace_mut()
            .editors
            .get_mut(editor_id)
            .expect("editor exists")
            .display_map
    }

    /// The page content version and the minimap window stamp, read the way
    /// `emit_smooth_scroll` reads them. Every other page input is held
    /// still, so only the mapping is under test.
    fn keys(h: &mut TestHarness) -> (u64, u64) {
        let map = focused_display_map(h);
        let buffer_version = map.buffer_snapshot().version();
        let paint_version = map.snapshot().paint_version();
        (
            editor_page_content_version(
                true,
                3,
                None,
                None,
                0,
                false,
                0,
                0.0,
                DiffDials::shipped(),
                buffer_version,
                paint_version,
                0,
                None,
            ),
            display_map_stamp(buffer_version, paint_version),
        )
    }

    let mut h = TestHarness::with_size(40, 10);
    let path = h.write_file("plain.txt", "one\ntwo\nthree\nfour\n");
    h.open_file(&path);

    let (page_at_open, window_at_open) = keys(&mut h);
    h.seed_focused_buffer("x");
    let (page_after_edit, window_after_edit) = keys(&mut h);
    assert_ne!(
        page_at_open, page_after_edit,
        "a typed character refills the buffered pages"
    );
    assert_ne!(
        window_at_open, window_after_edit,
        "and re-derives the minimap window"
    );

    focused_display_map(&mut h).fold(vec![Point::new(0, 0)..Point::new(2, 0)]);
    let (page_after_fold, window_after_fold) = keys(&mut h);
    assert_ne!(
        page_after_edit, page_after_fold,
        "and a fold still refills them"
    );
    assert_ne!(
        window_after_edit, window_after_fold,
        "and re-derives the window too"
    );
}

#[test]
fn editor_mouse_down_lands_block_cursor_at_clicked_offset() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "abcdef\nghi");
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + 3,
        area.y,
    ));
    assert_eq!(focused_primary_offsets(&mut h), (3, 4));
    assert!(h.stoat.editor_drag.is_some(), "drag state armed");
}

/// Clicking any cell a joined sequence is drawn over lands on the whole
/// sequence.
///
/// The display clip walks codepoints, so an interior cell resolves to a
/// byte offset inside the sequence. Landing there would leave the cursor
/// covering only its tail, and a delete would strand the codepoints before
/// it.
#[test]
fn editor_mouse_down_inside_a_cluster_lands_on_the_whole_cluster() {
    // Five codepoints over eighteen bytes, rendered across six cells.
    for cell in 1..6 {
        let mut h = Stoat::test();
        let _ = open_scratch_file(&mut h, &format!("{FAMILY}b\n"));
        let area = focused_editor_pane_area(&h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + cell,
            area.y,
        ));
        assert_eq!(
            focused_primary_offsets(&mut h),
            (0, 18),
            "a click on cell {cell} covers the whole sequence",
        );
    }
}

/// Open a location picker over `count` candidates in a seeded file, each
/// row's text naming its 1-based position, and return the file's path.
fn open_location_picker(h: &mut crate::test_harness::TestHarness, count: usize) -> PathBuf {
    use crate::location_picker::LocationEntry;

    let root = PathBuf::from("/loc-picker");
    let path = root.join("target.rs");
    let text = "line\n".repeat(count.max(1));
    h.fake_fs()
        .insert_files(std::iter::once((path.clone(), text.as_bytes())));
    h.stoat.active_workspace_mut().git_root = root;

    let entries = (0..count)
        .map(|i| LocationEntry {
            path: path.clone(),
            offset: i * 5,
            line: i as u32 + 1,
            column: 1,
            text: format!("candidate-{}", i + 1),
        })
        .collect();
    h.stoat.location_picker = Some(action_handlers::lsp::open_location_picker(
        &mut h.stoat,
        entries,
    ));
    h.snapshot();
    path
}

/// The location picker's inner rows rect for the harness' screen size.
fn location_picker_rows(h: &crate::test_harness::TestHarness) -> Rect {
    crate::render::picker::target_picker_layout(
        h.stoat.size(),
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::LocationPicker),
        modal_split_percent(&h.stoat.modal_split, ModalKind::LocationPicker),
    )
    .expect("picker laid out")
    .list
}

/// The picker prompts like every other target list, so typing narrows the
/// candidates rather than reaching the buffer behind it.
#[test]
fn typing_narrows_the_location_candidates() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    open_location_picker(&mut h, 12);
    let all = h
        .stoat
        .location_picker
        .as_ref()
        .expect("open")
        .filtered()
        .len();

    h.type_text("candidate-7");
    h.stoat.drive_background();

    let narrowed = h
        .stoat
        .location_picker
        .as_ref()
        .expect("the picker stays open while typing")
        .filtered()
        .len();
    assert_eq!(
        (all, narrowed),
        (12, 1),
        "the query keeps only the candidate it names"
    );
}

/// The preview shows the selected candidate's file, scrolled to the line it
/// points at, so a reader sees the target before committing to the jump.
#[test]
fn the_location_preview_follows_the_selection() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    open_location_picker(&mut h, 40);
    h.stoat.drive_background();
    h.snapshot();

    let preview_editor = h
        .stoat
        .location_picker
        .as_ref()
        .expect("open")
        .preview
        .editor;
    let scroll = |h: &crate::test_harness::TestHarness| {
        h.stoat
            .active_workspace()
            .editors
            .get(preview_editor)
            .expect("preview editor")
            .scroll_row
    };
    let before = scroll(&h);

    h.stoat
        .location_picker
        .as_mut()
        .expect("open")
        .move_selection(39);
    h.stoat.drive_background();
    h.snapshot();

    assert!(
        scroll(&h) > before,
        "the preview scrolled toward the last candidate's line"
    );
}

/// Escape closes the picker and releases the editors it owns, so a closed
/// picker leaves no scratch buffers behind.
#[test]
fn escape_closes_the_location_picker_and_disposes_it() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    let before = h.stoat.active_workspace().editors.len();
    open_location_picker(&mut h, 4);
    assert!(
        h.stoat.active_workspace().editors.len() > before,
        "the picker took editors for its prompt and preview"
    );

    h.type_keys("escape");

    assert_eq!(
        (
            h.stoat.location_picker.is_some(),
            h.stoat.active_workspace().editors.len()
        ),
        (false, before),
        "closing gives back every editor the picker took"
    );
}

#[test]
fn location_picker_click_selects_the_clicked_row() {
    let mut h = Stoat::test();
    open_location_picker(&mut h, 4);
    let rows = location_picker_rows(&h);

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        rows.x + 2,
        rows.y + 1,
    ));

    let picker = h.stoat.location_picker.as_ref().expect("picker still open");
    assert_eq!(
        picker.selected(),
        1,
        "the second row is selected, not jumped"
    );
}

#[test]
fn location_picker_click_on_the_selected_row_jumps() {
    let mut h = Stoat::test();
    let path = open_location_picker(&mut h, 4);
    let rows = location_picker_rows(&h);

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        rows.x + 2,
        rows.y + 1,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        rows.x + 2,
        rows.y + 1,
    ));
    h.settle();

    assert!(h.stoat.location_picker.is_none(), "the picker closed");
    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.id_for_path(&path).expect("target opened");
    assert_eq!(
        h.stoat.focused_editor_ids().map(|(editor_id, _)| {
            h.stoat
                .active_workspace()
                .editors
                .get(editor_id)
                .expect("editor")
                .buffer_id
        }),
        Some(buffer),
        "the jump landed in the candidate's file"
    );
}

#[test]
fn location_picker_wheel_moves_the_selection() {
    let mut h = Stoat::test();
    open_location_picker(&mut h, 4);

    h.stoat
        .update(mouse_event(MouseEventKind::ScrollDown, 1, 1));
    assert_eq!(
        h.stoat.location_picker.as_ref().expect("open").selected(),
        1
    );

    h.stoat.update(mouse_event(MouseEventKind::ScrollUp, 1, 1));
    assert_eq!(
        h.stoat.location_picker.as_ref().expect("open").selected(),
        0
    );
}

#[test]
fn location_picker_click_outside_is_swallowed() {
    let mut h = Stoat::test();
    open_location_picker(&mut h, 4);
    let rows = location_picker_rows(&h);
    let before = focused_primary_offsets(&mut h);

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        rows.x,
        rows.y.saturating_sub(2),
    ));

    assert_eq!(
        h.stoat.location_picker.as_ref().expect("open").selected(),
        0,
        "a click outside the rows changes nothing"
    );
    assert_eq!(
        focused_primary_offsets(&mut h),
        before,
        "the buffer beneath keeps its cursor"
    );
}

#[test]
fn location_picker_renders_a_selection_past_the_visible_rows() {
    // Wider than the default terminal so the hints overlay, which is
    // right-aligned and sized by the bindings it lists, sits clear of the
    // centered modal. At 80 columns it covers the rows this reads.
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    // More candidates than the box shows, so the window has to scroll to
    // reach the last one.
    open_location_picker(&mut h, 60);
    h.stoat
        .location_picker
        .as_mut()
        .expect("open")
        .move_selection(59);
    h.snapshot();

    // Rows are read individually rather than by scanning the whole frame.
    // The key hints overlay paints across the right end of the bottom rows.
    let rows = location_picker_rows(&h);
    let row_text = |row: u16| -> String {
        let buf = h.rendered_buffer();
        (rows.x..rows.x + rows.width)
            .map(|col| buf[(col, row)].symbol())
            .collect()
    };
    let last_row = rows.y + rows.height - 1;

    assert!(
        !row_text(rows.y).contains("candidate-1 "),
        "the window scrolled the first candidates off: {}",
        row_text(rows.y)
    );
    assert!(
        row_text(last_row).contains("60:1"),
        "the selected 60th candidate paints on the last row: {}",
        row_text(last_row)
    );

    let selection = h.stoat.theme.get(crate::theme::scope::UI_SELECTION);
    assert_eq!(
        h.rendered_buffer()[(rows.x + 1, last_row)].style().bg,
        selection.bg,
        "the selected candidate is painted as selected"
    );
}

/// Open a workspace-scope diagnostics picker over `count` diagnostics, one
/// per line of a seeded file, each message naming its 1-based line so a
/// painted row is identifiable. Returns the file's path.
fn open_diagnostics_picker(h: &mut crate::test_harness::TestHarness, count: u32) -> PathBuf {
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

    let root = PathBuf::from("/diag-picker");
    let path = root.join("target.rs");
    let text: String = (0..count.max(1)).map(|i| format!("line {i}\n")).collect();
    h.fake_fs()
        .insert_files(std::iter::once((path.clone(), text.as_bytes())));
    h.stoat.active_workspace_mut().git_root = root;

    let diagnostics = (0..count)
        .map(|line| {
            let position = Position { line, character: 0 };
            Diagnostic {
                range: Range {
                    start: position,
                    end: position,
                },
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("diagnostic-{}", line + 1),
                ..Diagnostic::default()
            }
        })
        .collect();
    h.stoat
        .diagnostics
        .replace_for_path(path.clone(), diagnostics);

    action_handlers::dispatch(
        &mut h.stoat,
        &stoat_action::OpenWorkspaceDiagnosticsPicker {},
    );
    h.snapshot();
    path
}

/// The diagnostics picker's inner rows rect for the harness' screen size.
fn diagnostics_picker_rows(h: &crate::test_harness::TestHarness) -> Rect {
    crate::render::picker::target_picker_layout(
        h.stoat.size(),
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::DiagnosticsPicker),
        modal_split_percent(&h.stoat.modal_split, ModalKind::DiagnosticsPicker),
    )
    .expect("picker laid out")
    .list
}

/// The picker prompts like every other target list, so typing narrows the
/// diagnostics rather than reaching the buffer behind it.
#[test]
fn typing_narrows_the_diagnostics() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    open_diagnostics_picker(&mut h, 12);
    let all = h
        .stoat
        .diagnostics_picker
        .as_ref()
        .expect("open")
        .filtered()
        .len();

    h.type_text("diagnostic-7");
    h.stoat.drive_background();

    let narrowed = h
        .stoat
        .diagnostics_picker
        .as_ref()
        .expect("the picker stays open while typing")
        .filtered()
        .len();
    assert_eq!(
        (all, narrowed),
        (12, 1),
        "the query keeps only the diagnostic it names"
    );
}

/// The preview shows the selected diagnostic's file, scrolled to the line it
/// points at, so a reader sees the offending code before jumping to it.
#[test]
fn the_diagnostics_preview_follows_the_selection() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    open_diagnostics_picker(&mut h, 40);
    h.stoat.drive_background();
    h.snapshot();

    let preview_editor = h
        .stoat
        .diagnostics_picker
        .as_ref()
        .expect("open")
        .picker
        .preview
        .editor;
    let scroll = |h: &crate::test_harness::TestHarness| {
        h.stoat
            .active_workspace()
            .editors
            .get(preview_editor)
            .expect("preview editor")
            .scroll_row
    };
    let before = scroll(&h);

    h.stoat
        .diagnostics_picker
        .as_mut()
        .expect("open")
        .picker
        .move_selection(39);
    h.stoat.drive_background();
    h.snapshot();

    assert!(
        scroll(&h) > before,
        "the preview scrolled toward the last diagnostic's line"
    );
}

/// Escape closes the picker and releases the editors it owns, so a closed
/// picker leaves no scratch buffers behind.
#[test]
fn escape_closes_the_diagnostics_picker_and_disposes_it() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    let before = h.stoat.active_workspace().editors.len();
    open_diagnostics_picker(&mut h, 4);
    assert!(
        h.stoat.active_workspace().editors.len() > before,
        "the picker took editors for its prompt and preview"
    );

    h.type_keys("escape");

    assert_eq!(
        (
            h.stoat.diagnostics_picker.is_some(),
            h.stoat.active_workspace().editors.len()
        ),
        (false, before),
        "closing gives back every editor the picker took"
    );
}

/// A page covers half the rows the render stamped, and stops at each end
/// rather than wrapping.
#[test]
fn diagnostics_picker_pages_by_half_a_screen_and_stops_at_the_ends() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    open_diagnostics_picker(&mut h, 20);

    let half = h
        .stoat
        .diagnostics_picker
        .as_ref()
        .expect("open")
        .picker
        .viewport_rows
        .expect("the render stamped a viewport")
        / 2;
    assert!(
        half > 1,
        "a meaningful page needs more than one row: {half}"
    );

    let page = |h: &mut crate::test_harness::TestHarness, dir: i32| {
        h.stoat.diagnostics_picker.as_mut().expect("open").page(dir);
    };
    let selected = |h: &crate::test_harness::TestHarness| {
        h.stoat
            .diagnostics_picker
            .as_ref()
            .expect("open")
            .selected()
    };

    page(&mut h, 1);
    assert_eq!(selected(&h), half, "a page down covers half a screen");
    page(&mut h, -1);
    assert_eq!(selected(&h), 0, "and a page up returns");

    for _ in 0..20 {
        page(&mut h, 1);
    }
    assert_eq!(selected(&h), 19, "paging past the end stops on it");
    for _ in 0..20 {
        page(&mut h, -1);
    }
    assert_eq!(selected(&h), 0, "and past the start stops there");
}

/// More diagnostics than the box shows means the window has to scroll to
/// reach the last one, and the row it lands on paints as selected.
#[test]
fn diagnostics_picker_renders_a_selection_past_the_visible_rows() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    open_diagnostics_picker(&mut h, 60);
    h.stoat
        .diagnostics_picker
        .as_mut()
        .expect("open")
        .picker
        .move_selection(59);
    h.snapshot();

    // Rows are read individually rather than by scanning the whole frame.
    // The key hints overlay paints across the right end of the bottom rows.
    let rows = diagnostics_picker_rows(&h);
    let row_text = |row: u16| -> String {
        let buf = h.rendered_buffer();
        (rows.x..rows.x + rows.width)
            .map(|col| buf[(col, row)].symbol())
            .collect()
    };
    let last_row = rows.y + rows.height - 1;

    assert!(
        !row_text(rows.y).contains("diagnostic-1 "),
        "the window scrolled the first diagnostics off: {}",
        row_text(rows.y)
    );
    assert!(
        row_text(last_row).contains("diagnostic-60"),
        "the selected 60th diagnostic paints on the last row: {}",
        row_text(last_row)
    );

    let selection = h.stoat.theme.get(crate::theme::scope::UI_SELECTION);
    assert_eq!(
        h.rendered_buffer()[(rows.x + 1, last_row)].style().bg,
        selection.bg,
        "the selected diagnostic is painted as selected"
    );
}

/// Open a jumplist picker over `count` jumps into a seeded file, one per
/// line, each line naming its own 1-based number so a row is identifiable.
///
/// The list holds `count + 1` rows: opening the file records a jump out of
/// the scratch buffer the harness starts on.
fn open_jumplist_picker(h: &mut crate::test_harness::TestHarness, count: usize) -> PathBuf {
    let root = PathBuf::from("/jump-picker");
    let path = root.join("target.rs");
    let text: String = (1..=count)
        .map(|i| format!("jump-{i} lands here\n"))
        .collect();
    h.fake_fs()
        .insert_files(std::iter::once((path.clone(), text.as_bytes())));
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();

    for _ in 0..count {
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveDown);
    }
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenJumplistPicker);
    h.snapshot();
    path
}

/// The jumplist picker's inner rows rect for the harness' screen size.
fn jumplist_picker_rows(h: &crate::test_harness::TestHarness) -> Rect {
    crate::render::picker::target_picker_layout(
        h.stoat.size(),
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::JumplistPicker),
        modal_split_percent(&h.stoat.modal_split, ModalKind::JumplistPicker),
    )
    .expect("picker laid out")
    .list
}
/// The picker prompts like every other target list, so typing narrows the
/// jumps rather than reaching the buffer behind it.
#[test]
fn typing_narrows_the_jumps() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    let jumps = 12;
    open_jumplist_picker(&mut h, jumps);
    let all = h
        .stoat
        .jumplist_picker
        .as_ref()
        .expect("open")
        .filtered()
        .len();

    h.type_text("jump-7");
    h.stoat.drive_background();

    let narrowed = h
        .stoat
        .jumplist_picker
        .as_ref()
        .expect("the picker stays open while typing")
        .filtered()
        .len();
    // The list holds the fixture's jumps and nothing else: the pane opened
    // from a scratch it left behind, which goes with the switch.
    assert_eq!(
        (all, narrowed),
        (jumps, 1),
        "the query keeps only the jump it names"
    );
}

/// The preview shows the selected jump's buffer, scrolled to the line it
/// points at, so a reader sees the target before committing to the jump.
#[test]
fn the_jumplist_preview_follows_the_selection() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    open_jumplist_picker(&mut h, 40);
    h.stoat
        .jumplist_picker
        .as_mut()
        .expect("open")
        .picker
        .move_selection(-39);
    h.stoat.drive_background();
    h.snapshot();

    let preview_editor = h
        .stoat
        .jumplist_picker
        .as_ref()
        .expect("open")
        .picker
        .preview
        .editor;
    let scroll = |h: &crate::test_harness::TestHarness| {
        h.stoat
            .active_workspace()
            .editors
            .get(preview_editor)
            .expect("preview editor")
            .scroll_row
    };
    let before = scroll(&h);

    h.stoat
        .jumplist_picker
        .as_mut()
        .expect("open")
        .picker
        .move_selection(39);
    h.stoat.drive_background();
    h.snapshot();

    assert!(
        scroll(&h) > before,
        "the preview scrolled toward the last jump's line"
    );
}

/// Escape closes the picker and releases the editors it owns, so a closed
/// picker leaves no scratch buffers behind.
#[test]
fn escape_closes_the_jumplist_picker_and_disposes_it() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    open_jumplist_picker(&mut h, 4);
    let before = h.stoat.active_workspace().editors.len();
    assert!(
        before > 1,
        "the picker took editors for its prompt and preview"
    );

    h.type_keys("escape");

    assert_eq!(
        (
            h.stoat.jumplist_picker.is_some(),
            h.stoat.active_workspace().editors.len()
        ),
        (false, before - 2),
        "closing gives back the prompt and preview editors"
    );
}

/// A page covers half the rows the render stamped, and stops at each end
/// rather than wrapping.
#[test]
fn jumplist_picker_pages_by_half_a_screen_and_stops_at_the_ends() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    open_jumplist_picker(&mut h, 19);

    let half = h
        .stoat
        .jumplist_picker
        .as_ref()
        .expect("open")
        .picker
        .viewport_rows
        .expect("the render stamped a viewport")
        / 2;
    assert!(
        half > 1,
        "a meaningful page needs more than one row: {half}"
    );

    let page = |h: &mut crate::test_harness::TestHarness, dir: i32| {
        h.stoat.jumplist_picker.as_mut().expect("open").page(dir);
    };
    let selected = |h: &crate::test_harness::TestHarness| {
        h.stoat.jumplist_picker.as_ref().expect("open").selected()
    };

    // The picker opens on the walk cursor, so paging starts from a known row.
    h.stoat
        .jumplist_picker
        .as_mut()
        .expect("open")
        .picker
        .move_selection(-20);
    assert_eq!(selected(&h), 0, "the selection starts at the first row");

    page(&mut h, 1);
    assert_eq!(selected(&h), half, "a page down covers half a screen");
    page(&mut h, -1);
    assert_eq!(selected(&h), 0, "and a page up returns");

    // Read rather than stated, since the jumps the fixture makes are what
    // the list holds and the pane it opened from leaves nothing behind.
    let last = h
        .stoat
        .jumplist_picker
        .as_ref()
        .expect("open")
        .filtered()
        .len()
        - 1;
    for _ in 0..20 {
        page(&mut h, 1);
    }
    assert_eq!(selected(&h), last, "paging past the end stops on it");
    for _ in 0..20 {
        page(&mut h, -1);
    }
    assert_eq!(selected(&h), 0, "and past the start stops there");
}

/// More jumps than the box shows means the window has to scroll to reach
/// the last one, and the row it lands on paints as selected.
///
/// The terminal is short on purpose: the jumplist caps at
/// [`JUMP_LIST_CAPACITY`](crate::jumplist) entries, so a full-height box
/// would hold every row it can ever have and never scroll.
#[test]
fn jumplist_picker_renders_a_selection_past_the_visible_rows() {
    let mut h = crate::test_harness::TestHarness::with_size(120, 26);
    open_jumplist_picker(&mut h, 40);

    let rows = jumplist_picker_rows(&h);
    let entries = h
        .stoat
        .jumplist_picker
        .as_ref()
        .expect("open")
        .entries()
        .len();
    assert!(
        entries > rows.height as usize,
        "the list has to outgrow its box: {entries} entries in {} rows",
        rows.height
    );

    h.stoat
        .jumplist_picker
        .as_mut()
        .expect("open")
        .picker
        .move_selection(entries as i32);
    h.snapshot();

    // Rows are read individually rather than by scanning the whole frame.
    // The key hints overlay paints across the right end of the bottom rows.
    let row_text = |row: u16| -> String {
        let buf = h.rendered_buffer();
        (rows.x..rows.x + rows.width)
            .map(|col| buf[(col, row)].symbol())
            .collect()
    };
    let last_row = rows.y + rows.height - 1;

    assert!(
        !row_text(rows.y).contains("jump-11 "),
        "the window scrolled the oldest surviving jump off: {}",
        row_text(rows.y)
    );
    assert!(
        row_text(last_row).contains("jump-40"),
        "the selected newest jump paints on the last row: {}",
        row_text(last_row)
    );

    let selection = h.stoat.theme.get(crate::theme::scope::UI_SELECTION);
    assert_eq!(
        h.rendered_buffer()[(rows.x + 1, last_row)].style().bg,
        selection.bg,
        "the selected jump is painted as selected"
    );
}

/// The rows follow the box, so a list longer than the box paints inside it
/// rather than spilling the surplus onto the editor behind.
///
/// The terminal is short on purpose, for the same reason the scrolling test
/// above uses one: a full-height box never overflows a capped jumplist.
#[test]
fn the_jumplist_box_paints_nothing_outside_itself() {
    let mut h = crate::test_harness::TestHarness::with_size(120, 26);
    open_jumplist_picker(&mut h, 40);

    let modal = crate::render::picker::target_picker_layout(
        h.stoat.size(),
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::JumplistPicker),
        modal_split_percent(&h.stoat.modal_split, ModalKind::JumplistPicker),
    )
    .expect("picker laid out")
    .modal;

    let before = h.rendered_buffer().clone();
    h.stoat
        .jumplist_picker
        .as_mut()
        .expect("open")
        .picker
        .move_selection(40);
    h.snapshot();
    let after = h.rendered_buffer();

    let changed: Vec<(u16, u16)> = (0..26u16)
        .flat_map(|y| (0..120u16).map(move |x| (x, y)))
        .filter(|&(x, y)| !modal.contains((x, y).into()))
        .filter(|&(x, y)| after[(x, y)] != before[(x, y)])
        .collect();
    assert_eq!(
        changed,
        Vec::new(),
        "scrolling the list repaints only cells inside the box"
    );
}

/// The preview editor behind the open location picker, and its scroll row.
fn location_preview_scroll(h: &crate::test_harness::TestHarness) -> u32 {
    let editor = h
        .stoat
        .location_picker
        .as_ref()
        .expect("open")
        .preview
        .editor;
    h.stoat
        .active_workspace()
        .editors
        .get(editor)
        .expect("preview editor")
        .scroll_row
}

/// Ctrl-d is bound over every target picker, so it has to reach the pane it
/// names. The three converted pickers were bound before the handler knew
/// which editor backs their preview.
#[test]
fn ctrl_d_scrolls_the_location_preview() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    open_location_picker(&mut h, 200);
    h.stoat.drive_background();
    h.snapshot();
    let before = location_preview_scroll(&h);

    h.type_keys("ctrl-d");
    h.snapshot();

    assert!(
        location_preview_scroll(&h) > before,
        "Ctrl-d scrolled the preview down from {before}"
    );
}

/// A wheel over the preview scrolls that pane, the same as over any editor.
#[test]
fn a_wheel_over_the_diagnostics_preview_scrolls_it() {
    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    open_diagnostics_picker(&mut h, 200);
    h.stoat.drive_background();
    h.snapshot();

    let preview = crate::render::picker::target_picker_layout(
        h.stoat.size(),
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::DiagnosticsPicker),
        modal_split_percent(&h.stoat.modal_split, ModalKind::DiagnosticsPicker),
    )
    .expect("picker laid out")
    .preview
    .expect("the terminal is wide enough for a preview");

    let scroll = |h: &crate::test_harness::TestHarness| {
        let editor = h
            .stoat
            .diagnostics_picker
            .as_ref()
            .expect("open")
            .picker
            .preview
            .editor;
        h.stoat
            .active_workspace()
            .editors
            .get(editor)
            .expect("preview editor")
            .scroll_row
    };
    let before = scroll(&h);

    h.stoat.update(mouse_event(
        MouseEventKind::ScrollDown,
        preview.x + 2,
        preview.y + 2,
    ));
    h.snapshot();

    assert!(
        scroll(&h) > before,
        "the wheel scrolled the preview down from {before}"
    );
}
/// Seed a definition-capable fake server, open `main.rs` holding
/// `abc\ndef\nghi\n`, and return its path.
fn open_file_with_lsp(h: &mut crate::test_harness::TestHarness) -> PathBuf {
    use lsp_types::{OneOf, ServerCapabilities};

    h.fake_lsp().set_capabilities(ServerCapabilities {
        definition_provider: Some(OneOf::Left(true)),
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
        ..Default::default()
    });

    let root = PathBuf::from("/mouse-lsp");
    let path = root.join("main.rs");
    h.fake_fs().insert_files(std::iter::once((
        path.clone(),
        b"abc\ndef\nghi\n".as_slice(),
    )));
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    path
}

#[test]
fn middle_click_jumps_to_the_clicked_symbols_definition() {
    let mut h = Stoat::test();
    let path = open_file_with_lsp(&mut h);
    let uri = path.to_str().expect("utf8 path");
    h.fake_lsp().set_definition(uri, 1, 1, uri, 2, 0);

    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Middle),
        area.x + 1,
        area.y + 1,
    ));
    h.settle();

    assert!(
        h.stoat.location_picker.is_none(),
        "a single target skips the picker"
    );
    assert_eq!(
        focused_primary_offsets(&mut h),
        (8, 9),
        "the jump landed on line 3, so the request read the clicked cell"
    );
}

#[test]
fn middle_click_with_several_definitions_opens_the_picker() {
    let mut h = Stoat::test();
    let path = open_file_with_lsp(&mut h);
    let uri = path.to_str().expect("utf8 path");
    h.fake_lsp()
        .set_definitions(uri, 0, 0, &[(uri, 1, 0), (uri, 2, 0)]);

    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Middle),
        area.x,
        area.y,
    ));
    h.settle();

    let picker = h.stoat.location_picker.as_ref().expect("picker open");
    assert_eq!(picker.entries().len(), 2);
}

#[test]
fn right_click_requests_hover_at_the_clicked_cell() {
    let mut h = Stoat::test();
    open_file_with_lsp(&mut h);

    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Right),
        area.x + 2,
        area.y + 1,
    ));

    assert_eq!(
        focused_primary_offsets(&mut h),
        (6, 7),
        "hover reads the cursor, which the click moved to the clicked cell"
    );
    assert!(h.stoat.pending_hover_request.is_some(), "hover requested");
    assert!(
        h.stoat.editor_drag.is_none(),
        "a right click is not a selection gesture"
    );
}

#[test]
fn middle_click_focuses_the_pane_it_lands_in() {
    let mut h = Stoat::test();
    open_file_with_lsp(&mut h);
    let left_pane = {
        let ws = h.stoat.active_workspace_mut();
        let left_pane = ws.panes.focus();
        let right_pane = ws.panes.split(crate::pane::Axis::Vertical);
        ws.panes.set_focus(right_pane);
        ws.panes.pane_mut(left_pane).area = Rect::new(0, 0, 40, 24);
        ws.panes.pane_mut(right_pane).area = Rect::new(40, 0, 40, 24);
        left_pane
    };

    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Middle), 5, 5));

    assert_eq!(
        h.stoat.active_workspace().panes.focus(),
        left_pane,
        "the click focuses the pane under the pointer before dispatching"
    );
}

#[test]
fn editor_mouse_down_below_last_line_lands_on_the_buffer_end() {
    let mut h = Stoat::test();
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x,
        area.y + 3,
    ));
    assert_eq!(
        focused_primary_offsets(&mut h),
        (0, 0),
        "a click past the content clamps to the end of the empty scratch"
    );
}

#[test]
fn editor_click_excludes_the_diagnostic_gutter() {
    // The no-gutter path (no render leaves gutter_width zero) is covered by
    // editor_mouse_down_lands_block_cursor_at_clicked_offset above.
    let mut h = Stoat::test();
    let root = PathBuf::from("/gutter-click");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"abcdef\nghi\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    h.seed_diagnostics(
        path,
        vec![lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: String::new(),
            ..Default::default()
        }],
    );
    h.stoat.render();

    let gutter_w = focused_gutter_width(&h);
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + gutter_w + 2,
        area.y,
    ));
    assert_eq!(
        focused_primary_offsets(&mut h),
        (2, 3),
        "the line-number gutter shifts text right, so the click excludes it"
    );
}

#[test]
fn editor_mouse_drag_extends_selection_forward() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "abcdef\nghi");
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + 1,
        area.y,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        area.x + 5,
        area.y,
    ));
    assert_eq!(focused_primary_offsets(&mut h), (1, 5));
}

#[test]
fn editor_mouse_drag_extends_selection_backward_reverses() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "abcdef\nghi");
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + 5,
        area.y,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        area.x + 1,
        area.y,
    ));
    assert_eq!(focused_primary_offsets(&mut h), (1, 5));
}

#[test]
fn editor_mouse_click_outside_pane_text_is_noop() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "abc");
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + area.width + 4,
        area.y,
    ));
    assert!(
        h.stoat.editor_drag.is_none(),
        "click past pane right edge does not arm drag",
    );
}

#[test]
fn editor_mouse_up_clears_drag_state() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "abcdef");
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + 2,
        area.y,
    ));
    assert!(h.stoat.editor_drag.is_some());
    h.stoat.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        area.x + 2,
        area.y,
    ));
    assert!(h.stoat.editor_drag.is_none(), "Up clears drag state");
}

#[test]
fn editor_mouse_up_after_drag_writes_selection_to_clipboard() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "hello\nworld");
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + 1,
        area.y,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        area.x + 4,
        area.y,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        area.x + 4,
        area.y,
    ));
    assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
}

/// A terminal in any-motion tracking reports a drag per pointer motion, not
/// per cell, so most of a sweep's events land where the last one did. The
/// one that moves the head paints. The repeats behind it have nothing to
/// show and must not cost a frame. Releasing still copies, which is what
/// says the drag itself was left alone.
#[test]
fn a_repeated_editor_drag_on_the_settled_cell_costs_no_frame() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "hello\nworld");
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + 1,
        area.y,
    ));

    let moved = h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        area.x + 4,
        area.y,
    ));
    let repeat = h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        area.x + 4,
        area.y,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        area.x + 4,
        area.y,
    ));

    assert_eq!(moved, UpdateEffect::Redraw, "the head moved, so repaint");
    assert_eq!(repeat, UpdateEffect::None, "nothing moved, so no repaint");
    assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
}

#[test]
fn editor_mouse_up_without_drag_skips_clipboard() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "hello\nworld");
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + 2,
        area.y,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        area.x + 2,
        area.y,
    ));
    assert!(h.fake_clipboard().writes().is_empty());
}

#[test]
fn editor_mouse_up_with_no_selection_skips_clipboard() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "hello\nworld");
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        area.x + 2,
        area.y,
    ));
    assert!(h.fake_clipboard().writes().is_empty());
}

#[test]
fn editor_mouse_up_multi_line_drag_writes_joined_text() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "hello\nworld");
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + 2,
        area.y,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        area.x + 2,
        area.y + 1,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        area.x + 2,
        area.y + 1,
    ));
    assert_eq!(h.fake_clipboard().writes(), vec!["llo\nwo"]);
}

#[test]
fn editor_osc52_emit_fires_in_ssh_without_mux() {
    let mut h = Stoat::test();
    h.fake_env().set("SSH_CONNECTION", "1.2.3.4 22 5.6.7.8 22");
    let _ = open_scratch_file(&mut h, "hello\nworld");
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + 1,
        area.y,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        area.x + 4,
        area.y,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        area.x + 4,
        area.y,
    ));
    assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
    assert_eq!(h.fake_clipboard().osc52_emits(), vec!["ell"]);
}

#[test]
fn editor_osc52_emit_skipped_locally() {
    let mut h = Stoat::test();
    let _ = open_scratch_file(&mut h, "hello\nworld");
    let area = focused_editor_pane_area(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + 1,
        area.y,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        area.x + 4,
        area.y,
    ));
    h.stoat.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        area.x + 4,
        area.y,
    ));
    assert_eq!(h.fake_clipboard().writes(), vec!["ell"]);
    assert!(h.fake_clipboard().osc52_emits().is_empty());
}

#[test]
fn agent_event_drives_owning_workspace_status() {
    let mut h = Stoat::test();
    let uid = h.stoat.active_workspace().uid;

    let effect = h.stoat.handle_agent_event(AgentEvent {
        uid,
        event: AgentHookEvent::PreToolUse {
            tool: "Bash".into(),
        },
    });

    assert_eq!(effect, UpdateEffect::Redraw);
    let label = h
        .stoat
        .active_workspace()
        .agent
        .as_ref()
        .and_then(|status| status.badge())
        .map(|badge| badge.label);
    assert_eq!(label, Some("claude: Bash".to_string()));
}

#[test]
fn agent_event_for_unknown_session_is_ignored() {
    let mut h = Stoat::test();

    let effect = h.stoat.handle_agent_event(AgentEvent {
        uid: WorkspaceUid(0xdead_beef),
        event: AgentHookEvent::SessionStart,
    });

    assert_eq!(effect, UpdateEffect::None);
    assert!(h.stoat.active_workspace().agent.is_none());
}

fn open_agent_editor(
    h: &mut crate::test_harness::TestHarness,
) -> (BufferId, tokio::sync::oneshot::Receiver<()>) {
    let root = PathBuf::from("/bridge");
    let path = root.join("msg.txt");
    h.fake_fs().insert_file(&path, b"draft\n");
    h.stoat.active_workspace_mut().git_root = root;
    let uid = h.stoat.active_workspace().uid;

    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let effect = h.stoat.handle_agent_control(AgentControl::OpenEditor {
        uid,
        path,
        done: done_tx,
    });
    h.settle();

    assert_eq!(effect, UpdateEffect::Redraw);
    let buffer_id = action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .buffer_id;
    (buffer_id, done_rx)
}

#[test]
fn agent_open_editor_waiter_fires_on_buffer_close() {
    let mut h = Stoat::test();
    let (buffer_id, mut done_rx) = open_agent_editor(&mut h);

    assert!(
        h.stoat
            .active_workspace()
            .editor_bridge_waiters
            .contains_key(&buffer_id),
        "a waiter is registered for the opened buffer",
    );
    assert!(done_rx.try_recv().is_err(), "waiter not fired before close");

    assert_eq!(
        action_handlers::dispatch(&mut h.stoat, &stoat_action::CloseBuffer),
        UpdateEffect::Redraw
    );

    assert!(
        done_rx.try_recv().is_ok(),
        "closing the buffer fires the waiter"
    );
    assert!(
        !h.stoat
            .active_workspace()
            .editor_bridge_waiters
            .contains_key(&buffer_id),
        "the fired waiter is removed",
    );
}

#[test]
fn agent_open_editor_waiter_fires_on_pane_close() {
    let mut h = Stoat::test();
    let (_buffer_id, mut done_rx) = open_agent_editor(&mut h);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::ClosePane);

    assert!(
        done_rx.try_recv().is_ok(),
        "closing the pane fires the waiter"
    );
}

/// The whole point of the emission: an image pane on a capable terminal
/// puts the file's pixels on the wire and asks for them to be drawn.
#[test]
fn an_image_pane_transmits_its_file_and_places_it() {
    use std::io::Cursor;
    use stoat_action::OpenFile;
    use stoatty_protocol::{
        command::{decode_stream, Command},
        kitty::Action,
    };

    let mut h = crate::test_harness::TestHarness::with_size(40, 12);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.stoatty = true;
    h.stoat.stoatty_protocol = 2;
    h.stoat.cell_pixels = Some((8, 16));

    let png = {
        let buffer = image::RgbaImage::from_pixel(32, 32, image::Rgba([1, 2, 3, 255]));
        let mut out = Cursor::new(Vec::new());
        buffer
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    };
    h.fake_fs().insert_file("/repo/pic.png", png);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    action_handlers::dispatch(
        &mut h.stoat,
        &OpenFile {
            path: PathBuf::from("/repo/pic.png"),
        },
    );
    h.settle();

    // The first pass starts the read; the second finds it done and sends.
    crate::image_emit::emit_images(&mut h.stoat);
    h.settle();
    crate::image_emit::emit_images(&mut h.stoat);

    let sent: Vec<u8> = std::iter::from_fn(|| rx.try_recv().ok())
        .flatten()
        .collect();
    let actions: Vec<Action> = decode_stream(&sent)
        .into_iter()
        .filter_map(|command| match command {
            Command::Kitty(frame) => Some(frame.control.action),
            _ => None,
        })
        .collect();

    assert_eq!(
        actions,
        [Action::Transmit, Action::Delete, Action::Put],
        "the pixels go out once, then the placement is cleared and set",
    );
}

/// The terminal scales what it holds down to the placement, so a photo
/// transmitted at its own size costs a decode, an encode, the wire, and a
/// texture for pixels nobody sees. stoatty refuses a decode past 64 MiB
/// outright, so a large enough photo would show nothing after all of it.
#[test]
fn an_image_larger_than_its_pane_transmits_at_the_pane_size() {
    use std::io::Cursor;
    use stoat_action::OpenFile;

    let mut h = crate::test_harness::TestHarness::with_size(40, 12);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.stoatty = true;
    h.stoat.stoatty_protocol = 2;
    h.stoat.cell_pixels = Some((8, 16));

    let png = {
        let buffer = image::RgbaImage::from_pixel(3200, 3200, image::Rgba([1, 2, 3, 255]));
        let mut out = Cursor::new(Vec::new());
        buffer
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    };
    h.fake_fs().insert_file("/repo/big.png", png);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    action_handlers::dispatch(
        &mut h.stoat,
        &OpenFile {
            path: PathBuf::from("/repo/big.png"),
        },
    );
    h.settle();

    crate::image_emit::emit_images(&mut h.stoat);
    h.settle();
    crate::image_emit::emit_images(&mut h.stoat);

    let rect = image_pane_content(&h);
    let sent: Vec<u8> = std::iter::from_fn(|| rx.try_recv().ok())
        .flatten()
        .collect();
    assert_eq!(
        transmitted_size(&sent),
        Some((u32::from(rect.height) * 16, u32::from(rect.height) * 16)),
        "a square source is scaled to the shorter side of the pane's box",
    );
}

/// A pane grown past what was sent has to read again, since the terminal
/// can only scale down the pixels it holds. The fresh transmission takes a
/// new id, and the batch that places it frees the old one, so no frame in
/// between shows nothing.
#[test]
fn a_grown_image_pane_transmits_again_and_frees_the_old_pixels() {
    use std::io::Cursor;
    use stoat_action::OpenFile;
    use stoatty_protocol::{
        command::{decode_stream, Command},
        kitty::Action,
    };

    let mut h = crate::test_harness::TestHarness::with_size(20, 8);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.stoatty = true;
    h.stoat.stoatty_protocol = 2;
    h.stoat.cell_pixels = Some((8, 16));

    let png = {
        let buffer = image::RgbaImage::from_pixel(3200, 3200, image::Rgba([1, 2, 3, 255]));
        let mut out = Cursor::new(Vec::new());
        buffer
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    };
    h.fake_fs().insert_file("/repo/big.png", png);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    action_handlers::dispatch(
        &mut h.stoat,
        &OpenFile {
            path: PathBuf::from("/repo/big.png"),
        },
    );
    h.settle();

    crate::image_emit::emit_images(&mut h.stoat);
    h.settle();
    crate::image_emit::emit_images(&mut h.stoat);
    let small = image_pane_content(&h);
    while rx.try_recv().is_ok() {}

    h.resize(60, 30);
    crate::image_emit::emit_images(&mut h.stoat);
    h.settle();
    crate::image_emit::emit_images(&mut h.stoat);

    let grown = image_pane_content(&h);
    assert!(
        grown.height > small.height,
        "the pane has to grow for this to measure anything",
    );

    let sent: Vec<u8> = std::iter::from_fn(|| rx.try_recv().ok())
        .flatten()
        .collect();
    // A chunked transmission's continuations carry the payload and no id,
    // so the frame naming an id is the one that names the action too.
    let actions: Vec<(Action, u32, bool)> = decode_stream(&sent)
        .into_iter()
        .filter_map(|command| match command {
            Command::Kitty(frame) if frame.control.id != 0 => Some((
                frame.control.action,
                frame.control.id,
                frame.control.delete.free_data,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        actions,
        [
            (Action::Transmit, 2, false),
            (Action::Delete, 1, true),
            (Action::Delete, 2, false),
            (Action::Put, 2, false),
        ],
        "the larger pixels go out, the old ones are freed, and the new id is placed",
    );
    assert_eq!(
        transmitted_size(&sent),
        Some((u32::from(grown.height) * 16, u32::from(grown.height) * 16)),
        "at the grown pane's size",
    );
}

/// A pane that stops showing an image has its pixels freed along with the
/// placement, so reopening the file has to send them again. Placing the id
/// the terminal freed would show nothing at all.
#[test]
fn an_image_reopened_after_its_pane_moved_on_transmits_again() {
    use std::io::Cursor;
    use stoat_action::OpenFile;
    use stoatty_protocol::{
        command::{decode_stream, Command},
        kitty::Action,
    };

    let mut h = crate::test_harness::TestHarness::with_size(40, 12);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.stoatty = true;
    h.stoat.stoatty_protocol = 2;
    h.stoat.cell_pixels = Some((8, 16));

    let png = {
        let buffer = image::RgbaImage::from_pixel(32, 32, image::Rgba([1, 2, 3, 255]));
        let mut out = Cursor::new(Vec::new());
        buffer
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    };
    h.fake_fs().insert_file("/repo/pic.png", png);
    h.fake_fs().insert_file("/repo/a.txt", b"text");
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");

    let open = |h: &mut crate::test_harness::TestHarness, name: &str| {
        action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: PathBuf::from(format!("/repo/{name}")),
            },
        );
        h.settle();
        crate::image_emit::emit_images(&mut h.stoat);
        h.settle();
        crate::image_emit::emit_images(&mut h.stoat);
    };

    open(&mut h, "pic.png");
    open(&mut h, "a.txt");
    while rx.try_recv().is_ok() {}
    open(&mut h, "pic.png");

    let sent: Vec<u8> = std::iter::from_fn(|| rx.try_recv().ok())
        .flatten()
        .collect();
    let transmits: Vec<u32> = decode_stream(&sent)
        .into_iter()
        .filter_map(|command| match command {
            Command::Kitty(frame)
                if frame.control.id != 0 && frame.control.action == Action::Transmit =>
            {
                Some(frame.control.id)
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        transmits,
        vec![2],
        "the reopened file sends its pixels again, under a fresh id",
    );
}

/// The content rectangle of the one image pane on screen, which is what an
/// image is fitted into.
fn image_pane_content(h: &crate::test_harness::TestHarness) -> Rect {
    h.stoat
        .active_workspace()
        .panes
        .split_panes()
        .find_map(|(_, pane)| {
            matches!(pane.view, View::Image { .. })
                .then(|| crate::render::layout::split_pane_status(pane.area).0)
        })
        .expect("an image pane")
}

/// The pixel size of the image a batch transmitted, read from the PNG the
/// graphics frames carry.
fn transmitted_size(batch: &[u8]) -> Option<(u32, u32)> {
    use base64::Engine;
    use std::io::Cursor;
    use stoatty_protocol::command::{decode_stream, Command};

    let payload: Vec<u8> = decode_stream(batch)
        .into_iter()
        .filter_map(|command| match command {
            Command::Kitty(frame) => Some(frame.payload),
            _ => None,
        })
        .flatten()
        .collect();
    let png = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;

    image::ImageReader::new(Cursor::new(&png))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Install a passthrough link on `stoat` and hand back the two ends a test
/// inspects: the slot the input thread reads, and the control lane the UI
/// thread drains.
fn install_passthrough(
    stoat: &mut Stoat,
) -> (Arc<ssh::PassthroughSlot>, UnboundedReceiver<ssh::UiControl>) {
    let (slot, ack_rx) = ssh::PassthroughSlot::new();
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
    stoat.set_passthrough_link(ssh::PassthroughLink {
        slot: slot.clone(),
        ui_tx,
        ack_rx,
    });
    (slot, ui_rx)
}

fn pool_region(pool: u32) -> PoolRegionCommand {
    PoolRegionCommand {
        pool,
        top: 0,
        left: 0,
        width: 40,
        height: 10,
        window: 0,
    }
}

/// A `:ssh` hands the terminal to a remote stoat, so everything this
/// session declared has to be retired first and the claim on the zoom combo
/// dropped. The remote drives the same terminal, and nothing tells it what
/// is already there.
#[test]
fn connect_retires_the_terminal_state_and_arms_the_slot() {
    use stoatty_protocol::command::{Command, PoolDropCommand};

    let mut h = crate::test_harness::TestHarness::with_size(80, 24);
    h.stoat.stoatty = true;
    h.stoat.zoom_claimed = true;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    let (slot, _ui_rx) = install_passthrough(&mut h.stoat);

    let mut declared = Vec::new();
    stoat_widgets::pool::emit_into(
        &mut declared,
        &mut h.stoat.smooth_scroll,
        pool_region(7),
        0.0,
        0,
        false,
        |_| Vec::new(),
    );
    while rx.try_recv().is_ok() {}

    assert_eq!(
        ssh::connect(&mut h.stoat, ssh::Transport::Ssh, Some("somewhere"), &[]),
        UpdateEffect::None,
    );

    let sent = crate::test_fixture::drain_apc(&mut rx);
    assert!(
        sent.contains(&Command::PoolDrop(PoolDropCommand { pool: 7 })),
        "the declared pool is retired: {sent:?}",
    );
    assert!(sent.contains(&Command::Reset), "and the scene is reset");
    assert!(
        sent.iter()
            .any(|cmd| matches!(cmd, Command::ZoomCapture { on: false, .. })),
        "and the zoom claim is released",
    );
    assert!(
        matches!(h.stoat.passthrough, Some(ssh::Passthrough::Pending { .. })),
        "the session is armed and waiting for the input thread's ack",
    );
    assert!(
        matches!(slot.state(), ssh::SlotState::Pending),
        "and the input thread is told to stop parsing fd 0",
    );
}

/// A detached pane lives in an aux window the remote knows nothing about,
/// so the handoff refuses rather than stranding it.
#[test]
fn connect_refuses_while_a_pane_is_detached() {
    let mut h = Stoat::test();
    install_passthrough(&mut h.stoat);
    h.stoat.aux_windows.insert(1, (80, 24));

    assert_eq!(
        ssh::connect(&mut h.stoat, ssh::Transport::Ssh, Some("somewhere"), &[]),
        UpdateEffect::Redraw,
    );
    assert!(h.stoat.passthrough.is_none(), "nothing was handed over");
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("reattach detached panes before :ssh"),
    );
}

/// The whole run, from the ack the spawn waits for to the exit report.
#[test]
fn a_remote_session_spawns_pipes_its_output_and_reports_its_exit() {
    let mut h = Stoat::test();
    let fake = Arc::new(crate::host::FakeTerminalSession::new());
    let host = Arc::new(crate::host::FakeTerminalHost::new(fake));
    h.stoat.terminal_host = host.clone();
    h.allow_host_swap();
    let (slot, mut ui_rx) = install_passthrough(&mut h.stoat);
    h.stoat.settings.ssh_program = Some("/opt/stoat".to_owned());

    ssh::connect(
        &mut h.stoat,
        ssh::Transport::Ssh,
        Some("box"),
        &["~/proj".to_owned()],
    );
    assert_eq!(
        h.stoat.active_workspace().remote,
        Some(ssh::RemoteTarget {
            transport: ssh::Transport::Ssh,
            host: "box".to_owned(),
            args: vec!["~/proj".to_owned()],
        }),
        "the workspace records where its window went",
    );
    assert_eq!(ssh::spawn_armed(&mut h.stoat), UpdateEffect::None);

    let spawns = host.spawns();
    assert_eq!(spawns.len(), 1, "one ssh process");
    assert_eq!(spawns[0].program, "ssh");
    assert_eq!(
        spawns[0].args,
        vec![
            "-e",
            "none",
            "-t",
            "box",
            &format!(
                "/opt/stoat --attachable {} ~/proj",
                h.stoat.active_workspace().uid
            ),
        ],
        "the remote runs attachable under a PTY with the escape off",
    );
    assert_eq!(
        (spawns[0].width, spawns[0].rows),
        (h.stoat.size().width, h.stoat.size().height),
        "and the remote opens at this window's size",
    );
    assert!(
        matches!(slot.state(), ssh::SlotState::Active(_)),
        "fd 0 now feeds the session",
    );

    h.stoat.handle_pty_notification(PtyNotification::SshOutput {
        data: b"ssh: connect refused\r\n".to_vec(),
    });
    assert!(
        matches!(ui_rx.try_recv(), Ok(ssh::UiControl::Raw(bytes)) if bytes == b"ssh: connect refused\r\n"),
        "remote output reaches stdout untouched",
    );

    // A key pressed while the remote owns the keyboard was parsed out of
    // the byte stream before the switch and belongs to nobody now.
    assert_eq!(
        h.stoat.update(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE
        ))),
        UpdateEffect::None,
    );

    assert_eq!(
        h.stoat.handle_pty_notification(PtyNotification::SshExited {
            exit_status: Some(255)
        }),
        UpdateEffect::Redraw,
    );
    assert!(h.stoat.passthrough.is_none(), "the session is over");
    assert!(
        matches!(slot.state(), ssh::SlotState::Idle),
        "and fd 0 feeds the editor again",
    );
    assert!(
        matches!(ui_rx.try_recv(), Ok(ssh::UiControl::Resume)),
        "the UI thread is told to take the screen back",
    );
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("ssh exited (255): ssh: connect refused"),
        "and the error the remote printed inside the alternate screen survives",
    );
    assert_eq!(
        h.stoat.active_workspace().remote,
        Some(ssh::RemoteTarget {
            transport: ssh::Transport::Ssh,
            host: "box".to_owned(),
            args: vec!["~/proj".to_owned()],
        }),
        "and a dropped link keeps the target for the next reopen",
    );
}

/// The user closing the remote editor is the one ending that means the
/// window belongs here again, so nothing is left to reconnect to.
#[test]
fn a_clean_remote_exit_clears_the_workspace_target() {
    let mut h = Stoat::test();
    let fake = Arc::new(crate::host::FakeTerminalSession::new());
    let host = Arc::new(crate::host::FakeTerminalHost::new(fake));
    h.stoat.terminal_host = host;
    h.allow_host_swap();
    install_passthrough(&mut h.stoat);

    ssh::connect(&mut h.stoat, ssh::Transport::Ssh, Some("box"), &[]);
    ssh::spawn_armed(&mut h.stoat);
    assert!(h.stoat.active_workspace().remote.is_some());

    h.stoat.handle_pty_notification(PtyNotification::SshExited {
        exit_status: Some(0),
    });
    assert_eq!(h.stoat.active_workspace().remote, None);
}

/// mosh takes the remote command as separate argv entries and disables its
/// escape key through the environment, so the same handoff spawns a
/// differently shaped process.
#[test]
fn a_mosh_session_spawns_with_the_server_flag_and_the_escape_key_off() {
    let mut h = Stoat::test();
    let fake = Arc::new(crate::host::FakeTerminalSession::new());
    let host = Arc::new(crate::host::FakeTerminalHost::new(fake));
    h.stoat.terminal_host = host.clone();
    h.allow_host_swap();
    install_passthrough(&mut h.stoat);
    h.stoat.settings.ssh_program = Some("/opt/stoat".to_owned());
    h.stoat.settings.mosh_server = Some("/opt/mosh-server".to_owned());

    ssh::connect(
        &mut h.stoat,
        ssh::Transport::Mosh,
        Some("box"),
        &["~/proj".to_owned()],
    );
    assert_eq!(ssh::spawn_armed(&mut h.stoat), UpdateEffect::None);

    let spawns = host.spawns();
    assert_eq!(spawns.len(), 1, "one mosh process");
    assert_eq!(spawns[0].program, "mosh");
    assert_eq!(
        spawns[0].args,
        vec![
            "--server=/opt/mosh-server",
            "--",
            "box",
            "/opt/stoat",
            "--attachable",
            &h.stoat.active_workspace().uid.to_string(),
            "~/proj",
        ],
        "the remote command stays in separate unquoted entries",
    );
    assert_eq!(
        spawns[0].env,
        vec![("MOSH_ESCAPE_KEY".to_owned(), String::new())],
        "and Ctrl-^ reaches the remote instead of ending the session",
    );

    h.stoat.handle_pty_notification(PtyNotification::SshOutput {
        data: b"Did not find mosh server startup message.\r\n".to_vec(),
    });
    h.stoat.handle_pty_notification(PtyNotification::SshExited {
        exit_status: Some(1),
    });
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("mosh exited (1): Did not find mosh server startup message."),
        "and the exit report names mosh",
    );
}

/// An attached terminal holds nothing this session sent and answers for
/// itself, so everything declared is retired and the identity starts over.
#[test]
fn an_attach_retires_the_terminal_and_asks_who_it_is() {
    use stoatty_protocol::command::{Command, PoolDropCommand};

    let mut h = crate::test_harness::TestHarness::with_size(80, 24);
    h.stoat.stoatty = true;
    h.stoat.stoatty_protocol = 7;
    h.stoat.zoom_claimed = true;
    h.stoat.terminal_reported = true;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    let (slot, mut ui_rx) = install_passthrough(&mut h.stoat);

    let mut declared = Vec::new();
    stoat_widgets::pool::emit_into(
        &mut declared,
        &mut h.stoat.smooth_scroll,
        pool_region(4),
        0.0,
        0,
        false,
        |_| Vec::new(),
    );
    while rx.try_recv().is_ok() {}

    assert_eq!(ssh::terminal_replaced(&mut h.stoat), UpdateEffect::None);

    let sent = crate::test_fixture::drain_apc(&mut rx);
    assert!(
        sent.contains(&Command::PoolDrop(PoolDropCommand { pool: 4 })),
        "the declared pool is retired: {sent:?}",
    );
    assert!(sent.contains(&Command::Reset), "and the scene is reset");

    assert!(!h.stoat.stoatty, "the terminal is unidentified again");
    assert_eq!(h.stoat.stoatty_protocol, 0);
    assert!(!h.stoat.terminal_reported);
    assert!(
        matches!(slot.state(), ssh::SlotState::Handshake),
        "and the input thread is asked to identify the new terminal",
    );
    assert!(
        matches!(ui_rx.try_recv(), Ok(ssh::UiControl::Resume)),
        "the UI thread re-enters the alternate screen",
    );

    h.stoat.handle_stoatty_present(Some(1));
    assert!(h.stoat.stoatty, "and the new terminal's answer upgrades it");
    assert_eq!(h.stoat.stoatty_protocol, 1);
}

/// A remote owns the screen, so the client that got replaced is that
/// session's, not this one's. This process declared nothing to retire.
#[test]
fn an_attach_while_a_remote_runs_leaves_the_passthrough_alone() {
    let mut h = Stoat::test();
    let (slot, mut ui_rx) = install_passthrough(&mut h.stoat);
    ssh::connect(&mut h.stoat, ssh::Transport::Ssh, Some("box"), &[]);

    assert_eq!(ssh::terminal_replaced(&mut h.stoat), UpdateEffect::None);

    assert!(
        matches!(h.stoat.passthrough, Some(ssh::Passthrough::Pending { .. })),
        "the armed session is untouched",
    );
    assert!(matches!(slot.state(), ssh::SlotState::Pending));
    assert!(
        ui_rx.try_recv().is_err(),
        "and the screen is not taken back"
    );
}

/// A bare `:ssh` reuses the host and args the workspace last went to. The
/// typed command still picks the link, so a bare `:mosh` after an `:ssh`
/// reaches the same remote over the other one.
#[test]
fn a_bare_command_reconnects_to_the_stored_target() {
    let mut h = Stoat::test();
    install_passthrough(&mut h.stoat);
    h.stoat.active_workspace_mut().remote = Some(ssh::RemoteTarget {
        transport: ssh::Transport::Ssh,
        host: "box".to_owned(),
        args: vec!["~/proj".to_owned()],
    });

    ssh::connect(&mut h.stoat, ssh::Transport::Mosh, None, &[]);

    assert!(
        matches!(
            h.stoat.passthrough,
            Some(ssh::Passthrough::Pending {
                transport: ssh::Transport::Mosh,
                ref host,
                ref args,
                ..
            }) if host == "box" && args == &["~/proj".to_owned()]
        ),
        "the stored host and args ride the typed link",
    );
    assert_eq!(
        h.stoat.active_workspace().remote,
        Some(ssh::RemoteTarget {
            transport: ssh::Transport::Mosh,
            host: "box".to_owned(),
            args: vec!["~/proj".to_owned()],
        }),
        "and the record follows the link it went over",
    );
}

#[test]
fn a_bare_command_with_nothing_stored_says_so() {
    let mut h = Stoat::test();
    install_passthrough(&mut h.stoat);

    assert_eq!(
        ssh::connect(&mut h.stoat, ssh::Transport::Ssh, None, &[]),
        UpdateEffect::Redraw,
    );
    assert!(h.stoat.passthrough.is_none(), "nothing was handed over");
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("no remote to reconnect to"),
    );
}

/// The handoff waits on the terminal's report. An earlier run races that
/// report's own zoom claim and theme colors onto the remote's screen
/// instead of this one.
#[test]
fn a_stored_target_reconnects_only_after_the_terminal_reports() {
    let mut h = Stoat::test();
    install_passthrough(&mut h.stoat);
    h.stoat.active_workspace_mut().remote = Some(ssh::RemoteTarget {
        transport: ssh::Transport::Ssh,
        host: "box".to_owned(),
        args: Vec::new(),
    });
    h.stoat.remote_pending = true;

    assert_eq!(ssh::reconnect_when_ready(&mut h.stoat), UpdateEffect::None);
    assert!(
        h.stoat.passthrough.is_none(),
        "nothing is handed over before the terminal answers",
    );
    assert!(h.stoat.remote_pending, "and the workspace stays armed");

    h.stoat.terminal_reported = true;
    ssh::reconnect_when_ready(&mut h.stoat);
    assert!(
        matches!(
            h.stoat.passthrough,
            Some(ssh::Passthrough::Pending { ref host, .. }) if host == "box"
        ),
        "the report releases the handoff",
    );
    assert!(!h.stoat.remote_pending, "and the arming is spent");
}

/// Becoming active is the other half of the pair, so a switch onto a
/// workspace that carries a target sends the window back to that host.
#[test]
fn switching_to_a_workspace_with_a_target_reconnects_to_it() {
    let mut h = Stoat::test();
    install_passthrough(&mut h.stoat);
    h.stoat.terminal_reported = true;
    h.stoat.active_workspace_mut().remote = Some(ssh::RemoteTarget {
        transport: ssh::Transport::Mosh,
        host: "box".to_owned(),
        args: Vec::new(),
    });

    // The copy carries the source's target and is switched to, which is
    // the switch path a picker selection also takes.
    action_handlers::dispatch(&mut h.stoat, &stoat_action::CopyWorkspace);

    assert!(
        matches!(
            h.stoat.passthrough,
            Some(ssh::Passthrough::Pending {
                transport: ssh::Transport::Mosh,
                ref host,
                ..
            }) if host == "box"
        ),
        "the workspace switched to hands its window back",
    );
}
