use ratatui::layout::Rect;
use std::path::{Path, PathBuf};
use stoat::{
    action_handlers, commit_picker,
    render::{blame_card::CARD_EDGES, OVERLAY_LANE},
    test_harness::{CommitSpec, TestHarness},
    theme::PanelRole,
};
use stoatty_protocol::{
    command::{decode_stream, BorderStyle, Command, Edge},
    PROTOCOL_VERSION,
};

/// A nudge of the overlay lane as its offset, or a panel of it as its
/// cells, its border, its radius, and its edges.
type CardCommand = Result<(Rect, BorderStyle, u8, [Edge; 4]), [i16; 2]>;

/// Without components the card is plain rows under the cursor's line,
/// from the card's second column to its last but one. The rows read the
/// sha and the subject, the meta line cut to the card, the body, and the
/// card's keys cut to the card, over the frame's border on the first and
/// last rows.
#[test]
fn the_cell_card_reads_its_rows_under_the_line() {
    let mut h = card_open();
    h.stoat.stoatty = false;
    h.snapshot();

    let text = text_rect(&mut h);
    let rows: Vec<String> = h
        .rendered_text()
        .lines()
        .skip(usize::from(text.y) + 2)
        .take(4)
        .map(|row| row.chars().skip(usize::from(text.x) - 1).take(56).collect())
        .collect();

    let meta: String = {
        let card = h.stoat.blame_card.as_ref().expect("the card opened");
        let now = commit_picker::now_epoch(&h.stoat.executor);
        let age = commit_picker::age_words(now, card.commit.info.time);
        format!("fake \u{b7} 2023/11/14 22:13 \u{b7} {age} \u{b7} parent c1 \u{b7} +1 \u{2212}0 in 1 file")
            .chars()
            .take(56)
            .collect()
    };
    assert_eq!(
        rows,
        [
            format!("{:\u{2500}<56}", "c2 two"),
            meta,
            format!("{:<56}", "the body of two"),
            format!(
                "{:\u{2500}<56}",
                "Ret preview patch  c check out  p blame parent  y copy s"
            ),
        ],
    );
}

/// Under the nudge protocol the card is one borderless panel in the popout
/// role's radius over the four rows under the cursor's line, 58 columns
/// from two left of the text, with the board's edges. The panel draws
/// 10 px down, its text 15 px right of that, and the lane's nudge resets
/// after the card.
#[test]
fn the_nudged_card_draws_one_borderless_panel() {
    let mut h = card_open();
    h.stoat.stoatty_protocol = PROTOCOL_VERSION;
    h.stoat.render();

    let text = text_rect(&mut h);
    let radius = h.stoat.theme.panel(PanelRole::Popout).corner_radius;
    assert_eq!(
        card_commands(&h),
        [
            Err([0, 10]),
            Ok((
                Rect::new(text.x - 2, text.y + 2, 58, 4),
                BorderStyle::None_,
                radius,
                CARD_EDGES
            )),
            Err([15, 10]),
            Err([0, 0]),
        ],
    );
}

/// A card with no room under its line opens over it, 8 px down, so the box
/// ends 4 px over the line. The second commit's message has no body, so
/// the card has three rows.
#[test]
fn a_card_near_the_pane_bottom_opens_over_the_line() {
    let base: String = (0..40).map(|i| format!("line {i}\n")).collect();
    let text = base.replace("line 39\n", "LINE 39\n");
    let mut h = blamed_pane(
        &[
            ("c1", "one", &[("a.rs", &base)]),
            ("c2", "two", &[("a.rs", &text)]),
        ],
        &text,
    );
    h.type_keys("4 0 G K");
    h.settle();
    h.stoat.stoatty_protocol = PROTOCOL_VERSION;
    h.stoat.render();

    let (area, line) = {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let area = editor.text_rect.expect("the text painted");
        (area, area.y + (39 - editor.scroll_row) as u16)
    };
    let radius = h.stoat.theme.panel(PanelRole::Popout).corner_radius;
    assert_eq!(
        card_commands(&h),
        [
            Err([0, 8]),
            Ok((
                Rect::new(area.x - 2, line - 4, 58, 3),
                BorderStyle::None_,
                radius,
                CARD_EDGES
            )),
            Err([15, 8]),
            Err([0, 0]),
        ],
    );
}

/// The overlay lane's nudges and panels from its first nudge on, which is
/// where the card starts.
fn card_commands(h: &TestHarness) -> Vec<CardCommand> {
    decode_stream(h.stoat.apc_scene.lane_bytes(OVERLAY_LANE))
        .iter()
        .skip_while(|command| !matches!(command, Command::Nudge(_)))
        .filter_map(|command| match command {
            Command::Nudge(offset) => Some(Err(*offset)),
            Command::Panel(panel) => Some(Ok((
                Rect::new(panel.left, panel.top, panel.width, panel.height),
                panel.style,
                panel.corner_radius,
                panel.edges,
            ))),
            _ => None,
        })
        .collect()
}

/// A blamed pane over `a.rs` of a two-commit repository, with the card
/// open on line 2 and its details landed.
fn card_open() -> TestHarness {
    let mut h = blamed_pane(
        &[
            ("c1", "one", &[("a.rs", "one\n")]),
            ("c2", "two\n\nthe body\nof two\n", &[("a.rs", "one\ntwo\n")]),
        ],
        "one\ntwo\n",
    );
    h.type_keys("j K");
    h.settle();
    h
}

/// A 120 by 24 window over `a.rs` of a repository at `/repo` that holds
/// `history`, with `text` in the working tree and the Blame Column on.
fn blamed_pane(history: &[CommitSpec<'_>], text: &str) -> TestHarness {
    let mut h = TestHarness::with_size(120, 24);
    h.seed_linear_history("/repo", history);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_fs().insert_file("/repo/a.rs", text);
    h.open_file(Path::new("/repo/a.rs"));
    h.type_keys("space h a");
    h.settle();
    h
}

/// The focused editor's text rect as the last paint left it.
fn text_rect(h: &mut TestHarness) -> Rect {
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    editor.text_rect.expect("the text painted")
}
