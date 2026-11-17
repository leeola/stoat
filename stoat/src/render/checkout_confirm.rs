//! The checkout confirm's box, as the BlameFlow board draws it.
//!
//! The box names the commit it checks out, says what the checkout does to HEAD
//! and to the working tree's changes, lists the changed files, and ends in its
//! keys.

use crate::{
    checkout_confirm::CheckoutConfirm,
    render::{self, chrome, paint::style_rgb, text},
    theme::{scope, Theme, TypeRole},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders, StatefulWidget},
};
use stoat_widgets::{
    text_run::{self, TextRun},
    ApcScene,
};

/// The files the file line names before it counts the rest.
const SHOWN_FILES: usize = 3;

/// The cells the box takes inside its border on each side of its text.
const PAD: u16 = 1;

/// Paint the confirm in a box centered over `area`.
///
/// The box is 56 cells wide, as the board's 500 px are, and it grows a row for
/// each row the message wraps onto. Its keys draw at the caption scale under a
/// live scene and as cells otherwise. An area too small for the box paints
/// nothing.
///
/// Under the plate look of [`chrome::plate_colors`], the box is a plate whose
/// edges stand at [`chrome::CONFIRM_EDGES`] around its text, and the keys draw
/// on the plate's ground. Otherwise it is a frame over `ui.modal.picker`.
pub(crate) fn render_checkout_confirm(
    confirm: &CheckoutConfirm,
    theme: &Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let Some(width) = chrome::modal_box(area, (0, 6), (56, 6), (40, 6), 0).map(|modal| modal.width)
    else {
        return;
    };
    let text_width = usize::from(width.saturating_sub(2 + 2 * PAD));
    let message = text::wrap_styled(&[message(confirm, theme)], text_width);
    let height = 5 + message.len() as u16;
    let Some(modal) = chrome::modal_box(area, (0, height), (56, 6), (40, 6), 0) else {
        return;
    };

    let style = theme.get(scope::UI_MODAL_PICKER);
    let inner = Block::default().borders(Borders::ALL).inner(modal);
    let (x, end) = (inner.x + PAD, inner.right().saturating_sub(PAD));
    let plate = chrome::plate_colors(theme, scene);
    match plate {
        Some(plate) => {
            let text = Rect::new(x, inner.y, end.saturating_sub(x), inner.height);
            chrome::modal_plate(buf, text, chrome::CONFIRM_EDGES, plate, theme, scene);
        },
        None => {
            render::clear_modal(modal, buf, theme, style);
            chrome::modal_frame_above_pools(buf, modal, None, style, theme, scene);
        },
    }

    let mut y = inner.y;
    paint_cells(buf, x, y, end, &title(confirm, theme));
    for line in &message {
        y += 1;
        paint_cells(buf, x, y, end, line);
    }
    paint_cells(buf, x, y + 1, end, &file_line(confirm, theme));

    let hints = hint_parts(&confirm.hints, theme);
    let runs_bg = match plate {
        Some(plate) => Some(plate.ground),
        None => render::modal_fill(theme, style).filter(|_| scene.live()),
    };
    match runs_bg {
        Some(bg) => paint_runs(&hints, x, y + 2, bg, theme, buf, scene),
        None => paint_cells(buf, x, y + 2, end, &hints),
    }
}

/// The first row, `Check out <sha>?` in bold.
fn title(confirm: &CheckoutConfirm, theme: &Theme) -> Vec<(String, Style)> {
    let bold = |style: Style| style.add_modifier(Modifier::BOLD);
    let text = bold(theme.get(scope::UI_TEXT));
    vec![
        ("Check out ".to_owned(), text),
        (
            confirm.target.commit.short_sha.clone(),
            bold(theme.get(scope::VCS_COMMIT_SHA)),
        ),
        ("?".to_owned(), text),
    ]
}

/// What the checkout does to HEAD and to the changes, as one line that the box
/// wraps.
fn message(confirm: &CheckoutConfirm, theme: &Theme) -> (String, Style) {
    let head = match &confirm.branch {
        Some(branch) => format!("HEAD detaches from {branch}."),
        None => format!("HEAD moves from {}.", confirm.head),
    };
    let changes = match confirm.files.len() {
        1 => "Your 1 uncommitted change is stashed and comes back".to_owned(),
        count => format!("Your {count} uncommitted changes are stashed and come back"),
    };
    (
        format!("{head} {changes} when you return."),
        theme.get(scope::UI_TEXT_DIM),
    )
}

