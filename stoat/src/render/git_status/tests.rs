use super::{divergence_text, git_status_layout, note_text, render_git_status};
use crate::{
    app::DEFAULT_KEYMAP,
    git_status::{
        Divergence, FileRow, GitStatusModel, GitStatusPanel, Note, OpBanner, OpKind, RowKind,
        Section, SectionKind,
    },
    theme::{scope, Theme},
};
use ratatui::{buffer::Buffer, layout::Rect};
use std::path::PathBuf;
use stoat_widgets::ApcScene;
use stoatty_protocol::command::{decode_stream, Command};

/// A model with one section of `rows` files and no banner.
fn model(rows: usize) -> GitStatusModel {
    GitStatusModel {
        root: PathBuf::from("/repo"),
        branch: Some("main".to_string()),
        divergence: Divergence::Unknown,
        upstream: String::new(),
        op: None,
        sections: vec![Section {
            kind: SectionKind::Staged,
            rows: (0..rows)
                .map(|i| FileRow {
                    path: PathBuf::from(format!("/repo/{i}.rs")),
                    kind: RowKind::Modified,
                    note: Note::None,
                    from_pause: false,
                })
                .collect(),
        }],
    }
}

/// The box hangs two rows under the tab bar's rows, one cell in from the
/// window's right edge, and fits the header, the section head, and its rows.
/// A plain bar takes one row, a band two, and a hidden bar none.
#[test]
fn the_box_hangs_from_the_top_right_and_fits_its_rows() {
    let layout = |tab_bar_rows: u16| {
        git_status_layout(Rect::new(0, 0, 160, 50), Some(&model(2)), tab_bar_rows, 1)
            .expect("the box fits")
    };
    assert_eq!(
        [layout(1), layout(2), layout(0)],
        [
            (Rect::new(108, 3, 51, 6), Rect::new(109, 4, 49, 4)),
            (Rect::new(108, 4, 51, 6), Rect::new(109, 5, 49, 4)),
            (Rect::new(108, 2, 51, 6), Rect::new(109, 3, 49, 4)),
        ],
    );
}

/// A list longer than the window stops above the window's bottom rows, which
/// the status rows hold. A plain row takes one, and a band two under a band's
/// two-row bar.
#[test]
fn a_long_list_clips_above_the_status_bar() {
    let rect = |rows: u16| {
        git_status_layout(Rect::new(0, 0, 80, 20), Some(&model(40)), rows, rows)
            .expect("the box fits")
            .0
    };
    assert_eq!(
        [rect(1), rect(2)],
        [Rect::new(28, 3, 51, 16), Rect::new(28, 4, 51, 14)],
    );
}

/// The box opens with one blank row before the first read lands, and a
/// window narrower than the box holds none.
#[test]
fn the_box_opens_before_the_read_and_not_in_a_narrow_window() {
    assert_eq!(
        (
            git_status_layout(Rect::new(0, 0, 80, 20), None, 0, 1).map(|(rect, _)| rect),
            git_status_layout(Rect::new(0, 0, 51, 20), Some(&model(1)), 0, 1),
        ),
        (Some(Rect::new(28, 2, 51, 3)), None),
    );
}

#[test]
fn the_header_names_each_divergence_as_the_board_does() {
    let texts: Vec<String> = [
        Divergence::Equal("origin/main".to_string()),
        Divergence::AheadBehind(7, 2),
        Divergence::AheadBehind(1, 0),
        Divergence::AheadBehind(0, 3),
        Divergence::Detached("3e9f0aa".to_string()),
        Divergence::Rebase {
            done: 3,
            total: 7,
            onto: "main".to_string(),
        },
        Divergence::Unknown,
    ]
    .iter()
    .map(divergence_text)
    .collect();
    assert_eq!(
        texts,
        [
            "= origin/main",
            "\u{2191}7 \u{2193}2",
            "\u{2191}1",
            "\u{2193}3",
            "detached at 3e9f0aa",
            "3 / 7 onto main",
            "",
        ],
    );
}

#[test]
fn each_note_reads_as_the_board_draws_it() {
    let notes = [
        Note::Open { open: 2, total: 4 },
        Note::Resolved,
        Note::TookOurs,
        Note::TookTheirs,
        Note::Lines {
            added: 5,
            deleted: 6,
        },
        Note::Lines {
            added: 6,
            deleted: 0,
        },
        Note::Lines {
            added: 0,
            deleted: 3,
        },
        Note::None,
    ]
    .map(note_text);
    let text = |text: &str, scope| Some((text.to_string(), scope));
    assert_eq!(
        notes,
        [
            text("2 / 4 open", scope::VCS_STATUS_CONFLICTED),
            text("resolved", scope::VCS_STATUS_STAGED),
            text("took ours", scope::VCS_STATUS_STAGED),
            text("took theirs", scope::VCS_STATUS_STAGED),
            text("+5 \u{2212}6", scope::UI_TEXT_MUTED),
            text("+6", scope::UI_TEXT_MUTED),
            text("\u{2212}3", scope::UI_TEXT_MUTED),
            None,
        ],
    );
}

/// The banner's rows come out of the body, so with a banner the paint scrolls
/// further to keep the same row in view.
#[test]
fn the_banner_rows_come_out_of_the_body_the_scroll_keeps() {
    let (config, _) = stoat_config::parse("theme t { }");
    let theme = Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");
    let top = |op: Option<OpBanner>| {
        let mut panel = GitStatusPanel::default();
        panel.land(GitStatusModel { op, ..model(12) });
        panel.selected = 11;
        let area = Rect::new(0, 0, 80, 14);
        let mut buf = Buffer::empty(area);
        render_git_status(
            &mut panel,
            &theme,
            area,
            0,
            1,
            &mut buf,
            &mut ApcScene::new(),
        );
        panel.top
    };
    let banner = OpBanner {
        kind: OpKind::Merge,
        head: String::new(),
        message: String::new(),
        detail: String::new(),
        keys: Vec::new(),
    };

    assert_eq!((top(None), top(Some(banner))), (5, 8));
}

/// The banner's tag chip and each row's kind letter draw bold, as the
/// VcsStatus board draws them, and no other run of the panel does.
#[test]
fn the_tag_chip_and_the_kind_letters_draw_bold() {
    let (config, _) = stoat_config::parse(DEFAULT_KEYMAP);
    let theme = Theme::from_config(&config.expect("the shipped config parses"), "ermine_night")
        .expect("ermine_night loads");
    let mut panel = GitStatusPanel::default();
    panel.land(GitStatusModel {
        op: Some(OpBanner {
            kind: OpKind::Merge,
            head: "feature".to_string(),
            message: "merge feature".to_string(),
            detail: String::new(),
            keys: Vec::new(),
        }),
        ..model(2)
    });
    let area = Rect::new(0, 0, 80, 14);
    let mut scene = ApcScene::new();
    render_git_status(
        &mut panel,
        &theme,
        area,
        0,
        1,
        &mut Buffer::empty(area),
        &mut scene,
    );

    let bold: Vec<String> = decode_stream(scene.buffer())
        .into_iter()
        .filter_map(|command| match command {
            Command::TextRun(run) if run.bold => Some(run.text),
            _ => None,
        })
        .collect();
    assert_eq!(bold, ["MERGE", "M", "M"]);
}
