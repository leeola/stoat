use super::{divergence_text, git_status_layout, note_text};
use crate::{
    git_status::{Divergence, FileRow, GitStatusModel, Note, RowKind, Section, SectionKind},
    theme::scope,
};
use ratatui::layout::Rect;
use std::path::PathBuf;

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
                })
                .collect(),
        }],
    }
}

/// The box hangs from row two, one cell in from the window's right edge, and
/// fits the header, the section head, and its rows.
#[test]
fn the_box_hangs_from_the_top_right_and_fits_its_rows() {
    let (rect, inner) =
        git_status_layout(Rect::new(0, 0, 160, 50), Some(&model(2))).expect("the box fits");
    assert_eq!(
        (rect, inner),
        (Rect::new(108, 2, 51, 6), Rect::new(109, 3, 49, 4)),
    );
}

/// A list longer than the window stops one row above the window's bottom,
/// which the status bar holds.
#[test]
fn a_long_list_clips_above_the_status_bar() {
    let (rect, _) =
        git_status_layout(Rect::new(0, 0, 80, 20), Some(&model(40))).expect("the box fits");
    assert_eq!(rect, Rect::new(28, 2, 51, 17));
}

/// The box opens with one blank row before the first read lands, and a
/// window narrower than the box holds none.
#[test]
fn the_box_opens_before_the_read_and_not_in_a_narrow_window() {
    assert_eq!(
        (
            git_status_layout(Rect::new(0, 0, 80, 20), None).map(|(rect, _)| rect),
            git_status_layout(Rect::new(0, 0, 51, 20), Some(&model(1))),
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
            text("+5 \u{2212}6", scope::UI_TEXT_MUTED),
            text("+6", scope::UI_TEXT_MUTED),
            text("\u{2212}3", scope::UI_TEXT_MUTED),
            None,
        ],
    );
}