/// The changed files, each as its status and its path, the first
/// [`SHOWN_FILES`] of them and then a count of the rest.
fn file_line(confirm: &CheckoutConfirm, theme: &Theme) -> Vec<(String, Style)> {
    let status = theme.get(scope::UI_MESSAGE_ERROR);
    let muted = theme.get(scope::UI_TEXT_MUTED);
    let mut parts = Vec::new();
    for (index, (code, path)) in confirm.files.iter().take(SHOWN_FILES).enumerate() {
        if index > 0 {
            parts.push(("  ".to_owned(), muted));
        }
        parts.push((code.clone(), status));
        parts.push((format!(" {path}"), muted));
    }
    let rest = confirm.files.len().saturating_sub(SHOWN_FILES);
    if rest > 0 {
        parts.push((format!("  +{rest} more"), muted));
    }
    parts
}

/// The keys of `hints`, each key in the key label color and its word muted.
fn hint_parts(hints: &[(String, String)], theme: &Theme) -> Vec<(String, Style)> {
    let muted = theme.get(scope::UI_TEXT_MUTED);
    let mut parts = Vec::new();
    for (index, (key, label)) in hints.iter().enumerate() {
        if index > 0 {
            parts.push(("  ".to_owned(), muted));
        }
        parts.push((key.clone(), theme.get(scope::UI_KEY_LABEL)));
        parts.push((format!(" {label}"), muted));
    }
    parts
}

/// Write `parts` as cells along row `y` from `x`, cut at `end`.
fn paint_cells(buf: &mut Buffer, x: u16, y: u16, end: u16, parts: &[(String, Style)]) {
    let mut col = x;
    for (part, style) in parts {
        text::write_str_clipped(buf, col, y, part, *style, end);
        col = col.saturating_add(part.chars().count() as u16);
    }
}

/// Draw `parts` as runs at the caption scale along row `y` from cell `x`, each
/// on the box's `bg`.
fn paint_runs(
    parts: &[(String, Style)],
    x: u16,
    y: u16,
    bg: [u8; 3],
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let scale = theme.type_scale(TypeRole::Caption);
    let mut col: u16 = 0;
    for (part, style) in parts {
        if let Some(color) = style_rgb(style.fg) {
            TextRun {
                col: col as i16,
                row: 0,
                scale,
                color,
                bg: Some(bg),
                bold: false,
                italic: false,
                text: part,
                follow: 0,
                anchor: None,
            }
            .render(Rect::new(x, y, 1, 1), buf, scene);
        }
        col += text_run::advance_sixteenths(part.chars().count(), scale);
    }
}

#[cfg(test)]
mod tests {
    use super::render_checkout_confirm;
    use crate::{
        checkout_confirm::{CheckoutConfirm, CheckoutTarget},
        host::CommitInfo,
        render::chrome::CONFIRM_EDGES,
        theme::Theme,
    };
    use ratatui::{buffer::Buffer, layout::Rect};
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{decode_stream, Command};

    /// Without a live scene the box is cells, 56 wide and centered at 100 by
    /// 40. It reads the title, the message wrapped onto two rows, the file
    /// line, and the keys.
    #[test]
    fn the_cell_box_reads_its_four_lines() {
        let side = |text: &str| format!("\u{2502} {text:<52} \u{2502}");
        let edge = |left: char, right: char| format!("{left}{}{right}", "\u{2500}".repeat(54));
        assert_eq!(
            painted(&confirm(
                Some("main"),
                &[(" M", "b.rs"), ("??", "notes.md")]
            )),
            [
                edge('\u{250c}', '\u{2510}'),
                side("Check out b2c3d4e?"),
                side("HEAD detaches from main. Your 2 uncommitted changes"),
                side("are stashed and come back when you return."),
                side(" M b.rs  ?? notes.md"),
                side("Ret stash and check out  Esc cancel"),
                edge('\u{2514}', '\u{2518}'),
            ],
        );
    }

    /// A detached HEAD names its sha, one change reads in the singular, and
    /// the file line names three files and counts the rest.
    #[test]
    fn the_cell_box_names_a_detached_head_one_change_and_the_files_past_three() {
        let one = painted(&confirm(None, &[(" M", "a.rs")]));
        let five = painted(&confirm(
            Some("main"),
            &[
                (" M", "a"),
                ("M ", "b"),
                ("A ", "c"),
                ("??", "d"),
                ("??", "e"),
            ],
        ));
        let side = |text: &str| format!("\u{2502} {text:<52} \u{2502}");
        assert_eq!(
            [&one[2], &one[3], &five[4]],
            [
                &side("HEAD moves from c3d4e5f. Your 1 uncommitted change"),
                &side("is stashed and comes back when you return."),
                &side(" M a  M  b  A  c  +2 more"),
            ],
        );
    }

    /// Under a live scene the keys draw as runs at the caption scale, each run
    /// starting where the one before it ends.
    #[test]
    fn the_live_box_draws_its_keys_as_caption_runs() {
        let (config, _) = stoat_config::parse(
            r##"theme t {
                ui.modal.picker.bg = "#252830";
                ui.key_label.fg = "#6cc7d6";
                ui.text.muted.fg = "#838a98";
            }"##,
        );
        let theme = Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        render_checkout_confirm(
            &confirm(Some("main"), &[(" M", "b.rs"), ("??", "notes.md")]),
            &theme,
            area,
            &mut buf,
            &mut scene,
        );

        let runs: Vec<(String, i16)> = decode_stream(scene.bytes())
            .into_iter()
            .filter_map(|command| match command {
                Command::TextRun(run) => Some((run.text, run.col)),
                _ => None,
            })
            .collect();
        let want = [
            ("Ret", 384),
            (" stash and check out", 432),
            ("  ", 752),
            ("Esc", 784),
            (" cancel", 832),
        ];
        assert_eq!(runs, want.map(|(text, col)| (text.to_string(), col)));
    }

    /// Under the plate look the box is the plate's two panels over the text
    /// cells, which stand one cell inside the border, with their edges at the
    /// confirm's. A plain panel goes first, and a backdrop in the ground
    /// follows. Each run of the keys draws on the ground, not on the box's
    /// `ui.modal.picker` surface.
    #[test]
    fn the_plate_box_stands_its_edges_around_the_text() {
        let (config, _) = stoat_config::parse(
            r##"theme t {
                ui.surface.ground.bg = "#0c0e11";
                ui.background.bg = "#17191e";
                ui.modal.picker.bg = "#252830";
                ui.key_label.fg = "#6cc7d6";
                ui.text.muted.fg = "#838a98";
            }"##,
        );
        let theme = Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        scene.set_nudge_protocol(true);
        render_checkout_confirm(
            &confirm(Some("main"), &[(" M", "b.rs"), ("??", "notes.md")]),
            &theme,
            area,
            &mut buf,
            &mut scene,
        );

        let commands = decode_stream(scene.bytes());
        let panels: Vec<_> = commands
            .iter()
            .filter_map(|command| match command {
                Command::Panel(panel) => Some((
                    Rect::new(panel.left, panel.top, panel.width, panel.height),
                    panel.fill,
                    panel.edges,
                    panel.backdrop,
                )),
                _ => None,
            })
            .collect();
        let run_bgs: Vec<_> = commands
            .iter()
            .filter_map(|command| match command {
                Command::TextRun(run) => Some(run.bg),
                _ => None,
            })
            .collect();
        let (text, ground) = (Rect::new(24, 17, 52, 5), [0x0c, 0x0e, 0x11]);
        assert_eq!(
            (panels, run_bgs),
            (
                vec![
                    (text, None, CONFIRM_EDGES, false),
                    (text, Some(ground), CONFIRM_EDGES, true),
                ],
                vec![Some(ground); 5],
            ),
        );
    }

    /// A confirm of `b2c3d4e5` over `files`, on `branch` or on a detached HEAD
    /// at `c3d4e5f`, with the shipped keys.
    fn confirm(branch: Option<&str>, files: &[(&str, &str)]) -> CheckoutConfirm {
        CheckoutConfirm {
            target: CheckoutTarget {
                workdir: "/repo".into(),
                commit: CommitInfo {
                    sha: "b2c3d4e5".into(),
                    short_sha: "b2c3d4e".into(),
                    summary: "chore: tweak a".into(),
                    author_name: "fake".into(),
                    author_email: "fake@example.invalid".into(),
                    time: 0,
                    parents: vec!["a1b2c3d4".into()],
                },
                path: "a.rs".into(),
                row: 2,
            },
            branch: branch.map(str::to_string),
            head: "c3d4e5f".to_string(),
            files: files
                .iter()
                .map(|(code, path)| (code.to_string(), path.to_string()))
                .collect(),
            behind: None,
            hints: vec![
                ("Ret".to_string(), "stash and check out".to_string()),
                ("Esc".to_string(), "cancel".to_string()),
            ],
        }
    }

    /// The seven rows of the box that `confirm` paints in cells at 100 by 40,
    /// under a dead scene.
    fn painted(confirm: &CheckoutConfirm) -> Vec<String> {
        let (config, _) = stoat_config::parse(r##"theme t { ui.text.fg = "#c8ccd4"; }"##);
        let theme = Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        scene.set_live(false);
        render_checkout_confirm(confirm, &theme, area, &mut buf, &mut scene);
        (16..23)
            .map(|y| (22..78).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }
}
